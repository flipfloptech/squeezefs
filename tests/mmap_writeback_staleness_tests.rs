//! VL8 catalog item 1 — the generic/074 fstest.4 sub-page staleness hunt.
//!
//! Signature (074_run9 + vl4 run_3, both fstest.4 `-s 10M -b 512 -mS`):
//! ~1/20 runs a pread verify finds a stale-by-exactly-one-loop region
//! starting at a PAGE-ALIGNED offset (≥ 512 B long), daemon logs clean,
//! all counters silent. fstest.4 semantics (pinned from src/fstest.c):
//! per loop each file is open(O_TRUNC) → ftruncate(file_size) → mmap
//! STORES of a per-512B pattern v=(loop+child+fnum+ofs/bs)%256 → munmap →
//! close → pread verify. Kernel-side that is: SETATTR(size=0) →
//! SETATTR(size=N) → out-of-order page-granular writeback WRITEs →
//! READs. The stale bytes are the PREVIOUS loop's — so some artifact of
//! generation g-1 (parked extent overlay, staged extent record, staged
//! full sibling, tier entry, map key) survived the truncate-to-zero and
//! composed over generation g's acked writes.
//!
//! This harness models that shape in-process, deterministically driven:
//! seeded-permutation page-granular writes (runs of 1..=4 pages, the
//! kernel-writeback coalescing envelope), truncate(0)+regrow between
//! generations, several files interleaved (cross-ino spill/fold traffic),
//! per-512B generation-stamped patterns, full verify per generation, and
//! adversarial fold/park thresholds so the W2 extent machinery (park →
//! spill → record → fold) cycles constantly instead of ~never.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536; // downscaled stripe block (4 MiB in production)
const PAGE: usize = 4096; // kernel writeback granularity
const UNIT: usize = 512; // fstest verify granularity
/// 2.5 blocks — mirrors 10 MiB over 4 MiB blocks (partial tail block).
const FILE_SIZE: usize = (2 * BS + BS / 2) as usize;
const PAGES: usize = FILE_SIZE / PAGE;

/// Process-global knobs (fold/park/patch) are mutated per test.
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

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"), // roomy enough that spill refusals stay rare (loud EIO is FIND-RW5-A, not this hunt)
        dlm.meta_client().clone(),
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
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0x074C_0FFE_E074_2026,
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

