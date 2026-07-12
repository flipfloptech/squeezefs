//! Inline block-map refcount-recovery prefixed-key regression suite.
//!
//! Residual found by the indirect-map fix (commit 0c07bc0): the **inline**
//! branch of the mount-time refcount recovery scan
//! (`BlockAllocator::recover_from_layout`, src/block_allocator.rs)
//! normalized stored block-map values with `split(':')` — which turns a
//! backend-true prefixed key (`oss2://123` / `oss2://123:extra`) into the
//! bare volume name (`oss2`). That parses on no volume, so recovery silently
//! skips every inline-map entry that lives on a non-default volume: the
//! block stays unallocated in its owner's fresh allocator, `allocate_block`
//! can hand the SAME offset to a new writer, and the old file's bytes are
//! clobbered — a remount-then-corrupt window. The indirect branch (fixed in
//! 0c07bc0) already does this correctly via `clean_block_key` + owner-volume
//! matching; this suite pins the SAME discipline onto the inline branch.
//!
//! Contract pinned here:
//! 1. Recovery of a striped layout with an INLINE block map recovers every
//!    entry on the volume that OWNS it — prefixed keys (`name://offset`,
//!    with or without a `:extra` trailer) on the named volume, bare keys
//!    (`offset` / `offset:extra`) on the default slot.
//! 2. A recovered block is NOT double-allocatable: the owner's fresh
//!    allocator must never hand out a recovered offset again, and its
//!    refcount is live (an `increment_refcount` probe succeeds).
//! 3. No cross-claim: a volume must not recover entries owned by another
//!    volume (over-recovery leaks blocks on the wrong allocator).
//! 4. End-to-end: a multi-volume mount with an inline-map striped file
//!    survives a cold remount + recovery — reads stay byte-exact, a
//!    post-recovery writer cannot clobber the old file, and deletes free
//!    every block on its owning volume back to zero.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{DataRouter, LayoutMetadata, StorageBackend};
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

/// Small router block size so a burst is cheap; the inline record cap is
/// far above a dozen entries, so maps here never spill.
const BLOCK: usize = 64 * 1024;
/// Allocator stride (fixed 4 MiB chunk).
const CHUNK: u64 = 4 * 1024 * 1024;
/// Blocks written by the end-to-end burst: enough to spread across volumes,
/// far below the spill boundary (map stays INLINE).
const INLINE_BLOCKS: usize = 12;

