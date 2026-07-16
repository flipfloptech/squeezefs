//! PR RW2 of docs/design-random-small-writes.md — the **W1 sole-owner
//! extent patch** red suite (§5.1: the 6-condition predicate, the
//! clone/patch `fence(SeqCst)` protocol, the mechanism, the exclusions).
//!
//! Contracts under test:
//!
//! - **Byte-exactness on a freshly-created striped file** — the designated
//!   Issue-19 polarity tripwire: fresh striped fixtures map in the
//!   undecorated 2-part whole-block form, `is_whole_block_mapping()` must
//!   classify them ELIGIBLE, and every aligned in-block small overwrite
//!   must ride the patch (`patch_writes == ops`, zero seed reads, zero
//!   staging, zero meta) while staying byte-exact hot, cold, and durable.
//!   A predicate keyed on `exact == true` patches NOTHING — this suite
//!   fails on its first run.
//! - **`clone_cfr_vs_patch_storm`** — the §5.1 Blocker fence, both
//!   interleaving orders: clone-pins-first ⇒ the patch observes
//!   refcount 2 after its unstable-mark and falls back to CoW
//!   (`patch_ineligible_shared`); patch-unstable-first ⇒ the clone's
//!   validate-after-pin observes instability and unpins/retries (bounded
//!   `attempt >= 3` loud refusal accepted). A COMPLETED clone never
//!   references a block that mutates afterward: full byte-audit of both
//!   files post-race and post-remount; never a hang, never corruption.
//! - **The exclusion matrix** (decision ledger `patch_ineligible_*`):
//!   decorated mapping / unaligned offset or len / refcount > 1 /
//!   RAM+staged overlay / compressed-encrypted / hole / extending /
//!   stream-adjacent — each falls back to today's accumulation path and
//!   counts exactly its bucket, byte-exact.
//! - **Crash blast radius (kill-9-equivalent two-session audits)**: only
//!   app-written sectors are ever rewritten — foreign bytes are never
//!   perturbed by a patch, in the ACKed-unfsynced crash AND the failed-DMA
//!   crash (the aligned-only v1 contract; the item-B
//!   `crash_inside_window_leaves_old_block_intact` guarantee, patched
//!   shapes included).
//! - **Read-mid-patch coherence**: concurrent reads never see torn state
//!   (per-LBA old/new is POSIX-legal mid-race; every serve is a clean
//!   prefix of some legal image) and post-ACK reads never serve a stale
//!   tier (binding-validated fills + purge-before-ACK).
//! - **EIO path**: a failed patch DMA fails exactly this write —
//!   `publish_block` re-stabilizes, tiers are purged, old bytes intact,
//!   the next patch succeeds.
//! - **WRITE_VERIFICATION** covers exactly the patched window for free
//!   (`nvme_dev::write_block`'s window-exact read-back).
//! - **CLI-clone live-writer refusal** — the D0 writer guard forbids the
//!   offline `squeezefs clone` verb against a live write mount: enforced,
//!   loud, tested (no longer an assertion in prose).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{set_patch_max_bytes, SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::routing::{block_mapping_form, is_whole_block_mapping, DataRouter};
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Sandbox block size: the patch anatomy is block-size-relative (64 KiB
/// here, 4 MiB on the scoreboard — same predicate, same DMA shape; every
/// offset/length in this suite speaks in the 4096-byte LBA quantum of
/// §5.1 predicate 5).
const BS: u64 = 64 * 1024;

/// Process-global METRICS deltas: serialize tests (the
/// rand_write_amp_tests pattern; the cargo gate runs `--test-threads=1`,
/// this makes the suite order-robust on its own too).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn open_fs(
    tag: &str,
    meta_path: &std::path::Path,
    backing_path: &std::path::Path,
    staging: &std::path::Path,
) -> SqueezefsFilesystem {
    let dlm = DlmClient::new("local").unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing_path.to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), tag)
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
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let be = KvMetaBackend::open(meta_path).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    // v3 refcount recovery, exactly like a real remount (main.rs mount
    // path): the RAM-authoritative refcounts predicate 4 reads.
    for kv in &routed.volumes {
        ba.recover_active_blocks_v3(kv, &fs.router.backend_router)
            .await
            .expect("v3 refcount recovery");
    }
    fs
}

async fn format_meta(path: &std::path::Path, uuid: [u8; 16]) {
    path.metadata().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid,
    })
    .unwrap()
    .build(path, 128 * 1024 * 1024)
    .await
    .unwrap();
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    // Every test pins the knob explicitly (the §6 A/B lever must never
    // leak across tests in this shared-process binary).
    set_patch_max_bytes(512 * 1024);
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    format_meta(m.path(), uuid).await;
    let s = tempdir().unwrap();
    let fs = open_fs(alloc_ns, m.path(), b.path(), s.path()).await;
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

async fn try_write_at(h: &H, ino: u64, off: u64, data: &[u8]) -> Result<u32, fuse3::Errno> {
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
    .map(|w| w.written)
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let written = try_write_at(h, ino, off, data)
        .await
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 249) as u8) ^ tag | 1).collect()
}

fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        panic!(
            "{what}: first mismatch at byte {i}: got {:#04x} want {:#04x}",
            got[i], want[i]
        );
    }
}

