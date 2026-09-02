//! **The kvmap read/records economy — PR 3 (`feat/kvmap-read`)** of the
//! PB-class file ladder (`docs/design-kvmap-block-map-tree.md`, Rev 1.3 —
//! §§3, 6 A5/A7/A9, all six §8 addenda, and the §9 landed-deviation map:
//! PR 2 rehydrates the FULL tree-resolved map into `CachedMetadata`, so
//! the read consumers already work on kvmap inos and this PR is the
//! records ECONOMY + hardening rung).
//!
//! Contracts pinned here:
//!
//! 1. **POINT emission** (Rev 1.3 #3). The publish path emits the 18-byte
//!    binary POINT form for every undecorated backend-key string —
//!    round-tripping through publish → tree → fetch to the IDENTICAL key
//!    string — while decorated/damaged shapes ride STRING verbatim, and
//!    `publish_map_record_bytes` shows the shrink on a streaming fixture.
//! 2. **EFBIG at the u32 ceiling** (A5). On a kvmap-class file the last
//!    representable block index (`u32::MAX − 1`) admits and round-trips;
//!    anything that would mint index `u32::MAX` refuses EFBIG at the
//!    FUSE admission — before any allocation, never as a codec error
//!    mid-publish.
//! 3. **The giant-sparse matrix** (§4 PR 3). Indices {0, 2^20, 2^30,
//!    u32::MAX − 1} publish, survive a remount, and read back exactly.
//! 4. **A7 probation** (§8 #3). Demand-loaded tree-7 LEAF nodes enter the
//!    node-cache clock with no second chance: a once-streamed giant map
//!    cannot evict a twice-touched foreign-tree working set.
//! 5. **A9 reader bracket** (§8 #4). On ARMED reader mounts the kvmap
//!    fetch rehydration brackets head + tree-range reads with the
//!    revalidation seqlock (retry on epoch change); write mounts perform
//!    zero epoch reads (pinned no-bracket byte-identity).
//! 6. **Stats split** (§8 #5). The merged PR-1 lookups counter splits
//!    into exact-hits vs range-reads, and demand-loaded tree-7 leaves are
//!    counted as leaf reads at the node-cache miss site.
//! 7. **Resolve economy** (§9 #2's read half). Fetch-rehydration of a
//!    4096-entry kvmap ino performs a leaf-amortized, bounded number of
//!    tree range calls.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_map::MapEntry;
use squeezefs::meta_backend::kv::block_refs::volume_tag;
use squeezefs::meta_backend::kv::builder::{format_v3, BuilderConfig, FormatV3Options, ImageBuilder};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_map_tree_bit, set_block_refcounts_bit, write_superblock_v3,
    VolumeFormat, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::kv::META_KV_BLOCK_MAP_PUTS;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const META_LEN: u64 = 256 * 1024 * 1024;
/// Sparse on purpose (the crossing fixture's shape): legs allocate many
/// offsets without writing most of them.
const DATA_LEN: u64 = 32 * 1024 * 1024 * 1024;
const DATA_VOL_ID: &str = "vol-00000000000000c2";
/// A second, NAMED data volume — its persisted keys carry the
/// `vol-{16 hex}://offset` form (the field's multi-volume shape, the one
/// the POINT economy is priced against).
const DATA_VOL_ID_2: &str = "vol-00000000000000d3";
/// Enough mapped blocks to push the encoded map past the 64 KiB-node
/// volume's ~16 KiB xattr cap — the crossing trigger.
const SPILL_BLOCKS: u32 = 1200;

// ---------------------------------------------------------------------------
// Serialization (process-global METRICS deltas + env knob mutation)
// ---------------------------------------------------------------------------

static SERIAL_HELD: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL_HELD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        std::thread::yield_now();
    }
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        SERIAL_HELD.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// The Rig (the kvmap_crossing_tests fixture: real v3 meta volume, real
// file-backed data volume, the router that binds them)
// ---------------------------------------------------------------------------

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format WITHOUT bit 16/bit 9 (immune to the `SQUEEZEFS_TEST_STAMP_*`
/// seams), then stamp both explicitly — the crossing-ready shape.
async fn format_meta_kvmap(path: &Path) {
    format_v3(path, META_LEN, &opts())
        .await
        .expect("format v3 meta volume");
    let VolumeFormat::V3(mut sb) = classify_volume(path).await.expect("classify") else {
        panic!("expected v3");
    };
    let strip = FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS | FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE;
    if sb.features_incompat & strip != 0 {
        sb.features_incompat &= !strip;
        write_superblock_v3(path, &sb).await.expect("strip seams");
    }
    assert!(set_block_refcounts_bit(path).await.expect("stamp bit 9"));
    assert!(set_block_map_tree_bit(path).await.expect("stamp bit 16"));
}

struct Rig {
    router: DataRouter,
    alloc: Arc<BlockAllocator>,
    routed: Arc<RoutedMetaBackend>,
    _staging: TempDir,
}

async fn mount(meta: &Path, data: &Path) -> Rig {
    let kv = KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv]));
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL_ID).await.unwrap());
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
    Rig {
        router,
        alloc,
        routed,
        _staging: staging,
    }
}

