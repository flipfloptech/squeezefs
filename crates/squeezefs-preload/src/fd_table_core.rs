//! The shim fd table's **lock-free protocol core** — extracted per the
//! house `#[path]`-shared-core convention so `loom-models/` can check the
//! shipped code, not a copy (`docs/pre-rc-engineering-spec.md` §11
//! **TEST-5**).
//!
//! Why this file exists: [`crate::fd_table`] is a 9-CAS lock-free table
//! that runs inside **arbitrary host applications**, across `fork`, and
//! from contexts that must stay async-signal-safe — and it had no model
//! and no in-module tests. The three protocols below are everything in it
//! that is *concurrent*; the table around them (a two-level radix of
//! write-once directory entries over leaked cells) is single-slot atomics
//! whose only ordering question is answered here.
//!
//! ## 1. [`RefCore`] — the Issue-14 refcount law
//!
//! A binding is shared by every fd entry `dup*` propagated it to.
//! `close`/`close_range`/the creator sweep release one ref; the caller
//! sends the async unbind ctl message only when a release reports the
//! terminal transition — which must happen **exactly once**, on the last
//! ref. The count is monotone-to-zero: [`RefCore::acquire`] refuses to
//! resurrect a zero count, so a racing `dup` can never revive a binding
//! whose unbind is already on the wire.
//!
//! ## 2. [`MirrorCore`] — the PERF-7 offset mirror and its Dekker pair
//!
//! Offsetful ops (`read`/`write`/`readv`/`writev`) consume the file
//! description's `f_pos`. The mirror moves that authority out of the
//! kernel (2 `lseek64` per op) into the shared cell. It is authoritative
//! **iff** armed at bind with a fork-epoch snapshot taken before the fd
//! existed, that snapshot still equals the table's current epoch, and
//! nothing has disarmed it.
//!
//! The op side ([`MirrorCore::publish`]) and the demote side
//! ([`MirrorCore::disarm_if_current`]) form a **Dekker pair**: each stores
//! to one word and then loads the OTHER. Release/Acquire alone admits the
//! store-buffering outcome — both sides reading old — in which the op's
//! final offset reaches *neither* the mirror's consumer nor the kernel,
//! and a fork child resumes reading at a stale position. Both sides
//! therefore interpose `fence(SeqCst)`. That is the property the loom
//! model exists to check, and weakening either fence must fail it.
//!
//! ## 3. [`EpochCore`] — the fork epoch
//!
//! A monotone counter bumped by the atfork-prepare / `posix_spawn` walk.
//! Its job is conservative-correctness across the bind-vs-fork race: a
//! fork between "snapshot the epoch" and "install the binding" stales the
//! snapshot, so the binding never reads armed and the fd silently falls
//! back to the kernel-authoritative discipline.
//!
//! Nothing here allocates, blocks, or calls into libc — the interposers
//! must stay async-signal-safe (POSIX lists `close` as AS-safe, so apps
//! legitimately call it from signal handlers).

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
}

use atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// 1. the refcount law
// ---------------------------------------------------------------------------

/// One binding's live fd-entry references. `0` is **terminal**: a zero
/// count is never resurrected, and exactly one releaser observes the
/// 1 → 0 transition.
#[derive(Debug)]
pub struct RefCore {
    refs: AtomicU32,
}

impl RefCore {
    /// A freshly installed binding: one entry holds it.
    pub fn new_one() -> Self {
        Self {
            refs: AtomicU32::new(1),
        }
    }

    /// Current count — diagnostics only; never a decision input (the
    /// decision is [`Self::acquire`]'s CAS or [`Self::release`]'s RMW).
    pub fn peek(&self) -> u32 {
        self.refs.load(Ordering::Acquire)
    }

    /// Take one more ref, unless the cell already hit zero (a racing
    /// last-close won; the binding's unbind may already be on the wire).
    pub fn acquire(&self) -> bool {
        self.refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n == 0 {
                    None
                } else {
                    Some(n + 1)
                }
            })
            .is_ok()
    }

    /// Drop one ref; `true` exactly on the terminal 1 → 0 transition —
    /// the caller's cue to send the async unbind ctl message.
    ///
    /// A CAS loop rather than `fetch_sub`: an over-release (a double
    /// close of the same table entry, which the table's swap-to-null
    /// makes hard but not impossible across a raw-syscall close) would
    /// **wrap** a `fetch_sub` from 0 to `u32::MAX`, and a wrapped count is
    /// a binding that can never report its unbind again — the leak the
    /// gauge would never show. Saturating at zero is total.
    pub fn release(&self) -> bool {
        self.refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n == 0 {
                    None
                } else {
                    Some(n - 1)
                }
            })
            .is_ok_and(|prev| prev == 1)
    }
}

// ---------------------------------------------------------------------------
// 2. the offset mirror + the Dekker pair
// ---------------------------------------------------------------------------

