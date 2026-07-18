//! PR 6 (N6) of the NVMe-oF dual-stack target-management program
//! (`docs/design-nvmeof-target-management.md`, Approved rev 4 — §6.9):
//! the sysfs fabric controller-state sampler behind the daemon
//! `fabric_*` `.stats` family and the `squeezefs status` `"Fabric"`
//! section.
//!
//! §6.8 zero-mock policy: everything here drives the REAL enumeration /
//! sampler / section code through the injection seam (an explicit
//! fixture sysfs tree on disk) — no env behavior forks.

use squeezefs::nvmeof::fabric::{
    device_base_names, enumerate_fabric_controllers, fabric_status_section, normalize_nvme_base,
    FabricController, FabricStatsSampler,
};
use std::path::Path;

/// Build one controller directory in a fixture `/sys/class/nvme` tree.
/// `attrs` are `(file_name, contents)` pairs; `children` are namespace
/// child directories (`nvme1n1`, `nvme1c1n1`, …).
fn mk_ctrl(root: &Path, name: &str, attrs: &[(&str, &str)], children: &[&str]) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    for (file, contents) in attrs {
        // Real sysfs attribute files carry a trailing newline — the
        // parser must trim.
        std::fs::write(dir.join(file), format!("{contents}\n")).unwrap();
    }
    for child in children {
        std::fs::create_dir_all(dir.join(child)).unwrap();
    }
}

fn tcp_ctrl(root: &Path, name: &str, state: &str, nqn: &str, addr: &str, children: &[&str]) {
    mk_ctrl(
        root,
        name,
        &[
            ("transport", "tcp"),
            ("state", state),
            ("subsysnqn", nqn),
            ("address", addr),
        ],
        children,
    );
}

// ===========================================================================
// Enumeration: state parse from fixture sysfs trees
// ===========================================================================

#[test]
fn test_enumerate_parses_fabric_controllers_and_skips_pcie() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    // A local PCIe controller (the user's real disk shape on the dev
    // box) must NEVER be counted as fabric.
    mk_ctrl(
        root,
        "nvme0",
        &[
            ("transport", "pcie"),
            ("state", "live"),
            ("subsysnqn", "nqn.2014.local:pcie-disk"),
            ("address", "0000:03:00.0"),
        ],
        &["nvme0n1"],
    );
    tcp_ctrl(
        root,
        "nvme1",
        "live",
        "nqn.2026-07.io.squeezefs:fideli-g2-meta",
        "traddr=127.0.0.1,trsvcid=4621",
        &["nvme1n1"],
    );
    tcp_ctrl(
        root,
        "nvme2",
        "connecting",
        "nqn.2026-07.io.squeezefs:fideli-g2-data",
        "traddr=127.0.0.1,trsvcid=4622",
        &["nvme2n1"],
    );
    // Non-controller clutter and a controller with no transport file
    // (unclassifiable → not fabric) must both be skipped, not errors.
    std::fs::create_dir_all(root.join("weird-entry")).unwrap();
    mk_ctrl(root, "nvme9", &[("state", "live")], &[]);

    let ctrls = enumerate_fabric_controllers(root);
    assert_eq!(
        ctrls.len(),
        2,
        "exactly the two fabric controllers enumerate: {ctrls:?}"
    );
    // Sorted by name for stable rendering.
    assert_eq!(ctrls[0].name, "nvme1");
    assert_eq!(ctrls[0].transport, "tcp");
    assert_eq!(ctrls[0].state, "live");
    assert!(ctrls[0].is_live());
    assert_eq!(
        ctrls[0].subsysnqn,
        "nqn.2026-07.io.squeezefs:fideli-g2-meta"
    );
    assert_eq!(ctrls[0].address, "traddr=127.0.0.1,trsvcid=4621");
    assert_eq!(ctrls[0].namespaces, vec!["nvme1n1".to_string()]);
    assert_eq!(ctrls[1].name, "nvme2");
    assert_eq!(ctrls[1].state, "connecting");
    assert!(!ctrls[1].is_live());
}

