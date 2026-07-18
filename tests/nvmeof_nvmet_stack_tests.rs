//! `NvmetStack` contract tests (`docs/design-nvmeof-target-management.md`
//! §6.6 + §6.4, landed by PR 2/N2) — the rebuilt kernel-nvmet configfs
//! path, driven through the finalized `TargetStack` trait against an
//! **injected configfs root** (the §6.8/A5-sanctioned seam: every code
//! path executed is the production path pointed at a different location;
//! no env forks, no mocks, no root). Real-kernel semantics are proven by
//! the root-tier fidelity legs; this tier pins path composition, the
//! ledger intent protocol, the live-state duplicate guard's runbook
//! refusals, ns identity (`device_uuid` = recorded `ns_uuid`,
//! re-presented by restore), and teardown-to-zero-residue.

use std::fs;
use std::path::{Path, PathBuf};

use squeezefs::nvmeof::ledger::Ledger;
use squeezefs::nvmeof::nvmet::{NvmetStack, NVMET_PORT_ID_BASE_DEFAULT};
use squeezefs::nvmeof::stack::{
    Listener, PreflightOp, RestoreOutcome, ShareRequest, ShareState, TargetStack,
};
use squeezefs::nvmeof::StackKind;

const BASE: u32 = NVMET_PORT_ID_BASE_DEFAULT;

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state_dir: PathBuf,
    stack: NvmetStack,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("nvmet");
    // The "mounted tree" shape: configfs default groups pre-exist.
    fs::create_dir_all(root.join("subsystems")).expect("mkdir subsystems");
    fs::create_dir_all(root.join("ports")).expect("mkdir ports");
    fs::create_dir_all(root.join("hosts")).expect("mkdir hosts");
    let state_dir = dir.path().join("state");
    let stack = NvmetStack::new(root.clone(), Ledger::new(&state_dir), BASE);
    Rig {
        _dir: dir,
        root,
        state_dir,
        stack,
    }
}

fn req(subnqn: &str, backing: &str, ns_uuid: &str, listeners: &[(&str, u16)]) -> ShareRequest {
    ShareRequest {
        subnqn: subnqn.to_string(),
        backing_path: backing.to_string(),
        backing_canonical: backing.to_string(),
        nsid: None,
        ns_uuid: ns_uuid.to_string(),
        listeners: listeners
            .iter()
            .map(|(ip, port)| Listener {
                ip: ip.to_string(),
                port: *port,
                nvmet_port_id: None,
            })
            .collect(),
        allow_hosts: Vec::new(),
    }
}

const UUID_A: &str = "e2b1c9a4-52d1-4a08-9f31-7c2b8d1e0aa1";
const UUID_B: &str = "0f0e0d0c-0b0a-4a09-8807-060504030201";

/// Hand-build a live (foreign-style) nvmet subsystem in the injected root
/// — the shape a pre-rebuild binary or another tenant leaves behind.
fn plant_live_subsystem(root: &Path, nqn: &str, device: &str, uuid: Option<&str>) {
    let ns = root
        .join("subsystems")
        .join(nqn)
        .join("namespaces")
        .join("1");
    fs::create_dir_all(&ns).expect("plant ns");
    fs::write(ns.join("device_path"), device).expect("plant device_path");
    if let Some(u) = uuid {
        fs::write(ns.join("device_uuid"), u).expect("plant device_uuid");
    }
    fs::write(ns.join("enable"), "1").expect("plant enable");
}

// ---------------------------------------------------------------------------
// share
// ---------------------------------------------------------------------------

