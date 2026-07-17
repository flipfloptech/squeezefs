//! RW4 — W2 extent-record crash recovery, remount replay, and the §5.2
//! honest downgrade mechanism (docs/design-random-small-writes.md §5.2
//! review Issues 5 → 17; risk R8).
//!
//! Contracts pinned here, per direction:
//!
//! - **Remount replay**: kill-9-class residue (`active_block_ext:` records
//!   in staging at mount — a CLEAN shutdown drains every record to fold,
//!   so any record here is crash residue) is generation-bound and
//!   fencing-stamped; the recovery sweep KEEPS current-generation records
//!   (composable + foldable custody) and logs the orphan population
//!   LOUDLY — the forward-detection arm of the below-RW4 downgrade
//!   residual.
//! - **The remount law**: a record whose fencing stamp is STALE (the ino's
//!   generator moved past it — another writer era) is discarded, loudly.
//! - **Torn records detected-and-ignored loudly**: a blob under an ext key
//!   that fails structural validation (magic/table/checksum) is discarded
//!   with a counter + log, never parsed, never wedging folds.
//! - **Future-downgrade fence, record level**: a record naming a NEWER
//!   record version is refused as a unit, loudly, and LEFT IN PLACE.
//! - **Future-downgrade fence, dir level**: a staging dir whose format
//!   marker names a NEWER content version than this binary refuses the
//!   segment AS A UNIT (mount construction fails loudly, nothing wiped);
//!   pre-RW4 dirs (no marker) adopt + stamp; the stamp is the current
//!   version.
//! - **Clean unmount leaves ZERO ext records** (asserted on the staged key
//!   population) — clean downgrades carry zero exposure by construction.
//! - **Kill-9 mid-fold**: the old durable block stays intact; the extents
//!   are recovered (or discarded per fencing) — never a torn/partial
//!   block, never conjured zeros.
//!
//! RED against the RW4 scaffolding: parking/spill/recovery are inert, so
//! every contract fails on its counter/behavior assertions.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::nvme::{
    ExtentRecord, EXTENT_RECORD_VERSION, STAGING_FORMAT_MARKER, STAGING_FORMAT_VERSION,
};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn reset_knobs() {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    squeezefs::fuse_client::set_fold_max_extents(64);
    squeezefs::fuse_client::set_fold_max_bytes(1024 * 1024);
    squeezefs::fuse_client::set_parked_cap_buffers(256);
}

struct H {
    fs: SqueezefsFilesystem,
    dlm: DlmClient,
    req: Request,
}

/// A session over PERSISTENT meta + backing + staging (the two-session
/// crash pattern): dropping the returned handle is the kill-9 equivalent
/// for all RAM state.
async fn session(
    alloc_ns: &str,
    meta_path: &std::path::Path,
    backing_path: &std::path::Path,
    staging: &std::path::Path,
) -> H {
    reset_knobs();
    let dlm = DlmClient::new("local").unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing_path.to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let cache = TieredCache::new(
        vec![staging.to_path_buf()],
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
    router.set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
        "lz4".to_string(),
        "none".to_string(),
        None,
    ));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let be = KvMetaBackend::open(meta_path).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H { fs, dlm, req }
}

struct Vol {
    meta: NamedTempFile,
    backing: NamedTempFile,
    staging: TempDir,
}

async fn vol(uuid: [u8; 16]) -> Vol {
    let meta = NamedTempFile::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let staging = tempdir().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid,
    })
    .unwrap()
    .build(meta.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    Vol {
        meta,
        backing,
        staging,
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

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        panic!(
            "{what}: first mismatch at {i}: got {:#04x} want {:#04x}",
            got[i], want[i]
        );
    }
}

