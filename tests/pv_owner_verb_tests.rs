//! **Per-volume claim admission — the OPERATOR VERB**
//! (`docs/design-per-volume-claim-admission.md` §5.6/§6.1, KD-PV-15,
//! KD-PV-11's M3, rulings D19/D20; PR 7).
//!
//! # Why this rung is the one that makes the program reachable
//!
//! PR 5 landed the derived ownership map and said so in its own milestone
//! wording: *"the only supported way to create an assignment is PR 7's
//! verb, so nothing is production-reachable until then"*. Everything the
//! earlier rungs built — `claim_set.owner`/`successors` (PR 2), the
//! `owner_assign:` marker (PR 2, decoded but never encoded), the partial
//! open (PR 4), the derivation (PR 5) — reads durable state **no product
//! path writes**. This file pins the writer.
//!
//! # The three properties every test here is about
//!
//! * **D19 — the bracketed offline coordinator.** The assignment runs
//!   under the D0-guarded open of the WHOLE set, where the process is
//!   momentarily the sole authority of every volume and writes every
//!   record itself. That is what makes the two-party hand-off protocol
//!   unnecessary, and it is why the enforcement point is the guarded open
//!   (a heartbeat-fresh foreign claim refuses) rather than a `live()`
//!   target-form test.
//! * **A half-assigned set is unreachable.** `owner == None` on one volume
//!   of a set another volume assigns is the shape PR 5's derivation
//!   refuses (*"the unassigned volume belongs to everyone and to
//!   nobody"*), so the verb must never leave one: the `owner_assign:`
//!   marker brackets the act (a writable mount refuses while it exists)
//!   and an idempotent re-run completes it.
//! * **KD-PV-15 — ownership is a property of a SUBTREE.** Without a
//!   verb-minted root on the volume it assigns, M2 pins every ino to its
//!   parent's owner, every ino descends from root, and the inversion
//!   happens for nobody (risk R16). The mint is deterministic — the
//!   preset-ino create path — never round-robin luck.
//!
//! Every refusal is asserted for its NUMBERS, not merely for failing:
//! the house law is that a refusal names the volume, the observed state
//! and the remedy (the VL4 capacity-preflight discipline).

use squeezefs::config_ops::{self, SetOwnersCrash, SetOwnersHooks, SetOwnersOptions};
use squeezefs::membership::{self, ClaimSet, MemberRole, CLAIM_SET_XATTR};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, WriterClaim, WRITER_CLAIM_XATTR};
use squeezefs::meta_backend::kv::builder::FormatV3Options;
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;

/// The node the fixtures assign the slot-0 volume to — the SET AUTHORITY
/// (D20). KD-MW-2 spelling: `node_{16 hex}.m{8 hex}`.
const NODE_A: &str = "node_00000000deadbeef.m00000001";
/// The peer the second volume is assigned to.
const NODE_B: &str = "node_00000000feedface.m00000001";
/// A third node, used as a declared successor and as a stranger.
const NODE_C: &str = "node_00000000badc0ffe.m00000001";

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// The nine-bit multi-writer stamp — `volume enable-multi-writer`'s
/// offline act, as every PV suite spells it.
async fn stamp_capabilities(path: &Path, claim_set: bool) {
    sb::set_durable_term_bit(path).await.expect("bit 7");
    sb::set_block_refcounts_bit(path).await.expect("bit 9");
    sb::set_layout_versions_bit(path).await.expect("bit 15");
    sb::set_ino_lanes_bit(path).await.expect("bit 12");
    sb::set_block_key_incarnation_bit(path)
        .await
        .expect("bit 13");
    sb::set_partitioned_append_bit(path).await.expect("bit 8");
    sb::set_writer_scoped_staging_bit(path)
        .await
        .expect("bit 10");
    if claim_set {
        sb::set_claim_set_bit(path).await.expect("bit 14");
    }
    sb::set_multi_writer_data_bit(path).await.expect("bit 11");
}

