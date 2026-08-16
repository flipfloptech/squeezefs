//! PR-identity contracts — design-full-multi-writer.md rung 2
//! (`feat/mw-per-mount-hostnqn`): KD-MW-3 + §5.2 rules 1–2 + KD-MW-15.
//!
//! What is pinned here, red-first:
//!
//! * **KD-MW-3 pair-or-neither**: per-mount `hostnqn`/`hostid` resolve as a
//!   PAIR (`SQUEEZEFS_HOSTNQN`/`SQUEEZEFS_HOSTID` env, `-o hostnqn=`/`-o
//!   hostid=` mount options, option > env per field) — configuring exactly
//!   one refuses loud (a mismatched pair is how registrants alias).
//! * **KD-MW-15 `fabric_endpoint:` records**: durable per-DATA-volume
//!   connect coordinates on the KD-2 plane (ino-1 xattr records, versioned +
//!   checksummed, VAL-2-allowlist-invisible), written ONLY by `squeezefs
//!   config set-fabric-endpoints` (the `set-cache-paths` pattern) with
//!   `get-fabric-endpoints` for reads. **Mount reads, never overrides** — a
//!   mount flag naming coordinates is rejected loud naming the verb (the
//!   cache-path-policy precedent verbatim).
//! * **The §5.2 refusal ladder, decidable at every shape** (explicit
//!   identity set): (i) a shared/foreign-identity device path ⇒ refuse
//!   naming rule 2; (ii) a data volume with no `fabric_endpoint:` record ⇒
//!   refuse naming the verb — UNIFORMLY, including the edge shape where the
//!   operator pre-connected a DEDICATED data controller whose actual
//!   identity matches the configured pair ("explicit identity means
//!   daemon-owned data-plane connects, full stop"); (iii) the META plane is
//!   the ONE bootstrap exemption — operator-established connects, rule-2
//!   sysfs-verified (matching = accepted, mismatching = refused).
//! * **Rule 2 — actual controller identity from sysfs** under every device
//!   fd (`/sys/class/nvme/<ctrl>/hostnqn` + `hostid`), on the §6.8
//!   injection seam (explicit fixture roots, zero mocks) — and
//!   per-controller namespace resolution under OUR OWN identity, never a
//!   global device path.
//! * **`pr_registrant_shared` from ACTUAL identities**: another registrant
//!   under a different key carrying OUR wire host id means the device sees
//!   one host for two holders — computed from the device's answer, never
//!   from configured strings.
//!
//! The daemon-owned connect execution itself (root + real fabrics) is the
//! fidelity-rig leg's job (`tests/mw_two_registrants_leg.sh`); everything
//! here drives the pure decision cores and the real CLI verbs on
//! file-backed sandboxes — no root, no kernel modules, no skips.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use squeezefs::config_ops::parse_fabric_endpoint_spec;
use squeezefs::meta_backend::reservation::{
    registrant_identity_shared, HostIdentity, ReservationRegistrant, ReservationReport,
};
use squeezefs::nvmeof::initiator::{
    controller_identities_for_device_at, explicit_host_identity_from, fabric_ladder_verdict,
    find_device_for_nqn_under_identity_at, multipath_merged_shape_at, nvme_cli_connect_args,
    ConnectOptions, FabricPlane, LadderOutcome, MultipathMergedShape, VolumeShape,
};
use squeezefs::{fabric_endpoint_record_name, FabricEndpoint, FABRIC_ENDPOINT_RECORD_PREFIX};

const NQN_SUBSYS: &str = "nqn.2026-08.io.squeezefs:mwvol-aaaa";

fn id_a() -> HostIdentity {
    HostIdentity {
        hostnqn: "nqn.2014-08.org.nvmexpress:uuid:aaaaaaaa-0000-0000-0000-000000000001".to_string(),
        hostid: "aaaaaaaa-0000-0000-0000-000000000001".to_string(),
    }
}

fn id_b() -> HostIdentity {
    HostIdentity {
        hostnqn: "nqn.2014-08.org.nvmexpress:uuid:bbbbbbbb-0000-0000-0000-000000000002".to_string(),
        hostid: "bbbbbbbb-0000-0000-0000-000000000002".to_string(),
    }
}

// ===========================================================================
// KD-MW-3 — the pair-or-neither identity resolution (pure, injectable)
// ===========================================================================

#[test]
fn explicit_identity_absent_everywhere_is_none() {
    assert_eq!(
        explicit_host_identity_from(None, None, None).expect("neither = valid"),
        None,
        "no explicit identity anywhere ⇒ today's /etc/nvme posture verbatim"
    );
    // ENG-10 absence rule: empty / whitespace-only = unset.
    assert_eq!(
        explicit_host_identity_from(Some(""), Some("   "), None).expect("blank = absent"),
        None
    );
}

#[test]
fn explicit_identity_pair_from_env_resolves() {
    let a = id_a();
    let got = explicit_host_identity_from(Some(&a.hostnqn), Some(&a.hostid), None)
        .expect("a full pair is valid")
        .expect("pair present");
    assert_eq!(got, a);
}

#[test]
fn explicit_identity_one_without_the_other_refuses_naming_the_pair() {
    for (nqn, id, missing) in [
        (Some(id_a().hostnqn), None, "SQUEEZEFS_HOSTID"),
        (None, Some(id_a().hostid), "SQUEEZEFS_HOSTNQN"),
    ] {
        let err = explicit_host_identity_from(nqn.as_deref(), id.as_deref(), None)
            .expect_err("exactly one of the pair must refuse (pair-or-neither, KD-MW-3)");
        assert!(
            err.contains("SQUEEZEFS_HOSTNQN") && err.contains("SQUEEZEFS_HOSTID"),
            "the refusal names both halves of the pair: {err}"
        );
        assert!(
            err.contains(missing),
            "the refusal names the missing half {missing}: {err}"
        );
    }
}

#[test]
fn explicit_identity_mount_options_win_over_env_per_field() {
    let a = id_a();
    let b = id_b();
    // Option overrides env per field; the merged pair is still a pair.
    let opts = format!("rw,hostnqn={},noexec", b.hostnqn);
    let got = explicit_host_identity_from(Some(&a.hostnqn), Some(&a.hostid), Some(&opts))
        .expect("option+env merged pair is valid")
        .expect("pair present");
    assert_eq!(got.hostnqn, b.hostnqn, "-o hostnqn= wins over the env knob");
    assert_eq!(
        got.hostid, a.hostid,
        "unoverridden field keeps the env value"
    );
}

