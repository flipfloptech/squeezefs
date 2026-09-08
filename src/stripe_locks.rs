//! Striped lock array shared across the FUSE and metadata layers.
//!
//! `StripeLocks<L>` is a fixed array of `width` locks (a power of two, set
//! once at construction — D-3 made the width RUNTIME-sized so the 4a
//! tables can derive it from the transport's delivered concurrency)
//! selected by a splitmix64 hash of an inode (and optional sub-key). It is
//! zero-allocation after construction and lock-free to index (one mask,
//! never a division), so it backs all of SqueezeFS's per-inode /
//! per-block serialization without a global mutex or a growing map.
//! Extracted from `fuse_client` so `meta_backend` can depend on it
//! without a module cycle.
//!
//! # The stripe-collision census (D-3, e2e perf audit DLM board #4)
//!
//! A striped table maps many keys onto few locks, so a contended acquire
//! has two causes the wait histograms cannot separate: the stripe is held
//! by the SAME key (a true wait — the workload asked for it) or by a
//! DIFFERENT key that merely hashes alongside (a false-sharing wait — the
//! table's width did). On the 4a tables the guard is held across the
//! whole commit park (D5), so a false-sharing wait there costs a full
//! commit. [`StripeCensus`] splits the two per table from the stripe's
//! last-acquirer word — the RW1 H2b audit's shape, always-on: one relaxed
//! store per acquisition, one relaxed load + one counter bump on the
//! CONTENDED path only. The per-table counters ([`DLM_INODE_CENSUS`] …)
//! export flat on the stats inode as `<table>_stripe_collisions` /
//! `<table>_key_waits` beside `<table>_stripes`.

use std::sync::atomic::{AtomicU64, Ordering};

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
///
///      **D-2 (e2e audit DLM #2) — the two-stage conveyor**: the pass is
///      now the APPLY stage only (it drops its leaf locks before
///      SUBMITTING the batch's entries — the hold never spans the device
///      write, `lock_phase_ns.leaf_lock_hold` is the tripwire), and a
///      per-volume DURABILITY lane awaits writes / prefix / barrier and
///      fans out. The lane takes leaf locks in exactly one arm — the §4.4
///      pt 4 seq-conditional rollback of a FAILED write (`rollback_failed_
///      tx`: ascending, deduped, revalidated, released before its
///      compensation commit) — so the taker population is {apply pass,
///      lane rollback arm, checkpoint/SMO task}; no node-lock holder ever
///      waits on the lane, and the lane holds node locks only while
///      waiting on other node locks ascending. The full wait-for argument
///      is `src/meta_backend/kv/backend.rs`'s module doc ("The two-stage
///      commit conveyor").
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
pub struct StripeLocks<L> {
    locks: Box<[L]>,
    /// `width − 1`; the index is `hash & mask` (width is a power of two).
    mask: usize,
}

/// splitmix64 finalizer — the ONE mix every stripe index and census key
/// word derives from.
#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^= x >> 31;
    x
}

impl<L: Default> StripeLocks<L> {
    /// A table of `width` locks. `width` MUST be a power of two (the index
    /// is a mask): every shipped width is, and [`derived_stripe_width`]
    /// rounds up by construction — a violation is a programming error at
    /// construction, never a runtime path.
    pub fn new(width: usize) -> Self {
        assert!(
            width.is_power_of_two(),
            "StripeLocks width {width} is not a power of two"
        );
        let mut locks = Vec::with_capacity(width);
        for _ in 0..width {
            locks.push(L::default());
        }
        Self {
            locks: locks.into_boxed_slice(),
            mask: width - 1,
        }
    }

    /// The stripe population.
    #[inline]
    pub fn width(&self) -> usize {
        self.mask + 1
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
        (splitmix64(ino ^ ((key as u64) << 32)) as usize) & self.mask
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
        (splitmix64(ino) as usize) & self.mask
    }

    /// The lock at a given shard index (see [`Self::shard_index`]).
    #[inline]
    pub fn get_by_index(&self, index: usize) -> &L {
        &self.locks[index & self.mask]
    }
}

// ===========================================================================
// The stripe-collision census
// ===========================================================================

