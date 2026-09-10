//! The packed-mapping WIRE LAW and the ONE ranged, routed read funnel for
//! size-carrying mappings — PR PK1 of `docs/design-small-file-packing.md`
//! (§1.3 FIND-PK-0 / FIND-PK-1 / FIND-PK-3, §5.1, §5.10, §7, KD-6, KD-7).
//!
//! A size-carrying mapping `[be://]offset[@inc]:rel_off:packed_len` is the
//! form every promoted staged file carries today with `rel_off == 0`; the
//! packing program makes `rel_off` real (a tenant's slot inside a shared
//! block), so this suite pins what every consumer of the form must do at
//! ANY `(rel_off, packed_len)` inside the block — before any packer exists.
//!
//! In-tree consumers this census covers (design §1.2 — a future consumer
//! joins it here): `DataRouter::parse_block_mapping` (the decoder every
//! read funnel runs), `clean_block_key` (accounting / free / recovery /
//! fsck / movers resolve the BASE), `is_whole_block_mapping` (every
//! in-place arm refuses the decorated form), `jobs::decoration_suffix`
//! (`move_one` re-attaches the decoration verbatim),
//! `BackendRouter::block_key_map_entry` (the kvmap tree carries decorated
//! shapes as STRING records verbatim), the fsck C7 scrub's tenant-window
//! read, and the read funnel itself.
//!
//! Contracts (red-first):
//!  1. **FIND-PK-0** — on a 2-data-volume set with placement steered to the
//!     SECOND volume, a promoted staged file reads byte-exact after its
//!     ring entry is released (the old funnel read `self.nvme_writer`, the
//!     DEFAULT device, at the mapping's offset). Live mount.
//!  2. **FIND-PK-1** — the device-fetch funnel's exact arm reads exactly the
//!     tenant's `LBA_GRAIN`-rounded window for a nonzero `rel_off`, never
//!     the `ceil(off + len)` block PREFIX (`packed_read_bytes`).
//!  3. The wire law, property-tested over random `(off, len)`.
//!  4. The three refusals at the single choke point: an overrun window, a
//!     non-numeric `rel_off`, a non-numeric `packed_len` — `EIO`, counted on
//!     `packed_mapping_refusals`, never tenant 0's bytes.
//!  5. The fsck C7 scrub reads the tenant window of a nonzero-`rel_off`
//!     mapping (already right today — pinned).
//!  6. **FIND-PK-3** — a clone of a PROMOTED staged file takes a RAM
//!     reference on the shared base: deleting the source is nonterminal,
//!     the clone reads byte-exact, and the C8 oracle reads drift 0 on the
//!     remount.
//!  7. A stale-incarnation refusal on a mapping the fresh identity STILL
//!     binds surfaces `EIO` and trips `invariant_tripwires` — never zeros.
//!
//! Contract 1 is mount-class: it self-skips through the testkit where a
//! mount is not possible and rides the require-mount gate
//! (`tests/run_require_mount_gate.sh`). The rest run in-process.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestRunner};
use squeezefs::block_allocator::{BlockAllocator, CHUNK_SIZE};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsck::{run as run_fsck, FsckCtx, FsckOptions};
use squeezefs::fuse_client::{SqueezefsFilesystem, STATS_INODE};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_map::{decode_block_map_value, MapEntry};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata as _, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{
    block_key_with_incarnation, clean_block_key, is_whole_block_mapping, BlockMapOp, DataRouter,
    LayoutFlip, LBA_GRAIN,
};
use squeezefs_testkit::{mount_supported, site};
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::{tempdir, NamedTempFile, TempDir};

const META_LEN: u64 = 256 * 1024 * 1024;
const DATA_LEN: u64 = 512 * 1024 * 1024;
/// A staged-layout file: above the one-page inline ceiling, far below
/// the 4 MiB block.
const FILE_LEN: usize = 64 * 1024;
const GRAIN: usize = LBA_GRAIN as usize;

fn ceil_grain(len: usize) -> usize {
    len.div_ceil(GRAIN) * GRAIN
}

