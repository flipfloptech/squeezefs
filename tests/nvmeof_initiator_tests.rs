//! Initiator device discovery vs CONFIG_NVME_MULTIPATH kernels
//! (2026-07-25 live 6-node bring-up bug): `squeezefs nvmeof
//! connect`/`list` reported the HIDDEN per-controller path node
//! `/dev/nvme0c0n1` instead of the user-visible multipath head node
//! `/dev/nvme0n1`, because the old walk scanned only the controller
//! class (`/sys/class/nvme/nvmeX/`) and matched the first child that
//! `starts_with(dev_name) && contains('n')` — trivially true for the
//! `nvmeXcYnZ` path nodes multipath kernels put there.
//!
//! Contract pinned here:
//! * a namespace BLOCK device name matches `^nvme\d+n\d+$` EXACTLY —
//!   any `c`-infixed controller-path name is never returned;
//! * discovery prefers the SUBSYSTEM class
//!   (`/sys/class/nvme-subsystem/*/subsysnqn` → its `nvme<X>n<Y>`
//!   child — the head node's instance number is the subsystem's, not
//!   necessarily any controller's), falling back to the controller
//!   class for non-multipath kernels with the same strict shape check;
//! * no name is ever FABRICATED by string concatenation (the old
//!   `format!("/dev/{}n1", ctrl)` fallback could mint a nonexistent
//!   node) — unresolvable is an honest `None`;
//! * iteration stays sorted-deterministic.
//!
//! §6.8 zero-mock policy: everything drives the REAL walkers through
//! the injection seam (explicit fixture sysfs roots on disk) — no env
//! behavior forks.

use std::path::Path;

use squeezefs::nvmeof::fabric::subsysnqn_of_namespace;
use squeezefs::nvmeof::initiator::{
    connect_target, connected_fabric_disks_at, disconnect_controllers_at, fabrics_connect_string,
    find_device_for_nqn_at, nvme_cli_connect_args, ConnectOptions,
};

const NQN_A: &str = "nqn.2026-07.io.squeezefs:share-aaaa";
const NQN_B: &str = "nqn.2026-07.io.squeezefs:share-bbbb";

/// Build one class-entry directory (`nvme3` controller or
/// `nvme-subsys0` subsystem) in a fixture sysfs tree. `attrs` are
/// `(file_name, contents)` pairs; `children` are namespace child
/// directories (`nvme0n1`, `nvme0c0n1`, …).
fn mk_entry(root: &Path, name: &str, attrs: &[(&str, &str)], children: &[&str]) {
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

/// Two fixture roots: (`/sys/class/nvme-subsystem`, `/sys/class/nvme`).
fn roots(t: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let subsys = t.path().join("nvme-subsystem");
    let nvme = t.path().join("nvme");
    std::fs::create_dir_all(&subsys).unwrap();
    std::fs::create_dir_all(&nvme).unwrap();
    (subsys, nvme)
}

// ===========================================================================
// find_device_for_nqn_at — the connect-time device resolution
// ===========================================================================

#[test]
fn test_non_multipath_layout_resolves_plain_namespace() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    // Non-multipath kernel: no subsystem entry, the controller dir
    // carries the plain namespace child.
    mk_entry(&nvme, "nvme0", &[("subsysnqn", NQN_A)], &["nvme0n1"]);

    let dev = find_device_for_nqn_at(&subsys, &nvme, NQN_A).expect("walk must not error");
    assert_eq!(
        dev.as_deref(),
        Some("/dev/nvme0n1"),
        "non-multipath controller-class fallback must resolve the plain namespace child"
    );
}

#[test]
fn test_multipath_layout_returns_head_node_never_controller_path_node() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    // The user's CONFIG_NVME_MULTIPATH shape: the controller dir holds
    // only the HIDDEN per-controller path node nvme0c0n1; the
    // user-visible head node nvme0n1 lives under the subsystem.
    mk_entry(&nvme, "nvme0", &[("subsysnqn", NQN_A)], &["nvme0c0n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_A)],
        &["nvme0n1"],
    );

    let dev = find_device_for_nqn_at(&subsys, &nvme, NQN_A).expect("walk must not error");
    assert_eq!(
        dev.as_deref(),
        Some("/dev/nvme0n1"),
        "multipath resolution must return the subsystem head node, never nvme0c0n1"
    );
}

