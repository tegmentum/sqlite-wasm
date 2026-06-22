//! Path D cache. Bounded shadow pool of `sz_page + sz_extra`
//! slots in default memory, backed by a `Region` cold-storage
//! tier for pages above the pool capacity.
//!
//! ## Invariants
//!
//! - `lookup` holds an entry for every live page (shadow + cold).
//!   Live shadow entries point at a `ShadowSlot`; cold entries
//!   carry `None` and indicate the page bytes live only in the
//!   region.
//! - The LRU list links unpinned `ShadowSlot`s only. Pinned slots
//!   are out-of-list (no LRU prev/next links).
//! - `shadow_count == lookup_in_shadow.count()` at every step.
//! - For every shadow slot S: S.key is in lookup AND lookup[S.key]
//!   points at S. The HashMap is the source of truth for keys.
//!
//! ## Eviction (locked-in design)
//!
//! Pinned-aware LRU: pinned slots are not eviction candidates;
//! among unpinned slots, evict the LRU tail. On eviction the
//! victim's bytes get flushed to the region (per the
//! always-flush dirty-tracking decision) and the slot is freed.
//! See the Phase 1.1 architectural-finding block in
//! PLAN-tvm-integration.md for the rationale.

use std::alloc::{alloc, dealloc, Layout};
use std::collections::HashMap;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr::{self, NonNull};

use libsqlite3_sys::sqlite3_pcache_page;

use crate::region::Region;

/// Per-cache shadow slot. Tail-allocated so `pBuf` and `pExtra`
/// point into the contiguous block following the struct header.
/// Same layout idea as the Phase 1.0 `PageEntry`, with extra
/// fields for the pinned-aware LRU bookkeeping.
#[repr(C)]
pub(crate) struct ShadowSlot {
    /// SQLite-visible page handle. `pBuf` / `pExtra` are wired
    /// to point into the tail bytes that follow this struct.
    pub(crate) header: sqlite3_pcache_page,
    /// Layout saved at alloc, replayed at dealloc to satisfy the
    /// global allocator's matched-alloc/dealloc contract.
    layout: Layout,
    /// SQLite page key this slot is currently caching.
    pub(crate) key: c_uint,
    /// True iff SQLite holds the pointer (between `xFetch` and
    /// `xUnpin`). Pinned slots are not in the LRU list.
    pub(crate) pinned: bool,
    /// LRU links — `None` while the slot is pinned.
    lru_prev: Option<NonNull<ShadowSlot>>,
    lru_next: Option<NonNull<ShadowSlot>>,
}

impl ShadowSlot {
    /// Allocate a new slot sized for `sz_page + sz_extra` payload.
    /// Caller owns the returned pointer; drop via `dealloc`.
    fn alloc_zeroed(sz_page: c_int, sz_extra: c_int) -> *mut ShadowSlot {
        let header_size = std::mem::size_of::<ShadowSlot>();
        let total = header_size + sz_page as usize + sz_extra as usize;
        // 16 byte align — matches what most libc mallocs return
        // and what SQLite expects for general-purpose buffers.
        let layout = Layout::from_size_align(total, 16).expect("pcache shadow-slot layout");
        let raw = unsafe { alloc(layout) };
        if raw.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        // SAFETY: raw points at uninit memory of at least `total`
        // bytes, aligned to 16. Zero the entire block so SQLite
        // sees zeroed page + extra bytes (its pcache1 default).
        unsafe { std::ptr::write_bytes(raw, 0, total) };
        let slot = raw as *mut ShadowSlot;
        let pbuf_ptr = unsafe { (raw as *mut u8).add(header_size) };
        let pextra_ptr = unsafe { pbuf_ptr.add(sz_page as usize) };
        // SAFETY: `slot` points at uninit (zeroed) memory laid
        // out for `ShadowSlot`; we initialize all fields directly.
        unsafe {
            std::ptr::write(
                slot,
                ShadowSlot {
                    header: sqlite3_pcache_page {
                        pBuf: pbuf_ptr as *mut c_void,
                        pExtra: pextra_ptr as *mut c_void,
                    },
                    layout,
                    key: 0,
                    pinned: false,
                    lru_prev: None,
                    lru_next: None,
                },
            );
        }
        slot
    }

    /// SAFETY: `slot` must be non-null, from `alloc_zeroed`, with
    /// no live references remaining.
    unsafe fn dealloc(slot: *mut ShadowSlot) {
        let layout = (*slot).layout;
        std::ptr::drop_in_place(slot);
        dealloc(slot as *mut u8, layout);
    }

