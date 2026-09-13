//! `nvmeof adopt <subnqn>` contract tests
//! (`docs/design-nvmeof-target-management.md` §6.10, PR 4b/N4b) —
//! foreign-share absorption into the ledger, driven through the same
//! §6.8 zero-mock seam as the stack suite: an **injected configfs root**
//! for the kernel nvmet target (the ONE target since SPDK was retired —
//! R-SYM-8, `docs/design-symmetric-metadata.md` §5.8.1; the SPDK arm's
//! fake `spdk_tgt`, `adopt_ambiguous` and the bare-bdev scan left with
//! it). Pinned here:
//!
//! * **classification + the named refusal classes** against injected
//!   live-state snapshots (`adopt_not_live` / `adopt_already_ledgered`
//!   (NQN **or** backing, any intent state — incl. a record the retired
//!   SPDK stack wrote) / `adopt_backing_duplicated` (the §6.4
//!   duplicate-guard laws applied verbatim) / `adopt_harness_owned`
//!   (devsub-/fideli-/spdkscope prefixes + port ids 52026/52470/52471 —
//!   never absorb the test fabric) / `adopt_shape_unsupported` (ns index
//!   ≠ 1 / multi-ns, unmaterialized shells, listener-less objects));
//! * **candidate shape**: the live object read into a `pending` record
//!   with `adopted_from` provenance (class heuristic: product `share-`
//!   prefix unledgered ⇒ ledger-loss, pre-rebuild default prefixes ⇒
//!   pre-rebuild, else foreign), identity recorded with **loud nulls**
//!   (missing `device_uuid` ⇒ null + re-share note), out-of-range nvmet
//!   port ids **recorded as-is** under the link-free teardown law,
//!   allow-hosts captured so a restored adopted share never widens to
//!   allow-any;
//! * **absorption via the intent protocol** (§6.4 law 6): `pending`
//!   before anything else, TOCTOU re-verify against a fresh live probe
//!   (drift = loud abort + the pending intent garbage-collected — pinned
//!   end-to-end through `adopt_over`'s `TargetStack` seam with a
//!   two-snapshot fake whose second `live_shares()` drifts or errors),
//!   finalize `active`;
//! * **zero target mutation**: the injected configfs tree snapshots
//!   byte-identical (shape + content + symlink targets);
//! * **adopted shares are fully managed**: `restore` verifies them as
//!   no-ops, `unshare` tears them down cleanly — including the
//!   out-of-range port-id removal (link-free only);
//! * the §6.4 duplicate-guard refusal names `adopt` beside the manual
//!   removal steps.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use squeezefs::nvmeof::ledger::Ledger;
use squeezefs::nvmeof::nvmet::{NvmetStack, NVMET_PORT_ID_BASE_DEFAULT};
use squeezefs::nvmeof::stack::{
    AdoptClass, Listener, LiveShare, NvmeofError, PreflightError, PreflightOp, RestoreReport,
    ShareRecord, ShareRequest, ShareState, TargetStack, TargetStatus,
};
use squeezefs::nvmeof::{
    adopt_candidate, adopt_class_of, adopt_over, adopt_verify_unchanged, StackKind,
    HARNESS_NQN_MARKERS, HARNESS_NVMET_PORT_IDS,
};

const UUID_A: &str = "e2b1c9a4-52d1-4a08-9f31-7c2b8d1e0aa1";
const UUID_B: &str = "0f0e0d0c-0b0a-4a09-8807-060504030201";
const NQN_FOREIGN: &str = "nqn.2026-06.io.foreign:handbuilt-1";
const NQN_PRE_REBUILD: &str = "nqn.2026-06.io.squeezefs:subsystem-0af1";
const NQN_LEDGER_LOSS: &str = "nqn.2026-07.io.squeezefs:share-lost01";

// ---------------------------------------------------------------------------
// rig: injected configfs root + relocated state dir
// ---------------------------------------------------------------------------

struct Rig {
    _dir: tempfile::TempDir,
    nvmet_root: PathBuf,
    nvmet: NvmetStack,
    ledger: Ledger,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let nvmet_root = dir.path().join("nvmet");
    fs::create_dir_all(nvmet_root.join("subsystems")).expect("mkdir subsystems");
    fs::create_dir_all(nvmet_root.join("ports")).expect("mkdir ports");
    fs::create_dir_all(nvmet_root.join("hosts")).expect("mkdir hosts");
    let state_dir = dir.path().join("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let nvmet = NvmetStack::new(
        nvmet_root.clone(),
        Ledger::new(&state_dir),
        NVMET_PORT_ID_BASE_DEFAULT,
    );
    Rig {
        nvmet_root,
        nvmet,
        ledger: Ledger::new(&state_dir),
        _dir: dir,
    }
}

