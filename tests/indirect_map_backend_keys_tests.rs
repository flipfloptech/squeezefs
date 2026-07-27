//! Indirect block-map backend-true key regression suite.
//!
//! Residual of the phantom-`backend_0` fix (commit 8380049): all eight
//! block-key PERSIST sites became backend-true (`name://offset` on
//! non-default volumes, bare offset only for the default slot) — but the
//! **indirect block map**, the spill target for layout maps whose serialized
//! size exceeds the per-volume inline record cap (§5.3), still serialized
//! `Vec<(u32, u64)>` bare offsets and rehydrated bare keys. On a
//! multi-data-volume mount every over-spill file's blocks placed on a
//! non-first volume were read/freed from the WRONG device once the map
//! spilled — the same written-on-ossN-read-from-oss1 corruption 8380049
//! fixed for inline maps.
//!
//! Contract pinned here:
//! 1. A spilled (indirect) block map round-trips IDENTITY-PRESERVING: the
//!    rehydrated map carries byte-identical key strings to what the inline
//!    map persisted (`persist_block_key` output), across a cold remount.
//! 2. The indirect blob on disk is VERSIONED: magic `SQFSIMAP` + u32 LE
//!    version 1 + bincode `Vec<(u32, String)>` — so the NEXT format change
//!    does not need a break.
//! 3. Inline↔indirect spill/unspill preserves prefixed keys in BOTH
//!    directions (grow past the cap, shrink back under it).
//! 4. Deleting a spilled file frees every block on the volume that OWNS it
//!    (allocator used-block counts return to zero per volume).
//! 5. Single-volume mounts keep persisting bare offset keys through the
//!    indirect mechanism (default-slot rule — on-disk pin).
//! 6. An old-shape (pre-versioned bare-offset) indirect blob fails LOUD
//!    with a typed error — never a silent misread.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{CachedMetadata, DataRouter, LayoutMetadata, StorageBackend};
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Router block size: big enough that the spilled indirect blob (≈22 KiB at
/// 700 entries) reads back whole from one block, small enough that a
/// 700-block burst writes only ~44 MiB of data.
const BLOCK: usize = 64 * 1024;
/// 64 KiB meta nodes ⇒ 16 KiB per-ino record cap ⇒ 12 KiB inline block-map
/// budget after the 4 KiB framing headroom — a ~700-entry map (~15-22 KiB
/// serialized) is safely past the spill boundary.
const SPILL_NODE_SIZE: usize = 64 * 1024;
/// Block-map entries written per burst; past the spill boundary with margin.
const SPILL_BLOCKS: usize = 700;
/// Allocator stride (fixed 4 MiB chunk).
const CHUNK: u64 = 4 * 1024 * 1024;

/// On-disk header of the versioned indirect block map (contract 2). Pinned
/// here as raw bytes on purpose: the test asserts the FORMAT, not whatever
/// helper the implementation uses.
const INDIRECT_MAGIC: &[u8; 8] = b"SQFSIMAP";
const INDIRECT_VERSION: u32 = 1;

/// Format (or reopen) one v3 metadata volume with 64 KiB nodes.
async fn open_v3_meta(path: &std::path::Path, len: u64, format: bool) -> Arc<KvMetaBackend> {
    if format {
        squeezefs::meta_backend::kv::builder::format_v3(
            path,
            len,
            &squeezefs::meta_backend::kv::builder::FormatV3Options {
                node_size: SPILL_NODE_SIZE,
                journal_len_override: None,
                force: true,
                full_wipe: false,
                format_config_xattr: None,
            },
        )
        .await
        .expect("format v3 meta volume");
    }
    KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

/// A data volume's stable identity across mounts: its name and backing file.
struct VolSpec {
    name: String,
    backing: NamedTempFile,
}

fn make_vol_specs(names: &[&str], len: u64) -> Vec<VolSpec> {
    names
        .iter()
        .map(|name| {
            let backing = NamedTempFile::new().unwrap();
            std::fs::File::create(backing.path())
                .unwrap()
                .set_len(len)
                .unwrap();
            VolSpec {
                name: name.to_string(),
                backing,
            }
        })
        .collect()
}

struct NamedVolume {
    name: String,
    device: Arc<NvmeBlockDev>,
    allocator: Arc<BlockAllocator>,
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    volumes: Vec<NamedVolume>,
    routed: Arc<RoutedMetaBackend>,
    _staging: TempDir,
}

/// Build a filesystem the way `main.rs` `Commands::Mount` does for named data
/// volumes: the FIRST volume's device+allocator become the router's default
/// slot (the `backend_0` alias target) and EVERY volume is registered in
/// `backends` under its real name. `format` controls whether the meta volume
/// is formatted fresh or reopened (cold remount).
async fn mount_h(specs: &[VolSpec], meta_path: &std::path::Path, format: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new("local").unwrap();

    let mut volumes = Vec::new();
    for spec in specs {
        let device = Arc::new(NvmeBlockDev::new(spec.backing.path().to_str().unwrap()));
        let allocator = Arc::new(
            BlockAllocator::new(dlm.meta_client().clone(), &spec.name)
                .await
                .unwrap(),
        );
        volumes.push(NamedVolume {
            name: spec.name.clone(),
            device,
            allocator,
        });
    }

    let first_dev = volumes[0].device.clone();
    let first_alloc = volumes[0].allocator.clone();

    let staging = tempdir().unwrap();
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
    for vol in &volumes {
        router.backend_router.backends.insert(
            vol.name.clone(),
            Arc::new(StorageBackend {
                device: vol.device.clone(),
                block_allocator: vol.allocator.clone(),
            }),
        );
    }

    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let meta_backend = open_v3_meta(meta_path, 256 * 1024 * 1024, format).await;
    let routed = Arc::new(RoutedMetaBackend::new(vec![meta_backend]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = Request {
        unique: 1,
        uid,
        gid,
        pid: 1234,
    };

    H {
        fs,
        req,
        volumes,
        routed,
        _staging: staging,
    }
}

/// Distinct, position-dependent content for block `b`: catches both
/// wrong-block and wrong-device (zeros / other block) read-backs.
fn pattern_block(b: usize) -> Vec<u8> {
    let mut v = vec![0u8; BLOCK];
    for (i, x) in v.iter_mut().enumerate() {
        *x = (b.wrapping_mul(31).wrapping_add(i.wrapping_mul(7)) % 251) as u8;
    }
    v
}

fn assert_block_bytes(b: usize, got: &[u8]) {
    let want = pattern_block(b);
    if got != want.as_slice() {
        let first_diff = got
            .iter()
            .zip(want.iter())
            .position(|(g, w)| g != w)
            .unwrap_or(got.len().min(want.len()));
        panic!(
            "block {b} read back wrong bytes — its key resolved to the wrong device \
             (len got={} want={}, first diff at byte {first_diff}: got=0x{:02x} want=0x{:02x})",
            got.len(),
            want.len(),
            got.get(first_diff).copied().unwrap_or(0),
            want.get(first_diff).copied().unwrap_or(0),
        );
    }
}

/// Write a striped burst of `nblocks` distinct BLOCK-sized blocks and fsync
/// so every block flushes through write placement and the layout persists.
async fn striped_spill_burst(h: &H, ino: u64, nblocks: usize) {
    // Force the striped layout transition first: a > BLOCK initial write.
    let dummy = vec![0u8; BLOCK + 1];
    h.fs.write(
        h.req,
        ino,
        0,
        0,
        bytes::Bytes::copy_from_slice(&dummy),
        0,
        0,
    )
    .await
    .expect("initial striped-transition write");

    for b in 0..nblocks {
        h.fs.write(
            h.req,
            ino,
            0,
            (b * BLOCK) as u64,
            bytes::Bytes::from(pattern_block(b)),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write of block {b} failed: {e:?}"));
    }
    h.fs.fsync(h.req, ino, 0, false)
        .await
        .expect("fsync after burst");
}

async fn create_file(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Drop every RAM/NVMe read-tier entry so subsequent reads must resolve each
/// block-map key through the backend router down to a real device read.
fn purge_read_tiers(h: &H) {
    for key in h.fs.router.cache.read_lru.keys() {
        h.fs.router.cache.read_lru.remove(&key);
    }
    for key in h.fs.router.cache.nvme.list_cached_blocks() {
        h.fs.router.cache.nvme.remove_cached_read_block(&key);
    }
}

async fn persisted_layout(routed: &RoutedMetaBackend, ino: u64) -> LayoutMetadata {
    let bytes = routed
        .getxattr(ino, "layout")
        .await
        .expect("getxattr layout")
        .expect("layout xattr present");
    bincode::deserialize::<LayoutMetadata>(&bytes).expect("deserialize layout")
}

fn is_indirect(l: &LayoutMetadata) -> bool {
    l.block_map.is_none()
        && l.block_map_id
            .as_deref()
            .is_some_and(|s| s.starts_with("indirect:"))
}

fn is_inline(l: &LayoutMetadata) -> bool {
    l.block_map.is_some()
        && l.block_map_id
            .as_deref()
            .is_some_and(|s| s.starts_with("block_map_"))
}

/// Persist a seeded dirty layout while HOLDING a freshly-acquired lease so
/// the fencing token is current (mirrors the production writer path; the
/// process-global "local" DLM fencing map is shared across tests).
async fn persist_under_lease(router: &DataRouter, dlm: &DlmClient, path: &str) {
    let lease = dlm
        .acquire_lock(path, None, std::time::Duration::from_secs(5))
        .await
        .expect("acquire lease");
    router
        .persist_dirty_layout_if_needed(path, lease.fencing_token())
        .await
        .expect("persist layout");
}

// ---------------------------------------------------------------------------
// 1. Multi-volume over-spill file: backend-true keys survive the spill and a
//    COLD REMOUNT; every block reads back byte-exact. (RED pre-fix: the
//    rehydrated map is all bare offsets and non-first-volume blocks read
//    from the wrong device.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multi_volume_spilled_map_survives_cold_remount_byte_exact() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();

    let specs = make_vol_specs(
        &["rm_oss1", "rm_oss2", "rm_oss3", "rm_oss4"],
        1536 * 1024 * 1024,
    );
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(256 * 1024 * 1024).unwrap();

    let h = mount_h(&specs, meta.path(), true).await;
    let ino = create_file(&h, "spill_burst.bin").await;
    striped_spill_burst(&h, ino, SPILL_BLOCKS).await;

    // The layout must actually have SPILLED — otherwise this proves nothing
    // (inline maps are already backend-true since 8380049).
    let layout = persisted_layout(&h.routed, ino).await;
    assert!(
        is_indirect(&layout),
        "a {SPILL_BLOCKS}-entry map must spill past the 16 KiB record cap \
         (id={:?}, inline_map={})",
        layout.block_map_id,
        layout.block_map.is_some()
    );

    // Placement sanity: the burst must have spread beyond the first volume.
    let spread = h
        .volumes
        .iter()
        .filter(|v| v.allocator.get_used_blocks() > 0)
        .count();
    assert!(
        spread >= 2,
        "burst did not spread across named volumes (used-block spread = {spread})"
    );

    // Cold fetch in-session: the rehydrated map must still carry backend-true
    // (prefixed) keys for the blocks placed on non-default volumes.
    let path = squeezefs::keys::inode_path(ino).to_string();
    h.fs.router.metadata_cache.invalidate(&ino);
    let fetched = h.fs.router.fetch_metadata(&path).await.expect("cold fetch");
    let map = fetched
        .block_map
        .as_ref()
        .expect("striped file has a block map");
    assert!(
        map.len() >= SPILL_BLOCKS,
        "fetched map lost entries: {} < {SPILL_BLOCKS}",
        map.len()
    );
    let prefixed = map.values().filter(|k| k.contains("://")).count();
    assert!(
        prefixed > 0,
        "indirect-map round-trip STRIPPED every backend prefix: {} entries, 0 prefixed — \
         non-first-volume blocks will be read/freed from the wrong device",
        map.len()
    );

    // Cold remount: clean shutdown, reopen the same meta volume, fresh router
    // over the same backing devices — the exact bytes on disk are the truth.
    h.routed.volumes[0]
        .shutdown()
        .await
        .expect("clean meta shutdown");
    drop(h);

    let h2 = mount_h(&specs, meta.path(), false).await;
    let entry = h2
        .fs
        .lookup(h2.req, 1, OsStr::new("spill_burst.bin"))
        .await
        .expect("lookup after remount");
    let ino2 = entry.attr.ino;
    assert_eq!(ino2, ino, "inode identity must survive remount");

    for b in 0..SPILL_BLOCKS {
        let reply = h2
            .fs
            .read(h2.req, ino2, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
            .await
            .unwrap_or_else(|e| {
                panic!("read of block {b} after remount errored (EIO to the app): {e:?}")
            });
        assert_block_bytes(b, &reply.data);
    }
}

// ---------------------------------------------------------------------------
// 2. Choke-point round-trip, both directions, plus the on-disk header pin:
//    a handcrafted mixed bare/prefixed map spills to an indirect block whose
//    blob is versioned (`SQFSIMAP` v1, bincode `Vec<(u32, String)>`),
//    rehydrates key-identical, reads sampled blocks from the RIGHT devices,
//    and re-inlines on shrink preserving the prefixed keys.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_spill_roundtrip_preserves_prefixed_keys_and_versioned_header() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());

    let dlm = DlmClient::new("local").unwrap();
    let specs = make_vol_specs(&["rt_ossa", "rt_ossb"], 256 * 1024 * 1024);
    let mut devs = Vec::new();
    for spec in &specs {
        let device = Arc::new(NvmeBlockDev::new(spec.backing.path().to_str().unwrap()));
        let allocator = Arc::new(
            BlockAllocator::new(dlm.meta_client().clone(), &spec.name)
                .await
                .unwrap(),
        );
        devs.push((spec.name.clone(), device, allocator));
    }

    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        devs[0].2.clone(),
        devs[0].1.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, devs[0].2.clone(), devs[0].1.clone());
    for (name, device, allocator) in &devs {
        router.backend_router.backends.insert(
            name.clone(),
            Arc::new(StorageBackend {
                device: device.clone(),
                block_allocator: allocator.clone(),
            }),
        );
    }

    let metaf = NamedTempFile::new().unwrap();
    metaf.as_file().set_len(128 * 1024 * 1024).unwrap();
    let be = open_v3_meta(metaf.path(), 128 * 1024 * 1024, true).await;
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    router.set_meta_backend(routed.clone());

    let ino = routed
        .create(1, "roundtrip", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create")
        .ino;
    let path = squeezefs::keys::inode_path(ino).to_string();

    // 8 REAL blocks, alternating volumes: even → default slot (bare key),
    // odd → second volume (prefixed key). Distinct patterns per block.
    let mut map = std::collections::HashMap::new();
    for i in 0..8u32 {
        let (name, device, allocator) = &devs[(i % 2) as usize];
        let off = allocator.allocate_block().await.expect("allocate");
        device
            .write_block(off, bytes::Bytes::from(pattern_block(i as usize)))
            .await
            .expect("write sample block");
        allocator.publish_block(off);
        let key = router.backend_router.persist_block_key(name, off);
        if i % 2 == 0 {
            assert!(
                !key.contains("://"),
                "default-slot key must stay bare, got {key:?}"
            );
        } else {
            assert!(
                key.starts_with("rt_ossb://"),
                "second-volume key must be prefixed, got {key:?}"
            );
        }
        map.insert(i, key);
    }
    // Synthetic entries 8..SPILL_BLOCKS force the spill (never read/freed);
    // offsets beyond the real allocations, alternating bare/prefixed.
    for i in 8..SPILL_BLOCKS as u32 {
        let off = (1000 + i as u64) * CHUNK;
        let key = if i % 2 == 0 {
            off.to_string()
        } else {
            format!("rt_ossb://{off}")
        };
        map.insert(i, key);
    }
    let expect = map.clone();

    router.metadata_cache.insert(
        ino,
        CachedMetadata {
            file_type: "striped".to_string(),
            size: (SPILL_BLOCKS * BLOCK) as u64,
            block_map: Some(std::sync::Arc::new(map.clone())),
            layout_dirty: true,
            ..Default::default()
        },
    );
    persist_under_lease(&router, &dlm, &path).await;

    let grown = persisted_layout(&routed, ino).await;
    assert!(
        is_indirect(&grown),
        "{SPILL_BLOCKS}-entry map must spill to an indirect block (id={:?})",
        grown.block_map_id
    );

    // --- Contract 2: the on-disk blob is versioned and carries key STRINGS.
    let ikey = grown
        .block_map_id
        .as_deref()
        .unwrap()
        .strip_prefix("indirect:")
        .unwrap();
    let raw = router
        .backend_router
        .read_block(ikey, BLOCK)
        .await
        .expect("read indirect block");
    assert!(
        raw.len() >= 12 && &raw[..8] == INDIRECT_MAGIC,
        "indirect blob must start with the {} magic (got first bytes {:02x?}) — \
         unversioned blobs can never be format-evolved without a break",
        String::from_utf8_lossy(INDIRECT_MAGIC),
        &raw[..raw.len().min(12)]
    );
    let version = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    assert_eq!(version, INDIRECT_VERSION, "indirect blob header version");
    let entries: Vec<(u32, String)> =
        bincode::deserialize(&raw[12..]).expect("indirect payload is bincode Vec<(u32, String)>");
    assert_eq!(entries.len(), expect.len(), "blob holds every entry");
    for (b, key) in &entries {
        assert_eq!(
            Some(key),
            expect.get(b),
            "blob entry {b} must carry the persisted key string verbatim"
        );
    }

    // --- Contract 1: cold rehydrate is key-identical.
    router.metadata_cache.invalidate(&ino);
    let fetched = router.fetch_metadata(&path).await.expect("cold fetch");
    let fetched_map = fetched.block_map.clone().expect("rehydrated block map");
    assert_eq!(
        *fetched_map, expect,
        "rehydrated indirect map must be IDENTICAL to what was persisted \
         (bare offsets here = the wrong-device corruption)"
    );

    // Device read-back of the real sampled blocks through the FETCHED keys.
    for i in 0..8u32 {
        let key = fetched_map.get(&i).unwrap();
        let got = router
            .backend_router
            .read_block(key, BLOCK)
            .await
            .unwrap_or_else(|e| panic!("read of sampled block {i} via {key:?} failed: {e:?}"));
        assert_block_bytes(i as usize, &got);
    }

    // --- Contract 3: shrink back under the cap → re-inlines, keys preserved,
    // and the old indirect block is freed.
    let used_before: u64 = devs.iter().map(|(_, _, a)| a.get_used_blocks()).sum();
    let shrunk_map: std::collections::HashMap<u32, String> = fetched_map
        .iter()
        .filter(|(&b, _)| b < 10)
        .map(|(&b, k)| (b, k.clone()))
        .collect();
    let expect_shrunk = shrunk_map.clone();
    let mut shrunk = fetched.clone();
    shrunk.size = 10 * BLOCK as u64;
    shrunk.block_map = Some(std::sync::Arc::new(shrunk_map));
    shrunk.layout_dirty = true;
    router.metadata_cache.insert(ino, shrunk);
    persist_under_lease(&router, &dlm, &path).await;

    let inline = persisted_layout(&routed, ino).await;
    assert!(
        is_inline(&inline),
        "shrunk map must re-inline (id={:?})",
        inline.block_map_id
    );
    assert_eq!(
        inline.block_map.unwrap(),
        expect_shrunk,
        "re-inlined map must preserve the prefixed keys byte-identical"
    );
    // Async block-reclaim: the displaced indirect block's finish_free
    // rides the background queue — drain before observing used-blocks.
    router.backend_router.reclaim_drain().await;
    let used_after: u64 = devs.iter().map(|(_, _, a)| a.get_used_blocks()).sum();
    assert_eq!(
        used_after + 1,
        used_before,
        "re-inlining must free exactly the old indirect block on its owning volume"
    );
}

// ---------------------------------------------------------------------------
// 3. Refcount/free correctness: deleting a spilled multi-volume file frees
//    every block on the allocator that OWNS it — every volume returns to
//    zero used blocks (including the indirect block itself).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_delete_spilled_file_frees_blocks_on_owning_volumes() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();

    let specs = make_vol_specs(
        &["del_oss1", "del_oss2", "del_oss3", "del_oss4"],
        1536 * 1024 * 1024,
    );
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(256 * 1024 * 1024).unwrap();

    let h = mount_h(&specs, meta.path(), true).await;
    let ino = create_file(&h, "spill_delete.bin").await;
    striped_spill_burst(&h, ino, SPILL_BLOCKS).await;

    let layout = persisted_layout(&h.routed, ino).await;
    assert!(
        is_indirect(&layout),
        "the map must have spilled before the delete is meaningful (id={:?})",
        layout.block_map_id
    );
    let spread = h
        .volumes
        .iter()
        .filter(|v| v.allocator.get_used_blocks() > 0)
        .count();
    assert!(
        spread >= 2,
        "burst did not spread across named volumes (used-block spread = {spread})"
    );

    // Cold delete: the metadata (and its spilled map) must be re-read from
    // the backend, exactly like a delete after remount/cache expiry.
    let path = squeezefs::keys::inode_path(ino).to_string();
    h.fs.router.metadata_cache.invalidate(&ino);
    purge_read_tiers(&h);

    let mut con = h.fs.router.dlm.get_connection().await.unwrap();
    h.fs.router
        .delete_file(&path, &mut con)
        .await
        .expect("delete_file");
    // Async block-reclaim: delete's frees finish on the background queue.
    h.fs.router.backend_router.reclaim_drain().await;

    for vol in &h.volumes {
        assert_eq!(
            vol.allocator.get_used_blocks(),
            0,
            "volume {} leaked blocks after delete — spilled-map frees routed to the wrong \
             allocator",
            vol.name
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Single-volume pin: bare-offset keys still round-trip byte-identical
//    through the indirect mechanism (default-slot rule), read back exact,
//    and free to zero. Must stay green before AND after the fix.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_single_volume_spilled_map_keys_stay_bare_and_roundtrip() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();

    let specs = make_vol_specs(&["solo_spill"], 3584 * 1024 * 1024);
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(256 * 1024 * 1024).unwrap();

    let h = mount_h(&specs, meta.path(), true).await;
    let ino = create_file(&h, "solo_spill.bin").await;
    striped_spill_burst(&h, ino, SPILL_BLOCKS).await;

    let layout = persisted_layout(&h.routed, ino).await;
    assert!(
        is_indirect(&layout),
        "single-volume {SPILL_BLOCKS}-entry map must spill too (id={:?})",
        layout.block_map_id
    );

    let path = squeezefs::keys::inode_path(ino).to_string();
    h.fs.router.metadata_cache.invalidate(&ino);
    let fetched = h.fs.router.fetch_metadata(&path).await.expect("cold fetch");
    let map = fetched.block_map.as_ref().expect("block map");
    for (b, key) in map.iter() {
        assert!(
            !key.contains("://"),
            "single-volume mounts must keep persisting BARE block keys through the \
             indirect map (on-disk pin) — block {b} rehydrated {key:?}"
        );
    }

    purge_read_tiers(&h);
    for b in (0..SPILL_BLOCKS).step_by(97) {
        let reply =
            h.fs.read(h.req, ino, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
                .await
                .unwrap_or_else(|e| panic!("single-volume read of block {b} errored: {e:?}"));
        assert_block_bytes(b, &reply.data);
    }

    h.fs.router.metadata_cache.invalidate(&ino);
    let mut con = h.fs.router.dlm.get_connection().await.unwrap();
    h.fs.router
        .delete_file(&path, &mut con)
        .await
        .expect("delete_file");
    // Async block-reclaim: delete's frees finish on the background queue.
    h.fs.router.backend_router.reclaim_drain().await;
    assert_eq!(
        h.volumes[0].allocator.get_used_blocks(),
        0,
        "single-volume delete must free every block including the indirect one"
    );
}

// ---------------------------------------------------------------------------
// 5 + 6. Loud typed failure on foreign blob shapes: the old pre-versioned
//    bare-offset encoding and an unknown header version must both surface a
//    typed error (EIO), never a silently-misread block map.
// ---------------------------------------------------------------------------

/// Single-volume router harness for the crafted-blob tests. Returns the
/// pieces needed to plant a blob and point a striped layout at it.
async fn crafted_blob_harness(
    tag: &str,
) -> (
    DataRouter,
    Arc<RoutedMetaBackend>,
    DlmClient,
    NamedTempFile,
    NamedTempFile,
    TempDir,
) {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), tag)
            .await
            .unwrap(),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);

    let metaf = NamedTempFile::new().unwrap();
    metaf.as_file().set_len(128 * 1024 * 1024).unwrap();
    let be = open_v3_meta(metaf.path(), 128 * 1024 * 1024, true).await;
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    router.set_meta_backend(routed.clone());
    (router, routed, dlm, backing, metaf, staging)
}

/// Plant `blob` as ino's indirect block map and return the cold-fetch result.
async fn fetch_with_planted_blob(
    router: &DataRouter,
    routed: &RoutedMetaBackend,
    name: &str,
    mut blob: Vec<u8>,
) -> squeezefs::error::Result<CachedMetadata> {
    let ino = routed
        .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create")
        .ino;
    let off = router
        .backend_router
        .default_allocator
        .allocate_block()
        .await
        .expect("allocate indirect block");
    let aligned = (blob.len() + 4095) & !4095;
    blob.resize(aligned, 0);
    router
        .backend_router
        .default_device
        .write_block(off, bytes::Bytes::from(blob))
        .await
        .expect("write crafted blob");
    router.backend_router.default_allocator.publish_block(off);

    let layout = LayoutMetadata {
        file_type: "striped".to_string(),
        size: 2 * BLOCK as u64,
        block_map_id: Some(format!("indirect:{off}")),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: None,
    };
    let bytes = bincode::serialize(&layout).expect("serialize layout");
    routed
        .set_layout_and_size(ino, &bytes, layout.size)
        .await
        .expect("plant layout");

    router
        .fetch_metadata(&squeezefs::keys::inode_path(ino).to_string())
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_old_shape_indirect_blob_fails_loud_never_silent_misread() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let (router, routed, _dlm, _b, _m, _s) = crafted_blob_harness("imap_oldshape").await;

    // The exact pre-versioned on-disk shape: raw bincode Vec<(u32, u64)>.
    let old_entries: Vec<(u32, u64)> = vec![(0, 0), (1, CHUNK)];
    let blob = bincode::serialize(&old_entries).expect("serialize old shape");

    let res = fetch_with_planted_blob(&router, &routed, "old_shape.bin", blob).await;
    let err = res.expect_err(
        "an old-shape (pre-versioned bare-offset) indirect blob must fail LOUD — \
         silently rehydrating it as a block map is exactly the wrong-device corruption",
    );
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("IndirectMapFormat"),
        "must be the typed indirect-map format error, got: {dbg}"
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("reformat"),
        "the error must tell the operator to reformat (pre-beta blob), got: {msg}"
    );
    assert_eq!(err.to_errno(), libc::EIO, "surfaces as EIO to the app");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unknown_version_indirect_blob_fails_loud() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let (router, routed, _dlm, _b, _m, _s) = crafted_blob_harness("imap_badver").await;

    // Well-formed magic, future version: must refuse loudly instead of
    // guessing at the payload shape.
    let mut blob = Vec::new();
    blob.extend_from_slice(INDIRECT_MAGIC);
    blob.extend_from_slice(&99u32.to_le_bytes());
    let entries: Vec<(u32, String)> = vec![(0, "0".to_string())];
    blob.extend_from_slice(&bincode::serialize(&entries).unwrap());

    let res = fetch_with_planted_blob(&router, &routed, "bad_version.bin", blob).await;
    let err = res.expect_err("an unknown-version indirect blob must fail loud");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("IndirectMapFormat"),
        "must be the typed indirect-map format error, got: {dbg}"
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("version"),
        "the error must name the unsupported version, got: {msg}"
    );
    assert_eq!(err.to_errno(), libc::EIO, "surfaces as EIO to the app");
}