/// An `n`-volume stamped set in canonical slot-plan order.
async fn volume_set(dir: &Path, tag: &str, n: usize, claim_set: bool) -> Vec<PathBuf> {
    let plan = squeezefs::meta_backend::plan_meta_slot_set(n).expect("derived slot plan");
    let mut out = Vec::new();
    for (i, stamp) in plan.stamps.iter().enumerate().take(n) {
        let p = dir.join(format!("{tag}-meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        squeezefs::meta_backend::kv::builder::format_v3_stamped_single_writer(
            &p,
            VOL_LEN,
            &opts(),
            stamp.clone(),
        )
        .await
        .expect("format stamped meta volume");
        stamp_capabilities(&p, claim_set).await;
        out.push(p);
    }
    out
}

async fn two_volumes(dir: &Path, tag: &str) -> Vec<PathBuf> {
    volume_set(dir, tag, 2, true).await
}

fn uris(vols: &[PathBuf]) -> Vec<String> {
    vols.iter().map(|v| v.display().to_string()).collect()
}

async fn vol_id(path: &Path) -> String {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe open");
    probe.durable_volume_id()
}

/// The claim-set record as it lies on disk (`None` = no record at all —
/// the never-assigned image).
async fn raw_claim_set(path: &Path) -> Option<Vec<u8>> {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe open");
    probe.getxattr(1, CLAIM_SET_XATTR).await.expect("read")
}

async fn owner_of(path: &Path) -> Option<String> {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe open");
    ClaimSet::load(&probe).await.and_then(|s| s.owner)
}

async fn marker_present(path: &Path) -> bool {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe open");
    probe
        .getxattr(1, squeezefs::OWNER_ASSIGN_MARKER_XATTR)
        .await
        .expect("read")
        .is_some()
}

fn spec(vol_id: &str, owner: &str, root: Option<&str>) -> config_ops::OwnerAssignSpec {
    config_ops::OwnerAssignSpec {
        volume_id: vol_id.to_string(),
        owner: owner.to_string(),
        successors: Vec::new(),
        subtree_root: root.map(str::to_string),
    }
}

fn plain() -> SetOwnersOptions {
    SetOwnersOptions::default()
}

fn accepting(n: u64) -> SetOwnersOptions {
    SetOwnersOptions {
        accept_cross_owner_names: Some(n),
        ..SetOwnersOptions::default()
    }
}

fn dry() -> SetOwnersOptions {
    SetOwnersOptions {
        dry_run: true,
        ..SetOwnersOptions::default()
    }
}

/// The operator's documented two-step workflow — `--dry-run` to learn the
/// number, then acknowledge exactly it — used as a fixture so every root
/// test also pins that the two runs COUNT THE SAME.
async fn plan(u: &[String], specs: &[config_ops::OwnerAssignSpec]) -> config_ops::SetOwnersReport {
    config_ops::set_owners(u, specs, &dry())
        .await
        .expect("a dry run reports the plan rather than refusing on the census")
}

async fn assign(
    u: &[String],
    specs: &[config_ops::OwnerAssignSpec],
) -> config_ops::SetOwnersReport {
    let n = plan(u, specs).await.census.total;
    config_ops::set_owners(u, specs, &accepting(n))
        .await
        .expect("the acknowledged assignment applies")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A live FOREIGN claim: another host's boot id, so no dead-pid proof can
/// reclaim it and the D0 ladder reads it as fresh.
fn foreign_claim() -> WriterClaim {
    WriterClaim {
        id: "5f1d0e2a-0000-4000-8000-00000000ab01".to_string(),
        ts: now_secs() + 2,
        pid: 4242,
        boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        term: 7,
    }
}

/// Plant a claim record and DROP the backend (no shutdown), so the record
/// on disk names a live holder.
async fn plant_claim(path: &Path, claim: &WriterClaim) {
    let be = KvMetaBackend::open(path).await.expect("planting open");
    be.setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
        .await
        .expect("plant claim");
    be.sync_device().await.expect("barrier");
    drop(be);
}

/// Open the set writable, run `body`, release every guard.
async fn with_write_set<F, Fut, T>(uris: &[String], body: F) -> T
where
    F: FnOnce(std::sync::Arc<RoutedMetaBackend>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let routed = squeezefs::meta_backend::open_routed_meta_set(uris)
        .await
        .expect("the fixture's write open");
    let out = body(std::sync::Arc::clone(&routed)).await;
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
    out
}

/// Every dentry the fixture created, so a test can compute the expected
/// cross-owner population from the DEFINITION (parent's owner vs child's
/// owner) instead of re-implementing the census it is checking.
#[derive(Default)]
struct Tree {
    dentries: Vec<(u64, u64)>,
}

impl Tree {
    async fn mkdir(&mut self, routed: &RoutedMetaBackend, parent: u64, name: &str) -> u64 {
        let ino = routed
            .create(parent, name, libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir")
            .ino;
        self.dentries.push((parent, ino));
        ino
    }

    async fn mkfile(&mut self, routed: &RoutedMetaBackend, parent: u64, name: &str) -> u64 {
        let ino = routed
            .create(parent, name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        self.dentries.push((parent, ino));
        ino
    }

    /// The cross-owner name count the assignment `owner_by_volume` would
    /// create over exactly these dentries.
    fn cross_owner(&self, routed: &RoutedMetaBackend, owner_by_volume: &[&str]) -> u64 {
        self.dentries
            .iter()
            .filter(|(parent, child)| {
                owner_by_volume[routed.route_ino(*parent).0]
                    != owner_by_volume[routed.route_ino(*child).0]
            })
            .count() as u64
    }
}

// ---------------------------------------------------------------------------
// The grammar (§6.1: `<vol-id>=<member-id>[+<successor-id>...][:<path>]`)
// ---------------------------------------------------------------------------

#[test]
fn the_assignment_grammar_carries_successors_and_the_subtree_root() {
    let s = config_ops::parse_owner_assign_spec(&format!(
        "vol-0a1b2c3d4e5f6071={NODE_A}+{NODE_B}+{NODE_C}:/projects/a"
    ))
    .expect("the documented spelling parses");
    assert_eq!(s.volume_id, "vol-0a1b2c3d4e5f6071");
    assert_eq!(s.owner, NODE_A);
    assert_eq!(s.successors, vec![NODE_B.to_string(), NODE_C.to_string()]);
    assert_eq!(s.subtree_root.as_deref(), Some("/projects/a"));

    let bare = config_ops::parse_owner_assign_spec(&format!("vol-1122334455667788={NODE_B}"))
        .expect("omitting the root is legal (§5.5.1: it warns, it does not refuse)");
    assert!(bare.successors.is_empty() && bare.subtree_root.is_none());

    for bad in [
        "vol-1122334455667788",
        "=node_0000000000000001.m00000001",
        "vol-1122334455667788=",
        "vol-1122334455667788=node_0000000000000001.m00000001:relative/path",
    ] {
        let err = config_ops::parse_owner_assign_spec(bad)
            .err()
            .unwrap_or_else(|| panic!("'{bad}' must refuse"));
        assert!(
            err.to_string().contains(bad),
            "a grammar refusal must quote what it refused: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// The refusal ladder — every one names its numbers
// ---------------------------------------------------------------------------

/// **The D19 enforcement point.** Not `live(&target)` (a target-FORM test
/// that cannot see a mounted set behind a URI): the D0-guarded coordinator
/// open, which refuses on the first volume carrying a heartbeat-fresh
/// foreign claim — and writes nothing anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_owners_refuses_while_any_volume_carries_a_fresh_foreign_claim() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "fresh-foreign").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    plant_claim(&vols[1], &foreign_claim()).await;

    let before = squeezefs::meta_ship::stats().owner_assign_refusals;
    let err = config_ops::set_owners(
        &uris(&vols),
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &plain(),
    )
    .await
    .expect_err("a fresh foreign claim must refuse the assignment");
    let text = err.to_string();
    assert!(
        text.contains(&vols[1].display().to_string()),
        "the refusal must name the volume it refused on: {text}"
    );
    assert!(
        text.contains("unmounted"),
        "the refusal must name the requirement — every owner of this set unmounted: {text}"
    );
    assert!(
        squeezefs::meta_ship::stats().owner_assign_refusals > before,
        "owner_assign_refusals is the verb's refusal ledger"
    );
    assert!(
        !marker_present(&vols[0]).await,
        "a refusal writes NO marker"
    );
    assert!(
        raw_claim_set(&vols[0]).await.is_none() && raw_claim_set(&vols[1]).await.is_none(),
        "a refusal writes NO ownership record"
    );
}

/// Bit 14 is the whole format gate (KD-PV-9): an unstamped set is refused
/// before anything is written, naming the offline upgrade verb.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_owners_refuses_a_set_without_the_claim_set_capability() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "no-bit-14", 2, false).await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);

    let err = config_ops::set_owners(
        &uris(&vols),
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &plain(),
    )
    .await
    .expect_err("a bit-14-less set must refuse");
    let text = err.to_string();
    assert!(text.contains("bit 14"), "{text}");
    assert!(
        text.contains("volume enable-multi-writer"),
        "the refusal must name the remedy: {text}"
    );
    assert!(text.contains(&id0), "and the volume: {text}");
}

/// An owner id that can never equal any mount's KD-MW-2 identity would
/// assign the volume to nobody — refused at parse/plan time, naming the
/// form.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_owners_refuses_an_unenrollable_member() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "unenrollable").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);

    let err = config_ops::set_owners(
        &uris(&vols),
        &[spec(&id0, NODE_A, None), spec(&id1, "node-b", None)],
        &plain(),
    )
    .await
    .expect_err("an unenrollable member id must refuse");
    let text = err.to_string();
    assert!(text.contains("node-b"), "name the id it refused: {text}");
    assert!(
        text.contains("node_"),
        "and the form a member id must take: {text}"
    );
    assert!(raw_claim_set(&vols[0]).await.is_none(), "nothing written");
}

/// A partial map is the shape PR 5's derivation refuses at every mount —
/// so the verb refuses to CREATE one, naming the volume nobody claimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_owners_refuses_a_partial_map() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "partial-map").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);

    let err = config_ops::set_owners(&uris(&vols), &[spec(&id0, NODE_A, None)], &plain())
        .await
        .expect_err("an assignment that names only part of the set must refuse");
    let text = err.to_string();
    assert!(
        text.contains(&id1),
        "the refusal must name the volume left unassigned: {text}"
    );
    assert!(
        text.contains("WHOLE set") || text.contains("whole set"),
        "…and the remedy: {text}"
    );
    assert!(raw_claim_set(&vols[0]).await.is_none(), "nothing written");

    // A volume id that is not a member of this set is the same class of
    // mistake in the other direction.
    let err = config_ops::set_owners(
        &uris(&vols),
        &[
            spec(&id0, NODE_A, None),
            spec("vol-0000000000000000", NODE_B, None),
        ],
        &plain(),
    )
    .await
    .expect_err("naming a foreign volume id must refuse");
    assert!(
        err.to_string().contains("vol-0000000000000000"),
        "{err} must quote the id it could not place"
    );
}

/// §5.4a's barrier: the verb refuses to write the first owner record while
/// ANY volume carries an open cross-volume intent. The intent is produced
/// by the REAL path (a genuine cross-volume unlink severed at its
/// commit-boundary seam), because a planted record would prove only that
/// the refusal reads a record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_owners_refuses_an_open_cross_volume_intent() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "open-intent").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);

    with_write_set(&u, |routed| async move {
        let mut tree = Tree::default();
        let parent = loop {
            let ino = tree
                .mkdir(&routed, 1, &format!("d{}", tree.dentries.len()))
                .await;
            if routed.route_ino(ino).0 == 0 {
                break ino;
            }
        };
        let (child, name) = loop {
            let n = format!("f{}", tree.dentries.len());
            let ino = tree.mkfile(&routed, parent, &n).await;
            if routed.route_ino(ino).0 == 1 {
                break (ino, n);
            }
        };
        assert_ne!(routed.route_ino(parent).0, routed.route_ino(child).0);
        squeezefs::meta_backend::crossvol_tx::TEST_XV_SEAM_AFTER_STEPS.store(2, Ordering::Relaxed);
        let _ = routed.unlink(parent, &name).await;
        squeezefs::meta_backend::crossvol_tx::TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::Relaxed);
    })
    .await;

    let err = config_ops::set_owners(
        &u,
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &SetOwnersOptions {
            accept_cross_owner_names: Some(u64::MAX),
            ..SetOwnersOptions::default()
        },
    )
    .await
    .expect_err("an open cross-volume intent must refuse the assignment");
    let text = err.to_string();
    assert!(
        text.contains("intent"),
        "the refusal must name what it found: {text}"
    );
    assert!(
        text.contains(&id0) || text.contains(&id1),
        "…on which volume: {text}"
    );
    assert!(
        owner_of(&vols[0]).await.is_none() && owner_of(&vols[1]).await.is_none(),
        "the barrier refuses BEFORE the first owner record"
    );
}

/// KD-PV-11's M3: the pre-existing tree is MEASURED and the operator
/// acknowledges the exact number. An unacknowledged population refuses; a
/// wrong acknowledgement refuses printing BOTH numbers; the right one
/// admits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_owners_refuses_an_unacknowledged_cross_owner_name_census() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "census").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);

    let expected = with_write_set(&u, |routed| async move {
        let mut tree = Tree::default();
        let d = tree.mkdir(&routed, 1, "shared").await;
        for i in 0..8 {
            tree.mkfile(&routed, d, &format!("f{i}")).await;
        }
        let n = tree.cross_owner(&routed, &[NODE_A, NODE_B]);
        assert!(
            n > 0,
            "the fixture must actually spread inodes across the two volumes"
        );
        (n, tree.dentries.len() as u64)
    })
    .await;
    let (cross, dentries) = expected;

    let err = config_ops::set_owners(
        &u,
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &plain(),
    )
    .await
    .expect_err("an existing cross-owner population must be acknowledged");
    let text = err.to_string();
    assert!(
        text.contains(&format!("{cross}")),
        "the refusal must print the COUNT ({cross}): {text}"
    );
    assert!(
        text.contains("--accept-cross-owner-names"),
        "…and the acknowledgement flag: {text}"
    );
    assert!(
        text.contains("EXDEV"),
        "…and what the operator would be accepting: {text}"
    );

    let err = config_ops::set_owners(
        &u,
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &accepting(cross + 1),
    )
    .await
    .expect_err("a mismatched acknowledgement must refuse");
    let text = err.to_string();
    assert!(
        text.contains(&format!("{}", cross + 1)) && text.contains(&format!("{cross}")),
        "a mismatch prints BOTH numbers: {text}"
    );

    let report = config_ops::set_owners(
        &u,
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &accepting(cross),
    )
    .await
    .expect("the acknowledged assignment is admitted");
    assert_eq!(report.census.total, cross);
    assert_eq!(
        report.census.dentries_scanned, dentries,
        "the census is ONE pass over every dentry in the set"
    );
    assert!(
        !report.census.sample.is_empty(),
        "the census carries a bounded sample so the operator can see WHICH names"
    );
    assert_eq!(owner_of(&vols[1]).await.as_deref(), Some(NODE_B));
}