#[test]
fn test_enumerate_missing_state_file_reads_unknown_and_counts_not_live() {
    let t = tempfile::tempdir().unwrap();
    mk_ctrl(
        t.path(),
        "nvme4",
        &[
            ("transport", "rdma"),
            ("subsysnqn", "nqn.x:no-state"),
            ("address", "traddr=10.0.0.1,trsvcid=4420"),
        ],
        &[],
    );
    let ctrls = enumerate_fabric_controllers(t.path());
    assert_eq!(ctrls.len(), 1);
    assert_eq!(
        ctrls[0].state, "unknown",
        "a controller we cannot prove live reads 'unknown'"
    );
    assert!(!ctrls[0].is_live());

    let mut sampler = FabricStatsSampler::default();
    let s = sampler.observe(&ctrls);
    assert_eq!((s.controllers, s.not_live), (1, 1));
}

#[test]
fn test_missing_sysfs_root_yields_zero_sample_never_error() {
    // Boxes with no fabric controllers — including no /sys/class/nvme
    // at all (containers) — emit zeros, never an error (design §6.9).
    let t = tempfile::tempdir().unwrap();
    let missing = t.path().join("no-such-sysfs");
    let ctrls = enumerate_fabric_controllers(&missing);
    assert!(ctrls.is_empty());

    let mut sampler = FabricStatsSampler::default();
    let s = sampler.observe(&ctrls);
    assert_eq!(
        (s.controllers, s.not_live, s.reconnects_observed),
        (0, 0, 0),
        "the zero-case box emits a zero family"
    );
}

// ===========================================================================
// Sampler: gauges + the sampled-transition reconnect counter
// ===========================================================================

fn ctrl(name: &str, state: &str, nqn: &str, addr: &str) -> FabricController {
    FabricController {
        name: name.into(),
        transport: "tcp".into(),
        state: state.into(),
        subsysnqn: nqn.into(),
        address: addr.into(),
        namespaces: vec![format!("{name}n1")],
    }
}

const NQN_A: &str = "nqn.2026-07.io.squeezefs:share-a";
const NQN_B: &str = "nqn.2026-07.io.squeezefs:share-b";
const ADDR_A: &str = "traddr=127.0.0.1,trsvcid=54001";
const ADDR_B: &str = "traddr=127.0.0.1,trsvcid=54002";

#[test]
fn test_gauges_track_population_and_not_live_states() {
    let mut sampler = FabricStatsSampler::default();
    let s = sampler.observe(&[
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "connecting", NQN_B, ADDR_B),
        ctrl(
            "nvme3",
            "resetting",
            "nqn.x:c",
            "traddr=127.0.0.1,trsvcid=54003",
        ),
        ctrl(
            "nvme4",
            "deleting",
            "nqn.x:d",
            "traddr=127.0.0.1,trsvcid=54004",
        ),
    ]);
    assert_eq!(s.controllers, 4);
    assert_eq!(
        s.not_live, 3,
        "connecting/resetting/deleting all count not-live"
    );
    assert_eq!(s.reconnects_observed, 0, "first observations never count");
}

#[test]
fn test_reconnect_counts_observed_not_live_to_live_transition() {
    let mut sampler = FabricStatsSampler::default();
    // Beat 1: both live (baseline).
    let s = sampler.observe(&[
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "live", NQN_B, ADDR_B),
    ]);
    assert_eq!((s.not_live, s.reconnects_observed), (0, 0));
    // Beat 2: target died — both controllers reconnecting.
    let s = sampler.observe(&[
        ctrl("nvme1", "connecting", NQN_A, ADDR_A),
        ctrl("nvme2", "connecting", NQN_B, ADDR_B),
    ]);
    assert_eq!((s.not_live, s.reconnects_observed), (2, 0));
    // Beat 3: target restarted — both reattached. TWO observed
    // reconnects, one per controller identity.
    let s = sampler.observe(&[
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "live", NQN_B, ADDR_B),
    ]);
    assert_eq!((s.not_live, s.reconnects_observed), (0, 2));
    // Beat 4: steady state — no further counting.
    let s = sampler.observe(&[
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "live", NQN_B, ADDR_B),
    ]);
    assert_eq!(s.reconnects_observed, 0);
}

#[test]
fn test_first_observation_live_is_not_a_reconnect() {
    let mut sampler = FabricStatsSampler::default();
    // A controller first seen live (fresh connect at mount time, or a
    // sampler that started mid-steady-state) is NOT a reconnect.
    let s = sampler.observe(&[ctrl("nvme1", "live", NQN_A, ADDR_A)]);
    assert_eq!(s.reconnects_observed, 0);
    // A controller first seen not-live that then goes live IS one — the
    // transition was observed.
    let s = sampler.observe(&[
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "connecting", NQN_B, ADDR_B),
    ]);
    assert_eq!(s.reconnects_observed, 0);
    let s = sampler.observe(&[
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "live", NQN_B, ADDR_B),
    ]);
    assert_eq!(s.reconnects_observed, 1);
}

