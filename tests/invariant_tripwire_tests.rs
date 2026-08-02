//! RES-22 (pre-RC engineering spec §7): runtime CONCURRENCY-outcome
//! assertions must be loud-never-fatal counters, not `debug_assert!`.
//!
//! `debug_assert!` is exactly right for pure arithmetic — bounds, LBA
//! alignment, range ordering — where the predicate is a property of the
//! caller's arguments and a violation is a coding error the developer
//! can see. It is exactly WRONG for a predicate whose truth depends on a
//! concurrent schedule: those hold in every schedule the author imagined
//! and fail in the one production finds, and in a debug build the
//! failure is a PANIC inside a handler task. That is the class that
//! already produced a lost-reply stall here — the §5.4 transport-lease
//! watchdog's `debug_assert` panicked write-handler tasks whose
//! invocations legitimately exceeded 1 s under writeback backpressure,
//! losing the FUSE reply (fsync in D-state forever, umount joins) — and
//! the fix was to make it the loud-never-fatal `transport_lease_overlong`
//! tripwire. This item applies the same ruling to the rest of the class.
//!
//! Contracts pinned here:
//!
//! 1. **The tripwire is counted and never fatal**: it bumps
//!    `invariant_tripwires` (stats inode; 0 on a healthy daemon) and
//!    returns.
//! 2. **A converted site no longer panics in a debug build**:
//!    `ActiveBlockBuf::zero_complete` on a deferred-seed buffer — an
//!    ORDERING outcome between the owner's seed materialization and the
//!    exit — reports and proceeds.
//!
//! RED against dev 7d1ec2e1: `invariant_tripwires` /
//! `note_invariant_tripwire` do not exist, and contract 2 panics
//! ("zero_complete on a deferred-seed buffer") in a debug build.

use squeezefs::cache::active_block::ActiveBlockBuf;
use squeezefs::fuse_client::METRICS;
use std::sync::atomic::Ordering;

fn tripwires() -> u64 {
    METRICS.invariant_tripwires.load(Ordering::Relaxed)
}

#[test]
fn tripwire_is_counted_and_never_fatal() {
    let before = tripwires();
    squeezefs::note_invariant_tripwire(
        "res22_unit",
        "a concurrency outcome that must never happen",
    );
    assert_eq!(
        tripwires() - before,
        1,
        "RES-22: a tripwire must be COUNTED — that is what makes it \
         actionable without being fatal"
    );
}

#[test]
fn deferred_seed_exit_reports_instead_of_panicking() {
    let before = tripwires();
    // The ORDERING outcome: the owner is supposed to materialize the
    // old-block seed before any content-establishing exit. Whether it
    // did is a property of the schedule, not of these arguments — so a
    // violation must be reported, never a panic inside a handler task.
    let mut buf = ActiveBlockBuf::extent(4096, true);
    buf.zero_complete();
    assert!(
        tripwires() > before,
        "RES-22: the deferred-seed exit violation must be counted"
    );
    assert!(
        buf.is_content_valid(),
        "the exit still establishes content-validity — reporting is not \
         refusing (a refusal here would strand the caller's block)"
    );
}
