//! End-to-end test for the host's extension-loader path.
//!
//! Exercises Host::load_extension on a real wasm component
//! (build/extensions/wasm-demo.wasm) to validate that:
//!   - the wasmtime engine compiles the component
//!   - the registry retains it under the file-stem name
//!   - is_loaded / list / unload work
//!
//! The test depends on `make extension-demo` having produced
//! build/extensions/wasm-demo.wasm; if that file is absent the test
//! is silently skipped so the suite stays green in environments
//! without the wasm toolchain.

use std::path::PathBuf;

use sqlite_wasm_host::{Capability, Host, Policy};

fn demo_wasm_path() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .join("build/extensions/wasm-demo.wasm");
    path.exists().then_some(path)
}

#[test]
fn loads_and_unloads_an_extension() {
    let Some(path) = demo_wasm_path() else {
        eprintln!("skipping: build/extensions/wasm-demo.wasm not built (run `make extension-demo`)");
        return;
    };

    let host = Host::new().expect("engine");
    assert!(host.list().is_empty(), "registry starts empty");

    let policy = Policy::deny_all().with_grants([Capability::Text]);
    let name = host.load_extension(path, policy).expect("load");

    assert_eq!(name, "wasm-demo");
    assert!(host.is_loaded(&name));
    assert_eq!(host.list(), vec!["wasm-demo".to_string()]);

    host.unload(&name).expect("unload");
    assert!(!host.is_loaded(&name));
    assert!(host.list().is_empty());
}

#[test]
fn double_unload_errors() {
    let host = Host::new().expect("engine");
    let err = host.unload("never-loaded").err().expect("must error");
    assert!(err.to_string().contains("never-loaded"));
}