/// THE §6.9 caveat, pinned by name (design Issue 14 resolution / PR 6
/// row): `fabric_ctrl_reconnects` is a SAMPLED-transition counter —
/// sysfs exposes only instantaneous controller state (there is no
/// native cumulative reconnect counter to read), so flaps faster than
/// the stats cadence are UNDERCOUNTED by construction: a controller
/// that bounced live→connecting→live entirely between two beats counts
/// zero. Acceptable for the storm detector (measured storms run at 10 s
/// cadence for ~10 min); nobody gets to "fix" this against a kernel
/// counter that does not exist.
#[test]
fn test_fabric_reconnects_is_sampled_transition_counter_undercounts_bursts() {
    let mut sampler = FabricStatsSampler::default();
    // Beat 1: live.
    sampler.observe(&[ctrl("nvme1", "live", NQN_A, ADDR_A)]);
    // Between beats the controller ACTUALLY flapped twice
    // (live→connecting→live→connecting→live), but the sampler only sees
    // the next instantaneous state:
    // Beat 2: live again — the burst is invisible. Zero counted.
    let s = sampler.observe(&[ctrl("nvme1", "live", NQN_A, ADDR_A)]);
    assert_eq!(
        s.reconnects_observed, 0,
        "flaps faster than the cadence undercount to zero — sampled-transition law"
    );
    // A flap slow enough for one beat to land inside the down window
    // counts exactly once, however many kernel-level retries happened.
    sampler.observe(&[ctrl("nvme1", "connecting", NQN_A, ADDR_A)]);
    let s = sampler.observe(&[ctrl("nvme1", "live", NQN_A, ADDR_A)]);
    assert_eq!(
        s.reconnects_observed, 1,
        "one observed not-live→live transition = one reconnect, regardless of burst width"
    );
}

// ===========================================================================
// Identity tracking across controller renumbering
// ===========================================================================

#[test]
fn test_reconnect_tracked_across_controller_renumbering() {
    let mut sampler = FabricStatsSampler::default();
    // nvme3 serves the endpoint, live.
    sampler.observe(&[ctrl("nvme3", "live", NQN_A, ADDR_A)]);
    // Target dies; controller reconnects…
    let s = sampler.observe(&[ctrl("nvme3", "connecting", NQN_A, ADDR_A)]);
    assert_eq!(s.not_live, 1);
    // …then gives up entirely (ctrl_loss_tmo): the controller VANISHES
    // for a beat. Nothing to count yet.
    let s = sampler.observe(&[]);
    assert_eq!(
        (s.controllers, s.not_live, s.reconnects_observed),
        (0, 0, 0)
    );
    // The endpoint reappears under a NEW kernel name (renumbered) and is
    // live: same (transport, subsysnqn, address) identity — that is ONE
    // observed reconnect, not a fresh first-seen controller.
    let s = sampler.observe(&[ctrl("nvme7", "live", NQN_A, ADDR_A)]);
    assert_eq!(
        s.reconnects_observed, 1,
        "identity is (transport, subsysnqn, address) — never the nvmeN name"
    );
}

#[test]
fn test_vanished_while_live_then_reappearing_live_is_not_a_reconnect() {
    let mut sampler = FabricStatsSampler::default();
    sampler.observe(&[ctrl("nvme1", "live", NQN_A, ADDR_A)]);
    // Clean disconnect + later fresh reconnect, with no observed
    // not-live beat in between: no transition was OBSERVED, so nothing
    // counts (the undercount law again — absence while live carries no
    // evidence of a reconnect episode).
    sampler.observe(&[]);
    let s = sampler.observe(&[ctrl("nvme2", "live", NQN_A, ADDR_A)]);
    assert_eq!(s.reconnects_observed, 0);
}

#[test]
fn test_same_nqn_on_two_addresses_are_distinct_identities() {
    // Two portals to one subsystem (or two test ports reusing an NQN)
    // are distinct controllers: address is part of identity.
    let mut sampler = FabricStatsSampler::default();
    sampler.observe(&[
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "connecting", NQN_A, ADDR_B),
    ]);
    let s = sampler.observe(&[
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "live", NQN_A, ADDR_B),
    ]);
    assert_eq!(
        s.reconnects_observed, 1,
        "only the second portal reconnected"
    );
}

