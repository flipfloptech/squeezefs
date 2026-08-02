//! TEST-1 — the **data-device** power-cut harness (pre-RC engineering spec
//! §11 TEST-1; execution plan §6.1).
//!
//! `uring_fs::arm_power_cut`/`power_cut` is a correct volatile-cache-loss
//! simulator pointed at the metadata plane only: `NvmeBlockDev` runs its
//! own io_uring worker and never passes through that shim, so before this
//! suite **no harness in the tree could reach the data device** — and the
//! whole test/benchmark fleet (zram, null_blk, tempfiles) has no volatile
//! write cache, so a green gate carried zero durability information.
//!
//! These are the harness's own self-tests: the harness must be
//! trustworthy before it can pin anything (the `crash_contract_tests.rs`
//! shim-self-test precedent). The contract:
//!
//! * armed, the worker journals `(offset, len, prior bytes)` per write;
//! * [`power_cut`] restores everything not covered by a **completed
//!   barrier** op (`NvmeBlockDev::flush`, DUR-2);
//! * the barrier-epoch observer answers "which barrier covered this
//!   push?" (the DUR-3 checkpoint-concurrency leg's question);
//! * disarmed, the seam is completely inert.
//!
//! Bootstrapping note (load-bearing for the Phase-2 sequence): until
//! DUR-2 lands there is no data-device barrier at all, so an armed
//! harness treats *zero* writes as durable — exactly the red state the
//! DUR-1/DUR-2 legs need.

use squeezefs::dev_power_cut;
use squeezefs::nvme_dev::NvmeBlockDev;
use tempfile::NamedTempFile;

/// RAII: harness state is process-global; never leak it across tests.
struct CutGuard;
impl Drop for CutGuard {
    fn drop(&mut self) {
        dev_power_cut::clear_faults();
    }
}

fn backing(len: u64) -> NamedTempFile {
    let f = NamedTempFile::new().expect("backing tempfile");
    f.as_file().set_len(len).expect("size backing tempfile");
    f
}

/// Armed, an un-flushed device write is volatile: `power_cut` reverts it
/// and the device reads back its ORIGINAL bytes.
#[tokio::test]
async fn test_unflushed_data_device_write_is_reverted() {
    let tmp = backing(16 * 1024 * 1024);
    let path = tmp.path().to_str().unwrap().to_string();
    let dev = NvmeBlockDev::new(&path);
    let _g = CutGuard;

    let off = 1024 * 1024;
    dev_power_cut::arm_power_cut(&path);
    dev.write_block(off, bytes::Bytes::from(vec![0xABu8; 4096]))
        .await
        .expect("device write");

    // Pre-cut the device serves the new bytes.
    let live = dev.read_block(off, 4096).await.expect("read back");
    assert_eq!(&live[..], &[0xABu8; 4096][..], "write must land pre-cut");

    let reverted = dev_power_cut::power_cut(&path);
    assert_eq!(
        reverted, 1,
        "exactly one volatile data-device write must be reverted"
    );

    let after = dev.read_block(off, 4096).await.expect("read after cut");
    assert_eq!(
        &after[..],
        &[0u8; 4096][..],
        "un-flushed device bytes must not survive a data-device power cut"
    );
}

/// A `flush()`ed write survives the cut — the barrier covers it. This is
/// the DUR-2 primitive seen from the harness side.
#[tokio::test]
async fn test_flushed_data_device_write_survives_power_cut() {
    let tmp = backing(16 * 1024 * 1024);
    let path = tmp.path().to_str().unwrap().to_string();
    let dev = NvmeBlockDev::new(&path);
    let _g = CutGuard;

    let off = 2 * 1024 * 1024;
    dev_power_cut::arm_power_cut(&path);
    dev.write_block(off, bytes::Bytes::from(vec![0x5Au8; 4096]))
        .await
        .expect("device write");
    dev.flush().await.expect("data-device barrier");

    let reverted = dev_power_cut::power_cut(&path);
    assert_eq!(
        reverted, 0,
        "a barriered write is durable — nothing to revert"
    );

    let after = dev.read_block(off, 4096).await.expect("read after cut");
    assert_eq!(
        &after[..],
        &[0x5Au8; 4096][..],
        "flushed device bytes must survive a data-device power cut"
    );
}

/// Only the writes admitted AFTER the last barrier are volatile: the
/// covered prefix survives, the tail is reverted.
#[tokio::test]
async fn test_power_cut_reverts_only_the_uncovered_tail() {
    let tmp = backing(16 * 1024 * 1024);
    let path = tmp.path().to_str().unwrap().to_string();
    let dev = NvmeBlockDev::new(&path);
    let _g = CutGuard;

    let a = 4 * 1024 * 1024;
    let b = 5 * 1024 * 1024;
    dev_power_cut::arm_power_cut(&path);

    dev.write_block(a, bytes::Bytes::from(vec![0x11u8; 4096]))
        .await
        .expect("covered write");
    dev.flush().await.expect("barrier");
    dev.write_block(b, bytes::Bytes::from(vec![0x22u8; 4096]))
        .await
        .expect("volatile write");

    assert_eq!(
        dev_power_cut::volatile_writes(&path),
        1,
        "only the post-barrier write is volatile"
    );
    assert_eq!(dev_power_cut::power_cut(&path), 1);

    assert_eq!(
        &dev.read_block(a, 4096).await.unwrap()[..],
        &[0x11u8; 4096][..],
        "the barriered write must survive"
    );
    assert_eq!(
        &dev.read_block(b, 4096).await.unwrap()[..],
        &[0u8; 4096][..],
        "the post-barrier write must be reverted"
    );
}