fn data_file() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(DATA_LEN)
        .unwrap();
    f
}

impl Rig {
    fn kv(&self) -> &Arc<KvMetaBackend> {
        &self.routed.volumes[0]
    }

    async fn mk_file(&self, name: &str) -> u64 {
        self.routed
            .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino
    }

    fn token(&self, ino: u64) -> u64 {
        self.router.dlm.get_fencing_token_ino(ino)
    }

    /// Publish `entries` in ONE merge at `size`.
    async fn publish_entries(&self, ino: u64, entries: &[(u32, String)], size: u64) {
        self.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(entries),
                size,
                LayoutFlip::ToStripedKeepStagedIdentity,
                self.token(ino),
            )
            .await
            .expect("merge publish");
    }

    /// Allocate `n` blocks and bind them at indices `0..n` with BARE
    /// default-slot keys — the crossing trigger.
    async fn publish_spill(&self, ino: u64, n: u32) -> Vec<(u32, String)> {
        let mut entries: Vec<(u32, String)> = Vec::new();
        for b in 0..n {
            let offset = self.alloc.allocate_block().await.expect("allocate");
            self.alloc.publish_block(offset);
            entries.push((b, offset.to_string()));
        }
        self.publish_entries(ino, &entries, u64::from(n) * 4 * 1024 * 1024)
            .await;
        entries
    }

    /// The durable layout head, decoded (bincode — kvmap/inline heads).
    async fn durable_head(&self, ino: u64) -> LayoutMetadata {
        let bytes = self
            .kv()
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("layout exists");
        bincode::deserialize(&bytes).expect("bincode head")
    }

    /// Every tree-7 record of `ino`, RAW (kind-visible).
    async fn raw_records(&self, ino: u64) -> Vec<(u32, MapEntry)> {
        let mut out = Vec::new();
        let mut cursor = 0u32;
        loop {
            let page = self
                .kv()
                .block_map_range(ino, cursor, 512)
                .await
                .expect("tree scan");
            let Some(last) = page.last().map(|(i, _)| *i) else {
                break;
            };
            out.extend(page);
            cursor = match last.checked_add(1) {
                Some(n) => n,
                None => break,
            };
        }
        out
    }

    /// The tree-resolved map as the READ path sees it (evict + refetch).
    async fn refetched_map(&self, ino: u64) -> std::collections::HashMap<u32, String> {
        self.router.metadata_cache.invalidate(&ino);
        let fetched = self
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .expect("refetch");
        assert_eq!(fetched.block_map_id.as_deref(), Some("kvmap:1"));
        fetched
            .block_map
            .as_deref()
            .expect("tree-resolved map")
            .clone()
    }

    async fn shutdown(self) {
        self.routed.volumes[0]
            .shutdown()
            .await
            .expect("clean shutdown");
    }
}

// ===========================================================================
// 1. POINT emission (Rev 1.3 #3; design §2)
// ===========================================================================

