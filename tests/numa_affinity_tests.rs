//! NUMA-affinity campaign contracts (2026-07-31, `perf/numa-affinity`) —
//! the **N-topology-general nearest-resource map** (`squeezefs::numa_core`)
//! and the whole-application locality policy layer (`squeezefs::numa`).
//!
//! Charter amendment (user directive, standing): the design must be
//! N-topology-general, never 2-node-shaped — no assumptions about node
//! count, NIC count, or their ratio. These contracts therefore cover the
//! four synthetic shapes the amendment names, fed through the SAME code
//! path the runtime sysfs reader uses (injectable topology descriptions):
//!
//! 1. single-node — the whole machinery is structurally a no-op;
//! 2. the field's 2-socket / 1-NIC-per-node 1:1 split;
//! 3. a 4-node / 2-NIC NPS-style shape (two nodes share each nearest NIC);
//! 4. a node-without-NIC + a CPU-less (CXL-style) memory node.
//!
//! Locality classification is DISTANCE-based (local = the minimal-distance
//! choice was taken), so the `numa_local_bytes`/`numa_remote_bytes`
//! instrument stays honest on any shape.

use squeezefs::numa_core::{self, NumaTopology};

// ---------------------------------------------------------------------
// topology construction
// ---------------------------------------------------------------------

/// Shape 2: the field client — 2× Xeon, node0 = even CPUs, node1 = odd
/// CPUs, canonical 10/21 distance matrix.
fn field_two_socket() -> NumaTopology {
    NumaTopology::synthetic(
        vec![
            (0, (0..32).step_by(2).collect()),
            (1, (0..32).skip(1).step_by(2).collect()),
        ],
        vec![vec![10, 21], vec![21, 10]],
    )
    .expect("valid 2-socket synthetic topology")
}

/// Shape 3: NPS-style 4 nodes on 2 sockets — nodes {0,1} share socket A
/// (intra-socket distance 12), nodes {2,3} share socket B; cross-socket
/// distance 32. NICs live on nodes 0 and 2 only.
fn nps_four_node() -> NumaTopology {
    NumaTopology::synthetic(
        vec![
            (0, vec![0, 1, 2, 3]),
            (1, vec![4, 5, 6, 7]),
            (2, vec![8, 9, 10, 11]),
            (3, vec![12, 13, 14, 15]),
        ],
        vec![
            vec![10, 12, 32, 32],
            vec![12, 10, 32, 32],
            vec![32, 32, 10, 12],
            vec![32, 32, 12, 10],
        ],
    )
    .expect("valid NPS synthetic topology")
}

/// Shape 4: 3 nodes — node0 (cpus, has NIC), node1 (cpus, NO NIC),
/// node2 CPU-LESS memory-only (CXL-style), nearer to node0 than node1.
fn cxl_three_node() -> NumaTopology {
    NumaTopology::synthetic(
        vec![(0, vec![0, 1]), (1, vec![2, 3]), (2, vec![])],
        vec![vec![10, 21, 14], vec![21, 10, 24], vec![14, 24, 10]],
    )
    .expect("valid CXL-style synthetic topology")
}

// ---------------------------------------------------------------------
// shape 1: single node — structural no-op
// ---------------------------------------------------------------------

#[test]
fn single_node_is_structurally_noop() {
    let t = NumaTopology::synthetic(vec![(0, vec![0, 1, 2, 3])], vec![vec![10]])
        .expect("valid single-node topology");
    assert!(t.is_single(), "one node must classify as single");
    // Every owner maps to the one node — the partition is degenerate.
    assert_eq!(t.owner_nodes(8), vec![0; 8]);
    // Every classification is local (there is no remote choice to take).
    assert!(t.is_local_choice(0, 0));
    // Owner pick reduces EXACTLY to (load, index) — today's behavior.
    let owner_nodes = t.owner_nodes(4);
    assert_eq!(
        numa_core::pick_owner(&t, 0, &owner_nodes, &[2, 1, 1, 3]),
        1,
        "single node: lightest owner wins, ties to lowest index"
    );
    assert_eq!(
        numa_core::pick_owner(&t, 0, &owner_nodes, &[0, 0, 0, 0]),
        0,
        "single node: all-tied load resolves to index 0 (dense fill)"
    );
    // No placement plan: pinning/binding are skipped on single-node
    // machines (the policy gate, independent of the env switch).
    assert!(!numa_core::placement_applies(&t, true));
}

// ---------------------------------------------------------------------
// shape 2: the field's 2×1:1 split
// ---------------------------------------------------------------------