/// Deterministic content salted by `seed` (a zeros read, a prefix read
/// or a cross-tenant mix-up is caught).
fn pattern(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (seed
                .wrapping_mul(131)
                .wrapping_add(i.wrapping_mul(7))
                .wrapping_add(i >> 8)
                % 251) as u8
                | 1
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The in-process fixture: a real v3 meta volume (the DEFAULT format's
// bits — durable references + incarnation-stamped keys engaged), a real
// file-backed data volume, a staging dir, and the FUSE layer on top.
// ---------------------------------------------------------------------------

struct H {
    fs: SqueezefsFilesystem,
    routed: Arc<RoutedMetaBackend>,
    alloc: Arc<BlockAllocator>,
    nvme: Arc<NvmeBlockDev>,
    req: Request,
    staging: TempDir,
}

fn format_meta(meta: &Path) {
    std::fs::File::create(meta)
        .expect("create meta file")
        .set_len(META_LEN)
        .expect("size meta file");
}

async fn format_v3_default(meta: &Path) {
    format_meta(meta);
    format_v3(
        meta,
        META_LEN,
        &FormatV3Options {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
}

fn data_file() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(DATA_LEN)
        .unwrap();
    f
}

/// Open the fixture on `meta` + `data`; `vol_id` is the data volume's
/// durable id (the C8 ledger's `vol_tag`), so a REOPEN on the same id is
/// a remount in the ledger's eyes. Runs the mount's ownership recovery
/// (durable seed on a non-empty ledger) exactly as `squeezefs mount` does.
async fn open(meta: &Path, data: &Path, vol_id: &str) -> H {
    // The process default block size (4 MiB): files below it are STAGED.
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(vol_id).await.unwrap());
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
    let router = DataRouter::new(dlm.clone(), cache, alloc.clone(), nvme.clone());
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let kv = KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    fs.router
        .backend_router
        .recover_durable_block_refs(&routed)
        .await
        .expect("mount-time ownership recovery");
    let req = Request {
        unique: 1,
        // SAFETY: getuid/getgid are trivially safe.
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs,
        routed,
        alloc,
        nvme,
        req,
        staging,
    }
}

impl H {
    async fn create(&self, name: &str) -> u64 {
        self.fs
            .create(self.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create")
            .attr
            .ino
    }

    async fn write_at(&self, ino: u64, off: u64, data: &[u8]) {
        let w = self
            .fs
            .write(
                self.req,
                ino,
                0,
                off,
                bytes::Bytes::copy_from_slice(data),
                0,
                0,
            )
            .await
            .expect("write");
        assert_eq!(w.written as usize, data.len(), "short write at {off}");
    }

    async fn read(&self, ino: u64, off: u64, len: usize) -> Result<Vec<u8>, i32> {
        self.fs
            .read(self.req, ino, 0, off, len as u32, 0)
            .await
            .map(|r| r.data.to_vec())
            .map_err(i32::from)
    }

    /// Whole-file read in 1 MiB pieces (the kernel's max_read shape).
    async fn read_all(&self, ino: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let want = (len - out.len()).min(1 << 20);
            let piece = self
                .read(ino, out.len() as u64, want)
                .await
                .unwrap_or_else(|e| panic!("read ino {ino} at {}: errno {e}", out.len()));
            assert!(!piece.is_empty(), "short read at {}", out.len());
            out.extend_from_slice(&piece);
        }
        out
    }

    async fn stats(&self) -> serde_json::Value {
        let raw = self
            .fs
            .read(self.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read .stats")
            .data;
        serde_json::from_slice(&raw).expect(".stats is JSON")
    }

    async fn metric(&self, name: &str) -> u64 {
        self.stats().await["metrics"][name]
            .as_u64()
            .unwrap_or_else(|| panic!("stats metric `{name}` exported"))
    }

    /// A BEFORE snapshot of a gauge this PR introduces: absent reads as 0,
    /// so the contract's substantive assertion is what fails red, and the
    /// strict AFTER read pins the export.
    async fn metric_or_zero(&self, name: &str) -> u64 {
        self.stats().await["metrics"][name].as_u64().unwrap_or(0)
    }

    /// The CURRENT durable layout's `block_map[0]` mapping string.
    async fn mapping0(&self, ino: u64) -> String {
        self.fs
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .expect("fetch metadata")
            .block_map
            .as_ref()
            .and_then(|m| m.get(&0).cloned())
            .expect("block_map[0] is bound")
    }

    /// Write a staged-layout file and PROMOTE it through the fsync lever
    /// (`SQUEEZEFS_FSYNC_PROMOTE_STAGED` — the merge worker's and the
    /// dismount pass's own primitive, `promote_staged_file`). Returns the
    /// promoted mapping (`bk@inc:0:len`).
    async fn staged_promoted(&self, name: &str, content: &[u8]) -> (u64, String) {
        squeezefs::fsync_economy::test_set_promote_staged(Some(true));
        let before = self.metric("fsync_promoted_files").await;
        let ino = self.create(name).await;
        self.write_at(ino, 0, content).await;
        self.fs
            .fsync(self.req, ino, 0, false)
            .await
            .expect("fsync promotes");
        assert_eq!(
            self.metric("fsync_promoted_files").await,
            before + 1,
            "premise: the fsync lever promoted {name}"
        );
        let mapping = self.mapping0(ino).await;
        let (_, off, sz, exact) = self
            .fs
            .router
            .parse_block_mapping(&mapping)
            .expect("promoted mapping decodes");
        assert!(
            exact && off == 0,
            "today's promotion publishes bk:0:len ({mapping})"
        );
        assert_eq!(sz, content.len(), "passthrough image length ({mapping})");
        (ino, mapping)
    }

    /// The base offset a (decorated, stamped) mapping names.
    fn offset_of(&self, mapping: &str) -> u64 {
        self.fs
            .router
            .backend_router
            .parse_block_offset(&clean_block_key(mapping))
            .expect("mapping base offset")
    }

    /// The default slot's persisted key for `offset` (stamped when the
    /// meta volume engages incarnation keys — the default format's bit 13).
    fn key_for(&self, offset: u64) -> String {
        self.fs
            .router
            .backend_router
            .persist_block_key("backend_0", offset)
    }

    /// Allocate + publish one block; lay `head` at its start.
    async fn block_with_head(&self, head: &[u8]) -> u64 {
        let base = self.alloc.allocate_block().await.expect("allocate");
        self.nvme
            .write_block(base, bytes::Bytes::copy_from_slice(head))
            .await
            .expect("write head");
        self.alloc.publish_block(base);
        base
    }

    async fn shutdown(self) {
        for vol in &self.routed.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

/// Production unlink order (`fuse_client::reclaim_orphaned_batch`): the
/// dentry commit, then `delete_file` (the frees), then the batched destroy.
async fn unlink_and_reclaim(h: &H, ino: u64, name: &str) {
    h.routed.unlink(1, name).await.expect("unlink");
    h.fs.router
        .delete_file(&squeezefs::keys::inode_path(ino))
        .await
        .expect("delete_file");
    h.routed.destroy_inodes(&[ino]).await.expect("destroy");
}

// ---------------------------------------------------------------------------
// 3. The wire law (design §7, KD-7): the value range of `rel_off` widens
//    from {0} to every LBA_GRAIN multiple inside the block, and every
//    consumer of the form already does the right thing at every value.
// ---------------------------------------------------------------------------

/// Random `(off, len)` inside the block — `off` a grain multiple,
/// `off + ceil(len) ≤ CHUNK_SIZE` — on each of the three base-key shapes
/// (bare default-slot, named-backend, incarnation-stamped):
/// `parse_block_mapping` returns them exactly with `exact == true`,
/// `clean_block_key` returns the base, `is_whole_block_mapping` is false,
/// `decoration_suffix` is `:off:len`, and the kvmap entry is a STRING
/// record that round-trips byte-identically.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_law_holds_at_every_off_len_inside_the_block() {
    let meta = NamedTempFile::new().unwrap();
    format_v3_default(meta.path()).await;
    let data = data_file();
    let h = open(meta.path(), data.path(), "vol-00000000000000e1").await;
    let router = &h.fs.router;

    let slots = CHUNK_SIZE / LBA_GRAIN;
    let strategy = (0..slots, 0u64..CHUNK_SIZE, 0u8..3, 1u64..(1u64 << 40)).prop_map(
        |(slot, len_seed, kind, inc)| {
            let off = slot * LBA_GRAIN;
            // `CHUNK − off` is a grain multiple, so `len ≤ CHUNK − off`
            // keeps `off + ceil(len)` inside the block.
            let len = 1 + (len_seed % (CHUNK_SIZE - off));
            (off, len as usize, kind, inc)
        },
    );
    let mut runner = TestRunner::new(ProptestConfig {
        cases: 512,
        ..ProptestConfig::default()
    });
    runner
        .run(&strategy, |(off, len, kind, inc)| {
            let offset = 7 * CHUNK_SIZE;
            let base = match kind {
                0 => offset.to_string(),
                1 => format!("vol-00000000000000ab://{offset}"),
                _ => block_key_with_incarnation(&offset.to_string(), inc),
            };
            let mapping = format!("{base}:{off}:{len}");

            let (bk, got_off, got_len, exact) = router
                .parse_block_mapping(&mapping)
                .map_err(|e| TestCaseError::fail(format!("{mapping}: parse refused: {e}")))?;
            prop_assert_eq!(bk, offset, "{}: base offset", mapping);
            prop_assert_eq!(got_off, off, "{}: rel_off", mapping);
            prop_assert_eq!(got_len, len, "{}: packed_len", mapping);
            prop_assert!(exact, "{}: the size-carrying form is exact", mapping);
            prop_assert_eq!(
                clean_block_key(&mapping),
                base.clone(),
                "{}: base key",
                mapping
            );
            prop_assert!(
                !is_whole_block_mapping(&mapping),
                "{}: a decorated mapping is never whole-block (every in-place arm refuses it)",
                mapping
            );
            let suffix = format!(":{off}:{len}");
            prop_assert_eq!(
                squeezefs::jobs::decoration_suffix(&mapping, &base),
                suffix.as_str(),
                "{}: the mover re-attaches this suffix verbatim",
                mapping
            );
            let entry = router.backend_router.block_key_map_entry(&mapping);
            prop_assert_eq!(
                &entry,
                &MapEntry::String(mapping.clone().into_bytes()),
                "{}: rides the kvmap tree as a STRING record verbatim",
                mapping
            );
            let decoded = decode_block_map_value(&entry.encode())
                .map_err(|e| TestCaseError::fail(format!("{mapping}: STRING decode: {e:?}")))?;
            prop_assert_eq!(decoded, entry, "{}: STRING round trip", mapping);
            Ok(())
        })
        .expect("the wire law holds at every (off, len) inside the block");
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. The refusals (design §5.1): a read must never run past its block
//    into a neighbour's, and a malformed decoration must never resolve
//    to a DIFFERENT tenant's bytes.
// ---------------------------------------------------------------------------

/// An overrun window (`off + ceil(len) > CHUNK`), a non-numeric `rel_off`
/// (today `unwrap_or(0)` — tenant 0's window served as this tenant's), a
/// non-numeric `packed_len` (today a whole-block inexact read) and a
/// grain-misaligned `off` each answer `EIO` at the single choke point and
/// count `packed_mapping_refusals`; through the device-fetch funnel the
/// malformed decorations refuse instead of serving another window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overrun_and_malformed_decorations_refuse_eio_never_another_tenants_bytes() {
    let meta = NamedTempFile::new().unwrap();
    format_v3_default(meta.path()).await;
    let data = data_file();
    let h = open(meta.path(), data.path(), "vol-00000000000000e2").await;

    // Tenant 0's bytes at the block start, a second tenant behind it.
    let head = pattern(1, GRAIN);
    let base = h.block_with_head(&head).await;
    h.nvme
        .write_block(base + LBA_GRAIN, bytes::Bytes::from(pattern(2, GRAIN)))
        .await
        .expect("write tenant 1");
    let key = h.key_for(base);

    let refusals_before = h.metric_or_zero("packed_mapping_refusals").await;
    let cases: [(String, &str); 4] = [
        (
            format!("{key}:{}:{}", CHUNK_SIZE - LBA_GRAIN, GRAIN + 1),
            "overrun: off + ceil(len) reaches the neighbouring block",
        ),
        (format!("{key}:abc:{GRAIN}"), "non-numeric rel_off"),
        (format!("{key}:{LBA_GRAIN}:abc"), "non-numeric packed_len"),
        (
            format!("{key}:{}:{GRAIN}", LBA_GRAIN / 2),
            "grain-misaligned rel_off",
        ),
    ];
    for (mapping, what) in &cases {
        match h.fs.router.parse_block_mapping(mapping) {
            Err(e) => assert_eq!(
                e.to_errno(),
                libc::EIO,
                "{what} ({mapping}) must refuse EIO at the choke point, got {e}"
            ),
            Ok(decoded) => panic!("{what} ({mapping}) decoded to {decoded:?} instead of refusing"),
        }
    }
    assert_eq!(
        h.metric("packed_mapping_refusals").await,
        refusals_before + cases.len() as u64,
        "every refusal is counted (the must-stay-0 tripwire)"
    );

    // The device-fetch funnel: the malformed rel_off must not serve tenant
    // 0's window, and the malformed len must not fall back to a whole-block
    // read.
    for (mapping, what) in &cases[1..3] {
        match h.fs.router.read_nvme_block(mapping).await {
            Err(e) => assert_eq!(e.to_errno(), libc::EIO, "{what}: EIO through the funnel"),
            Ok(bytes) => panic!(
                "{what} ({mapping}) served {} bytes through the device-fetch funnel{} — a \
                 malformed decoration resolved to another window",
                bytes.len(),
                if bytes[..] == head[..] {
                    " (tenant 0's bytes!)"
                } else {
                    ""
                }
            ),
        }
    }
    // The whole-block and the well-formed decorated forms keep decoding.
    let (bk, off, len, exact) =
        h.fs.router
            .parse_block_mapping(&key)
            .expect("bare key decodes");
    assert!(bk == base && off == 0 && !exact && len == CHUNK_SIZE as usize);
    let (bk, off, len, exact) =
        h.fs.router
            .parse_block_mapping(&format!("{key}:{LBA_GRAIN}:{GRAIN}"))
            .expect("well-formed tenant decodes");
    assert!(bk == base && off == LBA_GRAIN && exact && len == GRAIN);
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. FIND-PK-1: the device-fetch funnel's exact arm read `ceil(off + len)`
//    from the block START — a 4 MiB read for a 4 KiB tail tenant.
// ---------------------------------------------------------------------------

/// A tenant at the block's TAIL reads exactly its own `LBA_GRAIN`-rounded
/// window: `packed_read_bytes` grows by `ceil(len)`, never by
/// `ceil(off + len)`, and the bytes are the tenant's (not the head's).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tail_tenant_reads_its_own_window_not_the_block_prefix() {
    let meta = NamedTempFile::new().unwrap();
    format_v3_default(meta.path()).await;
    let data = data_file();
    let h = open(meta.path(), data.path(), "vol-00000000000000e3").await;

    let head = pattern(11, GRAIN);
    let base = h.block_with_head(&head).await;
    let key = h.key_for(base);
    // Two tail tenants: one exactly one grain, one an unaligned 5000 B
    // image (its slot is two grains).
    let t1_off = CHUNK_SIZE - 2 * LBA_GRAIN;
    let t1 = pattern(12, GRAIN);
    h.nvme
        .write_block(base + t1_off, bytes::Bytes::from(t1.clone()))
        .await
        .expect("write tenant 1");
    let t2_off = CHUNK_SIZE - 6 * LBA_GRAIN;
    let t2 = pattern(13, 5000);
    h.nvme
        .write_block(base + t2_off, bytes::Bytes::from(t2.clone()))
        .await
        .expect("write tenant 2");

    let reads0 = h.metric_or_zero("packed_reads").await;
    let bytes0 = h.metric_or_zero("packed_read_bytes").await;
    let got =
        h.fs.router
            .read_nvme_block(&format!("{key}:{t1_off}:{GRAIN}"))
            .await
            .expect("tenant 1 read");
    assert_eq!(got[..], t1[..], "tenant 1 reads its own bytes");
    assert_eq!(h.metric("packed_reads").await, reads0 + 1);
    let win1 = h.metric("packed_read_bytes").await - bytes0;
    assert_eq!(
        win1,
        GRAIN as u64,
        "FIND-PK-1: a one-grain tail tenant is ONE grain of device bytes, not the \
         {}-byte block prefix",
        t1_off + LBA_GRAIN
    );

    let got =
        h.fs.router
            .read_nvme_block(&format!("{key}:{t2_off}:{}", t2.len()))
            .await
            .expect("tenant 2 read");
    assert_eq!(
        got[..],
        t2[..],
        "tenant 2 reads exactly its 5000 image bytes"
    );
    assert_eq!(h.metric("packed_reads").await, reads0 + 2);
    assert_eq!(
        h.metric("packed_read_bytes").await - bytes0 - win1,
        ceil_grain(t2.len()) as u64,
        "the window is the image rounded to the grain"
    );
    // The head is untouched by either read and still reads through the
    // rel_off == 0 shape every promotion publishes today.
    let got =
        h.fs.router
            .read_nvme_block(&format!("{key}:0:{GRAIN}"))
            .await
            .expect("head read");
    assert_eq!(got[..], head[..]);
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5. The C7 scrub already reads `(base + rel, ceil(len))` through the
//    ROUTED backend — the one funnel the design copies. Pinned at a
//    nonzero rel_off.
// ---------------------------------------------------------------------------

/// A striped layout carrying a nonzero-`rel_off` mapping at block 0: the
/// scrub's device read starts at `base + rel_off` (the read-fault seam
/// observes the offset) and scans exactly the tenant's `len` bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_c7_scrub_reads_the_tenant_window_of_a_nonzero_rel_off_mapping() {
    let meta = NamedTempFile::new().unwrap();
    format_v3_default(meta.path()).await;
    let data = data_file();
    let h = open(meta.path(), data.path(), "vol-00000000000000e5").await;

    let base = h.block_with_head(&pattern(21, GRAIN)).await;
    let off = 3 * LBA_GRAIN;
    let image = pattern(22, 9000);
    h.nvme
        .write_block(base + off, bytes::Bytes::from(image.clone()))
        .await
        .expect("write tenant");
    let mapping = format!("{}:{off}:{}", h.key_for(base), image.len());
    let ino = h.create("tenant.bin").await;
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);
    h.fs.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(0, mapping.clone())]),
            image.len() as u64,
            LayoutFlip::ToStripedKeepStagedIdentity,
            token,
        )
        .await
        .expect("publish the tenant mapping at block 0");
    assert_eq!(
        h.mapping0(ino).await,
        mapping,
        "the layout carries it verbatim"
    );

    let seen: Arc<parking_lot::Mutex<Vec<u64>>> = Arc::new(parking_lot::Mutex::new(Vec::new()));
    {
        let seen = seen.clone();
        squeezefs::fsck::set_scrub_read_fault_hook(Arc::new(move |read_off| {
            seen.lock().push(read_off);
            false
        }));
    }
    let ctx = FsckCtx {
        meta: h.routed.clone(),
        router: h.fs.router.clone(),
        staging_dirs: vec![h.staging.path().to_path_buf()],
        expected_generation: None,
    };
    let mut opts = FsckOptions::online();
    opts.settle = Duration::from_millis(100);
    opts.scrub = true;
    let report = run_fsck(&ctx, &opts).await.expect("fsck scrub");
    squeezefs::fsck::clear_scrub_read_fault_hook();
    assert!(
        report.findings.is_empty(),
        "a healthy tenant scrubs clean: {:?}",
        report.findings
    );
    assert_eq!(
        seen.lock().as_slice(),
        &[base + off],
        "the scrub's device read starts at the TENANT window, not the block start"
    );
    assert_eq!(
        report.counters.scrub_bytes_scanned,
        image.len() as u64,
        "the scrub verifies exactly the tenant's image bytes"
    );
    assert_eq!(report.counters.scrub_readability_only, 1);
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// 6. FIND-PK-3: the staged clone arm shares a PROMOTED source's mapping
//    durably (+1 C8 record) but took no RAM reference — the RAM count read
//    1 for a block two inos reference, so deleting the source freed it
//    under the live clone.
// ---------------------------------------------------------------------------

/// Clone a promoted staged file (`copy_file_range` whole-file → the clone
/// fast path), delete the source, read the clone byte-exact — the block's
/// RAM refcount read 2 and the source's free was NONTERMINAL — then reopen
/// with the oracle: drift 0, refcount 1, the clone still byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_of_a_promoted_staged_file_pins_the_shared_block() {
    let meta = NamedTempFile::new().unwrap();
    format_v3_default(meta.path()).await;
    let data = data_file();
    const VOL: &str = "vol-00000000000000e6";
    let h = open(meta.path(), data.path(), VOL).await;

    // A striped anchor first, so the ledger is a MIXED population (the
    // FIND-PK-2 lesson: a suite whose whole population rides one publish
    // path proves nothing about that path's accounting).
    let anchor = h.create("anchor_striped.bin").await;
    let anchor_bytes = pattern(31, CHUNK_SIZE as usize + FILE_LEN);
    for (i, chunk) in anchor_bytes.chunks(1 << 20).enumerate() {
        h.write_at(anchor, (i as u64) << 20, chunk).await;
    }
    h.fs.fsync(h.req, anchor, 0, false)
        .await
        .expect("anchor fsync");

    let content = pattern(32, FILE_LEN);
    let (src, mapping) = h.staged_promoted("src.bin", &content).await;
    let base = h.offset_of(&mapping);
    assert_eq!(
        h.alloc.refcount(base),
        Some(1),
        "premise: the promoted block is owned once"
    );

    let dst = h.create("clone.bin").await;
    let copied =
        h.fs.copy_file_range(h.req, src, 0, 0, dst, 0, 0, FILE_LEN as u64, 0)
            .await
            .expect("copy_file_range")
            .copied;
    assert_eq!(copied, FILE_LEN as u64);
    assert_eq!(
        h.mapping0(dst).await,
        mapping,
        "premise: the clone of a PROMOTED source SHARES its mapping (two inos, one window)"
    );
    assert_eq!(
        h.alloc.refcount(base),
        Some(2),
        "FIND-PK-3: the clone must take a RAM reference on the shared base — a count of \
         1 for a block two inos reference means the source's delete frees it under the \
         live clone"
    );
    assert_eq!(
        h.read(dst, 0, FILE_LEN).await.expect("read clone"),
        content,
        "the clone reads byte-exact"
    );

    let queued0 = h.metric("block_free_reclaim_queued").await;
    unlink_and_reclaim(&h, src, "src.bin").await;
    assert_eq!(
        h.alloc.refcount(base),
        Some(1),
        "the source's free is NONTERMINAL: the clone's reference keeps the block"
    );
    assert_eq!(
        h.metric("block_free_reclaim_queued").await,
        queued0,
        "no device reclaim was queued for a block a live clone still references"
    );
    assert_eq!(
        h.read(dst, 0, FILE_LEN)
            .await
            .expect("read clone after source delete"),
        content,
        "the clone reads byte-exact after the source is gone"
    );

    // The oracle on a reopen (a remount at another mount point: fresh
    // staging root, ownership seeded from the durable ledger).
    h.shutdown().await;
    let h2 = open(meta.path(), data.path(), VOL).await;
    let drift = h2
        .fs
        .router
        .backend_router
        .verify_durable_block_refs(&h2.routed)
        .await
        .expect("oracle pass");
    assert!(
        drift.is_empty(),
        "durable-vs-derived block references must agree after the clone + delete: {drift:?}"
    );
    assert_eq!(
        h2.alloc.refcount(base),
        Some(1),
        "the remount recovers the block with exactly the clone's reference"
    );
    assert_eq!(
        h2.read(dst, 0, FILE_LEN)
            .await
            .expect("read clone on the remount"),
        content,
        "another client reads the clone byte-exact"
    );
    assert_eq!(
        h2.read_all(anchor, anchor_bytes.len()).await,
        anchor_bytes,
        "the striped anchor is intact"
    );
    h2.shutdown().await;
}

