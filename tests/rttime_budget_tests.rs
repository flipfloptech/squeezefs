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
//! Pinned: the pure verdict law over `(soft, hard, lifted)`; the live
//! lift — the soft limit lowered to a finite budget below an unlimited hard
//! (always admissible) is LIFTED back to unlimited, and a finite HARD the
//! process may not raise is judged as the pair in force after the soft is
//! raised to it (review round 2, Issue 2: the first build judged the
//! pre-fallback pair, so `(0, 200 ms)` read as the zero class and `mount`
//! would have refused a 200 ms budget); and the `mount --daemon` refusal
//! under a zero that stands reaching the PARENT's console through the
//! handshake pipe (Issue 1: the lift runs in the forked child, whose stderr
//! is `/dev/null`). ONE live lift test per binary: the limit is a
//! process-wide resource, and its last step lowers the hard limit for good.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

    // LAST step (irreversible for an unprivileged process): a zero SOFT
    // under a finite HARD. The lift raises the soft to the hard and the
    // verdict names the 200 ms budget now in force — never the zero it
    // started as. Root may raise the hard too, and then it is the lift.
    let want = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 200_000,
    };
    // SAFETY: as above.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &want) }, 0);
    let verdict = lift_rttime_budget();
    // SAFETY: as above.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut rl) }, 0);
    // SAFETY: geteuid is trivially safe.
    if unsafe { libc::geteuid() } == 0 {
        assert_eq!(verdict, RttimeBudget::Lifted { was_us: 0 });
        assert_eq!(rl.rlim_cur, libc::RLIM_INFINITY);
        assert_eq!(rl.rlim_max, libc::RLIM_INFINITY);
    } else {
        assert_eq!(
            verdict,
            RttimeBudget::FiniteStands { hard_us: 200_000 },
            "the verdict judges the pair IN FORCE after the soft is raised to the hard"
        );
        assert_eq!(rl.rlim_cur, 200_000, "the soft was raised to the hard");
        assert_eq!(rl.rlim_max, 200_000);
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_rttime_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// `mount --daemon` under a zero realtime budget that STANDS (the
/// launcher's `0/0`, set on the child before exec — always admissible,
/// no root, no FUSE: the refusal precedes every volume read and every
/// FUSE act, so a blank meta file is enough): the parent's console names
/// `RLIMIT_RTTIME` through the handshake pipe. As root the lift succeeds
/// and the child proceeds to its ordinary bootstrap refusal.
#[test]
fn a_daemon_mount_under_a_zero_budget_that_stands_reports_it_on_the_parents_console() {
    let base = scratch("daemon");
    let blank = base.join("blank.bin");
    std::fs::File::create(&blank)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let log = base.join("mount.log");

    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", blank.display()))
        .arg(&mnt)
        .arg("--daemon")
        .arg("--log-file")
        .arg(&log)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setrlimit with a stack struct is async-signal-safe and touches
    // no memory shared with the parent.
    unsafe {
        cmd.pre_exec(|| {
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_RTTIME, &zero) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn squeezefs mount --daemon");
    let deadline = Duration::from_secs(40);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if start.elapsed() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("mount --daemon did not exit within {deadline:?}");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    let elapsed = start.elapsed();
    let out = child.wait_with_output().expect("collect output");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "the mount must fail\n{text}");
    assert!(
        elapsed < Duration::from_secs(20),
        "the refusal must not wait out the 30 s handshake; took {elapsed:?}\n{text}"
    );
    assert!(
        !text.contains("no error reported over the handshake pipe"),
        "the child's reason must travel the pipe, never the generic line; got:\n{text}"
    );
    // SAFETY: geteuid is trivially safe.
    if unsafe { libc::geteuid() } == 0 {
        assert!(
            text.contains("not formatted") && !text.contains("RLIMIT_RTTIME hard limit is 0"),
            "as root the zero budget is lifted and the bootstrap proceeds; got:\n{text}"
        );
    } else {
        assert!(
            text.contains("RLIMIT_RTTIME hard limit is 0 µs and could not be lifted"),
            "the parent's console must name the zero realtime budget; got:\n{text}"
        );
        assert!(
            !text.contains("not formatted"),
            "the refusal precedes every volume read; got:\n{text}"
        );
    }
    assert!(!mnt.join(".stats").exists(), "nothing may be mounted");

    let _ = std::fs::remove_dir_all(&base);
}
