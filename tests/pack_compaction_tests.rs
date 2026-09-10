//! Small-file PACKING — PR PK6's contracts: the pack occupancy face of
//! defrag D1 and the re-pack COMPACTION mover
//! (`docs/design-small-file-packing.md` §5.5, §5.8, §5.11, §10; PR plan
//! PK6).
//!
//! A pack block's OCCUPANCY is `Σ_{distinct off} pack_slot_len(max len at
//! that off) / CHUNK_SIZE` over its live tenants — derived from the mover
//! census the fabric already walks (no new durable structure, no new tree
//! walk): two referencers with byte-identical windows (a clone share) and
//! two with nested same-`off` windows (clone + passthrough clip) occupy
//! their slot ONCE. A block is a compaction VICTIM iff its live bytes fit
//! the own-block threshold `pack_max_slot_bytes()` (derived `CHUNK/2`) —
//! ONE law, two faces (§5.5, KD-5): copying `live` reclaims `CHUNK − live ≥
//! live`. The mover (`JobType::DefragPack`, `squeezefs defrag --pack`)
//! re-packs each victim's live windows into the mount's OPEN pack block —
//! one ranged read per distinct `off` at its `max(len)`, one slot DMA,
//! every referencer republished to the same destination `off'` with its
//! own `len` under the ino's CURRENT token — and the victim frees only
//! when its population reaches 0 through the ordinary terminal-free
//! ladder. Its FIRST customer is the legacy one-block-per-file population
//! the lever-OFF promotion arms mint (`bk:0:len`, one tenant per block).
//!
//! The contracts (lever ON through the seam unless stated):
//!  1. The occupancy derivation is EXACT on a planted population, both
//!     share classes counted once; a striped file's blocks are not pack
//!     blocks.
//!  2. Victims below half re-pack into one fresh pack and FREE (net ≥ 1
//!     block, `copied ≤ reclaimed`); a block above half is NOT touched;
//!     C8 and C12 clean after the pass.
//!  3. The legacy one-block-per-file population is compacted
//!     (`blocks_freed = files`, one pack afterwards).
//!  4. A clone share is copied ONCE: both referencers land on the same
//!     destination window (`windows_copied < tenants_moved`, refcount 2 on
//!     the destination for the pair).
//!  5. A nested prefix share (clone + clip) is copied ONCE at the longer
//!     `len`; each referencer keeps its own `len` at the shared `off'`.
//!  6. A tenant re-staged mid-plan is DEFERRED, not copied
//!     (`pack_mover_resident_defers` / `pack_compaction_deferred` move).
//!  7. A refused publish releases exactly ONE destination reference (no
//!     leak, no double free).
//!  8. 💥 A kill-9 mid-pass (the in-process crash analog) leaves no leak
//!     and no double free; the adopted job re-plans idempotently and
//!     converges (KD-6).
//!  9. The throttle law engages; pause/resume/cancel work.
//! 10. The VL9 mover-scope pin: a compaction serializes LOUD behind a
//!     running drain (`job_serialized_waits + 1`).
//! 11. Lever OFF gates the WHOLE arm: the job refuses loud naming the
//!     lever and moves nothing — the legacy population is compacted under
//!     the lever ON only.
//! 12. `--report-only` measures without moving: the `frag_d1_pack_*`
//!     gauges publish, no `pack_compaction_*` counter moves.
//! 13. (mount-class) The CLI face on a real daemon: `defrag --report-only`
//!     reads the legacy population; `defrag --pack` is REFUSED under the
//!     lever OFF and compacts it under the lever ON; a remount with the
//!     C8 oracle armed reads drift 0 and every file byte-exact.
//! 14. The report is BOUNDED on the wire: `DefragReport::to_bounded_json`
//!     serves exact aggregates + the worst-occupancy row prefix that fits
//!     under `ADMIN_BODY_MAX` + `rows_elided` (the fsck precedent).

use fuse3::raw::{Filesystem, Request};
use squeezefs::block_allocator::{BlockAllocator, CHUNK_SIZE};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsync_economy;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::jobs::{JobFabric, JobSpec, JobState, JobType, MoverCtx};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{pack_slot_len, DataRouter};
use squeezefs::{DataVolumeRecord, FormatConfig};
use squeezefs_testkit::{mount_supported, site};
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const KIB: usize = 1024;
const BLOCK: usize = 4 * 1024 * KIB;
const LEVER: &str = "SQUEEZEFS_SMALL_FILE_PACKING";
/// The mount's default `--dismount-wait`.
const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Deterministic per-file content, salted by the tag (a zeros read or a
/// cross-tenant mix-up is caught).
fn pattern(tag: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (tag.wrapping_mul(131)
                .wrapping_add(i.wrapping_mul(7))
                .wrapping_add(i >> 8)
                % 251) as u8
        })
        .collect()
}

fn metric(a: &squeezefs::fuse_client::Align64<AtomicU64>) -> u64 {
    a.load(Ordering::Relaxed)
}

// ===========================================================================
// The in-process venue
// ===========================================================================

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
        squeezefs::routing::set_pack_max_slot_bytes_override(None);
        squeezefs::routing::set_inline_max_bytes_override(None);
        fsync_economy::test_set_promote_staged(None);
        squeezefs::jobs::clear_evacuate_pre_publish_hook();
    }
}

/// The one-page inline ceiling pinned, fsync = a promotion, the packing
/// lever as asked.
fn arm_levers(packing: bool) -> LeverGuard {
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

/// Mount-shaped fixture: `n` data volumes registered (the first is the
/// default slot), a staging dir, the job fabric with the mount's mover
/// context (quiesce probe included), allocator recovery like a mount.
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    fabric: Arc<JobFabric>,
    records: Vec<DataVolumeRecord>,
    staging_path: PathBuf,
    _staging: TempDir,
}

/// A fresh format + open: `n` volumes of `dev_bytes` each.
async fn open_fresh(dir: &Path, n: usize, dev_bytes: u64, tag: &str) -> (PathBuf, Fx) {
    let meta = make_dev_file(dir, &format!("meta-{tag}"), 256 * 1024 * 1024);
    let paths: Vec<PathBuf> = (0..n)
        .map(|i| make_dev_file(dir, &format!("oss{}-{tag}", i + 1), dev_bytes))
        .collect();
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    format_meta(&meta, &refs).await;
    let records = base_format_config(&refs).resolved_data_volumes();
    let fx = open_at(&meta, &records).await;
    (meta, fx)
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
    // Allocator refcount recovery exactly like a mount.
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
    let fs = Arc::new(fs);
    let fabric = JobFabric::start(
        routed.clone(),
        2,
        100,
        Some(MoverCtx::new(fs.router.clone(), fs.mover_quiesce_probe())),
    )
    .await
    .expect("fabric start");
    fs.job_fabric.store(Arc::new(Some(fabric.clone())));
    Fx {
        fs,
        meta: routed,
        fabric,
        records: records.to_vec(),
        staging_path,
        _staging: staging,
    }
}

impl Fx {
    /// Mount-faithful close: the dismount seal first (every open pack's
    /// pin releases and its pack-open ledger entry — process-global —
    /// leaves with it), then the reclaim drain, then the volumes.
    async fn close(self) {
        self.fs.router.seal_open_packs().await;
        self.fabric.shutdown_abrupt().await;
        self.fs.router.backend_router.reclaim_drain().await;
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }

    /// Kill-9 analog (the VL4 crash shape): nothing seals, nothing drains
    /// — the worker future is dropped wherever it parked. The
    /// process-global pack-open ledger is cleared the way a real kill-9
    /// loses it with the process (a fresh fixture mints the same stamped
    /// base keys, so a stale entry would read as an open pack later).
    async fn crash(self) {
        self.fabric.shutdown_abrupt().await;
        for vol in &self.meta.volumes {
            let _ = vol.shutdown().await;
        }
        squeezefs::jobs::test_pack_ledger_clear();
    }

    fn alloc(&self, idx: usize) -> Arc<BlockAllocator> {
        self.fs
            .router
            .backend_router
            .backends
            .get(&self.records[idx].id)
            .expect("registered backend")
            .block_allocator
            .clone()
    }

    /// Force every NEW placement onto volume `idx`.
    fn place_only_on(&self, idx: usize) {
        for (i, rec) in self.records.iter().enumerate() {
            self.fs
                .router
                .backend_router
                .set_health_override(&rec.id, i != idx)
                .unwrap();
        }
    }

    fn clear_health_overrides(&self) {
        for rec in &self.records {
            self.fs
                .router
                .backend_router
                .set_health_override(&rec.id, false)
                .unwrap();
        }
    }