async fn read_at(h: &HRef, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn truncate_to(h: &HRef, ino: u64, size: u64) {
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(size),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

/// One FUSE WRITE with a bounded EAGAIN retry (adversarial spill knobs
/// can race a lease drop/re-acquire; staleness, not availability, is the
/// target — any other errno fails loud).
async fn write_task(fs: SqueezefsFilesystem, req: Request, ino: u64, off: u64, data: Vec<u8>) {
    for attempt in 0..50u32 {
        match fs
            .write(req, ino, 0, off, bytes::Bytes::from(data.clone()), 0, 0)
            .await
        {
            Ok(w) => {
                assert_eq!(w.written as usize, data.len(), "short write at {off}");
                return;
            }
            Err(e) if e == fuse3::Errno::from(libc::EAGAIN) && attempt < 49 => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => panic!("write at {off} failed: {e:?}"),
        }
    }
}

/// fstest's gen_buffer, per 512-B unit: one byte value per (generation,
/// unit index) — a stale unit names its generation exactly.
fn unit_value(generation: u32, unit_idx: usize) -> u8 {
    (generation as usize + unit_idx) as u8
}

fn fill_pattern(buf: &mut [u8], generation: u32, file_off: usize) {
    for (i, chunk) in buf.chunks_mut(UNIT).enumerate() {
        let unit_idx = file_off / UNIT + i;
        chunk.fill(unit_value(generation, unit_idx));
    }
}

/// xorshift64* — seeded, deterministic permutation driver.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// One fstest.4 loop for one file, kernel-faithful:
/// `open(O_TRUNC)` (FUSE_ATOMIC_O_TRUNC is negotiated — the kernel sends
/// the flag on FUSE_OPEN and NO SETATTR(0) fallback) → `ftruncate(N)` →
/// mmap stores with the `-F do_frags=2` stride: each page is FAULTED in
/// first (a read the daemon must serve as post-truncate ZEROS — the red
/// assertion of VL8 catalog item 1), EVEN 512-B units get the generation
/// pattern, ODD units keep the faulted bytes, and the kernel writes back
/// WHOLE dirty pages — out of order, coalesced, concurrent.
async fn run_generation(h: &HRef, ino: u64, generation: u32, rng: &mut Rng) {
    h.fs.open(h.req, ino, (libc::O_RDWR | libc::O_TRUNC) as u32)
        .await
        .expect("open(O_TRUNC)");
    truncate_to(h, ino, FILE_SIZE as u64).await;

    // Seeded permutation of pages, coalesced into runs of 1..=4 pages —
    // the out-of-order page-granular writeback envelope.
    let mut pages: Vec<usize> = (0..PAGES).collect();
    for i in (1..pages.len()).rev() {
        pages.swap(i, rng.below(i + 1));
    }
    let mut i = 0usize;
    let mut writes: Vec<(u64, Vec<u8>)> = Vec::new();
    while i < pages.len() {
        // Coalesce a run when the permutation happens to be ascending-
        // adjacent (writeback merges contiguous dirty pages; occasionally
        // a big flush run — the ≤1 MiB kernel writeback envelope, which
        // at this geometry SPANS block boundaries like production
        // kernel-split WRITEs span max_pages).
        let run_cap = if rng.below(8) == 0 {
            24
        } else {
            1 + rng.below(4)
        };
        let mut run = 1usize;
        while run < run_cap && i + run < pages.len() && pages[i + run] == pages[i + run - 1] + 1 {
            run += 1;
        }
        let off = pages[i] * PAGE;
        let len = run * PAGE;
        // The mmap store faults each page in FIRST. After the O_TRUNC +
        // sparse regrow the fault MUST read zeros (hole) — pre-fix the
        // daemon served the previous generation here (O_TRUNC ignored).
        let faulted = read_at(h, ino, off as u64, len).await;
        if let Some(bad) = faulted.iter().position(|&b| b != 0) {
            panic!(
                "gen {generation} ino {ino}: page fault at file off {} read {:#04x} \
                 (want 0 — a truncated file's pages are holes); the previous \
                 generation survived open(O_TRUNC) (VL8 catalog item 1)",
                off + bad,
                faulted[bad],
            );
        }
        // -F stride: EVEN units take the generation pattern, ODD units
        // keep the faulted (zero) bytes; the writeback WRITE is the whole
        // dirty page run.
        let mut buf = faulted;
        for (u, chunk) in buf.chunks_mut(UNIT).enumerate() {
            let unit_idx = off / UNIT + u;
            if unit_idx.is_multiple_of(2) {
                chunk.fill(unit_value(generation, unit_idx));
            }
        }
        writes.push((off as u64, buf));
        i += run;
    }
    // Dispatch in concurrent batches (FOPEN_PARALLEL_DIRECT_WRITES /
    // multiple writeback threads): up to 8 WRITEs in flight as REAL tasks
    // (the fuse3 dispatch shape; also what lets a taskdump trace them),
    // order decided by the seeded permutation above.
    for batch in writes.chunks(8) {
        let handles: Vec<_> = batch
            .iter()
            .map(|(off, buf)| {
                let fs = h.fs.clone();
                let req = h.req;
                let off = *off;
                let buf = buf.clone();
                tokio::spawn(async move { write_task(fs, req, ino, off, buf).await })
            })
            .collect();
        for jh in handles {
            jh.await.expect("write task panicked");
        }
    }

    // close() after munmap → FUSE FLUSH on the dirty handle (folds,
    // writeback enqueues, dirty-layout persists — the per-loop drain the
    // wild workload always runs between the stores and the verify).
    h.fs.flush(h.req, ino, 0, 0)
        .await
        .expect("flush after generation stores");

    // pread verify, whole file, per 512-B unit.
    let mut off = 0usize;
    while off < FILE_SIZE {
        let len = (64 * 1024).min(FILE_SIZE - off);
        let got = read_at(h, ino, off as u64, len).await;
        assert_eq!(got.len(), len, "short read at {off} (gen {generation})");
        for (i, chunk) in got.chunks(UNIT).enumerate() {
            let unit_idx = off / UNIT + i;
            // -F stride verify: odd units were never stored this
            // generation — they must read the faulted zeros.
            let want = if unit_idx.is_multiple_of(2) {
                unit_value(generation, unit_idx)
            } else {
                0
            };
            if let Some(bad) = chunk.iter().position(|&b| b != want) {
                let gotv = chunk[bad];
                let stale_gen = (gotv as i32) - (unit_idx as i32 % 256);
                panic!(
                    "gen {generation} ino {ino}: unit {unit_idx} (file off {}) read {:#04x}, \
                     want {:#04x} — looks like generation {} content (the generic/074 \
                     fstest.4 stale-unit signature); first bad byte at unit offset {bad}",
                    unit_idx * UNIT,
                    gotv,
                    want,
                    stale_gen.rem_euclid(256),
                );
            }
        }
        off += len;
    }
}

/// The core soak: several files, interleaved generations, seeded orders.
/// One `H` across every seed (one daemon lifetime — a fresh instance per
/// seed would collide identical ino paths on the process-global local
/// DLM once background workers keep the old instance's leases alive).
async fn soak(h: &H, seed: u64, files: usize, generations: u32) {
    // fstest runs its children CONCURRENTLY on one mount: each file's
    // generation loop is its own task (cross-ino spill/fold traffic,
    // shared block-lock stripes), seeded per file.
    let mut tasks = Vec::new();
    for f in 0..files {
        let ino = create(h, &format!("file_{seed:x}_{f}")).await;
        let fs = h.fs.clone();
        let req = h.req;
        tasks.push(tokio::spawn(async move {
            let hh = HRef { fs, req };
            let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) ^ (f as u64) | 1);
            for g in 0..generations {
                run_generation(&hh, ino, g, &mut rng).await;
            }
        }));
    }
    for t in tasks {
        t.await.expect("file soak task panicked");
    }
}

