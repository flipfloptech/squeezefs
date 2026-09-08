//! KERNEL-lane op economy (PERF-12) — the alloc-site instrument the IPC
//! lane already has, pointed at the FUSE handlers.
//!
//! The 2026-07-28 op-economy campaign made the §5.5.1 warm IPC serve
//! allocation-free and measured 22–29 % more IOPS for it
//! (`.benchmarks/2026-07-28-ipc-op-economy.md`). The KERNEL lane — the
//! default mount, every non-intercepted process — was never given the same
//! treatment: it still minted a per-op `inode_{ino}` `String`, cloned the
//! layout type into a per-write `String`, and resolved single-block keys
//! through `load_striped_block_keys`, which allocates a `Vec`, clones the
//! key `String` into it and sorts the one-element result — twice per
//! tier-hit serve (once for the serve, once for the binding recheck).
//!
//! This suite is the standing instrument for that lane, mirroring
//! `tests/ipc_op_economy_tests.rs`:
//!
//! * a counting global allocator around a quiesced, warm window of kernel
//!   READ / WRITE handler calls, with per-op bounds; and
//! * `SQZ_ALLOC_TRACE=1 cargo test --test kernel_op_economy_tests --
//!   --nocapture` flips the same harness into the PROFILER, printing the
//!   deduped alloc-site backtrace table.
//!
//! The bounds are deliberately *budgets*, not zero: the kernel lane has no
//! caller-supplied arena, so a handler that is not serving into a transport
//! payload lease must hand back an owned `Bytes` (the harness shape here),
//! and the reply framing is legitimately one allocation. What the budgets
//! pin is that the PRELUDE — key minting, layout classification, binding
//! resolution — stays off the allocator.
//!
//! The WRITE lane got its own campaign 2026-09-08 (W-6, e2e perf audit
//! write board #7 — `.benchmarks/2026-09-08-w6-write-handler-economy.md`):
//! the W1 patch write went 14.6 → 3 allocs/op, and its budget is now the
//! device layer's three (see the write test's doc).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// counting global allocator (the profiling instrument — ipc_op_economy shape)
// ---------------------------------------------------------------------------

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static TRACE: AtomicBool = AtomicBool::new(false);
static TRACE_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

thread_local! {
    static IN_TRACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct CountingAlloc;

impl CountingAlloc {
    fn record(&self, layout: Layout) {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        if TRACE.load(Ordering::Relaxed) {
            IN_TRACE.with(|flag| {
                if !flag.get() {
                    flag.set(true);
                    let bt = std::backtrace::Backtrace::force_capture();
                    if let Ok(mut log) = TRACE_LOG.lock() {
                        log.push(format!("[{} B]\n{bt}", layout.size()));
                    }
                    flag.set(false);
                }
            });
        }
    }
}

// SAFETY: delegates verbatim to `System`; the accounting side effects are
// atomic counters plus a recursion-guarded trace hook.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.record(layout);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.record(layout);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn allocs_now() -> u64 {
    ALLOC_COUNT.load(Ordering::Relaxed)
}

fn trace_enabled() -> bool {
    std::env::var("SQZ_ALLOC_TRACE").as_deref() == Ok("1")
}

fn print_site_table() {
    let log = TRACE_LOG.lock().expect("trace log mutex");
    let mut sites: HashMap<String, u64> = HashMap::new();
    for entry in log.iter() {
        let key: String = entry
            .lines()
            .filter(|l| {
                (l.contains("squeezefs") || l.contains("fuse3"))
                    && !l.contains("kernel_op_economy_tests")
                    && !l.contains("CountingAlloc")
            })
            .take(4)
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>()
            .join("\n    ");
        *sites.entry(key).or_default() += 1;
    }
    let mut rows: Vec<_> = sites.into_iter().collect();
    rows.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("--- kernel-lane alloc sites (deduped, most frequent first) ---");
    for (site, n) in rows.iter().take(24) {
        println!("[{n}]\n    {site}\n");
    }
}

// ---------------------------------------------------------------------------
// fixture — 512 KiB blocks so a small file is genuinely STRIPED
// ---------------------------------------------------------------------------

const BS: u64 = 512 * 1024;