async fn durable_striped(h: &H, name: &str, blocks: u64, tag: u8) -> (u64, Vec<u8>) {
    let len = (blocks * BS) as usize;
    let ino = create(h, name).await;
    let base = pattern(len, tag);
    write_at(h, ino, 0, &base).await;
    fsync(h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    (ino, base)
}

/// Park extents on `blocks` and force them to SPILL as staged records (the
/// crash-survivable custody form); returns the expected file image.
async fn park_and_spill(h: &H, ino: u64, base: &[u8], tag: u8) -> Vec<u8> {
    squeezefs::fuse_client::set_parked_cap_buffers(1);
    let p = pattern(4096, tag);
    let mut want = base.to_vec();
    for blk in 0..3u64 {
        for slot in 0..3u64 {
            let off = blk * BS + slot * 20480;
            write_at(h, ino, off, &p).await;
            want[off as usize..off as usize + 4096].copy_from_slice(&p);
        }
    }
    let prefix = squeezefs::keys::active_block_ext_ino_prefix(ino);
    assert!(
        !h.fs
            .router
            .cache
            .nvme
            .extent_record_keys(prefix.as_str())
            .is_empty(),
        "premise: extent records spilled to staging (crash-survivable)"
    );
    squeezefs::fuse_client::set_parked_cap_buffers(256);
    want
}

/// 1. Remount replay: kill-9 residue records are recovered (loud orphan
/// detection — the §5.2(iii) forward-detection line), composable by reads,
/// and folded durably by fsync; a clean unmount then leaves ZERO records.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remount_replays_records_and_detects_orphans_loudly() {
    let _g = serial().await;
    let v = vol(*b"rw4-recover-a-01").await;

    let (ino, want) = {
        let h = session(
            "rw4r_ns_a1",
            v.meta.path(),
            v.backing.path(),
            v.staging.path(),
        )
        .await;
        let (ino, base) = durable_striped(&h, "recov.dat", 4, 0x00).await;
        let want = park_and_spill(&h, ino, &base, 0x5A).await;
        (ino, want)
        // Dropped WITHOUT fsync/teardown: kill-9 equivalent.
    };

    let h2 = session(
        "rw4r_ns_a1",
        v.meta.path(),
        v.backing.path(),
        v.staging.path(),
    )
    .await;
    let before = METRICS.extent_records_recovered.load(Ordering::Relaxed);
    let recovered = h2.fs.recover_extent_records().await;
    let after = METRICS.extent_records_recovered.load(Ordering::Relaxed);
    assert!(
        recovered >= 1,
        "the mount sweep must RECOVER the crash-left records (got {recovered})"
    );
    assert_eq!(
        after - before,
        recovered as u64,
        "orphan detection must be counter-visible (the loud forward-\
         detection arm)"
    );

    // Recovered records are composable custody.
    let got = read_at(&h2, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-crash read composes recovered records");

    // ... and foldable: fsync drains them durably.
    fsync(&h2, ino).await;
    let prefix = squeezefs::keys::active_block_ext_ino_prefix(ino);
    assert!(
        h2.fs
            .router
            .cache
            .nvme
            .extent_record_keys(prefix.as_str())
            .is_empty(),
        "fsync folds recovered records"
    );
    let got = read_at(&h2, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-fold read");

    // Third session: a CLEAN previous shutdown leaves zero records ⇒ the
    // sweep finds none.
    drop(h2);
    let h3 = session(
        "rw4r_ns_a1",
        v.meta.path(),
        v.backing.path(),
        v.staging.path(),
    )
    .await;
    assert_eq!(
        h3.fs.recover_extent_records().await,
        0,
        "a cleanly-drained volume has no orphan records to recover"
    );
    let got = read_at(&h3, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "durable bytes after the full cycle");
}

/// 2. The remount law: records stamped by a SUPERSEDED fencing generation
/// are discarded loudly ("stale fencing tokens discard staged work"); the
/// durable base bytes stay authoritative.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remount_discards_stale_fencing_records() {
    let _g = serial().await;
    let v = vol(*b"rw4-recover-b-01").await;

    let (ino, base) = {
        let h = session(
            "rw4r_ns_b1",
            v.meta.path(),
            v.backing.path(),
            v.staging.path(),
        )
        .await;
        let (ino, base) = durable_striped(&h, "stale.dat", 4, 0x01).await;
        let _ = park_and_spill(&h, ino, &base, 0x66).await;
        (ino, base)
    };

    let h2 = session(
        "rw4r_ns_b1",
        v.meta.path(),
        v.backing.path(),
        v.staging.path(),
    )
    .await;
    // Another writer era: bump the ino's fencing generator past every
    // record stamp BEFORE the sweep (ranged lock shares the file's
    // generator without contending).
    let path = squeezefs::keys::inode_path(ino);
    for _ in 0..4 {
        let lease = h2
            .dlm
            .acquire_lock(&path, Some((0, 1)), std::time::Duration::from_secs(5))
            .await
            .unwrap();
        lease.release().await.unwrap();
    }
    let before = METRICS
        .extent_records_stale_discarded
        .load(Ordering::Relaxed);
    let recovered = h2.fs.recover_extent_records().await;
    let after = METRICS
        .extent_records_stale_discarded
        .load(Ordering::Relaxed);
    assert!(
        after > before,
        "stale-stamped records must be DISCARDED (the remount law), loudly"
    );
    assert_eq!(
        recovered, 0,
        "nothing recovers from a superseded writer era"
    );
    let prefix = squeezefs::keys::active_block_ext_ino_prefix(ino);
    assert!(
        h2.fs
            .router
            .cache
            .nvme
            .extent_record_keys(prefix.as_str())
            .is_empty(),
        "discarded records leave the staging population"
    );
    // The durable base is untouched (discard ≠ corruption).
    let got = read_at(&h2, ino, 0, base.len()).await;
    assert_bytes(&got, &base, "durable base after stale-record discard");
}

/// 3. Torn records: a blob under an ext key that fails structural
/// validation (garbage magic / torn payload) is detected-and-ignored
/// LOUDLY at the sweep — discarded, counted, never parsed into extents,
/// never wedging reads or folds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torn_record_detected_and_ignored_loudly() {
    let _g = serial().await;
    let v = vol(*b"rw4-recover-c-01").await;
    let h = session(
        "rw4r_ns_c1",
        v.meta.path(),
        v.backing.path(),
        v.staging.path(),
    )
    .await;
    let (ino, base) = durable_striped(&h, "torn.dat", 3, 0x02).await;

    // Plant a torn blob directly under the block-1 ext key: a valid record
    // image with flipped payload bytes (checksum mismatch) — the
    // crash-mid-ring-write shape the ring's header scan cannot see.
    let key = squeezefs::keys::active_block_ext(ino, 1).to_string();
    let good = ExtentRecord {
        version: EXTENT_RECORD_VERSION,
        fencing_token: h.dlm.get_fencing_token_ino(ino),
        block_idx: 1,
        base_deferred: true,
        extents: vec![(4096, pattern(2048, 0x77))],
    };
    let mut torn = good.serialize();
    let n = torn.len();
    torn[n - 1] ^= 0xFF;
    torn[n / 2] ^= 0xFF;
    assert!(
        h.fs.router
            .cache
            .nvme
            .put_active_block(&key, &torn, good.fencing_token),
        "planting the torn blob"
    );

    let before = METRICS
        .extent_records_torn_discarded
        .load(Ordering::Relaxed);
    let _ = h.fs.recover_extent_records().await;
    let after = METRICS
        .extent_records_torn_discarded
        .load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        1,
        "a torn record must be detected-and-ignored LOUDLY (counted), \
         never parsed"
    );
    assert!(
        h.fs.router.cache.nvme.read_extent_record(&key).is_none(),
        "the torn blob is discarded from staging"
    );
    // Reads/folds proceed over the durable base — no wedge, no zeros.
    let got = read_at(&h, ino, 0, base.len()).await;
    assert_bytes(&got, &base, "reads after a torn-record discard");
    fsync(&h, ino).await;
}

