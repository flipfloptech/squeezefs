//! nvmet port-id allocator law tests
//! (`docs/design-nvmeof-target-management.md` §6.6, landed by PR 2/N2).
//!
//! Ports carry no name, so ownership rides a **reserved id range**
//! `[base, base+999]` (base = `SQUEEZEFS_NVMET_PORT_ID_BASE`, default
//! 53000 — disjoint from dev_substrate's 52026 and the scoping rig's
//! 52470/52471). Laws pinned here, against the pure allocator
//! (`allocate_port_id` over an injected configfs snapshot — no root, no
//! kernel):
//!
//! * deterministic first candidate `base + (fnv1a64("tcp:{ip}:{port}") % 900)`
//!   (the last 100 ids are pure probe headroom, reachable only by probe);
//! * linear probe wraps through the FULL `[base, base+999]` range;
//! * a candidate is reusable only if its attrs match exactly
//!   (`addr_trtype/traddr/trsvcid/adrfam`) AND every subsystem link under
//!   it is ours (**ownership = ledger membership**) — otherwise it is
//!   foreign: skip, never touch;
//! * every id in the range occupied by foreign/incompatible ports ⇒
//!   refuse loud, naming the `SQUEEZEFS_NVMET_PORT_ID_BASE` knob.

use squeezefs::nvmeof::nvmet::{
    allocate_port_id, fnv1a_64, PortSnapshotEntry, NVMET_PORT_HASH_SLOTS,
    NVMET_PORT_ID_BASE_DEFAULT, NVMET_PORT_ID_BASE_ENV, NVMET_PORT_ID_RANGE,
};

const BASE: u32 = NVMET_PORT_ID_BASE_DEFAULT;

fn port(id: u32, ip: &str, svc: &str, links: &[&str]) -> PortSnapshotEntry {
    PortSnapshotEntry {
        id,
        trtype: "tcp".to_string(),
        traddr: ip.to_string(),
        trsvcid: svc.to_string(),
        adrfam: "ipv4".to_string(),
        subsystem_links: links.iter().map(|s| s.to_string()).collect(),
    }
}

fn ours(nqn: &str) -> bool {
    nqn.starts_with("nqn.test:ledgered-")
}

/// Deterministic first candidate: the frozen fnv1a-64 vectors. These
/// constants pin the hash function itself — the ledger records allocated
/// ids, so the probe start must never drift across refactors.
#[test]
fn test_port_alloc_deterministic_first_candidate_frozen_vectors() {
    // fnv1a64 pins (computed once, frozen).
    assert_eq!(fnv1a_64(b"tcp:10.0.0.1:4420"), 0x8a90_79e4_09b2_1076);
    assert_eq!(fnv1a_64(b"tcp:127.0.0.1:4420"), 0xef9a_f88e_36f0_6311);

    // Empty snapshot: the allocation IS the first candidate.
    for (ip, svc_port, want) in [
        ("10.0.0.1", 4420u16, BASE + 382),
        ("127.0.0.1", 4420, BASE + 773),
        ("127.0.0.1", 4421, BASE + 862),
        ("10.10.10.50", 4420, BASE + 804),
    ] {
        let alloc = allocate_port_id(BASE, ip, svc_port, &[], &ours)
            .unwrap_or_else(|e| panic!("alloc {ip}:{svc_port} must succeed: {e}"));
        assert_eq!(
            alloc.id, want,
            "first candidate for {ip}:{svc_port} must be base + fnv1a64 % {NVMET_PORT_HASH_SLOTS}"
        );
        assert!(!alloc.reuse, "fresh id on an empty snapshot");
        // Determinism: same inputs, same answer, every time.
        for _ in 0..3 {
            assert_eq!(
                allocate_port_id(BASE, ip, svc_port, &[], &ours).unwrap().id,
                want
            );
        }
    }
}

/// Hash slots spread over 900; ids `base+900 ..= base+999` are pure probe
/// headroom — never a first candidate, only reachable by probing.
#[test]
fn test_port_alloc_range_bounds() {
    for seed in 0..200u32 {
        let ip = format!("10.{}.{}.{}", seed % 7, seed % 13, seed % 251);
        let alloc = allocate_port_id(BASE, &ip, 4420 + (seed % 100) as u16, &[], &ours)
            .expect("alloc on empty snapshot");
        assert!(
            (BASE..BASE + NVMET_PORT_HASH_SLOTS).contains(&alloc.id),
            "first candidate {} must land in the hash slots [{}, {})",
            alloc.id,
            BASE,
            BASE + NVMET_PORT_HASH_SLOTS
        );
    }
}

