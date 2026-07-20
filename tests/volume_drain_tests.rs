//! PR VL4 — data-volume drain/remove, red-first
//! (docs/design-volume-lifecycle.md §5.2/§5.4, KD-6, KD-12, gate G-VL-3):
//!
//! - **Capacity preflight (§5.2)**: the closed-form property — refuse iff
//!   `avail < needed + transient + headroom` — fuzzed over censuses;
//!   `transient = 64 blocks × ACTUAL workers`; `headroom =
//!   max(write_rate_est × drain_eta, 1 GiB)`; refusals print the exact
//!   numbers (honest refusal) and count `volume_preflight_refusals`.
//! - **The evacuation mover (`JobType::EvacuateVolume`)**: census-planned
//!   copy-then-republish CoW (KD-6) — copy lock-free, verify, publish via
//!   `merge_block_mappings` under the ino's CURRENT fencing token,
//!   free displaced source clone-aware; drain converges to `retired`;
//!   byte-identity end to end; `evacuate_*` engagement counters.
//! - **Clone move-once (G-VL-3 c)**: a shared block moves exactly once —
//!   every referencing block_map updates to the new key, refcount
//!   transfers pre-publish, both clones byte-identical.
//! - **Supersession (FIND-M11-A applied to movers)**: a foreground write
//!   that replaces the mapping mid-move makes the mover's publish a
//!   contractual no-op (`evacuate_stale_token_noops`), never a clobber.
//! - **Draining semantics (G-VL-3 f)**: excluded from write placement,
//!   still SERVES READS to completion (the disable-EIO foot-gun retired
//!   for the drain path).
//! - **W1 ledger (G-VL-3 d)**: `patch_ineligible_*` and
//!   `write_path_seed_read_bytes` deltas are 0 across a drain.
//! - **Quiescent-first (§5.4 step 3)**: blocks with live active buffers
//!   defer (`evacuate_deferred_staged_blocks`) and move once quiescent.
//! - **Undrain**: state back to `active`, evacuation job cancelled.
//! - **Auto-rebalance default (KD-12)**: `volume add-data` submits the
//!   bounded rebalance pass unless `--no-rebalance`; offline adds write a
//!   durable Queued record the next mount's fabric adopts.
//! - **Crash-resume (KD-6 / G-VL-3 a, cargo-level)**: abrupt shutdown
//!   mid-drain + reopen ⇒ the adopted job re-plans idempotently and
//!   converges with a byte-identical manifest.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::jobs::{
    drain_headroom_bytes, drain_transient_bytes, DrainPreflight, JobFabric, JobState, JobType,
    MoverCtx, DRAIN_HEADROOM_FLOOR_BYTES, EVACUATE_INFLIGHT_WINDOW_BLOCKS,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{
    DataVolumeRecord, FormatConfig, VOL_STATE_ACTIVE, VOL_STATE_DRAINING, VOL_STATE_RETIRED,
};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::TempDir;

const BLOCK: usize = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
    }
}

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
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
        meta_slot_map: None,
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

/// Mount-shaped fixture: resolved volume records drive
/// `register_backend`, the first record's device/allocator are the
/// router's default slot, and — new for VL4 — the job fabric runs with
/// the mover context wired (the VL4 shape of the mount).
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    fabric: Arc<JobFabric>,
    _staging: TempDir,
}

async fn open_fixture(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new("local").unwrap();

    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), &first.id)
            .await
            .unwrap(),
    );
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }

    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in records {
        if rec.state == VOL_STATE_RETIRED {
            continue; // retired ids are never re-registered (KD-5)
        }
        router
            .backend_router
            .register_backend(rec, dlm.meta_client().clone())
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());
    router
        .backend_router
        .active_write_backend
        .store(Arc::new(first.id.clone()));

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    // Allocator refcount recovery exactly like a mount (the census
    // ground truth the preflight and mover rely on).
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
        _staging: staging,
    }
}

impl Fx {
    async fn close(self) {
        self.fabric.shutdown_abrupt().await;
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }

    /// Kill-9 analog: abort the fabric mid-work and drop the volumes
    /// WITHOUT a clean shutdown path for the workers (the meta volumes
    /// still close their claims so the same process can reopen).
    async fn crash(self) {
        self.fabric.shutdown_abrupt().await;
        for vol in &self.meta.volumes {
            let _ = vol.shutdown().await;
        }
    }
}

