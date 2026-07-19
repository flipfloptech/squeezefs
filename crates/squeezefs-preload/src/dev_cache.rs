//! The process-wide **negative `st_dev` cache** (§5.4): remembers
//! filesystems already classified as *not SqueezeFS*, so every
//! subsequent `open*` on them costs one probe instead of an
//! `fstatfs`/xattr bootstrap attempt. Opens are control-plane; this
//! exists to keep the shim's per-open tax at ~100–200 ns on foreign
//! filesystems.
//!
//! Fixed-size, lock-free, set-associative-by-hash with plain overwrite
//! eviction: a lost entry costs one re-classification, never
//! correctness. `st_dev` 0 is a legal value (virtual filesystems), so
//! slots store `dev + 1` and reserve raw 0 for "empty".

use std::sync::atomic::{AtomicU64, Ordering};

/// Slot count. 1024 × 8 B = 8 KiB — larger than any realistic set of
/// distinct mounted filesystems a process touches.
const SLOTS: usize = 1024;
/// Ways probed per dev (tiny linear probe window keeps collisions from
/// thrashing one slot while staying O(1)).
const WAYS: usize = 4;

pub struct NegativeDevCache {
    slots: Box<[AtomicU64; SLOTS]>,
}

impl Default for NegativeDevCache {
    fn default() -> Self {
        Self::new()
    }
}

impl NegativeDevCache {
    pub fn new() -> Self {
        let v: Vec<AtomicU64> = (0..SLOTS).map(|_| AtomicU64::new(0)).collect();
        let slots: Box<[AtomicU64; SLOTS]> = v
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!("built with SLOTS entries"));
        Self { slots }
    }

    #[inline]
    fn home(dev: u64) -> usize {
        // splitmix64 finalizer — cheap, well-distributed for small keys.
        let mut x = dev.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (x ^ (x >> 31)) as usize % SLOTS
    }

    /// Is `dev` a known non-SqueezeFS filesystem? A miss only means
    /// "re-classify" — never a correctness signal.
    pub fn contains(&self, dev: u64) -> bool {
        let tagged = dev.wrapping_add(1);
        let home = Self::home(dev);
        (0..WAYS).any(|i| self.slots[(home + i) % SLOTS].load(Ordering::Relaxed) == tagged)
    }

    /// Remember `dev` as non-SqueezeFS. Prefers an empty way; otherwise
    /// overwrites the home slot (eviction = a future re-classification).
    pub fn insert(&self, dev: u64) {
        let tagged = dev.wrapping_add(1);
        let home = Self::home(dev);
        for i in 0..WAYS {
            let slot = &self.slots[(home + i) % SLOTS];
            let cur = slot.load(Ordering::Relaxed);
            if cur == tagged {
                return; // already present
            }
            if cur == 0
                && slot
                    .compare_exchange(0, tagged, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                return;
            }
        }
        self.slots[home].store(tagged, Ordering::Relaxed);
    }
}
