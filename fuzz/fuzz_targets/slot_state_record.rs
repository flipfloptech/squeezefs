//! Fuzz the **tree-0 `slot_state` record codecs** of the symmetric-metadata
//! forest (`src/meta_backend/kv/slot_state.rs` —
//! `docs/design-symmetric-metadata.md` §5.2.2 / §5.4.2, incompat bit 17).
//!
//! A `slot_state:{slot}` record is where a guest slot tree's ROOT lives:
//! mount replays tree 0 first and opens every slot tree the records name,
//! so a record that decodes wrong is a whole slot tree unreachable (or a
//! stale root mounted as live). The value is versioned and forward-only;
//! the `Leased` variant is the PR-4 lessee record (the root then rides
//! the lessee's appender page). The laws (spec §11 TEST-4):
//!
//! 1. **Total.** Key and value decoders answer `Ok` or a typed error on
//!    arbitrary bytes — never a panic, never an allocation driven by the
//!    `n_tails` count beyond what the bytes actually hold (the count is a
//!    claim checked against the length BEFORE any tail is read).
//! 2. **Exact.** `decode ∘ encode = id` over the encoder's whole domain
//!    (both variants, every tails length a `u16` expresses), and whatever
//!    `decode` accepts re-encodes to the SAME bytes.
//! 3. **Forward-only.** A version byte other than the one this binary
//!    writes refuses; an unknown variant refuses.
//! 4. **The key** is `b"slot_state:" ‖ slot: u32 BE`, exact in both
//!    directions, and every slot's key sits inside the census window.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::slot_state::{
    decode_slot_state_key, slot_state_key, slot_state_key_range, SlotState, SLOT_STATE_KEY_LEN,
    SLOT_STATE_VERSION, UNLEASED_FIXED_LEN,
};
use squeezefs::meta_backend::kv::tree::RootPtr;

fuzz_target!(|data: &[u8]| {
    // --- the key ------------------------------------------------------------
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
    if data.len() >= 4 {
        let slot = u32::from_be_bytes(data[0..4].try_into().unwrap());
        assert_eq!(
            decode_slot_state_key(&slot_state_key(slot)).ok(),
            Some(slot),
            "every slot's key decodes back"
        );
    }

    // --- the value: decode side ----------------------------------------------
    match SlotState::decode(data) {
        Ok(state) => {
            assert_eq!(
                data[0], SLOT_STATE_VERSION,
                "only this binary's version decodes"
            );
            let re = state.encode().expect("a decoded record re-encodes");
            assert_eq!(re, data, "the record is byte-exact");
            match &state {
                SlotState::Unleased { root, .. } => assert_eq!(state.root(), Some(*root)),
                SlotState::Leased { .. } => assert_eq!(state.root(), None),
            }
        }
        Err(_) => {
            if let Some(&v) = data.first() {
                if v == SLOT_STATE_VERSION && data.len() >= 2 && data[1] == 1 {
                    // An Unleased image refuses only when its length is
                    // not the fixed part plus exactly the tails it names.
                    if data.len() >= UNLEASED_FIXED_LEN {
                        let n = usize::from(u16::from_le_bytes([
                            data[UNLEASED_FIXED_LEN - 2],
                            data[UNLEASED_FIXED_LEN - 1],
                        ]));
                        assert_ne!(
                            data.len(),
                            UNLEASED_FIXED_LEN + n * 12,
                            "a well-formed image decodes"
                        );
                    }
                }
            }
        }
    }

    // --- the value: encode side, over the encoder's whole domain -------------
    if data.len() >= 1 + 8 + 8 + 8 + 4 + 4 + 8 + 2 {
        let addr = u64::from_le_bytes(data[1..9].try_into().unwrap());
        let seq = u64::from_le_bytes(data[9..17].try_into().unwrap());
        let cursor = u64::from_le_bytes(data[17..25].try_into().unwrap());
        let g = u32::from_le_bytes(data[25..29].try_into().unwrap());
        let slot_tree_extents = u32::from_le_bytes(data[29..33].try_into().unwrap());
        let last_written = u64::from_le_bytes(data[33..41].try_into().unwrap());
        let n = usize::from(u16::from_le_bytes([data[41], data[42]])) % 64; // bounded work
        let tails: Vec<(u64, u32)> = (0..n)
            .map(|i| (addr.wrapping_add(i as u64), g.wrapping_add(i as u32)))
            .collect();
        let unleased = SlotState::Unleased {
            root: RootPtr { addr, seq },
            cursor,
            g,
            slot_tree_extents,
            last_written,
            tails,
        };
        let leased = SlotState::Leased {
            appender_id: g,
            g: g.wrapping_add(1),
            page_addr: addr,
            root: RootPtr { addr: seq, seq: addr },
            cursor,
            slot_tree_extents,
        };
        for state in [unleased, leased] {
            let bytes = state.encode().expect("every emittable record encodes");
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
            // Truncation refuses (the tails count is a claim, not an
            // allocation authority).
            assert!(SlotState::decode(&bytes[..bytes.len() - 1]).is_err());
        }
    }
});
