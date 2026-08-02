//! Rewrite-publish-drain campaign (2026-08-01) — the **publish
//! decomposition** contracts (Phase 1 instrument;
//! `.benchmarks/2026-08-01-rewrite-publish-drain.md`).
//!
//! The conviction chain: the write wall's dominant DISPLACEABLE term is
//! `admit_gate` ≈ 2.8–3.1 ms/op — CONSERVED queueing whose drain is
//! priced by the in-pipe legs behind the ACK, and the drain's named
//! anomaly is **publish 6.36 ms/block under rewrite vs 0.83 ms fresh —
//! a 7.7× rewrite-specific tax** (`.benchmarks/2026-08-01-write-in-
//! handler.md` §9.2). Five hypotheses died to instruments this month;
//! this family decomposes the publish span into named constituents with
//! CLOSED residue before anything is built.
//!
//! The instrument: an ALWAYS-ON `publish_phase_ns` histogram family
//! (the `write_pipeline_phase_ns` pattern and cost contract — one
//! `Instant` read + one relaxed `fetch_add` per boundary per publish
//! op/pass, invisible at any credible block rate) decomposing the
//! pipeline `publish` phase: conveyor queue wait → pass lock wait →
//! RMW base resolve → batch apply → save encode → indirect blob write
//! → meta commit → total; plus the **base-provenance ledger**
//! (`publish_base_dirty_serves` / `publish_base_fetches` — every pass
//! resolves exactly one RMW base) and the **full-save decision ledger**
//! (`publish_full_save_{indirect,chain_cap,other}` — why a publish-class
//! save fell off the O(batch) delta path).
//!
//! Contracts:
//!
//! 1. **The family exists always-on** with exactly the eight phase
//!    keys, surfaced on the stats inode unconditionally (no profile
//!    gate).
//! 2. **A coalesced pipeline stream drives the phases with a closed
//!    ledger**: per-op phases (queue_wait/total) account every
//!    conveyor-published block; per-pass phases (lock_wait/base_fetch/
//!    apply) and the base-provenance ledger account every pass exactly
//!    (`dirty_serves + fetches == lock_wait spans`); per-save phases
//!    (save_encode/meta_commit) account every batch.
//! 3. **The rewrite conviction is observable**: a rewrite whose layout
//!    was persisted-and-cleaned by fsync pays `publish_base_fetches`
//!    (the clean-base backend refetch the fresh path dodges via its
//!    dirty RAM authority) — the instrument names the rewrite-specific
//!    constituent instead of guessing it.
//! 4. **Recording is phase-exact** (pure): a span recorded against a
//!    phase lands in that phase's histogram and no other.
//!
//! RED against dev 31c89c5: the family does not exist.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    publish_phase_json, publish_phase_record, PublishPhase, SqueezefsFilesystem, METRICS,
    STATS_INODE,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    // A real memory budget (the publish_coalesce_tests finding): without
    // it the pipeline's R5 cap clamps depth to one block and the
    // coalescing machinery under test serializes.
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

const PHASES: [&str; 12] = [
    "queue_wait",
    "lock_wait",
    "base_fetch",
    "apply",
    "save_encode",
    "blob_write",
    "meta_commit",
    "commit_guard",
    "commit_inode_read",
    "commit_slot_probe",
    "commit_tx_wait",
    "total",
];