/// A durable, cold, freshly-striped fixture: `blocks` × [`BS`] written in
/// one pass, fsynced (map published), all read tiers purged. Every block
/// maps in the undecorated whole-block form — the W1-eligible population.
async fn durable_striped(h: &H, name: &str, blocks: u64, tag: u8) -> (u64, Vec<u8>) {
    let ino = create(h, name).await;
    let base = pattern((blocks * BS) as usize, tag);
    write_at(h, ino, 0, &base).await;
    fsync(h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    purge_read_tiers(h, ino).await;
    (ino, base)
}

/// Drop every RAM/NVMe read tier for `ino` so later reads are device-honest.
async fn purge_read_tiers(h: &H, ino: u64) {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.cache.write_lru.remove(&path);
    h.fs.router.cache.read_lru.remove(&path);
    if let Ok(m) = h.fs.router.fetch_metadata(&path).await {
        if let Some(bm) = m.block_map.as_ref() {
            for bk in bm.values() {
                h.fs.router.cache.purge_block_key(bk);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Counter snapshots
// ---------------------------------------------------------------------------

/// The W1 decision ledger (§5.4) + the device-byte ledger legs the patch
/// must keep at ZERO on the eligible shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Snap {
    patch_writes: u64,
    patch_write_bytes: u64,
    patch_edge_rmw_reads: u64,
    unmapped: u64,
    decorated: u64,
    unaligned: u64,
    overlay: u64,
    shared: u64,
    transform: u64,
    adjacent: u64,
    oversize: u64,
    dma_errors: u64,
    // Device-byte legs (RW1 always-on ledger).
    seed_read_bytes: u64,
    staging_put_bytes: u64,
    durable_upload_bytes: u64,
    write_through_bytes: u64,
    writeback_enqueued: u64,
    get_obj: u64,
    put_obj: u64,
}

fn snap() -> Snap {
    let l = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    Snap {
        patch_writes: l(&METRICS.patch_writes),
        patch_write_bytes: l(&METRICS.patch_write_bytes),
        patch_edge_rmw_reads: l(&METRICS.patch_edge_rmw_reads),
        unmapped: l(&METRICS.patch_ineligible_unmapped),
        decorated: l(&METRICS.patch_ineligible_decorated),
        unaligned: l(&METRICS.patch_ineligible_unaligned),
        overlay: l(&METRICS.patch_ineligible_overlay),
        shared: l(&METRICS.patch_ineligible_shared),
        transform: l(&METRICS.patch_ineligible_transform),
        adjacent: l(&METRICS.patch_ineligible_adjacent),
        oversize: l(&METRICS.patch_ineligible_oversize),
        dma_errors: l(&METRICS.patch_dma_errors),
        seed_read_bytes: l(&METRICS.spill_seed_read_bytes)
            + l(&METRICS.flush_seed_read_bytes)
            + l(&METRICS.write_path_seed_read_bytes),
        staging_put_bytes: l(&METRICS.spill_staging_put_bytes)
            + l(&METRICS.staging_put_bytes_drain)
            + l(&METRICS.staging_put_bytes_flush)
            + l(&METRICS.staging_put_bytes_teardown)
            + l(&METRICS.staging_put_bytes_wt_fallback),
        durable_upload_bytes: l(&METRICS.durable_upload_bytes_writeback)
            + l(&METRICS.durable_upload_bytes_self_flush)
            + l(&METRICS.durable_upload_bytes_escalation),
        write_through_bytes: l(&METRICS.write_through_bytes),
        writeback_enqueued: l(&METRICS.writeback_enqueued_drain)
            + l(&METRICS.writeback_enqueued_flush)
            + l(&METRICS.writeback_enqueued_teardown)
            + l(&METRICS.writeback_enqueued_wt_fallback),
        get_obj: l(&METRICS.get_obj),
        put_obj: l(&METRICS.put_obj),
    }
}

macro_rules! delta {
    ($after:expr, $before:expr, $field:ident) => {
        $after.$field - $before.$field
    };
}

// ---------------------------------------------------------------------------
// Predicate-1 source: is_whole_block_mapping (Issue 19 polarity)
// ---------------------------------------------------------------------------

/// The single predicate-1 source classifies the undecorated 2-part
/// whole-block form ELIGIBLE and everything else ineligible — never raw
/// `exact`-flag polarity. Cross-checked against the rig's classifier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn is_whole_block_mapping_truth_table() {
    // Eligible: persist_block_key's bare offset strings.
    for m in ["0", "12345", "4194304", "be1://12345", "oss2://0"] {
        assert!(
            is_whole_block_mapping(m),
            "{m:?}: the undecorated 2-part whole-block form IS the eligible \
             population (Issue-19 polarity: keying on `exact == true` would \
             patch NOTHING)"
        );
        assert_eq!(block_mapping_form(m), "undecorated-2part");
    }
    // Ineligible: decorated size-carrying + malformed.
    for m in ["12345:0:65536", "be1://12345:4096:512", "a:b", ""] {
        assert!(
            !is_whole_block_mapping(m),
            "{m:?}: decorated/malformed forms are patch-INELIGIBLE"
        );
    }
}

// ---------------------------------------------------------------------------
// Byte-exactness on a freshly-created striped file (the polarity tripwire)
// ---------------------------------------------------------------------------

/// THE first-run catcher: on a fresh striped file, every LBA-aligned
/// in-block small overwrite patches in place — `patch_writes == ops`,
/// `patch_write_bytes == user bytes`, ZERO seed reads, ZERO staging, ZERO
/// writeback, ZERO meta journal entries, `patch_edge_rmw_reads == 0` —
/// and the file stays byte-exact hot, cold, and after fsync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_byte_exactness_on_fresh_striped_file() {
    let _g = serial().await;
    let h = make(*b"rw2-bytes-000001", "rw2_ns_bytes").await;
    let (ino, mut want) = durable_striped(&h, "bytes.dat", 8, 0x00).await;
    let path = squeezefs::keys::inode_path(ino);

    // Premise (Issue-19 tripwire): the fixture maps whole-block undecorated
    // and the single predicate source classifies it ELIGIBLE.
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    let bm = m.block_map.as_ref().expect("striped fixture block map");
    for (b, mapping) in bm.iter() {
        assert!(
            is_whole_block_mapping(mapping),
            "block {b} mapping {mapping:?} (form {}) must be W1-ELIGIBLE — \
             a fresh striped file is the loss-shape population itself",
            block_mapping_form(mapping)
        );
    }
    let journal_entries_0 = meta_journal_entries(&h);

    // Aligned, non-adjacent overwrites: block starts, interiors, tails —
    // 4 KiB and 8 KiB shapes (all multiples of the LBA quantum).
    let writes: &[(u64, usize, u8)] = &[
        (0, 4096, 0xA1),                    // block 0 start
        (2 * BS + 8192, 4096, 0xA2),        // block 2 interior
        (5 * BS + (BS - 4096), 4096, 0xA3), // block 5 tail
        (BS + 16384, 8192, 0xA4),           // block 1, 8 KiB
        (7 * BS + 4096, 4096, 0xA5),        // block 7 near-start
        (3 * BS + 32768, 16384, 0xA6),      // block 3, 16 KiB
    ];
    let before = snap();
    let mut user_bytes = 0u64;
    for &(off, len, tag) in writes {
        let p = pattern(len, tag);
        write_at(&h, ino, off, &p).await;
        want[off as usize..off as usize + len].copy_from_slice(&p);
        user_bytes += len as u64;
    }
    let d_after = snap();

    // The decision ledger: every op patched; nothing fell back.
    assert_eq!(
        delta!(d_after, before, patch_writes),
        writes.len() as u64,
        "every aligned in-block overwrite of a fresh striped file must ride \
         the W1 patch (patch_writes == ops) — zero means the predicate is \
         inverted (Issue-19) or the path does not exist"
    );
    assert_eq!(
        delta!(d_after, before, patch_write_bytes),
        user_bytes,
        "patch_write_bytes must equal delivered user bytes"
    );
    assert_eq!(
        delta!(d_after, before, patch_edge_rmw_reads),
        0,
        "v1 is aligned-only: patch_edge_rmw_reads must stay 0 (G-RW2 clause)"
    );
    for (name, v) in [
        ("unmapped", delta!(d_after, before, unmapped)),
        ("decorated", delta!(d_after, before, decorated)),
        ("unaligned", delta!(d_after, before, unaligned)),
        ("overlay", delta!(d_after, before, overlay)),
        ("shared", delta!(d_after, before, shared)),
        ("transform", delta!(d_after, before, transform)),
        ("adjacent", delta!(d_after, before, adjacent)),
        ("oversize", delta!(d_after, before, oversize)),
    ] {
        assert_eq!(
            v, 0,
            "patch_ineligible_{name} must not fire on the eligible shape"
        );
    }

    // The device-cost contract: ONE aligned DMA per op — zero reads, zero
    // staging, zero writeback, zero whole-block uploads, zero meta.
    assert_eq!(
        delta!(d_after, before, get_obj),
        0,
        "zero device READS per patch"
    );
    assert_eq!(
        delta!(d_after, before, put_obj),
        writes.len() as u64,
        "exactly one device write (the patch DMA) per op"
    );
    assert_eq!(
        delta!(d_after, before, seed_read_bytes),
        0,
        "zero RMW seed reads"
    );
    assert_eq!(
        delta!(d_after, before, staging_put_bytes),
        0,
        "zero staging puts"
    );
    assert_eq!(
        delta!(d_after, before, durable_upload_bytes),
        0,
        "zero CoW uploads"
    );
    assert_eq!(
        delta!(d_after, before, write_through_bytes),
        0,
        "zero write-through"
    );
    assert_eq!(
        delta!(d_after, before, writeback_enqueued),
        0,
        "zero writeback units"
    );
    assert_eq!(
        meta_journal_entries(&h),
        journal_entries_0,
        "zero meta commits: the patch changes no map, no size, no layout"
    );

    // Read-your-writes, hot.
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "hot read-back after patches");

    // Cold: purge every tier — the device itself must carry the patches.
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "cold read-back (device-honest) after patches");

    // fsync is a no-op barrier for patched blocks (device-resident before
    // ACK — nothing parked, nothing staged) and must not disturb bytes.
    fsync(&h, ino).await;
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-fsync cold read-back");
}

