//! Pure lock-free inode-bitmap allocator core.
//!
//! Self-contained (no crate dependencies) so the `loom-models` crate can
//! `#[path]`-include this file and exhaustively model-check the fetch_or /
//! fetch_and interleavings under `cfg(loom)`. The crate-facing wrapper
//! ([`super::alloc::InodeAllocator`]) adds error mapping and metrics.
//!
//! Under `cfg(loom)` the atomics come from `loom::sync::atomic` (the main
//! build never sets that cfg; see `loom-models/`).

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// First allocatable inode number (0 and 1 are reserved; 1 is the root inode).
pub const FIRST_ALLOCATABLE_INO: u64 = 2;

/// Outcome of [`AllocCore::alloc`]: the claimed inode plus how many set bits
/// the scan raced past (contention / stale hint) for the caller's metrics.
pub struct AllocOutcome {
    pub ino: u64,
    pub lost_attempts: u64,
}

pub struct AllocCore {
    /// One bit per inode index; bit set == allocated.
    words: Box<[AtomicU64]>,
    /// Exclusive upper bound on allocatable inode numbers.
    limit: u64,
    /// Next-scan hint (monotonic-ish; reset downward on free).
    hint: AtomicU64,
}

impl AllocCore {
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

    /// Claim a free inode atomically; `None` if the table is full. Lock-free:
    /// `fetch_or` returns the previous word, so whoever flips a bit 0→1 is
    /// the unique winner for that inode.
    pub fn alloc(&self) -> Result<AllocOutcome, u64> {
        let start = self.hint.load(Ordering::Relaxed).max(FIRST_ALLOCATABLE_INO);
        // Scan [start, limit) then wrap [2, start): every allocatable index once.
        let wrap_end = start.min(self.limit);
        let mut lost_attempts: u64 = 0;
        for ino in (start..self.limit).chain(FIRST_ALLOCATABLE_INO..wrap_end) {
            let w = (ino / 64) as usize;
            let mask = 1u64 << (ino % 64);
            if self.words[w].fetch_or(mask, Ordering::AcqRel) & mask == 0 {
                self.hint.store(ino + 1, Ordering::Relaxed);
                return Ok(AllocOutcome { ino, lost_attempts });
            }
            lost_attempts += 1;
        }
        Err(lost_attempts)
    }

    /// Release an inode. Idempotent; out-of-range inos are ignored.
    ///
    /// Correctness (design Key Decision 11): callers must invoke this only
    /// AFTER the destroy transaction durably commits, never inside the
    /// closure — there is no DLM exclusion between a destroy and a future
    /// create reusing the ino.
    pub fn free(&self, ino: u64) {
        if ino < FIRST_ALLOCATABLE_INO || ino >= self.limit {
            return;
        }
        let w = (ino / 64) as usize;
        self.words[w].fetch_and(!(1u64 << (ino % 64)), Ordering::AcqRel);
        // Prefer reusing recently-freed slots under create/unlink churn.
        let _ = self.hint.fetch_min(ino, Ordering::Relaxed);
    }

    /// Mark an inode as allocated (mount-time seeding). Out-of-range ignored.
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

    /// Number of allocated inodes in `[2, limit)` (popcount).
    pub fn allocated_count(&self) -> u64 {
        self.words
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as u64)
            .sum()
    }
}