/// One table class's process-global census words. `collisions` = contended
/// acquires whose stripe was last taken by a DIFFERENT key (false sharing
/// — the width's cost); `key_waits` = contended acquires on the SAME key
/// (true contention — the workload's).
pub struct StripeCensusCounters {
    pub collisions: AtomicU64,
    pub key_waits: AtomicU64,
}

impl StripeCensusCounters {
    pub const fn new() -> Self {
        Self {
            collisions: AtomicU64::new(0),
            key_waits: AtomicU64::new(0),
        }
    }

    /// `(collisions, key_waits)`.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.collisions.load(Ordering::Relaxed),
            self.key_waits.load(Ordering::Relaxed),
        )
    }
}

impl Default for StripeCensusCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// 4a `DlmLockManager` inode class (`I{ino}`) — one table per volume, one
/// census class.
pub static DLM_INODE_CENSUS: StripeCensusCounters = StripeCensusCounters::new();
/// 4a `DlmLockManager` dentry class (`D{parent,name}`).
pub static DLM_DENTRY_CENSUS: StripeCensusCounters = StripeCensusCounters::new();
/// The S9 owner-side serve stripe (`meta_ship::publish::SERVE_INO_LOCKS`,
/// rung 17) — held across a served publish's read → compose → commit.
pub static SERVE_INO_CENSUS: StripeCensusCounters = StripeCensusCounters::new();
/// Order 3.5 `INODE_META_LOCKS` (the block-map merge domain).
pub static INODE_META_CENSUS: StripeCensusCounters = StripeCensusCounters::new();
/// Order 3 `BLOCK_FLUSH_LOCKS`.
pub static BLOCK_FLUSH_CENSUS: StripeCensusCounters = StripeCensusCounters::new();
/// The lease waiter stripes (`dlm::LOCK_WAITERS`): a "collision" here is a
/// SPURIOUS WAKE (the custody table is per key; the stripe only fans the
/// release notification out), never a wait.
pub static LEASE_WAITER_CENSUS: StripeCensusCounters = StripeCensusCounters::new();

/// The first-touch spin bound: the grant→stamp gap is a handful of
/// instructions, so a few hundred `spin_loop` iterations (well under a
/// microsecond) cover it unless the holder was preempted between the two
/// — then the census under-reports one collision rather than waiting.
const FIRST_TOUCH_SPINS: u32 = 256;

/// Per-table census state: the stripe's last-acquirer key word (never
/// cleared on release — "last acquirer" semantics, diagnostic-grade: a
/// stale word misclassifies one wait, never a correctness event) plus the
/// class's counters.
pub struct StripeCensus {
    holders: Box<[AtomicU64]>,
    counters: &'static StripeCensusCounters,
}

impl StripeCensus {
    pub fn new(width: usize, counters: &'static StripeCensusCounters) -> Self {
        Self {
            holders: (0..width).map(|_| AtomicU64::new(0)).collect(),
            counters,
        }
    }

    /// The CONTENDED arm (the try-acquire refused): classify against the
    /// stripe's last acquirer BEFORE parking. The stamp trails the grant
    /// by a few instructions, so a FIRST-TOUCH stripe can read 0 while its
    /// holder is between the two (a storm's synchronized start makes that
    /// common); a bounded spin absorbs it. A word still 0 after the spin
    /// classifies as a key wait — the census under-reports collisions,
    /// never over-reports them.
    #[inline]
    pub fn classify_contended(&self, stripe: usize, key: u64) {
        classify_against(&self.holders[stripe], u64::MAX, key, self.counters);
    }

    /// Every acquisition stamps its stripe (one relaxed store).
    #[inline]
    pub fn stamp(&self, stripe: usize, key: u64) {
        self.holders[stripe].store(key, Ordering::Relaxed);
    }
}

/// The classification itself over any last-holder `word` (the block
/// table's always-on holder word carries site bits — `key_mask` selects
/// the key bits; the census tables pass `u64::MAX`): `word & key_mask`
/// naming a different key is a collision, the same key (or a word still
/// 0 after the first-touch spin) a key wait.
#[inline]
pub fn classify_against(
    word: &AtomicU64,
    key_mask: u64,
    key: u64,
    counters: &StripeCensusCounters,
) {
    let mut holder = word.load(Ordering::Relaxed);
    let mut spins = 0u32;
    while holder == 0 && spins < FIRST_TOUCH_SPINS {
        std::hint::spin_loop();
        holder = word.load(Ordering::Relaxed);
        spins += 1;
    }
    if holder != 0 && (holder & key_mask) != key {
        counters.collisions.fetch_add(1, Ordering::Relaxed);
    } else {
        counters.key_waits.fetch_add(1, Ordering::Relaxed);
    }
}

