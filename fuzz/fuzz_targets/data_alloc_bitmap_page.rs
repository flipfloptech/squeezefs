//! Fuzz the **data allocation bitmap page codec and its kind-4 delta
//! records** (`src/data_alloc_bitmap.rs` —
//! `docs/design-symmetric-metadata.md` §5.5.1, KD-SYM-9; PR 8).
//!
//! A page is one data volume's free-list truth for 32,512 blocks: a page
//! that verifies wrong is a granted range read as FREE (the double
//! allocation the whole structure exists to prevent) or a free block
//! read as SET (a leak). The laws (spec §11 TEST-4):
//!
//! 1. **Total.** The page decoder answers `Ok` or a typed error on
//!    arbitrary bytes — never a panic, never an allocation the bytes do
//!    not hold; the delta decoder likewise over an arbitrary record.
//! 2. **Exact.** `decode ∘ encode = id` over the encoder's domain (any
//!    payload ≤ 4,064 bytes zero-extends to the page), and a page never
//!    verifies under another volume's tag, another index, or a flipped
//!    byte.
//! 3. **The replay is total and idempotent** over an arbitrary delta
//!    stream: applying it twice leaves the bitmap where one pass did.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::data_alloc_bitmap::{
    decode_data_alloc_page, decode_data_alloc_record, encode_data_alloc_page,
    is_data_alloc_delta_key, DataAllocBitmap, DATA_ALLOC_PAGE_DATA_LEN, DATA_ALLOC_PAGE_LEN,
};
use squeezefs::meta_backend::kv::record::{Record, TREE_ALLOC_RESERVED};

fuzz_target!(|data: &[u8]| {
    // --- the page, raw -------------------------------------------------
    let _ = decode_data_alloc_page(data, 0, 0);
    if data.len() >= 16 {
        let tag = u64::from_le_bytes(data[..8].try_into().unwrap());
        let idx = u32::from_le_bytes(data[8..12].try_into().unwrap());
        let _ = decode_data_alloc_page(data, tag, idx);
    }
    // --- the page, constructive: encode ∘ decode over the domain --------
    if data.len() >= 20 {
        let tag = u64::from_le_bytes(data[..8].try_into().unwrap());
        let idx = u32::from_le_bytes(data[8..12].try_into().unwrap());
        let gen = u64::from_le_bytes(data[12..20].try_into().unwrap());
        let bits = &data[20..data.len().min(20 + DATA_ALLOC_PAGE_DATA_LEN)];
        let img = encode_data_alloc_page(tag, idx, gen, bits).expect("in-domain payload");
        assert_eq!(img.len(), DATA_ALLOC_PAGE_LEN as usize);
        let (g, got) = decode_data_alloc_page(&img, tag, idx).expect("a fresh page verifies");
        assert_eq!(g, gen);
        assert_eq!(&got[..bits.len()], bits);
        assert!(got[bits.len()..].iter().all(|b| *b == 0), "zero-extended");
        assert!(decode_data_alloc_page(&img, tag.wrapping_add(1), idx).is_err());
        assert!(decode_data_alloc_page(&img, tag, idx.wrapping_add(1)).is_err());
        let mut torn = img.clone();
        let at = 32 + (data.len() % (DATA_ALLOC_PAGE_LEN as usize - 32));
        torn[at] ^= 0x01;
        assert!(
            decode_data_alloc_page(&torn, tag, idx).is_err(),
            "a flipped byte never verifies"
        );
        assert!(
            encode_data_alloc_page(tag, idx, gen, &vec![0u8; DATA_ALLOC_PAGE_DATA_LEN + 1])
                .is_err()
        );
    }
    // --- the deltas and the replay -------------------------------------
    let key = data[..data.len().min(16)].to_vec();
    let value = data[data.len().min(16)..data.len().min(26)].to_vec();
    let rec = Record::put(key.clone(), 7, value);
    if is_data_alloc_delta_key(&key) {
        if let Ok(delta) = decode_data_alloc_record(&rec) {
            assert_eq!(
                delta.vol_tag(),
                u64::from_be_bytes(key[..8].try_into().unwrap())
            );
        }
    } else {
        assert!(decode_data_alloc_record(&rec).is_err());
    }
    let bm = DataAllocBitmap::new(0x11, 4096);
    let stream: Vec<Record> = data
        .chunks(5)
        .enumerate()
        .map(|(i, c)| {
            let block = u64::from(c[0]) | (u64::from(*c.get(1).unwrap_or(&0)) << 8);
            let set = c.get(2).is_none_or(|b| b & 1 == 0);
            let (_, r) = if set {
                squeezefs::data_alloc_bitmap::set_record(0x11, block % 5000, 3, i as u64)
            } else {
                squeezefs::data_alloc_bitmap::clear_record(0x11, block % 5000, 3, i as u64)
            };
            r
        })
        .collect();
    let once = bm.replay(stream.iter().map(|r| (TREE_ALLOC_RESERVED, r)), 3);
    let after = bm.set_blocks();
    let twice = bm.replay(stream.iter().map(|r| (TREE_ALLOC_RESERVED, r)), 3);
    assert_eq!(twice, 0, "a second pass changes nothing (idempotent)");
    assert_eq!(bm.set_blocks(), after);
    assert!(once as usize <= stream.len());
    assert!(bm.population() <= 4096);
    let other = DataAllocBitmap::new(0x11, 4096);
    assert_eq!(
        other.replay(stream.iter().map(|r| (TREE_ALLOC_RESERVED, r)), 4),
        0,
        "another holder term's deltas never fold in"
    );
    // The region loader is total: arbitrary bytes either load or refuse,
    // and an all-zero region is a fresh bitmap.
    let _ = DataAllocBitmap::from_region_image(0x11, 4096, data);
    assert_eq!(
        DataAllocBitmap::from_region_image(0x11, 4096, &[])
            .expect("all-zero = fresh")
            .population(),
        0
    );
});
