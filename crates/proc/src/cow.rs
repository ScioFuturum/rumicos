use kernel_paging::PhysAddr;
use kernel_sync::SpinLock;

pub const COW_BUCKETS: usize = 256;
const COW_BUCKET_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CowEntry {
    frame: u64,
    refcount: u32,
}

struct CowBucket {
    entries: [Option<CowEntry>; COW_BUCKET_CAPACITY],
}

impl CowBucket {
    pub const fn new() -> Self {
        Self {
            entries: [const { None }; COW_BUCKET_CAPACITY],
        }
    }
}

static COW_TABLE: [SpinLock<CowBucket>; COW_BUCKETS] =
    [const { SpinLock::new(CowBucket::new()) }; COW_BUCKETS];

/// Fibonacci-hash the frame number into a bucket index.
///
/// Mirrors `kernel_sched::futex::bucket_index` bit-for-bit (multiplicative
/// hash by the same odd constant, top 8 bits of the 64-bit product) rather
/// than a plain mask of the low bits — kept consistent with that existing
/// design intentionally, not just "functionally equivalent", per the
/// project convention of mirroring proven bucket-table designs exactly.
pub fn bucket_index(frame: PhysAddr) -> usize {
    let key = frame.as_u64() >> 12; // physical frame number
    let hash = key.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (hash >> (64 - 8)) as usize
}

pub fn cow_share(frame: PhysAddr) {
    let mut bucket = COW_TABLE[bucket_index(frame)].lock();
    let frame = frame.as_u64();
    let mut free_slot = None;
    for (idx, slot) in bucket.entries.iter_mut().enumerate() {
        match slot {
            Some(entry) if entry.frame == frame => {
                entry.refcount = entry.refcount.saturating_add(1);
                return;
            }
            None if free_slot.is_none() => free_slot = Some(idx),
            _ => {}
        }
    }

    if let Some(idx) = free_slot {
        bucket.entries[idx] = Some(CowEntry { frame, refcount: 2 });
    }
}

pub fn cow_refcount(frame: PhysAddr) -> u32 {
    let bucket = COW_TABLE[bucket_index(frame)].lock();
    let frame = frame.as_u64();
    for entry in bucket.entries.iter().flatten() {
        if entry.frame == frame {
            return entry.refcount;
        }
    }
    1
}

pub fn cow_unshare(frame: PhysAddr) {
    let mut bucket = COW_TABLE[bucket_index(frame)].lock();
    let frame = frame.as_u64();
    for slot in bucket.entries.iter_mut() {
        if let Some(entry) = slot
            && entry.frame == frame
        {
            if entry.refcount > 2 {
                entry.refcount -= 1;
            } else {
                *slot = None;
            }
            return;
        }
    }
}