/// Undecorated backend-key strings — bare default-slot offsets AND the
/// named `vol-{16 hex}://offset` form — publish as 18-byte POINT records
/// (`vol_tag` = the durable KD-5 identity), the round trip through
/// publish → tree → fetch yields the IDENTICAL key strings, and the
/// `publish_map_record_bytes` gauge shows the shrink vs the PR-2 STRING
/// sizing on the streaming (named-volume) fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undecorated_keys_publish_as_point_records_and_round_trip() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;

    // A SECOND named data volume: its keys are the `vol-…://offset` form.
    let data2 = data_file();
    let rec = squeezefs::DataVolumeRecord {
        id: DATA_VOL_ID_2.to_string(),
        backing_dev: data2.path().display().to_string(),
        state: squeezefs::VOL_STATE_ACTIVE.to_string(),
        added_ts: 0,
    };
    rig.router
        .backend_router
        .register_backend(&rec)
        .await
        .expect("register the named volume");

    // --- The named-volume streaming fixture (the shrink measurement). --
    let ino = rig.mk_file("named_stream").await;
    let entries: Vec<(u32, String)> = (0..SPILL_BLOCKS)
        .map(|b| {
            (
                b,
                format!("{DATA_VOL_ID_2}://{}", u64::from(b) * 4 * 1024 * 1024),
            )
        })
        .collect();
    let bytes_before = METRICS.publish_map_record_bytes.load(Ordering::Relaxed);
    rig.publish_entries(ino, &entries, u64::from(SPILL_BLOCKS) * 4 * 1024 * 1024)
        .await;
    assert_eq!(
        rig.durable_head(ino).await.block_map_id.as_deref(),
        Some("kvmap:1")
    );
    let record_bytes = METRICS.publish_map_record_bytes.load(Ordering::Relaxed) - bytes_before;

    // Every record is the binary POINT form: the named volume's durable
    // tag + the device offset — 30 B per record (12 B key + 18 B value).
    let records = rig.raw_records(ino).await;
    assert_eq!(records.len(), entries.len());
    let tag2 = volume_tag(DATA_VOL_ID_2);
    for (idx, entry) in &records {
        assert_eq!(
            *entry,
            MapEntry::Point {
                vol_tag: tag2,
                offset: u64::from(*idx) * 4 * 1024 * 1024,
            },
            "index {idx} must be the compact POINT form"
        );
    }
    let point_bytes = entries.len() as u64 * 30;
    let string_bytes: u64 = entries
        .iter()
        .map(|(_, k)| (12 + 2 + k.len()) as u64)
        .sum();
    assert_eq!(
        record_bytes, point_bytes,
        "the gauge accounts POINT records exactly"
    );
    assert!(
        record_bytes < string_bytes,
        "the POINT economy must SHRINK the record bytes: {record_bytes} vs the \
         PR-2 STRING sizing {string_bytes}"
    );
    println!(
        "POINT shrink on the named-volume streaming fixture: {record_bytes} B vs \
         {string_bytes} B STRING ({:.1} %)",
        100.0 * (string_bytes - record_bytes) as f64 / string_bytes as f64
    );

    // --- Round trip: fetch resolves the IDENTICAL key strings. ---------
    let map = rig.refetched_map(ino).await;
    assert_eq!(map.len(), entries.len());
    for (b, k) in &entries {
        assert_eq!(
            map.get(b),
            Some(k),
            "index {b} must round-trip to the identical key string"
        );
    }

    // --- Bare default-slot keys take POINT too (the single-volume form).
    let bare = rig.mk_file("bare_stream").await;
    let bare_entries = rig.publish_spill(bare, SPILL_BLOCKS).await;
    let tag1 = volume_tag(DATA_VOL_ID);
    for (idx, entry) in rig.raw_records(bare).await {
        let MapEntry::Point { vol_tag, offset } = entry else {
            panic!("bare key at index {idx} must be POINT, got {entry:?}");
        };
        assert_eq!(vol_tag, tag1);
        assert_eq!(offset.to_string(), bare_entries[idx as usize].1);
    }
    let map = rig.refetched_map(bare).await;
    for (b, k) in &bare_entries {
        assert_eq!(map.get(b), Some(k), "bare key at index {b} round-trips");
    }
    rig.shutdown().await;
}

