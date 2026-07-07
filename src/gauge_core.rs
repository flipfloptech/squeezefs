//! Saturating byte-budget gauge core (staging admission accounting).
//!
//! The staged-write budget (`current_staged_write_bytes`) is charged with
//! `fetch_add` and credited with a saturating CAS loop: a credit can race a
//! charge, but the gauge must never wrap below zero — a wrapped gauge reads
//! as "pool full forever", the exact failure mode of the bench small-file
//! hang. Self-contained so `loom-models/` can `#[path]`-include it and
//! model-check the add/sub interleavings. The main build never sets
//! `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// Credit `amount` back, saturating at zero (a stale credit racing a fresh
/// charge must clamp, never wrap).
pub fn sub_saturating(gauge: &AtomicU64, amount: u64) {
    let mut val = gauge.load(Ordering::Relaxed);
    loop {
        let new_val = val.saturating_sub(amount);
        match gauge.compare_exchange_weak(val, new_val, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => val = actual,
        }
    }
}
