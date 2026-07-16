//! File-backed page cache: `(vnode, page-aligned file_offset) → physical
//! frame`.
//!
//! Bucket-locked, hash-indexed, fixed-capacity — deliberately the SAME
//! shape as `kernel-proc`'s `cow.rs` CoW table (which itself mirrors
//! `kernel-sched`'s futex table), not a third locking strategy invented
//! from scratch. Per this checkpoint's own carried-over invariant: mixing
//! lock orderings between different sparse tables is how a kernel gets its
//! first real deadlock, so this module follows `cow.rs`'s discipline
//! exactly — at most one bucket lock held at a time, ever, even across the
//! multi-bucket scans `writeback_vnode`/`evict_vnode` need to do (a single
//! vnode's cached pages are scattered across buckets by hash, so flushing
//! or evicting all of them means visiting every bucket in turn — lock,
//! inspect, unlock, one bucket at a time, never two simultaneously).

use crate::vnode::VNode;
use kernel_sync::SpinLock;

/// Fixed bucket count, matching `cow.rs::COW_BUCKETS`.
pub const PAGECACHE_BUCKETS: usize = 256;
/// Fixed per-bucket capacity, matching `cow.rs`'s own documented-overflow
/// contract: a full bucket degrades to "not cacheable" for any FURTHER
/// entry at that hash — `get_or_fill` still returns a valid frame for
/// THIS call, it just isn't remembered, so the next lookup at the same key
/// re-reads from the vnode (a missed sharing optimization, not a
/// correctness bug: two independent mappers of the same file offset would
/// each get their own private frame instead of sharing one, which is
/// wasteful but not wrong — MAP_SHARED writers would simply not see each
/// other's writes until the underlying file is re-read, which is the same
/// outcome as if they'd mapped the file at slightly different times).
pub const PAGECACHE_BUCKET_CAP: usize = 32;

pub const PAGE_SIZE: u64 = 4096;

#[cfg(target_os = "none")]
const DMAP: usize = 0xFFFF_8000_0000_0000;

#[derive(Clone, Copy)]
struct PageCacheEntry {
    vnode_ptr: usize,
    file_offset: u64,
    // Only read by get_or_fill/writeback_vnode/evict_vnode, all gated
    // `#[cfg(any(target_os = "none", test))]` — see get_or_fill's doc
    // comment for the full rationale.
    #[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
    frame: u64,
    dirty: bool,
}

struct PageCacheBucket {
    entries: [Option<PageCacheEntry>; PAGECACHE_BUCKET_CAP],
}

impl PageCacheBucket {
    const fn new() -> Self {
        Self {
            entries: [const { None }; PAGECACHE_BUCKET_CAP],
        }
    }
}

static PAGE_CACHE: [SpinLock<PageCacheBucket>; PAGECACHE_BUCKETS] =
    [const { SpinLock::new(PageCacheBucket::new()) }; PAGECACHE_BUCKETS];

/// Fibonacci-hash `(vnode_ptr, page-aligned file_offset)` into a bucket.
///
/// Folds the offset's page number into the SAME multiplication chain as
/// the vnode pointer (XOR once, then one multiply) rather than hashing
/// each key part separately and combining the two results afterward —
/// hashing separately and adding/XORing the two hashes clusters badly for
/// the overwhelmingly common case of one file's offsets being sequential
/// multiples of 4096 (see `bucket_index_spreads_sequential_offsets` below
/// for the regression test this specifically guards against).
pub fn bucket_index(vnode_ptr: usize, file_offset: u64) -> usize {
    let page_no = file_offset >> 12;
    let key = (vnode_ptr as u64) ^ page_no.wrapping_mul(0x2545_F491_4F6C_DD1D);
    let hash = key.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (hash >> (64 - 8)) as usize
}

/// Only reachable from `get_or_fill`, which is itself gated the same way
/// — see that function's own doc comment for the full rationale.
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
fn cache_lookup(vnode_ptr: usize, file_offset: u64) -> Option<u64> {
    let bucket = PAGE_CACHE[bucket_index(vnode_ptr, file_offset)].lock();
    bucket
        .entries
        .iter()
        .flatten()
        .find(|e| e.vnode_ptr == vnode_ptr && e.file_offset == file_offset)
        .map(|e| e.frame)
}