/// §5.7's hard fleet-width bound: `MAX_LANES = journal::MAX_APPENDERS = 16`
/// appenders, so a set naming more than 16 members cannot be granted
/// allocation lanes at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_owners_refuses_more_than_sixteen_members() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "too-many").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);

    let successors: Vec<String> = (0..16)
        .map(|i| format!("node_00000000000000{i:02x}.m00000001"))
        .collect();
    let mut first = spec(&id0, NODE_A, None);
    first.successors = successors;
    let err = config_ops::set_owners(&uris(&vols), &[first, spec(&id1, NODE_B, None)], &plain())
        .await
        .expect_err("more than 16 members must refuse");
    let text = err.to_string();
    assert!(text.contains("16"), "the refusal prints the bound: {text}");
    assert!(
        text.contains("18") || text.contains("17"),
        "…and the number named: {text}"
    );
}

// ---------------------------------------------------------------------------
// The bracket: idempotent, crash-resumable, and unreachable half-way
// ---------------------------------------------------------------------------

/// A kill between adjacent volumes leaves the marker up. Three things must
/// then be true: the set is NOT half-assigned in the operator's hands (a
/// writable mount refuses, naming the re-run), the resume completes the
/// SAME act, and the completed set mounts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_between_adjacent_volumes_resumes_idempotently() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "resume").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);
    let specs = [spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)];

    let err = config_ops::set_owners_with(
        &u,
        &specs,
        &plain(),
        &SetOwnersHooks {
            crash_after: Some(SetOwnersCrash::AfterVolume { volume: 0 }),
        },
    )
    .await
    .expect_err("the crash seam fails the run");
    assert!(err.to_string().contains("crash injection"), "{err}");

    assert_eq!(owner_of(&vols[0]).await.as_deref(), Some(NODE_A));
    assert_eq!(owner_of(&vols[1]).await, None, "the crash window's shape");
    assert!(marker_present(&vols[0]).await, "the bracket is still open");

    // The half-assigned set is UNREACHABLE: a writable mount refuses.
    let mount_err = squeezefs::meta_backend::open_routed_meta_set(&u)
        .await
        .err()
        .unwrap_or_else(|| panic!("a writable mount must refuse while the bracket is open"));
    let text = mount_err.to_string();
    assert!(
        text.contains("set-owners"),
        "the mount refusal names the idempotent re-run: {text}"
    );

    // A resume must name the SAME act.
    let wrong = config_ops::set_owners(
        &u,
        &[spec(&id0, NODE_C, None), spec(&id1, NODE_B, None)],
        &plain(),
    )
    .await
    .expect_err("a resume naming a different act must refuse");
    assert!(
        wrong.to_string().contains(&id0),
        "the refusal names the volume whose assignment differs: {wrong}"
    );

    let report = config_ops::set_owners(&u, &specs, &plain())
        .await
        .expect("the idempotent re-run completes the act");
    assert_eq!(owner_of(&vols[0]).await.as_deref(), Some(NODE_A));
    assert_eq!(owner_of(&vols[1]).await.as_deref(), Some(NODE_B));
    assert!(!marker_present(&vols[0]).await, "the bracket closes LAST");
    assert_eq!(report.records_written, 2);

    let routed = squeezefs::meta_backend::open_routed_meta_set(&u)
        .await
        .expect("the completed set mounts");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// `--clear` is the terminal state and the remedy the mismatch refusal
