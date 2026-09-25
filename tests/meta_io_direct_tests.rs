//! PR 13i F-C1 — shared-LUN metadata I/O is `O_DIRECT` and every shape has
//! an aligned form (design-symmetric-metadata §5.12).
//!
//! The two-host pin itself (`tests/run_mw_matrix.sh sym-two-host`) needs
//! two kernels on one LUN; these contracts pin the MECHANISM in-process:
//! the registration's posture and derived grain, the worker's alignment
//! discipline (a misaligned direct write is a refused caller bug, a
//! misaligned buffer is bounced, a narrow read is served out of a widened
//! span), the journal ring's sector-pad law (every reservation ends on a
//! physical sector boundary, the pad is a checksummed PAD entry replay
//! walks as a chain link), and the writer's head alignment over a ring
//! written unpadded before this binary.
//!
//! Suite runs `--test-threads=1` (the registry and the gauges are
//! process-global).

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options, ROOT_INO};
use squeezefs::meta_backend::kv::journal::{
    checkpoint_reserve_bytes, entry_len_for, JournalRing, JOURNAL_PAGE_DATA_LEN,
    JOURNAL_PAGE_HDR_LEN, JOURNAL_PAGE_LEN,
};
use squeezefs::meta_backend::kv::journal_core::{AdmissionClass, CoreGeometry, PAD_MIN};
use squeezefs::meta_backend::kv::record::{
    forest_key, inode_key, xattr_key, xattr_name_hash56, Record, XattrValue, TREE_INODES,
    TREE_XATTRS,
};
use squeezefs::meta_backend::kv::{META_KV_JOURNAL_PAD_BYTES, META_KV_JOURNAL_PAD_ENTRIES};
use squeezefs::meta_backend::Metadata;
use squeezefs::uring_fs::{
    self, clear_meta_devices, meta_io_grain, meta_io_mode, register_meta_device, AlignedBuf,
    META_IO_BOUNCE_BYTES, META_IO_BUFFERED_FALLBACK, META_IO_READ_WIDENED,
    META_IO_UNALIGNED_REFUSALS,
};
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;

const PAGES: u64 = 64;
const BASE: u64 = 0;

/// The direct posture's contracts DECLINE where the scratch filesystem
/// refuses `O_DIRECT` (the registration took its loud buffered fallback)
/// — through the testkit's ledger (TEST-2; review round 1, Issue 7a), so
/// `SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1` (the zc-capability gate, root on
/// the sqz box) turns the decline into a FAILURE and a green run on a
/// venue that never exercised the direct path is impossible. Expands at
/// the gate call so the ledger names the test.
macro_rules! require_direct {
    ($mode:expr) => {
        if !$mode.direct {
            let _ = squeezefs_testkit::declare(
                squeezefs_testkit::site!(),
                squeezefs_testkit::SkipClass::Capability,
                "the scratch filesystem refuses O_DIRECT (the registration took the loud \
                 buffered fallback) — the direct posture's contract cannot run here; point \
                 TMPDIR at a filesystem that serves it",
            );
            return;
        }
    };
}

/// A zero-filled volume file under `TMPDIR` (the matrix points it at a
/// real filesystem; `/tmp`'s tmpfs accepts `O_DIRECT` on this kernel too).
fn volume_file(len: u64) -> NamedTempFile {
    let f = NamedTempFile::new().expect("temp file");
    f.as_file().set_len(len).expect("size");
    f
}

fn records(i: u64, value_len: usize, seq: u64) -> Vec<(u8, Record)> {
    vec![(
        TREE_INODES,
        Record::put(inode_key(i).to_vec(), seq, vec![0xA5; value_len]),
    )]
}

