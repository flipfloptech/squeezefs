//! fsck detection class **C12 — tenant-range consistency**, red-first
//! (`docs/design-small-file-packing.md` §5.9 / §10 / KD-9; PR plan PK5).
//!
//! The C8 ledger says HOW MANY references a block has; nothing said their
//! windows sit inside the block and nest legally. A packer mints slots by
//! one `fetch_add` on a block one mount owns, so it can never produce two
//! windows sharing a start; the two LEGAL sharing classes — the clone of a
//! promoted small file (identical windows) and that clone composed with
//! the passthrough clip (nested windows) — both keep the SAME `off`. The
//! corruption shape is therefore **intersection at DIFFERENT `off`** (a
//! slot minted inside another slot), and it is disjoint from the legal
//! classes by construction.
//!
//! Contracts (PK5):
//!
//! 1. Two hand-planted tenants intersecting at DIFFERENT `off` on one
//!    block are reported as ONE C12 finding (`fsck_findings + 1`,
//!    `fsck_tenant_overlap_findings + 1`); repair plans `report-only`,
//!    apply REFUSES, neither layout is touched, `fsck_repair_classC12`
//!    stays 0.
//! 2. An overrun (`off + ceil(len) > CHUNK`), a misaligned `off`, and an
//!    UNDECODABLE decoration (`bk:garbage:len`, `bk:off:garbage`, a
//!    two-component `bk:len`) are each reported (C12Overrun); C2 counts
//!    the base reference and stays silent.
//! 3. Two whole-block referencers (a striped clone) are NOT a finding.
//! 4. Two referencers with byte-identical windows — the clone of a
//!    PROMOTED small file, on a lever-OFF volume — are NOT a finding.
//! 5. Two referencers with nested same-`off` windows (clone + clip, either
//!    order — the same durable state) are NOT a finding; moving the
//!    clipped window's start IS (the class is not vacuous there).
//! 6. Zero-FP: a tenant released between the census read and the verify
//!    does not fire (the fresh re-read under both leases reproduces
//!    nothing); an OPEN pack block never fires (and fires once sealed —
//!    the exemption is load-bearing); a healthy packed population with
//!    both share classes runs C12 EMPTY ×10 online under live promotion.
//!
//! Shapes are planted DIRECTLY in the `layout` xattr (the meta backend's
//! internal writer, never the FUSE path) on top of REAL references: the
//! clone shares the block durably (+1 C8 record) and in RAM (+1 pin), so
//! rewriting only the DECORATION leaves C2/C3/C8 quiet and C12 the sole
//! judge of the window.

use fuse3::raw::{Filesystem, Request};
use squeezefs::block_allocator::{BlockAllocator, CHUNK_SIZE};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsck::{
    clear_pre_registry_check_hook, repair as run_repair, run as run_fsck,
    set_pre_registry_check_hook, FsckCtx, FsckFinding, FsckOptions, FsckReport, RepairOptions,
};
use squeezefs::fsync_economy;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::layout_wire::encode_layout;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{clean_block_key, DataRouter, LayoutMetadata, LBA_GRAIN};
use squeezefs::{DataVolumeRecord, FormatConfig};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;

const KIB: usize = 1024;
/// The volume block size: a small file (4 KiB < size ≤ block) STAGES, so
/// its fsync promotion is what produces the size-carrying mapping.
const BLOCK: usize = 4 * 1024 * KIB;
const TENANT: usize = 16 * KIB;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Lever seams return to the knob on drop.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::test_set_small_file_packing(None);
        squeezefs::routing::set_inline_max_bytes_override(None);
        fsync_economy::test_set_promote_staged(None);
    }
}

/// fsync promotes staged files; the one-page inline floor keeps every
/// TENANT-sized file staged; the packing lever as the contract needs it.
fn arm_promotion(packing: bool) -> LeverGuard {
    squeezefs::routing::set_inline_max_bytes_override(Some(squeezefs::routing::INLINE_MAX_FLOOR));
    squeezefs::routing::test_set_small_file_packing(Some(packing));
    fsync_economy::test_set_promote_staged(Some(true));
    LeverGuard
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
        ..Default::default()
    }
}

fn metric(a: &squeezefs::fuse_client::Align64<AtomicU64>) -> u64 {
    a.load(Ordering::Relaxed)
}

/// Deterministic per-file content, salted by the tag.
fn pattern(tag: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (tag.wrapping_mul(131).wrapping_add(i.wrapping_mul(7)) % 251) as u8)
        .collect()
}

fn make_dev_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn base_format_config(data_lvs: &[&Path]) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK as u64,
        capacity: 1 << 34,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(
            data_lvs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        ),
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
    }
}

async fn format_meta(meta: &Path, data_lvs: &[&Path]) {
    let cfg = base_format_config(data_lvs);
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
}