/// The rebuilt share path: subsystem + fixed namespace index 1 with
/// `device_uuid` stamped from the request's `ns_uuid`, port objects from
/// the reserved range with attrs + subsystem symlink, and a finalized
/// ledger record carrying the §6.4 nvmet presence shape (`ns_uuid`
/// recorded, `nsid` never recorded, per-listener `nvmet_port_id`
/// recorded).
#[test]
fn test_nvmet_share_composes_configfs_and_records_identity() {
    let r = rig();
    let request = req(
        "nqn.2026-07.io.squeezefs:share-t1",
        "/dev/null",
        UUID_A,
        &[("127.0.0.1", 4420), ("127.0.0.1", 4421)],
    );
    let record = r.stack.share(&request).expect("share must succeed");

    // Ledger record: finalized active, nvmet presence rules.
    assert_eq!(record.state, ShareState::Active);
    assert_eq!(record.stack, StackKind::Nvmet);
    assert_eq!(record.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(
        record.nsid, None,
        "nsid is never recorded on nvmet (structural index 1)"
    );
    assert_eq!(record.bdev_name, None);
    assert_eq!(record.ptpl_file, None);
    assert_eq!(
        record.loop_device, None,
        "/dev/null is not a regular file — no loop"
    );

    // Namespace: index structurally fixed at 1; identity stamped.
    let sub = r.root.join("subsystems").join(&record.subnqn);
    let ns = sub.join("namespaces").join("1");
    assert_eq!(
        fs::read_to_string(ns.join("device_path"))
            .expect("device_path")
            .trim(),
        "/dev/null"
    );
    assert_eq!(
        fs::read_to_string(ns.join("device_uuid"))
            .expect("device_uuid")
            .trim(),
        UUID_A,
        "device_uuid must be stamped with the recorded ns_uuid"
    );
    assert_eq!(
        fs::read_to_string(ns.join("enable"))
            .expect("enable")
            .trim(),
        "1"
    );
    assert!(
        !sub.join("namespaces").join("2").exists(),
        "one namespace per subsystem — the structural convention"
    );
    // No fake bookkeeping files in configfs (§6.4 law 5).
    assert!(
        !sub.join("associated_loop_device").exists(),
        "the configfs fake-file bookkeeping is dead"
    );
    assert_eq!(
        fs::read_to_string(sub.join("attr_allow_any_host"))
            .expect("allow_any")
            .trim(),
        "1"
    );

    // Ports: one configfs port object per (ip, port) listener, ids from
    // the reserved range, recorded in the ledger.
    assert_eq!(record.listeners.len(), 2);
    let mut ids = Vec::new();
    for l in &record.listeners {
        let id = l
            .nvmet_port_id
            .expect("per-listener port id must be recorded");
        assert!(
            (BASE..BASE + 1000).contains(&id),
            "id {id} in reserved range"
        );
        ids.push(id);
        let p = r.root.join("ports").join(id.to_string());
        assert_eq!(
            fs::read_to_string(p.join("addr_trtype"))
                .expect("trtype")
                .trim(),
            "tcp"
        );
        assert_eq!(
            fs::read_to_string(p.join("addr_traddr"))
                .expect("traddr")
                .trim(),
            l.ip
        );
        assert_eq!(
            fs::read_to_string(p.join("addr_trsvcid"))
                .expect("trsvcid")
                .trim(),
            l.port.to_string()
        );
        assert_eq!(
            fs::read_to_string(p.join("addr_adrfam"))
                .expect("adrfam")
                .trim(),
            "ipv4"
        );
        assert!(
            p.join("subsystems").join(&record.subnqn).exists(),
            "subsystem symlink under port {id}"
        );
    }
    ids.dedup();
    assert_eq!(ids.len(), 2, "distinct listeners get distinct port objects");

    // The record round-trips through the ledger.
    let loaded = Ledger::new(&r.state_dir)
        .find(&record.subnqn)
        .expect("ledger load")
        .expect("record present");
    assert_eq!(loaded, record);
}

/// `resv_enable` is written (1) before enable whenever the knob exists —
/// pre-seeding the knob file in the injected root exercises the
/// knob-present arm (real kernels pre-create it; the root tier proves
/// that end).
#[test]
fn test_nvmet_share_sets_resv_enable_when_knob_exists() {
    let r = rig();
    let nqn = "nqn.2026-07.io.squeezefs:share-resv";
    let ns = r
        .root
        .join("subsystems")
        .join(nqn)
        .join("namespaces")
        .join("1");
    fs::create_dir_all(&ns).expect("pre-create ns dir");
    fs::write(ns.join("resv_enable"), "0").expect("seed knob");

    r.stack
        .share(&req(nqn, "/dev/null", UUID_A, &[("127.0.0.1", 4430)]))
        .expect("share");
    assert_eq!(
        fs::read_to_string(ns.join("resv_enable"))
            .expect("resv_enable")
            .trim(),
        "1",
        "resv_enable must be written 1 (before enable) when the knob exists"
    );
}

/// `--allow-host` wiring: allow_any flips to 0, host objects are created,
/// and the subsystem's allowed_hosts carries one link per host NQN; the
/// hosts ride the ledger record so restore can re-present them.
#[test]
fn test_nvmet_share_allow_host_wiring() {
    let r = rig();
    let mut request = req(
        "nqn.2026-07.io.squeezefs:share-ah",
        "/dev/null",
        UUID_A,
        &[("127.0.0.1", 4440)],
    );
    request.allow_hosts = vec![
        "nqn.2014-08.org.nvmexpress:uuid:host-1".to_string(),
        "nqn.2014-08.org.nvmexpress:uuid:host-2".to_string(),
    ];
    let record = r.stack.share(&request).expect("share");
    assert_eq!(record.allow_hosts, request.allow_hosts);

    let sub = r.root.join("subsystems").join(&record.subnqn);
    assert_eq!(
        fs::read_to_string(sub.join("attr_allow_any_host"))
            .expect("attr")
            .trim(),
        "0",
        "an allowlisted share must not serve allow-any"
    );
    for h in &request.allow_hosts {
        assert!(r.root.join("hosts").join(h).exists(), "host object {h}");
        assert!(
            sub.join("allowed_hosts").join(h).exists(),
            "allowed_hosts link {h}"
        );
    }
}

// ---------------------------------------------------------------------------
// duplicate guard (live-state) — the refusal message is the runbook
// ---------------------------------------------------------------------------

/// A live FOREIGN (unledgered) subsystem serving the same backing refuses
/// the share; the message names the holder NQN, its classification, and
/// the manual configfs removal steps (§6.4: removal-first is the only
/// re-share path while the old object serves).
#[test]
fn test_nvmet_share_refuses_foreign_live_duplicate_backing_with_runbook() {
    let r = rig();
    plant_live_subsystem(
        &r.root,
        "nqn.2026-06.io.squeezefs:pre-rebuild-x",
        "/dev/null",
        None,
    );

    let err = r
        .stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-dup",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect_err("live duplicate backing must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("nqn.2026-06.io.squeezefs:pre-rebuild-x"),
        "refusal must name the live holder: {msg}"
    );
    assert!(
        msg.contains("foreign"),
        "refusal must classify the unledgered holder as foreign: {msg}"
    );
    assert!(
        msg.contains("rmdir") && msg.contains("subsystems"),
        "refusal must hand over the manual configfs removal steps: {msg}"
    );

    // Nothing was mutated and no intent record was stranded.
    assert!(
        Ledger::new(&r.state_dir)
            .find("nqn.2026-07.io.squeezefs:share-dup")
            .expect("load")
            .is_none(),
        "the guard fires before the intent record is written"
    );
}