/// Hand-build a live nvmet subsystem in the injected root — the shape a
/// pre-rebuild binary or another tenant leaves behind (namespace index 1
/// unless overridden, real attr files, port object + subsystem link).
#[allow(clippy::too_many_arguments)]
fn plant_nvmet_subsystem(
    root: &Path,
    nqn: &str,
    device: &str,
    uuid: Option<&str>,
    ns_indexes: &[u32],
    port_id: Option<u32>,
    listener: (&str, u16),
    allow_hosts: &[&str],
) {
    let sub = root.join("subsystems").join(nqn);
    for idx in ns_indexes {
        let ns = sub.join("namespaces").join(idx.to_string());
        fs::create_dir_all(&ns).expect("plant ns");
        fs::write(ns.join("device_path"), device).expect("plant device_path");
        if let Some(u) = uuid {
            fs::write(ns.join("device_uuid"), u).expect("plant device_uuid");
        }
        fs::write(ns.join("enable"), "1").expect("plant enable");
    }
    if ns_indexes.is_empty() {
        fs::create_dir_all(&sub).expect("plant bare subsystem");
    }
    fs::write(
        sub.join("attr_allow_any_host"),
        if allow_hosts.is_empty() { "1" } else { "0" },
    )
    .expect("plant allow_any");
    for host in allow_hosts {
        let host_obj = root.join("hosts").join(host);
        fs::create_dir_all(&host_obj).expect("plant host");
        let links = sub.join("allowed_hosts");
        fs::create_dir_all(&links).expect("plant allowed_hosts");
        std::os::unix::fs::symlink(&host_obj, links.join(host)).expect("plant host link");
    }
    if let Some(id) = port_id {
        let p = root.join("ports").join(id.to_string());
        fs::create_dir_all(p.join("subsystems")).expect("plant port");
        fs::write(p.join("addr_trtype"), "tcp").expect("trtype");
        fs::write(p.join("addr_adrfam"), "ipv4").expect("adrfam");
        fs::write(p.join("addr_traddr"), listener.0).expect("traddr");
        fs::write(p.join("addr_trsvcid"), listener.1.to_string()).expect("trsvcid");
        std::os::unix::fs::symlink(&sub, p.join("subsystems").join(nqn)).expect("port link");
    }
}

/// Full-tree snapshot (relative path, kind, content / symlink target) —
/// the unit-tier zero-mutation witness for the injected configfs root.
fn tree_snapshot(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(base).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&path).expect("stat");
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path)
                    .expect("readlink")
                    .display()
                    .to_string();
                out.push(format!("L {rel} -> {target}"));
            } else if meta.is_dir() {
                out.push(format!("D {rel}"));
                walk(base, &path, out);
            } else {
                let content = fs::read_to_string(&path).unwrap_or_default();
                out.push(format!("F {rel} = {content}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn live(
    subnqn: &str,
    device: &str,
    uuid: Option<&str>,
    listeners: &[(&str, u16, Option<u32>)],
) -> LiveShare {
    LiveShare {
        subnqn: subnqn.to_string(),
        device_path: device.to_string(),
        backing_canonical: device.to_string(),
        ns_uuid: uuid.map(str::to_string),
        listeners: listeners
            .iter()
            .map(|(ip, port, id)| Listener {
                ip: ip.to_string(),
                port: *port,
                nvmet_port_id: *id,
            })
            .collect(),
        enabled: true,
        nsids: vec![1],
        allow_hosts: Vec::new(),
    }
}

fn tmp_ledger() -> (tempfile::TempDir, Ledger) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = Ledger::new(dir.path());
    (dir, ledger)
}

fn seed_record(ledger: &Ledger, subnqn: &str, backing: &str, stack: StackKind, state: ShareState) {
    let record = ShareRecord {
        subnqn: subnqn.to_string(),
        stack,
        state: ShareState::Pending,
        backing_path: backing.to_string(),
        backing_canonical: backing.to_string(),
        nsid: None,
        ns_uuid: Some(UUID_B.to_string()),
        listeners: vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: Some(53000),
        }],
        bdev_name: None,
        ptpl_file: None,
        loop_device: None,
        created_utc: "2026-07-18T00:00:00Z".to_string(),
        allow_hosts: Vec::new(),
        adopted_from: None,
    };
    ledger.begin_share(&record).expect("seed record");
    match state {
        ShareState::Pending => {}
        ShareState::Active => ledger.finalize_share(subnqn).expect("finalize"),
        ShareState::Removing => ledger.mark_removing(subnqn).expect("mark removing"),
    }
}

// ---------------------------------------------------------------------------
// provenance-class heuristic
// ---------------------------------------------------------------------------