/// Nonzero identity word for a `(a, b)` key: the table's own splitmix64
/// mix `| 1`, so 0 stays the never-acquired sentinel. Two distinct keys
/// colliding on one WORD is a ~2⁻⁶³ diagnostic misclassification.
#[inline]
pub fn key_word(a: u64, b: u64) -> u64 {
    splitmix64(a ^ b.rotate_left(32)) | 1
}

// ===========================================================================
// Width derivation (D-3)
// ===========================================================================

/// The load-factor target a derived table is sized to: `α = in-flight ÷
/// stripes` IS the false-sharing probability of one acquire against the
/// keys already held, and on the 4a tables a false-sharing wait costs a
/// whole commit (D5). 1/16 keeps that under one acquire in sixteen at the
/// transport's FULL delivered concurrency (`possible_cpus × q_depth`, the
/// most ops the ring can present); each stripe is one ~80 B `Arc<RwLock>`,
/// so the table stays 1.3 MiB per class at the 32 × 32 field geometry.
pub const STRIPE_LOAD_FACTOR_INV: usize = 16;

/// D-3's width law, pure (the tie-test form): `next_power_of_two(max(
/// shipped, possible_cpus × q_depth × STRIPE_LOAD_FACTOR_INV))`. `shipped`
/// is the never-regress-below floor (the `Q_DEPTH_FLOOR` house law — no
/// box gets a narrower table than every box already ran); the product is
/// the concurrency the FUSE-over-io_uring transport can present (one queue
/// per possible CPU, `q_depth` entries each), so the width scales with the
/// only thing that can widen the in-flight population.
pub fn derived_stripe_width(shipped: usize, possible_cpus: usize, q_depth: usize) -> usize {
    possible_cpus
        .max(1)
        .saturating_mul(q_depth.max(1))
        .saturating_mul(STRIPE_LOAD_FACTOR_INV)
        .max(shipped.max(1))
        .next_power_of_two()
}

/// `SQUEEZEFS_DLM_STRIPES` resolution, pure: an explicit value wins
/// verbatim (rounded UP to a power of two when it is not one — the index
/// is a mask; the registry admits `[1, 2^24]`, so `1` is the everything-
/// serializes crucible and `4096` the shipped-4a A/B control), else
/// [`derived_stripe_width`].
pub fn resolve_dlm_stripes(
    env: Option<usize>,
    shipped: usize,
    possible_cpus: usize,
    q_depth: usize,
) -> usize {
    match env {
        Some(n) => n.max(1).next_power_of_two(),
        None => derived_stripe_width(shipped, possible_cpus, q_depth),
    }
}

/// The env knob every DLM-class table's width honors.
pub const DLM_STRIPES_ENV: &str = "SQUEEZEFS_DLM_STRIPES";
/// The shipped 4a `DlmLockManager` width (both classes) — the derived
/// table's never-regress floor.
pub const DLM_STRIPES_SHIPPED: usize = 4096;
/// The shipped width of the lease-waiter / grant-floor pair
/// (`dlm::LOCK_WAITERS`, `dlm::LAST_GRANT_FLOOR`) and of the S9 serve
/// stripe (`meta_ship::publish::SERVE_INO_LOCKS`).
pub const LEASE_STRIPES_SHIPPED: usize = 1024;

