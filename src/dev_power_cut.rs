//! TEST-1 — the **data-device** power-cut harness (pre-RC engineering
//! spec §11 TEST-1; execution plan §6.1 "the durability spine").
//!
//! [`crate::uring_fs`] carries a correct volatile-cache-loss simulator
//! (`arm_power_cut`/`power_cut`) — it reverts every write not covered by
//! an `fdatasync`. Its coverage is the problem: [`crate::nvme_dev`] runs
//! its own io_uring worker and never passes through that shim, so before
//! this module **no harness in the tree could reach the data device**,
//! while the whole test/benchmark fleet (zram, null_blk, tempfiles) has
//! no volatile write cache. A green gate carried zero durability
//! information.
//!
//! ## The seam
//!
//! A test-only fault seam at the `NvmeBlockDev` worker boundary, armed
//! per device path:
//!
//! * **journal** — armed, the worker records `(offset, len, prior bytes)`
//!   for every write it submits (the bytes a real volatile cache would
//!   drop);
//! * **barrier** — a completed [`crate::nvme_dev::NvmeBlockDev::flush`]
//!   (DUR-2's `Fsync { DATASYNC }` op) covers everything journaled before
//!   it started; covered entries leave the journal and the device's
//!   barrier **epoch** advances;
//! * **cut** — [`power_cut`] restores, in reverse admission order,
//!   everything still uncovered, then settles the bytes. The caller must
//!   have quiesced the device (no in-flight I/O), exactly as a crash
//!   point does.
//!
//! Bootstrapping subtlety, load-bearing for the Phase-2 sequence: until
//! DUR-2 lands there is no barrier op at all, so an armed harness treats
//! **zero** writes as durable — which is exactly the red state the
//! DUR-1/DUR-2 legs require.
//!
//! ## Cost when disarmed
//!
//! One relaxed atomic load per submitted write ([`ARMED`], the
//! `uring_fs::FAULTS_ACTIVE` precedent). Nothing else runs, nothing is
//! allocated, no fd is opened. Arming is available in-process
//! ([`arm_power_cut`], the shim's API shape) or — the
//! `SQUEEZEFS_TEST_WRITE_STALL_MS` precedent, read ONCE per worker and
//! never in production — via `SQUEEZEFS_TEST_POWER_CUT_DEVS`, a
//! comma-separated device-path list armed as each worker starts.
//!
//! ## Coverage law and its limit
//!
//! A barrier covers the writes **journaled before it started**. The
//! durability contract's callers await their writes before barriering
//! (`flush_inode_to_backend`), so this is exact for them; like the
//! `uring_fs` shim ("the serial test harness admits no concurrent writes
//! in the window"), the harness does not model a write that is still
//! in flight when a barrier starts. A test that needs that shape must
//! quiesce the device first.

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Fast disarmed-path guard: `false` (default) ⇒ the seam is completely
/// inert and the worker does no further harness work.
static ARMED: AtomicBool = AtomicBool::new(false);

/// One journaled write: the bytes the device held before it landed.
struct Entry {
    /// Admission sequence (the "push" identity the barrier-epoch
    /// observer answers for).
    seq: u64,
    offset: u64,
    prior: Vec<u8>,
}

/// One completed barrier: everything with `seq < covered_upto` is durable
/// as of `epoch`.
#[derive(Clone, Copy)]
struct BarrierRec {
    epoch: u64,
    covered_upto: u64,
}

#[derive(Default)]
struct DevJournal {
    /// Still-volatile writes, in admission order.
    entries: Vec<Entry>,
    /// Sequence of the next journaled write.
    next_seq: u64,
    /// Completed data-device barriers on this path.
    epoch: u64,
    /// Completed barriers, ascending — the epoch observer's index.
    barriers: Vec<BarrierRec>,
}