/// §6.10 pt 3: pre-rebuild default prefixes ⇒ pre-rebuild; the product's
/// own N2+ ownership prefix unledgered ⇒ ledger-loss; anything else ⇒
/// foreign.
#[test]
fn test_adopt_class_heuristic_pre_rebuild_ledger_loss_foreign() {
    assert_eq!(adopt_class_of(NQN_PRE_REBUILD), AdoptClass::PreRebuild);
    assert_eq!(
        adopt_class_of("nqn.2026-06.io.squeezefs:spdk-subsystem-9d"),
        AdoptClass::PreRebuild
    );
    assert_eq!(adopt_class_of(NQN_LEDGER_LOSS), AdoptClass::LedgerLoss);
    assert_eq!(adopt_class_of(NQN_FOREIGN), AdoptClass::Foreign);
    assert_eq!(
        adopt_class_of("nqn.2014-08.org.nvmexpress:some-operator-thing"),
        AdoptClass::Foreign
    );
}

// ---------------------------------------------------------------------------
// the named refusal classes (§6.10 pt 2)
// ---------------------------------------------------------------------------

#[test]
fn test_adopt_not_live_refusal_names_class_and_list_guidance() {
    let (_dir, ledger) = tmp_ledger();
    let err = adopt_candidate(NQN_FOREIGN, &[], &ledger).expect_err("nothing live must refuse");
    let text = err.to_string();
    assert!(text.contains("adopt_not_live"), "names the class: {text}");
    assert!(text.contains(NQN_FOREIGN), "names the NQN: {text}");
    assert!(
        text.contains("nvmeof list"),
        "points at the reconciliation view: {text}"
    );
    assert!(
        text.contains("restore"),
        "a ledgered-but-down share is restore territory: {text}"
    );
}

#[test]
fn test_adopt_already_ledgered_refusal_any_state_nqn_or_backing() {
    // (a) the NQN itself is ledgered (active).
    let (_dir, ledger) = tmp_ledger();
    seed_record(
        &ledger,
        NQN_FOREIGN,
        "/dev/zram13",
        StackKind::Nvmet,
        ShareState::Active,
    );
    let holder = live(NQN_FOREIGN, "/dev/zram13", Some(UUID_A), &[]);
    let err = adopt_candidate(NQN_FOREIGN, std::slice::from_ref(&holder), &ledger)
        .expect_err("a ledgered NQN is never adopt territory");
    let text = err.to_string();
    assert!(
        text.contains("adopt_already_ledgered"),
        "names the class: {text}"
    );
    assert!(text.contains("active"), "names the record state: {text}");

    // (b) a pending intent record (crash window) — restore territory.
    let (_dir2, ledger2) = tmp_ledger();
    seed_record(
        &ledger2,
        NQN_FOREIGN,
        "/dev/zram13",
        StackKind::Nvmet,
        ShareState::Pending,
    );
    let err = adopt_candidate(NQN_FOREIGN, std::slice::from_ref(&holder), &ledger2)
        .expect_err("pending intents belong to restore");
    let text = err.to_string();
    assert!(
        text.contains("adopt_already_ledgered") && text.contains("pending"),
        "names class + intent state: {text}"
    );
    assert!(
        text.contains("restore"),
        "the crash-window remediation is restore: {text}"
    );

    // (c) the BACKING is ledgered under another NQN.
    let (_dir3, ledger3) = tmp_ledger();
    seed_record(
        &ledger3,
        "nqn.2026-07.io.squeezefs:share-other",
        "/dev/zram14",
        StackKind::Nvmet,
        ShareState::Active,
    );
    let holder14 = live(NQN_FOREIGN, "/dev/zram14", Some(UUID_A), &[]);
    let err = adopt_candidate(NQN_FOREIGN, std::slice::from_ref(&holder14), &ledger3)
        .expect_err("a ledgered backing refuses");
    let text = err.to_string();
    assert!(
        text.contains("adopt_already_ledgered"),
        "names the class: {text}"
    );
    assert!(
        text.contains("nqn.2026-07.io.squeezefs:share-other"),
        "names the holding record: {text}"
    );

    // (d) a record the RETIRED SPDK stack wrote: the exit is the re-share
    // sequence (unshare removes it ledger-only, then share on nvmet) —
    // never adopt, never a re-presentation onto the retired stack.
    let (_dir4, ledger4) = tmp_ledger();
    seed_record(
        &ledger4,
        NQN_FOREIGN,
        "/dev/zram13",
        StackKind::Spdk,
        ShareState::Active,
    );
    let err = adopt_candidate(NQN_FOREIGN, std::slice::from_ref(&holder), &ledger4)
        .expect_err("a retired-SPDK record is unshare territory");
    let text = err.to_string();
    assert!(
        text.contains("adopt_already_ledgered") && text.contains("spdk"),
        "names class + the retired stack: {text}"
    );
    assert!(
        text.contains("RETIRED") && text.contains("nvmeof unshare") && text.contains("nvmet"),
        "the remediation is the re-share sequence: {text}"
    );
}

