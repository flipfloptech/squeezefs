//! The READ dest-window lease — phase 1 of the read copy-elimination
//! program (read CPU-wall ruling, `.benchmarks/2026-08-06-read-cpu-wall.md`:
//! the serve dest-copy is 1.00 CPU passes/byte ≈ 1/2.7 of the whole read
//! CPU budget on the walled field client).
//!
//! Kernel adjudication (patched linux-6.19.14, the sqz kernel — see the
//! campaign report): the fuse-over-uring COMMIT path consumes ONLY
//! `commit_id`/`qid` from the COMMIT_AND_FETCH SQE
//! (`fuse_uring_commit_fetch`), copies the reply body from the REGISTER-
//! time ent payload VA (`setup_fuse_copy_state` → `import_ubuf(ent->payload …)`)
//! or the kernel-attached kmbuf (`cs->kaddr = ent->payload_kvec.iov_base`),
//! and `fuse_uring_ent_in_out` carries no payload offset/address field —
//! so a reply body must SIT AT the ent window base at commit time, and
//! LENDING a tier buffer to the transport can only relocate the serve
//! copy into `apply_reply`, never delete it. The always-expressible
//! inverse is built here instead: **lease the request's own dest window
//! to the FILL** — cold sub-block windows DMA device bytes straight into
//! the registered ent payload (`RangedDest` under the §5.4 exclusivity
//! argument), so the serve is pointer arithmetic + header write.
//!
//! Contracts pinned (red-first):
//! 1. **Engagement**: a cold, 4 KiB-aligned, sub-block, dest-armed read
//!    with `ReadClassHint::dest_lease` serves by device→dest DMA —
//!    `read_dest_lease_bytes` accounts every served byte,
//!    `read_copy_dest_bytes` stays 0, and the reply is in place.
//!    Streaming classification does NOT veto the leg (the old
//!    `ranged_eligible` smallness/streaming gate is the exact reason the
//!    field's cold EXA row pays 1.00 passes/byte).
//! 2. **Lane yield**: dest-leaseable traffic stands the speculative
//!    fill machinery down — no R2 prefetch issue, no read-lane ahead
//!    fetches (a pooled fill for a window the demand read will DMA
//!    itself is a pure double-fetch: device bytes with no CPU pass
//!    saved). Without the yield the mechanism is incoherent, so the
//!    yield is part of contract 1's assertions.
//! 3. **Fallback**: unaligned windows keep today's ladder verbatim
//!    (fill + one lawful serve copy, counted on `read_copy_dest_bytes`)
//!    — a refused lease must never become a lost or wrong read.
//! 4. **Control**: hints WITHOUT `dest_lease` (internal readers, il
//!    arena dests in phase 1, the `SQUEEZEFS_READ_DEST_LEASE=0` lever
//!    at the handler) keep today's shape byte-identically — fills,
//!    serve copies, pipeline engagement.
//! 5. **Ledger law**: `read_dest_lease_bytes ⊆ read_dest_dma_bytes`
//!    (the closure equation `dest + bounce + dest_dma ≡ served` is
//!    unchanged — lease bytes ride the dest_dma bucket; the new counter
//!    is the phase-1 engagement split, the `warm_serve ⊂ dest`
//!    precedent applied to the DMA bucket).
//!
//! Counter-asserting phases share one test fn (the ledger suite's
//! counter-isolation discipline — the counters are process-global).

