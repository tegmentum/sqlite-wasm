// Build script for sqlite-component-core.
//
// On wasm32 targets, compiles src/vfs/vfs_wasi.c (from the parent
// `sqlink` repo, shared with the C CLI build) into the core
// crate. The wasivfs registers itself via `sqlite3_wasivfs_register`
// and bridges sqlite3's VFS calls onto WASI's filesystem syscalls,
// so a file-backed open under a host-preopened directory writes to
// disk.
//
// On native targets the wasivfs source isn't useful — libsqlite3-sys
// already has the unix VFS as default — so we skip the compile.

use std::env;
use std::path::PathBuf;

fn main() {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != "wasm32" {
        return;
    }

    // libsqlite3-sys exposes its bundled sqlite3 header dir via the
    // DEP_SQLITE3_INCLUDE env var when the `bundled` feature is on.
    let sqlite3_include = env::var("DEP_SQLITE3_INCLUDE").ok();

    // src/vfs/vfs_wasi.c is two dirs up from core/. Anchor to the
    // workspace root via CARGO_MANIFEST_DIR.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().expect("core has a parent dir");
    let vfs_src = workspace_root.join("src/vfs/vfs_wasi.c");
    let memvfs_src = workspace_root.join("src/vfs/vfs_memvfs.c");

    println!("cargo:rerun-if-changed={}", vfs_src.display());
    println!("cargo:rerun-if-changed={}", memvfs_src.display());

    let mut build = cc::Build::new();
    build
        .file(&vfs_src)
        .file(&memvfs_src)
        // suppress unused-function warnings on the bundled file
        .flag_if_supported("-Wno-unused-function")
        .flag_if_supported("-Wno-unused-variable")
        .flag_if_supported("-Wno-unused-parameter");
    if let Some(inc) = sqlite3_include {
        build.include(inc);
    }
    build.compile("sqlite_component_wasivfs");
}