/// Sum of one phase histogram's buckets (= spans recorded).
fn phase_count(family: &serde_json::Value, phase: &str) -> u64 {
    family
        .get(phase)
        .unwrap_or_else(|| panic!("phase key {phase} missing from publish_phase_ns"))
        .as_object()
        .expect("phase histogram must be a bucket object")
        .values()
        .map(|v| v.as_u64().expect("bucket counts are u64"))
        .sum()
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The publish_coalesce_tests harness: real v3 meta backend, real
/// striped write path, real publish conveyor.
async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    // Pin the W1 patch path OFF (downscaled BS would make sub-block
    // segments patch-eligible and bypass the publish machinery under
    // test).
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid,
    })
    .unwrap()
    .build(m.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let be = KvMetaBackend::open(m.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// Write `blocks` whole blocks at `[start..start+blocks)`, fsync, and
/// drain the pipeline (every publish terminal).
async fn stream_blocks(h: &H, ino: u64, start: u32, blocks: u32, tag: u8) {
    for b in start..start + blocks {
        write_at(
            h,
            ino,
            b as u64 * BS,
            &pattern(BS as usize, tag ^ (b as u8)),
        )
        .await;
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "pipeline must drain"
    );
}

// =========================================================================
// Contract 1 — the family exists always-on with exactly the eight keys
// and rides the stats inode UNGATED.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_phase_family_is_always_on_with_exact_keys() {
    let _g = serial().await;
    assert!(
        std::env::var("SQUEEZEFS_OP_PROFILE").is_err(),
        "fixture premise: the profile rig must be OFF — this family is \
         deliberately always-on"
    );
    let family = publish_phase_json();
    let obj = family
        .as_object()
        .expect("publish_phase_ns must be an object");
    assert_eq!(
        obj.len(),
        PHASES.len(),
        "exactly the twelve publish phases: {obj:?}"
    );
    for p in PHASES {
        let _ = phase_count(&family, p); // key exists, histogram-shaped
    }

    // Stats-inode surface, ungated.
    let h = make([0xA1; 16], "pub_phase_stats_surface").await;
    let reply =
        h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value =
        serde_json::from_slice(&reply.data).expect("stats inode must be valid JSON");
    let fam = stats
        .get("metrics")
        .and_then(|m| m.get("publish_phase_ns"))
        .expect("stats inode metrics must carry publish_phase_ns UNGATED");
    for p in PHASES {
        let _ = phase_count(fam, p);
    }
    // The ledger counters ride the stats inode too (field attribution
    // needs them without a remount).
    for k in [
        "publish_base_dirty_serves",
        "publish_base_fetches",
        "publish_base_ram_serves",
        "publish_commit_groups",
        "publish_commit_group_saves",
        "publish_full_save_indirect",
        "publish_full_save_chain_cap",
        "publish_full_save_other",
        "publish_indirect_blob_bytes",
        "layout_indirect_map_reads",
        "layout_indirect_map_read_bytes",
    ] {
        assert!(
            stats.get("metrics").and_then(|m| m.get(k)).is_some(),
            "stats inode metrics must carry {k}"
        );
    }
}

// =========================================================================
// Contract 2 — a coalesced pipeline stream drives the phases with a
// CLOSED ledger (per-op, per-pass, per-save counts all account).
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coalesced_stream_records_phases_with_closed_ledger() {
    let _g = serial().await;
    let h = make([0xA2; 16], "pub_phase_stream").await;
    let ino = create(&h, "phased").await;
    // Striped fixture first: the fresh small-file route promotes via
    // staging (direct primitive, not the conveyor) — the measured
    // stream below is all write-through.
    stream_blocks(&h, ino, 0, 2, 0x22).await;

    let f0 = publish_phase_json();
    let batches_0 = METRICS.layout_publish_batches.load(Ordering::Relaxed);
    let blocks_0 = METRICS
        .layout_publish_batched_blocks
        .load(Ordering::Relaxed);
    let dirty_0 = METRICS.publish_base_dirty_serves.load(Ordering::Relaxed);
    let fetch_0 = METRICS.publish_base_fetches.load(Ordering::Relaxed);
    let ram_0 = METRICS.publish_base_ram_serves.load(Ordering::Relaxed);
    let delta_0 = squeezefs::meta_backend::kv::META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed);
    let groups_0 = METRICS.publish_commit_groups.load(Ordering::Relaxed);

    let n_blocks = 24u32;
    stream_blocks(&h, ino, 2, n_blocks, 0x33).await;

    let f1 = publish_phase_json();
    let batches = METRICS.layout_publish_batches.load(Ordering::Relaxed) - batches_0;
    let pub_blocks = METRICS
        .layout_publish_batched_blocks
        .load(Ordering::Relaxed)
        - blocks_0;
    let dirty = METRICS.publish_base_dirty_serves.load(Ordering::Relaxed) - dirty_0;
    let fetch = METRICS.publish_base_fetches.load(Ordering::Relaxed) - fetch_0;
    let ram = METRICS.publish_base_ram_serves.load(Ordering::Relaxed) - ram_0;

    assert!(
        pub_blocks >= n_blocks as u64,
        "fixture premise: the stream must ride the conveyor \
         (batched_blocks {pub_blocks} < {n_blocks})"
    );
    assert!(batches >= 1, "at least one publish pass must have run");

    let d = |p: &str| phase_count(&f1, p) - phase_count(&f0, p);

    // Per-op phases: every conveyor-published op records queue_wait at
    // drain and total at terminal fan-out.
    assert!(
        d("queue_wait") >= pub_blocks,
        "queue_wait spans ({}) must account every conveyor op ({pub_blocks})",
        d("queue_wait")
    );
    assert!(
        d("total") >= pub_blocks,
        "total spans ({}) must account every conveyor op ({pub_blocks})",
        d("total")
    );

    // Per-pass phases: lock_wait/base_fetch/apply record once per pass;
    // the base-provenance ledger closes against them EXACTLY (every
    // pass resolves exactly one RMW base).
    for p in ["lock_wait", "base_fetch", "apply"] {
        assert!(
            d(p) >= batches,
            "{p} spans ({}) must account every pass (>= {batches} batches)",
            d(p)
        );
    }
    assert_eq!(
        dirty + fetch + ram,
        d("lock_wait"),
        "base-provenance ledger must close: dirty_serves {dirty} + fetches \
         {fetch} + ram_serves {ram} == passes {}",
        d("lock_wait")
    );

    // Per-save phases: every committed batch records save_encode +
    // meta_commit (publish-class saves only).
    for p in ["save_encode", "meta_commit"] {
        assert!(
            d(p) >= batches,
            "{p} spans ({}) must account every committed batch ({batches})",
            d(p)
        );
    }
    // The meta_commit INTERIOR (the field's dominant constituent —
    // 2.07 ms/pass rewrite vs 0.39 fresh): delta saves record the
    // per-op legs (inode read / slot probe); the aggregated commit
    // (Lever B) records guard / tx-wait once per GROUP.
    let delta_commits =
        squeezefs::meta_backend::kv::META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed) - delta_0;
    let groups = METRICS.publish_commit_groups.load(Ordering::Relaxed) - groups_0;
    assert!(
        delta_commits >= 1,
        "fixture premise: the stream must stage layout deltas"
    );
    assert!(
        groups >= 1,
        "fixture premise: delta saves must ride the aggregated commit"
    );
    for p in ["commit_inode_read", "commit_slot_probe"] {
        assert!(
            d(p) >= delta_commits,
            "{p} spans ({}) must account every delta save ({delta_commits})",
            d(p)
        );
    }
    for p in ["commit_guard", "commit_tx_wait"] {
        assert!(
            d(p) >= groups,
            "{p} spans ({}) must account every aggregated commit ({groups})",
            d(p)
        );
    }

    // No indirect engagement at this venue (inline maps): blob_write
    // stays flat and the full-save decision ledger stays quiet on the
    // indirect arm.
    assert_eq!(d("blob_write"), 0, "inline venue must never write blobs");
}

