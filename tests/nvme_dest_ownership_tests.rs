//! MEM-1 (pre-rc engineering spec §2, P0): zero-copy read destinations
//! must convey ownership.
//!
//! The `dest_addr` read arm DMAs device bytes straight into a registered
//! transport payload buffer. Before the fix, the 30 s caller timeout (or a
//! dropped future) abandoned the in-flight SQE: the READ handler replied,
//! the transport's COMMIT_AND_FETCH re-armed the payload buffer for a NEW
//! kernel request, and the late DMA landed in someone else's buffer —
//! cross-request corruption.
//!
//! The contract pinned here: at request build the read path CLAIMS an
//! owner token for the destination window; the WORKER holds that token for
//! exactly the SQE's lifetime and drops it at CQE completion. Composed
//! with the transport's §5.4 lease gate (`EntLeaseState` — the REAL
//! shipped type, exercised directly in the composition legs), a held
//! token parks the ent's COMMIT_AND_FETCH, so the buffer is never
//! re-armed while an SQE can still write it.
//!
//! Device stalls are selected deterministically via the
//! `set_test_read_stall` seam (the `SQUEEZEFS_TEST_WRITE_STALL_MS`
//! precedent: load selects such schedules; the seam selects them
//! deterministically), and the caller timeout is shortened via
//! `set_test_read_timeout_ms` so the legs run in test time. Run with
//! `--test-threads=1` (process-global seams).

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fuse3::raw::connection::fuse_over_uring::lease_core::{CommitGate, EntLeaseState};
use squeezefs::nvme_dev::{self, NvmeBlockDev};

/// 4 KiB-aligned stand-in for one registered transport payload buffer
/// (O_DIRECT-capable temp dirs need the alignment; buffered fallback does
/// not care).
struct AlignedRegion {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for AlignedRegion {}
unsafe impl Sync for AlignedRegion {}

impl AlignedRegion {
    fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, 4096).expect("region layout");
        // SAFETY: non-zero size, valid layout; zeroed so asserts on
        // untouched bytes are deterministic.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "region allocation failed");
        Self { ptr, len }
    }

    fn addr(&self) -> u64 {
        self.ptr as u64
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `[0, len)` is our own live allocation.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for AlignedRegion {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.len, 4096).expect("region layout");
        // SAFETY: allocated in `new` with the same layout.
        unsafe { std::alloc::dealloc(self.ptr, layout) };
    }
}

/// Token the fake resolver hands out: bumps `drops` when the worker
/// releases it, and releases the (real) transport lease state it holds —
/// exactly the `DestDmaLease` drop shape.
struct TokenProbe {
    drops: Arc<AtomicUsize>,
    lease: Arc<EntLeaseState>,
}

impl Drop for TokenProbe {
    fn drop(&mut self) {
        self.lease.release();
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct Rig {
    /// Keeps the resolver's liveness anchor alive for the test's duration.
    _anchor: Arc<dyn std::any::Any + Send + Sync>,
    claims: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    /// The REAL §5.4 gate word the tokens acquire/release — the transport
    /// commit gate the composition legs interrogate.
    lease: Arc<EntLeaseState>,
}

/// Register a resolver over `region` whose tokens ride the real
/// `EntLeaseState` protocol (acquire at claim, release at drop).
fn register_region(region: &AlignedRegion) -> Rig {
    let anchor: Arc<dyn std::any::Any + Send + Sync> = Arc::new(());
    let claims = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let lease = Arc::new(EntLeaseState::new());

    let base = region.addr();
    let span = region.len;
    let claims_c = Arc::clone(&claims);
    let drops_c = Arc::clone(&drops);
    let lease_c = Arc::clone(&lease);
    nvme_dev::register_dest_resolver(
        Arc::downgrade(&anchor),
        Arc::new(move |addr, len| {
            let end = addr.checked_add(len as u64)?;
            if addr < base || end > base + span as u64 {
                return None;
            }
            lease_c.acquire();
            claims_c.fetch_add(1, Ordering::SeqCst);
            Some(Box::new(TokenProbe {
                drops: Arc::clone(&drops_c),
                lease: Arc::clone(&lease_c),
            }) as nvme_dev::DestToken)
        }),
    );

    Rig {
        _anchor: anchor,
        claims,
        drops,
        lease,
    }
}

/// Backing file with a deterministic byte pattern.
fn pattern_file(len: usize) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("temp backing file");
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    f.write_all(&data).expect("write pattern");
    f.flush().expect("flush pattern");
    f
}

fn reset_seams() {
    nvme_dev::set_test_read_stall(0, 0);
    nvme_dev::set_test_read_timeout_ms(30_000);
}

async fn wait_for(counter: &AtomicUsize, want: usize, deadline: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < deadline {
        if counter.load(Ordering::SeqCst) >= want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    counter.load(Ordering::SeqCst) >= want
}

/// The MEM-1 acceptance leg: a device stall past the caller timeout must
/// NOT release the destination — the worker holds the owner token until
/// the late CQE lands, so the transport re-arm path (which waits on the
/// token's lease) cannot hand the buffer to a new request while the SQE
/// still references it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_keeps_dest_token_held_until_late_completion() {
    let file = pattern_file(8192);
    let region = AlignedRegion::new(4096);
    let rig = register_region(&region);

    nvme_dev::set_test_read_timeout_ms(200);
    nvme_dev::set_test_read_stall(1, 1_200);

    let dev = NvmeBlockDev::new(file.path().to_str().unwrap());
    let res = dev.read_block_with_dest(0, 4096, Some(region.addr())).await;

    let err = res.expect_err("stalled read must time out, not succeed");
    assert!(
        err.to_string().contains("timed out"),
        "expected the read-timeout error, got: {err}"
    );

    // The ownership property (red before the fix): the read path claimed
    // an owner token for the destination…
    assert_eq!(
        rig.claims.load(Ordering::SeqCst),
        1,
        "read path must claim the dest owner token at request build (MEM-1)"
    );
    // …and the worker still holds it — the SQE is still in flight, so a
    // release here would let the ent re-arm into a live DMA.
    assert_eq!(
        rig.drops.load(Ordering::SeqCst),
        0,
        "dest token released while the SQE was still in flight — the \
         transport could re-arm the payload buffer into a live DMA"
    );
    // Transport-gate composition: with the token live, the ent's
    // COMMIT_AND_FETCH must PARK (no re-arm), exactly the §5.4 write-lease
    // discipline.
    assert!(
        matches!(rig.lease.try_commit(), CommitGate::Parked),
        "commit gate must park (no re-arm) while a dest token holds the lease"
    );

    // Liveness: when the stalled device finally completes, the worker
    // drops the token and the parked commit becomes releasable.
    assert!(
        wait_for(&rig.drops, 1, Duration::from_secs(5)).await,
        "worker never released the dest token after the late completion"
    );
    assert!(
        rig.lease.try_unpark(),
        "parked commit not releasable after the token dropped"
    );

    // The late DMA landed in memory the request still owned (the whole
    // point): the pattern is in OUR region, not someone else's buffer.
    let expect: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        region.as_slice(),
        &expect[..],
        "late DMA did not land in the owned destination region"
    );