/// 4. Future-downgrade fence, record level: a record naming a NEWER record
/// version is refused as a unit, loudly, and LEFT IN PLACE (acked custody
/// of a newer binary — never wiped, never guessed at).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn future_version_record_refused_loudly_and_left() {
    let _g = serial().await;
    let v = vol(*b"rw4-recover-d-01").await;
    let h = session(
        "rw4r_ns_d1",
        v.meta.path(),
        v.backing.path(),
        v.staging.path(),
    )
    .await;
    let (ino, base) = durable_striped(&h, "future.dat", 3, 0x03).await;

    let key = squeezefs::keys::active_block_ext(ino, 2).to_string();
    let future = ExtentRecord {
        version: EXTENT_RECORD_VERSION + 1,
        fencing_token: h.dlm.get_fencing_token_ino(ino),
        block_idx: 2,
        base_deferred: true,
        extents: vec![(0, pattern(1024, 0x88))],
    };
    assert!(
        h.fs.router
            .cache
            .nvme
            .put_active_block(&key, &future.serialize(), future.fencing_token),
        "planting the future-version record"
    );

    let before = METRICS
        .extent_records_future_refused
        .load(Ordering::Relaxed);
    let _ = h.fs.recover_extent_records().await;
    let after = METRICS
        .extent_records_future_refused
        .load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        1,
        "a future-version record must be refused loudly (the §5.2 \
         forward-only fence, record level)"
    );
    assert!(
        h.fs.router.cache.nvme.has_staged_extent_record(&key),
        "the refused record is LEFT IN PLACE (custody of a newer binary)"
    );
    // The base stays served; the refused record is never composed.
    let got = read_at(&h, ino, 0, base.len()).await;
    assert_bytes(&got, &base, "reads never compose a refused record");
}