static STATE: Lazy<Mutex<HashMap<PathBuf, DevJournal>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Begin volatile-cache tracking on `device_path`: every subsequent write
/// the `NvmeBlockDev` worker submits is captured (original bytes) until a
/// completed [`crate::nvme_dev::NvmeBlockDev::flush`] covers it.
/// [`power_cut`] then reverts whatever is still volatile. Test-only.
pub fn arm_power_cut(device_path: impl AsRef<Path>) {
    STATE
        .lock()
        .unwrap()
        .insert(device_path.as_ref().to_path_buf(), DevJournal::default());
    ARMED.store(true, Ordering::Relaxed);
}

/// Env seam (`SQUEEZEFS_TEST_POWER_CUT_DEVS=<path>[,<path>…]`): arm the
/// listed device paths. Read ONCE as each worker starts — zero cost
/// unset, never set in production (the `SQUEEZEFS_TEST_WRITE_STALL_MS`
/// precedent). Arming an already-armed path is a no-op so a second
/// worker on the same device cannot drop the first one's journal.
pub(crate) fn arm_from_env(device_path: &str) {
    let Ok(list) = std::env::var("SQUEEZEFS_TEST_POWER_CUT_DEVS") else {
        return;
    };
    if !list.split(',').any(|p| p.trim() == device_path) {
        return;
    }
    let mut st = STATE.lock().unwrap();
    st.entry(PathBuf::from(device_path)).or_default();
    ARMED.store(true, Ordering::Relaxed);
}

/// Journal one submitted device write. Called from the `NvmeBlockDev`
/// worker with the exact `(offset, len)` of the SQE, BEFORE it is
/// pushed — the prior bytes must be read while they are still the
/// device's contents.
#[inline]
pub(crate) fn note_write(device_path: &str, offset: u64, len: usize) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    note_write_cold(device_path, offset, len);
}

#[cold]
fn note_write_cold(device_path: &str, offset: u64, len: usize) {
    let path = Path::new(device_path);
    // Read the prior image OUTSIDE the state lock only after confirming
    // the path is tracked (the probe is one map lookup).
    {
        let st = STATE.lock().unwrap();
        if !st.contains_key(path) {
            return;
        }
    }
    let prior = read_prior(path, offset, len);
    let mut st = STATE.lock().unwrap();
    if let Some(j) = st.get_mut(path) {
        let seq = j.next_seq;
        j.next_seq += 1;
        j.entries.push(Entry {
            seq,
            offset,
            prior,
        });
    }
}

/// Snapshot the coverage frontier as a barrier starts: everything
/// journaled so far is covered when that barrier completes. Public so a
/// harness can drive coverage for a device barrier it issues itself.
pub fn mark_barrier_start(device_path: &str) -> u64 {
    if !ARMED.load(Ordering::Relaxed) {
        return 0;
    }
    STATE
        .lock()
        .unwrap()
        .get(Path::new(device_path))
        .map(|j| j.next_seq)
        .unwrap_or(0)
}

/// A barrier completed successfully: retire everything it covered and
/// advance the device's barrier epoch. Pairs with
/// [`mark_barrier_start`].
pub fn complete_barrier(device_path: &str, covered_upto: u64) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let mut st = STATE.lock().unwrap();
    if let Some(j) = st.get_mut(Path::new(device_path)) {
        j.entries.retain(|e| e.seq >= covered_upto);
        j.epoch += 1;
        let epoch = j.epoch;
        j.barriers.push(BarrierRec {
            epoch,
            covered_upto,
        });
    }
}

/// Simulate power loss on `device_path`: revert (in reverse admission
/// order) every journaled write not covered by a completed barrier.
/// Returns how many writes were reverted. The caller must have quiesced
/// the device — exactly as a crash point does.
pub fn power_cut(device_path: impl AsRef<Path>) -> usize {
    let path = device_path.as_ref().to_path_buf();
    let entries = {
        let mut st = STATE.lock().unwrap();
        match st.get_mut(&path) {
            Some(j) => std::mem::take(&mut j.entries),
            None => return 0,
        }
    };
    if entries.is_empty() {
        return 0;
    }
    let count = entries.len();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("dev power_cut: open armed device path");
    // Reverse order restores pre-write bytes under overlapping writes.
    for e in entries.into_iter().rev() {
        use std::os::unix::fs::FileExt;
        f.write_all_at(&e.prior, e.offset)
            .expect("dev power_cut: revert journaled device write");
    }
    f.flush().ok();
    f.sync_data().expect("dev power_cut: settle reverted bytes");
    count
}