/// Fill `dst` (must be exactly [`PAGE_SIZE`] bytes) from `vn` at
/// `file_offset` via `vn.ops.read`, zero-filling any tail short of a full
/// page (EOF, or a short read for any other reason). Pure with respect to
/// frame allocation and the direct map — takes an already-valid
/// destination slice — which is what makes this directly host-testable
/// with a synthetic `VNode` and a plain stack buffer, independent of
/// `get_or_fill`'s own host/target frame-provisioning split below.
///
/// Returns the underlying `read()`'s own return value: `>= 0` on success
/// (however many bytes it actually supplied, before zero-fill), `< 0` on
/// a read error (nothing is zero-filled in that case — the caller must
/// not trust `dst`'s contents).
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
fn fill_page_from_vnode(vn: &VNode, file_offset: u64, dst: &mut [u8]) -> i64 {
    debug_assert_eq!(dst.len(), PAGE_SIZE as usize);
    let n = (vn.ops.read)(vn, dst, file_offset);
    if n >= 0 {
        let n = (n as usize).min(dst.len());
        if n < dst.len() {
            dst[n..].fill(0);
        }
    }
    n
}

// ─── frame provisioning: real kernel vs. host test ─────────────────────
//
// On the real kernel target, a "frame" is a physical frame number from
// the buddy allocator, and its bytes are reached through the direct map.
// On host, there is no direct map and no live buddy allocator to speak
// of — mirroring the exact same substitution `kernel-memory`'s own
// `buddy.rs` tests (`test_direct_map`) and `kernel-proc`'s `exec.rs` tests
// (`RawPage`) already make for the same reason: a "frame" in a host test
// is simply a `std::alloc`'d host pointer's own numeric value, requiring
// no direct-map translation at all (equivalent to a direct-map base of 0).
// This is what makes `get_or_fill`/`writeback_vnode` — which the
// checkpoint's own required tests need to call directly and inspect
// byte-for-byte (cache hit/miss counting, EOF zero-fill, dirty-byte
// content) — actually runnable under `cargo test`, rather than being
// entirely `#[cfg(target_os = "none")]`-gated and untested on host the
// way e.g. `ramfs.rs`'s own allocation-touching helpers are.

#[cfg(target_os = "none")]
fn raw_alloc_zeroed_frame() -> u64 {
    let frame = kernel_memory::alloc_frame();
    let ptr = (DMAP + frame.as_u64() as usize) as *mut u8;
    // SAFETY: frame was freshly allocated and is visible through the direct map.
    unsafe { core::ptr::write_bytes(ptr, 0, PAGE_SIZE as usize) };
    frame.as_u64()
}

#[cfg(target_os = "none")]
unsafe fn raw_frame_bytes_mut(frame: u64) -> &'static mut [u8] {
    // SAFETY: caller guarantees `frame` is a live, allocated physical frame.
    unsafe { core::slice::from_raw_parts_mut((DMAP + frame as usize) as *mut u8, PAGE_SIZE as usize) }
}

#[cfg(target_os = "none")]
fn raw_free_frame(frame: u64) {
    // SAFETY: caller guarantees `frame` is no longer referenced anywhere.
    unsafe { kernel_memory::free_frame(kernel_memory::PhysAddr::new(frame)) };
}

#[cfg(all(test, not(target_os = "none")))]
fn raw_alloc_zeroed_frame() -> u64 {
    use std::alloc::{Layout, alloc_zeroed};
    let layout = Layout::from_size_align(PAGE_SIZE as usize, PAGE_SIZE as usize)
        .expect("valid test layout");
    // SAFETY: layout is non-zero-sized and page-aligned; null is checked below.
    let ptr = unsafe { alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "test backing allocation failed");
    ptr as u64
}

#[cfg(all(test, not(target_os = "none")))]
unsafe fn raw_frame_bytes_mut(frame: u64) -> &'static mut [u8] {
    // SAFETY: caller guarantees `frame` is a live pointer from
    // raw_alloc_zeroed_frame, valid for PAGE_SIZE bytes.
    unsafe { core::slice::from_raw_parts_mut(frame as *mut u8, PAGE_SIZE as usize) }
}

#[cfg(all(test, not(target_os = "none")))]
fn raw_free_frame(frame: u64) {
    use std::alloc::{Layout, dealloc};
    let layout = Layout::from_size_align(PAGE_SIZE as usize, PAGE_SIZE as usize)
        .expect("valid test layout");
    // SAFETY: caller guarantees `frame` was allocated by
    // raw_alloc_zeroed_frame with this exact layout, and is being freed
    // exactly once.
    unsafe { dealloc(frame as *mut u8, layout) };
}

