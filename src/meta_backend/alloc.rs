//! Lock-free in-RAM inode allocator.
//!
//! Replaces the global-`inode_lock`-guarded on-disk bitmap RMW
//! (`MetaLvStorage::alloc_inode_bit_locked`) for the sector-sharded commit path.
//! Inode occupancy is a bit per inode index in a `Box<[AtomicU64]>`; `alloc`
//! claims a free bit with a single `fetch_or` (the winner is whoever flips 0→1),
//! so concurrent creates cannot double-allocate without any lock. The bitmap is
//! treated as *derived* from the inode table (`DiskInode::magic == 0x4E4F4445`)
//! and is rebuilt on mount via [`crate::meta_backend::storage::MetaLvStorage::seed_inode_alloc_from_table`].
//!
//! Inodes 0 and 1 are reserved (1 = root); allocatable range is `[2, limit)`
//! where `limit == min(20000, max_inodes)`.
//!
//! NOTE (PR 2 of the transaction_lock-removal design): this type is introduced
//! but NOT yet wired into `create`; the legacy `alloc_inode_bit_locked` remains
//! the live allocator until the sector-sharded commit PR flips the flag.

use crate::error::{Result, SqueezefsError};
use std::sync::atomic::{AtomicU64, Ordering};

/// First allocatable inode number (0 and 1 are reserved; 1 is the root inode).
pub const FIRST_ALLOCATABLE_INO: u64 = 2;

pub struct InodeAllocator {
    /// One bit per inode index; bit set == allocated.
    words: Box<[AtomicU64]>,
    /// Exclusive upper bound on allocatable inode numbers (`min(20000, max_inodes)`).
    limit: u64,
    /// Next-scan hint (monotonic-ish; reset downward on free).
    hint: AtomicU64,
}

impl InodeAllocator {
    /// Build an empty allocator for `[2, limit)`.
    pub fn new(limit: u64) -> Self {
        let n_words = limit.div_ceil(64) as usize;
        let words: Vec<AtomicU64> = (0..n_words).map(|_| AtomicU64::new(0)).collect();
        Self {
            words: words.into_boxed_slice(),
            limit,
            hint: AtomicU64::new(FIRST_ALLOCATABLE_INO),
        }
    }

    #[inline]
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Claim a free inode atomically. `Err` if the table is full. No lock.
    pub fn alloc(&self) -> Result<u64> {
        let start = self.hint.load(Ordering::Relaxed).max(FIRST_ALLOCATABLE_INO);
        // Scan [start, limit) then wrap [2, start): every allocatable index once.
        let wrap_end = start.min(self.limit);
        let mut lost_attempts: u64 = 0;
        for ino in (start..self.limit).chain(FIRST_ALLOCATABLE_INO..wrap_end) {
            let w = (ino / 64) as usize;
            let mask = 1u64 << (ino % 64);
            // fetch_or returns the previous word; if our bit was 0, we won the race.
            if self.words[w].fetch_or(mask, Ordering::AcqRel) & mask == 0 {
                self.hint.store(ino + 1, Ordering::Relaxed);
                if lost_attempts > 0 {
                    // §Observability (PR 7): bits we raced past (contention or a
                    // stale hint). One amortized add, never per-iteration.
                    crate::fuse_client::METRICS
                        .meta_inode_alloc_cas_retries
                        .fetch_add(lost_attempts, Ordering::Relaxed);
                }
                return Ok(ino);
            }
            lost_attempts += 1;
        }
        if lost_attempts > 0 {
            crate::fuse_client::METRICS
                .meta_inode_alloc_cas_retries
                .fetch_add(lost_attempts, Ordering::Relaxed);
        }
        Err(SqueezefsError::InvalidOperation(
            "Inode table full".to_string(),
        ))
    }

    /// Release an inode. Idempotent; out-of-range inos are ignored.
    ///
    /// Correctness (design Key Decision 11): callers must invoke this only AFTER
    /// the destroy transaction durably commits, never inside the closure — there
    /// is no DLM exclusion between a destroy and a future create reusing the ino.
    pub fn free(&self, ino: u64) {
        if ino < FIRST_ALLOCATABLE_INO || ino >= self.limit {
            return;
        }
        let w = (ino / 64) as usize;
        self.words[w].fetch_and(!(1u64 << (ino % 64)), Ordering::AcqRel);
        // Prefer reusing recently-freed slots under create/unlink churn.
        let _ = self.hint.fetch_min(ino, Ordering::Relaxed);
    }