    /// Page-bytes view. The slice spans `sz_page` bytes at `pBuf`
    /// — what gets flushed to the region on `xUnpin` and overwritten
    /// on a re-fetch from the cold tier.
    pub(crate) unsafe fn page_bytes(&self, sz_page: c_int) -> &[u8] {
        std::slice::from_raw_parts(self.header.pBuf as *const u8, sz_page as usize)
    }

    /// Mutable counterpart, used when faulting a page in from the
    /// region into the shadow slot.
    pub(crate) unsafe fn page_bytes_mut(&mut self, sz_page: c_int) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.header.pBuf as *mut u8, sz_page as usize)
    }
}

/// What `lookup` stores for a given SQLite key. Either the page
/// is in the shadow pool (active slot) or in the cold region
/// (no slot, bytes live only in `Region`).
#[derive(Copy, Clone)]
enum LookupEntry {
    Shadow(NonNull<ShadowSlot>),
    Cold,
}

/// The cache instance. One per `xCreate` call (== one per SQLite
/// `sqlite3_pcache` opaque handle == one per pager). Sized at
/// `xCachesize` time; defaults to `DEFAULT_SHADOW_CAPACITY` until
/// SQLite tells us otherwise.
pub struct ShadowCache<R: Region> {
    sz_page: c_int,
    sz_extra: c_int,
    /// SQLite's `bPurgeable` flag from `xCreate`. Means "the
    /// cache is allowed to evict pages under memory pressure"
    /// not "discard on every unpin." Our shadow pool enforces
    /// a strict cap regardless (the LRU evicts during `xFetch`
    /// when full), so the flag carries no extra information for
    /// v1. Kept on the struct so a future tier-shrinking policy
    /// can opt to demote-on-unpin when purgeable=true.
    #[allow(dead_code)]
    purgeable: bool,
    /// Shadow-pool cap in slots. `xCachesize` rewrites this; on
    /// shrink we evict from the LRU tail until we're within the
    /// new cap.
    shadow_capacity: u32,
    /// Source of truth for "which pages does this cache know
    /// about." Shadow vs Cold tells us where the bytes live now.
    lookup: HashMap<c_uint, LookupEntry>,
    /// LRU head (most recently unpinned). `xFetch` evicts from
    /// the tail.
    lru_head: Option<NonNull<ShadowSlot>>,
    /// LRU tail (oldest unpinned, next eviction candidate).
    lru_tail: Option<NonNull<ShadowSlot>>,
    /// Slots currently in the shadow pool (pinned or in LRU).
    shadow_count: u32,
    /// Cold-storage backend.
    region: R,
}

/// Conservative startup default for the shadow pool before SQLite
/// calls `xCachesize`. 100 pages * 4 KB = 400 KB, big enough for
/// schema + a small open path's working set without making the
/// pre-`xCachesize` window meaningfully limit memory.
pub const DEFAULT_SHADOW_CAPACITY: u32 = 100;

impl<R: Region> ShadowCache<R> {
    pub fn new(sz_page: c_int, sz_extra: c_int, purgeable: bool, region: R) -> Self {
        Self {
            sz_page,
            sz_extra,
            purgeable,
            shadow_capacity: DEFAULT_SHADOW_CAPACITY,
            lookup: HashMap::new(),
            lru_head: None,
            lru_tail: None,
            shadow_count: 0,
            region,
        }
    }

    /// Resize the shadow pool. May evict if the new cap is below
    /// the current shadow population.
    pub fn set_cachesize(&mut self, n: c_int) {
        let new_cap = if n < 0 { 0 } else { n as u32 };
        self.shadow_capacity = new_cap;
        // Evict from LRU tail until shadow_count <= shadow_capacity.
        while self.shadow_count > self.shadow_capacity {
            if self.evict_one().is_err() {
                // Eviction can only fail if every slot is pinned.
                // SQLite shouldn't ever do that, but if it does
                // we leave the cache over-cap rather than crash —
                // it'll come back into bounds the moment any page
                // unpins.
                break;
            }
        }
    }

    /// Number of pages SQLite is currently aware of (shadow + cold).
    pub fn page_count(&self) -> c_int {
        self.lookup.len() as c_int
    }

    /// Diagnostic. The configured shadow-pool cap.
    pub fn shadow_capacity(&self) -> u32 {
        self.shadow_capacity
    }

