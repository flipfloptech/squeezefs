//! **Metadata-plane durability integrity** — the pre-RC engineering spec
//! §1 items DUR-3, DUR-4 and DUR-5, each red-first against the behavior
//! its anchor documents.
//!
//! | Item | Law under test |
//! |---|---|
//! | DUR-3 | a reclamation watermark advances only on a barrier that COVERS the ledger record it names |
//! | DUR-4 | a failed bitmap page write leaves the dirty set intact, and a bitmap image never ties an on-disk generation |
//! | DUR-5 | sector 0 is redundant (a torn primary still mounts) and concurrent incompat-bit setters never lose a bit |
//!
//! Every leg drives the real machinery — the KV backend's own checkpoint
//! cycle, its own `sync_device`, the real superblock writers — over the
//! `uring_fs` fault shim (torn write, persistent sector error, and the
//! two DUR-3 stalls that hold an operation at an exact point so the
//! interleaving is built rather than raced).

use squeezefs::meta_backend::kv::alloc_ext::{
    bitmap_region_len, decode_bitmap_page, ExtentAllocator, ALLOC_PAGE_LEN,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_guest_slots_bit, set_layout_deltas_bit, set_slot_migration_bit,
    set_volume_lifecycle_bit, VolumeFormat, FEATURE_INCOMPAT_KV_GUEST_SLOTS,
    FEATURE_INCOMPAT_KV_LAYOUT_DELTAS, FEATURE_INCOMPAT_KV_SLOT_MIGRATION,
    FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
};
use squeezefs::meta_backend::Metadata;
use squeezefs::uring_fs;
use std::sync::Arc;
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 65536;

/// RAII: the shim state is process-global — never leak a fault.
struct FaultGuard;
impl Drop for FaultGuard {
    fn drop(&mut self) {
        uring_fs::clear_faults();
    }
}