// ===========================================================================
// `squeezefs status` Fabric section
// ===========================================================================

#[test]
fn test_normalize_nvme_base_strips_partitions_only() {
    assert_eq!(normalize_nvme_base("nvme1n1"), "nvme1n1");
    assert_eq!(normalize_nvme_base("nvme1n1p2"), "nvme1n1");
    assert_eq!(normalize_nvme_base("nvme12n34p5"), "nvme12n34");
    // Non-namespace names pass through untouched.
    assert_eq!(normalize_nvme_base("sda1"), "sda1");
    assert_eq!(normalize_nvme_base("zram0"), "zram0");
}

#[test]
fn test_device_base_names_fall_back_to_raw_basename() {
    // Paths that do not resolve (file-backed sandboxes, dead symlinks)
    // fall back to their raw basename — matching later simply fails,
    // never errors.
    let bases = device_base_names(&[
        "/dev/does-not-exist/nvme1n1p1".to_string(),
        "/tmp/meta.img".to_string(),
    ]);
    assert_eq!(bases, vec!["nvme1n1".to_string(), "meta.img".to_string()]);
}

#[test]
fn test_fabric_section_absent_for_non_fabric_volume() {
    // A volume on a local file / PCIe disk gains NO Fabric section —
    // and (the hard zero-case) neither does any volume on a box with no
    // fabric controllers at all.
    let ctrls = [ctrl("nvme1", "live", NQN_A, ADDR_A)];
    assert!(fabric_status_section(&["sda1".to_string()], &ctrls).is_none());
    assert!(fabric_status_section(&["nvme0n1".to_string()], &[]).is_none());
}

#[test]
fn test_fabric_section_renders_family_fields_and_controller_rows() {
    let ctrls = [
        ctrl("nvme1", "live", NQN_A, ADDR_A),
        ctrl("nvme2", "connecting", NQN_B, ADDR_B),
        // A third fabric controller NOT backing this volume must not be
        // counted — the section is volume-scoped.
        ctrl(
            "nvme3",
            "live",
            "nqn.x:other",
            "traddr=10.0.0.9,trsvcid=4420",
        ),
    ];
    let section = fabric_status_section(&["nvme1n1".to_string(), "nvme2n1".to_string()], &ctrls)
        .expect("fabric-attached volume gains the section");

    assert_eq!(section["fabric_controllers"], 2);
    assert_eq!(section["fabric_ctrl_not_live"], 1);
    assert_eq!(
        section["fabric_ctrl_reconnects"], 0,
        "one-shot CLI sample observes no transitions by construction"
    );
    let rows = section["Controllers"].as_array().expect("controller rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["Device"], "nvme1n1");
    assert_eq!(rows[0]["Controller"], "nvme1");
    assert_eq!(rows[0]["Transport"], "tcp");
    assert_eq!(rows[0]["State"], "live");
    assert_eq!(rows[0]["SubsysNqn"], NQN_A);
    assert_eq!(rows[0]["Address"], ADDR_A);
    assert_eq!(rows[1]["State"], "connecting");
}

#[test]
fn test_fabric_section_matches_multipath_path_nodes_and_partitions() {
    // Native nvme multipath: the head node the user mounts is nvme1n1,
    // but the CONTROLLER directory carries the path node nvme1c3n1.
    let mut c = ctrl("nvme1", "live", NQN_A, ADDR_A);
    c.namespaces = vec!["nvme1c3n1".to_string()];
    let section = fabric_status_section(&["nvme1n1".to_string()], &[c.clone()])
        .expect("multipath path node backs the head node");
    assert_eq!(section["fabric_controllers"], 1);

    // Partitioned namespace: /dev/nvme1n1p1 (pre-stripped by
    // device_base_names) still matches.
    let section = fabric_status_section(&device_base_names(&["/dev/nvme1n1p1".to_string()]), &[c]);
    assert!(section.is_some());

    // A device is only counted once even when several path controllers
    // back it (dedup by identity is on controllers, not rows).
    let c1 = ctrl("nvme1", "live", NQN_A, ADDR_A);
    let mut c2 = ctrl("nvme2", "live", NQN_A, ADDR_B);
    c2.namespaces = vec!["nvme1c2n1".to_string()];
    let section =
        fabric_status_section(&["nvme1n1".to_string()], &[c1, c2]).expect("two path controllers");
    assert_eq!(
        section["fabric_controllers"], 2,
        "both path controllers render — they are distinct identities"
    );
}