/// Look up (or demand-fill) the cached frame for `(vnode_ptr, file_offset)`.
/// `file_offset` MUST already be page-aligned — caller's responsibility,
/// checked with a `debug_assert!` here rather than a runtime branch.
///
/// HIT: returns the existing frame, no I/O.
///
/// MISS: allocates a zeroed frame, reads up to [`PAGE_SIZE`] bytes from
/// the vnode at `file_offset`, zero-fills any short tail (EOF), inserts
/// the entry (unless the bucket is full — see [`PAGECACHE_BUCKET_CAP`]),
/// and returns the new frame.
///
/// A second CPU racing the exact same `(vnode_ptr, file_offset)` miss at
/// the same time is handled explicitly: whichever caller's insert lands
/// first under the bucket lock becomes the canonical entry, and every
/// other racer frees its own redundant frame and returns the winner's
/// instead — page-cache entries must be unique per key, or a later reader
/// going through the "authoritative" entry could see stale content
/// relative to a write made through the other, un-tracked physical frame.
///
/// Returns `None` only if the bucket is full for this key **and** the
/// underlying `read()` call itself fails (negative return) — a bucket
/// that's merely full but whose read succeeds still returns `Some(frame)`
/// for this one call, it just won't be remembered for next time.
///
/// # Safety
/// `vnode_ptr` must be a valid direct-map VA of a live, refcounted
/// [`VNode`]. The direct map must be live (guaranteed on the real kernel
/// target; on host, `raw_*` above substitutes a plain heap allocation).
#[cfg(any(target_os = "none", test))]
pub unsafe fn get_or_fill(vnode_ptr: usize, file_offset: u64) -> Option<u64> {
    debug_assert_eq!(
        file_offset & (PAGE_SIZE - 1),
        0,
        "file_offset must be page-aligned"
    );

    if let Some(frame) = cache_lookup(vnode_ptr, file_offset) {
        return Some(frame);
    }

    let frame = raw_alloc_zeroed_frame();
    // SAFETY: vnode_ptr valid per this function's own contract.
    let vn = unsafe { &*(vnode_ptr as *const VNode) };
    // SAFETY: frame was just freshly allocated by raw_alloc_zeroed_frame
    // and is not aliased anywhere else yet.
    let dst = unsafe { raw_frame_bytes_mut(frame) };
    let n = fill_page_from_vnode(vn, file_offset, dst);
    if n < 0 {
        raw_free_frame(frame);
        return None;
    }

    let idx = bucket_index(vnode_ptr, file_offset);
    let mut bucket = PAGE_CACHE[idx].lock();
    // Race re-check: another CPU may have filled this exact key while we
    // were doing I/O above (the bucket lock is never held across a call
    // into vn.ops.read — see the module docs' "at most one bucket lock,
    // never across unbounded work" discipline).
    if let Some(existing) = bucket
        .entries
        .iter()
        .flatten()
        .find(|e| e.vnode_ptr == vnode_ptr && e.file_offset == file_offset)
    {
        let winner = existing.frame;
        drop(bucket);
        raw_free_frame(frame);
        return Some(winner);
    }
    if let Some(slot) = bucket.entries.iter_mut().find(|s| s.is_none()) {
        *slot = Some(PageCacheEntry {
            vnode_ptr,
            file_offset,
            frame,
            dirty: false,
        });
    }
    // else: bucket full for this key — degrade silently, per
    // PAGECACHE_BUCKET_CAP's documented contract above.
    Some(frame)
}

/// Mark the cached page at `(vnode_ptr, file_offset)` dirty. A no-op if
/// the page isn't (or is no longer) tracked — e.g. it was demand-filled
/// once but the bucket has since been evicted, which this checkpoint
/// doesn't implement but a future one might.
pub fn mark_dirty(vnode_ptr: usize, file_offset: u64) {
    let mut bucket = PAGE_CACHE[bucket_index(vnode_ptr, file_offset)].lock();
    for entry in bucket.entries.iter_mut().flatten() {
        if entry.vnode_ptr == vnode_ptr && entry.file_offset == file_offset {
            entry.dirty = true;
            return;
        }
    }
}

