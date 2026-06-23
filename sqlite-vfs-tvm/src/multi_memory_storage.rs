//! Multi-memory pool-backed file storage. Phase 4.1 destination
//! state, post-substrate-v2: file bytes live in pool 2 of the
//! `tvm-guest-mm` shell, addressed through the
//! `tvm-guest-mm-rt` dispatch helpers. Pool 2 is dedicated to
//! VFS files (the convention: pool 0 = default/heap, pool 1 =
//! pcache, pool 2 = VFS).
//!
//! Both target-arch gating lives in `lib.rs`; this module is
//! only compiled in for `target_arch = "wasm32"`. Native test
//! runs of the crate stay on the `InProcStorage` default and
//! never touch this code.
//!
//! ## Per-file slice + chunked addressing
//!
//! Each `xOpen` reserves a fixed-size `SLICE_BYTES` slice of
//! pool 2 via a static bump cursor (`NEXT_SLICE_OFFSET`). Within
//! a file's slice, bytes are organized in fixed-size
//! `CHUNK_SIZE` chunks, each chunk tracked in the file's
//! per-instance `HashSet<chunk_index>` to distinguish "never
//! written" (read as zeros + signal short read past EOF) from
//! "written" (read the actual bytes). The pool itself reads
//! zeros for never-written cells, but the trait contract is
//! that we honor `size()` boundaries, so we don't lean on the
//! pool's natural zero-fill behavior alone.
//!
//! Why chunk-grained instead of one slice per file as a flat
//! blob: tracking per-chunk write status lets `truncate` cheaply
//! drop the upper chunks without zeroing the underlying pool
//! cells; `read` past EOF returns a precise short-read signal;
//! and chunked metadata stays small (a u32 set, not a full page
//! payload).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use tvm_guest_mm_rt::{read_bytes, write_bytes, Pool};

use crate::storage::{FileStorage, StorageResult};
use libsqlite3_sys as ffi;

/// Pool reserved for the VFS file tier. Pool 0 is the cdylib's
/// default memory; pool 1 is pcache; pool 2 is VFS. Compose-time
/// pool count must be at least 3.
const VFS_POOL: Pool = Pool::new(2);

/// Per-chunk byte size. 4 KiB matches SQLite's default page size,
/// so aligned reads/writes hit a single chunk and skip the
/// read-modify-write tail. Cross-chunk I/O (journal headers,
/// page-zero updates) still works — it just splits across two
/// chunk-level operations.
const CHUNK_SIZE: u32 = 4096;

/// Bytes reserved per open VFS file. 256 MiB matches the legacy
/// `WitTvmStorage` per-file region size — enough for a
/// substantial single file (main db, journal, wal, temp) without
/// over-reserving across the small ones. Workloads with bigger
/// files should bump this constant + pool 2's `--max-pages` at
/// link time.
pub const SLICE_BYTES: u32 = 256 * 1024 * 1024;

/// Global bump cursor over pool 2. `MultiMemoryStorage::new`
/// reserves `SLICE_BYTES` starting at the current cursor when
/// the free list is empty; otherwise it pops a recycled base
/// from the free list (see `FREE_SLICES`).
static NEXT_SLICE_OFFSET: AtomicU32 = AtomicU32::new(0);

/// Free list of slice bases released by `MultiMemoryStorage::release`
/// (called from the VFS layer when SQLite deletes a file via
/// `xDelete` or when an auxiliary file's owner closes). Reusing
/// slices is load-bearing: SQLite opens/closes/deletes its rollback
/// journal many times during normal pager activity, and without
/// reuse the bump cursor walks past the pool's `--max-pages`
/// ceiling after only a couple of journal cycles — every subsequent
/// write to the journal traps OOB because the slice base lives in
/// a memory range the shell hasn't grown into.
///
/// The list uses a plain `Mutex<Vec<u32>>` rather than a lock-free
/// stack: SQLite's threading mode for the in-memory file path is
/// effectively single-threaded inside the wasm guest, contention is
/// nil, and the simpler shape keeps the implementation auditable.
static FREE_SLICES: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// File-storage backed by a slice of pool 2.
pub struct MultiMemoryStorage {
    /// Byte offset of this file's slice within pool 2.
    base: u32,
    /// Bytes available in this slice (always `SLICE_BYTES`).
    capacity: u32,
    /// Set of chunk indices (relative to `base`) that have
    /// been written at least once. Keyed by
    /// `chunk_index = byte_offset / CHUNK_SIZE`.
    written_chunks: HashSet<u32>,
    /// Logical file size in bytes. Independent of how many
    /// chunks are written — truncate can shrink size without
    /// scrubbing the higher chunks (we mark them
    /// unwritten by forgetting them).
    size: u64,
}

/// Storage construction failure.
#[derive(Debug)]
pub struct PoolFull;

impl MultiMemoryStorage {
    /// Reserve a slice of pool 2 and return a storage that addresses
    /// within it. Prefers a slice from the free list (released by an
    /// earlier `release` call) before bumping the cursor. Returns
    /// `Err(PoolFull)` if the bump cursor overflows u32 with the
    /// free list empty.
    pub fn new() -> Result<Self, PoolFull> {
        let base = if let Some(recycled) = FREE_SLICES.lock().ok().and_then(|mut v| v.pop()) {
            recycled
        } else {
            let b = NEXT_SLICE_OFFSET.fetch_add(SLICE_BYTES, Ordering::Relaxed);
            if b.checked_add(SLICE_BYTES).is_none() {
                return Err(PoolFull);
            }
            b
        };
        Ok(Self {
            base,
            capacity: SLICE_BYTES,
            written_chunks: HashSet::new(),
            size: 0,
        })
    }

}