/// The pad law, pure: for the two field grains every logical end position
/// of a page gets a pad that is 0 when already aligned, else ≥ `PAD_MIN`,
/// ≤ `max_pad`, and lands the next reservation sector-aligned.
#[test]
fn the_pad_law_lands_every_end_on_a_sector_boundary_within_its_bound() {
    for grain in [512u64, 4096] {
        let geo = CoreGeometry {
            page_data_len: JOURNAL_PAGE_DATA_LEN,
            pages: 8,
            reserve_bytes: 0,
            grain,
        };
        for end in 0..(3 * JOURNAL_PAGE_DATA_LEN) {
            let pad = geo.pad_of(end);
            assert!(
                geo.sector_aligned(end + pad),
                "grain {grain}: end {end} + pad {pad} is not sector-aligned"
            );
            if pad != 0 {
                assert!(
                    pad >= PAD_MIN,
                    "grain {grain}: end {end} pad {pad} < PAD_MIN"
                );
            } else {
                assert!(
                    geo.sector_aligned(end),
                    "grain {grain}: a zero pad at an unaligned end {end}"
                );
            }
            assert!(
                pad <= geo.max_pad(),
                "grain {grain}: end {end} pad {pad} > max_pad"
            );
            // The physical form: a page start is the page's header (4 KiB
            // aligned); anything else is `HDR + off` on the grain.
            let off = geo.in_page_off(end + pad);
            assert!(off == 0 || (JOURNAL_PAGE_HDR_LEN + off).is_multiple_of(grain));
        }
    }
    let unpadded = CoreGeometry {
        page_data_len: JOURNAL_PAGE_DATA_LEN,
        pages: 8,
        reserve_bytes: 0,
        grain: 1,
    };
    assert_eq!(unpadded.max_pad(), 0);
    assert_eq!(unpadded.pad_of(123), 0, "an unpadded ring pads nothing");
}

/// The registration's posture: a regular file on a filesystem that
/// serves `O_DIRECT` registers DIRECT with a power-of-two grain no wider
/// than the page; the recorded posture answers every later query (and
/// the canonical path spelling too).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registered_metadata_file_opens_o_direct_with_a_derived_grain() {
    clear_meta_devices();
    let f = volume_file(PAGES * JOURNAL_PAGE_LEN);
    assert_eq!(
        meta_io_grain(f.path()),
        1,
        "unregistered: the byte-grain arithmetic"
    );
    let mode = register_meta_device(f.path()).expect("register");
    if !mode.direct {
        // The one legal fallback venue: a REGULAR FILE whose filesystem
        // refused O_DIRECT — loud and counted; never on a block device.
        assert_eq!(mode.grain, 1);
        assert!(META_IO_BUFFERED_FALLBACK.load(Ordering::Relaxed) >= 1);
    }
    require_direct!(mode);
    assert!(
        mode.grain.is_power_of_two() && mode.grain <= JOURNAL_PAGE_LEN,
        "{mode:?}"
    );
    assert!(mode.mem_align.is_power_of_two() && mode.mem_align as u64 >= mode.grain);
    assert_eq!(meta_io_mode(f.path()), Some(mode));
    assert_eq!(meta_io_grain(f.path()), mode.grain);
    let canon = std::fs::canonicalize(f.path()).unwrap();
    assert_eq!(
        meta_io_mode(&canon),
        Some(mode),
        "the canonical spelling answers too"
    );
    // Idempotent.
    assert_eq!(register_meta_device(f.path()).unwrap(), mode);
}