/// names, so it must be able to SUPERSEDE an open bracket — otherwise an
/// operator whose run crashed is told to finish the assignment they no
/// longer want before they may undo it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clear_supersedes_an_open_assignment_bracket() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "clear-supersedes").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);

    config_ops::set_owners_with(
        &u,
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &plain(),
        &SetOwnersHooks {
            crash_after: Some(SetOwnersCrash::AfterVolume { volume: 0 }),
        },
    )
    .await
    .expect_err("the crash seam fails the run");
    assert!(marker_present(&vols[0]).await);

    let report = config_ops::set_owners(
        &u,
        &[],
        &SetOwnersOptions {
            clear: true,
            ..SetOwnersOptions::default()
        },
    )
    .await
    .expect("--clear supersedes the open bracket");
    assert_eq!(report.records_written, 2);
    assert!(report.cleared);
    assert!(!marker_present(&vols[0]).await, "and closes it");
    assert!(owner_of(&vols[0]).await.is_none() && owner_of(&vols[1]).await.is_none());
    let routed = squeezefs::meta_backend::open_routed_meta_set(&u)
        .await
        .expect("the set mounts again");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// The KD-PV-15 resumability window the design names explicitly: a kill
/// between a subtree root's mint and its volume's owner record. The
/// re-run ADOPTS the existing root (it does not mint a second one) and
/// finishes the assignment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_between_the_root_mint_and_the_owner_record_resumes_idempotently() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "root-resume").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);
    with_write_set(&u, |routed| async move {
        routed
            .create(1, "projects", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("the parent directory exists BEFORE the assignment");
    })
    .await;
    let specs = [
        spec(&id0, NODE_A, Some("/projects/a")),
        spec(&id1, NODE_B, Some("/projects/b")),
    ];
    let acknowledged = plan(&u, &specs).await.census.total;

    let err = config_ops::set_owners_with(
        &u,
        &specs,
        &accepting(acknowledged),
        &SetOwnersHooks {
            crash_after: Some(SetOwnersCrash::AfterRootMint { volume: 1 }),
        },
    )
    .await
    .expect_err("the crash seam fails the run");
    assert!(err.to_string().contains("crash injection"), "{err}");
    assert_eq!(
        owner_of(&vols[1]).await,
        None,
        "the crash lands between the mint and the record"
    );

    let minted_ino = config_ops::locate_path(&u, "/projects/b")
        .await
        .expect("the root survived the crash")
        .ino;

    let report = config_ops::set_owners(&u, &specs, &accepting(acknowledged))
        .await
        .expect(
            "the re-run converges — and the census it counts is UNCHANGED by the crash, \
                 because a root that moved from 'would mint' to 'already in the tree' is the \
                 same one name",
        );
    assert_eq!(owner_of(&vols[1]).await.as_deref(), Some(NODE_B));
    assert_eq!(
        config_ops::locate_path(&u, "/projects/b")
            .await
            .expect("locate")
            .ino,
        minted_ino,
        "an idempotent re-run ADOPTS the root it already minted"
    );
    assert!(
        report
            .roots
            .iter()
            .any(|r| r.path == "/projects/b" && r.minted_ino.is_none()),
        "and reports it as adopted rather than minted"
    );
}