/// Atomically decrement `frame`'s share count *and* report whether the
/// caller is now its sole remaining owner — in one bucket-lock critical
/// section, not two.
///
/// Returns `true` **only** if no tracked entry exists for `frame` at all —
/// meaning every peer that ever shared it has already independently
/// resolved (each of *their* `cow_release` calls already ran), so the
/// caller is provably the sole remaining claimant and may reuse `frame` in
/// place (or free it, from a teardown path) with no copy. Returns `false`
/// whenever an entry is found, *regardless of its refcount value* — even a
/// freshly-created refcount-2 entry means one OTHER real peer's claim is
/// still outstanding, so the caller must copy away (or, from a teardown
/// path, must not free the frame yet).
///
/// # Why the boundary is "entry exists ⇒ false", not "refcount > 2 ⇒ false"
///
/// An earlier version of this function treated `refcount == 2` as "this is
/// the last pair resolving, safe to reuse in place" and returned `true`
/// for it. That is backwards. `cow_share` brings a frame from absent (1,
/// implicit) to 2 in a *single* call representing **two** real claimants
/// (e.g. a fork's parent PTE and child PTE). The *first* of those two
/// claimants to actually resolve (via a write fault or a munmap/exit
/// teardown) sees refcount == 2 — and at that exact moment the *other*
/// claimant's PTE is still live, unresolved, and still pointing at the
/// same frame. Treating that first resolution as "sole owner" and letting
/// it reuse the frame in place (or free it, from a teardown path) is a
/// real bug with two distinct consequences depending on the caller:
///   - From `resolve_cow_fault_inner`: the first writer mutates the frame
///     in place while the second, unresolved peer's PTE — still present,
///     still read-only — transparently observes the mutation on its next
///     *read* (reads never fault against a present read-only page), which
///     is a CoW isolation violation between the two peers.
///   - From `unmap_pages`/`free_table`: the first peer to `munmap`/exit
///     frees a frame the second peer's PTE *still maps* — a dangling
///     page-table entry pointing at memory the buddy allocator may hand
///     out to something completely unrelated. This is the more serious of
///     the two: physical-memory corruption, not just an isolation leak.
///
/// The correct rule is that reuse/free-in-place is only safe once *every*
/// original claimant has resolved — i.e. once the table entry is gone
/// entirely. So: any call that finds an entry must decrement it (removing
/// it outright once it would drop to the implicit "1" state) and report
/// `false`; the *next* claimant's resolution, finding no entry at all,
/// correctly gets `true`. This generalizes to any starting refcount N: the
/// first N-1 resolutions each see an entry and get `false`; only the Nth
/// (and every resolution after a frame is no longer shared at all) sees no
/// entry and gets `true`.
///
/// This also makes it safe for `pagecache`-backed private (MAP_PRIVATE)
/// file mappings to fold the page cache's own permanent interest in a
/// frame into this same table (see `AddressSpace::resolve_file_fault` in
/// `kernel-proc`): the very first private mapper of a freshly cached page
/// calls `cow_share` once, which — thanks to `cow_share` jumping straight
/// from absent to 2 on its first call — represents *both* the page
/// cache's own never-decrementing interest and that first mapper's real
/// claim in one shot. Every subsequent private mapper of the same cached
/// page (whether from a fresh `mmap` + fault or from `fork` CoW-copying an
/// already-present private mapping) adds its own `cow_share` call. Because
/// an entry now *always* means "false, copy away" regardless of its
/// refcount, no real mapper can ever be told "you're the last one, reuse
/// the cache's frame in place" — which is exactly the property a page
/// cache's backing frame needs: it must never be mutated in place by a
/// private mapper, no matter how many real mappers have come and gone,
/// since the cache may still hand that same frame to a brand-new mapper
/// at any point in the future.
///
/// # Why the *count* being atomic still isn't the whole SMP story
///
/// This only protects the share-*count* bookkeeping. It does not by
/// itself make the surrounding page-table edit (the actual Writable/CoW
/// bit flip in the PTE) atomic with the count change. There remains a
/// narrower, harder-to-hit window where a parent re-forking an
/// already-shared page can race a sibling's concurrent CoW resolution of
/// that same physical frame and the two page tables can briefly disagree
/// about who holds the authoritative (writable) copy before the next
/// fault re-synchronizes them. Closing that fully would require holding
/// this same per-frame lock across the page-table mutation too (which
/// today lives in `kernel-paging`/`AddressSpace`, a different lock
/// domain) — flagged here as a follow-up, not solved in this checkpoint,
/// consistent with the other SMP TODOs already called out for fork/CoW.
pub fn cow_release(frame: PhysAddr) -> bool {
    let mut bucket = COW_TABLE[bucket_index(frame)].lock();
    let frame = frame.as_u64();
    for slot in bucket.entries.iter_mut() {
        if let Some(entry) = slot
            && entry.frame == frame
        {
            if entry.refcount > 2 {
                entry.refcount -= 1;
            } else {
                *slot = None;
            }
            // An entry existed, so at least one other real peer's claim
            // was (until this decrement) still outstanding — never sole.
            return false;
        }
    }
    // No entry at all: every original peer that ever shared this frame
    // has already independently resolved — sole remaining claimant.
    true
}