#[test]
fn test_multipath_mismatched_instance_numbers_follow_the_subsystem() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    // Controller nvme3, subsystem nvme-subsys1 exposing nvme1n1: the
    // head node's instance number is the SUBSYSTEM's — a walk that
    // derives the name from the controller instance fabricates a
    // nonexistent /dev/nvme3n1.
    mk_entry(&nvme, "nvme3", &[("subsysnqn", NQN_A)], &["nvme3c3n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys1",
        &[("subsysnqn", NQN_A)],
        &["nvme1n1"],
    );

    let dev = find_device_for_nqn_at(&subsys, &nvme, NQN_A).expect("walk must not error");
    assert_eq!(
        dev.as_deref(),
        Some("/dev/nvme1n1"),
        "the head node name follows the subsystem, not the controller instance"
    );
}

#[test]
fn test_multi_subsystem_tree_returns_namespace_of_the_matching_nqn_only() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    mk_entry(&nvme, "nvme0", &[("subsysnqn", NQN_A)], &["nvme0c0n1"]);
    mk_entry(&nvme, "nvme1", &[("subsysnqn", NQN_B)], &["nvme1c1n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_A)],
        &["nvme0n1"],
    );
    mk_entry(
        &subsys,
        "nvme-subsys1",
        &[("subsysnqn", NQN_B)],
        &["nvme1n1"],
    );

    let dev_a = find_device_for_nqn_at(&subsys, &nvme, NQN_A).expect("walk must not error");
    let dev_b = find_device_for_nqn_at(&subsys, &nvme, NQN_B).expect("walk must not error");
    assert_eq!(dev_a.as_deref(), Some("/dev/nvme0n1"));
    assert_eq!(dev_b.as_deref(), Some("/dev/nvme1n1"));
}

#[test]
fn test_multi_namespace_subsystem_picks_sorted_first_deterministically() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_A)],
        &["nvme0n2", "nvme0n1"],
    );

    let dev = find_device_for_nqn_at(&subsys, &nvme, NQN_A).expect("walk must not error");
    assert_eq!(
        dev.as_deref(),
        Some("/dev/nvme0n1"),
        "multi-namespace subsystems resolve sorted-deterministically"
    );
}

#[test]
fn test_unresolvable_device_is_honest_none_never_a_fabricated_name() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    // Matching controller whose only child is a hidden path node, and
    // NO subsystem tree (the head node has not appeared yet): the old
    // code fabricated "/dev/nvme0n1" by concatenation — the honest
    // answer is None (the connect poll loop keeps waiting).
    mk_entry(&nvme, "nvme0", &[("subsysnqn", NQN_A)], &["nvme0c0n1"]);

    let dev = find_device_for_nqn_at(&subsys, &nvme, NQN_A).expect("walk must not error");
    assert_eq!(
        dev, None,
        "no strict-shape namespace visible ⇒ None — never a concatenated guess"
    );
}

#[test]
fn test_missing_roots_resolve_to_none_without_error() {
    let t = tempfile::tempdir().unwrap();
    let subsys = t.path().join("nvme-subsystem-absent");
    let nvme = t.path().join("nvme-absent");
    let dev = find_device_for_nqn_at(&subsys, &nvme, NQN_A).expect("absent roots tolerated");
    assert_eq!(dev, None);
}

// ===========================================================================
// connected_fabric_disks_at — the `list` walk
// ===========================================================================