/// The worker's alignment discipline on a direct path: a misaligned
/// WRITE (offset or length off the grain) is a refused caller bug
/// (`meta_io_unaligned_refusals`, nothing written); an aligned write whose
/// BUFFER is misaligned is bounced into an aligned copy
/// (`meta_io_bounce_bytes`) and lands; a narrow READ is served out of a
/// grain-widened span (`meta_io_read_widened`) byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn misaligned_direct_writes_refuse_and_misaligned_buffers_bounce_and_narrow_reads_widen() {
    clear_meta_devices();
    let f = volume_file(PAGES * JOURNAL_PAGE_LEN);
    let mode = register_meta_device(f.path()).expect("register");
    require_direct!(mode);
    let g = mode.grain as usize;
    let refusals0 = META_IO_UNALIGNED_REFUSALS.load(Ordering::Relaxed);
    let bounces0 = META_IO_BOUNCE_BYTES.load(Ordering::Relaxed);
    let widened0 = META_IO_READ_WIDENED.load(Ordering::Relaxed);

    // A misaligned offset, then a misaligned length: refused, nothing lands.
    let payload = AlignedBuf::from_slice(&vec![0x5Au8; g], mode.mem_align).into_bytes();
    let err = uring_fs::write_at(f.path(), 1, payload.clone())
        .await
        .expect_err("a misaligned direct write is a caller bug");
    assert_eq!(err.to_errno(), libc::EINVAL, "{err}");
    let err = uring_fs::write_at(f.path(), g as u64, payload.slice(..g - 1))
        .await
        .expect_err("a misaligned length is a caller bug");
    assert_eq!(err.to_errno(), libc::EINVAL, "{err}");
    assert_eq!(
        META_IO_UNALIGNED_REFUSALS.load(Ordering::Relaxed),
        refusals0 + 2
    );
    let untouched = uring_fs::read_at(f.path(), 0, 2 * g).await.unwrap();
    assert!(untouched.iter().all(|b| *b == 0), "nothing landed");

    // An aligned write from a deliberately misaligned buffer: bounced,
    // counted, landed byte-exact.
    let raw = vec![0xC3u8; g + 1];
    let misaligned = bytes::Bytes::from(raw).slice(1..);
    assert!(
        !AlignedBuf::ptr_aligned(misaligned.as_ptr(), mode.mem_align),
        "the fixture must present a misaligned pointer"
    );
    uring_fs::write_at(f.path(), 2 * g as u64, misaligned)
        .await
        .expect("an aligned write from a misaligned buffer lands");
    assert_eq!(
        META_IO_BOUNCE_BYTES.load(Ordering::Relaxed),
        bounces0 + g as u64
    );
    uring_fs::fdatasync(f.path()).await.unwrap();

    // A narrow read inside the written grain: widened, sliced exactly.
    let got = uring_fs::read_at(f.path(), 2 * g as u64 + 7, 21)
        .await
        .unwrap();
    assert_eq!(got.len(), 21);
    assert!(got.iter().all(|b| *b == 0xC3), "{got:?}");
    assert!(META_IO_READ_WIDENED.load(Ordering::Relaxed) > widened0);
    // A read straddling the written grain and its zero neighbour.
    let got = uring_fs::read_at(f.path(), 3 * g as u64 - 4, 8)
        .await
        .unwrap();
    assert_eq!(&got[..4], &[0xC3; 4]);
    assert_eq!(&got[4..], &[0; 4]);

    // A variable-length unit's aligned form: `pad_to_grain` zero-extends
    // to the grain (the slot-tails spill's shape — 12 B entries into an
    // extent it owns whole), and the padded image is accepted verbatim.
    let unit = vec![0x77u8; 12 * 37];
    let padded = uring_fs::pad_to_grain(f.path(), unit.clone());
    assert_eq!(padded.len(), (unit.len()).div_ceil(g) * g);
    assert_eq!(&padded[..unit.len()], &unit[..]);
    assert!(padded[unit.len()..].iter().all(|b| *b == 0));
    uring_fs::write_at(f.path(), 4 * g as u64, padded)
        .await
        .expect("the padded unit is an aligned write");
    let back = uring_fs::read_at(f.path(), 4 * g as u64, unit.len())
        .await
        .unwrap();
    assert_eq!(&back[..], &unit[..]);
    // An unregistered (buffered, grain 1) path keeps the image verbatim.
    let other = volume_file(JOURNAL_PAGE_LEN);
    assert_eq!(
        uring_fs::pad_to_grain(other.path(), unit.clone()).len(),
        unit.len()
    );

    // The harness's byte plant: `patch_at` lands bytes at ANY offset by
    // read-modify-write of the covering span, the neighbours untouched.
    let refusals1 = META_IO_UNALIGNED_REFUSALS.load(Ordering::Relaxed);
    uring_fs::patch_at(f.path(), 4 * g as u64 + 5, vec![0xEEu8; 3])
        .await
        .expect("a byte plant lands on a direct path");
    let back = uring_fs::read_at(f.path(), 4 * g as u64, 12).await.unwrap();
    assert_eq!(&back[..5], &[0x77; 5]);
    assert_eq!(&back[5..8], &[0xEE; 3]);
    assert_eq!(&back[8..], &[0x77; 4]);
    assert_eq!(
        META_IO_UNALIGNED_REFUSALS.load(Ordering::Relaxed),
        refusals1,
        "the plant is never a refused product write"
    );
}