    reset_seams();
}

/// Dropping the read future mid-flight (no timeout involved) is the same
/// abandonment face: the token must stay worker-held until the CQE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_future_keeps_dest_token_held() {
    let file = pattern_file(8192);
    let region = AlignedRegion::new(4096);
    let rig = register_region(&region);

    nvme_dev::set_test_read_timeout_ms(30_000);
    nvme_dev::set_test_read_stall(1, 800);

    let dev = NvmeBlockDev::new(file.path().to_str().unwrap());
    let fut = dev.read_block_with_dest(0, 4096, Some(region.addr()));
    // Poll long enough to enqueue (the claim + try_send run on the first
    // poll), then DROP the future while the worker is stalled.
    let raced = tokio::time::timeout(Duration::from_millis(100), fut).await;
    assert!(raced.is_err(), "read must still be in flight at drop time");

    assert_eq!(
        rig.claims.load(Ordering::SeqCst),
        1,
        "read path must claim the dest owner token at request build (MEM-1)"
    );
    assert_eq!(
        rig.drops.load(Ordering::SeqCst),
        0,
        "dropping the caller future must not release the destination — \
         the worker owns the token for the SQE's lifetime"
    );
    assert!(
        matches!(rig.lease.try_commit(), CommitGate::Parked),
        "commit gate must park while the dropped read's SQE is in flight"
    );

    assert!(
        wait_for(&rig.drops, 1, Duration::from_secs(5)).await,
        "worker never released the dest token after the late completion"
    );
    assert!(rig.lease.try_unpark());

    reset_seams();
}

/// Happy path: a completed dest read releases its token promptly (before
/// the caller can reply), so the transport gate is Ready at reply time —
/// no spurious parking tax on the hot path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_read_releases_token_promptly() {
    let file = pattern_file(8192);
    let region = AlignedRegion::new(4096);
    let rig = register_region(&region);

    reset_seams();

    let dev = NvmeBlockDev::new(file.path().to_str().unwrap());
    let out = dev
        .read_block_with_dest(0, 4096, Some(region.addr()))
        .await
        .expect("read");
    let expect: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    assert_eq!(out.as_ref(), &expect[..], "dest read served wrong bytes");

    assert_eq!(
        rig.claims.load(Ordering::SeqCst),
        1,
        "read path must claim the dest owner token at request build (MEM-1)"
    );
    // The worker drops the token BEFORE completing the caller oneshot, so
    // by the time the handler could reply the gate is already Ready.
    assert!(
        wait_for(&rig.drops, 1, Duration::from_millis(500)).await,
        "completed read must release its dest token promptly"
    );
    assert!(
        matches!(rig.lease.try_commit(), CommitGate::Ready),
        "commit gate must be Ready once the CQE landed and the token dropped"
    );
}

/// Pooled (no-dest) reads never claim: the pool buffer's ownership already
/// moves into the request (the correct arm the spec contrasts against).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pooled_reads_do_not_claim_tokens() {
    let file = pattern_file(8192);
    let region = AlignedRegion::new(4096);
    let rig = register_region(&region);

    reset_seams();

    let dev = NvmeBlockDev::new(file.path().to_str().unwrap());
    let out = dev.read_block(4096, 4096).await.expect("pooled read");
    let expect: Vec<u8> = (4096..8192).map(|i| (i % 251) as u8).collect();
    assert_eq!(out.as_ref(), &expect[..]);

    assert_eq!(
        rig.claims.load(Ordering::SeqCst),
        0,
        "pooled reads must not claim dest tokens (ownership already moves \
         with the PooledBuf)"
    );
}