#[test]
fn two_socket_lookups_match_the_field() {
    let t = field_two_socket();
    assert!(!t.is_single());
    assert_eq!(t.node_of_cpu(0), Some(0), "even CPUs are node0");
    assert_eq!(t.node_of_cpu(7), Some(1), "odd CPUs are node1");
    assert_eq!(t.node_of_cpu(999), None, "unknown CPUs answer None");
    assert_eq!(t.distance(0, 1), 21);
    assert!(t.is_local_choice(0, 0));
    assert!(!t.is_local_choice(0, 1), "cross-socket is never minimal");
    // Nearest-resource lookup: one NIC per node ⇒ each node's nearest
    // NIC is its own.
    assert_eq!(t.nearest(0, &[0, 1]), Some(0));
    assert_eq!(t.nearest(1, &[0, 1]), Some(1));
    // Owner partition alternates across equal-weight nodes so dense
    // fill keeps every node represented early.
    assert_eq!(t.owner_nodes(8), vec![0, 1, 0, 1, 0, 1, 0, 1]);
}

/// The 530k-ceiling law (2026-08-05, `perf/il-530k-ceiling`): the pick is
/// **balance-first, locality-tiebreak** — `(load, distance, index)`. The
/// prior `(distance, load, index)` order let distance strictly dominate,
/// so a process fleet whose HELLO-instant inference clustered on ONE node
/// (fork placement — 32 fio processes at fleet launch) was CONFINED to
/// that node's owner subset: 4 of 8 service-thread lanes on the 2×16
/// field box, 4-5 live `ipc_direct_shards`, and the ~530k rand-4k il
/// service ceiling (clat doubling per qd step) while the derived drain
/// width sat half dark. Balance across the derived width is a structural
/// property; locality remains the zero-cost tiebreak (arena placement
/// follows the CHOSEN owner, so daemon-side passes stay node-local by
/// construction either way).
#[test]
fn two_socket_owner_pick_is_balance_first_locality_tiebreak() {
    let t = field_two_socket();
    let owner_nodes = t.owner_nodes(8); // [0,1,0,1,0,1,0,1]

    // Balance first: a node0 session must take an IDLE node1 owner over
    // any loaded node0 owner (the confinement shape, inverted).
    let loads = [9, 0, 8, 0, 7, 0, 9, 0];
    assert_eq!(
        numa_core::pick_owner(&t, 0, &owner_nodes, &loads),
        1,
        "node0 session must take the idle node1 owner — width beats locality"
    );

    // Locality tiebreak: at equal load the nearest owner still wins.
    let loads = [0; 8];
    assert_eq!(
        numa_core::pick_owner(&t, 1, &owner_nodes, &loads),
        1,
        "all-idle: the nearest (node1) owner wins the tie"
    );
    // Among equal-load equal-distance candidates, lowest index (dense fill).
    let loads = [1, 0, 1, 0, 1, 0, 1, 0];
    assert_eq!(
        numa_core::pick_owner(&t, 0, &owner_nodes, &loads),
        1,
        "equal-load node1 candidates resolve to the lowest index"
    );
}

/// The field-fleet engagement contract: sequential same-node admissions
/// (every session's inference says node 0 — the fork-clustered fleet
/// launch) must engage EVERY owner of the derived width, nearest-first
/// within each load level. Under the retired locality-first order this
/// fill never leaves {0,2,4,6} — the 4-of-8 `ipc_direct_shards` field
/// signature.
#[test]
fn two_socket_same_node_fleet_fill_engages_every_owner() {
    let t = field_two_socket();
    let owner_nodes = t.owner_nodes(8); // [0,1,0,1,0,1,0,1]
    let mut loads = vec![0usize; 8];
    let mut picks = Vec::new();
    for _ in 0..8 {
        let o = numa_core::pick_owner(&t, 0, &owner_nodes, &loads);
        loads[o] += 1;
        picks.push(o);
    }
    assert_eq!(
        picks,
        vec![0, 2, 4, 6, 1, 3, 5, 7],
        "same-node fleet fill: local owners first at load 0, then the far \
         node's owners — all 8 engaged, never confined to one node's subset"
    );
}

// ---------------------------------------------------------------------
// shape 3: NPS 4-node / 2-NIC — shared nearest NICs
// ---------------------------------------------------------------------

#[test]
fn nps_two_nodes_share_each_nearest_nic() {
    let t = nps_four_node();
    let nic_nodes = [0usize, 2usize];
    // Node1 has no NIC: its nearest is the SOCKET-SHARED node0 NIC
    // (distance 12), never the cross-socket node2 NIC (distance 32).
    assert_eq!(t.nearest(1, &nic_nodes), Some(0));
    assert_eq!(t.nearest(3, &nic_nodes), Some(2));
    // NIC-owning nodes resolve to themselves.
    assert_eq!(t.nearest(0, &nic_nodes), Some(0));
    assert_eq!(t.nearest(2, &nic_nodes), Some(2));
    // Rank order from node1: self, socket sibling, then the far socket
    // (ties resolve by index — deterministic).
    assert_eq!(t.ranked_by_distance(1), vec![1, 0, 2, 3]);
    // Owner partition covers all four equal-weight nodes round-robin.
    assert_eq!(t.owner_nodes(8), vec![0, 1, 2, 3, 0, 1, 2, 3]);
    // Distance-based classification: intra-socket non-self is still
    // NOT the minimal choice (12 > 10) — remote, honestly.
    assert!(!t.is_local_choice(1, 0));
}

