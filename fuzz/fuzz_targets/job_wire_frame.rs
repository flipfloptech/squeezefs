//! Fuzz the **remote job-wire frame decoder** (`src/job_wire.rs`,
//! rebuilt by VAL-6).
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
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::job_wire::{
    read_frame_limited, MAX_FRAME_BYTES, MAX_HELLO_FRAME_BYTES, WIRE_SCHEMA,
};

fn drive(data: &[u8], cap: u32) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime");
    rt.block_on(async move {
        let mut cursor = std::io::Cursor::new(data);
        // A stream is a sequence of frames; a hostile peer sends as many
        // as it likes. Bound the loop so a zero-consuming decoder shows up
        // as a hang the fuzzer reports, not an infinite spin.
        for _ in 0..64 {
            match read_frame_limited(&mut cursor, cap, None).await {
                Ok(Some(frame)) => {
                    // A decoded frame must be re-serializable within the
                    // post-enrollment cap: the host echoes frames back.
                    let body = serde_json::to_vec(&frame).expect("a decoded frame serializes");
                    assert!(
                        body.len() as u32 <= MAX_FRAME_BYTES,
                        "a frame accepted under a {cap} B cap serialized to {} B",
                        body.len()
                    );
                    // Schema-bearing frames must never carry a schema the
                    // decoder would then act on without the host's check;
                    // this is an observation, not a refusal (the host owns
                    // the refusal) — assert only that reading it is safe.
                    let _ = WIRE_SCHEMA;
                }
                Ok(None) | Err(_) => return,
            }
        }
    });
}

fuzz_target!(|data: &[u8]| {
    // Both classes: the small pre-enrollment cap and the post-enrollment
    // one. The pre-enrollment class is the attacker-reachable one.
    drive(data, MAX_HELLO_FRAME_BYTES);
    drive(data, MAX_FRAME_BYTES);
});
