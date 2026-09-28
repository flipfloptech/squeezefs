//! The daemon's realtime-CPU budget (`RLIMIT_RTTIME` — record §4.4bw).
//!
//! The 1.3.0 release chain's attempt 5 went RED at fstests generic/631 with
//! `rm -rf … Killed`: a desktop launcher (Omarchy's `quickshell`) hands every
//! process it spawns a ZERO realtime-CPU hard limit, and the kernel's
//! RCU-boost kthreads (`CONFIG_RCU_BOOST`) priority-inherit an ordinary
//! task caught in a preempted RCU read section into the realtime class for
//! one tick — at a zero budget that tick trips the realtime watchdog and the
//! kernel SIGKILLs the whole thread group, silently. The daemon inherits
//! the same limit and its threads are boostable like any other, so a
//! long-lived mount under such a budget dies by statistics.
//!
//! Pinned: the pure verdict law over `(soft, hard, lifted)`, and the live
//! lift — the soft limit lowered to a finite budget below an unlimited hard
//! (always admissible) is LIFTED back to unlimited. ONE live test per
//! binary: the limit is a process-wide resource.

use squeezefs::{lift_rttime_budget, rttime_budget_verdict, RttimeBudget};

#[test]
fn the_verdict_law_over_soft_hard_and_the_lift() {
    let inf = libc::RLIM_INFINITY;
    assert_eq!(
        rttime_budget_verdict(inf, inf, false),
        RttimeBudget::Unlimited
    );
    assert_eq!(
        rttime_budget_verdict(0, 0, false),
        RttimeBudget::ZeroStands,
        "the launcher's shape: 0/0 that could not be lifted refuses a mount"
    );
    assert_eq!(
        rttime_budget_verdict(0, 0, true),
        RttimeBudget::Lifted { was_us: 0 },
        "0/0 lifted as root"
    );
    assert_eq!(
        rttime_budget_verdict(200_000, 200_000, false),
        RttimeBudget::FiniteStands { hard_us: 200_000 },
        "PipeWire's 200 ms default: a slow fuse, loud"
    );
    assert_eq!(
        rttime_budget_verdict(0, inf, false),
        RttimeBudget::ZeroStands,
        "a zero SOFT under an unlimited hard that somehow stood is still the zero class"
    );
    assert_eq!(
        rttime_budget_verdict(50_000, inf, true),
        RttimeBudget::Lifted { was_us: 50_000 }
    );
}

#[test]
fn a_finite_soft_budget_under_an_unlimited_hard_is_lifted_at_startup() {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit into and from a stack struct.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut rl) }, 0);
    if rl.rlim_max != libc::RLIM_INFINITY {
        // The launcher's own shape reached this test process: the lift's
        // hard half is root's, so the live half of the contract has no
        // admissible setup here — the pure law above is the pin, and the
        // verdict reads what the launcher did.
        let verdict = lift_rttime_budget();
        assert!(
            matches!(
                verdict,
                RttimeBudget::ZeroStands
                    | RttimeBudget::FiniteStands { .. }
                    | RttimeBudget::Lifted { .. }
            ),
            "a finite hard limit is never read as unlimited: {verdict:?}"
        );
        return;
    }
    // Lower the SOFT limit to a finite budget (always admissible under an
    // unlimited hard), then run the startup act.
    let want = libc::rlimit {
        rlim_cur: 200_000,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: as above.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &want) }, 0);
    assert_eq!(
        lift_rttime_budget(),
        RttimeBudget::Lifted { was_us: 200_000 },
        "the finite soft budget is lifted to unlimited"
    );
    // SAFETY: as above.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut rl) }, 0);
    assert_eq!(rl.rlim_cur, libc::RLIM_INFINITY);
    assert_eq!(rl.rlim_max, libc::RLIM_INFINITY);
    assert_eq!(lift_rttime_budget(), RttimeBudget::Unlimited, "idempotent");
}