#[test]
fn explicit_identity_options_only_pair_and_options_only_half_refusal() {
    let b = id_b();
    let opts = format!("hostnqn={},hostid={}", b.hostnqn, b.hostid);
    let got = explicit_host_identity_from(None, None, Some(&opts))
        .expect("options-only pair is valid")
        .expect("pair present");
    assert_eq!(got, b);

    let err = explicit_host_identity_from(None, None, Some("hostnqn=nqn.2014-08.org.x:h"))
        .expect_err("-o hostnqn= without hostid refuses (pair-or-neither)");
    assert!(
        err.contains("hostnqn") && err.contains("hostid"),
        "names both option halves: {err}"
    );
}

#[test]
fn explicit_identity_empty_option_value_refuses_loud() {
    let err = explicit_host_identity_from(None, None, Some("hostnqn=,hostid=x"))
        .expect_err("an empty -o hostnqn= value is malformed, never silently absent");
    assert!(err.contains("hostnqn"), "names the malformed option: {err}");
}

#[test]
fn hostnqn_hostid_knobs_are_registered_eng10() {
    for key in ["SQUEEZEFS_HOSTNQN", "SQUEEZEFS_HOSTID"] {
        let knob = squeezefs::env_knobs::lookup(key)
            .unwrap_or_else(|| panic!("{key} must be a registered knob (ENG-10)"));
        assert!(
            matches!(knob.kind, squeezefs::env_knobs::Kind::Str),
            "{key} is a string knob"
        );
    }
}

// ===========================================================================
// KD-MW-15 — the fabric_endpoint: record (codec + name + VAL-2 invisibility)
// ===========================================================================

#[test]
fn fabric_endpoint_record_roundtrips() {
    let ep = FabricEndpoint {
        traddr: "127.0.0.1".to_string(),
        trsvcid: "54129".to_string(),
        subnqn: NQN_SUBSYS.to_string(),
    };
    let bytes = ep.encode();
    let back = FabricEndpoint::decode(&bytes).expect("own encoding decodes");
    assert_eq!(back, ep);
}

#[test]
fn fabric_endpoint_future_version_refuses_loud() {
    let ep = FabricEndpoint {
        traddr: "10.0.0.1".to_string(),
        trsvcid: "4420".to_string(),
        subnqn: NQN_SUBSYS.to_string(),
    };
    // A FUTURE binary writes a well-formed (checksummed) image under the
    // next version: bump the version byte and re-checksum, exactly as
    // that binary would.
    let mut bytes = ep.encode();
    bytes.truncate(bytes.len() - 8);
    bytes[0] = bytes[0].wrapping_add(1);
    let sum = xxhash_rust::xxh3::xxh3_64(&bytes);
    bytes.extend_from_slice(&sum.to_le_bytes());
    let err = FabricEndpoint::decode(&bytes)
        .expect_err("a future record version refuses loud (forward-only)");
    assert!(
        err.contains("version"),
        "the refusal names the version: {err}"
    );
}

#[test]
fn fabric_endpoint_torn_record_refuses_on_checksum() {
    let ep = FabricEndpoint {
        traddr: "10.0.0.1".to_string(),
        trsvcid: "4420".to_string(),
        subnqn: NQN_SUBSYS.to_string(),
    };
    let mut bytes = ep.encode();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    let err = FabricEndpoint::decode(&bytes).expect_err("a torn record refuses");
    assert!(
        err.contains("checksum"),
        "the refusal names the checksum: {err}"
    );
    assert!(
        FabricEndpoint::decode(&bytes[..4]).is_err(),
        "a truncated record refuses"
    );
}

#[test]
fn fabric_endpoint_record_name_is_the_vol_tag_kd5() {
    // KD-5: the tag decodes the durable `vol-{16 hex}` id verbatim; legacy
    // basename ids hash. Either way the name is `fabric_endpoint:{tag:016x}`.
    assert_eq!(
        fabric_endpoint_record_name("vol-0000000000000001"),
        "fabric_endpoint:0000000000000001"
    );
    let legacy = fabric_endpoint_record_name("nvme0n1");
    assert!(legacy.starts_with(FABRIC_ENDPOINT_RECORD_PREFIX));
    assert_eq!(
        legacy,
        fabric_endpoint_record_name("nvme0n1"),
        "legacy ids tag deterministically"
    );
}

#[test]
fn fabric_endpoint_records_are_val2_allowlist_invisible() {
    // VAL-2: the FUSE xattr screen is an ALLOWLIST (user./security./
    // trusted.) — an unprefixed internal family is invisible/EPERM through
    // FUSE by construction, no denylist edit needed.
    let name = fabric_endpoint_record_name("vol-00000000000000aa");
    for visible in ["user.", "security.", "trusted."] {
        assert!(
            !name.starts_with(visible),
            "'{name}' must stay outside the FUSE-visible allowlist"
        );
    }
}

// ===========================================================================
// The set-fabric-endpoints spec grammar: <vol-id>=<traddr>:<trsvcid>:<subnqn>
// ===========================================================================

#[test]
fn endpoint_spec_parses_with_colons_in_the_nqn() {
    let (vol, ep) = parse_fabric_endpoint_spec(&format!(
        "vol-0000000000000001=127.0.0.1:54129:{NQN_SUBSYS}"
    ))
    .expect("the spec grammar parses");
    assert_eq!(vol, "vol-0000000000000001");
    assert_eq!(ep.traddr, "127.0.0.1");
    assert_eq!(ep.trsvcid, "54129");
    assert_eq!(
        ep.subnqn, NQN_SUBSYS,
        "NQNs carry colons — split first-two only"
    );
}

#[test]
fn endpoint_spec_parses_bracketed_ipv6_traddr() {
    let (_vol, ep) =
        parse_fabric_endpoint_spec(&format!("vol-00000000000000ff=[::1]:4420:{NQN_SUBSYS}"))
            .expect("bracketed IPv6 traddr parses");
    assert_eq!(ep.traddr, "::1");
    assert_eq!(ep.trsvcid, "4420");
    assert_eq!(ep.subnqn, NQN_SUBSYS);
}

