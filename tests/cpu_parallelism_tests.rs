//! Hang-1 regression pins — affinity-poisoned pool sizing.
//!
//! `main.rs` pins every tokio runtime worker to a single core
//! (`on_thread_start` → `core_affinity::set_for_current`). Any Lazy pool
//! whose size is derived from `std::thread::available_parallelism()` (or
//! `core_affinity::get_core_ids()`) at first touch FROM such a worker sees
//! a 1-CPU affinity mask and collapses: the fuse3 TPC handler pool became
//! ONE thread and `striped_block_concurrency` reported 4 on a 16-core mount
//! (both observed in the wedged daemon). Sizing must come from the PROCESS
//! (main-thread) affinity mask, immune to the calling thread's pin.

use std::sync::mpsc;

/// Pin the calling thread to exactly one allowed CPU. Returns the previous
/// mask so the test can restore it.
fn pin_current_thread_to_one_cpu() -> libc::cpu_set_t {
    unsafe {
        let mut old: libc::cpu_set_t = std::mem::zeroed();
        assert_eq!(
            libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut old),
            0,
            "sched_getaffinity(self)"
        );
        let first = (0..libc::CPU_SETSIZE as usize)
            .find(|&i| libc::CPU_ISSET(i, &old))
            .expect("at least one allowed cpu");
        let mut one: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(first, &mut one);
        assert_eq!(
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &one),
            0,
            "sched_setaffinity(self → 1 cpu)"
        );
        old
    }
}

fn restore_affinity(mask: &libc::cpu_set_t) {
    unsafe {
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), mask);
    }
}

/// The process parallelism helper must not depend on the calling thread's
/// affinity: a 1-CPU-pinned thread must observe the same value as an
/// unpinned one (the process/main-thread mask).
#[test]
fn test_process_parallelism_ignores_calling_thread_pin() {
    let unpinned = squeezefs::cpu::process_parallelism();
    assert!(unpinned >= 1);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let old = pin_current_thread_to_one_cpu();
        let pinned_view = squeezefs::cpu::process_parallelism();
        restore_affinity(&old);
        tx.send(pinned_view).expect("send");
    });
    let pinned_view = rx.recv().expect("pinned thread result");

    assert_eq!(
        pinned_view, unpinned,
        "process_parallelism must be calling-thread-affinity independent"
    );
    // The real regression: on any multi-CPU process mask the pinned view
    // must NOT collapse to 1.
    if unpinned > 1 {
        assert!(
            pinned_view > 1,
            "1-CPU-pinned caller collapsed process parallelism to 1 (Hang-1 sizing poison)"
        );
    }
}

/// `striped_block_concurrency`'s auto policy must be computed from process
/// parallelism — a pinned first/any caller must see clamp(process*2, 4, 64),
/// not clamp(1*2, 4, 64) == 4.
#[test]
fn test_striped_block_concurrency_immune_to_thread_pin() {
    squeezefs::bg_admit::set_striped_block_concurrency(0);
    let expected = squeezefs::cpu::process_parallelism()
        .saturating_mul(2)
        .clamp(4, 64);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let old = pin_current_thread_to_one_cpu();
        let seen = squeezefs::bg_admit::striped_block_concurrency();
        restore_affinity(&old);
        tx.send(seen).expect("send");
    });
    let seen = rx.recv().expect("pinned thread result");

    assert_eq!(
        seen, expected,
        "striped_block_concurrency must derive from process affinity, not the caller's pin"
    );
}