async fn create_file(fx: &Fx, name: &str) -> u64 {
    fx.fs
        .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Striped burst: force striped layout, write `nblocks` distinct blocks,
/// fsync. Returns the expected content.
async fn striped_burst(fx: &Fx, ino: u64, nblocks: usize) -> Vec<u8> {
    let dummy = vec![0u8; BLOCK + 1];
    fx.fs
        .write(
            req(),
            ino,
            0,
            0,
            bytes::Bytes::copy_from_slice(&dummy),
            0,
            0,
        )
        .await
        .unwrap();
    let mut expected = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let data = vec![(b as u8) ^ 0x5C; BLOCK];
        expected.extend_from_slice(&data);
        fx.fs
            .write(
                req(),
                ino,
                0,
                (b * BLOCK) as u64,
                bytes::Bytes::copy_from_slice(&data),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("write of block {b} failed: {e:?}"));
    }
    fx.fs.fsync(req(), ino, 0, false).await.unwrap();
    expected
}

async fn read_back(fx: &Fx, ino: u64, nblocks: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let reply = fx
            .fs
            .read(req(), ino, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("read of block {b} failed: {e:?}"));
        out.extend_from_slice(&reply.data);
    }
    out
}

/// Distinct victim base offsets referenced by the ino's durable block
/// map (the census the mover must clear).
async fn victim_blocks_of(fx: &Fx, ino: u64, victim: &str) -> Vec<(u32, String)> {
    let meta = fx
        .fs
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("layout");
    let mut out = Vec::new();
    if let Some(map) = meta.block_map.as_deref() {
        for (&b, mapping) in map {
            let clean = mapping
                .find("://")
                .map(|p| {
                    let rest = &mapping[p + 3..];
                    format!(
                        "{}://{}",
                        &mapping[..p],
                        rest.split(':').next().unwrap_or(rest)
                    )
                })
                .unwrap_or_else(|| mapping.split(':').next().unwrap_or(mapping).to_string());
            let (be, _off) = fx
                .fs
                .router
                .backend_router
                .parse_block_key(&clean)
                .expect("parse");
            if be == victim {
                out.push((b, mapping.clone()));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// §5.2: the capacity-preflight closed form (G-VL-3 e)
// ---------------------------------------------------------------------------

#[test]
fn test_preflight_property_refuse_iff_closed_form() {
    // The terms are the spec: transient = 64 blocks × ACTUAL workers;
    // headroom = max(write_rate_est × drain_eta, 1 GiB).
    assert_eq!(EVACUATE_INFLIGHT_WINDOW_BLOCKS, 64);
    assert_eq!(drain_transient_bytes(8, 4 << 20), 64 * 8 * (4 << 20));
    assert_eq!(DRAIN_HEADROOM_FLOOR_BYTES, 1 << 30);
    assert_eq!(drain_headroom_bytes(0, 0), 1 << 30, "floor = 1 GiB");
    assert_eq!(
        drain_headroom_bytes(1 << 21, 4096),
        (1u64 << 21) * 4096,
        "rate × eta above the floor wins"
    );

    // Fuzzed censuses: refuse iff avail < needed + transient + headroom.
    for _ in 0..2000 {
        let needed = fastrand::u64(0..1 << 40);
        let avail = fastrand::u64(0..1 << 41);
        let workers = fastrand::usize(1..=8);
        let block = 1u64 << fastrand::u32(12..=22);
        let rate = fastrand::u64(0..1 << 30);
        let eta = fastrand::u64(0..100_000);
        let pf = DrainPreflight {
            needed_bytes: needed,
            avail_bytes: avail,
            transient_bytes: drain_transient_bytes(workers, block),
            headroom_bytes: drain_headroom_bytes(rate, eta),
        };
        let required = needed
            .saturating_add(drain_transient_bytes(workers, block))
            .saturating_add(drain_headroom_bytes(rate, eta));
        assert_eq!(pf.required_bytes(), required);
        assert_eq!(
            pf.admits(),
            avail >= required,
            "refuse iff avail < needed+transient+headroom (needed {needed} avail {avail})"
        );
        if !pf.admits() {
            // Honest refusal: the exact numbers appear in the message.
            let msg = pf.refusal();
            for n in [needed, avail, pf.transient_bytes, pf.headroom_bytes] {
                assert!(
                    msg.contains(&n.to_string()),
                    "refusal must print the exact number {n}: {msg}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// G-VL-3 f: draining serves reads, excluded from placement (router level)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_draining_volume_serves_reads_and_takes_no_placements() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    let br = &fx.fs.router.backend_router;

    // Put a block on oss2 directly, then flip it to draining.
    let be = br.backends.get("oss2").unwrap().value().clone();
    let offset = be.block_allocator.allocate_block().await.unwrap();
    let payload = vec![0xB6u8; BLOCK];
    be.device
        .write_block(offset, bytes::Bytes::copy_from_slice(&payload))
        .await
        .unwrap();
    be.block_allocator.publish_block(offset);

    br.set_volume_state("oss2", VOL_STATE_DRAINING)
        .expect("flip to draining");

    // Placement never selects a draining volume...
    for _ in 0..32 {
        let (be_id, _, _) = br.get_active_backend().unwrap();
        assert_eq!(be_id, "oss1", "draining volume must take no placements");
    }
    // ...but reads are served to completion (the EIO foot-gun retired).
    let got = br
        .read_block(&format!("oss2://{offset}"), BLOCK)
        .await
        .expect("a draining volume must serve reads");
    assert_eq!(&got[..], &payload[..]);
    // refcount ops still work on a draining volume.
    assert!(br.increment_refcount(&format!("oss2://{offset}")));
    br.free_block(&format!("oss2://{offset}")).await.unwrap();
    br.free_block(&format!("oss2://{offset}")).await.unwrap();

    // Unknown ids / unknown states refuse.
    assert!(br.set_volume_state("nope", VOL_STATE_DRAINING).is_err());
    assert!(br.set_volume_state("oss2", "melting").is_err());

    br.set_volume_state("oss2", VOL_STATE_ACTIVE).unwrap();
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The full drain: census → move → converge → retire; byte-identity;
// engagement counters; W1 ledger delta 0 (G-VL-3 a/b/d instruments)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_drain_evacuates_retires_and_preserves_bytes_w1_ledger_zero() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    const NBLOCKS: usize = 24;
    let ino = create_file(&fx, "burst.bin").await;
    let expected = striped_burst(&fx, ino, NBLOCKS).await;
    let on_victim = victim_blocks_of(&fx, ino, "oss2").await;
    assert!(
        !on_victim.is_empty(),
        "placement must have spread blocks onto oss2"
    );

    // W1 ledger + engagement snapshots.
    let ledger = || -> u64 {
        METRICS.patch_ineligible_unmapped.load(Ordering::Relaxed)
            + METRICS.patch_ineligible_decorated.load(Ordering::Relaxed)
            + METRICS.patch_ineligible_unaligned.load(Ordering::Relaxed)
            + METRICS.patch_ineligible_overlay.load(Ordering::Relaxed)
            + METRICS.patch_ineligible_shared.load(Ordering::Relaxed)
            + METRICS.patch_ineligible_transform.load(Ordering::Relaxed)
            + METRICS.patch_ineligible_adjacent.load(Ordering::Relaxed)
            + METRICS.patch_ineligible_oversize.load(Ordering::Relaxed)
    };
    let ledger_before = ledger();
    let seed_before = METRICS.write_path_seed_read_bytes.load(Ordering::Relaxed);
    let moved_before = METRICS.evacuate_blocks_moved.load(Ordering::Relaxed);
    let bytes_before = METRICS.evacuate_bytes_moved.load(Ordering::Relaxed);

    // The online remove: preflight → durable draining → evacuation job.
    let job_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits (plenty of survivor capacity)");

    // Durable state flipped before the job ran to completion.
    let states = fx.fs.router.backend_router.volume_states();
    let row = states.iter().find(|s| s.id == "oss2").unwrap();
    assert!(
        row.state == VOL_STATE_DRAINING || row.state == VOL_STATE_RETIRED,
        "remove-data must flip the durable state (got {})",
        row.state
    );

    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(120))
        .await
        .expect("evacuation terminal");
    assert_eq!(end, JobState::Completed, "the drain must converge");

    // Retired: durable record state, backend deregistered, id kept.
    let states = fx.fs.router.backend_router.volume_states();
    let row = states.iter().find(|s| s.id == "oss2").unwrap();
    assert_eq!(row.state, VOL_STATE_RETIRED, "census 0 ⇒ retired");
    assert!(
        !fx.fs.router.backend_router.backends.contains_key("oss2"),
        "a retired volume's runtime backend is deregistered"
    );

    // Census empty: no block key parses to the victim anymore.
    assert!(
        victim_blocks_of(&fx, ino, "oss2").await.is_empty(),
        "no block may still reference the victim after retire"
    );

    // Byte identity.
    let got = read_back(&fx, ino, NBLOCKS).await;
    assert_eq!(got, expected, "drain must preserve every byte");

    // Engagement: evacuate counters account for the victim blocks.
    let moved = METRICS.evacuate_blocks_moved.load(Ordering::Relaxed) - moved_before;
    let bytes = METRICS.evacuate_bytes_moved.load(Ordering::Relaxed) - bytes_before;
    assert!(
        moved >= on_victim.len() as u64,
        "evacuate_blocks_moved ({moved}) must account for the {} victim blocks",
        on_victim.len()
    );
    assert!(bytes >= moved * BLOCK as u64 / 2, "bytes_moved engagement");

    // W1 ledger delta 0 across the drain (G-VL-3 d).
    assert_eq!(ledger() - ledger_before, 0, "patch_ineligible_* delta");
    assert_eq!(
        METRICS.write_path_seed_read_bytes.load(Ordering::Relaxed) - seed_before,
        0,
        "write_path_seed_read_bytes delta"
    );
    // The copy-window gauge always drains back to zero.
    assert_eq!(
        METRICS.evacuate_inflight_bytes.load(Ordering::Relaxed),
        0,
        "inflight gauge must drain"
    );

    // Post-retire, new writes still work and land on the survivor.
    let ino2 = create_file(&fx, "after.bin").await;
    let exp2 = striped_burst(&fx, ino2, 4).await;
    assert_eq!(read_back(&fx, ino2, 4).await, exp2);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// G-VL-3 c: clone-shared blocks move ONCE
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_clone_shared_blocks_move_once_both_clones_intact() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    const NBLOCKS: usize = 16;
    let src_ino = create_file(&fx, "orig.bin").await;
    let expected = striped_burst(&fx, src_ino, NBLOCKS).await;
    let dst_ino = create_file(&fx, "clone.bin").await;
    // Present the CURRENT tokens (the FUSE layer still holds the create
    // leases in this fixture; the whole-file copy_file_range handler
    // passes tokens the same way).
    let src_token = fx.fs.dlm().get_fencing_token_ino(src_ino);
    let dst_token = fx.fs.dlm().get_fencing_token_ino(dst_ino);
    fx.fs
        .router
        .clone_file(
            &squeezefs::keys::inode_path(src_ino),
            &squeezefs::keys::inode_path(dst_ino),
            Some(src_token),
            Some(dst_token),
        )
        .await
        .expect("clone");

    let shared_on_victim = victim_blocks_of(&fx, src_ino, "oss2").await;
    assert!(
        !shared_on_victim.is_empty(),
        "victim must hold shared blocks"
    );

    let moved_before = METRICS.evacuate_blocks_moved.load(Ordering::Relaxed);
    let shared_before = METRICS.evacuate_shared_blocks_moved.load(Ordering::Relaxed);

    let job_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(120))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed);

    // Move-once: blocks_moved counts each DISTINCT victim offset once —
    // exactly the victim's distinct block census, clone sharing included.
    let moved = METRICS.evacuate_blocks_moved.load(Ordering::Relaxed) - moved_before;
    assert_eq!(
        moved,
        shared_on_victim.len() as u64,
        "a clone-shared block must move exactly once"
    );
    let shared_moved = METRICS.evacuate_shared_blocks_moved.load(Ordering::Relaxed) - shared_before;
    assert_eq!(
        shared_moved,
        shared_on_victim.len() as u64,
        "every moved block here was refcount-shared (clone)"
    );

    // Both clones byte-identical; no key references the victim.
    assert_eq!(read_back(&fx, src_ino, NBLOCKS).await, expected);
    assert_eq!(read_back(&fx, dst_ino, NBLOCKS).await, expected);
    assert!(victim_blocks_of(&fx, src_ino, "oss2").await.is_empty());
    assert!(victim_blocks_of(&fx, dst_ino, "oss2").await.is_empty());

    // Both referencers now share the SAME new key per block (refcount
    // transferred, not duplicated): freeing one clone must not disturb
    // the other.
    let mut con = fx
        .fs
        .router
        .dlm
        .get_connection()
        .await
        .expect("meta connection");
    fx.fs
        .router
        .delete_file(&squeezefs::keys::inode_path(dst_ino), &mut con)
        .await
        .expect("delete the clone");
    assert_eq!(
        read_back(&fx, src_ino, NBLOCKS).await,
        expected,
        "the surviving clone must read intact after its twin is deleted"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// FIND-M11-A supersession: mover publish is a no-op when the mapping moved
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stale_mapping_supersession_is_contractual_noop() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    const NBLOCKS: usize = 8;
    let ino = create_file(&fx, "racy.bin").await;
    striped_burst(&fx, ino, NBLOCKS).await;
    let on_victim = victim_blocks_of(&fx, ino, "oss2").await;
    assert!(!on_victim.is_empty());
    let (race_block, _) = on_victim[0];

    // The test hook: the FIRST publish attempt for (ino, race_block)
    // parks the mover while the test lands a foreground write on the
    // same block — the mover's publish must then supersede (no-op).
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let fired = std::sync::atomic::AtomicBool::new(false);
    let target = (ino, race_block);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |h_ino, h_block| {
        if (h_ino, h_block) == target && !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let _ = hit_tx.send(());
            let _ = go_rx.lock().unwrap().recv();
        }
    }));

    let noops_before = METRICS.evacuate_stale_token_noops.load(Ordering::Relaxed);
    let replans_before = METRICS.evacuate_replans.load(Ordering::Relaxed);

    let job_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");

    // Mover parked pre-publish: land the foreground write (new bytes,
    // new key on the survivor — draining excludes the victim).
    hit_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the mover must reach the hooked publish");
    let fresh = vec![0xEEu8; BLOCK];
    fx.fs
        .write(
            req(),
            ino,
            0,
            (race_block as usize * BLOCK) as u64,
            bytes::Bytes::copy_from_slice(&fresh),
            0,
            0,
        )
        .await
        .expect("foreground write during the parked publish");
    fx.fs.fsync(req(), ino, 0, false).await.unwrap();
    go_tx.send(()).unwrap();

    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(120))
        .await
        .expect("terminal");
    assert_eq!(
        end,
        JobState::Completed,
        "supersession must not wedge the drain"
    );
    squeezefs::jobs::clear_evacuate_pre_publish_hook();

    // The superseded publish was a contractual no-op, and re-planning
    // revisited the volume to convergence.
    assert!(
        METRICS.evacuate_stale_token_noops.load(Ordering::Relaxed) > noops_before,
        "the raced publish must count evacuate_stale_token_noops"
    );
    assert!(
        METRICS.evacuate_replans.load(Ordering::Relaxed) > replans_before,
        "convergence is by re-planning"
    );

    // The foreground write's bytes won — never the mover's stale copy.
    let reply = fx
        .fs
        .read(
            req(),
            ino,
            0,
            (race_block as usize * BLOCK) as u64,
            BLOCK as u32,
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        &reply.data[..],
        &fresh[..],
        "the foreground write must win over the mover's stale copy"
    );
    assert!(victim_blocks_of(&fx, ino, "oss2").await.is_empty());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// §5.4 step 3: quiescent-first — live-buffered blocks defer, then move
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_quiescent_first_defers_live_buffers_then_converges() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    // Arm the FUSE-init workers (writeback flusher included): fsync of a
    // parked partial buffer rides the writeback channel — the flush leg
    // this test's convergence needs.
    fx.fs.init(req()).await.expect("fuse init");

    const NBLOCKS: usize = 8;
    let ino = create_file(&fx, "hot.bin").await;
    striped_burst(&fx, ino, NBLOCKS).await;
    let on_victim = victim_blocks_of(&fx, ino, "oss2").await;
    assert!(!on_victim.is_empty());
    let (hot_block, _) = on_victim[0];

    // Re-dirty ONE victim block without fsync — PARTIAL coverage, so
    // the write parks as a live `ActiveBlockBuf` (a full-block write
    // would write-through immediately and leave nothing to defer): the
    // quiescent-first rule must defer this block.
    let hot = vec![0x77u8; BLOCK / 4];
    fx.fs
        .write(
            req(),
            ino,
            0,
            (hot_block as usize * BLOCK) as u64,
            bytes::Bytes::copy_from_slice(&hot),
            0,
            0,
        )
        .await
        .unwrap();

    let deferred_before = METRICS
        .evacuate_deferred_staged_blocks
        .load(Ordering::Relaxed);

    let job_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");

    // The mover must observe the live buffer and defer (counted), while
    // the drain keeps re-planning rather than completing.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while METRICS
        .evacuate_deferred_staged_blocks
        .load(Ordering::Relaxed)
        == deferred_before
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the mover must count the deferred live-buffered block"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Quiesce (flush) — the re-plan pass picks the block up and the
    // drain converges.
    fx.fs.fsync(req(), ino, 0, false).await.unwrap();
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(120))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed);
    assert!(victim_blocks_of(&fx, ino, "oss2").await.is_empty());
    let reply = fx
        .fs
        .read(
            req(),
            ino,
            0,
            (hot_block as usize * BLOCK) as u64,
            BLOCK as u32,
            0,
        )
        .await
        .unwrap();
    assert_eq!(
        &reply.data[..hot.len()],
        &hot[..],
        "the flushed partial write must survive the drain"
    );
    let original = vec![(hot_block as u8) ^ 0x5C; BLOCK];
    assert_eq!(
        &reply.data[hot.len()..],
        &original[hot.len()..],
        "the unwritten remainder of the hot block must keep its bytes"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Preflight refusal (integration): honest numbers, counted, state intact
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_remove_data_refuses_without_survivor_capacity() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    // Survivor far under the 1 GiB headroom floor ⇒ must refuse.
    let oss1 = make_file(dir.path(), "oss1", 256 * 1024 * 1024);
    let oss2 = make_file(dir.path(), "oss2", 256 * 1024 * 1024);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "data.bin").await;
    striped_burst(&fx, ino, 8).await;

    let refusals_before = METRICS.volume_preflight_refusals.load(Ordering::Relaxed);
    let err = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect_err("removing into a too-small survivor set must refuse");
    // The honest refusal names the terms with exact numbers.
    for term in ["needed", "avail", "transient", "headroom"] {
        assert!(
            err.contains(term),
            "refusal must name the {term} term: {err}"
        );
    }
    assert!(
        METRICS.volume_preflight_refusals.load(Ordering::Relaxed) > refusals_before,
        "refusals count volume_preflight_refusals"
    );

    // Nothing flipped, nothing submitted.
    let states = fx.fs.router.backend_router.volume_states();
    assert_eq!(
        states.iter().find(|s| s.id == "oss2").unwrap().state,
        VOL_STATE_ACTIVE,
        "a refused remove must leave the state untouched"
    );
    assert!(
        JobFabric::list_records(&fx.meta)
            .await
            .unwrap()
            .iter()
            .all(|r| !matches!(r.job_type, JobType::EvacuateVolume { .. })),
        "a refused remove must submit no job"
    );

    // Unknown ids refuse too.
    assert!(fx.fs.admin_remove_data_volume("nope", 100).await.is_err());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Undrain: state back to active, evacuation job cancelled cleanly
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_undrain_restores_active_and_cancels_the_job() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "data.bin").await;
    let expected = striped_burst(&fx, ino, 16).await;

    // Park the mover at its first publish so the drain is provably
    // in-flight when the undrain lands.
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
        let _ = hit_tx.send(());
        let _ = go_rx
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(30));
    }));

    let job_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");
    let _ = hit_rx.recv_timeout(std::time::Duration::from_secs(60));

    fx.fs
        .admin_undrain_data_volume("oss2")
        .await
        .expect("undrain a draining volume");
    let _ = go_tx.send(());
    squeezefs::jobs::clear_evacuate_pre_publish_hook();

    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(60))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Cancelled, "undrain cancels the evacuation");

    // State restored, placement includes the volume again, bytes intact.
    let states = fx.fs.router.backend_router.volume_states();
    assert_eq!(
        states.iter().find(|s| s.id == "oss2").unwrap().state,
        VOL_STATE_ACTIVE
    );
    let mut saw_oss2 = false;
    for _ in 0..64 {
        if fx.fs.router.backend_router.get_active_backend().unwrap().0 == "oss2" {
            saw_oss2 = true;
            break;
        }
    }
    assert!(saw_oss2, "an undrained volume rejoins placement");
    assert_eq!(read_back(&fx, ino, 16).await, expected);

    // Undraining a non-draining volume refuses.
    assert!(fx.fs.admin_undrain_data_volume("oss2").await.is_err());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// KD-12: auto-rebalance on add-data (default on, --no-rebalance opts out)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_auto_rebalance_submitted_on_add_and_suppressed_by_opt_out() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    let oss3 = make_file(dir.path(), "oss3", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // Seed data so the rebalance pass has something to move.
    let ino = create_file(&fx, "seed.bin").await;
    let expected = striped_burst(&fx, ino, 32).await;

    // Default: add-data auto-submits the bounded rebalance pass at the
    // conservative 25 % throttle (KD-12). On this nearly-empty set the
    // pass is a bounded no-op (every volume is already within 10 pp of
    // the mean — the §5.3 step-6 objective); the MOVEMENT semantics are
    // pinned by test_rebalance_moves_toward_the_set_mean below.
    let _rec = fx
        .fs
        .admin_add_data_volume(oss2.to_str().unwrap(), false)
        .await
        .expect("online add");
    let recs_now = JobFabric::list_records(&fx.meta).await.unwrap();
    let rebalance: Vec<_> = recs_now
        .iter()
        .filter(|r| matches!(r.job_type, JobType::Rebalance))
        .collect();
    assert_eq!(
        rebalance.len(),
        1,
        "add-data must auto-submit exactly one rebalance job"
    );
    assert_eq!(
        rebalance[0].throttle_pct,
        squeezefs::jobs::REBALANCE_DEFAULT_THROTTLE_PCT,
        "the auto pass runs at the conservative default throttle"
    );
    let rebalance_id = rebalance[0].job_id.clone();
    let end = fx
        .fabric
        .wait_terminal(&rebalance_id, std::time::Duration::from_secs(120))
        .await
        .expect("rebalance terminal");
    assert_eq!(end, JobState::Completed);
    assert_eq!(read_back(&fx, ino, 32).await, expected, "bytes survive");

    // Opt-out: --no-rebalance adds capacity without a pass.
    let before = JobFabric::list_records(&fx.meta).await.unwrap().len();
    fx.fs
        .admin_add_data_volume(oss3.to_str().unwrap(), true)
        .await
        .expect("online add with --no-rebalance");
    let after = JobFabric::list_records(&fx.meta).await.unwrap().len();
    assert_eq!(after, before, "--no-rebalance must submit no job");
    fx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rebalance_moves_toward_the_set_mean() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // Seed data, then construct a > 10 pp imbalance by CLAMPING the
    // capacity bounds (the fill ratio is used/capacity — the same §5.2
    // numbers): oss1 ≈ 50 % full, oss2 ≈ 0 %.
    let ino = create_file(&fx, "seed.bin").await;
    let expected = striped_burst(&fx, ino, 64).await;
    let br = &fx.fs.router.backend_router;
    let (used1, used2) = {
        let a1 = br.backends.get("oss1").unwrap().block_allocator.clone();
        let a2 = br.backends.get("oss2").unwrap().block_allocator.clone();
        (
            a1.get_used_blocks() * a1.chunk_size(),
            a2.get_used_blocks() * a2.chunk_size(),
        )
    };
    // Clamp CAPACITIES so oss1 is overfull (fill 1.0) and oss2 sits
    // below `mean − 10 pp` regardless of the round-robin split:
    // cap1 = used1 ⇒ fill1 = 1.0; cap2 = 2 × total ⇒ fill2 ≈ 0.25 with
    // mean ≈ 0.4 — squarely inside the §5.3-step-6 objective band.
    let total_used = used1 + used2;
    assert!(used1 > 0 && used2 > 0, "seed must spread over both volumes");
    br.backends
        .get("oss1")
        .unwrap()
        .block_allocator
        .set_capacity_bytes(used1);
    br.backends
        .get("oss2")
        .unwrap()
        .block_allocator
        .set_capacity_bytes(total_used * 2);

    let job_id = fx
        .fabric
        .submit(squeezefs::jobs::JobSpec {
            job_type: JobType::Rebalance,
            throttle_pct: 100,
        })
        .await
        .expect("submit rebalance");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(120))
        .await
        .expect("rebalance terminal");
    assert_eq!(end, JobState::Completed, "the bounded pass completes");

    // Blocks moved toward the under-filled volume; bytes intact.
    let used2_after = {
        let a2 = br.backends.get("oss2").unwrap().block_allocator.clone();
        a2.get_used_blocks() * a2.chunk_size()
    };
    assert!(
        used2_after > used2,
        "the rebalance pass must move blocks onto the under-filled volume \
         (oss2 used {used2} -> {used2_after})"
    );
    assert_eq!(read_back(&fx, ino, 64).await, expected, "bytes survive");
    fx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_offline_add_writes_durable_queued_rebalance_record() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 512 * 1024 * 1024);
    let oss2 = make_file(dir.path(), "oss2", 512 * 1024 * 1024);
    format_meta(&meta, &[&oss1]).await;
    let meta_lvs = vec![meta.display().to_string()];

    // Offline add (no live mount): the default posture writes a durable
    // Queued rebalance record the next mount's fabric adopts (§5.3).
    squeezefs::config_ops::add_data_volume(&meta_lvs, oss2.to_str().unwrap(), false)
        .await
        .expect("offline add");
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(&meta)
        .await
        .unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let records = JobFabric::list_records(&routed).await.unwrap();
    let rebalance: Vec<_> = records
        .iter()
        .filter(|r| matches!(r.job_type, JobType::Rebalance))
        .collect();
    assert_eq!(rebalance.len(), 1, "offline add writes ONE queued record");
    assert_eq!(rebalance[0].state, JobState::Queued);
    drop(routed);

    // --no-rebalance suppresses the record.
    let oss3 = make_file(dir.path(), "oss3", 512 * 1024 * 1024);
    squeezefs::config_ops::add_data_volume(&meta_lvs, oss3.to_str().unwrap(), true)
        .await
        .expect("offline add --no-rebalance");
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(&meta)
        .await
        .unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let records = JobFabric::list_records(&routed).await.unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r.job_type, JobType::Rebalance))
            .count(),
        1,
        "--no-rebalance must not write a second record"
    );
}

