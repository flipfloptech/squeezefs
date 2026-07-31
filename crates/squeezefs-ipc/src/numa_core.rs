//! N-topology-general NUMA nearest-resource map — whole-application
//! NUMA-affinity campaign, 2026-07-31 (`perf/numa-affinity`).
//!
//! **The charter's structural law (user directive, standing):** nothing
//! here may assume node count, NIC count, or their ratio. Real targets
//! include 4-socket hosts, EPYC NPS partitioning (4–8 nodes per socket),
//! nodes with zero NICs, several NICs on one node, CPU-less memory-only
//! (CXL) nodes, and asymmetric RAM per node. The core abstraction is a
//! **runtime-derived map** built from the kernel's node distance matrix
//! (`/sys/devices/system/node/nodeN/distance`) plus per-node CPU lists;
//! "my memory node" and "my nearest X" are LOOKUPS in that map — never
//! arithmetic on node ids, never a boolean local/remote. Ties and
//! resource-less nodes resolve by distance, then by the caller's load,
//! then by index (deterministic).
//!
//! Single-node machines make the whole machinery **structurally a no-op**
//! ([`NumaTopology::is_single`] gates every placement action) — the
//! portable-by-default law: one code path, no fleet-shaped branches.
//!
//! Canonical file in the `squeezefs-ipc` tree, `#[path]`-included by the
//! root crate and the fuse3 fork (the `thp.rs` / `wake_core` production-
//! sharing precedent): the ipc LIBRARY itself stays dependency-free while
//! both consumers already link libc. Type identities never cross a crate
//! boundary.
//!
//! Env: `SQUEEZEFS_NUMA=0` disables every placement/pinning action while
//! keeping the locality INSTRUMENT alive (the A/B lever, the
//! `SQUEEZEFS_NT_COPY` pattern); read by call sites via
//! [`env_enabled_from`] — this core is env-free and pure per call.

use std::sync::OnceLock;

/// One NUMA node: its kernel id and the CPUs it owns (possibly none —
/// CXL-style memory-only nodes are first-class here).
#[derive(Debug, Clone)]
pub struct NodeDesc {
    /// The kernel node id (`nodeN`). Dense index == position in
    /// [`NumaTopology::nodes`]; the two coincide on every kernel we
    /// target (possible-node ids are dense), but lookups never rely on
    /// it — the map carries both.
    pub id: usize,
    /// CPUs on this node (kernel cpu ids, from `cpulist`).
    pub cpus: Vec<usize>,
}

/// The runtime-derived nearest-resource map. Built once from sysfs at
/// process start ([`NumaTopology::from_sysfs`], cached via [`topology`])
/// or from an injected description in tests ([`NumaTopology::synthetic`])
/// — both through the same constructor, so the tested code path IS the
/// production one.
#[derive(Debug, Clone)]
pub struct NumaTopology {
    nodes: Vec<NodeDesc>,
    /// Kernel cpu id → dense node index.
    cpu_node: Vec<Option<usize>>,
    /// Dense `distance[a][b]` (ACPI SLIT values; self = 10 by convention,
    /// but nothing here assumes the constant — only relative order).
    distance: Vec<Vec<u32>>,
    /// Per-node rank order: `ranked[a]` = all node indices sorted by
    /// (distance from `a`, index). Precomputed so hot-path nearest
    /// lookups are table walks.
    ranked: Vec<Vec<usize>>,
}

