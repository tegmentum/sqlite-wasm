//! Multi-memory pool-backed cold-storage region. Phase 1.1
//! destination state, post-substrate-v2: page bytes live in a
//! non-default wasm memory pool (declared by the
//! `tvm-guest-mm` shell at link time), accessed via the
//! `tvm-guest-mm-rt` dispatch helpers. Each `ShadowCache`
//! grabs a fixed-size slice of pool 1 at construction; within
//! that slice, the trait's logical `offset` (which is
//! `key * sz_page` set by the cache) addresses pages directly.
//!
//! Both target-arch and feature gating live in `lib.rs`; this
//! module is only compiled in for `target_arch = "wasm32"`.
//! Native test runs of the crate stay on the `InProcRegion`
//! default and never touch this code.
//!
//! ## Pool layout
//!
//! Pool 1 is sized at composition time (see the linker's
//! `--max-pages` knob). Pcache instances bump-allocate a fixed
//! `SLICE_BYTES` window apiece via the static
//! `NEXT_SLICE_OFFSET` cursor. We never reclaim slices once a
//! cache drops — pcache lifetimes are coupled to SQLite
//! connection lifetimes, which in a wasm32 cdylib generally
//! match process lifetime. If the cursor ever bumps past pool
//! 1's capacity, region construction returns an error and the
//! caller falls back to the in-proc region (or fails).
//!
//! ## Tracking written offsets
//!
//! The `Region::read` contract distinguishes "page never
//! written" (returns `Ok(None)` → cache treats as zeroed) from
//! "page exists" (returns `Ok(Some(bytes))`). The pool memory
//! itself reads as zero for unwritten cells, so we track which
//! offsets have been written in a `HashSet<u32>` to preserve
//! the trait contract exactly. The set lives in default memory
//! (a few bytes per page); only the page payload itself lives
//! in the pool.
//!
//! ## Reentrancy / shared state
//!
//! The pcache runtime is single-threaded — SQLite calls our
//! trampolines synchronously — so the slice cursor uses a
//! relaxed atomic (no other thread to race with) and the
//! per-region written-set uses a plain `HashSet`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};

use tvm_guest_mm_rt::{read_bytes, write_bytes, Pool};

use crate::region::{Region, RegionError};

/// Pool reserved for the pcache cold tier. Pool 0 is the cdylib's
/// default memory; pool 1 is the convention for pcache pages.
/// Compose-time pool count must be at least 2.
const PCACHE_POOL: Pool = Pool::new(1);

/// Bytes per pcache instance. 256 MiB matches the legacy
/// `WitTvmRegion` default region size — large enough for a
/// realistic working set without over-reserving in the common
/// case.
pub const SLICE_BYTES: u32 = 256 * 1024 * 1024;

/// Global bump cursor over pool 1. Each `MultiMemoryRegion::new`
/// reserves `SLICE_BYTES` starting at the current cursor; the
/// cursor advances atomically. Wraps on overflow — operators
/// who hit this should pass a larger pool to the linker.
static NEXT_SLICE_OFFSET: AtomicU32 = AtomicU32::new(0);

/// Phase 1.3 capacity-test diagnostic: counts how many times
/// `Region::write` got called across all instances during this
/// wasm process. Surface kept identical to the legacy
/// `WitTvmRegion::lifetime_write_count` so probes that observe
/// the diagnostic continue to work without API change.
static LIFETIME_WRITE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Reads the lifetime write counter. Crate-public so probes can
/// expose it through a WIT export.
pub fn lifetime_write_count() -> u32 {
    LIFETIME_WRITE_COUNT.load(Ordering::Relaxed)
}

/// Multi-memory pool-backed Region. One slice of pool 1 per
/// `ShadowCache` (i.e. per SQLite pager), reserved on
/// construction; slices are never reclaimed (see module doc).
pub struct MultiMemoryRegion {
    /// Byte offset of this region's slice within pool 1.
    base: u32,
    /// Bytes available in this slice (always `SLICE_BYTES`).
    capacity: u32,
    /// Set of offsets (relative to `base`) that have been written
    /// at least once. The set is keyed by the cache's logical
    /// `offset` argument (= `key * sz_page`), matching how the
    /// cache invokes `read` / `write`.
    written: RefCell<HashSet<u32>>,
}

