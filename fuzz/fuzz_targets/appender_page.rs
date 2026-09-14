//! Fuzz the **appender page and directory-header codecs** of the
//! symmetric-metadata appender region (`src/meta_backend/kv/appender.rs`
//! — `docs/design-symmetric-metadata.md` §5.3.2, incompat bit 17, PR 2).
//!
//! The page is an appender's directory entry AND its ledger record: its
//! ring segments, its replay tail, the roots of the slots it leases.
//! Mount reads it newest-valid-wins over four page slots and recovers
//! rings by what it says, so a page that decodes wrong is a ring replayed
//! from the wrong place or a slot tree opened at a stale root. The laws
//! (spec §11 TEST-4):
//!
//! 1. **Total.** `classify_page` / `AppenderPage::decode` /
//!    `DirHeader::decode` answer `Blank`, `Valid` or `Corrupt` (a typed
//!    error) on arbitrary bytes — never a panic, never an allocation the
//!    image's own counts drive (every count is a claim checked against
//!    its bound BEFORE an element is read).
//! 2. **Canonical.** `decode ∘ encode = id` over the encoder's whole
//!    domain (every state, every count within the bounds), and whatever
//!    `decode` accepts re-encodes to the SAME bytes (unused table entries
//!    and reserved bytes are zero, slot entries slot-ascending).
//! 3. **Bounded.** One entry past `SLOT_PAGE_BUDGET`, one segment past
//!    `RING_SEGMENTS_MAX`, one run past `GRANT_RUNS_MAX` refuse at the
//!    encoder; a checksum-valid image claiming more refuses at the decoder.
//! 4. **Newest-valid-wins.** Over any image set the valid page with the
//!    highest generation is chosen; tearing it falls back to the next.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::appender::{
    classify_page, newest_valid, AppenderIdentity, AppenderPage, AppenderState, DirHeader,
    GrantRun, PageRead, SlotEntry, SlotEntryState, APPENDER_PAGE_LEN, GRANT_RUNS_MAX,
    RING_SEGMENTS_MAX, SLOT_PAGE_BUDGET,
};
use squeezefs::meta_backend::kv::superblock::ExtentRef;
use squeezefs::meta_backend::kv::tree::RootPtr;