/// The per-description offset mirror (PERF-7). `epoch` is the fork-epoch
/// snapshot taken **before** the fd's `open` syscall; it is immutable for
/// the cell's life and is what makes the arm predicate fork-safe.
#[derive(Debug)]
pub struct MirrorCore {
    off: AtomicU64,
    /// Arm flag; cleared exactly once by the first demoter (monotone — a
    /// mirror never re-arms; the fd falls back to kernel authority).
    flag: AtomicBool,
    epoch: u64,
}

impl MirrorCore {
    /// A cell whose offsetful ops run the kernel lseek-resync discipline.
    pub fn unarmed() -> Self {
        Self {
            off: AtomicU64::new(0),
            flag: AtomicBool::new(false),
            epoch: 0,
        }
    }

    /// An armed cell seeded with the fd's current kernel offset and the
    /// pre-`open` fork-epoch snapshot. A stale snapshot installs the
    /// mirror but it never reads armed — the fallthrough is silent,
    /// structural, and costs nothing.
    pub fn armed_at(seed_off: u64, epoch: u64) -> Self {
        Self {
            off: AtomicU64::new(seed_off),
            flag: AtomicBool::new(true),
            epoch,
        }
    }

    /// The bind-time epoch snapshot (immutable).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The normative authority predicate: armed flag set AND the
    /// bind-time snapshot is still the table's current epoch.
    pub fn armed(&self, table_epoch: &EpochCore) -> bool {
        self.flag.load(Ordering::Acquire) && self.epoch == table_epoch.current()
    }

    /// The mirrored offset. Meaningful only while [`Self::armed`].
    pub fn load(&self) -> u64 {
        self.off.load(Ordering::Acquire)
    }

    /// Plain offset store (caller holds the cell's offset lock).
    pub fn store(&self, off: u64) {
        self.off.store(off, Ordering::Release);
    }

    /// **Dekker side A** — op completion: store the final offset,
    /// `fence(SeqCst)`, re-check the arm.
    ///
    /// `false` = a demote raced this op (fork flush, unbind demote): the
    /// CALLER owns the kernel write-through of `off`. The fence is
    /// load-bearing — without it this store and the flag load may both
    /// pass a concurrent [`Self::disarm_if_current`] unseen (store
    /// buffering), and the final offset would reach neither the mirror's
    /// consumer nor the kernel.
    pub fn publish(&self, table_epoch: &EpochCore, off: u64) -> bool {
        self.off.store(off, Ordering::Release);
        fence(Ordering::SeqCst);
        self.armed(table_epoch)
    }

    /// **Dekker side B** — demote: clear the arm exactly once and capture
    /// the latest mirrored offset for the caller's kernel flush.
    ///
    /// `None` = this cell was not authoritative (never armed, stale
    /// epoch, or already demoted) — the caller must NOT touch kernel
    /// `f_pos` (flushing a never-armed seed would REWIND an offset the
    /// kernel has legitimately advanced).
    pub fn disarm_if_current(&self, table_epoch: &EpochCore) -> Option<u64> {
        if !self.armed(table_epoch) {
            return None;
        }
        if !self.flag.swap(false, Ordering::AcqRel) {
            return None; // another demoter won
        }
        fence(Ordering::SeqCst);
        Some(self.off.load(Ordering::Acquire))
    }

    /// The fork-walk's demote: only cells whose snapshot is exactly the
    /// **previous** epoch are flushed. Stale cells (never armed since an
    /// earlier fork) and post-bump ones (their open began after this
    /// bump, so the fd cannot be in the child) keep their state — no
    /// flush, no clear, because flushing them would rewind a
    /// kernel-authoritative offset.
    ///
    /// `Some(off)` = this call performed the once-per-cell disarm and the
    /// caller owes the kernel one `SEEK_SET` to `off`.
    pub fn fork_demote(&self, prev_epoch: u64) -> Option<u64> {
        if self.epoch != prev_epoch {
            return None;
        }
        if !self.flag.swap(false, Ordering::AcqRel) {
            return None;
        }
        fence(Ordering::SeqCst);
        Some(self.off.load(Ordering::Acquire))
    }
}

// ---------------------------------------------------------------------------
// 3. the fork epoch
// ---------------------------------------------------------------------------

/// The table's monotone fork epoch.
#[derive(Debug)]
pub struct EpochCore {
    epoch: AtomicU64,
}

impl Default for EpochCore {
    fn default() -> Self {
        Self::new()
    }
}

