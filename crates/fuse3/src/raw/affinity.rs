//! Thread-affinity scope for the FUSE transport's OS threads — the
//! transport-ingress economy campaign (2026-08-01).
//!
//! The serve-latency decomposition named transport ingress queueing the
//! shared dominant term of both walls (queue_wait 1.55 + dispatch_lag
//! 1.70 ms/op on cold reads at the EXA shape). The mechanism hunt
//! convicted the **pinned-thread runqueue hostage**: every `fuse3-tpcN`
//! handler lane and every `fuse-over-uring-N` queue worker was hard-pinned
//! to ONE core, so under load a cross-thread wake waited ms-class for that
//! specific core's runqueue (lane schedstats: seconds of runqueue wait vs
//! comparable CPU per window; SCHED_FIFO and affinity-widening
//! discriminators each collapsed both terms ~99 %).
//!
//! The fix is **node-scoped affinity** (the default): a transport thread
//! keeps its home NUMA node — `node_lanes` grouping, payload-arena
//! binding, and `tpc_spawn_on_node` locality are untouched — but may run
//! on ANY process-mask CPU of that node, so a wake lands on the first
//! idle core instead of waiting for one specific busy one. Single-node
//! maps degrade to the whole available set (the structural no-op law:
//! locality cannot be lost where there is only one node).
//!
//! `SQUEEZEFS_FUSE_PIN_SCOPE=core` is the A0 measurement lever and the
//! operational escape: the exact pre-campaign 1-CPU pin posture.

use std::sync::OnceLock;

/// Affinity scope for transport OS threads (lanes + queue workers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinScope {
    /// Pre-campaign posture: hard-pin each thread to its home core.
    Core,
    /// Default: schedulable on every available CPU of the home core's
    /// node (locality preserved, runqueue hostage deleted).
    Node,
}

/// Parse the `SQUEEZEFS_FUSE_PIN_SCOPE` value. Unknown values degrade to
/// the default posture loudly (never crash a mount over a typo'd knob).
pub fn pin_scope_from_env(v: Option<&str>) -> PinScope {
    match v.map(str::trim) {
        None | Some("") | Some("node") => PinScope::Node,
        Some("core") => PinScope::Core,
        Some(other) => {
            eprintln!(
                "SQUEEZEFS_FUSE_PIN_SCOPE={other:?} is not 'node' or 'core' — \
                 using the default 'node' posture"
            );
            PinScope::Node
        }
    }
}

/// The session-wide pin scope, resolved once from the environment.
pub(crate) fn pin_scope() -> PinScope {
    static SCOPE: OnceLock<PinScope> = OnceLock::new();
    *SCOPE.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_FUSE_PIN_SCOPE").ok();
        pin_scope_from_env(v.as_deref())
    })
}

/// The CPU set a transport thread with home CPU `home` may run on, given
/// the AVAILABLE set (the caller's process mask, minus any reserve it
/// applies — order is preserved):
///
/// - `Core` → exactly `[home]` (the pre-campaign pin).
/// - `Node` → the available CPUs of `home`'s node; an unknown node or an
///   empty intersection degrades to the WHOLE available set — freedom is
///   the safe direction (a 1-CPU pin is the measured failure mode).
pub fn scoped_affinity_cpus(
    scope: PinScope,
    home: usize,
    avail: &[usize],
    node_of: impl Fn(usize) -> Option<usize>,
    node_cpus: impl Fn(usize) -> Vec<usize>,
) -> Vec<usize> {
    match scope {
        PinScope::Core => vec![home],
        PinScope::Node => {
            if let Some(node) = node_of(home) {
                let members = node_cpus(node);
                let scoped: Vec<usize> = avail
                    .iter()
                    .copied()
                    .filter(|c| members.contains(c))
                    .collect();
                if !scoped.is_empty() {
                    return scoped;
                }
            }
            avail.to_vec()
        }
    }
}

/// Set the CURRENT thread's affinity to `cpus`. Best-effort (a refused
/// mask leaves the inherited one — strictly wider, never a hostage).
pub(crate) fn set_current_affinity(cpus: &[usize]) -> bool {
    // SAFETY: zeroed cpu_set_t is a valid empty set; CPU_SET bounds are
    // checked below; sched_setaffinity on tid 0 = the current thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        let mut any = false;
        for &c in cpus {
            if c < libc::CPU_SETSIZE as usize {
                libc::CPU_SET(c, &mut set);
                any = true;
            }
        }
        if !any {
            return false;
        }
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// CPU ids of the PROCESS affinity mask (the main thread's — tid == pid —
/// which is never core-pinned), NOT the calling thread's.
///
/// `TPC_SCHEDULER` is a `Lazy` first touched from a FUSE dispatch task,
/// which the embedding daemon runs on a runtime worker pinned to ONE
/// core. `core_affinity::get_core_ids()` consults the calling thread's
/// mask, so sizing from it collapsed the whole handler pool to a single
/// LocalSet thread — every handler future serialized onto it, and one
/// synchronously parked handler wedged every FUSE request on the mount
/// (the SqueezeFS Hang-1 fsx `copy_file_range` wedge).
pub(crate) fn process_cpus() -> Vec<usize> {
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
            let ids: Vec<usize> = (0..libc::CPU_SETSIZE as usize)
                .filter(|&i| libc::CPU_ISSET(i, &set))
                .collect();
            if !ids.is_empty() {
                return ids;
            }
        }
    }
    core_affinity::get_core_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.id)
        .collect()
}