    /// Diagnostic. Pages currently held in default memory (pinned
    /// + LRU-resident).
    pub fn shadow_count(&self) -> u32 {
        self.shadow_count
    }

    /// Fetch the page for `key`, allocating storage if missing
    /// and `create_flag != 0`. Returns the SQLite-visible header
    /// pointer, or null if no page exists and SQLite told us not
    /// to create one.
    pub fn fetch(&mut self, key: c_uint, create_flag: c_int) -> *mut sqlite3_pcache_page {
        // Hit on the shadow tier.
        if let Some(LookupEntry::Shadow(slot_ptr)) = self.lookup.get(&key).copied() {
            // SAFETY: slot_ptr came from our own allocation and
            // is owned by this cache; we have exclusive access
            // through &mut self.
            unsafe {
                if !(*slot_ptr.as_ptr()).pinned {
                    self.lru_remove(slot_ptr);
                    (*slot_ptr.as_ptr()).pinned = true;
                }
                return &mut (*slot_ptr.as_ptr()).header;
            }
        }

        // Hit on the cold tier. Promote: pull bytes from the region
        // into a fresh / evicted shadow slot. Falls through to the
        // miss-path below; the difference is whether we attempt the
        // region.read.
        let cold_hit = matches!(self.lookup.get(&key), Some(LookupEntry::Cold));

        if !cold_hit && create_flag == 0 {
            // Pure miss and SQLite said "don't allocate." That's
            // a real return-null path (not an error) — SQLite uses
            // create_flag=0 to probe whether a page is already
            // cached.
            return ptr::null_mut();
        }

        // Need a shadow slot. Either reuse an evicted one or alloc
        // a fresh one — easier to just always alloc fresh; the
        // eviction path is independent from slot allocation in this
        // impl. (Re-using the evicted block would save a malloc;
        // optimization to revisit when profiling demands it.)
        if self.shadow_count >= self.shadow_capacity {
            if self.evict_one().is_err() {
                // Every slot pinned. SQLite's contract says
                // create_flag=2 means "allocate even if over budget"
                // (we treat that as "make space anyway") and
                // create_flag=1 means "respect budget" (we return
                // null so SQLite can recover). For now we error
                // out gracefully — returning null keeps the
                // contract.
                if create_flag != 2 {
                    return ptr::null_mut();
                }
                // create_flag == 2: SQLite says it really needs
                // the page. We allocate over-cap.
            }
        }
        let slot = ShadowSlot::alloc_zeroed(self.sz_page, self.sz_extra);
        // SAFETY: just allocated; valid pointer with zero refs.
        unsafe {
            (*slot).key = key;
            (*slot).pinned = true;
        }

        if cold_hit {
            // Page bytes exist in the region — pull them in.
            match self.region.read(key * self.sz_page as u32, self.sz_page as u32) {
                Ok(Some(bytes)) => {
                    if bytes.len() == self.sz_page as usize {
                        // SAFETY: slot is alive; we just allocated
                        // and pin it. page_bytes_mut returns a
                        // mutable slice of exactly sz_page bytes.
                        unsafe {
                            (*slot).page_bytes_mut(self.sz_page).copy_from_slice(&bytes);
                        }
                    } else {
                        // Region returned wrong size — treat as
                        // missing data (zeros, already there from
                        // alloc_zeroed). Better to surface a
                        // visible SQL error if this ever fires;
                        // the trampoline can map this to NULL
                        // return at the boundary.
                    }
                }
                Ok(None) | Err(_) => {
                    // Page wasn't actually cold or region failed;
                    // leave zeroed. SQLite handles a brand-new
                    // page being zero.
                }
            }
        }
        // Insert into the shadow tier and update the lookup. cold_hit
        // means the entry already exists as Cold; we overwrite to
        // Shadow.
        self.lookup.insert(
            key,
            LookupEntry::Shadow(NonNull::new(slot).expect("non-null fresh slot")),
        );
        self.shadow_count += 1;
        // SAFETY: slot just got installed into lookup; pointer is
        // valid until xUnpin/xTruncate/xDestroy releases it.
        unsafe { &mut (*slot).header }
    }