#[test]
fn test_adopt_backing_duplicated_refusal_names_the_other_live_holder() {
    let (_dir, ledger) = tmp_ledger();
    let live_set = vec![
        live(NQN_FOREIGN, "/dev/zram15", Some(UUID_A), &[]),
        live(
            "nqn.2026-06.io.foreign:twin",
            "/dev/zram15",
            Some(UUID_B),
            &[],
        ),
    ];
    let err = adopt_candidate(NQN_FOREIGN, &live_set, &ledger)
        .expect_err("a double-served backing must refuse");
    let text = err.to_string();
    assert!(
        text.contains("adopt_backing_duplicated"),
        "names the class: {text}"
    );
    assert!(
        text.contains("nqn.2026-06.io.foreign:twin") && text.contains("/dev/zram15"),
        "names the other live holder + the backing: {text}"
    );

    // An unmaterialized shell on the same backing is not a double-serve.
    let mut shell = live(
        "nqn.2026-06.io.foreign:shell",
        "",
        None,
        &[("127.0.0.1", 4421, None)],
    );
    shell.enabled = false;
    shell.backing_canonical = "/dev/zram15".to_string();
    let live_set = vec![
        live(
            NQN_FOREIGN,
            "/dev/zram15",
            Some(UUID_A),
            &[("127.0.0.1", 4420, Some(53000))],
        ),
        shell,
    ];
    adopt_candidate(NQN_FOREIGN, &live_set, &ledger)
        .expect("an unmaterialized shell never trips the duplicate guard");
}

#[test]
fn test_adopt_harness_owned_refusal_prefixes_and_port_ids() {
    let (_dir, ledger) = tmp_ledger();
    // Every known harness NQN marker refuses by name.
    for (marker, nqn) in [
        (":devsub-", "nqn.2026-07.io.squeezefs:devsub-meta"),
        (":fideli-", "nqn.2026-07.io.squeezefs:fideli-a1"),
        ("spdkscope", "nqn.2026-07.io.spdkscope:bench-rig"),
    ] {
        assert!(
            HARNESS_NQN_MARKERS.contains(&marker),
            "marker {marker} must be a named constant"
        );
        let holder = live(nqn, "/dev/zram18", Some(UUID_A), &[]);
        let err = adopt_candidate(nqn, std::slice::from_ref(&holder), &ledger)
            .expect_err("harness NQNs must never be absorbed");
        let text = err.to_string();
        assert!(
            text.contains("adopt_harness_owned"),
            "names the class for {nqn}: {text}"
        );
        assert!(
            text.contains(marker),
            "names the matched marker for {nqn}: {text}"
        );
        assert!(
            text.contains("teardown"),
            "points at the harness's own teardown: {text}"
        );
    }
    // Harness-reserved nvmet port ids refuse by number, even under a
    // non-harness NQN (a share serving through the test fabric's port
    // objects is the test fabric's).
    for port_id in HARNESS_NVMET_PORT_IDS {
        let holder = live(
            NQN_FOREIGN,
            "/dev/zram19",
            Some(UUID_A),
            &[("127.0.0.1", 4420, Some(port_id))],
        );
        let err = adopt_candidate(NQN_FOREIGN, std::slice::from_ref(&holder), &ledger)
            .expect_err("harness port ids must never be absorbed");
        let text = err.to_string();
        assert!(
            text.contains("adopt_harness_owned") && text.contains(&port_id.to_string()),
            "names class + port id {port_id}: {text}"
        );
    }
}

#[test]
fn test_adopt_shape_unsupported_nvmet_index_and_multi_ns() {
    let (_dir, ledger) = tmp_ledger();
    // Namespace index != 1 (the §6.6 structural convention).
    let mut wrong_index = live(NQN_FOREIGN, "/dev/zram20", Some(UUID_A), &[]);
    wrong_index.nsids = vec![2];
    let err =
        adopt_candidate(NQN_FOREIGN, &[wrong_index], &ledger).expect_err("index 2 unsupported");
    let text = err.to_string();
    assert!(
        text.contains("adopt_shape_unsupported"),
        "names the class: {text}"
    );
    assert!(text.contains('2'), "names the observed index: {text}");
    assert!(
        text.contains("re-share"),
        "remediation is removal-first + re-share: {text}"
    );

    // Multi-namespace subsystem.
    let mut multi = live(NQN_FOREIGN, "/dev/zram20", Some(UUID_A), &[]);
    multi.nsids = vec![1, 2];
    let err = adopt_candidate(NQN_FOREIGN, &[multi], &ledger).expect_err("multi-ns unsupported");
    assert!(err.to_string().contains("adopt_shape_unsupported"));

    // An unmaterialized shell serves nothing absorbable.
    let mut shell = live(NQN_FOREIGN, "", None, &[]);
    shell.nsids = vec![];
    shell.enabled = false;
    let err = adopt_candidate(NQN_FOREIGN, &[shell], &ledger).expect_err("shell unsupported");
    assert!(err.to_string().contains("adopt_shape_unsupported"));
}

