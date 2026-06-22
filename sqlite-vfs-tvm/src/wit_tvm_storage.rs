//! `tvm:memory`-backed file storage. Phase 4.1 destination state
//! for `sqlite-vfs-tvm`: file bytes live in non-default wasm
//! memory via the `tvm:memory/manager + bytes` interfaces, not
//! in the `Vec<u8>` of the in-proc backend.
//!
//! Target-arch and feature gating live in `lib.rs`; this module
//! is only compiled in for `target_arch = "wasm32"` +
//! `feature = "tvm"`. The host build with `--features tvm`
//! ignores it (because target_arch != wasm32) so native unit
//! tests stay on the in-proc backend.
//!
//! ## Chunked addressing
//!
//! Each file gets its own TVM region at construction. Within
//! the region, bytes are stored in fixed-size CHUNK_SIZE pieces,
//! each backed by one `manager.alloc` handle, keyed in a
//! `HashMap<chunk_index, Handle>`. VFS reads/writes that span
//! chunk boundaries get split into per-chunk operations; partial
//! writes inside a chunk read-modify-write through default
//! memory.
//!
//! Why chunk-grained instead of one-allocation-per-file: a TVM
//! region's allocation size is fixed at create time. Growing a
//! file would otherwise require destroying and re-creating the
//! whole storage. Chunking gives natural growth without that
//! reshuffle.

use std::collections::HashMap;

use crate::storage::{FileStorage, StorageResult};
use libsqlite3_sys as ffi;

// `tvm:memory` bindings come from the shared crate so the
// `encoded world` custom section appears exactly once in the
// final wasm (see sqlite-tvm-bindings).
use sqlite_tvm_bindings as bindings;
use bindings::tvm::memory::bytes;
use bindings::tvm::memory::manager;
use bindings::tvm::memory::types::{Handle, RegionKind, TvmError};

/// Size of each TVM-allocated chunk. 4 KB matches SQLite's
/// default page size, so most aligned reads/writes hit exactly
/// one chunk and skip the read-modify-write path. Cross-page
/// I/O (journal headers, page-zero updates) still works  it
/// just costs an extra `bytes::read` for the partially-written
/// chunks.
const CHUNK_SIZE: u32 = 4096;

/// Default initial capacity of a new file's TVM region. SQLite
/// uses one region per file (db + journal + wal + temp); 256 MiB
/// is enough for a substantial single file without over-reserving
/// for the small ones. Deployments with bigger files should
/// configure a larger initial capacity at the host's `TvmHost`
/// level rather than per-file.
const DEFAULT_REGION_BYTES: u32 = 256 * 1024 * 1024;

pub struct WitTvmStorage {
    region_id: u16,
    /// chunk_index -> handle from manager.alloc.
    chunks: HashMap<u32, Handle>,
    /// Logical file size in bytes. Independent from how many
    /// chunks are allocated  truncate can shrink size without
    /// freeing chunks below the new boundary.
    size: u64,
}

impl WitTvmStorage {
    pub fn new() -> Result<Self, TvmError> {
        let region_id = manager::create_region(RegionKind::PageStore, DEFAULT_REGION_BYTES)?;
        Ok(Self {
            region_id,
            chunks: HashMap::new(),
            size: 0,
        })
    }
}

impl Drop for WitTvmStorage {
    fn drop(&mut self) {
        // Free the entire region in one host call. TVM frees every
        // allocation inside it; no need to walk the chunks map.
        let _ = manager::destroy_region(self.region_id);
    }
}

impl FileStorage for WitTvmStorage {
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

            match self.chunks.get(&chunk_idx) {
                Some(handle) => {
                    // Whole-chunk read; copy the sliced portion. The
                    // tvm:memory.bytes.read host call returns a
                    // freshly-allocated Vec of exactly CHUNK_SIZE
                    // bytes (or an error on stale handle / OOB).
                    let chunk_bytes = bytes::read(*handle, CHUNK_SIZE)
                        .map_err(|_| ffi::SQLITE_IOERR_READ)?;
                    if chunk_bytes.len() < in_chunk + copy_len {
                        return Err(ffi::SQLITE_IOERR_READ);
                    }
                    buf[buf_pos..buf_pos + copy_len]
                        .copy_from_slice(&chunk_bytes[in_chunk..in_chunk + copy_len]);
                }
                None => {
                    // No chunk allocated  read as zeros (sparse file
                    // semantics). SQLite tolerates this because it
                    // doesn't read past xFileSize unless we tell it
                    // the file's bigger.
                    buf[buf_pos..buf_pos + copy_len].fill(0);
                }
            }

            buf_pos += copy_len;
            cur += copy_len as u64;
        }

        // SQLite contract: if the read extends past the end of the
        // file, fill what's available, zero the rest, return
        // SQLITE_IOERR_SHORT_READ.
        if end > self.size {
            // We already zero-filled any unallocated chunks above,
            // so buf is correctly shaped  just signal the short
            // read.
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

            // Look up or allocate the chunk handle. New chunks get
            // a fresh CHUNK_SIZE allocation.
            let handle = match self.chunks.get(&chunk_idx) {
                Some(h) => *h,
                None => {
                    let h = manager::alloc(self.region_id, CHUNK_SIZE)
                        .map_err(|_| ffi::SQLITE_IOERR_WRITE)?;
                    self.chunks.insert(chunk_idx, h);
                    h
                }
            };

            // For full-chunk writes (aligned start, full length),
            // skip the read-modify-write. Otherwise pull existing
            // bytes into a local buffer, splice in the new content,
            // write it back.
            if in_chunk == 0 && copy_len == CHUNK_SIZE as usize {
                bytes::write(handle, &buf[buf_pos..buf_pos + copy_len])
                    .map_err(|_| ffi::SQLITE_IOERR_WRITE)?;
            } else {
                let mut chunk_bytes = bytes::read(handle, CHUNK_SIZE)
                    .unwrap_or_else(|_| vec![0u8; CHUNK_SIZE as usize]);
                if chunk_bytes.len() < CHUNK_SIZE as usize {
                    chunk_bytes.resize(CHUNK_SIZE as usize, 0);
                }
                chunk_bytes[in_chunk..in_chunk + copy_len]
                    .copy_from_slice(&buf[buf_pos..buf_pos + copy_len]);
                bytes::write(handle, &chunk_bytes)
                    .map_err(|_| ffi::SQLITE_IOERR_WRITE)?;
            }

            buf_pos += copy_len;
            cur += copy_len as u64;
        }

        if end > self.size {
            self.size = end;
        }
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> StorageResult<()> {
        // Determine which chunks are entirely above the new size.
        // A chunk_index is "above" if chunk_index * CHUNK_SIZE
        // is >= size; those get dropped wholesale. The boundary
        // chunk (if any) keeps its handle  any future read past
        // `size` short-reads anyway via the size check above.
        let first_dead = (size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64;
        let dead: Vec<u32> = self
            .chunks
            .keys()
            .copied()
            .filter(|&k| (k as u64) >= first_dead)
            .collect();
        for k in dead {
            if let Some(h) = self.chunks.remove(&k) {
                let _ = manager::dealloc(h);
            }
        }
        self.size = size;
        Ok(())
    }

    fn size(&self) -> u64 {
        self.size
    }
}
