//! Shared `tvm:memory` guest bindings.
//!
//! `sqlite-pcache-tvm` and `sqlite-vfs-tvm` both need the
//! `tvm-guest` world's imports. If each invoked
//! `wit_bindgen::generate!` independently the resulting wasm
//! would contain two copies of the same encoded-world custom
//! section, which is what wasm-tools 1.252's `component new`
//! chokes on (the second \0asm at byte 0x77f of the section
//! aborts parsing). Generate once, share.

wit_bindgen::generate!({
    path: "wit",
    world: "tvm-guest",
});