// ---------------------------------------------------------------------------
// candidate shape (§6.10 pt 1: identity read live, loud nulls,
// out-of-range port ids recorded as-is)
// ---------------------------------------------------------------------------

#[test]
fn test_adopt_candidate_nvmet_shape_records_identity_listeners_loop() {
    let (_dir, ledger) = tmp_ledger();
    // A pre-rebuild-style holder: small-int (out-of-range) port id.
    let mut holder = live(
        NQN_PRE_REBUILD,
        "/dev/zram22",
        Some(UUID_A),
        &[("127.0.0.1", 4420, Some(4))],
    );
    holder.allow_hosts = vec!["nqn.2014-08.org.nvmexpress:uuid:h1".to_string()];
    let (candidate, notes) =
        adopt_candidate(NQN_PRE_REBUILD, &[holder], &ledger).expect("candidate builds");

    assert_eq!(candidate.stack, StackKind::Nvmet);
    assert_eq!(candidate.state, ShareState::Pending, "intent protocol");
    assert_eq!(candidate.backing_path, "/dev/zram22");
    assert_eq!(candidate.backing_canonical, "/dev/zram22");
    assert_eq!(candidate.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(candidate.nsid, None, "nsid never recorded on nvmet");
    assert_eq!(candidate.bdev_name, None);
    assert_eq!(candidate.ptpl_file, None);
    assert_eq!(candidate.loop_device, None);
    assert_eq!(
        candidate.listeners,
        vec![Listener {
            ip: "127.0.0.1".to_string(),
            port: 4420,
            nvmet_port_id: Some(4),
        }],
        "the out-of-range port id is recorded AS-IS"
    );
    assert_eq!(
        candidate.allow_hosts,
        vec!["nqn.2014-08.org.nvmexpress:uuid:h1".to_string()],
        "the live allowlist is captured — a restored adopted share never widens to allow-any"
    );
    let adopted = candidate.adopted_from.as_ref().expect("provenance");
    assert_eq!(adopted.class, AdoptClass::PreRebuild);
    assert!(
        adopted.utc.contains('T') && adopted.utc.ends_with('Z'),
        "RFC3339 provenance timestamp: {}",
        adopted.utc
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("outside the reserved range") && n.contains("link-free")),
        "the out-of-range port id gets a loud recorded-as-is note: {notes:?}"
    );
    candidate.validate().expect("presence rules hold");

    // Loop-served file backing: device_path is the loop node, the
    // canonical resolves to the file — backing_path records the file and
    // loop_device the node (§6.4 law 5: teardown learns it from the
    // ledger).
    let mut loop_holder = live(
        NQN_FOREIGN,
        "/dev/loop7",
        Some(UUID_A),
        &[("127.0.0.1", 4421, Some(53002))],
    );
    loop_holder.backing_canonical = "/srv/backing.img".to_string();
    let (candidate, _) =
        adopt_candidate(NQN_FOREIGN, &[loop_holder], &ledger).expect("loop candidate");
    assert_eq!(candidate.backing_path, "/srv/backing.img");
    assert_eq!(candidate.loop_device.as_deref(), Some("/dev/loop7"));

    // A live object with NO listener is not representable (the §6.4
    // schema requires >= 1) — shape-unsupported, never a validate panic.
    let bare_no_listener = live(
        "nqn.2026-06.io.foreign:no-listener",
        "/dev/zram23",
        None,
        &[],
    );
    let err = adopt_candidate(
        "nqn.2026-06.io.foreign:no-listener",
        &[bare_no_listener],
        &ledger,
    )
    .expect_err("no listener = unsupported shape");
    let text = err.to_string();
    assert!(
        text.contains("adopt_shape_unsupported") && text.contains("listener"),
        "no-listener objects refuse as shape-unsupported: {text}"
    );

    // Missing device_uuid: recorded null with the loud re-share note.
    let bare = live(
        "nqn.2026-06.io.foreign:no-uuid",
        "/dev/zram23",
        None,
        &[("127.0.0.1", 4422, Some(53003))],
    );
    let (candidate, notes) = adopt_candidate("nqn.2026-06.io.foreign:no-uuid", &[bare], &ledger)
        .expect("null identity is recordable");
    assert_eq!(candidate.ns_uuid, None);
    assert!(
        notes
            .iter()
            .any(|n| n.contains("re-share") && (n.contains("identity") || n.contains("uuid"))),
        "null identity gets the loud restart-stability note: {notes:?}"
    );
}

// ---------------------------------------------------------------------------
// TOCTOU verify (§6.10 pt 3)
// ---------------------------------------------------------------------------