impl NumaTopology {
    /// Build from an explicit description: `(node_id, cpus)` per node and
    /// a full square distance matrix. `None` when the description is
    /// inconsistent (matrix not square over the node count, or empty).
    pub fn synthetic(nodes: Vec<(usize, Vec<usize>)>, distance: Vec<Vec<u32>>) -> Option<Self> {
        if nodes.is_empty()
            || distance.len() != nodes.len()
            || distance.iter().any(|row| row.len() != nodes.len())
        {
            return None;
        }
        let nodes: Vec<NodeDesc> = nodes
            .into_iter()
            .map(|(id, cpus)| NodeDesc { id, cpus })
            .collect();
        let max_cpu = nodes
            .iter()
            .flat_map(|n| n.cpus.iter().copied())
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        let mut cpu_node = vec![None; max_cpu];
        for (idx, n) in nodes.iter().enumerate() {
            for &c in &n.cpus {
                cpu_node[c] = Some(idx);
            }
        }
        let ranked = (0..nodes.len())
            .map(|a| {
                let mut order: Vec<usize> = (0..nodes.len()).collect();
                order.sort_by_key(|&b| (distance[a][b], b));
                order
            })
            .collect();
        Some(Self {
            nodes,
            cpu_node,
            distance,
            ranked,
        })
    }

    /// The runtime constructor: read possible nodes' `cpulist` +
    /// `distance` from sysfs. ANY read/parse failure degrades to the
    /// single-node identity map (structurally a no-op) — placement is an
    /// optimization, never a correctness need, so the fallback is silent
    /// at this layer (call sites log once at mount).
    pub fn from_sysfs() -> Self {
        Self::from_sysfs_root(std::path::Path::new("/sys/devices/system/node"))
            .unwrap_or_else(Self::single_fallback)
    }

    /// Sysfs reader against an injectable root (tests exercise the SAME
    /// parser against fixture trees).
    pub fn from_sysfs_root(root: &std::path::Path) -> Option<Self> {
        let mut ids: Vec<usize> = std::fs::read_dir(root)
            .ok()?
            .filter_map(|e| {
                let name = e.ok()?.file_name();
                let name = name.to_str()?;
                name.strip_prefix("node")?.parse::<usize>().ok()
            })
            .collect();
        if ids.is_empty() {
            return None;
        }
        ids.sort_unstable();
        let mut nodes = Vec::with_capacity(ids.len());
        let mut distance = Vec::with_capacity(ids.len());
        for &id in &ids {
            let dir = root.join(format!("node{id}"));
            let cpus = parse_cpulist(&std::fs::read_to_string(dir.join("cpulist")).ok()?)?;
            let dist_row: Vec<u32> = std::fs::read_to_string(dir.join("distance"))
                .ok()?
                .split_whitespace()
                .map(|t| t.parse::<u32>())
                .collect::<Result<_, _>>()
                .ok()?;
            nodes.push((id, cpus));
            distance.push(dist_row);
        }
        Self::synthetic(nodes, distance)
    }

    /// The degenerate one-node map (also the failure fallback): every
    /// CPU is node 0, every choice is local, every action is a no-op.
    pub fn single_fallback() -> Self {
        Self::synthetic(vec![(0, Vec::new())], vec![vec![10]])
            .expect("the single-node identity map is always consistent")
    }

