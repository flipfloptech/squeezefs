//! Fuzz the **journal entry payload** decoder
//! (`src/meta_backend/kv/journal.rs::decode_entry_payload`).
//!
//! Replay reads this at EVERY mount, over a ring whose tail is by
//! definition partially-written. §4.1's rule is that nothing inside the
//! replay window ever fails a mount loud *by panicking* — a malformed
//! payload is an error the caller classifies, so this decoder must be
//! total over arbitrary bytes (spec §11 TEST-4).
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::journal::{decode_entry_payload, encode_entry_payload, untag};

fuzz_target!(|data: &[u8]| {
    let Ok(records) = decode_entry_payload(data) else {
        return;
    };
    // Everything a successful decode reports must be structurally sound:
    // a tree id inside the §4.2 table and a non-empty key.
    for (tag, r) in &records {
        let (tree_id, _level) = untag(*tag);
        assert!(
            tree_id <= 0x0F,
            "untag must never report a tree id outside a nibble"
        );
        assert!(!r.key.is_empty(), "an empty key must have been rejected");
    }
    // Round-trip: re-encoding the decoded set and decoding again must be
    // a fixpoint (replay applies the decoded records, so a lossy decode
    // is a silently-diverging mount).
    let re = encode_entry_payload(&records);
    let again = decode_entry_payload(&re).expect("a re-encoded payload decodes");
    assert_eq!(again.len(), records.len(), "payload must round-trip");
    for (a, b) in again.iter().zip(records.iter()) {
        assert_eq!(a.0, b.0, "tag round-trips");
        assert_eq!(a.1.key, b.1.key, "key round-trips");
        assert_eq!(a.1.seq, b.1.seq, "seq round-trips");
        assert_eq!(a.1.value, b.1.value, "value round-trips");
    }
});
