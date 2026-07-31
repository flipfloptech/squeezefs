//! Whole-application NUMA-affinity policy + instrument layer
//! (2026-07-31, `perf/numa-affinity`) — the METRICS-wired face of
//! [`crate::numa_core`].
//!
//! **The instrument (stage 0):** `numa_local_bytes` / `numa_remote_bytes`
//! classify every instrumented CPU pass over payload bytes by the
//! DISTANCE-based rule (`numa_core::NumaTopology::is_local_choice` —
//! local iff the memory node was a minimal-distance choice from the
//! executing node), so the gauge stays honest on any topology. Sites:
//! the §5.5.2 ring-write sever (arena read on the service thread), the
//! §5.5.1 arena completion serves (tier legs writing into the arena),
//! and the fuse3-side transport gauges (surfaced separately). Bytes with
//! an UNKNOWN node on either side never enter the instrument.
//!
//! **The policy (stage 1):** placement/pinning actions run only when
//! [`crate::numa_core::placement_active`] — the `SQUEEZEFS_NUMA=0` A/B
//! lever AND the structural single-node no-op gate. The instrument stays
//! alive on BOTH sides of the lever (that is what makes the field A/B
//! attributable).

use crate::fuse_client::METRICS;
use crate::numa_core::{self, NumaTopology};
use std::sync::atomic::Ordering;

/// Classify one CPU pass over `bytes` payload bytes: executing node vs
/// memory node, distance-based. Unknown nodes stay out of the
/// instrument (never guessed).
#[inline]
pub fn classify_and_count(
    t: &NumaTopology,
    exec_node: Option<usize>,
    mem_node: Option<usize>,
    bytes: usize,
) {
    let (Some(e), Some(m)) = (exec_node, mem_node) else {
        return;
    };
    if e >= t.len() || m >= t.len() {
        return;
    }
    if t.is_local_choice(e, m) {
        METRICS
            .numa_local_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    } else {
        METRICS
            .numa_remote_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// [`classify_and_count`] with exec = the CURRENT thread's node and the
/// process-wide cached map — the hot-path form (one vDSO `sched_getcpu`
/// + two table lookups; single-node maps classify everything local,
/// which is the honest reading).
#[inline]
pub fn count_current_pass(mem_node: Option<usize>, bytes: usize) {
    let t = numa_core::topology();
    classify_and_count(t, t.current_node(), mem_node, bytes);
}

/// The session→node inference at HELLO (daemon-side only — no wire/ABI
/// change): the peer's last-run CPU from proc(5) via the
/// SO_PEERCRED-verified pid, mapped through the topology; falls back to
/// the accepting thread's own node. `None` only on a single-node map
/// (nothing to infer).
pub fn session_node_for_pid(pid: u32) -> Option<usize> {
    let t = numa_core::topology();
    if t.is_single() {
        return None;
    }
    numa_core::last_cpu_of_pid(pid)
        .and_then(|c| t.node_of_cpu(c))
        .or_else(|| t.current_node())
}

/// Best-effort arena placement: bind the session mapping to prefer the
/// session's node BEFORE first touch (composes with the THP
/// populate+collapse — the pages fault on the right node, then collapse
/// keeps them there). Gated by [`numa_core::placement_active`]; refusal
/// is logged at debug and the mapping stays fully usable.
pub fn bind_session_arena(base: *mut u8, len: usize, node: Option<usize>) -> bool {
    if !numa_core::placement_active() {
        return false;
    }
    let Some(node) = node else {
        return false;
    };
    let took = numa_core::topology().bind_region_preferred(base, len, node);
    log::debug!("numa: session arena bind to node {node}: took={took} ({len} bytes)");
    took
}

/// Best-effort service-thread pin to its owner node's CPU set
/// (intersected with the process mask — a taskset-restricted mount is
/// never widened). Gated; refusal leaves the thread's mask untouched.
pub fn pin_service_thread(owner_node: usize) -> bool {
    if !numa_core::placement_active() {
        return false;
    }
    let took = numa_core::topology().pin_current_to_node(owner_node);
    log::debug!("numa: service thread pin to node {owner_node}: took={took}");
    took
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_nodes_never_enter_the_instrument() {
        let t = NumaTopology::single_fallback();
        let l0 = METRICS.numa_local_bytes.load(Ordering::Relaxed);
        classify_and_count(&t, Some(0), Some(7), 4096); // mem out of range
        classify_and_count(&t, Some(7), Some(0), 4096); // exec out of range
        assert_eq!(METRICS.numa_local_bytes.load(Ordering::Relaxed), l0);
    }

    #[test]
    fn session_node_inference_is_none_on_single_node_maps() {
        // The cached process topology on a single-node dev box answers
        // None (structural no-op); on a multi-node box it answers a
        // valid dense index. Both are the contract.
        if let Some(n) = session_node_for_pid(std::process::id()) {
            assert!(n < numa_core::topology().len());
        } else {
            // None is only legal when the map is single-node or proc is
            // unreadable — on our own pid proc is readable.
            assert!(numa_core::topology().is_single());
        }
    }
}