struct H {
    fs: squeezefs::fuse_client::SqueezefsFilesystem,
    req: Request,
    _b: tempfile::NamedTempFile,
    _m: tempfile::NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make() -> H {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::meta_backend::kv::backend::KvMetaBackend;
    use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
    use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
    use squeezefs::meta_backend::RoutedMetaBackend;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;

    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    // Pin the request-driven shape: prefetch/ranged machinery has its own
    // suites and would add allocations from ITS lanes to this window.
    std::env::set_var("SQUEEZEFS_READ_PREFETCH_WINDOW", "0");
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");

    let dlm = DlmClient::new().unwrap();
    let b = tempfile::NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("kernel_op_economy").await.unwrap());
    let s = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
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
    let mut fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = tempfile::NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_9911_2233,
            uuid: *b"kernel-op-econ-1",
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
        .expect("create")
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    write_bytes_at(h, ino, off, bytes::Bytes::copy_from_slice(data)).await;
}

/// The measured-window form: the caller owns the payload `Bytes` (minted
/// once, cloned per op) so the harness itself allocates NOTHING inside
/// the window — the kernel path hands the handler a `Bytes::from_owner`
/// payload lease whose clone is a refcount bump, and a per-op
/// `copy_from_slice` here would add the harness's own `Vec` plus the
/// first-clone promotion of a `Vec`-backed `Bytes` (two allocations the
/// field never pays) to the handler's ledger.
async fn write_bytes_at(h: &H, ino: u64, off: u64, data: bytes::Bytes) {
    let len = data.len();
    let w =
        h.fs.write(h.req, ino, 0, off, data, 0, 0)
            .await
            .expect("write");
    assert_eq!(w.written as usize, len);
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> usize {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .expect("read")
        .data
        .len()
}

/// A striped fixture: three full blocks, flushed, so reads take the striped
/// serve path (block-map resolve + tier probe + binding recheck).
async fn striped_fixture(h: &H, name: &str) -> u64 {
    let ino = create(h, name).await;
    for b in 0..3u64 {
        write_at(h, ino, b * BS, &vec![0x6Du8; BS as usize]).await;
    }
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    ino
}

// ---------------------------------------------------------------------------
// READ — the warm striped tier-hit serve
// ---------------------------------------------------------------------------

/// **The kernel READ lane's per-op allocation budget.**
///
/// Shape: warm 4 KiB sub-block reads of a striped file whose block is
/// resident in the RAM tiers — the tier-hit serve, i.e. the path a
/// re-reading workload lives on. It resolves the block key, probes the
/// tiers, slices, and RECHECKS the binding.
///
/// Measured on this box (dev profile, 2,000 ops, counted A-B on the same
/// binary pair):
///   * pre-PERF-12:  **14.05** allocs/op
///   * post-PERF-12: **8.05** allocs/op  (−43 %)
///
/// What PERF-12 removed, per op: the `inode_{ino}` layout-identity `String`
/// (1), the handler's `active_block:…` key (`CompactString` + `String`, 2),
/// the router's staging-probe key (2), and the binding recheck's
/// `Vec` + key clone (`load_striped_block_keys` for a single block, 2 — the
/// recheck now compares against the live map by reference).
///
/// The eight that remain are named by the trace mode and are follow-on work
/// (`ReadCustodyFp::build_sync` ×2, the serve's own `load_striped_block_keys`
/// resolve ×2, `get_block_for_index`'s owned binding, `staged_extent_runs_in`'s
/// `active_block_ext:` key, the reply body, one moka guard). The bar is set
/// at 9/op — above the shipped number so incidental churn cannot flake it,
/// far below the pre-fix 14.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_kernel_read_prelude_allocation_budget() {
    let h = make().await;
    let ino = striped_fixture(&h, "warm_read.bin").await;

    // Warm: the first read fills the tiers; from then on the serve is a
    // tier hit (the shape under measurement).
    for _ in 0..8 {
        assert_eq!(read_at(&h, ino, 4096, 4096).await, 4096);
    }

    if trace_enabled() {
        TRACE.store(true, Ordering::SeqCst);
        let a0 = allocs_now();
        for _ in 0..32 {
            let _ = read_at(&h, ino, 4096, 4096).await;
        }
        let allocs = allocs_now() - a0;
        TRACE.store(false, Ordering::SeqCst);
        println!("traced kernel READ window: {allocs} allocs / 32 ops");
        print_site_table();
        return;
    }

    const OPS: u64 = 2_000;
    // Settle so idle housekeeping (moka, tokio timers) is not inside the
    // measured window.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let a0 = allocs_now();
    for _ in 0..OPS {
        let _ = read_at(&h, ino, 4096, 4096).await;
    }
    let allocs = allocs_now() - a0;
    let per_op_centi = allocs * 100 / OPS;
    println!(
        "kernel READ: {allocs} allocs / {OPS} ops = {}.{:02} per op",
        per_op_centi / 100,
        per_op_centi % 100
    );
    assert!(
        per_op_centi <= 900,
        "warm kernel READ serve allocates {}.{:02} per op — the PERF-12 \
         budget is 9.00 (prelude work must not allocate: stack keys, \
         borrowed single-block resolve, no per-op layout String)",
        per_op_centi / 100,
        per_op_centi % 100
    );
}