// ---------------------------------------------------------------------------
// 7. The stale-incarnation disposition inside the still-bound loops
//    (design §5.10): a refusal on a mapping the FRESH identity still binds
//    is a contradiction (a live layout naming a retired lifetime — the
//    finding-51 class) and surfaces EIO + `invariant_tripwires`, never
//    zeros and never the reissued offset's bytes.
// ---------------------------------------------------------------------------

/// Promote a staged file, then retire and REISSUE its block's offset
/// underneath the still-bound layout (the allocator's own free + a fresh
/// mint, no layout change): the read refuses `EIO`, counts one
/// incarnation refusal and one invariant tripwire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_incarnation_on_a_still_bound_mapping_refuses_eio_and_trips_the_tripwire() {
    let meta = NamedTempFile::new().unwrap();
    format_v3_default(meta.path()).await;
    let data = data_file();
    let h = open(meta.path(), data.path(), "vol-00000000000000e7").await;
    assert!(
        h.routed.volumes[0].block_key_incarnation_engaged(),
        "premise: the default format engages incarnation-stamped keys"
    );

    let content = pattern(41, FILE_LEN);
    let (ino, mapping) = h.staged_promoted("victim.bin", &content).await;
    assert_eq!(h.read(ino, 0, FILE_LEN).await.expect("read"), content);
    let base = h.offset_of(&mapping);

    // Kill the lifetime under the live layout: terminal free + reissue.
    assert!(h.alloc.begin_free(base), "premise: the free is terminal");
    h.alloc.finish_free(base);
    let reissued = h.alloc.allocate_block().await.expect("reissue");
    assert_eq!(reissued, base, "premise: the freed offset is minted again");
    h.nvme
        .write_block(base, bytes::Bytes::from(pattern(42, FILE_LEN)))
        .await
        .expect("the new owner's bytes");
    h.alloc.publish_block(base);
    assert!(
        !h.fs
            .router
            .backend_router
            .block_key_incarnation_ok(&mapping),
        "premise: the layout's mapping now names a dead lifetime"
    );

    let tripwires0 = h.metric("invariant_tripwires").await;
    let refusals0 = h.metric("block_key_incarnation_refusals").await;
    match h.read(ino, 0, FILE_LEN).await {
        Err(errno) => assert_eq!(errno, libc::EIO, "the refusal surfaces as EIO"),
        Ok(bytes) => panic!(
            "a read of a mapping naming a DEAD lifetime served {} bytes ({}) instead of \
             refusing",
            bytes.len(),
            if bytes == content {
                "the old image"
            } else if bytes.iter().all(|&b| b == 0) {
                "zeros"
            } else {
                "the reissued offset's new owner's bytes"
            }
        ),
    }
    assert_eq!(
        h.metric("block_key_incarnation_refusals").await,
        refusals0 + 1,
        "the funnel refused the stale binding"
    );
    assert_eq!(
        h.metric("invariant_tripwires").await,
        tripwires0 + 1,
        "a stale refusal on a STILL-BOUND mapping is the finding-51 contradiction: counted"
    );
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// 1. FIND-PK-0 (live mount): the promoted-staged read funnel read
//    `self.nvme_writer` — the DEFAULT device — at the mapping's offset,
//    while `allocate_placed_block` can place the promotion on ANY healthy
//    data volume.
// ---------------------------------------------------------------------------

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);
/// The live population: enough files to make a wrong-device read
/// unmissable, few enough for a per-commit suite.
const LIVE_FILES: usize = 8;

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_packwire_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