    /// Release a page. If `discard`, free the slot and forget the
    /// key entirely (drops both shadow and cold copies). Else flush
    /// the shadow bytes back to the region (per the always-flush
    /// dirty-tracking decision) and push to LRU head.
    ///
    /// SAFETY: `page` must be a header pointer returned by `fetch`
    /// against this cache and not yet unpinned-and-discarded.
    pub unsafe fn unpin(&mut self, page: *mut sqlite3_pcache_page, discard: bool) {
        let slot = page as *mut ShadowSlot;
        let key = (*slot).key;

        if discard {
            // SQLite says drop it. (`purgeable` doesn't trigger
            // this path — that flag means "the cache is allowed to
            // evict pages under pressure," which we do via the
            // LRU during xFetch. xUnpin with discard=0 against a
            // purgeable cache still means "keep this reachable.")
            //
            // Free the slot AND drop any cold copy so the key
            // vanishes from this cache entirely (a re-fetch with
            // create_flag=0 must return null; with create_flag!=0
            // must give back a zeroed page).
            self.lookup.remove(&key);
            // If the slot was the LRU head/tail we'd be in trouble,
            // but a pinned slot is never in the LRU.
            debug_assert!((*slot).pinned, "unpin called on already-unpinned slot");
            self.shadow_count -= 1;
            ShadowSlot::dealloc(slot);
            return;
        }

        // Always-flush per the locked design decision. The cost is
        // the host call (for the TVM backend) — see the dirty-
        // tracking discussion in PLAN-tvm-integration.md.
        let bytes_copy: Vec<u8> = (*slot).page_bytes(self.sz_page).to_vec();
        let _ = self
            .region
            .write(key * self.sz_page as u32, &bytes_copy);

        (*slot).pinned = false;
        let slot_ptr = NonNull::new(slot).expect("non-null unpinned slot");
        self.lru_push_front(slot_ptr);
    }

    /// Update the key for a previously-fetched page. Reflects the
    /// rename into both the shadow lookup and the region (an
    /// in-region copy from old offset to new offset).
    ///
    /// SAFETY: `page` must be a header pointer returned by `fetch`
    /// against this cache.
    pub unsafe fn rekey(
        &mut self,
        page: *mut sqlite3_pcache_page,
        old_key: c_uint,
        new_key: c_uint,
    ) {
        let slot = page as *mut ShadowSlot;
        // If new_key already exists in the cache, the contract
        // says to drop it (the slot we're renaming TO becomes
        // canonical).
        if let Some(entry) = self.lookup.remove(&new_key) {
            match entry {
                LookupEntry::Shadow(existing) => {
                    if existing.as_ptr() != slot {
                        // Drop the displaced slot. It's in the LRU
                        // (or pinned — that would be a bug on
                        // SQLite's side).
                        if !(*existing.as_ptr()).pinned {
                            self.lru_remove(existing);
                        }
                        self.shadow_count -= 1;
                        ShadowSlot::dealloc(existing.as_ptr());
                    }
                }
                LookupEntry::Cold => {}
            }
        }
        self.lookup.remove(&old_key);
        (*slot).key = new_key;
        self.lookup.insert(new_key, LookupEntry::Shadow(NonNull::new(slot).unwrap()));

        // Move region bytes too so a future eviction round trip
        // doesn't surprise SQLite.
        let _ = self.region.copy(
            old_key * self.sz_page as u32,
            new_key * self.sz_page as u32,
            self.sz_page as u32,
        );
    }

    /// Drop every page with `key >= limit`. Used by `xTruncate`.
    pub fn truncate(&mut self, limit: c_uint) {
        let to_remove: Vec<c_uint> = self
            .lookup
            .keys()
            .copied()
            .filter(|k| *k >= limit)
            .collect();
        for k in to_remove {
            if let Some(entry) = self.lookup.remove(&k) {
                if let LookupEntry::Shadow(slot_ptr) = entry {
                    // SAFETY: slot_ptr was installed into the
                    // lookup by `fetch`; no other refs exist.
                    unsafe {
                        if !(*slot_ptr.as_ptr()).pinned {
                            self.lru_remove(slot_ptr);
                        }
                        self.shadow_count -= 1;
                        ShadowSlot::dealloc(slot_ptr.as_ptr());
                    }
                }
            }
        }
        // Drop the cold tail too.
        let cold_offset_floor = limit * self.sz_page as u32;
        let _ = self.region.truncate_above(cold_offset_floor);
    }

