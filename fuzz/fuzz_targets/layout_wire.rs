//! Fuzz the **layout delta wire** (`src/layout_wire.rs`) — the
//! write-commit-economy record format (`KV_LAYOUT_DELTAS`, superblock
//! incompat bit 5) plus the bincode base it folds onto.
//!
//! Both halves are on-disk metadata. `decode` must reject a wrong magic,
//! unknown flags, truncation, lying lengths, non-UTF-8 and trailing bytes
//! — and `apply` must never fold a delta onto a base it refused
//! (spec §11 TEST-4).
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::layout_wire::{decode_base_layout, encode_layout, LayoutDelta};

fuzz_target!(|data: &[u8]| {
    // --- the base decoder (bincode + the refusal ladder) ---------------
    if let Ok(layout) = decode_base_layout(data) {
        assert!(
            !layout
                .block_map_id
                .as_deref()
                .is_some_and(|id| id.starts_with("indirect:")),
            "an indirect base must be REFUSED, never returned"
        );
        let re = encode_layout(&layout).expect("a decoded base re-encodes");
        let again = decode_base_layout(&re).expect("re-encoded base decodes");
        assert_eq!(again.size, layout.size, "base must round-trip");
        assert_eq!(again.file_type, layout.file_type);
        assert_eq!(again.block_map, layout.block_map);
    }

    // --- the delta record ----------------------------------------------
    let Ok(delta) = LayoutDelta::decode(data) else {
        return;
    };
    let re = delta.encode();
    let again = LayoutDelta::decode(&re).expect("a decoded delta re-encodes and re-decodes");
    assert_eq!(again.file_type, delta.file_type, "delta must round-trip");
    assert_eq!(again.size, delta.size);
    assert_eq!(again.block_map_id, delta.block_map_id);
    assert_eq!(again.block_prefix, delta.block_prefix);
    assert_eq!(again.file_id, delta.file_id);
    assert_eq!(again.data_key, delta.data_key);
    assert_eq!(again.entries, delta.entries);

    // Folding onto a legitimate base is total: the result decodes, and
    // the delta's absolute fields won.
    let base = encode_layout(&Default::default()).expect("default layout encodes");
    if let Ok(folded) = delta.apply(&base) {
        let out = decode_base_layout(&folded).expect("a folded layout decodes");
        assert_eq!(out.size, delta.size, "the delta's size is absolute");
        assert_eq!(out.file_type, delta.file_type);
    }
});
