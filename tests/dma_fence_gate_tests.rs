//! RES-6 (pre-RC engineering spec §7): a FENCED daemon must not issue
//! data-plane DMA.
//!
//! The D0 fail-stop lattice fences a usurped writer at its first
//! post-fence journal barrier, and `src/block_reclaim.rs` already
//! observes that latch — a fenced zombie can never issue another
//! destructive discard (contract 6 of `async_block_reclaim_tests`). The
//! WRITE path had no such predicate: NVMe reservations cover metadata
//! volumes only, and `nvme_dev.rs` submitted every `write_block`
//! unconditionally. `write_pipeline_fence_drops` does NOT close this —
//! it classifies a DLM *token* expiry, not the D0 `failed` latch, so a
//! zombie holding a locally-valid token kept DMA-ing over offsets the
//! successor writer had replayed and reallocated.
//!
//! This is the LOCAL face only: one relaxed load per submit against the
//! same latch the reclaimer reads. The cross-host face — a
//! data-namespace NVMe reservation for the mount lifetime — is DLM stage
//! S7 and is deliberately NOT built here.
//!
//! Contracts pinned here:
//!
//! 1. **Gated**: once the probe latches, `write_block` refuses loudly
//!    with `WriterGuardFenced` and submits nothing.
//! 2. **Counted**: every refusal bumps `data_dma_fence_refusals` (0 on
//!    healthy mounts — investigate alongside `writer_guard_fenced`).
//! 3. **Sticky**: the latch never clears in-process (a fenced holder is
//!    dead until remount), and it is observed even if the probe stops
//!    reporting.
//! 4. **Reads are unaffected**: a fenced holder reading its own device
//!    corrupts nothing, and refusing reads would only turn a fail-stop
//!    into a hang.
//! 5. **Disposition**: the write pipeline classifies the refusal as a
//!    FENCE DROP, not a retry — a fenced holder that retried forever
//!    would park custody until OOM (the W5 law: publish nothing, free
//!    nothing, successor accounting owns it).
//!
//! RED against dev 7d1ec2e1: `NvmeBlockDev::set_fence_signal` does not
//! exist and `write_block` has no fence predicate.
//!
//! DLM **S7** amendment: the submit gate now RETIRES process-wide
//! data-plane custody on the first latch observation
//! (`data_custody::poison` — the D0 fail-stop lattice is mount-wide, so a
//! sibling volume must not have to evaluate the same probe before it stops
//! writing). Poison is sticky, so the fencing tests here serialize and
//! clear it on the way out; the contracts themselves are unchanged.

use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::METRICS;
use squeezefs::nvme_dev::NvmeBlockDev;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::NamedTempFile;

/// S7: the poison latch is process-global and sticky — a test that fences
/// must not fence its neighbours. Serialize (an atomic latch: the guard is
/// held across `.await` points by design), and clear on the way out.
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

struct PoisonGuard;
impl Drop for PoisonGuard {
    fn drop(&mut self) {
        squeezefs::data_custody::test_clear_poison();
    }
}

fn refusals() -> u64 {
    METRICS.data_dma_fence_refusals.load(Ordering::Relaxed)
}

fn backing() -> (NamedTempFile, NvmeBlockDev) {
    let f = NamedTempFile::new().expect("backing file");
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let dev = NvmeBlockDev::new(f.path().to_str().unwrap());
    (f, dev)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fenced_writer_guard_refuses_data_plane_dma() {
    let _serial = serial();
    let _poison = PoisonGuard;
    let (_f, dev) = backing();
    let fenced = Arc::new(AtomicBool::new(false));
    let probe = fenced.clone();
    dev.set_fence_signal(Arc::new(move || probe.load(Ordering::Relaxed)));

    let payload = bytes::Bytes::from(vec![0xABu8; 4096]);

    // Unfenced: the write lands and nothing is counted.
    let before = refusals();
    dev.write_block(0, payload.clone())
        .await
        .expect("an unfenced write must land");
    assert_eq!(refusals(), before, "no refusal on a healthy mount");
    assert!(!dev.fenced(), "the latch must not fire without the probe");

    // Fence the mount (the D0 fail-stop latch the reclaimer reads).
    fenced.store(true, Ordering::Relaxed);

    let err = dev
        .write_block(4096, payload.clone())
        .await
        .expect_err("RES-6: a fenced daemon must not issue data-plane DMA");
    assert!(
        matches!(err, SqueezefsError::WriterGuardFenced),
        "the refusal must be loud and classifiable, got {err:?}"
    );
    assert_eq!(refusals(), before + 1, "every refusal is counted");

    // Sticky: the latch survives the probe going quiet.
    fenced.store(false, Ordering::Relaxed);
    let err = dev
        .write_block(8192, payload.clone())
        .await
        .expect_err("the fence latch is permanent in-process");
    assert!(matches!(err, SqueezefsError::WriterGuardFenced));
    assert!(dev.fenced(), "latched");

    // Reads stay open: a fenced holder reading corrupts nothing, and
    // refusing would turn a fail-stop into a hang.
    let got = dev
        .read_block(0, 4096)
        .await
        .expect("reads must not be fenced");
    assert_eq!(&got[..], &payload[..], "the pre-fence write is readable");
}

/// The refusal must be a FENCE DROP for the write pipeline, never a
/// retry: a fenced holder that retried forever would park custody until
/// the R5 budget went Red and stayed there.
#[test]
fn fenced_dma_refusal_is_a_fence_drop_not_a_retry() {
    use squeezefs::write_pipeline::{pipeline_disposition, PipelineDisposition};
    let res: Result<(), SqueezefsError> = Err(SqueezefsError::WriterGuardFenced);
    assert!(
        matches!(pipeline_disposition(&res), PipelineDisposition::FenceDrop),
        "RES-6: a fenced-guard refusal must drop custody (W5), not stay parked"
    );
}

/// The fence signal is shared across clones — one device, one latch,
/// however many handles the router holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_latch_is_shared_across_device_clones() {
    let _serial = serial();
    let _poison = PoisonGuard;
    let (_f, dev) = backing();
    let fenced = Arc::new(AtomicBool::new(true));
    let probe = fenced.clone();
    dev.set_fence_signal(Arc::new(move || probe.load(Ordering::Relaxed)));
    let clone = dev.clone();

    let err = clone
        .write_block(0, bytes::Bytes::from(vec![1u8; 4096]))
        .await
        .expect_err("a clone must observe the same latch");
    assert!(matches!(err, SqueezefsError::WriterGuardFenced));
    assert!(dev.fenced(), "the latch is shared, not per-handle");
}