// =========================================================================
// Contract 3 — the rewrite base provenance is OBSERVABLE. Phase 1's
// red pinned the conviction (a persisted-and-cleaned base made every
// rewrite pass pay a backend refetch); Phase 2's Lever A moved the law
// — this contract moved with it, as its red-phase text promised: the
// rewrite now serves its RMW base from the era-coherent RAM entry
// (`publish_base_ram_serves`), and the ledger still accounts every
// pass. The full Lever A suite lives in
// `tests/publish_drain_economy_tests.rs`.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewrite_on_clean_persisted_base_is_ledger_attributed() {
    let _g = serial().await;
    // The rewrite-shadow epoch (Idea 1) records rewrite publishes
    // RAM-only — this contract pins the durable publish-pass
    // base-provenance ledger specifically, so the lever is off
    // (tests/rewrite_shadow_tests.rs owns the epoch venue).
    struct ShadowOff;
    impl Drop for ShadowOff {
        fn drop(&mut self) {
            squeezefs::routing::set_rewrite_shadow(true);
        }
    }
    let _so = ShadowOff;
    squeezefs::routing::set_rewrite_shadow(false);
    let h = make([0xA3; 16], "pub_phase_rewrite").await;
    let ino = create(&h, "rw").await;

    // Fresh stream: publishes ride the dirty RAM authority (the write
    // path's size-floor bumps mark the entry dirty ahead of every
    // publish).
    let n_blocks = 12u32;
    stream_blocks(&h, ino, 0, n_blocks, 0x44).await;

    // fsync persisted + CLEANED the layout (persist_dirty_layout_if_
    // needed): the RAM entry is now clean — the rewrite premise.
    let clean =
        h.fs.router
            .metadata_cache
            .get(&ino)
            .expect("fixture premise: cached entry present after fsync");
    assert!(
        !clean.layout_dirty,
        "fixture premise: fsync must persist-and-clean the layout"
    );

    let fetch_0 = METRICS.publish_base_fetches.load(Ordering::Relaxed);
    let ram_0 = METRICS.publish_base_ram_serves.load(Ordering::Relaxed);
    let batches_0 = METRICS.layout_publish_batches.load(Ordering::Relaxed);
    let f0 = publish_phase_json();

    // The rewrite: same blocks, new bytes — size never grows, so no
    // dirty marking precedes the publishes.
    stream_blocks(&h, ino, 0, n_blocks, 0x55).await;

    let fetch = METRICS.publish_base_fetches.load(Ordering::Relaxed) - fetch_0;
    let ram = METRICS.publish_base_ram_serves.load(Ordering::Relaxed) - ram_0;
    let batches = METRICS.layout_publish_batches.load(Ordering::Relaxed) - batches_0;
    let f1 = publish_phase_json();
    assert!(batches >= 1, "rewrite passes must publish");
    assert!(
        ram >= batches.saturating_sub(fetch),
        "rewrite passes on a clean persisted base must serve the RMW base \
         from the era-coherent RAM entry (ram {ram}, fetch {fetch}, \
         batches {batches})"
    );
    assert!(
        fetch <= 1,
        "the Lever A law: at most one backend fetch on a steady rewrite \
         (got {fetch})"
    );
    assert!(
        phase_count(&f1, "base_fetch") > phase_count(&f0, "base_fetch"),
        "base_fetch spans record the resolution on every pass (RAM serves \
         included — the span is the resolve, not the fetch)"
    );
}