// ---------------------------------------------------------------------------
// KD-PV-15 — the subtree roots
// ---------------------------------------------------------------------------

/// The headline of rev 3: the verb mints each owner's subtree root ON THE
/// VOLUME IT ASSIGNS, deterministically (the preset-ino create path), so
/// every descendant inherits that owner by M2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_verb_mints_each_subtree_root_on_the_volume_it_assigns() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "roots", 3, true).await;
    let ids = [
        vol_id(&vols[0]).await,
        vol_id(&vols[1]).await,
        vol_id(&vols[2]).await,
    ];
    let u = uris(&vols);
    with_write_set(&u, |routed| async move {
        routed
            .create(1, "projects", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir /projects");
    })
    .await;

    let before = squeezefs::meta_ship::stats().subtree_roots_minted;
    let report = assign(
        &u,
        &[
            spec(&ids[0], NODE_A, Some("/projects/a")),
            spec(&ids[1], NODE_B, Some("/projects/b")),
            spec(&ids[2], NODE_C, Some("/projects/c")),
        ],
    )
    .await;

    assert_eq!(report.roots_minted, 3);
    assert_eq!(
        squeezefs::meta_ship::stats().subtree_roots_minted,
        before + 3,
        "subtree_roots_minted is KD-PV-15's ledger"
    );
    for (i, path) in ["/projects/a", "/projects/b", "/projects/c"]
        .into_iter()
        .enumerate()
    {
        let found = config_ops::locate_path(&u, path).await.expect("locate");
        assert_eq!(
            found.volume_id, ids[i],
            "{path} must home on the volume it was minted for"
        );
    }
    // Every minted root is exactly one cross-owner name (its dentry lives
    // in the parent's volume, its ino on the assignee's) — the whole
    // undeletable-in-place population of the supported shape.
    assert!(
        report.census.roots <= 3,
        "at most one cross-owner name per root: {}",
        report.census.roots
    );
}