#[test]
fn test_adopt_verify_unchanged_detects_drift() {
    let (_dir, ledger) = tmp_ledger();
    let holder = live(
        NQN_FOREIGN,
        "/dev/zram25",
        Some(UUID_A),
        &[("127.0.0.1", 4420, Some(4))],
    );
    let (candidate, _) =
        adopt_candidate(NQN_FOREIGN, std::slice::from_ref(&holder), &ledger).expect("candidate");

    // Unchanged live state verifies.
    adopt_verify_unchanged(&candidate, std::slice::from_ref(&holder))
        .expect("identical live state must verify");

    // Vanished.
    let err = adopt_verify_unchanged(&candidate, &[]).expect_err("vanished = drift");
    assert!(err.contains("vanished"), "names the drift: {err}");

    // Identity drift.
    let mut drifted = holder.clone();
    drifted.ns_uuid = Some(UUID_B.to_string());
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("uuid drift");
    assert!(err.contains("ns_uuid") || err.contains("uuid"), "{err}");

    // Backing drift.
    let mut drifted = holder.clone();
    drifted.device_path = "/dev/zram26".to_string();
    drifted.backing_canonical = "/dev/zram26".to_string();
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("backing drift");
    assert!(err.contains("backing"), "{err}");

    // Listener drift (a port id moved).
    let mut drifted = holder.clone();
    drifted.listeners[0].nvmet_port_id = Some(5);
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("listener drift");
    assert!(err.contains("listener"), "{err}");

    // Allowlist drift.
    let mut drifted = holder.clone();
    drifted.allow_hosts = vec!["nqn.x:new-host".to_string()];
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("allowlist drift");
    assert!(err.contains("allow"), "{err}");

    // Namespace-shape drift (a second namespace appeared).
    let mut drifted = holder.clone();
    drifted.nsids = vec![1, 2];
    let err = adopt_verify_unchanged(&candidate, &[drifted]).expect_err("shape drift");
    assert!(err.contains("namespace"), "{err}");
}

// ---------------------------------------------------------------------------
// the walker feeds adopt (the §6.10 classification rides the SAME live
// walker list/guards use — extended shape pinned here)
// ---------------------------------------------------------------------------

#[test]
fn test_nvmet_walker_reports_nsids_and_allow_hosts() {
    let r = rig();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_FOREIGN,
        "/dev/null",
        Some(UUID_A),
        &[1],
        Some(4),
        ("127.0.0.1", 4420),
        &["nqn.2014-08.org.nvmexpress:uuid:h1"],
    );
    let live = r.nvmet.live_shares().expect("walk");
    assert_eq!(live.len(), 1);
    let l = &live[0];
    assert_eq!(l.nsids, vec![1], "namespace indexes enumerated");
    assert_eq!(
        l.allow_hosts,
        vec!["nqn.2014-08.org.nvmexpress:uuid:h1".to_string()],
        "allowed_hosts links surface"
    );
    assert_eq!(l.listeners.len(), 1);
    assert_eq!(l.listeners[0].nvmet_port_id, Some(4));

    // Multi-ns shape surfaces for the shape check.
    plant_nvmet_subsystem(
        &r.nvmet_root,
        "nqn.2026-06.io.foreign:multi",
        "/dev/null2",
        None,
        &[1, 2],
        None,
        ("127.0.0.1", 4421),
        &[],
    );
    let live = r.nvmet.live_shares().expect("walk");
    let multi = live
        .iter()
        .find(|l| l.subnqn == "nqn.2026-06.io.foreign:multi")
        .expect("present");
    assert_eq!(multi.nsids, vec![1, 2]);
}

// ---------------------------------------------------------------------------
// absorption end-to-end (adopt_over): zero mutation, intent ordering,
// provenance, managed lifecycle afterwards
// ---------------------------------------------------------------------------

#[test]
fn test_adopt_over_nvmet_absorbs_planted_subsystem_zero_mutation() {
    let r = rig();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_PRE_REBUILD,
        "/dev/null",
        Some(UUID_A),
        &[1],
        Some(4), // the pre-rebuild small-int port id
        ("127.0.0.1", 4420),
        &[],
    );
    let before = tree_snapshot(&r.nvmet_root);

    let record = adopt_over(NQN_PRE_REBUILD, &r.ledger, &r.nvmet).expect("adopt succeeds");

    assert_eq!(record.state, ShareState::Active, "finalized active");
    assert_eq!(record.stack, StackKind::Nvmet);
    assert_eq!(record.ns_uuid.as_deref(), Some(UUID_A));
    assert_eq!(record.listeners.len(), 1);
    assert_eq!(
        record.listeners[0].nvmet_port_id,
        Some(4),
        "the out-of-range port id rides the record as-is"
    );
    assert_eq!(
        record.adopted_from.as_ref().map(|a| a.class),
        Some(AdoptClass::PreRebuild)
    );

    // Ledger round-trip.
    let loaded = r.ledger.find(NQN_PRE_REBUILD).unwrap().expect("recorded");
    assert_eq!(loaded, record);

    // ZERO target mutation: the configfs tree is byte-identical.
    let after = tree_snapshot(&r.nvmet_root);
    assert_eq!(
        before, after,
        "adopt mutates no target state — the configfs tree must be untouched"
    );
}

