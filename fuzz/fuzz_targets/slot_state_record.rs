//! Fuzz the **tree-0 `slot_state` and `slot_tails` record codecs** of the
//! symmetric-metadata forest (`src/meta_backend/kv/slot_state.rs` —
//! `docs/design-symmetric-metadata.md` §5.2.2 / §5.4.2 / §5.8.2, incompat
//! bit 17).
//!
//! A `slot_state:{slot}` record is where a guest slot tree's ROOT lives:
//! mount replays tree 0 first and opens every slot tree the records name,
//! so a record that decodes wrong is a whole slot tree unreachable (or a
//! stale root mounted as live). The value is versioned and forward-only;
//! the `Leased` variant is the PR-4 lessee record (the root then rides
//! the lessee's appender page); both variants are FIXED-SIZE since PR 4
//! review round 3. A `slot_tails:{slot}` record is the flushed-leaf log
//! tails a release recorded — INLINE under the KV value cap, SPILLED to
//! heap extents the record names above it (PR 5's frame screen reads
//! it). The laws (spec §11 TEST-4):
//!
//! 1. **Total.** Key and value decoders answer `Ok` or a typed error on
//!    arbitrary bytes — never a panic, never an allocation driven by the
//!    `n_tails` / `n_runs` count beyond what the bytes actually hold (the
//!    count is a claim checked against the length BEFORE any entry is
//!    read).
//! 2. **Exact.** `decode ∘ encode = id` over the encoder's whole domain
//!    (both `slot_state` variants; inline tails at every length below the
//!    spill sentinel and spilled sets at every run count), and whatever
//!    `decode` accepts re-encodes to the SAME bytes.
//! 3. **Forward-only.** A version byte other than the one this binary
//!    writes refuses; an unknown variant refuses.
//! 4. **The keys** are `b"slot_state:" ‖ slot: u32 BE` and `b"slot_tails:"
//!    ‖ slot: u32 BE`, exact in both directions, every slot's key inside
//!    its census window.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::slot_state::{
    custody_quarantine_key, custody_quarantine_key_range, decode_appender_hint,
    decode_custody_quarantine, decode_custody_quarantine_key, decode_slot_state_key,
    decode_slot_tails_key, encode_appender_hint, encode_custody_quarantine, slot_state_key,
    slot_state_key_range, slot_tails_key, slot_tails_key_range, SlotState, SlotTails,
    SlotTailsRecord, TailsSpill, APPENDER_HINT_LEN, APPENDER_HINT_VERSION,
    CUSTODY_QUARANTINE_KEY_LEN, CUSTODY_QUARANTINE_LEN, CUSTODY_QUARANTINE_VERSION, LEASED_LEN,
    SLOT_STATE_KEY_LEN, SLOT_STATE_VERSION, SLOT_TAILS_FIXED_LEN, SLOT_TAILS_KEY_LEN,
    SLOT_TAILS_VERSION, TAILS_SPILLED, TAIL_ENTRY_LEN, UNLEASED_LEN,
};
use squeezefs::meta_backend::kv::tree::RootPtr;

