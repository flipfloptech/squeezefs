//! The armed-forest harness (the `sym_slot_transfer_tests` shape: 64 KiB
//! nodes, a 1 MiB fixed ring, one stamped member, the plane armed through
//! the registered knob and the non-PR lab opt-in). Every knob is
//! process-global — a suite serializes its tests on [`SEAM`].
//!
//! `allow(dead_code)` is the SHARED-FIXTURE exception to the repo's
//! dead-code law (the only one under `tests/`): every consumer binary
//! compiles this module whole and uses a subset, so the lint fires per
//! binary on items another binary needs. Nothing here is unused by the
//! set of its consumers; a fixture no suite calls is deleted, not kept.

#![allow(dead_code)]

use squeezefs::meta_backend::kv::appender::TEST_APPENDER_SLOTS_ENV;
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::record::{forest_slot_of_ino, ForestSlot};
use squeezefs::meta_backend::kv::slot_lease::{
    SYMMETRIC_META_ENV, SYM_AFFINITY_MAX_MB_ENV, SYM_MINT_SLOTS_ENV, SYM_T_IDLE_MS_ENV,
};
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, RoutedMetaBackend};
use std::sync::Arc;

pub const VOL_LEN: u64 = 64 * 1024 * 1024;
pub const NODE_SIZE: usize = 64 * 1024;
pub const RING_LEN: u64 = 1024 * 1024;

/// The seams and knobs are process-global; every test serializes on it.
pub static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// One member formatted under the bit-17 seam.
pub async fn format_stamped_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped member");
    p.display().to_string()
}

/// A SET of `names.len()` members formatted under the bit-17 seam (one
/// plan, one stamp per member — the routed open needs the members to
/// agree on the slot map).
pub async fn format_stamped_set(dir: &std::path::Path, names: &[&str]) -> Vec<String> {
    let plan = plan_meta_slot_set(names.len()).expect("derived plan");
    let mut uris = Vec::with_capacity(names.len());
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    for (i, name) in names.iter().enumerate() {
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[i].clone()).await;
        r.expect("format stamped set member");
        uris.push(p.display().to_string());
    }
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    uris
}

/// One member formatted FLAT (the seam cleared).
pub async fn format_flat_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("format flat member");
    p.display().to_string()
}

/// The knobs a mount reads at open, restored after it.
pub struct Knobs {
    pub armed: bool,
    pub partition: Option<&'static str>,
    pub mint_slots: Option<&'static str>,
    pub affinity_mb: Option<&'static str>,
    pub t_idle_ms: Option<&'static str>,
}

impl Knobs {
    pub fn armed() -> Self {
        Self {
            armed: true,
            partition: None,
            mint_slots: None,
            affinity_mb: None,
            t_idle_ms: None,
        }
    }
    pub fn unarmed() -> Self {
        Self {
            armed: false,
            ..Self::armed()
        }
    }
    pub fn partition(mut self, p: &'static str) -> Self {
        self.partition = Some(p);
        self
    }
    pub fn mint_slots(mut self, m: &'static str) -> Self {
        self.mint_slots = Some(m);
        self
    }
    pub fn affinity_mb(mut self, mb: &'static str) -> Self {
        self.affinity_mb = Some(mb);
        self
    }
    pub fn t_idle_ms(mut self, ms: &'static str) -> Self {
        self.t_idle_ms = Some(ms);
        self
    }
    pub fn apply(&self) {
        if self.armed {
            std::env::set_var(SYMMETRIC_META_ENV, "1");
        } else {
            std::env::remove_var(SYMMETRIC_META_ENV);
        }
        std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
        match self.partition {
            Some(p) => std::env::set_var(TEST_APPENDER_SLOTS_ENV, p),
            None => std::env::remove_var(TEST_APPENDER_SLOTS_ENV),
        }
        match self.mint_slots {
            Some(m) => std::env::set_var(SYM_MINT_SLOTS_ENV, m),
            None => std::env::remove_var(SYM_MINT_SLOTS_ENV),
        }
        match self.affinity_mb {
            Some(mb) => std::env::set_var(SYM_AFFINITY_MAX_MB_ENV, mb),
            None => std::env::remove_var(SYM_AFFINITY_MAX_MB_ENV),
        }
        match self.t_idle_ms {
            Some(ms) => std::env::set_var(SYM_T_IDLE_MS_ENV, ms),
            None => std::env::remove_var(SYM_T_IDLE_MS_ENV),
        }
    }
    pub fn clear() {
        for k in [
            SYMMETRIC_META_ENV,
            "SQUEEZEFS_SYM_ALLOW_NON_PR",
            TEST_APPENDER_SLOTS_ENV,
            SYM_MINT_SLOTS_ENV,
            SYM_AFFINITY_MAX_MB_ENV,
            SYM_T_IDLE_MS_ENV,
        ] {
            std::env::remove_var(k);
        }
    }
}

