//! Symmetric metadata program, PR 7b — **directory STRIPING**
//! (`docs/design-symmetric-metadata.md` §5.6.5, KD-SYM-20; owner ruling
//! R-SYM-5; §11 the Striping family; risks R24 / R26).
//!
//! Under `SQUEEZEFS_SYMMETRIC_META=1` a hot shared directory `D` flips
//! into `K` STRIPES — `K` `S_IFDIR` stripe inos on `D`'s volume, each in a
//! creator's slot, `stripe(name) = hash54(name) % K` — so a create into
//! `D` is PR 6's shipped `InsertDentry` with the stripe as its key parent
//! and `N` creators spread over `K` holders. The stripe map is `K + 2`
//! reserved-name (NUL-led) dentries in `D`'s own tree written as ONE PR 6
//! intent (the commit marker LAST), names re-home lazily under a
//! `migrating` flag, `readdir` merges the `K` stripes in the shipped
//! cookie order, and `rmdir` probes every stripe empty under ONE
//! exclusive guard set and removes the directory, its markers and the
//! stripes' counts in ONE intent (the dying mark an insert parked behind
//! it is refused by).
//!
//! **The multi-holder shape** is PR 6's: ONE process holds region 0 (the
//! manager) and the DECLARED regions (`SQUEEZEFS_TEST_SYM_APPENDER_SLOTS`),
//! each with its own ring and lease set; a stripe in a declared region's
//! slot is FOREIGN to appender 0 and every op on it ships over a real
//! `cluster_wire` session to the S8 service over the same backend.
//!
//! **`SQUEEZEFS_SYMMETRIC_META=0`, a bit-17-absent volume and
//! `SQUEEZEFS_SYM_DIR_STRIPES=1` are the shipped posture exactly**: no
//! flip, no map, every Striping gauge 0.

use squeezefs::cluster_wire as cw;
use squeezefs::data_grant::AsyncVerbRouter;
use squeezefs::meta_backend::crossvol_tx::{
    self, cross_owner_stats, install_xv_shipper, uninstall_xv_shipper, TEST_XV_SEAM_AFTER_STEPS,
};
use squeezefs::meta_backend::dir_stripe::{
    self, is_marker_name, stripe_of, stripe_stats, DIR_STRIPES_ENV, MIGRATING_MARKER,
    STRIPED_MARKER, STRIPES_MAX, TEST_STRIPE_HOLD_MIGRATION, TEST_STRIPE_RMDIR_GUARDS_HELD,
    TEST_STRIPE_RMDIR_HOLD_MS,
};
use squeezefs::meta_backend::kv::appender::TEST_APPENDER_SLOTS_ENV;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options, ROOT_INO};
use squeezefs::meta_backend::kv::record::{dentry_name_hash54, ForestSlot};
use squeezefs::meta_backend::kv::slot_lease::SYMMETRIC_META_ENV;
use squeezefs::meta_backend::{
    make_global_ino_width, open_routed_meta_set, plan_meta_slot_set, IntentCreatePreset, Metadata,
    RoutedMetaBackend,
};
use squeezefs::meta_ship::manager::ManagerSetService;
use squeezefs::meta_ship::{MetaShipRouter, MetaShipService};
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Harness (the sym_cross_owner_tests shape: 64 KiB nodes, a 1 MiB ring).
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const SECRET: &[u8] = b"sym-dir-stripe-tests-enroll-secret";

/// Forest slot 4 (routing slot 3) — the declared appender 1's.
const SLOT_B: ForestSlot = 4;
/// Forest slot 6 (routing slot 5) — the declared appender 2's.
const SLOT_C: ForestSlot = 6;
const TWO_HOLDERS: &str = "1:4";
const THREE_HOLDERS: &str = "1:4;2:6";

/// The seams and knobs are process-global; every test serializes on it.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// The wave's wider leg formats with 256 KiB nodes (31 declared ids per
/// directory extent); every other contract keeps the 64 KiB harness.
fn node_size() -> usize {
    std::env::var("SQZ_STRIPE_WAVE_NODE_KIB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map_or(NODE_SIZE, |kib| kib * 1024)
}

/// The volume scales with the node size so the EXTENT population stays
/// the default harness's (31 declared rings + their slot trees exhausted
/// a 64 MiB volume of 256 KiB extents at the open).
fn vol_len() -> u64 {
    VOL_LEN * (node_size() / NODE_SIZE) as u64
}

fn set_opts(dir: &std::path::Path) -> FormatV3Options {
    let node_size = node_size();
    FormatV3Options {
        node_size,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: Some(format_config_for(dir)),
    }
}

async fn format_member(dir: &std::path::Path, name: &str, stamped: bool) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p)
        .unwrap()
        .set_len(vol_len())
        .unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    if stamped {
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    } else {
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    }
    let r = format_v3_stamped(&p, vol_len(), &set_opts(dir), plan.stamps[0].clone()).await;
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