#[test]
fn endpoint_spec_malformed_shapes_refuse_loud() {
    for bad in [
        "vol-0000000000000001",                  // no '='
        "vol-0000000000000001=127.0.0.1",        // missing trsvcid+subnqn
        "vol-0000000000000001=127.0.0.1:4420",   // missing subnqn
        "=127.0.0.1:4420:nqn.x",                 // empty vol id
        "vol-0000000000000001=127.0.0.1::nqn.x", // empty trsvcid
        "vol-0000000000000001=:4420:nqn.x",      // empty traddr
        "vol-0000000000000001=[::1:4420:nqn.x",  // unterminated bracket
    ] {
        let err = parse_fabric_endpoint_spec(bad)
            .map(|_| ())
            .expect_err(&format!("'{bad}' must refuse"));
        let msg = format!("{err}");
        assert!(
            msg.contains("vol-id") || msg.contains("traddr") || msg.contains("trsvcid"),
            "'{bad}': the refusal teaches the grammar: {msg}"
        );
    }
}

// ===========================================================================
// Rule 2 — the ACTUAL controller identity under a device (sysfs, injectable)
// ===========================================================================

/// Build one class-entry directory in a fixture sysfs tree (the
/// nvmeof_initiator_tests fixture shape; real sysfs attrs carry a
/// trailing newline — the parser must trim).
fn mk_entry(root: &Path, name: &str, attrs: &[(&str, &str)], children: &[&str]) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    for (file, contents) in attrs {
        std::fs::write(dir.join(file), format!("{contents}\n")).unwrap();
    }
    for child in children {
        std::fs::create_dir_all(dir.join(child)).unwrap();
    }
}

fn roots(t: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let subsys = t.path().join("nvme-subsystem");
    let nvme = t.path().join("nvme");
    std::fs::create_dir_all(&subsys).unwrap();
    std::fs::create_dir_all(&nvme).unwrap();
    (subsys, nvme)
}

fn ctrl_attrs<'a>(subnqn: &'a str, id: &'a HostIdentity) -> Vec<(&'a str, &'a str)> {
    vec![
        ("subsysnqn", subnqn),
        ("hostnqn", id.hostnqn.as_str()),
        ("hostid", id.hostid.as_str()),
    ]
}

#[test]
fn actual_identity_reads_from_the_serving_controller() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0n1"]);

    let ids = controller_identities_for_device_at(&subsys, &nvme, "/dev/nvme0n1")
        .expect("walk must not error");
    assert_eq!(
        ids,
        vec![a],
        "the controller's hostnqn/hostid attrs verbatim"
    );
}

#[test]
fn actual_identity_multipath_head_node_collects_every_serving_controller() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    let b = id_b();
    // Two controllers (two identities) serve ONE subsystem; the visible
    // node is the multipath head under the subsystem. Rule 2 must see BOTH
    // identities — a shared head node can never verify as dedicated.
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0c0n1"]);
    mk_entry(&nvme, "nvme1", &ctrl_attrs(NQN_SUBSYS, &b), &["nvme1c1n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_SUBSYS)],
        &["nvme0n1"],
    );

    let ids = controller_identities_for_device_at(&subsys, &nvme, "/dev/nvme0n1")
        .expect("walk must not error");
    assert_eq!(
        ids.len(),
        2,
        "both serving controllers' identities: {ids:?}"
    );
    assert!(ids.contains(&a) && ids.contains(&b));
}

#[test]
fn actual_identity_non_nvme_and_identityless_controllers_read_honest_empty() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    // A PCIe/local controller carries no fabric hostnqn/hostid attrs.
    mk_entry(&nvme, "nvme0", &[("subsysnqn", NQN_SUBSYS)], &["nvme0n1"]);

    assert!(
        controller_identities_for_device_at(&subsys, &nvme, "/dev/nvme0n1")
            .expect("walk must not error")
            .is_empty(),
        "no fabric identity attrs ⇒ honest empty, never fabricated"
    );
    assert!(
        controller_identities_for_device_at(&subsys, &nvme, "/dev/zram0")
            .expect("non-NVMe tolerated")
            .is_empty(),
        "a non-NVMe path has no controller identity"
    );
    assert!(
        controller_identities_for_device_at(&subsys, &nvme, "/tmp/meta.bin")
            .expect("file path tolerated")
            .is_empty()
    );
}

// ===========================================================================
// Per-controller namespace resolution under OUR identity — never a global
// device path (§5.2 rule 1's resolution half)
// ===========================================================================

#[test]
fn identity_resolution_two_controllers_one_subnqn_pick_our_own() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    let b = id_b();
    // The two-co-located-mounts shape: two controllers to ONE subsystem,
    // each under its own identity, per-controller namespace nodes.
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0n1"]);
    mk_entry(&nvme, "nvme1", &ctrl_attrs(NQN_SUBSYS, &b), &["nvme1n1"]);

    let dev_a = find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, &a)
        .expect("walk must not error");
    let dev_b = find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, &b)
        .expect("walk must not error");
    assert_eq!(
        dev_a.as_deref(),
        Some("/dev/nvme0n1"),
        "mount A's controller"
    );
    assert_eq!(
        dev_b.as_deref(),
        Some("/dev/nvme1n1"),
        "mount B's controller"
    );
}

#[test]
fn identity_resolution_foreign_identity_only_is_honest_none() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let b = id_b();
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &b), &["nvme0n1"]);

    assert_eq!(
        find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, &id_a())
            .expect("walk must not error"),
        None,
        "a foreign controller's namespace is never resolved as ours"
    );
}

#[test]
fn identity_resolution_multipath_head_only_when_dedicated() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    // Dedicated multipath: every controller of the subsystem is OURS —
    // the head node is safely this mount's.
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0c0n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_SUBSYS)],
        &["nvme0n1"],
    );
    assert_eq!(
        find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, &a)
            .expect("walk must not error")
            .as_deref(),
        Some("/dev/nvme0n1"),
        "a head node whose every serving controller is ours resolves"
    );

    // Shared multipath: a foreign controller also serves the subsystem —
    // the head node round-robins BOTH identities and must never resolve.
    mk_entry(
        &nvme,
        "nvme1",
        &ctrl_attrs(NQN_SUBSYS, &id_b()),
        &["nvme1c1n1"],
    );
    assert_eq!(
        find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, &a)
            .expect("walk must not error"),
        None,
        "a head node shared with a foreign identity is never handed out"
    );
}