/// Reuse law: an existing id with exactly matching attrs whose every
/// subsystem link is ledgered is reused; any attr mismatch or unledgered
/// link makes it foreign — skipped, never adopted.
#[test]
fn test_port_alloc_ownership_reuse_and_foreign_skip() {
    let c0 = BASE + 773; // tcp:127.0.0.1:4420

    // (a) Ours + matching attrs (links all ledgered) => reuse.
    let snap = vec![port(c0, "127.0.0.1", "4420", &["nqn.test:ledgered-a"])];
    let alloc = allocate_port_id(BASE, "127.0.0.1", 4420, &snap, &ours).expect("reuse");
    assert_eq!(alloc.id, c0);
    assert!(alloc.reuse, "matching+ours port must be reused");

    // (b) Matching attrs but an unledgered (foreign) link => skip to next id.
    let snap = vec![port(c0, "127.0.0.1", "4420", &["nqn.foreign:squatter"])];
    let alloc = allocate_port_id(BASE, "127.0.0.1", 4420, &snap, &ours).expect("skip");
    assert_eq!(
        alloc.id,
        c0 + 1,
        "foreign-linked port must be skipped, never touched"
    );
    assert!(!alloc.reuse);

    // (c) One ledgered + one foreign link => still foreign (EVERY link must be ours).
    let snap = vec![port(
        c0,
        "127.0.0.1",
        "4420",
        &["nqn.test:ledgered-a", "nqn.foreign:squatter"],
    )];
    let alloc = allocate_port_id(BASE, "127.0.0.1", 4420, &snap, &ours).expect("skip");
    assert_eq!(alloc.id, c0 + 1);

    // (d) Attr mismatch (different trsvcid) with ours-only links => not
    // reusable for THIS listener; skip.
    let snap = vec![port(c0, "127.0.0.1", "4499", &["nqn.test:ledgered-a"])];
    let alloc = allocate_port_id(BASE, "127.0.0.1", 4420, &snap, &ours).expect("skip");
    assert_eq!(alloc.id, c0 + 1, "attr-mismatched port must be skipped");

    // (e) Link-free port with matching attrs (our leftover) => reuse.
    let snap = vec![port(c0, "127.0.0.1", "4420", &[])];
    let alloc = allocate_port_id(BASE, "127.0.0.1", 4420, &snap, &ours).expect("reuse");
    assert_eq!(alloc.id, c0);
    assert!(
        alloc.reuse,
        "a link-free matching port in our range is our leftover"
    );

    // (f) Probe chain: c0..c0+2 all foreign => c0+3.
    let snap = vec![
        port(c0, "127.0.0.1", "4420", &["nqn.foreign:a"]),
        port(c0 + 1, "127.0.0.1", "4420", &["nqn.foreign:b"]),
        port(c0 + 2, "10.9.9.9", "4420", &["nqn.test:ledgered-a"]),
    ];
    let alloc = allocate_port_id(BASE, "127.0.0.1", 4420, &snap, &ours).expect("probe");
    assert_eq!(alloc.id, c0 + 3);
}

/// The linear probe wraps through the FULL `[base, base+999]` range: a
/// first candidate near the top probes into the headroom slots and then
/// wraps to `base`.
#[test]
fn test_port_alloc_probe_wraps_through_full_range() {
    // tcp:127.0.0.1:4421 hashes to BASE+862 (frozen vector).
    let c0 = BASE + 862;
    // Occupy c0 ..= base+999 with foreign ports (into the headroom slice).
    let mut snap: Vec<PortSnapshotEntry> = (c0..BASE + NVMET_PORT_ID_RANGE)
        .map(|id| port(id, "127.0.0.1", "4421", &["nqn.foreign:x"]))
        .collect();
    let alloc = allocate_port_id(BASE, "127.0.0.1", 4421, &snap, &ours).expect("wrap");
    assert_eq!(
        alloc.id, BASE,
        "probe must wrap to base after walking the headroom slice"
    );

    // And with base also taken, the wrap continues linearly.
    snap.push(port(BASE, "127.0.0.1", "4421", &["nqn.foreign:y"]));
    let alloc = allocate_port_id(BASE, "127.0.0.1", 4421, &snap, &ours).expect("wrap+1");
    assert_eq!(alloc.id, BASE + 1);
}

/// Exhaustion: every id in the range occupied by foreign/incompatible
/// ports refuses loud, naming the relocation knob.
#[test]
fn test_port_alloc_exhaustion_refuses_loud_naming_knob() {
    let snap: Vec<PortSnapshotEntry> = (BASE..BASE + NVMET_PORT_ID_RANGE)
        .map(|id| port(id, "10.66.0.1", "9999", &["nqn.foreign:squat"]))
        .collect();
    let err = allocate_port_id(BASE, "127.0.0.1", 4420, &snap, &ours)
        .expect_err("a fully foreign-occupied range must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains(NVMET_PORT_ID_BASE_ENV),
        "exhaustion refusal must name the {NVMET_PORT_ID_BASE_ENV} knob: {msg}"
    );
    assert!(
        msg.contains("53000") && msg.contains("53999"),
        "exhaustion refusal must name the exhausted range: {msg}"
    );
}

/// The base is an env-relocatable knob: a non-default base shifts the
/// whole range (dev boxes running multiple tenants relocate, never share).
#[test]
fn test_port_alloc_respects_relocated_base() {
    let alt_base = 61000u32;
    let alloc = allocate_port_id(alt_base, "10.0.0.1", 4420, &[], &ours).expect("alloc");
    assert_eq!(
        alloc.id,
        alt_base + 382,
        "hash slot rides the relocated base"
    );
    // A port at the DEFAULT base's candidate is outside the relocated
    // range and must be invisible to the allocator's decisions.
    let snap = vec![port(BASE + 382, "10.0.0.1", "4420", &["nqn.foreign:x"])];
    let alloc = allocate_port_id(alt_base, "10.0.0.1", 4420, &snap, &ours).expect("alloc");
    assert_eq!(alloc.id, alt_base + 382);
}
