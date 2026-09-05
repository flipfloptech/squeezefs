//! PR 1 of `docs/design-write-inode-convoy.md` — the rebuilt write
//! lock-scope classifier, red-first.
//!
//! Contracts pinned here:
//! - §4.1 classifier matrix: the THREE classes, the folded staged bypass,
//!   and every any-doubt-routes-exclusive arm (floor miss, map miss/
//!   anomaly, hole, beyond-floor).
//! - R2-M2 range validation: zero-length success with ZERO side effects
//!   (no lease, no dirty generation), checked-overflow EFBIG, the last
//!   representable byte, block-boundary touched-block derivation.
//! - KD-8 preview ledger: candidates are COUNTED while every class still
//!   takes today's exclusive guard (PR 1 behavior is byte-identical to
//!   MetaPrepOnly for Shared candidates).
//!
//! RED (PR 1 seam commit): the Shared arm is not yet admitted by the
//! classifier — the matrix rows expecting `Shared` and the candidate-
//! ledger integration row fail; the R2-M2 rows pin current behavior.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    cached_range_fully_mapped, inode_write_lock_scope, write_touched_blocks, InodeWriteLockScope,
    SqueezefsFilesystem, METRICS,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{CachedMetadata, DataRouter};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

// ---------------------------------------------------------------------------
// §4.1 classifier matrix (pure)
// ---------------------------------------------------------------------------

const BS: u64 = 65536;

/// The convoy campaign's v1 class (W-2's `SQUEEZEFS_WRITE_GUARD_NARROW=0`
/// leg): the matrix below pins it verbatim as the A/B control.
fn classify(
    file_type: &str,
    expected_new_size: u64,
    floor: Option<u64>,
    end: u64,
    mapped: Option<bool>,
) -> InodeWriteLockScope {
    inode_write_lock_scope(file_type, expected_new_size, BS, floor, end, mapped, false)
}

/// The W-2 narrowed class (`SQUEEZEFS_WRITE_GUARD_NARROW=1`, the default).
fn classify_narrow(
    file_type: &str,
    expected_new_size: u64,
    floor: Option<u64>,
    end: u64,
    mapped: Option<bool>,
) -> InodeWriteLockScope {
    inode_write_lock_scope(file_type, expected_new_size, BS, floor, end, mapped, true)
}

/// W-2: with the narrowed posture EVERY cache-resident striped write is
/// Shared — the extending stream, the hole-fill and the mapped overwrite
/// alike — while every non-resident / non-striped arm keeps its class.
#[test]
fn narrowed_class_admits_every_resident_striped_shape() {
    // The fresh/append stream: extends past the floor.
    assert_eq!(
        classify_narrow("striped", 5 * BS, Some(4 * BS), 5 * BS, Some(false)),
        InodeWriteLockScope::Shared,
        "an extending striped write is Shared under the narrowed class"
    );
    // A within-EOF hole-fill.
    assert_eq!(
        classify_narrow("striped", 4 * BS, Some(4 * BS), 2 * BS, Some(false)),
        InodeWriteLockScope::Shared,
        "a hole-fill is Shared under the narrowed class (its map insert runs \
         under (3)+(3.5) after the drop, on both modes)"
    );
    // The v1 class stays in.
    assert_eq!(
        classify_narrow("striped", 2 * BS, Some(4 * BS), 2 * BS, Some(true)),
        InodeWriteLockScope::Shared
    );
    // A non-resident map (indirect, not rehydrated) is still Shared: the
    // classifier decides the guard MODE, the data path never consults it.
    assert_eq!(
        classify_narrow("striped", 2 * BS, Some(4 * BS), 2 * BS, None),
        InodeWriteLockScope::Shared
    );
    // The rails: snapshot miss ⇒ exclusive-class (KD-2/KD-7), and the
    // non-striped arms are untouched.
    assert_eq!(
        classify_narrow("striped", 2 * BS, None, 2 * BS, Some(true)),
        InodeWriteLockScope::MetaPrepOnly,
        "a size-floor miss routes exclusive-class regardless of posture"
    );
    assert_eq!(
        classify_narrow("staged", BS, None, BS, None),
        InodeWriteLockScope::MetaPrepOnly
    );
    assert_eq!(
        classify_narrow("staged", BS + 1, None, BS + 1, None),
        InodeWriteLockScope::EntireOp
    );
    assert_eq!(
        classify_narrow("inline", 4096, None, 4096, None),
        InodeWriteLockScope::EntireOp
    );
}

#[test]
fn striped_fully_mapped_within_floor_is_shared() {
    // The diagnosis-row shape: within-EOF overwrite, range mapped.
    assert_eq!(
        classify("striped", 2 * BS, Some(4 * BS), 2 * BS, Some(true)),
        InodeWriteLockScope::Shared,
        "fully-mapped within-EOF striped overwrite is THE Shared class (§4.1)"
    );
    // Boundary: end exactly at the floor is within EOF.
    assert_eq!(
        classify("striped", 4 * BS, Some(4 * BS), 4 * BS, Some(true)),
        InodeWriteLockScope::Shared,
        "end == floor is within EOF (closed bound)"
    );
}