/// The v3 KV journal-entry counter (the stats surface's
/// `meta_kv_journal_entries` source) — the zero-meta proof.
fn meta_journal_entries(_h: &H) -> u64 {
    META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The exclusion matrix (decision ledger + fallback byte-exactness)
// ---------------------------------------------------------------------------

/// Unaligned offset or length ⇒ `patch_ineligible_unaligned`, today's
/// accumulation path, byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_unaligned_offset_or_len() {
    let _g = serial().await;
    let h = make(*b"rw2-unaligned-01", "rw2_ns_unal").await;
    let (ino, mut want) = durable_striped(&h, "unal.dat", 4, 0x10).await;

    let before = snap();
    // Misaligned offset (2 KiB into a block).
    let p1 = pattern(4096, 0xB1);
    write_at(&h, ino, BS + 2048, &p1).await;
    want[(BS + 2048) as usize..(BS + 2048) as usize + 4096].copy_from_slice(&p1);
    // Misaligned length (aligned offset, 2 KiB length).
    let p2 = pattern(2048, 0xB2);
    write_at(&h, ino, 2 * BS + 8192, &p2).await;
    want[(2 * BS + 8192) as usize..(2 * BS + 8192) as usize + 2048].copy_from_slice(&p2);
    let after = snap();

    assert_eq!(
        delta!(after, before, unaligned),
        2,
        "both shapes count unaligned"
    );
    assert_eq!(
        delta!(after, before, patch_writes),
        0,
        "no patch on unaligned shapes"
    );
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "unaligned fallback byte-exactness");
}

/// Extending writes (offset+len > i_size) ⇒ the size/window bucket
/// (`patch_ineligible_oversize` — a grown i_size owes a meta commit),
/// today's path, size grows correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_extending_write() {
    let _g = serial().await;
    let h = make(*b"rw2-extend-00001", "rw2_ns_ext").await;
    let (ino, base) = durable_striped(&h, "ext.dat", 4, 0x11).await;

    let before = snap();
    // Aligned shape, but extends the file by one LBA into block 4.
    let p = pattern(4096, 0xB3);
    write_at(&h, ino, 4 * BS, &p).await;
    let after = snap();

    assert_eq!(
        delta!(after, before, oversize),
        1,
        "extending writes land in the size/window bucket (oversize)"
    );
    assert_eq!(delta!(after, before, patch_writes), 0);
    let got = read_at(&h, ino, 0, (4 * BS) as usize + 4096).await;
    let mut want = base;
    want.extend_from_slice(&p);
    assert_bytes(&got, &want, "extending fallback byte-exactness");
    let size = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr.size;
    assert_eq!(size, 4 * BS + 4096, "the file grew");
}

/// Length past `SQUEEZEFS_PATCH_MAX_BYTES` ⇒ oversize; the knob at 0
/// disables the path entirely (the §6 A/B lever).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_oversize_len_and_knob_disable() {
    let _g = serial().await;
    let h = make(*b"rw2-oversize-001", "rw2_ns_ovr").await;
    let (ino, mut want) = durable_striped(&h, "ovr.dat", 4, 0x12).await;

    // Cap at 8 KiB: a 16 KiB aligned write is oversize.
    set_patch_max_bytes(8192);
    let before = snap();
    let p = pattern(16384, 0xB4);
    write_at(&h, ino, BS + 4096, &p).await;
    want[(BS + 4096) as usize..(BS + 4096) as usize + 16384].copy_from_slice(&p);
    let after = snap();
    assert_eq!(
        delta!(after, before, oversize),
        1,
        "len > knob counts oversize"
    );
    assert_eq!(delta!(after, before, patch_writes), 0);

    // Knob 0 = the A/B lever: the patch path is OFF, an otherwise-eligible
    // write takes today's path and counts NOTHING in the patch ledger.
    set_patch_max_bytes(0);
    let before = snap();
    let p0 = pattern(4096, 0xB5);
    write_at(&h, ino, 2 * BS + 8192, &p0).await;
    want[(2 * BS + 8192) as usize..(2 * BS + 8192) as usize + 4096].copy_from_slice(&p0);
    let after = snap();
    assert_eq!(
        delta!(after, before, patch_writes),
        0,
        "knob 0 disables the patch"
    );
    assert_eq!(
        after,
        Snap {
            // Only the accumulation-path parking is allowed to move — the
            // patch DECISION ledger must be silent when disabled.
            patch_writes: before.patch_writes,
            patch_write_bytes: before.patch_write_bytes,
            patch_edge_rmw_reads: before.patch_edge_rmw_reads,
            unmapped: before.unmapped,
            decorated: before.decorated,
            unaligned: before.unaligned,
            overlay: before.overlay,
            shared: before.shared,
            transform: before.transform,
            adjacent: before.adjacent,
            oversize: before.oversize,
            dma_errors: before.dma_errors,
            ..after
        },
        "knob 0: the whole patch decision ledger stays untouched"
    );
    set_patch_max_bytes(512 * 1024);

    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "oversize/knob-off fallback byte-exactness");
}

/// Stream-adjacency (predicate 6): `offset == last_write_end` routes to
/// accumulation so sequential 4 KiB streams keep the whole-block
/// write-through economy; a non-adjacent follow-up patches again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_stream_adjacent() {
    let _g = serial().await;
    let h = make(*b"rw2-adjacent-001", "rw2_ns_adj").await;
    let (ino, mut want) = durable_striped(&h, "adj.dat", 4, 0x13).await;

    let before = snap();
    let p1 = pattern(4096, 0xB6);
    write_at(&h, ino, BS + 8192, &p1).await; // isolated: patches
    let p2 = pattern(4096, 0xB7);
    write_at(&h, ino, BS + 12288, &p2).await; // == prev end: adjacent
    let p3 = pattern(4096, 0xB8);
    write_at(&h, ino, 3 * BS + 8192, &p3).await; // isolated again: patches
    want[(BS + 8192) as usize..(BS + 8192) as usize + 4096].copy_from_slice(&p1);
    want[(BS + 12288) as usize..(BS + 12288) as usize + 4096].copy_from_slice(&p2);
    want[(3 * BS + 8192) as usize..(3 * BS + 8192) as usize + 4096].copy_from_slice(&p3);
    let after = snap();

    assert_eq!(
        delta!(after, before, adjacent),
        1,
        "the stream-adjacent write (offset == last_write_end) must fall back"
    );
    assert_eq!(
        delta!(after, before, patch_writes),
        2,
        "the two isolated writes patch; the adjacent one accumulates"
    );
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "adjacency fallback byte-exactness");
}