fuzz_target!(|data: &[u8]| {
    // --- decode side: total, canonical -------------------------------------
    match classify_page(data) {
        PageRead::Blank => assert!(data.iter().all(|b| *b == 0)),
        PageRead::Valid(p) => {
            assert_eq!(data.len(), APPENDER_PAGE_LEN);
            assert_eq!(p.encode().expect("re-encodes"), data, "canonical");
            assert!(p.segments.len() <= RING_SEGMENTS_MAX);
            assert!(p.grant.len() <= GRANT_RUNS_MAX);
            assert!(p.slots.len() <= SLOT_PAGE_BUDGET);
            for w in p.slots.windows(2) {
                assert!(w[0].slot < w[1].slot, "slot-ascending");
            }
        }
        PageRead::Corrupt(_) => {}
    }
    if let Ok(h) = DirHeader::decode(data) {
        assert_eq!(h.encode(), data, "canonical header");
    }
    let _ = newest_valid(&[data, &[0u8; APPENDER_PAGE_LEN]]);

    // --- encode side, over the encoder's domain ------------------------------
    if data.len() >= 48 {
        let u64_at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
        let mut page = AppenderPage::free(
            u32::from_le_bytes(data[0..4].try_into().unwrap()),
            u64_at(4),
        );
        page.identity = AppenderIdentity {
            node_token: u64_at(12),
            mount_slot: u32::from_le_bytes(data[20..24].try_into().unwrap()),
            writer_id: u128::from_le_bytes(data[24..40].try_into().unwrap()),
        };
        page.term = u64_at(40);
        page.state = match data[40] % 4 {
            0 => AppenderState::Free,
            1 => AppenderState::Live,
            2 => AppenderState::Recovering,
            _ => AppenderState::Recovered,
        };
        page.is_manager = data[41] & 1 == 1;
        page.home_volume = data[42];
        page.ledger_tail_seq = u64_at(12) ^ u64_at(4);
        page.ckpt_seq = page.ledger_tail_seq.wrapping_add(1);
        page.seq_offset = u64_at(20) ^ u64_at(28);
        let n_segments = usize::from(data[43]) % (RING_SEGMENTS_MAX + 1);
        let n_runs = usize::from(data[44]) % (GRANT_RUNS_MAX + 1);
        let n_slots =
            usize::from(u16::from_le_bytes([data[45], data[46]])) % (SLOT_PAGE_BUDGET + 1);
        page.segments = (0..n_segments as u64)
            .map(|i| ExtentRef {
                start: u64_at(12).wrapping_add(i * 0x4_0000),
                len: 0x4_0000,
            })
            .collect();
        page.grant = (0..n_runs as u64)
            .map(|i| GrantRun {
                start: i * 8,
                len: u32::from(data[47]),
            })
            .collect();
        page.slots = (0..n_slots as u16)
            .map(|i| SlotEntry {
                slot: i.wrapping_mul(3),
                state: if i % 2 == 0 {
                    SlotEntryState::Live
                } else {
                    SlotEntryState::Releasing
                },
                g: u32::from(i),
                slot_tree_extents: u32::from(i) * 2,
                root: RootPtr {
                    addr: u64::from(i) * 0x1_0000,
                    seq: u64_at(4).wrapping_add(u64::from(i)),
                },
                cursor: u64::from(i),
            })
            .collect();
        let img = page.encode().expect("every emittable page encodes");
        assert_eq!(
            AppenderPage::decode(&img).expect("an encoded page decodes"),
            page,
            "round trip"
        );
        // One past each bound refuses at the encoder.
        let mut over_slots = page.clone();
        over_slots.slots = (0..=SLOT_PAGE_BUDGET as u16)
            .map(|i| SlotEntry {
                slot: i,
                state: SlotEntryState::Live,
                g: 0,
                slot_tree_extents: 0,
                root: RootPtr { addr: 0, seq: 0 },
                cursor: 0,
            })
            .collect();
        assert!(over_slots.encode().is_err());
        let mut over_segments = page.clone();
        over_segments.segments = vec![ExtentRef { start: 0, len: 1 }; RING_SEGMENTS_MAX + 1];
        assert!(over_segments.encode().is_err());
        let mut over_runs = page.clone();
        over_runs.grant = vec![GrantRun { start: 0, len: 1 }; GRANT_RUNS_MAX + 1];
        assert!(over_runs.encode().is_err());
        // Any flipped byte refuses (the whole page is covered).
        let mut torn = img.clone();
        torn[usize::from(data[47]) % APPENDER_PAGE_LEN] ^= 0x01;
        assert!(matches!(classify_page(&torn), PageRead::Corrupt(_)));
        // Newest-valid-wins over the pair, and the torn newest falls back.
        let mut older = page.clone();
        older.generation = page.generation.wrapping_sub(1);
        let older_img = older.encode().expect("encodes");
        if page.generation > 0 {
            assert_eq!(
                newest_valid(&[older_img.clone(), img.clone()]).unwrap().0,
                1
            );
            assert_eq!(newest_valid(&[older_img, torn]).unwrap().0, 0);
        }
        // The directory header round-trips and refuses a tear.
        let hdr = DirHeader {
            chain_index: u32::from_le_bytes(data[0..4].try_into().unwrap()),
            next: ExtentRef {
                start: u64_at(4),
                len: u64_at(12),
            },
            pairs: u16::from_le_bytes([data[45], data[46]]),
        };
        let h_img = hdr.encode();
        assert_eq!(DirHeader::decode(&h_img).unwrap(), hdr);
        let mut h_torn = h_img.clone();
        h_torn[usize::from(data[47]) % APPENDER_PAGE_LEN] ^= 0x01;
        assert!(DirHeader::decode(&h_torn).is_err());
    }
});