impl MultiMemoryRegion {
    /// Reserve a fresh slice of pool 1 and return a region that
    /// addresses within it. Returns `Err` if pool 1 is full.
    pub fn new() -> Result<Self, RegionError> {
        let base = NEXT_SLICE_OFFSET.fetch_add(SLICE_BYTES, Ordering::Relaxed);
        // Overflow check: a u32 + SLICE_BYTES wrap is the failure
        // signal. The bump cursor doesn't shrink; once over, all
        // subsequent constructions fail too, which is fine — the
        // cache falls back to its `InProcRegion` ctor (which never
        // runs on wasm; here it surfaces as a hard error so the
        // bug is visible).
        if base.checked_add(SLICE_BYTES).is_none() {
            return Err(RegionError::Backing(format!(
                "pcache pool 1 slice cursor overflowed (base={base}, slice={SLICE_BYTES}); \
                 increase --max-pages at link time or reduce slice count"
            )));
        }
        Ok(Self {
            base,
            capacity: SLICE_BYTES,
            written: RefCell::new(HashSet::new()),
        })
    }
}

impl Default for MultiMemoryRegion {
    fn default() -> Self {
        Self::new().expect("reserve pcache pool slice")
    }
}

impl Region for MultiMemoryRegion {
    fn read(&self, offset: u32, len: u32) -> Result<Option<Vec<u8>>, RegionError> {
        if !self.written.borrow().contains(&offset) {
            return Ok(None);
        }
        self.check_in_bounds(offset, len)?;
        let mut buf = vec![0u8; len as usize];
        read_bytes(PCACHE_POOL, self.base + offset, &mut buf);
        Ok(Some(buf))
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), RegionError> {
        LIFETIME_WRITE_COUNT.fetch_add(1, Ordering::Relaxed);
        let len = data.len() as u32;
        self.check_in_bounds(offset, len)?;
        write_bytes(PCACHE_POOL, self.base + offset, data);
        self.written.borrow_mut().insert(offset);
        Ok(())
    }

    fn copy(&mut self, src_offset: u32, dst_offset: u32, len: u32) -> Result<(), RegionError> {
        if !self.written.borrow().contains(&src_offset) {
            // No source bytes; the cache layer is best-effort on
            // copy (used by xRekey). Match the legacy
            // WitTvmRegion semantics: silently no-op.
            return Ok(());
        }
        self.check_in_bounds(src_offset, len)?;
        self.check_in_bounds(dst_offset, len)?;
        // Pool 1 has no in-pool copy primitive. Stage through a
        // default-memory scratch buffer; one host call each way.
        // sz_page is typically 4 KiB so the scratch is small.
        let mut scratch = vec![0u8; len as usize];
        read_bytes(PCACHE_POOL, self.base + src_offset, &mut scratch);
        write_bytes(PCACHE_POOL, self.base + dst_offset, &scratch);
        self.written.borrow_mut().insert(dst_offset);
        Ok(())
    }

    fn truncate_above(&mut self, limit: u32) -> Result<(), RegionError> {
        // The pool bytes don't need explicit reclamation — they
        // just become inaccessible once we forget them in
        // `written`. Future writes to the same offset re-mark the
        // entry and overwrite in place.
        self.written.borrow_mut().retain(|k| *k < limit);
        Ok(())
    }
}

impl MultiMemoryRegion {
    fn check_in_bounds(&self, offset: u32, len: u32) -> Result<(), RegionError> {
        let end = offset.checked_add(len).ok_or_else(|| {
            RegionError::Backing(format!(
                "pcache region access overflow: offset={offset}, len={len}"
            ))
        })?;
        if end > self.capacity {
            return Err(RegionError::Backing(format!(
                "pcache region access out of slice: offset={offset}, len={len}, capacity={}",
                self.capacity
            )));
        }
        Ok(())
    }
}