/// On drop, return the slice's base to the free list so future
/// opens can recycle it. The VFS layer maintains `FILES` as
/// `Arc<Mutex<Box<dyn FileStorage>>>`; once `xDelete` or the
/// owning main-db's `io_close` removes the entry and the last
/// reference drops, this fires automatically.
///
/// Test note: native builds use `InProcStorage` and never reach
/// this `Drop`, so the free list never sees a stale base from
/// outside wasm.
impl Drop for MultiMemoryStorage {
    fn drop(&mut self) {
        if let Ok(mut v) = FREE_SLICES.lock() {
            v.push(self.base);
        }
    }
}

impl FileStorage for MultiMemoryStorage {
    fn read(&self, offset: u64, buf: &mut [u8]) -> StorageResult<()> {
        let end = offset + buf.len() as u64;
        let mut cur = offset;
        let mut buf_pos = 0;

        while cur < end {
            let chunk_idx = (cur / CHUNK_SIZE as u64) as u32;
            let in_chunk = (cur % CHUNK_SIZE as u64) as usize;
            let remaining_in_chunk = CHUNK_SIZE as usize - in_chunk;
            let needed = (end - cur) as usize;
            let copy_len = remaining_in_chunk.min(needed);

            if self.written_chunks.contains(&chunk_idx) {
                let chunk_base = self.chunk_byte_offset(chunk_idx);
                if let Err(rc) = self.check_in_bounds(chunk_base, CHUNK_SIZE) {
                    return Err(rc);
                }
                // Whole-chunk read, then slice the requested
                // range into `buf`. `read_bytes` is one host call
                // per chunk regardless of in-chunk slice size.
                let mut chunk_bytes = [0u8; CHUNK_SIZE as usize];
                read_bytes(VFS_POOL, self.base + chunk_base, &mut chunk_bytes);
                buf[buf_pos..buf_pos + copy_len]
                    .copy_from_slice(&chunk_bytes[in_chunk..in_chunk + copy_len]);
            } else {
                // Unwritten chunk → sparse-file zero semantics.
                buf[buf_pos..buf_pos + copy_len].fill(0);
            }

            buf_pos += copy_len;
            cur += copy_len as u64;
        }

        // SQLite contract: a read that extends past EOF fills
        // what's available, zeroes the rest (already done above)
        // and signals `SQLITE_IOERR_SHORT_READ`.
        if end > self.size {
            return Err(ffi::SQLITE_IOERR_SHORT_READ);
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> StorageResult<()> {
        let end = offset + buf.len() as u64;
        let mut cur = offset;
        let mut buf_pos = 0;

        while cur < end {
            let chunk_idx = (cur / CHUNK_SIZE as u64) as u32;
            let in_chunk = (cur % CHUNK_SIZE as u64) as usize;
            let remaining_in_chunk = CHUNK_SIZE as usize - in_chunk;
            let needed = (end - cur) as usize;
            let copy_len = remaining_in_chunk.min(needed);
            let chunk_base = self.chunk_byte_offset(chunk_idx);
            self.check_in_bounds(chunk_base, CHUNK_SIZE)
                .map_err(|_| ffi::SQLITE_IOERR_WRITE)?;

            if in_chunk == 0 && copy_len == CHUNK_SIZE as usize {
                // Full-chunk aligned write — straight bulk copy
                // into the pool. No read-modify-write needed.
                write_bytes(VFS_POOL, self.base + chunk_base, &buf[buf_pos..buf_pos + copy_len]);
            } else {
                // Partial-chunk write. Pull existing bytes (or
                // zeros for never-written chunks), splice in the
                // new content, write the whole chunk back.
                let mut chunk_bytes = [0u8; CHUNK_SIZE as usize];
                if self.written_chunks.contains(&chunk_idx) {
                    read_bytes(VFS_POOL, self.base + chunk_base, &mut chunk_bytes);
                }
                chunk_bytes[in_chunk..in_chunk + copy_len]
                    .copy_from_slice(&buf[buf_pos..buf_pos + copy_len]);
                write_bytes(VFS_POOL, self.base + chunk_base, &chunk_bytes);
            }

            self.written_chunks.insert(chunk_idx);
            buf_pos += copy_len;
            cur += copy_len as u64;
        }

        if end > self.size {
            self.size = end;
        }
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> StorageResult<()> {
        // Determine which chunks are entirely above the new
        // size. A `chunk_idx` is "above" if
        // `chunk_idx * CHUNK_SIZE >= size`; those get forgotten
        // wholesale (cheap: just drop the metadata). The
        // boundary chunk (if any) keeps its written marker — any
        // future read past `size` short-reads via the size check
        // in `read()` anyway.
        let first_dead = (size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64;
        self.written_chunks.retain(|&k| (k as u64) < first_dead);
        self.size = size;
        Ok(())
    }

    fn size(&self) -> u64 {
        self.size
    }
}

impl MultiMemoryStorage {
    fn chunk_byte_offset(&self, chunk_idx: u32) -> u32 {
        // `chunk_idx * CHUNK_SIZE` may overflow u32 for files
        // bigger than ~4 GiB; the bounds check catches that and
        // surfaces IOERR_WRITE. (SLICE_BYTES today caps the
        // per-file budget at 256 MiB anyway.)
        chunk_idx.saturating_mul(CHUNK_SIZE)
    }

    fn check_in_bounds(&self, offset: u32, len: u32) -> StorageResult<()> {
        let end = offset
            .checked_add(len)
            .ok_or(ffi::SQLITE_IOERR_WRITE)?;
        if end > self.capacity {
            return Err(ffi::SQLITE_IOERR_WRITE);
        }
        Ok(())
    }
}