/// Decorated shapes — the size-carrying staged-promotion form and the
/// fsck `damaged:` quarantine marker — still ride STRING verbatim and
/// round-trip byte-identically (design §2: POINT is for the undecorated
/// `{vol_tag}:{offset}` class ONLY; guessing a decorated key's meaning
/// would resolve blocks to wrong device bytes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decorated_keys_still_ride_string_verbatim() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("decorated").await;

    // Cross with bare keys, then bind two DECORATED entries.
    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    let off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(off);
    let promoted = format!("{off}:0:65536"); // size-carrying staged promotion
    let damaged = "damaged:blk-gone".to_string(); // §5.6a quarantine marker
    let decorated = vec![
        (SPILL_BLOCKS, promoted.clone()),
        (SPILL_BLOCKS + 1, damaged.clone()),
    ];
    rig.publish_entries(
        ino,
        &decorated,
        u64::from(SPILL_BLOCKS + 2) * 4 * 1024 * 1024,
    )
    .await;
    entries.extend(decorated);

    let records = rig.raw_records(ino).await;
    let kind_of = |idx: u32| {
        records
            .iter()
            .find(|(i, _)| *i == idx)
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| panic!("index {idx} missing"))
    };
    assert_eq!(
        kind_of(SPILL_BLOCKS),
        MapEntry::String(promoted.clone().into_bytes()),
        "the size-carrying decoration rides STRING verbatim"
    );
    assert_eq!(
        kind_of(SPILL_BLOCKS + 1),
        MapEntry::String(damaged.clone().into_bytes()),
        "the damaged marker rides STRING verbatim"
    );

    // And the whole map — POINT and STRING mixed — round-trips exactly.
    let map = rig.refetched_map(ino).await;
    entries.sort_unstable_by_key(|&(b, _)| b);
    assert_eq!(map.len(), entries.len());
    for (b, k) in &entries {
        assert_eq!(map.get(b), Some(k), "index {b} round-trips");
    }
    rig.shutdown().await;
}

// ===========================================================================
// 2. EFBIG at the u32 ceiling (design §6 A5), on a kvmap-class file
// ===========================================================================

/// The FUSE-admission harness (the sparse_write_bounded_tests shape) over
/// a bit-16 volume: 64 KiB blocks so the ceiling — `block_size ×
/// (2^32 − 1)` — is reachable sparsely.
struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
}

const BS: u64 = 65536;

async fn make_fuse_kvmap() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(DATA_VOL_ID).await.unwrap());
    // Cache-less: beyond-inline writes route striped — every data write
    // exercises the kvmap publish path directly.
    let cache = TieredCache::new(
        Vec::new(),
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
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
        node_size: 64 * 1024,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_5EEC_0003,
        uuid: *b"kvmap-read-pr3!!",
    })
    .unwrap()
    .build(m.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    // The crossing-ready posture: bits 9 + 16 (idempotent if a test seam
    // already stamped them).
    let _ = set_block_refcounts_bit(m.path()).await.unwrap();
    let _ = set_block_map_tree_bit(m.path()).await.unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
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
    }
}

