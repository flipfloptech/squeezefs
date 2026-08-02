//! RES-4 (pre-RC engineering spec §7): the LRU eviction channels' live
//! payload bytes must be inside the R5 memory budget.
//!
//! `LruCache` bounds the eviction channel at
//! [`LruCache::EVICT_CHANNEL_BYTE_BOUND`] (256 MiB) and gauges it in
//! `evict_channel_bytes()`, whose field doc already calls it "the R5
//! gauge" — but no `Component` was ever registered, so the bytes sat
//! OUTSIDE the authority. Two caches take a dehydration receiver
//! (`read_lru` and `hot_block`; `write_lru` never does, so its victims
//! drop at the source), which is up to 512 MiB of live multi-MiB `Bytes`
//! the budget could not see, could not attribute, and could not shed —
//! precisely the class the cage-OOM fix that introduced the bound was
//! about (RSS 2.1 → 7.5 GiB in < 30 s at Green).
//!
//! Contracts pinned here:
//!
//! 1. **Registered**: the registration helper puts BOTH channels in the
//!    registry, and their gauges report the parked bytes.
//! 2. **Sheddable**: the shed closure clamps the send-side admission, so
//!    a Red pulse stops NEW victims parking (counted in
//!    `evict_channel_drops`) while the worker drains what is already
//!    parked — the R5 never-lossy discipline (dehydration is best-effort
//!    warmth).
//! 3. **Green restores the static bound**: a shed pulse is not a
//!    permanent clamp — once the budget is back to Green the channel
//!    admits again, so a single pressure episode cannot cold-start the
//!    disk tier forever.
//!
//! RED against dev 7d1ec2e1: `register_lru_evict_channel_components` and
//! `shed_evict_channel` do not exist, and no `Component` named
//! `*_evict_channel` is ever registered.

use bytes::Bytes;
use squeezefs::cache::lru::LruCache;
use squeezefs::mem_budget::MemBudget;

/// A cache whose eviction receiver has been ARMED (a worker took it):
/// the send side only parks victims for such caches.
fn armed_cache(max_bytes: u64) -> LruCache {
    let c = LruCache::with_capacity(max_bytes);
    let rx = c.take_evict_rx().expect("receiver available exactly once");
    // Keep it alive but never drained — the parked-channel shape.
    std::mem::forget(rx);
    c
}

// ---------------------------------------------------------------------------
// Contract 1 — both channels are registered and gauged.
// ---------------------------------------------------------------------------

#[test]
fn evict_channels_are_registered_r5_components() {
    let mb = MemBudget::new_for_test();
    let read_lru = armed_cache(4 * 1024 * 1024);
    let hot = armed_cache(4 * 1024 * 1024);
    squeezefs::mem_budget::register_lru_evict_channel_components(&mb, &read_lru, &hot);

    let names: Vec<&str> = mb.stats_components().iter().map(|(n, ..)| *n).collect();
    assert!(
        names.contains(&"read_lru_evict_channel"),
        "RES-4: the read_lru eviction channel must be an R5 component \
         (registry: {names:?})"
    );
    assert!(
        names.contains(&"hot_block_evict_channel"),
        "RES-4: the hot-block eviction channel must be an R5 component \
         (registry: {names:?})"
    );

    // Park a victim: evict by overflowing the cache, then the gauge the
    // registry reads must account it.
    let payload = Bytes::from(vec![9u8; 1024 * 1024]);
    for i in 0..8u32 {
        read_lru.put(&format!("k{i}"), payload.clone());
    }
    let parked = read_lru.evict_channel_bytes();
    assert!(
        parked > 0,
        "fixture premise: overflowing an armed cache parks victims in the \
         eviction channel"
    );
    let gauged = mb
        .stats_components()
        .into_iter()
        .find(|(n, ..)| *n == "read_lru_evict_channel")
        .map(|(_, current, ..)| current)
        .expect("component present");
    assert_eq!(
        gauged, parked,
        "RES-4: the registered gauge must report the channel's live bytes"
    );
}

// ---------------------------------------------------------------------------
// Contract 2/3 — the shed clamps admission under pressure and Green
// restores the static bound.
// ---------------------------------------------------------------------------

#[test]
fn evict_channel_shed_clamps_admission_only_under_pressure() {
    let cache = armed_cache(4 * 1024 * 1024);
    let payload = Bytes::from(vec![1u8; 1024 * 1024]);

    // Green: the static bound governs — victims park.
    for i in 0..8u32 {
        cache.put(&format!("g{i}"), payload.clone());
    }
    assert!(
        cache.evict_channel_bytes() > 0,
        "Green admits victims to the channel"
    );

    // A Red shed pulse targeting zero must stop NEW victims parking.
    let before_bytes = cache.evict_channel_bytes();
    let before_drops = cache.evict_channel_drops();
    cache.shed_evict_channel(0);
    squeezefs::mem_budget::MEM_BUDGET.force_level_for_test(squeezefs::mem_budget::Level::Red);
    for i in 0..8u32 {
        cache.put(&format!("r{i}"), payload.clone());
    }
    assert_eq!(
        cache.evict_channel_bytes(),
        before_bytes,
        "RES-4: a shed to 0 must admit no further victims (nothing tears \
         what is already parked — the worker drains it)"
    );
    assert!(
        cache.evict_channel_drops() > before_drops,
        "refused victims are counted, never silent"
    );

    // Green again: the static bound is back, not the pulse's target.
    squeezefs::mem_budget::MEM_BUDGET.force_level_for_test(squeezefs::mem_budget::Level::Green);
    for i in 0..8u32 {
        cache.put(&format!("h{i}"), payload.clone());
    }
    assert!(
        cache.evict_channel_bytes() > before_bytes,
        "RES-4: a shed pulse must not permanently clamp the channel — \
         Green restores EVICT_CHANNEL_BYTE_BOUND"
    );
}
