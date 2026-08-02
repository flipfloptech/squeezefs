//! Fuzz the v3 **bset** image parser (`src/meta_backend/kv/bset.rs`).
//!
//! Threat model (spec §11 TEST-4): a bset image is on-disk metadata. It is
//! xxh3-checksummed but **not authenticated** — anyone who can write the
//! device can compute a valid checksum, and a torn/aged/misdirected write
//! produces arbitrary bytes with a valid-looking header. The crash
//! contract's whole point is that such bytes are *detected*, never
//! branched on and never fatal.
//!
//! Invariants asserted: `parse` returns or errors, never panics, never
//! over-allocates; and every record a successful parse hands out is
//! in-bounds and re-decodes.
//!
//! Two input arms, because a blind fuzzer essentially never guesses a
//! valid xxh3:
//!   * **raw** — the bytes as given (exercises the reject paths);
//!   * **restamped** — the same bytes with a *recomputed* checksum, which
//!     is exactly the corrupt-but-checksum-valid image a device-level
//!     attacker or a writer bug produces. This arm is what reaches the
//!     record walk at all.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::bset::{checksum_image, BsetView, BSET_HEADER_LEN};

fn exercise(buf: &[u8]) {
    let Ok(view) = BsetView::parse(buf) else {
        return;
    };
    // A successful parse promises infallible, in-bounds record access.
    let n = view.len();
    assert_eq!(view.is_empty(), n == 0);
    let mut prev: Option<(Vec<u8>, u64)> = None;
    for i in 0..n {
        let r = view.record(i);
        assert!(!r.key.is_empty(), "parse must reject empty keys");
        // The parse-time ordering promise the fold algebra relies on.
        if let Some((pk, ps)) = prev {
            assert!(
                (pk.as_slice(), ps) < (r.key, r.seq),
                "records must be strictly (key, seq) ascending after parse"
            );
        }
        prev = Some((r.key.to_vec(), r.seq));
        // `find` must agree with the record it names.
        let range = view.find(r.key);
        assert!(range.contains(&i), "find must locate its own record");
    }
    let _ = view.journal_seq_horizon();
    assert_eq!(view.iter().count(), n, "iter must visit every record");
}

fuzz_target!(|data: &[u8]| {
    exercise(data);
    if data.len() >= BSET_HEADER_LEN {
        let mut restamped = data.to_vec();
        restamped[24..32].copy_from_slice(&0u64.to_le_bytes());
        let sum = checksum_image(&restamped);
        restamped[24..32].copy_from_slice(&sum.to_le_bytes());
        exercise(&restamped);
    }
});