/// A RAM `ActiveBlockBuf` or staged `active_block:` sibling owns the block
/// (accumulation in progress) ⇒ `patch_ineligible_overlay`, merge into it
/// (today's path), never a patch that the overlay's later flush would
/// stomp with stale bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_overlay_present_ram_and_staged() {
    let _g = serial().await;
    let h = make(*b"rw2-overlay-0001", "rw2_ns_ovl").await;
    let (ino, mut want) = durable_striped(&h, "ovl.dat", 4, 0x14).await;

    // RAM overlay: an unaligned write parks an ActiveBlockBuf for block 1.
    let p_unal = pattern(2048, 0xB9);
    write_at(&h, ino, BS + 1024, &p_unal).await;
    want[(BS + 1024) as usize..(BS + 1024) as usize + 2048].copy_from_slice(&p_unal);

    let before = snap();
    let p = pattern(4096, 0xBA);
    write_at(&h, ino, BS + 8192, &p).await; // aligned, but block 1 is owned
    want[(BS + 8192) as usize..(BS + 8192) as usize + 4096].copy_from_slice(&p);
    let after = snap();
    assert_eq!(
        delta!(after, before, overlay),
        1,
        "a parked RAM buffer owns block 1: the aligned write must merge into \
         it (patch_ineligible_overlay), never patch around it"
    );
    assert_eq!(delta!(after, before, patch_writes), 0);

    // Staged overlay: stage a whole-block image for block 3 directly (the
    // put_active_block producer used by the spill/flush paths).
    let key = squeezefs::keys::active_block(ino, 3).to_string();
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);
    let staged_img = {
        let mut img = want[(3 * BS) as usize..(4 * BS) as usize].to_vec();
        let sp = pattern(4096, 0xBB);
        img[0..4096].copy_from_slice(&sp);
        img
    };
    assert!(
        h.fs.router
            .cache
            .nvme
            .put_active_block(&key, &staged_img, token),
        "premise: staging admits the block-3 image"
    );
    assert!(
        h.fs.router.cache.nvme.has_staged_active_block(&key),
        "the lock-free staged-existence probe must see the staged sibling \
         (predicate 2's second arm — Issue 10's index, not the \
         spawn_blocking/shard-write-lock hop)"
    );
    want[(3 * BS) as usize..(4 * BS) as usize].copy_from_slice(&staged_img);

    let before = snap();
    let p = pattern(4096, 0xBC);
    write_at(&h, ino, 3 * BS + 8192, &p).await;
    want[(3 * BS + 8192) as usize..(3 * BS + 8192) as usize + 4096].copy_from_slice(&p);
    let after = snap();
    assert_eq!(
        delta!(after, before, overlay),
        1,
        "a staged active_block sibling owns block 3: the aligned write must \
         consume it via checkout (today's path), never patch the base block"
    );
    assert_eq!(delta!(after, before, patch_writes), 0);

    fsync(&h, ino).await;
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "overlay fallback byte-exactness (durable)");
}

/// Clone-shared blocks (refcount > 1) ⇒ `patch_ineligible_shared`, CoW
/// fallback; the clone's bytes never move. Also pins the read-only
/// refcount accessor RW2 adds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_shared_refcount_after_clone() {
    let _g = serial().await;
    let h = make(*b"rw2-shared-00001", "rw2_ns_shr").await;
    let (src, base) = durable_striped(&h, "shr_src.dat", 4, 0x15).await;

    // The read-only refcount accessor (predicate 4's source): sole owner.
    let path = squeezefs::keys::inode_path(src);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    let bk0 = m.block_map.as_ref().unwrap().get(&0).unwrap().clone();
    let off0 = h.fs.router.backend_router.parse_block_offset(&bk0).unwrap();
    assert_eq!(
        h.fs.router.backend_router.default_allocator.refcount(off0),
        Some(1),
        "fresh striped block must be sole-owned (read-only accessor)"
    );

    // Whole-file CFR = the in-process clone_file arm.
    let dst = create(&h, "shr_dst.dat").await;
    let copied =
        h.fs.copy_file_range(h.req, src, 0, 0, dst, 0, 0, base.len() as u64, 0)
            .await
            .expect("whole-file CFR clone")
            .copied;
    assert_eq!(copied, base.len() as u64, "clone copies the whole file");
    assert_eq!(
        h.fs.router.backend_router.default_allocator.refcount(off0),
        Some(2),
        "the clone pinned every source block (refcount 2)"
    );

    let before = snap();
    let p = pattern(4096, 0xBD);
    write_at(&h, src, 8192, &p).await; // aligned patch shape, but shared
    let after = snap();
    assert_eq!(
        delta!(after, before, shared),
        1,
        "a clone-shared block must CoW (patch_ineligible_shared), never \
         mutate in place under the clone"
    );
    assert_eq!(delta!(after, before, patch_writes), 0);

    // The clone's snapshot never moves; the source shows the write.
    // `force_flush_all_staged_data` drives the CoW to its durable merge
    // (the sandbox runs no background writeback worker — on a live mount
    // the queued unit does this), displacing the shared block.
    fsync(&h, src).await;
    h.fs.force_flush_all_staged_data()
        .await
        .expect("teardown-grade flush");
    purge_read_tiers(&h, src).await;
    purge_read_tiers(&h, dst).await;
    let got_dst = read_at(&h, dst, 0, base.len()).await;
    assert_bytes(&got_dst, &base, "clone snapshot untouched by the CoW write");
    let mut want_src = base.clone();
    want_src[8192..8192 + 4096].copy_from_slice(&p);
    let got_src = read_at(&h, src, 0, base.len()).await;
    assert_bytes(&got_src, &want_src, "source carries the CoW write");

    // The CoW displaced the shared block: the source's NEW block 0 is
    // sole-owned again, so the next aligned write patches.
    let before = snap();
    let p2 = pattern(4096, 0xBE);
    write_at(&h, src, 8192, &p2).await;
    let after = snap();
    assert_eq!(
        delta!(after, before, patch_writes),
        1,
        "post-CoW the source block is sole-owned again: patch resumes"
    );
}

/// Compressed/encrypted volumes ⇒ `patch_ineligible_transform` (a
/// transform image cannot be patched in place), today's path, byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_transform_compressed_volume() {
    let _g = serial().await;
    let h = make(*b"rw2-transform-01", "rw2_ns_xfm").await;
    // lz4 volume: every image is framed — in-place sub-block DMA is illegal.
    h.fs.router
        .set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            "lz4".to_string(),
            "none".to_string(),
            None,
        ));
    let (ino, mut want) = durable_striped(&h, "xfm.dat", 4, 0x16).await;

    let before = snap();
    let p = pattern(4096, 0xBF);
    write_at(&h, ino, BS + 8192, &p).await;
    want[(BS + 8192) as usize..(BS + 8192) as usize + 4096].copy_from_slice(&p);
    let after = snap();
    assert_eq!(
        delta!(after, before, transform),
        1,
        "non-passthrough volumes must count patch_ineligible_transform"
    );
    assert_eq!(delta!(after, before, patch_writes), 0);
    fsync(&h, ino).await;
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "transform fallback byte-exactness (durable)");
}

