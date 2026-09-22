//! The two readers of `RLIMIT_NOFILE` and the startup raise between them
//! (symmetric PR 13c, F-B3 — review round 1, Issue 8).
//!
//! `cluster_wire::raise_nofile_soft_limit` lifts the soft limit to the
//! hard one so the cluster-wire LISTENER caps can derive their fd ceiling
//! from a real budget. `uring_fs::fd_cache_cap` also derives from the soft
//! limit — and the first build let it read the RAISED limit, growing the
//! open-file cache 32× (`(524288 / 4 / 8)` clamped to 1024 against the
//! shipped `(1024 / 4 / 8) = 32`) on EVERY posture: the flat mount,
//! `format`, `fsck`, `bench` — a flat-path behaviour change the rung's own
//! law forbids, sitting in the arm the gate-1 bracket attributes. The law
//! now: the limit AS FOUND is recorded before the raise
//! (`cpu::nofile_soft_as_found`) and `fd_cache_cap` derives from it, so the
//! flat path's fd economy is byte-identical to the pre-raise binary; the
//! raise moves the listener caps alone.
//!
//! ONE test per binary: the snapshot is a process-wide `OnceLock` and the
//! limit a process-wide resource, so no sibling test may touch either.

use squeezefs::cluster_wire;
use squeezefs::cpu;
use squeezefs::uring_fs;

/// The systemd / login default soft limit — the shipped derivation's
/// input on every field box before the raise existed.
const LOGIN_DEFAULT_SOFT: libc::rlim_t = 1024;

#[test]
fn the_fd_cache_derives_from_the_limit_as_found_and_the_raise_moves_the_listener_alone() {
    // The pure derivation: the pre-13c value on the box's 8 workers, and
    // what the first build would have made it after the raise.
    assert_eq!(uring_fs::fd_cache_cap_from(1024, 8), 32);
    assert_eq!(uring_fs::fd_cache_cap_from(524_288, 8), 1024);
    assert_eq!(uring_fs::fd_cache_cap_from(0, 8), 16, "the floor");
    assert_eq!(uring_fs::fd_cache_cap_from(1 << 40, 1), 1024, "the cap");

    // The process at the login default: lower the soft limit (always
    // admissible below the hard one) BEFORE anything reads it.
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit into and from a stack struct.
    let got = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) };
    assert_eq!(got, 0, "getrlimit");
    let hard = rl.rlim_max;
    let soft_before = LOGIN_DEFAULT_SOFT.min(hard);
    let want = libc::rlimit {
        rlim_cur: soft_before,
        rlim_max: hard,
    };
    // SAFETY: as above.
    let set = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &want) };
    assert_eq!(set, 0, "setrlimit soft → {soft_before}");

    // The daemon's startup act: record-then-raise.
    let raised = cluster_wire::raise_nofile_soft_limit();
    let soft_before = usize::try_from(soft_before).unwrap();
    let hard = usize::try_from(hard).unwrap_or(usize::MAX);
    assert_eq!(raised, hard, "the raise lands on the hard limit");
    assert_eq!(
        cluster_wire::nofile_soft_limit(),
        hard,
        "the listener's face reads the limit IN FORCE (the raised one)"
    );
    assert_eq!(
        cpu::nofile_soft_as_found(),
        soft_before,
        "the as-found word is the pre-raise limit"
    );

    // The flat path's fd economy: the cache derives from the as-found
    // limit — the pre-13c value on this box — whatever the raise did.
    let (cap, workers) = uring_fs::fd_cache_cap_in_force();
    assert_eq!(cap, uring_fs::fd_cache_cap_from(soft_before, workers));
    let raised_would_give = uring_fs::fd_cache_cap_from(hard, workers);
    if raised_would_give != uring_fs::fd_cache_cap_from(soft_before, workers) {
        assert_ne!(
            cap, raised_would_give,
            "the cache never reads the raised limit (the first build's 32× growth)"
        );
    }
}