/// Cloneable handler handle for spawned per-file soak tasks.
struct HRef {
    fs: SqueezefsFilesystem,
    req: Request,
}

/// Adversarial thresholds: constant extent-park → fold → spill cycling
/// (production defaults make folds rare at this downscaled geometry).
fn crank_knobs() {
    squeezefs::fuse_client::set_patch_max_bytes(0); // shapes park, never patch
    squeezefs::fuse_client::set_fold_max_extents(3); // fold hints constantly
    squeezefs::fuse_client::set_fold_max_bytes(3 * PAGE as u64);
    squeezefs::fuse_client::set_parked_cap_buffers(4); // spill pressure
}

fn default_knobs() {
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    squeezefs::fuse_client::set_fold_max_extents(64);
    squeezefs::fuse_client::set_fold_max_bytes(1024 * 1024);
    squeezefs::fuse_client::set_parked_cap_buffers(256);
}

/// THE generic/074 fstest.4 root-cause pin (VL8 catalog item 1):
/// `open(O_TRUNC)` MUST truncate daemon-side state.
///
/// fuse3 advertises `FUSE_ATOMIC_O_TRUNC`, so the kernel NEVER sends the
/// SETATTR(size=0) fallback — it truncates its own page cache/i_size and
/// trusts the daemon to honor the O_TRUNC flag in FUSE_OPEN. The open
/// handler ignored `flags` entirely: the previous generation's ENTIRE
/// daemon state (size, block map, staged ring images, parked overlays,
/// staged extent records) survived every fstest.4 loop's O_TRUNC — and
/// fstest -F writes only every OTHER 512-B unit through mmap, so its
/// stores FAULT each page in first (a read the daemon serves from the
/// un-truncated previous generation) and the whole cross-generation
/// compose surface (record-over-base ordering, fold seeds from the
/// never-pruned old map, patch-in-place into old keys) stayed live.
/// The wild signature — a page-aligned ≥512-B run reading exactly one
/// generation stale, daemon logs clean — is that residue surfacing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_o_trunc_truncates_daemon_state() {
    let _g = serial().await;
    default_knobs();
    let h = make(*b"mmapwb074otrunc!", "mmap_wb_otrunc").await;
    let ino = create(&h, "victim").await;
    let hh = HRef {
        fs: h.fs.clone(),
        req: h.req,
    };

    // Generation 0: real content across two blocks, flushed durable.
    let mut gen0 = vec![0u8; FILE_SIZE];
    fill_pattern(&mut gen0, 0, 0);
    write_task(h.fs.clone(), h.req, ino, 0, gen0).await;
    h.fs.flush(h.req, ino, 0, 0).await.expect("flush gen0");

    // The kernel's atomic-O_TRUNC open: FUSE_OPEN with O_TRUNC, NO
    // SETATTR follows (that is the negotiated contract).
    h.fs.open(h.req, ino, (libc::O_RDWR | libc::O_TRUNC) as u32)
        .await
        .expect("open(O_TRUNC)");

    // Daemon-side size authority must be 0 now.
    let attr =
        h.fs.getattr(h.req, ino, None, 0)
            .await
            .expect("getattr")
            .attr;
    assert_eq!(
        attr.size, 0,
        "open(O_TRUNC) must truncate the daemon's size authority \
         (FUSE_ATOMIC_O_TRUNC is advertised: no SETATTR fallback ever comes)"
    );

    // And the old content must be GONE: regrow sparse, then a read of the
    // old range must be zeros (hole semantics), never generation-0 bytes.
    truncate_to(&hh, ino, FILE_SIZE as u64).await;
    let got = read_at(&hh, ino, 0, PAGE).await;
    assert!(
        got.iter().all(|&b| b == 0),
        "post-O_TRUNC regrown range must read as zeros (hole), got stale \
         generation-0 bytes: {:02x?}…",
        &got[..16]
    );
}