#[test]
fn every_doubt_routes_to_a_write_guard_class() {
    // Floor miss (attr or meta cache cold) — never fetch, never Shared.
    assert_eq!(
        classify("striped", 2 * BS, None, 2 * BS, Some(true)),
        InodeWriteLockScope::MetaPrepOnly,
        "size-floor cache miss routes exclusive-class (KD-2)"
    );
    // Map miss/anomaly (block_map_id without a resident map).
    assert_eq!(
        classify("striped", 2 * BS, Some(4 * BS), 2 * BS, None),
        InodeWriteLockScope::MetaPrepOnly,
        "map-probe miss/anomaly routes exclusive-class (KD-3)"
    );
    // A genuine hole: within EOF but unmapped ⇒ hole-fill = map insert.
    assert_eq!(
        classify("striped", 2 * BS, Some(4 * BS), 2 * BS, Some(false)),
        InodeWriteLockScope::MetaPrepOnly,
        "hole-fill is an inode-plane mutation — out of the v1 class (KD-3)"
    );
    // Beyond the floor: extending (or floor-stale-low) shapes.
    assert_eq!(
        classify("striped", 5 * BS, Some(4 * BS), 5 * BS, Some(true)),
        InodeWriteLockScope::MetaPrepOnly,
        "end > floor extends: size publish ⇒ MetaPrepOnly"
    );
}

#[test]
fn folded_staged_bypass_and_entireop_arms_are_byte_identical() {
    // The former `:18350` bypass: staged within one block.
    assert_eq!(
        classify("staged", BS, None, BS, None),
        InodeWriteLockScope::MetaPrepOnly,
        "staged && expected_new_size <= block_size keeps MetaPrepOnly"
    );
    // Staged growing past a block: layout transition ⇒ EntireOp.
    assert_eq!(
        classify("staged", BS + 1, None, BS + 1, None),
        InodeWriteLockScope::EntireOp
    );
    // Inline and unknown layouts: whole-op exclusive.
    assert_eq!(
        classify("inline", 4096, None, 4096, None),
        InodeWriteLockScope::EntireOp
    );
    assert_eq!(
        classify("unknown", 123, None, 123, None),
        InodeWriteLockScope::EntireOp
    );
}

// ---------------------------------------------------------------------------
// R2-M2: touched-block derivation + the map probe (pure)
// ---------------------------------------------------------------------------

#[test]
fn touched_blocks_derive_from_the_exclusive_end() {
    // End exactly on a block boundary touches ONLY the block before it.
    assert_eq!(write_touched_blocks(0, BS, BS), (0, 0));
    assert_eq!(write_touched_blocks(0, BS + 1, BS), (0, 1));
    assert_eq!(write_touched_blocks(BS - 1, BS, BS), (0, 0));
    assert_eq!(write_touched_blocks(3 * BS, 5 * BS, BS), (3, 4));
    // The last representable byte of a max-file-size shape.
    let max_end = BS * (u32::MAX as u64 + 1);
    assert_eq!(
        write_touched_blocks(max_end - 1, max_end, BS),
        (u32::MAX as u64, u32::MAX as u64)
    );
}

fn meta_with_map(size: u64, blocks: &[u32]) -> CachedMetadata {
    CachedMetadata {
        file_type: "striped".into(),
        size,
        block_map: Some(Arc::new(
            blocks
                .iter()
                .map(|b| (*b, format!("k{b}")))
                .collect::<HashMap<u32, String>>(),
        )),
        ..Default::default()
    }
}

#[test]
fn map_probe_is_snapshot_exact_and_anomaly_safe() {
    let m = meta_with_map(4 * BS, &[0, 1, 2, 3]);
    assert_eq!(cached_range_fully_mapped(&m, 0, 4 * BS, BS), Some(true));
    assert_eq!(
        cached_range_fully_mapped(&m, BS, 2 * BS, BS),
        Some(true),
        "interior single block"
    );
    // A hole inside the probed range.
    let holey = meta_with_map(4 * BS, &[0, 2, 3]);
    assert_eq!(
        cached_range_fully_mapped(&holey, 0, 4 * BS, BS),
        Some(false)
    );
    assert_eq!(
        cached_range_fully_mapped(&holey, 2 * BS, 4 * BS, BS),
        Some(true),
        "the probe is O(touched), not O(map): untouched holes don't veto"
    );
    // The anomaly arm: an indirect map id whose blob is NOT resident.
    let anomaly = CachedMetadata {
        file_type: "striped".into(),
        size: 4 * BS,
        block_map_id: Some(Arc::from("blob")),
        block_map: None,
        ..Default::default()
    };
    assert_eq!(
        cached_range_fully_mapped(&anomaly, 0, BS, BS),
        None,
        "map-id-without-map is the anomaly arm: no verdict, never Shared"
    );
}

// ---------------------------------------------------------------------------
// Handler-level contracts (live fs fixture)
// ---------------------------------------------------------------------------