#[test]
fn test_list_reports_head_node_on_multipath_and_honest_none_when_unresolvable() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    // Multipath controller: resolvable through the subsystem.
    mk_entry(
        &nvme,
        "nvme0",
        &[
            ("subsysnqn", NQN_A),
            ("address", "traddr=10.0.0.1,trsvcid=4420"),
        ],
        &["nvme0c0n1"],
    );
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_A)],
        &["nvme0n1"],
    );
    // Controller whose namespace has not materialized anywhere yet.
    mk_entry(
        &nvme,
        "nvme1",
        &[
            ("subsysnqn", NQN_B),
            ("address", "traddr=10.0.0.2,trsvcid=4420"),
        ],
        &[],
    );

    let disks = connected_fabric_disks_at(&subsys, &nvme).expect("walk must not error");
    assert_eq!(disks.len(), 2, "one row per controller with a subsysnqn");

    assert_eq!(disks[0].subnqn, NQN_A);
    assert_eq!(
        disks[0].device.as_deref(),
        Some("/dev/nvme0n1"),
        "list must report the multipath head node, never nvme0c0n1"
    );
    assert_eq!(disks[0].address, "traddr=10.0.0.1,trsvcid=4420");

    assert_eq!(disks[1].subnqn, NQN_B);
    assert_eq!(
        disks[1].device, None,
        "no visible namespace ⇒ honest None, never format!(\"/dev/{{}}n1\")"
    );
}

// ===========================================================================
// disconnect_controllers_at — the disconnect walk
// ===========================================================================

#[test]
fn test_disconnect_writes_delete_controller_on_every_matching_controller() {
    let t = tempfile::tempdir().unwrap();
    let (_subsys, nvme) = roots(&t);
    // Multipath: two path controllers serve one subsystem NQN — both
    // must receive the delete request; the foreign controller must not.
    mk_entry(
        &nvme,
        "nvme0",
        &[("subsysnqn", NQN_A), ("delete_controller", "")],
        &["nvme0c0n1"],
    );
    mk_entry(
        &nvme,
        "nvme1",
        &[("subsysnqn", NQN_A), ("delete_controller", "")],
        &["nvme0c1n1"],
    );
    mk_entry(
        &nvme,
        "nvme2",
        &[("subsysnqn", NQN_B), ("delete_controller", "")],
        &["nvme2c2n1"],
    );

    let deleted = disconnect_controllers_at(&nvme, NQN_A).expect("walk must not error");
    assert_eq!(
        deleted,
        vec!["nvme0".to_string(), "nvme1".to_string()],
        "both path controllers of the NQN, sorted, and only them"
    );
    assert_eq!(
        std::fs::read_to_string(nvme.join("nvme0/delete_controller")).unwrap(),
        "1"
    );
    assert_eq!(
        std::fs::read_to_string(nvme.join("nvme1/delete_controller")).unwrap(),
        "1"
    );
    assert_eq!(
        std::fs::read_to_string(nvme.join("nvme2/delete_controller")).unwrap(),
        // The fixture seeds attribute files with a trailing newline
        // (mk_entry) — anything else here means the walk wrote to a
        // foreign NQN's controller.
        "\n",
        "a foreign NQN's controller is never touched"
    );

    let none = disconnect_controllers_at(&nvme, "nqn.2026-07.io.squeezefs:share-missing")
        .expect("walk must not error");
    assert!(
        none.is_empty(),
        "no matching controller ⇒ empty, caller refuses loud"
    );
}

// ===========================================================================
// connect path/queue options (2026-07-25 live-cluster findings) — the
// injectable connect-argument builders behind `nvmeof connect
// --host-traddr/--host-iface/--nr-io-queues`: pass through EXACTLY when
// supplied, change NOTHING when absent
// ===========================================================================

#[test]
fn test_nvme_cli_args_base_shape_without_options() {
    let args = nvme_cli_connect_args("10.0.0.1", 4420, NQN_A, &ConnectOptions::default());
    assert_eq!(
        args,
        vec![
            "connect".to_string(),
            "-t".to_string(),
            "tcp".to_string(),
            "-a".to_string(),
            "10.0.0.1".to_string(),
            "-s".to_string(),
            "4420".to_string(),
            "-n".to_string(),
            NQN_A.to_string(),
        ],
        "no options supplied ⇒ the pre-existing argv exactly (no behavior change)"
    );
}