/// The journal ring's aligned form: on a registered direct path every
/// reservation ends sector-aligned (its pad is a PAD entry), the window
/// writes are aligned runs the worker accepts, and the replay walks the
/// pads as chain links — every entry recovered, nothing torn, the head at
/// the last reservation's padded end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_padded_ring_writes_aligned_runs_and_replay_walks_the_pads() {
    clear_meta_devices();
    let f = volume_file(PAGES * JOURNAL_PAGE_LEN);
    let mode = register_meta_device(f.path()).expect("register");
    require_direct!(mode);
    let reserve = checkpoint_reserve_bytes(PAGES * JOURNAL_PAGE_LEN).min(64 * 1024);
    let ring = JournalRing::new(f.path(), BASE, PAGES, reserve);
    assert_eq!(
        ring.grain(),
        mode.grain,
        "the ring pads to the registered grain"
    );
    let geo = *ring.core().geometry();
    let pads0 = META_KV_JOURNAL_PAD_ENTRIES.load(Ordering::Relaxed);
    let pad_bytes0 = META_KV_JOURNAL_PAD_BYTES.load(Ordering::Relaxed);
    // Odd sizes so no entry ends aligned by luck; some span pages.
    let sizes = [1usize, 77, 500, 3000, 9000, 33, 4050, 2];
    let mut last_end = 0;
    let mut padded = 0u64;
    for (i, v) in sizes.iter().enumerate() {
        let probe = records(i as u64 + 1, *v, 0);
        let need = entry_len_for(&probe).unwrap();
        let adm = ring.try_admit(need, AdmissionClass::User).expect("room");
        let (res, seq_base) = ring.reserve_registered(adm);
        assert_eq!(res.len, need);
        assert!(
            geo.sector_aligned(res.padded_end()),
            "reservation {i} ends unaligned: {res:?}"
        );
        assert!(
            geo.sector_aligned(res.start),
            "reservation {i} starts unaligned: {res:?}"
        );
        if res.pad > 0 {
            padded += 1;
            assert!(res.pad >= PAD_MIN && res.pad <= geo.max_pad(), "{res:?}");
        }
        ring.commit_entry(&res, &records(i as u64 + 1, *v, seq_base))
            .await
            .unwrap_or_else(|e| panic!("entry {i} ({v} B): {e}"));
        last_end = res.padded_end();
    }
    assert!(padded >= 1, "an odd-sized run pads at least once");
    assert_eq!(
        META_KV_JOURNAL_PAD_ENTRIES.load(Ordering::Relaxed) - pads0,
        padded
    );
    assert!(META_KV_JOURNAL_PAD_BYTES.load(Ordering::Relaxed) > pad_bytes0);
    assert_eq!(
        ring.written_pad_bytes(),
        META_KV_JOURNAL_PAD_BYTES.load(Ordering::Relaxed) - pad_bytes0
    );
    assert_eq!(ring.core().head(), last_end);
    uring_fs::fdatasync(f.path()).await.unwrap();

    let (rec_ring, rec) = JournalRing::recover(f.path(), BASE, PAGES, reserve, 0)
        .await
        .expect("recover");
    assert_eq!(
        rec.entries.len(),
        sizes.len(),
        "every entry replays; pads are not entries"
    );
    assert_eq!(rec.dropped_torn, 0, "a pad is a chain link, never a tear");
    assert_eq!(rec.head_pos, last_end, "the head resumes at the padded end");
    assert!(geo.sector_aligned(rec_ring.core().head()));
    for (i, e) in rec.entries.iter().enumerate() {
        assert_eq!(e.records.len(), 1);
        assert_eq!(e.records[0].1.value.len(), sizes[i]);
    }
    assert_eq!(
        rec_ring.align_head_for_writing().await.unwrap(),
        0,
        "an aligned head needs no recovery pad"
    );
}

