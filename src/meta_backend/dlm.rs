//! Per-metadata-object lock manager (`I{ino}` / `D{parent,name}`).
//!
//! Fixed per-class stripe arrays replace the former unbounded
//! `DashMap<String, Arc<RwLock<()>>>`: zero allocation per acquisition
//! (the old path built three `String`s per lock — the `format!`ed key, the
//! `entry(key.to_string())` probe, and a dead copy inside the guard), zero
//! map growth, and integer hashing instead of byte-string hashing.
//!
//! # Collision model
//!
//! Two distinct objects may share a stripe; a collision only widens a
//! critical section (never under-locks) — but since PR M7 (D5) the guard
//! is held across the whole commit park, so that widening costs the
//! colliding op a full commit. D-3 (e2e perf audit DLM board #4) made the
//! width DERIVED (`stripe_locks::dlm_stripe_width`: `SQUEEZEFS_DLM_STRIPES`
//! explicit, else `next_pow2(max(4096, 16 × possible_cpus × q_depth))` —
//! the table sized so a false-sharing wait hits ≤ 1 acquire in 16 at the
//! transport's full delivered concurrency) and gave each class a
//! collision census (`dlm_{inode,dentry}_stripe_collisions` vs
//! `_key_waits`, plus the 4a wait `lock_phase_ns.dlm_guard_wait`).
//! Correctness then rests on one rule, enforced by
//! [`DlmLockManager::lock_many`]:
//!
//! **Canonical acquisition order** — all `I`-class locks before any
//! `D`-class lock; within each class, deduplicate by stripe index and
//! acquire in ascending index order (a stripe requested both shared and
//! exclusive is acquired exclusive). Re-locking a shared stripe would
//! self-deadlock the non-reentrant `RwLock`; opposite per-op orders would
//! ABBA. Operations that *discover* lock targets mid-flight (unlink's
//! child inode) must not take `I` after `D`: they lock a first phase,
//! read, drop, then re-lock the full set and revalidate (see
//! `unlink`'s two-phase in `mod.rs`).
//!
//! Cross-volume operations acquire per-volume lock sets in ascending
//! volume order; each volume's set is internally canonical, so the
//! composition remains cycle-free.

use crate::sqz_sync::SqzRwLock as RwLock;
use crate::stripe_locks::{
    dlm_stripe_width, key_word, StripeCensus, StripeLocks, DLM_DENTRY_CENSUS, DLM_INODE_CENSUS,
};
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

/// One lock class: the stripe table plus its D-3 collision census.
struct LockClass {
    locks: StripeLocks<Arc<RwLock<()>>>,
    census: StripeCensus,
}

impl LockClass {
    fn new(width: usize, counters: &'static crate::stripe_locks::StripeCensusCounters) -> Self {
        Self {
            locks: StripeLocks::new(width),
            census: StripeCensus::new(width, counters),
        }
    }
}

#[derive(Clone)]
pub struct DlmLockManager {
    inode: Arc<LockClass>,
    dentry: Arc<LockClass>,
}

impl Default for DlmLockManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DlmLockManager {
    /// Both classes at the D-3 derived width ([`dlm_stripe_width`]:
    /// `SQUEEZEFS_DLM_STRIPES` explicit, else `next_pow2(max(4096,
    /// 16 × possible_cpus × q_depth))`).
    pub fn new() -> Self {
        let width = dlm_stripe_width();
        Self {
            inode: Arc::new(LockClass::new(width, &DLM_INODE_CENSUS)),
            dentry: Arc::new(LockClass::new(width, &DLM_DENTRY_CENSUS)),
        }
    }

    /// The stripe population of each class.
    pub fn width(&self) -> usize {
        self.inode.locks.width()
    }

    /// Stripe index of an inode-class lock (public: tests force collisions).
    #[inline]
    pub fn inode_stripe(&self, ino: u64) -> usize {
        self.inode.locks.shard_index(ino)
    }

    /// The dentry-class hash input for `(parent, name)`.
    #[inline]
    fn dentry_key(parent: u64, name: &str) -> u64 {
        parent ^ xxh3_64(name.as_bytes()).rotate_left(1)
    }

    /// Stripe index of a dentry-class lock (public: tests force collisions).
    #[inline]
    pub fn dentry_stripe(&self, parent: u64, name: &str) -> usize {
        self.dentry
            .locks
            .shard_index(Self::dentry_key(parent, name))
    }

