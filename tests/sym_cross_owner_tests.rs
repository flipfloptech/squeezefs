//! Symmetric metadata program, PR 6 — **cross-owner transactions**
//! (`docs/design-symmetric-metadata.md` §5.6 D18 reversed, §5.6.4 the
//! set-wide directory-rename lock, §5.3.5 idempotent verbs, §11 the
//! Cross-owner family; KD-SYM-14).
//!
//! Under `SQUEEZEFS_SYMMETRIC_META=1` a namespace mutation whose objects
//! live in slots OTHER appenders lease is one S3.5 intent whose foreign
//! steps SHIP to their slot holders as S8 verbs: the initiator's half and
//! the intent land in ONE entry of the initiator's ring, the intent's
//! ring is barriered, every foreign step travels to the slot's holder
//! (`xv_apply_step` stays the ONE applier — the served side is the same
//! function), the holder's reply follows its durability lane, and the
//! intent retires. Roll-forward is whoever recovers the initiator's ring;
//! a holder's successor applies-or-recognizes exactly once under the
//! `(pre, post)` witness. Every `rename` of a DIRECTORY takes the set-wide
//! `dir_rename` lease on volume 0's manager and checks ancestry on exact
//! data.
//!
//! **The multi-holder shape** is PR 2–4's: ONE process holds region 0
//! (the manager — the initiator's own appender) and the DECLARED regions
//! the seam names (`SQUEEZEFS_TEST_SYM_APPENDER_SLOTS`), each with its own
//! ring, its own lease set and its own tree-0 lessee record; a step homed
//! on a declared region's slot is FOREIGN to appender 0 and ships over a
//! real `cluster_wire` session to the endpoint registered for that
//! appender — the S8 owner service over the same backend, which applies
//! it under ITS lease and ITS ring. N daemon processes on one volume is
//! PR 12's join ladder.
//!
//! **`SQUEEZEFS_SYMMETRIC_META=0` is the shipped posture exactly**: the
//! S3.5 same-volume / cross-volume paths, intents at ino 0, no ship, no
//! lock, every Cross-owner gauge 0.

use squeezefs::cluster_wire as cw;
use squeezefs::data_grant::AsyncVerbRouter;
use squeezefs::meta_backend::crossvol_tx::{
    self, cross_owner_stats, install_xv_shipper, uninstall_xv_shipper, TEST_XV_SEAM_AFTER_STEPS,
    TEST_XV_SERVE_MISDELIVER_ONCE, TEST_XV_SERVE_REFUSE, TEST_XV_STUCK_AFTER_MS,
};
use squeezefs::meta_backend::kv::appender::TEST_APPENDER_SLOTS_ENV;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options, ROOT_INO};
use squeezefs::meta_backend::kv::record::ForestSlot;
use squeezefs::meta_backend::kv::slot_lease::SYMMETRIC_META_ENV;
use squeezefs::meta_backend::{
    make_global_ino_width, open_routed_meta_set, plan_meta_slot_set, IntentCreatePreset, Metadata,
    RoutedMetaBackend,
};
use squeezefs::meta_ship::manager::{ManagerClient, ManagerReply, ManagerSetService};
use squeezefs::meta_ship::{MetaShipRouter, MetaShipService};
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Harness (the sym_slot_transfer_tests shape: 64 KiB nodes, a 1 MiB ring).
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const SECRET: &[u8] = b"sym-cross-owner-tests-enroll-secret";

/// Forest slot 4 (routing slot 3) — the declared appender 1's.
const SLOT_B: ForestSlot = 4;
/// Forest slot 6 (routing slot 5) — the declared appender 2's.
const SLOT_C: ForestSlot = 6;
const TWO_HOLDERS: &str = "1:4";
const THREE_HOLDERS: &str = "1:4;2:6";

/// The seams and knobs are process-global; every test serializes on it.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The volume-set format config the member records (what the offline
/// fsck harness reads back to build its router), naming one file-backed
/// data volume under `dir`.
fn format_config_for(dir: &std::path::Path) -> Vec<u8> {
    let oss = dir.join("oss0");
    std::fs::File::create(&oss)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let cfg = squeezefs::FormatConfig {
        name: "squeezefs".to_string(),
        block_size: 4096,
        capacity: 1 << 30,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(vec![oss.display().to_string()]),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    };
    serde_json::to_vec(&cfg).unwrap()
}

fn set_opts(dir: &std::path::Path) -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: Some(format_config_for(dir)),
    }
}

async fn format_member(dir: &std::path::Path, name: &str, stamped: bool) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    if stamped {
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    } else {
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    }
    let r = format_v3_stamped(&p, VOL_LEN, &set_opts(dir), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format member");
    p.display().to_string()
}

fn apply_knobs(armed: bool, partition: Option<&str>) {
    if armed {
        std::env::set_var(SYMMETRIC_META_ENV, "1");
    } else {
        std::env::remove_var(SYMMETRIC_META_ENV);
    }
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    match partition {
        Some(p) => std::env::set_var(TEST_APPENDER_SLOTS_ENV, p),
        None => std::env::remove_var(TEST_APPENDER_SLOTS_ENV),
    }
}

fn clear_knobs() {
    for k in [
        SYMMETRIC_META_ENV,
        "SQUEEZEFS_SYM_ALLOW_NON_PR",
        TEST_APPENDER_SLOTS_ENV,
    ] {
        std::env::remove_var(k);
    }
}