/// Holes (unmapped blocks inside i_size — punched or sparse) ⇒
/// `patch_ineligible_unmapped`; the write allocates through today's path
/// and the rest of the hole still reads zeros.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_hole_unmapped_block() {
    let _g = serial().await;
    let h = make(*b"rw2-hole-0000001", "rw2_ns_hol").await;
    let (ino, mut want) = durable_striped(&h, "hole.dat", 6, 0x17).await;

    // Punch block 2 whole (FALLOC_FL_PUNCH_HOLE | KEEP_SIZE): unmapped.
    h.fs.fallocate(
        h.req,
        ino,
        0,
        2 * BS,
        BS,
        (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u32,
    )
    .await
    .expect("punch whole block 2");
    for b in want[(2 * BS) as usize..(3 * BS) as usize].iter_mut() {
        *b = 0;
    }

    let before = snap();
    let p = pattern(4096, 0xC0);
    write_at(&h, ino, 2 * BS + 8192, &p).await; // aligned, inside the hole
    want[(2 * BS + 8192) as usize..(2 * BS + 8192) as usize + 4096].copy_from_slice(&p);
    let after = snap();
    assert_eq!(
        delta!(after, before, unmapped),
        1,
        "a hole (unmapped block) must count patch_ineligible_unmapped — \
         there is nothing to patch in place"
    );
    assert_eq!(delta!(after, before, patch_writes), 0);

    fsync(&h, ino).await;
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(
        &got,
        &want,
        "hole-write fallback: patch + zeros + old bytes",
    );
}

/// Decorated `bk:off:len` mappings (promoted-staged form) ⇒
/// `patch_ineligible_decorated`; the fallback path owns the decorated
/// window semantics — nothing ever scribbles outside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusion_decorated_mapping_never_scribbles_outside_window() {
    let _g = serial().await;
    let h = make(*b"rw2-decorated-01", "rw2_ns_dec").await;
    let (ino, mut want) = durable_striped(&h, "dec.dat", 4, 0x18).await;
    let path = squeezefs::keys::inode_path(ino);

    // Turn block 1's mapping into the size-carrying decorated form with
    // IDENTICAL semantics (off 0, len == BS — the exact whole-block stored
    // image, what a promoted-staged publish looks like for a full block),
    // persisted through the meta backend like any promotion would.
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    let bm = m.block_map.as_ref().unwrap();
    let decorated = format!("{}:0:{}", bm.get(&1).unwrap(), BS);
    let mut new_map: std::collections::HashMap<u32, String> =
        bm.iter().map(|(k, v)| (*k, v.clone())).collect();
    new_map.insert(1, decorated.clone());
    let layout = squeezefs::routing::LayoutMetadata {
        file_type: m.file_type.clone(),
        size: m.size,
        block_map_id: Some(format!("block_map_{ino}")),
        block_prefix: None,
        file_id: m.file_id.clone(),
        data_key: m.data_key.as_ref().map(|b| b.to_vec()),
        block_map: Some(new_map),
    };
    let backend = h.fs.meta_backend.as_ref().unwrap();
    backend
        .setxattr(ino, "layout", &bincode::serialize(&layout).unwrap())
        .await
        .unwrap();
    h.fs.router.metadata_cache.remove(&ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    let m1 = m.block_map.as_ref().unwrap().get(&1).unwrap().clone();
    assert_eq!(
        block_mapping_form(&m1),
        "decorated-3part",
        "premise: block 1 now carries the decorated promoted-staged form"
    );
    assert!(
        !is_whole_block_mapping(&m1),
        "decorated == patch-ineligible"
    );

    // The overwrite: a whole-block-covering aligned write of block 1 (at
    // 64 KiB sandbox blocks the shape is patch-eligible by size — a real
    // 4 MiB-block volume never sees a block-covering patch shape, the
    // 512 KiB cap forbids it — so the DECORATED predicate is what must
    // refuse it here). Block-complete on purpose: the fallback rides the
    // write-through (no deferred RMW seed), because materializing a
    // deferred seed under a decorated STRIPED mapping is the pre-existing
    // FIND-RW2-A limitation (`fetch_seed_image → get_block_for_index`
    // cannot decode `bk:off:len` keys — fails identically with the patch
    // knob at 0; recorded in the RW2 note, out of this PR's scope).
    let before = snap();
    let p = pattern(BS as usize, 0xC1);
    write_at(&h, ino, BS, &p).await; // aligned + complete, but DECORATED
    want[BS as usize..2 * BS as usize].copy_from_slice(&p);
    let after = snap();
    assert_eq!(
        delta!(after, before, decorated),
        1,
        "decorated mappings must count patch_ineligible_decorated — patch \
         arithmetic must never trust a decorated window (base-offset+rel \
         could scribble outside it)"
    );
    assert_eq!(delta!(after, before, patch_writes), 0);

    fsync(&h, ino).await;
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(
        &got,
        &want,
        "decorated fallback: the write landed inside the window and every \
         byte outside it is untouched",
    );
}

// ---------------------------------------------------------------------------
// clone_cfr_vs_patch_storm — the §5.1 Blocker fence, both orders
// ---------------------------------------------------------------------------

