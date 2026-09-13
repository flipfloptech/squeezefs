//! Symmetric metadata program, PR 2 — **the appender region**
//! (`docs/design-symmetric-metadata.md` §5.3 B(ii), §5.2.3, §5.7.3, §5.9,
//! §6.1/§6.2/§6.4, §7.1, §11; KD-SYM-3/4/10).
//!
//! Under incompat bit 17 every metadata writer is an APPENDER with its
//! own 4 KiB page (identity, term, state, ring segments, tail, checkpoint
//! seq, extent grant, leased slots), its own journal ring (a segment
//! table), and — from PR 3 — its own extent grant. Appender 0's page and
//! ring live with the format-time fixed ring; appenders ≥ 1 live in the
//! appender DIRECTORY, an extent chain the superblock names.
//!
//! **Bit 17 stays DARK**: the only stamp is the PR-1 seam
//! `SQUEEZEFS_TEST_STAMP_SYMMETRIC=1`, and a bit-17-absent volume is
//! byte-for-byte the shipped format — the negative contract every
//! positive one here rides beside.
//!
//! Contracts pinned (the PR-2 row of the design's PR plan):
//! - the page codec round-trips at every bound and its decode is TOTAL;
//! - `SLOT_PAGE_BUDGET` derives from the page's own fixed part;
//! - newest-valid-wins over the four page slots; a torn newest page falls
//!   back to its predecessor;
//! - the directory header codec; pairs per extent derive from the node
//!   size; the chain grows past one extent;
//! - the ring derivation: floor 512 KiB (reserve + one max entry, rounded
//!   to a power of two), ceiling = the solo ring, the knob wins verbatim;
//!   `appenders_capacity = heap/16 ÷ ring`;
//! - routing slot ↔ forest slot around the native slot;
//! - `appender_dir` rides sector 0 ONLY under bit 17 (byte-identical
//!   sector 0 otherwise); format under the seam writes appender 0's page
//!   pair + the directory's first extent.

use squeezefs::meta_backend::kv::appender::{
    appender0_page_offsets, appender0_ring_extent, appender_ring_bytes_derived, appenders_capacity,
    classify_page, dir_pairs_per_extent, forest_slot_of_page_slot, newest_valid, page_slot_for,
    page_slot_of_forest_slot, read_directory, ring_budget_bytes, sym_ring_ceiling_bytes,
    AppenderIdentity, AppenderPage, AppenderState, DirHeader, GrantRun, PageRead, SlotEntry,
    SlotEntryState, APPENDER0_RESERVED_PAGES, APPENDER_PAGE_FIXED_LEN, APPENDER_PAGE_LEN,
    APPENDER_PAGE_SLOTS, GRANT_RUNS_MAX, RING_SEGMENTS_MAX, SLOT_ENTRY_LEN, SLOT_PAGE_BUDGET,
    SYM_RING_FLOOR_BYTES,
};
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder, ROOT_INO};
use squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS;
use squeezefs::meta_backend::kv::journal::{checkpoint_reserve_bytes, MAX_ENTRY_LEN};
use squeezefs::meta_backend::kv::record::{guest_forest_slot, NATIVE_FOREST_SLOT};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, journal_ring_len, ExtentRef, SuperblockV3, VolumeFormat,
    FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST, SUPERBLOCK_V3_LEN,
};
use squeezefs::meta_backend::kv::tree::RootPtr;
use std::collections::HashMap;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Harness (the sym_forest_tests shape: 64 KiB nodes, a 1 MiB fixed ring).
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const TEST_SEED: u64 = 0x5EED_A99E_0DE7_0002;
const TEST_UUID: [u8; 16] = *b"sym-appender-tst";

/// The seam is process-global; format calls serialize through it.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn builder_config() -> BuilderConfig {
    BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    }
}

fn describe() -> (ImageBuilder, HashMap<&'static str, u64>) {
    let mut b = ImageBuilder::new(builder_config()).unwrap();
    let mut inos = HashMap::new();
    let docs = b.add_dir(ROOT_INO, "docs", 0o750, 1000, 1000).unwrap();
    inos.insert("docs", docs);
    let readme = b
        .add_file(docs, "readme.txt", 0o644, 1000, 1000, 4096)
        .unwrap();
    inos.insert("readme.txt", readme);
    b.set_xattr(readme, "user.color", b"blue").unwrap();
    (b, inos)
}