// ===========================================================================
// The connect plumbing carries the explicit identity (KD-MW-3)
// ===========================================================================

#[test]
fn nvme_cli_args_carry_the_explicit_identity_pair() {
    let a = id_a();
    let args = nvme_cli_connect_args(
        "127.0.0.1",
        54129,
        NQN_SUBSYS,
        &ConnectOptions {
            identity: Some(a.clone()),
            ..Default::default()
        },
    );
    assert!(
        args.windows(2)
            .any(|w| w == ["--hostnqn", a.hostnqn.as_str()]),
        "--hostnqn passes through: {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w == ["--hostid", a.hostid.as_str()]),
        "--hostid passes through: {args:?}"
    );

    let base = nvme_cli_connect_args("127.0.0.1", 54129, NQN_SUBSYS, &ConnectOptions::default());
    assert!(
        !base.iter().any(|s| s == "--hostnqn" || s == "--hostid"),
        "no explicit identity ⇒ the pre-existing argv exactly: {base:?}"
    );
}

// ===========================================================================
// The §5.2 refusal ladder — every decidable shape (pure decision core)
// ===========================================================================

fn data_shape<'a>(record: bool, actual: &'a [HostIdentity]) -> VolumeShape<'a> {
    VolumeShape {
        plane: FabricPlane::Data,
        descriptor: "vol-0000000000000001",
        has_endpoint_record: record,
        actual,
        multipath_merged: None,
    }
}

fn meta_shape(actual: &[HostIdentity]) -> VolumeShape<'_> {
    VolumeShape {
        plane: FabricPlane::Meta,
        descriptor: "/dev/nvme9n1",
        has_endpoint_record: false,
        actual,
        multipath_merged: None,
    }
}

#[test]
fn ladder_no_explicit_identity_accepts_every_shape_unchanged() {
    // Without explicit identity nothing changes: the actual identity feeds
    // the guarantee row / pr_registrant_shared, never a refusal.
    for shape in [
        data_shape(false, &[]),
        data_shape(true, &[]),
        meta_shape(&[]),
    ] {
        assert_eq!(
            fabric_ladder_verdict(None, &shape).expect("no explicit identity never refuses"),
            LadderOutcome::Accept
        );
    }
    let foreign = [id_b()];
    assert_eq!(
        fabric_ladder_verdict(None, &data_shape(false, &foreign)).expect("accepted"),
        LadderOutcome::Accept,
        "pre-connected devices stay the supported no-explicit-identity shape"
    );
}

#[test]
fn ladder_data_with_record_is_a_daemon_owned_connect() {
    let a = id_a();
    assert_eq!(
        fabric_ladder_verdict(Some(&a), &data_shape(true, &[]))
            .expect("record-covered data volume proceeds"),
        LadderOutcome::DaemonConnect,
        "rule 1: the daemon issues its OWN connect from the durable record"
    );
}

#[test]
fn ladder_shape_i_foreign_connected_data_device_refuses_naming_rule_2() {
    // The INERT-KNOB shape: identity configured, the volume's device rides
    // a controller someone else connected under a DIFFERENT identity — the
    // knob must refuse, never silently degrade to the shared connection.
    let a = id_a();
    let foreign = [id_b()];
    let err = fabric_ladder_verdict(Some(&a), &data_shape(false, &foreign))
        .expect_err("a foreign-identity device path refuses (rule 2)");
    assert!(err.contains("rule 2"), "names the rule: {err}");
    assert!(
        err.contains(&a.hostnqn) && err.contains(&id_b().hostnqn),
        "names both the configured and the actual identity: {err}"
    );
}

#[test]
fn ladder_shape_ii_missing_record_refuses_naming_the_verb() {
    let a = id_a();
    let err = fabric_ladder_verdict(Some(&a), &data_shape(false, &[]))
        .expect_err("explicit identity + no fabric_endpoint record refuses");
    assert!(
        err.contains("set-fabric-endpoints"),
        "the refusal names the verb: {err}"
    );
    assert!(
        err.contains("daemon-owned"),
        "the refusal states the law: {err}"
    );
}

#[test]
fn ladder_matching_preconnected_dedicated_data_controller_refuses_uniformly() {
    // The §5.2 edge: the operator pre-connected a DEDICATED data controller
    // whose actual identity MATCHES the configured pair. It still refuses —
    // "explicit identity means daemon-owned data-plane connects, full stop"
    // — because admitting it would make "was this controller really
    // dedicated?" a per-mount judgment the guarantee row cannot rest on.
    let a = id_a();
    let ours = [id_a()];
    let err = fabric_ladder_verdict(Some(&a), &data_shape(false, &ours))
        .expect_err("the matching pre-connected shape refuses UNIFORMLY");
    assert!(
        err.contains("set-fabric-endpoints"),
        "same remedy as every missing-record shape: {err}"
    );
    assert!(
        err.contains("full stop"),
        "states the uniform law verbatim: {err}"
    );
    // Uniformity proof: the matching edge and the non-NVMe shape produce
    // the SAME refusal class (the verb), not a special-cased acceptance.
    let non_nvme = fabric_ladder_verdict(Some(&a), &data_shape(false, &[]))
        .expect_err("missing record refuses");
    assert!(
        non_nvme.contains("set-fabric-endpoints"),
        "one rule keeps the ladder decidable: {non_nvme}"
    );
}

#[test]
fn ladder_bootstrap_meta_matching_identity_accepts() {
    let a = id_a();
    let ours = [id_a()];
    assert_eq!(
        fabric_ladder_verdict(Some(&a), &meta_shape(&ours))
            .expect("the bootstrap exemption: operator-established META connects"),
        LadderOutcome::Accept,
        "explicit identity + operator-connected META with MATCHING actual identity = accepted"
    );
}

#[test]
fn ladder_bootstrap_meta_mismatching_identity_refuses() {
    let a = id_a();
    let foreign = [id_b()];
    let err = fabric_ladder_verdict(Some(&a), &meta_shape(&foreign))
        .expect_err("a mismatching META controller identity refuses (rule 2)");
    assert!(err.contains("rule 2"), "names the rule: {err}");
}

