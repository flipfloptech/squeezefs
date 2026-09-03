//! Fuzz the **kvmap block-map tree** codecs
//! (`src/meta_backend/kv/block_map.rs` — `docs/design-kvmap-block-map-tree.md`
//! §2/§12): the `owner_ino ‖ block_index` key, the versioned value in its
//! five kinds (POINT / STRING / RUN / POINT2 / RUN2), and the head
//! sentinel grammar `kvmap:1[;sweep:K][;gen:N]` that the layout's
//! `block_map_id` carries.
//!
//! These are on-disk metadata (incompat bit 16): a mount resolves every
//! striped block of a PB-class file through them, so a mapping that
//! decodes WRONG resolves a block to the wrong device bytes — the exact
//! failure the tree exists to prevent. The law (spec §11 TEST-4 + the
//! module's own): every decoder is total, and the grammar is EXACT — the
//! parser accepts nothing its encoder cannot produce, so whatever decodes
//! must re-encode to the SAME bytes/string. The reserved index
//! (`u32::MAX`, design A5), an unknown kind, a run outside `2..=RUN_LEN_MAX`
//! (A6) and a zero/wrapping incarnation stamp are all REFUSALS, never
//! panics.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::block_map::{
    block_map_key, decode_block_map_key, decode_block_map_value, index_range_from,
    parse_kvmap_head, KvmapHead, MapEntry, BLOCK_MAP_KEY_LEN, RUN_LEN_MAX,
};

fuzz_target!(|data: &[u8]| {
    // --- the key ----------------------------------------------------------
    if let Ok((ino, idx)) = decode_block_map_key(data) {
        assert_ne!(
            idx,
            u32::MAX,
            "the reserved index must be refused, not decoded"
        );
        let re = block_map_key(ino, idx).expect("a decoded key re-encodes");
        assert_eq!(&re[..], data, "the key is byte-exact");
        // The range window for this ino contains its own key and stops
        // short of the next ino: the bound is the reserved index, raw.
        let (lo, hi) = index_range_from(ino, idx);
        assert!(
            lo <= re && re <= hi,
            "a key must sit inside its own range window"
        );
        assert!(
            decode_block_map_key(&hi).is_err(),
            "the end bound is a bound, never a key"
        );
    }
    if data.len() >= BLOCK_MAP_KEY_LEN {
        // The constructive direction on the same bytes: any (ino, idx)
        // pair except the reserved index encodes and decodes back.
        let ino = u64::from_be_bytes(data[0..8].try_into().unwrap());
        let idx = u32::from_be_bytes(data[8..12].try_into().unwrap());
        match block_map_key(ino, idx) {
            Ok(k) => assert_eq!(decode_block_map_key(&k).ok(), Some((ino, idx))),
            Err(_) => assert_eq!(idx, u32::MAX, "only the reserved index is refused"),
        }
    }

    // --- the value ----------------------------------------------------------
    if let Ok(entry) = decode_block_map_value(data) {
        let re = entry.encode();
        assert_eq!(re, data, "a decoded value re-encodes byte-identically");
        assert_eq!(
            entry.encoded_len(),
            re.len(),
            "encoded_len is the encode's length"
        );
        let run = entry.run_len();
        match &entry {
            MapEntry::Run { len, .. } | MapEntry::RunStamped { len, .. } => {
                assert!(
                    (2..=RUN_LEN_MAX).contains(len),
                    "run length outside A6's bound"
                );
                assert_eq!(run, *len);
            }
            MapEntry::Point { .. } | MapEntry::String(_) | MapEntry::PointStamped { .. } => {
                assert_eq!(run, 1);
            }
        }
        if let MapEntry::PointStamped { incarnation, .. } = &entry {
            assert_ne!(
                *incarnation, 0,
                "stamp 0 has a POINT spelling — must refuse"
            );
        }
        if let MapEntry::RunStamped {
            len,
            start_incarnation,
            ..
        } = &entry
        {
            assert_ne!(*start_incarnation, 0);
            assert!(
                start_incarnation.checked_add(u64::from(*len) - 1).is_some(),
                "a wrapping RUN2 span cannot have been emitted"
            );
        }
    }

    // --- the head sentinel --------------------------------------------------
    if let Ok(s) = std::str::from_utf8(data) {
        for candidate in [s.to_string(), format!("kvmap:{s}")] {
            if let Ok(head) = parse_kvmap_head(&candidate) {
                // Exactness covers the `gen:0` law by itself: a parse of
                // ";gen:0" would re-encode WITHOUT the segment and fail here
                // (gen 0 is ABSENT — `kvmap:1` legitimately parses to it).
                assert_eq!(head.encode(), candidate, "the head grammar is exact");
                assert!(
                    !candidate.contains(";gen:0"),
                    "an explicit gen:0 has no encoder — must refuse"
                );
                let again = parse_kvmap_head(&head.encode()).expect("a re-encoded head parses");
                assert_eq!(again, head);
            }
        }
    }
    // The constructive direction: every (sweep, gen) the encoder can
    // produce parses back — gen 0 is encoded as absence by law.
    if data.len() >= 13 {
        let sweep = (data[0] & 1 == 1).then(|| u32::from_le_bytes(data[1..5].try_into().unwrap()));
        let gen = u64::from_le_bytes(data[5..13].try_into().unwrap());
        let head = KvmapHead {
            sweep_cursor: sweep,
            gen,
        };
        let s = head.encode();
        assert_eq!(parse_kvmap_head(&s).expect("an encoded head parses"), head);
    }
});
