//! `squeezefs clone` — the CLI verb must produce a REAL CoW clone.
//!
//! The bug (surfaced by the DLM S2 audit, pre-RC loose ends): the
//! `Commands::Clone` arm built a bare `DataRouter` and never called
//! `set_meta_backend`, so `resolve_path_to_inode` short-circuited to
//! ino 1 (its whole body is `if let Some(backend) = …`) and
//! `clone_path`'s body — another `if let Some(backend) = …` — was
//! skipped entirely. The verb printed "File cloned successfully." and
//! did NOTHING: no destination inode, no refcount, no layout. A
//! documented CLI verb that exits 0 having performed no work is worse
//! than a missing one.
//!
//! Contracts pinned here (the clone-semantics definition — instant CoW,
//! metadata-only):
//!
//! 1. The verb creates the destination inode under the destination
//!    parent and it reads back the SOURCE's bytes.
//! 2. It is metadata-only: the destination's block map names the SAME
//!    block keys (identical device offsets) — nothing is copied.
//! 3. The shared blocks are PINNED: after the clone, allocator refcount
//!    recovery over the durable layouts counts 2 references per shared
//!    block (source + clone), so dropping one is not terminal.
//! 4. A missing metadata URI is a LOUD refusal, never a silent success
//!    (without a meta set the verb cannot resolve either path).
//! 5. A staged source is a loud refusal offline: the payload lives in
//!    the mount's per-mount-isolated staging, which the offline
//!    coordinator deliberately never opens (the `remove-data` /
//!    `fsck --repair` posture), so it cannot be carried honestly.
//!
//! Root is never required: the source file is built in-process through
//! the same `SqueezefsFilesystem` write path a mount uses, then the meta
//! volume is released and the REAL binary runs the verb.

use fuse3::raw::prelude::Filesystem;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::Metadata;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig, VOL_STATE_ACTIVE};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const BLOCK: usize = 65536;
const META_LEN: u64 = 256 * 1024 * 1024;
const DATA_LEN: u64 = 256 * 1024 * 1024;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn req() -> fuse3::raw::Request {
    fuse3::raw::Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    }
}

fn format_config(data_lvs: &[&Path]) -> FormatConfig {
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
        // Cache-less by format (the policy's permanent posture): every
        // beyond-inline write routes STRIPED, which is the layout class
        // an offline clone can carry by refcount.
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

/// One meta + one data volume, formatted the way `squeezefs format`
/// records them (format config xattr names the data volume, so the
/// binary's offline router finds it).
async fn format_set(dir: &Path) -> (PathBuf, PathBuf) {
    let meta = dir.join("meta.bin");
    let data = dir.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta")
        .set_len(META_LEN)
        .expect("size meta");
    std::fs::File::create(&data)
        .expect("create data")
        .set_len(DATA_LEN)
        .expect("size data");
    let cfg = format_config(&[data.as_path()]);
    squeezefs::meta_backend::kv::builder::format_v3(
        &meta,
        META_LEN,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).expect("encode format config")),
        },
    )
    .await
    .expect("format v3 meta volume");
    (meta, data)
}

fn records(data: &Path) -> Vec<DataVolumeRecord> {
    vec![DataVolumeRecord {
        id: data
            .file_name()
            .and_then(|n| n.to_str())
            .expect("data basename")
            .to_string(),
        backing_dev: data.display().to_string(),
        state: VOL_STATE_ACTIVE.to_string(),
        added_ts: 0,
    }]
}

/// The mount-shaped in-process fixture (the fsck/drain offline shape):
/// resolved records drive `register_backend`, allocator refcount
/// recovery runs like a mount, and NO staging dirs exist (cache-less).
struct Fx {
    fs: SqueezefsFilesystem,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
}

async fn open_fixture(meta_path: &Path, recs: &[DataVolumeRecord]) -> Fx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new().expect("dlm");
    let first = &recs[0];
    let dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let alloc = Arc::new(BlockAllocator::new(&first.id).await.expect("allocator"));
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        alloc.set_capacity_bytes(cap);
    }
    let cache = TieredCache::new(
        Vec::new(), // cache-less: beyond-inline writes route striped
        Some("64MB"),
        Some("64MB"),
        None,
        None,
        alloc.clone(),
        dev.clone(),
        None,
    )
    .await
    .expect("cache");
    let router = DataRouter::new(dlm.clone(), cache, alloc, dev);
    router.set_block_size(BLOCK as u64);
    for rec in recs {
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(recs.to_vec());

    // The MOUNT's open (not the in-RAM `RoutedMetaBackend::new`
    // constructor): the derived routing width + mint spread decide which
    // global inos this process mints, and the binary under test opens the
    // same way — a fixture on the identity map would route the verb's
    // freshly minted global ino to a nonexistent local one.
    let routed = squeezefs::meta_backend::open_routed_meta_set(&[meta_path.display().to_string()])
        .await
        .expect("routed open of the meta set");
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    for vol in &routed.volumes {
        for entry in fs.router.backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(vol, &fs.router.backend_router)
                .await
                .expect("allocator refcount recovery");
        }
    }
    Fx { fs, meta: routed }
}

