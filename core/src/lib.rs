//! Shared SQLite wrapper. A thin sync layer over `libsqlite3-sys`
//! that the cli and sqlite-lib crates both build on. Owns the
//! Connection, Statement, Value, Error types and the scalar /
//! aggregate / collation / hook / authorizer registration paths.
//!
//! Built for wasm32-wasip2 by both consumers — the bundled sqlite3.c
//! is compiled via `cc-rs` against the wasi-sdk sysroot.

pub mod db;

pub use db::*;