/// Format (or reopen) one v3 metadata volume.
async fn open_v3_meta(path: &std::path::Path, len: u64, format: bool) -> Arc<KvMetaBackend> {
    if format {
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
    router
        .backend_router
        .active_write_backend
        .store(Arc::new(volumes[0].name.clone()));

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

/// Run mount-time refcount recovery the way `main.rs` does: every data
/// volume's allocator walks every meta volume's live inode tree.
async fn run_recovery(h: &H) {
    for kv in &h.routed.volumes {
        for vol in &h.volumes {
            vol.allocator
                .recover_active_blocks_v3(kv, &h.fs.router.backend_router)
                .await
                .unwrap_or_else(|e| panic!("recovery on volume {} failed: {e:?}", vol.name));
        }
    }
}

/// Distinct, position-dependent content for block `b` of file `salt`:
/// catches wrong-block, wrong-device, and clobbered read-backs.
fn pattern_block(salt: usize, b: usize) -> Vec<u8> {
    let mut v = vec![0u8; BLOCK];
    for (i, x) in v.iter_mut().enumerate() {
        *x = (salt
            .wrapping_mul(131)
            .wrapping_add(b.wrapping_mul(31))
            .wrapping_add(i.wrapping_mul(7))
            % 251) as u8;
    }
    v
}

fn assert_block_bytes(salt: usize, b: usize, got: &[u8], when: &str) {
    let want = pattern_block(salt, b);
    if got != want.as_slice() {
        let first_diff = got
            .iter()
            .zip(want.iter())
            .position(|(g, w)| g != w)
            .unwrap_or(got.len().min(want.len()));
        panic!(
            "block {b} read back wrong bytes {when} (len got={} want={}, first diff at byte \
             {first_diff}: got=0x{:02x} want=0x{:02x})",
            got.len(),
            want.len(),
            got.get(first_diff).copied().unwrap_or(0),
            want.get(first_diff).copied().unwrap_or(0),
        );
    }
}

/// Write a striped burst of `nblocks` distinct BLOCK-sized blocks and fsync
/// so every block flushes through write placement and the layout persists.
async fn striped_burst(h: &H, ino: u64, salt: usize, nblocks: usize) {
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
            bytes::Bytes::from(pattern_block(salt, b)),
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

async fn persisted_layout(routed: &RoutedMetaBackend, ino: u64) -> LayoutMetadata {
    let bytes = routed
        .getxattr(ino, "layout")
        .await
        .expect("getxattr layout")
        .expect("layout xattr present");
    bincode::deserialize::<LayoutMetadata>(&bytes).expect("deserialize layout")
}

// ---------------------------------------------------------------------------
// 1 + 2 + 3. Deterministic planted-layout recovery: an inline block map
//    carrying every persisted key shape — bare, bare+extra, prefixed,
//    prefixed+extra — recovers each entry on EXACTLY the volume that owns
//    it, refcounts live, offsets not double-allocatable. (RED pre-fix: the
//    inline branch's `split(':')` mangles both prefixed shapes to the bare
//    volume name, so the second volume recovers nothing.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_inline_map_recovery_recovers_prefixed_keys_on_owning_volume() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();

    let specs = make_vol_specs(&["rp_oss1", "rp_oss2"], 256 * 1024 * 1024);
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(256 * 1024 * 1024).unwrap();

    let h = mount_h(&specs, meta.path(), true).await;

    // Allocate + write four REAL blocks: two on the default slot (bare
    // keys), two on the second volume (prefixed keys). One of each carries
    // the historical `:extra` trailer that block-map values may store after
    // the offset (packed length / crypto framing).
    let ino = h
        .routed
        .create(1, "planted_inline.bin", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create")
        .ino;

    let mut map = std::collections::HashMap::new();
    let mut owned_offsets: Vec<(usize, u64)> = Vec::new(); // (volume idx, offset)
    for i in 0..4u32 {
        let vol_idx = (i % 2) as usize;
        let vol = &h.volumes[vol_idx];
        let off = vol.allocator.allocate_block().await.expect("allocate");
        vol.device
            .write_block(off, bytes::Bytes::from(pattern_block(1, i as usize)))
            .await
            .expect("write sample block");
        vol.allocator.publish_block(off);
        let key = h.fs.router.backend_router.persist_block_key(&vol.name, off);
        if vol_idx == 0 {
            assert!(
                !key.contains("://"),
                "default-slot key must stay bare, got {key:?}"
            );
        } else {
            assert!(
                key.starts_with("rp_oss2://"),
                "second-volume key must be prefixed, got {key:?}"
            );
        }
        // Entries 2 and 3 carry a `:extra` trailer after the offset.
        let stored = if i >= 2 { format!("{key}:12345") } else { key };
        map.insert(i, stored);
        owned_offsets.push((vol_idx, off));
    }

    let layout = LayoutMetadata {
        file_type: "striped".to_string(),
        size: 4 * BLOCK as u64,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map),
    };
    let bytes = bincode::serialize(&layout).expect("serialize layout");
    h.routed
        .set_layout_and_size(ino, &bytes, layout.size)
        .await
        .expect("plant layout");

    // --- Simulated remount: fresh allocators (same volume names), fresh
    // router wired exactly like mount, recovery walk.
    let h2 = mount_h(&specs, meta.path(), false).await;
    for vol in &h2.volumes {
        assert_eq!(
            vol.allocator.get_used_blocks(),
            0,
            "fresh allocator {} must start empty",
            vol.name
        );
    }
    run_recovery(&h2).await;

    // Contract 1: each volume recovered EXACTLY its own two blocks.
    for (vol_idx, vol) in h2.volumes.iter().enumerate() {
        let owned: Vec<u64> = owned_offsets
            .iter()
            .filter(|(v, _)| *v == vol_idx)
            .map(|(_, off)| *off)
            .collect();
        assert_eq!(
            vol.allocator.get_used_blocks(),
            owned.len() as u64,
            "volume {} must recover exactly its {} inline-map blocks — a mangled \
             prefixed key under-recovers (0) and a cross-claim over-recovers",
            vol.name,
            owned.len()
        );

        let free = vol.allocator.get_free_blocks().await.unwrap();
        for off in &owned {
            let idx = off / CHUNK;
            assert!(
                !free.contains(&idx),
                "volume {}: recovered block idx {idx} must NOT sit in the free list \
                 (double-allocatable => future corruption)",
                vol.name
            );
            // Contract 2: the refcount is live — a reference probe succeeds.
            assert!(
                vol.allocator.increment_refcount(*off),
                "volume {}: offset {off} must have a live recovered refcount",
                vol.name
            );
            // Undo the probe's reference.
            vol.allocator.begin_free(*off);
        }

        // Contract 2: not double-allocatable — a fresh allocation must not
        // return any recovered offset.
        let fresh_off = vol.allocator.allocate_block().await.expect("allocate");
        assert!(
            !owned.contains(&fresh_off),
            "volume {}: allocate_block after recovery handed out RECOVERED offset \
             {fresh_off} — the old file's bytes are now one write away from clobber",
            vol.name
        );
    }
}

// ---------------------------------------------------------------------------
// 4. End-to-end multi-volume remount smoke: an inline-map striped file with
//    blocks on oss2+ survives cold remount + recovery — byte-exact reads, a
//    post-recovery writer cannot clobber it, delete frees per-volume to
//    zero. (RED pre-fix: the post-recovery burst reuses the old file's
//    non-first-volume offsets and the re-read sees clobbered bytes.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multi_volume_inline_map_remount_recovery_smoke() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();

    let specs = make_vol_specs(&["sm_oss1", "sm_oss2", "sm_oss3"], 512 * 1024 * 1024);
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(256 * 1024 * 1024).unwrap();

    let h = mount_h(&specs, meta.path(), true).await;
    let ino = create_file(&h, "inline_multi.bin").await;
    striped_burst(&h, ino, 1, INLINE_BLOCKS).await;

    // The layout must be striped with an INLINE map (this suite's subject —
    // spilled maps are covered by the indirect suite) and must carry at
    // least one prefixed (non-first-volume) key.
    let layout = persisted_layout(&h.routed, ino).await;
    assert_eq!(layout.file_type, "striped", "burst must persist striped");
    let map = layout
        .block_map
        .as_ref()
        .expect("small striped file keeps its block map INLINE");
    let prefixed = map.values().filter(|k| k.contains("://")).count();
    assert!(
        prefixed > 0,
        "burst placed no blocks on non-first volumes ({} entries, 0 prefixed) — \
         the regression needs oss2+ placement",
        map.len()
    );
    let used_before: Vec<u64> = h
        .volumes
        .iter()
        .map(|v| v.allocator.get_used_blocks())
        .collect();
    let spread = used_before.iter().filter(|&&u| u > 0).count();
    assert!(
        spread >= 2,
        "burst did not spread across named volumes (used-block spread = {spread})"
    );

    // Cold remount: clean shutdown, reopen the same meta volume, fresh
    // allocators over the same backing devices, then the mount-time
    // recovery walk.
    h.routed.volumes[0]
        .shutdown()
        .await
        .expect("clean meta shutdown");
    drop(h);

    let h2 = mount_h(&specs, meta.path(), false).await;
    run_recovery(&h2).await;

    // Per-volume used counts must match the pre-remount truth exactly.
    for (vol, want) in h2.volumes.iter().zip(used_before.iter()) {
        assert_eq!(
            vol.allocator.get_used_blocks(),
            *want,
            "volume {} recovered the wrong number of blocks (want {want}) — \
             inline-map recovery must be per-owning-volume exact",
            vol.name
        );
    }

    let entry = h2
        .fs
        .lookup(h2.req, 1, OsStr::new("inline_multi.bin"))
        .await
        .expect("lookup after remount");
    let ino2 = entry.attr.ino;
    assert_eq!(ino2, ino, "inode identity must survive remount");

    for b in 0..INLINE_BLOCKS {
        let reply = h2
            .fs
            .read(h2.req, ino2, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("read of block {b} after remount errored: {e:?}"));
        assert_block_bytes(1, b, &reply.data, "after remount+recovery");
    }

    // Corruption probe: a NEW file written after recovery must not be
    // handed the old file's offsets (that is exactly what under-recovery
    // permits). Write it, then re-read the OLD file byte-exact.
    let probe_ino = create_file(&h2, "post_recovery_probe.bin").await;
    striped_burst(&h2, probe_ino, 2, INLINE_BLOCKS).await;
    for b in 0..INLINE_BLOCKS {
        let reply = h2
            .fs
            .read(h2.req, ino2, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("re-read of block {b} errored: {e:?}"));
        assert_block_bytes(
            1,
            b,
            &reply.data,
            "after a post-recovery writer (offsets were double-allocated)",
        );
    }

    // Delete both files cold (metadata re-read from the backend): every
    // volume must return to zero used blocks — frees must route to the
    // allocator that OWNS each block.
    for (name, target) in [
        ("inline_multi.bin", ino2),
        ("post_recovery_probe.bin", probe_ino),
    ] {
        let path = squeezefs::keys::inode_path(target).to_string();
        h2.fs.router.metadata_cache.invalidate(&path);
        let mut con = h2.fs.router.dlm.get_connection().await.unwrap();
        h2.fs
            .router
            .delete_file(&path, &mut con)
            .await
            .unwrap_or_else(|e| panic!("delete of {name} failed: {e:?}"));
    }
    for vol in &h2.volumes {
        assert_eq!(
            vol.allocator.get_used_blocks(),
            0,
            "volume {} leaked blocks after deleting every file — frees routed to \
             the wrong allocator or refcounts were never recovered",
            vol.name
        );
    }
}