// ---------------------------------------------------------------------
// shape 4: node without NIC + CPU-less memory node
// ---------------------------------------------------------------------

#[test]
fn cxl_and_nicless_nodes_resolve_by_distance() {
    let t = cxl_three_node();
    // The CPU-less node never receives owners (nothing can execute there).
    let owners = t.owner_nodes(6);
    assert!(
        owners.iter().all(|&n| n != 2),
        "CPU-less node must never be an owner node (got {owners:?})"
    );
    // A memory node with no CPUs: nearest EXEC node is a distance lookup.
    assert_eq!(
        t.nearest_exec_node(2),
        Some(0),
        "node2's nearest CPUs are node0's"
    );
    // A node with no NIC resolves its NIC set by distance (NIC on node0
    // only ⇒ everyone's nearest NIC is node0).
    assert_eq!(t.nearest(1, &[0]), Some(0));
    // Memory deliberately on the CXL node: minimal-distance classification
    // from node0's CPUs — the CXL node (14) is NOT the minimal choice
    // vs node0 itself (10); the instrument reads it remote, honestly.
    assert!(!t.is_local_choice(0, 2));
}

// ---------------------------------------------------------------------
// same-pid sibling rotation (fd-sharded sessions of one app)
// ---------------------------------------------------------------------

/// One multi-threaded app opens N fd-sharded sessions from one pid; its
/// main-thread CPU sample must not pile every arena + service thread
/// onto one socket (drain capacity halves — the ingest-economy wall
/// shape). Law: sibling k of a pid lands on the k-th exec node ranked
/// by distance from the inferred base (nearest first, ties by index).
#[test]
fn same_pid_sessions_rotate_across_exec_nodes() {
    let t = field_two_socket();
    // Base node 1: rank [1, 0] — siblings alternate 1,0,1,0…
    assert_eq!(t.rotate_exec_from(1, 0), 1);
    assert_eq!(t.rotate_exec_from(1, 1), 0);
    assert_eq!(t.rotate_exec_from(1, 2), 1);
    assert_eq!(t.rotate_exec_from(1, 7), 0);

    // NPS: base 1 ranks exec nodes [1(10), 0(12), 2(32), 3(32)].
    let nps = nps_four_node();
    assert_eq!(nps.rotate_exec_from(1, 0), 1);
    assert_eq!(nps.rotate_exec_from(1, 1), 0);
    assert_eq!(nps.rotate_exec_from(1, 2), 2);
    assert_eq!(nps.rotate_exec_from(1, 3), 3);
    assert_eq!(nps.rotate_exec_from(1, 4), 1, "wraps");

    // CPU-less nodes never enter the rotation.
    let cxl = cxl_three_node();
    for k in 0..6 {
        assert_ne!(cxl.rotate_exec_from(0, k), 2, "CXL node excluded");
    }

    // Single node: every sibling lands on the one node (no-op).
    let single = NumaTopology::synthetic(vec![(0, vec![0, 1])], vec![vec![10]])
        .expect("valid single-node topology");
    assert_eq!(single.rotate_exec_from(0, 3), 0);
}

// ---------------------------------------------------------------------
// weighted owner partition (asymmetric CPU counts)
// ---------------------------------------------------------------------

#[test]
fn owner_partition_is_cpu_weighted() {
    // 24-CPU node0 vs 8-CPU node1 ⇒ 3:1 owner split.
    let t = NumaTopology::synthetic(
        vec![(0, (0..24).collect()), (1, (24..32).collect())],
        vec![vec![10, 21], vec![21, 10]],
    )
    .expect("valid asymmetric topology");
    let owners = t.owner_nodes(4);
    assert_eq!(
        owners.iter().filter(|&&n| n == 0).count(),
        3,
        "24:8 CPUs must partition owners 3:1 (got {owners:?})"
    );
    assert_eq!(owners.iter().filter(|&&n| n == 1).count(), 1);
}

// ---------------------------------------------------------------------
// runtime reader + pure parsers
// ---------------------------------------------------------------------

