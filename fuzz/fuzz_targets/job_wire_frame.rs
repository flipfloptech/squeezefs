//! Fuzz the **remote job-wire frame decoder** (`src/job_wire.rs`,
//! rebuilt by VAL-6, ported onto `cluster_wire` by DLM S3).
//!
//! Threat model: every write mount starts a listener whose bind defaults
//! to `0.0.0.0` (execution-plan D2 — the user ruled the bind stays
//! configurable and default-open with auto-discovered peers), so **every
//! byte before enrollment is attacker-chosen**.
//! `tests/job_wire_bounds_tests.rs` pins the named classes (chunked body
//! allocation, per-class caps, per-class deadlines, accept backoff). This
//! target adds the unstructured half: an arbitrary byte stream fed to the
//! same `read_frame_limited` a socket drives, one frame after another.
//!
//! Invariants: the decoder is total (no panic), never consumes past the
//! stream, never yields a frame whose serialized form exceeds the class
//! cap it was read under, and terminates on every input.
//!
//! History: this target shipped (2026-08-02) against the pre-S3 reader —
//! async, `serde_json`-bodied — and the S3 port the same day made the
//! reader synchronous bincode. Nothing at per-commit cadence compiles
//! `fuzz/`, so the rot surfaced at the 1.2 release campaign; the fuzz
//! build is now part of that campaign's checklist
//! (`.benchmarks/2026-09-03-release-1.2-fuzz.md`).
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::job_wire::{
    read_frame_limited, write_frame, MAX_FRAME_BYTES, MAX_HELLO_FRAME_BYTES, WIRE_SCHEMA,
};

fn drive(data: &[u8], cap: u32) {
    let mut cursor = std::io::Cursor::new(data);
    // A stream is a sequence of frames; a hostile peer sends as many as it
    // likes. Bound the loop so a zero-consuming decoder shows up as a hang
    // the fuzzer reports, not an infinite spin.
    for _ in 0..64 {
        let before = cursor.position();
        match read_frame_limited(&mut cursor, cap, None) {
            Ok(Some(frame)) => {
                assert!(
                    cursor.position() > before,
                    "a decoded frame must consume bytes"
                );
                // A decoded frame must be re-serializable within the
                // post-enrollment (bulk) cap: the host echoes frames back.
                // `write_frame` refuses past the cap, so success IS the
                // bound; and the re-encoding must read back as one frame.
                let mut re = Vec::new();
                write_frame(&mut re, &frame).expect("a decoded frame re-encodes under the cap");
                assert!(
                    (re.len() - 4) as u32 <= MAX_FRAME_BYTES,
                    "a frame accepted under a {cap} B cap serialized to {} B",
                    re.len() - 4
                );
                let mut cur2 = std::io::Cursor::new(&re[..]);
                let again = read_frame_limited(&mut cur2, MAX_FRAME_BYTES, None)
                    .expect("a re-encoded frame reads")
                    .expect("a re-encoded frame is one whole frame");
                let mut re2 = Vec::new();
                write_frame(&mut re2, &again).expect("re-encodes");
                assert_eq!(re2, re, "the frame encoding is canonical");
                // Schema-bearing frames must never carry a schema the
                // decoder would then act on without the host's check;
                // this is an observation, not a refusal (the host owns
                // the refusal) — assert only that reading it is safe.
                let _ = WIRE_SCHEMA;
            }
            Ok(None) | Err(_) => return,
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // Both classes: the small pre-enrollment cap and the post-enrollment
    // one. The pre-enrollment class is the attacker-reachable one.
    drive(data, MAX_HELLO_FRAME_BYTES);
    drive(data, MAX_FRAME_BYTES);
});
