//! The staged-ring generation ABA — the aged zeros-LOSS root cause
//! (follow-up to C; pre-existing on e1601bc, aged fsx 3/3):
//!
//! `remove_staged_if_generation` is the promotion commit/removal guard: a
//! promotion captures the stage generation when it reads the blob and may
//! only publish + release the ring entry if the generation is unchanged.
//! But the generation lived INSIDE the ledger entry, and removal deletes
//! the entry — the next re-stage of the same file_id re-created it with
//! the generation counter RESTARTED. A queued/slow promotion that read the
//! OLD incarnation's image then passed its commit check against the NEW
//! incarnation's recycled generation, published the stale image as the
//! durable mapping, and destroyed the ring's ONLY copy of the newer acked
//! bytes (tape evidence: two promote/remove cycles at gen=1 for one
//! file_id, both `removed=Ok(true)`, in the failing aged round). Reads
//! then served the stale image — extended/rewritten ranges came back as
//! ZEROS.
//!
//! Contract pinned here: a stage generation captured under one ring-entry
//! incarnation can NEVER match a later incarnation — stale removals and
//! stale promotion commits must fail closed.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use std::sync::Arc;
use tempfile::tempdir;

async fn staging_fixture(ns: &str) -> (TieredCache, tempfile::TempDir, tempfile::NamedTempFile) {
    let dlm = DlmClient::new("local").unwrap();
    let b = tempfile::NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("16MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme,
        None,
    )
    .await
    .unwrap();
    (cache, s, b)
}

/// The ABA scenario, step by step:
///   1. stage(id, I1)          — incarnation 1; capture gen g1 (promotion P2's read)
///   2. remove_if_gen(id, g1)  — promotion P1 legitimately releases the entry
///   3. stage(id, I2)          — incarnation 2 (newer acked bytes; sole copy)
///   4. remove_if_gen(id, g1)  — P2's STALE removal must FAIL: g1 belongs to a
///      dead incarnation. Pre-fix the recycled counter made it match — the
///      removal destroyed I2 (`removed=Ok(true)` on the aged tape) and the
///      stale promoted image took over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_generation_never_matches_a_new_incarnation() {
    let (cache, _s, _b) = staging_fixture("aba_ns_a").await;
    let nvme = &cache.nvme;
    let id = "aba-file-id-1";

    nvme.stage_write(
        "inode_9001",
        id,
        bytes::Bytes::from(vec![0xAA; 64 * 1024]),
        7,
    )
    .await
    .unwrap();
    let g1 = nvme
        .staged_generation(id)
        .await
        .expect("staged after write");

    // P1: legitimate release of incarnation 1.
    assert!(
        nvme.remove_staged_if_generation(id, g1),
        "same-incarnation removal must succeed"
    );
    assert!(nvme.read_staged(id).is_none(), "incarnation 1 released");

    // Incarnation 2: newer acked bytes — the ring is their SOLE copy.
    nvme.stage_write(
        "inode_9001",
        id,
        bytes::Bytes::from(vec![0xBB; 96 * 1024]),
        7,
    )
    .await
    .unwrap();
    let g2 = nvme
        .staged_generation(id)
        .await
        .expect("staged after re-write");
    assert_ne!(
        g2, g1,
        "a new incarnation must never recycle a prior incarnation's generation \
         (the promote-vs-restage ABA that destroyed newer acked bytes)"
    );

    // P2's stale removal (captured g1 before the re-stage) must fail closed.
    assert!(
        !nvme.remove_staged_if_generation(id, g1),
        "a generation captured under a dead incarnation must never release \
         the new incarnation's ring entry"
    );
    let blob = nvme.read_staged(id).expect("incarnation 2 must survive");
    assert_eq!(blob.len(), 96 * 1024);
    assert!(blob.iter().all(|&b| b == 0xBB), "newer bytes intact");
}

/// Same-generation double-release: after ONE successful removal the same
/// generation must not release anything again (the second queued promotion
/// on the tape) — even if a re-stage re-created the entry in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generation_is_single_use_across_release_and_restage_cycles() {
    let (cache, _s, _b) = staging_fixture("aba_ns_b").await;
    let nvme = &cache.nvme;
    let id = "aba-file-id-2";

    let mut seen = std::collections::HashSet::new();
    for round in 0u8..6 {
        nvme.stage_write(
            "inode_9002",
            id,
            bytes::Bytes::from(vec![round; 32 * 1024]),
            7,
        )
        .await
        .unwrap();
        let g = nvme.staged_generation(id).await.expect("staged");
        assert!(
            seen.insert(g),
            "round {round}: generation {g} was already used by a prior \
             incarnation — recycled generations are the ABA"
        );
        assert!(nvme.remove_staged_if_generation(id, g));
        assert!(
            !nvme.remove_staged_if_generation(id, g),
            "round {round}: a consumed generation must never release again"
        );
    }
}