async fn build_image(file: &NamedTempFile, symmetric: bool) -> HashMap<&'static str, u64> {
    file.as_file().set_len(VOL_LEN).unwrap();
    let (b, inos) = describe();
    let _g = SEAM.lock().await;
    if symmetric {
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    } else {
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    }
    let built = b.build(file.path(), VOL_LEN).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    built.expect("build image");
    inos
}

async fn superblock_of(file: &NamedTempFile) -> SuperblockV3 {
    match classify_volume(file.path()).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 superblock, got {other:?}"),
    }
}

fn full_page() -> AppenderPage {
    AppenderPage {
        appender_id: 7,
        generation: 42,
        identity: AppenderIdentity {
            node_token: 0x0123_4567_89AB_CDEF,
            mount_slot: 0xDEAD_BEEF,
            writer_id: 0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF,
        },
        term: 3,
        state: AppenderState::Live,
        recovered_by_term: 0,
        is_manager: true,
        home_volume: 2,
        segments: (0..RING_SEGMENTS_MAX as u64)
            .map(|i| ExtentRef {
                start: 0x10_0000 + i * 0x4_0000,
                len: 0x4_0000,
            })
            .collect(),
        head_hint: 123_456,
        ledger_tail_seq: 100_000,
        ckpt_seq: 17,
        grant: (0..GRANT_RUNS_MAX as u64)
            .map(|i| GrantRun {
                start: 1000 + i * 8,
                len: 8,
            })
            .collect(),
        slots: (0..SLOT_PAGE_BUDGET as u16)
            .map(|i| SlotEntry {
                slot: i * 3,
                state: if i % 2 == 0 {
                    SlotEntryState::Live
                } else {
                    SlotEntryState::Releasing
                },
                g: u32::from(i) + 1,
                slot_tree_extents: u32::from(i) * 2,
                root: RootPtr {
                    addr: 0x20_0000 + u64::from(i) * 0x1_0000,
                    seq: 5000 + u64::from(i),
                },
                cursor: 77 + u64::from(i),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// §5.3.2 — the page codec.
// ---------------------------------------------------------------------------

#[test]
fn slot_page_budget_derives_from_the_pages_fixed_part() {
    // The budget is what the page's own arithmetic leaves, never a free
    // constant: fixed part + budget × entry ≤ one page, and one more
    // entry would not fit.
    assert_eq!(APPENDER_PAGE_LEN, 4096);
    assert_eq!(
        SLOT_PAGE_BUDGET,
        (APPENDER_PAGE_LEN - APPENDER_PAGE_FIXED_LEN) / SLOT_ENTRY_LEN
    );
    let budget = std::hint::black_box(SLOT_PAGE_BUDGET);
    assert!(APPENDER_PAGE_FIXED_LEN + budget * SLOT_ENTRY_LEN <= APPENDER_PAGE_LEN);
    assert!(APPENDER_PAGE_FIXED_LEN + (budget + 1) * SLOT_ENTRY_LEN > APPENDER_PAGE_LEN);
    // The §5.3.2 field list at natural widths: slot u16 + state u8 + g u32
    // + slot_tree_extents u32 + root (u64, u64) + cursor u64.
    assert_eq!(SLOT_ENTRY_LEN, 2 + 1 + 4 + 4 + 8 + 8 + 8);
    assert_eq!(
        RING_SEGMENTS_MAX, 8,
        "2 MiB per growth step at 256 KiB segments"
    );
    assert_eq!(
        GRANT_RUNS_MAX, 4,
        "current + successor + two returns in flight"
    );
    assert_eq!(
        APPENDER_PAGE_SLOTS, 4,
        "the A/B pair + the two ring-side pages"
    );
    assert_eq!(APPENDER0_RESERVED_PAGES, 4);
}

#[test]
fn appender_page_round_trips_at_every_bound_and_refuses_past_them() {
    let page = full_page();
    let img = page.encode().expect("encode");
    assert_eq!(img.len(), APPENDER_PAGE_LEN);
    assert_eq!(AppenderPage::decode(&img).expect("decode"), page);
    assert_eq!(page.ring_bytes(), RING_SEGMENTS_MAX as u64 * 0x4_0000);

    let mut too_many_segments = page.clone();
    too_many_segments
        .segments
        .push(ExtentRef { start: 1, len: 1 });
    assert!(too_many_segments.encode().is_err(), "9 segments refuse");
    let mut too_many_runs = page.clone();
    too_many_runs.grant.push(GrantRun { start: 1, len: 1 });
    assert!(too_many_runs.encode().is_err(), "5 grant runs refuse");
    let mut too_many_slots = page.clone();
    too_many_slots.slots.push(SlotEntry {
        slot: u16::MAX,
        state: SlotEntryState::Live,
        g: 0,
        slot_tree_extents: 0,
        root: RootPtr { addr: 0, seq: 0 },
        cursor: 0,
    });
    assert!(
        too_many_slots.encode().is_err(),
        "SLOT_PAGE_BUDGET + 1 slots refuse — the budget is a hard bound, never a truncation"
    );

    // A Free page names nothing.
    let free = AppenderPage::free(3, 1);
    let free_img = free.encode().unwrap();
    let back = AppenderPage::decode(&free_img).unwrap();
    assert_eq!(back.state, AppenderState::Free);
    assert!(back.segments.is_empty() && back.slots.is_empty() && back.grant.is_empty());
}

#[test]
fn appender_page_decode_is_total_and_canonical() {
    let page = full_page();
    let img = page.encode().unwrap();
    // Short / long images refuse by length.
    assert!(AppenderPage::decode(&img[..img.len() - 1]).is_err());
    assert!(AppenderPage::decode(&[0u8; 4097]).is_err());
    // Blank classifies as Blank, not Corrupt.
    assert_eq!(classify_page(&[0u8; APPENDER_PAGE_LEN]), PageRead::Blank);
    // Bad magic.
    let mut bad_magic = img.clone();
    bad_magic[0] ^= 0xFF;
    assert!(matches!(classify_page(&bad_magic), PageRead::Corrupt(_)));
    // Any flipped byte fails the checksum (the whole image is covered).
    for off in [5usize, 30, 60, 100, 300, 2000, 4095] {
        let mut torn = img.clone();
        torn[off] ^= 0x01;
        assert!(
            matches!(classify_page(&torn), PageRead::Corrupt(_)),
            "flip at {off} must not verify"
        );
    }
    // A structurally impossible image with a VALID checksum refuses too:
    // an unknown state, a non-ascending slot list, a count past its bound.
    let mut state9 = page.clone();
    state9.slots.clear();
    let mut s_img = state9.encode().unwrap();
    s_img[52] = 9;
    let sum = recompute_checksum(&s_img);
    s_img[16..24].copy_from_slice(&sum.to_le_bytes());
    assert!(
        AppenderPage::decode(&s_img).is_err(),
        "unknown state refuses"
    );
    let mut unsorted = page.clone();
    unsorted.slots.truncate(2);
    unsorted.slots.swap(0, 1);
    let u_img = unsorted.encode().unwrap();
    assert!(
        AppenderPage::decode(&u_img).is_err(),
        "slot entries must be slot-ascending"
    );
    let mut count = page.clone();
    count.slots.clear();
    let mut c_img = count.encode().unwrap();
    c_img[304..306].copy_from_slice(&((SLOT_PAGE_BUDGET as u16) + 1).to_le_bytes());
    let sum = recompute_checksum(&c_img);
    c_img[16..24].copy_from_slice(&sum.to_le_bytes());
    assert!(
        AppenderPage::decode(&c_img).is_err(),
        "a slot count past the budget is a claim, never an allocation authority"
    );
}

/// The page's own checksum law (xxh3_64 with the field zeroed) — used to
/// build checksum-valid but structurally wrong images.
fn recompute_checksum(img: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&img[..16]);
    h.update(&[0u8; 8]);
    h.update(&img[24..]);
    h.digest()
}

#[test]
fn newest_valid_page_wins_and_a_torn_newest_falls_back_to_its_predecessor() {
    let mut images: Vec<Vec<u8>> = Vec::new();
    for g in 1..=4u64 {
        let mut p = AppenderPage::free(1, g);
        p.term = g;
        images.push(p.encode().unwrap());
    }
    let (slot, newest) = newest_valid(&images).expect("a valid page");
    assert_eq!((slot, newest.generation), (3, 4));
    // Tear the newest: its predecessor wins.
    images[3][100] ^= 0xFF;
    let (slot, newest) = newest_valid(&images).expect("a predecessor");
    assert_eq!((slot, newest.generation), (2, 3));
    // Tear that too: two of slack (§5.3.2).
    images[2][100] ^= 0xFF;
    let (slot, newest) = newest_valid(&images).expect("a second predecessor");
    assert_eq!((slot, newest.generation), (1, 2));
    // Everything torn / blank ⇒ nothing.
    images[1][100] ^= 0xFF;
    images[0] = vec![0u8; APPENDER_PAGE_LEN];
    assert!(newest_valid(&images).is_none());
    // Generation order, not slot order, decides.
    let mut p9 = AppenderPage::free(1, 9);
    p9.term = 9;
    let imgs = [
        p9.encode().unwrap(),
        AppenderPage::free(1, 2).encode().unwrap(),
    ];
    assert_eq!(newest_valid(&imgs).unwrap().0, 0);
}

#[test]
fn page_slot_rotation_covers_the_four_slots() {
    let slots: Vec<usize> = (1..=8u64).map(page_slot_for).collect();
    assert_eq!(slots, vec![1, 2, 3, 0, 1, 2, 3, 0]);
}

// ---------------------------------------------------------------------------
// §5.3.1 — the directory chain.
// ---------------------------------------------------------------------------

#[test]
fn directory_header_round_trips_and_pairs_per_extent_derive_from_the_node_size() {
    // Every page but the header page, in pairs.
    assert_eq!(dir_pairs_per_extent(64 * 1024), 7);
    assert_eq!(dir_pairs_per_extent(256 * 1024), 31);
    assert_eq!(dir_pairs_per_extent(1024 * 1024), 127);
    let hdr = DirHeader {
        chain_index: 3,
        next: ExtentRef {
            start: 0x40_0000,
            len: 0x4_0000,
        },
        pairs: 31,
    };
    let img = hdr.encode();
    assert_eq!(img.len(), APPENDER_PAGE_LEN);
    assert_eq!(DirHeader::decode(&img).unwrap(), hdr);
    let mut torn = img.clone();
    torn[20] ^= 1;
    assert!(DirHeader::decode(&torn).is_err());
    assert!(DirHeader::decode(&img[..100]).is_err());
    assert!(DirHeader::decode(&[0u8; APPENDER_PAGE_LEN]).is_err());
}

// ---------------------------------------------------------------------------
// §1.6 / §6.1 — the ring derivation and the capacity.
// ---------------------------------------------------------------------------

#[test]
fn ring_size_derivation_floor_ceiling_and_capacity_tie() {
    // Floor: the checkpoint carve-out + one max entry, rounded to a power
    // of two — 512 KiB, derived, not declared.
    assert_eq!(
        SYM_RING_FLOOR_BYTES,
        (checkpoint_reserve_bytes(512 * 1024) + MAX_ENTRY_LEN).next_power_of_two()
    );
    assert_eq!(SYM_RING_FLOOR_BYTES, 512 * 1024);
    // Ceiling: the solo ring's derivation.
    assert_eq!(sym_ring_ceiling_bytes(VOL_LEN), journal_ring_len(VOL_LEN));
    let tib = 1u64 << 40;
    assert_eq!(sym_ring_ceiling_bytes(tib), journal_ring_len(tib));
    // A fresh join (no EWMA) takes the floor; a runaway rate the ceiling.
    assert_eq!(
        appender_ring_bytes_derived(0, VOL_LEN),
        SYM_RING_FLOOR_BYTES
    );
    assert_eq!(
        appender_ring_bytes_derived(u64::MAX / 4, VOL_LEN),
        journal_ring_len(VOL_LEN)
    );
    // The middle: 2 × rate × CHECKPOINT_MAX_AGE — 1 MiB/s ⇒ 2 MiB.
    let rate = 1024 * 1024;
    assert_eq!(
        appender_ring_bytes_derived(rate, tib),
        2 * rate * CHECKPOINT_MAX_AGE_MS as u64 / 1000
    );
    // Capacity: the ring budget (heap/16) over the ring size.
    let heap = 4 * tib;
    assert_eq!(ring_budget_bytes(heap), heap / 16);
    assert_eq!(
        appenders_capacity(heap, SYM_RING_FLOOR_BYTES),
        heap / 16 / SYM_RING_FLOOR_BYTES
    );
    assert_eq!(appenders_capacity(heap, 0), 0);
}

#[test]
fn routing_slot_and_forest_slot_convert_both_ways_around_the_native_slot() {
    let native = 5u16;
    assert_eq!(forest_slot_of_page_slot(native, native), NATIVE_FOREST_SLOT);
    assert_eq!(forest_slot_of_page_slot(7, native), guest_forest_slot(7));
    assert_eq!(
        page_slot_of_forest_slot(NATIVE_FOREST_SLOT, native).unwrap(),
        native
    );
    assert_eq!(
        page_slot_of_forest_slot(guest_forest_slot(7), native).unwrap(),
        7
    );
    assert!(
        page_slot_of_forest_slot(guest_forest_slot(native), native).is_err(),
        "the native slot's guest keyspace is never minted"
    );
    assert!(page_slot_of_forest_slot(guest_forest_slot(u16::MAX) + 1, native).is_err());
    for s in [0u16, 1, 100, u16::MAX] {
        if s == native {
            continue;
        }
        assert_eq!(
            page_slot_of_forest_slot(forest_slot_of_page_slot(s, native), native).unwrap(),
            s
        );
    }
}

// ---------------------------------------------------------------------------
// §6.4 / §7.1 — the superblock and the format-time image.
// ---------------------------------------------------------------------------

#[test]
fn appender_dir_rides_sector_zero_only_under_bit_17() {
    let mut sb =
        SuperblockV3::plan(VOL_LEN, NODE_SIZE, Some(RING_LEN), TEST_UUID, TEST_SEED).expect("plan");
    assert_eq!(
        sb.appender_dir,
        ExtentRef { start: 0, len: 0 },
        "plan never names a directory"
    );
    let flat = sb.encode_sector().unwrap();
    assert!(
        flat[136..152].iter().all(|b| *b == 0),
        "sector 0 bytes [136, 152) are zero on a bit-17-absent volume"
    );
    // Under bit 17 the field round-trips.
    sb.features_incompat |= FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST;
    sb.appender_dir = ExtentRef {
        start: sb.heap.start,
        len: NODE_SIZE as u64,
    };
    let img = sb.encode_sector().unwrap();
    assert_eq!(img.len(), SUPERBLOCK_V3_LEN);
    let back = SuperblockV3::decode_sector(&img).unwrap();
    assert_eq!(back.appender_dir, sb.appender_dir);
    // Without the bit a named directory is unrepresentable — refused at
    // the encoder AND the decoder (the byte-identity law's two teeth).
    sb.features_incompat &= !FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST;
    assert!(sb.encode_sector().is_err());
    let mut forged = flat.clone();
    forged[136..144].copy_from_slice(&sb.heap.start.to_le_bytes());
    forged[144..152].copy_from_slice(&(NODE_SIZE as u64).to_le_bytes());
    // Re-checksum so only the gate can refuse it.
    let sum = xxhash_rust::xxh3::xxh3_64(&{
        let mut z = forged.clone();
        z[120..128].fill(0);
        z
    });
    forged[120..128].copy_from_slice(&sum.to_le_bytes());
    assert!(SuperblockV3::decode_sector(&forged).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_without_the_seam_leaves_the_directory_and_the_fixed_ring_untouched() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, false).await;
    let sb = superblock_of(&file).await;
    assert_eq!(sb.appender_dir, ExtentRef { start: 0, len: 0 });
    // The journal extent's first four pages are ring pages on a flat
    // volume — zero after format (nothing to replay).
    let head = squeezefs::uring_fs::read_at(
        file.path(),
        sb.journal.start,
        (APPENDER0_RESERVED_PAGES * APPENDER_PAGE_LEN as u64) as usize,
    )
    .await
    .unwrap();
    assert!(head.iter().all(|b| *b == 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_under_the_seam_writes_appender_zeros_page_pair_and_the_first_directory_extent() {
    let file = NamedTempFile::new().unwrap();
    build_image(&file, true).await;
    let sb = superblock_of(&file).await;
    assert!(sb.symmetric_forest_stamped());
    assert_eq!(sb.appender_dir.len, NODE_SIZE as u64);
    assert!(sb.appender_dir.start >= sb.heap.start && sb.appender_dir.end() <= sb.heap.end());
    let entries = read_directory(file.path(), &sb).await.expect("directory");
    // Appender 0 + every pair of the first extent (7 at 64 KiB nodes).
    assert_eq!(
        entries.len() as u64,
        1 + dir_pairs_per_extent(NODE_SIZE as u64)
    );
    let zero = entries[0].page.as_ref().expect("appender 0's page");
    assert_eq!(zero.appender_id, 0);
    assert_eq!(
        zero.state,
        AppenderState::Free,
        "nobody has joined at format"
    );
    assert_eq!(zero.segments, vec![appender0_ring_extent(&sb.journal)]);
    assert_eq!(
        entries[0].dir_offsets,
        [sb.journal.start, sb.journal.start + 4096]
    );
    assert_eq!(
        appender0_page_offsets(&sb.journal)[3],
        sb.journal.start + 3 * 4096
    );
    for e in &entries[1..] {
        assert!(e.page.is_none(), "an unallocated id has no page");
    }
}