// ---------------------------------------------------------------------------
// KD-6 / G-VL-3 a (cargo-level): crash mid-drain, resume by re-plan
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_crash_mid_drain_resumes_by_replan_and_preserves_bytes() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    const NBLOCKS: usize = 24;
    let ino = create_file(&fx, "crashy.bin").await;
    let expected = striped_burst(&fx, ino, NBLOCKS).await;

    // Park the mover mid-move (post-copy, pre-publish) and crash there —
    // the worst window: the destination copy exists, nothing published.
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let parked = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let parked_hook = parked.clone();
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
        let _ = hit_tx.send(());
        while parked_hook.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }));

    let _job_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");
    hit_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("mover reached mid-move");

    // Kill-9 analog: abort workers mid-task, persist nothing extra.
    fx.crash().await;
    parked.store(false, std::sync::atomic::Ordering::SeqCst);
    squeezefs::jobs::clear_evacuate_pre_publish_hook();

    // "Remount": reopen the set; the fabric adopts the durable job and
    // re-plans from current state (KD-6 — idempotent).
    let listed = squeezefs::config_ops::resolved_volume_records(&[meta.display().to_string()])
        .await
        .expect("volume records probe");
    assert_eq!(
        listed.iter().find(|r| r.id == "oss2").unwrap().state,
        VOL_STATE_DRAINING,
        "the durable draining state survives the crash"
    );
    let fx2 = open_fixture(&meta, &listed).await;

    // The adopted evacuation job converges without operator input.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let recs_now = JobFabric::list_records(&fx2.meta).await.unwrap();
        let evac: Vec<_> = recs_now
            .iter()
            .filter(|r| matches!(r.job_type, JobType::EvacuateVolume { .. }))
            .collect();
        assert!(!evac.is_empty(), "the durable job record must survive");
        if evac.iter().all(|r| r.state == JobState::Completed) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the resumed drain must converge (states: {:?})",
            evac.iter().map(|r| r.state).collect::<Vec<_>>()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Byte-identical manifest; volume retired; census empty.
    assert_eq!(read_back(&fx2, ino, NBLOCKS).await, expected);
    let states = fx2.fs.router.backend_router.volume_states();
    assert_eq!(
        states.iter().find(|s| s.id == "oss2").unwrap().state,
        VOL_STATE_RETIRED
    );
    assert!(victim_blocks_of(&fx2, ino, "oss2").await.is_empty());
    fx2.close().await;
}