    /// Evict the LRU tail slot. Returns Err if every slot is
    /// pinned (no eviction candidate).
    fn evict_one(&mut self) -> Result<(), ()> {
        let victim = self.lru_tail.ok_or(())?;
        // SAFETY: victim came from `lru_push_front`; alive while
        // it's in the LRU list (i.e. unpinned).
        unsafe {
            let key = (*victim.as_ptr()).key;
            // Flush victim bytes to the region before freeing.
            let bytes: Vec<u8> = (*victim.as_ptr()).page_bytes(self.sz_page).to_vec();
            let _ = self.region.write(key * self.sz_page as u32, &bytes);
            self.lru_remove(victim);
            // Demote the lookup entry: shadow → cold. The bytes
            // live only in the region now.
            self.lookup.insert(key, LookupEntry::Cold);
            self.shadow_count -= 1;
            ShadowSlot::dealloc(victim.as_ptr());
        }
        Ok(())
    }

    /// Push a slot onto the LRU head. Caller must guarantee the
    /// slot is not currently in the LRU.
    unsafe fn lru_push_front(&mut self, slot: NonNull<ShadowSlot>) {
        let s = slot.as_ptr();
        (*s).lru_prev = None;
        (*s).lru_next = self.lru_head;
        if let Some(old_head) = self.lru_head {
            (*old_head.as_ptr()).lru_prev = Some(slot);
        } else {
            // Empty list → tail also points at the new slot.
            self.lru_tail = Some(slot);
        }
        self.lru_head = Some(slot);
    }

    /// Remove a slot from the LRU. Caller must guarantee the slot
    /// is currently in the LRU.
    unsafe fn lru_remove(&mut self, slot: NonNull<ShadowSlot>) {
        let s = slot.as_ptr();
        let prev = (*s).lru_prev;
        let next = (*s).lru_next;
        match prev {
            Some(p) => (*p.as_ptr()).lru_next = next,
            None => self.lru_head = next,
        }
        match next {
            Some(n) => (*n.as_ptr()).lru_prev = prev,
            None => self.lru_tail = prev,
        }
        (*s).lru_prev = None;
        (*s).lru_next = None;
    }
}