#[test]
fn test_adopt_over_nvmet_adopted_share_restore_noop_then_unshare_clean() {
    let r = rig();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_FOREIGN,
        "/dev/null",
        Some(UUID_A),
        &[1],
        Some(4),
        ("127.0.0.1", 4420),
        &[],
    );
    let record = adopt_over(NQN_FOREIGN, &r.ledger, &r.nvmet).expect("adopt");

    // Fully managed: restore verifies the adopted share as a no-op…
    let report = r
        .nvmet
        .restore(std::slice::from_ref(&record))
        .expect("restore runs");
    assert_eq!(
        report.entries[0].outcome,
        squeezefs::nvmeof::stack::RestoreOutcome::VerifiedNoop,
        "an adopted share is restore-reconcilable like any other"
    );

    // …and unshare tears it down cleanly, INCLUDING the out-of-range
    // port id (recorded ⇒ removed when link-free — the §6.6 law).
    r.nvmet.unshare(&record).expect("unshare adopted share");
    assert!(
        !r.nvmet_root.join("subsystems").join(NQN_FOREIGN).exists(),
        "subsystem gone"
    );
    assert!(
        !r.nvmet_root.join("ports").join("4").exists(),
        "the adopted out-of-range port object is removed once link-free"
    );
    assert!(r.ledger.find(NQN_FOREIGN).unwrap().is_none(), "record gone");
}

// ---------------------------------------------------------------------------
// TOCTOU abort-and-GC, end to end (§6.10 pt 3) — through adopt_over's
// TargetStack seam: a stack whose successive live_shares() answers are
// scripted (first the classification probe, then the re-verify probe). An
// injection seam in the §6.8 sense — the production path runs unchanged.
// ---------------------------------------------------------------------------

/// The scripted stack: `live_shares()` pops the next answer; every other
/// verb is a mutation adopt must never reach for (the zero-mutation law) —
/// reaching one fails the test.
struct ScriptedStack {
    probes: Mutex<VecDeque<Result<Vec<LiveShare>, String>>>,
}

impl ScriptedStack {
    fn new(probes: Vec<Result<Vec<LiveShare>, String>>) -> Self {
        ScriptedStack {
            probes: Mutex::new(probes.into()),
        }
    }

    fn remaining(&self) -> usize {
        self.probes.lock().unwrap().len()
    }
}

impl TargetStack for ScriptedStack {
    fn kind(&self) -> StackKind {
        StackKind::Nvmet
    }
    fn preflight(&self, _op: PreflightOp) -> Result<(), PreflightError> {
        panic!("adopt never preflights through the stack")
    }
    fn share(&self, _req: &ShareRequest) -> Result<ShareRecord, NvmeofError> {
        panic!("adopt never mutates target state (share)")
    }
    fn unshare(&self, _rec: &ShareRecord) -> Result<(), NvmeofError> {
        panic!("adopt never mutates target state (unshare)")
    }
    fn live_shares(&self) -> Result<Vec<LiveShare>, NvmeofError> {
        self.probes
            .lock()
            .unwrap()
            .pop_front()
            .expect("adopt probed live state more often than scripted")
            .map_err(|why| NvmeofError::Io(io::Error::other(why)))
    }
    fn restore(&self, _recs: &[ShareRecord]) -> Result<RestoreReport, NvmeofError> {
        panic!("adopt never mutates target state (restore)")
    }
    fn target_status(&self) -> Result<TargetStatus, NvmeofError> {
        panic!("adopt never reads target status")
    }
}

/// The identity flips between the classification probe and the re-verify
/// probe (the TOCTOU window made real): the abort is loud and names the
/// drift, the pending intent adopt itself wrote is garbage-collected, and
/// exactly two probes were spent — no third look, no mutation.
#[test]
fn test_adopt_over_toctou_drift_aborts_loud_and_gcs_pending() {
    let (_dir, ledger) = tmp_ledger();
    let holder = live(
        NQN_FOREIGN,
        "/dev/zram40",
        Some(UUID_A),
        &[("127.0.0.1", 4420, Some(53040))],
    );
    let mut drifted = holder.clone();
    drifted.ns_uuid = Some(UUID_B.to_string());
    let stack = ScriptedStack::new(vec![Ok(vec![holder]), Ok(vec![drifted])]);

    let err = adopt_over(NQN_FOREIGN, &ledger, &stack).expect_err("drift must abort the adopt");
    let text = err.to_string();
    assert!(
        text.contains(NQN_FOREIGN) && text.contains("drift") && text.contains("ns_uuid"),
        "the abort is loud and names the drift: {text}"
    );
    assert!(
        text.contains("garbage-collected"),
        "the abort says what happened to the intent: {text}"
    );
    assert!(
        ledger.find(NQN_FOREIGN).unwrap().is_none(),
        "the pending intent is garbage-collected on abort — nothing strands"
    );
    assert_eq!(
        stack.remaining(),
        0,
        "exactly two probes: classify, re-verify"
    );
}