    fn fsck_ctx(&self) -> squeezefs::fsck::FsckCtx {
        squeezefs::fsck::FsckCtx {
            meta: self.meta.clone(),
            router: self.fs.router.clone(),
            staging_dirs: vec![self.staging_path.clone()],
            expected_generation: None,
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

    async fn read(&self, ino: u64, len: usize) -> Vec<u8> {
        self.fs
            .read(req(), ino, 0, 0, len as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("read ino {ino} failed: {e:?}"))
            .data
            .to_vec()
    }

    /// SETATTR(size) — the truncate the kernel issues.
    async fn truncate(&self, ino: u64, size: u64) {
        self.fs
            .setattr(
                req(),
                ino,
                None,
                fuse3::SetAttr {
                    size: Some(size),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("truncate ino {ino} to {size} failed: {e:?}"));
    }

    /// `clone_file` — a PROMOTED source's clone SHARES its mapping
    /// verbatim (+1 durable reference, +1 RAM pin).
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

    /// Unlink + reclaim: block frees are deferred to FORGET + the reclaim
    /// pool (armed at FUSE INIT, which this harness never runs) — drive the
    /// batch entry point directly, exactly what the kernel's forget reaches.
    async fn unlink(&self, name: &str, ino: u64) {
        let _ = self.fs.release(req(), ino, 0, 0, 0, true).await;
        self.fs
            .unlink(req(), 1, OsStr::new(name))
            .await
            .unwrap_or_else(|e| panic!("unlink {name} failed: {e:?}"));
        self.fs.reclaim_orphaned_batch(vec![ino]).await;
    }

    /// A staged-layout file of `len` bytes (resident in the ring, no map).
    async fn staged_file(&self, name: &str, len: usize, tag: usize) -> (u64, String) {
        let ino = self.create(name).await;
        self.write_at(ino, 0, &pattern(tag, len)).await;
        let m = self.fs.router.metadata_cache.get(&ino).expect("RAM layout");
        assert_eq!(m.file_type, "staged", "fixture premise: staged layout");
        let fid = m.file_id.as_deref().expect("file_id").to_string();
        assert!(
            self.fs.router.cache.nvme.read_staged(&fid).is_some(),
            "fixture premise: ring-resident"
        );
        (ino, fid)
    }

    /// A staged file fsync-promoted — into the open pack under the lever,
    /// into its own block without it.
    async fn promoted_file(&self, name: &str, len: usize, tag: usize) -> u64 {
        let (ino, fid) = self.staged_file(name, len, tag).await;
        self.fsync(ino).await;
        assert!(
            self.fs.router.cache.nvme.read_staged(&fid).is_none(),
            "fixture premise: the promotion released the ring entry"
        );
        ino
    }

    /// A striped file of `nblocks` whole blocks, fsync'd — bare mappings,
    /// never a pack block.
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

    /// `block_map[0]` verbatim — the RAM layout, or the durable one on a
    /// fresh reopen.
    async fn mapping_str(&self, ino: u64) -> String {
        let m = self
            .fs
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .unwrap_or_else(|e| panic!("layout of ino {ino}: {e:?}"));
        m.block_map
            .as_ref()
            .and_then(|bm| bm.get(&0).cloned())
            .unwrap_or_else(|| panic!("ino {ino} has no block_map[0]: {m:?}"))
    }

    /// The tenant mapping `block_map[0]` decoded: `(base key, off, len)`.
    async fn mapping(&self, ino: u64) -> (String, u64, usize) {
        let s = self.mapping_str(ino).await;
        let (prefix, rest) = match s.find("://") {
            Some(p) => s.split_at(p + 3),
            None => ("", s.as_str()),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        assert_eq!(parts.len(), 3, "size-carrying mapping: {s}");
        (
            format!("{prefix}{}", parts[0]),
            parts[1].parse().unwrap(),
            parts[2].parse().unwrap(),
        )
    }

    /// The device offset a base key names (through the router's parser).
    fn offset_of(&self, base_key: &str) -> u64 {
        self.fs
            .router
            .backend_router
            .parse_block_offset(base_key)
            .expect("base key parses")
    }

    /// The C8 oracle: durable vs derived, the drifting blocks.
    async fn drift(&self) -> Vec<(String, u64, u32, u32)> {
        self.fs
            .router
            .backend_router
            .verify_durable_block_refs(&self.meta)
            .await
            .expect("verification pass")
    }

    /// The online fsck (C1–C12) with a short settle; `findings` must be
    /// empty on every healthy population.
    async fn fsck_findings(&self) -> Vec<squeezefs::fsck::FsckFinding> {
        let mut opts = squeezefs::fsck::FsckOptions::online();
        opts.settle = Duration::from_millis(100);
        squeezefs::fsck::run(&self.fsck_ctx(), &opts)
            .await
            .expect("fsck")
            .findings
    }

    /// The compaction plan's measurement face.
    async fn pack_report(&self) -> squeezefs::defrag::PackReport {
        squeezefs::defrag::measure_pack(&self.meta, &self.fs.router)
            .await
            .expect("pack occupancy measurement")
    }

    /// Submit `defrag --pack` (whole set) and wait for its terminal state.
    async fn compact(&self) -> JobState {
        let id = self
            .fabric
            .submit(JobSpec {
                job_type: JobType::DefragPack { volume_id: None },
                throttle_pct: 100,
            })
            .await
            .expect("submit defrag --pack");
        self.fabric
            .wait_terminal(&id, Duration::from_secs(180))
            .await
            .expect("terminal")
    }

    /// Seal the open pack (the dismount seal) so the next promotion opens
    /// a fresh one — the way a population lands in DISTINCT sealed blocks.
    async fn seal(&self) {
        self.fs.router.seal_open_packs().await;
    }

    async fn assert_clean(&self, what: &str) {
        let drift = self.drift().await;
        assert!(
            drift.is_empty(),
            "{what}: the C8 oracle must be clean — drifting (vol, offset, durable, derived): \
             {drift:?}"
        );
        let findings = self.fsck_findings().await;
        assert!(
            findings.is_empty(),
            "{what}: fsck_findings must be 0 (C12 included), got {findings:?}"
        );
    }
}

/// A snapshot of the PK6 counters — every contract asserts DELTAS.
#[derive(Clone, Copy, Debug)]
struct Counters {
    compactions: u64,
    tenants_moved: u64,
    windows_copied: u64,
    bytes_copied: u64,
    bytes_reclaimed: u64,
    blocks_freed: u64,
    deferred: u64,
    open_defers: u64,
    resident_defers: u64,
    slots_abandoned: u64,
    double_frees: u64,
    untracked_refusals: u64,
}

fn counters() -> Counters {
    Counters {
        compactions: metric(&METRICS.pack_compactions),
        tenants_moved: metric(&METRICS.pack_compaction_tenants_moved),
        windows_copied: metric(&METRICS.pack_compaction_windows_copied),
        bytes_copied: metric(&METRICS.pack_compaction_bytes_copied),
        bytes_reclaimed: metric(&METRICS.pack_compaction_bytes_reclaimed),
        blocks_freed: metric(&METRICS.pack_compaction_blocks_freed),
        deferred: metric(&METRICS.pack_compaction_deferred),
        open_defers: metric(&METRICS.pack_mover_open_defers),
        resident_defers: metric(&METRICS.pack_mover_resident_defers),
        slots_abandoned: metric(&METRICS.pack_slots_abandoned),
        double_frees: metric(&METRICS.block_double_frees),
        untracked_refusals: metric(&METRICS.block_untracked_free_refusals),
    }
}

fn assert_no_forensics(before: Counters) {
    let now = counters();
    assert_eq!(
        now.double_frees, before.double_frees,
        "block_double_frees must stay flat"
    );
    assert_eq!(
        now.untracked_refusals, before.untracked_refusals,
        "block_untracked_free_refusals must stay flat"
    );
}

// ---------------------------------------------------------------------------
// Contract 1: the occupancy derivation is exact, both share classes once
// ---------------------------------------------------------------------------

/// One pack block holds a (16 KiB), b (8 KiB), c (20 KiB), d = clone(a)
/// (the identical-window share) and e = clone(a) clipped to 9 000 B (the
/// nested prefix share): five tenants, THREE windows, live bytes
/// 16 + 8 + 20 KiB. A two-block striped file beside it is not a pack
/// block. The measurement reads the same before and after the seal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn occupancy_is_exact_on_a_planted_population_and_counts_both_share_classes_once() {
    let _g = serial().await;
    let _l = arm_levers(true);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "occupancy").await;

    fx.striped_file("striped.bin", 2).await;
    let a = fx.promoted_file("a.bin", 16 * KIB, 1).await;
    let b = fx.promoted_file("b.bin", 8 * KIB, 2).await;
    let c = fx.promoted_file("c.bin", 20 * KIB, 3).await;
    let d = fx.clone(a, "d.bin").await;
    let e = fx.clone(a, "e.bin").await;
    fx.truncate(e, 9_000).await;

    let (base, off_a, _) = fx.mapping(a).await;
    for ino in [b, c, d, e] {
        assert_eq!(fx.mapping(ino).await.0, base, "one pack block");
    }
    assert_eq!(fx.mapping(d).await, (base.clone(), off_a, 16 * KIB));
    assert_eq!(fx.mapping(e).await, (base.clone(), off_a, 9_000));
    let live = pack_slot_len(16 * KIB as u64)
        + pack_slot_len(8 * KIB as u64)
        + pack_slot_len(20 * KIB as u64);
    assert!(
        squeezefs::defrag::is_pack_victim(live),
        "{live} B live is below the derived half-chunk threshold"
    );

    let check = |report: &squeezefs::defrag::PackReport, when: &str| {
        assert_eq!(
            report.blocks, 1,
            "{when}: exactly one pack block (the striped file is not one)"
        );
        assert_eq!(report.below_half, 1, "{when}: it is a victim");
        assert_eq!(
            report.live_bytes, live,
            "{when}: live bytes = Σ slot(max len at each off)"
        );
        assert_eq!(
            report.reclaimable_bytes,
            CHUNK_SIZE - live,
            "{when}: reclaimable = CHUNK − live"
        );
        let occupancy = live as f64 / CHUNK_SIZE as f64;
        assert!(
            (report.worst_occupancy - occupancy).abs() < 1e-9,
            "{when}: worst occupancy"
        );
        assert!(
            (report.mean_occupancy - occupancy).abs() < 1e-9,
            "{when}: mean occupancy"
        );
        let row = &report.rows[0];
        assert_eq!(row.tenants, 5, "{when}: five referencers");
        assert_eq!(row.windows, 3, "{when}: three distinct windows — the clone share and the nested prefix share collapse onto a's");
        assert_eq!(row.live_bytes, live);
        assert!(row.victim);
        assert_eq!(fx.offset_of(&row.base_key), fx.offset_of(&base));
    };
    check(&fx.pack_report().await, "open pack");
    fx.seal().await;
    check(&fx.pack_report().await, "sealed pack");

    // The full report carries the pack face; the gauges publish from it.
    let report = squeezefs::defrag::measure(&fx.meta, &fx.fs.router)
        .await
        .expect("four-axis report");
    assert_eq!(report.pack.blocks, 1);
    assert_eq!(metric(&METRICS.pack_blocks_below_half), 1);
    assert_eq!(metric(&METRICS.pack_reclaimable_bytes), CHUNK_SIZE - live);
    let worst =
        squeezefs::defrag::decode_ratio(metric(&METRICS.frag_d1_pack_occupancy)).expect("measured");
    assert!((worst - live as f64 / CHUNK_SIZE as f64).abs() < 0.001);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 2: victims below half re-pack and free; above half untouched
// ---------------------------------------------------------------------------

/// Sealed pack P1 {a, b} (32 KiB live) and P2 {c} (16 KiB live) are
/// victims; sealed P3 {t1, t2} (2 × 1.5 MiB = 3 MiB live) is above half.
/// `defrag --pack` re-packs a, b, c into ONE fresh open pack (distinct
/// slots), frees P1 and P2 through the terminal ladder (2 freed, 1
/// consumed — the net block the plan promised), leaves P3 byte-identical,
/// keeps `copied ≤ reclaimed`, and the C8 oracle + C12 read clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn victims_below_half_repack_into_one_fresh_pack_and_free_while_a_block_above_half_is_untouched(
) {
    let _g = serial().await;
    let _l = arm_levers(true);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "victims").await;
    let alloc = fx.alloc(0);

    let a = fx.promoted_file("a.bin", 16 * KIB, 11).await;
    let b = fx.promoted_file("b.bin", 16 * KIB, 12).await;
    fx.seal().await;
    let c = fx.promoted_file("c.bin", 16 * KIB, 13).await;
    fx.seal().await;
    let big = 3 * KIB * KIB / 2;
    let t1 = fx.promoted_file("t1.bin", big, 14).await;
    let t2 = fx.promoted_file("t2.bin", big, 15).await;
    fx.seal().await;

    let (p1, _, _) = fx.mapping(a).await;
    assert_eq!(fx.mapping(b).await.0, p1, "a and b share P1");
    let (p2, _, _) = fx.mapping(c).await;
    let (p3, off_t1, _) = fx.mapping(t1).await;
    let (p3b, off_t2, _) = fx.mapping(t2).await;
    assert_eq!(p3, p3b, "t1 and t2 share P3");
    assert_eq!(alloc.get_used_blocks(), 3, "three sealed packs");
    let report = fx.pack_report().await;
    assert_eq!(report.blocks, 3);
    assert_eq!(
        report.below_half, 2,
        "P1 and P2 are victims; P3 is above half"
    );

    let c0 = counters();
    assert_eq!(fx.compact().await, JobState::Completed);
    let c1 = counters();

    // The victims' tenants share ONE fresh pack at distinct slots.
    let (dst, off_a, len_a) = fx.mapping(a).await;
    let (dst_b, off_b, _) = fx.mapping(b).await;
    let (dst_c, off_c, _) = fx.mapping(c).await;
    assert_eq!(len_a, 16 * KIB, "the tenant keeps its own len");
    assert_eq!(dst, dst_b);
    assert_eq!(dst, dst_c);
    assert_ne!(fx.offset_of(&dst), fx.offset_of(&p1));
    assert_ne!(fx.offset_of(&dst), fx.offset_of(&p2));
    let mut offs = vec![off_a, off_b, off_c];
    offs.sort_unstable();
    offs.dedup();
    assert_eq!(offs.len(), 3, "distinct destination slots");
    // P3 untouched: same base, same slots, refcount 2.
    assert_eq!(fx.mapping(t1).await, (p3.clone(), off_t1, big));
    assert_eq!(fx.mapping(t2).await, (p3.clone(), off_t2, big));
    assert_eq!(alloc.refcount(fx.offset_of(&p3)), Some(2));
    // The victims freed through the ladder (population 0 → terminal):
    // no longer allocator-tracked, back on the free list after the
    // mover's convergence drain.
    assert_eq!(alloc.refcount(fx.offset_of(&p1)), None, "P1 freed");
    assert_eq!(alloc.refcount(fx.offset_of(&p2)), None, "P2 freed");
    assert_eq!(
        alloc.get_used_blocks(),
        2,
        "P3 + the destination pack: two freed, one consumed"
    );
    assert_eq!(
        alloc.refcount(fx.offset_of(&dst)),
        Some(3 + 1),
        "the destination is the OPEN pack: three tenants + the packer's pin"
    );

    // Engagement, exact.
    assert_eq!(
        c1.compactions - c0.compactions,
        1,
        "one pass executed a plan"
    );
    assert_eq!(c1.tenants_moved - c0.tenants_moved, 3);
    assert_eq!(c1.windows_copied - c0.windows_copied, 3);
    assert_eq!(c1.bytes_copied - c0.bytes_copied, 3 * 16 * KIB as u64);
    assert_eq!(c1.blocks_freed - c0.blocks_freed, 2);
    assert_eq!(
        c1.bytes_reclaimed - c0.bytes_reclaimed,
        (CHUNK_SIZE - 32 * KIB as u64) + (CHUNK_SIZE - 16 * KIB as u64),
        "reclaimed = Σ (CHUNK − live) over the freed victims"
    );
    assert!(
        c1.bytes_copied - c0.bytes_copied <= c1.bytes_reclaimed - c0.bytes_reclaimed,
        "the break-even law: copied ≤ reclaimed"
    );
    assert_eq!(c1.slots_abandoned, c0.slots_abandoned);
    assert_no_forensics(c0);

    for (ino, tag) in [(a, 11), (b, 12), (c, 13)] {
        assert_eq!(
            fx.read(ino, 16 * KIB).await,
            pattern(tag, 16 * KIB),
            "tenant {ino}"
        );
    }
    assert_eq!(fx.read(t1, big).await, pattern(14, big));
    assert_eq!(fx.read(t2, big).await, pattern(15, big));
    fx.assert_clean("after the compaction pass").await;

    // A second pass moves nothing: the OPEN pack (now the only low block)
    // is deferred, never touched (`pack_mover_open_defers` moves).
    let c2 = counters();
    assert_eq!(fx.compact().await, JobState::Completed);
    let c3 = counters();
    assert_eq!(c3.tenants_moved, c2.tenants_moved, "nothing moved");
    assert_eq!(c3.blocks_freed, c2.blocks_freed);
    assert!(
        c3.open_defers > c2.open_defers,
        "the open pack is deferred, not compacted into itself"
    );
    assert_eq!(
        alloc.refcount(fx.offset_of(&dst)),
        Some(3 + 1),
        "the open pack is untouched"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 3: the legacy one-block-per-file population is the first customer
// ---------------------------------------------------------------------------

/// Six 64 KiB files promoted with the lever OFF take six blocks
/// (`bk:0:65536`, one tenant each — the 64× law); the measurement reads
/// six victims; under the lever ON `defrag --pack` re-packs them into ONE
/// block, frees six, and every file reads byte-exact with the oracle clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_legacy_one_block_per_file_population_is_compacted_into_packs() {
    let _g = serial().await;
    let _l = arm_levers(false);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "legacy").await;
    let alloc = fx.alloc(0);

    const N: usize = 6;
    let len = 64 * KIB;
    let block0 = metric(&METRICS.layout_promoted_block);
    let mut inos = Vec::new();
    let mut bases = Vec::new();
    for i in 0..N {
        let ino = fx
            .promoted_file(&format!("legacy{i}.bin"), len, 20 + i)
            .await;
        let (base, off, l) = fx.mapping(ino).await;
        assert_eq!((off, l), (0, len), "the legacy shape: bk:0:len");
        inos.push(ino);
        bases.push(base);
    }
    assert_eq!(metric(&METRICS.layout_promoted_block) - block0, N as u64);
    assert_eq!(alloc.get_used_blocks(), N as u64, "one block per file");
    let report = fx.pack_report().await;
    assert_eq!(
        report.blocks, N as u64,
        "every one-tenant block is a pack block"
    );
    assert_eq!(
        report.below_half, N as u64,
        "…at occupancy len/CHUNK — a victim"
    );
    assert_eq!(
        report.reclaimable_bytes,
        N as u64 * (CHUNK_SIZE - len as u64)
    );

    // The lever ON: compaction's first customer.
    squeezefs::routing::test_set_small_file_packing(Some(true));
    let c0 = counters();
    assert_eq!(fx.compact().await, JobState::Completed);
    let c1 = counters();
    assert_eq!(c1.blocks_freed - c0.blocks_freed, N as u64);
    assert_eq!(c1.tenants_moved - c0.tenants_moved, N as u64);
    assert_eq!(c1.windows_copied - c0.windows_copied, N as u64);
    assert_eq!(c1.bytes_copied - c0.bytes_copied, (N * len) as u64);
    assert_no_forensics(c0);
    assert_eq!(
        alloc.get_used_blocks(),
        1,
        "the population fits ONE pack: {N} blocks → 1"
    );
    let (dst, _, _) = fx.mapping(inos[0]).await;
    for (i, ino) in inos.iter().enumerate() {
        let (b, off, l) = fx.mapping(*ino).await;
        assert_eq!(b, dst, "every file names the one pack");
        assert_eq!(l, len);
        assert_eq!(off % 4096, 0);
        assert_eq!(fx.read(*ino, len).await, pattern(20 + i, len), "file {i}");
    }
    for base in &bases {
        assert_eq!(
            alloc.refcount(fx.offset_of(base)),
            None,
            "legacy block freed"
        );
    }
    let report = fx.pack_report().await;
    assert_eq!(report.blocks, 1);
    assert_eq!(report.live_bytes, (N * len) as u64);
    fx.assert_clean("after compacting the legacy population")
        .await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contracts 4 + 5: the two share classes survive compaction, copied once
// ---------------------------------------------------------------------------

/// Victim P1 {a, d = clone(a)} and victim P2 {b}: the clone share is
/// copied ONCE (`windows_copied = 2 < tenants_moved = 3`), a and d land on
/// the SAME destination window, the destination carries a reference per
/// referencer, both read byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_share_is_copied_once_and_both_referencers_land_on_one_destination_window() {
    let _g = serial().await;
    let _l = arm_levers(true);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "clone").await;
    let alloc = fx.alloc(0);