// ---------------------------------------------------------------------------
// READ — the cold FUSE-zc direct leg (the field kern rand-4k posture)
// ---------------------------------------------------------------------------

/// The injected zc fetch primitive: the transport's `READ_FIXED(device →
/// slot)` stands in as an immediately-ready `Ok(len)` — the DMA is the
/// kernel's work and not what this ledger prices. Its `Box::pin` is the
/// one structural allocation the injected shape carries (the live
/// connection arm awaits the transport's own future unboxed).
fn zc_ready_serve() -> squeezefs::routing::ZcReadServe {
    squeezefs::routing::ZcReadServe::new(Box::new(|_fd, _off, len| Box::pin(async move { Ok(len) })))
}

/// **The kernel READ lane's COLD zc-leg allocation budget** (R-5).
///
/// Shape: the field kern rand-4k posture on the sqz kernel — every tier
/// probe misses and the router's FUSE-zc direct leg DMAs the 4 KiB
/// window straight into the caller's pages (`zc_device_resolve` →
/// incarnation snapshot → fetch → still-check → binding recheck). The
/// leg deposits nothing, so every read of the window is cold again;
/// the fixture is the router primitive with an injected fetch (the
/// `fuse_zc_serve_tests` venue).
///
/// Measured on this box (dev profile, 2,000 ops, counted A-B on the same
/// binary pair):
///   * pre-R-5:  **11.00** allocs/op — the W2 overlay key (1), the
///     single-block `load_striped_block_keys` (`Vec` + key clone, 2),
///     and FOUR key re-parses that each minted a `String` (and three of
///     them a `clean_block_key` `String` first): `zc_device_resolve` (1),
///     `key_incarnation_tracked` (2), `fill_incarnation` (2),
///     `fill_incarnation_still` (2) — plus the injected fetch's `Box::pin`.
///   * post-R-5: **1.00** allocs/op — the injected fetch's `Box::pin`
///     alone (the live connection arm carries no box either).
///
/// The bar is 2/op — headroom for incidental churn, red on the pre-R-5
/// tree by a factor of five.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_kernel_read_zc_leg_allocation_budget() {
    let h = make().await;
    let ino = striped_fixture(&h, "cold_zc.bin").await;
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .expect("layout")
            .block_map
            .expect("striped fixture carries an inline map");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }

    async fn read_cold(
        h: &H,
        path: &str,
        zc: &squeezefs::routing::ZcReadServe,
    ) -> squeezefs::error::Result<(
        bytes::Bytes,
        Option<Arc<dyn std::any::Any + Send + Sync>>,
    )> {
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                path,
                4096,
                4096,
                None,
                squeezefs::routing::ReadClassHint::default(),
                None,
                Some(zc),
            )
            .await
    }
    for _ in 0..8 {
        let zc = zc_ready_serve();
        let (data, _) = read_cold(&h, &path, &zc).await.expect("cold zc read");
        assert!(data.is_empty(), "a zc-served read replies with an empty body");
        assert_eq!(zc.served(), Some(4096), "the direct leg must engage");
    }

    if trace_enabled() {
        let zc = zc_ready_serve();
        TRACE.store(true, Ordering::SeqCst);
        let a0 = allocs_now();
        for _ in 0..32 {
            let _ = read_cold(&h, &path, &zc).await;
        }
        let allocs = allocs_now() - a0;
        TRACE.store(false, Ordering::SeqCst);
        println!("traced cold zc READ window: {allocs} allocs / 32 ops");
        print_site_table();
        return;
    }

    const OPS: u64 = 2_000;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let zc = zc_ready_serve();
    let a0 = allocs_now();
    for _ in 0..OPS {
        let _ = read_cold(&h, &path, &zc).await;
    }
    let allocs = allocs_now() - a0;
    assert_eq!(zc.served(), Some(4096), "the direct leg served every op");
    let per_op_centi = allocs * 100 / OPS;
    println!(
        "kernel READ cold zc leg: {allocs} allocs / {OPS} ops = {}.{:02} per op",
        per_op_centi / 100,
        per_op_centi % 100
    );
    assert!(
        per_op_centi <= 200,
        "cold kernel READ zc leg allocates {}.{:02} per op — the R-5 budget \
         is 2.00 (one borrowed key resolve per op, no per-parse Strings, \
         no Vec for a single-block map probe)",
        per_op_centi / 100,
        per_op_centi % 100
    );
}

