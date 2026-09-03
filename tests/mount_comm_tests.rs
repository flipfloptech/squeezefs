//! PR 3 — `fix/mw-colocated-collisions` (design-full-multi-writer §5.3):
//! per-mount thread-comm disambiguation.
//!
//! N co-located daemons all name their hot threads identically
//! (`sqz-ipc-svc0`, `fuse3-tpc3`, `sqz-meta0`, `sqz-timer`, …), so
//! pidstat/perf attribution on a fleet box cannot tell one mount's
//! service population from another's (the design row: "comm strings
//! collide across daemons; append `m{slot}` suffix for pidstat
//! attribution — the `fuse3-tpcN` lesson"). The fix is ONE shared core
//! (`squeezefs-ipc/src/comm_core.rs`, the `numa_core`/`thp.rs`
//! production-sharing precedent — the fuse3 fork `#[path]`-includes its
//! own copy, so the daemon seeds EVERY crate copy from
//! `writer_scope::set_mount_identity`, the KD-MW-2 identity point):
//! every per-mount thread name routes through `comm_name`, which
//! appends the derived suffix `m{:x}` (the mount slot's LOW HEX nibble
//! — cheap, derived, never a knob) inside the kernel's 15-char comm
//! budget (TASK_COMM_LEN − NUL), truncating the BASE, never the suffix
//! (a truncated base still greps by prefix; a truncated suffix is a
//! silent attribution lie).
//!
//! These pins were RED before `comm_core` landed.

use std::sync::atomic::{AtomicBool, Ordering};

/// One slot for the whole suite: tests within one binary share the
/// process-global tag word, so every test seeds the SAME identity
/// (low nibble 0xb ⇒ suffix "mb").
const SUITE_SLOT: u32 = 0x0000_00ab;
const SUITE_SUFFIX: &str = "mb";

fn seed_identity() {
    static SEEDED: AtomicBool = AtomicBool::new(false);
    if !SEEDED.swap(true, Ordering::SeqCst) {
        squeezefs::writer_scope::set_mount_identity(SUITE_SLOT, "/tmp/mount-comm-suite");
    }
}

/// The kernel comm budget: TASK_COMM_LEN (16) minus the NUL — anything
/// longer is silently truncated by the kernel, which is exactly how a
/// suffix gets eaten.
const COMM_BUDGET: usize = 15;

/// The 15-char budget, pinned over the tree's ACTUAL base literals at
/// their worst index widths (the derivation-tie-test discipline: a new
/// base that cannot carry the suffix inside the budget is a red test,
/// not a silent kernel truncation).
#[test]
fn comm_names_fit_the_kernel_budget_for_every_base_in_the_tree() {
    // Worst-width spellings of every base routed through comm_name
    // (indices at their derivation ceilings: svc/dd lanes ≤ 64,
    // fuse3-tpc ≤ 512 queues, meta lanes ≤ 2).
    let bases = [
        "sqz-ipc-svc63",
        "sqz-ipc-dd63",
        "sqz-nvme127",
        "sqz-ipc-accept",
        "sqz-ipc-accp",
        "sqz-ipc-reap",
        "sqz-ipc-ctl",
        "sqz-ipc-thp",
        "sqz-meta1",
        // C-2 journal lanes: one per writable volume, indexed by the
        // process-wide spawn count (a 4-digit index = 9,999 volumes
        // opened over the process life, far past any set).
        "sqz-jrnl9999",
        "sqz-timer",
        "sqz-fdwatch",
        "sqz-signal",
        "fuse3-tpc511",
        "fuse3-tpc-fallback",
        "fuse3-mount",
        "fuse3-unmount",
        "f3-ur0-511",
        "f3-ur-watch",
        "sqfs-supervise-probe",
    ];
    for base in bases {
        // Worst-case suffix nibble.
        let name = squeezefs_ipc::comm_core::comm_name_with(base, 0xffff_ffff);
        assert!(
            name.len() <= COMM_BUDGET,
            "{base:?} → {name:?} blows the {COMM_BUDGET}-char kernel comm budget \
             ({} chars): the kernel would silently truncate the mount suffix",
            name.len()
        );
        assert!(
            name.ends_with("mf"),
            "{base:?} → {name:?}: the suffix must survive verbatim at the END \
             (truncate the base, never the suffix)"
        );
    }
}