/// Each option passes through singly — and only itself.
#[test]
fn test_nvme_cli_args_pass_each_option_through_singly() {
    let with_traddr = nvme_cli_connect_args(
        "10.0.0.1",
        4420,
        NQN_A,
        &ConnectOptions {
            host_traddr: Some("10.0.0.2".to_string()),
            ..Default::default()
        },
    );
    assert!(
        with_traddr
            .windows(2)
            .any(|w| w == ["--host-traddr", "10.0.0.2"]),
        "--host-traddr passes through as a flag/value pair: {with_traddr:?}"
    );
    assert!(
        !with_traddr.iter().any(|a| a == "--host-iface")
            && !with_traddr.iter().any(|a| a == "--nr-io-queues"),
        "unsupplied options never appear: {with_traddr:?}"
    );

    let with_iface = nvme_cli_connect_args(
        "10.0.0.1",
        4420,
        NQN_A,
        &ConnectOptions {
            host_iface: Some("eth1".to_string()),
            ..Default::default()
        },
    );
    assert!(
        with_iface.windows(2).any(|w| w == ["--host-iface", "eth1"]),
        "--host-iface passes through: {with_iface:?}"
    );
    assert!(
        !with_iface.iter().any(|a| a == "--host-traddr"),
        "unsupplied options never appear: {with_iface:?}"
    );

    let with_queues = nvme_cli_connect_args(
        "10.0.0.1",
        4420,
        NQN_A,
        &ConnectOptions {
            nr_io_queues: Some(8),
            ..Default::default()
        },
    );
    assert!(
        with_queues.windows(2).any(|w| w == ["--nr-io-queues", "8"]),
        "--nr-io-queues passes through: {with_queues:?}"
    );
}

/// All three combined: base argv prefix unchanged, all three pairs present.
#[test]
fn test_nvme_cli_args_combined_options_extend_the_base() {
    let opts = ConnectOptions {
        host_traddr: Some("10.0.0.2".to_string()),
        host_iface: Some("eth1".to_string()),
        nr_io_queues: Some(4),
    };
    let args = nvme_cli_connect_args("10.0.0.1", 4420, NQN_A, &opts);
    let base = nvme_cli_connect_args("10.0.0.1", 4420, NQN_A, &ConnectOptions::default());
    assert_eq!(
        &args[..base.len()],
        &base[..],
        "options only APPEND — the base argv is a stable prefix"
    );
    for pair in [
        ["--host-traddr", "10.0.0.2"],
        ["--host-iface", "eth1"],
        ["--nr-io-queues", "4"],
    ] {
        assert!(
            args.windows(2).any(|w| w == pair),
            "combined options all pass through ({pair:?}): {args:?}"
        );
    }
}

#[test]
fn test_fabrics_string_base_shape_without_options() {
    let s = fabrics_connect_string(
        "10.0.0.1",
        4420,
        NQN_A,
        "nqn.2014-08.org.nvmexpress:uuid:host",
        "hostid-1",
        &ConnectOptions::default(),
    );
    assert_eq!(
        s,
        format!(
            "transport=tcp,traddr=10.0.0.1,trsvcid=4420,nqn={},hostnqn=nqn.2014-08.org.\
             nvmexpress:uuid:host,hostid=hostid-1",
            NQN_A
        ),
        "no options supplied ⇒ the pre-existing fabrics option string exactly"
    );
}

/// The raw `/dev/nvme-fabrics` write maps the flags to the kernel's
/// fabrics option names — each appended only when supplied.
#[test]
fn test_fabrics_string_maps_options_to_kernel_option_names() {
    let all = fabrics_connect_string(
        "10.0.0.1",
        4420,
        NQN_A,
        "hnqn",
        "hid",
        &ConnectOptions {
            host_traddr: Some("10.0.0.2".to_string()),
            host_iface: Some("eth1".to_string()),
            nr_io_queues: Some(8),
        },
    );
    for needle in ["host_traddr=10.0.0.2", "host_iface=eth1", "nr_io_queues=8"] {
        assert!(all.contains(needle), "must carry {needle}: {all}");
    }

    let only_iface = fabrics_connect_string(
        "10.0.0.1",
        4420,
        NQN_A,
        "hnqn",
        "hid",
        &ConnectOptions {
            host_iface: Some("eth1".to_string()),
            ..Default::default()
        },
    );
    assert!(
        only_iface.contains("host_iface=eth1")
            && !only_iface.contains("host_traddr=")
            && !only_iface.contains("nr_io_queues="),
        "unsupplied options never reach the fabrics write: {only_iface}"
    );
}