// ---------------------------------------------------------------------------
// WRITE — the sub-block overwrite of a striped block
// ---------------------------------------------------------------------------

/// **The kernel WRITE lane's per-op allocation budget.**
///
/// Shape: 4 KiB sub-block overwrites of an already-striped block — the W1
/// sole-owner patch shape, the random-small-write population.
///
/// Measured on this box (dev profile, 1,000 ops, counted A-B on the same
/// binary pair):
///   * pre-PERF-12:  **31.15** allocs/op
///   * post-PERF-12: **29.09** allocs/op (30.00 budget)
///   * pre-W-6 (this harness form, 2026-09-08): **14.60** allocs/op
///     (the 16.14 the old harness read minus its own per-op payload
///     mint — see `write_bytes_at`)
///   * post-W-6: **3.00** allocs/op
///
/// What W-6 (e2e perf audit write board #7) removed, per op, all named
/// by the trace mode: the stage-1b write-phase census's scc bucket array
/// (3 — the map shrank to ZERO between writes and re-allocated its array
/// on every `write_phase_begin`; it now keeps a derived minimum capacity),
/// the per-block `active_block:` key (`FsKey` + `String`, 2 → the stack
/// key), the per-block `inode_{ino}` `String` (1 → the stack key), the
/// single-block future `Vec` + `try_join_all`'s boxed slice (2 → the one
/// block future is awaited inline), and the W1 patch's three: its
/// `active_block_ext:` probe key (→ stack key), its owned
/// `parse_block_key` backend id (→ the borrow form) and its block-key
/// `String` clone out of the map (→ borrowed from the map's `Arc`).
///
/// The three that remain live BELOW the handler in the device layer —
/// `PooledBuf::into_bytes`'s `Bytes::from_owner` box (the DMA payload's
/// owner), the uring worker's completion oneshot (`Arc<Shared>`), and the
/// worker thread's per-request record — and are follow-on work. The bar
/// is 4/op: above the shipped number so incidental churn cannot flake
/// it, red on the pre-W-6 tree (14.60).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_kernel_write_prelude_allocation_budget() {
    let h = make().await;
    let ino = striped_fixture(&h, "warm_write.bin").await;

    // Minted once; the first clone promotes the Vec-backed `Bytes` to its
    // shared form OUTSIDE the window, every later clone is a refcount bump
    // (the transport lease's shape).
    let payload = bytes::Bytes::from(vec![0xC4u8; 4096]);
    for _ in 0..8 {
        write_bytes_at(&h, ino, 8192, payload.clone()).await;
    }

    if trace_enabled() {
        TRACE.store(true, Ordering::SeqCst);
        let a0 = allocs_now();
        for _ in 0..32 {
            write_bytes_at(&h, ino, 8192, payload.clone()).await;
        }
        let allocs = allocs_now() - a0;
        TRACE.store(false, Ordering::SeqCst);
        println!("traced kernel WRITE window: {allocs} allocs / 32 ops");
        print_site_table();
        return;
    }

    const OPS: u64 = 1_000;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let a0 = allocs_now();
    for _ in 0..OPS {
        write_bytes_at(&h, ino, 8192, payload.clone()).await;
    }
    let allocs = allocs_now() - a0;
    let per_op_centi = allocs * 100 / OPS;
    println!(
        "kernel WRITE: {allocs} allocs / {OPS} ops = {}.{:02} per op",
        per_op_centi / 100,
        per_op_centi % 100
    );
    assert!(
        per_op_centi <= 400,
        "warm kernel WRITE handler allocates {}.{:02} per op — the W-6 \
         budget is 4.00 (stack keys, a capacity-floored phase census, an \
         inline single-block future, a borrow-only patch prelude; growth \
         past this is a regression, run with SQZ_ALLOC_TRACE=1 for sites)",
        per_op_centi / 100,
        per_op_centi % 100
    );
}
