//! **S9-c — co-located device fencing** (rung 10; design-full-multi-writer
//! PR row 10; operator story `docs/operations.md` §Multi-writer co-writer
//! mounts, the co-located honest residual).
//!
//! The co-located shape shares the PR host identity (rung-9 finding #1's
//! adoption: a co-located co-writer ADOPTS the authority's standing WERO
//! hold instead of registering its own key, because the merged multipath
//! head round-robins PR mutations), so **NVMe Persistent Reservations
//! cannot discriminate between co-located mounts**: a fenced co-located
//! co-writer's DMA presents the HOLDER's own registration and the device
//! accepts it. `classify_dma_outcome` — the S7 device-rejection arm — is
//! structurally unreachable for this shape (no reservation-conflict errno
//! can ever fire).
//!
//! The fencing story for a fenced CO-LOCATED co-writer therefore rests
//! entirely on the CLIENT-side gates, and this file is the proof they
//! compose — the composition no other suite pins as one story:
//!
//! 1. **the epoch gate rejects where the device cannot**: a DMA carrying
//!    an authorization whose custody epoch is no longer current is refused
//!    at [`squeezefs::data_custody::authorize_dma`] BEFORE any device
//!    work, with the class split (`data_dma_epoch_refusals` ⊆
//!    `data_dma_fence_refusals`) and the device bytes untouched;
//! 2. **the device-fence latch never fires**: the refusal is not
//!    `classify_dma_outcome`'s — the device never spoke — so
//!    `NvmeBlockDev::fenced()` stays false throughout (the S7 arm and the
//!    S9-c arm are DIFFERENT doors, and this shape proves the second one
//!    alone suffices);
//! 3. **custody movement is not mount death**: a FRESH authorization
//!    under the current epoch still lands (an epoch advance retires old
//!    authorizations and never poisons — "a client that can re-join is
//!    not a fenced zombie");
//! 4. **the self-fence face is total**: once T_self poisons process
//!    custody, even fresh authorizations refuse — counted on the FENCE
//!    class, not the epoch split (the split stays honest: epoch counts
//!    custody that MOVED, poison counts a mount that DIED).
//!
//! The live half is `tests/run_mw_matrix.sh s9-colocated-fence`: a real
//! co-located co-writer SIGSTOPped past the authority's TTL, whose resumed
//! DMA is refused by these gates while its daemon log carries NO
//! device-rejection line and the fsck/C8 oracle stays clean.
//!
//! Process-global state (custody generation, poison latch, METRICS):
//! serialized and restored, the `dma_fence_gate_tests` discipline.

use squeezefs::data_custody;
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::METRICS;
use squeezefs::nvme_dev::NvmeBlockDev;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::NamedTempFile;

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

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        data_custody::test_clear_poison();
        data_custody::test_reset_custody_generation();
    }
}

fn fence_refusals() -> u64 {
    METRICS.data_dma_fence_refusals.load(Ordering::Relaxed)
}

fn epoch_refusals() -> u64 {
    METRICS.data_dma_epoch_refusals.load(Ordering::Relaxed)
}

/// A device that ALWAYS accepts — the co-located merged-head shape: the
/// zombie's registration IS the holder's, so a reservation conflict is
/// structurally impossible and `classify_dma_outcome` can never latch.
fn accepting_device() -> (NamedTempFile, NvmeBlockDev) {
    let f = NamedTempFile::new().expect("backing file");
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let dev = NvmeBlockDev::new(f.path().to_str().unwrap());
    (f, dev)
}

fn bytes_at(path: &std::path::Path, offset: u64, len: usize) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).expect("read backing");
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).unwrap();
    buf
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_co_located_zombies_dma_is_refused_by_the_epoch_gate_where_the_device_cannot() {
    let _serial = serial();
    let _restore = Restore;
    data_custody::test_clear_poison();
    data_custody::test_reset_custody_generation();
    let (f, dev) = accepting_device();

    // Custody established: the authorization epoch is captured HERE (the
    // write-pipeline permit / S9 remote-grant shape).
    let auth = data_custody::authorize_dma(None).expect("custody is healthy");

    // Under the current epoch the co-located co-writer writes — the device
    // accepts (it cannot do otherwise for this identity).
    dev.write_block_authorized(0, bytes::Bytes::from(vec![0xAA; 4096]), auth)
        .await
        .expect("a current-epoch DMA lands");
    assert_eq!(bytes_at(f.path(), 0, 4), vec![0xAA; 4], "the bytes landed");

    // Custody MOVES under the in-flight authorization (revocation /
    // successor era — the authority's act, observed via the pull channel).
    data_custody::advance_custody_generation(
        "s9-c: the authority revoked this mount's custody (test)",
    );

    // The zombie's DMA: the DEVICE would accept it — the gate must not.
    let fence_before = fence_refusals();
    let epoch_before = epoch_refusals();
    let err = dev
        .write_block_authorized(4096, bytes::Bytes::from(vec![0xBB; 4096]), auth)
        .await
        .expect_err("a dead-epoch DMA must be refused BEFORE the device");
    assert!(
        matches!(err, SqueezefsError::WriterGuardFenced),
        "the refusal class is the fence's (got {err})"
    );
    assert_eq!(
        bytes_at(f.path(), 4096, 4096),
        vec![0u8; 4096],
        "not one byte reached the device — the refusal is at the authorization point"
    );
    assert_eq!(
        fence_refusals() - fence_before,
        1,
        "counted on the fence tripwire"
    );
    assert_eq!(
        epoch_refusals() - epoch_before,
        1,
        "and split into the epoch class (custody MOVED — data_dma_epoch_refusals)"
    );
    assert!(
        !dev.fenced(),
        "the DEVICE fence latch never fired: no reservation conflict exists for a \
         co-located identity — the epoch gate alone carried the fencing story"
    );
    assert!(
        !data_custody::poisoned(),
        "an epoch advance never poisons — a client that can re-join is not a fenced zombie"
    );

    // Custody re-established (the re-join): a FRESH authorization lands.
    let fresh = data_custody::authorize_dma(None).expect("fresh custody authorizes");
    dev.write_block_authorized(8192, bytes::Bytes::from(vec![0xCC; 4096]), fresh)
        .await
        .expect("a re-joined mount's DMA lands");
    assert_eq!(bytes_at(f.path(), 8192, 4), vec![0xCC; 4]);

    // The self-fence face (T_self / UnknownLease past deadline): total,
    // and counted on the FENCE class alone — the split stays honest.
    data_custody::poison("s9-c: T_self fired (test)");
    let fence_before = fence_refusals();
    let epoch_before = epoch_refusals();
    let err = dev
        .write_block_authorized(12288, bytes::Bytes::from(vec![0xDD; 4096]), fresh)
        .await
        .expect_err("a poisoned mount submits nothing");
    assert!(matches!(err, SqueezefsError::WriterGuardFenced));
    assert!(
        data_custody::authorize_dma(None).is_err(),
        "poison refuses even fresh authorizations (dead until remount)"
    );
    assert_eq!(bytes_at(f.path(), 12288, 4096), vec![0u8; 4096]);
    assert!(fence_refusals() - fence_before >= 2);
    assert_eq!(
        epoch_refusals(),
        epoch_before,
        "poison is NOT the epoch class: the split separates 'custody moved' from 'this \
         mount died', and S9-c's live leg reads both"
    );
    assert!(
        !dev.fenced(),
        "still no device-side latch — the whole story was client-side, which is the \
         co-located shape's ONLY fencing story"
    );
}