async fn sandbox(len: u64) -> (Arc<KvMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(len).unwrap();
    format_v3(
        file.path(),
        len,
        &FormatV3Options {
            node_size: NODE_SIZE,
            journal_len_override: None,
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let kv = KvMetaBackend::open(file.path()).await.expect("open");
    (kv, file)
}

/// Dirty the trees so the next checkpoint has a tail to publish.
async fn churn(kv: &Arc<KvMetaBackend>, tag: &str, n: usize) {
    for i in 0..n {
        kv.create(1, &format!("{tag}_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
    }
}

// ---------------------------------------------------------------------------
// DUR-3 · Reclamation watermarks must only advance on barriers that cover
//         them (`backend.rs` `after_durable_barrier`; `checkpoint.rs`'s
//         UNCONDITIONAL `pending_reclaim` push).
// ---------------------------------------------------------------------------

/// The exact shape the spec names: a cadence checkpoint writes its ledger
/// slot and pushes the tail **while another barrier is in flight**. That
/// barrier's `fdatasync` was submitted before the ledger record existed,
/// so it cannot make it durable — and must therefore not release the
/// reclamation the record covers (journal hole / unmountable-volume
/// vector).
///
/// Built, not raced: the write stall parks the cadence cycle exactly at
/// its ledger write (its own barrier already done), the barrier stall
/// then holds a third party's `fdatasync` open across the push.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dur3_a_barrier_in_flight_before_the_push_never_drains_it() {
    let _g = FaultGuard;
    let (kv, file) = sandbox(VOL_LEN).await;
    let path = file.path().to_path_buf();

    // Settle every watermark first: after this, reclamation is quiescent.
    churn(&kv, "seed", 8).await;
    kv.checkpoint_now().await.expect("settle checkpoint");
    let settled = kv.journal_ring().core().reusable_upto();

    // Fresh journal traffic so the next cadence checkpoint publishes a
    // STRICTLY higher tail (otherwise the leg is vacuous).
    churn(&kv, "window", 8).await;
    assert!(
        kv.journal_ring().core().head() > settled,
        "leg is vacuous: no journal traffic past the settled watermark"
    );

    // (1) Park the cadence cycle at its ledger write. Its own barrier has
    //     completed by then, so the coalescer is free.
    let ledger = kv.superblock().root_ledger;
    let mut at_ledger = uring_fs::arm_write_stall(&path, ledger.start, ledger.len);
    let cp = {
        let kv = kv.clone();
        tokio::spawn(async move { kv.checkpoint_cadence().await })
    };
    at_ledger
        .recv()
        .await
        .expect("cadence checkpoint never reached its ledger write");

    // (2) A third party's barrier starts NOW — before the ledger record
    //     exists — and is held open across the push.
    let mut at_barrier = uring_fs::arm_barrier_stall(&path);
    let barrier = {
        let kv = kv.clone();
        tokio::spawn(async move { kv.sync_device().await })
    };
    at_barrier
        .recv()
        .await
        .expect("the third-party barrier never reached the stall");

    // (3) The ledger write lands and the cycle pushes its tail — inside
    //     the barrier's window.
    uring_fs::release_write_stall(&path);
    cp.await.expect("checkpoint task").expect("cadence cycle");

    // (4) The stale barrier completes and does its post-barrier work.
    uring_fs::release_barrier_stall(&path);
    barrier.await.expect("barrier task").expect("sync_device");

    let after_stale = kv.journal_ring().core().reusable_upto();
    assert_eq!(
        after_stale, settled,
        "a barrier whose fdatasync was submitted BEFORE the ledger record was \
         written released the reclamation that record covers (spec DUR-3: journal \
         hole + unmountable-volume vector)"
    );

    // Liveness: reclamation must not be stranded — the next barrier, which
    // genuinely starts after the push, releases it.
    kv.sync_device().await.expect("covering barrier");
    assert!(
        kv.journal_ring().core().reusable_upto() > settled,
        "the covering barrier failed to release the deferred reclamation — the \
         epoch gate stranded the watermark instead of deferring it one barrier"
    );
}

/// The plain-cadence face of the same law: a checkpoint that defers
/// durability leaves its reclamation pending until a covering barrier
/// runs, and that barrier releases ALL of it (never a partial drain).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dur3_deferred_reclamation_releases_on_the_next_covering_barrier() {
    let _g = FaultGuard;
    let (kv, _file) = sandbox(VOL_LEN).await;

    churn(&kv, "seed", 8).await;
    kv.checkpoint_now().await.expect("settle checkpoint");

    for round in 0..3 {
        let before = kv.journal_ring().core().reusable_upto();
        churn(&kv, &format!("r{round}"), 8).await;
        kv.checkpoint_cadence().await.expect("cadence cycle");
        kv.sync_device().await.expect("covering barrier");
        assert!(
            kv.journal_ring().core().reusable_upto() > before,
            "round {round}: a covering barrier must release the cadence \
             checkpoint's reclamation"
        );
    }
}

// ---------------------------------------------------------------------------
// DUR-4 · A failed bitmap page write must not discard the dirty set
//         (`alloc_ext.rs::write_dirty_pages`).
// ---------------------------------------------------------------------------

fn bitmap_alloc(total_extents: u64) -> ExtentAllocator {
    ExtentAllocator::format(total_extents, 8, 64)
}

/// Fault-inject the bitmap write, then verify the NEXT successful cycle
/// persists exactly the bits the failed one was carrying. Today the dirty
/// words are `swap(0)`-ed before the first fallible step, so the failure
/// drops them and the extents come back FREE on the next mount while live
/// nodes occupy them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dur4_a_failed_bitmap_write_keeps_its_dirty_bits() {
    let _g = FaultGuard;
    let f = NamedTempFile::new().unwrap();
    let total_extents: u64 = 4096;
    let base = 0u64;
    f.as_file()
        .set_len(bitmap_region_len(total_extents))
        .unwrap();

    let alloc = bitmap_alloc(total_extents);
    // Page 0 only: a fresh allocator marks every page dirty, so drain them
    // once with a clean write first.
    alloc
        .write_dirty_pages(f.path(), base, 1)
        .await
        .expect("initial bitmap write");

    // Claim an extent: page 0 goes dirty again with a bit that exists
    // NOWHERE else once the journal records fall out of the window.
    let claimed = alloc.claim_user().expect("claim");
    assert!(alloc.is_allocated(claimed));

    // The page-0 A/B pair lives at [base, base + 2 * ALLOC_PAGE_LEN); the
    // first write took slot A, so this one targets slot B.
    uring_fs::arm_sector_write_error(base + ALLOC_PAGE_LEN);
    let err = alloc.write_dirty_pages(f.path(), base, 2).await;
    assert!(err.is_err(), "the injected sector error must surface");
    uring_fs::clear_faults();

    // The retry must still know page 0 is dirty.
    let written = alloc
        .write_dirty_pages(f.path(), base, 3)
        .await
        .expect("bitmap retry");
    assert!(
        written.contains(&0),
        "the failed write DISCARDED page 0's dirty bit — the retry wrote {written:?}, \
         so the claim is on disk nowhere (spec DUR-4: the bitmap reports the extent \
         free while a live node occupies it)"
    );

    // And the durable image really carries the claim.
    let raw = uring_fs::read_at(f.path(), base, 2 * ALLOC_PAGE_LEN as usize)
        .await
        .expect("read page pair");
    let newest = [0usize, ALLOC_PAGE_LEN as usize]
        .into_iter()
        .filter_map(|off| {
            decode_bitmap_page(&raw[off..off + ALLOC_PAGE_LEN as usize], 0)
                .ok()
                .map(|(gen, bits)| (gen, bits.to_vec()))
        })
        .max_by_key(|(gen, _)| *gen)
        .expect("a valid page-0 slot");
    let byte = newest.1[(claimed / 8) as usize];
    assert!(
        byte & (1 << (claimed % 8)) != 0,
        "extent {claimed} reads FREE in the durable bitmap after a failed-then-retried \
         write"
    );
}

/// The A/B protocol's monotonicity: a bitmap image may never carry a
/// generation that ties (or trails) the newest valid on-disk copy —
/// newest-valid-wins would then pick between the slots arbitrarily. The
/// guard was `debug_assert!`-only (release: silent tie). A checkpoint
/// whose ledger write failed retries with the SAME seq, so the tie is a
/// reachable steady-state shape, not a synthetic one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dur4_bitmap_generations_never_tie_on_disk() {
    let _g = FaultGuard;
    let f = NamedTempFile::new().unwrap();
    let total_extents: u64 = 4096;
    f.as_file()
        .set_len(bitmap_region_len(total_extents))
        .unwrap();
    let alloc = bitmap_alloc(total_extents);
    alloc
        .write_dirty_pages(f.path(), 0, 7)
        .await
        .expect("first write");

    // The retry shape: same generation, new bits.
    let claimed = alloc.claim_user().expect("claim");
    alloc
        .write_dirty_pages(f.path(), 0, 7)
        .await
        .expect("a same-generation retry must persist, not panic or tie");

    let raw = uring_fs::read_at(f.path(), 0, 2 * ALLOC_PAGE_LEN as usize)
        .await
        .expect("read page pair");
    let mut gens = Vec::new();
    let mut newest: Option<(u64, Vec<u8>)> = None;
    for off in [0usize, ALLOC_PAGE_LEN as usize] {
        if let Ok((gen, bits)) = decode_bitmap_page(&raw[off..off + ALLOC_PAGE_LEN as usize], 0) {
            gens.push(gen);
            if newest.as_ref().is_none_or(|(g, _)| gen > *g) {
                newest = Some((gen, bits.to_vec()));
            }
        }
    }
    assert_eq!(gens.len(), 2, "both slots must hold a valid page image");
    assert_ne!(
        gens[0], gens[1],
        "the A and B slots carry the SAME generation: newest-valid-wins now picks \
         between them arbitrarily (spec DUR-4)"
    );
    let (_, bits) = newest.expect("newest slot");
    assert!(
        bits[(claimed / 8) as usize] & (1 << (claimed % 8)) != 0,
        "newest-valid-wins must resolve to the image carrying the newer claim"
    );
}

