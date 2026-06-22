//! Cold-storage abstraction. The Path D cache hot-fetches pages
//! from a region into bounded default-memory shadow slots and
//! flushes them back on unpin. The region is whatever survives
//! beyond the shadow pool — Phase 1.1's default is an in-process
//! `HashMap<u32, Vec<u8>>` so we can unit-test the eviction +
//! flush logic without a TVM host. Behind the `tvm` cargo feature,
//! a `tvm:memory`-backed region will plug in via the same trait
//! (follow-up commit; the trait shape is the integration point).
//!
//! Region addressing is `key * sz_page` — the SQLite page key
//! becomes the byte offset. `xRekey` does an in-region `copy`
//! between old and new offsets so the key→offset mapping stays
//! stable.

use std::collections::HashMap;

/// Failure modes a region can surface. Kept narrow on purpose —
/// the caller (the pcache trampolines) maps these to the right
/// SQLite return codes and the in-WASM CLI surfaces them to SQL.
#[derive(Debug)]
pub enum RegionError {
    /// The underlying store rejected an allocation / write — e.g.
    /// TVM's `bytes.write` returned a `tvm-error.out-of-bounds`.
    /// Message names the specific failure for diagnostics.
    Backing(String),
}

impl std::fmt::Display for RegionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegionError::Backing(s) => write!(f, "region backing error: {s}"),
        }
    }
}

impl std::error::Error for RegionError {}

/// Cold-storage interface the Path D cache calls into on miss /
/// flush. Methods are sync because the pcache trampolines are
/// sync (SQLite's pcache2 contract is synchronous); a future
/// async-capable variant would need a different cache shape.
///
/// Implementations: see `InProcRegion` (the in-process Vec<u8>
/// backed default for testing and the no-feature build) and the
/// forthcoming `WitTvmRegion` (gated on `feature = "tvm"`).
pub trait Region: Send {
    /// Read `len` bytes at `offset`. Returning `Ok(None)` means
    /// "no data has ever been written at that offset"; the cache
    /// then treats the page as freshly zeroed. `Ok(Some(_))` must
    /// be exactly `len` bytes.
    fn read(&self, offset: u32, len: u32) -> Result<Option<Vec<u8>>, RegionError>;

    /// Write `data` at `offset`. Creates the storage slot if
    /// missing. `data.len()` is the full page size — partial
    /// writes aren't part of the contract.
    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), RegionError>;

    /// Copy `len` bytes from `src_offset` to `dst_offset` within
    /// the region. Used by `xRekey` so the cache doesn't have to
    /// route bytes through default memory just to relocate them.
    /// An implementation that doesn't have a native copy primitive
    /// can `read` then `write`.
    fn copy(&mut self, src_offset: u32, dst_offset: u32, len: u32) -> Result<(), RegionError>;

    /// Drop bytes at every `offset >= limit`. Called by `xTruncate`
    /// to reclaim space when SQLite shrinks the cache.
    fn truncate_above(&mut self, limit: u32) -> Result<(), RegionError>;
}

/// In-process region. Backs pages with `Vec<u8>` keyed by offset;
/// no eviction here — the cache's shadow pool is what's bounded.
/// Used as the default region when the `tvm` feature is off, and
/// as the mock region for native unit tests of the shadow-pool
/// machinery.
pub struct InProcRegion {
    /// `key * sz_page` → page bytes. The cache writes only at
    /// page boundaries, so this is structurally one-slot-per-page.
    pages: HashMap<u32, Vec<u8>>,
}

impl InProcRegion {
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
        }
    }

    /// How many pages this region currently holds. Surfaced for
    /// tests that want to assert on the cold-tier population.
    #[cfg(test)]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }
}

impl Default for InProcRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl Region for InProcRegion {
    fn read(&self, offset: u32, len: u32) -> Result<Option<Vec<u8>>, RegionError> {
        match self.pages.get(&offset) {
            Some(v) => {
                if v.len() != len as usize {
                    return Err(RegionError::Backing(format!(
                        "in-proc region: offset {offset} has {} bytes, requested {len}",
                        v.len()
                    )));
                }
                Ok(Some(v.clone()))
            }
            None => Ok(None),
        }
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), RegionError> {
        self.pages.insert(offset, data.to_vec());
        Ok(())
    }

    fn copy(&mut self, src_offset: u32, dst_offset: u32, len: u32) -> Result<(), RegionError> {
        let src = match self.pages.get(&src_offset) {
            Some(v) => v.clone(),
            None => return Ok(()), // copy of nothing == nothing
        };
        if src.len() != len as usize {
            return Err(RegionError::Backing(format!(
                "in-proc region: copy len mismatch at src {src_offset}: {} != {len}",
                src.len()
            )));
        }
        self.pages.insert(dst_offset, src);
        Ok(())
    }

    fn truncate_above(&mut self, limit: u32) -> Result<(), RegionError> {
        self.pages.retain(|k, _| *k < limit);
        Ok(())
    }
}
