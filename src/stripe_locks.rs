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
/// 3.5. `INODE_META_LOCKS` (routing, per-inode `Mutex`) — the striped
///    block-map merge domain (`DataRouter::merge_block_mappings`, §5.3 one
///    merge discipline)
/// 4. MetaLV metadata-transaction locks, acquired in this sub-order:
///    - a. DLM `I{ino}` / `D{parent:name}` (per-object; MetaLV `DlmLockManager`)
///    - b. **format v2**: dentry bucket lock (`dentry_bucket_locks`) — in-RAM
///      dentry-chain integrity. **Format v3** (design-cow-kv-metadata §4.9 4b):
///      per-node write locks — the commit path takes **leaf locks only**, in
///      ascending NodeId order, deduped, lock-then-revalidate-then-retry
///      against SMOs; interior-node locks belong exclusively to the
///      serialized per-volume checkpoint/SMO task (parent-then-child), which
///      is what keeps the two lock populations acyclic. Node locks are
///      **never held across device I/O** (commit apply is RAM-only; the
///      journal entry write happens after unlock; writeback freezes under
///      the lock and appends outside it; SMOs reserve in-window and write
///      after release) **and never held while waiting on ring space**
///      (ring admission happens before any node lock — §4.4 pt 5; the
///      checkpoint task's own admissions never park, they drain-and-retry).
///
///      **PR M7 (metadata-throughput §5.5 D5) — the commit conveyor
///      refinement**: the leaf-lock TAKER population shrinks to exactly
///      {the per-volume conveyor **pass task**, the checkpoint/SMO task}
///      — user committers no longer take node locks at all; they enqueue
///      `{records, Arc<[DlmGuard]>, oneshot}` and park on the fan-out.
///      The pass takes the batch's UNION leaf set under the same 4b
///      discipline (ascending, deduped, whole-set drop-all-and-relock on
///      SMO revalidation failure) and **holds — but never acquires — DLM
///      guards** (level 4a): each queue entry co-owns its transaction's
///      I/D guard set until that tx's terminal outcome (post-ack /
///      post-rollback), so guard holders never wait on anything the
///      checkpoint drain needs (it takes no DLM locks, ever) and the
///      §4.4 pt 5 / R10 acyclicity argument transfers with the taker
///      population renamed. Batch ring admission still precedes every
///      node lock (one Σ-admission per batch).
///    - c. **format v2**: sector locks (`sector_locks`) — commit-time
///      RMW+apply, acquired in **ascending sector-offset order** (total order
///      ⇒ deadlock-free) and only at commit, never taken while holding a DLM
///      lock across the tx closure. **Format v3**: the journal reservation —
///      a wait-free atomic, not a lock; ordered inside 4b by protocol, it
///      imposes no ordering edges (§4.9 4c)
///
/// The extended order `active_inode_locks (1) → BLOCK_FLUSH_LOCKS (3) →
/// INODE_META_LOCKS → meta backend (4)` is established: `write_file_staged`'s
/// per-block future calls `fetch_metadata` under the block guard, which takes
/// `INODE_META_LOCKS` on refill, and `upload_full_block` holds the block lock
/// across `merge_block_mappings`. No existing or new path acquires
/// `BLOCK_FLUSH_LOCKS` or `active_inode_locks` while holding
/// `INODE_META_LOCKS` (`merge_block_mappings` is forbidden from doing so by
/// contract), so the extended order is acyclic.
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
        &self.locks[self.block_shard_index(ino, key)]
    }

    /// The shard index `get_lock(ino, key)` maps to — the same splitmix64
    /// mix, exposed for the RW1 stripe-collision audit
    /// (docs/design-random-small-writes.md §5.3 H2b): attributing a wait to
    /// cross-key stripe collision vs same-key contention requires naming the
    /// ACTUAL lock instance two distinct `(ino, key)` pairs can share.
    #[inline]
    pub fn block_shard_index(&self, ino: u64, key: u32) -> usize {
        let mut x = ino ^ ((key as u64) << 32);
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58476d1ce4e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        (x as usize) % N
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
