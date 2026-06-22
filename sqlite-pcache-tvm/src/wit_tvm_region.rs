//! `tvm:memory`-backed cold-storage region. The Phase 1.1
//! destination state: page bytes that don't fit in the shadow
//! pool live in a TVM `page-store` region (which itself can grow
//! past 4 GiB by tiering to disk or non-default wasm memory),
//! and the cache reads/writes them through the
//! `tvm:memory/manager` + `tvm:memory/bytes` interfaces.
//!
//! Both target-arch and feature gating live in `lib.rs`; this
//! module is only compiled in for `target_arch = "wasm32"` +
//! `feature = "tvm"`. Native test runs of the crate stay on the
//! `InProcRegion` default and never touch this code.
//!
//! ## Offset-to-handle translation
//!
//! The `Region` trait addresses pages by `(offset, len)`. TVM
//! addresses by `Handle { region-id, generation, offset }` — each
//! `manager.alloc(region, size)` returns a fresh handle. We carry
//! a `HashMap<u32, Handle>` keyed by the trait's logical offset
//! so `region.read(off, len)` can look up the corresponding TVM
//! handle. Cost: ~24 bytes / page in default memory for the
//! HashMap entry (key + handle + table overhead). For a 4 KB page
//! that's ~0.6% of the page size — small enough that the
//! "metadata stays in default memory, bytes go to TVM" split
//! still pays off.
//!
//! ## Reentrancy / shared state
//!
//! The `Region::read` method is `&self` but the handle table is
//! mutated lazily (e.g. by `write` allocating new handles). The
//! pcache runtime is single-threaded — SQLite calls into our
//! trampolines synchronously — so a `RefCell` is sufficient. We
//! don't need locking.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::region::{Region, RegionError};

/// Phase 1.3 capacity-test diagnostic: counts how many times
/// `Region::write` got called against any WitTvmRegion instance
/// during this wasm process. Exposed via the public
/// `lifetime_write_count()` accessor so the probe can return it
/// to the host and the host can assert non-zero (i.e., the
/// cold-tier write path actually fired).
static LIFETIME_WRITE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Reads the lifetime write counter. Crate-public so the probe
/// (in a sibling cdylib) can expose it through a WIT export.
pub fn lifetime_write_count() -> u32 {
    LIFETIME_WRITE_COUNT.load(Ordering::Relaxed)
}

// `tvm:memory` bindings come from the shared crate so the
// `encoded world` custom section appears exactly once in the
// final wasm (see sqlite-tvm-bindings).
use sqlite_tvm_bindings as bindings;
use bindings::tvm::memory::bytes;
use bindings::tvm::memory::manager;
use bindings::tvm::memory::types::{Handle, RegionKind, TvmError};

/// `tvm:memory`-backed Region. One region per `ShadowCache` (i.e.
/// per SQLite pager) — created on construction, destroyed when
/// the cache drops.
pub struct WitTvmRegion {
    region_id: u16,
    /// `offset` -> handle from `manager.alloc`. Single-threaded
    /// access (the pcache runtime is sync) so `RefCell` is the
    /// right shape for the `&self` read path.
    handles: RefCell<HashMap<u32, Handle>>,
}

impl WitTvmRegion {
    /// Create a new region of the given byte capacity. SQLite's
    /// `xCreate` is what eventually drives this — once we know
    /// `sz_page` and the shadow_capacity it sets a reasonable
    /// starting capacity for the cold tail.
    pub fn new(capacity: u32) -> Result<Self, RegionError> {
        let region_id = manager::create_region(RegionKind::PageStore, capacity)
            .map_err(tvm_err)?;
        Ok(Self {
            region_id,
            handles: RefCell::new(HashMap::new()),
        })
    }
}

/// Default initial capacity of a new `WitTvmRegion` in bytes
/// (256 MiB). SQLite hasn't yet told us its page size or cache
/// budget when we hand the cache its region, and tvm-core regions
/// don't grow past their declared capacity, so we pick a value
/// large enough to hold a realistic working set without
/// over-reserving in the common case. Operators who need the
/// full > 4 GiB story should configure a larger initial capacity
/// at the host's `TvmHost` level (the engine, not this default).
const DEFAULT_REGION_BYTES: u32 = 256 * 1024 * 1024;

impl Default for WitTvmRegion {
    fn default() -> Self {
        Self::new(DEFAULT_REGION_BYTES).expect("create initial tvm page-store region")
    }
}

impl Region for WitTvmRegion {
    fn read(&self, offset: u32, len: u32) -> Result<Option<Vec<u8>>, RegionError> {
        let map = self.handles.borrow();
        match map.get(&offset) {
            Some(h) => {
                let v = bytes::read(*h, len).map_err(tvm_err)?;
                if v.len() != len as usize {
                    return Err(RegionError::Backing(format!(
                        "tvm region read: expected {len} bytes at offset {offset}, got {}",
                        v.len()
                    )));
                }
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), RegionError> {
        LIFETIME_WRITE_COUNT.fetch_add(1, Ordering::Relaxed);
        let mut map = self.handles.borrow_mut();
        let handle = match map.get(&offset).copied() {
            Some(h) => h,
            None => {
                let h = manager::alloc(self.region_id, data.len() as u32).map_err(tvm_err)?;
                map.insert(offset, h);
                h
            }
        };
        bytes::write(handle, data).map_err(tvm_err)
    }

    fn copy(&mut self, src_offset: u32, dst_offset: u32, len: u32) -> Result<(), RegionError> {
        let mut map = self.handles.borrow_mut();
        let src = match map.get(&src_offset).copied() {
            Some(h) => h,
            None => return Ok(()), // nothing at src, nothing to copy
        };
        let dst = match map.get(&dst_offset).copied() {
            Some(h) => h,
            None => {
                let h = manager::alloc(self.region_id, len).map_err(tvm_err)?;
                map.insert(dst_offset, h);
                h
            }
        };
        bytes::copy(src, dst, len).map_err(tvm_err)
    }

    fn truncate_above(&mut self, limit: u32) -> Result<(), RegionError> {
        let mut map = self.handles.borrow_mut();
        // Collect first to drop the borrow before mutating, then
        // dealloc each handle  TVM does the actual byte reclaim.
        let dropped: Vec<(u32, Handle)> = map
            .iter()
            .filter(|(k, _)| **k >= limit)
            .map(|(k, h)| (*k, *h))
            .collect();
        for (k, h) in dropped {
            map.remove(&k);
            // Best-effort dealloc; if TVM returns an error we drop
            // the handle but report through the trait. The cache's
            // truncate path ignores errors anyway (it's running
            // during SQLite's xTruncate which is fire-and-forget).
            let _ = manager::dealloc(h);
        }
        Ok(())
    }
}

impl Drop for WitTvmRegion {
    fn drop(&mut self) {
        // Drop the region wholesale. TVM frees every allocation
        // inside it; we don't have to walk the handle table.
        let _ = manager::destroy_region(self.region_id);
    }
}

fn tvm_err(e: TvmError) -> RegionError {
    RegionError::Backing(format!("tvm-error: {e:?}"))
}