/// **A PAD whose checksum fails is a TORN ENTRY** (review round 1, Issue
/// 7b — the pad's own parse arm, pinned): the chain-primary walk stops at
/// it, counts it as a pending drop and RESYNCS at the next page header
/// (§4.1) — never a chain link, never a hole silently stepped over. Two
/// shapes: (i) a torn pad with a LATER window behind it — the later entry
/// is found by the resync (on a 4 KiB-grain ring every window ends at a
/// page end, so nothing stands between the torn pad and the next header
/// to be lost) and CONFIRMS the drop (`dropped_torn` = 1); (ii) the LAST
/// window's pad torn — an unconfirmed trailing failure (`dropped_torn`
/// = 0), the head resumes at the entry's UNPADDED end, mid-sector, and the
/// writer's head alignment writes a fresh recovery pad there — the F-C1
/// "torn window" shape, closed by the law that pads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pad_whose_checksum_fails_is_a_torn_entry_the_scan_resyncs_past() {
    clear_meta_devices();
    let f = volume_file(PAGES * JOURNAL_PAGE_LEN);
    let mode = register_meta_device(f.path()).expect("register");
    require_direct!(mode);
    let reserve = checkpoint_reserve_bytes(PAGES * JOURNAL_PAGE_LEN).min(64 * 1024);
    let ring = JournalRing::new(f.path(), BASE, PAGES, reserve);
    let geo = *ring.core().geometry();
    // Three windows, each an odd-sized entry plus its pad.
    let sizes = [100usize, 700, 300];
    let mut reservations = Vec::new();
    for (i, v) in sizes.iter().enumerate() {
        let probe = records(i as u64 + 1, *v, 0);
        let need = entry_len_for(&probe).unwrap();
        let adm = ring.try_admit(need, AdmissionClass::User).expect("room");
        let (res, seq_base) = ring.reserve_registered(adm);
        assert!(res.pad >= PAD_MIN, "every odd window pads: {res:?}");
        ring.commit_entry(&res, &records(i as u64 + 1, *v, seq_base))
            .await
            .unwrap();
        reservations.push(res);
    }
    uring_fs::fdatasync(f.path()).await.unwrap();
    drop(ring);
    // A pad entry's header is `seq | len | xxh3`; flipping a checksum byte
    // leaves seq == position (the chain reaches it) and fails the verify.
    let tear_pad_checksum = |res: &squeezefs::meta_backend::kv::journal_core::Reservation| {
        let pad_pos = res.start + res.len;
        let probe = JournalRing::new(f.path(), BASE, PAGES, reserve);
        probe.physical_offset_of(pad_pos) + 12
    };

    // (i) The FIRST window's pad torn: the scan fails at it, resyncs at
    // page 1's header (window 2 starts there), recovers windows 2 and 3
    // and confirms the drop.
    let off = tear_pad_checksum(&reservations[0]);
    let orig = uring_fs::read_at(f.path(), off, 1).await.unwrap()[0];
    uring_fs::patch_at(f.path(), off, vec![orig ^ 0xFF])
        .await
        .expect("plant");
    let (rec_ring, rec) = JournalRing::recover(f.path(), BASE, PAGES, reserve, 0)
        .await
        .expect("recover");
    assert_eq!(
        rec.entries.len(),
        3,
        "the entry before the torn pad and both later windows recover"
    );
    assert_eq!(
        rec.dropped_torn, 1,
        "the torn pad is a confirmed drop, never a chain link"
    );
    assert_eq!(
        rec.head_pos,
        reservations[2].padded_end(),
        "the head resumes at the last intact window's padded end"
    );
    assert!(geo.sector_aligned(rec_ring.core().head()));
    assert_eq!(
        rec_ring.align_head_for_writing().await.unwrap(),
        0,
        "an aligned head needs no recovery pad"
    );
    drop(rec_ring);
    // Restore window 1's pad for shape (ii).
    uring_fs::patch_at(f.path(), off, vec![orig]).await.unwrap();

    // (ii) The LAST window's pad torn: an unconfirmed trailing failure —
    // every entry recovers, nothing is counted dropped, the head stands at
    // the last entry's UNPADDED end (mid-sector), and the writer pads it.
    let off = tear_pad_checksum(&reservations[2]);
    let orig = uring_fs::read_at(f.path(), off, 1).await.unwrap()[0];
    uring_fs::patch_at(f.path(), off, vec![orig ^ 0xFF])
        .await
        .expect("plant");
    let (rec_ring, rec) = JournalRing::recover(f.path(), BASE, PAGES, reserve, 0)
        .await
        .expect("recover");
    assert_eq!(rec.entries.len(), 3);
    assert_eq!(
        rec.dropped_torn, 0,
        "a trailing torn pad is unconfirmed (nothing behind it)"
    );
    let unpadded_end = reservations[2].start + reservations[2].len;
    assert_eq!(rec.head_pos, unpadded_end, "the head is the entry's end");
    assert!(
        !geo.sector_aligned(unpadded_end),
        "the torn window leaves the head mid-sector"
    );
    let pad = rec_ring
        .align_head_for_writing()
        .await
        .expect("recovery pad");
    assert_eq!(
        pad, reservations[2].pad,
        "the recovery pad is exactly the torn pad's length"
    );
    assert_eq!(rec_ring.core().head(), reservations[2].padded_end());
    // The re-padded ring replays whole, nothing torn.
    uring_fs::fdatasync(f.path()).await.unwrap();
    let (_, rec2) = JournalRing::recover(f.path(), BASE, PAGES, reserve, 0)
        .await
        .unwrap();
    assert_eq!((rec2.entries.len(), rec2.dropped_torn), (3, 0));
    assert_eq!(rec2.head_pos, reservations[2].padded_end());
}

