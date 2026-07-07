//! Lock-free in-RAM inode allocator (crate-facing wrapper).
//!
//! The atomic bitmap protocol lives in [`super::alloc_core`] — a
//! self-contained module that the `loom-models/` crate `#[path]`-includes
//! and exhaustively model-checks under `cfg(loom)` (`tests/run_loom.sh`).
//! This wrapper adds error mapping and the allocator-contention metrics.
//!
//! Inode occupancy is a bit per inode index in a `Box<[AtomicU64]>`; `alloc`
//! claims a free bit with a single `fetch_or` (the winner is whoever flips
//! 0→1), so concurrent creates cannot double-allocate without any lock. The
//! bitmap is treated as *derived* from the inode table
//! (`DiskInode::magic == 0x4E4F4445`) and is rebuilt on mount via
//! [`crate::meta_backend::storage::MetaLvStorage::seed_inode_alloc_from_table`].
//!
//! Inodes 0 and 1 are reserved (1 = root); allocatable range is `[2, limit)`
//! where `limit == min(20000, max_inodes)`.

use super::alloc_core::AllocCore;
use crate::error::{Result, SqueezefsError};
use std::sync::atomic::Ordering;

pub use super::alloc_core::FIRST_ALLOCATABLE_INO;

pub struct InodeAllocator {
    core: AllocCore,
}

impl InodeAllocator {
    /// Build an empty allocator for `[2, limit)`.
    pub fn new(limit: u64) -> Self {
        Self {
            core: AllocCore::new(limit),
        }
    }

    #[inline]
    pub fn limit(&self) -> u64 {
        self.core.limit()
    }

    /// Claim a free inode atomically. `Err` if the table is full. No lock.
    pub fn alloc(&self) -> Result<u64> {
        match self.core.alloc() {
            Ok(outcome) => {
                if outcome.lost_attempts > 0 {
                    // §Observability (PR 7): bits we raced past (contention or
                    // a stale hint). One amortized add, never per-iteration.
                    crate::fuse_client::METRICS
                        .meta_inode_alloc_cas_retries
                        .fetch_add(outcome.lost_attempts, Ordering::Relaxed);
                }
                Ok(outcome.ino)
            }
            Err(lost_attempts) => {
                if lost_attempts > 0 {
                    crate::fuse_client::METRICS
                        .meta_inode_alloc_cas_retries
                        .fetch_add(lost_attempts, Ordering::Relaxed);
                }
                Err(SqueezefsError::InvalidOperation(
                    "Inode table full".to_string(),
                ))
            }
        }
    }

    /// Release an inode. Idempotent; out-of-range inos are ignored.
    ///
    /// Correctness (design Key Decision 11): callers must invoke this only AFTER
    /// the destroy transaction durably commits, never inside the closure — there
    /// is no DLM exclusion between a destroy and a future create reusing the ino.
    pub fn free(&self, ino: u64) {
        self.core.free(ino)
    }

    /// Mark an inode as allocated (used by mount-time seeding). Out-of-range ignored.
    pub fn set(&self, ino: u64) {
        self.core.set(ino)
    }

    /// Whether `ino` is currently allocated.
    pub fn is_set(&self, ino: u64) -> bool {
        self.core.is_set(ino)
    }

    /// Number of allocated inodes in `[2, limit)` (popcount). Replaces the
    /// on-disk-bitmap-reading `get_allocated_inode_count`.
    pub fn allocated_count(&self) -> u64 {
        self.core.allocated_count()
    }
}

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
    /// The bounded-interleaving proof of the same property lives in
    /// `loom-models/` (`tests/run_loom.sh`).
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
