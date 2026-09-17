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

/// A stamped SET of `n` metadata volumes (one plan, one set uuid — the
/// per-volume stamps of `plan_meta_slot_set(n)`); the format config
/// rides volume 0.
async fn format_stamped_set(dir: &std::path::Path, n: usize) -> Vec<String> {
    let plan = plan_meta_slot_set(n).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let mut uris = Vec::with_capacity(n);
    for i in 0..n {
        let p = dir.join(format!("meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        let opts = FormatV3Options {
            format_config_xattr: (i == 0).then(|| format_config_for(dir)),
            ..set_opts(dir)
        };
        let r = format_v3_stamped(&p, VOL_LEN, &opts, plan.stamps[i].clone()).await;
        if r.is_err() {
            std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        }
        r.expect("format set member");
        uris.push(p.display().to_string());
    }
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    uris
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
    let tripwires_before = squeezefs::fuse_client::METRICS
        .invariant_tripwires
        .load(Ordering::Relaxed);

    let file = routed
        .create(shared, "out.bin", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create in a foreign directory is an ordinary op");
    let sub = routed
        .create(shared, "sub", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir in a foreign directory");
    // PR 12: the local `CreateInode` step is a FRESH mint — nothing names
    // the child until the shipped insert lands, the 4a law takes no
    // `I{child}` on it (the local create path holds none), so the
    // coverage belt has nothing to judge and must not fire (it fired on
    // every cross-owner create before, hidden until 7b read the gauge).
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .invariant_tripwires
            .load(Ordering::Relaxed),
        tripwires_before,
        "a cross-owner create trips no invariant tripwire (xv_local_step_unguarded on a \
         fresh mint is a predicate error, not a lock-law violation)"
    );

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

/// KD-SYM-14's pin (review round 1, Issue 5 — the first pin was hollow:
/// one identity took the lock `already`, and the two renames shared
/// `I{b}` so 4a serialized them). Here the two initiators are DISTINCT
/// lock identities (the second takes the lease under a wire joiner's id
/// through `TEST_DIR_RENAME_IDENTITY_ONCE`) and their 4a guard sets are
/// DISJOINT — no shared ino, no shared stripe, forced — so only the
/// set-wide lease stands between them and a cycle: `a/b → c/d/e` against
/// `root/c → a/b/x/y`. Exactly one completes; the loser waited on `Busy`
/// (`dir_rename_lock_wait_ns` grows) and refuses `EINVAL` under the
/// lease; every directory's chain still reaches the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_cannot_rename_directories_into_a_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B, SLOT_C]).await;
    let (a, c) = (dirs[0], dirs[1]);
    let routed = open_under(&uris, true, Some(THREE_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1, 2]).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let (mut joiner, other_identity) =
        wire_joiner(&holders.host.endpoint().to_string(), 5, 0).await;
    let dlm = vol.dlm();
    let local = |ino: u64| routed.route_ino(ino).1;
    // The two FIXED held dentry keys — r1's `D{a,b}` and r2's
    // `D{root,shared1}` — must not share a stripe either (review round 2,
    // Issue 25a): `b`'s name is chosen off `shared1`'s stripe.
    let b_name = (0u64..)
        .map(|i| format!("b{i}"))
        .find(|n| dlm.dentry_stripe(local(a), n) != dlm.dentry_stripe(local(ROOT_INO), "shared1"))
        .unwrap();
    let b = routed
        .create(a, &b_name, libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    // r1 = rename(a, "b", d, "e"): guards I{a} I{d} I{b} D{a,b} D{d,e}.
    // r2 = rename(root, "shared1", x, "y"): guards I{root} I{x} I{c}
    // D{root,shared1} D{x,y}. Mint `x` and `d` until every inode stripe of
    // one set avoids the other's, then pick "e"/"y" off both sets' dentry
    // stripes.
    let fixed_i: Vec<usize> = [a, b, ROOT_INO, c]
        .iter()
        .map(|i| dlm.inode_stripe(local(*i)))
        .collect();
    let (mut x, mut d) = (0u64, 0u64);
    for k in 0.. {
        x = routed
            .create(b, &format!("x{k}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        d = routed
            .create(c, &format!("d{k}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        let r1_i = [
            dlm.inode_stripe(local(a)),
            dlm.inode_stripe(local(d)),
            dlm.inode_stripe(local(b)),
        ];
        let r2_i = [
            dlm.inode_stripe(local(ROOT_INO)),
            dlm.inode_stripe(local(x)),
            dlm.inode_stripe(local(c)),
        ];
        if r1_i.iter().all(|s| !r2_i.contains(s)) {
            break;
        }
        assert!(
            k < 64,
            "could not mint disjoint inode stripes in 64 tries: {fixed_i:?}"
        );
    }
    let (x_name, d_name) = (
        names_in(&routed, b).await.pop().unwrap(),
        names_in(&routed, c)
            .await
            .into_iter()
            .find(|n| n.starts_with('d'))
            .unwrap(),
    );
    let taken: Vec<usize> = vec![
        dlm.dentry_stripe(local(a), &b_name),
        dlm.dentry_stripe(local(ROOT_INO), "shared1"),
    ];
    assert_ne!(
        taken[0], taken[1],
        "the fixed pair is disjoint by construction"
    );
    let e_name = (0u64..)
        .map(|i| format!("e{i}"))
        .find(|n| !taken.contains(&dlm.dentry_stripe(local(d), n)))
        .unwrap();
    let y_name = (0u64..)
        .map(|i| format!("y{i}"))
        .find(|n| {
            let st = dlm.dentry_stripe(local(x), n);
            !taken.contains(&st) && st != dlm.dentry_stripe(local(d), &e_name)
        })
        .unwrap();
    let _ = (x_name, d_name);
    let before = cross_owner_stats();
    // r2 runs under the OTHER identity and holds the lease across its
    // shipped steps (held 300 ms each at the holder) — long enough for r1
    // to arrive and park on `Busy`.
    crossvol_tx::TEST_XV_SERVE_HOLD_MS.store(300, Ordering::SeqCst);
    crossvol_tx::TEST_DIR_RENAME_IDENTITY_ONCE.store(other_identity, Ordering::SeqCst);
    let r2 = {
        let routed = Arc::clone(&routed);
        let y = y_name.clone();
        tokio::spawn(async move { routed.rename(ROOT_INO, "shared1", x, &y, 0).await })
    };
    let mut held = false;
    for _ in 0..200 {
        if vol.dir_rename_record().await.unwrap().map(|r| r.holder) == Some(other_identity) {
            held = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(held, "r2 took the set-wide lease under the other identity");
    let t_r1 = std::time::Instant::now();
    let r1 = {
        let routed = Arc::clone(&routed);
        let e = e_name.clone();
        let b_name = b_name.clone();
        tokio::spawn(async move { routed.rename(a, &b_name, d, &e, 0).await })
    };
    let (r1, r2) = (r1.await.unwrap(), r2.await.unwrap());
    crossvol_tx::TEST_XV_SERVE_HOLD_MS.store(0, Ordering::SeqCst);
    assert!(r2.is_ok(), "the lease holder completes: {r2:?}");
    let refused = r1.expect_err("the second initiator refuses under the lease");
    assert_eq!(
        refused.to_errno(),
        libc::EINVAL,
        "the loser refuses EINVAL — the target lies inside the source: {refused}"
    );
    let after = cross_owner_stats();
    assert!(
        after.dir_rename_lock_wait_ns_sum - before.dir_rename_lock_wait_ns_sum >= 200_000_000,
        "the loser WAITED on the contended lease (r1 wall {:?})",
        t_r1.elapsed()
    );
    assert_eq!(
        after.dir_rename_lock_acquires - before.dir_rename_lock_acquires,
        2,
        "both directory renames took the set-wide lock"
    );
    // No cycle: every directory's parent chain reaches the root.
    for d0 in [a, b, c, d, x] {
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
    assert!(vol.dir_rename_record().await.unwrap().is_none(), "released");
    assert!(matches!(
        joiner.dir_rename_unlock(other_identity).await.unwrap(),
        ManagerReply::DirRenameUnlocked { already: true }
    ));
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

/// A wire joiner on the manager's listener: joined through
/// `JoinAppender` (PR 3/4's fixture), so its appender id names a `Live`
/// directory page — the identity every slot verb, and since review
/// round 1 the two lock verbs, is screened against.
fn joiner_identity(n: u64) -> squeezefs::meta_backend::kv::appender::AppenderIdentity {
    squeezefs::meta_backend::kv::appender::AppenderIdentity {
        node_token: 0x5EED_0000_0000_0000 | n,
        mount_slot: 0x1000 + n as u32,
        writer_id: 0xABCD_0000 + u128::from(n),
    }
}

async fn wire_joiner(endpoint: &str, n: u64, volume: u16) -> (ManagerClient, u32) {
    let mut client = ManagerClient::connect(endpoint, SECRET, &format!("joiner-{n}"), volume)
        .await
        .expect("storage-trust enrollment");
    let reply = client.join(joiner_identity(n), 0).await.unwrap();
    let ManagerReply::Joined { appender_id, .. } = reply else {
        panic!("{reply:?}");
    };
    (client, appender_id)
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
    let (mut a, id_a) = wire_joiner(&endpoint, 7, 0).await;
    let (mut b, id_b) = wire_joiner(&endpoint, 9, 0).await;
    assert!(vol.dir_rename_record().await.unwrap().is_none());
    match a.dir_rename_lock(id_a).await.unwrap() {
        ManagerReply::DirRenameLocked { already } => assert!(!already),
        other => panic!("{other:?}"),
    }
    let rec = vol.dir_rename_record().await.unwrap().expect("durable");
    assert_eq!(rec.holder, id_a);
    assert!(
        rec.since_ns > 1_600_000_000 * 1_000_000_000,
        "a realtime stamp"
    );
    match a.dir_rename_lock(id_a).await.unwrap() {
        ManagerReply::DirRenameLocked { already } => assert!(already, "KD-SYM-7"),
        other => panic!("{other:?}"),
    }
    match b.dir_rename_lock(id_b).await.unwrap() {
        ManagerReply::DirRenameBusy { holder } => assert_eq!(holder, id_a),
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
    vol.manager_dir_rename_release_dead(id_a).await.unwrap();
    let lock = waiter.await.unwrap().expect("granted after the release");
    assert_eq!(vol.dir_rename_record().await.unwrap().unwrap().holder, 0);
    lock.release().await.unwrap();
    assert!(vol.dir_rename_record().await.unwrap().is_none());
    match a.dir_rename_unlock(id_a).await.unwrap() {
        ManagerReply::DirRenameUnlocked { already } => assert!(already),
        other => panic!("{other:?}"),
    }
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    assert_eq!(vol.appender_stats().unwrap().manager_verb_rejected, 0);
    host.shutdown();
    shutdown(&routed).await;
}

/// Two directory renames of ONE identity are SERIALIZED in-process
/// (review round 2, Issue 23): the lease serializes ops, not identities
/// — the second take waits for the first's release rather than joining
/// its record, so the law never leans on the kernel's
/// `s_vfs_rename_mutex` (an S8-served `Rename`, the offline callers and
/// the contracts reach `RoutedMetaBackend::rename` without it). Pinned
/// with a directory rename held inside the lease (its shipped step
/// parked at the holder) while a second same-identity directory rename
/// is issued: the second completes only after the first releases; the
/// record is held throughout and free at the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_directory_renames_of_one_identity_serialize_under_the_lease() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let mine = routed
        .create(ROOT_INO, "mine", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let other = routed
        .create(ROOT_INO, "other", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    routed
        .create(mine, "p", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    routed
        .create(other, "q", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let before = cross_owner_stats();
    // r1 holds the lease across its shipped steps (400 ms each at the
    // holder); r2 — same identity, a SAME-SLOT directory rename under a
    // DISJOINT parent (no 4a key in common), shipping nothing — must not
    // run beside it.
    crossvol_tx::TEST_XV_SERVE_HOLD_MS.store(400, Ordering::SeqCst);
    let r1 = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move { routed.rename(mine, "p", shared, "p2", 0).await })
    };
    let mut held = false;
    for _ in 0..200 {
        if vol.dir_rename_record().await.unwrap().is_some() {
            held = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(held, "r1 holds the lease");
    let t = std::time::Instant::now();
    let r2 = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move { routed.rename(other, "q", other, "q2", 0).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        !r2.is_finished(),
        "r2 waits for r1's release — never a join"
    );
    r1.await.unwrap().unwrap();
    r2.await.unwrap().unwrap();
    crossvol_tx::TEST_XV_SERVE_HOLD_MS.store(0, Ordering::SeqCst);
    assert!(
        t.elapsed() >= std::time::Duration::from_millis(300),
        "r2 completed only after r1's shipped steps: {:?}",
        t.elapsed()
    );
    assert!(vol.dir_rename_record().await.unwrap().is_none(), "released");
    let after = cross_owner_stats();
    assert_eq!(
        after.dir_rename_lock_acquires - before.dir_rename_lock_acquires,
        2
    );
    assert!(
        after.dir_rename_lock_wait_ns_sum - before.dir_rename_lock_wait_ns_sum >= 200_000_000,
        "the second take WAITED"
    );
    assert_eq!(names_in(&routed, other).await, vec!["q2".to_string()]);
    assert_eq!(names_in(&routed, shared).await, vec!["p2".to_string()]);
    assert!(names_in(&routed, mine).await.is_empty());
    assert_closed("same-identity serialization");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The lock verbs' wire words are SCREENED (review round 1, Issue 3 —
/// PR 4 round 6's law for every slot verb): the id must name a `Live`
/// directory page and never one of this mount's own regions; an unlock
/// naming an id that is not the holder while another holds the lock is a
/// foreign release — `Rejected` + `manager_verb_rejected`, nothing
/// written; the verbs are volume 0's manager's alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lock_verbs_reject_a_foreign_unlock_a_dead_id_and_any_volume_but_zero() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = format_stamped_set(dir.path(), 2).await;
    let routed = open_under(&uris, true, None).await;
    let vol0 = Arc::clone(&routed.volumes[0]);
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        Arc::new(AsyncVerbRouter::new().with_manager(ManagerSetService::new(&routed.volumes))),
    )
    .unwrap();
    let endpoint = host.endpoint().to_string();
    let (mut a, id_a) = wire_joiner(&endpoint, 7, 0).await;
    let (mut b, id_b) = wire_joiner(&endpoint, 9, 0).await;
    let rejected_before = vol0.appender_stats().unwrap().manager_verb_rejected;
    match a.dir_rename_lock(id_a).await.unwrap() {
        ManagerReply::DirRenameLocked { already } => assert!(!already),
        other => panic!("{other:?}"),
    }
    // The client surfaces `STATUS_REJECTED` as `Err` (the slot verbs'
    // convention); the record is what says nothing was written.
    let rejected = |r: squeezefs::error::Result<ManagerReply>| {
        matches!(r, Err(squeezefs::error::SqueezefsError::InvalidOperation(ref m))
            if m.contains("manager_verb_rejected"))
    };
    // A foreign unlock: B releases A's lock — rejected, the record stays.
    assert!(
        rejected(b.dir_rename_unlock(id_b).await),
        "an unlock by a non-holder while another holds is a foreign release"
    );
    assert_eq!(
        vol0.dir_rename_record().await.unwrap().unwrap().holder,
        id_a
    );
    // A lock under an id with no Live page — rejected, nothing parked.
    assert!(rejected(b.dir_rename_lock(4242).await));
    // A lock naming the manager's OWN region from the wire — rejected.
    assert!(rejected(b.dir_rename_lock(vol0.own_appender_id()).await));
    match a.dir_rename_unlock(id_a).await.unwrap() {
        ManagerReply::DirRenameUnlocked { already } => assert!(!already),
        other => panic!("{other:?}"),
    }
    assert!(vol0.dir_rename_record().await.unwrap().is_none());
    // The verbs on volume 1: rejected — the set-wide lock is volume 0's.
    let mut on_vol1 = ManagerClient::connect(&endpoint, SECRET, "joiner-7", 1)
        .await
        .unwrap();
    assert!(rejected(on_vol1.dir_rename_lock(id_a).await));
    assert!(rejected(on_vol1.dir_rename_unlock(id_a).await));
    assert!(routed.volumes[1]
        .dir_rename_record()
        .await
        .unwrap()
        .is_none());
    let rejected = vol0.appender_stats().unwrap().manager_verb_rejected - rejected_before
        + routed.volumes[1]
            .appender_stats()
            .unwrap()
            .manager_verb_rejected;
    assert_eq!(
        rejected, 5,
        "every screen refusal counted on manager_verb_rejected"
    );
    assert_eq!(vol0.appender_stats().unwrap().manager_verb_refusals, 0);
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
        "xv_cross_owner_steps_rejected",
        "xv_cross_owner_intents_stuck",
        "xv_cross_owner_phase_ns",
        "xv_cross_owner_guard_rpcs",
        "xv_cross_owner_guards_parked",
        "xv_cross_owner_guard_expiries",
        "dir_rename_lock_acquires",
        "dir_rename_lock_wait_ns",
        "dir_rename_parent_scans",
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
        "local_steps",
        "retire",
        "total",
    ] {
        assert!(phases.contains_key(p), "missing phase {p}");
    }
}

// ---------------------------------------------------------------------------
// The roll-forward cadence beside LIVE ops (review round 1, Issue 4).
// ---------------------------------------------------------------------------

/// The cadence never adopts a LIVE initiator's intent — its own op is
/// its own, however long it runs (a step parked at a busy holder, a wire
/// timeout, a D1.b stall): adoption is keyed on the register's
/// in-flight / abandoned state, never on elapsed time or a tick count.
/// A `link` whose shipped insert is HELD at the holder spans several
/// cadence passes; none re-applies it, and after the op completes and
/// the user unlinks the name, a further pass resurrects nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_op_spanning_cadence_passes_is_never_replayed_by_the_cadence() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let f = routed
        .create(ROOT_INO, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let before = cross_owner_stats();
    crossvol_tx::TEST_XV_SERVE_HOLD_MS.store(600, Ordering::SeqCst);
    let op = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move { routed.link(f, shared, "l").await })
    };
    // Several cadence passes while the op is parked at the holder.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut passes = 0;
    while !op.is_finished() && passes < 20 {
        assert_eq!(
            crossvol_tx::roll_forward_open_intents(&routed)
                .await
                .unwrap(),
            0,
            "a live op's intent is its own — never adopted"
        );
        assert_eq!(cross_owner_stats().intents_open, 1, "registered in flight");
        passes += 1;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(passes >= 2, "the op spanned at least two passes ({passes})");
    crossvol_tx::TEST_XV_SERVE_HOLD_MS.store(0, Ordering::SeqCst);
    op.await.unwrap().unwrap();
    assert_eq!(routed.getattr(f).await.unwrap().nlink, 2);
    // The user removes the link; a later pass must resurrect nothing.
    routed.unlink(shared, "l").await.unwrap();
    assert_eq!(
        crossvol_tx::roll_forward_open_intents(&routed)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        routed.getattr(f).await.unwrap().nlink,
        1,
        "no resurrected count"
    );
    assert!(
        names_in(&routed, shared).await.is_empty(),
        "no resurrected name"
    );
    let after = cross_owner_stats();
    assert_eq!(after.intents_open, 0);
    assert_eq!(
        after.intents_minted - before.intents_minted,
        2,
        "the link and the unlink"
    );
    assert_closed("live op vs cadence");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// A cadence pass that SCANNED an intent another pass then retired
/// re-reads the record under the guards it acquires and finds it gone —
/// a no-op, even when the objects have moved since in a way the stale
/// plan's witnesses would accept (the unlinked name is free again, the
/// count is back at `pre`): nothing is re-applied, no RAM ghost survives
/// (`intents_open` 0, `intents_stuck` 0 past the grace window).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retired_intent_met_by_a_stale_scan_is_a_no_op_and_leaves_no_ghost() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let f = routed
        .create(ROOT_INO, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    // An ABANDONED intent: the holder is down for the op, which errors.
    TEST_XV_SERVE_REFUSE.store(true, Ordering::SeqCst);
    assert!(routed.link(f, shared, "l").await.is_err());
    TEST_XV_SERVE_REFUSE.store(false, Ordering::SeqCst);
    assert_eq!(cross_owner_stats().intents_open, 1);
    // Pass A scans, then parks; pass B completes and retires the intent;
    // the user unlinks the name; pass A resumes on its stale scan.
    crossvol_tx::TEST_XV_CADENCE_HOLD_AFTER_SCAN_MS.store(400, Ordering::SeqCst);
    let pass_a = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move { crossvol_tx::roll_forward_open_intents(&routed).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    crossvol_tx::TEST_XV_CADENCE_HOLD_AFTER_SCAN_MS.store(0, Ordering::SeqCst);
    assert_eq!(
        crossvol_tx::roll_forward_open_intents(&routed)
            .await
            .unwrap(),
        1
    );
    assert_eq!(routed.getattr(f).await.unwrap().nlink, 2);
    routed.unlink(shared, "l").await.unwrap();
    assert_eq!(routed.getattr(f).await.unwrap().nlink, 1);
    assert_eq!(
        pass_a.await.unwrap().unwrap(),
        0,
        "the stale scan applies nothing"
    );
    assert_eq!(
        routed.getattr(f).await.unwrap().nlink,
        1,
        "no resurrected count"
    );
    assert!(
        names_in(&routed, shared).await.is_empty(),
        "no resurrected name"
    );
    let s = cross_owner_stats();
    assert_eq!(s.intents_open, 0, "no RAM ghost");
    TEST_XV_STUCK_AFTER_MS.store(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(
        cross_owner_stats().intents_stuck,
        0,
        "nothing stuck behind a ghost"
    );
    TEST_XV_STUCK_AFTER_MS.store(0, Ordering::SeqCst);
    assert_closed("stale scan");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
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

/// The ancestor check reads WITHOUT 4a guards (review round 1, Issue 2):
/// its links are confirmed under the set-wide `dir_rename` lease — the
/// lease is what makes the chain consistent — never under a `D{}` guard,
/// because the walk runs while the initiator HOLDS its own exclusive
/// `D{}` guards and a stripe collision would park the initiator behind
/// itself, holding the lease, wedging every directory rename in the set.
/// The collision is forced by NAME: the moved directory's `D{mine,sub}`
/// (held exclusive by the initiator) shares a dentry stripe with the
/// target's ancestor link `D{root,shared0}` the walk confirms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_rename_whose_ancestor_link_collides_with_its_own_guard_stripe_completes() {
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
    let dlm = routed.volumes[0].dlm();
    let (_, local_mine) = routed.route_ino(mine);
    let (_, local_root) = routed.route_ino(ROOT_INO);
    let sub = (0u64..)
        .map(|i| format!("sub{i}"))
        .find(|n| dlm.dentry_stripe(local_mine, n) == dlm.dentry_stripe(local_root, "shared0"))
        .expect("a colliding name exists below the stripe width");
    routed
        .create(mine, &sub, libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let before = cross_owner_stats();
    let renamed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        routed.rename(mine, &sub, shared, "moved", 0),
    )
    .await
    .expect("the ancestor walk takes no guard the initiator holds — the rename completes");
    renamed.unwrap();
    assert_eq!(names_in(&routed, shared).await, vec!["moved".to_string()]);
    let after = cross_owner_stats();
    assert_eq!(
        after.dir_rename_lock_acquires - before.dir_rename_lock_acquires,
        1
    );
    assert!(
        after.dir_rename_lock_wait_ns_sum - before.dir_rename_lock_wait_ns_sum < 5_000_000_000,
        "the lock wait is bounded (uncontended: one control entry + barrier)"
    );
    assert_closed("ancestor collision");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The scan ADOPTS only intents homed in slots THIS mount's step-home is
/// `Local` for (review round 2, Issue 21): an intent in a slot another
/// appender leases is that appender's (its re-read here would be a
/// projection, and its retirement by the peer invisible) — skipped by
/// the cadence and the mount's recovery, never registered. Planted here
/// as a raw record in the declared region's slot home; the raw scan sees
/// it, the roll-forward does not adopt it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_intent_homed_in_a_slot_another_appender_leases_is_never_adopted_here() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let before = cross_owner_stats();
    // A well-formed intent (a TouchCtime of `shared`) planted in slot B's
    // intent home — appender 1's slot.
    let tx_id = 0x0000_00ab_cdef_0123u64;
    let rec = crossvol_tx::IntentRecord {
        tx_id,
        op: crossvol_tx::XvOp::Rename,
        steps: vec![crossvol_tx::XvStep::TouchCtime {
            ino: shared,
            ctime: KvMetaBackend::now_ns_pub(),
        }],
    };
    let home = crossvol_tx::intent_ino_for_slot(SLOT_B);
    vol.xv_write_intent(
        &crossvol_tx::XvRider::Put {
            intent_ino: home,
            tx_id,
            image: rec.encode().unwrap(),
        },
        Arc::from(Vec::new()),
    )
    .await
    .unwrap();
    assert_eq!(
        vol.xv_scan_intents_homed().await.unwrap().len(),
        1,
        "the raw scan sees it"
    );
    assert_eq!(
        crossvol_tx::roll_forward_open_intents(&routed)
            .await
            .unwrap(),
        0,
        "not this mount's to roll forward"
    );
    let after = cross_owner_stats();
    assert_eq!(after.intents_minted, before.intents_minted, "never adopted");
    assert_eq!(after.intents_open, 0);
    assert_eq!(
        vol.xv_scan_intents_homed().await.unwrap().len(),
        1,
        "the record stays for its lessee"
    );
    // Clean up as the lessee would: retire it directly.
    vol.xv_retire_intent_at(home, tx_id, Arc::from(Vec::new()))
        .await
        .unwrap();
    assert_eq!(open_intents(&routed).await, 0);
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The wire half of the travelling guard: a REMOTE initiator's
/// `XvGuards` parks the named 4a guards at the holder under its scope
/// (a local acquirer of the same key waits), the steps it ships under
/// that scope apply without taking a guard, `XvRelease` frees them
/// (idempotent — `already` on a scope the holder no longer has), and a
/// scope whose initiator is gone expires: with NO membership plane armed
/// at the grace-window BELT (this leg — `xv_cross_owner_guard_expiries`,
/// must-stay-0 on a healthy set); with one armed, by its LEASE (the next
/// contract).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_initiators_guards_park_at_the_holder_until_release_and_expire_at_the_grace_belt()
{
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

/// With the S6 membership authority ARMED in this process, a parked
/// scope expires with its initiator's LEASE, never with the clock
/// (review round 1, Issue 9): a live member's scope survives the grace
/// window; the sweep after the owner EVICTS the member releases it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_scope_expires_with_its_initiators_membership_lease_when_a_plane_is_armed() {
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    use squeezefs::meta_backend::dlm::LockMode;
    use squeezefs::meta_ship::{MetaCall, MetaOp, MetaReply, PeerOwner};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let endpoint = holders.host.endpoint().to_string();
    // The plane: this process is the lease authority; "node-c" is a live
    // member of it.
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ticks));
    let clocks = LeaseClocks::derive(std::time::Duration::from_micros(250)).unwrap();
    let owner = MembershipOwner::arm("lease-owner", 3, 2, clocks, clock).unwrap();
    membership::install_owner(Arc::clone(&owner));
    let joined = owner.join(JoinRequest {
        id: "node-c".to_string(),
        role: MemberRole::Writer,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-sym-test".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    });
    assert!(matches!(joined, JoinOutcome::Granted(_)), "{joined:?}");
    let remote = MetaShipRouter::new(Arc::clone(&routed), "node-c", SECRET.to_vec());
    let peer = Arc::new(PeerOwner::new("appender-1", &endpoint));
    let (_, local_shared) = routed.route_ino(shared);
    let dlm = routed.volumes[0].dlm();
    let before = cross_owner_stats();
    let op = MetaOp {
        id: remote.next_request_id(),
        call: MetaCall::XvGuards {
            scope: 91,
            inodes: vec![(shared, true)],
            dentries: vec![],
        },
    };
    let mut r = remote.ship_ops(&peer, vec![op]).await.unwrap();
    assert!(matches!(r.pop().unwrap().outcome, Ok(MetaReply::Unit)));
    // Past the grace window with the member LIVE: nothing expires.
    TEST_XV_STUCK_AFTER_MS.store(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(
        crossvol_tx::sweep_expired_guards(),
        0,
        "a live member's scope survives the clock"
    );
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(200),
        dlm.lock_many(&[(local_shared, LockMode::Exclusive)], &[]),
    )
    .await
    .is_err());
    // The owner evicts the member: the next sweep releases its scope.
    assert!(owner.evict("node-c", "the contract's death").is_some());
    assert_eq!(crossvol_tx::sweep_expired_guards(), 1);
    TEST_XV_STUCK_AFTER_MS.store(0, Ordering::SeqCst);
    let local = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        dlm.lock_many(&[(local_shared, LockMode::Exclusive)], &[]),
    )
    .await
    .expect("the lease's death frees the stripe");
    drop(local);
    // A client this owner does NOT know (review round 2, Issue 22 — a
    // cross-shard initiator, a joiner the ladder has not bound yet): its
    // scope keeps the grace-window BELT, never an immediate release.
    let unknown = MetaShipRouter::new(Arc::clone(&routed), "node-d", SECRET.to_vec());
    let op = MetaOp {
        id: unknown.next_request_id(),
        call: MetaCall::XvGuards {
            scope: 92,
            inodes: vec![(shared, true)],
            dentries: vec![],
        },
    };
    let mut r = unknown.ship_ops(&peer, vec![op]).await.unwrap();
    assert!(matches!(r.pop().unwrap().outcome, Ok(MetaReply::Unit)));
    assert_eq!(
        crossvol_tx::sweep_expired_guards(),
        0,
        "an unknown client's scope survives a sweep inside the grace window"
    );
    TEST_XV_STUCK_AFTER_MS.store(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(
        crossvol_tx::sweep_expired_guards(),
        1,
        "…and expires at the belt"
    );
    TEST_XV_STUCK_AFTER_MS.store(0, Ordering::SeqCst);
    membership::uninstall();
    let after = cross_owner_stats();
    assert_eq!(after.guard_expiries - before.guard_expiries, 2);
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

/// Every cross-owner verb pays ONE guard round trip per foreign holder
/// table (review round 1, Issue 13 — the design's RPC count): under
/// `TEST_XV_GUARDS_FORCE_REMOTE` a create, an unlink (whose discovery
/// phase acquires no foreign table), a link and a rename into a foreign
/// directory each ship exactly one `XvGuards`; a rename across TWO
/// foreign holders ships two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_cross_owner_verb_pays_one_guard_round_trip_per_foreign_holder_table() {
    // The tape a load-selected wire stall needs (PR 5 review round 3,
    // Issue 26): `RUST_LOG=squeezefs=debug` names the served side's park.
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B, SLOT_C]).await;
    let (shared, other) = (dirs[0], dirs[1]);
    let routed = open_under(&uris, true, Some(THREE_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1, 2]).await;
    let mine = routed
        .create(ROOT_INO, "mine", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let f = routed
        .create(mine, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    crossvol_tx::TEST_XV_GUARDS_FORCE_REMOTE.store(true, Ordering::SeqCst);
    let rpcs = || cross_owner_stats().guard_rpcs;
    let mut expect = rpcs();
    for (what, delta, op) in [
        ("create", 1, 0u8),
        ("unlink", 1, 1),
        ("link", 1, 2),
        ("rename into", 1, 3),
        ("rename across two holders", 2, 4),
    ] {
        match op {
            0 => {
                routed
                    .create(shared, "c", libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap();
            }
            1 => {
                routed.unlink(shared, "c").await.unwrap();
            }
            2 => {
                routed.link(f, shared, "l").await.unwrap();
            }
            3 => {
                routed.rename(mine, "f", shared, "g", 0).await.unwrap();
            }
            _ => {
                routed.rename(shared, "g", other, "h", 0).await.unwrap();
            }
        }
        expect += delta;
        assert_eq!(
            rpcs(),
            expect,
            "{what}: one XvGuards per foreign holder table"
        );
    }
    crossvol_tx::TEST_XV_GUARDS_FORCE_REMOTE.store(false, Ordering::SeqCst);
    assert_eq!(names_in(&routed, other).await, vec!["h".to_string()]);
    assert_eq!(names_in(&routed, shared).await, vec!["l".to_string()]);
    assert_closed("rpc counts");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// **A parked `XvGuards` never blocks the release that unparks it** (PR
/// 5 review round 3, Issue 26 — the attribution of the load-selected `no
/// reply to call 1 within 10s` of the contract above; both recorded reds
/// ran 33.9 s = the suite's 13.5 s + TWO 10 s bounds). The S8 lane to one
/// owner is stop-and-wait and the owner runs a frame's ops serially;
/// `XvGuards` PARKS at the holder by design (on a stripe the previous
/// scope still holds) and the previous scope's `XvRelease` was a
/// fire-and-forget task on that SAME lane — so when the release lost the
/// race for the lane to the next op's guards, the holder's serve parked
/// on guards whose release sat BEHIND it: head-of-line until the call
/// bound (10 s), then the resend parked behind the dedup winner (10 s
/// more). The schedule made deterministic: the create's release is held
/// 50 ms (`TEST_XV_RELEASE_HOLD_MS`) so the unlink's guards on the same
/// `D{shared:"c"}` reach the holder first; the unlink must complete in
/// the release's hold plus a round trip, never the wire's bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_xv_guards_never_blocks_the_release_that_unparks_it() {
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    crossvol_tx::TEST_XV_GUARDS_FORCE_REMOTE.store(true, Ordering::SeqCst);
    crossvol_tx::TEST_XV_RELEASE_HOLD_MS.store(50, Ordering::SeqCst);
    let before = cross_owner_stats();
    let rounds = 8u32;
    let bound = std::time::Duration::from_secs(3);
    for i in 0..rounds {
        routed
            .create(shared, "c", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        // The create's scope is still parked at the holder (its release
        // is held); the unlink's guards on the same dentry key park
        // behind it — and must be unparked by that release, not by the
        // wire's bound.
        let t0 = std::time::Instant::now();
        tokio::time::timeout(bound, routed.unlink(shared, "c"))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "round {i}: the unlink did not complete within {bound:?} — the create's \
                     release queued behind the unlink's parked guards"
                )
            })
            .unwrap();
        assert!(
            t0.elapsed() < bound,
            "round {i}: {:?} — a guard round trip waited the wire bound",
            t0.elapsed()
        );
    }
    crossvol_tx::TEST_XV_RELEASE_HOLD_MS.store(0, Ordering::SeqCst);
    crossvol_tx::TEST_XV_GUARDS_FORCE_REMOTE.store(false, Ordering::SeqCst);
    let after = cross_owner_stats();
    assert_eq!(
        after.guard_rpcs - before.guard_rpcs,
        u64::from(rounds) * 2,
        "one XvGuards per op"
    );
    assert_eq!(
        after.guard_expiries, before.guard_expiries,
        "no scope expired"
    );
    assert!(names_in(&routed, shared).await.is_empty());
    // The held releases LAND (observed: the holder's parked-scope gauge
    // reaches 0) before the teardown — never a sleep for synchronization.
    let started = std::time::Instant::now();
    while crossvol_tx::parked_scopes() != 0 {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the held releases never landed ({} scope(s) still parked)",
            crossvol_tx::parked_scopes()
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_closed("release ordering");
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// `Covered` requires COVERAGE (review round 1, Issue 8b; pinned in
/// round 2, Issue 25b): a scope that parked guards on one key does not
/// let a step needing another apply unguarded — the step falls to `Take`
/// and PARKS behind a local exclusive holder of its key until the holder
/// releases (under a bare `Covered` it would apply at once beside it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_step_outside_its_scopes_parked_keys_takes_its_own_guards_and_parks() {
    use squeezefs::meta_backend::dlm::LockMode;
    use squeezefs::meta_ship::{MetaCall, MetaOp, MetaReply, PeerOwner};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let endpoint = holders.host.endpoint().to_string();
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
    // The scope parks ONLY `D{shared, "other"}` — not `I{shared}`.
    let parked = ship(MetaCall::XvGuards {
        scope: 81,
        inodes: vec![],
        dentries: vec![(shared, "other".to_string(), true)],
    })
    .await;
    assert!(matches!(parked, Ok(MetaReply::Unit)), "{parked:?}");
    // A local exclusive holder of I{shared}.
    let held = dlm
        .lock_many(&[(local_shared, LockMode::Exclusive)], &[])
        .await;
    // A TouchCtime(shared) under scope 81 needs I{shared}: not covered ⇒
    // `Take` ⇒ parks behind the holder.
    let now = KvMetaBackend::now_ns_pub();
    let step = tokio::spawn(ship(MetaCall::XvStep {
        tx_id: 0x81,
        step_idx: 0,
        step: crossvol_tx::XvStep::TouchCtime {
            ino: shared,
            ctime: now,
        },
        scope: 81,
    }));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !step.is_finished(),
        "the step PARKED behind the local holder — never Covered"
    );
    drop(held);
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), step)
        .await
        .expect("released")
        .unwrap();
    assert!(
        matches!(out, Ok(MetaReply::XvStep { status: 0, .. })),
        "{out:?}"
    );
    let released = ship(MetaCall::XvRelease {
        scope: 81,
        ino: shared,
    })
    .await;
    assert!(matches!(released, Ok(MetaReply::Unit)));
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The LOCAL applier's belt (review round 1, Issue 8c; pinned in round
/// 2, Issue 25b): a local step whose keys the op's guard scope does not
/// cover trips `invariant_tripwires` (`xv_local_step_unguarded`) — loud,
/// never silent. Built from the public faces: a scope acquired over ino
/// A's key, a plan whose step names ino B under those guards; the
/// covered shape trips nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_step_outside_its_scopes_keys_trips_the_invariant_tripwire() {
    use squeezefs::meta_backend::dlm::LockMode;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_member(dir.path(), "meta0", true).await];
    let routed = open_under(&uris, true, None).await;
    let a = routed
        .create(ROOT_INO, "a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let b = routed
        .create(ROOT_INO, "b", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let tripwires = || {
        squeezefs::fuse_client::METRICS
            .invariant_tripwires
            .load(Ordering::Relaxed)
    };
    let plan = crossvol_tx::XvPlan {
        op: crossvol_tx::XvOp::Rename,
        steps: vec![crossvol_tx::XvStep::TouchCtime {
            ino: b,
            ctime: KvMetaBackend::now_ns_pub(),
        }],
    };
    for (guarded, expect) in [(a, 1u64), (b, 0)] {
        let (_, local) = routed.route_ino(guarded);
        let mut scope = None;
        let guards: Arc<[squeezefs::meta_backend::dlm::DlmGuard]> = Arc::from(
            crossvol_tx::acquire_guards_leased(
                &routed,
                0,
                &mut scope,
                &[(local, LockMode::Exclusive)],
                &[],
                false,
            )
            .await
            .unwrap(),
        );
        assert!(scope.is_some(), "an armed acquisition mints the scope");
        let before = tripwires();
        crossvol_tx::execute(&routed, &plan, guards).await.unwrap();
        assert_eq!(
            tripwires() - before,
            expect,
            "guarding {guarded}: an uncovered local step trips the belt, a covered one does not"
        );
    }
    assert_closed("local tripwire");
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The served insert's `child` is screened (review round 1, Issue 8a):
/// a step naming a child with no inode record in a slot nobody leases is
/// refused (`EINVAL`, `xv_cross_owner_steps_rejected`) and plants no
/// dentry. (The coverage-checked `Covered` verdict has its own pin,
/// `a_step_outside_its_scopes_parked_keys_takes_its_own_guards_and_parks`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_served_insert_naming_an_unmintable_child_is_refused_and_plants_nothing() {
    use squeezefs::meta_ship::{MetaCall, MetaOp, PeerOwner};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let endpoint = holders.host.endpoint().to_string();
    let remote = MetaShipRouter::new(Arc::clone(&routed), "node-c", SECRET.to_vec());
    let peer = Arc::new(PeerOwner::new("appender-1", &endpoint));
    let before = cross_owner_stats();
    // Routing slot 39 (forest slot 40) is leased by nobody in this set;
    // local 77 there has no record — `allocate_guest_ino` never minted it.
    let bogus = make_global_ino_width(77, 39, routed.routing_width());
    let op = MetaOp {
        id: remote.next_request_id(),
        call: MetaCall::XvStep {
            tx_id: 0x99,
            step_idx: 0,
            step: crossvol_tx::XvStep::InsertDentry {
                parent: shared,
                name: "planted".into(),
                child: bogus,
                ft_bits: libc::S_IFREG,
                parent_update: 0,
            },
            scope: 0,
        },
    };
    let mut r = remote.ship_ops(&peer, vec![op]).await.expect("shipped");
    let outcome = r.pop().unwrap().outcome;
    let err = outcome.expect_err("refused");
    assert_eq!(err.errno, libc::EINVAL, "{err:?}");
    assert!(
        names_in(&routed, shared).await.is_empty(),
        "nothing planted"
    );
    let after = cross_owner_stats();
    assert_eq!(after.steps_rejected - before.steps_rejected, 1);
    assert_eq!(after.steps_served - before.steps_served, 0);
    holders.tear_down();
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// A LIVE witness refusal at a shipped step compensates every applied
/// half (review round 1, Issue 15): a `link` whose foreign insert the
/// holder refuses gets its raised count back (no C10 leak); a `rename`
/// whose foreign insert is refused after its source removal committed
/// gets the source name back (no C9 orphan); each op answers `EEXIST`,
/// the intent retires, the tree is byte-exact and fsck clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_refusal_compensates_a_links_count_and_a_renames_removed_source() {
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
    let f = routed
        .create(mine, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    // link: [SetNlink(f) @ own, InsertDentry(shared) @ holder — refused].
    // The retirement rides the last inverse's own entry (review round 2,
    // Issue 26): tx0 + ONE compensating entry, never a third.
    let entries = || squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let entries_before = entries();
    crossvol_tx::TEST_XV_SERVE_SKIP_ONCE.store(true, Ordering::SeqCst);
    let e = routed.link(f, shared, "l").await.expect_err("refused");
    assert_eq!(e.to_errno(), libc::EEXIST, "{e}");
    assert_eq!(
        entries() - entries_before,
        2,
        "tx0 + one compensating entry carrying the retirement"
    );
    assert_eq!(
        routed.getattr(f).await.unwrap().nlink,
        1,
        "the raised count came back"
    );
    assert!(names_in(&routed, shared).await.is_empty());
    // rename: [RemoveDentry(mine, f) @ own, InsertDentry(shared, g) @
    // holder — refused, TouchCtime]: the source name returns.
    crossvol_tx::TEST_XV_SERVE_SKIP_ONCE.store(true, Ordering::SeqCst);
    let e = routed
        .rename(mine, "f", shared, "g", 0)
        .await
        .expect_err("refused");
    assert_eq!(e.to_errno(), libc::EEXIST, "{e}");
    assert_eq!(
        names_in(&routed, mine).await,
        vec!["f".to_string()],
        "the source name returned"
    );
    assert!(names_in(&routed, shared).await.is_empty());
    assert_eq!(routed.lookup(mine, "f").await.unwrap().ino, f);
    assert!(!crossvol_tx::TEST_XV_SERVE_SKIP_ONCE.load(Ordering::SeqCst));
    assert_eq!(open_intents(&routed).await, 0, "both intents retired");
    assert_closed("live refusal compensation");
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
