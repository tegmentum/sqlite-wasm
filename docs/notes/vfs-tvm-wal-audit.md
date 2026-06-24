# vfs-tvm WAL support — pre-implementation audit

This note records the state of the `sqlite-vfs-tvm` crate as of
the audit pass that precedes adding WAL substrate (file locking
+ shared memory), and the design we plan to apply.

## Why WAL doesn't work today

The in-wasm composed runtime that routes I/O through
`sqlite-vfs-tvm` cannot enter WAL journal mode. A user running
`PRAGMA journal_mode = WAL;` against a connection backed by
`tvm-mem` gets `"delete"` back (the rollback-journal default)
instead of `"wal"`. The browser `composed-wal-hook.spec.js`
already SKIPs with exactly this diagnostic.

WAL requires two VFS surfaces the current implementation does
not expose:

1. **File locking.** `xLock`, `xUnlock`, `xCheckReservedLock`,
   with SQLite's lock-level hierarchy (NONE / SHARED / RESERVED
   / PENDING / EXCLUSIVE). Today these are all stubs returning
   `SQLITE_OK` (see `src/lib.rs` `io_lock` / `io_unlock` /
   `io_check_reserved_lock`). They report success without
   tracking the lock level, which works for rollback-journal
   mode but fails SQLite's tighter assertions about transitions
   when WAL is requested.
2. **Shared memory** for the WAL index — the `-shm` file.
   SQLite expects `xShmMap`, `xShmLock`, `xShmBarrier`,
   `xShmUnmap` on `sqlite3_io_methods` with `iVersion >= 2`.
   Today our `IO_METHODS` constant is `iVersion: 1` and the
   shm slots are all `None`, so SQLite refuses to set up the
   wal-index and falls back to a non-WAL journal mode.

## Single-process, single-threaded simplification

The composed runtime is single-process and single-threaded
inside the wasm guest. Real multi-process locking is therefore
unnecessary: no other process can hold a competing lock, no
shared memory has to cross process boundaries. The WAL
substrate degenerates to bookkeeping that satisfies SQLite's
internal assertions:

- Each file tracks its own lock level. `xLock(target)` bumps
  the level to `target`; `xUnlock(target)` drops to `target`;
  `xCheckReservedLock` always reports 0 (nobody else holds it).
- The `-shm` file's contents are ordinary in-process memory.
  Each region SQLite asks for via `xShmMap` is a `Box<[u8]>`
  of the requested size, owned by the file (so it survives
  for the file's lifetime and is freed when the file closes).
- Per-file `[ShmLockState; 8]` tracks SQLite's 8 shm-lock
  slots (`SQLITE_SHM_NLOCK`). Single-process means contention
  is impossible; we still track state because SQLite asserts
  consistent transitions (e.g. an UNLOCK after a LOCK SHARED,
  not after a LOCK EXCLUSIVE held by someone else).
- `xShmBarrier` is `std::sync::atomic::fence(Ordering::SeqCst)`
  — cheap, matches the contract.

## Storage model recap

For context (verbatim from the existing code, not new):

- `FILES` is a `Mutex<HashMap<String, Arc<Mutex<Box<dyn FileStorage>>>>>`.
  One entry per logical file the VFS holds.
- `xOpen(name, ...)` either looks up an existing entry or, if
  `SQLITE_OPEN_CREATE` is set, allocates fresh storage via
  `make_storage()`.
- Native + browser-single-memory builds use `InProcStorage`
  (`Vec<u8>`); the multi-memory wasm build uses
  `MultiMemoryStorage` (pool 2 of `tvm-guest-mm`).
- Auxiliary files (`-journal` / `-wal` / `-shm`) live in the
  same FILES table as the main db; they share its lifecycle
  via the `is_auxiliary` flag and the prefix sweep in
  `io_close`.

Per-file shm regions and per-file lock state are NOT shared
across multiple opens — they live alongside the file's
storage handle in a new sibling struct (`ShmState`) keyed by
the same name. We keep that state in the FILES table so it
survives multiple opens of the same `-shm` filename (SQLite
opens the shm file as a logical second file and the wal-index
mapping has to be coherent across the two views).

## Implementation map

| Stage | File / function                                  |
| ----- | ------------------------------------------------ |
| 1     | this audit                                       |
| 2     | `src/lib.rs` `io_lock` / `io_unlock` /            |
|       | `io_check_reserved_lock` — track per-file level   |
| 3     | `src/lib.rs` new `io_shm_*` functions; new       |
|       | `ShmState` struct stored alongside `TvmFileInner` |
| 4     | `src/lib.rs` `IO_METHODS` — bump to `iVersion: 2`,|
|       | populate `xShm*` slots                            |
| 5     | rebuild scripts, native smoke regression          |
| 6     | browser composed-wal-hook spec                    |
| 7     | docs                                              |

## Constants we care about

From `libsqlite3-sys` 0.38.1 bindgen (sqlite 3.34.1 baseline):

- `SQLITE_LOCK_NONE = 0`, `SHARED = 1`, `RESERVED = 2`,
  `PENDING = 3`, `EXCLUSIVE = 4`
- `SQLITE_SHM_UNLOCK = 1`, `SHM_LOCK = 2`, `SHM_SHARED = 4`,
  `SHM_EXCLUSIVE = 8`, `SHM_NLOCK = 8`
- `SQLITE_FCNTL_LOCKSTATE = 1`
- `SQLITE_OPEN_WAL = 0x80000` (524288), `MAIN_DB = 256`

These guide the body of the new functions in Stage 2 + 3.