/// An existing path whose ino homes elsewhere is refused loud, naming the
/// instrument that answers where it DOES home.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_existing_root_path_whose_ino_homes_elsewhere_refuses_naming_volume_locate() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "root-elsewhere").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);
    let home = with_write_set(&u, |routed| async move {
        routed
            .create(1, "projects", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir /projects");
        let mut names = 0;
        loop {
            let name = format!("b{names}");
            let ino = routed
                .create(
                    routed.lookup(1, "projects").await.expect("lookup").ino,
                    &name,
                    libc::S_IFDIR | 0o755,
                    0,
                    0,
                )
                .await
                .expect("mkdir")
                .ino;
            if routed.route_ino(ino).0 == 0 {
                break format!("/projects/{name}");
            }
            names += 1;
        }
    })
    .await;

    // `home` lives on volume 0; assign it as volume 1's subtree root.
    let err = config_ops::set_owners(
        &u,
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, Some(&home))],
        &accepting(0),
    )
    .await
    .expect_err("a root whose ino homes elsewhere must refuse");
    let text = err.to_string();
    assert!(
        text.contains("volume locate"),
        "the refusal must name the instrument: {text}"
    );
    assert!(
        text.contains(&id0) && text.contains(&id1),
        "…the observed volume and the named one: {text}"
    );
    assert!(owner_of(&vols[0]).await.is_none(), "nothing written");

    // A missing PARENT refuses rather than being created implicitly.
    let err = config_ops::set_owners(
        &u,
        &[
            spec(&id0, NODE_A, None),
            spec(&id1, NODE_B, Some("/nowhere/b")),
        ],
        &accepting(0),
    )
    .await
    .expect_err("a missing parent must refuse");
    assert!(
        err.to_string().contains("/nowhere"),
        "naming the parent it could not resolve: {err}"
    );
}

/// Omitting `:<path>` is legal and LOUD: that node will own a volume but
/// no work (the Issue-23 shape, risk R16).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_volume_assigned_without_a_subtree_root_warns_that_the_node_will_own_no_new_work() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "no-root").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);

    let report = config_ops::set_owners(
        &u,
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &plain(),
    )
    .await
    .expect("an unrooted assignment is admitted, not refused");
    assert_eq!(report.roots_minted, 0);
    let warned = report.warnings.join("\n");
    assert!(
        warned.contains(&id1) && warned.contains(NODE_B),
        "the warning names the volume and its owner: {warned}"
    );
    assert!(
        warned.contains("no new work") || warned.contains("no NEW work"),
        "…and what it costs: {warned}"
    );
    assert!(
        !warned.contains(&id0),
        "…and NOT the slot-0 volume: ino 1 homes there, so its owner owns every ino that is \
         not under another owner's subtree. A false warning is how a true one stops being \
         read: {warned}"
    );
}

// ---------------------------------------------------------------------------
// The read verbs
// ---------------------------------------------------------------------------