impl Fx {
    async fn close(self) {
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean meta shutdown");
        }
    }

    /// The refcount the allocator holds for `mapping`'s block (`None` =
    /// untracked). Resolves the mapping exactly like the router does:
    /// strip the size-carrying tail, then split backend id from offset.
    fn refcount_of(&self, mapping: &str) -> Option<u32> {
        let clean = squeezefs::routing::clean_block_key(mapping);
        let br = &self.fs.router.backend_router;
        let (be_id, offset) = br.parse_block_key(&clean).expect("block key");
        // Bare keys resolve to the default slot (the bare-key invariant);
        // prefixed keys to their registered member.
        if be_id == "backend_0" {
            br.default_allocator.refcount(offset)
        } else {
            br.backends
                .get(&be_id)
                .expect("backend registered")
                .value()
                .block_allocator
                .refcount(offset)
        }
    }

    async fn create(&self, name: &str) -> u64 {
        self.fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create")
            .attr
            .ino
    }

    async fn read(&self, ino: u64, off: u64, size: u32) -> Vec<u8> {
        self.fs
            .read(req(), ino, 0, off, size, 0)
            .await
            .expect("read")
            .data
            .to_vec()
    }

    /// Durable block mappings `(idx, mapping)` for `ino`.
    async fn mappings(&self, ino: u64) -> Vec<(u32, String)> {
        let meta = self
            .fs
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .expect("layout");
        let mut out = Vec::new();
        if let Some(map) = meta.block_map.as_deref() {
            for (&b, mapping) in map {
                out.push((b, mapping.clone()));
            }
        }
        out.sort();
        out
    }

    async fn lookup(&self, name: &str) -> Option<u64> {
        match self.meta.lookup(1, name).await {
            Ok(inode) if inode.ino != 0 => Some(inode.ino),
            Ok(inode) => {
                eprintln!("lookup({name}) returned ino {}", inode.ino);
                None
            }
            Err(e) => {
                eprintln!("lookup({name}) failed: {e:?}");
                None
            }
        }
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 251) as u8) ^ seed).collect()
}

/// Build a two-block striped source file and return its bytes.
async fn build_striped_source(fx: &Fx, name: &str, nblocks: usize) -> (u64, Vec<u8>) {
    let ino = fx.create(name).await;
    let mut expected = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let data = pattern(BLOCK, b as u8 ^ 0x5A);
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
            .unwrap_or_else(|e| panic!("write block {b}: {e:?}"));
    }
    fx.fs.fsync(req(), ino, 0, false).await.expect("fsync");
    assert!(
        fx.fs
            .write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "write pipeline must drain before the map is observed"
    );
    (ino, expected)
}

fn run_clone(meta: &Path, src: &str, dest: &str) -> std::process::Output {
    Command::new(bin())
        .arg("clone")
        .arg("-g")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(src)
        .arg(dest)
        .output()
        .expect("run squeezefs clone")
}

/// Contracts 1–3: the verb produces a real CoW clone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cli_clone_verb_produces_a_real_cow_clone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (meta, data) = format_set(dir.path()).await;
    let recs = records(&data);

    // Source: two striped blocks, durably published, guard released.
    let (src_ino, expected, src_map) = {
        let fx = open_fixture(&meta, &recs).await;
        let (ino, expected) = build_striped_source(&fx, "src.bin", 2).await;
        let map = fx.mappings(ino).await;
        assert_eq!(
            map.len(),
            2,
            "source must be striped over 2 blocks: {map:?}"
        );
        for (_, mapping) in &map {
            assert_eq!(
                fx.refcount_of(mapping),
                Some(1),
                "pre-clone refcount must be 1 for {mapping}"
            );
        }
        fx.close().await;
        (ino, expected, map)
    };

    // The verb, through the real binary.
    let out = run_clone(&meta, "/src.bin", "/dest.bin");
    assert!(
        out.status.success(),
        "clone verb failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Durable effects, observed by a fresh mount-shaped fixture.
    let fx = open_fixture(&meta, &recs).await;
    let dest_ino = fx
        .lookup("dest.bin")
        .await
        .expect("clone must create the destination inode (silent no-op regression)");
    assert_ne!(dest_ino, src_ino, "clone must mint a NEW inode");

    // Contract 2 — metadata-only: identical block keys, nothing copied.
    let dest_map = fx.mappings(dest_ino).await;
    assert_eq!(
        dest_map, src_map,
        "clone must reference the SOURCE's blocks (instant CoW, no data copy)"
    );

    // Contract 3 — the shared blocks are pinned twice (source + clone).
    for (_, mapping) in &dest_map {
        assert_eq!(
            fx.refcount_of(mapping),
            Some(2),
            "shared block {mapping} must carry 2 references after the clone"
        );
    }

    // Contract 1 — the clone reads back the source's bytes.
    let got = fx.read(dest_ino, 0, expected.len() as u32).await;
    assert_eq!(
        got.len(),
        expected.len(),
        "clone short read: {} vs {}",
        got.len(),
        expected.len()
    );
    assert_eq!(got, expected, "clone content differs from the source");

    fx.close().await;
}