// ---------------------------------------------------------------------------
// Offline drain (§5.8): the D0-guarded coordinator process shape
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_offline_remove_data_drains_to_retired() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let meta_lvs = vec![meta.display().to_string()];

    // Seed data through a mount-shaped fixture, then close it.
    let fx = open_fixture(&meta, &recs).await;
    const NBLOCKS: usize = 16;
    let ino = create_file(&fx, "cold.bin").await;
    let expected = striped_burst(&fx, ino, NBLOCKS).await;
    fx.close().await;

    // The offline verb: guarded open, run the drain in-process to
    // completion (§5.8 — the short-lived D0-guarded coordinator).
    squeezefs::config_ops::remove_data_volume_offline(&meta_lvs, "oss2", 100)
        .await
        .expect("offline remove-data drains to completion");

    let listed = squeezefs::config_ops::resolved_volume_records(&meta_lvs)
        .await
        .unwrap();
    let victim = listed.iter().find(|r| r.id == "oss2").unwrap();
    assert_eq!(victim.state, VOL_STATE_RETIRED, "retired durably");

    // Reopen and verify byte identity on the survivor set.
    let fx2 = open_fixture(&meta, &listed).await;
    assert_eq!(read_back(&fx2, ino, NBLOCKS).await, expected);
    assert!(victim_blocks_of(&fx2, ino, "oss2").await.is_empty());
    fx2.close().await;

    // Undrain of a retired volume refuses (retired is terminal, KD-5).
    let err = squeezefs::config_ops::undrain_data_volume(&meta_lvs, "oss2")
        .await
        .expect_err("undrain of a retired volume must refuse");
    assert!(!format!("{err}").is_empty());
}