    /// `hold_timed`: the 4a EXCLUSIVE `I{ino}` guard records its hold in
    /// `lock_phase_ns.dlm_guard_hold` (e2e audit D); shared and dentry
    /// guards do not.
    ///
    /// `key` is the object's census identity word ([`key_word`] of the
    /// class's hash input): the try-acquire fast path is the same one core
    /// critical section the parking acquire takes uncontended, so the
    /// census costs the uncontended acquire one relaxed store; a REFUSED
    /// try is the contended arm — classify against the stripe's last
    /// acquirer (`<class>_stripe_collisions` vs `<class>_key_waits`),
    /// record the parked span in `lock_phase_ns.dlm_guard_wait` (D-3: the
    /// 4a wait the audit-D family lacked; one sample per CONTENDED acquire,
    /// so its count ≡ Σ both classes' census), then park.
    async fn lock_stripe(
        class: &LockClass,
        index: usize,
        key: u64,
        mode: LockMode,
        hold_timed: bool,
    ) -> DlmGuard {
        let cell = class.locks.get_by_index(index).clone();
        let inner = match mode {
            LockMode::Shared => match cell.try_read_owned() {
                Ok(g) => DlmGuardInner::Shared { _g: g },
                Err(cell) => {
                    class.census.classify_contended(index, key);
                    let t0 = std::time::Instant::now();
                    let g = cell.read_owned().await;
                    crate::fuse_client::lock_phase_record(
                        crate::fuse_client::LockPhase::DlmGuardWait,
                        t0.elapsed(),
                    );
                    DlmGuardInner::Shared { _g: g }
                }
            },
            LockMode::Exclusive => match cell.try_write_owned() {
                Ok(g) => DlmGuardInner::Exclusive { _g: g },
                Err(cell) => {
                    class.census.classify_contended(index, key);
                    let t0 = std::time::Instant::now();
                    let g = cell.write_owned().await;
                    crate::fuse_client::lock_phase_record(
                        crate::fuse_client::LockPhase::DlmGuardWait,
                        t0.elapsed(),
                    );
                    DlmGuardInner::Exclusive { _g: g }
                }
            },
        };
        class.census.stamp(index, key);
        // op-trace (audit A2): the guard is dropped at its tx's terminal
        // outcome — often on the conveyor pass task, outside the op's
        // scope — so the op id travels ON the guard, and the stamps ride
        // the hold histogram's own two clock reads (the hold-timed class
        // is exactly the one the trace names: the 4a exclusive I-guard).
        let trace_id = if hold_timed {
            crate::op_trace::current_op()
        } else {
            0
        };
        let acquired = if hold_timed {
            let now = std::time::Instant::now();
            crate::op_trace::stamp(trace_id, crate::op_trace::Stage::DlmGuardAcquired, now);
            Some(now)
        } else {
            None
        };
        DlmGuard {
            _inner: inner,
            acquired,
            trace_id,
        }
    }

    /// Exclusive `I{ino}` lock.
    pub async fn lock_inode_exclusive(&self, ino: u64) -> DlmGuard {
        Self::lock_stripe(
            &self.inode,
            self.inode_stripe(ino),
            key_word(ino, 0),
            LockMode::Exclusive,
            true,
        )
        .await
    }

    /// Shared `I{ino}` lock.
    pub async fn lock_inode_shared(&self, ino: u64) -> DlmGuard {
        Self::lock_stripe(
            &self.inode,
            self.inode_stripe(ino),
            key_word(ino, 0),
            LockMode::Shared,
            false,
        )
        .await
    }

    /// Exclusive `D{parent,name}` lock.
    pub async fn lock_dentry_exclusive(&self, parent: u64, name: &str) -> DlmGuard {
        Self::lock_stripe(
            &self.dentry,
            self.dentry_stripe(parent, name),
            key_word(Self::dentry_key(parent, name), 0),
            LockMode::Exclusive,
            false,
        )
        .await
    }

    /// Shared `D{parent,name}` lock.
    pub async fn lock_dentry_shared(&self, parent: u64, name: &str) -> DlmGuard {
        Self::lock_stripe(
            &self.dentry,
            self.dentry_stripe(parent, name),
            key_word(Self::dentry_key(parent, name), 0),
            LockMode::Shared,
            false,
        )
        .await
    }