use fuse3::raw::prelude::Filesystem;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::ReadClassHint;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 1_048_576;
/// The lease-window read size: 4 KiB-aligned, sub-block (2 windows per
/// block), and ABOVE the old `ranged_eligible` threshold (256 KiB) so
/// phase A's engagement cannot be served by the pre-existing
/// small-read ranged gate — every phase-A byte must ride the NEW lease
/// gate (streaming or not).
const WIN: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make_with(block_size: &str, uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", block_size);
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = Some(tempdir().unwrap());
    let staging_dirs = s
        .as_ref()
        .map(|d| vec![d.path().to_path_buf()])
        .unwrap_or_default();
    let cache = TieredCache::new(
        staging_dirs,
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
    let router = squeezefs::routing::DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_1234_5679,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = fuse3::raw::Request {
        unique: 2,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 4,
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

/// Deterministic ground-truth byte for global file offset `off`.
fn pat(off: u64) -> u8 {
    ((off % 241) as u8) ^ (((off / 4096) % 11) as u8)
}

async fn write_pattern(h: &H, ino: u64, len: u64) {
    let mut off = 0u64;
    while off < len {
        let chunk = std::cmp::min(BS, len - off) as usize;
        let data: Vec<u8> = (0..chunk as u64).map(|i| pat(off + i)).collect();
        write_at(h, ino, off, &data).await;
        off += chunk as u64;
    }
}

async fn make_cold(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
    assert!(!map.is_empty(), "fixture must promote to striped");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
}

/// 4 KiB-aligned scratch destination (the registered-payload stand-in).
struct AlignedDest {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedDest {
    fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        // SAFETY: non-zero size, valid layout.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Self { ptr, layout }
    }
    /// The FUSE-4e destination WINDOW for this allocation (the registered
    /// payload window's stand-in — cap == the whole allocation).
    fn dest(&self) -> squeezefs::routing::ReadDest {
        // SAFETY: the allocation outlives every read it is handed to and is
        // exclusively this test's.
        unsafe { squeezefs::routing::ReadDest::new(self.ptr as u64, self.layout.size()) }
    }
    fn slice(&self, len: usize) -> &[u8] {
        assert!(len <= self.layout.size());
        // SAFETY: within the allocation; initialized (zeroed at birth,
        // then written by the serve under test).
        unsafe { std::slice::from_raw_parts(self.ptr, len) }
    }
}

impl Drop for AlignedDest {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout above.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

#[derive(Debug)]
struct Snap {
    lease: u64,
    dest: u64,
    bounce: u64,
    dest_dma: u64,
    fill_dma: u64,
    prefetch_issued: u64,
    lane_fetches: u64,
}

fn snap() -> Snap {
    Snap {
        lease: METRICS.read_dest_lease_bytes.load(Ordering::Relaxed),
        dest: METRICS.read_copy_dest_bytes.load(Ordering::Relaxed),
        bounce: METRICS.read_copy_bounce_bytes.load(Ordering::Relaxed),
        dest_dma: METRICS.read_dest_dma_bytes.load(Ordering::Relaxed),
        fill_dma: METRICS.read_fill_dma_bytes.load(Ordering::Relaxed),
        prefetch_issued: METRICS.prefetch_issued.load(Ordering::Relaxed),
        lane_fetches: METRICS.read_lane_fetches.load(Ordering::Relaxed),
    }
}

fn delta(s0: &Snap) -> Snap {
    let s1 = snap();
    Snap {
        lease: s1.lease - s0.lease,
        dest: s1.dest - s0.dest,
        bounce: s1.bounce - s0.bounce,
        dest_dma: s1.dest_dma - s0.dest_dma,
        fill_dma: s1.fill_dma - s0.fill_dma,
        prefetch_issued: s1.prefetch_issued - s0.prefetch_issued,
        lane_fetches: s1.lane_fetches - s0.lane_fetches,
    }
}

/// The lease hint the FUSE read handler mints for kernel ent-payload
/// dests when the `SQUEEZEFS_READ_DEST_LEASE` lever is on.
fn lease_hint() -> ReadClassHint {
    ReadClassHint {
        dest_lease: true,
        ..Default::default()
    }
}

/// Sequentially read `blocks × per_block` aligned windows of `WIN` bytes
/// through the router with `hint`, each into its own checked dest, and
/// assert content + in-place serve for every window. Returns total bytes.
async fn read_windows(h: &H, path: &str, blocks: u64, hint: ReadClassHint) -> u64 {
    let mut total = 0u64;
    let per_block = BS / WIN;
    for b in 0..blocks {
        for w in 0..per_block {
            let off = b * BS + w * WIN;
            let s_win = snap();
            let dest = AlignedDest::new(WIN as usize);
            let data =
                h.fs.router
                    .read_file_range_zero_copy_with_meta(
                        path,
                        off,
                        WIN as u32,
                        Some(dest.dest()),
                        hint,
                        None,
                        None,
                    )
                    .await
                    .unwrap_or_else(|e| panic!("window read at {off} failed: {e:?}"));
            assert_eq!(data.len(), WIN as usize, "window length at {off}");
            assert_eq!(
                data.as_ptr(),
                dest.ptr as *const u8,
                "window at {off} must serve in place (the dest IS the reply)"
            );
            assert!(
                dest.slice(WIN as usize)
                    .iter()
                    .enumerate()
                    .all(|(j, &x)| x == pat(off + j as u64)),
                "window content at {off}"
            );
            drop(data);
            if hint.dest_lease {
                let dw = delta(&s_win);
                assert_eq!(
                    dw.lease, WIN,
                    "window at {off}: expected a lease serve, got {dw:?}"
                );
            }
            total += WIN;
        }
    }
    total
}

/// Phase A — engagement + lane yield; Phase B — unaligned fallback;
/// Phase C — the no-hint control keeps today's shape; Phase D — the
/// subset ledger law.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dest_lease_serves_cold_windows_by_dma_and_stands_the_fill_machinery_down() {
    let h = make_with("1048576", *b"rdstlease-v1-26!", "rdl_ns_a").await;
    // Shift ino allocation off the process-global DLM lock map hot keys
    // (the op-economy salt_inos discipline, ledger-suite pattern).
    for i in 0..8 {
        let _ = create(&h, &format!("salt{i}")).await;
    }

    // ---- Phase A: cold aligned sub-block windows with the lease hint.
    // 24 blocks × 4 windows — the stream classifies within the first
    // block, so the phase proves BOTH the pre-classification and the
    // classified-streaming populations ride the lease leg (the old
    // ranged gate's streaming veto is exactly what this phase pins OUT).
    let ino_a = create(&h, "rdl_a").await;
    write_pattern(&h, ino_a, 24 * BS).await;
    make_cold(&h, ino_a).await;
    let path_a = squeezefs::keys::inode_path(ino_a);

    let s0 = snap();
    let total = read_windows(&h, &path_a, 24, lease_hint()).await;
    let d = delta(&s0);
    assert_eq!(
        d.lease, total,
        "phase A: every cold aligned dest-armed window byte is lease-served \
         (engagement counter accounts the row)"
    );
    assert_eq!(
        d.dest, 0,
        "phase A: the serve dest-copy is DELETED on the lease leg — a serve \
         copy here is the 1.00 passes/byte the campaign exists to kill"
    );
    assert_eq!(d.bounce, 0, "phase A: no intermediate copies");
    assert_eq!(
        d.dest_dma, total,
        "phase A: lease bytes ride the dest-DMA bucket (closure law unchanged)"
    );
    assert_eq!(
        d.fill_dma, 0,
        "phase A: no pooled fills — the dest window IS the fill destination"
    );
    assert_eq!(
        d.prefetch_issued, 0,
        "phase A (lane yield): dest-leaseable traffic must not drive R2 \
         speculative fills — a pooled fill the demand read will DMA itself \
         is a pure double-fetch"
    );
    assert_eq!(
        d.lane_fetches, 0,
        "phase A (lane yield): no read-lane ahead fetches for dest-leaseable \
         traffic"
    );

    // ---- Phase B: an unaligned window (len % 4096 != 0) with the lease
    // hint falls back to today's ladder — fill + ONE lawful serve copy,
    // counted, content exact. A refused lease is never a lost read.
    let ulen = 100_000usize;
    let uoff = 5 * BS; // still cold: phase A lease serves deposit nothing
    let dest = AlignedDest::new(ulen);
    let s0 = snap();
    let data =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path_a,
                uoff,
                ulen as u32,
                Some(dest.dest()),
                lease_hint(),
                None,
                None,
            )
            .await
            .expect("phase B read");
    assert_eq!(data.len(), ulen);
    assert!(
        dest.slice(ulen)
            .iter()
            .enumerate()
            .all(|(j, &x)| x == pat(uoff + j as u64)),
        "phase B content (fallback serves exact bytes)"
    );
    drop(data);
    let d = delta(&s0);
    assert_eq!(d.lease, 0, "phase B: unaligned windows never lease");
    assert_eq!(
        d.dest, ulen as u64,
        "phase B: the fallback pays exactly one lawful serve copy"
    );

    // ---- Phase C: the control — the SAME cold aligned window shape
    // WITHOUT the lease hint keeps today's machinery byte-identically:
    // fills + serve copies + pipeline engagement (this is the shape the
    // `SQUEEZEFS_READ_DEST_LEASE=0` lever and every internal reader keep).
    let ino_c = create(&h, "rdl_c").await;
    write_pattern(&h, ino_c, 24 * BS).await;
    make_cold(&h, ino_c).await;
    let path_c = squeezefs::keys::inode_path(ino_c);

    let s0 = snap();
    let total_c = read_windows(&h, &path_c, 24, ReadClassHint::default()).await;
    let d = delta(&s0);
    assert_eq!(d.lease, 0, "phase C: no lease hint — no lease serves");
    assert!(
        d.dest + d.dest_dma >= total_c,
        "phase C: every control byte is served through today's ladder \
         (serve copies from fills/tiers, plus the pre-classification \
         small-window ranged DMAs), got dest={} dest_dma={} for {}",
        d.dest,
        d.dest_dma,
        total_c
    );
    assert!(
        d.dest > 0,
        "phase C: the control shape still pays serve dest-copies \
         (fill/hold/tier slice-outs) — if this is 0 the control fixture \
         no longer exercises the copy the campaign killed"
    );
    assert!(
        d.prefetch_issued + d.lane_fetches > 0,
        "phase C: the control shape keeps the speculative fill machinery \
         engaged (classified sequential stream drives R2/read-lane issue)"
    );

    // ---- Phase D: the subset ledger law over the whole run.
    let end = snap();
    assert!(
        end.lease <= end.dest_dma,
        "phase D: read_dest_lease_bytes ⊆ read_dest_dma_bytes (lease is an \
         engagement split of the DMA bucket, the warm_serve⊂dest precedent)"
    );
}