/// `volume locate` answers the question nothing in the CLI could answer:
/// which volume hosts this path's inode, and who owns that volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volume_locate_names_the_hosting_volume_and_its_owner() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "locate").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);
    with_write_set(&u, |routed| async move {
        routed
            .create(1, "projects", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir /projects");
    })
    .await;
    assign(
        &u,
        &[
            spec(&id0, NODE_A, Some("/projects/a")),
            spec(&id1, NODE_B, Some("/projects/b")),
        ],
    )
    .await;

    let root = config_ops::locate_path(&u, "/").await.expect("locate /");
    assert_eq!(root.ino, 1);
    assert!(root.hosts_slot_0, "ino 1 pins to slot 0 (KD-PV-6)");
    assert_eq!(root.owner.as_deref(), Some(NODE_A));

    let b = config_ops::locate_path(&u, "/projects/b")
        .await
        .expect("locate the peer's subtree root");
    assert_eq!(b.volume_id, id1);
    assert_eq!(b.owner.as_deref(), Some(NODE_B));
    assert!(!b.hosts_slot_0);

    let err = config_ops::locate_path(&u, "/projects/nope")
        .await
        .expect_err("an unresolvable path refuses");
    assert!(err.to_string().contains("/projects/nope"), "{err}");
}

/// `get-owners` prints assignment BESIDE evidence — it is the drift
/// instrument, so a volume whose assigned owner is not claiming must say
/// so in words.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_owners_renders_assignment_beside_evidence_and_names_drift() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "drift").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);

    let mut a = spec(&id0, NODE_A, None);
    a.successors = vec![NODE_C.to_string()];
    config_ops::set_owners(&u, &[a, spec(&id1, NODE_B, None)], &plain())
        .await
        .expect("assign");

    let rows = config_ops::get_owners(&u).await.expect("get-owners");
    assert_eq!(rows.len(), 2);
    assert!(rows[0].hosts_slot_0, "the slot-0 row is marked (D20)");
    assert_eq!(rows[0].owner.as_deref(), Some(NODE_A));
    assert_eq!(rows[0].successors, vec![NODE_C.to_string()]);
    assert_eq!(rows[1].volume_id, id1);
    for row in &rows {
        let drift = row.drift.clone().unwrap_or_default();
        assert!(
            drift.contains("not claiming"),
            "an assigned volume nobody claims is reported: {drift}"
        );
        assert!(
            drift.contains("not mounted"),
            "…and while NO volume of the set is claimed, it is reported as the fleet being \
             down rather than as drift — crying wolf on the state every assignment is made \
             in is how the row that matters stops being read: {drift}"
        );
    }

    // One volume claimed and the other not IS the drift shape, and a live
    // claim whose holder resolves to nothing is the sharper one: the
    // map's fail-closed poison predicate, seen offline.
    plant_claim(&vols[1], &foreign_claim()).await;
    let rows = config_ops::get_owners(&u).await.expect("get-owners");
    let idle = rows[0].drift.clone().unwrap_or_default();
    assert!(
        idle.contains("not claiming") && !idle.contains("not mounted"),
        "with a sibling claimed, an unclaimed assigned volume is real drift: {idle}"
    );
    let drift = rows[1].drift.clone().unwrap_or_default();
    assert!(
        drift.contains("holder") || drift.contains("resolves"),
        "an unattested live claim is named as such: {drift}"
    );
}

// ---------------------------------------------------------------------------
// Dry run, clear, and the durable state the verb writes
// ---------------------------------------------------------------------------

/// `--dry-run` shows exactly what would change — including the census —
/// and writes no ownership state at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dry_run_shows_the_plan_and_writes_no_ownership_state() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "dry-run").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);
    with_write_set(&u, |routed| async move {
        routed
            .create(1, "projects", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir /projects");
    })
    .await;

    let report = config_ops::set_owners(
        &u,
        &[
            spec(&id0, NODE_A, Some("/projects/a")),
            spec(&id1, NODE_B, Some("/projects/b")),
        ],
        &SetOwnersOptions {
            dry_run: true,
            ..SetOwnersOptions::default()
        },
    )
    .await
    .expect("a dry run over a healthy set reports the plan");

    assert!(report.dry_run);
    assert_eq!(report.records_written, 0);
    assert_eq!(report.roots_minted, 0);
    assert_eq!(report.roots.len(), 2, "it names the roots it WOULD mint");
    assert!(
        report.census.dentries_scanned > 0,
        "the M3 pass RAN: a dry run REPORTS an unacknowledged population — refusing here \
         would make the plan unprintable exactly when the operator needs the number"
    );
    assert_eq!(report.set_authority.as_deref(), Some(NODE_A));
    assert!(
        report.volumes.iter().all(|v| v.previous_owner.is_none()),
        "and what stands today"
    );

    assert!(!marker_present(&vols[0]).await);
    assert!(raw_claim_set(&vols[0]).await.is_none());
    assert!(config_ops::locate_path(&u, "/projects/a").await.is_err());
}