// ---------------------------------------------------------------------------
// DUR-5 · The superblock needs redundancy and serialized updates
//         (`superblock.rs`; the runtime `set_*_bit` writers).
// ---------------------------------------------------------------------------

/// A torn sector-0 write must not make the volume unmountable: the
/// redundant copy carries it. Reachable in steady state — the layout-delta
/// bit is stamped during ordinary write traffic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dur5_a_torn_primary_superblock_still_classifies() {
    let _g = FaultGuard;
    let (kv, file) = sandbox(VOL_LEN).await;
    let path = file.path().to_path_buf();
    kv.shutdown().await.expect("clean shutdown");
    drop(kv);

    // A live-traffic bit stamp writes sector 0 — and its redundant copy.
    set_layout_deltas_bit(&path).await.expect("stamp bit");

    // Tear sector 0 mid-write: 512 bytes land, the rest never does.
    uring_fs::arm_torn_write(0, 512);
    let mut torn = vec![0xAB; 4096];
    torn[..8].copy_from_slice(b"METALV01");
    let _ = uring_fs::write_at(&path, 0, bytes::Bytes::from(torn)).await;
    uring_fs::clear_faults();

    match classify_volume(&path).await {
        Ok(VolumeFormat::V3(sb)) => assert!(
            sb.features_incompat & FEATURE_INCOMPAT_KV_LAYOUT_DELTAS != 0,
            "the recovered superblock lost the stamped layout-delta bit"
        ),
        other => panic!(
            "a torn sector 0 made the volume permanently unmountable — no redundant \
             superblock copy was consulted (spec DUR-5): {other:?}"
        ),
    }

    // And the volume really mounts off the recovered copy.
    let kv = KvMetaBackend::open(&path)
        .await
        .expect("mount after a torn sector 0");
    kv.shutdown().await.expect("shutdown");
}