#[test]
fn ladder_bootstrap_meta_shared_head_with_foreign_identity_refuses() {
    // A META head node served by our controller AND a foreign one: rule 2
    // sees both identities and the shared shape refuses (one of them is not
    // the configured pair).
    let a = id_a();
    let both = [id_a(), id_b()];
    assert!(
        fabric_ladder_verdict(Some(&a), &meta_shape(&both)).is_err(),
        "a shared-identity META device path refuses"
    );
}

#[test]
fn ladder_bootstrap_meta_without_controller_identity_accepts_unverified() {
    // File-backed / local-PCIe META volumes carry no fabric controller
    // identity: verification is INAPPLICABLE and says so loud (the warn
    // outcome) — never a silent pass-as-verified, never a refusal that
    // would ban explicit identity from every mixed substrate.
    let a = id_a();
    assert_eq!(
        fabric_ladder_verdict(Some(&a), &meta_shape(&[]))
            .expect("verification-inapplicable META accepted with the loud warning"),
        LadderOutcome::AcceptUnverified
    );
}

// ===========================================================================
// rung 5b — the multipath-MERGED shape (the rung-6 STOP finding): a
// subsystem head whose SERVING controllers carry >1 distinct hostnqn.
// On nvme_core.multipath=Y kernels the kernel groups fabric controllers
// by subsysnqn IGNORING hostnqn, so co-located identities merge under
// ONE shared head whose round-robin voids per-mount device fencing.
// Detection rides the same §6.8 sysfs injection seam; the rule-2
// refusal must NAME the shape and BOTH remedies (the sqz kernel's
// nvme_core.fabrics_host_scoped_subsystems=Y, and stock
// nvme_core.multipath=N as the documented workaround).
// ===========================================================================

#[test]
fn multipath_merged_shape_detects_two_hostnqns_under_one_head() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    let b = id_b();
    // The stock multipath=Y merged shape: two controllers under two
    // identities serve ONE subsystem; the visible node is the one head.
    // (No controller entries inside the subsystem dir — the attr-match
    // fallback arm.)
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0c0n1"]);
    mk_entry(&nvme, "nvme1", &ctrl_attrs(NQN_SUBSYS, &b), &["nvme1c1n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_SUBSYS)],
        &["nvme0n1"],
    );

    let merged = multipath_merged_shape_at(&subsys, &nvme, "/dev/nvme0n1")
        .expect("walk must not error")
        .expect("two distinct hostnqns under one head IS the merged shape");
    assert_eq!(merged.head, "/dev/nvme0n1");
    assert_eq!(merged.subsysnqn, NQN_SUBSYS);
    let mut want = vec![a.hostnqn.clone(), b.hostnqn.clone()];
    want.sort();
    assert_eq!(
        merged.hostnqns, want,
        "the distinct serving hostnqns, sorted (deterministic messages)"
    );
}

#[test]
fn multipath_merged_shape_single_identity_multipath_is_none() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    // Same-identity multipath (N paths, ONE hostnqn) is the healthy
    // upstream shape — never "merged".
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0c0n1"]);
    mk_entry(&nvme, "nvme1", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme1c1n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_SUBSYS)],
        &["nvme0n1"],
    );

    assert_eq!(
        multipath_merged_shape_at(&subsys, &nvme, "/dev/nvme0n1").expect("walk must not error"),
        None,
        "one hostnqn serving the head = dedicated, not merged"
    );
}

#[test]
fn multipath_merged_shape_scopes_to_the_subsystem_dirs_controller_entries() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    let b = id_b();
    // The sqz host-scoped kernel shape: TWO sibling subsystems share one
    // subsysnqn (grouped apart by hostnqn), each carrying its OWN
    // controller entry (the kernel's sysfs_create_link membership) and
    // its OWN head. A global subsysnqn-attr walk would conflate the
    // siblings and read every dedicated scoped head as "merged" — the
    // serving set MUST be the subsystem dir's controller entries when
    // they exist.
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0c0n1"]);
    mk_entry(&nvme, "nvme1", &ctrl_attrs(NQN_SUBSYS, &b), &["nvme1c1n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_SUBSYS)],
        &["nvme0n1"],
    );
    mk_entry(
        &subsys,
        "nvme-subsys1",
        &[("subsysnqn", NQN_SUBSYS)],
        &["nvme1n1"],
    );
    // Controller entries inside each subsystem dir (fixture stand-ins
    // for the kernel's controller links — dirs carrying the same attrs
    // the linked controller dir answers).
    for (sdir, ctrl, id) in [("nvme-subsys0", "nvme0", &a), ("nvme-subsys1", "nvme1", &b)] {
        let dir = subsys.join(sdir).join(ctrl);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, contents) in ctrl_attrs(NQN_SUBSYS, id) {
            std::fs::write(dir.join(file), format!("{contents}\n")).unwrap();
        }
    }

    assert_eq!(
        multipath_merged_shape_at(&subsys, &nvme, "/dev/nvme0n1").expect("walk must not error"),
        None,
        "a host-scoped sibling head is DEDICATED — membership is the \
         subsystem dir's controller entries, never the global attr match"
    );
    assert_eq!(
        multipath_merged_shape_at(&subsys, &nvme, "/dev/nvme1n1").expect("walk must not error"),
        None,
        "the other sibling likewise"
    );
}

#[test]
fn multipath_merged_shape_non_head_and_identityless_paths_are_none() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    // A per-controller namespace node (multipath=N shape): no subsystem
    // dir carries it — never merged.
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0n1"]);
    assert_eq!(
        multipath_merged_shape_at(&subsys, &nvme, "/dev/nvme0n1").expect("walk must not error"),
        None
    );
    // Non-NVMe paths are honest None.
    assert_eq!(
        multipath_merged_shape_at(&subsys, &nvme, "/dev/zram0").expect("walk must not error"),
        None
    );
    // A head served by identity-LESS controllers (local PCIe under
    // multipath=Y): zero hostnqns, never merged.
    mk_entry(
        &nvme,
        "nvme2",
        &[("subsysnqn", "pcie-subsys")],
        &["nvme2c2n1"],
    );
    mk_entry(
        &subsys,
        "nvme-subsys2",
        &[("subsysnqn", "pcie-subsys")],
        &["nvme2n1"],
    );
    assert_eq!(
        multipath_merged_shape_at(&subsys, &nvme, "/dev/nvme2n1").expect("walk must not error"),
        None,
        "identity-less controllers contribute no hostnqn — a PCIe head is never merged"
    );
}