/// The suffix law: derived from the slot's low hex nibble, appended —
/// prefix greps (`starts_with("sqz-ipc-svc")`, the ingest-economy and
/// transport-ingress suites' own consumption) keep working; slot 0
/// (no mount: offline verbs, tests) keeps today's bare names
/// byte-identical.
#[test]
fn comm_suffix_is_derived_appended_and_slot_zero_is_bare() {
    assert_eq!(
        squeezefs_ipc::comm_core::comm_name_with("sqz-ipc-svc63", 0x0000_00a7),
        "sqz-ipc-svc63m7",
        "suffix = m + the slot's low hex nibble, appended"
    );
    assert!(
        squeezefs_ipc::comm_core::comm_name_with("sqz-ipc-svc63", 0x0000_00a7)
            .starts_with("sqz-ipc-svc"),
        "prefix consumers must survive the suffix"
    );
    assert_eq!(
        squeezefs_ipc::comm_core::comm_name_with("sqz-ipc-svc63", 0),
        "sqz-ipc-svc63",
        "no mount slot ⇒ bare base, byte-identical to the pre-PR names"
    );
    // Distinct low nibbles ⇒ distinct comms (the attribution point).
    assert_ne!(
        squeezefs_ipc::comm_core::comm_name_with("sqz-timer", 0x0000_000a),
        squeezefs_ipc::comm_core::comm_name_with("sqz-timer", 0x0000_000b),
    );
    // Over-budget base: truncated deterministically, suffix intact.
    assert_eq!(
        squeezefs_ipc::comm_core::comm_name_with("sqz-ipc-accept", 0x0000_000b),
        "sqz-ipc-accepmb",
    );
}

/// KD-MW-2's identity point seeds EVERY crate copy of the core: the
/// root/squeezefs-ipc copy (real dependency — one static) AND the fuse3
/// fork's `#[path]`-included copy (its own static, invisible to the
/// other two). A copy left unseeded is a fleet box where fuse3-tpc
/// lanes stay ambiguous while svc threads carry the tag.
#[test]
fn set_mount_identity_seeds_every_crate_copy_of_the_comm_tag() {
    seed_identity();
    assert_eq!(
        squeezefs_ipc::comm_core::comm_name("sqz-timer"),
        format!("sqz-timer{SUITE_SUFFIX}"),
        "the squeezefs-ipc copy must be seeded by set_mount_identity"
    );
    assert_eq!(
        fuse3::comm_core::comm_name("fuse3-tpc0"),
        format!("fuse3-tpc0{SUITE_SUFFIX}"),
        "the fuse3 fork's #[path] copy must be seeded by set_mount_identity"
    );
}

/// Live end-to-end pin (the ingest-economy /proc/self/task precedent):
/// after the identity is seeded, threads the daemon actually spawns
/// carry the suffix within budget — the sqz-meta pool and the sqz-timer
/// service (both lazily spawned, so the mount-time seeding order is
/// part of the contract under test).
#[test]
fn live_threads_carry_the_mount_suffix_within_budget() {
    seed_identity();
    // Force the lazily-spawned populations AFTER seeding (mount does the
    // same: set_mount_identity runs before any plane opens).
    squeezefs::meta_exec::spawn_meta("mount_comm_test", async {});
    drop(squeezefs_ipc::sqz_time::sleep(
        std::time::Duration::from_millis(1),
    ));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let comms: Vec<String> = std::fs::read_dir("/proc/self/task")
            .expect("procfs")
            .flatten()
            .filter_map(|t| std::fs::read_to_string(t.path().join("comm")).ok())
            .map(|c| c.trim_end().to_string())
            .collect();
        for c in &comms {
            assert!(
                c.len() <= COMM_BUDGET,
                "thread comm {c:?} exceeds the kernel budget"
            );
        }
        let meta_ok = comms
            .iter()
            .any(|c| c.starts_with("sqz-meta") && c.ends_with(SUITE_SUFFIX));
        let timer_ok = comms
            .iter()
            .any(|c| c.starts_with("sqz-timer") && c.ends_with(SUITE_SUFFIX));
        if meta_ok && timer_ok {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sqz-meta/sqz-timer threads never appeared with the mount suffix \
             {SUITE_SUFFIX:?}; comms: {comms:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