/// Concurrent whole-file-CFR clones × a same-file patch storm, both
/// interleaving orders, on PERSISTENT volumes (post-remount audit):
///
/// * order 1 (clone-pins-first): a completed pre-storm clone forces every
///   first patch of a shared block to CoW (`patch_ineligible_shared`); the
///   clone's bytes never change afterward — not during the storm, not
///   after remount.
/// * order 2 (patch-unstable-first): clones issued INTO the storm either
///   complete (their captures are immutable + every block is a legal
///   point-in-time image) or refuse loudly within the `attempt >= 3` bound
///   (the accepted bounded EBUSY-class refusal) — never hang, never
///   corrupt.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn clone_cfr_vs_patch_storm() {
    let _g = serial().await;
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    set_patch_max_bytes(512 * 1024);

    const BLOCKS: u64 = 12;
    const STORM_ROUNDS: usize = 240;
    const CLONES_IN_STORM: usize = 6;

    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path(), *b"rw2-clonestorm-1").await;
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(512 * 1024 * 1024)
        .unwrap();
    let staging = tempdir().unwrap();

    // ---- session 1: the race ----
    let (src, base, final_img, captures) = {
        let fs = open_fs("rw2_ns_storm", meta.path(), backing.path(), staging.path()).await;
        let req = Request {
            unique: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            pid: 1,
        };
        let h = Arc::new(H {
            fs,
            req,
            _b: NamedTempFile::new().unwrap(),
            _m: NamedTempFile::new().unwrap(),
            _s: tempdir().unwrap(),
        });
        let (src, base) = durable_striped(&h, "storm_src.dat", BLOCKS, 0x20).await;

        // Legal-content oracle: per block, the set of payloads ever written
        // to its patch window (offset b*BS+8192, 4 KiB). A clone may
        // capture base or any prefix state; nothing else is legal.
        let mut legal: Vec<Vec<Vec<u8>>> = (0..BLOCKS as usize)
            .map(|b| vec![base[b * BS as usize + 8192..b * BS as usize + 8192 + 4096].to_vec()])
            .collect();
        let mut written: Vec<Vec<u8>> = Vec::with_capacity(STORM_ROUNDS);

        // ORDER 1 — clone-pins-first: complete a clone BEFORE any patch.
        let dst0 = create(&h, "storm_dst0.dat").await;
        let copied =
            h.fs.copy_file_range(h.req, src, 0, 0, dst0, 0, 0, base.len() as u64, 0)
                .await
                .expect("pre-storm whole-file clone")
                .copied;
        assert_eq!(copied, base.len() as u64);
        let capture0 = read_at(&h, dst0, 0, base.len()).await;
        assert_bytes(&capture0, &base, "pre-storm clone captures base");

        // Phase A: patches against the clone-pinned map. EVERY block is
        // refcount-2, so the patch's post-unstable re-check must observe
        // the pin and fall back to CoW (`patch_ineligible_shared` on first
        // touch, then `patch_ineligible_overlay` while the CoW accumulation
        // owns the block) — never an in-place mutation under the pin.
        let before_a = snap();
        for round in 0..STORM_ROUNDS / 4 {
            let b = (round as u64 * 5 + 1) % BLOCKS;
            let p = pattern(4096, (round % 251) as u8);
            write_at(&h, src, b * BS + 8192, &p).await;
            written.push(p);
        }
        let after_a = snap();
        assert!(
            delta!(after_a, before_a, shared) > 0,
            "clone-pins-first: the first patch of a clone-pinned block must \
             observe refcount 2 AFTER its unstable-mark and fall back to CoW \
             (patch_ineligible_shared)"
        );
        assert_eq!(
            delta!(after_a, before_a, patch_writes),
            0,
            "NO write may mutate a clone-pinned block in place — the §5.1 \
             fence's whole point"
        );
        // The pinned clone is byte-identical to its capture right now.
        let now0 = read_at(&h, dst0, 0, base.len()).await;
        assert_bytes(&now0, &capture0, "pinned clone untouched by phase-A CoW");

        // Flush: CoW displaces the shared blocks (dest keeps them); src's
        // fresh blocks are sole-owned again.
        fsync(&h, src).await;

        // Phase B: the concurrent race — a live patch storm × clones
        // issued INTO it (ORDER 2: patch-unstable-first interleavings).
        let before_b = snap();
        let storm = {
            let h = h.clone();
            tokio::spawn(async move {
                let mut written_b: Vec<Vec<u8>> = Vec::with_capacity(STORM_ROUNDS);
                for round in STORM_ROUNDS / 4..STORM_ROUNDS {
                    let b = (round as u64 * 5 + 1) % BLOCKS;
                    let p = pattern(4096, (round % 251) as u8);
                    write_at(&h, src, b * BS + 8192, &p).await;
                    written_b.push(p);
                    if round % 8 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
                written_b
            })
        };

        // Clones INTO the storm: bounded-loud refusal or success, never a
        // hang (60 s watchdog per attempt), captures immutable.
        let mut captures: Vec<(u64, Vec<u8>)> = vec![(dst0, capture0)];
        for c in 0..CLONES_IN_STORM {
            let dst = create(&h, &format!("storm_dst{}.dat", c + 1)).await;
            let attempt = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                h.fs.copy_file_range(h.req, src, 0, 0, dst, 0, 0, base.len() as u64, 0),
            )
            .await
            .expect(
                "clone under patch storm HUNG (>60 s): the §5.1 disposition is \
                 bounded loud refusal or success, never a hang",
            );
            match attempt {
                Ok(r) => {
                    assert_eq!(r.copied, base.len() as u64);
                    let cap = read_at(&h, dst, 0, base.len()).await;
                    captures.push((dst, cap));
                }
                Err(errno) => {
                    // Bounded loud refusal (attempt >= 3 → EIO-class errno
                    // through the CFR handler). The CONTRACT is bounded-
                    // refusal-or-success; the refusal must be an error, not
                    // a short copy.
                    eprintln!(
                        "clone {} refused under storm (bounded EBUSY-class, \
                         accepted): errno {errno:?}",
                        c + 1
                    );
                }
            }
            tokio::task::yield_now().await;
        }

        written.extend(storm.await.expect("storm task"));
        for (round, p) in written.iter().enumerate() {
            let b = ((round as u64 * 5 + 1) % BLOCKS) as usize;
            legal[b].push(p.clone());
        }
        let after_b = snap();

        // The storm DID exercise the patch path (RED today: patch_writes
        // never moves — the path does not exist).
        assert!(
            delta!(after_b, before_b, patch_writes) > 0,
            "the phase-B storm must drive the W1 patch path on sole-owned \
             blocks (patch_writes moved 0 — the path does not exist or the \
             predicate never passes)"
        );

        // Post-race audit: every capture is immutable (the fence's whole
        // point) and every block of it is a legal point-in-time image.
        for (dst, cap) in &captures {
            let now = read_at(&h, *dst, 0, base.len()).await;
            assert_bytes(
                &now,
                cap,
                &format!(
                    "clone ino {dst} content CHANGED after completion — a \
                     completed clone referenced a block that mutated (the \
                     §5.1 fence is broken)"
                ),
            );
            for b in 0..BLOCKS as usize {
                let win = &cap[b * BS as usize + 8192..b * BS as usize + 8192 + 4096];
                assert!(
                    legal[b].iter().any(|l| l == win),
                    "clone ino {dst} block {b}: captured window is not ANY \
                     legal point-in-time image (torn/foreign bytes)"
                );
                // Outside the patch window the block must be base, always.
                let pre = &cap[b * BS as usize..b * BS as usize + 8192];
                let base_pre = &base[b * BS as usize..b * BS as usize + 8192];
                assert_bytes(pre, base_pre, &format!("dst {dst} block {b} pre-window"));
            }
        }

        // Source audit: final storm state, byte-exact.
        let mut want_src = base.clone();
        for (round, p) in written.iter().enumerate() {
            let b = (round as u64 * 5 + 1) % BLOCKS;
            want_src[(b * BS + 8192) as usize..(b * BS + 8192) as usize + 4096].copy_from_slice(p);
        }
        let got_src = read_at(&h, src, 0, base.len()).await;
        assert_bytes(&got_src, &want_src, "source == final storm state post-race");

        // Make everything durable for the remount audit.
        fsync(&h, src).await;
        for (dst, _) in &captures {
            fsync(&h, *dst).await;
        }
        (src, base, want_src, captures)
    };

    // ---- session 2: post-remount audit (same meta + backing + staging) ----
    let fs2 = open_fs("rw2_ns_storm", meta.path(), backing.path(), staging.path()).await;
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    let h2 = H {
        fs: fs2,
        req,
        _b: NamedTempFile::new().unwrap(),
        _m: NamedTempFile::new().unwrap(),
        _s: tempdir().unwrap(),
    };
    let got_src = read_at(&h2, src, 0, base.len()).await;
    assert_bytes(
        &got_src,
        &final_img,
        "post-remount: source == final storm state",
    );
    for (dst, cap) in &captures {
        let got = read_at(&h2, *dst, 0, base.len()).await;
        assert_bytes(
            &got,
            cap,
            &format!("post-remount: clone ino {dst} still byte-identical to its capture"),
        );
    }
}