/// The re-verify probe itself fails (the tree went unreadable between the
/// two looks): the error propagates loud and the pending intent is still
/// garbage-collected — a failed re-probe never leaves a `pending` record
/// for a foreign object behind.
#[test]
fn test_adopt_over_reprobe_error_propagates_and_gcs_pending() {
    let (_dir, ledger) = tmp_ledger();
    let holder = live(
        NQN_FOREIGN,
        "/dev/zram41",
        Some(UUID_A),
        &[("127.0.0.1", 4420, Some(53041))],
    );
    let stack = ScriptedStack::new(vec![
        Ok(vec![holder]),
        Err("configfs walk: permission denied mid-verb".to_string()),
    ]);

    let err = adopt_over(NQN_FOREIGN, &ledger, &stack).expect_err("a failed re-probe aborts");
    assert!(
        err.to_string().contains("permission denied mid-verb"),
        "the probe error propagates verbatim: {err}"
    );
    assert!(
        ledger.find(NQN_FOREIGN).unwrap().is_none(),
        "the pending intent is garbage-collected on a failed re-probe too"
    );
    assert_eq!(stack.remaining(), 0);
}

/// The vanished shape (the object was removed between the two probes) is
/// drift too.
#[test]
fn test_adopt_over_vanished_between_probes_aborts_and_gcs_pending() {
    let (_dir, ledger) = tmp_ledger();
    let holder = live(
        NQN_FOREIGN,
        "/dev/zram42",
        Some(UUID_A),
        &[("127.0.0.1", 4420, Some(53042))],
    );
    let stack = ScriptedStack::new(vec![Ok(vec![holder]), Ok(vec![])]);
    let err = adopt_over(NQN_FOREIGN, &ledger, &stack).expect_err("vanished = drift");
    assert!(err.to_string().contains("vanished"), "{err}");
    assert!(ledger.find(NQN_FOREIGN).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// adopted-record schema (§6.4 presence rules + provenance)
// ---------------------------------------------------------------------------

#[test]
fn test_adopted_record_schema_roundtrips_with_provenance() {
    let r = rig();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_LEDGER_LOSS,
        "/dev/null",
        Some(UUID_A),
        &[1],
        Some(53017),
        ("127.0.0.1", 4420),
        &[],
    );
    let record = adopt_over(NQN_LEDGER_LOSS, &r.ledger, &r.nvmet).expect("adopt");
    record.validate().expect("presence rules hold");
    let adopted = record.adopted_from.as_ref().expect("provenance present");
    assert_eq!(adopted.class, AdoptClass::LedgerLoss);
    assert!(adopted.utc.contains('T') && adopted.utc.ends_with('Z'));

    // Round-trips byte-faithfully through the ledger (deny_unknown_fields
    // schema — the provenance object is part of format 1).
    let loaded = r.ledger.find(NQN_LEDGER_LOSS).unwrap().expect("loaded");
    assert_eq!(loaded, record);
    let raw: Value =
        serde_json::from_slice(&fs::read(r.ledger.ledger_path()).unwrap()).expect("json");
    let rec = &raw["shares"][0];
    assert_eq!(rec["adopted_from"]["class"], "ledger-loss");
    assert!(rec["adopted_from"]["utc"].is_string());
}

// ---------------------------------------------------------------------------
// the §6.4 duplicate-guard refusal names adopt beside the manual steps
// (§6.10: "the duplicate-guard refusal message adds adopt")
// ---------------------------------------------------------------------------

#[test]
fn test_duplicate_guard_foreign_refusal_names_adopt() {
    let r = rig();
    plant_nvmet_subsystem(
        &r.nvmet_root,
        NQN_FOREIGN,
        "/dev/zram31",
        Some(UUID_A),
        &[1],
        None,
        ("127.0.0.1", 4420),
        &[],
    );
    let err = r
        .nvmet
        .share(&ShareRequest {
            subnqn: "nqn.2026-07.io.squeezefs:share-x1".to_string(),
            backing_path: "/dev/zram31".to_string(),
            backing_canonical: "/dev/zram31".to_string(),
            ns_uuid: UUID_B.to_string(),
            listeners: vec![Listener {
                ip: "127.0.0.1".to_string(),
                port: 4430,
                nvmet_port_id: None,
            }],
            allow_hosts: Vec::new(),
        })
        .expect_err("same backing refused");
    let text = err.to_string();
    assert!(
        text.contains("nvmeof adopt") && text.contains(NQN_FOREIGN),
        "the nvmet guard refusal names adopt beside the manual steps: {text}"
    );
}