/// `nr_io_queues = 0` refuses at the library boundary BEFORE the root
/// check (zero I/O queues is not a connection — the flag bounds the
/// request, it never zeroes it). Unprivileged-safe by construction:
/// the refusal precedes every side effect.
#[test]
fn test_connect_target_refuses_zero_io_queues_before_anything() {
    let err = connect_target(
        "127.0.0.1",
        4420,
        NQN_A,
        &ConnectOptions {
            nr_io_queues: Some(0),
            ..Default::default()
        },
    )
    .expect_err("zero I/O queues must refuse");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let msg = err.to_string();
    assert!(
        msg.contains("--nr-io-queues") && !msg.contains("root"),
        "names the flag and fires before the root rung: {msg}"
    );
}

/// Ledger/list coherence: the `list` walk surfaces the controller's
/// sysfs `address` attribute VERBATIM — a traddr-pinned connection's
/// `host_traddr=`/`src_addr=` fields show up untouched.
#[test]
fn test_list_passes_pinned_source_address_through_verbatim() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let pinned = "traddr=10.0.0.1,trsvcid=4420,host_traddr=10.0.0.2,src_addr=10.0.0.2";
    mk_entry(
        &nvme,
        "nvme0",
        &[("subsysnqn", NQN_A), ("address", pinned)],
        &["nvme0n1"],
    );

    let disks = connected_fabric_disks_at(&subsys, &nvme).expect("walk must not error");
    assert_eq!(disks.len(), 1);
    assert_eq!(
        disks[0].address, pinned,
        "the pinned source address surfaces verbatim from sysfs"
    );
}

// ===========================================================================
// subsysnqn_of_namespace — the storage.rs PV-classification direction
// (namespace basename → NQN), same strict shape + subsystem-first rule
// ===========================================================================

#[test]
fn test_subsysnqn_of_namespace_follows_subsystem_on_mismatched_instances() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    mk_entry(&nvme, "nvme3", &[("subsysnqn", NQN_A)], &["nvme3c3n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys1",
        &[("subsysnqn", NQN_A)],
        &["nvme1n1"],
    );

    assert_eq!(
        subsysnqn_of_namespace(&subsys, &nvme, "nvme1n1").as_deref(),
        Some(NQN_A),
        "the head node's NQN lives on its subsystem, not on controller nvme1 (absent)"
    );
    // Partition suffixes normalize before the lookup (LVM PVs are often
    // partitions).
    assert_eq!(
        subsysnqn_of_namespace(&subsys, &nvme, "nvme1n1p2").as_deref(),
        Some(NQN_A)
    );
}

#[test]
fn test_subsysnqn_of_namespace_controller_fallback_and_strict_shape() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    // Non-multipath: controller instance == namespace instance.
    mk_entry(&nvme, "nvme0", &[("subsysnqn", NQN_B)], &["nvme0n1"]);

    assert_eq!(
        subsysnqn_of_namespace(&subsys, &nvme, "nvme0n1").as_deref(),
        Some(NQN_B),
        "non-multipath fallback reads the controller's subsysnqn"
    );
    // A hidden controller-path name is NOT a user-visible namespace
    // block device — the strict shape rule rejects it outright.
    assert_eq!(
        subsysnqn_of_namespace(&subsys, &nvme, "nvme0c0n1"),
        None,
        "cXnY names are rejected by the ^nvme\\d+n\\d+$ shape rule"
    );
    assert_eq!(subsysnqn_of_namespace(&subsys, &nvme, "sda1"), None);
}