// =========================================================================
// Contract 5 — the M7 journal-conveyor pass interior (`meta_txpass_
// phase_ns`): the field named commit_tx_wait (2.23 of 2.26 ms) as the
// dominant publish constituent, but that span mixes conveyor queueing,
// the union leaf-lock window (checkpoint-freeze interference), and the
// journal device write — different Phase 2 builds. This family closes
// the residue: per-tx queue wait + per-pass admission/leaf-locks/
// journal-write/total.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_txpass_family_records_the_conveyor_interior() {
    let _g = serial().await;
    let h = make([0xA4; 16], "pub_phase_txpass").await;
    let ino = create(&h, "txp").await;
    stream_blocks(&h, ino, 0, 2, 0x66).await;

    let f0 = squeezefs::fuse_client::meta_txpass_phase_json();
    let passes_0 = squeezefs::meta_backend::kv::META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed);
    stream_blocks(&h, ino, 2, 8, 0x77).await;
    let f1 = squeezefs::fuse_client::meta_txpass_phase_json();
    let passes =
        squeezefs::meta_backend::kv::META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed) - passes_0;
    assert!(passes >= 1, "fixture premise: conveyor passes must run");

    let d = |p: &str| phase_count(&f1, p) - phase_count(&f0, p);
    assert!(
        d("tx_queue_wait") >= passes,
        "tx_queue_wait ({}) must account every drained tx (>= {passes} passes)",
        d("tx_queue_wait")
    );
    for p in [
        "pass_admission",
        "pass_leaf_locks",
        "pass_journal_write",
        "pass_total",
    ] {
        assert!(
            d(p) >= passes,
            "{p} spans ({}) must account every committed pass ({passes})",
            d(p)
        );
    }

    // Stats-inode surface, ungated.
    let reply =
        h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value =
        serde_json::from_slice(&reply.data).expect("stats inode must be valid JSON");
    let fam = stats
        .get("metrics")
        .and_then(|m| m.get("meta_txpass_phase_ns"))
        .expect("stats inode metrics must carry meta_txpass_phase_ns UNGATED");
    for p in [
        "tx_queue_wait",
        "pass_admission",
        "pass_leaf_locks",
        "pass_journal_write",
        "pass_total",
    ] {
        let _ = phase_count(fam, p);
    }
}

// =========================================================================
// Contract 4 — recording is phase-exact (pure).
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_phase_recording_is_phase_exact() {
    let _g = serial().await;
    let f0 = publish_phase_json();
    let t0 = std::time::Instant::now();
    publish_phase_record(PublishPhase::QueueWait, t0);
    let f1 = publish_phase_json();
    assert_eq!(
        phase_count(&f1, "queue_wait"),
        phase_count(&f0, "queue_wait") + 1,
        "the recorded span must land in its phase"
    );
    for p in PHASES.iter().filter(|p| **p != "queue_wait") {
        assert_eq!(
            phase_count(&f1, p),
            phase_count(&f0, p),
            "no other phase may move"
        );
    }
}