/// Open the set under the knobs; retry while a previous incarnation's
/// detached tasks still pin the flock (harness plumbing, as in the S3.5
/// suite).
async fn open_under(
    uris: &[String],
    armed: bool,
    partition: Option<&str>,
) -> Arc<RoutedMetaBackend> {
    let mut last = String::new();
    for _ in 0..200 {
        apply_knobs(armed, partition);
        let r = open_routed_meta_set(uris).await;
        clear_knobs();
        match r {
            Ok(r) => return r,
            Err(e) => {
                last = format!("{e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    panic!("the writer guard must release once the previous set is dropped: {last}");
}

async fn shutdown(routed: &RoutedMetaBackend) {
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

fn listener_cfg() -> cw::RpcListenerConfig {
    cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    }
}

/// A sequenced service front (test-local): the listener keeps one
/// `Arc<dyn RpcAsyncService>`, so a HOLDER RESTART — a fresh
/// `MetaShipService` with an EMPTY dedup window — is modelled by handing
/// the N-th call to the N-th service (the last one serves every call
/// past the list). One service = the ordinary front.
struct SwapFront {
    services: Vec<Arc<dyn cw::RpcAsyncService>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl cw::RpcAsyncService for SwapFront {
    fn call<'a>(
        &'a self,
        req: cw::RpcRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = cw::RpcResponse> + Send + 'a>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let svc = &self.services[n.min(self.services.len() - 1)];
            svc.call(req).await
        })
    }
}

fn meta_router(routed: &Arc<RoutedMetaBackend>) -> Arc<dyn cw::RpcAsyncService> {
    Arc::new(
        AsyncVerbRouter::new()
            .with_meta(MetaShipService::new(Arc::clone(routed)))
            .with_manager(ManagerSetService::new(&routed.volumes)),
    )
}

/// The holders' venue: the S8 owner service over `routed` on a listener,
/// every declared appender's endpoint registered on the plane, and the
/// initiator's shipper installed — what PR 12's join ladder wires from
/// the census.
struct Holders {
    host: Arc<cw::RpcListener>,
}

impl Holders {
    async fn stand_up(routed: &Arc<RoutedMetaBackend>, appenders: &[u32]) -> Self {
        Self::stand_up_sequenced(routed, appenders, 1).await
    }

    /// `incarnations` services in sequence — the second one is a holder
    /// RESTART (an empty dedup window) for the call after the first.
    async fn stand_up_sequenced(
        routed: &Arc<RoutedMetaBackend>,
        appenders: &[u32],
        incarnations: usize,
    ) -> Self {
        let front = Arc::new(SwapFront {
            services: (0..incarnations).map(|_| meta_router(routed)).collect(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let host = cw::RpcListener::start_async(
            listener_cfg(),
            SECRET.to_vec(),
            front as Arc<dyn cw::RpcAsyncService>,
        )
        .expect("owner listener");
        let endpoint = host.endpoint().to_string();
        for vol in &routed.volumes {
            let plane = vol.slot_leases().expect("armed");
            for id in appenders {
                plane.holders.set_endpoint(*id, &endpoint);
            }
        }
        install_xv_shipper(MetaShipRouter::new(
            Arc::clone(routed),
            "node-b",
            SECRET.to_vec(),
        ));
        Self { host }
    }

    fn tear_down(self) {
        uninstall_xv_shipper();
        self.host.shutdown();
    }
}

/// Mint a DIRECTORY under the root whose ino routes to forest slot
/// `slot` while the slot is still the manager's (a preset ino routes
/// itself), then release the slot so the next open's declared region
/// takes it (`Unleased` in tree 0 — the seam's wish-list grants it).
async fn seed_dir_in_slot(routed: &RoutedMetaBackend, slot: ForestSlot, name: &str) -> u64 {
    let vol = &routed.volumes[0];
    let width = routed.routing_width();
    let routing = u64::from(slot) - 1;
    let local = vol
        .allocate_guest_ino(routing as u16)
        .expect("a guest cursor");
    let global = make_global_ino_width(local, routing, width);
    let ino = routed
        .create_with_rdev_preset(
            ROOT_INO,
            name,
            libc::S_IFDIR | 0o755,
            0,
            0,
            0,
            0,
            Some(IntentCreatePreset {
                global_ino: global,
                ts_ns: KvMetaBackend::now_ns_pub(),
            }),
        )
        .await
        .expect("seed dir")
        .ino;
    assert_eq!(ino, global);
    ino
}

/// A stamped volume with one directory per `slots` entry, each slot
/// released to `Unleased` in tree 0 so the declared regions of the NEXT
/// open take them. Returns `(uris, dir inos in `slots` order)`.
async fn seeded_volume(dir: &std::path::Path, slots: &[ForestSlot]) -> (Vec<String>, Vec<u64>) {
    let uris = vec![format_member(dir, "meta0", true).await];
    let routed = open_under(&uris, true, None).await;
    let mut dirs = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        dirs.push(seed_dir_in_slot(&routed, *slot, &format!("shared{i}")).await);
    }
    let vol = Arc::clone(&routed.volumes[0]);
    for slot in slots {
        vol.release_slot_handover(0, *slot)
            .await
            .expect("release to unleased");
    }
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    (uris, dirs)
}

fn slot_of(routed: &RoutedMetaBackend, ino: u64) -> ForestSlot {
    let (_, local) = routed.route_ino(ino);
    squeezefs::meta_backend::kv::record::forest_slot_of_ino(local)
}

async fn open_intents(routed: &RoutedMetaBackend) -> usize {
    let mut n = 0;
    for vol in &routed.volumes {
        n += vol.xv_scan_intents().await.expect("scan intents").len();
    }
    n
}

async fn names_in(routed: &RoutedMetaBackend, dir: u64) -> Vec<String> {
    let mut names: Vec<String> = routed
        .readdir(dir, 0, 4096)
        .await
        .expect("readdir")
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n != "." && n != "..")
        .collect();
    names.sort();
    names
}

async fn lookup_opt(routed: &RoutedMetaBackend, parent: u64, name: &str) -> Option<u64> {
    routed.lookup(parent, name).await.ok().map(|i| i.ino)
}

async fn fsck_clean(uris: &[String]) {
    let mut opts = squeezefs::fsck::FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(10);
    let report = squeezefs::fsck::run_offline(uris, &opts)
        .await
        .expect("offline fsck runs");
    assert!(
        !report.has_findings(),
        "fsck must be clean: {:?}",
        report.findings
    );
}

/// Every Cross-owner gauge at rest: minted ≡ retired, nothing open,
/// nothing stuck.
fn assert_closed(what: &str) {
    let s = cross_owner_stats();
    assert_eq!(
        s.intents_minted,
        s.intents_retired + s.intents_open,
        "{what}: minted ≡ retired + open"
    );
    assert_eq!(s.intents_open, 0, "{what}: no open intent");
    assert_eq!(
        s.intents_stuck, 0,
        "{what}: xv_cross_owner_intents_stuck must stay 0"
    );
}

// ---------------------------------------------------------------------------
// §5.6 — create in a foreign directory (the D1 shape).
// ---------------------------------------------------------------------------

/// The sequence diagram of §5.6: `creat(D/name)` with `D`'s slot leased by
/// another appender mints the child in the CREATOR's rotor slot (affinity
/// never follows a foreign parent), commits `[inode(child), intent]` as
/// ONE entry in the creator's ring, ships exactly ONE `InsertDentry` to
/// the holder — which commits the dentry + the parent's Δtime in ITS ring
/// — and retires the intent. The name resolves and lists; the parent's
/// `nlink` moved for a `mkdir`; the ledger closes; no lock RPC was paid
/// (`dlm_rpcs == 0`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_in_a_foreign_directory_ships_one_insert_dentry_and_mints_in_the_creators_rotor() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(
        plane.holders.holder(SLOT_B).map(|h| h.appender_id),
        Some(1),
        "the declared region leases the shared directory's slot"
    );
    let parent_nlink = routed.getattr(shared).await.unwrap().nlink;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let before = cross_owner_stats();
    let before_rpcs = squeezefs::dlm_slot::dlm_rpcs();

    let file = routed
        .create(shared, "out.bin", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create in a foreign directory is an ordinary op");
    let sub = routed
        .create(shared, "sub", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir in a foreign directory");

    let rotor = plane.rotor.load();
    for child in [file.ino, sub.ino] {
        let s = slot_of(&routed, child);
        assert!(
            rotor.contains(&s),
            "child {child} minted in forest slot {s}, not one of the creator's rotor {rotor:?}"
        );
        assert_ne!(s, SLOT_B, "never in the foreign parent's slot");
    }
    assert_eq!(lookup_opt(&routed, shared, "out.bin").await, Some(file.ino));
    assert_eq!(lookup_opt(&routed, shared, "sub").await, Some(sub.ino));
    assert_eq!(names_in(&routed, shared).await, vec!["out.bin", "sub"]);
    assert_eq!(
        routed.getattr(shared).await.unwrap().nlink,
        parent_nlink + 1,
        "the mkdir bumped the foreign parent's nlink through the holder's step"
    );
    assert_eq!(routed.getattr(sub.ino).await.unwrap().nlink, 2);

    let after = cross_owner_stats();
    assert_eq!(after.intents_minted - before.intents_minted, 2);
    assert_eq!(after.intents_retired - before.intents_retired, 2);
    assert_eq!(
        after.steps_shipped - before.steps_shipped,
        2,
        "exactly ONE shipped step per create — the InsertDentry"
    );
    assert_eq!(after.steps_served - before.steps_served, 2);
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        before_rpcs,
        "no lock round trip: the holder applies under ITS 4a guards"
    );
    assert_eq!(open_intents(&routed).await, 0);
    assert_closed("create");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The whole verb set across TWO holders leaves the tree byte-exact:
/// `unlink` of a foreign-directory entry (the child ours), `rmdir` of a
/// foreign-directory subdirectory, `link` of our file into the foreign
/// directory, `rename` out of it into ours and back, `RENAME_EXCHANGE`
/// across the two — each one intent, each retired, fsck clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlink_rmdir_link_rename_and_exchange_across_two_holders_leave_a_byte_exact_tree() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let mine = routed
        .create(ROOT_INO, "mine", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    assert_ne!(slot_of(&routed, mine), SLOT_B);
    let before = cross_owner_stats();

    // create + unlink in the foreign directory.
    let f = routed
        .create(shared, "gone", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert_eq!(routed.unlink(shared, "gone").await.unwrap(), f.ino);
    assert_eq!(lookup_opt(&routed, shared, "gone").await, None);
    assert!(
        routed.getattr(f.ino).await.is_err() || routed.getattr(f.ino).await.unwrap().nlink == 0,
        "the unlinked child is unreferenced (reclaimable)"
    );
    // mkdir + rmdir in the foreign directory.
    routed
        .create(shared, "tmpdir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let nl = routed.getattr(shared).await.unwrap().nlink;
    routed.unlink(shared, "tmpdir").await.unwrap();
    assert_eq!(routed.getattr(shared).await.unwrap().nlink, nl - 1);
    // link: our file gains a second name in the foreign directory.
    let a = routed
        .create(mine, "a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let linked = routed.link(a.ino, shared, "a_link").await.unwrap();
    assert_eq!(linked.nlink, 2);
    assert_eq!(lookup_opt(&routed, shared, "a_link").await, Some(a.ino));
    assert_eq!(routed.getattr(a.ino).await.unwrap().nlink, 2);
    // rename: out of the foreign directory into ours, and back.
    routed.rename(shared, "a_link", mine, "b", 0).await.unwrap();
    assert_eq!(lookup_opt(&routed, shared, "a_link").await, None);
    assert_eq!(lookup_opt(&routed, mine, "b").await, Some(a.ino));
    routed.rename(mine, "b", shared, "c", 0).await.unwrap();
    assert_eq!(lookup_opt(&routed, shared, "c").await, Some(a.ino));
    assert_eq!(routed.getattr(a.ino).await.unwrap().nlink, 2);
    // rename-over: a foreign name replaced by ours (the victim settles).
    let victim = routed
        .create(shared, "victim", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    routed.rename(mine, "a", shared, "victim", 0).await.unwrap();
    assert_eq!(lookup_opt(&routed, shared, "victim").await, Some(a.ino));
    assert_eq!(lookup_opt(&routed, mine, "a").await, None);
    assert!(
        routed.getattr(victim.ino).await.is_err()
            || routed.getattr(victim.ino).await.unwrap().nlink == 0
    );
    // exchange across the two directories.
    let x = routed
        .create(mine, "x", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    routed
        .rename(mine, "x", shared, "c", libc::RENAME_EXCHANGE)
        .await
        .unwrap();
    assert_eq!(lookup_opt(&routed, mine, "x").await, Some(a.ino));
    assert_eq!(lookup_opt(&routed, shared, "c").await, Some(x.ino));

    assert_eq!(names_in(&routed, shared).await, vec!["c", "victim"]);
    assert_eq!(names_in(&routed, mine).await, vec!["x"]);
    let after = cross_owner_stats();
    assert!(
        after.intents_minted - before.intents_minted >= 9,
        "every cross-owner op minted an intent: {:?}",
        after
    );
    assert_eq!(after.intents_minted, after.intents_retired);
    assert_eq!(open_intents(&routed).await, 0);
    assert_closed("verb set");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// Three holders: a rename whose old parent, new parent and moved child
/// live in three different appenders' slots — two shipped steps (one per
/// foreign dentry side), the child's own ctime local — and a link from
/// one foreign directory's file into the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rename_across_three_holders_ships_a_step_to_each_foreign_side() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B, SLOT_C]).await;
    let (db, dc) = (dirs[0], dirs[1]);
    let routed = open_under(&uris, true, Some(THREE_HOLDERS)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(plane.holders.holder(SLOT_B).map(|h| h.appender_id), Some(1));
    assert_eq!(plane.holders.holder(SLOT_C).map(|h| h.appender_id), Some(2));
    let holders = Holders::stand_up(&routed, &[1, 2]).await;
    let f = routed
        .create(db, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert!(plane.rotor.load().contains(&slot_of(&routed, f.ino)));
    let before = cross_owner_stats();
    routed.rename(db, "f", dc, "g", 0).await.unwrap();
    let after = cross_owner_stats();
    assert_eq!(after.intents_minted - before.intents_minted, 1);
    assert_eq!(
        after.steps_shipped - before.steps_shipped,
        2,
        "RemoveDentry to appender 1, InsertDentry to appender 2"
    );
    assert_eq!(lookup_opt(&routed, db, "f").await, None);
    assert_eq!(lookup_opt(&routed, dc, "g").await, Some(f.ino));
    let linked = routed.link(f.ino, db, "h").await.unwrap();
    assert_eq!(linked.nlink, 2);
    assert_eq!(names_in(&routed, db).await, vec!["h"]);
    assert_eq!(names_in(&routed, dc).await, vec!["g"]);
    assert_closed("three holders");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// §5.6 — the crash windows (the S3.5 table one axis over).
// ---------------------------------------------------------------------------

/// The INITIATOR dies (a) after `tx0` (child + intent durable, nothing
/// shipped) and (b) after the ship (the holder committed, the intent not
/// yet retired): the next mount's `recover_open_intents` re-ships under
/// the intent — the holder applies (a) or recognizes (b) exactly once —
/// and retires it. Window (c) — before any step — leaves nothing. No
/// window acks a create that is not there afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_initiator_crash_window_of_a_foreign_create_rolls_forward_with_no_acked_loss() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    // 1 = before any step; 2 = after tx0 (child + intent); 3 = after the
    // shipped InsertDentry, before the retirement.
    for window in 1..=3u64 {
        let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
        let holders = Holders::stand_up(&routed, &[1]).await;
        let name = format!("w{window}");
        let out = {
            TEST_XV_SEAM_AFTER_STEPS.store(window, Ordering::Relaxed);
            let out = routed
                .create(shared, &name, libc::S_IFREG | 0o644, 0, 0)
                .await;
            TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::Relaxed);
            out
        };
        assert!(out.is_err(), "the seam severs the plan (window {window})");
        let listed_before = names_in(&routed, shared).await;
        holders.tear_down();
        // Crash: drop without shutdown; reopen — recovery runs before the
        // mount serves.
        drop(routed);
        let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
        let holders = Holders::stand_up(&routed, &[1]).await;
        // Recovery ships what the window left: the intent is gone either
        // way, and the name is present iff the intent was durable.
        crossvol_tx::roll_forward_open_intents(&routed)
            .await
            .expect("roll-forward");
        assert_eq!(open_intents(&routed).await, 0, "window {window}: retired");
        let listed = names_in(&routed, shared).await;
        if window == 1 {
            assert!(
                !listed.contains(&name),
                "window {window}: nothing was durable, nothing appears"
            );
        } else {
            assert!(
                listed.contains(&name),
                "window {window}: the intent was durable ⇒ rolled forward ({listed_before:?} → {listed:?})"
            );
            let ino = lookup_opt(&routed, shared, &name).await.expect("the child");
            assert_eq!(routed.getattr(ino).await.unwrap().nlink, 1);
        }
        assert_eq!(
            listed.iter().filter(|n| **n == name).count(),
            usize::from(window != 1),
            "window {window}: exactly once, never twice"
        );
        assert_eq!(cross_owner_stats().intents_stuck, 0);
        holders.tear_down();
        shutdown(&routed).await;
    }
    fsck_clean(&uris).await;
}

/// The HOLDER dies after committing the step and before replying, and its
/// SUCCESSOR has no dedup window: the initiator's resend (same request
/// id) reaches a fresh service, whose `xv_apply_step` recognizes the
/// dentry it already holds — `AlreadyApplied`, exactly once — and the
/// intent retires. Modelled with the misdelivered-reply seam (the client
/// refuses a reply whose correlation id is not its call's, exactly as a
/// dead session would fail it) plus a service swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_dying_after_commit_before_reply_is_recognized_exactly_once_by_its_successor() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    // Two incarnations of the holder: the first serves ONE call (the step
    // it commits and then misdelivers), the second — a fresh service with
    // an empty dedup window — serves the resend.
    let holders = Holders::stand_up_sequenced(&routed, &[1], 2).await;
    let applied_before = crossvol_tx::XV_STEPS_APPLIED.load(Ordering::Relaxed);
    let already_before = crossvol_tx::XV_STEPS_ALREADY_APPLIED.load(Ordering::Relaxed);
    let before = cross_owner_stats();
    TEST_XV_SERVE_MISDELIVER_ONCE.store(true, Ordering::SeqCst);
    let f = routed
        .create(shared, "once", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("the resend completes the create");
    assert!(
        !TEST_XV_SERVE_MISDELIVER_ONCE.load(Ordering::SeqCst),
        "the seam fired on the first served step"
    );
    assert_eq!(lookup_opt(&routed, shared, "once").await, Some(f.ino));
    assert_eq!(
        names_in(&routed, shared)
            .await
            .iter()
            .filter(|n| *n == "once")
            .count(),
        1
    );
    let after = cross_owner_stats();
    assert_eq!(
        after.steps_served - before.steps_served,
        2,
        "the step was SERVED twice — the original and the resend"
    );
    assert_eq!(
        crossvol_tx::XV_STEPS_APPLIED.load(Ordering::Relaxed) - applied_before,
        2,
        "the child mint (local) and the dentry insert (served) applied ONCE each"
    );
    assert_eq!(
        crossvol_tx::XV_STEPS_ALREADY_APPLIED.load(Ordering::Relaxed) - already_before,
        1,
        "the successor RECOGNIZED the resend under the witness"
    );
    assert_eq!(open_intents(&routed).await, 0);
    assert_closed("holder restart");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The HOLDER dies BEFORE committing (the served step refuses at its
/// door): the initiator's op fails, its intent stays OPEN — the volume is
/// NOT fail-stopped (the armed plane's live mid-plan class is a retry,
/// never a lattice latch) — and the roll-forward cadence completes it
/// once the holder serves again. Past the grace window an intent no
/// holder serves counts on `xv_cross_owner_intents_stuck`; served, it
/// reads 0 again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_dying_before_commit_leaves_an_open_intent_the_cadence_rolls_forward() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let before = cross_owner_stats();
    TEST_XV_SERVE_REFUSE.store(true, Ordering::SeqCst);
    let e = routed
        .create(shared, "later", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect_err("the holder refused before committing");
    TEST_XV_SERVE_REFUSE.store(false, Ordering::SeqCst);
    assert!(
        !routed.disabled_volumes.contains_key(&0),
        "a live mid-plan failure under the armed plane never fail-stops the initiator: {e}"
    );
    assert_eq!(
        open_intents(&routed).await,
        1,
        "the intent stays open for roll-forward"
    );
    assert_eq!(cross_owner_stats().intents_open, 1);
    // The grace window (seam-shortened): the open intent reads STUCK
    // while no holder serves it…
    TEST_XV_STUCK_AFTER_MS.store(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    holders.tear_down();
    let n = crossvol_tx::roll_forward_open_intents(&routed)
        .await
        .expect("a scan with no holder endpoint is not an error");
    assert_eq!(n, 0, "nothing could be rolled forward without a holder");
    assert_eq!(
        cross_owner_stats().intents_stuck,
        1,
        "past the grace window: stuck"
    );
    // …and the cadence completes it once the holder serves.
    let holders = Holders::stand_up(&routed, &[1]).await;
    let n = crossvol_tx::roll_forward_open_intents(&routed)
        .await
        .expect("roll-forward");
    TEST_XV_STUCK_AFTER_MS.store(0, Ordering::SeqCst);
    assert_eq!(n, 1);
    assert_eq!(open_intents(&routed).await, 0);
    assert_eq!(cross_owner_stats().intents_stuck, 0);
    let ino = lookup_opt(&routed, shared, "later")
        .await
        .expect("rolled forward");
    assert_eq!(routed.getattr(ino).await.unwrap().nlink, 1);
    let after = cross_owner_stats();
    assert_eq!(after.intents_minted - before.intents_minted, 1);
    assert_eq!(after.intents_retired - before.intents_retired, 1);
    assert_closed("holder down");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// All parties down: the process dies after the ship, before the
/// retirement, and NOBODY is up until the next mount — which rolls the
/// intent forward before it serves (`recover_open_intents`, the S3.5
/// driver over the shipped applier). The rmdir and unlink shapes: the
/// initiator's half (the count) is step 0, the foreign name the shipped
/// step, and a recovered volume shows neither a name without its count
/// nor a count without its name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_parties_down_after_a_foreign_unlink_the_next_mount_rolls_forward() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let f_ino = {
        let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
        let holders = Holders::stand_up(&routed, &[1]).await;
        let f = routed
            .create(shared, "doomed", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        assert_eq!(routed.getattr(f.ino).await.unwrap().nlink, 1);
        // Sever after every step (the count locally, the name at the
        // holder) but before the retirement.
        TEST_XV_SEAM_AFTER_STEPS.store(3, Ordering::Relaxed);
        let out = routed.unlink(shared, "doomed").await;
        TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::Relaxed);
        assert!(out.is_err());
        holders.tear_down();
        drop(routed);
        f.ino
    };
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    crossvol_tx::roll_forward_open_intents(&routed)
        .await
        .expect("roll-forward");
    assert_eq!(open_intents(&routed).await, 0);
    assert_eq!(lookup_opt(&routed, shared, "doomed").await, None);
    assert!(
        routed.getattr(f_ino).await.is_err() || routed.getattr(f_ino).await.unwrap().nlink == 0,
        "the count followed the name"
    );
    assert_closed("all down");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// §5.6.4 — the set-wide directory-rename lock (KD-SYM-14).
// ---------------------------------------------------------------------------

/// Two initiators, disjoint ancestor views, concurrent `rename(a/b →
/// c/d/e)` and `rename(c → a/b/f)`: under the set-wide lock they
/// serialize, the ancestor check runs on EXACT data, and exactly ONE
/// completes — the other refuses `EINVAL` (POSIX: the target is inside
/// the source) — so no directory is ever its own ancestor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_cannot_rename_directories_into_a_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B, SLOT_C]).await;
    let (a, c) = (dirs[0], dirs[1]);
    let routed = open_under(&uris, true, Some(THREE_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1, 2]).await;
    let b = routed
        .create(a, "b", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let d = routed
        .create(c, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let before = cross_owner_stats();
    let r1 = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move { routed.rename(a, "b", d, "e", 0).await })
    };
    let r2 = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move { routed.rename(ROOT_INO, "shared1", b, "f", 0).await })
    };
    let (r1, r2) = (r1.await.unwrap(), r2.await.unwrap());
    let ok = usize::from(r1.is_ok()) + usize::from(r2.is_ok());
    assert_eq!(ok, 1, "exactly one completes: {r1:?} / {r2:?}");
    let refused = match (r1, r2) {
        (Err(e), Ok(())) | (Ok(()), Err(e)) => e,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        refused.to_errno(),
        libc::EINVAL,
        "the loser refuses EINVAL — the target lies inside the source: {refused}"
    );
    // No cycle: every directory's parent chain reaches the root.
    for d0 in [a, b, c, d] {
        let mut cur = d0;
        let mut hops = 0;
        while cur != ROOT_INO {
            cur = routed
                .parent_of_directory(cur)
                .await
                .expect("a parent")
                .expect("every live directory has one name");
            hops += 1;
            assert!(hops < 16, "a cycle: {d0} never reaches the root");
        }
    }
    let after = cross_owner_stats();
    assert_eq!(
        after.dir_rename_lock_acquires - before.dir_rename_lock_acquires,
        2,
        "both directory renames took the set-wide lock"
    );
    assert_closed("cycle");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// A FILE rename never takes the set-wide lock, whatever its slots; a
/// same-slot DIRECTORY rename does (a same-slot rename can complete the
/// cycle a cross-slot one is checking against).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_rename_never_takes_the_dir_rename_lock_and_every_directory_rename_does() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let mine = routed
        .create(ROOT_INO, "mine", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    routed
        .create(mine, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    routed
        .create(mine, "sub", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let before = cross_owner_stats();
    routed.rename(mine, "f", mine, "f2", 0).await.unwrap();
    routed.rename(mine, "f2", shared, "f3", 0).await.unwrap();
    assert_eq!(
        cross_owner_stats().dir_rename_lock_acquires,
        before.dir_rename_lock_acquires,
        "file renames never take it"
    );
    routed.rename(mine, "sub", mine, "sub2", 0).await.unwrap();
    assert_eq!(
        cross_owner_stats().dir_rename_lock_acquires - before.dir_rename_lock_acquires,
        1,
        "a same-slot directory rename takes it"
    );
    routed
        .rename(mine, "sub2", shared, "sub3", 0)
        .await
        .unwrap();
    assert_eq!(
        cross_owner_stats().dir_rename_lock_acquires - before.dir_rename_lock_acquires,
        2
    );
    assert_eq!(names_in(&routed, shared).await, vec!["f3", "sub3"]);
    assert_closed("lock scope");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The lock is a DURABLE record in tree 0 of volume 0 (`dir_rename`),
/// served by the manager's verbs on the wire: `DirRenameLock` grants or
/// answers `already` to its holder and `busy` to another; `DirRenameUnlock`
/// releases (`already` when absent); a holder that DIED keeps the record
/// until the recovery driver releases it (PR 10's death ledger — the
/// expiry law: the lock dies with the initiator's membership lease).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dir_rename_lock_is_durable_in_tree0_and_a_dead_holders_lock_is_released_by_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_member(dir.path(), "meta0", true).await];
    let routed = open_under(&uris, true, None).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        Arc::new(AsyncVerbRouter::new().with_manager(ManagerSetService::new(&routed.volumes))),
    )
    .unwrap();
    let endpoint = host.endpoint().to_string();
    let mut client = ManagerClient::connect(&endpoint, SECRET, "joiner-7", 0)
        .await
        .unwrap();
    assert!(vol.dir_rename_record().await.unwrap().is_none());
    match client.dir_rename_lock(7).await.unwrap() {
        ManagerReply::DirRenameLocked { already } => assert!(!already),
        other => panic!("{other:?}"),
    }
    let rec = vol.dir_rename_record().await.unwrap().expect("durable");
    assert_eq!(rec.holder, 7);
    match client.dir_rename_lock(7).await.unwrap() {
        ManagerReply::DirRenameLocked { already } => assert!(already, "KD-SYM-7"),
        other => panic!("{other:?}"),
    }
    match client.dir_rename_lock(9).await.unwrap() {
        ManagerReply::DirRenameBusy { holder } => assert_eq!(holder, 7),
        other => panic!("{other:?}"),
    }
    // The in-process manager's own take parks behind the wire holder;
    // a dead holder's record is released by the recovery driver's entry.
    let waiter = {
        let vol = Arc::clone(&vol);
        tokio::spawn(async move { vol.dir_rename_lock_held(0).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!waiter.is_finished(), "the manager parks behind the holder");
    vol.manager_dir_rename_release_dead(7).await.unwrap();
    let lock = waiter.await.unwrap().expect("granted after the release");
    assert_eq!(vol.dir_rename_record().await.unwrap().unwrap().holder, 0);
    lock.release().await.unwrap();
    assert!(vol.dir_rename_record().await.unwrap().is_none());
    match client.dir_rename_unlock(7).await.unwrap() {
        ManagerReply::DirRenameUnlocked { already } => assert!(already),
        other => panic!("{other:?}"),
    }
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    host.shutdown();
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// The shipped posture, byte-identical.
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_SYMMETRIC_META=0` (a bit-17 volume) and a bit-17-ABSENT
/// volume take the S3.5 paths verbatim: intents key on ino 0, no step
/// ships, no lock is taken, every Cross-owner gauge stays where it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_unarmed_and_flat_paths_ship_nothing_and_lock_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    for (name, stamped) in [("flat", false), ("dark", true)] {
        let uris = vec![format_member(dir.path(), name, stamped).await];
        let routed = open_under(&uris, false, None).await;
        let before = cross_owner_stats();
        let a = routed
            .create(ROOT_INO, "a", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        let b = routed
            .create(ROOT_INO, "b", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        let f = routed
            .create(a, "f", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        routed.rename(a, "f", b, "g", 0).await.unwrap();
        routed.rename(ROOT_INO, "a", b, "a2", 0).await.unwrap();
        routed.link(f.ino, b, "h").await.unwrap();
        routed.unlink(b, "h").await.unwrap();
        let after = cross_owner_stats();
        assert_eq!(
            after, before,
            "{name}: the Cross-owner family never moves unarmed"
        );
        assert_eq!(open_intents(&routed).await, 0);
        assert!(!routed.volumes[0].slot_lease_armed());
        shutdown(&routed).await;
    }
}

/// The stats family's shape: every gauge of §11's Cross-owner family
/// exists on the snapshot, the phase table names `plan / intent_barrier /
/// ship_rtt / retire / total`, and the JSON the stats inode serves carries
/// them under their published names.
#[test]
fn the_cross_owner_family_is_exported_under_its_published_names() {
    let json = crossvol_tx::cross_owner_stats_json();
    for key in [
        "xv_cross_owner_intents_minted",
        "xv_cross_owner_intents_retired",
        "xv_cross_owner_intents_open",
        "xv_cross_owner_steps_shipped",
        "xv_cross_owner_steps_served",
        "xv_cross_owner_intents_stuck",
        "xv_cross_owner_phase_ns",
        "xv_cross_owner_guard_rpcs",
        "xv_cross_owner_guards_parked",
        "xv_cross_owner_guard_expiries",
        "dir_rename_lock_acquires",
        "dir_rename_lock_wait_ns",
    ] {
        assert!(json.get(key).is_some(), "missing {key}: {json:?}");
    }
    let phases = json["xv_cross_owner_phase_ns"]
        .as_object()
        .expect("a phase table");
    for p in [
        "guard_rtt",
        "plan",
        "intent_barrier",
        "ship_rtt",
        "retire",
        "total",
    ] {
        assert!(phases.contains_key(p), "missing phase {p}");
    }
}

// ---------------------------------------------------------------------------
// The 4a guards travel (design §5.6 line 1) — found by the scoping row.
// ---------------------------------------------------------------------------

/// A served step never PARKS behind the initiator's held guards. The
/// scoping row's 27th cross-holder directory rename found the holder's
/// `insert_dentry` waiting on a 4a stripe the initiator held across the
/// ship — in one process a stripe collision (the shared table), in a
/// fleet the two-mutual-initiator cycle the design's "foreign-home guards
/// travel" line exists to make impossible. The collision is forced here
/// by NAME: the moved file's `D{mine,f}` (held by the initiator) and the
/// destination's `D{shared,g}` (the served insert's) share a dentry
/// stripe. Pre-fix: the rename hangs for the wire's two call timeouts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_served_step_never_parks_behind_the_initiators_guards_even_on_a_stripe_collision() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let mine = routed
        .create(ROOT_INO, "mine", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    routed
        .create(mine, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let dlm = routed.volumes[0].dlm();
    let (_, local_mine) = routed.route_ino(mine);
    let (_, local_shared) = routed.route_ino(shared);
    let colliding = (0u64..)
        .map(|i| format!("g{i}"))
        .find(|n| dlm.dentry_stripe(local_shared, n) == dlm.dentry_stripe(local_mine, "f"))
        .expect("a colliding name exists below the stripe width");
    let before = cross_owner_stats();
    let renamed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        routed.rename(mine, "f", shared, &colliding, 0),
    )
    .await
    .expect("the rename completes — the served step waits on no guard the initiator holds");
    renamed.unwrap();
    assert_eq!(names_in(&routed, shared).await, vec![colliding.clone()]);
    assert!(names_in(&routed, mine).await.is_empty());
    let after = cross_owner_stats();
    assert!(
        after.steps_shipped > before.steps_shipped,
        "the insert shipped to the holder"
    );
    assert_closed("collision");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The wire half of the travelling guard: a REMOTE initiator's
/// `XvGuards` parks the named 4a guards at the holder under its scope
/// (a local acquirer of the same key waits), the steps it ships under
/// that scope apply without taking a guard, `XvRelease` frees them
/// (idempotent — `already` on a scope the holder no longer has), and a
/// scope whose initiator died expires with its lease (the grace window,
/// `xv_cross_owner_guard_expiries` — must-stay-0 on a healthy set).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_initiators_guards_park_at_the_holder_until_release_and_expire_with_its_lease() {
    use squeezefs::meta_backend::dlm::LockMode;
    use squeezefs::meta_ship::{MetaCall, MetaOp, MetaReply, PeerOwner};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let endpoint = holders.host.endpoint().to_string();
    // A SECOND node's router — not this process's installed shipper, so
    // its scopes are remote to the holder by identity.
    let remote = MetaShipRouter::new(Arc::clone(&routed), "node-c", SECRET.to_vec());
    let peer = Arc::new(PeerOwner::new("appender-1", &endpoint));
    let ship = |call: MetaCall| {
        let remote = Arc::clone(&remote);
        let peer = Arc::clone(&peer);
        async move {
            let op = MetaOp {
                id: remote.next_request_id(),
                call,
            };
            let mut r = remote.ship_ops(&peer, vec![op]).await.expect("shipped");
            r.pop().expect("one result").outcome
        }
    };
    let (_, local_shared) = routed.route_ino(shared);
    let dlm = routed.volumes[0].dlm();
    let before = cross_owner_stats();

    // Acquire under scope 77: the holder parks I{shared} + D{shared,z}.
    let acquired = ship(MetaCall::XvGuards {
        scope: 77,
        inodes: vec![(shared, true)],
        dentries: vec![(shared, "z".to_string(), true)],
    })
    .await;
    assert!(matches!(acquired, Ok(MetaReply::Unit)), "{acquired:?}");
    // A resend of the same acquisition is answered, never double-parked.
    let again = ship(MetaCall::XvGuards {
        scope: 77,
        inodes: vec![(shared, true)],
        dentries: vec![],
    })
    .await;
    assert!(matches!(again, Ok(MetaReply::Unit)), "{again:?}");
    let parked = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        dlm.lock_many(&[(local_shared, LockMode::Exclusive)], &[]),
    )
    .await;
    assert!(
        parked.is_err(),
        "a local acquirer waits behind the remote scope"
    );
    // A step under the scope applies WITHOUT taking a guard (it would
    // self-deadlock behind its own scope otherwise).
    let now = KvMetaBackend::now_ns_pub();
    let stepped = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ship(MetaCall::XvStep {
            tx_id: 0x77,
            step_idx: 0,
            step: crossvol_tx::XvStep::TouchCtime {
                ino: shared,
                ctime: now,
            },
            scope: 77,
        }),
    )
    .await
    .expect("the scoped step never parks");
    assert!(
        matches!(stepped, Ok(MetaReply::XvStep { status: 0, .. })),
        "{stepped:?}"
    );
    // Release; the local acquirer proceeds; a second release is `already`.
    // (A read of `shared` BEFORE the release would take its shared
    // `I{}` guard and wait behind the parked exclusive one — the lock
    // being a lock — so the step's effect is read after it.)
    let released = ship(MetaCall::XvRelease {
        scope: 77,
        ino: shared,
    })
    .await;
    assert!(matches!(released, Ok(MetaReply::Unit)), "{released:?}");
    // The applier stamps `max(step ctime, now)` — the step landed.
    assert!(routed.getattr(shared).await.unwrap().ctime >= now);
    let local = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        dlm.lock_many(&[(local_shared, LockMode::Exclusive)], &[]),
    )
    .await
    .expect("the release frees the stripe");
    drop(local);
    let released = ship(MetaCall::XvRelease {
        scope: 77,
        ino: shared,
    })
    .await;
    assert!(matches!(released, Ok(MetaReply::Unit)), "{released:?}");

    // A scope its initiator never releases (it died) expires with the
    // initiator's lease — the grace window, swept by the cadence.
    let acquired = ship(MetaCall::XvGuards {
        scope: 78,
        inodes: vec![(shared, true)],
        dentries: vec![],
    })
    .await;
    assert!(matches!(acquired, Ok(MetaReply::Unit)), "{acquired:?}");
    assert_eq!(
        crossvol_tx::sweep_expired_guards(),
        0,
        "inside the grace window"
    );
    TEST_XV_STUCK_AFTER_MS.store(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(crossvol_tx::sweep_expired_guards(), 1);
    TEST_XV_STUCK_AFTER_MS.store(0, Ordering::SeqCst);
    let local = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        dlm.lock_many(&[(local_shared, LockMode::Exclusive)], &[]),
    )
    .await
    .expect("the expiry frees the stripe");
    drop(local);
    let after = cross_owner_stats();
    assert_eq!(after.guard_expiries - before.guard_expiries, 1);
    assert_eq!(
        after.guards_parked - before.guards_parked,
        2,
        "scopes 77 and 78"
    );
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The initiator's half of the travelling guard, forced onto the wire
/// (`TEST_XV_GUARDS_FORCE_REMOTE` — an in-process holder shares the
/// initiator's table and is otherwise taken in the ONE canonical
/// `lock_many`): a create in a foreign directory acquires its foreign
/// keys as ONE `XvGuards` at the holder (`xv_cross_owner_guard_rpcs`),
/// ships its step under that scope, and releases at its terminal
/// outcome — nothing left parked, the stripe free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_initiator_acquires_its_foreign_guards_at_the_holder_and_releases_them_at_the_end() {
    use squeezefs::meta_backend::dlm::LockMode;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let before = cross_owner_stats();
    crossvol_tx::TEST_XV_GUARDS_FORCE_REMOTE.store(true, Ordering::SeqCst);
    let created = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        routed.create(shared, "remote", libc::S_IFREG | 0o644, 0, 0),
    )
    .await
    .expect("completes");
    crossvol_tx::TEST_XV_GUARDS_FORCE_REMOTE.store(false, Ordering::SeqCst);
    created.unwrap();
    let after = cross_owner_stats();
    assert_eq!(
        after.guard_rpcs - before.guard_rpcs,
        1,
        "one XvGuards per holder table"
    );
    assert_eq!(after.guards_parked - before.guards_parked, 1);
    assert_eq!(after.steps_shipped - before.steps_shipped, 1);
    // The release rode the guard set's drop: nothing stays parked.
    let (_, local_shared) = routed.route_ino(shared);
    let dlm = routed.volumes[0].dlm();
    let mut freed = false;
    for _ in 0..100 {
        if tokio::time::timeout(
            std::time::Duration::from_millis(50),
            dlm.lock_many(&[(local_shared, LockMode::Exclusive)], &[]),
        )
        .await
        .is_ok()
        {
            freed = true;
            break;
        }
    }
    assert!(
        freed,
        "the remote scope was released at the op's terminal outcome"
    );
    assert_eq!(crossvol_tx::sweep_expired_guards(), 0);
    assert_eq!(names_in(&routed, shared).await, vec!["remote".to_string()]);
    assert_closed("remote guards");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// The evidence note's instrument (dev box = SCOPING; `--ignored --nocapture`).
// ---------------------------------------------------------------------------

/// `.benchmarks/2026-09-15-sym-pr6-cross-owner.md`'s row: N creates into a
/// foreign directory (one shipped step each) and N directory renames under
/// the set-wide lock, then the Cross-owner phase table, the S8 client
/// phases and the lock wait — printed, never asserted (a scoping row on
/// the laptop; the box brackets are PR 13's).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the evidence note's scoping instrument — run with --ignored --nocapture"]
async fn scoping_row_per_verb_wire_and_barrier_cost() {
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let n = 200u32;
    let t = std::time::Instant::now();
    for i in 0..n {
        routed
            .create(shared, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    let create_wall = t.elapsed();
    let mine = routed
        .create(ROOT_INO, "mine", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    for i in 0..n {
        routed
            .create(mine, &format!("d{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap();
    }
    let t = std::time::Instant::now();
    for i in 0..n {
        routed
            .rename(mine, &format!("d{i}"), shared, &format!("e{i}"), 0)
            .await
            .unwrap();
    }
    let dir_rename_wall = t.elapsed();
    let json = serde_json::Value::Object(crossvol_tx::cross_owner_stats_json());
    println!(
        "SCOPING create-in-foreign-dir ×{n}: {:.1} µs/op; dir-rename across holders ×{n}: \
         {:.1} µs/op",
        create_wall.as_micros() as f64 / f64::from(n),
        dir_rename_wall.as_micros() as f64 / f64::from(n)
    );
    println!(
        "SCOPING xv_cross_owner_phase_ns = {}",
        serde_json::to_string_pretty(&json["xv_cross_owner_phase_ns"]).unwrap()
    );
    println!(
        "SCOPING dir_rename_lock_wait_ns = {}",
        serde_json::to_string_pretty(&json["dir_rename_lock_wait_ns"]).unwrap()
    );
    println!(
        "SCOPING meta_ship_phase_ns = {}",
        serde_json::to_string_pretty(&squeezefs::meta_ship::phase_json()).unwrap()
    );
    println!(
        "SCOPING meta_ship_owner_phase_ns = {}",
        serde_json::to_string_pretty(&squeezefs::meta_ship::owner_phase_json()).unwrap()
    );
    println!("SCOPING stats = {:?}", cross_owner_stats());
    holders.tear_down();
    shutdown(&routed).await;
}