async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3")
}

struct H {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make(tag: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    b.as_file().set_len(256 * 1024 * 1024).unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 128 * 1024 * 1024).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = fuse3::raw::Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
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
    use fuse3::raw::prelude::Filesystem;
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) -> u32 {
    use fuse3::raw::prelude::Filesystem;
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
    .unwrap_or_else(|e| panic!("write off {off} failed: {e:?}"))
    .written
}

/// R2-M2: zero-length success with ZERO side effects — no lease minted,
/// no dirty generation, before killpriv/time publication.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_length_write_has_no_side_effects() {
    use fuse3::raw::prelude::Filesystem;
    let h = make("wls_zero").await;
    let ino = create(&h, "z").await;
    let token_before = h.fs.dlm().get_fencing_token_ino(ino);
    let w =
        h.fs.write(h.req, ino, 0, 7, bytes::Bytes::new(), 0, 0)
            .await
            .expect("zero-length write succeeds");
    assert_eq!(w.written, 0);
    assert_eq!(
        h.fs.dlm().get_fencing_token_ino(ino),
        token_before,
        "a zero-length write must not mint a lease (R2-M2)"
    );
}

/// R2-M2: checked-overflow and beyond-max are EFBIG; the last
/// representable byte is writable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn range_overflow_and_max_bounds_are_exact() {
    use fuse3::raw::prelude::Filesystem;
    let h = make("wls_range").await;
    let ino = create(&h, "r").await;
    // u64 overflow: offset near MAX.
    let e =
        h.fs.write(
            h.req,
            ino,
            0,
            u64::MAX - 2,
            bytes::Bytes::from_static(&[0u8; 8]),
            0,
            0,
        )
        .await
        .expect_err("overflowing range must refuse");
    assert_eq!(e, libc::EFBIG.into(), "checked_add overflow ⇒ EFBIG");
    // One past the representable maximum.
    let max = h.fs.max_file_size();
    let e =
        h.fs.write(h.req, ino, 0, max, bytes::Bytes::from_static(&[1u8]), 0, 0)
            .await
            .expect_err("end > max_file_size must refuse");
    assert_eq!(e, libc::EFBIG.into());
    // The last representable byte is legal.
    let w = write_at(&h, ino, max - 1, &[0xAB]).await;
    assert_eq!(w, 1, "the last representable byte writes");
}

/// KD-8 preview ledger: a fully-mapped within-EOF striped overwrite
/// counts `write_lock_candidate_shared` — while PR 1 still executes it
/// on the exclusive (MetaPrepOnly-behavior) path, byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mapped_overwrite_counts_a_shared_candidate() {
    use fuse3::raw::prelude::Filesystem;
    let h = make("wls_candidate").await;
    let ino = create(&h, "c").await;
    // Grow striped: 4 blocks, then publish the maps durably.
    let base = vec![0x11u8; (4 * BS) as usize];
    write_at(&h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline drains"
    );

    // Warm both floor caches the way a real writer is warmed (getattr).
    let _ = h.fs.getattr(h.req, ino, None, 0).await.expect("getattr");

    let shared0 = METRICS.write_lock_candidate_shared.load(Ordering::Relaxed);
    let data = vec![0x5Au8; 8192];
    let w = write_at(&h, ino, BS, &data).await; // block 1, within EOF, mapped
    assert_eq!(w as usize, data.len());
    assert_eq!(
        METRICS.write_lock_candidate_shared.load(Ordering::Relaxed) - shared0,
        1,
        "the diagnosis-row shape must classify as a Shared CANDIDATE \
         (design §4.1; counted even though PR 1 executes exclusive)"
    );
    // Byte-identity of the PR 1 behavior mapping.
    let back =
        h.fs.read(h.req, ino, 0, BS, 8192, 0)
            .await
            .unwrap()
            .data
            .to_vec();
    assert_eq!(back, data);

    // The extending sibling: a Shared candidate under W-2's narrowed class
    // (the default), MetaPrepOnly under the `=0` A/B leg.
    let sh0 = METRICS.write_lock_candidate_shared.load(Ordering::Relaxed);
    write_at(&h, ino, 4 * BS, &data).await; // extends
    assert!(
        METRICS.write_lock_candidate_shared.load(Ordering::Relaxed) > sh0,
        "an extending striped write is a Shared candidate under the narrowed class"
    );
    squeezefs::fuse_client::set_write_guard_narrow_for_tests(false);
    let mp0 = METRICS
        .write_lock_candidate_metaprep
        .load(Ordering::Relaxed);
    write_at(&h, ino, 5 * BS, &data).await; // extends
    squeezefs::fuse_client::set_write_guard_narrow_for_tests(true);
    assert!(
        METRICS
            .write_lock_candidate_metaprep
            .load(Ordering::Relaxed)
            > mp0,
        "under SQUEEZEFS_WRITE_GUARD_NARROW=0 an extending striped write is a \
         MetaPrepOnly candidate (the pre-campaign class)"
    );
}