/// The O_TRUNC-fix follow-up (found by the first counted generic/074 ×20:
/// run 2 aborted the count with `fstest.0 … Input/output error` +
/// `FencingTokenExpired {1, 2}`): setattr's size path presented a BARE
/// `get_fencing_token_ino` snapshot instead of holding the shared cached
/// lease. Sequence: close drops the cached lease → a background flush
/// re-acquires (token bump) → the O_TRUNC open's truncate presents the
/// stale snapshot → `save_metadata_to_backend` fences it → EIO to
/// `open(2)`. Truncate is a mutation: it must ride the SAME
/// `get_or_acquire_lease` discipline as writes (shared cache ⇒ no bump,
/// or a fresh acquisition serialized behind the transient holder).
///
/// Race-loop repro (the window is bump-between-snapshot-and-save; no
/// deterministic seam exists, so the loop drives it statistically —
/// red-verified firing well within the budget pre-fix; post-fix every
/// iteration must succeed by construction).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_o_trunc_never_races_lease_churn_into_eio() {
    let _g = serial().await;
    default_knobs();
    let h = make(*b"mmapwb074lease!!", "mmap_wb_lease_race").await;
    let ino = create(&h, "victim").await;
    let path = squeezefs::keys::inode_path(ino);

    for i in 0..300u32 {
        // A write caches the op lease…
        write_task(h.fs.clone(), h.req, ino, 0, vec![i as u8; 8192]).await;
        // …release drops it…
        h.fs.invalidate_local_lease(ino);
        // …and the O_TRUNC open races a transient background acquirer
        // (drain/clone-class): pre-fix some iteration's truncate presents
        // a stale token and open(2) gets EIO.
        let dlm = h.fs.dlm().clone();
        let p = path.clone();
        let acquirer = tokio::spawn(async move {
            // Best-effort with a short budget: post-fix the open's
            // truncate legitimately holds the cached op lease, so this
            // acquirer finding the lock held (LockFailed) is the CORRECT
            // outcome — no bump can occur under a held lease.
            if let Ok(lease) = dlm
                .acquire_lock(&p, None, std::time::Duration::from_millis(50))
                .await
            {
                drop(lease);
            }
        });
        let opened =
            h.fs.open(h.req, ino, (libc::O_RDWR | libc::O_TRUNC) as u32)
                .await;
        acquirer.await.expect("acquirer task");
        assert!(
            opened.is_ok(),
            "iteration {i}: open(O_TRUNC) must never surface a transient \
             lease-churn fencing race as an open(2) error: {:?}",
            opened.err()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mmap_writeback_generations_stay_current_default_knobs() {
    let _g = serial().await;
    default_knobs();
    let h = make(*b"mmapwb074dflt!!!", "mmap_wb_stale_default").await;
    for seed in [0x074A_u64, 0x074B, 0x074C] {
        soak(&h, seed, 3, 6).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mmap_writeback_generations_stay_current_adversarial_knobs() {
    let _g = serial().await;
    crank_knobs();
    // Wedge tripwire: if the soak stops making progress, dump the
    // named-holder lock-wait census and abort loud instead of hanging the
    // suite (the D1.b watchdog surface, driven directly).
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let tripwire = std::thread::spawn(move || {
        for _ in 0..120 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if stop2.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
        }
        eprintln!("=== WEDGE TRIPWIRE: lock-wait census ===");
        for e in squeezefs::fuse_client::lock_wait_census(std::time::Duration::from_secs(5)) {
            eprintln!(
                "  {}/{} ino {} key {} waited {} ms holder={:?}",
                e.class, e.site, e.ino, e.key, e.waited_ms, e.holder
            );
        }
        eprintln!("=== overdue ops ===");
        for o in squeezefs::fuse_client::op_watchdog_tick(std::time::Duration::from_secs(5)) {
            eprintln!("  {} ino {} age {} ms", o.op, o.ino, o.age_ms);
        }
        let m = &squeezefs::fuse_client::METRICS;
        use std::sync::atomic::Ordering::Relaxed;
        eprintln!(
            "gauges: parked_full={} parked_ext={} extent_parks={} spills={} folds={} \
             fold_seed_reads={} wt_blocks={} stag_flush={} wb_flush_enq={} sup_noops={} \
             stale_tok={} lease_ok={} lease_fail={}",
            m.parked_full_buffer_bytes.load(Relaxed),
            m.parked_extent_bytes.load(Relaxed),
            m.extent_parks.load(Relaxed),
            m.extent_spills.load(Relaxed),
            m.fold_passes.load(Relaxed),
            m.fold_seed_reads.load(Relaxed),
            m.write_through_blocks.load(Relaxed),
            m.staging_put_bytes_flush.load(Relaxed),
            m.writeback_enqueued_flush.load(Relaxed),
            m.writeback_superseded_noops.load(Relaxed),
            m.writeback_stale_token_retries.load(Relaxed),
            m.lease_acquire_ok.load(Relaxed),
            m.lease_acquire_fail.load(Relaxed),
        );
        eprintln!(
            "write phases: {}",
            squeezefs::fuse_client::write_profile_phase_json()
        );
        eprintln!(
            "op phases: {}",
            squeezefs::fuse_client::op_profile_phase_json()
        );
        std::process::abort();
    });
    let h = make(*b"mmapwb074advrs!!", "mmap_wb_stale_adv").await;
    for seed in [0x1074_u64, 0x2074, 0x3074, 0x4074] {
        soak(&h, seed, 3, 6).await;
    }
    stop.store(true, std::sync::atomic::Ordering::Release);
    let _ = tripwire.join();
    default_knobs();
}