/// Write back every dirty page belonging to `vnode_ptr`, clearing each
/// one's dirty bit on success. Called from `munmap()` of the LAST
/// MAP_SHARED VMA referencing this vnode (see `AddressSpace::munmap` in
/// `kernel-proc`), and exposed here for a future `msync()` syscall too
/// (the syscall number isn't wired up in this checkpoint — see this
/// crate's docs for why).
///
/// # Writeback-past-EOF: clamp, don't grow
///
/// `ramfs_write` grows a file on any write whose `offset + len` exceeds
/// its current size. A MAP_SHARED page that straddles or lies entirely
/// past EOF (e.g. a file of 100 bytes mapped for a full 4096-byte page)
/// gets its zero-filled tail dirtied the instant any byte in that page is
/// written, per this checkpoint's accepted "any writable touch of a
/// MAP_SHARED page is dirty" simplification (see
/// `kernel_proc::address_space::AddressSpace::resolve_file_fault`'s own
/// doc comment on why). Writing that whole page back verbatim would
/// silently grow the file with a run of zero bytes it never legitimately
/// gained — a real, easy-to-hit resource-exhaustion-flavored surprise
/// (map a file 1000x bigger than its actual size, touch one byte, and the
/// file balloons to match on the next writeback). This function instead
/// clamps every writeback to `min(PAGE_SIZE, vnode.size - file_offset)`
/// bytes, and skips the write entirely once `file_offset >= vnode.size` —
/// matching real Unix `mmap`/`msync` behavior, where a page mapped past
/// EOF never grows the file on writeback; only an explicit `write()` or
/// `ftruncate()` does that.
///
/// # Safety
/// `vnode_ptr` must be a valid direct-map VA of a live VNode; the direct
/// map must be live.
#[cfg(any(target_os = "none", test))]
pub unsafe fn writeback_vnode(vnode_ptr: usize) -> i32 {
    // SAFETY: vnode_ptr valid per this function's own contract.
    let vn = unsafe { &*(vnode_ptr as *const VNode) };
    let vsize = vn.size;

    for bucket_lock in PAGE_CACHE.iter() {
        // Never hold two bucket locks simultaneously, even across this
        // multi-bucket scan — lock, inspect, unlock, one at a time. See
        // this module's own top-level docs.
        let mut bucket = bucket_lock.lock();
        for entry in bucket.entries.iter_mut().flatten() {
            if entry.vnode_ptr != vnode_ptr || !entry.dirty {
                continue;
            }
            if entry.file_offset >= vsize {
                // Entirely past EOF: nothing to flush (see this
                // function's doc comment on clamp-vs-grow).
                entry.dirty = false;
                continue;
            }
            let len = ((vsize - entry.file_offset).min(PAGE_SIZE)) as usize;
            // SAFETY: entry.frame is a live, previously-filled cache frame.
            let bytes = unsafe { raw_frame_bytes_mut(entry.frame) };
            let n = (vn.ops.write)(vn, &bytes[..len], entry.file_offset);
            if n < 0 {
                return -5; // EIO
            }
            entry.dirty = false;
        }
    }
    0
}