/// A ledgered holder (any intent state) refuses with the `unshare` exit
/// and its classification named.
#[test]
fn test_nvmet_share_refuses_ledgered_duplicate_with_unshare_exit() {
    let r = rig();
    r.stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-holder",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect("first share");

    let err = r
        .stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-second",
            "/dev/null",
            UUID_B,
            &[("127.0.0.1", 4421)],
        ))
        .expect_err("duplicate backing must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("nqn.2026-07.io.squeezefs:share-holder"),
        "must name the holder: {msg}"
    );
    assert!(
        msg.contains("unshare"),
        "ledgered holder's exit is unshare: {msg}"
    );
}

/// A live unledgered subsystem already squatting the requested NQN
/// refuses loud (we never adopt silently and never clobber).
#[test]
fn test_nvmet_share_refuses_live_nqn_squat() {
    let r = rig();
    plant_live_subsystem(
        &r.root,
        "nqn.2026-07.io.squeezefs:share-squat",
        "/dev/zero",
        None,
    );
    let err = r
        .stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-squat",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect_err("live NQN squat must refuse");
    assert!(err
        .to_string()
        .contains("nqn.2026-07.io.squeezefs:share-squat"));
}

// ---------------------------------------------------------------------------
// port allocator through the stack
// ---------------------------------------------------------------------------