    let a = fx.promoted_file("a.bin", 16 * KIB, 31).await;
    let d = fx.clone(a, "d.bin").await;
    fx.seal().await;
    let b = fx.promoted_file("b.bin", 16 * KIB, 32).await;
    fx.seal().await;
    let (p1, off_a, _) = fx.mapping(a).await;
    assert_eq!(
        fx.mapping(d).await,
        (p1.clone(), off_a, 16 * KIB),
        "identical windows"
    );
    assert_eq!(alloc.refcount(fx.offset_of(&p1)), Some(2));

    let c0 = counters();
    assert_eq!(fx.compact().await, JobState::Completed);
    let c1 = counters();
    assert_eq!(c1.tenants_moved - c0.tenants_moved, 3, "a, d, b");
    assert_eq!(
        c1.windows_copied - c0.windows_copied,
        2,
        "a's window once, b's once"
    );
    assert!(c1.windows_copied - c0.windows_copied < c1.tenants_moved - c0.tenants_moved);
    assert_eq!(c1.bytes_copied - c0.bytes_copied, 2 * 16 * KIB as u64);
    assert_eq!(c1.blocks_freed - c0.blocks_freed, 2);
    assert_no_forensics(c0);

    let (dst, off_a2, len_a) = fx.mapping(a).await;
    assert_eq!(
        fx.mapping(d).await,
        (dst.clone(), off_a2, len_a),
        "the clone lands on a's window"
    );
    let (dst_b, off_b, _) = fx.mapping(b).await;
    assert_eq!(dst_b, dst);
    assert_ne!(off_b, off_a2);
    assert_eq!(
        alloc.refcount(fx.offset_of(&dst)),
        Some(3 + 1),
        "a, d, b + the open pack's pin"
    );
    assert_eq!(alloc.refcount(fx.offset_of(&p1)), None, "P1 freed");
    assert_eq!(fx.read(a, 16 * KIB).await, pattern(31, 16 * KIB));
    assert_eq!(fx.read(d, 16 * KIB).await, pattern(31, 16 * KIB));
    assert_eq!(fx.read(b, 16 * KIB).await, pattern(32, 16 * KIB));
    fx.assert_clean("clone share compacted").await;
    fx.close().await;
}