/// Overlapping volatile writes revert in reverse admission order — the
/// device reads back the bytes it held before the FIRST of them.
#[tokio::test]
async fn test_overlapping_volatile_writes_revert_to_prior_image() {
    let tmp = backing(16 * 1024 * 1024);
    let path = tmp.path().to_str().unwrap().to_string();
    let dev = NvmeBlockDev::new(&path);
    let _g = CutGuard;

    let off = 6 * 1024 * 1024;
    // Durable baseline (written before arming).
    dev.write_block(off, bytes::Bytes::from(vec![0x01u8; 8192]))
        .await
        .unwrap();

    dev_power_cut::arm_power_cut(&path);
    dev.write_block(off, bytes::Bytes::from(vec![0x02u8; 8192]))
        .await
        .unwrap();
    dev.write_block(off + 4096, bytes::Bytes::from(vec![0x03u8; 4096]))
        .await
        .unwrap();

    assert_eq!(dev_power_cut::power_cut(&path), 2);
    let after = dev.read_block(off, 8192).await.unwrap();
    assert_eq!(
        &after[..],
        &[0x01u8; 8192][..],
        "overlapping volatile writes must revert to the pre-arm image"
    );
}

/// The barrier-epoch observer (DUR-3's leg): every journaled write can be
/// asked which barrier covered it — `None` while volatile, the completed
/// barrier's epoch afterwards.
#[tokio::test]
async fn test_barrier_epoch_observer_names_the_covering_barrier() {
    let tmp = backing(16 * 1024 * 1024);
    let path = tmp.path().to_str().unwrap().to_string();
    let dev = NvmeBlockDev::new(&path);
    let _g = CutGuard;

    dev_power_cut::arm_power_cut(&path);
    let e0 = dev_power_cut::barrier_epoch(&path);
    assert_eq!(e0, 0, "a freshly armed device starts at epoch 0");

    let seq = dev_power_cut::next_write_seq(&path);
    dev.write_block(7 * 1024 * 1024, bytes::Bytes::from(vec![0x77u8; 4096]))
        .await
        .unwrap();
    assert_eq!(
        dev_power_cut::covering_epoch(&path, seq),
        None,
        "an un-barriered push is covered by no epoch"
    );

    dev.flush().await.expect("barrier");
    assert_eq!(
        dev_power_cut::barrier_epoch(&path),
        1,
        "a completed barrier advances the epoch"
    );
    assert_eq!(
        dev_power_cut::covering_epoch(&path, seq),
        Some(1),
        "the push is covered by barrier epoch 1"
    );

    // A push AFTER that barrier is not covered by it.
    let seq2 = dev_power_cut::next_write_seq(&path);
    dev.write_block(8 * 1024 * 1024, bytes::Bytes::from(vec![0x88u8; 4096]))
        .await
        .unwrap();
    assert_eq!(dev_power_cut::covering_epoch(&path, seq2), None);
    dev.flush().await.expect("second barrier");
    assert_eq!(dev_power_cut::covering_epoch(&path, seq2), Some(2));
}

/// Disarmed, the seam is inert: nothing is journaled and `power_cut` is a
/// no-op that never touches the device (the zero-cost-when-off law).
#[tokio::test]
async fn test_disarmed_seam_is_inert() {
    let tmp = backing(16 * 1024 * 1024);
    let path = tmp.path().to_str().unwrap().to_string();
    let dev = NvmeBlockDev::new(&path);
    let _g = CutGuard;

    let off = 9 * 1024 * 1024;
    dev.write_block(off, bytes::Bytes::from(vec![0xCDu8; 4096]))
        .await
        .unwrap();

    assert_eq!(dev_power_cut::volatile_writes(&path), 0);
    assert_eq!(
        dev_power_cut::power_cut(&path),
        0,
        "an unarmed path reverts nothing"
    );
    assert_eq!(
        &dev.read_block(off, 4096).await.unwrap()[..],
        &[0xCDu8; 4096][..],
        "an unarmed device must never be rewritten by the harness"
    );
}

/// `clear_faults` disarms every path (the suite-hygiene primitive).
#[tokio::test]
async fn test_clear_faults_disarms() {
    let tmp = backing(16 * 1024 * 1024);
    let path = tmp.path().to_str().unwrap().to_string();
    let dev = NvmeBlockDev::new(&path);

    dev_power_cut::arm_power_cut(&path);
    dev.write_block(10 * 1024 * 1024, bytes::Bytes::from(vec![0xEEu8; 4096]))
        .await
        .unwrap();
    assert_eq!(dev_power_cut::volatile_writes(&path), 1);

    dev_power_cut::clear_faults();
    assert_eq!(
        dev_power_cut::volatile_writes(&path),
        0,
        "clear_faults drops every journal"
    );
    assert_eq!(dev_power_cut::power_cut(&path), 0);
    assert_eq!(
        &dev.read_block(10 * 1024 * 1024, 4096).await.unwrap()[..],
        &[0xEEu8; 4096][..],
        "cleared faults must leave the device untouched"
    );
}