/// A foreign port squatting the deterministic candidate id is skipped —
/// never reused, never modified — and the share lands on the next id.
#[test]
fn test_nvmet_share_skips_foreign_port_squat() {
    let r = rig();
    // tcp:127.0.0.1:4420 hashes to BASE+773 (frozen vector).
    let squat = r.root.join("ports").join((BASE + 773).to_string());
    fs::create_dir_all(squat.join("subsystems")).expect("squat dir");
    fs::write(squat.join("addr_trtype"), "tcp").expect("squat attr");
    fs::write(squat.join("addr_traddr"), "10.99.99.99").expect("squat attr");
    fs::write(squat.join("addr_trsvcid"), "4420").expect("squat attr");
    fs::write(squat.join("addr_adrfam"), "ipv4").expect("squat attr");

    let record = r
        .stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-sq",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect("share must probe past the squatter");
    assert_eq!(record.listeners[0].nvmet_port_id, Some(BASE + 774));

    // The foreign port is untouched.
    assert_eq!(
        fs::read_to_string(squat.join("addr_traddr"))
            .expect("squat attr")
            .trim(),
        "10.99.99.99"
    );
    assert!(squat.exists());
}

/// Two shares with the same (ip, port) listener share one port object
/// (deterministic candidate + ours-and-matching reuse); teardown removes
/// the port only when it goes link-free.
#[test]
fn test_nvmet_shared_port_reuse_and_lastout_removal() {
    let r = rig();
    let rec1 = r
        .stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-p1",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect("share 1");
    let rec2 = r
        .stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-p2",
            "/dev/zero",
            UUID_B,
            &[("127.0.0.1", 4420)],
        ))
        .expect("share 2");
    let id1 = rec1.listeners[0].nvmet_port_id.expect("id1");
    let id2 = rec2.listeners[0].nvmet_port_id.expect("id2");
    assert_eq!(
        id1, id2,
        "same (ip,port) listener must reuse one port object"
    );

    let port_dir = r.root.join("ports").join(id1.to_string());
    r.stack.unshare(&rec1).expect("unshare 1");
    assert!(
        port_dir.exists(),
        "port with a remaining ledgered link must survive"
    );
    assert!(!port_dir.join("subsystems").join(&rec1.subnqn).exists());
    assert!(port_dir.join("subsystems").join(&rec2.subnqn).exists());

    r.stack.unshare(&rec2).expect("unshare 2");
    assert!(
        !port_dir.exists(),
        "link-free recorded port must be removed"
    );
}

// ---------------------------------------------------------------------------
// unshare
// ---------------------------------------------------------------------------

/// Full teardown: child→parent removal, recorded port removed when
/// link-free, ledger record deleted — zero residue in the injected root.
#[test]
fn test_nvmet_unshare_zero_residue_and_ledger_delete() {
    let r = rig();
    let mut request = req(
        "nqn.2026-07.io.squeezefs:share-zr",
        "/dev/null",
        UUID_A,
        &[("127.0.0.1", 4420), ("127.0.0.1", 4421)],
    );
    request.allow_hosts = vec!["nqn.2014-08.org.nvmexpress:uuid:zr-host".to_string()];
    let record = r.stack.share(&request).expect("share");

    r.stack.unshare(&record).expect("unshare");

    assert!(
        !r.root.join("subsystems").join(&record.subnqn).exists(),
        "subsystem removed"
    );
    for l in &record.listeners {
        let id = l.nvmet_port_id.expect("id");
        assert!(
            !r.root.join("ports").join(id.to_string()).exists(),
            "recorded link-free port {id} removed"
        );
    }
    assert!(
        !r.root.join("hosts").join(&request.allow_hosts[0]).exists(),
        "now-unreferenced host object removed"
    );
    // Zero residue: the default groups are empty again.
    for group in ["subsystems", "ports", "hosts"] {
        let n = fs::read_dir(r.root.join(group)).expect("read_dir").count();
        assert_eq!(n, 0, "{group}/ must be empty after unshare (zero residue)");
    }
    assert!(
        Ledger::new(&r.state_dir)
            .find(&record.subnqn)
            .expect("load")
            .is_none(),
        "ledger record deleted after teardown"
    );
}