/// Mount-shaped fixture (the C9/C10 suites' shape with the PK2 suite's
/// staged-file geometry): one data volume, a staging dir, allocator
/// recovery like a mount, the fsck context on the live meta + router.
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    records: Vec<DataVolumeRecord>,
    staging_path: PathBuf,
    _staging: TempDir,
}

async fn open_fresh(dir: &Path, tag: &str) -> Fx {
    let meta = make_dev_file(dir, &format!("meta-{tag}"), 256 * 1024 * 1024);
    let oss = make_dev_file(dir, &format!("oss-{tag}"), 4 << 30);
    format_meta(&meta, &[&oss]).await;
    let records = base_format_config(&[&oss]).resolved_data_volumes();
    open_at(&meta, &records).await
}

async fn open_at(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    let dlm = DlmClient::new().unwrap();
    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(BlockAllocator::new(&first.id).await.unwrap());
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let staging = tempfile::tempdir().unwrap();
    let staging_path = staging.path().to_path_buf();
    let cache = TieredCache::new(
        vec![staging_path.clone()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in records {
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    for kv in &routed.volumes {
        for entry in fs.router.backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(kv, &fs.router.backend_router)
                .await
                .expect("allocator recovery");
        }
    }
    Fx {
        fs: Arc::new(fs),
        meta: routed,
        records: records.to_vec(),
        staging_path,
        _staging: staging,
    }
}

impl Fx {
    fn ctx(&self) -> FsckCtx {
        FsckCtx {
            meta: self.meta.clone(),
            router: self.fs.router.clone(),
            staging_dirs: vec![self.staging_path.clone()],
            expected_generation: None,
        }
    }

    fn alloc(&self) -> Arc<BlockAllocator> {
        self.fs
            .router
            .backend_router
            .backends
            .get(&self.records[0].id)
            .expect("registered backend")
            .block_allocator
            .clone()
    }

    /// Mount-faithful close: the dismount seal first (an open pack's pin
    /// and its process-global ledger entry leave with it), then the
    /// reclaim drain, then the volumes.
    async fn close(self) {
        self.fs.router.seal_open_packs().await;
        self.fs.router.backend_router.reclaim_drain().await;
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }

    async fn create(&self, name: &str) -> u64 {
        self.fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino
    }

    async fn write_at(&self, ino: u64, off: u64, data: &[u8]) {
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

    async fn fsync(&self, ino: u64) {
        self.fs
            .fsync(req(), ino, 0, false)
            .await
            .unwrap_or_else(|e| panic!("fsync ino {ino} failed: {e:?}"));
    }

    /// A small file staged then PROMOTED by fsync (the lever decides the
    /// arm): its durable `block_map[0]` is a size-carrying mapping.
    async fn promoted_small_file(&self, name: &str, len: usize, tag: usize) -> u64 {
        let ino = self.create(name).await;
        self.write_at(ino, 0, &pattern(tag, len)).await;
        let m = self.fs.router.metadata_cache.get(&ino).expect("RAM layout");
        assert_eq!(m.file_type, "staged", "fixture premise: staged layout");
        self.fsync(ino).await;
        let mapping = self.durable_mapping(ino, 0).await;
        assert_eq!(
            decoration(&mapping).map(|(_, l)| l),
            Some(len as u64),
            "fixture premise: fsync promoted {name} into a size-carrying mapping ({mapping})"
        );
        ino
    }

    /// A striped file of `nblocks` whole blocks (force the striped layout
    /// with a write past one block, then fill the blocks), fsync'd.
    async fn striped_file(&self, name: &str, nblocks: usize) -> u64 {
        let ino = self.create(name).await;
        self.write_at(ino, 0, &vec![0u8; BLOCK + 1]).await;
        for b in 0..nblocks {
            self.write_at(ino, (b * BLOCK) as u64, &vec![(b as u8) ^ 0xA7; BLOCK])
                .await;
        }
        self.fsync(ino).await;
        ino
    }

    /// `clone_file` — a PROMOTED source's clone SHARES its mapping verbatim
    /// (+1 durable reference, +1 RAM pin); a striped source's clone pins
    /// every block.
    async fn clone(&self, src: u64, name: &str) -> u64 {
        let dst = self.create(name).await;
        let src_tok = self.fs.dlm().get_fencing_token_ino(src);
        let dst_tok = self.fs.dlm().get_fencing_token_ino(dst);
        self.fs
            .router
            .clone_file(
                &squeezefs::keys::inode_path(src),
                &squeezefs::keys::inode_path(dst),
                Some(src_tok),
                Some(dst_tok),
            )
            .await
            .unwrap_or_else(|e| panic!("clone {src} -> {dst} failed: {e:?}"));
        dst
    }

    /// The DURABLE layout of `ino` (the `layout` xattr fsck reads —
    /// never the RAM cache).
    async fn durable_layout(&self, ino: u64) -> (usize, u64, LayoutMetadata) {
        let (vol_idx, local) = self.meta.route_ino(ino);
        let bytes = self.meta.volumes[vol_idx]
            .getxattr(local, "layout")
            .await
            .expect("layout read")
            .unwrap_or_else(|| panic!("ino {ino} has no layout xattr"));
        let layout: LayoutMetadata = bincode::deserialize(&bytes).expect("bincode layout");
        (vol_idx, local, layout)
    }

    async fn durable_layout_bytes(&self, ino: u64) -> Vec<u8> {
        let (vol_idx, local) = self.meta.route_ino(ino);
        self.meta.volumes[vol_idx]
            .getxattr(local, "layout")
            .await
            .expect("layout read")
            .unwrap_or_default()
    }

    async fn durable_mapping(&self, ino: u64, block_idx: u32) -> String {
        let (_, _, layout) = self.durable_layout(ino).await;
        layout
            .block_map
            .as_ref()
            .and_then(|m| m.get(&block_idx).cloned())
            .unwrap_or_else(|| panic!("ino {ino} has no block_map[{block_idx}]: {layout:?}"))
    }

    /// **The planting seam**: rewrite ONE entry of `ino`'s durable block
    /// map (and optionally its size) through the meta backend's internal
    /// xattr writer — the way the C9/C10 suites plant their damage, never
    /// the FUSE path. `None` removes the entry (the tenant released its
    /// window). The RAM layout is invalidated so nothing stale writes back.
    async fn plant(&self, ino: u64, block_idx: u32, mapping: Option<&str>, size: Option<u64>) {
        let (vol_idx, local, mut layout) = self.durable_layout(ino).await;
        let map = layout.block_map.get_or_insert_with(HashMap::new);
        match mapping {
            Some(m) => {
                map.insert(block_idx, m.to_string());
            }
            None => {
                map.remove(&block_idx);
            }
        }
        if let Some(s) = size {
            layout.size = s;
        }
        let encoded = encode_layout(&layout).expect("layout encodes");
        self.meta.volumes[vol_idx]
            .setxattr_internal(local, "layout", &encoded)
            .await
            .expect("plant the layout");
        self.fs.router.metadata_cache.invalidate(&ino);
    }

    /// Re-decorate `ino`'s `block_map[0]` as `base:off:len` on the SAME
    /// base (the block, incarnation stamp included, is untouched — only
    /// the window moves); the size follows `len`.
    async fn plant_window(&self, ino: u64, off: u64, len: u64) {
        let base = clean_block_key(&self.durable_mapping(ino, 0).await);
        self.plant(ino, 0, Some(&format!("{base}:{off}:{len}")), Some(len))
            .await;
    }

    /// The device offset a mapping's base names.
    fn offset_of(&self, mapping: &str) -> u64 {
        self.fs
            .router
            .backend_router
            .parse_block_offset(&clean_block_key(mapping))
            .expect("base key parses")
    }
}

/// `(off, len)` of a size-carrying mapping; `None` for a bare key.
fn decoration(mapping: &str) -> Option<(u64, u64)> {
    let rest = match mapping.find("://") {
        Some(p) => &mapping[p + 3..],
        None => mapping,
    };
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((parts[1].parse().ok()?, parts[2].parse().ok()?))
}

/// Fast-settle online options (the full suspect → settle → re-check
/// machinery still runs; only the wall clock shrinks).
fn online_opts() -> FsckOptions {
    let mut o = FsckOptions::online();
    o.settle = std::time::Duration::from_millis(100);
    o
}

fn offline_opts() -> FsckOptions {
    let mut o = FsckOptions::offline();
    o.settle = std::time::Duration::from_millis(10);
    o
}

fn dry_run() -> RepairOptions {
    RepairOptions {
        apply: false,
        quarantine_dir: None,
        multi_owner: false,
    }
}

fn apply() -> RepairOptions {
    RepairOptions {
        apply: true,
        quarantine_dir: None,
        multi_owner: false,
    }
}

fn c12(report: &FsckReport) -> Vec<&FsckFinding> {
    report
        .findings
        .iter()
        .filter(|f| f.class == "C12")
        .collect()
}

fn assert_zero_findings(report: &FsckReport, what: &str) {
    assert!(
        report.findings.is_empty(),
        "{what}: fsck_findings must be 0 on a healthy volume (tripwire), got {:?}",
        report.findings
    );
}

// ---------------------------------------------------------------------------
// Contract 1: two tenants intersecting at DIFFERENT `off` — one finding,
// report-only
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_tenants_intersecting_at_different_off_are_one_report_only_finding() {
    let _g = serial().await;
    let _l = arm_promotion(false);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "overlap").await;

    // A promoted small file and its clone share ONE block: two durable
    // references, RAM refcount 2, identical windows `[0, 16 KiB)`.
    let a = fx.promoted_small_file("a.bin", TENANT, 1).await;
    let b = fx.clone(a, "b.bin").await;
    assert_eq!(
        fx.durable_mapping(a, 0).await,
        fx.durable_mapping(b, 0).await,
        "fixture premise: the clone shares the mapping verbatim"
    );
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "clone-shared promoted file (the control)",
    );

    // The corruption: B's window moves to `[4 KiB, 20 KiB)` — a slot
    // minted inside another slot. The block, its two references and its
    // refcount are all untouched, so C2/C3/C8 have nothing to say.
    fx.plant_window(b, LBA_GRAIN, TENANT as u64).await;
    let mapping_a = fx.durable_mapping(a, 0).await;
    let mapping_b = fx.durable_mapping(b, 0).await;
    let offset = fx.offset_of(&mapping_a);
    let findings0 = metric(&METRICS.fsck_findings);
    let overlap0 = metric(&METRICS.fsck_tenant_overlap_findings);

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(
        report.findings.len(),
        1,
        "exactly ONE finding — C12 alone judges the window: {:?}",
        report.findings
    );
    let f = c12(&report)[0];
    assert!(
        f.object.ends_with(&format!(":{offset}")),
        "the finding names the block `vol:offset`: {f:?}"
    );
    for (ino, mapping) in [(a, &mapping_a), (b, &mapping_b)] {
        assert!(
            f.evidence.contains(&format!("ino {ino} ")) && f.evidence.contains(mapping.as_str()),
            "the evidence names both tenants ({ino}, {mapping}): {f:?}"
        );
    }
    assert!(
        f.evidence.contains("REPORT-ONLY"),
        "the evidence states the posture: {f:?}"
    );
    assert_eq!(report.counters.findings, 1);
    assert_eq!(
        report.counters.tenant_overlap_findings, 1,
        "the class counter counts the verified finding: {:?}",
        report.counters
    );
    assert_eq!(metric(&METRICS.fsck_findings), findings0 + 1);
    assert_eq!(
        metric(&METRICS.fsck_tenant_overlap_findings),
        overlap0 + 1,
        "fsck_tenant_overlap_findings is the live tripwire"
    );

    // Offline sees the same durable state (no leases — nothing in flight).
    let offline = run_fsck(&fx.ctx(), &offline_opts()).await.expect("fsck");
    assert_eq!(c12(&offline).len(), 1, "{:?}", offline.findings);

    // Repair: planned as report-only, REFUSED on apply, nothing touched.
    let bytes_a = fx.durable_layout_bytes(a).await;
    let bytes_b = fx.durable_layout_bytes(b).await;
    let plan = run_repair(&fx.ctx(), &report, &dry_run())
        .await
        .expect("plan");
    assert_eq!(plan.planned.len(), 1, "{plan:?}");
    assert_eq!(plan.planned[0].class, "C12");
    assert_eq!(
        plan.planned[0].action, "report-only",
        "the planner states the C8 posture: {:?}",
        plan.planned[0]
    );
    assert!(plan.applied.is_empty() && plan.refused.is_empty());

    let refused0 = metric(&METRICS.fsck_repairs_refused);
    let applied = run_repair(&fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert!(
        applied.applied.is_empty(),
        "nothing is ever applied: {applied:?}"
    );
    assert_eq!(applied.refused.len(), 1, "{applied:?}");
    assert_eq!(applied.refused[0].class, "C12");
    assert_eq!(applied.counters.refused, 1);
    assert_eq!(
        metric(&METRICS.fsck_repairs_refused),
        refused0 + 1,
        "fsck_repairs_refused counts the attempted C12 repair"
    );
    assert_eq!(
        metric(&METRICS.fsck_repair_class_c12),
        0,
        "fsck_repair_classC12 is structurally 0"
    );
    assert_eq!(
        fx.durable_layout_bytes(a).await,
        bytes_a,
        "A's layout untouched"
    );
    assert_eq!(
        fx.durable_layout_bytes(b).await,
        bytes_b,
        "B's layout untouched"
    );
    assert_eq!(
        fx.alloc().refcount(offset),
        Some(2),
        "neither reference was released"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 2: overrun / misaligned / undecodable decorations — C12Overrun,
// C2 silent
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overrun_misaligned_and_undecodable_decorations_are_each_reported() {
    let _g = serial().await;
    let _l = arm_promotion(false);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "overrun").await;

    // Five promoted files, each on its own block (lever OFF), each
    // re-decorated into one defect class. The base of every mapping stays
    // the block the file really owns.
    let overrun = fx.promoted_small_file("overrun.bin", TENANT, 1).await;
    let misaligned = fx.promoted_small_file("misaligned.bin", TENANT, 2).await;
    let bad_off = fx.promoted_small_file("bad_off.bin", TENANT, 3).await;
    let bad_len = fx.promoted_small_file("bad_len.bin", TENANT, 4).await;
    let two_part = fx.promoted_small_file("two_part.bin", TENANT, 5).await;
    let control = fx.promoted_small_file("control.bin", TENANT, 6).await;
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "six promoted files (the control)",
    );

    // `off + ceil(len) > CHUNK` by exactly one LBA grain.
    fx.plant_window(
        overrun,
        CHUNK_SIZE - (TENANT as u64) + LBA_GRAIN,
        TENANT as u64,
    )
    .await;
    // `off % LBA_GRAIN != 0`.
    fx.plant_window(misaligned, LBA_GRAIN / 2, TENANT as u64)
        .await;
    // Undecodable `off`, undecodable `len`, a two-component decoration.
    for (ino, deco) in [
        (bad_off, format!("garbage:{TENANT}")),
        (bad_len, "0:garbage".to_string()),
        (two_part, format!("{TENANT}")),
    ] {
        let base = clean_block_key(&fx.durable_mapping(ino, 0).await);
        fx.plant(ino, 0, Some(&format!("{base}:{deco}")), None)
            .await;
    }
    let planted: Vec<(u64, String)> = {
        let mut v = Vec::new();
        for ino in [overrun, misaligned, bad_off, bad_len, two_part] {
            v.push((ino, fx.durable_mapping(ino, 0).await));
        }
        v
    };

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let findings = c12(&report);
    assert_eq!(
        findings.len(),
        5,
        "each defect is ONE C12 finding: {:?}",
        report.findings
    );
    assert_eq!(
        report.findings.len(),
        5,
        "C2 counts the base reference and stays silent — no other class fires: {:?}",
        report.findings
    );
    for (ino, mapping) in &planted {
        let mine: Vec<_> = findings
            .iter()
            .filter(|f| f.evidence.contains(&format!("ino {ino} ")))
            .collect();
        assert_eq!(mine.len(), 1, "one finding names ino {ino}: {findings:?}");
        assert!(
            mine[0].evidence.contains(mapping.as_str()),
            "the finding quotes the mapping verbatim ({mapping}): {:?}",
            mine[0]
        );
        let offset = fx.offset_of(mapping);
        assert!(
            mine[0].object.ends_with(&format!(":{offset}")),
            "the finding names the block: {:?}",
            mine[0]
        );
    }
    let evidence_of = |ino: u64| -> String {
        findings
            .iter()
            .find(|f| f.evidence.contains(&format!("ino {ino} ")))
            .map(|f| f.evidence.clone())
            .unwrap_or_default()
    };
    assert!(
        evidence_of(overrun).contains("past"),
        "the overrun arm says the window reaches past the chunk: {}",
        evidence_of(overrun)
    );
    assert!(
        evidence_of(misaligned).contains("aligned"),
        "the misaligned arm names the grain: {}",
        evidence_of(misaligned)
    );
    for ino in [bad_off, bad_len, two_part] {
        assert!(
            evidence_of(ino).contains("undecodable") || evidence_of(ino).contains("decodes"),
            "the third arm says the decoration does not decode: {}",
            evidence_of(ino)
        );
    }
    assert!(
        !findings
            .iter()
            .any(|f| f.evidence.contains(&format!("ino {control} "))),
        "the healthy neighbour is never named"
    );
    assert_eq!(report.counters.tenant_overlap_findings, 5);

    // Report-only, every arm.
    let applied = run_repair(&fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert!(applied.applied.is_empty(), "{applied:?}");
    assert_eq!(applied.refused.len(), 5, "{applied:?}");
    for (ino, mapping) in &planted {
        assert_eq!(
            fx.durable_mapping(*ino, 0).await,
            *mapping,
            "the planted mapping stays exactly where it was (the owner's to fix)"
        );
    }
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 3: two whole-block referencers (a striped clone) — not a finding
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_whole_block_referencers_are_not_a_finding() {
    let _g = serial().await;
    let _l = arm_promotion(false);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "striped").await;

    let a = fx.striped_file("a.bin", 2).await;
    let b = fx.clone(a, "b.bin").await;
    let (_, _, la) = fx.durable_layout(a).await;
    let (_, _, lb) = fx.durable_layout(b).await;
    let ma = la.block_map.clone().unwrap_or_default();
    let mb = lb.block_map.clone().unwrap_or_default();
    assert!(ma.len() >= 2, "fixture premise: a striped source: {la:?}");
    assert_eq!(ma, mb, "fixture premise: the clone shares every block");
    for mapping in ma.values() {
        assert!(
            decoration(mapping).is_none(),
            "fixture premise: whole-block (bare) mappings: {mapping}"
        );
    }

    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "two whole-block referencers per block (online)",
    );
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &offline_opts()).await.expect("fsck"),
        "two whole-block referencers per block (offline)",
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 4: byte-identical windows (the clone of a promoted small file,
// lever OFF) — not a finding
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_windows_of_a_cloned_promoted_small_file_are_not_a_finding() {
    let _g = serial().await;
    let _l = arm_promotion(false);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "identical").await;

    let a = fx.promoted_small_file("a.bin", TENANT, 1).await;
    let b = fx.clone(a, "b.bin").await;
    let c = fx.clone(a, "c.bin").await;
    let ma = fx.durable_mapping(a, 0).await;
    assert_eq!(ma, fx.durable_mapping(b, 0).await);
    assert_eq!(ma, fx.durable_mapping(c, 0).await);
    assert_eq!(
        fx.alloc().refcount(fx.offset_of(&ma)),
        Some(3),
        "fixture premise: three referencers, one block, identical windows"
    );

    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "identical windows (online)",
    );
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &offline_opts()).await.expect("fsck"),
        "identical windows (offline)",
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 5: nested same-`off` windows (clone + clip, either order) — not
// a finding; a moved start IS
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_same_off_windows_are_not_a_finding_but_a_moved_start_is() {
    let _g = serial().await;
    let _l = arm_promotion(false);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "nested").await;

    // A promoted, cloned twice; then the passthrough CLIP on A (clone-then-
    // clip: B keeps the long window) and a deeper clip on C (clip-then-
    // clone reads the same durable state: the shorter is a prefix of the
    // same image at the same `off`).
    let a = fx.promoted_small_file("a.bin", TENANT, 1).await;
    let b = fx.clone(a, "b.bin").await;
    let c = fx.clone(a, "c.bin").await;
    fx.plant_window(a, 0, 8 * KIB as u64).await;
    fx.plant_window(c, 0, 4 * KIB as u64).await;
    let base = clean_block_key(&fx.durable_mapping(a, 0).await);
    assert_eq!(
        fx.durable_mapping(a, 0).await,
        format!("{base}:0:{}", 8 * KIB)
    );
    assert_eq!(fx.durable_mapping(b, 0).await, format!("{base}:0:{TENANT}"));
    assert_eq!(
        fx.durable_mapping(c, 0).await,
        format!("{base}:0:{}", 4 * KIB)
    );

    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "nested same-off windows (online)",
    );
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &offline_opts()).await.expect("fsck"),
        "nested same-off windows (offline)",
    );

    // The control: the SAME population with C's start moved inside B's
    // window is the corruption class — the legal-share exemption is a
    // property of the start, not of nesting.
    fx.plant_window(c, LBA_GRAIN, 4 * KIB as u64).await;
    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(
        report.findings.len(),
        1,
        "a nested window whose start moved is one C12 finding: {:?}",
        report.findings
    );
    assert_eq!(c12(&report).len(), 1);
    assert!(
        c12(&report)[0].evidence.contains(&format!("ino {c} ")),
        "the moved tenant is named: {:?}",
        c12(&report)[0]
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 6a: zero-FP — a tenant released between the census read and
// the verify does not fire
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_released_between_the_census_and_the_verify_does_not_fire() {
    let _g = serial().await;
    let _l = arm_promotion(false);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "race").await;

    // The census sees the corruption shape (A at 0, B at 4 KiB) …
    let a = fx.promoted_small_file("a.bin", TENANT, 1).await;
    let b = fx.clone(a, "b.bin").await;
    fx.plant_window(b, LBA_GRAIN, TENANT as u64).await;
    let key = fx.fs.router.backend_router.persist_block_key(
        &fx.records[0].id,
        fx.offset_of(&fx.durable_mapping(a, 0).await),
    );

    // … and at the exact boundary between the suspect's registry check
    // and its verification, A releases its window (the tenant is gone —
    // the block a later reader of B's mapping sees is a different
    // lifetime of the offset). The fresh re-read under both leases finds
    // no second window, so nothing fires.
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let fx_fs = fx.fs.clone();
    let fx_meta = fx.meta.clone();
    let releaser = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || go_rx.recv())
            .await
            .expect("join")
            .expect("hook signal");
        let (vol_idx, local) = fx_meta.route_ino(a);
        let kv = &fx_meta.volumes[vol_idx];
        let bytes = kv
            .getxattr(local, "layout")
            .await
            .expect("layout")
            .expect("present");
        let mut layout: LayoutMetadata = bincode::deserialize(&bytes).expect("layout");
        layout.block_map = Some(HashMap::new());
        layout.size = 0;
        kv.setxattr_internal(local, "layout", &encode_layout(&layout).expect("encode"))
            .await
            .expect("release A's window");
        fx_fs.router.metadata_cache.invalidate(&a);
        done_tx.send(()).expect("done signal");
    });
    let fired = Arc::new(AtomicBool::new(false));
    let hook_key = key.clone();
    let hook_fired = fired.clone();
    let done_rx = std::sync::Mutex::new(done_rx);
    set_pre_registry_check_hook(Arc::new(move |suspect_key: &str| {
        if suspect_key == hook_key && !hook_fired.swap(true, Ordering::SeqCst) {
            go_tx.send(()).expect("go signal");
            done_rx
                .lock()
                .expect("poisoned")
                .recv()
                .expect("release landed");
        }
    }));

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    clear_pre_registry_check_hook();
    releaser.await.expect("releaser join");
    assert!(
        fired.load(Ordering::SeqCst),
        "the C12 suspect must have reached the verify boundary (hook fired)"
    );
    assert_zero_findings(&report, "tenant released between census and verify");
    assert!(
        report.counters.suspects_cleared >= 1,
        "the suspect was raised and CLEARED, not never raised: {:?}",
        report.counters
    );
    assert_eq!(report.counters.tenant_overlap_findings, 0);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 6b: an OPEN pack block never fires — and fires once sealed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_pack_block_never_fires_and_the_sealed_one_does() {
    let _g = serial().await;
    let _l = arm_promotion(true);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "openpack").await;

    // The lever ON: fsync promotes A into the mount's OPEN pack block
    // (its slots are being minted right now — the pack-open ledger names
    // it). A clone shares the tenant; its window is then moved inside A's.
    let a = fx.promoted_small_file("a.bin", TENANT, 1).await;
    let base = clean_block_key(&fx.durable_mapping(a, 0).await);
    assert!(
        squeezefs::jobs::pack_open_ledger()
            .iter()
            .any(|k| k == &base),
        "fixture premise: the tenant's block is an OPEN pack ({base}): {:?}",
        squeezefs::jobs::pack_open_ledger()
    );
    let b = fx.clone(a, "b.bin").await;
    fx.plant_window(b, LBA_GRAIN, TENANT as u64).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&report, "an open pack block");
    assert!(
        report.counters.pack_ledger_exempted >= 1,
        "the pack-open ledger excused the block (counted, not silent): {:?}",
        report.counters
    );

    // Sealed, the same durable state is the corruption class — the
    // exemption was load-bearing, not vacuous.
    assert!(fx.fs.router.seal_open_packs().await >= 1, "one pack sealed");
    assert!(
        !squeezefs::jobs::pack_open_ledger()
            .iter()
            .any(|k| k == &base),
        "the seal leaves the ledger"
    );
    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(
        report.findings.len(),
        1,
        "the sealed pack's overlapping tenants are one C12 finding: {:?}",
        report.findings
    );
    assert_eq!(c12(&report).len(), 1);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 6c: a healthy packed population with both share classes runs