/// A ring written UNPADDED (a buffered binary before PR 13i — or an
/// unregistered path, the same bytes) replays under the direct posture,
/// and the WRITER's head alignment writes one recovery pad so its first
/// reservation begins on a sector boundary; a later replay walks the old
/// entries, the recovery pad and the new entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ring_written_unpadded_replays_direct_and_the_writer_aligns_its_head() {
    clear_meta_devices();
    let f = volume_file(PAGES * JOURNAL_PAGE_LEN);
    let reserve = checkpoint_reserve_bytes(PAGES * JOURNAL_PAGE_LEN).min(64 * 1024);
    // Unregistered: grain 1, the pre-PR-13i bytes.
    let old = JournalRing::new(f.path(), BASE, PAGES, reserve);
    assert_eq!(old.grain(), 1);
    let mut end = 0;
    for i in 0..5u64 {
        let probe = records(i + 1, 100 + i as usize * 13, 0);
        let need = entry_len_for(&probe).unwrap();
        let adm = old.try_admit(need, AdmissionClass::User).unwrap();
        let (res, seq_base) = old.reserve_registered(adm);
        assert_eq!(res.pad, 0);
        old.commit_entry(&res, &records(i + 1, 100 + i as usize * 13, seq_base))
            .await
            .unwrap();
        end = res.padded_end();
    }
    uring_fs::fdatasync(f.path()).await.unwrap();
    drop(old);

    // The next binary registers the path (every open does) and recovers.
    let mode = register_meta_device(f.path()).expect("register");
    require_direct!(mode);
    let (ring, rec) = JournalRing::recover(f.path(), BASE, PAGES, reserve, 0)
        .await
        .unwrap();
    assert_eq!(rec.entries.len(), 5);
    assert_eq!(rec.head_pos, end);
    let geo = *ring.core().geometry();
    assert!(
        !geo.sector_aligned(end),
        "the fixture's unpadded head must stand mid-sector (end {end})"
    );
    let pad = ring.align_head_for_writing().await.expect("recovery pad");
    assert!(pad >= PAD_MIN, "a recovery pad was written: {pad}");
    assert!(geo.sector_aligned(ring.core().head()));
    assert_eq!(ring.core().head(), end + pad);
    // The writer goes on: aligned reservations, aligned runs.
    let probe = records(9, 40, 0);
    let need = entry_len_for(&probe).unwrap();
    let adm = ring.try_admit(need, AdmissionClass::User).unwrap();
    let (res, seq_base) = ring.reserve_registered(adm);
    assert_eq!(res.start, end + pad);
    ring.commit_entry(&res, &records(9, 40, seq_base))
        .await
        .unwrap();
    uring_fs::fdatasync(f.path()).await.unwrap();

    let (_, rec2) = JournalRing::recover(f.path(), BASE, PAGES, reserve, 0)
        .await
        .unwrap();
    assert_eq!(
        rec2.entries.len(),
        6,
        "the old entries, the pad (no entry), the new one"
    );
    assert_eq!(rec2.dropped_torn, 0);
    assert_eq!(rec2.head_pos, res.padded_end());
}

/// **A reported grain the ring's page arithmetic cannot honour REFUSES the
/// registration — never a clamp under the device's grain** (PR 13i review
/// round 1, Issue 5b). The first build clamped a grain > 4096 (or an odd
/// one) to the 4096 default: an UNDER-aligned posture on such a device,
/// every later write `EINVAL` with a misattributed message. The law is one
/// pure function every source (statx, sysfs, the default) passes through:
/// the two field grains and every power of two up to the page admit
/// verbatim; a wider grain, a non-power-of-two and zero refuse naming the
/// device, the source and the grain.
#[test]
fn a_grain_the_page_arithmetic_cannot_honour_refuses_the_registration() {
    let path = std::path::Path::new("/dev/example-lun");
    for g in [512u64, 1024, 2048, 4096, 1, 2, 4] {
        assert_eq!(
            uring_fs::admit_meta_io_grain(path, "statx(STATX_DIOALIGN)", g).expect("admitted"),
            g,
            "grain {g} admits verbatim"
        );
    }
    for (g, source) in [
        (8192u64, "statx(STATX_DIOALIGN)"),
        (16384, "sysfs logical_block_size"),
        (65536, "sysfs logical_block_size"),
        (3072, "statx(STATX_DIOALIGN)"),
        (520, "sysfs logical_block_size"),
        (0, "statx(STATX_DIOALIGN)"),
    ] {
        let err = uring_fs::admit_meta_io_grain(path, source, g)
            .expect_err("a grain the page arithmetic cannot honour is refused");
        let text = err.to_string();
        assert!(
            text.contains("example-lun")
                && text.contains(source)
                && text.contains(&format!("{g} B")),
            "the refusal names the device, the source and the grain: {text}"
        );
        assert!(
            text.contains("REFUSING") && !text.contains("clamp"),
            "a refusal, never a clamp: {text}"
        );
    }
}