impl EpochCore {
    pub fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
        }
    }

    /// Snapshot this **before** the real `open` syscall of any fd whose
    /// bind will arm a mirror: a fork between snapshot and install stales
    /// the snapshot, so the binding never arms (conservative-correct).
    pub fn current(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// The atfork-prepare / `posix_spawn` bump; returns the epoch that
    /// just ended — the one [`MirrorCore::fork_demote`] flushes.
    pub fn bump(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// pure seek arithmetic (no atomics — here because it is the mirror's law)
// ---------------------------------------------------------------------------

/// Seek arithmetic for the interposed `lseek`/`lseek64` mirror arm
/// (`SEEK_SET`/`SEEK_CUR` only — `SEEK_END`/`SEEK_DATA`/`SEEK_HOLE` need
/// the kernel's size/extent authority and are routed to the real call by
/// the interposer, keeping the POSIX-8 `i_size` staleness scope exactly
/// where it was). Matches the 64-bit kernel: a negative or sign-wrapped
/// result is `EINVAL` (`vfs_setpos`).
///
/// `EINVAL` is spelled as its numeric value so the core stays
/// dependency-free for the loom build.
pub const SEEK_EINVAL: i32 = 22;

pub fn mirror_seek_core(cur: u64, offset: i64, whence: Whence) -> Result<u64, i32> {
    match whence {
        Whence::Set => {
            if offset < 0 {
                Err(SEEK_EINVAL)
            } else {
                Ok(offset as u64)
            }
        }
        Whence::Cur => match (cur as i64).checked_add(offset) {
            Some(n) if n >= 0 => Ok(n as u64),
            _ => Err(SEEK_EINVAL),
        },
    }
}

/// The two whences the mirror can serve without the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whence {
    Set,
    Cur,
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn refs_are_monotone_to_zero() {
        let r = RefCore::new_one();
        assert!(r.acquire(), "a live binding accepts a dup");
        assert_eq!(r.peek(), 2);
        assert!(!r.release(), "not the last ref");
        assert!(r.release(), "the last ref reports terminal exactly once");
        assert!(!r.acquire(), "a zero count is never resurrected");
        assert!(!r.release(), "and never reports terminal twice");
        assert_eq!(r.peek(), 0, "over-release saturates, never wraps");
    }

    #[test]
    fn unarmed_mirror_is_never_authoritative() {
        let ep = EpochCore::new();
        let m = MirrorCore::unarmed();
        assert!(!m.armed(&ep));
        assert_eq!(
            m.disarm_if_current(&ep),
            None,
            "flushing a never-armed seed would REWIND the kernel offset"
        );
    }

    #[test]
    fn armed_mirror_disarms_exactly_once() {
        let ep = EpochCore::new();
        let m = MirrorCore::armed_at(4096, ep.current());
        assert!(m.armed(&ep));
        assert_eq!(m.disarm_if_current(&ep), Some(4096));
        assert_eq!(m.disarm_if_current(&ep), None, "monotone: never re-arms");
        assert!(!m.armed(&ep));
    }

    #[test]
    fn a_fork_bump_stales_the_bind_time_snapshot() {
        let ep = EpochCore::new();
        let m = MirrorCore::armed_at(0, ep.current());
        assert!(m.armed(&ep));
        let prev = ep.bump();
        assert!(
            !m.armed(&ep),
            "a fork between snapshot and use must fall back to kernel authority"
        );
        assert_eq!(
            m.fork_demote(prev),
            Some(0),
            "the fork walk still owes the kernel this cell's offset"
        );
        assert_eq!(m.fork_demote(prev), None, "once per cell");
    }

    #[test]
    fn fork_demote_skips_cells_from_other_epochs() {
        let ep = EpochCore::new();
        let prev = ep.bump(); // prev == 0, current == 1
        let post = MirrorCore::armed_at(8192, ep.current());
        assert_eq!(
            post.fork_demote(prev),
            None,
            "a cell whose open began AFTER the bump cannot be in the child"
        );
        assert!(post.armed(&ep), "and keeps its arm");
    }

    #[test]
    fn publish_reports_whether_the_mirror_still_owns_the_offset() {
        let ep = EpochCore::new();
        let m = MirrorCore::armed_at(0, ep.current());
        assert!(m.publish(&ep, 4096), "an armed mirror absorbs the offset");
        assert_eq!(m.load(), 4096);
        let _ = m.disarm_if_current(&ep);
        assert!(
            !m.publish(&ep, 8192),
            "after a demote the CALLER owns the kernel write-through"
        );
    }

    #[test]
    fn seek_arithmetic_matches_the_64_bit_kernel() {
        assert_eq!(mirror_seek_core(0, 4096, Whence::Set), Ok(4096));
        assert_eq!(mirror_seek_core(0, -1, Whence::Set), Err(SEEK_EINVAL));
        assert_eq!(mirror_seek_core(4096, -4096, Whence::Cur), Ok(0));
        assert_eq!(mirror_seek_core(0, -1, Whence::Cur), Err(SEEK_EINVAL));
        assert_eq!(
            mirror_seek_core(i64::MAX as u64, 1, Whence::Cur),
            Err(SEEK_EINVAL),
            "sign wrap is EINVAL, exactly like vfs_setpos"
        );
    }
}