// C12 empty ×10 online under live promotion
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_healthy_packed_population_runs_c12_empty_ten_times_under_live_promotion() {
    let _g = serial().await;
    let _l = arm_promotion(true);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "storm").await;

    // The seed: three tenants of one pack at distinct slots, the identical-
    // window share (a clone of the SECOND tenant — nonzero `off`) and the
    // nested share (a clone of the third, clipped to its first grain).
    let t0 = fx.promoted_small_file("t0.bin", TENANT, 1).await;
    let t1 = fx.promoted_small_file("t1.bin", TENANT, 2).await;
    let t2 = fx.promoted_small_file("t2.bin", TENANT, 3).await;
    let m0 = fx.durable_mapping(t0, 0).await;
    let m1 = fx.durable_mapping(t1, 0).await;
    let m2 = fx.durable_mapping(t2, 0).await;
    assert_eq!(clean_block_key(&m0), clean_block_key(&m1), "one pack block");
    assert_eq!(clean_block_key(&m0), clean_block_key(&m2), "one pack block");
    let (off1, _) = decoration(&m1).expect("size-carrying");
    let (off2, _) = decoration(&m2).expect("size-carrying");
    assert!(off1 > 0 && off2 > off1, "distinct slots: {m0} {m1} {m2}");
    let identical = fx.clone(t1, "t1-clone.bin").await;
    assert_eq!(fx.durable_mapping(identical, 0).await, m1);
    let nested = fx.clone(t2, "t2-clip.bin").await;
    fx.plant_window(nested, off2, LBA_GRAIN).await;
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "the seeded population (open pack, both share classes)",
    );

    // The storm: create + write + fsync — every fsync promotes into the
    // open pack (mid-flight tenants ride the in-flight registry, the pin
    // rides the ledger) — while ten online passes run against it. Each
    // pass is the population AS OF ITS START (PR 14 — the per-slot ino
    // watermarks; the contract below), so a creator can no longer keep a
    // pass chasing the tree's tail; the storm's LEAD over the census is
    // still capped (`STORM_LEAD` files per pass — it yields until the
    // next pass completes) because ten passes over a population a
    // creator DOUBLES between passes is the harness's own arithmetic
    // (uncapped, the tenth pass read 348 s and filled the 4 GiB data
    // volume, `ENOSPC`), never the census's liveness law.
    const STORM_LEAD: usize = 400;
    let stop = Arc::new(AtomicBool::new(false));
    let runs_done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let storm = {
        let fs = fx.fs.clone();
        let stop = stop.clone();
        let runs_done = runs_done.clone();
        tokio::spawn(async move {
            let mut n = 0usize;
            let mut run_seen = 0usize;
            let mut n_at_run = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let run = runs_done.load(Ordering::Acquire);
                if run != run_seen {
                    run_seen = run;
                    n_at_run = n;
                }
                if n - n_at_run >= STORM_LEAD {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    continue;
                }
                let name = format!("storm_{n:05}.bin");
                let ino = fs
                    .create(req(), 1, OsStr::new(&name), libc::S_IFREG | 0o644, 0)
                    .await
                    .expect("create")
                    .attr
                    .ino;
                let len = (8 + 4 * (n % 7)) * KIB;
                fs.write(
                    req(),
                    ino,
                    0,
                    0,
                    bytes::Bytes::from(pattern(100 + n, len)),
                    0,
                    0,
                )
                .await
                .expect("write");
                fs.fsync(req(), ino, 0, false).await.expect("fsync");
                n += 1;
            }
            n
        })
    };
    let packed0 = metric(&METRICS.layout_promoted_packed);
    for run in 0..10 {
        let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
        runs_done.fetch_add(1, Ordering::AcqRel);
        assert!(
            report.findings.is_empty(),
            "run {run}: an online fsck under live packing found something: {:?}",
            report.findings
        );
        assert_eq!(report.counters.tenant_overlap_findings, 0, "run {run}");
    }
    stop.store(true, Ordering::Relaxed);
    let promoted = storm.await.expect("storm join");
    assert!(promoted > 0, "the storm promoted files");
    assert!(
        metric(&METRICS.layout_promoted_packed) - packed0 >= promoted as u64,
        "every storm fsync packed a tenant (engagement)"
    );

    // Quiesced and sealed: the whole population, both share classes, the
    // storm's tenants — still empty.
    fx.fs.router.seal_open_packs().await;
    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&report, "the sealed packed population");
    assert_eq!(report.counters.tenant_overlap_findings, 0);
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &offline_opts()).await.expect("fsck"),
        "the sealed packed population (offline)",
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The census's liveness under a creator (PR 13i's record §4.4aw, landed in
// PR 14)
// ---------------------------------------------------------------------------