impl<R: Region> Drop for ShadowCache<R> {
    fn drop(&mut self) {
        // Walk lookup, free every shadow slot. Cold entries are
        // just HashMap pairs — drop falls out naturally.
        let keys: Vec<c_uint> = self.lookup.keys().copied().collect();
        for k in keys {
            if let Some(LookupEntry::Shadow(slot_ptr)) = self.lookup.remove(&k) {
                // SAFETY: slot_ptr came from `fetch`; nothing else
                // holds a ref now that Drop is running.
                unsafe { ShadowSlot::dealloc(slot_ptr.as_ptr()) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the shadow-pool + LRU + flush logic against
    //! the in-process region. Exercise the Path D invariants
    //! directly without going through SQLite.
    //!
    //! End-to-end tests with real SQLite live in
    //! `tests/serves_real_sqlite.rs`; those use the same
    //! `ShadowCache<InProcRegion>` and cover the integration with
    //! SQLite's pcache2 callback contract.

    use super::*;
    use crate::region::InProcRegion;

    /// Convenience: build a cache + immediately fetch a page (with
    /// create=1) so test bodies don't have to repeat the boilerplate.
    fn cache_with_capacity(cap: u32) -> ShadowCache<InProcRegion> {
        let mut c = ShadowCache::new(64, 8, true, InProcRegion::new());
        c.set_cachesize(cap as c_int);
        c
    }

    #[test]
    fn fetch_then_unpin_keeps_page_in_shadow_until_evicted() {
        let mut c = cache_with_capacity(4);
        let page = c.fetch(1, 1);
        assert!(!page.is_null(), "fresh fetch with create=1 should allocate");
        // Write recognisable bytes; the region should NOT have
        // them yet because we haven't unpinned.
        unsafe {
            std::ptr::write_bytes((*page).pBuf as *mut u8, 0xab, 64);
        }
        unsafe { c.unpin(page, false) };
        // After unpin the slot stays in shadow (we haven't fetched
        // a new page that would force eviction).
        assert_eq!(c.page_count(), 1);

        // Re-fetching the same key should hit shadow (no region
        // round-trip).
        let page2 = c.fetch(1, 0); // create=0 — only return if cached
        assert!(!page2.is_null(), "re-fetch of unpinned shadow page should hit");
        unsafe {
            let bytes = std::slice::from_raw_parts((*page2).pBuf as *const u8, 64);
            assert!(bytes.iter().all(|b| *b == 0xab), "shadow bytes preserved");
            c.unpin(page2, false);
        }
    }

    #[test]
    fn shadow_overflow_evicts_lru_tail_to_region() {
        let mut c = cache_with_capacity(3);
        // Fill the shadow pool: pages 1..=3, all unpinned.
        let mut last = ptr::null_mut();
        for key in 1..=3 {
            let p = c.fetch(key, 1);
            assert!(!p.is_null());
            unsafe {
                std::ptr::write_bytes((*p).pBuf as *mut u8, key as u8, 64);
                c.unpin(p, false);
            }
            last = p;
        }
        let _ = last;
        // Fetch a 4th page — forces eviction of LRU tail (page 1).
        let p4 = c.fetch(4, 1);
        assert!(!p4.is_null());
        unsafe {
            std::ptr::write_bytes((*p4).pBuf as *mut u8, 4, 64);
            c.unpin(p4, false);
        }
        // Re-fetch page 1 with create=0: should return non-null
        // because the lookup still knows about it (it's now cold).
        // The bytes must be 0xab (== key 1) — proving the eviction
        // flushed to the region and the promotion pulled them back.
        let p1_again = c.fetch(1, 0);
        assert!(
            !p1_again.is_null(),
            "cold-tier page 1 should promote on create=0 fetch"
        );
        unsafe {
            let bytes = std::slice::from_raw_parts((*p1_again).pBuf as *const u8, 64);
            assert!(
                bytes.iter().all(|b| *b == 1),
                "promoted bytes should match what we wrote before eviction"
            );
            c.unpin(p1_again, false);
        }
    }

    #[test]
    fn lru_promote_on_hit_protects_recent_pages() {
        let mut c = cache_with_capacity(3);
        // Populate three pages in order 1, 2, 3 — LRU tail is 1.
        for key in 1..=3 {
            let p = c.fetch(key, 1);
            unsafe {
                std::ptr::write_bytes((*p).pBuf as *mut u8, key as u8, 64);
                c.unpin(p, false);
            }
        }
        // Touch page 1 (fetch+unpin) — promotes it to MRU. Tail
        // is now page 2.
        let p1 = c.fetch(1, 0);
        unsafe { c.unpin(p1, false) };
        // Fourth fetch forces eviction. Expectation: page 2 is the
        // victim, page 1 stays in shadow.
        let p4 = c.fetch(4, 1);
        unsafe {
            std::ptr::write_bytes((*p4).pBuf as *mut u8, 4, 64);
            c.unpin(p4, false);
        }
        // Probe shadow vs cold by checking LookupEntry shape.
        match c.lookup.get(&1).expect("page 1 still tracked") {
            LookupEntry::Shadow(_) => {}
            LookupEntry::Cold => panic!("page 1 should be in shadow (LRU promoted it)"),
        }
        match c.lookup.get(&2).expect("page 2 still tracked") {
            LookupEntry::Cold => {}
            LookupEntry::Shadow(_) => panic!("page 2 should have been evicted"),
        }
    }

    #[test]
    fn discard_drops_page_entirely() {
        let mut c = cache_with_capacity(2);
        let p = c.fetch(1, 1);
        unsafe {
            std::ptr::write_bytes((*p).pBuf as *mut u8, 0xab, 64);
            c.unpin(p, true /* discard */);
        }
        // Re-fetch with create=0: should miss (no shadow, no cold).
        let probe = c.fetch(1, 0);
        assert!(probe.is_null(), "discarded page should not resurface");
    }

    #[test]
    fn truncate_drops_pages_above_limit() {
        let mut c = cache_with_capacity(8);
        for key in 1..=5 {
            let p = c.fetch(key, 1);
            unsafe { c.unpin(p, false) };
        }
        c.truncate(3); // keep pages 1, 2; drop 3, 4, 5
        assert_eq!(c.page_count(), 2);
        assert!(c.lookup.contains_key(&1));
        assert!(c.lookup.contains_key(&2));
        assert!(!c.lookup.contains_key(&3));
    }

    #[test]
    fn shrinking_xcachesize_evicts_to_fit() {
        let mut c = cache_with_capacity(5);
        for key in 1..=5 {
            let p = c.fetch(key, 1);
            unsafe { c.unpin(p, false) };
        }
        assert_eq!(c.shadow_count, 5);
        // Shrink to 2 — should evict 3 pages.
        c.set_cachesize(2);
        assert_eq!(c.shadow_count, 2);
        // 5 entries still tracked overall (evicted pages went to
        // cold, not gone).
        assert_eq!(c.page_count(), 5);
    }
}