/// Contract 4: no meta URI ⇒ loud refusal, never a silent success.
#[test]
fn test_cli_clone_without_meta_uri_refuses_loudly() {
    let out = Command::new(bin())
        .arg("clone")
        .arg("/src.bin")
        .arg("/dest.bin")
        .env_remove("SQUEEZEFS_META_URI")
        .output()
        .expect("run squeezefs clone");
    assert!(
        !out.status.success(),
        "clone without a metadata volume must FAIL (it cannot resolve either path): {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("sqmeta://"),
        "the refusal must name the missing metadata URI, got: {msg}"
    );
    assert!(
        !msg.contains("cloned successfully"),
        "a refusal must never print success: {msg}"
    );
}

/// Contract 4b: a source that does not exist is a loud refusal too — the
/// no-op verb "succeeded" on every nonexistent path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cli_clone_missing_source_refuses_loudly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (meta, _data) = format_set(dir.path()).await;

    let out = run_clone(&meta, "/nope.bin", "/dest.bin");
    assert!(
        !out.status.success(),
        "clone of a nonexistent source must FAIL: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Contract 5: a staged source refuses loudly offline (its acked payload
/// lives in the mount's isolated staging, which this coordinator never
/// opens) — never a zero-filled "successful" clone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cli_clone_staged_source_refuses_loudly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (meta, data) = format_set(dir.path()).await;
    let recs = records(&data);

    // A staged source needs a staging dir, so this fixture gets one
    // (the format config stays cache-less — only this in-process writer
    // stages, exactly like a mount that formatted with cache paths).
    let staging = tempfile::tempdir().expect("staging");
    let ino = {
        std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
        // The staged-source contract pins the inline ceiling at one page
        // (the default since the phase-B sweep) explicitly, so an
        // environment override cannot move the 16 KiB source inline.
        squeezefs::routing::set_inline_max_bytes_override(Some(
            squeezefs::routing::INLINE_MAX_FLOOR,
        ));
        let dlm = DlmClient::new().expect("dlm");
        let first = &recs[0];
        let dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
        let alloc = Arc::new(BlockAllocator::new(&first.id).await.expect("allocator"));
        let cache = TieredCache::new(
            vec![staging.path().to_path_buf()],
            Some("64MB"),
            Some("64MB"),
            Some("64MB"),
            Some("64MB"),
            alloc.clone(),
            dev.clone(),
            None,
        )
        .await
        .expect("cache");
        let router = DataRouter::new(dlm.clone(), cache, alloc, dev);
        router.set_block_size(BLOCK as u64);
        router
            .backend_router
            .register_backend(first)
            .await
            .expect("register backend");
        router.backend_router.set_volume_records(recs.to_vec());
        let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(&meta)
            .await
            .expect("open meta volume");
        let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
        let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
        fs.router.set_meta_backend(routed.clone());
        fs.meta_backend = Some(routed.clone());

        let ino = fs
            .create(req(), 1, OsStr::new("staged.bin"), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create")
            .attr
            .ino;
        let data = pattern(8192, 0x11);
        fs.write(req(), ino, 0, 0, bytes::Bytes::copy_from_slice(&data), 0, 0)
            .await
            .expect("staged write");
        fs.fsync(req(), ino, 0, false).await.expect("fsync");
        let meta_snapshot = fs
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .expect("layout");
        assert_eq!(
            meta_snapshot.file_type.as_str(),
            "staged",
            "fixture must produce a STAGED source"
        );
        for vol in &routed.volumes {
            vol.shutdown().await.expect("clean meta shutdown");
        }
        ino
    };
    assert!(ino > 1);

    let out = run_clone(&meta, "/staged.bin", "/staged_clone.bin");
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "an offline clone of a staged source must REFUSE (its payload is not \
         reachable without the mount's staging): {msg}"
    );
    assert!(
        msg.contains("staged"),
        "the refusal must name the staged custody, got: {msg}"
    );
}