    /// Mark an inode as allocated (used by mount-time seeding). Out-of-range ignored.
    pub fn set(&self, ino: u64) {
        if ino < FIRST_ALLOCATABLE_INO || ino >= self.limit {
            return;
        }
        let w = (ino / 64) as usize;
        self.words[w].fetch_or(1u64 << (ino % 64), Ordering::AcqRel);
    }

    /// Whether `ino` is currently allocated.
    pub fn is_set(&self, ino: u64) -> bool {
        if ino >= self.limit {
            return false;
        }
        let w = (ino / 64) as usize;
        self.words[w].load(Ordering::Acquire) & (1u64 << (ino % 64)) != 0
    }

    /// Number of allocated inodes in `[2, limit)` (popcount). Replaces the
    /// on-disk-bitmap-reading `get_allocated_inode_count`.
    pub fn allocated_count(&self) -> u64 {
        // Only indices < limit are ever set, and 0/1 are never allocated, so a
        // plain popcount over all words yields the [2, limit) occupancy.
        self.words
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as u64)
            .sum()
    }
}

// NOTE: exhaustive `loom` model-checking of the fetch_or/free interleavings is
// deferred — running `loom` needs an isolated test crate because a global
// `--cfg loom` poisons transitive deps (concurrent-queue, etc.). The
// high-contention real-thread stress tests below are the current no-double-alloc
// proof.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering as StdOrdering};
    use std::sync::Arc;

    #[test]
    fn test_alloc_unique_until_full_then_errors() {
        // limit 10 -> allocatable {2..10} = 8 inodes.
        let a = InodeAllocator::new(10);
        let mut seen = HashSet::new();
        for _ in 0..8 {
            let ino = a.alloc().expect("should allocate");
            assert!((2..10).contains(&ino), "ino {ino} out of allocatable range");
            assert!(seen.insert(ino), "duplicate ino {ino}");
        }
        assert!(a.alloc().is_err(), "table must be full after 8 allocs");
        assert_eq!(a.allocated_count(), 8);
    }

    #[test]
    fn test_free_makes_reusable() {
        let a = InodeAllocator::new(4); // allocatable {2,3}
        let x = a.alloc().unwrap();
        let y = a.alloc().unwrap();
        assert!(a.alloc().is_err());
        a.free(x);
        assert!(!a.is_set(x));
        let z = a.alloc().unwrap();
        assert_eq!(z, x, "freed slot must be reused (hint reset downward)");
        assert!(a.is_set(y));
    }

    #[test]
    fn test_reserved_and_out_of_range_are_noops() {
        let a = InodeAllocator::new(8);
        a.set(0);
        a.set(1);
        a.set(9999);
        assert_eq!(
            a.allocated_count(),
            0,
            "reserved/out-of-range must not set bits"
        );
        a.free(0);
        a.free(9999); // must not panic
    }

    /// High-contention hammer: many threads racing on a small range must never
    /// hand the same inode to two callers (the core `fetch_or` invariant #2).
    #[test]
    fn test_concurrent_alloc_no_double_allocation() {
        let limit = 4096u64; // allocatable {2..4096} = 4094
        let a = Arc::new(InodeAllocator::new(limit));
        let n_threads = 8;
        let mut handles = Vec::new();
        for _ in 0..n_threads {
            let a = a.clone();
            handles.push(std::thread::spawn(move || {
                let mut mine = Vec::new();
                while let Ok(ino) = a.alloc() {
                    mine.push(ino);
                }
                mine
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        let unique: HashSet<u64> = all.iter().copied().collect();
        assert_eq!(
            all.len(),
            unique.len(),
            "an inode was allocated to two threads"
        );
        assert_eq!(all.len() as u64, limit - FIRST_ALLOCATABLE_INO);
        assert_eq!(a.allocated_count(), limit - FIRST_ALLOCATABLE_INO);
    }

    /// Concurrent alloc + free churn must keep every live inode unique.
    #[test]
    fn test_concurrent_alloc_free_churn() {
        let a = Arc::new(InodeAllocator::new(512));
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let a = a.clone();
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || {
                let mut iters = 0u64;
                while !stop.load(StdOrdering::Relaxed) && iters < 50_000 {
                    if let Ok(ino) = a.alloc() {
                        assert!(a.is_set(ino));
                        a.free(ino);
                    }
                    iters += 1;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // After all churn completes, nothing should remain allocated.
        assert_eq!(
            a.allocated_count(),
            0,
            "leaked allocation after alloc/free churn"
        );
    }
}