#[test]
fn sysfs_reader_never_panics_and_covers_this_host() {
    let t = NumaTopology::from_sysfs();
    assert!(!t.is_empty(), "at least one node always exists");
    // Every node's self-distance is minimal from itself.
    for n in 0..t.len() {
        assert!(
            t.is_local_choice(n, n),
            "self must always be the minimal choice"
        );
    }
    // The current CPU maps to a node (the classification prerequisite).
    if let Some(cpu) = numa_core::current_cpu() {
        assert!(
            t.node_of_cpu(cpu).is_some(),
            "the executing CPU {cpu} must map to a node"
        );
    }
}

#[test]
fn proc_stat_cpu_parser_survives_hostile_comms() {
    // proc(5): field 2 (comm) may contain spaces and parens — parse from
    // the LAST ')'. After it, token index i is field (i + 3), so the
    // processor field (39) is token index 36. Build the tail so token i
    // literally reads "i": the expectation is then self-evident.
    let tail: Vec<String> = (0..45)
        .map(|i| {
            if i == 0 {
                "S".to_string()
            } else {
                i.to_string()
            }
        })
        .collect();
    let stat = format!("1234 (a b) c) evil) {}", tail.join(" "));
    assert_eq!(
        numa_core::parse_proc_stat_cpu(&stat),
        Some(36),
        "processor = field 39 = post-paren token 36, comm-paren-immune"
    );
    assert_eq!(numa_core::parse_proc_stat_cpu("garbage"), None);
    assert_eq!(numa_core::parse_proc_stat_cpu(""), None);
    // A truncated stat (fewer than 39 fields) answers None, never panics.
    assert_eq!(numa_core::parse_proc_stat_cpu("1 (c) S 2 3"), None);
}

#[test]
fn env_switch_is_pure_and_defaults_on() {
    assert!(numa_core::env_enabled_from(None), "default is ON");
    assert!(!numa_core::env_enabled_from(Some("0")), "0 disables");
    assert!(numa_core::env_enabled_from(Some("1")));
    assert!(
        numa_core::env_enabled_from(Some("garbage")),
        "unparsable values keep the default (loud logs, never a refusal)"
    );
}

// ---------------------------------------------------------------------
// the locality instrument (numa_local_bytes / numa_remote_bytes)
// ---------------------------------------------------------------------

#[test]
fn gauge_classification_is_distance_based_and_exact() {
    use squeezefs::fuse_client::METRICS;
    use std::sync::atomic::Ordering;

    let t = field_two_socket();
    let l0 = METRICS.numa_local_bytes.load(Ordering::Relaxed);
    let r0 = METRICS.numa_remote_bytes.load(Ordering::Relaxed);

    // exec node == mem node ⇒ local, byte-exact.
    squeezefs::numa::classify_and_count(&t, Some(0), Some(0), 4096);
    assert_eq!(METRICS.numa_local_bytes.load(Ordering::Relaxed) - l0, 4096);
    assert_eq!(METRICS.numa_remote_bytes.load(Ordering::Relaxed) - r0, 0);

    // cross-node ⇒ remote, byte-exact.
    squeezefs::numa::classify_and_count(&t, Some(1), Some(0), 1024);
    assert_eq!(METRICS.numa_remote_bytes.load(Ordering::Relaxed) - r0, 1024);

    // Unknown nodes never enter the instrument (neither counter moves).
    squeezefs::numa::classify_and_count(&t, None, Some(0), 512);
    squeezefs::numa::classify_and_count(&t, Some(0), None, 512);
    assert_eq!(METRICS.numa_local_bytes.load(Ordering::Relaxed) - l0, 4096);
    assert_eq!(METRICS.numa_remote_bytes.load(Ordering::Relaxed) - r0, 1024);
}

// ---------------------------------------------------------------------
// placement helpers are refusal-tolerant (best-effort, never fatal)
// ---------------------------------------------------------------------

#[test]
fn placement_helpers_tolerate_refusal() {
    let t = NumaTopology::from_sysfs();
    // Binding a fresh private mapping to node 0 either takes or refuses
    // — never panics, never corrupts (we immediately write the region).
    let len = 2 * 1024 * 1024;
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED);
    let _took = t.bind_region_preferred(base as *mut u8, len, 0);
    unsafe {
        std::ptr::write_bytes(base as *mut u8, 0xa5, len);
        assert_eq!(*(base as *const u8), 0xa5);
        libc::munmap(base, len);
    }
    // Pinning to a node whose CPU set does not intersect the process
    // mask must refuse (false), not distort affinity: probe with an
    // impossible synthetic node.
    let ghost = NumaTopology::synthetic(
        vec![(0, vec![4093, 4094]), (1, vec![0, 1])],
        vec![vec![10, 21], vec![21, 10]],
    )
    .expect("valid ghost topology");
    assert!(
        !ghost.pin_current_to_node(0),
        "empty process-mask intersection must refuse the pin"
    );
}
