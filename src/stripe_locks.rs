//! Striped lock array shared across the FUSE and metadata layers.
//!
//! `StripeLocks<L, N>` is a fixed array of `N` locks selected by a splitmix64
//! hash of an inode (and optional sub-key). It is zero-allocation after
//! construction and lock-free to index, so it backs all of SqueezeFS's
//! per-inode / per-block / (future) per-sector serialization without a global
//! mutex or a growing map. Extracted from `fuse_client` so `meta_backend` can
//! depend on it without a module cycle.

/// # Lock order (P1-9) — always acquire in this order; never invert.
///
/// 1. `active_inode_locks` (per-inode `RwLock`, striped) — FUSE op serialization
/// 2. `lease_locks` (per-inode `Mutex`) — only while acquiring/refreshing a DLM lease
/// 3. `BLOCK_FLUSH_LOCKS` (per block) — active-block flush mutual exclusion
/// 4. MetaLV metadata-transaction locks, acquired in this sub-order:
///    - a. DLM `I{ino}` / `D{parent:name}` (per-object; MetaLV `DlmLockManager`)
///    - b. dentry bucket lock (`dentry_bucket_locks`) — in-RAM dentry-chain integrity
///    - c. sector locks (`sector_locks`) — commit-time RMW+apply, acquired in
///      **ascending sector-offset order** (total order ⇒ deadlock-free) and only
///      at commit, never taken while holding a DLM lock across the tx closure
///
/// Do not hold (1) write-guard across long backend I/O when a finer lock suffices
/// (see [`crate::fuse_client::InodeWriteLockScope`] / P1-8). Do not acquire (1)
/// while holding (3). The MetaLV backend is self-contained (no external
/// Redis/Garnet); never hold a pooled backend handle across durable NVMe/staging I/O.
pub struct StripeLocks<L, const N: usize> {
    locks: Vec<L>,
}

impl<L: Default, const N: usize> StripeLocks<L, N> {
    pub fn new() -> Self {
        let mut locks = Vec::with_capacity(N);
        for _ in 0..N {
            locks.push(L::default());
        }
        Self { locks }
    }

    #[inline]
    pub fn get_lock(&self, ino: u64, key: u32) -> &L {
        let mut x = ino ^ ((key as u64) << 32);
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58476d1ce4e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        &self.locks[(x as usize) % N]
    }

    #[inline]
    pub fn get_inode_lock(&self, ino: u64) -> &L {
        &self.locks[self.shard_index(ino)]
    }

    /// The shard index a key maps to. Two distinct keys may collide on one shard
    /// (the array is fixed-size). Callers that acquire *multiple* stripe locks in
    /// one critical section MUST deduplicate by this index and acquire in
    /// ascending index order — the fixed array means "ascending key" is **not** a
    /// valid total order over the actual lock instances, and re-locking a shared
    /// shard would self-deadlock the non-reentrant lock.
    #[inline]
    pub fn shard_index(&self, ino: u64) -> usize {
        let mut x = ino;
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58476d1ce4e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        (x as usize) % N
    }

    /// The lock at a given shard index (see [`Self::shard_index`]).
    #[inline]
    pub fn get_by_index(&self, index: usize) -> &L {
        &self.locks[index % N]
    }

    pub fn remove(&self, _ino: &u64) {
        // No-op for static array locks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same key must always resolve to the same lock instance (a stable
    /// per-inode critical section is the whole point).
    #[test]
    fn test_same_key_maps_to_same_lock() {
        let locks: StripeLocks<std::sync::Mutex<()>, 256> = StripeLocks::new();
        assert!(
            std::ptr::eq(locks.get_inode_lock(12345), locks.get_inode_lock(12345)),
            "same inode must map to the same stripe"
        );
        assert!(
            std::ptr::eq(locks.get_lock(7, 3), locks.get_lock(7, 3)),
            "same (ino,key) must map to the same stripe"
        );
    }

    /// The splitmix hash must spread inodes across shards (not collapse to one).
    #[test]
    fn test_distributes_across_shards() {
        let locks: StripeLocks<std::sync::Mutex<()>, 256> = StripeLocks::new();
        let base = locks.get_inode_lock(0) as *const _;
        let distinct = (1..1000u64)
            .filter(|&i| locks.get_inode_lock(i) as *const _ != base)
            .count();
        assert!(distinct > 0, "hash must spread inodes across shards");
    }

    /// The sub-key dimension participates in shard selection.
    #[test]
    fn test_sub_key_varies_shard() {
        let locks: StripeLocks<std::sync::Mutex<()>, 4096> = StripeLocks::new();
        let base = locks.get_lock(42, 0) as *const _;
        let distinct = (1..500u32)
            .filter(|&k| locks.get_lock(42, k) as *const _ != base)
            .count();
        assert!(distinct > 0, "sub-key must participate in shard selection");
    }
}