/// The per-queue depth the width law sizes for: the explicit
/// `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` clamped exactly as the
/// transport clamps it (`1..=Q_DEPTH_DESIRED`), else `Q_DEPTH_DESIRED` —
/// the ceiling the L1 depth policy converges to on any box whose payload
/// budget allows it, i.e. the most concurrency the ring can present. The
/// mount-time DEGRADED depth is unknowable here (the tables are built at
/// volume open, before the transport plans), and sizing for the ceiling is
/// the conservative direction.
fn transport_q_depth_for_sizing() -> usize {
    crate::env_knobs::opt_int_knob::<usize>("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
        .map(|d| d.clamp(1, fuse3::raw::Q_DEPTH_DESIRED))
        .unwrap_or(fuse3::raw::Q_DEPTH_DESIRED)
}

/// The 4a `DlmLockManager` tables' width in force — one process-lifetime
/// resolution (every volume's table must be the same width, and the
/// contract suite reads the same number the tables were built with).
pub fn dlm_stripe_width() -> usize {
    static W: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *W.get_or_init(|| {
        resolve_dlm_stripes(
            crate::env_knobs::opt_int_knob::<usize>(DLM_STRIPES_ENV),
            DLM_STRIPES_SHIPPED,
            crate::cpu::possible_cpus(),
            transport_q_depth_for_sizing(),
        )
    })
}

/// The transport's delivered-concurrency ceiling — `possible_cpus ×
/// q_depth`, the most requests the FUSE-over-io_uring ring can present at
/// once (one queue per possible CPU, the sizing depth per queue). The
/// population bound every per-in-flight-op census derives its retained
/// capacity from (W-6: the write-phase census map keeps this many entries
/// of capacity so a quiet mount never re-allocates its table per op).
pub fn transport_inflight_ceiling() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        crate::cpu::possible_cpus()
            .max(1)
            .saturating_mul(transport_q_depth_for_sizing().max(1))
    })
}

/// The lease-waiter / grant-floor / serve-stripe width in force (the
/// same law and knob as [`dlm_stripe_width`] over the 1024 shipped floor).
pub fn lease_stripe_width() -> usize {
    static W: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *W.get_or_init(|| {
        resolve_dlm_stripes(
            crate::env_knobs::opt_int_knob::<usize>(DLM_STRIPES_ENV),
            LEASE_STRIPES_SHIPPED,
            crate::cpu::possible_cpus(),
            transport_q_depth_for_sizing(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same key must always resolve to the same lock instance (a stable
    /// per-inode critical section is the whole point).
    #[test]
    fn test_same_key_maps_to_same_lock() {
        let locks: StripeLocks<std::sync::Mutex<()>> = StripeLocks::new(256);
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
        let locks: StripeLocks<std::sync::Mutex<()>> = StripeLocks::new(256);
        let base = locks.get_inode_lock(0) as *const _;
        let distinct = (1..1000u64)
            .filter(|&i| locks.get_inode_lock(i) as *const _ != base)
            .count();
        assert!(distinct > 0, "hash must spread inodes across shards");
    }

    /// The sub-key dimension participates in shard selection.
    #[test]
    fn test_sub_key_varies_shard() {
        let locks: StripeLocks<std::sync::Mutex<()>> = StripeLocks::new(4096);
        let base = locks.get_lock(42, 0) as *const _;
        let distinct = (1..500u32)
            .filter(|&k| locks.get_lock(42, k) as *const _ != base)
            .count();
        assert!(distinct > 0, "sub-key must participate in shard selection");
    }

    /// The mask index is the modulo index for every power-of-two width
    /// (the D-3 conversion changed the representation, not one stripe
    /// assignment).
    #[test]
    fn mask_index_equals_modulo_index_at_every_pow2_width() {
        for width in [1usize, 2, 1024, 4096, 16384] {
            let locks: StripeLocks<std::sync::atomic::AtomicU64> = StripeLocks::new(width);
            assert_eq!(locks.width(), width);
            for ino in [0u64, 1, 42, 0xDEAD_BEEF, u64::MAX] {
                assert_eq!(
                    locks.shard_index(ino),
                    (splitmix64(ino) as usize) % width,
                    "width {width} ino {ino}"
                );
                assert_eq!(
                    locks.block_shard_index(ino, 7),
                    (splitmix64(ino ^ (7u64 << 32)) as usize) % width
                );
            }
        }
    }

    #[test]
    #[should_panic(expected = "not a power of two")]
    fn non_pow2_width_is_a_construction_error() {
        let _: StripeLocks<std::sync::atomic::AtomicU64> = StripeLocks::new(1000);
    }

    #[test]
    fn key_word_is_never_the_sentinel() {
        for a in [0u64, 1, 7, u64::MAX] {
            for b in [0u64, 3, u64::MAX] {
                assert_ne!(key_word(a, b), 0);
            }
        }
    }
}