    /// Number of nodes in the map.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when the map cannot express a placement choice — the
    /// structural no-op gate for every placement/pinning action.
    pub fn is_single(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub fn is_empty(&self) -> bool {
        false // by construction (synthetic refuses empty descriptions)
    }

    /// The node descriptors (kernel ids + CPU lists), dense order.
    pub fn nodes(&self) -> &[NodeDesc] {
        &self.nodes
    }

    /// Dense node index of a kernel cpu id; `None` for CPUs the map has
    /// never seen (offlined mid-run, or a truncated fixture) — callers
    /// must treat `None` as "stay out of the instrument / no placement".
    pub fn node_of_cpu(&self, cpu: usize) -> Option<usize> {
        self.cpu_node.get(cpu).copied().flatten()
    }

    /// The dense node index the CURRENT thread executes on right now.
    pub fn current_node(&self) -> Option<usize> {
        current_cpu().and_then(|c| self.node_of_cpu(c))
    }

    /// SLIT distance between two dense node indices.
    pub fn distance(&self, a: usize, b: usize) -> u32 {
        self.distance[a][b]
    }

    /// All node indices ranked by (distance from `from`, index) —
    /// deterministic, precomputed.
    pub fn ranked_by_distance(&self, from: usize) -> Vec<usize> {
        self.ranked[from].clone()
    }

    /// Nearest candidate node to `from` among `candidates` (distance,
    /// then index). `None` on an empty candidate set. THE lookup the
    /// charter mandates in place of node-id arithmetic: NIC-less nodes,
    /// shared-NIC NPS shapes, and multi-NIC nodes all resolve here with
    /// zero special cases.
    pub fn nearest(&self, from: usize, candidates: &[usize]) -> Option<usize> {
        candidates
            .iter()
            .copied()
            .filter(|&c| c < self.nodes.len())
            .min_by_key(|&c| (self.distance[from][c], c))
    }

    /// Nearest node that can EXECUTE (owns at least one CPU) — the
    /// CPU-less (CXL) memory node's route to a service context.
    pub fn nearest_exec_node(&self, from: usize) -> Option<usize> {
        let exec: Vec<usize> = (0..self.nodes.len())
            .filter(|&n| !self.nodes[n].cpus.is_empty())
            .collect();
        self.nearest(from, &exec)
    }

    /// Distance-based locality classification (charter amendment §4):
    /// the access was LOCAL iff the memory node was a minimal-distance
    /// choice from the executing node — i.e. no other node could have
    /// been strictly nearer. Reduces to `exec == mem` on ordinary shapes
    /// without ever encoding that as the rule.
    pub fn is_local_choice(&self, exec_node: usize, mem_node: usize) -> bool {
        let min = (0..self.nodes.len())
            .map(|n| self.distance[exec_node][n])
            .min()
            .unwrap_or(0);
        self.distance[exec_node][mem_node] == min
    }

    /// Owner-index → node partition for a pool of `count` slots: a
    /// largest-remainder weighted round-robin over the nodes that own
    /// CPUs, weight = CPU share (pool sizes stay THE existing
    /// derivations; only their node spread is decided here). Interleaved
    /// so dense lowest-first fill keeps every node represented early.
    /// CPU-less nodes never appear. Single node ⇒ all zeros (no-op).
    pub fn owner_nodes(&self, count: usize) -> Vec<usize> {
        let exec: Vec<usize> = (0..self.nodes.len())
            .filter(|&n| !self.nodes[n].cpus.is_empty())
            .collect();
        if exec.len() <= 1 {
            return vec![exec.first().copied().unwrap_or(0); count];
        }
        let total: usize = exec.iter().map(|&n| self.nodes[n].cpus.len()).sum();
        // Interleave by cumulative weight (largest-remainder flavor):
        // slot i goes to the exec node whose weighted window covers i.
        let mut out = Vec::with_capacity(count);
        let mut credit: Vec<f64> = vec![0.0; exec.len()];
        for _ in 0..count {
            for (k, &n) in exec.iter().enumerate() {
                credit[k] += self.nodes[n].cpus.len() as f64 / total as f64;
            }
            // Take the most-credited node (ties → lower index).
            let (best, _) = credit
                .iter()
                .enumerate()
                .max_by(|(ai, av), (bi, bv)| {
                    av.partial_cmp(bv)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(bi.cmp(ai))
                })
                .expect("exec set non-empty");
            credit[best] -= 1.0;
            out.push(exec[best]);
        }
        out
    }

    /// Bind `[base, base + len)` to prefer allocation on `node`
    /// (`MPOL_PREFERRED` — falls back to other nodes under pressure,
    /// never OOMs a mount for locality). Best-effort: `false` = refused
    /// (old kernel, cpuset restriction), mapping stays fully usable.
    /// Callers apply BEFORE first touch (fault-time placement).
    pub fn bind_region_preferred(&self, base: *mut u8, len: usize, node: usize) -> bool {
        if self.is_single() || base.is_null() || len == 0 || node >= self.nodes.len() {
            return false;
        }
        let kernel_id = self.nodes[node].id;
        let mut mask = [0u64; 16]; // 1024 nodes — max_possible on our targets
        if kernel_id >= mask.len() * 64 {
            return false;
        }
        mask[kernel_id / 64] |= 1u64 << (kernel_id % 64);
        const MPOL_PREFERRED: libc::c_int = 1;
        // SAFETY: mbind(2) over a caller-owned mapping; the kernel
        // validates the range and the mask — refusal is an errno, never
        // memory corruption.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_mbind,
                base as usize,
                len,
                MPOL_PREFERRED,
                mask.as_ptr(),
                (mask.len() * 64 + 1) as libc::c_ulong,
                0u32,
            )
        };
        rc == 0
    }

    /// Dense node index of the page backing `addr` (get_mempolicy
    /// `MPOL_F_NODE|MPOL_F_ADDR` — per numa(7) it allocates the page as
    /// if read-faulted when none exists yet, so callers query AFTER any
    /// deliberate placement/populate). `None` on failure or an id the
    /// map has never seen. This is the INSTRUMENT's memory-node source:
    /// it reports where pages actually landed, never where policy asked
    /// them to land.
    pub fn node_of_addr(&self, addr: *const u8) -> Option<usize> {
        if addr.is_null() {
            return None;
        }
        const MPOL_F_NODE: libc::c_ulong = 1 << 0;
        const MPOL_F_ADDR: libc::c_ulong = 1 << 1;
        let mut node: libc::c_int = -1;
        // SAFETY: get_mempolicy writes one int; addr is validated by the
        // kernel (EFAULT on bad ranges — reported as None).
        let rc = unsafe {
            libc::syscall(
                libc::SYS_get_mempolicy,
                &mut node as *mut libc::c_int,
                std::ptr::null_mut::<libc::c_ulong>(),
                0usize,
                addr as usize,
                MPOL_F_NODE | MPOL_F_ADDR,
            )
        };
        if rc != 0 || node < 0 {
            return None;
        }
        let kernel_id = node as usize;
        self.nodes.iter().position(|n| n.id == kernel_id)
    }

    /// Pin the CURRENT thread to `node`'s CPU set INTERSECTED with the
    /// process affinity mask (a taskset-restricted mount must never be
    /// widened). `false` = refused (empty intersection, single node,
    /// syscall failure) — the thread keeps its previous mask.
    pub fn pin_current_to_node(&self, node: usize) -> bool {
        if self.is_single() || node >= self.nodes.len() {
            return false;
        }
        let Some(process_mask) = process_affinity_mask() else {
            return false;
        };
        // SAFETY: zeroed cpu_set_t is a valid empty set; CPU_SET bounds
        // are checked below.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        let mut any = false;
        for &cpu in &self.nodes[node].cpus {
            if cpu < libc::CPU_SETSIZE as usize && unsafe { libc::CPU_ISSET(cpu, &process_mask) } {
                unsafe { libc::CPU_SET(cpu, &mut set) };
                any = true;
            }
        }
        if !any {
            return false;
        }
        // SAFETY: plain sched_setaffinity on the current thread (tid 0).
        unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 }
    }
}