    /// Acquire a multi-object lock set in canonical order: inode class
    /// first, then dentry class, each deduplicated by stripe index
    /// (exclusive wins on a shared/exclusive tie) and acquired ascending.
    ///
    /// Every operation that takes more than one metadata lock MUST come
    /// through here (or replicate this order exactly): ad-hoc sequences
    /// self-deadlock on stripe collisions.
    pub async fn lock_many(
        &self,
        inodes: &[(u64, LockMode)],
        dentries: &[(u64, &str, LockMode)],
    ) -> Vec<DlmGuard> {
        let mut plan: Vec<(usize, u64, LockMode)> = inodes
            .iter()
            .map(|&(ino, mode)| (self.inode_stripe(ino), key_word(ino, 0), mode))
            .collect();
        let mut guards = Vec::with_capacity(inodes.len() + dentries.len());
        guards.extend(Self::acquire_deduped(&self.inode, &mut plan, true).await);

        let mut plan: Vec<(usize, u64, LockMode)> = dentries
            .iter()
            .map(|&(parent, name, mode)| {
                (
                    self.dentry_stripe(parent, name),
                    key_word(Self::dentry_key(parent, name), 0),
                    mode,
                )
            })
            .collect();
        guards.extend(Self::acquire_deduped(&self.dentry, &mut plan, false).await);
        guards
    }

    /// The plan carries each object's census key beside its stripe; the
    /// dedup keeps the FIRST object of a shared stripe as the stripe's
    /// census identity (an in-group stripe collision is one acquire —
    /// never a self-collision in the census).
    async fn acquire_deduped(
        class: &LockClass,
        plan: &mut Vec<(usize, u64, LockMode)>,
        inode_class: bool,
    ) -> Vec<DlmGuard> {
        plan.sort_unstable_by_key(|&(idx, _, mode)| (idx, mode == LockMode::Shared));
        // After the sort an exclusive request on a stripe precedes a shared
        // one, so dedup-by-index keeps the strongest mode.
        plan.dedup_by_key(|&mut (idx, _, _)| idx);
        let mut guards = Vec::with_capacity(plan.len());
        for &(idx, key, mode) in plan.iter() {
            let hold_timed = inode_class && mode == LockMode::Exclusive;
            guards.push(Self::lock_stripe(class, idx, key, mode, hold_timed).await);
        }
        guards
    }
}

/// The held RwLock guard (RAII only — never read).
enum DlmGuardInner {
    Shared {
        _g: crate::sqz_sync::OwnedSqzRwLockReadGuard<()>,
    },
    Exclusive {
        _g: crate::sqz_sync::OwnedSqzRwLockWriteGuard<()>,
    },
    /// A guard whose lock lives in ANOTHER table (symmetric PR 6, design
    /// §5.6 line 1 — "foreign-home guards travel"): the holder of the
    /// object's slot parks the real guards under `scope`, and this
    /// handle's drop runs `on_drop` — the release toward that holder —
    /// at the SAME instant the op's local guards go, so the travelling
    /// guard rides every `Arc<[DlmGuard]>` signature unchanged and obeys
    /// the terminal-outcome release law by construction.
    External {
        scope: u64,
        on_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
    },
}

/// One held 4a object lock. Dropping an EXCLUSIVE `I{ino}` guard records
/// its hold in `lock_phase_ns.dlm_guard_hold` (e2e audit D).
pub struct DlmGuard {
    _inner: DlmGuardInner,
    /// `Some` = hold-timed (exclusive inode class).
    acquired: Option<std::time::Instant>,
    /// The acquiring op's trace id (0 = untraced) — see `lock_stripe`.
    trace_id: u64,
}

impl DlmGuard {
    /// A guard held in another table under `scope` (see
    /// [`DlmGuardInner::External`]); `on_drop` runs exactly once, at the
    /// drop.
    pub fn external(scope: u64, on_drop: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            _inner: DlmGuardInner::External {
                scope,
                on_drop: Some(Box::new(on_drop)),
            },
            acquired: None,
            trace_id: 0,
        }
    }

    /// The scope of an external guard; `None` for a lock in this table.
    pub fn external_scope(&self) -> Option<u64> {
        match &self._inner {
            DlmGuardInner::External { scope, .. } => Some(*scope),
            DlmGuardInner::Shared { .. } | DlmGuardInner::Exclusive { .. } => None,
        }
    }
}

impl Drop for DlmGuard {
    fn drop(&mut self) {
        if let Some(t) = self.acquired {
            let now = std::time::Instant::now();
            crate::fuse_client::lock_phase_record(
                crate::fuse_client::LockPhase::DlmGuardHold,
                now.saturating_duration_since(t),
            );
            crate::op_trace::stamp(self.trace_id, crate::op_trace::Stage::DlmGuardReleased, now);
        }
        if let DlmGuardInner::External { on_drop, .. } = &mut self._inner {
            if let Some(release) = on_drop.take() {
                release();
            }
        }
    }
}
