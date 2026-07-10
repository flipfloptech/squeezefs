//! Hang-1 regression pin — the fuse3 TPC handler-pool collapse.
//!
//! `fuse3::raw::session::TPC_SCHEDULER` is a `Lazy` sized from the CPU set
//! at FIRST TOUCH. First touch happens on a core-pinned tokio runtime
//! worker (main.rs pins every worker), so `core_affinity::get_core_ids()`
//! returned the caller's 1-CPU mask and the whole FUSE handler pool
//! collapsed to ONE LocalSet thread — every handler future serialized onto
//! it, and one parked shard read wedged the entire daemon (the observed
//! fsx CFR wedge: gdb thread 4 `lock_shared` under `LocalSet::tick`).
//!
//! Pin: even when the first touch comes from a 1-CPU-pinned thread, the
//! pool must size from the PROCESS affinity mask. This test must be the
//! Lazy's first toucher, so it owns its own integration-test binary (own
//! process) and is the only test in it.

use std::sync::mpsc;

#[test]
fn test_tpc_pool_sizes_from_process_affinity_despite_pinned_first_touch() {
    let process_cpus = squeezefs::cpu::process_parallelism();

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Pin this thread to a single CPU, then be the FIRST to touch the
        // TPC scheduler — exactly the shipped init-on-pinned-worker shape.
        unsafe {
            let mut old: libc::cpu_set_t = std::mem::zeroed();
            assert_eq!(
                libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut old),
                0
            );
            let first = (0..libc::CPU_SETSIZE as usize)
                .find(|&i| libc::CPU_ISSET(i, &old))
                .expect("at least one allowed cpu");
            let mut one: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(first, &mut one);
            assert_eq!(
                libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &one),
                0
            );
        }
        tx.send(fuse3::raw::tpc_thread_count()).expect("send");
    });
    let tpc_threads = rx.recv().expect("pinned first-touch result");

    if process_cpus > 2 {
        assert!(
            tpc_threads > 1,
            "TPC pool collapsed to {tpc_threads} thread(s) under a pinned first toucher \
             (process allows {process_cpus} CPUs) — the Hang-1 single-handler-thread poison"
        );
    } else {
        assert!(tpc_threads >= 1);
    }
}