/// KD-PV-4 + sweep row 17 + §5.9.2, in the one act D19 says they belong
/// to: the roster enrollment is PID-LESS (so the rung-8 same-boot prune
/// can never manufacture an assignment-vs-enrollment disagreement), the
/// stale rendezvous records leave the non-slot-0 volumes, and every
/// volume is checkpointed so the assignment is projection-visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bracket_enrolls_pid_less_members_and_scopes_the_rendezvous_to_slot_0() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "enroll").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);

    // A stale rendezvous record on the volume a PEER will own: nobody but
    // this verb can ever remove it once the peer stops writing there.
    for path in &vols {
        let be = KvMetaBackend::open(path).await.expect("open");
        membership::publish_owner_record(
            &be,
            &membership::OwnerRecord {
                v: 1,
                id: "stale-owner-uuid".to_string(),
                term: 3,
                endpoint: "127.0.0.1:9".to_string(),
                ttl_ms: 30_000,
                owner_claim_id: NODE_A.to_string(),
                ts: 1,
                pid: 1,
                boot: "b".to_string(),
            },
        )
        .await
        .expect("plant a rendezvous record");
        be.shutdown().await.expect("release");
    }

    let mut a = spec(&id0, NODE_A, None);
    a.successors = vec![NODE_C.to_string()];
    let report = config_ops::set_owners(&u, &[a, spec(&id1, NODE_B, None)], &plain())
        .await
        .expect("assign");
    assert_eq!(report.members_enrolled, 3, "owners ∪ successors");

    for path in &vols {
        let probe = KvMetaBackend::open_probe(path).await.expect("probe");
        let set = ClaimSet::load(&probe).await.expect("durable set");
        assert!(set.durable);
        let mut ids: Vec<&str> = set.members.iter().map(|m| m.identity.id.as_str()).collect();
        ids.sort_unstable();
        let mut want = vec![NODE_A, NODE_B, NODE_C];
        want.sort_unstable();
        assert_eq!(
            ids, want,
            "KD-PV-4: every named member is enrolled on EVERY volume"
        );
        for m in &set.members {
            assert_eq!(m.identity.role, MemberRole::Writer);
            assert_eq!(
                (m.identity.pid, m.identity.boot.as_str()),
                (0, ""),
                "KD-PV-4: the roster enrollment is deliberately process-less"
            );
        }
    }

    assert!(
        membership::read_owner_record(&KvMetaBackend::open_probe(&vols[0]).await.expect("probe"))
            .await
            .is_some(),
        "the slot-0 volume keeps its rendezvous record"
    );
    assert!(
        membership::read_owner_record(&KvMetaBackend::open_probe(&vols[1]).await.expect("probe"))
            .await
            .is_none(),
        "sweep row 17: a peer-owned volume's stale rendezvous record is deleted by the verb, \
         because nobody else can ever remove it"
    );
}

/// `--clear` restores the UNASSIGNED image byte-identically and takes no
/// superblock byte with it — the rollback §7 promises. (The KD-PV-4
/// roster enrollment is a separate durable act and survives, exactly as
/// PR 2's byte-identity pin has it.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_owners_clear_restores_the_unassigned_record_byte_identically() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "clear").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);
    let sector0_before: Vec<Vec<u8>> = vols
        .iter()
        .map(|v| std::fs::read(v).expect("read")[..sb::SUPERBLOCK_V3_LEN].to_vec())
        .collect();

    config_ops::set_owners(
        &u,
        &[spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)],
        &plain(),
    )
    .await
    .expect("assign");

    let report = config_ops::set_owners(
        &u,
        &[],
        &SetOwnersOptions {
            clear: true,
            ..SetOwnersOptions::default()
        },
    )
    .await
    .expect("--clear");
    assert_eq!(report.records_written, 2);

    for (i, path) in vols.iter().enumerate() {
        let probe = KvMetaBackend::open_probe(path).await.expect("probe");
        let set = ClaimSet::load(&probe).await.expect("the record survives");
        assert!(set.owner.is_none() && set.successors.is_empty() && set.holder.is_none());
        let raw = probe
            .getxattr(1, CLAIM_SET_XATTR)
            .await
            .expect("read")
            .expect("record");
        assert_eq!(
            raw,
            set.encode(),
            "the cleared record IS the unassigned image, byte for byte"
        );
        assert_eq!(
            std::fs::read(path).expect("read")[..sb::SUPERBLOCK_V3_LEN].to_vec(),
            sector0_before[i],
            "ownership takes NO incompat bit — sector 0 never moves"
        );
    }

    // And the set derives all-local again: a plain writable mount serves.
    let routed = squeezefs::meta_backend::open_routed_meta_set(&u)
        .await
        .expect("a cleared set mounts as today's sole authority");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// The verb's ledger (§11.1): one assignment per volume record written,
/// and an idempotent re-run of a COMPLETE assignment writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_verb_counts_its_assignments_and_a_complete_re_run_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let vols = two_volumes(dir.path(), "ledger").await;
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let u = uris(&vols);
    let specs = [spec(&id0, NODE_A, None), spec(&id1, NODE_B, None)];

    let before = squeezefs::meta_ship::stats().owner_assignments;
    config_ops::set_owners(&u, &specs, &plain())
        .await
        .expect("assign");
    assert_eq!(
        squeezefs::meta_ship::stats().owner_assignments,
        before + 2,
        "owner_assignments counts the volumes assigned"
    );

    let raw: Vec<Option<Vec<u8>>> =
        vec![raw_claim_set(&vols[0]).await, raw_claim_set(&vols[1]).await];
    let report = config_ops::set_owners(&u, &specs, &plain())
        .await
        .expect("re-running the same act converges");
    assert_eq!(
        report.records_written, 0,
        "an already-applied assignment writes NOTHING"
    );
    assert_eq!(raw[0], raw_claim_set(&vols[0]).await);
    assert_eq!(raw[1], raw_claim_set(&vols[1]).await);
    assert!(!marker_present(&vols[0]).await);
}
