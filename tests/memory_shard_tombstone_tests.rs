//! RES-10 (pre-RC engineering spec §7): `MemoryCacheShard::remove` leaves
//! its eviction-queue node behind.
//!
//! **Spec-reading correction (recorded deliberately).** The item predicts
//! that `try_evict`'s `max_loops` "can then exhaust without freeing, so
//! the shard exceeds `max_bytes` and the R5 Red clamp can fail to
//! clamp". That half does NOT reproduce, and the reason is structural:
//! both budgets are `max(eviction_queue.len() × 2, N)` — read from the
//! queue that CONTAINS the tombstones — so the budget scales with the
//! garbage. With `T` tombstones and `L` live entries a pass spends `T`
//! iterations clearing dead nodes, `L` handing second chances, and `L`
//! evicting, i.e. `T + 2L ≤ 2(T + L)` — it always converges. Contracts 1
//! and 2 below pin that (they are regression guards for the budget's
//! self-scaling property, not repros).
//!
//! What IS real is the leak underneath, and it is the RES-3 shape: an
//! invalidation-heavy tier (R-6 unified purge on every overwrite,
//! truncate, `free_block`'s read-tier sweep) leaves one dead `Bytes` key
//! node per removal, FOREVER. Nothing pops it except an eviction pass,
//! and a cache that stays under budget never runs one. Two costs, both
//! unbounded:
//!
//! * **RAM the R5 authority cannot see** — the `read_lru` / `hot_block` /
//!   `write_lru` components gauge PAYLOAD bytes; the ordering queue's
//!   nodes are not payload.
//! * **Put-path latency** — a pass is `O(queue length)`, and the queue
//!   grows with lifetime removals rather than with live entries, so the
//!   next over-budget `put` pays for every invalidation the mount ever
//!   did.
//!
//! Contract 3 pins the bound. RED against dev 7d1ec2e1: 20,000 removals
//! leave 20,000 nodes.

use bytes::Bytes;
use squeezefs::cache::lru::LruCache;

const VAL: usize = 64 * 1024;

/// Build a queue that is overwhelmingly tombstones: insert `dead` keys
/// and remove them all (each leaves its eviction node behind), then
/// insert `live` keys.
fn tombstoned_cache(max_bytes: u64, dead: usize, live: usize) -> LruCache {
    let cache = LruCache::with_capacity(max_bytes);
    let payload = Bytes::from(vec![5u8; VAL]);
    for i in 0..dead {
        let k = format!("dead{i}");
        cache.put(&k, payload.clone());
        cache.remove(&k);
    }
    for i in 0..live {
        cache.put(&format!("live{i}"), payload.clone());
    }
    cache
}

// ---------------------------------------------------------------------------
// Contracts 1/2 — the self-scaling budget (regression guards).
// ---------------------------------------------------------------------------

#[test]
fn eviction_converges_under_a_tombstone_dominated_queue() {
    let max_bytes = (16 * VAL) as u64;
    let cache = tombstoned_cache(max_bytes, 4000, 64);
    assert!(
        cache.current_bytes() <= max_bytes,
        "the shard holds {} B over a {} B budget — the clock's loop budget \
         must stay proportional to the queue it walks",
        cache.current_bytes(),
        max_bytes
    );
}

#[test]
fn r5_red_clamp_empties_the_cache_despite_tombstones() {
    let max_bytes = (16 * VAL) as u64;
    let cache = tombstoned_cache(max_bytes, 4000, 64);
    cache.shed_to(0);
    assert_eq!(
        cache.current_bytes(),
        0,
        "the R5 Red clamp is an ORDER, not a scan — it must empty the cache \
         however many tombstones sit in front of the live keys"
    );
}

// ---------------------------------------------------------------------------
// Contract 3 — the real leak: dead nodes are never reclaimed.
// ---------------------------------------------------------------------------

#[test]
fn eviction_nodes_are_bounded_by_the_live_set_not_by_lifetime_removals() {
    // Comfortably under budget for the churn, so no eviction pass ever
    // runs on its own — the healthy steady state of an
    // invalidation-heavy tier. The shard count (and therefore the
    // per-shard hysteresis floor) is a property of the box, so the
    // contract is stated as a GROWTH bound: doubling the removals must
    // not grow the backlog.
    let cache = LruCache::with_capacity((256 * VAL) as u64);
    let payload = Bytes::from(vec![5u8; VAL]);
    let churn = |from: usize, to: usize| {
        for i in from..to {
            let k = format!("churn{i}");
            cache.put(&k, payload.clone());
            cache.remove(&k);
        }
    };

    churn(0, 20_000);
    assert_eq!(
        cache.current_bytes(),
        0,
        "payload gauge converges — it always did"
    );
    let after_20k = cache.eviction_queue_len();
    churn(20_000, 60_000);
    let after_60k = cache.eviction_queue_len();

    assert!(
        after_60k <= after_20k,
        "RES-10: the eviction backlog grew {after_20k} → {after_60k} across \
         40,000 further removals — one dead key per invalidation, forever, \
         invisible to the R5 payload-byte gauge and paid for by the next \
         over-budget put (a pass is O(queue length))"
    );
    assert!(
        after_60k < 20_000 / 4,
        "RES-10: {after_60k} nodes is still per-removal accumulation, not a \
         live-set bound"
    );
}

/// Reclamation must not cost live entries their place: an entry removed
/// from the queue by mistake would never be evicted, and the shard would
/// exceed its budget permanently.
#[test]
fn tombstone_reclamation_preserves_live_entries_and_eviction_order() {
    let max_bytes = (8 * VAL) as u64;
    let cache = LruCache::with_capacity(max_bytes);
    let payload = Bytes::from(vec![7u8; VAL]);

    // A live entry first, then heavy churn behind it.
    cache.put("pinned", payload.clone());
    for i in 0..8_000usize {
        let k = format!("churn{i}");
        cache.put(&k, payload.clone());
        cache.remove(&k);
    }
    assert!(
        cache.get("pinned").is_some(),
        "reclamation must not drop a live entry"
    );
    assert_eq!(cache.current_bytes(), VAL as u64, "gauge exact");

    // And the budget is still enforceable.
    for i in 0..32usize {
        cache.put(&format!("fill{i}"), payload.clone());
    }
    assert!(
        cache.current_bytes() <= max_bytes,
        "eviction still enforces the budget after reclamation ({} B > {} B)",
        cache.current_bytes(),
        max_bytes
    );
}