/// Victim P1 {a (16 KiB), e = clone(a) clipped to 9 000 B} and victim P2
/// {b}: the nested prefix share is copied ONCE at the LONGER len; a and e
/// land at the same destination `off'`, each with its own `len`
/// (`dst:off':16384` and `dst:off':9000`), both byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nested_prefix_share_is_copied_once_at_the_longer_len() {
    let _g = serial().await;
    let _l = arm_levers(true);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "nested").await;
    let alloc = fx.alloc(0);

    let a = fx.promoted_file("a.bin", 16 * KIB, 41).await;
    let e = fx.clone(a, "e.bin").await;
    fx.truncate(e, 9_000).await;
    fx.seal().await;
    let b = fx.promoted_file("b.bin", 16 * KIB, 42).await;
    fx.seal().await;
    let (p1, off_a, _) = fx.mapping(a).await;
    assert_eq!(
        fx.mapping(e).await,
        (p1.clone(), off_a, 9_000),
        "the nested prefix share"
    );

    let c0 = counters();
    assert_eq!(fx.compact().await, JobState::Completed);
    let c1 = counters();
    assert_eq!(c1.tenants_moved - c0.tenants_moved, 3, "a, e, b");
    assert_eq!(
        c1.windows_copied - c0.windows_copied,
        2,
        "a's window once (at 16 KiB), b's once"
    );
    assert_eq!(
        c1.bytes_copied - c0.bytes_copied,
        2 * 16 * KIB as u64,
        "the shared window is copied at max(len)"
    );
    assert_no_forensics(c0);

    let (dst, off_a2, len_a) = fx.mapping(a).await;
    assert_eq!(len_a, 16 * KIB);
    assert_eq!(
        fx.mapping(e).await,
        (dst.clone(), off_a2, 9_000),
        "the clip lands at a's off' with its OWN len"
    );
    assert_eq!(fx.mapping(b).await.0, dst);
    assert_eq!(alloc.refcount(fx.offset_of(&dst)), Some(3 + 1));
    assert_eq!(alloc.refcount(fx.offset_of(&p1)), None, "P1 freed");
    assert_eq!(fx.read(a, 16 * KIB).await, pattern(41, 16 * KIB));
    assert_eq!(
        fx.read(e, 9_000).await,
        pattern(41, 16 * KIB)[..9_000].to_vec()
    );
    assert_eq!(fx.read(b, 16 * KIB).await, pattern(42, 16 * KIB));
    fx.assert_clean("nested prefix share compacted").await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 6: a re-staged tenant defers its victim, never copied
// ---------------------------------------------------------------------------

/// Victims P1 {a, b} and P2 {c}; a is overwritten (re-staged, ring-
/// resident, no fsync) before the pass. P1 DEFERS (the resident-ring
/// clause — `pack_mover_resident_defers` / `pack_compaction_deferred`
/// move), P2 moves; a reads its NEW content, b stays on P1 byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_re_staged_mid_plan_is_deferred_not_copied() {
    let _g = serial().await;
    let _l = arm_levers(true);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "restaged").await;
    let alloc = fx.alloc(0);

    let a = fx.promoted_file("a.bin", 16 * KIB, 51).await;
    let b = fx.promoted_file("b.bin", 16 * KIB, 52).await;
    fx.seal().await;
    let c = fx.promoted_file("c.bin", 16 * KIB, 53).await;
    fx.seal().await;
    let (p1, off_b, _) = fx.mapping(b).await;
    let (p2, _, _) = fx.mapping(c).await;

    // The overwrite re-stages a's image: resident again, a newer image
    // about to supersede the durable tenant.
    fx.write_at(a, 0, &pattern(61, 16 * KIB)).await;
    assert!(
        fx.fs.router.staged_tenant_ring_resident(a, 0),
        "fixture premise: a is ring-resident"
    );

    let c0 = counters();
    assert_eq!(fx.compact().await, JobState::Completed);
    let c1 = counters();
    assert!(
        c1.deferred > c0.deferred,
        "P1 deferred (pack_compaction_deferred)"
    );
    assert!(
        c1.resident_defers > c0.resident_defers,
        "the resident-ring clause fired (pack_mover_resident_defers)"
    );
    assert_eq!(c1.tenants_moved - c0.tenants_moved, 1, "only c moved");
    assert_eq!(c1.blocks_freed - c0.blocks_freed, 1, "only P2 freed");
    assert_no_forensics(c0);

    assert_eq!(
        fx.mapping(b).await,
        (p1.clone(), off_b, 16 * KIB),
        "b stays on P1"
    );
    assert!(
        alloc.refcount(fx.offset_of(&p1)).is_some(),
        "P1 stays allocated"
    );
    assert_eq!(alloc.refcount(fx.offset_of(&p2)), None, "P2 freed");
    assert_eq!(
        fx.read(a, 16 * KIB).await,
        pattern(61, 16 * KIB),
        "a reads its NEW content"
    );
    assert_eq!(fx.read(b, 16 * KIB).await, pattern(52, 16 * KIB));
    assert_eq!(fx.read(c, 16 * KIB).await, pattern(53, 16 * KIB));

    // Once a promotes again, P1 becomes movable: b re-packs.
    fx.fsync(a).await;
    fx.seal().await;
    let c2 = counters();
    assert_eq!(fx.compact().await, JobState::Completed);
    assert!(metric(&METRICS.pack_compaction_tenants_moved) > c2.tenants_moved);
    assert_eq!(fx.read(a, 16 * KIB).await, pattern(61, 16 * KIB));
    assert_eq!(fx.read(b, 16 * KIB).await, pattern(52, 16 * KIB));
    fx.assert_clean("after the deferred victim re-packed").await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// FIND-PK-5 (found by contract 6): an overwritten tenant re-promoted into