// ---------------------------------------------------------------------------
// Read-mid-patch coherence
// ---------------------------------------------------------------------------

/// Concurrent reads of a block under a patch storm never see torn state:
/// every racing serve is per-LBA clean (each 4 KiB half of the 8 KiB
/// window is a whole half of SOME legal payload), and once the storm
/// quiesces every read — hot, then cold — serves exactly the final bytes
/// (binding-validated fills + purge-before-ACK: no stale tier serve).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn read_mid_patch_coherence_no_torn_or_stale_serves() {
    let _g = serial().await;
    let h = Arc::new(make(*b"rw2-readmid-0001", "rw2_ns_rmp").await);
    let (ino, base) = durable_striped(&h, "rmp.dat", 4, 0x21).await;

    const ROUNDS: usize = 160;
    let woff = 2 * BS + 16384; // block 2 interior, 8 KiB window = 2 LBAs
    let wlen = 8192usize;

    let before = snap();
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);

    // Legal halves: base's + every round's.
    let legal: Arc<Vec<Vec<u8>>> = Arc::new(
        std::iter::once(base[woff as usize..woff as usize + wlen].to_vec())
            .chain((0..ROUNDS).map(|r| pattern(wlen, (r % 250) as u8)))
            .collect(),
    );

    let patcher = {
        let h = h.clone();
        tokio::spawn(async move {
            for r in 0..ROUNDS {
                let p = pattern(wlen, (r % 250) as u8);
                write_at(&h, ino, woff, &p).await;
                if r % 4 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            let _ = done_tx.send(true);
        })
    };

    let mut readers = Vec::new();
    for rd in 0..4u32 {
        let h = h.clone();
        let legal = legal.clone();
        let mut done = done_rx.clone();
        readers.push(tokio::spawn(async move {
            loop {
                let got = read_at(&h, ino, woff, wlen).await;
                for (half_i, half) in got.chunks(4096).enumerate() {
                    assert!(
                        legal
                            .iter()
                            .any(|l| &l[half_i * 4096..(half_i + 1) * 4096] == half),
                        "reader {rd}: LBA half {half_i} is not a clean half of \
                         ANY legal payload — torn serve mid-patch"
                    );
                }
                if *done.borrow_and_update() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    patcher.await.unwrap();
    for r in readers {
        r.await.unwrap();
    }
    let after = snap();
    assert_eq!(
        delta!(after, before, patch_writes),
        ROUNDS as u64,
        "every storm round must patch (isolated aligned 8 KiB window)"
    );

    // Post-quiesce: hot reads and cold (tiers purged) reads both serve the
    // FINAL payload — a stale tier serve here is the ABA the incarnation
    // seqlock + purge-before-ACK exist to kill.
    let final_p = pattern(wlen, ((ROUNDS - 1) % 250) as u8);
    for pass in 0..2 {
        if pass == 1 {
            purge_read_tiers(&h, ino).await;
        }
        let got = read_at(&h, ino, woff, wlen).await;
        assert_bytes(
            &got,
            &final_p,
            &format!("post-quiesce read (pass {pass}) must serve the final patch"),
        );
        let whole = read_at(&h, ino, 2 * BS, BS as usize).await;
        let mut want_block = base[(2 * BS) as usize..(3 * BS) as usize].to_vec();
        want_block[16384..16384 + wlen].copy_from_slice(&final_p);
        assert_bytes(&whole, &want_block, "whole-block read post-quiesce");
    }
}

// ---------------------------------------------------------------------------
// EIO path + WRITE_VERIFICATION window
// ---------------------------------------------------------------------------

/// A failed patch DMA fails exactly this write (EIO), `publish_block`
/// re-stabilizes, tiers are purged — old bytes intact everywhere, no
/// stale serve, and the next patch of the same block succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_dma_error_returns_eio_and_restabilizes() {
    let _g = serial().await;
    let h = make(*b"rw2-eio-00000001", "rw2_ns_eio").await;
    let (ino, base) = durable_striped(&h, "eio.dat", 4, 0x22).await;

    let before = snap();
    squeezefs::nvme_dev::set_fail_next_writes(1);
    let res = try_write_at(&h, ino, BS + 8192, &pattern(4096, 0xC2)).await;
    squeezefs::nvme_dev::clear_fail_next_writes();
    assert!(
        res.is_err(),
        "a failed patch DMA must fail THIS write loudly (EIO class) — \
         nothing acked, nothing parked, nothing lost"
    );
    let after = snap();
    assert_eq!(
        delta!(after, before, dma_errors),
        1,
        "counted patch_dma_errors"
    );

    // Old bytes intact (hot + cold): the failed DMA never became servable.
    let got = read_at(&h, ino, 0, base.len()).await;
    assert_bytes(&got, &base, "old bytes intact after the failed patch (hot)");
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, base.len()).await;
    assert_bytes(
        &got,
        &base,
        "old bytes intact after the failed patch (cold)",
    );

    // Re-stabilized: the next patch of the same block succeeds (a stuck
    // unstable word would refuse validated fills forever and a broken
    // error path would leave the predicate failing).
    let before = snap();
    let p = pattern(4096, 0xC3);
    write_at(&h, ino, BS + 8192, &p).await;
    let after = snap();
    assert_eq!(
        delta!(after, before, patch_writes),
        1,
        "the block must be patchable again after the error path re-stabilized"
    );
    let mut want = base.clone();
    want[(BS + 8192) as usize..(BS + 8192) as usize + 4096].copy_from_slice(&p);
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, base.len()).await;
    assert_bytes(&got, &want, "post-recovery patch is durable + byte-exact");
}

/// Opt-in write verification covers exactly the patched window for free
/// (`write_block`'s window-exact read-back): clean passes verify, an
/// injected corruption fails the write loudly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_verification_covers_patched_window() {
    let _g = serial().await;
    let h = make(*b"rw2-verify-00001", "rw2_ns_vfy").await;
    let (ino, base) = durable_striped(&h, "vfy.dat", 4, 0x23).await;

    squeezefs::set_write_verification(true);
    squeezefs::set_write_verification_sample_rate(1);

    // Clean pass: the patch verifies its own window and succeeds.
    let before = snap();
    let p = pattern(4096, 0xC4);
    write_at(&h, ino, 2 * BS + 8192, &p).await;
    let after = snap();
    assert_eq!(
        delta!(after, before, patch_writes),
        1,
        "verified patch must succeed and count patch_writes"
    );

    // Injected corruption: the window-exact read-back must fail the write.
    squeezefs::nvme_dev::set_simulate_corruption(true);
    let attempted = pattern(4096, 0xC5);
    let res = try_write_at(&h, ino, 3 * BS + 8192, &attempted).await;
    squeezefs::nvme_dev::set_simulate_corruption(false);
    squeezefs::set_write_verification(false);
    assert!(
        res.is_err(),
        "a corrupted patched window must fail write verification loudly"
    );

    // Verified patch exact; every byte OUTSIDE the failed write's own
    // window exact; the failed window itself is POSIX-unspecified (a
    // failed write's range) — old or the attempted payload, never foreign.
    let mut want = base.clone();
    want[(2 * BS + 8192) as usize..(2 * BS + 8192) as usize + 4096].copy_from_slice(&p);
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, base.len()).await;
    let failed_at = (3 * BS + 8192) as usize;
    assert_bytes(
        &got[..failed_at],
        &want[..failed_at],
        "bytes before the failed window",
    );
    assert_bytes(
        &got[failed_at + 4096..],
        &want[failed_at + 4096..],
        "bytes after the failed window",
    );
    let win = &got[failed_at..failed_at + 4096];
    assert!(
        win == &want[failed_at..failed_at + 4096] || win == attempted.as_slice(),
        "the verification-failed window must be OLD or the attempted \
         payload (unspecified per POSIX), never foreign bytes"
    );
}