/// §6.4 law 4: unshare of a record whose live objects already vanished is
/// a verified no-op that still cleans the ledger.
#[test]
fn test_nvmet_unshare_vanished_objects_verified_noop() {
    let r = rig();
    let record = r
        .stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-van",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect("share");
    // Simulate the configfs wipe (reboot / manual removal).
    fs::remove_dir_all(r.root.join("subsystems").join(&record.subnqn)).expect("wipe sub");
    let id = record.listeners[0].nvmet_port_id.expect("id");
    fs::remove_dir_all(r.root.join("ports").join(id.to_string())).expect("wipe port");

    r.stack
        .unshare(&record)
        .expect("unshare of vanished objects must be a verified no-op");
    assert!(Ledger::new(&r.state_dir)
        .find(&record.subnqn)
        .expect("load")
        .is_none());
}

// ---------------------------------------------------------------------------
// restore (idempotent replay + §6.4 law-6 reconciliation)
// ---------------------------------------------------------------------------

#[test]
fn test_nvmet_restore_reconciles_all_intent_states() {
    let r = rig();
    let ledger = Ledger::new(&r.state_dir);

    // (1) active + gone => re-shared, SAME ns_uuid re-presented.
    let rec_a = r
        .stack
        .share(&req(
            "nqn.test:restore-a",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect("share a");
    // (4-prep) a share whose unshare will be interrupted.
    let rec_d = r
        .stack
        .share(&req(
            "nqn.test:restore-d",
            "/dev/zero",
            UUID_B,
            &[("127.0.0.1", 4421)],
        ))
        .expect("share d");

    // The reboot: configfs wiped, ledger survives.
    for group in ["subsystems", "ports", "hosts"] {
        fs::remove_dir_all(r.root.join(group)).expect("wipe");
        fs::create_dir_all(r.root.join(group)).expect("recreate");
    }

    // (2) pending + no live objects => GC'd.
    let mut pending_gc = rec_a.clone();
    pending_gc.subnqn = "nqn.test:restore-pending-gc".to_string();
    pending_gc.backing_path = "/dev/fake-gc".to_string();
    pending_gc.backing_canonical = "/dev/fake-gc".to_string();
    pending_gc.state = ShareState::Pending;
    ledger.begin_share(&pending_gc).expect("begin pending-gc");

    // (3) pending + live objects => finalized.
    let mut pending_live = pending_gc.clone();
    pending_live.subnqn = "nqn.test:restore-pending-live".to_string();
    pending_live.backing_path = "/dev/fake-live".to_string();
    pending_live.backing_canonical = "/dev/fake-live".to_string();
    ledger
        .begin_share(&pending_live)
        .expect("begin pending-live");
    plant_live_subsystem(
        &r.root,
        "nqn.test:restore-pending-live",
        "/dev/fake-live",
        pending_live.ns_uuid.as_deref(),
    );

    // (4) removing => teardown resumed.
    ledger.mark_removing(&rec_d.subnqn).expect("mark removing");

    let records = ledger.load().expect("load");
    let report = r.stack.restore(&records).expect("restore");
    assert_eq!(
        report.failures(),
        0,
        "no failures expected: {:?}",
        report.entries
    );

    let outcome_of = |nqn: &str| {
        report
            .entries
            .iter()
            .find(|e| e.subnqn == nqn)
            .unwrap_or_else(|| panic!("report entry for {nqn}"))
            .outcome
            .clone()
    };

    // (1) re-shared with the recorded identity re-presented.
    assert_eq!(outcome_of("nqn.test:restore-a"), RestoreOutcome::Restored);
    let ns_a = r
        .root
        .join("subsystems")
        .join("nqn.test:restore-a")
        .join("namespaces")
        .join("1");
    assert_eq!(
        fs::read_to_string(ns_a.join("device_uuid"))
            .expect("uuid")
            .trim(),
        UUID_A,
        "restore must re-present the RECORDED ns_uuid — never regenerate"
    );

    // (2) GC'd loud.
    assert_eq!(
        outcome_of("nqn.test:restore-pending-gc"),
        RestoreOutcome::GarbageCollectedPending
    );
    assert!(ledger
        .find("nqn.test:restore-pending-gc")
        .expect("load")
        .is_none());

    // (3) finalized.
    assert_eq!(
        outcome_of("nqn.test:restore-pending-live"),
        RestoreOutcome::FinalizedPending
    );
    assert_eq!(
        ledger
            .find("nqn.test:restore-pending-live")
            .expect("load")
            .expect("kept")
            .state,
        ShareState::Active
    );

    // (4) teardown resumed, record deleted.
    assert_eq!(
        outcome_of("nqn.test:restore-d"),
        RestoreOutcome::TeardownResumed
    );
    assert!(ledger.find("nqn.test:restore-d").expect("load").is_none());
    assert!(!r
        .root
        .join("subsystems")
        .join("nqn.test:restore-d")
        .exists());

    // Idempotency (law 4): a second restore is all verified no-ops.
    let records = ledger.load().expect("load");
    let report2 = r.stack.restore(&records).expect("second restore");
    assert_eq!(report2.failures(), 0);
    assert!(
        report2
            .entries
            .iter()
            .all(|e| e.outcome == RestoreOutcome::VerifiedNoop),
        "already-live shares must be verified no-ops: {:?}",
        report2.entries
    );
}

/// Existing-but-mismatched live state (same NQN, different identity) is a
/// loud per-record conflict — never clobbered (§6.6 Restore).
#[test]
fn test_nvmet_restore_mismatched_identity_conflicts_never_clobbers() {
    let r = rig();
    let ledger = Ledger::new(&r.state_dir);
    let record = r
        .stack
        .share(&req(
            "nqn.test:restore-mm",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect("share");

    // Overwrite the live identity with a different UUID (the foreign-
    // rebuild / operator-hand-edit shape).
    let ns = r
        .root
        .join("subsystems")
        .join(&record.subnqn)
        .join("namespaces")
        .join("1");
    fs::write(ns.join("device_uuid"), UUID_B).expect("clobber uuid");

    let records = ledger.load().expect("load");
    let report = r.stack.restore(&records).expect("restore runs");
    assert_eq!(
        report.failures(),
        1,
        "mismatch must be a per-record failure"
    );
    let entry = &report.entries[0];
    match &entry.outcome {
        RestoreOutcome::Failed(msg) => {
            assert!(
                msg.contains(UUID_B) || msg.contains("mismatch"),
                "conflict must be named: {msg}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    // Never clobbered: the live (mismatched) state is untouched.
    assert_eq!(
        fs::read_to_string(ns.join("device_uuid"))
            .expect("uuid")
            .trim(),
        UUID_B
    );
}

// ---------------------------------------------------------------------------
// live_shares + target_status + preflight
// ---------------------------------------------------------------------------

#[test]
fn test_nvmet_live_shares_walk_and_target_status_counts() {
    let r = rig();
    r.stack
        .share(&req(
            "nqn.2026-07.io.squeezefs:share-ls",
            "/dev/null",
            UUID_A,
            &[("127.0.0.1", 4420)],
        ))
        .expect("share");
    plant_live_subsystem(&r.root, "nqn.foreign:manual", "/dev/zero", None);

    let live = r.stack.live_shares().expect("live_shares");
    assert_eq!(live.len(), 2);
    let ours = live
        .iter()
        .find(|l| l.subnqn == "nqn.2026-07.io.squeezefs:share-ls")
        .expect("our share visible");
    assert_eq!(ours.device_path, "/dev/null");
    assert_eq!(ours.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(ours.listeners.len(), 1);
    assert_eq!(ours.listeners[0].ip, "127.0.0.1");
    assert_eq!(ours.listeners[0].port, 4420);
    let foreign = live
        .iter()
        .find(|l| l.subnqn == "nqn.foreign:manual")
        .expect("foreign share visible");
    assert!(
        foreign.listeners.is_empty(),
        "no port links planted for the foreign one"
    );

    let status = r.stack.target_status().expect("target_status");
    assert_eq!(status.stack, StackKind::Nvmet);
    assert!(status.configfs_mounted);
    assert_eq!(status.subsystems, 2);
    assert_eq!(status.namespaces, 2);
    assert_eq!(status.ports, 1);

    // Preflight over a present tree succeeds for every op.
    for op in [
        PreflightOp::Share,
        PreflightOp::Unshare,
        PreflightOp::List,
        PreflightOp::Restore,
    ] {
        r.stack
            .preflight(op)
            .unwrap_or_else(|e| panic!("preflight {op:?} over a live tree: {e}"));
    }
}

// ---------------------------------------------------------------------------
// mod-level pure helpers (per-stack flag semantics + backing preparation)
// ---------------------------------------------------------------------------

/// §6.2 per-stack flag semantics: `--nsid` is SPDK-only; the nvmet
/// namespace index is structurally fixed at 1, so `--nsid` ≠ 1 with
/// nvmet refuses loud (the `--disk-cache-paths` precedent — never a
/// silent flag-ignore) while `--nsid 1` (the structural index) passes.
#[test]
fn test_validate_share_flags_nsid_per_stack_semantics() {
    use squeezefs::nvmeof::validate_share_flags;

    let err = validate_share_flags(StackKind::Nvmet, Some(2), None)
        .expect_err("--nsid 2 with nvmet must refuse loud");
    let msg = err.to_string();
    assert!(msg.contains("--nsid"), "must name the flag: {msg}");
    assert!(
        msg.contains("structurally fixed at 1"),
        "must state the structural convention: {msg}"
    );

    validate_share_flags(StackKind::Nvmet, Some(1), None)
        .expect("--nsid 1 matches the structural index and passes");
    validate_share_flags(StackKind::Nvmet, None, None).expect("no nsid is the default");
    validate_share_flags(StackKind::Spdk, Some(7), None)
        .expect("--nsid is SPDK-only and any value validates for spdk");

    // --ns-uuid seeds BOTH stacks and must parse as a UUID.
    let err = validate_share_flags(StackKind::Nvmet, None, Some("not-a-uuid"))
        .expect_err("malformed --ns-uuid must refuse loud");
    assert!(err.to_string().contains("--ns-uuid"), "{err}");
    validate_share_flags(StackKind::Nvmet, None, Some(UUID_A)).expect("well-formed uuid");
    validate_share_flags(StackKind::Spdk, None, Some(UUID_A)).expect("both stacks");
}

/// Missing backing refuses loud (the silent 1 GiB sparse auto-create is
/// dead); `--create-size` is the explicit opt-in and creates a sparse
/// file of exactly the requested size; directories refuse.
#[test]
fn test_prepare_backing_refuses_missing_and_creates_on_optin() {
    use squeezefs::nvmeof::prepare_backing;

    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("not-there.img");

    let err = prepare_backing(missing.to_str().expect("utf8"), None)
        .expect_err("missing backing without --create-size must refuse");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(
        err.to_string().contains("--create-size"),
        "refusal must name the explicit opt-in: {err}"
    );
    assert!(!missing.exists(), "refusal must not conjure the file");

    prepare_backing(missing.to_str().expect("utf8"), Some(8 * 1024 * 1024))
        .expect("--create-size opt-in creates");
    assert_eq!(
        fs::metadata(&missing).expect("meta").len(),
        8 * 1024 * 1024,
        "sparse file of exactly the requested size"
    );

    // Existing file: fine (idempotent NoCOW guard).
    prepare_backing(missing.to_str().expect("utf8"), None).expect("existing file passes");

    // Directory: refuse.
    let err = prepare_backing(dir.path().to_str().expect("utf8"), None)
        .expect_err("directory backing must refuse");
    assert!(err.to_string().contains("directory"), "{err}");
}