fuzz_target!(|data: &[u8]| {
    // --- the keys -----------------------------------------------------------
    match decode_slot_state_key(data) {
        Ok(slot) => {
            assert_eq!(data.len(), SLOT_STATE_KEY_LEN);
            assert_eq!(slot_state_key(slot), data, "the key is byte-exact");
            let (lo, hi) = slot_state_key_range();
            assert!(
                lo <= data.to_vec() && data.to_vec() <= hi,
                "every slot's key sits inside the census window"
            );
        }
        Err(_) => assert!(
            data.len() != SLOT_STATE_KEY_LEN || !data.starts_with(b"slot_state:"),
            "only a wrong prefix or length refuses"
        ),
    }
    match decode_slot_tails_key(data) {
        Ok(slot) => {
            assert_eq!(data.len(), SLOT_TAILS_KEY_LEN);
            assert_eq!(slot_tails_key(slot), data, "the tails key is byte-exact");
            let (lo, hi) = slot_tails_key_range();
            assert!(lo <= data.to_vec() && data.to_vec() <= hi);
        }
        Err(_) => assert!(
            data.len() != SLOT_TAILS_KEY_LEN || !data.starts_with(b"slot_tails:"),
            "only a wrong prefix or length refuses"
        ),
    }
    // PR 10 review round 8 (Issue 36): the custody-quarantine record.
    match decode_custody_quarantine_key(data) {
        Ok(slot) => {
            assert_eq!(data.len(), CUSTODY_QUARANTINE_KEY_LEN);
            assert_eq!(custody_quarantine_key(slot), data, "the key is byte-exact");
            let (lo, hi) = custody_quarantine_key_range();
            assert!(lo <= data.to_vec() && data.to_vec() <= hi);
        }
        Err(_) => assert!(
            data.len() != CUSTODY_QUARANTINE_KEY_LEN || !data.starts_with(b"custody_quarantine:"),
            "only a wrong prefix or length refuses"
        ),
    }
    match decode_custody_quarantine(data) {
        Ok(until) => {
            assert_eq!(data.len(), CUSTODY_QUARANTINE_LEN);
            assert_eq!(data[0], CUSTODY_QUARANTINE_VERSION);
            assert_eq!(encode_custody_quarantine(until), data, "byte-exact");
        }
        Err(_) => assert!(
            data.len() != CUSTODY_QUARANTINE_LEN || data[0] != CUSTODY_QUARANTINE_VERSION,
            "only a wrong length or version refuses"
        ),
    }
    // PR 13g (F-R5): the appender-hint record — the rejoin's ring and
    // grant words.
    match decode_appender_hint(data) {
        Ok(hint) => {
            assert_eq!(data.len(), APPENDER_HINT_LEN);
            assert_eq!(data[0], APPENDER_HINT_VERSION);
            assert_eq!(encode_appender_hint(hint), data, "byte-exact");
        }
        Err(_) => assert!(
            data.len() != APPENDER_HINT_LEN || data[0] != APPENDER_HINT_VERSION,
            "only a wrong length or version refuses"
        ),
    }
    if data.len() >= 4 {
        let slot = u32::from_be_bytes(data[0..4].try_into().unwrap());
        assert_eq!(
            decode_slot_state_key(&slot_state_key(slot)).ok(),
            Some(slot)
        );
        assert_eq!(
            decode_slot_tails_key(&slot_tails_key(slot)).ok(),
            Some(slot)
        );
        assert_eq!(
            decode_custody_quarantine_key(&custody_quarantine_key(slot)).ok(),
            Some(slot)
        );
    }
    if data.len() >= 8 {
        let until = u64::from_le_bytes(data[0..8].try_into().unwrap());
        assert_eq!(
            decode_custody_quarantine(&encode_custody_quarantine(until)).ok(),
            Some(until)
        );
    }

    // --- slot_state: decode side --------------------------------------------
    match SlotState::decode(data) {
        Ok(state) => {
            assert_eq!(
                data[0], SLOT_STATE_VERSION,
                "only this binary's version decodes"
            );
            assert_eq!(state.encode(), data, "the record is byte-exact");
            match &state {
                SlotState::Unleased { .. } => {
                    assert_eq!(data[1], 1);
                    assert_eq!(data.len(), UNLEASED_LEN);
                }
                SlotState::Leased { .. } => {
                    assert_eq!(data[1], 2);
                    assert_eq!(data.len(), LEASED_LEN);
                }
            }
        }
        Err(_) => {
            if data.len() >= 2 && data[0] == SLOT_STATE_VERSION {
                // A fixed-size image refuses only at the wrong length.
                if data[1] == 1 {
                    assert_ne!(data.len(), UNLEASED_LEN, "a well-formed Unleased decodes");
                }
                if data[1] == 2 {
                    assert_ne!(data.len(), LEASED_LEN, "a well-formed Leased decodes");
                }
            }
        }
    }

    // --- slot_tails: decode side --------------------------------------------
    match SlotTailsRecord::decode(data) {
        Ok(rec) => {
            assert_eq!(data[0], SLOT_TAILS_VERSION);
            let re = rec.encode().expect("a decoded tails record re-encodes");
            assert_eq!(re, data, "the tails record is byte-exact");
            match &rec.tails {
                SlotTails::Inline(v) => {
                    assert_eq!(data.len(), SLOT_TAILS_FIXED_LEN + v.len() * TAIL_ENTRY_LEN);
                    assert!(v.len() < usize::from(TAILS_SPILLED));
                }
                SlotTails::Spilled(sp) => {
                    assert_eq!(
                        u16::from_le_bytes([data[5], data[6]]),
                        TAILS_SPILLED,
                        "the sentinel names a spill"
                    );
                    assert_eq!(rec.tails.spill_addrs().len(), sp.runs.len());
                }
            }
        }
        Err(_) => {
            if data.len() >= SLOT_TAILS_FIXED_LEN && data[0] == SLOT_TAILS_VERSION {
                let n = u16::from_le_bytes([data[5], data[6]]);
                if n != TAILS_SPILLED {
                    // An inline image refuses only when its length is not
                    // the fixed part plus exactly the entries it names.
                    assert_ne!(
                        data.len(),
                        SLOT_TAILS_FIXED_LEN + usize::from(n) * TAIL_ENTRY_LEN,
                        "a well-formed inline image decodes"
                    );
                } else if data.len() >= SLOT_TAILS_FIXED_LEN + 2 {
                    let runs = usize::from(u16::from_le_bytes([
                        data[SLOT_TAILS_FIXED_LEN],
                        data[SLOT_TAILS_FIXED_LEN + 1],
                    ]));
                    assert_ne!(
                        data.len(),
                        SLOT_TAILS_FIXED_LEN + 2 + runs * 12 + 8,
                        "a well-formed spill image decodes"
                    );
                }
            }
        }
    }

    // --- encode side, over the encoders' whole domain -------------------------
    if data.len() >= 1 + 8 + 8 + 8 + 4 + 4 + 8 + 2 {
        let addr = u64::from_le_bytes(data[1..9].try_into().unwrap());
        let seq = u64::from_le_bytes(data[9..17].try_into().unwrap());
        let cursor = u64::from_le_bytes(data[17..25].try_into().unwrap());
        let g = u32::from_le_bytes(data[25..29].try_into().unwrap());
        let slot_tree_extents = u32::from_le_bytes(data[29..33].try_into().unwrap());
        let last_written = u64::from_le_bytes(data[33..41].try_into().unwrap());
        let n = usize::from(u16::from_le_bytes([data[41], data[42]])) % 64; // bounded work
        let unleased = SlotState::Unleased {
            root: RootPtr { addr, seq },
            cursor,
            g,
            slot_tree_extents,
            last_written,
            seq_floor: last_written ^ cursor,
        };
        let leased = SlotState::Leased {
            appender_id: g,
            g: g.wrapping_add(1),
            page_addr: addr,
            root: RootPtr {
                addr: seq,
                seq: addr,
            },
            cursor,
            slot_tree_extents,
            seq_floor: addr ^ seq,
        };
        for state in [unleased, leased] {
            let bytes = state.encode();
            assert_eq!(
                SlotState::decode(&bytes).expect("an encoded record decodes"),
                state,
                "round trip"
            );
            // Forward-only: bump the version and the same bytes refuse.
            let mut future = bytes.clone();
            future[0] = SLOT_STATE_VERSION.wrapping_add(1);
            assert!(
                SlotState::decode(&future).is_err(),
                "a future version refuses"
            );
            // An unknown variant refuses.
            let mut variant = bytes.clone();
            variant[1] = 0x7F;
            assert!(
                SlotState::decode(&variant).is_err(),
                "an unknown variant refuses"
            );
            assert!(SlotState::decode(&bytes[..bytes.len() - 1]).is_err());
        }
        let entries: Vec<(u64, u32)> = (0..n)
            .map(|i| (addr.wrapping_add(i as u64), g.wrapping_add(i as u32)))
            .collect();
        let inline = SlotTailsRecord {
            g,
            tails: SlotTails::Inline(entries.clone()),
        };
        let spilled = SlotTailsRecord {
            g,
            tails: SlotTails::Spilled(TailsSpill {
                runs: entries
                    .iter()
                    .map(|(a, c)| (a & !0xFFF, c % 1024 + 1))
                    .collect(),
                checksum: seq,
            }),
        };
        for rec in [inline, spilled] {
            let bytes = rec.encode().expect("every emittable tails record encodes");
            assert_eq!(
                SlotTailsRecord::decode(&bytes).expect("an encoded tails record decodes"),
                rec,
                "round trip"
            );
            let mut future = bytes.clone();
            future[0] = SLOT_TAILS_VERSION.wrapping_add(1);
            assert!(SlotTailsRecord::decode(&future).is_err());
            // Truncation refuses (the counts are claims, not allocation
            // authorities).
            assert!(SlotTailsRecord::decode(&bytes[..bytes.len() - 1]).is_err());
        }
    }
});