/// Like [`cow_release`], but runs `action(is_sole)` **while still holding
/// the frame's bucket lock**, so the release decision and whatever the
/// caller does with the frame (copy-away, reuse-in-place, or free) are
/// atomic with respect to every other claimant of the SAME frame.
///
/// This closes the SMP race described in `cow_release`'s own doc comment.
/// `cow_release` alone only made the *count* atomic: a caller that got
/// `false` and then ran `copy_frame(new, old)` could be reading `old` at the
/// exact moment another caller — who got `true` an instant later — made
/// `old` writable and its faulting instruction wrote into it, tearing the
/// copy. (Symmetrically, a teardown path could `free` `old` while a copier
/// was still reading it.) Holding the bucket lock across the whole action
/// serialises copy-vs-reuse and copy-vs-free per frame: the second claimant
/// blocks on the bucket lock until the first's copy/free has finished.
///
/// `action` runs under the bucket lock, so it may take *lower* locks (the
/// frame allocator) but must never take a lock that is ever held while
/// acquiring a COW bucket lock. Keep it short — it holds up other CoW
/// resolutions that hash to the same bucket.
pub fn cow_resolve<R>(frame: PhysAddr, action: impl FnOnce(bool) -> R) -> R {
    let mut bucket = COW_TABLE[bucket_index(frame)].lock();
    let key = frame.as_u64();
    let mut sole = true;
    for slot in bucket.entries.iter_mut() {
        if let Some(entry) = slot
            && entry.frame == key
        {
            if entry.refcount > 2 {
                entry.refcount -= 1;
            } else {
                *slot = None;
            }
            sole = false;
            break;
        }
    }
    // The bucket lock is still held for the duration of `action`.
    action(sole)
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    for bucket in &COW_TABLE {
        let mut bucket = bucket.lock();
        for slot in bucket.entries.iter_mut() {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_frame_has_refcount_one() {
        reset_for_tests();
        assert_eq!(cow_refcount(PhysAddr::new(0x1000)), 1);
    }

    #[test]
    fn first_share_sets_refcount_two() {
        reset_for_tests();
        let frame = PhysAddr::new(0x2000);
        cow_share(frame);
        assert_eq!(cow_refcount(frame), 2);
    }

    #[test]
    fn repeated_share_and_unshare_tracks_count() {
        reset_for_tests();
        let frame = PhysAddr::new(0x3000);
        cow_share(frame);
        cow_share(frame);
        assert_eq!(cow_refcount(frame), 3);
        cow_unshare(frame);
        assert_eq!(cow_refcount(frame), 2);
        cow_unshare(frame);
        assert_eq!(cow_refcount(frame), 1);
    }

    #[test]
    fn bucket_index_is_stable_for_same_frame() {
        let frame = PhysAddr::new(0x1234_5000);
        assert_eq!(bucket_index(frame), bucket_index(frame));
        assert!(bucket_index(frame) < COW_BUCKETS);
    }

    #[test]
    fn cow_release_on_an_existing_entry_always_reports_not_sole_regardless_of_refcount() {
        // The refcount==2 boundary is the one this function's own doc
        // comment specifically calls out as the bug an earlier version
        // had backwards: an entry existing at all — even a freshly
        // shared, exactly-two-owners entry — means one OTHER real peer's
        // claim is still outstanding, so this call must report `false`.
        reset_for_tests();
        let frame = PhysAddr::new(0xb000);
        cow_share(frame); // refcount 2 (two real owners: e.g. parent+child)
        assert!(
            !cow_release(frame),
            "an entry existing at refcount==2 still means one other real \
             peer's claim is outstanding — this must NOT report sole \
             ownership, or the caller could mutate/free a frame the other \
             peer's PTE still maps"
        );
        assert_eq!(
            cow_refcount(frame),
            1,
            "the decrement removes the entry outright (refcount==2 case), \
             leaving it in the implicit-1/absent state for the OTHER \
             peer's own eventual resolution"
        );
    }

    #[test]
    fn cow_release_sequential_two_peer_resolution_first_false_second_true() {
        // The scenario `cow_release`'s doc comment walks through directly:
        // exactly two real peers (e.g. a fork's parent PTE and child PTE)
        // sharing one frame. The FIRST of the two to resolve (in either
        // order — whichever happens first) must be told "not sole" (copy
        // away / don't free yet); only the SECOND, finding the entry
        // already gone, may be told "sole" (safe to reuse in place / free).
        reset_for_tests();
        let frame = PhysAddr::new(0xd000);
        cow_share(frame); // refcount 2: exactly two real peers

        let first = cow_release(frame);
        assert!(!first, "first of two peers to resolve must copy away");
        assert_eq!(cow_refcount(frame), 1, "entry gone after the first resolution");

        let second = cow_release(frame);
        assert!(
            second,
            "second (last) peer, finding no entry, must be told it's sole owner"
        );
        assert_eq!(cow_refcount(frame), 1, "still untracked — nothing left to remove");
    }

    #[test]
    fn cow_release_on_untracked_frame_reports_sole_owner() {
        reset_for_tests();
        let frame = PhysAddr::new(0x9000);
        assert!(cow_release(frame), "untracked frame must be its own sole owner");
        assert_eq!(cow_refcount(frame), 1);
    }

    #[test]
    fn cow_release_keeps_other_owners_when_more_than_two_remain() {
        reset_for_tests();
        let frame = PhysAddr::new(0xa000);
        cow_share(frame); // refcount 2
        cow_share(frame); // refcount 3
        assert!(
            !cow_release(frame),
            "with 3 owners, releasing one must NOT report sole ownership"
        );
        assert_eq!(cow_refcount(frame), 2);
    }

    #[test]
    fn cow_release_combined_decrement_matches_manual_decrement_sequence() {
        // Sanity check that cow_release's combined "peek + decrement"
        // semantics track a plain sequence of manual cow_unshare() calls
        // one-for-one, differing only in ALSO reporting (correctly, per
        // the doc comment above) whether an entry still existed at each
        // step — never claiming "sole" while any entry remains.
        reset_for_tests();
        let frame = PhysAddr::new(0xc000);
        cow_share(frame);
        cow_share(frame);
        cow_share(frame); // refcount 4 (four real peers)

        assert!(!cow_release(frame)); // 4 -> 3, entry still exists
        assert_eq!(cow_refcount(frame), 3);

        assert!(!cow_release(frame)); // 3 -> 2, entry still exists
        assert_eq!(cow_refcount(frame), 2);

        assert!(!cow_release(frame)); // 2 -> entry removed, still "not sole"
        assert_eq!(cow_refcount(frame), 1);

        assert!(cow_release(frame)); // no entry left: sole owner at last
        assert_eq!(cow_refcount(frame), 1);
    }
}