// ANOTHER block must release its old durable reference
// ---------------------------------------------------------------------------

/// The staged RMW re-stages a promoted tenant (`block_map: None`, dirty)
/// and frees its old slot's RAM reference — but the durable `−ref` was
/// never NOTED for the save that persists the dirty layout, so a later
/// promotion (whose swap is computed against the RAM map) orphaned the
/// old C8 record whenever the new tenant landed in a DIFFERENT block
/// (same block ⇒ same record KEY ⇒ the `+ref` overwrote it, which is why
/// the open-pack overwrite contract never saw it). Compaction makes the
/// different-block case the common one: the block a re-promotion lands in
/// is whichever pack is open. Pinned here as the shape compaction depends
/// on — sealed pack {a, b}; overwrite a; fsync a (re-promoted into the
/// fresh open pack): the oracle is clean, P1's population is exactly {b}.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_overwritten_tenant_re_promoted_into_another_block_releases_its_old_reference() {
    let _g = serial().await;
    let _l = arm_levers(true);
    let dir = tempfile::tempdir().unwrap();
    let (meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "findpk5").await;
    let alloc = fx.alloc(0);

    let a = fx.promoted_file("a.bin", 16 * KIB, 151).await;
    let b = fx.promoted_file("b.bin", 16 * KIB, 152).await;
    fx.seal().await;
    let (p1, _, _) = fx.mapping(a).await;
    assert_eq!(alloc.refcount(fx.offset_of(&p1)), Some(2));

    fx.write_at(a, 0, &pattern(161, 16 * KIB)).await;
    assert_eq!(
        alloc.refcount(fx.offset_of(&p1)),
        Some(1),
        "a's old slot released (RAM)"
    );
    fx.fsync(a).await;
    let (dst, _, _) = fx.mapping(a).await;
    assert_ne!(
        fx.offset_of(&dst),
        fx.offset_of(&p1),
        "re-promoted into a fresh pack"
    );
    assert!(
        fx.drift().await.is_empty(),
        "the old tenant's durable record must leave with its RAM reference: {:?}",
        fx.drift().await
    );
    assert_eq!(fx.read(a, 16 * KIB).await, pattern(161, 16 * KIB));
    assert_eq!(fx.read(b, 16 * KIB).await, pattern(152, 16 * KIB));

    // The remount face: the ledger seeds P1 at exactly {b}, never {a, b}
    // — a stale record would pin P1 one above its tenants forever.
    let records = fx.records.clone();
    fx.close().await;
    let fx = open_at(&meta, &records).await;
    assert_eq!(
        fx.alloc(0).refcount(fx.offset_of(&p1)),
        Some(1),
        "after remount P1's population is b alone"
    );
    assert!(fx.drift().await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 7: a refused publish releases exactly one destination reference
// ---------------------------------------------------------------------------

/// Victims P1 {a, b} and P2 {c}. The pre-publish hook parks the mover at
/// b's publish while the test UNLINKS b: the publish is refused (the
/// mapping is gone), b's destination reference is released (the open pack
/// reads `a + c + pin = 3`, never 4), b's slot is abandoned, no double
/// free, P1 still frees through a's move and b's delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_publish_releases_exactly_one_destination_reference() {
    let _g = serial().await;
    let _l = arm_levers(true);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "refused").await;
    let alloc = fx.alloc(0);

    let a = fx.promoted_file("a.bin", 16 * KIB, 71).await;
    let b = fx.promoted_file("b.bin", 16 * KIB, 72).await;
    fx.seal().await;
    let c = fx.promoted_file("c.bin", 16 * KIB, 73).await;
    fx.seal().await;
    let (p1, _, _) = fx.mapping(a).await;
    let (p2, _, _) = fx.mapping(c).await;

    // Park at b's publish; the test unlinks b, then releases the hook.
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let fired = AtomicBool::new(false);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |ino, _b| {
        if ino == b && !fired.swap(true, Ordering::SeqCst) {
            let _ = hit_tx.send(());
            let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60));
        }
    }));
    let c0 = counters();
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragPack { volume_id: None },
            throttle_pct: 100,
        })
        .await
        .expect("submit");
    tokio::task::spawn_blocking(move || hit_rx.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("the mover must reach b's publish window");
    fx.unlink("b.bin", b).await;
    let _ = go_tx.send(());
    let end = fx
        .fabric
        .wait_terminal(&job_id, Duration::from_secs(180))
        .await
        .expect("terminal");
    squeezefs::jobs::clear_evacuate_pre_publish_hook();
    assert_eq!(end, JobState::Completed);
    let c1 = counters();

    assert_eq!(
        c1.tenants_moved - c0.tenants_moved,
        2,
        "a and c moved; b refused"
    );
    assert_eq!(
        c1.windows_copied - c0.windows_copied,
        2,
        "a's and c's windows committed"
    );
    assert_eq!(
        c1.slots_abandoned - c0.slots_abandoned,
        1,
        "b's copied slot is dead space (its reference released)"
    );
    assert_eq!(
        c1.blocks_freed - c0.blocks_freed,
        2,
        "P1 (a moved, b deleted) and P2 freed"
    );
    assert_no_forensics(c0);
    let (dst, _, _) = fx.mapping(a).await;
    assert_eq!(fx.mapping(c).await.0, dst);
    assert_eq!(
        alloc.refcount(fx.offset_of(&dst)),
        Some(2 + 1),
        "a + c + the pin — b's raised reference was released exactly once"
    );
    assert_eq!(alloc.refcount(fx.offset_of(&p1)), None);
    assert_eq!(alloc.refcount(fx.offset_of(&p2)), None);
    assert_eq!(fx.read(a, 16 * KIB).await, pattern(71, 16 * KIB));
    assert_eq!(fx.read(c, 16 * KIB).await, pattern(73, 16 * KIB));
    fx.assert_clean("after a refused publish").await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 8 💥: kill-9 mid-pass — no leak, no double free, re-run converges
// ---------------------------------------------------------------------------

/// Six legacy victims; the mover is parked at its FIRST publish (one
/// window already DMA'd into a fresh pack, nothing durable names it) and
/// the fixture crashes. The reopen recovers the victims at their tenant
/// counts and the never-committed pack FREE (§5.4), the fabric ADOPTS the
/// durable job, the re-plan converges (KD-6), every file byte-exact, the
/// oracle clean, `block_double_frees` / `block_untracked_free_refusals`
/// flat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill9_mid_pass_leaves_no_leak_and_no_double_free_and_the_rerun_converges() {
    let _g = serial().await;
    let _l = arm_levers(false);
    let dir = tempfile::tempdir().unwrap();
    let (meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "crash").await;

    const N: usize = 6;
    let len = 32 * KIB;
    let mut inos = Vec::new();
    for i in 0..N {
        inos.push(fx.promoted_file(&format!("k{i}.bin"), len, 80 + i).await);
    }
    assert_eq!(fx.alloc(0).get_used_blocks(), N as u64);
    squeezefs::routing::test_set_small_file_packing(Some(true));

    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let fired = AtomicBool::new(false);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
        if !fired.swap(true, Ordering::SeqCst) {
            let _ = hit_tx.send(());
            // KEPT sleep — the hang simulator: the mover is parked
            // forever-in-practice so the crash lands mid-publish; the test
            // proceeds on `hit_rx` and pays no wall clock for it.
            std::thread::sleep(Duration::from_secs(120));
        }
    }));
    let c0 = counters();
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragPack { volume_id: None },
            throttle_pct: 100,
        })
        .await
        .expect("submit");
    tokio::task::spawn_blocking(move || hit_rx.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("the mover must reach its parked publish");
    let records = fx.records.clone();
    fx.crash().await;
    squeezefs::jobs::clear_evacuate_pre_publish_hook();

    // Reopen: the durable record is adopted and re-planned from scratch.
    let fx = open_at(&meta, &records).await;
    assert_eq!(
        fx.alloc(0).get_used_blocks(),
        N as u64,
        "the never-committed pack recovered FREE; the victims at their tenants"
    );
    assert!(fx.drift().await.is_empty(), "the oracle is clean at reopen");
    let end = fx
        .fabric
        .wait_terminal(&job_id, Duration::from_secs(180))
        .await
        .expect("the adopted job must converge");
    assert_eq!(end, JobState::Completed, "re-run converges (KD-6)");
    assert_eq!(fx.alloc(0).get_used_blocks(), 1, "six blocks → one pack");
    let c1 = counters();
    assert_eq!(c1.blocks_freed - c0.blocks_freed, N as u64);
    assert_no_forensics(c0);
    for (i, ino) in inos.iter().enumerate() {
        assert_eq!(fx.read(*ino, len).await, pattern(80 + i, len), "file {i}");
    }
    fx.assert_clean("after the crash-resumed compaction").await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 9: throttle law, pause/resume, cancel
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_is_throttled_pause_resume_and_cancel_able() {
    let _g = serial().await;
    let _l = arm_levers(false);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "throttle").await;
    let len = 16 * KIB;
    let mut inos = Vec::new();
    for i in 0..8 {
        inos.push(fx.promoted_file(&format!("t{i}.bin"), len, 90 + i).await);
    }
    squeezefs::routing::test_set_small_file_packing(Some(true));

    // Throttle 1 %: the duty cycle stretches the job (KD-3) — it must
    // still be live shortly after submit; pause parks it; resume + a live
    // rethrottle to 100 converges.
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragPack { volume_id: None },
            throttle_pct: 1,
        })
        .await
        .expect("submit");
    let deadline = Instant::now() + Duration::from_secs(30);
    let st = loop {
        if let Some(st) = fx.fabric.status(&job_id).await.unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "no status record");
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    assert_eq!(st.throttle_pct, 1, "the submitted throttle is recorded");
    fx.fabric.pause(&job_id).await.expect("pause");
    let st = fx.fabric.status(&job_id).await.unwrap().unwrap();
    assert!(
        matches!(st.state, JobState::Paused | JobState::Completed),
        "pause must park a live compaction (got {:?})",
        st.state
    );
    fx.fabric.resume(&job_id).await.expect("resume");
    fx.fabric.throttle(&job_id, 100).await.expect("rethrottle");
    let end = fx
        .fabric
        .wait_terminal(&job_id, Duration::from_secs(180))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed);
    assert_eq!(
        fx.alloc(0).get_used_blocks(),
        1,
        "eight legacy blocks → one pack"
    );
    for (i, ino) in inos.iter().enumerate() {
        assert_eq!(fx.read(*ino, len).await, pattern(90 + i, len));
    }

    // Cancel: a fresh legacy population, the mover parked at its first
    // publish, cancelled, released — the job ends Cancelled and every
    // file reads byte-exact wherever it landed.
    squeezefs::routing::test_set_small_file_packing(Some(false));
    fx.seal().await;
    let mut more = Vec::new();
    for i in 0..4 {
        more.push(fx.promoted_file(&format!("c{i}.bin"), len, 100 + i).await);
    }
    squeezefs::routing::test_set_small_file_packing(Some(true));
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let fired = AtomicBool::new(false);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
        if !fired.swap(true, Ordering::SeqCst) {
            let _ = hit_tx.send(());
            let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60));
        }
    }));
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragPack { volume_id: None },
            throttle_pct: 100,
        })
        .await
        .expect("submit");
    tokio::task::spawn_blocking(move || hit_rx.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("parked publish");
    fx.fabric.cancel(&job_id).await.expect("cancel");
    let _ = go_tx.send(());
    let end = fx
        .fabric
        .wait_terminal(&job_id, Duration::from_secs(180))
        .await
        .expect("terminal");
    squeezefs::jobs::clear_evacuate_pre_publish_hook();
    assert_eq!(end, JobState::Cancelled);
    for (i, ino) in more.iter().enumerate() {
        assert_eq!(fx.read(*ino, len).await, pattern(100 + i, len));
    }
    fx.assert_clean("after a cancelled compaction").await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contract 10: the VL9 mover-scope pin — serialize loud behind a drain
// ---------------------------------------------------------------------------

/// Two data volumes; a sealed pack on oss2; the drain of oss2 is parked at
/// its pre-publish window (provably RUNNING). A whole-set `defrag --pack`
/// submitted then stays Queued and counts `job_serialized_waits` once;
/// released, both complete and the tenants read byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_serializes_loud_behind_a_running_drain_on_the_same_volume() {
    let _g = serial().await;
    let _l = arm_levers(true);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 2, 4 << 30, "serialize").await;

    fx.place_only_on(1);
    let a = fx.promoted_file("a.bin", 16 * KIB, 111).await;
    let b = fx.promoted_file("b.bin", 16 * KIB, 112).await;
    fx.seal().await;
    fx.clear_health_overrides();
    let (base, _, _) = fx.mapping(a).await;
    assert!(
        base.starts_with(&fx.records[1].id),
        "the pack sits on oss2: {base}"
    );

    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let fired = AtomicBool::new(false);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
        if !fired.swap(true, Ordering::SeqCst) {
            let _ = hit_tx.send(());
            let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60));
        }
    }));
    let evac_id = fx
        .fs
        .admin_remove_data_volume(&fx.records[1].id, 100)
        .await
        .expect("remove-data admits");
    tokio::task::spawn_blocking(move || hit_rx.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("the drain must reach its parked publish window");
    assert_eq!(
        fx.fabric.status(&evac_id).await.unwrap().unwrap().state,
        JobState::Running
    );

    let waits_before = metric(&METRICS.job_serialized_waits);
    let pack_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragPack { volume_id: None },
            throttle_pct: 100,
        })
        .await
        .expect("submit defrag --pack");
    let deadline = Instant::now() + Duration::from_secs(30);
    while metric(&METRICS.job_serialized_waits) < waits_before + 1 {
        assert!(
            Instant::now() < deadline,
            "the compaction must record its serialized wait (pin a)"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        fx.fabric.status(&pack_id).await.unwrap().unwrap().state,
        JobState::Queued,
        "a whole-set compaction must serialize behind the running drain"
    );

    let _ = go_tx.send(());
    squeezefs::jobs::clear_evacuate_pre_publish_hook();
    assert_eq!(
        fx.fabric
            .wait_terminal(&evac_id, Duration::from_secs(180))
            .await
            .expect("drain terminal"),
        JobState::Completed
    );
    assert_eq!(
        fx.fabric
            .wait_terminal(&pack_id, Duration::from_secs(120))
            .await
            .expect("compaction terminal"),
        JobState::Completed,
        "the compaction runs once the drain released its scope"
    );
    assert_eq!(fx.read(a, 16 * KIB).await, pattern(111, 16 * KIB));
    assert_eq!(fx.read(b, 16 * KIB).await, pattern(112, 16 * KIB));
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Contracts 11 + 12: the lever gates the whole arm; report-only moves nothing
// ---------------------------------------------------------------------------

/// With the lever OFF the legacy population is measured (the gauges are a
/// reading of durable state) but `defrag --pack` REFUSES loud naming the
/// lever and moves nothing; `--report-only` (the measurement) publishes
/// the `frag_d1_pack_*` gauges and leaves every `pack_compaction_*`
/// counter and every block untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_off_gates_the_whole_arm_and_report_only_moves_nothing() {
    let _g = serial().await;
    let _l = arm_levers(false);
    let dir = tempfile::tempdir().unwrap();
    let (_meta, fx) = open_fresh(dir.path(), 1, 4 << 30, "leveroff").await;
    let alloc = fx.alloc(0);
    const N: usize = 4;
    let len = 16 * KIB;
    let mut inos = Vec::new();
    for i in 0..N {
        inos.push(fx.promoted_file(&format!("l{i}.bin"), len, 120 + i).await);
    }
    assert_eq!(alloc.get_used_blocks(), N as u64);

    // Report-only: measured, published, nothing moved.
    let c0 = counters();
    let report = squeezefs::defrag::measure(&fx.meta, &fx.fs.router)
        .await
        .expect("report");
    assert_eq!(report.pack.blocks, N as u64);
    assert_eq!(report.pack.below_half, N as u64);
    assert_eq!(
        report.pack.rows.len(),
        N,
        "one row per pack block on the full report"
    );
    assert_eq!(report.pack.rows_elided, 0);
    assert!(
        report
            .pack
            .rows
            .windows(2)
            .all(|w| w[0].occupancy <= w[1].occupancy),
        "the constructor orders rows worst occupancy first"
    );
    assert_eq!(metric(&METRICS.pack_blocks_below_half), N as u64);
    assert_eq!(
        metric(&METRICS.pack_reclaimable_bytes),
        N as u64 * (CHUNK_SIZE - len as u64)
    );
    assert!(squeezefs::defrag::decode_ratio(metric(&METRICS.frag_d1_pack_occupancy)).is_some());
    assert!(
        squeezefs::defrag::decode_ratio(metric(&METRICS.frag_d1_pack_occupancy_mean)).is_some()
    );
    let c1 = counters();
    assert_eq!(c1.compactions, c0.compactions);
    assert_eq!(c1.tenants_moved, c0.tenants_moved);
    assert_eq!(
        alloc.get_used_blocks(),
        N as u64,
        "report-only moved nothing"
    );

    // The mover under the lever OFF: refused loud, byte-identical.
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragPack { volume_id: None },
            throttle_pct: 100,
        })
        .await
        .expect("submit");
    let end = fx
        .fabric
        .wait_terminal(&job_id, Duration::from_secs(60))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Failed, "the lever gates the whole arm");
    let rec = JobFabric::list_records(&fx.meta)
        .await
        .expect("records")
        .into_iter()
        .find(|r| r.job_id == job_id)
        .expect("the job record");
    assert!(
        rec.error.as_deref().is_some_and(|e| e.contains(LEVER)),
        "the refusal names the lever: {:?}",
        rec.error
    );
    let c2 = counters();
    assert_eq!(c2.tenants_moved, c0.tenants_moved);
    assert_eq!(c2.blocks_freed, c0.blocks_freed);
    assert_eq!(
        alloc.get_used_blocks(),
        N as u64,
        "nothing moved under the lever OFF"
    );
    for (i, ino) in inos.iter().enumerate() {
        let (_, off, l) = fx.mapping(*ino).await;
        assert_eq!((off, l), (0, len), "the legacy mapping is untouched");
        assert_eq!(fx.read(*ino, len).await, pattern(120 + i, len));
    }
    fx.assert_clean("lever OFF").await;
    fx.close().await;
}