/// The harness seam keeps a registered path BUFFERED (the two-host pin's
/// control arm): grain 1, nothing padded, the fallback gauge counts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_buffered_seam_keeps_the_pre_pr_13i_posture() {
    clear_meta_devices();
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_TEST_META_BUFFERED");
            clear_meta_devices();
        }
    }
    let _c = Cleanup;
    std::env::set_var("SQUEEZEFS_TEST_META_BUFFERED", "1");
    let f = volume_file(PAGES * JOURNAL_PAGE_LEN);
    let fallback0 = META_IO_BUFFERED_FALLBACK.load(Ordering::Relaxed);
    let mode = register_meta_device(f.path()).unwrap();
    assert!(!mode.direct);
    assert_eq!(mode.grain, 1);
    assert_eq!(
        META_IO_BUFFERED_FALLBACK.load(Ordering::Relaxed),
        fallback0 + 1
    );
    let ring = JournalRing::new(f.path(), BASE, PAGES, 4096);
    assert_eq!(ring.grain(), 1, "a buffered path pads nothing");
    // An unaligned write is admitted verbatim on the buffered posture.
    uring_fs::write_at(f.path(), 3, bytes::Bytes::from_static(b"abc"))
        .await
        .unwrap();
    assert_eq!(
        &uring_fs::read_at(f.path(), 3, 3).await.unwrap()[..],
        b"abc"
    );
}