/// Concurrent incompat-bit setters must not lose a bit: `set_incompat_bit`
/// is a read-modify-write, so two racing stampers both read the old word
/// and the second write erases the first's bit — defeating the
/// "bit durable before the record it gates" ordering the bits exist for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dur5_concurrent_incompat_bit_setters_never_lose_a_bit() {
    let _g = FaultGuard;
    // Repeat: one round could pass by luck even on the racy path.
    for round in 0..4 {
        let (kv, file) = sandbox(VOL_LEN).await;
        let path = Arc::new(file.path().to_path_buf());
        kv.shutdown().await.expect("clean shutdown");
        drop(kv);

        let mut tasks = Vec::new();
        for which in 0..4 {
            let p = path.clone();
            tasks.push(tokio::spawn(async move {
                match which {
                    0 => set_layout_deltas_bit(&p).await,
                    1 => set_guest_slots_bit(&p).await,
                    2 => set_volume_lifecycle_bit(&p).await,
                    _ => set_slot_migration_bit(&p).await,
                }
            }));
        }
        for t in tasks {
            t.await.expect("stamp task").expect("stamp");
        }

        let sb = match classify_volume(&path).await.expect("classify") {
            VolumeFormat::V3(sb) => sb,
            other => panic!("round {round}: volume no longer classifies: {other:?}"),
        };
        for (bit, name) in [
            (FEATURE_INCOMPAT_KV_LAYOUT_DELTAS, "layout-deltas"),
            (FEATURE_INCOMPAT_KV_GUEST_SLOTS, "guest-slots"),
            (FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE, "volume-lifecycle"),
            (FEATURE_INCOMPAT_KV_SLOT_MIGRATION, "slot-migration"),
        ] {
            assert!(
                sb.features_incompat & bit != 0,
                "round {round}: the {name} bit was LOST by a concurrent setter \
                 (features_incompat={:#x}) — an unsynchronized read-modify-write on \
                 sector 0 (spec DUR-5)",
                sb.features_incompat
            );
        }
    }
}