// ---------------------------------------------------------------------------
// Crash blast radius (kill-9-equivalent two-session audits)
// ---------------------------------------------------------------------------

/// ACKed-unfsynced patch + crash: after remount, every byte the app never
/// wrote is EXACTLY the old durable content (foreign bytes never
/// perturbed — the item-B guarantee, patched shapes included), and the
/// app-written window is old-or-new (v1 aligned-only: only app-written
/// sectors are ever rewritten). Plus the post-kill replay/read-back audit:
/// the remounted volume serves, fsyncs, and re-reads coherently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_acked_patch_foreign_bytes_intact_window_old_or_new() {
    let _g = serial().await;
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    set_patch_max_bytes(512 * 1024);

    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path(), *b"rw2-crash-ack-01").await;
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let staging = tempdir().unwrap();

    let woff = 2 * BS + 8192;
    let patch = pattern(4096, 0xE8);

    let (ino, base) = {
        let fs = open_fs("rw2_ns_cra", meta.path(), backing.path(), staging.path()).await;
        let req = Request {
            unique: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            pid: 1,
        };
        let h = H {
            fs,
            req,
            _b: NamedTempFile::new().unwrap(),
            _m: NamedTempFile::new().unwrap(),
            _s: tempdir().unwrap(),
        };
        let (ino, base) = durable_striped(&h, "crash_ack.dat", 6, 0x24).await;
        let before = snap();
        write_at(&h, ino, woff, &patch).await; // ACKed, UNFSYNCED
        let after = snap();
        assert_eq!(
            delta!(after, before, patch_writes),
            1,
            "the crash-window write must be a PATCH (in-place DMA) — that is \
             the shape whose blast radius this test pins"
        );
        (ino, base)
        // h drops here — the kill-9-equivalent for RAM state.
    };

    // Session 2: same meta + backing + staging.
    let fs2 = open_fs("rw2_ns_cra", meta.path(), backing.path(), staging.path()).await;
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    let h2 = H {
        fs: fs2,
        req,
        _b: NamedTempFile::new().unwrap(),
        _m: NamedTempFile::new().unwrap(),
        _s: tempdir().unwrap(),
    };
    let got = read_at(&h2, ino, 0, base.len()).await;

    // Foreign bytes: EXACT old content everywhere outside the window.
    assert_bytes(
        &got[..woff as usize],
        &base[..woff as usize],
        "post-crash: bytes before the app-written window are foreign — \
         they must be EXACTLY the old durable content",
    );
    assert_bytes(
        &got[woff as usize + 4096..],
        &base[woff as usize + 4096..],
        "post-crash: bytes after the app-written window are foreign — \
         they must be EXACTLY the old durable content",
    );
    // The app-written window: old-or-new (unfsynced data is unspecified;
    // an in-place DMA that completed reads new).
    let win = &got[woff as usize..woff as usize + 4096];
    assert!(
        win == &base[woff as usize..woff as usize + 4096] || win == patch.as_slice(),
        "post-crash: the app-written window must be OLD or NEW bytes, never \
         a foreign/torn mix at LBA granularity"
    );

    // Post-kill replay/read-back: the remounted volume keeps serving —
    // write, fsync, cold re-read.
    let p2 = pattern(4096, 0xE9);
    write_at(&h2, ino, 4 * BS + 8192, &p2).await;
    fsync(&h2, ino).await;
    purge_read_tiers(&h2, ino).await;
    let got2 = read_at(&h2, ino, 4 * BS + 8192, 4096).await;
    assert_bytes(&got2, &p2, "post-crash session writes + fsyncs + re-reads");
}

/// Failed-DMA crash: the patch DMA fails (injected), the write errors, the
/// session crashes — remount must read the old content EXACTLY (the failed
/// patch never touched the device; nothing acked, nothing lost).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_mid_patch_dma_failure_leaves_old_block_intact() {
    let _g = serial().await;
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    set_patch_max_bytes(512 * 1024);

    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path(), *b"rw2-crash-dma-01").await;
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let staging = tempdir().unwrap();

    let (ino, base) = {
        let fs = open_fs("rw2_ns_crb", meta.path(), backing.path(), staging.path()).await;
        let req = Request {
            unique: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            pid: 1,
        };
        let h = H {
            fs,
            req,
            _b: NamedTempFile::new().unwrap(),
            _m: NamedTempFile::new().unwrap(),
            _s: tempdir().unwrap(),
        };
        let (ino, base) = durable_striped(&h, "crash_dma.dat", 6, 0x25).await;
        squeezefs::nvme_dev::set_fail_next_writes(1);
        let res = try_write_at(&h, ino, 3 * BS + 8192, &pattern(4096, 0xEA)).await;
        squeezefs::nvme_dev::clear_fail_next_writes();
        assert!(res.is_err(), "the injected patch-DMA failure must surface");
        (ino, base)
    };

    let fs2 = open_fs("rw2_ns_crb", meta.path(), backing.path(), staging.path()).await;
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    let h2 = H {
        fs: fs2,
        req,
        _b: NamedTempFile::new().unwrap(),
        _m: NamedTempFile::new().unwrap(),
        _s: tempdir().unwrap(),
    };
    let got = read_at(&h2, ino, 0, base.len()).await;
    assert_bytes(
        &got,
        &base,
        "post-crash after a FAILED patch DMA: the old durable content must \
         be fully intact (the failed patch never reached the device)",
    );
}

// ---------------------------------------------------------------------------
// CLI-clone live-writer refusal (the D0 guard, enforced + tested)
// ---------------------------------------------------------------------------

/// The offline `squeezefs clone` verb must REFUSE to run against a meta
/// volume held by a live writer — its throwaway allocator's RAM refcounts
/// are invisible to the daemon, so an offline clone under a live write
/// mount could alias freed blocks. The D0 single-writer guard forbids it;
/// this pins the refusal as ENFORCED (loud, named), not prose.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_clone_refuses_live_writer() {
    let _g = serial().await;
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path(), *b"rw2-cli-clone-01").await;

    // The live writer: an open KvMetaBackend holds the D0 flock + claim.
    let holder = KvMetaBackend::open(meta.path()).await.expect("live writer");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_squeezefs"))
        .args([
            "clone",
            "-g",
            &format!("sqmeta://{}", meta.path().display()),
            "/src.dat",
            "/dst.dat",
        ])
        .output()
        .expect("spawn squeezefs clone");
    assert!(
        !out.status.success(),
        "`squeezefs clone` against a live write mount must exit nonzero \
         (the D0 writer guard forbids it)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("single-writer")
            || stderr.contains("writer lock")
            || stderr.contains("writer_claim")
            || stderr.contains("held by a live writer"),
        "the refusal must NAME the writer guard (loud, attributable): \
         stderr was:\n{stderr}"
    );
    drop(holder);
}