async fn fuse_create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn fuse_write(h: &H, ino: u64, off: u64, data: &[u8]) {
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn fuse_read(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// Sparse boundary honesty on a CROSSED (kvmap) file: block index
/// `u32::MAX − 1` admits, publishes a tree record, and round-trips;
/// every op that would mint index `u32::MAX` — write, truncate-up,
/// fallocate — refuses EFBIG at admission with ZERO map-plane side
/// effects (never a codec error mid-publish; the A5 reserved-index
/// refusal in the codec stays structurally unreachable from FUSE).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kvmap_boundary_admits_last_index_and_refuses_efbig_at_reserved() {
    let _serial = serial();
    let h = make_fuse_kvmap().await;
    let ino = fuse_create(&h, "boundary").await;

    // Cross the head: sequential striped writes until the map spills.
    let chunk = vec![b'c'; 8 * BS as usize];
    let mut blocks = 0u64;
    let crossed = loop {
        fuse_write(&h, ino, blocks * BS, &chunk).await;
        blocks += 8;
        let m =
            h.fs.router
                .fetch_metadata(&format!("inode_{ino}"))
                .await
                .expect("fetch");
        match m.block_map_id.as_deref() {
            Some(id) if id.starts_with("kvmap:") => break true,
            _ if blocks >= 4096 => break false,
            _ => {}
        }
    };
    assert!(crossed, "the fixture must cross to a kvmap head");

    // The LAST representable pair of blocks: indices u32::MAX−2 and
    // u32::MAX−1 (a 2-block aligned write is unambiguously striped
    // write-through). cap = block_size × (2^32 − 1) — §5's ceiling.
    let cap: u64 = BS * (u32::MAX as u64);
    let tail = vec![b'z'; 2 * BS as usize];
    fuse_write(&h, ino, cap - 2 * BS, &tail).await;
    assert_eq!(
        fuse_read(&h, ino, cap - 2 * BS, 2 * BS as u32).await,
        tail,
        "the last representable blocks round-trip"
    );
    assert_eq!(
        h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr.size,
        cap
    );
    // The boundary index is IN the tree-resolved map.
    let m =
        h.fs.router
            .fetch_metadata(&format!("inode_{ino}"))
            .await
            .expect("fetch");
    assert!(
        m.block_map
            .as_deref()
            .is_some_and(|map| map.contains_key(&(u32::MAX - 1))),
        "index u32::MAX − 1 must be mapped through the tree"
    );

    // Every mint of index u32::MAX refuses EFBIG at ADMISSION: no
    // allocation, no map ops, no crossing-train engagement.
    let puts_before = META_KV_BLOCK_MAP_PUTS.load(Ordering::Relaxed);
    let recs_before = METRICS.map_migrate_records.load(Ordering::Relaxed);

    let e = h
        .fs
        .write(h.req, ino, 0, cap, bytes::Bytes::from_static(b"x"), 0, 0)
        .await
        .expect_err("a write minting index u32::MAX must refuse");
    let io: std::io::Error = e.into();
    assert_eq!(io.raw_os_error(), Some(libc::EFBIG), "write → EFBIG");

    let e =
        h.fs.setattr(
            h.req,
            ino,
            None,
            fuse3::SetAttr {
                size: Some(cap + 1),
                ..Default::default()
            },
        )
        .await
        .expect_err("a truncate-up past the cap must refuse");
    let io: std::io::Error = e.into();
    assert_eq!(io.raw_os_error(), Some(libc::EFBIG), "truncate → EFBIG");

    let e =
        h.fs.fallocate(h.req, ino, 0, cap, BS, 0)
            .await
            .expect_err("a fallocate past the cap must refuse");
    let io: std::io::Error = e.into();
    assert_eq!(io.raw_os_error(), Some(libc::EFBIG), "fallocate → EFBIG");

    assert_eq!(
        META_KV_BLOCK_MAP_PUTS.load(Ordering::Relaxed),
        puts_before,
        "an EFBIG refusal stages NO map records (admission, not mid-publish)"
    );
    assert_eq!(
        METRICS.map_migrate_records.load(Ordering::Relaxed),
        recs_before,
        "an EFBIG refusal never engages the crossing train"
    );
    assert_eq!(
        h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr.size,
        cap,
        "an EFBIG op leaves no side effects"
    );
}

// ===========================================================================
// 3. The giant-sparse matrix (design §4 PR 3)
// ===========================================================================

/// Sparse kvmap indices {0, 2^20, 2^30, u32::MAX − 1}: publish, REMOUNT,
/// and the tree + the refetched map carry exactly those bindings — the
/// PB-class sparse shape survives the full durable round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn giant_sparse_matrix_survives_remount_exact() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    const SPARSE: [u32; 4] = [0, 1 << 20, 1 << 30, u32::MAX - 1];
    let (ino, entries) = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("pb_sparse").await;
        // Cross first (indices 0..SPILL), then bind the giant indices.
        let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
        let mut sparse: Vec<(u32, String)> = Vec::new();
        for idx in SPARSE {
            if idx < SPILL_BLOCKS {
                continue; // index 0 is already bound by the spill
            }
            let off = rig.alloc.allocate_block().await.unwrap();
            rig.alloc.publish_block(off);
            sparse.push((idx, off.to_string()));
        }
        rig.publish_entries(ino, &sparse, u64::from(u32::MAX) * 4 * 1024 * 1024)
            .await;
        entries.extend(sparse);
        entries.sort_unstable_by_key(|&(b, _)| b);
        rig.shutdown().await;
        (ino, entries)
    };

    let rig = mount(meta.path(), data.path()).await;
    assert_eq!(
        rig.durable_head(ino).await.block_map_id.as_deref(),
        Some("kvmap:1")
    );
    // The tree: exactly the published set, in index order.
    let records = rig.raw_records(ino).await;
    assert_eq!(records.len(), entries.len());
    // The read path: exact bindings at every sparse index.
    let map = rig.refetched_map(ino).await;
    assert_eq!(map.len(), entries.len());
    for (b, k) in &entries {
        assert_eq!(map.get(b), Some(k), "index {b} must survive the remount");
    }
    // The exact-lookup face resolves the boundary index too — as the
    // compact POINT form carrying the default slot's durable tag.
    let hit = rig
        .kv()
        .get_block_mapping(ino, u32::MAX - 1)
        .await
        .expect("exact lookup at the boundary")
        .expect("bound");
    let want: u64 = entries.last().unwrap().1.parse().expect("bare offset key");
    assert_eq!(
        hit,
        MapEntry::Point {
            vol_tag: volume_tag(DATA_VOL_ID),
            offset: want,
        },
        "the boundary record is the POINT form of its key string"
    );
    rig.shutdown().await;
}
