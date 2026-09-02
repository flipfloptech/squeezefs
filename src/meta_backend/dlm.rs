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
//! critical section (never under-locks). Correctness then rests on one
//! rule, enforced by [`DlmLockManager::lock_many`]:
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
use crate::stripe_locks::StripeLocks;
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

const STRIPES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Clone)]
pub struct DlmLockManager {
    inode_locks: Arc<StripeLocks<Arc<RwLock<()>>, STRIPES>>,
    dentry_locks: Arc<StripeLocks<Arc<RwLock<()>>, STRIPES>>,
}

impl Default for DlmLockManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DlmLockManager {
    pub fn new() -> Self {
        Self {
            inode_locks: Arc::new(StripeLocks::new()),
            dentry_locks: Arc::new(StripeLocks::new()),
        }
    }

    /// Stripe index of an inode-class lock (public: tests force collisions).
    #[inline]
    pub fn inode_stripe(&self, ino: u64) -> usize {
        self.inode_locks.shard_index(ino)
    }

    /// Stripe index of a dentry-class lock (public: tests force collisions).
    #[inline]
    pub fn dentry_stripe(&self, parent: u64, name: &str) -> usize {
        self.dentry_locks
            .shard_index(parent ^ xxh3_64(name.as_bytes()).rotate_left(1))
    }

    /// `hold_timed`: the 4a EXCLUSIVE `I{ino}` guard records its hold in
    /// `lock_phase_ns.dlm_guard_hold` (e2e audit D); shared and dentry
    /// guards do not.
    async fn lock_stripe(
        locks: &StripeLocks<Arc<RwLock<()>>, STRIPES>,
        index: usize,
        mode: LockMode,
        hold_timed: bool,
    ) -> DlmGuard {
        let cell = locks.get_by_index(index).clone();
        let inner = match mode {
            LockMode::Shared => DlmGuardInner::Shared {
                _g: cell.read_owned().await,
            },
            LockMode::Exclusive => DlmGuardInner::Exclusive {
                _g: cell.write_owned().await,
            },
        };
        DlmGuard {
            _inner: inner,
            acquired: hold_timed.then(std::time::Instant::now),
        }
    }

    /// Exclusive `I{ino}` lock.
    pub async fn lock_inode_exclusive(&self, ino: u64) -> DlmGuard {
        Self::lock_stripe(
            &self.inode_locks,
            self.inode_stripe(ino),
            LockMode::Exclusive,
            true,
        )
        .await
    }

    /// Shared `I{ino}` lock.
    pub async fn lock_inode_shared(&self, ino: u64) -> DlmGuard {
        Self::lock_stripe(
            &self.inode_locks,
            self.inode_stripe(ino),
            LockMode::Shared,
            false,
        )
        .await
    }

    /// Exclusive `D{parent,name}` lock.
    pub async fn lock_dentry_exclusive(&self, parent: u64, name: &str) -> DlmGuard {
        Self::lock_stripe(
            &self.dentry_locks,
            self.dentry_stripe(parent, name),
            LockMode::Exclusive,
            false,
        )
        .await
    }

    /// Shared `D{parent,name}` lock.
    pub async fn lock_dentry_shared(&self, parent: u64, name: &str) -> DlmGuard {
        Self::lock_stripe(
            &self.dentry_locks,
            self.dentry_stripe(parent, name),
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
        let mut plan: Vec<(usize, LockMode)> = inodes
            .iter()
            .map(|&(ino, mode)| (self.inode_stripe(ino), mode))
            .collect();
        let mut guards = Vec::with_capacity(inodes.len() + dentries.len());
        guards.extend(Self::acquire_deduped(&self.inode_locks, &mut plan, true).await);

        let mut plan: Vec<(usize, LockMode)> = dentries
            .iter()
            .map(|&(parent, name, mode)| (self.dentry_stripe(parent, name), mode))
            .collect();
        guards.extend(Self::acquire_deduped(&self.dentry_locks, &mut plan, false).await);
        guards
    }

    async fn acquire_deduped(
        locks: &StripeLocks<Arc<RwLock<()>>, STRIPES>,
        plan: &mut Vec<(usize, LockMode)>,
        inode_class: bool,
    ) -> Vec<DlmGuard> {
        plan.sort_unstable_by_key(|&(idx, mode)| (idx, mode == LockMode::Shared));
        // After the sort an exclusive request on a stripe precedes a shared
        // one, so dedup-by-index keeps the strongest mode.
        plan.dedup_by_key(|&mut (idx, _)| idx);
        let mut guards = Vec::with_capacity(plan.len());
        for &(idx, mode) in plan.iter() {
            let hold_timed = inode_class && mode == LockMode::Exclusive;
            guards.push(Self::lock_stripe(locks, idx, mode, hold_timed).await);
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
}

/// One held 4a object lock. Dropping an EXCLUSIVE `I{ino}` guard records
/// its hold in `lock_phase_ns.dlm_guard_hold` (e2e audit D).
pub struct DlmGuard {
    _inner: DlmGuardInner,
    /// `Some` = hold-timed (exclusive inode class).
    acquired: Option<std::time::Instant>,
}

impl Drop for DlmGuard {
    fn drop(&mut self) {
        if let Some(t) = self.acquired {
            crate::fuse_client::lock_phase_record(
                crate::fuse_client::LockPhase::DlmGuardHold,
                t.elapsed(),
            );
        }
    }
}