// ===========================================================================
// rung 6b — scoped-sibling RESOLUTION (the 5b deferred item, landed with
// the guest validation): on the sqz host-scoped kernel
// (`nvme_core.fabrics_host_scoped_subsystems=Y`) two sibling subsystems
// share one subsysnqn, each carrying its OWN controller entries (the
// kernel's sysfs_create_link membership), its OWN `sqz_host_scope`, and
// its OWN head. Fixture shape = the REAL scoped sysfs shape observed in
// the rung-6b qemu guest (6.19.14-sqz, patch 0030) — see the
// vm-hostscope-validate leg's captured evidence; the design §5 sketch
// (subsystem-dir controller links as membership) HELD as observed.
// The pre-6b `foreign_serves` computation was subsysnqn-attr-GLOBAL and
// read a scoped sibling as foreign (honest None for BOTH identities);
// resolution must scope serving membership to the subsystem dir's
// controller entries, exactly like `multipath_merged_shape_at`.
// ===========================================================================

/// The sqz host-scoped sibling shape: two subsystems, ONE subsysnqn, one
/// controller + one head each, controller entries inside each subsystem
/// dir (fixture stand-ins for the kernel's controller links — dirs
/// carrying the same attrs the linked controller dir answers).
fn scoped_sibling_fixture(subsys: &Path, nvme: &Path, a: &HostIdentity, b: &HostIdentity) {
    mk_entry(nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, a), &["nvme0c0n1"]);
    mk_entry(nvme, "nvme1", &ctrl_attrs(NQN_SUBSYS, b), &["nvme1c1n1"]);
    mk_entry(
        subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_SUBSYS), ("sqz_host_scope", &a.hostnqn)],
        &["nvme0n1"],
    );
    mk_entry(
        subsys,
        "nvme-subsys1",
        &[("subsysnqn", NQN_SUBSYS), ("sqz_host_scope", &b.hostnqn)],
        &["nvme1n1"],
    );
    for (sdir, ctrl, id) in [("nvme-subsys0", "nvme0", a), ("nvme-subsys1", "nvme1", b)] {
        let dir = subsys.join(sdir).join(ctrl);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, contents) in ctrl_attrs(NQN_SUBSYS, id) {
            std::fs::write(dir.join(file), format!("{contents}\n")).unwrap();
        }
    }
}

#[test]
fn identity_resolution_scoped_siblings_resolve_their_own_heads() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    let b = id_b();
    scoped_sibling_fixture(&subsys, &nvme, &a, &b);

    // Each identity resolves ITS sibling's head — the co-located
    // two-identity shape 0030 exists to produce. A global attr walk
    // reads the sibling as foreign and answers None for both.
    assert_eq!(
        find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, &a)
            .expect("walk must not error")
            .as_deref(),
        Some("/dev/nvme0n1"),
        "identity A resolves its own scoped sibling's head"
    );
    assert_eq!(
        find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, &b)
            .expect("walk must not error")
            .as_deref(),
        Some("/dev/nvme1n1"),
        "identity B resolves its own scoped sibling's head"
    );
}

#[test]
fn identity_resolution_scoped_membership_keeps_the_dedicated_only_law() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    let b = id_b();
    // A subsystem dir whose controller ENTRIES carry both identities (a
    // merged dir even under linked membership — the stock shape seen
    // through links, or a misconfigured scope): the dedicated-only law
    // holds — the head is never handed out to either identity.
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0c0n1"]);
    mk_entry(&nvme, "nvme1", &ctrl_attrs(NQN_SUBSYS, &b), &["nvme1c1n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_SUBSYS)],
        &["nvme0n1"],
    );
    for (ctrl, id) in [("nvme0", &a), ("nvme1", &b)] {
        let dir = subsys.join("nvme-subsys0").join(ctrl);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, contents) in ctrl_attrs(NQN_SUBSYS, id) {
            std::fs::write(dir.join(file), format!("{contents}\n")).unwrap();
        }
    }
    for id in [&a, &b] {
        assert_eq!(
            find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, id)
                .expect("walk must not error"),
            None,
            "a head whose linked membership carries a foreign identity is never handed out"
        );
    }
}

#[test]
fn identity_resolution_scoped_identityless_entry_is_never_ours() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    // A subsystem dir whose serving set contains an identity-LESS
    // controller entry (no hostnqn/hostid attrs): half an identity is no
    // identity — the set cannot verify as dedicated to us.
    mk_entry(&nvme, "nvme0", &ctrl_attrs(NQN_SUBSYS, &a), &["nvme0c0n1"]);
    mk_entry(
        &subsys,
        "nvme-subsys0",
        &[("subsysnqn", NQN_SUBSYS)],
        &["nvme0n1"],
    );
    let ours = subsys.join("nvme-subsys0").join("nvme0");
    std::fs::create_dir_all(&ours).unwrap();
    for (file, contents) in ctrl_attrs(NQN_SUBSYS, &a) {
        std::fs::write(ours.join(file), format!("{contents}\n")).unwrap();
    }
    let bare = subsys.join("nvme-subsys0").join("nvme1");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::write(bare.join("subsysnqn"), format!("{NQN_SUBSYS}\n")).unwrap();

    assert_eq!(
        find_device_for_nqn_under_identity_at(&subsys, &nvme, NQN_SUBSYS, &a)
            .expect("walk must not error"),
        None,
        "an identity-less serving entry can never verify as ours (fail-closed)"
    );
}

#[test]
fn actual_identity_scoped_sibling_head_reads_only_its_dirs_controllers() {
    let t = tempfile::tempdir().unwrap();
    let (subsys, nvme) = roots(&t);
    let a = id_a();
    let b = id_b();
    scoped_sibling_fixture(&subsys, &nvme, &a, &b);

    // Rule 2's census under a scoped sibling's head must read ONLY the
    // sibling's own linked controllers — the global attr walk collects
    // the OTHER identity too and refuses a genuinely dedicated head
    // (exactly how the un-fixed walk would refuse identity B's mount in
    // the rung-6b guest).
    assert_eq!(
        controller_identities_for_device_at(&subsys, &nvme, "/dev/nvme0n1")
            .expect("walk must not error"),
        vec![a.clone()],
        "sibling 0's head carries identity A alone"
    );
    assert_eq!(
        controller_identities_for_device_at(&subsys, &nvme, "/dev/nvme1n1")
            .expect("walk must not error"),
        vec![b.clone()],
        "sibling 1's head carries identity B alone"
    );
}