/// Open the set under `knobs`; the knobs are cleared after the open (a
/// mount reads them once, at open — the plane keeps them).
pub async fn open_under(uris: &[String], knobs: &Knobs) -> Arc<RoutedMetaBackend> {
    knobs.apply();
    let r = open_routed_meta_set(uris).await;
    Knobs::clear();
    r.expect("open routed set")
}

/// [`open_under`] after a DROP without shutdown (the in-process kill −9):
/// a previous incarnation's detached tasks may pin the writer flock for
/// a few ms after the `Arc` went, so the open is retried (the S3.5 and
/// PR 6 suites' harness plumbing). Returns the error text of the last
/// refusal when every attempt fails.
pub async fn open_under_retry(
    uris: &[String],
    knobs: &Knobs,
) -> Result<Arc<RoutedMetaBackend>, String> {
    let mut last = String::new();
    for _ in 0..200 {
        knobs.apply();
        let r = open_routed_meta_set(uris).await;
        Knobs::clear();
        match r {
            Ok(r) => return Ok(r),
            Err(e) => {
                last = format!("{e}");
                if !last.contains("writer lock") && !last.contains("flock") {
                    return Err(last);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    Err(last)
}

/// The volume-set format config a member records (what the offline fsck
/// harness reads back to build its router), naming one file-backed data
/// volume under `dir` (PR 6's fixture).
pub fn format_config_for(dir: &std::path::Path) -> Vec<u8> {
    let oss = dir.join("oss0");
    if !oss.exists() {
        std::fs::File::create(&oss)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
    }
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

/// A stamped SET of `n` members carrying the format config on volume 0
/// (the offline fsck's input), `names` = `meta0..meta{n-1}`.
pub async fn format_stamped_set_with_config(dir: &std::path::Path, n: usize) -> Vec<String> {
    format_stamped_set_with_config_len(dir, n, VOL_LEN).await
}

/// [`format_stamped_set_with_config`] at a chosen member length (the
/// appender capacity is `heap/16 ÷ ring` — a wider fleet needs a wider
/// volume, never a format-time client count).
pub async fn format_stamped_set_with_config_len(
    dir: &std::path::Path,
    n: usize,
    len: u64,
) -> Vec<String> {
    let plan = plan_meta_slot_set(n).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let mut uris = Vec::with_capacity(n);
    for i in 0..n {
        let p = dir.join(format!("meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(len).unwrap();
        let opts = FormatV3Options {
            format_config_xattr: (i == 0).then(|| format_config_for(dir)),
            ..set_opts()
        };
        let r = format_v3_stamped(&p, len, &opts, plan.stamps[i].clone()).await;
        if r.is_err() {
            std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        }
        r.expect("format set member");
        uris.push(p.display().to_string());
    }
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    uris
}

/// Mint a DIRECTORY under the root whose ino routes to forest slot
/// `slot` of volume `vol_idx` while the slot is still the manager's (a
/// preset ino routes itself) — PR 6's `seed_dir_in_slot`, per volume.
pub async fn seed_dir_in_slot(
    routed: &RoutedMetaBackend,
    vol_idx: usize,
    slot: ForestSlot,
    name: &str,
) -> u64 {
    use squeezefs::meta_backend::kv::backend::KvMetaBackend;
    use squeezefs::meta_backend::{make_global_ino_width, IntentCreatePreset};
    let vol = &routed.volumes[vol_idx];
    let width = routed.routing_width();
    // Routing slot = forest slot − 1, then the set's slot map places it on
    // its volume; the caller names a slot the volume hosts.
    let routing = u64::from(slot) - 1;
    let local = vol
        .allocate_guest_ino(routing as u16)
        .expect("a guest cursor");
    let global = make_global_ino_width(local, routing, width);
    let ino = routed
        .create_with_rdev_preset(
            squeezefs::meta_backend::kv::builder::ROOT_INO,
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

/// Re-stamp appender `id`'s page on `uri` with `identity` (the in-process
/// fixture's "another NODE held this region": a page's identity is the
/// only thing that distinguishes a foreign appender's residue from this
/// node's own). Written to BOTH directory slots one generation apart —
/// the newest valid image over the four page slots.
pub async fn restamp_page_identity(
    uri: &str,
    id: u32,
    identity: squeezefs::meta_backend::kv::appender::AppenderIdentity,
) {
    use squeezefs::meta_backend::kv::appender::{read_directory, write_page};
    use squeezefs::meta_backend::kv::superblock::{classify_volume, VolumeFormat};
    let path = std::path::Path::new(uri);
    let VolumeFormat::V3(sb) = classify_volume(path).await.expect("superblock") else {
        panic!("{uri}: not a v3 volume");
    };
    let entries = read_directory(path, &sb).await.expect("directory");
    let e = entries
        .iter()
        .find(|e| e.appender_id == id)
        .expect("the appender's page");
    let mut page = e.page.clone().expect("a valid page");
    page.identity = identity;
    for off in e.dir_offsets {
        page.generation += 1;
        write_page(path, off, page.encode().unwrap())
            .await
            .expect("page write");
    }
    squeezefs::uring_fs::fdatasync(path.to_path_buf())
        .await
        .expect("fdatasync");
}

/// The holders' venue (PR 6's `Holders`): the S8 owner service + the
/// manager set service over `routed` on a loopback listener, every
/// declared appender's endpoint registered on each volume's plane, and
/// the initiator's shipper installed — what PR 12's join ladder wires
/// from the census. A step homed on a declared region's slot ships over
/// a real `cluster_wire` session and is applied under THAT region's lease
/// and ring.
pub struct HoldersVenue {
    host: Arc<squeezefs::cluster_wire::RpcListener>,
}

pub const VENUE_SECRET: &[u8] = b"sym-common-holders-venue-enroll-secret";

impl HoldersVenue {
    pub async fn stand_up(routed: &Arc<RoutedMetaBackend>, appenders: &[u32]) -> Self {
        use squeezefs::cluster_wire as cw;
        use squeezefs::data_grant::AsyncVerbRouter;
        use squeezefs::meta_backend::crossvol_tx::install_xv_shipper;
        use squeezefs::meta_ship::manager::ManagerSetService;
        use squeezefs::meta_ship::{MetaShipRouter, MetaShipService};
        let front: Arc<dyn cw::RpcAsyncService> = Arc::new(
            AsyncVerbRouter::new()
                .with_meta(MetaShipService::new(Arc::clone(routed)))
                .with_manager(ManagerSetService::new(&routed.volumes)),
        );
        let host = cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            VENUE_SECRET.to_vec(),
            front,
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
            VENUE_SECRET.to_vec(),
        ));
        Self { host }
    }

    pub fn tear_down(self) {
        squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
        self.host.shutdown();
    }
}

/// The offline fsck of `uris` must report nothing.
pub async fn fsck_clean(uris: &[String]) {
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

/// An ino inside forest slot `slot`'s guest keyspace (local key ino).
pub fn ino_in_slot(slot: ForestSlot, local: u64) -> u64 {
    squeezefs::meta_backend::guest_local_ino((slot - 1) as u16, local)
}

/// The forest slot a GLOBAL ino's records live in on the routed set.
pub fn slot_of_global(routed: &RoutedMetaBackend, ino: u64) -> ForestSlot {
    let (_v, local) = routed.route_ino(ino);
    forest_slot_of_ino(local)
}

pub async fn shutdown(routed: &RoutedMetaBackend) {
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// The DATA rig: the routed set bound to a file-backed data volume through
// a `DataRouter`, with PR 7's arm run (`arm_shared_refs`).
// ---------------------------------------------------------------------------

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use tempfile::{tempdir, NamedTempFile, TempDir};

pub const DATA_LEN: u64 = 2 * 1024 * 1024 * 1024;
pub const DATA_VOL: &str = "vol-00000000000000b3";

pub struct DataRig {
    pub router: DataRouter,
    pub alloc: Arc<BlockAllocator>,
    pub routed: Arc<RoutedMetaBackend>,
    _staging: TempDir,
}

pub fn data_file() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(DATA_LEN)
        .unwrap();
    f
}

pub async fn mount_data(uris: &[String], data: &std::path::Path, knobs: &Knobs) -> DataRig {
    let routed = open_under(uris, knobs).await;
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL).await.unwrap());
    alloc.set_capacity_bytes(DATA_LEN);
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, alloc.clone(), nvme);
    router.set_meta_backend(routed.clone());
    router.arm_shared_refs().await.expect("arm shared refs");
    DataRig {
        router,
        alloc,
        routed,
        _staging: staging,
    }
}

impl DataRig {
    pub fn vol(&self) -> &Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
        &self.routed.volumes[0]
    }

    pub fn tag(&self) -> u64 {
        squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL)
    }

    pub async fn mk_file(&self, name: &str) -> u64 {
        use squeezefs::meta_backend::Metadata;
        self.routed
            .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino
    }

    /// A file created under `parent` (the parent-slot affinity puts a
    /// fresh directory's children in ITS slot).
    pub async fn mk_file_in(&self, parent: u64, name: &str) -> u64 {
        use squeezefs::meta_backend::Metadata;
        self.routed
            .create(parent, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino
    }

    pub async fn mk_dir(&self, name: &str) -> u64 {
        use squeezefs::meta_backend::Metadata;
        self.routed
            .create(1, name, libc::S_IFDIR | 0o755, 1000, 1000)
            .await
            .expect("mkdir")
            .ino
    }

    /// A file whose forest slot differs from `not`'s (the rotor spreads
    /// children of `/` over 64 slots; two creates land apart, but the
    /// contract states it rather than presumes it).
    pub async fn mk_file_apart(&self, prefix: &str, not: ForestSlot) -> u64 {
        for i in 0..70 {
            let ino = self.mk_file(&format!("{prefix}{i}")).await;
            if slot_of_global(&self.routed, ino) != not {
                return ino;
            }
        }
        panic!("the rotor never left slot {not}");
    }

    pub fn token(&self, ino: u64) -> u64 {
        self.router.dlm.get_fencing_token_ino(ino)
    }

    /// Allocate a real block and bind it at `block_index` of `ino`
    /// through the shared merge primitive (the one place a striped map
    /// changes — where the accounting is computed); the displaced keys
    /// the primitive hands back are freed after the guard drops (the
    /// write path's own discipline).
    pub async fn publish_block(&self, ino: u64, block_index: u32) -> u64 {
        let offset = self.alloc.allocate_block().await.expect("allocate");
        self.alloc.publish_block(offset);
        let key = offset.to_string();
        let displaced = self
            .router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[(block_index, key)]),
                (block_index as u64 + 1) * 4 * 1024 * 1024,
                LayoutFlip::ToStripedKeepStagedIdentity,
                self.token(ino),
            )
            .await
            .expect("merge published block");
        for k in displaced {
            self.router
                .backend_router
                .free_block(&k)
                .await
                .expect("free displaced");
        }
        offset
    }

    pub async fn clone(&self, src: u64, dst: u64) -> squeezefs::error::Result<()> {
        self.router
            .clone_file(
                &squeezefs::keys::inode_path(src),
                &squeezefs::keys::inode_path(dst),
                Some(self.token(src)),
                Some(self.token(dst)),
            )
            .await
    }

    pub async fn drift(&self) -> Vec<(String, u64, u32, u32)> {
        self.router
            .backend_router
            .verify_durable_block_refs(&self.routed)
            .await
            .expect("C8 oracle")
    }

    pub async fn shutdown(self) {
        shutdown(&self.routed).await;
    }
}

// ---------------------------------------------------------------------------
// The FUSE-layer rig: the routed set behind an in-process
// `SqueezefsFilesystem` (the `pack_tenant_ops_tests` fixture's shape) —
// real staged writes with rider extents, the clone through
// `copy_file_range`, SETATTR(size) truncates.
// ---------------------------------------------------------------------------

use fuse3::raw::{Filesystem, Request};
use squeezefs::fuse_client::SqueezefsFilesystem;
use std::ffi::OsStr;

pub struct FuseRig {
    pub fs: Arc<SqueezefsFilesystem>,
    pub routed: Arc<RoutedMetaBackend>,
    pub alloc: Arc<BlockAllocator>,
    _staging: TempDir,
}

pub fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
        ..Default::default()
    }
}