/// 5. Future-downgrade fence, dir level: a staging dir whose format marker
/// names a NEWER content version refuses construction AS A UNIT (loud
/// mount failure, nothing wiped); pre-RW4 dirs (no marker, with data)
/// adopt-and-stamp the current version; current-version dirs pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staging_format_version_gate_per_direction() {
    let _g = serial().await;
    reset_knobs();
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "rw4r_ns_e1")
            .await
            .unwrap(),
    );

    let make_cache = |dir: std::path::PathBuf| {
        let dlm = dlm.clone();
        let ba = ba.clone();
        let nvme = nvme.clone();
        async move {
            TieredCache::new(
                vec![dir],
                Some("64MB"),
                Some("64MB"),
                Some("16MB"),
                Some("64MB"),
                dlm.meta_client().clone(),
                ba,
                nvme,
                Some("rw4-format-gate-gen"),
            )
            .await
        }
    };

    // Direction 1 — FUTURE content: marker names version+1 over real
    // segment data ⇒ the unit refusal (loud Err, data untouched).
    let dir = tempdir().unwrap();
    let ss = dir.path().join("staging_segment");
    std::fs::create_dir_all(&ss).unwrap();
    std::fs::write(ss.join("segment_0.bin"), b"newer-binary dirty staging").unwrap();
    std::fs::write(
        dir.path().join(STAGING_FORMAT_MARKER),
        format!(
            "squeezefs-staging-format-v1\n{}\n",
            STAGING_FORMAT_VERSION + 1
        ),
    )
    .unwrap();
    // The generation marker matches, so only the format version gates.
    std::fs::write(
        dir.path().join(".squeezefs_generation"),
        "squeezefs-staging-generation-v1\nrw4-format-gate-gen\n",
    )
    .unwrap();
    let res = make_cache(dir.path().to_path_buf()).await;
    assert!(
        res.is_err(),
        "a FUTURE staging format version must refuse the segment as a \
         unit (loud mount failure) — got Ok"
    );
    let msg = format!("{:?}", res.err().unwrap());
    assert!(
        msg.contains("staging format"),
        "the refusal must NAME the staging-format fence (got: {msg})"
    );
    assert!(
        ss.join("segment_0.bin").exists()
            && std::fs::read(ss.join("segment_0.bin")).unwrap()
                == b"newer-binary dirty staging".to_vec(),
        "refusal must not wipe the newer binary's custody"
    );

    // Direction 2 — pre-RW4 dir (no format marker, with data): adopted,
    // stamped at the CURRENT version.
    let dir2 = tempdir().unwrap();
    let ss2 = dir2.path().join("staging_segment");
    std::fs::create_dir_all(&ss2).unwrap();
    std::fs::write(ss2.join("segment_0.bin"), vec![0u8; 8192]).unwrap();
    std::fs::write(
        dir2.path().join(".squeezefs_generation"),
        "squeezefs-staging-generation-v1\nrw4-format-gate-gen\n",
    )
    .unwrap();
    make_cache(dir2.path().to_path_buf()).await.expect(
        "pre-RW4 staging content must adopt (below-RW4 direction \
                 is forward-detected, not refused)",
    );
    let stamped = std::fs::read_to_string(dir2.path().join(STAGING_FORMAT_MARKER))
        .expect("the mount must STAMP the format marker");
    assert!(
        stamped.contains(&format!("\n{STAGING_FORMAT_VERSION}\n")),
        "the stamp names the current version (got: {stamped:?})"
    );

    // Direction 3 — current version: passes.
    make_cache(dir2.path().to_path_buf())
        .await
        .expect("current-version staging must mount");
}