fn merged_fixture() -> MultipathMergedShape {
    let mut hostnqns = vec![id_a().hostnqn, id_b().hostnqn];
    hostnqns.sort();
    MultipathMergedShape {
        head: "/dev/nvme0n1".to_string(),
        subsysnqn: NQN_SUBSYS.to_string(),
        hostnqns,
    }
}

#[test]
fn ladder_multipath_merged_refusal_names_the_shape_and_both_remedies() {
    let a = id_a();
    let both = [id_a(), id_b()];
    let merged = merged_fixture();
    let shape = VolumeShape {
        plane: FabricPlane::Meta,
        descriptor: "/dev/nvme0n1",
        has_endpoint_record: false,
        actual: &both,
        multipath_merged: Some(&merged),
    };
    let err = fabric_ladder_verdict(Some(&a), &shape)
        .expect_err("a merged head under explicit identity refuses");
    for needle in [
        "multipath-merged",
        "/dev/nvme0n1",
        NQN_SUBSYS,
        &id_a().hostnqn,
        &id_b().hostnqn,
        "nvme_core.fabrics_host_scoped_subsystems=Y",
        "nvme_core.multipath=N",
    ] {
        assert!(
            err.to_lowercase().contains(&needle.to_lowercase()),
            "the merged refusal must name the shape and BOTH remedies \
             verbatim — missing '{needle}' in: {err}"
        );
    }
}

#[test]
fn ladder_multipath_merged_refuses_even_when_actual_identities_cannot() {
    // The degenerate face: serving controllers carry hostnqn but no
    // hostid, so the actual-identity list is EMPTY (half an identity is
    // no identity) and the generic rule-2 foreign-find cannot fire. The
    // merged shape must refuse on its own — a shared head can never be
    // accepted because its identities were half-populated.
    let a = id_a();
    let merged = merged_fixture();
    let shape = VolumeShape {
        plane: FabricPlane::Meta,
        descriptor: "/dev/nvme0n1",
        has_endpoint_record: false,
        actual: &[],
        multipath_merged: Some(&merged),
    };
    assert!(
        fabric_ladder_verdict(Some(&a), &shape).is_err(),
        "merged refuses independent of the HostIdentity pair census"
    );
}

#[test]
fn ladder_multipath_merged_without_explicit_identity_accepts_unchanged() {
    // Without explicit identity nothing changes (the ladder's accept-all
    // law) — the guarantee-row gauges keep reading actual identities.
    let both = [id_a(), id_b()];
    let merged = merged_fixture();
    let shape = VolumeShape {
        plane: FabricPlane::Meta,
        descriptor: "/dev/nvme0n1",
        has_endpoint_record: false,
        actual: &both,
        multipath_merged: Some(&merged),
    };
    assert_eq!(
        fabric_ladder_verdict(None, &shape).expect("no explicit identity accepts"),
        LadderOutcome::Accept
    );
}

#[test]
fn ladder_multipath_merged_data_with_record_still_daemon_connects() {
    // Precedence unchanged: a record-covered DATA volume never opens the
    // shared path at all — DaemonConnect wins ahead of rule 2, and the
    // daemon's own post-connect resolution is what refuses on a merged
    // kernel (find_device_for_nqn_under_identity's dedicated-only law).
    let a = id_a();
    let both = [id_a(), id_b()];
    let merged = merged_fixture();
    let shape = VolumeShape {
        plane: FabricPlane::Data,
        descriptor: "vol-0000000000000001",
        has_endpoint_record: true,
        actual: &both,
        multipath_merged: Some(&merged),
    };
    assert_eq!(
        fabric_ladder_verdict(Some(&a), &shape).expect("record-covered data daemon-connects"),
        LadderOutcome::DaemonConnect
    );
}

// ===========================================================================
// pr_registrant_shared — computed from ACTUAL identities (the device's
// answer), never configured strings
// ===========================================================================

fn registrant(rkey: u64, host_id: &[u8]) -> ReservationRegistrant {
    ReservationRegistrant {
        rkey,
        host_id: host_id.to_vec(),
        holds_reservation: false,
    }
}

#[test]
fn registrant_shared_detects_a_second_key_under_our_wire_identity() {
    let ours: &[u8] = &[0xaa; 16];
    let report = ReservationReport {
        holder_key: None,
        registrants: vec![registrant(1, ours), registrant(2, ours)],
        rtype: 0,
    };
    assert!(
        registrant_identity_shared(&report, 1, ours),
        "two keys under ONE wire host id = the device sees one registrant for both mounts"
    );
}

#[test]
fn registrant_shared_is_quiet_for_sole_and_foreign_registrants() {
    let ours: &[u8] = &[0xaa; 16];
    let foreign: &[u8] = &[0xbb; 16];
    let sole = ReservationReport {
        holder_key: None,
        registrants: vec![registrant(1, ours)],
        rtype: 0,
    };
    assert!(!registrant_identity_shared(&sole, 1, ours));
    let with_foreign = ReservationReport {
        holder_key: None,
        registrants: vec![registrant(1, ours), registrant(2, foreign)],
        rtype: 0,
    };
    assert!(
        !registrant_identity_shared(&with_foreign, 1, ours),
        "a foreign host's registration is the healthy distinct-identity shape"
    );
    let empty_id = ReservationReport {
        holder_key: None,
        registrants: vec![registrant(1, ours), registrant(2, ours)],
        rtype: 0,
    };
    assert!(
        !registrant_identity_shared(&empty_id, 1, &[]),
        "an unreadable own wire id can never match anything (fail-closed)"
    );
}

// ===========================================================================
// CLI contracts — the real binary on file-backed sandboxes
// ===========================================================================

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch under ~/tmp (repo discipline: scratch lives in ~/tmp).
fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_mwident_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn run_with_deadline(mut cmd: Command, deadline: Duration, what: &str) -> (Output, Duration) {
    let start = Instant::now();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if start.elapsed() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{what} did not exit within {deadline:?} — must fail fast and loud");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    let elapsed = start.elapsed();
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("collect {what} output: {e}"));
    (out, elapsed)
}

