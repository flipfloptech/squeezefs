//! Process-level CPU parallelism, immune to calling-thread pinning.
//!
//! `main.rs` pins every tokio runtime worker to one core
//! (`on_thread_start` → `core_affinity::set_for_current`), and both
//! `std::thread::available_parallelism()` and
//! `core_affinity::get_core_ids()` consult the **calling thread's**
//! affinity mask. Any lazily-sized pool first touched from a pinned worker
//! therefore saw ONE CPU and collapsed — the fuse3 TPC handler pool became
//! a single LocalSet thread and `striped_block_concurrency` fell to its
//! floor, the enabling half of the Hang-1 CFR wedge. Every sizing decision
//! must instead use the PROCESS mask: the main thread (tid == pid) is never
//! pinned, so `sched_getaffinity(getpid())` returns the mount's original
//! taskset mask no matter which thread asks.

use std::sync::OnceLock;

/// Number of CPUs in the process's (main thread's) affinity mask.
///
/// Cached: the value is calling-thread independent by construction, so the
/// first caller — pinned or not — computes the same result. Falls back to
/// `available_parallelism()` if the syscall fails; never returns 0.
pub fn process_parallelism() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(compute_process_parallelism)
}

/// Kernel **possible** CPU count (`_SC_NPROCESSORS_CONF`) — the same
/// population the FUSE-over-io_uring transport sizes its queue set from
/// (kernel `fuse_uring_create()` uses `num_possible_cpus()`; fewer queues
/// than possible CPUs never becomes ready). Distinct from
/// [`process_parallelism`] on purpose: taskset/offline masks shrink the
/// affinity mask but never the delivered ring geometry, and anything
/// shadowing that geometry (the D1.b op registry) must size from THIS
/// number. Never returns 0.
pub fn possible_cpus() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
        if n > 0 {
            n as usize
        } else {
            process_parallelism()
        }
    })
}

fn compute_process_parallelism() -> usize {
    // SAFETY: zeroed cpu_set_t is a valid empty set; sched_getaffinity
    // writes at most size_of::<cpu_set_t>() bytes into it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(
            std::process::id() as libc::pid_t,
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut set,
        ) == 0
        {
            let count = (0..libc::CPU_SETSIZE as usize)
                .filter(|&i| libc::CPU_ISSET(i, &set))
                .count();
            if count > 0 {
                return count;
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_parallelism_nonzero_and_stable() {
        let a = process_parallelism();
        let b = process_parallelism();
        assert!(a >= 1);
        assert_eq!(a, b, "cached value must be stable");
    }
}