/// Whether placement/pinning actions apply at all: the env lever AND a
/// map that can express a choice. The single-node no-op is structural
/// (independent of the env value).
pub fn placement_applies(t: &NumaTopology, env_enabled: bool) -> bool {
    env_enabled && !t.is_single()
}

/// Locality-first owner pick: among `owner_nodes` (index → node), choose
/// the owner minimizing `(distance(session_node, owner_node), load,
/// index)`. On a single-node map every distance ties, so this IS the
/// pre-campaign `(load, index)` pick — the structural-no-op contract.
pub fn pick_owner(
    t: &NumaTopology,
    session_node: usize,
    owner_nodes: &[usize],
    loads: &[usize],
) -> usize {
    debug_assert_eq!(owner_nodes.len(), loads.len());
    let session_node = session_node.min(t.len().saturating_sub(1));
    (0..owner_nodes.len())
        .min_by_key(|&i| {
            let d = if owner_nodes[i] < t.len() {
                t.distance(session_node, owner_nodes[i])
            } else {
                u32::MAX
            };
            (d, loads[i], i)
        })
        .unwrap_or(0)
}

/// The kernel cpu id the current thread runs on (vDSO-fast; `None` only
/// on exotic failure).
pub fn current_cpu() -> Option<usize> {
    // SAFETY: plain sched_getcpu(3), no memory involved.
    let c = unsafe { libc::sched_getcpu() };
    (c >= 0).then_some(c as usize)
}