/// Format one meta + one data volume (file-backed, unprivileged).
fn format_volume(base: &Path) -> PathBuf {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let out = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--force")
        .output()
        .expect("run squeezefs format");
    assert!(
        out.status.success(),
        "format failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    meta
}

/// The volume ids `get-fabric-endpoints` reports for a fresh sandbox
/// (one line per data volume: `<vol-id>\t<endpoint|none>`).
fn get_endpoint_rows(meta: &Path) -> Vec<(String, String)> {
    let out = Command::new(bin())
        .arg("config")
        .arg("get-fabric-endpoints")
        .arg(format!("sqmeta://{}", meta.display()))
        .output()
        .expect("run get-fabric-endpoints");
    assert!(
        out.status.success(),
        "get-fabric-endpoints failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split_whitespace();
            (
                it.next().unwrap_or_default().to_string(),
                it.next().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn cli_set_and_get_fabric_endpoints_roundtrip_kd_mw_15() {
    let base = scratch("verbs");
    let meta = format_volume(&base);

    // A fresh set has volumes and no records.
    let rows = get_endpoint_rows(&meta);
    assert!(!rows.is_empty(), "the durable volume set lists");
    assert!(
        rows.iter().all(|(_, ep)| ep == "none"),
        "no records on a fresh set: {rows:?}"
    );
    let vol_id = rows[0].0.clone();

    // set-fabric-endpoints writes the durable record (the set-cache-paths
    // pattern: offline, guarded, journal-durable).
    let spec = format!("{vol_id}=127.0.0.1:54129:{NQN_SUBSYS}");
    let out = Command::new(bin())
        .arg("config")
        .arg("set-fabric-endpoints")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(&spec)
        .output()
        .expect("run set-fabric-endpoints");
    assert!(
        out.status.success(),
        "set-fabric-endpoints failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // get reads it back.
    let rows = get_endpoint_rows(&meta);
    let ep = &rows
        .iter()
        .find(|(v, _)| *v == vol_id)
        .expect("the volume still lists")
        .1;
    assert_eq!(
        ep,
        &format!("127.0.0.1:54129:{NQN_SUBSYS}"),
        "the durable record reads back verbatim"
    );
}

#[test]
fn cli_set_fabric_endpoints_unknown_volume_id_refuses_loud() {
    let base = scratch("unknown");
    let meta = format_volume(&base);
    let out = Command::new(bin())
        .arg("config")
        .arg("set-fabric-endpoints")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("vol-00000000000000ee=127.0.0.1:4420:{NQN_SUBSYS}"))
        .output()
        .expect("run set-fabric-endpoints");
    assert!(
        !out.status.success(),
        "an unknown volume id must refuse (coordinates for a volume the set does not have)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("vol-00000000000000ee"),
        "names the unknown id: {stderr}"
    );
}

#[test]
fn cli_mount_rejects_the_fabric_endpoints_flag_naming_the_verb() {
    // The cache-path-policy precedent verbatim: a mount flag naming fabric
    // coordinates is a LOUD, INSTANT error pointing at the admin verb —
    // mount READS the durable records, never overrides them.
    let base = scratch("mountflag");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();

    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(&mnt)
        .arg("--fabric-endpoints")
        .arg(format!("vol-0000000000000001=127.0.0.1:4420:{NQN_SUBSYS}"));
    let (out, elapsed) =
        run_with_deadline(cmd, Duration::from_secs(10), "mount --fabric-endpoints");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "mount --fabric-endpoints must FAIL; stderr:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "rejection must be instant, took {elapsed:?}"
    );
    assert!(
        stderr.contains("config set-fabric-endpoints"),
        "the error points at the admin verb: {stderr}"
    );
    assert!(
        !mnt.join(".stats").exists(),
        "a refused mount leaves nothing behind"
    );
}

#[test]
fn cli_mount_pair_or_neither_refusal_is_instant() {
    let base = scratch("pair");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();

    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(&mnt)
        .arg("-o")
        .arg(format!("hostnqn={}", id_a().hostnqn));
    let (out, elapsed) = run_with_deadline(cmd, Duration::from_secs(10), "mount half-pair");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "-o hostnqn= without hostid must refuse; stderr:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "instant, took {elapsed:?}"
    );
    assert!(
        stderr.contains("hostid"),
        "names the missing half: {stderr}"
    );
}

#[test]
fn cli_mount_explicit_identity_without_records_refuses_never_inert() {
    // The end-to-end never-inert pin: explicit identity on a volume set
    // whose data volume has NO fabric_endpoint record (a file-backed
    // sandbox) refuses loud naming the verb — the knob can never be
    // silently inert on a substrate rule 1 cannot bind to.
    let base = scratch("inert");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();

    for (label, env_pair, opt) in [
        (
            "env pair",
            Some((id_a().hostnqn, id_a().hostid)),
            String::new(),
        ),
        (
            "option pair",
            None,
            format!("hostnqn={},hostid={}", id_a().hostnqn, id_a().hostid),
        ),
    ] {
        let mut cmd = Command::new(bin());
        cmd.arg("mount")
            .arg(format!("sqmeta://{}", meta.display()))
            .arg(&mnt);
        if let Some((nqn, hid)) = &env_pair {
            cmd.env("SQUEEZEFS_HOSTNQN", nqn)
                .env("SQUEEZEFS_HOSTID", hid);
        }
        if !opt.is_empty() {
            cmd.arg("-o").arg(&opt);
        }
        let (out, elapsed) =
            run_with_deadline(cmd, Duration::from_secs(30), "mount explicit identity");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "{label}: explicit identity + no record must refuse (never inert); stderr:\n{stderr}"
        );
        assert!(
            elapsed < Duration::from_secs(20),
            "{label}: the refusal is a mount-time gate, took {elapsed:?}"
        );
        assert!(
            stderr.contains("set-fabric-endpoints"),
            "{label}: names the verb: {stderr}"
        );
        assert!(
            !mnt.join(".stats").exists(),
            "{label}: a refused mount leaves nothing behind"
        );
    }
}