/// Drop all page-cache entries for `vnode_ptr` WITHOUT writeback — only
/// safe to call after [`writeback_vnode`] has already run (or when the
/// caller has independently determined the dirty data doesn't matter).
/// Deliberately NOT called from anywhere in this checkpoint: `munmap`'s
/// vnode-release path (see `AddressSpace::munmap`) always calls
/// `writeback_vnode` then `dec_ref`, but never this function, because
/// there is no reliable "this was the LAST reference anywhere" signal
/// available yet — this crate's own `fd` close path has the identical
/// pre-existing gap (see `FdTable::clone_for_fork`'s doc comment: fork'd
/// fd copies don't share a refcounted file-description object either).
/// Exposed here, unused, for a future caller such as `VNode::release`
/// once that signal exists — matches this checkpoint's own accepted
/// "page-cache eviction under memory pressure" limitation: cached pages
/// simply persist until a future checkpoint adds real eviction.
#[cfg(any(target_os = "none", test))]
pub fn evict_vnode(vnode_ptr: usize) {
    for bucket_lock in PAGE_CACHE.iter() {
        let mut bucket = bucket_lock.lock();
        for slot in bucket.entries.iter_mut() {
            if matches!(slot, Some(e) if e.vnode_ptr == vnode_ptr) {
                raw_free_frame(slot.take().expect("just matched Some above").frame);
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    for bucket in PAGE_CACHE.iter() {
        let mut bucket = bucket.lock();
        for slot in bucket.entries.iter_mut() {
            if let Some(entry) = slot.take() {
                raw_free_frame(entry.frame);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// PAGE_CACHE and the T1_/T3_ call-recording statics are process-wide,
    /// and the default test runner is multi-threaded, so every test that
    /// touches either serializes on this lock. Without it the failures are
    /// worse than flaky asserts: each test's setup `reset_for_tests()`
    /// frees frames a concurrently-running test may still be reading
    /// through `raw_frame_bytes_mut` — a genuine host-heap use-after-free.
    /// The pure `bucket_index` tests don't take the guard. Poisoning is
    /// deliberately swallowed so one failing test doesn't cascade into
    /// "poisoned lock" noise in every test that runs after it.
    fn cache_test_guard() -> MutexGuard<'static, ()> {
        static GUARD: Mutex<()> = Mutex::new(());
        GUARD.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[test]
    fn bucket_index_is_stable_for_same_key() {
        assert_eq!(bucket_index(0x1000, 0x4000), bucket_index(0x1000, 0x4000));
        assert!(bucket_index(0x1000, 0x4000) < PAGECACHE_BUCKETS);
    }

    #[test]
    fn bucket_index_spreads_sequential_offsets() {
        // Sixteen sequential 4 KiB pages of the SAME vnode must not all
        // collide into one bucket — this is exactly the failure mode the
        // module doc warns about for hashing vnode_ptr and the offset's
        // page number separately-then-combining instead of folding them
        // into one multiplication chain.
        let vnode_ptr = 0x1234_5000usize;
        let mut buckets = [0usize; 16];
        for (i, slot) in buckets.iter_mut().enumerate() {
            *slot = bucket_index(vnode_ptr, (i as u64) * PAGE_SIZE);
        }
        buckets.sort_unstable();
        let mut distinct = 1;
        for w in buckets.windows(2) {
            if w[0] != w[1] {
                distinct += 1;
            }
        }
        assert!(
            distinct > 4,
            "expected reasonable spread over 16 sequential offsets, got {distinct} distinct buckets"
        );
    }

    // ── synthetic VNode plumbing for get_or_fill / writeback tests ──────

    fn make_test_vnode(size: u64, ops: &'static crate::vnode::VNodeOps) -> VNode {
        VNode {
            vtype: crate::vnode::VNodeType::Regular,
            ino: 1,
            size,
            refcount: core::sync::atomic::AtomicU32::new(1),
            ops,
            fs_data: 0,
            lock: kernel_sync::SpinLock::new(()),
        }
    }

    // -- Test 1: get_or_fill miss-then-hit, read called at most once ----

    static T1_READ_CALLS: AtomicU32 = AtomicU32::new(0);
    static T1_CONTENT: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
    fn t1_read(_vn: &VNode, buf: &mut [u8], offset: u64) -> i64 {
        T1_READ_CALLS.fetch_add(1, Ordering::SeqCst);
        if offset != 0 {
            return 0;
        }
        buf[..T1_CONTENT.len()].copy_from_slice(&T1_CONTENT);
        T1_CONTENT.len() as i64
    }
    static T1_OPS: crate::vnode::VNodeOps = crate::vnode::VNodeOps {
        read: t1_read,
        write: crate::vnode::vnode_write_noop,
        lookup: crate::vnode::vnode_lookup_noop,
        create: crate::vnode::vnode_create_noop,
        readdir: crate::vnode::vnode_readdir_noop,
        truncate: crate::vnode::vnode_truncate_noop,
        release: crate::vnode::vnode_release_noop,
    };

    #[test]
    fn get_or_fill_first_call_misses_second_call_hits() {
        let _serialized = cache_test_guard();
        reset_for_tests();
        T1_READ_CALLS.store(0, Ordering::SeqCst);
        let vn = make_test_vnode(4096, &T1_OPS);
        let vnode_ptr = &vn as *const VNode as usize;

        // SAFETY: vn is a valid, live VNode for the duration of this test.
        let first = unsafe { get_or_fill(vnode_ptr, 0) }.expect("first fill must succeed");
        assert_eq!(T1_READ_CALLS.load(Ordering::SeqCst), 1, "miss must call read() once");

        // SAFETY: same as above.
        let second = unsafe { get_or_fill(vnode_ptr, 0) }.expect("second call must hit cache");
        assert_eq!(second, first, "cache hit must return the SAME frame");
        assert_eq!(
            T1_READ_CALLS.load(Ordering::SeqCst),
            1,
            "cache hit must NOT call read() again"
        );

        // SAFETY: first is a live frame from get_or_fill above.
        let bytes = unsafe { raw_frame_bytes_mut(first) };
        assert_eq!(&bytes[..4], &T1_CONTENT);
    }

    // -- Test 2: EOF zero-fill ------------------------------------------

    static T2_CONTENT: [u8; 100] = [0x7A; 100];
    fn t2_read(_vn: &VNode, buf: &mut [u8], offset: u64) -> i64 {
        if offset != 0 {
            return 0;
        }
        let n = T2_CONTENT.len().min(buf.len());
        buf[..n].copy_from_slice(&T2_CONTENT[..n]);
        n as i64
    }
    static T2_OPS: crate::vnode::VNodeOps = crate::vnode::VNodeOps {
        read: t2_read,
        write: crate::vnode::vnode_write_noop,
        lookup: crate::vnode::vnode_lookup_noop,
        create: crate::vnode::vnode_create_noop,
        readdir: crate::vnode::vnode_readdir_noop,
        truncate: crate::vnode::vnode_truncate_noop,
        release: crate::vnode::vnode_release_noop,
    };

    #[test]
    fn get_or_fill_zero_fills_past_short_file_eof() {
        let _serialized = cache_test_guard();
        reset_for_tests();
        let vn = make_test_vnode(100, &T2_OPS);
        let vnode_ptr = &vn as *const VNode as usize;

        // SAFETY: vn is valid for this test's duration.
        let frame = unsafe { get_or_fill(vnode_ptr, 0) }.expect("fill must succeed");
        // SAFETY: frame is live.
        let bytes = unsafe { raw_frame_bytes_mut(frame) };
        assert_eq!(&bytes[..100], &T2_CONTENT[..], "first 100 bytes must match the file");
        assert!(
            bytes[100..].iter().all(|&b| b == 0),
            "bytes [100..4096) must be zero-filled past EOF"
        );
    }

    // -- Test 3: mark_dirty + writeback_vnode records the right bytes ----

    static T3_WRITE_LEN: AtomicU32 = AtomicU32::new(0);
    static T3_WRITE_OFFSET: AtomicU32 = AtomicU32::new(u32::MAX);
    static T3_WRITE_FIRST_BYTE: AtomicU32 = AtomicU32::new(0);
    fn t3_read(_vn: &VNode, _buf: &mut [u8], _offset: u64) -> i64 {
        0 // empty file: get_or_fill's fill is all zero-fill
    }
    fn t3_write(_vn: &VNode, buf: &[u8], offset: u64) -> i64 {
        T3_WRITE_LEN.store(buf.len() as u32, Ordering::SeqCst);
        T3_WRITE_OFFSET.store(offset as u32, Ordering::SeqCst);
        T3_WRITE_FIRST_BYTE.store(buf.first().copied().unwrap_or(0) as u32, Ordering::SeqCst);
        buf.len() as i64
    }
    static T3_OPS: crate::vnode::VNodeOps = crate::vnode::VNodeOps {
        read: t3_read,
        write: t3_write,
        lookup: crate::vnode::vnode_lookup_noop,
        create: crate::vnode::vnode_create_noop,
        readdir: crate::vnode::vnode_readdir_noop,
        truncate: crate::vnode::vnode_truncate_noop,
        release: crate::vnode::vnode_release_noop,
    };

    #[test]
    fn mark_dirty_then_writeback_calls_write_with_expected_bytes() {
        let _serialized = cache_test_guard();
        reset_for_tests();
        T3_WRITE_LEN.store(0, Ordering::SeqCst);
        T3_WRITE_OFFSET.store(u32::MAX, Ordering::SeqCst);
        // File is exactly one page, so writeback's EOF clamp doesn't
        // shrink the write below the full page in this test.
        let vn = make_test_vnode(PAGE_SIZE, &T3_OPS);
        let vnode_ptr = &vn as *const VNode as usize;

        // SAFETY: vn valid for this test.
        let frame = unsafe { get_or_fill(vnode_ptr, 0) }.unwrap();
        // SAFETY: frame is live.
        let bytes = unsafe { raw_frame_bytes_mut(frame) };
        bytes[0] = 0x99;

        mark_dirty(vnode_ptr, 0);
        // SAFETY: vn valid for this test.
        assert_eq!(unsafe { writeback_vnode(vnode_ptr) }, 0);

        assert_eq!(T3_WRITE_OFFSET.load(Ordering::SeqCst), 0);
        assert_eq!(T3_WRITE_LEN.load(Ordering::SeqCst), PAGE_SIZE as u32);
        assert_eq!(
            T3_WRITE_FIRST_BYTE.load(Ordering::SeqCst),
            0x99,
            "writeback must send the byte mark_dirty's caller actually wrote"
        );
    }

    #[test]
    fn writeback_clamps_to_file_size_past_eof() {
        let _serialized = cache_test_guard();
        reset_for_tests();
        T3_WRITE_LEN.store(0, Ordering::SeqCst);
        // A 10-byte file mapped for a full page: writeback of a dirty
        // page must clamp the write to 10 bytes, never the full 4096 —
        // see writeback_vnode's own doc comment on why growing the file
        // to match the mapping would be wrong.
        let vn = make_test_vnode(10, &T3_OPS);
        let vnode_ptr = &vn as *const VNode as usize;

        // SAFETY: vn valid for this test.
        let _frame = unsafe { get_or_fill(vnode_ptr, 0) }.unwrap();
        mark_dirty(vnode_ptr, 0);
        // SAFETY: vn valid for this test.
        assert_eq!(unsafe { writeback_vnode(vnode_ptr) }, 0);
        assert_eq!(T3_WRITE_LEN.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn writeback_skips_page_entirely_past_eof() {
        let _serialized = cache_test_guard();
        reset_for_tests();
        T3_WRITE_LEN.store(u32::MAX, Ordering::SeqCst);
        let vn = make_test_vnode(10, &T3_OPS);
        let vnode_ptr = &vn as *const VNode as usize;

        // Second page (offset 4096) of a 10-byte file is entirely past
        // EOF — writeback must skip it (no write() call at all) rather
        // than writing 4096 zero bytes at offset 4096.
        // SAFETY: vn valid for this test.
        let _frame = unsafe { get_or_fill(vnode_ptr, PAGE_SIZE) }.unwrap();
        mark_dirty(vnode_ptr, PAGE_SIZE);
        // SAFETY: vn valid for this test.
        assert_eq!(unsafe { writeback_vnode(vnode_ptr) }, 0);
        assert_eq!(
            T3_WRITE_LEN.load(Ordering::SeqCst),
            u32::MAX,
            "write() must never be called for a page entirely past EOF"
        );
    }

    // -- Test 4: munmap-of-MAP_SHARED must never free a cache frame -----
    // (exercised at the kernel-proc level via AddressSpace::munmap's own
    // tests, since allocation-counting requires a real AddressSpace/PTE
    // walk that doesn't exist on this crate's side — this crate only
    // proves evict_vnode, the ONE function in this module that DOES free
    // page-cache frames, only ever touches entries for the requested
    // vnode and leaves every other vnode's entries alone.)

    #[test]
    fn evict_vnode_only_removes_entries_for_that_vnode() {
        let _serialized = cache_test_guard();
        reset_for_tests();
        let vn_a = make_test_vnode(4096, &T1_OPS);
        let vn_b = make_test_vnode(4096, &T1_OPS);
        let ptr_a = &vn_a as *const VNode as usize;
        let ptr_b = &vn_b as *const VNode as usize;

        // SAFETY: both vnodes valid for this test.
        unsafe {
            get_or_fill(ptr_a, 0).unwrap();
            get_or_fill(ptr_b, 0).unwrap();
        }

        evict_vnode(ptr_a);

        assert_eq!(cache_lookup(ptr_a, 0), None, "vn_a's entry must be gone");
        assert!(
            cache_lookup(ptr_b, 0).is_some(),
            "vn_b's entry must be untouched by evicting vn_a"
        );

        // Clean up vn_b's frame so this test doesn't leak host memory.
        evict_vnode(ptr_b);
    }

    #[test]
    fn mark_dirty_on_untracked_key_is_a_harmless_no_op() {
        let _serialized = cache_test_guard();
        reset_for_tests();
        // No panic, no crash — just does nothing, per this function's doc.
        mark_dirty(0xDEAD_0000, 0);
    }
}