/// Writes currently volatile on `device_path` (journaled, uncovered).
pub fn volatile_writes(device_path: impl AsRef<Path>) -> usize {
    STATE
        .lock()
        .unwrap()
        .get(device_path.as_ref())
        .map(|j| j.entries.len())
        .unwrap_or(0)
}

/// Completed data-device barriers on `device_path` since it was armed.
pub fn barrier_epoch(device_path: impl AsRef<Path>) -> u64 {
    STATE
        .lock()
        .unwrap()
        .get(device_path.as_ref())
        .map(|j| j.epoch)
        .unwrap_or(0)
}

/// The sequence the NEXT journaled write on `device_path` will carry —
/// the caller's handle for [`covering_epoch`].
pub fn next_write_seq(device_path: impl AsRef<Path>) -> u64 {
    STATE
        .lock()
        .unwrap()
        .get(device_path.as_ref())
        .map(|j| j.next_seq)
        .unwrap_or(0)
}

/// **The barrier-epoch observer** (DUR-3's question): which barrier
/// covered the write pushed at `seq`? `None` while it is still volatile.
pub fn covering_epoch(device_path: impl AsRef<Path>, seq: u64) -> Option<u64> {
    let st = STATE.lock().unwrap();
    let j = st.get(device_path.as_ref())?;
    j.barriers
        .iter()
        .find(|b| b.covered_upto > seq)
        .map(|b| b.epoch)
}

/// Disarm every path and drop every journal (suite hygiene — the state
/// is process-global, like the `uring_fs` shim's).
pub fn clear_faults() {
    STATE.lock().unwrap().clear();
    ARMED.store(false, Ordering::Relaxed);
}

/// Capture the device's CURRENT bytes at `[offset, offset + len)` — what
/// a power cut would restore. Short reads (a backing file shorter than
/// the write, which the volume geometry forbids) capture zeros, matching
/// `uring_fs::capture_original`.
///
/// Buffered std I/O on purpose: the harness runs only with a fault armed,
/// determinism beats the uring hot path here, and an `O_DIRECT` write
/// invalidates the page-cache range it covers, so a buffered read taken
/// *before* the next write sees the device's true contents.
fn read_prior(path: &Path, offset: u64, len: usize) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let mut prior = vec![0u8; len];
    if let Ok(f) = std::fs::File::open(path) {
        let mut filled = 0usize;
        while filled < len {
            match f.read_at(&mut prior[filled..], offset + filled as u64) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
    }
    prior
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Coverage algebra without any device: pushes before a barrier are
    /// covered by it, pushes after are not.
    #[test]
    fn covering_epoch_tracks_barrier_boundaries() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.as_file().set_len(1 << 20).unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        arm_power_cut(&path);

        let s0 = next_write_seq(&path);
        note_write(&path, 0, 4096);
        assert_eq!(covering_epoch(&path, s0), None);

        let covered = mark_barrier_start(&path);
        complete_barrier(&path, covered);
        assert_eq!(barrier_epoch(&path), 1);
        assert_eq!(covering_epoch(&path, s0), Some(1));
        assert_eq!(volatile_writes(&path), 0);

        let s1 = next_write_seq(&path);
        note_write(&path, 4096, 4096);
        assert_eq!(covering_epoch(&path, s1), None, "post-barrier push");
        assert_eq!(volatile_writes(&path), 1);

        clear_faults();
        assert_eq!(volatile_writes(&path), 0);
    }

    /// Disarmed, nothing is journaled — the zero-cost-when-off law.
    #[test]
    fn disarmed_seam_journals_nothing() {
        clear_faults();
        note_write("/nonexistent/device", 0, 4096);
        assert_eq!(volatile_writes("/nonexistent/device"), 0);
        assert_eq!(power_cut("/nonexistent/device"), 0);
    }
}