// ===========================================================================
// The mount-class venue (contract 13)
// ===========================================================================

const FILES: usize = 24;
const SIZES_KIB: [usize; 4] = [8, 16, 32, 64];

fn file_len(idx: usize) -> usize {
    SIZES_KIB[idx % SIZES_KIB.len()] * KIB
}

fn file_name(idx: usize) -> String {
    format!("legacy_{idx:04}.bin")
}

fn population_bytes() -> u64 {
    (0..FILES).map(|i| file_len(i) as u64).sum()
}

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_pkcomp_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

/// Format one meta + one data volume WITH a staging dir.
fn format_volume(base: &Path, staging: &Path) -> PathBuf {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(256 * 1024 * 1024)
        .expect("size meta file");
    std::fs::File::create(&data)
        .expect("create data file")
        .set_len(2 * 1024 * 1024 * 1024)
        .expect("size data file");
    std::fs::create_dir_all(staging).expect("create staging dir");
    let out: Output = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(staging)
        .arg("--force")
        .output()
        .expect("run squeezefs format");
    assert!(
        out.status.success(),
        "format failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    meta
}

struct Mount {
    child: Child,
    mnt: PathBuf,
    log: PathBuf,
}

const TEARDOWN_CENSUS_MARKERS: &[&str] = &["Dismount clean", "at dismount"];

impl Mount {
    /// `squeezefs umount` on the default wait; waits for the teardown
    /// census and the daemon's clean exit.
    fn umount(&mut self) {
        let started = Instant::now();
        let mut umount = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn squeezefs umount");
        let deadline = started + DEFAULT_DISMOUNT_WAIT * 3;
        loop {
            if TEARDOWN_CENSUS_MARKERS
                .iter()
                .any(|m| log_contains(&self.log, m))
            {
                break;
            }
            if let Some(status) = self.child.try_wait().expect("try_wait daemon") {
                panic!(
                    "daemon exited ({status}) without logging its dismount census; log:\n{}",
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            assert!(
                Instant::now() < deadline,
                "dismount census not reached; log:\n{}",
                std::fs::read_to_string(&self.log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait daemon") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = umount.kill();
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "daemon did not exit within {:?} of `squeezefs umount`; log:\n{}",
                    DEFAULT_DISMOUNT_WAIT * 3,
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let out = umount.wait_with_output().expect("collect umount output");
        assert!(
            status.success(),
            "daemon exited {status} on unmount; umount said:\n{}{}\nlog:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.child.try_wait() {
            let _ = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.mnt)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return;
        }
        let _ = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + DEFAULT_DISMOUNT_WAIT * 2;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the real daemon (zc OFF — the fstests runner's default; the
/// one-page inline ceiling pinned so the 8–64 KiB population stays staged;
/// the admin lane's dev override so the `defrag` verb reaches it).
/// `extra_env` rides on top — the packing lever IS the seam.
fn spawn_mount(meta: &Path, mnt: &Path, log: &Path, extra_env: &[(&str, &str)]) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let child = Command::new(bin())
        .arg("mount")
        .envs(extra_env.iter().copied())
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        // SAFETY: getuid/getgid are trivially safe.
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .arg("--disk-cache-size")
        .arg("500MB")
        .env("SQUEEZEFS_FUSE_ZC", "0")
        .env("SQUEEZEFS_INLINE_MAX_BYTES", "4096")
        .env("SQUEEZEFS_FSCK_SETTLE_MS", "200")
        .env("SQUEEZEFS_IPC_ALLOW_DEV", "1")
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
    let mut mount = Mount {
        child,
        mnt: mnt.to_path_buf(),
        log: log.to_path_buf(),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            break;
        }
        if let Ok(Some(status)) = mount.child.try_wait() {
            panic!(
                "mount exited before becoming ready ({status}); log:\n{}",
                std::fs::read_to_string(log).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "mount did not become ready within 90 s; log:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    mount
}

fn stats_json(mnt: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(mnt.join(".stats")).expect("read .stats");
    serde_json::from_str(&raw).expect(".stats must be valid JSON")
}

fn stat_u64(mnt: &Path, key: &str) -> u64 {
    let v = stats_json(mnt);
    v["metrics"][key]
        .as_u64()
        .or_else(|| v[key].as_u64())
        .unwrap_or_else(|| panic!("{key} exported on the stats inode"))
}

fn log_contains(log: &Path, needle: &str) -> bool {
    std::fs::read_to_string(log)
        .map(|t| t.contains(needle))
        .unwrap_or(false)
}

/// Allocated chunks on the mounted set — `statvfs`'s used bytes are the
/// allocator's `used_blocks × chunk` (RAM-maintained, no I/O).
fn used_chunks(mnt: &Path) -> u64 {
    let c = std::ffi::CString::new(mnt.to_str().unwrap()).unwrap();
    // SAFETY: a zeroed statvfs is a valid out-parameter; the return is checked.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    assert_eq!(rc, 0, "statvfs({})", mnt.display());
    let used = (st.f_blocks as u64 - st.f_bavail as u64) * st.f_frsize as u64;
    used / CHUNK_SIZE
}

fn write_close(path: &Path, bytes: &[u8]) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn write_fsync(path: &Path, bytes: &[u8]) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    f.sync_all()
        .unwrap_or_else(|e| panic!("fsync {}: {e}", path.display()));
}

fn syncfs(mnt: &Path) {
    let root = std::fs::File::open(mnt).expect("open mount root");
    use std::os::fd::AsRawFd;
    // SAFETY: syncfs on a live fd; the return is checked.
    let rc = unsafe { libc::syncfs(root.as_raw_fd()) };
    assert_eq!(rc, 0, "syncfs({}) failed", mnt.display());
}

/// Write the population (open → write → close, no fsync) and wait for it
/// to settle as N staged-layout files with no active-block custody.
fn populate_staged_files(mnt: &Path) {
    for idx in 0..FILES {
        write_close(&mnt.join(file_name(idx)), &pattern(idx, file_len(idx)));
    }
    syncfs(mnt);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let v = stats_json(mnt);
        let staged = v["nvme_staged_write_file_count"].as_u64().unwrap_or(0) as usize;
        let active = v["active_write_block_count"].as_u64().unwrap_or(0) as usize;
        if staged == FILES && active == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the population never settled as {FILES} staged-layout files (staged = {staged}, \
             active = {active})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn verify_files(mnt: &Path) -> usize {
    (0..FILES)
        .filter(|&idx| {
            let path = mnt.join(file_name(idx));
            let got =
                std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            got != pattern(idx, file_len(idx))
        })
        .count()
}

/// The striped anchor: one file past the block size, so the durable
/// block-reference ledger is NON-EMPTY when the remount seeds it.
const ANCHOR: &str = "anchor_striped.bin";
const ANCHOR_BLOCKS: u64 = 2;

fn anchor_bytes() -> Vec<u8> {
    pattern(usize::MAX, BLOCK + 64 * KIB)
}

/// `squeezefs defrag <mnt> <args…>` against the live mount.
fn defrag(mnt: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .arg("defrag")
        .arg(mnt)
        .args(args)
        .env("SQUEEZEFS_IPC_ALLOW_DEV", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run squeezefs defrag")
}

/// The CLI face on a real daemon. Lever OFF: the dismount pass mints the
/// legacy one-block-per-file population (24 files, 24 blocks); a remount
/// (lever OFF, oracle armed) reads it through `defrag --report-only` (24
/// pack blocks below half, the `frag_d1_pack_*` gauges live on `.stats`)
/// and REFUSES `defrag --pack` loud naming the lever with nothing moved.
/// Lever ON: `defrag --pack` compacts the population into ONE block
/// (`pack_compaction_blocks_freed = 24`, `used = anchor + 1`), every file
/// reads byte-exact; a final remount with the oracle reads drift 0.
#[test]
fn on_a_live_mount_report_only_reads_the_legacy_population_and_defrag_pack_compacts_it() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("cli");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");

    // The legacy population: promoted one-per-block at a lever-OFF dismount.
    let log0 = base.join("mount0.log");
    let mut m0 = spawn_mount(&meta, &mnt, &log0, &[]);
    write_fsync(&mnt.join(ANCHOR), &anchor_bytes());
    populate_staged_files(&mnt);
    m0.umount();
    assert!(
        log_contains(&log0, &format!("0 packed, {FILES} to blocks")),
        "lever OFF: one block per file; log: {}",
        log0.display()
    );

    // Lever OFF remount: measured, refused, untouched.
    let log1 = base.join("mount1.log");
    let mut m1 = spawn_mount(&meta, &mnt, &log1, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert_eq!(stat_u64(&mnt, "meta_kv_block_refs_drift"), 0);
    assert_eq!(used_chunks(&mnt), ANCHOR_BLOCKS + FILES as u64);
    let out = defrag(&mnt, &["--report-only", "--json"]);
    assert!(
        out.status.success(),
        "report-only: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the report is JSON");
    assert_eq!(report["pack"]["blocks"].as_u64(), Some(FILES as u64));
    assert_eq!(report["pack"]["below_half"].as_u64(), Some(FILES as u64));
    assert_eq!(
        report["pack"]["reclaimable_bytes"].as_u64(),
        Some(FILES as u64 * CHUNK_SIZE - population_bytes())
    );
    assert_eq!(stat_u64(&mnt, "pack_blocks_below_half"), FILES as u64);
    let s = stats_json(&mnt);
    assert!(
        s["metrics"]["frag_d1_pack_occupancy"].as_f64().is_some(),
        "the worst pack occupancy publishes after a measurement: {}",
        s["metrics"]["frag_d1_pack_occupancy"]
    );
    let human = defrag(&mnt, &["--report-only"]);
    assert!(human.status.success());
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("D1 pack"),
        "the human report carries the pack row: {}",
        String::from_utf8_lossy(&human.stdout)
    );
    let refused = defrag(&mnt, &["--pack"]);
    assert!(
        !refused.status.success(),
        "lever OFF: defrag --pack must refuse: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(err.contains(LEVER), "the refusal names the lever: {err}");
    assert_eq!(stat_u64(&mnt, "pack_compaction_tenants_moved"), 0);
    assert_eq!(
        used_chunks(&mnt),
        ANCHOR_BLOCKS + FILES as u64,
        "nothing moved"
    );
    assert_eq!(verify_files(&mnt), 0);
    m1.umount();

    // Lever ON: compaction.
    let log2 = base.join("mount2.log");
    let mut m2 = spawn_mount(&meta, &mnt, &log2, &[(LEVER, "1")]);
    let out = defrag(&mnt, &["--pack"]);
    assert!(
        out.status.success(),
        "defrag --pack: {}{}\nlog: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        log2.display()
    );
    assert_eq!(stat_u64(&mnt, "pack_compaction_blocks_freed"), FILES as u64);
    assert_eq!(
        stat_u64(&mnt, "pack_compaction_tenants_moved"),
        FILES as u64
    );
    assert_eq!(
        stat_u64(&mnt, "pack_compaction_windows_copied"),
        FILES as u64
    );
    assert_eq!(
        stat_u64(&mnt, "pack_compaction_bytes_copied"),
        population_bytes()
    );
    assert!(
        stat_u64(&mnt, "pack_compaction_bytes_copied")
            <= stat_u64(&mnt, "pack_compaction_bytes_reclaimed")
    );
    assert_eq!(
        stat_u64(&mnt, "pack_compactions"),
        1,
        "one pass executed a plan"
    );
    assert_eq!(stat_u64(&mnt, "block_double_frees"), 0);
    assert_eq!(stat_u64(&mnt, "block_untracked_free_refusals"), 0);
    assert_eq!(
        used_chunks(&mnt),
        ANCHOR_BLOCKS + 1,
        "{FILES} legacy blocks → one pack"
    );
    assert_eq!(
        stat_u64(&mnt, "pack_blocks_below_half"),
        1,
        "the open pack is the one low block left"
    );
    assert_eq!(
        verify_files(&mnt),
        0,
        "every file reads byte-exact after compaction"
    );
    m2.umount();
    assert!(log_contains(&log2, "dismount sealed 1 open pack block(s)"));

    // The oracle remount.
    let log3 = base.join("mount3.log");
    let mut m3 = spawn_mount(&meta, &mnt, &log3, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert_eq!(stat_u64(&mnt, "meta_kv_block_refs_drift"), 0);
    assert_eq!(
        stat_u64(&mnt, "meta_kv_block_refs_recovered"),
        ANCHOR_BLOCKS + FILES as u64,
        "one durable reference per tenant + the anchor's blocks"
    );
    assert_eq!(used_chunks(&mnt), ANCHOR_BLOCKS + 1);
    assert_eq!(verify_files(&mnt), 0);
    assert_eq!(std::fs::read(mnt.join(ANCHOR)).unwrap(), anchor_bytes());
    m3.umount();
    let _ = std::fs::remove_dir_all(&base);
}

/// Contract 14 — **the report is BOUNDED on the wire** (found by the PK7
/// local rig 2026-09-10: `defrag --report-only --json` on the LEGACY
/// 2,000-block population answered "reply too large" — the admin lane
/// refuses any body past `ADMIN_BODY_MAX`, and the pack face carried one
/// row per block, unbounded). The fsck report's precedent (the PR 8
/// "152-finding wart") governs: `DefragReport::to_bounded_json(cap)`
/// serves the aggregates EXACT, the longest fitting WORST-OCCUPANCY row
/// prefix (the rows an operator acts on first), and `rows_elided`
/// counting the rest; the durable per-block table is the offline probe's.
#[test]
fn defrag_report_serves_a_bounded_view_under_the_admin_body_cap() {
    use squeezefs::defrag::{DefragReport, PackBlockRow, PackReport};
    let chunk = CHUNK_SIZE;
    let rows: Vec<PackBlockRow> = (0..5000u64)
        .map(|i| {
            let live = 16 * 1024 + (i % 200) * 4096;
            PackBlockRow {
                vol: format!("vol-{:016x}", i % 4),
                offset: (i / 4) * chunk,
                base_key: format!("nvme{}n1://{}@{:x}", 5 + i % 4, (i / 4) * chunk, i),
                tenants: 1 + i % 7,
                windows: 1 + i % 7,
                live_bytes: live,
                occupancy: live as f64 / chunk as f64,
                victim: true,
            }
        })
        .collect();
    // The constructor's order (contract 12 pins it): worst occupancy first.
    let mut rows = rows;
    rows.sort_by(|a, b| {
        a.live_bytes
            .cmp(&b.live_bytes)
            .then(a.vol.cmp(&b.vol))
            .then(a.offset.cmp(&b.offset))
    });
    let live_total: u64 = rows.iter().map(|r| r.live_bytes).sum();
    let report = DefragReport {
        d1: Vec::new(),
        d2: squeezefs::defrag::D2Report {
            files: 0,
            pairs: 0,
            local_pairs: 0,
            locality: 1.0,
        },
        d3: squeezefs::defrag::D3Report {
            parked_extent_bytes: 0,
            spilled_records: 0,
            spilled_record_bytes: 0,
            pressure_bytes: 0,
        },
        d4: Vec::new(),
        pack: PackReport {
            blocks: 5000,
            below_half: 5000,
            live_bytes: live_total,
            reclaimable_bytes: 5000 * chunk - live_total,
            worst_occupancy: (16 * 1024) as f64 / chunk as f64,
            mean_occupancy: 0.1,
            rows_elided: 0,
            rows,
        },
    };
    let full = serde_json::to_string(&report).unwrap();
    let cap = squeezefs_ipc::wire::ADMIN_BODY_MAX;
    assert!(
        full.len() > cap,
        "the fixture must exceed the cap ({} vs {cap})",
        full.len()
    );

    let body = report
        .to_bounded_json(cap)
        .expect("a bounded view always fits: the skeleton is a few hundred bytes");
    assert!(body.len() <= cap, "bounded body {} > cap {cap}", body.len());
    let view: DefragReport = serde_json::from_str(&body).expect("the bounded view decodes");
    // Aggregates EXACT.
    assert_eq!(view.pack.blocks, 5000);
    assert_eq!(view.pack.below_half, 5000);
    assert_eq!(view.pack.live_bytes, live_total);
    assert_eq!(view.pack.reclaimable_bytes, 5000 * chunk - live_total);
    // The prefix + the elided count close against the population.
    assert!(
        !view.pack.rows.is_empty(),
        "at least one row fits under a 60 KiB cap"
    );
    assert!(view.pack.rows.len() < 5000);
    assert_eq!(
        view.pack.rows.len() as u64 + view.pack.rows_elided,
        5000,
        "kept + elided ≡ blocks"
    );
    // Worst occupancy first: the kept rows are the least-occupied ones.
    let kept_max = view.pack.rows.iter().map(|r| r.live_bytes).max().unwrap();
    assert!(
        view.pack
            .rows
            .windows(2)
            .all(|w| w[0].occupancy <= w[1].occupancy),
        "kept rows are sorted worst-first"
    );
    assert!(kept_max <= 16 * 1024 + 199 * 4096);
    // A report that fits is served verbatim (rows_elided stays 0).
    let small = DefragReport {
        pack: PackReport {
            rows: report.pack.rows[..3].to_vec(),
            ..report.pack.clone()
        },
        ..report.clone()
    };
    let small_body = small.to_bounded_json(cap).unwrap();
    let small_view: DefragReport = serde_json::from_str(&small_body).unwrap();
    assert_eq!(small_view.pack.rows.len(), 3);
    assert_eq!(small_view.pack.rows_elided, 0);
}