/// The PROCESS affinity mask (main thread's — never core-pinned; the
/// `crate::cpu::process_parallelism` precedent).
fn process_affinity_mask() -> Option<libc::cpu_set_t> {
    // SAFETY: zeroed cpu_set_t is a valid empty set; sched_getaffinity
    // writes at most size_of::<cpu_set_t>() bytes into it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        (libc::sched_getaffinity(
            std::process::id() as libc::pid_t,
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut set,
        ) == 0)
            .then_some(set)
    }
}

/// Last-run CPU of an arbitrary pid — the session→node inference input
/// (peercred pid at HELLO; no wire/ABI change). `None` on any failure
/// (racing exit, hidepid) — the caller falls back to its own node.
pub fn last_cpu_of_pid(pid: u32) -> Option<usize> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_proc_stat_cpu(&stat)
}

/// proc(5) `stat` field 39 (`processor`), parsed from the LAST `)` so a
/// hostile comm (spaces, parens) cannot shift the fields.
pub fn parse_proc_stat_cpu(stat: &str) -> Option<usize> {
    let tail = &stat[stat.rfind(')')? + 1..];
    // Token index i after the paren is field (i + 3); processor = 39.
    tail.split_whitespace().nth(36)?.parse().ok()
}

/// Pure env-lever form (`SQUEEZEFS_NUMA`): default ON; only an explicit
/// `0` disables. Unparsable values keep the default.
pub fn env_enabled_from(v: Option<&str>) -> bool {
    !matches!(v.map(str::trim), Some("0"))
}

/// Process-wide cached topology + env lever (read once — placement
/// levers are A/B switches set before mount, the `nt_copy` pattern).
pub fn topology() -> &'static NumaTopology {
    static T: OnceLock<NumaTopology> = OnceLock::new();
    T.get_or_init(NumaTopology::from_sysfs)
}

/// Process-wide cached `SQUEEZEFS_NUMA` lever.
pub fn env_enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| env_enabled_from(std::env::var("SQUEEZEFS_NUMA").ok().as_deref()))
}

/// `topology()` + lever + single-node gate in one call — the placement
/// call sites' one-line guard.
pub fn placement_active() -> bool {
    placement_applies(topology(), env_enabled())
}

/// Parse a kernel `cpulist` (e.g. `0-3,8,10-11`; empty = CPU-less node).
fn parse_cpulist(s: &str) -> Option<Vec<usize>> {
    let s = s.trim();
    if s.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for part in s.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let a: usize = a.trim().parse().ok()?;
                let b: usize = b.trim().parse().ok()?;
                if b < a {
                    return None;
                }
                out.extend(a..=b);
            }
            None => out.push(part.trim().parse::<usize>().ok()?),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpulist_parses_kernel_shapes() {
        assert_eq!(
            parse_cpulist("0-3,8,10-11"),
            Some(vec![0, 1, 2, 3, 8, 10, 11])
        );
        assert_eq!(parse_cpulist(""), Some(vec![]));
        assert_eq!(parse_cpulist("7"), Some(vec![7]));
        assert_eq!(parse_cpulist("3-1"), None, "inverted ranges refuse");
        assert_eq!(parse_cpulist("x"), None);
    }

    #[test]
    fn synthetic_refuses_inconsistent_descriptions() {
        assert!(NumaTopology::synthetic(vec![], vec![]).is_none());
        assert!(
            NumaTopology::synthetic(vec![(0, vec![0])], vec![vec![10, 21]]).is_none(),
            "non-square distance matrix refuses"
        );
    }

    #[test]
    fn single_fallback_is_the_identity_map() {
        let t = NumaTopology::single_fallback();
        assert!(t.is_single());
        assert!(t.is_local_choice(0, 0));
        assert!(!placement_applies(&t, true));
    }
}