/// An ONLINE census walks the inode tree a page at a time with a `layout`
/// read per ino, so a creator that outpaces it kept every page's tail
/// ahead of the cursor and the walk chased the tree for the creator's
/// whole life (contract 6c caps its storm's lead per pass — the harness's
/// own wall bound, stated there). The
/// census is the population AS OF ITS START — bounded at the per-slot ino
/// watermarks it began with: a record minted past its slot's watermark is
/// skipped (the next census's) and the cursor jumps to the next slot once a
/// page reaches it. Pinned with the seam that parks the walk after
/// its first page while 2,000 inodes mint past every watermark.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_online_census_is_bounded_at_the_ino_watermarks_it_started_with() {
    use squeezefs::fsck::{test_census_hold_release, TEST_CENSUS_HOLD_AFTER_FIRST_PAGE};
    let _g = serial().await;
    let _l = arm_promotion(true);
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), "census-bound").await;
    const BEFORE: usize = 700;
    const DURING: usize = 2_000;
    for n in 0..BEFORE {
        fx.fs
            .create(
                req(),
                1,
                OsStr::new(&format!("b{n:05}")),
                libc::S_IFREG | 0o644,
                0,
            )
            .await
            .expect("create");
    }
    let base = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(
        base.counters.inodes_scanned,
        BEFORE as u64 + 1,
        "the root + the population"
    );

    TEST_CENSUS_HOLD_AFTER_FIRST_PAGE.store(true, Ordering::SeqCst);
    let ctx = fx.ctx();
    let census = tokio::spawn(async move { run_fsck(&ctx, &online_opts()).await });
    // The walk is parked past its first page; the creator mints past every
    // watermark the walk began with.
    for _ in 0..600 {
        if squeezefs::fsck::test_census_holds() > 0 {
            break;
        }
        squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        squeezefs::fsck::test_census_holds() > 0,
        "the walk parked after its first page"
    );
    for n in 0..DURING {
        fx.fs
            .create(
                req(),
                1,
                OsStr::new(&format!("d{n:05}")),
                libc::S_IFREG | 0o644,
                0,
            )
            .await
            .expect("create");
    }
    TEST_CENSUS_HOLD_AFTER_FIRST_PAGE.store(false, Ordering::SeqCst);
    test_census_hold_release();
    let report = tokio::time::timeout(std::time::Duration::from_secs(120), census)
        .await
        .expect("the parked census completes once released — it never chases the creator")
        .expect("join")
        .expect("fsck");
    // The census is the population AS OF ITS START: the root + the 700.
    assert_eq!(
        report.counters.inodes_scanned,
        BEFORE as u64 + 1,
        "the census scanned {} inodes: it is the {} that existed when it began (the {DURING} \
         minted past its watermarks are the next census's)",
        report.counters.inodes_scanned,
        BEFORE + 1
    );
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    // The next pass sees everything.
    let after = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(after.counters.inodes_scanned, (BEFORE + DURING) as u64 + 1);
    fx.close().await;
}