/// Drain the mount's background stripe work (a flip's migration) before
/// the volumes shut down — a task outliving the set would fail its
/// commits at the write gate and leave a RAM intent register entry no
/// later contract can close.
async fn shutdown(routed: &RoutedMetaBackend) {
    for _ in 0..800 {
        if routed.stripe_work_in_flight() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(
        routed.stripe_work_in_flight(),
        0,
        "background stripe work drained"
    );
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Hold the background migration for the test's life; released on drop
/// (a panic never leaves the process-global seam set for the next test).
struct HoldMigration;

impl HoldMigration {
    fn arm() -> Self {
        TEST_STRIPE_HOLD_MIGRATION.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for HoldMigration {
    fn drop(&mut self) {
        TEST_STRIPE_HOLD_MIGRATION.store(false, Ordering::SeqCst);
        TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::SeqCst);
    }
}

fn listener_cfg() -> cw::RpcListenerConfig {
    cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
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
/// every declared appender's endpoint registered, the shipper installed.
struct Holders {
    host: Arc<cw::RpcListener>,
}

impl Holders {
    async fn stand_up(routed: &Arc<RoutedMetaBackend>, appenders: &[u32]) -> Self {
        let host = cw::RpcListener::start_async(
            listener_cfg(),
            SECRET.to_vec(),
            meta_router(routed) as Arc<dyn cw::RpcAsyncService>,
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

/// Mint a DIRECTORY under the root whose ino routes to forest slot `slot`
/// while the slot is still the manager's, then release the slot so the
/// next open's declared region takes it.
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
/// released to `Unleased` so the declared regions of the NEXT open take
/// them. Returns `(uris, dir inos in `slots` order)`.
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

/// Every name `readdir` lists (sorted), through the trait face.
async fn names_in(routed: &RoutedMetaBackend, dir: u64) -> Vec<String> {
    let mut names: Vec<String> = routed
        .readdir(dir, 0, 1 << 16)
        .await
        .expect("readdir")
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n != "." && n != "..")
        .collect();
    names.sort();
    names
}

/// The RAW dentry names of `dir`'s OWN tree (markers included).
async fn raw_names(routed: &RoutedMetaBackend, dir: u64) -> Vec<String> {
    let (v, local) = routed.route_ino(dir);
    let mut out = Vec::new();
    let mut cursor = 0u64;
    loop {
        let page = routed.volumes[v]
            .readdir_page(local, cursor, 512)
            .await
            .expect("raw page");
        let Some((last, _)) = page.last() else { break };
        cursor = *last;
        out.extend(page.into_iter().map(|(_, e)| e.name));
    }
    out.sort();
    out
}

/// The fsck engine's INODE PLANE over an already-open probe set (the
/// `run_offline` harness's own context minus its live-client preflight,
/// which this process's live pid trips): a data router with no staging,
/// the inode-plane-only options.
async fn probe_inode_plane(probe: &Arc<RoutedMetaBackend>) -> squeezefs::fsck::FsckReport {
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let alloc = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("vol-probe")
            .await
            .unwrap(),
    );
    let dev = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new("/dev/null"));
    let cache = squeezefs::cache::TieredCache::new(
        Vec::new(),
        Some("64MB"),
        Some("64MB"),
        None,
        None,
        alloc.clone(),
        dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = squeezefs::routing::DataRouter::new(dlm, cache, alloc, dev);
    router.set_meta_backend(Arc::clone(probe));
    let ctx = squeezefs::fsck::FsckCtx {
        meta: Arc::clone(probe),
        router,
        staging_dirs: Vec::new(),
        expected_generation: None,
    };
    let mut opts = squeezefs::fsck::FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(10);
    opts.inode_plane_only = true;
    squeezefs::fsck::run(&ctx, &opts)
        .await
        .expect("the engine runs")
}

/// The RAW dentry of `name` in `dir`'s own tree (markers included).
async fn raw_entry(routed: &RoutedMetaBackend, dir: u64, name: &str) -> Option<u64> {
    let (v, local) = routed.route_ino(dir);
    let mut cursor = 0u64;
    loop {
        let page = routed.volumes[v]
            .readdir_page(local, cursor, 512)
            .await
            .expect("raw page");
        let (last, _) = page.last()?;
        cursor = *last;
        if let Some((_, e)) = page.iter().find(|(_, e)| e.name == name) {
            return Some(e.ino);
        }
    }
}

/// Wait for a flip's BACKGROUND migration to clear the flag (bounded).
async fn wait_migrated(routed: &RoutedMetaBackend, dir: u64) {
    for _ in 0..800 {
        let map = routed
            .stripe_map(dir)
            .await
            .expect("read")
            .expect("striped");
        if !map.migrating {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("directory {dir}: the migration never cleared its flag");
}

async fn lookup_opt(routed: &RoutedMetaBackend, parent: u64, name: &str) -> Option<u64> {
    routed.lookup(parent, name).await.ok().map(|i| i.ino)
}

/// The offline census as the contracts' oracle: clean, AND a census —
/// the inode plane recorded a verdict for every volume (a probe over an
/// uncovered ring window records none, so a silently skipped plane can
/// never read as "clean" — review round 1, Issue 21c).
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
    assert_eq!(
        report.counters.stripe_findings, 0,
        "fsck_stripe_findings must stay 0"
    );
    assert_eq!(
        report.counters.inode_plane_volumes_covered,
        uris.len() as u64,
        "the inode plane recorded a verdict for every volume (an uncovered window is not a \
         census)"
    );
}

/// The leave COVERED — asserted before the census (Issue 21c): a writer
/// re-open under the same partition finds NO own residue to replay
/// (`appender_self_recoveries == 0` — the belt gauge of an uncovered
/// leave: PR 3's grant path × PR 2's leave law), leaves again cleanly,
/// then the offline probe's census is a census.
async fn fsck_clean_covered(uris: &[String], partition: Option<&str>) {
    let routed = open_under(uris, true, partition).await;
    for (i, v) in routed.volumes.iter().enumerate() {
        let s = v.appender_stats().expect("a forest volume");
        assert_eq!(
            s.self_recoveries, 0,
            "volume {i}: the previous leave left an UNCOVERED ring (own residue replayed at \
             this open) — the shutdown fixpoint did not converge: {s:?}"
        );
        assert_eq!(
            s.dependency_stalls, 0,
            "volume {i}: manager_dependency_stalls must stay 0 at the sized grant: {s:?}"
        );
    }
    shutdown(&routed).await;
    drop(routed);
    fsck_clean(uris).await;
}

/// PR 6's `xv_local_step_unguarded` tripwire fires on EVERY cross-owner
/// create (the child ino is allocated inside `create_in_foreign_directory`
/// AFTER the op's guards, so the local `CreateInode` step's `I{child}` is
/// never in the scope's local set — pre-existing, PR 6's own; routed to
/// its owner / PR 12 in the review file, Issue 22). It is allow-listed BY
/// NAME: every other tripwire in this process must be 0.
const ROUTED_TRIPWIRE: &str = "xv_local_step_unguarded";

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
    let total = squeezefs::fuse_client::METRICS
        .invariant_tripwires
        .load(Ordering::Relaxed);
    let routed = squeezefs::invariant_tripwire_count(ROUTED_TRIPWIRE);
    assert_eq!(
        total, routed,
        "{what}: invariant_tripwires must stay 0 beyond the routed `{ROUTED_TRIPWIRE}` \
         (total {total}, routed {routed})"
    );
}

/// A solo armed volume (one appender, the manager) with an empty
/// directory `work` under the root.
async fn solo_armed(dir: &std::path::Path) -> (Vec<String>, Arc<RoutedMetaBackend>, u64) {
    let uris = vec![format_member(dir, "meta0", true).await];
    let routed = open_under(&uris, true, None).await;
    let work = routed
        .create(ROOT_INO, "work", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir work")
        .ino;
    (uris, routed, work)
}

async fn create_files(routed: &RoutedMetaBackend, dir: u64, prefix: &str, n: usize) -> Vec<String> {
    let mut names = Vec::with_capacity(n);
    for i in 0..n {
        let name = format!("{prefix}{i:05}");
        routed
            .create(dir, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
        names.push(name);
    }
    names.sort();
    names
}

fn hash_seed(routed: &RoutedMetaBackend, dir: u64) -> u64 {
    let (v, _) = routed.route_ino(dir);
    routed.volumes[v].superblock().hash_seed
}

/// Every stripe's own raw dentry set holds exactly the names whose
/// `hash54 % K` is its index, and no name is anywhere twice.
async fn assert_names_route_to_hash_mod_k(
    routed: &RoutedMetaBackend,
    map: &dir_stripe::StripeMap,
    expect: &[String],
) {
    let seed = hash_seed(routed, map.dir);
    let mut seen: Vec<String> = Vec::new();
    for (i, stripe) in map.stripes.iter().enumerate() {
        for name in raw_names(routed, *stripe).await {
            assert!(!is_marker_name(&name), "a stripe holds no marker");
            let h = dentry_name_hash54(name.as_bytes(), seed);
            assert_eq!(
                usize::from(stripe_of(h, map.k())),
                i,
                "{name:?} sits in stripe {i} but hashes to {}",
                stripe_of(h, map.k())
            );
            seen.push(name);
        }
    }
    seen.sort();
    assert_eq!(seen, expect, "every name in exactly one stripe");
    let own: Vec<String> = raw_names(routed, map.dir)
        .await
        .into_iter()
        .filter(|n| !is_marker_name(n))
        .collect();
    assert!(
        own.is_empty(),
        "the directory's own tree holds only markers: {own:?}"
    );
}

// ---------------------------------------------------------------------------
// The explicit flip, the routing, the LWW rule during migration (R24).
// ---------------------------------------------------------------------------

/// A directory with existing names flips into `K` stripes: the map is
/// `K + 2` markers in its own tree (the commit marker present), `readdir`
/// still lists every name exactly once while the names are unmigrated
/// (the fallback), a lookup resolves each, a NEW name lands in its stripe
/// and an existing name re-created answers `EEXIST` (the screen covers
/// both homes). After the migration every name sits in the stripe its
/// hash names, the directory's own tree holds only markers, the flag is
/// gone, `dir_stripe_migrated_names` counted every move, fsck is clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn names_route_to_hash_mod_k_and_are_served_from_exactly_one_place_during_migration() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _hold = HoldMigration::arm();
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let before = stripe_stats();
    let mut names = create_files(&routed, work, "pre-", 40).await;
    let sub = routed
        .create(work, "subdir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir")
        .ino;
    names.push("subdir".to_string());
    names.sort();
    let nlink_before = routed.getattr(work).await.unwrap().nlink;

    routed
        .stripe_dir(work, 8)
        .await
        .expect("the explicit flip on a directory this mount holds");
    let map = routed
        .stripe_map(work)
        .await
        .expect("read")
        .expect("striped");
    assert_eq!(map.k(), 8);
    assert!(map.migrating, "names are still in the directory's own tree");
    let raw = raw_names(&routed, work).await;
    assert!(raw.contains(&STRIPED_MARKER.to_string()));
    assert!(raw.contains(&MIGRATING_MARKER.to_string()));
    assert_eq!(
        raw.iter().filter(|n| is_marker_name(n)).count(),
        usize::from(map.k()) + 2,
        "K stripe entries + the flag + the commit marker"
    );
    for s in &map.stripes {
        let rec = routed
            .getattr(*s)
            .await
            .expect("a stripe is an S_IFDIR record");
        assert_eq!(rec.mode & libc::S_IFMT, libc::S_IFDIR);
        assert_eq!(rec.nlink, 2);
    }

    // The fallback: every unmigrated name resolves and lists once.
    assert_eq!(names_in(&routed, work).await, names);
    for n in &names {
        assert!(lookup_opt(&routed, work, n).await.is_some(), "{n} resolves");
    }
    assert!(
        routed
            .create(work, "pre-00007", libc::S_IFREG | 0o644, 0, 0)
            .await
            .is_err(),
        "an unmigrated name re-created is EEXIST — the screen covers both homes"
    );
    // A NEW name lands in its stripe, never in the directory's tree.
    routed
        .create(work, "fresh", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create into a striped directory");
    names.push("fresh".to_string());
    names.sort();
    let (i, stripe) = map.stripe_for(dentry_name_hash54(b"fresh", hash_seed(&routed, work)));
    assert!(
        raw_names(&routed, stripe)
            .await
            .contains(&"fresh".to_string()),
        "the new name sits in stripe {i}"
    );
    assert!(!raw_names(&routed, work)
        .await
        .contains(&"fresh".to_string()));
    assert_eq!(names_in(&routed, work).await, names);
    // The fold: the subdirectory's link is counted once — in the
    // directory's own tree until it moves, in its stripe after.
    assert_eq!(routed.getattr(work).await.unwrap().nlink, nlink_before);

    let moved = routed.migrate_dir(work).await.expect("the migration");
    assert_eq!(moved, 41, "40 files + the subdirectory re-homed");
    let map = routed
        .stripe_map(work)
        .await
        .expect("read")
        .expect("still striped");
    assert!(!map.migrating, "the flag cleared");
    assert_names_route_to_hash_mod_k(&routed, &map, &names).await;
    assert_eq!(names_in(&routed, work).await, names);
    for n in &names {
        assert!(
            lookup_opt(&routed, work, n).await.is_some(),
            "{n} resolves after"
        );
    }
    assert_eq!(
        routed.getattr(work).await.unwrap().nlink,
        nlink_before,
        "the fold is exact"
    );
    assert_eq!(lookup_opt(&routed, work, "subdir").await, Some(sub));
    assert_eq!(
        routed.lookup(sub, "..").await.expect("..").ino,
        work,
        "the child of a stripe answers the DIRECTORY as its parent"
    );

    let after = stripe_stats();
    assert_eq!(after.flips - before.flips, 1);
    assert_eq!(after.striped_dirs - before.striped_dirs, 1);
    assert_eq!(after.migrated_names - before.migrated_names, 41);
    assert!(after.readdir_merges > before.readdir_merges);
    assert_eq!(
        after.supply_rpcs - before.supply_rpcs,
        8,
        "every stripe was supplied — by the holder itself, no creator known"
    );
    assert_eq!(open_intents(&routed).await, 0);
    assert_closed("flip");
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// A flip is idempotent and its `K` is clamped into `2..=STRIPES_MAX`; an
/// explicit ask past the ceiling takes the ceiling; a second ask on a
/// striped directory moves nothing; the `getxattr` face answers `K`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flip_is_idempotent_and_its_k_is_clamped() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let before = stripe_stats();
    routed.stripe_dir(work, 1000).await.expect("flip");
    let map = routed.stripe_map(work).await.unwrap().unwrap();
    assert_eq!(map.k(), STRIPES_MAX, "clamped to the ceiling");
    routed
        .stripe_dir(work, 4)
        .await
        .expect("a second flip is a no-op");
    assert_eq!(
        routed.stripe_map(work).await.unwrap().unwrap().k(),
        STRIPES_MAX
    );
    assert_eq!(stripe_stats().flips - before.flips, 1);
    assert_eq!(
        routed.stripes_xattr_value(work).await.unwrap(),
        Some(STRIPES_MAX.to_string().into_bytes())
    );
    assert_eq!(routed.stripes_xattr_value(ROOT_INO).await.unwrap(), None);
    assert_eq!(dir_stripe::clamp_explicit_k(b"3"), 3);
    assert_eq!(dir_stripe::clamp_explicit_k(b"1"), 2);
    assert_eq!(
        dir_stripe::clamp_explicit_k(b"garbage"),
        dir_stripe::stripe_count()
    );
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// readdir: the K-way merge with a stable continuation.
// ---------------------------------------------------------------------------

/// `readdir` of a striped directory is the merge of its stripes in the
/// shipped `(hash54, coll)` cookie order — pages of 7 with the cookie as
/// the exact continuation — and every name present for the WHOLE scan is
/// returned exactly once while other names are created and unlinked
/// between the pages (the POSIX readdir contract).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readdir_merges_k_stripes_in_hash_order_with_a_stable_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let stable = create_files(&routed, work, "s-", 120).await;
    // The flip kicks the lazy migration in the background: the scan runs
    // WHILE names move from the directory's own tree into their stripes
    // (the same cookie on both sides — the seeded hash is the volume's),
    // beside the creates and unlinks between pages.
    routed.stripe_dir(work, 16).await.expect("flip");
    let before = stripe_stats();

    let mut seen: Vec<String> = Vec::new();
    let mut cookies: Vec<u64> = Vec::new();
    let mut cursor = 0u64;
    let mut churn = 0usize;
    loop {
        let page = routed.readdir_stream(work, cursor, 7).await.expect("page");
        if page.is_empty() {
            break;
        }
        for (cookie, entry) in &page {
            assert!(
                cookies.last().is_none_or(|last| cookie > last),
                "cookies ascend across pages and stripes"
            );
            cookies.push(*cookie);
            seen.push(entry.name.clone());
        }
        cursor = *cookies.last().unwrap();
        // Churn between pages: a create and an unlink of OTHER names.
        let n = format!("churn-{churn:04}");
        routed
            .create(work, &n, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("churn create");
        if churn > 0 {
            let prev = format!("churn-{:04}", churn - 1);
            let _ = routed.unlink(work, &prev).await;
        }
        churn += 1;
    }
    let mut stable_seen: Vec<String> = seen
        .iter()
        .filter(|n| n.starts_with("s-"))
        .cloned()
        .collect();
    stable_seen.sort();
    assert_eq!(stable_seen, stable, "every stable name exactly once");
    let mut dedup = seen.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), seen.len(), "no name twice in one scan");
    assert!(stripe_stats().readdir_merges > before.readdir_merges);
    wait_migrated(&routed, work).await;
    // A second scan over the settled tree lists the same stable set.
    assert!(names_in(&routed, work)
        .await
        .iter()
        .filter(|n| n.starts_with("s-"))
        .eq(stable.iter()));
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// The crash windows of the flip (§5.6.5 — the intent rolls forward).
// ---------------------------------------------------------------------------

/// A flip severed after `j` of its `K + 2` marker steps leaves NO live map
/// (the commit marker is last) and one open intent; the next open of the
/// slot's lessee rolls it forward to the WHOLE map, and the migration
/// completes from it — at every window: the first step, mid-map, after
/// the flag, and after every step but the retirement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_flip_completes_on_the_successor_lessee() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _hold = HoldMigration::arm();
    let k: u16 = 6;
    let steps = usize::from(k) + 2;
    for sever_after in [1usize, 3, steps - 1, steps] {
        let sub = dir.path().join(format!("w{sever_after}"));
        std::fs::create_dir_all(&sub).unwrap();
        let (uris, routed, work) = solo_armed(&sub).await;
        let names = create_files(&routed, work, "f-", 12).await;
        TEST_XV_SEAM_AFTER_STEPS.store(sever_after as u64 + 1, Ordering::SeqCst);
        let r = routed.stripe_dir(work, k).await;
        TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::SeqCst);
        assert!(r.is_err(), "window {sever_after}: the flip was severed");
        // The commit marker is the LAST step: severed before it, no map is
        // live; severed after every step but the retirement, the whole map
        // is live and only the intent is left to retire.
        assert_eq!(
            routed.stripe_map(work).await.unwrap().is_some(),
            sever_after == steps,
            "window {sever_after}: a live map iff the commit marker landed"
        );
        assert_eq!(
            names_in(&routed, work).await,
            names,
            "window {sever_after}: names intact"
        );
        assert_eq!(
            open_intents(&routed).await,
            1,
            "window {sever_after}: one open intent"
        );
        shutdown(&routed).await;
        drop(routed);

        // The successor lessee of the slot: the next open rolls forward.
        let routed = open_under(&uris, true, None).await;
        assert_eq!(
            open_intents(&routed).await,
            0,
            "window {sever_after}: rolled forward"
        );
        let map =
            routed.stripe_map(work).await.unwrap().unwrap_or_else(|| {
                panic!("window {sever_after}: the whole map exists after recovery")
            });
        assert_eq!(map.k(), k);
        assert!(map.migrating);
        assert_eq!(names_in(&routed, work).await, names);
        routed
            .migrate_dir(work)
            .await
            .expect("the migration resumes");
        let map = routed.stripe_map(work).await.unwrap().unwrap();
        assert!(!map.migrating);
        assert_names_route_to_hash_mod_k(&routed, &map, &names).await;
        shutdown(&routed).await;
        drop(routed);
        fsck_clean(&uris).await;
    }
}

/// A migration killed between a name's insert into its stripe and its
/// removal from the directory leaves the name in BOTH homes: the stripe
/// wins (one listing, one resolution), and the resumed migration drops the
/// duplicate from the directory alone — no second insert.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_in_both_homes_during_migration_is_served_from_the_stripe() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _hold = HoldMigration::arm();
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let names = create_files(&routed, work, "m-", 5).await;
    routed.stripe_dir(work, 4).await.expect("flip");
    // Sever the FIRST name's move after its insert (step 0 of the pair):
    // the name is then in the stripe AND in the directory.
    TEST_XV_SEAM_AFTER_STEPS.store(2, Ordering::SeqCst);
    let r = routed.migrate_dir(work).await;
    TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::SeqCst);
    assert!(r.is_err(), "the move was severed");
    let map = routed.stripe_map(work).await.unwrap().unwrap();
    assert!(map.migrating);
    let seed = hash_seed(&routed, work);
    let own: Vec<String> = raw_names(&routed, work)
        .await
        .into_iter()
        .filter(|n| !is_marker_name(n))
        .collect();
    // Exactly one name is in both homes.
    let mut in_both = Vec::new();
    for n in &own {
        let (_, s) = map.stripe_for(dentry_name_hash54(n.as_bytes(), seed));
        if raw_names(&routed, s).await.contains(n) {
            in_both.push(n.clone());
        }
    }
    assert_eq!(in_both.len(), 1, "one name in both homes: {in_both:?}");
    assert_eq!(names_in(&routed, work).await, names, "listed exactly once");
    assert!(lookup_opt(&routed, work, &in_both[0]).await.is_some());
    // The intent is open (the removal never ran); the cadence/next mount
    // completes it — here the resumed migration does, and the duplicate
    // costs no second insert.
    let before = cross_owner_stats();
    routed.migrate_dir(work).await.expect("resume");
    let after = cross_owner_stats();
    assert!(after.intents_retired > before.intents_retired);
    let map = routed.stripe_map(work).await.unwrap().unwrap();
    assert!(!map.migrating);
    assert_names_route_to_hash_mod_k(&routed, &map, &names).await;
    // The severed move's own intent is still durable and ABANDONED: the
    // set's roll-forward (the cadence's body) completes it — its insert
    // already applied, its removal gone (`AlreadyApplied` both) — and
    // the ledger closes.
    crossvol_tx::roll_forward_open_intents(&routed)
        .await
        .expect("roll-forward");
    assert_eq!(open_intents(&routed).await, 0);
    assert_closed("severed move");
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// rmdir (R26) and the attribute fold.
// ---------------------------------------------------------------------------

/// Hold a striped `rmdir` between its probes and its intent for the
/// test's life; released on drop.
struct HoldRmdir;

impl HoldRmdir {
    fn arm(ms: u64) -> Self {
        TEST_STRIPE_RMDIR_GUARDS_HELD.store(false, Ordering::SeqCst);
        TEST_STRIPE_RMDIR_HOLD_MS.store(ms, Ordering::SeqCst);
        Self
    }
    async fn wait_held(&self) {
        for _ in 0..400 {
            if TEST_STRIPE_RMDIR_GUARDS_HELD.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the rmdir never reached its hold");
    }
}

impl Drop for HoldRmdir {
    fn drop(&mut self) {
        TEST_STRIPE_RMDIR_HOLD_MS.store(0, Ordering::SeqCst);
        TEST_STRIPE_RMDIR_GUARDS_HELD.store(false, Ordering::SeqCst);
    }
}

/// `rmdir` of a striped directory (R26, review round 1 Issues 2 / 6): a
/// non-empty one answers `ENOTEMPTY` having written NOTHING (every stripe
/// still `nlink 2`, a create still lands); an empty one removes the
/// directory, its markers and every stripe record in ONE intent; and
/// every stripe-entering mutation that RACES the rmdir — a `create`, a
/// `link` and a `rename` into the directory, parked behind the rmdir's
/// exclusive stripe guards while it holds them — is refused `ENOENT`
/// after it (`dir_stripe_dying_refusals` + 3), never landed in a
/// destroyed stripe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rmdir_of_a_striped_directory_never_races_a_create() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let victim = routed
        .create(work, "victim", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir victim")
        .ino;
    routed.stripe_dir(victim, 4).await.expect("flip");
    routed.migrate_dir(victim).await.expect("migrate");
    routed
        .create(victim, "keep", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");
    let map = routed.stripe_map(victim).await.unwrap().unwrap();

    // Non-empty: ENOTEMPTY with nothing written — every stripe live,
    // creates still land.
    let e = routed.unlink(work, "victim").await.expect_err("non-empty");
    assert_eq!(e.to_errno(), libc::ENOTEMPTY, "{e}");
    for s in &map.stripes {
        assert_eq!(routed.getattr(*s).await.unwrap().nlink, 2, "untouched");
    }
    routed
        .create(victim, "more", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("creates still land after a refused rmdir");
    assert_eq!(lookup_opt(&routed, work, "victim").await, Some(victim));
    routed.unlink(victim, "keep").await.expect("unlink");
    routed.unlink(victim, "more").await.expect("unlink");

    // The R26 schedule: the rmdir holds its guards (the probes done, the
    // intent not yet written); a create, a link and a rename into the
    // directory arrive meanwhile and park on the stripes' guards; the
    // rmdir's intent lands; every parked insert resumes to a parent
    // whose record reads `nlink 0` and refuses `ENOENT`.
    let src = routed
        .create(work, "src-file", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a file to link and rename")
        .ino;
    routed
        .create(work, "moved", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a file to rename in");
    let before = stripe_stats();
    let hold = HoldRmdir::arm(600);
    let r = Arc::clone(&routed);
    let rmdir = tokio::spawn(async move { r.unlink(work, "victim").await });
    hold.wait_held().await;
    let r1 = Arc::clone(&routed);
    let create = tokio::spawn(async move {
        r1.create(victim, "late", libc::S_IFREG | 0o644, 0, 0)
            .await
            .map(|_| ())
    });
    let r2 = Arc::clone(&routed);
    let link = tokio::spawn(async move { r2.link(src, victim, "late-link").await.map(|_| ()) });
    let r3 = Arc::clone(&routed);
    let rename =
        tokio::spawn(async move { r3.rename(work, "moved", victim, "late-moved", 0).await });
    rmdir
        .await
        .unwrap()
        .expect("the rmdir completes over the parked inserts");
    drop(hold);
    for (what, out) in [
        ("create", create.await.unwrap()),
        ("link", link.await.unwrap()),
        ("rename", rename.await.unwrap()),
    ] {
        let e = out.expect_err(&format!("{what} into the removed directory is refused"));
        assert_eq!(e.to_errno(), libc::ENOENT, "{what}: {e}");
    }
    assert_eq!(stripe_stats().dying_refusals - before.dying_refusals, 3);
    assert_eq!(lookup_opt(&routed, work, "victim").await, None);
    assert!(
        lookup_opt(&routed, work, "moved").await.is_some(),
        "the refused rename left its source in place"
    );
    assert_eq!(
        routed.getattr(src).await.unwrap().nlink,
        1,
        "the refused link counted nothing"
    );
    for s in &map.stripes {
        let (v, local) = routed.route_ino(*s);
        assert!(
            routed.volumes[v]
                .read_inode_value_routed(local)
                .await
                .unwrap()
                .is_none(),
            "stripe {s} destroyed"
        );
    }
    assert_eq!(stripe_stats().rmdirs - before.rmdirs, 1);
    assert_eq!(open_intents(&routed).await, 0);
    assert_closed("rmdir");
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// A striped `rmdir` KILLED at every step of its ONE intent (review round
/// 1, Issue 2 — the first build's mark was its own retired intent, and a
/// kill after it left `nlink 0` stripes under a LIVE map for the corpse
/// sweep to destroy): the next lessee rolls the removal FORWARD whole —
/// the name gone from its parent, every marker gone, every stripe
/// `nlink 0` or destroyed, no C9/C10/C17 finding — and a sibling striped
/// directory on the same volume stays fully usable (a create lands, its
/// names resolve).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_striped_rmdir_rolls_forward_whole_at_every_window() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let k: u16 = 3;
    // Steps: the parent dentry, the commit marker, K stripe entries, K
    // marks, D's own count (the migration finished, so no flag step).
    let steps = 1 + 1 + usize::from(k) + usize::from(k) + 1;
    for sever_after in [1usize, 2, 3, steps - 1, steps] {
        let sub = dir.path().join(format!("w{sever_after}"));
        std::fs::create_dir_all(&sub).unwrap();
        let (uris, routed, work) = solo_armed(&sub).await;
        let victim = routed
            .create(work, "victim", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        let sibling = routed
            .create(work, "sibling", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        for d in [victim, sibling] {
            routed.stripe_dir(d, k).await.unwrap();
            wait_migrated(&routed, d).await;
        }
        let kept = create_files(&routed, sibling, "s-", 5).await;
        let map = routed.stripe_map(victim).await.unwrap().unwrap();
        TEST_XV_SEAM_AFTER_STEPS.store(sever_after as u64 + 1, Ordering::SeqCst);
        let r = routed.unlink(work, "victim").await;
        TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::SeqCst);
        assert!(r.is_err(), "window {sever_after}: the rmdir was severed");
        assert_eq!(open_intents(&routed).await, 1, "window {sever_after}");
        shutdown(&routed).await;
        drop(routed);

        let routed = open_under(&uris, true, None).await;
        assert_eq!(
            open_intents(&routed).await,
            0,
            "window {sever_after}: rolled forward at the next open"
        );
        assert_eq!(
            lookup_opt(&routed, work, "victim").await,
            None,
            "window {sever_after}: the directory is gone"
        );
        assert_eq!(
            raw_names(&routed, victim).await,
            Vec::<String>::new(),
            "window {sever_after}: every marker gone"
        );
        for s in &map.stripes {
            let (v, local) = routed.route_ino(*s);
            let rec = routed.volumes[v]
                .read_inode_value_routed(local)
                .await
                .unwrap();
            assert!(
                rec.is_none_or(|r| r.nlink == 0),
                "window {sever_after}: stripe {s} dying or destroyed, never live under no map"
            );
        }
        // The sibling: fully usable after the recovery.
        assert_eq!(names_in(&routed, sibling).await, kept);
        routed
            .create(sibling, "after", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap_or_else(|e| panic!("window {sever_after}: a create into the sibling: {e}"));
        assert!(lookup_opt(&routed, sibling, "after").await.is_some());
        shutdown(&routed).await;
        drop(routed);
        fsck_clean(&uris).await;
    }
}

/// `rmdir` of a striped directory whose OWN tree still holds a user name
/// (the migration held; `K = 64` markers ahead of it in hash order)
/// answers `ENOTEMPTY` through the trait face — review round 1, Issue 5:
/// the first build read 8 entries and called a directory carrying 66
/// markers empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rmdir_of_a_migrating_directory_with_an_unmigrated_name_answers_enotempty() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let _hold = HoldMigration::arm();
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let d = routed
        .create(work, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    routed
        .create(d, "only", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    routed
        .stripe_dir(d, STRIPES_MAX)
        .await
        .expect("flip at K = 64");
    let map = routed.stripe_map(d).await.unwrap().unwrap();
    assert_eq!(map.k(), STRIPES_MAX);
    assert!(map.migrating);
    let e = routed
        .unlink(work, "d")
        .await
        .expect_err("one unmigrated name");
    assert_eq!(e.to_errno(), libc::ENOTEMPTY, "{e}");
    assert_eq!(lookup_opt(&routed, work, "d").await, Some(d));
    assert!(lookup_opt(&routed, d, "only").await.is_some());
    // Its removal re-homes the name first, then removes it from the stripe.
    routed
        .unlink(d, "only")
        .await
        .expect("unlink through the re-home");
    assert!(stripe_stats().rehomed_on_touch >= 1);
    routed.unlink(work, "d").await.expect("now empty");
    assert_eq!(lookup_opt(&routed, work, "d").await, None);
    assert_closed("enotempty");
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The whole `rmdir` through the trait face on an EMPTY striped directory
/// (the FUSE handler's call): the name is gone, the stripes are gone,
/// fsck is clean; a striped directory holding a subdirectory in a stripe
/// answers `ENOTEMPTY` without a mark.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rmdir_through_the_trait_face_removes_the_directory_and_its_stripes() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let d = routed
        .create(work, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    routed.stripe_dir(d, 3).await.expect("flip");
    routed.migrate_dir(d).await.expect("migrate");
    routed
        .create(d, "sub", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let e = routed.unlink(work, "d").await.expect_err("a subdirectory");
    assert_eq!(e.to_errno(), libc::ENOTEMPTY);
    routed.unlink(d, "sub").await.expect("rmdir sub");
    let map = routed.stripe_map(d).await.unwrap().unwrap();
    let before = stripe_stats();
    routed
        .unlink(work, "d")
        .await
        .expect("rmdir of an empty striped directory");
    assert_eq!(lookup_opt(&routed, work, "d").await, None);
    for s in &map.stripes {
        let (v, local) = routed.route_ino(*s);
        assert!(routed.volumes[v]
            .read_inode_value_routed(local)
            .await
            .unwrap()
            .is_none());
    }
    assert_eq!(stripe_stats().rmdirs - before.rmdirs, 1);
    assert_eq!(names_in(&routed, work).await, Vec::<String>::new());
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// A striped directory's `nlink` and times are EXACT: `mkdir`s into its
/// stripes raise the fold by one each, `rmdir`s lower it, `mtime`/`ctime`
/// follow the newest stripe, and the fold is persisted onto the
/// directory's own record (`dir_stripe_time_batches`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_striped_directorys_nlink_and_times_are_exact() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    routed.stripe_dir(work, 8).await.expect("flip");
    routed.migrate_dir(work).await.expect("migrate");
    let base = routed.getattr(work).await.unwrap();
    assert_eq!(base.nlink, 2);
    let before = stripe_stats();
    for i in 0..9 {
        routed
            .create(work, &format!("sub{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap();
    }
    let a = routed.getattr(work).await.unwrap();
    assert_eq!(
        a.nlink,
        2 + 9,
        "one link per subdirectory, folded over the stripes"
    );
    assert!(a.mtime >= base.mtime && a.ctime >= base.ctime);
    for i in 0..4 {
        routed.unlink(work, &format!("sub{i}")).await.unwrap();
    }
    assert_eq!(routed.getattr(work).await.unwrap().nlink, 2 + 5);
    assert!(
        stripe_stats().time_batches > before.time_batches,
        "the fold was persisted onto the directory's record"
    );
    // The persist runs ONCE PER CHECKPOINT CADENCE per directory (review
    // round 1, Issue 8 — a `stat` never writes at the stat rate): a burst
    // of `stat`s inside one cadence persists once; past the cadence the
    // stored record catches up with the fold's times.
    let batches = stripe_stats().time_batches;
    for _ in 0..20 {
        let _ = routed.getattr(work).await.unwrap();
    }
    assert_eq!(
        stripe_stats().time_batches,
        batches,
        "stats inside one cadence persist nothing more"
    );
    tokio::time::sleep(std::time::Duration::from_millis(
        squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived() + 50,
    ))
    .await;
    let folded = routed.getattr(work).await.unwrap();
    let (v, local) = routed.route_ino(work);
    let stored = routed.volumes[v]
        .read_inode_value_routed(local)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.mtime, folded.mtime, "persisted at the cadence");
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// `rename` inside a striped directory moves a name between stripes
/// (both stripes on this mount: the ordinary two-parent rename), `link`
/// lands the new name in ITS stripe, and `unlink` removes from the
/// stripe — every face resolves through the map.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_link_and_unlink_route_through_the_stripes() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    routed.stripe_dir(work, 16).await.expect("flip");
    routed.migrate_dir(work).await.expect("migrate");
    let f = routed
        .create(work, "alpha", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let map = routed.stripe_map(work).await.unwrap().unwrap();
    let seed = hash_seed(&routed, work);
    // Find a target name in a DIFFERENT stripe.
    let (ia, _) = map.stripe_for(dentry_name_hash54(b"alpha", seed));
    let target = (0..1000)
        .map(|i| format!("beta{i}"))
        .find(|n| map.stripe_for(dentry_name_hash54(n.as_bytes(), seed)).0 != ia)
        .unwrap();
    routed
        .rename(work, "alpha", work, &target, 0)
        .await
        .expect("rename across stripes");
    assert_eq!(lookup_opt(&routed, work, "alpha").await, None);
    assert_eq!(lookup_opt(&routed, work, &target).await, Some(f));
    routed
        .link(f, work, "gamma")
        .await
        .expect("link into a striped directory");
    assert_eq!(routed.getattr(f).await.unwrap().nlink, 2);
    let mut expect = vec![target.clone(), "gamma".to_string()];
    expect.sort();
    assert_eq!(names_in(&routed, work).await, expect);
    assert_names_route_to_hash_mod_k(&routed, &map, &expect).await;
    routed.unlink(work, "gamma").await.expect("unlink");
    assert_eq!(routed.getattr(f).await.unwrap().nlink, 1);
    assert_eq!(names_in(&routed, work).await, vec![target.clone()]);
    // A rename OUT of the striped directory into a plain one and back.
    let plain = routed
        .create(ROOT_INO, "plain", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    routed
        .rename(work, &target, plain, "moved", 0)
        .await
        .expect("out");
    assert_eq!(names_in(&routed, work).await, Vec::<String>::new());
    assert_eq!(lookup_opt(&routed, plain, "moved").await, Some(f));
    routed
        .rename(plain, "moved", work, "back", 0)
        .await
        .expect("in");
    assert_eq!(names_in(&routed, work).await, vec!["back".to_string()]);
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// The trigger (§5.6.5 "when to stripe") and the multi-holder shape.
// ---------------------------------------------------------------------------

/// The census of a directory another appender holds: appender 0's
/// creates into the declared region's directory are served inserts whose
/// creator resolves to appender 0 (the child's slot); one creator, however
/// many ships, never flips the directory; two creators over `N_floor`
/// flip it, and the flip's stripes are supplied by the creators where
/// they can be reached and minted by the holder otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_flips_at_the_derived_trigger_and_never_below_it() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(plane.holders.holder(SLOT_B).map(|h| h.appender_id), Some(1));
    let holders = Holders::stand_up(&routed, &[1]).await;
    let n_floor = plane.n_floor();
    let before = stripe_stats();

    // ONE creator (appender 0), well past the floor: the census counts
    // it, the directory stays unstriped.
    let n = usize::try_from(n_floor).unwrap() + 4;
    for i in 0..n {
        routed
            .create(shared, &format!("one-{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create into the foreign directory");
    }
    let census = routed.stripe_census(shared);
    assert_eq!(census, vec![(0u32, n as u64)], "one creator, n ships");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        routed.stripe_map(shared).await.unwrap().is_none(),
        "one creator never flips"
    );
    assert_eq!(stripe_stats().flips, before.flips);
    assert!(!dir_stripe::flip_due(&census, n_floor));

    // A SECOND creator (appender 7 — a peer this mount cannot reach) puts
    // the window over the floor from two creators: the flip fires, the
    // unreachable creator declines and the holder mints its share.
    routed.note_served_insert(shared, "x", 7);
    let census = routed.stripe_census(shared);
    assert!(dir_stripe::flip_due(&census, n_floor));
    let mut map = None;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if let Some(m) = routed.stripe_map(shared).await.unwrap() {
            map = Some(m);
            break;
        }
    }
    let map = map.expect("the automatic flip landed");
    assert_eq!(map.k(), dir_stripe::stripe_count());
    assert_eq!(stripe_stats().flips - before.flips, 1);
    assert!(stripe_stats().supply_rpcs > before.supply_rpcs);
    // The names still resolve (migration in flight or done) and a create
    // into the striped foreign directory lands.
    for i in 0..n {
        assert!(lookup_opt(&routed, shared, &format!("one-{i}"))
            .await
            .is_some());
    }
    routed
        .create(shared, "after-flip", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create into a striped foreign directory");
    for _ in 0..400 {
        if !routed.stripe_map(shared).await.unwrap().unwrap().migrating {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(!routed.stripe_map(shared).await.unwrap().unwrap().migrating);
    let mut expect: Vec<String> = (0..n).map(|i| format!("one-{i}")).collect();
    expect.push("after-flip".to_string());
    expect.sort();
    assert_eq!(names_in(&routed, shared).await, expect);
    holders.tear_down();
    assert_closed("trigger");
    shutdown(&routed).await;
    drop(routed);
    fsck_clean_covered(&uris, Some(TWO_HOLDERS)).await;
}

/// A STRIPE never flips itself (review round 1, Issue 1 — the design's
/// own operating point: ≥ 2 creators shipping into one striped directory
/// past `N_floor` gave every stripe its own creator census, and the flip
/// of a stripe moved its names into nested stripes nobody routed to).
/// Three faces: the census hook skips a known stripe; a fresh mount that
/// knows nothing runs the belt (the reverse scan) and refuses; the
/// explicit flip of a stripe refuses. Every name stays resolvable, no
/// stripe carries a map, C17 clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stripe_never_flips_itself() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let plane = routed.volumes[0].slot_leases().expect("armed");
    let n_floor = plane.n_floor();
    routed
        .stripe_dir_with_suppliers(shared, 4, &[1])
        .await
        .expect("flip");
    wait_migrated(&routed, shared).await;
    let map = routed.stripe_map(shared).await.unwrap().unwrap();
    // Creates from appender 0 into the striped foreign directory: every
    // one a served insert whose PARENT is a stripe — the census must feed
    // nothing for them.
    let n = usize::try_from(n_floor).unwrap() * 2 + 8;
    let names = create_files(&routed, shared, "w-", n).await;
    let flips_before = stripe_stats().flips;
    for s in &map.stripes {
        assert!(
            routed.stripe_census(*s).is_empty(),
            "a stripe has no census"
        );
        // A second creator on a stripe's census (the old shape's trigger):
        // the known-stripe skip.
        for _ in 0..=n_floor {
            routed.note_served_insert(*s, "x", 7);
            routed.note_served_insert(*s, "y", 8);
        }
        assert!(routed.stripe_census(*s).is_empty());
        // The explicit flip of a stripe refuses.
        let e = routed
            .stripe_dir(*s, 4)
            .await
            .expect_err("a stripe cannot carry a map");
        assert!(e.to_string().contains("cannot carry a stripe map"), "{e}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(stripe_stats().flips, flips_before, "no stripe flipped");
    for s in &map.stripes {
        assert!(routed.stripe_map(*s).await.unwrap().is_none());
    }
    holders.tear_down();
    shutdown(&routed).await;
    drop(routed);

    // A fresh mount knows no stripe: the belt (the reverse scan) at the
    // spawned flip refuses and learns it; the census is dropped.
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1]).await;
    let flips_before = stripe_stats().flips;
    for s in &map.stripes {
        for _ in 0..=n_floor {
            routed.note_served_insert(*s, "x", 7);
            routed.note_served_insert(*s, "y", 8);
        }
    }
    for _ in 0..200 {
        if routed.stripe_work_in_flight() == 0
            && map
                .stripes
                .iter()
                .all(|s| routed.stripe_census(*s).is_empty())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        stripe_stats().flips,
        flips_before,
        "the belt refused every stripe flip"
    );
    for s in &map.stripes {
        assert!(routed.stripe_map(*s).await.unwrap().is_none());
        assert!(routed.stripe_census(*s).is_empty(), "the census is dropped");
    }
    assert_eq!(
        names_in(&routed, shared).await,
        names,
        "every name resolvable"
    );
    for nm in &names {
        assert!(lookup_opt(&routed, shared, nm).await.is_some());
    }
    holders.tear_down();
    assert_closed("stripe never flips");
    shutdown(&routed).await;
    fsck_clean_covered(&uris, Some(TWO_HOLDERS)).await;
}

/// `..` of a striped directory resolves to its PARENT on the reconnect
/// path whatever the key order (review round 1, Issue 3): the first build's
/// commit/migrating markers named the directory ITSELF, and the reverse
/// dentry scan's first hit was that self-reference whenever the directory's
/// key sorted below its parent's — `..` answered the directory. The markers
/// now name stripe 0; pinned with a striped directory whose parent sorts
/// ABOVE it, and with a child's `..` through a stripe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dotdot_of_a_striped_directory_resolves_to_its_parent_whatever_the_key_order() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    // Directories across the rotor: pick a pair `(d, p)` with `d`'s
    // (slot, local) key BELOW `p`'s, then move `d` under `p`.
    let mut made: Vec<u64> = Vec::new();
    for i in 0..12 {
        made.push(
            routed
                .create(work, &format!("c{i}"), libc::S_IFDIR | 0o755, 0, 0)
                .await
                .unwrap()
                .ino,
        );
    }
    let key = |ino: u64| {
        let (_, local) = routed.route_ino(ino);
        (slot_of(&routed, ino), local)
    };
    let mut sorted = made.clone();
    sorted.sort_by_key(|i| key(*i));
    let (d, p) = (sorted[0], sorted[sorted.len() - 1]);
    assert!(key(d) < key(p));
    let d_name = format!("c{}", made.iter().position(|i| *i == d).unwrap());
    routed
        .rename(work, &d_name, p, "d", 0)
        .await
        .expect("move d under p");
    routed.stripe_dir(d, 4).await.expect("flip");
    routed.migrate_dir(d).await.expect("migrate");
    let child = routed
        .create(d, "child", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    // `..` of the striped directory: its parent, never itself.
    assert_eq!(routed.lookup(d, "..").await.unwrap().ino, p);
    // `..` of a child named by a stripe: the directory (through the
    // stripe), never the stripe.
    assert_eq!(routed.lookup(child, "..").await.unwrap().ino, d);
    // No marker names the directory itself: the commit marker names
    // stripe 0 (the migrating one is gone after the migration).
    let map = routed.stripe_map(d).await.unwrap().unwrap();
    assert_eq!(
        raw_entry(&routed, d, STRIPED_MARKER).await,
        Some(map.stripes[0]),
        "the commit marker names stripe 0"
    );
    assert_eq!(raw_entry(&routed, d, MIGRATING_MARKER).await, None);
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The mutation paths are migration-safe (review round 1, Issue 4):
/// (a) an `unlink` of a name still in the directory's own tree re-homes it
/// and removes it — never a spurious `ENOENT` — while the background
/// migration runs beside it; (b) a `rename` onto an unmigrated destination
/// re-homes the destination first, so the overwritten inode is accounted
/// and the new name sits in its stripe (never stranded in the directory's
/// own tree); (c) the flag clear re-verifies the directory's own tree EMPTY
/// under its guard — a name inserted into it behind the scan is moved
/// before the flag goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutations_racing_the_migration_never_answer_enoent_or_strand_a_name() {
    use squeezefs::meta_backend::kv::backend::RoutedParentUpdate;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let hold = HoldMigration::arm();
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let names = create_files(&routed, work, "f-", 40).await;
    routed.stripe_dir(work, 8).await.expect("flip");
    let map = routed.stripe_map(work).await.unwrap().unwrap();
    assert!(map.migrating);
    let before = stripe_stats();

    // (a) unlink of an unmigrated name, racing the migration: both run
    // `migrate_one` under the same guards; the unlink lands on the stripe.
    drop(hold);
    let r = Arc::clone(&routed);
    let migration = tokio::spawn(async move { r.migrate_dir(work).await });
    for nm in names.iter().take(20) {
        routed
            .unlink(work, nm)
            .await
            .unwrap_or_else(|e| panic!("unlink {nm} racing the migration: {e}"));
    }
    migration.await.unwrap().expect("migration");
    let remaining: Vec<String> = names.iter().skip(20).cloned().collect();
    assert_eq!(names_in(&routed, work).await, remaining);

    // (b) a rename onto an UNMIGRATED destination: plant the destination
    // in the directory's own tree (the stale-route insert's shape) with
    // the flag set again by a second flip of a fresh directory.
    let hold = HoldMigration::arm();
    let d2 = routed
        .create(work, "d2", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let victim = routed
        .create(d2, "victim", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    routed
        .create(d2, "source", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    routed.stripe_dir(d2, 4).await.expect("flip d2");
    let m2 = routed.stripe_map(d2).await.unwrap().unwrap();
    assert!(m2.migrating);
    // Both names sit in d2's own tree: the rename overwrites `victim`.
    routed
        .rename(d2, "source", d2, "victim", 0)
        .await
        .expect("rename onto an unmigrated destination");
    let (vv, vlocal) = routed.route_ino(victim);
    assert!(
        routed.volumes[vv]
            .read_inode_value_routed(vlocal)
            .await
            .unwrap()
            .is_none_or(|r| r.nlink == 0),
        "the overwritten inode is accounted (nlink 0 / destroyed), never leaked"
    );
    let seed = hash_seed(&routed, d2);
    let (_, s) = m2.stripe_for(dentry_name_hash54(b"victim", seed));
    assert!(
        raw_names(&routed, s).await.contains(&"victim".to_string()),
        "the new name sits in its stripe"
    );
    let own: Vec<String> = raw_names(&routed, d2)
        .await
        .into_iter()
        .filter(|n| !is_marker_name(n))
        .collect();
    assert!(own.is_empty(), "nothing stranded in d2's own tree: {own:?}");
    assert!(stripe_stats().rehomed_on_touch > before.rehomed_on_touch);

    // (c) a name inserted into d2's OWN tree behind the migration's scan
    // (a stale route): the flag clear finds it under its guard and moves
    // it before clearing.
    let (d2v, d2local) = routed.route_ino(d2);
    let no_guards: Arc<[squeezefs::meta_backend::dlm::DlmGuard]> = Arc::from(Vec::new());
    let slot = routed.pick_mint_slot(0);
    let (ll, lg) = routed.allocate_local_ino_in_slot(0, slot).unwrap();
    routed.volumes[0]
        .routed_mint_inode(
            ll,
            libc::S_IFREG | 0o644,
            0,
            0,
            0,
            0,
            None,
            Arc::clone(&no_guards),
        )
        .await
        .unwrap();
    routed.volumes[d2v]
        .routed_add_dentry(
            d2local,
            "stale",
            lg,
            libc::S_IFREG,
            RoutedParentUpdate::None,
            no_guards,
        )
        .await
        .unwrap();
    drop(hold);
    routed.migrate_dir(d2).await.expect("migrate d2");
    let m2 = routed.stripe_map(d2).await.unwrap().unwrap();
    assert!(!m2.migrating, "the flag cleared");
    assert_names_route_to_hash_mod_k(&routed, &m2, &["stale".to_string(), "victim".to_string()])
        .await;
    assert_eq!(lookup_opt(&routed, d2, "stale").await, Some(lg));
    assert_closed("migration-safe mutations");
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// A PRESET (delegated) create into a STRIPED parent is refused loud
/// (review round 1, Issue 20a): the S10 delegation plane names the
/// directory while the stripe is the key parent — the two do not compose,
/// and a silent insert into the directory's own tree is exactly the
/// stranded shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preset_create_into_a_striped_parent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    routed.stripe_dir(work, 4).await.expect("flip");
    wait_migrated(&routed, work).await;
    let slot = routed.pick_mint_slot(0);
    let (_, global) = routed.allocate_local_ino_in_slot(0, slot).unwrap();
    let e = routed
        .create_with_rdev_preset(
            work,
            "preset",
            libc::S_IFREG | 0o644,
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
        .expect_err("refused");
    assert!(e.to_string().contains("STRIPED"), "{e}");
    assert_eq!(lookup_opt(&routed, work, "preset").await, None);
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The shutdown fixpoint over an EXHAUSTED grant (review round 1, Issue 21a
/// — the wave's 2-in-9 red): a declared region whose slot tree took a burst
/// of inserts under a parked cadence needs its compactions' extents at the
/// final cycles; the flush pass's reactive refill must be ADMITTED during
/// the shutdown (the INTERNAL class, bounded by the SMO's own need) or the
/// pass defers every SMO, the fixpoint never converges and the leave keeps
/// the region's page `Live` over an uncovered ring — acked records a probe
/// cannot see. Pinned: `manager_dependency_stalls == 0`, and the next
/// writer open finds NO own residue (`appender_self_recoveries == 0`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_over_an_exhausted_grant_refills_and_leaves_covered() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    // The cadence parked: every SMO of appender 1's tree is the shutdown
    // fixpoint's; the grant at its floor.
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    std::env::set_var("SQUEEZEFS_SYM_GRANT_EXTENTS", "8");
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let holders = Holders::stand_up(&routed, &[1]).await;
    // A burst of inserts into the region's slot tree — every create a
    // shipped `InsertDentry` whose dentry lands in appender 1's tree.
    let names = create_files(&routed, shared, "b-", 2_000).await;
    holders.tear_down();
    shutdown(&routed).await;
    drop(routed);
    std::env::remove_var("SQUEEZEFS_SYM_GRANT_EXTENTS");
    // The next open: no own residue (the leave covered), no stall.
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    let s = routed.volumes[0].appender_stats().expect("forest");
    assert_eq!(
        s.self_recoveries, 0,
        "the leave left an uncovered ring — own residue replayed: {s:?}"
    );
    assert_eq!(s.dependency_stalls, 0, "{s:?}");
    assert_eq!(names_in(&routed, shared).await, names);
    shutdown(&routed).await;
    drop(routed);
    fsck_clean(&uris).await;
}

/// The offline probe over a `Live` appender page records NO inode-plane
/// verdict (review round 1, Issue 21b): a writer that died with a declared
/// region left both rings' windows uncovered — acked names durable there
/// and replayed by nobody but a writer's own-residue open — so a census
/// over the trees alone would read every such child as C9-unreferenced.
/// The probe's inode plane covers 0 volumes and finds nothing; the
/// writer's next clean open + leave makes the census whole (covered 1,
/// clean).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_probe_over_a_live_page_records_no_inode_plane_verdict() {
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B]).await;
    let shared = dirs[0];
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let holders = Holders::stand_up(&routed, &[1]).await;
    let names = create_files(&routed, shared, "c-", 50).await;
    for v in &routed.volumes {
        v.sync_device().await.unwrap();
    }
    holders.tear_down();
    // The crash-equivalent: no shutdown, every page stays `Live`. (The
    // offline CLI's live-client preflight reads this process's own claim
    // as a live writer — the pid IS alive — so the engine is driven over
    // a read-only PROBE directly, the harness `run_offline` builds.)
    drop(routed);
    let probe = squeezefs::meta_backend::open_probe_routed_meta_set(&uris)
        .await
        .expect("a probe over a dead writer's pages");
    assert!(
        probe.volumes[0]
            .appender_stats()
            .expect("forest")
            .live_pages_at_mount
            >= 1,
        "the dead writer's pages read Live"
    );
    let report = probe_inode_plane(&probe).await;
    for v in &probe.volumes {
        v.shutdown().await.unwrap();
    }
    drop(probe);
    assert_eq!(
        report.counters.inode_plane_volumes_covered, 0,
        "no inode-plane verdict over an uncovered window: {:?}",
        report.findings
    );
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.class == "C9" || f.class == "C10" || f.class == "C17"),
        "no C9/C10/C17 verdict from an incomplete census: {:?}",
        report.findings
    );
    // The writer's own-residue open + clean leave: the census is whole.
    let routed = open_under(&uris, true, Some(TWO_HOLDERS)).await;
    assert!(routed.volumes[0].appender_stats().unwrap().self_recoveries >= 1);
    assert_eq!(names_in(&routed, shared).await, names);
    for nm in &names {
        routed
            .lookup(shared, nm)
            .await
            .unwrap_or_else(|e| panic!("{nm}: the replayed child resolves: {e}"));
    }
    shutdown(&routed).await;
    drop(routed);
    fsck_clean(&uris).await;
}

/// The 12,500-creator wave SIMULATED locally: `N` declared holders each
/// supply one stripe (the remainder the holder's), `W` creates from
/// appender 0 route by hash — every create into a foreign stripe is ONE
/// shipped insert (`dir_stripe_ships ≡ foreign creates`), the population
/// spreads over the stripes (max ≤ 2 × the mean), no handover fires, the
/// ledger closes and fsck is clean. `N = 7` here — the declared-partition
/// seam's page budget at 64 KiB nodes (7 ids per directory extent; the
/// chain's growth is the manager's `JoinAppender`, PR 3 — a wire join,
/// not a declaration); `SQZ_STRIPE_WAVE_HOLDERS=31` with
/// `SQZ_STRIPE_WAVE_NODE_KIB=256` is the wider leg the note's row runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_creator_wave_spreads_over_the_stripe_holders() {
    let _ = env_logger::builder().is_test(true).try_init();
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let holders_n: u32 = std::env::var("SQZ_STRIPE_WAVE_HOLDERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);
    let creates: usize = std::env::var("SQZ_STRIPE_WAVE_CREATES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let k = STRIPES_MAX.min(holders_n as u16 * 2);
    // Declared regions 1..=N on forest slots 4, 6, 8, …
    let slots: Vec<ForestSlot> = (0..holders_n).map(|i| 4 + 2 * i).collect();
    let partition = (1..=holders_n)
        .zip(&slots)
        .map(|(id, s)| format!("{id}:{s}"))
        .collect::<Vec<_>>()
        .join(";");
    let (uris, dirs) = seeded_volume(dir.path(), &slots).await;
    let shared = dirs[0];
    let partition: &'static str = Box::leak(partition.into_boxed_str());
    let routed = open_under(&uris, true, Some(partition)).await;
    let appenders: Vec<u32> = (1..=holders_n).collect();
    let holders = Holders::stand_up(&routed, &appenders).await;

    routed
        .stripe_dir_with_suppliers(shared, k, &appenders)
        .await
        .expect("flip with the holders as suppliers");
    let map = routed.stripe_map(shared).await.unwrap().unwrap();
    assert_eq!(map.k(), k);
    let mut supplied = std::collections::BTreeSet::new();
    for s in map.stripes.iter().take(holders_n as usize) {
        let h = routed.volumes[0]
            .slot_leases()
            .unwrap()
            .holders
            .holder(slot_of(&routed, *s))
            .map(|h| h.appender_id);
        supplied.insert(h);
    }
    assert_eq!(
        supplied.len(),
        holders_n as usize,
        "the first N stripes live in N distinct suppliers' slots"
    );
    wait_migrated(&routed, shared).await;
    // The wave's own ledger: after the flip's marker steps and the flag
    // clear (shipped to the directory's holder) have landed.
    let before = stripe_stats();
    let before_xo = cross_owner_stats();

    let t0 = std::time::Instant::now();
    for i in 0..creates {
        routed
            .create(shared, &format!("job-{i:06}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("mkdir /jobs/X shape");
    }
    let wall = t0.elapsed();
    let after = stripe_stats();
    let ships = after.ships - before.ships;
    // Per-stripe population = the ships each holder served.
    let mut per_stripe = Vec::new();
    let mut own = 0usize;
    for s in &map.stripes {
        let n = raw_names(&routed, *s).await.len();
        per_stripe.push(n);
        if routed.volumes[0]
            .slot_leases()
            .unwrap()
            .holders
            .holder(slot_of(&routed, *s))
            .map(|h| h.appender_id)
            == Some(0)
        {
            own += n;
        }
    }
    assert_eq!(per_stripe.iter().sum::<usize>(), creates);
    assert_eq!(
        ships as usize,
        creates - own,
        "dir_stripe_ships ≡ foreign creates"
    );
    let mean = creates as f64 / f64::from(k);
    let max = *per_stripe.iter().max().unwrap();
    assert!(
        (max as f64) <= 2.0 * mean,
        "the spread is the verdict: max {max} vs mean {mean:.1} — {per_stripe:?}"
    );
    eprintln!(
        "WAVE N={holders_n} K={k} creates={creates}: ships={ships} own={own} wall={wall:?} \
         per-stripe min={} max={max} mean={mean:.1}",
        per_stripe.iter().min().unwrap()
    );
    assert_eq!(
        routed.volumes[0]
            .slot_leases()
            .unwrap()
            .handovers
            .load(Ordering::Relaxed),
        0,
        "aggregate shipping never triggers a handover"
    );
    let after_xo = cross_owner_stats();
    assert_eq!(after_xo.steps_shipped - before_xo.steps_shipped, ships);
    assert_eq!(names_in(&routed, shared).await.len(), creates);
    holders.tear_down();
    assert_closed("wave");
    shutdown(&routed).await;
    drop(routed);
    // The oracle: the leave COVERED (no own residue at the next open),
    // then the census (Issue 21c).
    fsck_clean_covered(&uris, Some(partition)).await;
}

/// Three holders: the directory's holder is a declared region, two
/// stripes are supplied by two other appenders (one declared, one this
/// mount) — a create into a foreign stripe ships to ITS holder, not the
/// directory's; `IsEmpty`/`DestroyStripe` travel the same wire at rmdir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stripes_supplied_by_other_appenders_are_served_by_their_own_holders() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, dirs) = seeded_volume(dir.path(), &[SLOT_B, SLOT_C]).await;
    let shared = dirs[0];
    let routed = open_under(&uris, true, Some(THREE_HOLDERS)).await;
    let holders = Holders::stand_up(&routed, &[1, 2]).await;
    routed
        .stripe_dir_with_suppliers(shared, 4, &[2, 0])
        .await
        .expect("flip");
    let map = routed.stripe_map(shared).await.unwrap().unwrap();
    let plane = routed.volumes[0].slot_leases().unwrap();
    let holder_of = |s: u64| {
        plane
            .holders
            .holder(slot_of(&routed, s))
            .map(|h| h.appender_id)
    };
    assert_eq!(
        holder_of(map.stripes[0]),
        Some(2),
        "stripe 0 supplied by appender 2"
    );
    assert_eq!(
        holder_of(map.stripes[1]),
        Some(0),
        "stripe 1 supplied by this mount"
    );
    assert_eq!(
        holder_of(map.stripes[2]),
        Some(1),
        "the remainder is the holder's"
    );
    wait_migrated(&routed, shared).await;
    let before = stripe_stats();
    let before_xo = cross_owner_stats();
    let names = create_files(&routed, shared, "x-", 40).await;
    assert_names_route_to_hash_mod_k(&routed, &map, &names).await;
    let after = stripe_stats();
    let after_xo = cross_owner_stats();
    let own: usize = raw_names(&routed, map.stripes[1]).await.len();
    assert_eq!(after.ships - before.ships, 40 - own as u64);
    assert_eq!(
        after_xo.steps_shipped - before_xo.steps_shipped,
        40 - own as u64
    );
    for n in &names {
        routed
            .unlink(shared, n)
            .await
            .expect("unlink through the stripe");
    }
    let dying_before = stripe_stats().ships;
    routed
        .unlink(ROOT_INO, "shared0")
        .await
        .expect("rmdir across three holders");
    assert!(
        stripe_stats().ships > dying_before,
        "IsEmpty / DestroyStripe shipped"
    );
    for s in &map.stripes {
        let (v, local) = routed.route_ino(*s);
        assert!(routed.volumes[v]
            .read_inode_value_routed(local)
            .await
            .unwrap()
            .is_none());
    }
    holders.tear_down();
    assert_closed("three holders");
    shutdown(&routed).await;
    drop(routed);
    fsck_clean_covered(&uris, Some(THREE_HOLDERS)).await;
}

/// The evidence note's scoping instrument (SCOPING — the dev laptop; run
/// with `--ignored --nocapture`, `cargo test --release` for the numbers):
/// a solo armed volume, `work` with 2,000 names; the FLIP's wall (the K
/// local mints + the ONE intent of `K + 2` marker steps), the background
/// MIGRATION's wall (2,000 moved names, one intent each), `readdir` of
/// the whole directory flat (one stream) vs striped (the K-way merge —
/// the `ls` row: a token reader's cost is K + 1 grants on first touch,
/// then 0 RPCs, PR 5's law), and `lookup` per name before and after.
/// Then the holders' model: a flip whose K stripes are SUPPLIED by N = 7
/// declared appenders (`dir_stripe_supply_rpcs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "scoping instrument — the evidence note's rows"]
async fn scoping_row_flip_migration_and_readdir_cost() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    const NAMES: usize = 2_000;
    const K: u16 = 14;
    let names = create_files(&routed, work, "f-", NAMES).await;

    let t = std::time::Instant::now();
    let flat_list = names_in(&routed, work).await;
    let flat_readdir = t.elapsed();
    assert_eq!(flat_list, names);
    let t = std::time::Instant::now();
    for n in &names {
        lookup_opt(&routed, work, n).await.expect("resolves");
    }
    let flat_lookup = t.elapsed();

    let before = stripe_stats();
    let t = std::time::Instant::now();
    routed.stripe_dir(work, K).await.expect("flip");
    let flip = t.elapsed();
    let t = std::time::Instant::now();
    wait_migrated(&routed, work).await;
    let migration = t.elapsed();
    let after = stripe_stats();
    assert_eq!(after.flips - before.flips, 1);
    assert_eq!(after.migrated_names - before.migrated_names, NAMES as u64);

    let t = std::time::Instant::now();
    let striped_list = names_in(&routed, work).await;
    let striped_readdir = t.elapsed();
    assert_eq!(striped_list, names);
    let t = std::time::Instant::now();
    for n in &names {
        lookup_opt(&routed, work, n).await.expect("resolves");
    }
    let striped_lookup = t.elapsed();
    let map = routed.stripe_map(work).await.unwrap().unwrap();
    assert_names_route_to_hash_mod_k(&routed, &map, &names).await;
    eprintln!(
        "SCOPING solo K={K} names={NAMES}: flip={flip:?} migration={migration:?} \
         ({:.1} µs/name) readdir flat={flat_readdir:?} striped={striped_readdir:?} \
         lookup flat={:.1} µs/name striped={:.1} µs/name",
        migration.as_secs_f64() * 1e6 / NAMES as f64,
        flat_lookup.as_secs_f64() * 1e6 / NAMES as f64,
        striped_lookup.as_secs_f64() * 1e6 / NAMES as f64,
    );
    shutdown(&routed).await;
    fsck_clean(&uris).await;

    // The holders' model: N = 7 suppliers, one stripe each + the remainder.
    let dir2 = tempfile::tempdir().unwrap();
    let holders_n: u32 = 7;
    let slots: Vec<ForestSlot> = (0..holders_n).map(|i| 4 + 2 * i).collect();
    let partition = (1..=holders_n)
        .zip(&slots)
        .map(|(id, s)| format!("{id}:{s}"))
        .collect::<Vec<_>>()
        .join(";");
    let (uris, dirs) = seeded_volume(dir2.path(), &slots).await;
    let shared = dirs[0];
    let partition: &'static str = Box::leak(partition.into_boxed_str());
    let routed = open_under(&uris, true, Some(partition)).await;
    let appenders: Vec<u32> = (1..=holders_n).collect();
    let holders = Holders::stand_up(&routed, &appenders).await;
    let before = stripe_stats();
    let before_xo = cross_owner_stats();
    let t = std::time::Instant::now();
    routed
        .stripe_dir_with_suppliers(shared, K, &appenders)
        .await
        .expect("flip with the holders as suppliers");
    let flip = t.elapsed();
    wait_migrated(&routed, shared).await;
    let after = stripe_stats();
    let after_xo = cross_owner_stats();
    eprintln!(
        "SCOPING holders N={holders_n} K={K}: flip={flip:?} supply_rpcs={} \
         steps_shipped={} intents={}",
        after.supply_rpcs - before.supply_rpcs,
        after_xo.steps_shipped - before_xo.steps_shipped,
        after_xo.intents_minted - before_xo.intents_minted,
    );
    holders.tear_down();
    assert_closed("scoping holders");
    shutdown(&routed).await;
    drop(routed);
    fsck_clean_covered(&uris, Some(partition)).await;
}

// ---------------------------------------------------------------------------
// The off postures.
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_SYM_DIR_STRIPES=1` is striping OFF: the explicit flip
/// refuses, the mkdir arm flips nothing, the trigger fires nothing —
/// and a directory striped BEFORE the knob still routes by its map.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_knob_at_one_never_stripes_and_still_routes_an_existing_map() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    routed
        .stripe_dir(work, 4)
        .await
        .expect("flip before the knob");
    routed.migrate_dir(work).await.unwrap();
    let names = create_files(&routed, work, "k-", 6).await;
    std::env::set_var(DIR_STRIPES_ENV, "1");
    let before = stripe_stats();
    let plain = routed
        .create(ROOT_INO, "plain", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    assert!(
        routed.stripe_dir(plain, 8).await.is_err(),
        "the explicit flip refuses"
    );
    routed.set_stripe_dirs_at_mkdir(true);
    let d2 = routed
        .create(ROOT_INO, "d2", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    assert!(
        routed.stripe_map(d2).await.unwrap().is_none(),
        "the mkdir arm flips nothing"
    );
    routed.set_stripe_dirs_at_mkdir(false);
    for _ in 0..100 {
        routed.note_served_insert(plain, "x", 5);
        routed.note_served_insert(plain, "y", 6);
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        routed.stripe_map(plain).await.unwrap().is_none(),
        "the trigger fires nothing"
    );
    assert_eq!(stripe_stats().flips, before.flips);
    // The map written before the knob still routes.
    assert_eq!(names_in(&routed, work).await, names);
    routed
        .create(work, "still", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a create into an already-striped directory routes by its map");
    assert!(lookup_opt(&routed, work, "still").await.is_some());
    std::env::remove_var(DIR_STRIPES_ENV);
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// `-o stripe_dirs`: a `mkdir` under the option stripes at creation with
/// the knob's `K`; a subdirectory of a striped directory stripes too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stripe_dirs_at_mkdir_stripes_every_new_directory() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    routed.set_stripe_dirs_at_mkdir(true);
    let d = routed
        .create(work, "jobs", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let map = routed
        .stripe_map(d)
        .await
        .unwrap()
        .expect("striped at mkdir");
    assert_eq!(map.k(), dir_stripe::stripe_count());
    let sub = routed
        .create(d, "job-1", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    assert!(routed.stripe_map(sub).await.unwrap().is_some());
    assert_eq!(lookup_opt(&routed, d, "job-1").await, Some(sub));
    assert_eq!(routed.lookup(sub, "..").await.unwrap().ino, d);
    routed.set_stripe_dirs_at_mkdir(false);
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// The shipped postures: `SQUEEZEFS_SYMMETRIC_META=0` on a bit-17 volume
/// and a flat volume carry no striping — the flip refuses, `readdir`
/// takes the volume's page verbatim, every gauge is 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_unarmed_and_flat_paths_stripe_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let before = stripe_stats();
    for stamped in [true, false] {
        let sub = dir.path().join(if stamped { "stamped" } else { "flat" });
        std::fs::create_dir_all(&sub).unwrap();
        let uris = vec![format_member(&sub, "meta0", stamped).await];
        let routed = open_under(&uris, false, None).await;
        let work = routed
            .create(ROOT_INO, "work", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        let names = create_files(&routed, work, "u-", 5).await;
        assert!(
            routed.stripe_dir(work, 8).await.is_err(),
            "no plane, no flip"
        );
        assert!(routed.stripe_map(work).await.unwrap().is_none());
        routed.set_stripe_dirs_at_mkdir(true);
        let d = routed
            .create(work, "d", libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        assert!(routed.stripe_map(d).await.unwrap().is_none());
        routed.set_stripe_dirs_at_mkdir(false);
        let mut expect = names.clone();
        expect.push("d".to_string());
        expect.sort();
        assert_eq!(names_in(&routed, work).await, expect);
        assert_eq!(raw_names(&routed, work).await, expect, "no marker anywhere");
        shutdown(&routed).await;
        fsck_clean(&uris).await;
    }
    assert_eq!(stripe_stats(), before, "every Striping gauge unmoved");
}

/// The family is exported under its published names, 0 unarmed.
#[test]
fn the_striping_family_is_exported_under_its_published_names() {
    let m = dir_stripe::stats_json();
    for k in [
        "dir_striped_dirs",
        "dir_stripe_flips",
        "dir_stripe_supply_rpcs",
        "dir_stripe_ships",
        "dir_stripe_readdir_merges",
        "dir_stripe_migrated_names",
        "dir_stripe_time_batches",
        "dir_stripe_dying_refusals",
        "dir_stripe_rmdirs",
        "dir_stripe_rehomed_on_touch",
    ] {
        assert!(m.contains_key(k), "{k} exported");
    }
    assert_eq!(
        crossvol_tx::XV_MAX_STEPS,
        2 * squeezefs::meta_backend::MINT_SPREAD + 8,
        "the widest plan: the one-intent rmdir of a K = MINT_SPREAD directory"
    );
    assert_eq!(
        usize::from(STRIPES_MAX),
        squeezefs::meta_backend::MINT_SPREAD
    );
}

// ---------------------------------------------------------------------------
// fsck C17 — stripe consistency (§5.6.5 / §5.8.5): a healthy striped tree
// is clean (every contract above ends in `fsck_clean`); each of the five
// planted shapes is FOUND, report-only.
// ---------------------------------------------------------------------------

/// Plant each C17 shape on a settled striped tree — a stripe named by a
/// second directory's map, a map entry naming a destroyed stripe, a name in
/// the wrong stripe, a name left in the directory's own tree after its
/// migration, a commit marker naming the directory itself — and the
/// offline fsck reports exactly those five, report-only
/// (`fsck_repair_classC17` does not exist; repair refuses them).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_planted_c17_shape_is_found_and_nothing_else() {
    use squeezefs::meta_backend::kv::backend::RoutedParentUpdate;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, routed, work) = solo_armed(dir.path()).await;
    let seed = hash_seed(&routed, work);
    // Two striped directories, settled.
    let d1 = routed
        .create(work, "d1", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let d2 = routed
        .create(work, "d2", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    routed.stripe_dir(d1, 4).await.unwrap();
    routed.stripe_dir(d2, 4).await.unwrap();
    wait_migrated(&routed, d1).await;
    wait_migrated(&routed, d2).await;
    create_files(&routed, d1, "ok-", 6).await;
    let m1 = routed.stripe_map(d1).await.unwrap().unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    let no_guards: Arc<[squeezefs::meta_backend::dlm::DlmGuard]> = Arc::from(Vec::new());
    let mint = |routed: &RoutedMetaBackend| {
        let slot = routed.pick_mint_slot(0);
        routed.allocate_local_ino_in_slot(0, slot).unwrap()
    };

    // (1) multiply-mapped: d2's map gains a 5th entry naming d1's stripe 0.
    let (_, d2_local) = routed.route_ino(d2);
    vol.routed_add_dentry(
        d2_local,
        &dir_stripe::stripe_marker_name(4),
        m1.stripes[0],
        libc::S_IFDIR,
        RoutedParentUpdate::None,
        Arc::clone(&no_guards),
    )
    .await
    .unwrap();
    // (2) missing stripe: d1's stripe 3 record destroyed under its map.
    let (_, s3_local) = routed.route_ino(m1.stripes[3]);
    vol.destroy_unreferenced_inodes(&[s3_local]).await.unwrap();
    // (3) misrouted: a fresh file named in the WRONG stripe of d1.
    let (wrong_local, wrong_global) = mint(&routed);
    vol.routed_mint_inode(
        wrong_local,
        libc::S_IFREG | 0o644,
        0,
        0,
        0,
        0,
        None,
        Arc::clone(&no_guards),
    )
    .await
    .unwrap();
    let (i_right, _) = m1.stripe_for(dentry_name_hash54(b"misrouted", seed));
    let wrong_stripe = m1.stripes[usize::from((i_right + 1) % 3)];
    let (_, ws_local) = routed.route_ino(wrong_stripe);
    vol.routed_add_dentry(
        ws_local,
        "misrouted",
        wrong_global,
        libc::S_IFREG,
        RoutedParentUpdate::None,
        Arc::clone(&no_guards),
    )
    .await
    .unwrap();
    // (4) unmigrated: a fresh file named in d1's OWN tree, flag long gone.
    let (left_local, left_global) = mint(&routed);
    vol.routed_mint_inode(
        left_local,
        libc::S_IFREG | 0o644,
        0,
        0,
        0,
        0,
        None,
        Arc::clone(&no_guards),
    )
    .await
    .unwrap();
    let (_, d1_local) = routed.route_ino(d1);
    vol.routed_add_dentry(
        d1_local,
        "leftover",
        left_global,
        libc::S_IFREG,
        RoutedParentUpdate::None,
        Arc::clone(&no_guards),
    )
    .await
    .unwrap();
    // (5) incomplete map: d3 carries a commit marker naming ITSELF over
    // two stripe entries — the first build's self-reference (Issue 3);
    // the router ignores such a map and the census reports it.
    let d3 = routed
        .create(work, "d3", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let (_, d3_local) = routed.route_ino(d3);
    for i in 0..2u16 {
        let (sl, sg) = mint(&routed);
        vol.routed_mint_inode(
            sl,
            libc::S_IFDIR | 0o755,
            0,
            0,
            0,
            0,
            None,
            Arc::clone(&no_guards),
        )
        .await
        .unwrap();
        vol.routed_add_dentry(
            d3_local,
            &dir_stripe::stripe_marker_name(i),
            sg,
            libc::S_IFDIR,
            RoutedParentUpdate::None,
            Arc::clone(&no_guards),
        )
        .await
        .unwrap();
    }
    vol.routed_add_dentry(
        d3_local,
        STRIPED_MARKER,
        d3,
        libc::S_IFDIR,
        RoutedParentUpdate::None,
        Arc::clone(&no_guards),
    )
    .await
    .unwrap();
    assert!(
        routed.stripe_map(d3).await.unwrap().is_none(),
        "a commit marker naming the directory is no map"
    );
    shutdown(&routed).await;
    drop(vol);
    drop(routed);

    let mut opts = squeezefs::fsck::FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(10);
    let report = squeezefs::fsck::run_offline(&uris, &opts)
        .await
        .expect("offline fsck runs");
    let c17: Vec<&squeezefs::fsck::FsckFinding> = report
        .findings
        .iter()
        .filter(|f| f.class == "C17")
        .collect();
    let ids: Vec<(u64, u64, String, String)> = c17
        .iter()
        .filter_map(|f| match &f.identity {
            Some(squeezefs::fsck::FindingId::C17StripeInconsistency {
                dir,
                stripe,
                shape,
                name,
            }) => Some((*dir, *stripe, shape.clone(), name.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids.len(),
        c17.len(),
        "every C17 finding carries its identity"
    );
    let has = |shape: &str, dir: u64, name: &str| {
        ids.iter()
            .filter(|(d, _, s, n)| s == shape && *d == dir && n == name)
            .count()
    };
    assert_eq!(has("multiply-mapped", d1, ""), 1, "{ids:?}");
    assert_eq!(has("missing-stripe", d1, ""), 1, "{ids:?}");
    assert_eq!(has("misrouted-name", d1, "misrouted"), 1, "{ids:?}");
    assert_eq!(has("unmigrated-name", d1, "leftover"), 1, "{ids:?}");
    assert_eq!(has("incomplete-map", d3, ""), 1, "{ids:?}");
    // The multiply-mapped stripe's CONSEQUENCE: read as d2's fifth stripe,
    // d1's names in it route elsewhere under K = 5 — every remaining
    // finding is that shape on d2, and nothing else.
    for (d, _, shape, name) in &ids {
        let planted = (*d == d1 && matches!(shape.as_str(), "multiply-mapped" | "missing-stripe"))
            || (*d == d1 && shape == "misrouted-name" && name == "misrouted")
            || (*d == d1 && shape == "unmigrated-name" && name == "leftover")
            || (*d == d3 && shape == "incomplete-map");
        assert!(
            planted || (*d == d2 && shape == "misrouted-name"),
            "unexpected C17 finding: {d} {shape} {name:?} — {ids:?}"
        );
    }
    assert_eq!(report.counters.stripe_findings, c17.len() as u64);
    for f in &c17 {
        assert!(f.evidence.contains("REPORT-ONLY"), "{}", f.evidence);
    }
    // Nothing else — except the ONE face another class legitimately
    // shares: the destroyed stripe's map entry is a dentry naming an ino
    // with no record, C10's dangling-dentry shape (the inode plane sees
    // the same defect from its side).
    let others: Vec<&squeezefs::fsck::FsckFinding> = report
        .findings
        .iter()
        .filter(|f| f.class != "C17")
        .collect();
    assert_eq!(
        others.len(),
        1,
        "only C17 and the missing stripe's C10 face fire: {:?}",
        others
            .iter()
            .map(|f| (&f.class, &f.evidence))
            .collect::<Vec<_>>()
    );
    assert_eq!(others[0].class, "C10");
    assert!(others[0]
        .evidence
        .contains(&dir_stripe::stripe_marker_name(3)));
}
