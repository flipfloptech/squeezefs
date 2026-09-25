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

use squeezefs::meta_backend::kv::journal::{
    checkpoint_reserve_bytes, entry_len_for, JournalRing, JOURNAL_PAGE_DATA_LEN,
    JOURNAL_PAGE_HDR_LEN, JOURNAL_PAGE_LEN,
};
use squeezefs::meta_backend::kv::journal_core::{AdmissionClass, CoreGeometry, PAD_MIN};
use squeezefs::meta_backend::kv::record::{inode_key, Record, TREE_INODES};
use squeezefs::meta_backend::kv::{META_KV_JOURNAL_PAD_BYTES, META_KV_JOURNAL_PAD_ENTRIES};
use squeezefs::uring_fs::{
    self, clear_meta_devices, meta_io_grain, meta_io_mode, register_meta_device, AlignedBuf,
    META_IO_BOUNCE_BYTES, META_IO_BUFFERED_FALLBACK, META_IO_READ_WIDENED,
    META_IO_UNALIGNED_REFUSALS,
};
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;

const PAGES: u64 = 64;
const BASE: u64 = 0;

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
        return;
    }
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
    if !mode.direct {
        return;
    }
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
    if !mode.direct {
        return;
    }
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
    if !mode.direct {
        return;
    }
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