/// **A ring recovered within `max_pad` of 100 % full still opens for
/// writing** (PR 13i review round 1, Issue 3). The writer's head
/// alignment is the ring's FIRST write and needs `max_pad` of TOTAL ring
/// space (reserve included); a ring the checkpoint class filled to its
/// last bytes before the crash — a ring written UNPADDED by the binary
/// before PR 13i, whose head therefore stands mid-sector — cannot admit
/// it. The first build refused the open (`JournalReserveExhausted`) and
/// every later open refused identically: nothing drains a ring no writer
/// can open — the mount-refusal wedge face `preclaim_ring_recovery`
/// exists to prevent (P2 2026-07-26 §9), which the pre-13i binary
/// recovered from (its first pre-claim cycle needs no ring write). The
/// law: the alignment's refused admission runs the pre-claim's guarded
/// checkpoint cycles — each flushes the replayed dirt, writes the ledger
/// and advances the tail without a ring write — until the pad admits,
/// THEN pads, and the open proceeds with every replayed record present.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ring_recovered_within_max_pad_of_full_opens_through_the_guarded_cycles() {
    clear_meta_devices();
    const VOL_LEN: u64 = 64 * 1024 * 1024;
    const RING_LEN: u64 = 512 * 1024;
    let f = volume_file(VOL_LEN);
    format_v3(
        f.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: 256 * 1024,
            journal_len_override: Some(RING_LEN),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format");
    // The probe writes nothing and registers the path (every door does):
    // the geometry and the grain the writer open below will pad to.
    let (sb, mode) = {
        let probe = KvMetaBackend::open_probe(f.path()).await.expect("probe");
        let sb = probe.superblock().clone();
        probe.shutdown().await.expect("probe shutdown");
        (
            sb,
            meta_io_mode(f.path()).expect("the probe registered the path"),
        )
    };
    require_direct!(mode);
    let grain = mode.grain;
    let max_pad = PAD_MIN + grain;

    // The pre-13i binary's ring form: UNPADDED (an unregistered path is
    // grain 1), filled in the CHECKPOINT class (reserve 0 — the wedged-
    // tail escalation's form) until nothing admits, the last entries tiny
    // so the head stands within a few bytes of the ring's end.
    clear_meta_devices();
    let ring_extent = KvMetaBackend::fixed_ring_extent(&sb);
    let pages = ring_extent.len / JOURNAL_PAGE_LEN;
    let old = JournalRing::new(
        f.path(),
        ring_extent.start,
        pages,
        checkpoint_reserve_bytes(ring_extent.len),
    );
    assert_eq!(old.grain(), 1, "the fixture writes the pre-13i bytes");
    let journal_key = |kind: u8, legacy: &[u8]| -> Vec<u8> {
        if sb.symmetric_forest_stamped() {
            forest_key(kind, legacy).expect("forest key")
        } else {
            legacy.to_vec()
        }
    };
    // Four xattr names re-put round-robin: the window folds to four
    // records in RAM, so the guarded cycles' flush needs no split (no SMO
    // record — the one ring write a flush pass ever makes).
    const NAMES: [&str; 4] = ["user.pad0", "user.pad1", "user.pad2", "user.pad3"];
    let xattr_records = |i: usize, value_len: usize, seq: u64| -> Vec<(u8, Record)> {
        let name = NAMES[i % NAMES.len()];
        let key = xattr_key(
            ROOT_INO,
            xattr_name_hash56(name.as_bytes(), sb.hash_seed),
            0,
        );
        let value = XattrValue::encode_parts(name.as_bytes(), &vec![(i & 0xFF) as u8; value_len])
            .expect("xattr value");
        vec![(
            TREE_XATTRS,
            Record::put(journal_key(TREE_XATTRS, &key), seq, value),
        )]
    };
    let mut planted = 0usize;
    for value_len in [3900usize, 100, 1] {
        loop {
            let need = entry_len_for(&xattr_records(planted, value_len, 0)).unwrap();
            let Some(adm) = old.try_admit(need, AdmissionClass::Checkpoint) else {
                break;
            };
            let (res, seq_base) = old.reserve_registered(adm);
            assert_eq!(res.pad, 0, "an unpadded ring pads nothing");
            old.commit_entry(&res, &xattr_records(planted, value_len, seq_base))
                .await
                .expect("plant");
            planted += 1;
        }
    }
    let geo = *old.core().geometry();
    let head = old.core().head();
    let free = geo.logical_len() - head;
    assert!(
        free < max_pad,
        "fixture: the ring must stand within max_pad ({max_pad}) of full — {free} B free"
    );
    let direct_geo = CoreGeometry { grain, ..geo };
    assert!(
        !direct_geo.sector_aligned(head),
        "fixture: the recovered head must stand mid-sector at grain {grain} (head {head})"
    );
    uring_fs::fdatasync(f.path()).await.unwrap();
    drop(old);
    let pads0 = META_KV_JOURNAL_PAD_ENTRIES.load(Ordering::Relaxed);

    // This binary's WRITER open: registers the path direct, recovers the
    // window (the head mid-sector, the ring full), aligns its head THROUGH
    // the guarded cycles and serves.
    let be = KvMetaBackend::open(f.path()).await.expect(
        "a ring recovered within max_pad of full opens for writing through the pre-claim's \
         guarded cycles (review round 1, Issue 3) — a refusal here is the wedge face: every \
         later open refuses the same way",
    );
    assert_eq!(
        be.replay_stats().entries,
        planted as u64,
        "every planted entry replays"
    );
    assert_eq!(be.replay_stats().dropped_torn, 0);
    assert!(
        META_KV_JOURNAL_PAD_ENTRIES.load(Ordering::Relaxed) > pads0,
        "the recovery pad was written once the cycles made room"
    );
    let rgeo = *be.journal_ring().core().geometry();
    assert_eq!(rgeo.grain, grain);
    assert!(
        rgeo.sector_aligned(be.journal_ring().core().head()),
        "the writer's head stands aligned after the open"
    );
    // The replayed records serve, and the writer writes.
    for (i, name) in NAMES.iter().enumerate() {
        let v = be
            .getxattr(ROOT_INO, name)
            .await
            .expect("getxattr")
            .unwrap_or_else(|| panic!("{name} replayed"));
        assert!(!v.is_empty(), "{name} carries its last put ({i})");
    }
    let after = be
        .create(ROOT_INO, "after", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a user commit lands on the aligned ring")
        .ino;
    be.shutdown().await.expect("clean shutdown");
    let re = KvMetaBackend::open(f.path()).await.expect("remount");
    assert_eq!(
        re.lookup(ROOT_INO, "after").await.expect("after").ino,
        after
    );
    assert!(re
        .getxattr(ROOT_INO, NAMES[0])
        .await
        .expect("getxattr")
        .is_some());
    re.shutdown().await.expect("shutdown");
}