/// Deterministic bytes (the packing rows' pattern).
pub fn pattern(idx: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (idx.wrapping_mul(131)
                .wrapping_add(i.wrapping_mul(7))
                .wrapping_add(i >> 8)
                % 251) as u8
        })
        .collect()
}

/// Mount the set behind the FUSE layer with a staging ring of `ring`
/// bytes (a small ring makes the staged-clone spill reachable).
pub async fn mount_fuse(
    uris: &[String],
    data: &std::path::Path,
    knobs: &Knobs,
    ring: &str,
) -> FuseRig {
    let routed = open_under(uris, knobs).await;
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL).await.unwrap());
    alloc.set_capacity_bytes(DATA_LEN);
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some(ring),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, alloc.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    fs.router.arm_shared_refs().await.expect("arm shared refs");
    FuseRig {
        fs: Arc::new(fs),
        routed,
        alloc,
        _staging: staging,
    }
}

impl FuseRig {
    pub fn vol(&self) -> &Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
        &self.routed.volumes[0]
    }

    pub fn tag(&self) -> u64 {
        squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL)
    }

    pub fn token(&self, ino: u64) -> u64 {
        self.fs.router.dlm.get_fencing_token_ino(ino)
    }

    pub async fn create(&self, name: &str) -> u64 {
        self.fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino
    }

    pub async fn write_at(&self, ino: u64, off: u64, data: &[u8]) {
        let written = self
            .fs
            .write(
                req(),
                ino,
                0,
                off,
                bytes::Bytes::copy_from_slice(data),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"))
            .written;
        assert_eq!(written as usize, data.len(), "short write at {off}");
    }

    pub async fn read(&self, ino: u64, len: usize) -> Vec<u8> {
        self.fs
            .read(req(), ino, 0, 0, len as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("read ino {ino} failed: {e:?}"))
            .data
            .to_vec()
    }

    /// A ring-RESIDENT staged file: the base image plus a 2 KiB rider at
    /// `rider_off` (the W2 record pins the entry in the ring — the
    /// promotion skips it, so the ring stays full of it).
    pub async fn resident_staged(&self, name: &str, len: usize, tag: usize) -> (u64, String) {
        let ino = self.create(name).await;
        self.write_at(ino, 0, &pattern(tag, len)).await;
        self.write_at(ino, 64 * 1024, &pattern(tag + 7, 2048)).await;
        let m = self.fs.router.metadata_cache.get(&ino).expect("RAM layout");
        assert_eq!(m.file_type, "staged", "fixture premise: staged layout");
        let fid = m.file_id.as_deref().expect("file_id").to_string();
        assert!(
            self.fs.router.cache.nvme.read_staged(&fid).is_some(),
            "fixture premise: ring-resident"
        );
        (ino, fid)
    }

    /// Fill the ring with `files` rider-pinned staged files of `len` bytes
    /// (no residency premise — a filler past the cap spills through the
    /// write path's own escalation; what matters is that the ring is FULL
    /// of live custody when the next stage arrives).
    pub async fn fill_ring(&self, files: usize, len: usize, tag: usize) {
        for i in 0..files {
            let ino = self.create(&format!("filler_{tag}_{i}")).await;
            self.write_at(ino, 0, &pattern(tag + i, len)).await;
            self.write_at(ino, 4096, &pattern(tag ^ 0x0F, 2048)).await;
        }
    }

    /// The composed image `resident_staged` wrote.
    pub fn composed(tag: usize, len: usize) -> Vec<u8> {
        let mut want = pattern(tag, len);
        let rider = pattern(tag + 7, 2048);
        want[64 * 1024..64 * 1024 + rider.len()].copy_from_slice(&rider);
        want
    }

    /// Whole-file `copy_file_range` into an EMPTY destination — the clone
    /// fast path (`DataRouter::clone_file`).
    pub async fn clone_whole(
        &self,
        src: u64,
        dst: u64,
        len: usize,
    ) -> squeezefs::error::Result<()> {
        let copied = self
            .fs
            .copy_file_range(req(), src, 0, 0, dst, 0, 0, len as u64, 0)
            .await
            .map_err(|e| squeezefs::error::SqueezefsError::InvalidOperation(format!("{e:?}")))?
            .copied;
        assert_eq!(copied as usize, len, "the whole file cloned");
        Ok(())
    }

    pub async fn mapping0(&self, ino: u64) -> Option<String> {
        self.fs
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .unwrap_or_else(|e| panic!("layout of ino {ino}: {e:?}"))
            .block_map
            .as_ref()
            .and_then(|bm| bm.get(&0).cloned())
    }

    pub async fn drift(&self) -> Vec<(String, u64, u32, u32)> {
        self.fs
            .router
            .backend_router
            .verify_durable_block_refs(&self.routed)
            .await
            .expect("C8 oracle")
    }

    pub async fn shutdown(self) {
        self.fs.router.seal_open_packs().await;
        self.fs.router.backend_router.reclaim_drain().await;
        shutdown(&self.routed).await;
    }
}