/// Format one meta + TWO data volumes with a staging dir. The first data
/// volume is the mount's DEFAULT slot (`nvme_writer`); returns the meta
/// path and the two volumes' ids.
fn format_two_volumes(base: &Path, staging: &Path) -> (PathBuf, String, String) {
    let meta = base.join("meta.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(META_LEN)
        .expect("size meta file");
    let mut data = Vec::new();
    for name in ["data_a.bin", "data_b.bin"] {
        let p = base.join(name);
        std::fs::File::create(&p)
            .expect("create data file")
            .set_len(1024 * 1024 * 1024)
            .expect("size data file");
        data.push(p.display().to_string());
    }
    std::fs::create_dir_all(staging).expect("create staging dir");
    let out: Output = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.join(",")))
        .arg("--disk-cache-paths")
        .arg(staging)
        .arg("--force")
        .output()
        .expect("run squeezefs format");
    assert!(
        out.status.success(),
        "format failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (meta, "data_a.bin".to_string(), "data_b.bin".to_string())
}

/// Steer placement to the second volume: the DURABLE fail-stop health
/// override on the first (`config data-volume disable`, the offline
/// guarded path) routes every new placement away from it at mount.
fn disable_volume_offline(meta: &Path, volume_id: &str) {
    let out = Command::new(bin())
        .arg("config")
        .arg("-g")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg("data-volume")
        .arg("disable")
        .arg(volume_id)
        .output()
        .expect("run squeezefs config data-volume disable");
    assert!(
        out.status.success(),
        "offline disable of {volume_id} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Mount {
    child: Child,
    mnt: PathBuf,
    log: PathBuf,
}

impl Mount {
    fn umount_clean(&mut self) {
        let out = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .output()
            .expect("run squeezefs umount");
        let deadline = Instant::now() + DEFAULT_DISMOUNT_WAIT * 3;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait daemon") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "daemon did not exit within {:?} of `squeezefs umount`; log:\n{}",
                    DEFAULT_DISMOUNT_WAIT * 3,
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.success(),
            "daemon exited {status} on unmount; umount said:\n{}{}\nlog:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.child.try_wait() {
            return;
        }
        let _ = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + DEFAULT_DISMOUNT_WAIT * 2;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The real daemon on the staged-layout suites' posture (zc OFF, the
/// one-page inline ceiling pinned) plus `extra_env`.
fn spawn_mount(meta: &Path, mnt: &Path, log: &Path, extra_env: &[(&str, &str)]) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let child = Command::new(bin())
        .arg("mount")
        .envs(extra_env.iter().copied())
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        // SAFETY: getuid/getgid are trivially safe.
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .arg("--disk-cache-size")
        .arg("500MB")
        .env("SQUEEZEFS_FUSE_ZC", "0")
        .env("SQUEEZEFS_INLINE_MAX_BYTES", "4096")
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
    let mut mount = Mount {
        child,
        mnt: mnt.to_path_buf(),
        log: log.to_path_buf(),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            break;
        }
        if let Ok(Some(status)) = mount.child.try_wait() {
            panic!(
                "mount exited before becoming ready ({status}); log:\n{}",
                std::fs::read_to_string(log).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "mount did not become ready within 90 s; log:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    mount
}

fn live_stats(mnt: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(mnt.join(".stats")).expect("read .stats");
    serde_json::from_str(&raw).expect(".stats must be valid JSON")
}

fn live_metric(mnt: &Path, name: &str) -> u64 {
    live_stats(mnt)["metrics"][name]
        .as_u64()
        .unwrap_or_else(|| panic!("stats metric `{name}` exported"))
}

fn write_fsync(path: &Path, bytes: &[u8]) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    f.sync_all()
        .unwrap_or_else(|e| panic!("fsync {}: {e}", path.display()));
}

fn live_name(idx: usize) -> String {
    format!("promoted_{idx:02}.bin")
}

/// Read the live population back: `(wrong, of which size-consistent zeros)`.
fn read_population(mnt: &Path) -> (usize, usize) {
    let mut wrong = 0usize;
    let mut zeros = 0usize;
    for idx in 0..LIVE_FILES {
        let path = mnt.join(live_name(idx));
        let got = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let want = pattern(idx, FILE_LEN);
        if got != want {
            wrong += 1;
            if got.len() == want.len() && got.iter().all(|&b| b == 0) {
                zeros += 1;
            }
        }
    }
    (wrong, zeros)
}

/// Contract 1 — FIND-PK-0. Two data volumes; the FIRST (the default slot,
/// `DataRouter::nvme_writer`) durably disabled so every placement lands on
/// the second; N staged files promoted through the fsync lever (the ring
/// entry released, the promoted mapping `data_b.bin://off:0:len` the only
/// copy). Each file must read byte-exact from the promoting mount and —
/// the load-bearing leg — from a SECOND mount point (fresh RAM tiers, a
/// fresh staging root: every read is the ring-miss leg → the promoted
/// mapping → the device) with the C8 oracle armed. Pre-fix the funnel
/// reads the DEFAULT device at the mapping's offset: `data_a.bin`'s
/// never-written bytes — zeros.
#[test]
fn a_promoted_staged_file_placed_off_the_default_volume_reads_byte_exact() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("pk0");
    let staging = base.join("staging");
    let (meta, vol_a, vol_b) = format_two_volumes(&base, &staging);
    disable_volume_offline(&meta, &vol_a);

    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(
        &meta,
        &mnt,
        &log,
        &[("SQUEEZEFS_FSYNC_PROMOTE_STAGED", "1")],
    );
    // A striped anchor (the ledger's mixed population) — it lands on the
    // second volume too.
    let anchor = pattern(usize::MAX, CHUNK_SIZE as usize + FILE_LEN);
    write_fsync(&mnt.join("anchor_striped.bin"), &anchor);

    let promoted0 = live_metric(&mnt, "fsync_promoted_files");
    for idx in 0..LIVE_FILES {
        write_fsync(&mnt.join(live_name(idx)), &pattern(idx, FILE_LEN));
    }
    assert_eq!(
        live_metric(&mnt, "fsync_promoted_files"),
        promoted0 + LIVE_FILES as u64,
        "premise: the fsync lever promoted every file; log: {}",
        log.display()
    );
    assert_eq!(
        live_stats(&mnt)["nvme_staged_write_file_count"]
            .as_u64()
            .expect("nvme_staged_write_file_count exported"),
        0,
        "premise: every ring entry was released — the promoted mapping is the only copy"
    );
    // The promoting mount (its RAM tiers may still hold the content —
    // informational, but it must hold).
    let (wrong, _) = read_population(&mnt);
    assert_eq!(
        wrong,
        0,
        "{wrong} of {LIVE_FILES} promoted files read wrong on the mount that promoted them; \
         log: {}",
        log.display()
    );
    mount.umount_clean();

    // Another client's view — nothing but the device can serve these —
    // with the oracle armed.
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut mount2 = spawn_mount(&meta, &mnt2, &log2, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert_eq!(
        live_metric(&mnt2, "meta_kv_block_refs_drift"),
        0,
        "durable-vs-derived block references agree on the remount; log: {}",
        log2.display()
    );
    let (wrong, zeros) = read_population(&mnt2);
    assert_eq!(
        wrong,
        0,
        "FIND-PK-0: {wrong} of {LIVE_FILES} promoted files placed on {vol_b} read wrong from \
         a second mount point ({zeros} as zeros — the DEFAULT device {vol_a}'s never-written \
         bytes at the mapping's offset); log: {}",
        log2.display()
    );
    assert_eq!(live_metric(&mnt2, "staged_payload_lost_reads"), 0);
    assert_eq!(
        live_metric(&mnt2, "packed_reads"),
        LIVE_FILES as u64,
        "engagement: every promoted-file read rode the size-carrying funnel"
    );
    assert!(
        live_metric(&mnt2, "packed_read_bytes") <= (LIVE_FILES * ceil_grain(FILE_LEN)) as u64,
        "every read is one window ≤ ceil(image) (G-PK2's amplification bound)"
    );
    assert_eq!(
        std::fs::read(mnt2.join("anchor_striped.bin")).expect("read anchor"),
        anchor,
        "the striped anchor is intact"
    );
    mount2.umount_clean();
    let _ = std::fs::remove_dir_all(&base);
}