/// 6. Clean unmount drains every extent record (the §5.2 mandate): after
/// the teardown flush pair, the staged key population holds ZERO
/// `active_block_ext:` keys — clean downgrades carry zero exposure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clean_unmount_drains_all_extent_records() {
    let _g = serial().await;
    let v = vol(*b"rw4-recover-f-01").await;
    let h = session(
        "rw4r_ns_f1",
        v.meta.path(),
        v.backing.path(),
        v.staging.path(),
    )
    .await;
    let (ino, base) = durable_striped(&h, "clean.dat", 4, 0x04).await;
    let want = park_and_spill(&h, ino, &base, 0x99).await;
    // Plus RAM-parked extents that never spilled (block 3).
    let p = pattern(4096, 0x9A);
    let mut want = want;
    write_at(&h, ino, 3 * BS + 8192, &p).await;
    want[(3 * BS + 8192) as usize..(3 * BS + 8192) as usize + 4096].copy_from_slice(&p);

    // The teardown pair (destroy's force-flush path).
    h.fs.flush_all_memory_buffers_to_staging().await.unwrap();
    let _ = h.fs.flush_all_staged_blocks_to_backend().await;

    assert!(
        h.fs.router.cache.nvme.extent_record_keys("").is_empty(),
        "a clean unmount must drain EVERY extent record to fold (zero \
         clean-downgrade exposure)"
    );
    drop(h);

    // Remount: nothing to recover; bytes durable.
    let h2 = session(
        "rw4r_ns_f1",
        v.meta.path(),
        v.backing.path(),
        v.staging.path(),
    )
    .await;
    assert_eq!(
        h2.fs.recover_extent_records().await,
        0,
        "clean shutdown leaves no orphan records"
    );
    let got = read_at(&h2, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-clean-unmount durable bytes");
}

/// 7. Kill-9 around the fold boundary: whatever the crash cut — before the
/// fold (records live) or after it (block folded, records gone) — the old
/// durable block is never torn and the composed content is never zeros.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill9_around_fold_leaves_old_block_intact() {
    let _g = serial().await;
    let v = vol(*b"rw4-recover-g-01").await;

    // Session A: durable base + spilled records, then fold HALF the blocks
    // (fsync folds all — so fold block 0 only via the public fold entry),
    // then crash.
    let (ino, base, want) = {
        let h = session(
            "rw4r_ns_g1",
            v.meta.path(),
            v.backing.path(),
            v.staging.path(),
        )
        .await;
        let (ino, base) = durable_striped(&h, "killfold.dat", 4, 0x05).await;
        let want = park_and_spill(&h, ino, &base, 0xAB).await;
        // Fold exactly block 0 (the mid-fold cut: some blocks folded, some
        // still record-backed at crash time).
        let folded = h.fs.fold_extent_block(ino, 0).await.unwrap();
        assert!(folded, "premise: block 0 folded before the crash");
        (ino, base, want)
    };

    // Session B: every block reads exactly — folded block from the device,
    // unfolded blocks composed from recovered records; the uncovered
    // complement everywhere is the OLD bytes (never zeros, never torn).
    let h2 = session(
        "rw4r_ns_g1",
        v.meta.path(),
        v.backing.path(),
        v.staging.path(),
    )
    .await;
    let recovered = h2.fs.recover_extent_records().await;
    assert!(
        recovered >= 1,
        "the unfolded blocks' records survive the crash"
    );
    let got = read_at(&h2, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-kill-9 composed read (old block intact)");
    let _ = base;
    fsync(&h2, ino).await;
    let got = read_at(&h2, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-recovery fold");
}
