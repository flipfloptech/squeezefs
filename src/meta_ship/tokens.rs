//! The **client token cache** — S8's resolution of the S4 fencing-read
//! contract (`dlm_slot`'s module docs, contract "for S6/S8": *"when a
//! foreign home becomes possible, a fencing read on a foreign-home object
//! must become an owner read (or a leased/cached-token read) — it must NOT
//! keep serving the local view, which would then be a stale-generation
//! answer"*).
//!
//! # Why a cache, and not a round trip
//!
//! The census is ~24 fencing-read sites, several per write / publish /
//! flush. Spec §6.5 item 1 is explicit about what that costs: *"Adding one
//! 250 µs fabric RTT takes the create wall from 110 µs to 360 µs — a 69 %
//! regression … **Requirement: ≥ 99.5 % of lock operations must be served
//! from a locally cached or delegated token.**"* A per-read round trip is
//! therefore not a candidate; every reference client (Lustre's client lock
//! cache, Ceph's issued/wanted caps, NFSv4 delegations) caches on the
//! client and reclaims on revocation.
//!
//! # How it stays sound
//!
//! Entries are **only** written from an owner's answer — the grant that
//! piggybacks on a shipped verb's reply (spec §6.7 decision 3). The cache
//! is therefore never an independent generator, only a repeater, and it is
//! monotone: a `fetch_max`-style update can never move an object's
//! generation backwards, which is the property every consumer comparison
//! (`<`, `==`, `.max()`) needs.
//!
//! # The honest hole, made loud
//!
//! `get_fencing_token_ino` cannot fail — it returns `u64`. So a MISS on a
//! foreign object has no error channel, and both available answers are
//! wrong in a different direction: too low adopts a superseded record
//! (§6.11's inversion), too high fences live work. The resolution:
//!
//! * a miss returns the **owner era's base** (`term << 40`), which is
//!   exactly what a fresh local mount already returns for an object with
//!   no grant — the same honest degradation the shipped code has, not a
//!   new one — so pre-era stamps still classify stale and current-era
//!   stamps still classify live;
//! * a miss is a **must-stay-0 tripwire**: counted (`dlm_token_cache_misses`)
//!   and reported through `note_invariant_tripwire` (RES-22's
//!   loud-never-fatal law), because a miss means the intent-lock property
//!   was violated — some site needed a token without having performed a
//!   metadata RPC on the object first.
//!
//! The invariant that makes misses impossible in the operations that
//! matter is spec §6.7 decision 3's: *"There is no operation that needs a
//! token but performs no metadata RPC on the object first."* Where a
//! client's entry has aged out or the era moved,
//! [`super::MetaShipRouter::refresh_token`] re-earns it with a shipped
//! `getattr` — a metadata RPC, never a second lock protocol.
//!
//! # Bounded, because the population is unbounded
//!
//! `FENCING_MAP`'s unbounded growth was RES-2, closed by S1; this cache
//! must not reintroduce it. The cap derives from the R5 budget (the
//! standing "caps derive from system resources" law), and retirement is a
//! **second-chance clock**: one bounded sweep clears the reference bit of
//! recently-used entries and retires the rest. Retiring an entry costs at
//! most one loud miss and one refresh, never a wrong answer.

use super::wire::TokenGrant;
use crate::dlm::{compose_token, GRANT_SEQ_BITS};
use crate::fuse_client::LatencyHistogram;
use crate::token_cache_core::{record_grant_ordered, EraFloor, TokenSlot};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Absolute override for the derived entry cap.
pub const TOKEN_CACHE_MAX_ENV: &str = "SQUEEZEFS_DLM_TOKEN_CACHE_MAX";

/// Bytes charged per entry: the `scc` bucket slot for a `u64` key plus the
/// two words of value. Used for the `dlm_token_cache_bytes` gauge, which
/// is what an R5 registration would consume when S9 makes the cache
/// load-bearing (it is a control-plane cache today: bounded, sheddable,
/// and every eviction costs one refresh).
const ENTRY_BYTES: u64 = 32;

/// Floor for the derived cap: a client with a small budget still caches a
/// real working set of open objects rather than thrashing into misses.
const ENTRY_FLOOR: usize = 4096;

/// Share of the R5 budget the cache may occupy: 1/8192. At a 16 GiB
/// budget that is 2 MiB ≈ 65 k objects — the population of objects one
/// client has live metadata interest in, by construction bounded by its
/// open files and its dirty set, never by objects ever touched.
const BUDGET_DIVISOR: u64 = 8192;

// The per-entry word protocol (monotone token/term merge, the
// second-chance reference bit) lives in `crate::token_cache_core` —
// spec §6.9's `token_cache_core` loom obligation, model-checked there.
static CACHE: Lazy<scc::HashMap<u64, TokenSlot>> = Lazy::new(scc::HashMap::new);

/// The owner era the cache last learned — a miss's floor, and the reason a
/// miss degrades exactly as a fresh mount does rather than answering 0 on
/// a volume whose records name eras.
static OWNER_TERM: EraFloor = EraFloor::new();

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static EVICTIONS: AtomicU64 = AtomicU64::new(0);
static GRANTS: AtomicU64 = AtomicU64::new(0);

/// The read-token RECORDS budget, bytes (PR 5, review round 1 Issue 7):
/// a token entry carries the object's records — attrs, every carried
/// xattr, a directory's whole dentry set — so the reader's cache is bound
/// by BYTES on its own R5 component, never by the delegation cache's
/// 32-byte entry count. Derived as 1/256 of the R5 budget (64 MiB at
/// 16 GiB ≈ 1.6 M dentries or ≈ 1,000 full-size `layout` values — a
/// reader's live interest set: the directories it lists and the files it
/// has open, the same order as the node cache's share of the same budget
/// divided by the four kinds it holds per object); the R5 component is
/// floor 0, weight 1 (a re-earnable cache — a shed costs one grant round
/// trip per object, never a wrong answer). The budget's own floor is ONE
/// control frame (`CONTROL_MAX_FRAME_BYTES`): the physical minimum under
/// which a grant page can be resident at all — below it no token could
/// ever be served. Tie-tested in `derivation_sweep_tests`.
pub fn records_budget_bytes() -> u64 {
    (crate::mem_budget::MEM_BUDGET.budget_bytes() / RECORDS_BUDGET_DIVISOR)
        .max(u64::from(crate::cluster_wire::CONTROL_MAX_FRAME_BYTES))
}

/// The token records' share of the R5 budget (see
/// [`records_budget_bytes`]).
pub const RECORDS_BUDGET_DIVISOR: u64 = 256;

/// The cache's entry cap: derived from the R5 budget, absolute override
/// wins verbatim (the standing precedence law).
pub fn cache_cap() -> usize {
    if let Some(explicit) = crate::env_knobs::opt_int_knob::<usize>(TOKEN_CACHE_MAX_ENV) {
        return explicit.max(1);
    }
    let budget = crate::mem_budget::MEM_BUDGET.budget_bytes();
    let derived = (budget / BUDGET_DIVISOR / ENTRY_BYTES) as usize;
    derived.max(ENTRY_FLOOR)
}

/// Record the owner's era (learned from every reply frame). Monotone.
pub fn record_owner_term(term: u64) {
    OWNER_TERM.record(term);
}

/// The era the cache serves a miss in.
pub fn owner_term() -> u64 {
    OWNER_TERM.get()
}

/// Record a grant an owner sent us. Monotone per object: a reordered or
/// replayed reply can never lower an object's generation.
///
/// The era floor is recorded BEFORE the grant becomes findable
/// (`record_grant_ordered` — the core's order, weakening-verified in
/// its loom model): an entry the sweep later retires must never expose a
/// miss whose floor predates the grant's own era.
pub fn record_grant(grant: &TokenGrant) {
    record_grant_ordered(&OWNER_TERM, grant.term, || {
        GRANTS.fetch_add(1, Ordering::Relaxed);
        let updated = CACHE.read_sync(&grant.ino, |_, e| e.merge(grant.token, grant.term));
        if updated.is_some() {
            return;
        }
        if CACHE.len() >= cache_cap() {
            sweep();
        }
        let _ = CACHE.insert_sync(grant.ino, TokenSlot::granted(grant.token, grant.term));
    });
}

/// One bounded second-chance pass: entries used since the last sweep are
/// kept (and their bit cleared), the rest retire.
fn sweep() {
    let mut retired = 0u64;
    CACHE.retain_sync(|_, e| {
        if e.keep_for_another_pass() {
            true
        } else {
            retired += 1;
            false
        }
    });
    if retired > 0 {
        EVICTIONS.fetch_add(retired, Ordering::Relaxed);
    } else {
        // Every entry was hot: clear the whole generation rather than
        // grow without bound. Each retirement costs one loud miss and one
        // refresh — never a wrong answer.
        let mut cleared = 0u64;
        CACHE.retain_sync(|_, _| {
            cleared += 1;
            false
        });
        EVICTIONS.fetch_add(cleared, Ordering::Relaxed);
    }
}

/// The fencing generation of a **foreign-home** object, served from the
/// owner's own last answer.
///
/// A miss returns the owner era's base and trips the must-stay-0
/// tripwire — see the module docs for why that is the only sound
/// direction and why it is loud.
pub fn foreign_fencing_token(ino: u64) -> u64 {
    if let Some(token) = CACHE.read_sync(&ino, |_, e| e.serve()) {
        HITS.fetch_add(1, Ordering::Relaxed);
        return token;
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    let floor = compose_token(owner_term(), 0);
    crate::note_invariant_tripwire(
        "meta_ship::foreign_fencing_token",
        &format!(
            "no cached grant for foreign-home ino {ino}: a fencing read reached this site \
             without a metadata RPC on the object first (spec §6.7 decision 3's intent-lock \
             property) — serving the owner era's base {floor} (term {}, grant 0), which \
             classifies pre-era stamps stale and current-era stamps live, and never invents \
             a local grant",
            owner_term()
        ),
    );
    floor
}

/// Snapshot of the cache's counters (`dlm_token_cache_*` — spec §6.9's
/// named family).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenCacheStats {
    pub hits: u64,
    /// **Must stay 0**: a miss means the intent-lock property was violated.
    pub misses: u64,
    pub evictions: u64,
    pub grants: u64,
    pub entries: u64,
    /// Whole-object entries PLUS the S11 range-span extension (§9.2:
    /// `dlm_token_cache_bytes` is "extended to count range-span state").
    pub bytes: u64,
    /// Live cached range spans (the S11 rung-15 extension's gauge).
    pub range_spans: u64,
    pub cap_entries: u64,
    pub owner_term: u64,
}

/// Read the cache's counters.
pub fn token_cache_stats() -> TokenCacheStats {
    let entries = CACHE.len() as u64;
    let range_spans = RANGE_SPANS.load(Ordering::Relaxed);
    TokenCacheStats {
        hits: HITS.load(Ordering::Relaxed),
        misses: MISSES.load(Ordering::Relaxed),
        evictions: EVICTIONS.load(Ordering::Relaxed),
        grants: GRANTS.load(Ordering::Relaxed),
        entries,
        bytes: entries * ENTRY_BYTES
            + range_spans * RANGE_SPAN_BYTES
            + STRETCH_CEILINGS.len() as u64 * STRETCH_CEILING_BYTES,
        range_spans,
        cap_entries: cache_cap() as u64,
        owner_term: owner_term(),
    }
}

/// **Test seam**: drop every cached grant, so a suite can exercise the
/// COLD (miss) arm deliberately. Production never calls it — an
/// invalidation on a real revocation is S9's revoke verb, which will
/// retire the named objects, not the whole cache.
pub fn test_clear_token_cache() {
    CACHE.retain_sync(|_, _| false);
}

/// The composed-token width, re-exported for the callers that need to
/// reason about a miss's floor without importing the DLM.
pub const TOKEN_GRANT_BITS: u32 = GRANT_SEQ_BITS;

// ===========================================================================
// DLM S11 rung 15 — the CLIENT RANGE CACHE (KD-MW-7,
// docs/design-full-multi-writer.md §9.2: range grants are "cached
// client-side in the S8 token cache keyed (ino, [start,end), mode,
// token)").
//
// Unlike the whole-object token cache above (a bounded cache of owner
// ANSWERS, retired by a clock sweep), the range cache mirrors LIVE
// CUSTODY: entries enter on grant/extension, widen with the admit-time
// merge, leave on release/lease-loss, and are REBUILT verbatim from the
// renewal reply's range vector (the revalidation surface) — so its
// population is bounded by this client's live grants, which the
// authority's §9.2 caps already bound. Shedding it is always safe: the
// next probe misses and the write path re-acquires (never a wrong
// answer), which is why its R5 registration is floor-0/weight-1.
// ===========================================================================

/// Bytes charged per cached range span (the `dlm_grant_table_bytes`
/// estimate's client-side twin — §9.2's ~48 B span arithmetic).
const RANGE_SPAN_BYTES: u64 = 48;

/// One cached range grant: the span and the token the grant carries
/// (mode is EX by KD-MW-9's v1 issuance law — a mode field would be a
/// constant).
#[derive(Debug)]
struct RangeSpan {
    start: u64,
    end: u64,
    token: u64,
    /// §9.3a: the max byte end this client has been SERVED write custody
    /// for through this grant — the written high-water the tail-shrink
    /// ack answers. `fetch_max`ed at every covering serve and acquire
    /// outcome (before the write proceeds), lock-free per the hot-path
    /// law, so a shrink that reads it AFTER narrowing the span (under the
    /// same entry serialization) can never miss a served write.
    written: AtomicU64,
}

/// Per-ino live range grants, small and sorted-by-start (the population
/// is this client's live grants on one file — the §9.2 cap's bound).
static RANGE_CACHE: Lazy<scc::HashMap<u64, Vec<RangeSpan>>> = Lazy::new(scc::HashMap::new);

/// Cached range spans across all inos (the `dlm_token_cache_bytes` range
/// extension's gauge word).
static RANGE_SPANS: AtomicU64 = AtomicU64::new(0);

/// §9.3a: one learned **stretch ceiling** — what a tail-shrink notice
/// taught this client about the ino's contention neighborhood.
#[derive(Debug, Clone, Copy)]
struct StretchCeiling {
    /// The shrink's floor (an absolute boundary a peer claimed past) —
    /// a later grant covering at-or-beyond it proves the peer released
    /// and clears the ceiling.
    floor: u64,
    /// The stretch LENGTH that survived beyond this client's written
    /// frontier at notice time (`floor − watermark`, saturating). The
    /// clamp law caps future sequential doublings at this length beyond
    /// the ask's own aligned end — 0 on an exact-boundary shrink, i.e.
    /// "stop doubling on this ino": the honest posture for a
    /// block-cyclic interleave, whose EVERY stretch crosses a peer run.
    stretch_len: u64,
}

/// §9.3a: per-ino learned stretch ceilings. Bounded by the live-granted
/// ino population: pruned at every renewal rebuild (an ino with no live
/// grant left has ended its custody episode), cleared by a covering
/// grant past the floor, and dropped whole with the cache (lease loss /
/// the R5 shed).
static STRETCH_CEILINGS: Lazy<scc::HashMap<u64, StretchCeiling>> = Lazy::new(scc::HashMap::new);

/// Bytes charged per learned ceiling in the `dlm_token_cache_bytes`
/// gauge (key + two words, scc slot amortized — the RANGE_SPAN_BYTES
/// estimate's shape).
const STRETCH_CEILING_BYTES: u64 = 24;

/// Finding 24 (`.benchmarks/2026-08-25-s11-freeloop-stall.md`): per-ino
/// RANGE-EPISODE latch — MONOTONE for the mount's life. The f23 blob-
/// lifecycle gate keyed on [`range_span_hull`], which samples the LIVE
/// grants: the trim/doubling churn retires an ino's whole grant set for
/// an instant, and a save landing in that gap claimed the refetched
/// owner-composed blob again (attempt 5's 4 residual duplicate frees —
/// one of which freed a mid-write reallocated block and ENOSPC'd the
/// fleet). "This ino has been range-shared on this mount" is a monotone
/// fact, so the latch never clears in production (deliberately not part
/// of [`clear_range_grants`] — a custody-era end does not un-share the
/// blobs a stale cached head may still name); the cost of a stale latch
/// is one leak-safe skipped blob free per save, counted on
/// `publish_blob_foreign_free_skips`. ~8 B per ever-granted ino.
static RANGE_EPISODES: Lazy<scc::HashSet<u64>> = Lazy::new(scc::HashSet::new);

/// `true` ⇔ `ino` has held a byte-range grant at ANY point in this
/// mount's life — the f24 sticky discriminator the blob-lifecycle gate
/// keys on (a live-grant probe is [`range_span_hull`]).
pub fn range_episode(ino: u64) -> bool {
    RANGE_EPISODES.contains_sync(&ino)
}

/// Record (or WIDEN — same token, wider span: the admit-time merge's
/// client face) a granted range.
pub fn record_range_grant(ino: u64, span: (u64, u64), token: u64) {
    ensure_token_cache_r5();
    // Finding 24: the monotone episode latch — set here, cleared only by
    // the test-hygiene reset.
    let _ = RANGE_EPISODES.insert_sync(ino);
    let mut added = 0i64;
    let mut update = |spans: &mut Vec<RangeSpan>| {
        if let Some(existing) = spans.iter_mut().find(|s| s.token == token) {
            existing.start = existing.start.min(span.0);
            existing.end = existing.end.max(span.1);
        } else {
            spans.push(RangeSpan {
                start: span.0,
                end: span.1,
                token,
                written: AtomicU64::new(0),
            });
            spans.sort_unstable_by_key(|s| s.start);
            added = 1;
        }
    };
    match RANGE_CACHE.entry_sync(ino) {
        scc::hash_map::Entry::Occupied(mut occ) => update(occ.get_mut()),
        scc::hash_map::Entry::Vacant(vac) => {
            let mut spans = Vec::with_capacity(1);
            update(&mut spans);
            let _ = vac.insert_entry(spans);
        }
    }
    if added > 0 {
        RANGE_SPANS.fetch_add(added as u64, Ordering::Relaxed);
    }
    // §9.3a: a grant covering ACROSS a learned ceiling's floor proves the
    // peer released the boundary itself — the ceiling clears (the world
    // changed; the next shrink, if any, re-teaches). A disjoint grant
    // merely BEYOND the floor proves nothing: the block-cyclic shape
    // grants a new stripe past the floor every stride while the peer
    // still holds the boundary.
    let _ = STRETCH_CEILINGS.remove_if_sync(&ino, |c| span.0 <= c.floor && span.1 > c.floor);
}

/// The covering probe: the token of a live cached grant covering
/// `[start, end)` whole, or `None` (the caller acquires). This is the
/// §6.5-item-1 mechanism for range writers — subsequent writes inside a
/// granted stripe pay one lock-free read, no round trip.
///
/// The probe IS the write path's custody serve (both call sites are the
/// ranged write lease), so a hit also `fetch_max`es the grant's written
/// high-water to `end` (§9.3a): the mark lands BEFORE the token is
/// answered, so a tail-shrink that later narrows this span under the same
/// entry serialization always observes every serve that beat it. A future
/// read-side consumer of covering probes must split off a mark-free
/// variant rather than reuse this one.
pub fn range_token_covering(ino: u64, start: u64, end: u64) -> Option<u64> {
    if start >= end {
        return None;
    }
    RANGE_CACHE
        .read_sync(&ino, |_, spans| {
            if let Some(s) = spans.iter().find(|s| s.start <= start && s.end >= end) {
                s.written.fetch_max(end, Ordering::Relaxed);
                return Some(s.token);
            }
            // Finding 22: a straddling window covered by the UNION of
            // this holder's own cached spans is served too — the trim
            // teacher's clamp makes abutting own pairs the steady state,
            // and a probe miss here was one wire ask per straddling
            // write (the 285 k-refusal storm's engine). The mark is
            // SEGMENT-WISE: each overlapped grant's written high-water
            // rises to ITS OWN segment end — a straddle marked on one
            // grant only would let the other's tail shrink release
            // bytes the holder wrote (§9.3a's zeros class). Marks land
            // before the token answers, the single-span law verbatim.
            let mut sorted: Vec<&RangeSpan> = spans
                .iter()
                .filter(|s| s.end > start && s.start < end)
                .collect();
            sorted.sort_unstable_by_key(|s| s.start);
            let mut cursor = start;
            let mut first: Option<u64> = None;
            for s in &sorted {
                if s.start > cursor {
                    return None; // a gap inside the window
                }
                if first.is_none() {
                    first = Some(s.token);
                }
                cursor = cursor.max(s.end);
                if cursor >= end {
                    for s in &sorted {
                        s.written.fetch_max(end.min(s.end), Ordering::Relaxed);
                    }
                    return first;
                }
            }
            None
        })
        .flatten()
}

/// §9.3a: record that this client was served write custody up to `upto`
/// through the grant carrying `token` — the acquire-outcome face of the
/// covering probe's mark (a write whose custody arrived as a fresh
/// New/Extended/Covered wire answer never passed the probe).
pub fn note_range_write(ino: u64, token: u64, upto: u64) {
    let _ = RANGE_CACHE.read_sync(&ino, |_, spans| {
        if let Some(s) = spans.iter().find(|s| s.token == token) {
            s.written.fetch_max(upto, Ordering::Relaxed);
        }
    });
}

/// §9.3a: the client half of a tail shrink — narrow the cached span
/// carrying `token` to end at `floor` (the covering cache stops serving
/// the released tail), then answer the grant's written high-water, read
/// AFTER the narrow under the same entry serialization (any serve that
/// beat the shrink is visible in the mark, so the ack can never
/// under-report a served write). `None` ⇔ no cached entry (an R5 shed
/// dropped it): the honest answer is "unknown", and the caller acks
/// `u64::MAX` so the authority escalates instead of releasing.
pub fn shrink_range_grant(ino: u64, token: u64, floor: u64) -> Option<u64> {
    RANGE_CACHE
        .update_sync(&ino, |_, spans| {
            spans.iter_mut().find(|s| s.token == token).map(|s| {
                if floor > s.start && floor < s.end {
                    s.end = floor;
                }
                s.written.load(Ordering::Relaxed)
            })
        })
        .flatten()
}

/// §9.3a: learn a stretch ceiling from a shrink notice — the surviving
/// stretch length beyond this client's written frontier
/// (`floor − watermark`, saturating; an unknown watermark learns 0, the
/// stop-doubling posture). The clamp consumer is
/// [`stretch_ceiling`]; the lifecycle is the range cache's (pruned at
/// renewal rebuilds, cleared by a covering grant past the floor).
pub fn note_stretch_ceiling(ino: u64, floor: u64, watermark: u64) {
    let ceiling = StretchCeiling {
        floor,
        stretch_len: floor.saturating_sub(watermark),
    };
    match STRETCH_CEILINGS.entry_sync(ino) {
        scc::hash_map::Entry::Occupied(mut occ) => {
            let c = occ.get_mut();
            // Repeat lessons converge on the tighter posture.
            c.floor = c.floor.max(ceiling.floor);
            c.stretch_len = c.stretch_len.min(ceiling.stretch_len);
        }
        scc::hash_map::Entry::Vacant(vac) => {
            let _ = vac.insert_entry(ceiling);
        }
    }
}

/// §9.3a: the learned stretch-length cap for `ino`, or `None` (no shrink
/// has taught this ino anything — stretch freely).
pub fn stretch_ceiling(ino: u64) -> Option<u64> {
    STRETCH_CEILINGS.read_sync(&ino, |_, c| c.stretch_len)
}

/// Test seam: drop every learned stretch ceiling. The map is keyed by
/// ino and inos RECUR across a test binary's fresh volumes, so a suite
/// that teaches a ceiling must clear it or the next fixture inherits a
/// stop-doubling posture it never earned.
pub fn test_clear_stretch_ceilings() {
    STRETCH_CEILINGS.clear_sync();
}

/// Is `token` a STILL-LIVE cached range grant of `ino` (rung 18)? The
/// §9.2 fencing law's probe — "a range writer fences on its own lease
/// token": a writeback unit whose grant is alive is CURRENT custody even
/// when a sibling stripe's newer grant moved the ino's max generation
/// (the block-cyclic acquire storm otherwise livelocks the convergence
/// ladder — every in-flight unit reads stale forever while new stripes
/// keep minting).
pub fn range_token_live(ino: u64, token: u64) -> bool {
    RANGE_CACHE
        .read_sync(&ino, |_, spans| spans.iter().any(|s| s.token == token))
        .unwrap_or(false)
}

/// The HULL of this client's cached spans on `ino` — `(min start, max
/// end)` over live grants, or `None`. The write path's desired-window
/// seed: unioning the hull into desired is what makes the next ask
/// EXTEND the held grant instead of minting a stripe-mate (the §9.2
/// convergence).
pub fn range_span_hull(ino: u64) -> Option<(u64, u64)> {
    RANGE_CACHE
        .read_sync(&ino, |_, spans| {
            spans
                .iter()
                .map(|s| (s.start, s.end))
                .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
        })
        .flatten()
}

/// The hull of this mount's cached spans that OVERLAP or ABUT
/// `[start, end)` — the v2 desired-stretch's union source (rung 18): a
/// genuine stream extension touches the ask's own window, while a
/// DISJOINT stripe (the strided/block-cyclic shape) is excluded — the v1
/// whole-hull union bridged the gaps between a strided writer's stripes
/// and fabricated cross-mount conflicts on custody the holder never
/// writes (the `strided_asks_never_bridge_the_gap` pin).
pub fn range_span_abutting(ino: u64, start: u64, end: u64) -> Option<(u64, u64)> {
    RANGE_CACHE
        .read_sync(&ino, |_, spans| {
            spans
                .iter()
                .filter(|s| s.end >= start && s.start <= end)
                .map(|s| (s.start, s.end))
                .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
        })
        .flatten()
}

/// Retire one grant from the cache (release / grant-level revocation).
/// An ino whose LAST span leaves has ended its custody episode — its
/// learned stretch ceiling (§9.3a) retires with it.
pub fn retire_range_grant(ino: u64, token: u64) {
    let mut removed = 0u64;
    let mut episode_over = false;
    let _ = RANGE_CACHE.remove_if_sync(&ino, |spans| {
        let before = spans.len();
        spans.retain(|s| s.token != token);
        removed = (before - spans.len()) as u64;
        episode_over = spans.is_empty();
        episode_over
    });
    if removed > 0 {
        RANGE_SPANS.fetch_sub(removed, Ordering::Relaxed);
    }
    if episode_over {
        let _ = STRETCH_CEILINGS.remove_sync(&ino);
    }
}

/// Drop EVERY cached range grant — the lease-loss path (a client whose
/// custody lease died holds no range custody at all: a dead grant serving
/// a covering probe would be the one wrong answer available here). The
/// learned stretch ceilings (§9.3a) drop with it — a fresh epoch
/// re-learns from its own shrinks.
pub fn clear_range_grants() {
    clear_range_spans();
    STRETCH_CEILINGS.retain_sync(|_, _| false);
}

/// The span half of [`clear_range_grants`] — the renewal rebuild's clear,
/// which must NOT drop the learned ceilings (§9.3a: a ceiling survives
/// while its ino's custody episode is live; the rebuild prunes the dead
/// ones itself).
fn clear_range_spans() {
    let mut removed = 0u64;
    RANGE_CACHE.retain_sync(|_, spans| {
        removed += spans.len() as u64;
        false
    });
    if removed > 0 {
        RANGE_SPANS.fetch_sub(removed, Ordering::Relaxed);
    }
}

/// **The renewal revalidation** (design §11: "custody lease carries
/// optional range vector"): REPLACE the whole cache with the authority's
/// answer — the vector is the authority's own record of this client's
/// live range grants, so it is authoritative for presence AND absence
/// (a released/revoked grant leaves the cache at the next renewal even
/// if the local retire was lost).
///
/// §9.3a: the written high-waters survive the rebuild — they are CLIENT
/// knowledge the authority's vector cannot carry, and losing them across
/// a renewal would let a tail-shrink ack under-report served writes (the
/// order-is-load-bearing law's other face). The learned stretch ceilings
/// are pruned to the inos the vector still names: an ino with no live
/// grant has ended its custody episode.
pub fn replace_range_grants(entries: &[(u64, (u64, u64), u64)]) {
    let mut marks: Vec<(u64, u64, u64)> = Vec::new();
    RANGE_CACHE.iter_sync(|ino, spans| {
        for s in spans {
            let w = s.written.load(Ordering::Relaxed);
            if w > 0 {
                marks.push((*ino, s.token, w));
            }
        }
        true
    });
    clear_range_spans();
    for (ino, span, token) in entries {
        record_range_grant(*ino, *span, *token);
    }
    for (ino, token, w) in marks {
        // No-op where the vector no longer names the grant (the retired
        // custody carries no obligations).
        note_range_write(ino, token, w);
    }
    STRETCH_CEILINGS.retain_sync(|ino, _| RANGE_CACHE.read_sync(ino, |_, _| ()).is_some());
}

/// **Test seam**: drop the range cache, so a suite can exercise the cold
/// arm and the renewal rebuild deliberately.
pub fn test_clear_range_cache() {
    clear_range_grants();
    RANGE_EPISODES.retain_sync(|_| false);
    // Finding 35: the owner-side sticky episode latch clears with the
    // client's (one seam — suites share one process and re-mint low inos).
    crate::dlm::test_clear_range_episodes();
}

/// Register `dlm_token_cache_bytes` with the R5 authority (§9.2: the
/// client-side member of the spec-named R5 pair). Floor 0, weight 1: both
/// halves are re-earnable caches — the token half re-learns from the next
/// owner reply, the range half from the next renewal's range vector — so
/// a shed costs round trips, never a wrong answer.
pub fn ensure_token_cache_r5() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::mem_budget::MEM_BUDGET.register(crate::mem_budget::Component::new(
            "dlm_token_cache_bytes",
            0,
            1,
            std::sync::Arc::new(|| {
                CACHE.len() as u64 * ENTRY_BYTES
                    + RANGE_SPANS.load(Ordering::Relaxed) * RANGE_SPAN_BYTES
                    + STRETCH_CEILINGS.len() as u64 * STRETCH_CEILING_BYTES
            }),
            std::sync::Arc::new(|_| {
                CACHE.retain_sync(|_, _| false);
                clear_range_grants();
            }),
        ));
    });
}

// ===========================================================================
// DLM S10 rung 11 — the RECALL LANE + THRASH VALVE ("brake before engine").
//
// `docs/design-full-multi-writer.md` §8 (the recall law) + PR row 11; spec
// risk **R5** ("revoke storms on hot shared objects … verify the demotion
// valve engages before the fan-out hurts") and **R3** (deadlines derive
// from LIVE p99 evidence, never a constant); Ceph `mds_recall` lineage
// (spec §6.6). This machinery deliberately lands BEFORE any delegation
// grant exists, so delegation (rows 12–14) can never ship without its
// brake: today the grant population arrives only through the test seams,
// production mounts never touch the lane, and every stat is 0 BY
// CONSTRUCTION on every shipped mount.
//
// # The API contract rows 12–14 consume
//
// * Grant issuance (row 12's DelegGrant) calls [`RecallLane::try_grant`]
//   FIRST — the valve's gate. `Demoted` means the object is owner-served
//   for the remaining cooldown: do not issue, do not invent a second
//   arbitration. Roster admission (only an ADMITTED member may hold a
//   delegation — design §Security) belongs to the CALLER: the lane
//   bookkeeps whatever population the grant path admitted.
// * A conflicting mutation calls [`RecallLane::recall_object`]; the wire
//   half (row 12's DelegRecall verb) drains [`RecallLane::issue_pass`]
//   into frames (one frame per client — the batching law), correlates
//   acks by `(client, frame_id)` into [`RecallLane::ack_frame`], and runs
//   [`RecallLane::expire_overdue`] on its cadence sweep.
// * A recall TIMEOUT is terminal: the grant is DEAD and the object
//   grantable again. Sound because the deadline's CEILING is the
//   membership lease TTL (S6) and a member's own `T_self` fires strictly
//   before the owner's TTL (`T_self = T_owner − 2·skew_max − D_purge`),
//   so a live-but-partitioned holder has self-fenced — and its in-flight
//   DMA is bounded by S7's dead-epoch quarantine — before the owner acts
//   on the timeout. Rows 12+ escalate a timed-out client to membership
//   eviction (the `transport_lease_overlong` precedent: loud, never a
//   silent wait — the lane logs every expiry).
// ===========================================================================

/// Absolute override for the derived recall batch cap (measurement lever).
pub const RECALL_BATCH_MAX_ENV: &str = "SQUEEZEFS_DLM_RECALL_BATCH_MAX";
/// Absolute override for the derived recall deadline (measurement lever).
pub const RECALL_DEADLINE_ENV: &str = "SQUEEZEFS_DLM_RECALL_DEADLINE_MS";
/// Absolute override for the derived demotion cooldown (measurement lever).
pub const RECALL_COOLDOWN_ENV: &str = "SQUEEZEFS_DLM_RECALL_COOLDOWN_MS";

/// Wire bytes one recall entry budgets: an ino (u64, bincode varint ≤ 9 B)
/// plus per-entry framing — measured ≤ 20 B encoded; 32 is the
/// power-of-two ceiling so the batch arithmetic stays exact when row 12's
/// verb grows a generation stamp.
const RECALL_ENTRY_WIRE_BYTES: u32 = 32;

/// Half the CONTROL frame is entry budget; the other half is header, MAC,
/// ids and growth headroom (the same conservative split the S8 encode
/// check enforces at the frame level).
const RECALL_FRAME_HEADROOM_DIV: u32 = 2;

/// Consecutive grant→recall cycles inside the thrash window before the
/// valve demotes: **3** — the smallest run that separates a PATTERN from
/// a coincidence (one cycle is any legitimate writer conflict, two can be
/// one conflict's retry; the fsck verify-before-report settle posture:
/// single evidence is never a verdict).
pub const RECALL_THRASH_CYCLES: u32 = 3;

/// Demotion cooldown in thrash windows: **8** — bounds the worst-case
/// residual thrash duty cycle at `cycles/(cycles+8)` ≈ 27 % of the
/// un-valved volume for a permanently hot object, while a genuinely
/// cooled object re-promotes within one decade of the detection horizon
/// (the AIMD-retreat class: a failed re-promotion probe costs a full
/// threshold run of recalls, so probes are spaced an order of magnitude
/// apart).
const RECALL_COOLDOWN_WINDOWS: u32 = 8;

/// Deadline headroom over live p99: **4×** = two binary octaves — the
/// histogram's power-of-two buckets make a p99 read up to 2× coarse by
/// construction, and one more octave covers the drain the p99 predates
/// (spec R3's unbounded-writeback caveat). A deadline AT p99 would time
/// out ~1 % of healthy recalls, and every false timeout kills a live
/// grant.
const RECALL_DEADLINE_MARGIN: u64 = 4;

/// Deadline floor: the 1 ms timer grain (kernel/tokio scheduling quantum)
/// — below it a deadline is unmeasurable, not strict.
const RECALL_DEADLINE_FLOOR: Duration = Duration::from_millis(1);

/// The recall-channel PARK floor: how short an empty holder round may be
/// (the owner's park derivation clamps at this — `MetaShipService::
/// deleg_park`). A constant scheduling grain, not a tuning value: below
/// it the standing poll degenerates into a busy loop.
pub const RECALL_POLL_PARK_FLOOR: Duration = Duration::from_millis(100);

/// The **delivery term** the rung-12 wire added to the deadline
/// derivation (live finding #3): rung 11 priced the deadline as
/// `4 × (rtt_p99 + owner_p99)` — the drain-and-ack terms — but its own
/// residual #1 left the WIRE half unbuilt, and the wire that landed is a
/// standing poll: a recall issued while the holder's round is
/// mid-turnaround waits up to one park floor for the next round, and its
/// ack rides the round after. On the first fleet run the un-termed
/// derivation read **1 ms** (4× loopback p99, clamped to the floor) and
/// a HEALTHY holder was timed out and membership-EVICTED before its poll
/// could possibly answer. Two park floors is the structural bound of
/// that turnaround (deliver + ack rounds), independent of load evidence
/// — the p99 terms keep pricing the load-dependent halves.
const RECALL_DELIVERY_TERM: Duration =
    Duration::from_millis(2 * RECALL_POLL_PARK_FLOOR.as_millis() as u64);

/// The recall batch cap: entries per frame, derived from the wire's own
/// CONTROL-class bound (explicit lever wins verbatim — the precedence
/// law). At the shipped 1 MiB cap: 16,384 recalls/frame, so spec R6's
/// "one frame per client carrying its whole token set" fits the 1 k-token
/// shape 16× over.
pub fn recall_batch_max_from(explicit: Option<usize>, frame_cap_bytes: u32) -> usize {
    if let Some(e) = explicit {
        return e.max(1);
    }
    ((frame_cap_bytes / RECALL_FRAME_HEADROOM_DIV / RECALL_ENTRY_WIRE_BYTES) as usize).max(1)
}

/// The recall deadline (spec R3's law — never a constant):
///
/// * explicit lever wins verbatim;
/// * live evidence ⇒ `4 × (rtt_p99 + owner_total_p99)` **plus the wire's
///   structural delivery term** (rung 12: recalls travel on the holder's
///   standing poll — up to two park floors of turnaround exist even at
///   zero load; see `RECALL_DELIVERY_TERM`), floored at the 1 ms timer
///   grain, ceilinged at the membership lease TTL (past the TTL the
///   S6/S7 fence arithmetic bounds the client anyway — waiting longer
///   buys nothing);
/// * zero samples ⇒ the TTL itself, the only derivable bound with no
///   evidence (conservative toward the fence bound; on an armed mount
///   the grant's own metadata RPC has already fed the histograms).
pub fn recall_deadline_from(
    explicit_ms: Option<u64>,
    live_p99_us: Option<u64>,
    lease_ttl: Duration,
) -> Duration {
    if let Some(ms) = explicit_ms {
        return Duration::from_millis(ms.max(1));
    }
    let ceiling = lease_ttl.max(RECALL_DEADLINE_FLOOR);
    match live_p99_us {
        None => ceiling,
        Some(us) => Duration::from_micros(us.saturating_mul(RECALL_DEADLINE_MARGIN))
            .saturating_add(RECALL_DELIVERY_TERM)
            .clamp(RECALL_DEADLINE_FLOOR, ceiling),
    }
}

/// The demotion cooldown: `8 × thrash_window` (see
/// `RECALL_COOLDOWN_WINDOWS`); explicit lever wins verbatim.
pub fn recall_cooldown_from(explicit_ms: Option<u64>, thrash_window: Duration) -> Duration {
    if let Some(ms) = explicit_ms {
        return Duration::from_millis(ms.max(1));
    }
    thrash_window.saturating_mul(RECALL_COOLDOWN_WINDOWS)
}

/// The published aggregate recall rate cap, per second: `roster ×
/// batch_max / deadline` — wire budget × lease arithmetic × roster size,
/// no free constant. The lane ENFORCES it structurally (one in-flight
/// frame of ≤ `batch_max` entries per client per deadline round); this
/// arithmetic is the gauge that makes the bound visible (the
/// `free_grace_bound` publish-the-derivation pattern).
pub fn recall_rate_cap_per_s(batch_max: usize, deadline: Duration, roster: usize) -> u64 {
    let deadline_ms = deadline.as_millis().max(1) as u64;
    (batch_max as u64)
        .saturating_mul(roster.max(1) as u64)
        .saturating_mul(1000)
        / deadline_ms
}

/// The lease TTL the deadline ceilings at: the armed membership plane's
/// own `T_owner` when installed (the lane runs on the OWNER — S6's
/// authority), else the same knob/default `LeaseClocks::derive` reads, so
/// the two planes can never disagree about what "the lease" means.
/// `pub(crate)`: rung 14's placement valve derives its thrash window from
/// the same quantity (a slot moving twice inside one lease period is
/// cycling faster than clients re-home their custody).
pub(crate) fn recall_lease_ttl() -> Duration {
    if let Some(o) = crate::membership::installed_owner() {
        return o.clocks().t_owner;
    }
    Duration::from_millis(crate::env_knobs::int_knob(
        "SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS",
        crate::fuse_client::CLIENT_STALE_TTL_SECS * 1000,
    ))
}

/// Roster size for the published rate cap: the armed membership plane's
/// member census when installed (who can hold future delegations), floored
/// at the lane's own live holder population and at 1.
fn recall_roster_size(live_clients: usize) -> usize {
    let members = crate::membership::installed_owner()
        .map(|o| o.len())
        .unwrap_or(0);
    members.max(live_clients).max(1)
}

/// The recall lane's derived configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallConfig {
    /// Ack deadline per issued frame ([`recall_deadline_from`]).
    pub deadline: Duration,
    /// Recall entries per frame ([`recall_batch_max_from`]).
    pub batch_max: usize,
    /// The cycle-detection window: = `deadline` — the lane's own
    /// completion horizon (an object re-granted before its recall round
    /// could even complete is cycling faster than the mechanism serves).
    pub thrash_window: Duration,
    /// Cycles before demotion ([`RECALL_THRASH_CYCLES`]).
    pub thrash_cycles: u32,
    /// Demotion hold ([`recall_cooldown_from`]).
    pub cooldown: Duration,
    /// **Test seam only** (the spec-R5 red half is built against
    /// `valve: false`). Deliberately NOT an env knob: the brake must not
    /// be operationally removable — that is the whole point of landing
    /// this rung before the engine.
    pub valve: bool,
}

impl RecallConfig {
    /// Derive from the live inputs (explicit levers win verbatim).
    pub fn derived() -> Self {
        Self::derived_with_evidence(super::live_recall_evidence_us(), true)
    }

    /// [`Self::derived`] with the p99 evidence supplied (the read-token
    /// lane's own RTT) and the valve as the caller's law.
    pub fn derived_with_evidence(live_p99_us: Option<u64>, valve: bool) -> Self {
        let batch_max = recall_batch_max_from(
            crate::env_knobs::opt_int_knob::<usize>(RECALL_BATCH_MAX_ENV),
            crate::cluster_wire::CONTROL_MAX_FRAME_BYTES,
        );
        let deadline = recall_deadline_from(
            crate::env_knobs::opt_int_knob::<u64>(RECALL_DEADLINE_ENV),
            live_p99_us,
            recall_lease_ttl(),
        );
        let thrash_window = deadline;
        let cooldown = recall_cooldown_from(
            crate::env_knobs::opt_int_knob::<u64>(RECALL_COOLDOWN_ENV),
            thrash_window,
        );
        Self {
            deadline,
            batch_max,
            thrash_window,
            thrash_cycles: RECALL_THRASH_CYCLES,
            cooldown,
            valve,
        }
    }
}

/// The valve's answer at grant time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantDecision {
    /// The grant was admitted and is now tracked (recall-able).
    Granted,
    /// The object is demoted to owner-served: no grant for `remaining`.
    Demoted {
        /// Cooldown left at the decision instant.
        remaining: Duration,
    },
}

/// One batched recall frame for one client — row 12's DelegRecall payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallFrame {
    /// The holder (KD-MW-2 client id).
    pub client: String,
    /// Ack correlation id, monotone per lane.
    pub frame_id: u64,
    /// The recalled objects (≤ `batch_max`).
    pub inos: Vec<u64>,
}

/// One recall declared DEAD at its deadline (the caller's eviction-
/// escalation input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedOutRecall {
    pub client: String,
    pub ino: u64,
    pub frame_id: u64,
}

/// The lane's counter snapshot (`dlm_recall` on the stats inode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecallLaneStats {
    /// Recalls issued into frames (spec spelling `dlm_revokes_issued` —
    /// the recall lane's face; `dlm_custody.dlm_revokes_issued` remains
    /// the S9 custody plane's, scoped by its own object).
    pub issued: u64,
    /// Recalls acked (`dlm_revokes_acked`).
    pub acked: u64,
    /// Recalls declared dead at the deadline (`dlm_revokes_timed_out`).
    pub timed_out: u64,
    /// Frames issued — the batching-law denominator (frames << issued).
    pub frames: u64,
    /// Recalls elided because one was already pending/in flight.
    pub coalesced: u64,
    /// Recalls held back by the rate discipline (engagement gauge;
    /// counted per pass, so one entry may count across passes — the
    /// `parked_gate_waits` semantics).
    pub rate_deferred: u64,
    /// Acks that matched no in-flight frame (protocol hygiene).
    pub stale_acks: u64,
    /// Grants returned WITHOUT a wire recall (rung 12: the mutating
    /// holder's own grant dies with its mutation's reply — the
    /// self-conflict never rides the recall lane, and it never stamps the
    /// thrash slot, because the valve protects the WIRE fan-out and a
    /// surrender has none).
    pub surrenders: u64,
    /// Grants admitted.
    pub grants: u64,
    /// Grants refused while demoted (the valve's refusal gauge).
    pub grant_refusals: u64,
    /// Valve engagements (spec R5's `dlm_thrash_demotions`).
    pub thrash_demotions: u64,
    /// Demoted objects re-admitted after their cooldown.
    pub repromotions: u64,
    /// GAUGE: outstanding (grant, holder) pairs.
    pub outstanding: u64,
    /// GAUGE: recalls queued, not yet issued.
    pub pending: u64,
    /// GAUGE: objects currently demoted.
    pub demoted_objects: u64,
}

#[derive(Debug, Default)]
struct ThrashSlot {
    /// The last recall episode's instant; cleared when a cycle is counted
    /// (one cycle per grant-after-recall EPISODE, never per holder).
    last_recall_at: Option<Instant>,
    cycles: u32,
    demoted_until: Option<Instant>,
}

#[derive(Debug)]
struct PendingRecall {
    ino: u64,
    enqueued_at: Instant,
}

#[derive(Debug)]
struct InflightFrame {
    frame_id: u64,
    issued_at: Instant,
    deadline_at: Instant,
    recalls: Vec<PendingRecall>,
}

#[derive(Default)]
struct LaneState {
    /// Outstanding grants: object → holders.
    grants: HashMap<u64, HashSet<String>>,
    /// Live recall requests (pending OR in flight): client → objects —
    /// the at-most-one-outstanding-recall-per-(object, client) dedupe.
    requested: HashMap<String, HashSet<u64>>,
    thrash: HashMap<u64, ThrashSlot>,
    /// Queued, not yet framed, per client.
    pending: HashMap<String, VecDeque<PendingRecall>>,
    /// The rate discipline: at most ONE in-flight frame per client.
    inflight: HashMap<String, InflightFrame>,
    next_frame_id: u64,
}

enum ConfigMode {
    /// Pinned (tests; measurement).
    Fixed(RecallConfig),
    /// Re-derived per use, so the deadline tracks the LIVE p99 evidence
    /// (spec R3) instead of freezing at first touch.
    Live,
    /// PR 5's read-token lane (design-symmetric-metadata §5.7): the
    /// deadline is the reader's LEASE (`T_owner` — a dead reader's tokens
    /// die with its lease, §5.7.1; never a p99-derived bound, which would
    /// declare a reader mid-drain dead), re-read per use, and the thrash
    /// valve is OFF — under R-SYM-4 a demoted object would have no read
    /// method at all, so the lane never demotes.
    LiveTokens,
}

/// Phase indices for `dlm_revoke_phase_ns`.
const PH_ISSUE: usize = 0;
const PH_ACK_WAIT: usize = 1;
const PH_TOTAL: usize = 2;
const RECALL_PHASES: usize = 3;
const RECALL_PHASE_NAMES: [&str; RECALL_PHASES] = ["issue", "ack_wait", "total"];

/// The owner-side recall lane: rate-limited batched recall + the thrash
/// valve. Control-plane machinery — a mutexed table is sanctioned here
/// (lease-transaction class, never the data hot path), and every counter
/// is a lock-free atomic so the stats read stays cheap.
pub struct RecallLane {
    mode: ConfigMode,
    state: Mutex<LaneState>,
    phases: [LatencyHistogram; RECALL_PHASES],
    issued: AtomicU64,
    acked: AtomicU64,
    timed_out: AtomicU64,
    frames: AtomicU64,
    coalesced: AtomicU64,
    rate_deferred: AtomicU64,
    stale_acks: AtomicU64,
    surrenders: AtomicU64,
    grants: AtomicU64,
    grant_refusals: AtomicU64,
    thrash_demotions: AtomicU64,
    repromotions: AtomicU64,
    /// GAUGE mirror of the outstanding (grant, holder) population — the
    /// lock-free fast probe the rung-12 mutation gate reads before paying
    /// anything (zero grants ⇒ the gate is one relaxed load + one atomic).
    outstanding_fast: AtomicU64,
}

impl RecallLane {
    fn new(mode: ConfigMode) -> Self {
        Self {
            mode,
            state: Mutex::new(LaneState::default()),
            phases: std::array::from_fn(|_| LatencyHistogram::default()),
            issued: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            rate_deferred: AtomicU64::new(0),
            stale_acks: AtomicU64::new(0),
            surrenders: AtomicU64::new(0),
            grants: AtomicU64::new(0),
            grant_refusals: AtomicU64::new(0),
            thrash_demotions: AtomicU64::new(0),
            repromotions: AtomicU64::new(0),
            outstanding_fast: AtomicU64::new(0),
        }
    }

    /// A lane with a pinned config (tests; measurement rows).
    pub fn with_config(cfg: RecallConfig) -> Self {
        Self::new(ConfigMode::Fixed(cfg))
    }

    /// A lane that re-derives its config per use (the global's mode).
    pub fn live() -> Self {
        Self::new(ConfigMode::Live)
    }

    /// The read-token plane's lane (PR 5): the recall deadline is the
    /// reader's lease TTL by derivation, the valve off (the
    /// `ConfigMode::LiveTokens` mode).
    pub fn live_tokens() -> Self {
        Self::new(ConfigMode::LiveTokens)
    }

    /// The lane's config as of NOW (fixed lanes answer their pin).
    pub fn config(&self) -> RecallConfig {
        match &self.mode {
            ConfigMode::Fixed(c) => *c,
            ConfigMode::Live => RecallConfig::derived(),
            // No evidence ⇒ `recall_deadline_from` answers the lease TTL.
            ConfigMode::LiveTokens => RecallConfig::derived_with_evidence(None, false),
        }
    }

    /// The valve's gate + grant bookkeeping. Rows 12+ call this FIRST at
    /// grant issuance; the population today comes from the test seams.
    ///
    /// One cycle is counted per grant-after-recall EPISODE (the first
    /// grant following a recall inside the thrash window), never per
    /// holder — a 32-holder re-grant wave is ONE cycle, so the threshold
    /// counts grant→recall LOOPS, which is what thrash is.
    pub fn try_grant(&self, ino: u64, client: &str, now: Instant) -> GrantDecision {
        let cfg = self.config();
        let mut st = self.state.lock();
        {
            let slot = st.thrash.entry(ino).or_default();
            if cfg.valve {
                if let Some(until) = slot.demoted_until {
                    if now < until {
                        self.grant_refusals.fetch_add(1, Ordering::Relaxed);
                        return GrantDecision::Demoted {
                            remaining: until.saturating_duration_since(now),
                        };
                    }
                    // Cooldown served: the first attempt re-promotes, with
                    // the thrash evidence reset (a fresh trial, not a
                    // carried verdict).
                    slot.demoted_until = None;
                    slot.cycles = 0;
                    slot.last_recall_at = None;
                    self.repromotions.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(last) = slot.last_recall_at {
                    if now.saturating_duration_since(last) <= cfg.thrash_window {
                        slot.cycles = slot.cycles.saturating_add(1);
                        slot.last_recall_at = None;
                        if slot.cycles >= cfg.thrash_cycles {
                            slot.demoted_until = Some(now + cfg.cooldown);
                            slot.cycles = 0;
                            self.thrash_demotions.fetch_add(1, Ordering::Relaxed);
                            self.grant_refusals.fetch_add(1, Ordering::Relaxed);
                            log::warn!(
                                "S10 recall valve: object ino {ino} demoted to owner-served for \
                                 {:?} — {} grant→recall cycles inside {:?} (spec R5's thrash \
                                 shape; grants resume after the cooldown)",
                                cfg.cooldown,
                                cfg.thrash_cycles,
                                cfg.thrash_window
                            );
                            return GrantDecision::Demoted {
                                remaining: cfg.cooldown,
                            };
                        }
                    } else {
                        // The loop broke on its own: stale evidence resets.
                        slot.cycles = 0;
                        slot.last_recall_at = None;
                    }
                }
            }
        }
        if st.grants.entry(ino).or_default().insert(client.to_string()) {
            self.outstanding_fast.fetch_add(1, Ordering::Relaxed);
        }
        self.grants.fetch_add(1, Ordering::Relaxed);
        GrantDecision::Granted
    }

    /// A grant returned WITHOUT the wire (rung 12): the holder itself is
    /// the conflicting mutator — its grant retires as the mutation
    /// executes and the revocation rides the mutation's own reply, so no
    /// recall frame, no ack round, and no thrash-slot stamp (the valve
    /// protects wire fan-out; a surrender has none). Also the grant-path
    /// RETRACTION arm: a grant recorded and then found un-issuable (the
    /// in-flight-mutation re-check, a vanished object) is taken back
    /// before it was ever sent. Returns whether a grant was held.
    pub fn surrender(&self, ino: u64, client: &str) -> bool {
        let mut st = self.state.lock();
        let held = match st.grants.get_mut(&ino) {
            Some(hs) => {
                let removed = hs.remove(client);
                if hs.is_empty() {
                    st.grants.remove(&ino);
                }
                removed
            }
            None => false,
        };
        if held {
            self.outstanding_fast.fetch_sub(1, Ordering::Relaxed);
            self.surrenders.fetch_add(1, Ordering::Relaxed);
        }
        held
    }

    /// Lock-free probe of the outstanding (grant, holder) population —
    /// what lets the rung-12 mutation gate cost one atomic when nothing
    /// is delegated.
    pub fn outstanding_now(&self) -> u64 {
        self.outstanding_fast.load(Ordering::Relaxed)
    }

    /// Owner-initiated recall of EVERY outstanding grant on `ino`
    /// (batched per client by [`Self::issue_pass`]). Returns the number
    /// of recalls enqueued; already-pending/in-flight `(object, client)`
    /// recalls coalesce. Stamps the object's thrash slot — the recall
    /// half of the cycle evidence.
    pub fn recall_object(&self, ino: u64, now: Instant) -> usize {
        self.recall_object_excluding(ino, None, now)
    }

    /// The batched, rate-limited issue pass: for every client with queued
    /// recalls and NO in-flight frame, drain up to `batch_max` entries
    /// into one frame. The caller (row 12's wire half; the tests today)
    /// puts the frames on the wire. The rate limiter IS the structure:
    /// one in-flight frame of ≤ `batch_max` entries per client per
    /// ack/deadline round — `batch_max/deadline` per client, derived at
    /// both ends, no constant.
    pub fn issue_pass(&self, now: Instant) -> Vec<RecallFrame> {
        let cfg = self.config();
        let mut st = self.state.lock();
        let mut frames = Vec::new();
        let clients: Vec<String> = st
            .pending
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(c, _)| c.clone())
            .collect();
        for client in clients {
            if st.inflight.contains_key(&client) {
                let waiting = st.pending.get(&client).map(|q| q.len()).unwrap_or(0);
                self.rate_deferred
                    .fetch_add(waiting as u64, Ordering::Relaxed);
                continue;
            }
            let Some(q) = st.pending.get_mut(&client) else {
                continue;
            };
            let take = q.len().min(cfg.batch_max);
            let mut recalls = Vec::with_capacity(take);
            let mut inos = Vec::with_capacity(take);
            for _ in 0..take {
                let r = q.pop_front().expect("take <= q.len()");
                self.phases[PH_ISSUE].record(now.saturating_duration_since(r.enqueued_at));
                inos.push(r.ino);
                recalls.push(r);
            }
            let leftover = q.len();
            if leftover == 0 {
                st.pending.remove(&client);
            } else {
                // Held back by the batch cap — the limiter's other face.
                self.rate_deferred
                    .fetch_add(leftover as u64, Ordering::Relaxed);
            }
            st.next_frame_id += 1;
            let frame_id = st.next_frame_id;
            st.inflight.insert(
                client.clone(),
                InflightFrame {
                    frame_id,
                    issued_at: now,
                    deadline_at: now + cfg.deadline,
                    recalls,
                },
            );
            self.issued.fetch_add(take as u64, Ordering::Relaxed);
            self.frames.fetch_add(1, Ordering::Relaxed);
            frames.push(RecallFrame {
                client,
                frame_id,
                inos,
            });
        }
        frames
    }

    /// The client acked its frame: every recall in it is terminal, the
    /// surrendered grants leave the table, and the client's rate slot
    /// frees. An ack that matches no in-flight frame (a resend, a
    /// post-timeout straggler) is counted and changes nothing — the
    /// S8-dedup posture applied to acks.
    pub fn ack_frame(&self, client: &str, frame_id: u64, now: Instant) -> usize {
        let mut st = self.state.lock();
        let matches = st
            .inflight
            .get(client)
            .map(|f| f.frame_id == frame_id)
            .unwrap_or(false);
        if !matches {
            self.stale_acks.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        let f = st.inflight.remove(client).expect("checked above");
        for r in &f.recalls {
            self.phases[PH_ACK_WAIT].record(now.saturating_duration_since(f.issued_at));
            self.phases[PH_TOTAL].record(now.saturating_duration_since(r.enqueued_at));
            if retire_recall(&mut st, client, r.ino) {
                self.outstanding_fast.fetch_sub(1, Ordering::Relaxed);
            }
        }
        self.acked
            .fetch_add(f.recalls.len() as u64, Ordering::Relaxed);
        f.recalls.len()
    }

    /// The deadline sweep: every frame past its deadline is DEAD — the
    /// grants it recalled leave the table and the objects are grantable
    /// again. Loud, never silent (the `transport_lease_overlong`
    /// precedent); the returned list is the caller's eviction-escalation
    /// input (rows 12+ feed membership `evict`, minting the S7 dead
    /// epoch).
    ///
    /// Soundness: the deadline's ceiling is the membership lease TTL, and
    /// a member's `T_self` (= `T_owner − 2·skew_max − D_purge`) fires
    /// STRICTLY before the owner's TTL — so a holder that could not ack
    /// inside the deadline has either died or self-fenced before the
    /// owner may act as if the grant were gone; its in-flight DMA is
    /// S7's dead-epoch quarantine's problem, never this table's.
    pub fn expire_overdue(&self, now: Instant) -> Vec<TimedOutRecall> {
        let mut st = self.state.lock();
        let overdue: Vec<String> = st
            .inflight
            .iter()
            .filter(|(_, f)| now >= f.deadline_at)
            .map(|(c, _)| c.clone())
            .collect();
        let mut out = Vec::new();
        for client in overdue {
            let f = st.inflight.remove(&client).expect("collected above");
            log::warn!(
                "S10 recall lane: client '{client}' missed its recall deadline — frame {} \
                 carrying {} recall(s) is DEAD (grants retired; the S6 lease/T_self arithmetic \
                 bounds the holder, and rows 12+ escalate to membership eviction)",
                f.frame_id,
                f.recalls.len()
            );
            for r in f.recalls {
                self.phases[PH_TOTAL].record(now.saturating_duration_since(r.enqueued_at));
                if retire_recall(&mut st, &client, r.ino) {
                    self.outstanding_fast.fetch_sub(1, Ordering::Relaxed);
                }
                self.timed_out.fetch_add(1, Ordering::Relaxed);
                out.push(TimedOutRecall {
                    client: client.clone(),
                    ino: r.ino,
                    frame_id: f.frame_id,
                });
            }
        }
        out
    }

    /// **Test seam**: drop the lane's whole grant/recall/thrash state
    /// (counters stay — suites assert deltas). What lets a suite binary
    /// run its tests against fresh volumes whose ino numbering restarts,
    /// without one test's thrash slots or stranded grants leaking into
    /// the next test's identically-numbered objects. Production never
    /// calls it.
    pub fn test_clear_state(&self) {
        let mut st = self.state.lock();
        st.grants.clear();
        st.requested.clear();
        st.thrash.clear();
        st.pending.clear();
        st.inflight.clear();
        self.outstanding_fast.store(0, Ordering::Relaxed);
    }

    /// Outstanding holders of `ino`.
    pub fn holders(&self, ino: u64) -> usize {
        self.state
            .lock()
            .grants
            .get(&ino)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Every object with an outstanding grant that `keep` selects — the
    /// slot transfer's census (symmetric PR 12b: the objects of ONE forest
    /// slot, by the key ino's slot bits). One lock hold, O(objects
    /// outstanding).
    pub fn objects_where(&self, keep: impl Fn(u64) -> bool) -> Vec<u64> {
        self.state
            .lock()
            .grants
            .keys()
            .copied()
            .filter(|o| keep(*o))
            .collect()
    }

    /// The holders of `ino` by identity (the token plane's lease sweep
    /// reads each one's membership verdict before it recalls).
    pub fn holders_of(&self, ino: u64) -> Vec<String> {
        self.state
            .lock()
            .grants
            .get(&ino)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Every client with a recall PENDING or IN FLIGHT — the population
    /// the token plane's wait loop reads lease verdicts for.
    pub fn recall_clients(&self) -> Vec<String> {
        let st = self.state.lock();
        let mut out: Vec<String> = st.inflight.keys().cloned().collect();
        for c in st.pending.keys() {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
        out
    }

    /// **The lease sweep** (PR 5, design-symmetric-metadata §5.7.1 "a
    /// dead reader's tokens die with its lease"): retire EVERYTHING
    /// `client` holds or owes — its in-flight frame, its pending queue,
    /// its requested set and every grant it holds on any object — the
    /// moment its membership lease is seen expired, so a dead reader costs
    /// the volume's commit stream at most one wait to its lease expiry and
    /// never a second one. Returns `(recalls retired, grants retired)` —
    /// the recalls that were outstanding on it, and EVERY grant it held
    /// (the recalled ones included).
    pub fn retire_client(&self, client: &str) -> (usize, usize) {
        let mut st = self.state.lock();
        let mut recalls = 0usize;
        if let Some(f) = st.inflight.remove(client) {
            recalls += f.recalls.len();
            self.timed_out
                .fetch_add(f.recalls.len() as u64, Ordering::Relaxed);
        }
        if let Some(q) = st.pending.remove(client) {
            recalls += q.len();
        }
        st.requested.remove(client);
        let mut grants = 0usize;
        st.grants.retain(|_, hs| {
            if hs.remove(client) {
                grants += 1;
            }
            !hs.is_empty()
        });
        if grants > 0 {
            self.outstanding_fast
                .fetch_sub(grants as u64, Ordering::Relaxed);
        }
        (recalls, grants)
    }

    /// Outstanding holders of `ino` EXCLUDING `client` (rung 13: the
    /// intent apply's gate waits out every FOREIGN holder while the
    /// flushing UPDATE holder's own grant stays live — surrendering it
    /// would orphan the owner-side authority record and deadlock the
    /// recall the flush itself answers).
    pub fn holders_excluding(&self, ino: u64, client: &str) -> usize {
        self.state
            .lock()
            .grants
            .get(&ino)
            .map(|s| s.iter().filter(|c| c.as_str() != client).count())
            .unwrap_or(0)
    }

    /// Does `client` hold a live grant on `ino`? (The owner-side UPDATE
    /// exclusivity table's liveness check.)
    pub fn holds(&self, ino: u64, client: &str) -> bool {
        self.state
            .lock()
            .grants
            .get(&ino)
            .is_some_and(|s| s.contains(client))
    }

    /// [`Self::recall_object`] excluding one holder (rung 13: the intent
    /// apply recalls every FOREIGN grant on the directory and never its
    /// own — a wire recall to the flushing holder mid-flush would recurse
    /// the recall-forces-flush law into itself).
    pub fn recall_object_excluding(&self, ino: u64, skip: Option<&str>, now: Instant) -> usize {
        let mut st = self.state.lock();
        let holders: Vec<String> = st
            .grants
            .get(&ino)
            .map(|s| {
                s.iter()
                    .filter(|c| skip != Some(c.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if holders.is_empty() {
            return 0;
        }
        let mut enqueued = 0usize;
        for client in holders {
            if !st.requested.entry(client.clone()).or_default().insert(ino) {
                self.coalesced.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            st.pending
                .entry(client)
                .or_default()
                .push_back(PendingRecall {
                    ino,
                    enqueued_at: now,
                });
            enqueued += 1;
        }
        st.thrash.entry(ino).or_default().last_recall_at = Some(now);
        enqueued
    }

    /// Counter snapshot + gauges.
    pub fn stats(&self) -> RecallLaneStats {
        let now = Instant::now();
        let (outstanding, pending, demoted) = {
            let st = self.state.lock();
            (
                st.grants.values().map(|s| s.len() as u64).sum(),
                st.pending.values().map(|q| q.len() as u64).sum(),
                st.thrash
                    .values()
                    .filter(|t| t.demoted_until.is_some_and(|u| now < u))
                    .count() as u64,
            )
        };
        RecallLaneStats {
            issued: self.issued.load(Ordering::Relaxed),
            acked: self.acked.load(Ordering::Relaxed),
            timed_out: self.timed_out.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            coalesced: self.coalesced.load(Ordering::Relaxed),
            rate_deferred: self.rate_deferred.load(Ordering::Relaxed),
            stale_acks: self.stale_acks.load(Ordering::Relaxed),
            surrenders: self.surrenders.load(Ordering::Relaxed),
            grants: self.grants.load(Ordering::Relaxed),
            grant_refusals: self.grant_refusals.load(Ordering::Relaxed),
            thrash_demotions: self.thrash_demotions.load(Ordering::Relaxed),
            repromotions: self.repromotions.load(Ordering::Relaxed),
            outstanding,
            pending,
            demoted_objects: demoted,
        }
    }

    /// `dlm_revoke_phase_ns` — issue / ack_wait / total, on the shared
    /// 26-bucket latency core (bucket-compatible with every other phase
    /// table by construction).
    pub fn phase_json(&self) -> serde_json::Value {
        let mut phases = serde_json::Map::new();
        for (i, name) in RECALL_PHASE_NAMES.iter().enumerate() {
            phases.insert((*name).to_string(), self.phases[i].to_json());
        }
        serde_json::Value::Object(phases)
    }
}

/// The token plane's grant ∥ pass gate reads and registers holders
/// through the lane (`token_grant_core::HolderTable`): `register` is a
/// `holds`-then-`try_grant` (the valve is off on the token lane, so the
/// decision is always `Granted`).
impl crate::token_grant_core::HolderTable for RecallLane {
    fn register(&self, object: u64, client: &str) -> bool {
        if self.holds(object, client) {
            return false;
        }
        let _ = self.try_grant(object, client, Instant::now());
        true
    }

    fn holders(&self, object: u64) -> usize {
        RecallLane::holders(self, object)
    }
}

/// A recall reached its terminal outcome (ack or timeout): the grant and
/// the dedupe entry retire together. Returns whether a grant was actually
/// removed (the caller maintains the lock-free outstanding gauge).
fn retire_recall(st: &mut LaneState, client: &str, ino: u64) -> bool {
    let removed = match st.grants.get_mut(&ino) {
        Some(hs) => {
            let removed = hs.remove(client);
            if hs.is_empty() {
                st.grants.remove(&ino);
            }
            removed
        }
        None => false,
    };
    if let Some(req) = st.requested.get_mut(client) {
        req.remove(&ino);
        if req.is_empty() {
            st.requested.remove(client);
        }
    }
    removed
}

/// The process-global lane — the one the stats inode exports and the one
/// rows 12–14 populate. Production never touches it today (no delegation
/// grants exist), so every field is 0 on every shipped mount BY
/// CONSTRUCTION — the dark-posture law this rung pins.
static GLOBAL_RECALL_LANE: Lazy<RecallLane> = Lazy::new(RecallLane::live);

/// The global recall lane.
pub fn global_recall_lane() -> &'static RecallLane {
    &GLOBAL_RECALL_LANE
}

/// The `dlm_recall` stats-inode object. Spec spellings
/// (`dlm_revokes_{issued,acked,timed_out}`, `dlm_thrash_demotions`) plus
/// the lane's own engagement gauges and the DERIVED caps published as
/// numbers, so the operator page can never drift from the arithmetic in
/// force. (`dlm_custody.dlm_revokes_{issued,expired}` is the S9 custody
/// plane's pull-model face — no ack, expiry terminal; this object is the
/// S10 push-model recall lane — scoped apart by their objects, same
/// family spelling, never a fork.)
pub fn recall_stats_json() -> serde_json::Value {
    let lane = global_recall_lane();
    let s = lane.stats();
    let cfg = lane.config();
    let live_clients = lane.state.lock().grants.values().flatten().count();
    serde_json::json!({
        "dlm_revokes_issued": s.issued,
        "dlm_revokes_acked": s.acked,
        "dlm_revokes_timed_out": s.timed_out,
        "dlm_recall_frames": s.frames,
        "dlm_recall_coalesced": s.coalesced,
        "dlm_recall_rate_deferred": s.rate_deferred,
        "dlm_recall_stale_acks": s.stale_acks,
        "dlm_recall_surrenders": s.surrenders,
        "dlm_recall_grants": s.grants,
        "dlm_recall_grant_refusals": s.grant_refusals,
        "dlm_thrash_demotions": s.thrash_demotions,
        "dlm_recall_repromotions": s.repromotions,
        "dlm_recall_outstanding": s.outstanding,
        "dlm_recall_pending": s.pending,
        "dlm_recall_demoted_objects": s.demoted_objects,
        "dlm_recall_deadline_ms": cfg.deadline.as_millis() as u64,
        "dlm_recall_batch_max": cfg.batch_max as u64,
        "dlm_recall_cooldown_ms": cfg.cooldown.as_millis() as u64,
        "dlm_recall_rate_cap_per_s": recall_rate_cap_per_s(
            cfg.batch_max,
            cfg.deadline,
            recall_roster_size(live_clients),
        ),
    })
}

/// `dlm_revoke_phase_ns` for the stats inode (the global lane's table).
pub fn revoke_phase_json() -> serde_json::Value {
    global_recall_lane().phase_json()
}

// ===========================================================================
// DLM S10 rung 12 — the CLIENT DELEGATION CACHE (the engine's holder half).
//
// `docs/design-full-multi-writer.md` §8.2 lever 1: a delegation is a
// capability token over an object, piggybacked on the replies of metadata
// RPCs the holder was already issuing, serving LOOKUP-class verbs from the
// holder's reader-revalidation view under the owner-enforced coherence law
// (recall-before-conflicting-publish). RAM only (KD-MW-5).
//
// # Why a serve is sound (the three gates, in order)
//
// 1. **The stamp gate** closes the warming window: an entry serves only
//    when the holder's LOCAL view of the object matches the grant's
//    `DelegStamp` byte for byte — v3's whole-tx atomicity + checkpoint-
//    prefix visibility mean a matching inode record implies the view
//    includes every transaction up to the one that produced the stamp, so
//    the dentry set is exactly as current as the attrs.
// 2. **The recall channel's freshness** bounds delivery: serves run only
//    while the holder's standing DelegRecall round is fresh (a completed
//    round within the derived window), so a recall reaches the holder
//    promptly or serving stops on its own. The pathological cases (a
//    holder that answers nothing) are bounded by the S6 membership
//    arithmetic — a timed-out recall escalates to eviction and the
//    member's own `T_self` self-fence fires before the owner re-grants
//    (the S9 custody plane's exact posture: "the window is bounded by its
//    own T_self self-fence, never by its belief").
// 3. **The era gate**: an entry's `term` must match the channel's learned
//    owner term — a failover's survivors re-assert and re-install under
//    the successor's era, never serve across it.
//
// # The grant/recall reordering law (the fence)
//
// Grants ride the batch lane; recalls ride the recall channel — two
// sessions, no cross-ordering. Every recall carries the owner's
// delegation-sequence FENCE at frame-build time; the holder tombstones the
// recalled inos at that fence and drops any later-arriving grant at or
// below it. Owner-side, a grant's `seq` is minted BEFORE the lane records
// it, so any grant the recall could have named is ≤ the fence — a dropped
// grant is only ever a lost optimization (the next shipped verb re-earns
// it), never a correctness event.
// ===========================================================================

use super::wire::{DelegGrant, DelegStamp};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

/// The S10 A/B lever (design §11): ENG-10 `Kind::Bool`, static default
/// **on**, read only when the mw plane is armed (`ownership_armed`) — the
/// `SQUEEZEFS_MW_ROLE` precedent. `=1` on an unarmed mount is
/// announced-inert; `=0` on an armed mount is the A/B control.
pub const DELEGATION_ENV: &str = "SQUEEZEFS_DELEGATION";

/// **Test seam** (the `TEST_SHIP_DRAIN_HOLD_MS` precedent): `0` = read the
/// env knob, `1` = force on, `2` = force off — so one suite binary can pin
/// both sides of the A/B without racing process-global env mutation.
pub static TEST_DELEGATION_OVERRIDE: AtomicU8 = AtomicU8::new(0);

/// Is the delegation plane live on this process? One relaxed load on every
/// unarmed mount (the solo re-gate's law) — the knob is consulted only
/// past the armed gate.
pub fn delegation_enabled() -> bool {
    match TEST_DELEGATION_OVERRIDE.load(Ordering::Relaxed) {
        1 => return super::ownership_armed(),
        2 => return false,
        _ => {}
    }
    super::ownership_armed() && crate::env_knobs::bool_knob(DELEGATION_ENV, true)
}

/// Entry state: serving.
const DELEG_LIVE: u8 = 0;
/// Entry state: recalled/suspended — no NEW serve begins; in-flight serves
/// drain.
const DELEG_REVOKED: u8 = 1;
/// Entry state: the recall was ACKED (drain complete). A serve completing
/// in this state is the must-stay-0 `dlm_delegation_stale_serves`
/// tripwire — structurally unreachable because the ack waits for the
/// drain, and counted so a future regression is loud, never silent.
const DELEG_ACKED: u8 = 2;

/// One held delegation.
struct DelegEntry {
    endpoint: Arc<str>,
    class: u8,
    dir: bool,
    seq: u64,
    term: u64,
    stamp: DelegStamp,
    state: AtomicU8,
    /// Local serves in flight (the never-serve-after-ack law: the recall
    /// drain waits for this to reach 0 BEFORE the ack is queued).
    inflight: AtomicU64,
    /// Second-chance reference bit (the token cache's retirement law).
    used: std::sync::atomic::AtomicBool,
    /// Signaled by every serve completion while revoked — what the drain
    /// parks on (never a sleep-as-synchronization).
    drain: squeezefs_ipc::sqz_notify::Notify,
}

/// Bytes charged per cached delegation: the entry, its `Arc`, and the
/// `scc` bucket slot (same accounting style as [`ENTRY_BYTES`]).
const DELEG_ENTRY_BYTES: u64 = 96;

static DELEG_CACHE: Lazy<scc::HashMap<u64, Arc<DelegEntry>>> = Lazy::new(scc::HashMap::new);

/// The grant/recall reordering fence: ino → `(term, seq)` floor. A grant
/// at or below its object's floor is dead on arrival.
static DELEG_TOMBSTONES: Lazy<scc::HashMap<u64, (u64, u64)>> = Lazy::new(scc::HashMap::new);

/// Per-owner recall-channel health — the holder-side serve gate.
pub(crate) struct DelegChannel {
    pub(crate) healthy: std::sync::atomic::AtomicBool,
    /// Monotonic ms (process epoch) of the last completed round.
    pub(crate) last_ok_ms: AtomicU64,
    /// The owner's published park bound, ms (the freshness window input).
    pub(crate) park_ms: AtomicU64,
    /// The owner era the channel last learned.
    pub(crate) term: AtomicU64,
}

static DELEG_CHANNELS: Lazy<scc::HashMap<String, Arc<DelegChannel>>> = Lazy::new(scc::HashMap::new);

/// Process monotonic epoch for the channel arithmetic.
static DELEG_EPOCH: Lazy<Instant> = Lazy::new(Instant::now);

fn now_ms() -> u64 {
    DELEG_EPOCH.elapsed().as_millis() as u64
}

/// Conservative park default a channel is born with (the spawn-to-first-
/// round window): the owner's own floor-to-ceiling park derivation lands
/// in [100 ms, 5 s]; 1 s keeps the initial freshness window tight while
/// the first round is in flight, and the first reply replaces it with the
/// owner's published number (the token plane's recall channel reads the
/// same default and derives its reconnect backoff from it).
pub(super) const DELEG_PARK_DEFAULT_MS: u64 = 1_000;

/// Freshness slack over two park rounds: scheduling + one RTT of grace
/// (the token plane's recall channel reads the same law).
pub(super) const DELEG_FRESH_SLACK_MS: u64 = 2_000;

// The `dlm_delegation` family (design §13). Client-face counters live
// here; the owner-face counters (grants issued, declines, timeouts) are
// incremented by the service through the `note_*` fns so the whole family
// assembles in ONE place.
static DELEG_GRANTS_ISSUED: AtomicU64 = AtomicU64::new(0);
static DELEG_DECLINES: AtomicU64 = AtomicU64::new(0);
static DELEG_INSTALLS: AtomicU64 = AtomicU64::new(0);
static DELEG_HITS: AtomicU64 = AtomicU64::new(0);
static DELEG_RECALLS: AtomicU64 = AtomicU64::new(0);
static DELEG_REASSERTS: AtomicU64 = AtomicU64::new(0);
static DELEG_REPLY_REVOKES: AtomicU64 = AtomicU64::new(0);
static DELEG_STALE_SERVES: AtomicU64 = AtomicU64::new(0);
static DELEG_RECALL_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static DELEG_TOMBSTONE_DROPS: AtomicU64 = AtomicU64::new(0);
static DELEG_CHANNEL_SUSPENDS: AtomicU64 = AtomicU64::new(0);
static DELEG_CHANNEL_ROUNDS: AtomicU64 = AtomicU64::new(0);
static DELEG_EVICTIONS: AtomicU64 = AtomicU64::new(0);

/// The recall-invalidation sink: the FUSE mount installs a closure that
/// pushes `notify_inval_inode` for a dropped delegation, which is what
/// bounds the STRETCHED kernel attr TTL (design §8.2: "the recall is what
/// bounds staleness"). `None` (tests, unmounted processes) = the drop
/// still stops daemon-side serves.
#[allow(clippy::type_complexity)]
static DELEG_INVAL_SINK: Lazy<parking_lot::RwLock<Option<Arc<dyn Fn(u64) + Send + Sync>>>> =
    Lazy::new(|| parking_lot::RwLock::new(None));

/// Install (or clear) the kernel-invalidation sink.
pub fn set_deleg_inval_sink(sink: Option<Arc<dyn Fn(u64) + Send + Sync>>) {
    *DELEG_INVAL_SINK.write() = sink;
}

fn fire_inval_sink(ino: u64) {
    let sink = DELEG_INVAL_SINK.read().clone();
    if let Some(sink) = sink {
        sink(ino);
    }
}

/// The intents module's face of the invalidation sink (a destroyed mint's
/// kernel invalidation — rung 13).
pub(crate) fn fire_deleg_inval(ino: u64) {
    fire_inval_sink(ino);
}

/// The R5 registration (design §13: "bytes rides R5") — sheddable at
/// weight 1 with floor 0: every dropped entry costs one re-earned grant,
/// never a wrong answer (the token-cache retirement law).
static DELEG_R5: std::sync::Once = std::sync::Once::new();

fn ensure_deleg_r5() {
    DELEG_R5.call_once(|| {
        crate::mem_budget::MEM_BUDGET.register(crate::mem_budget::Component::new(
            "dlm_delegation_entries",
            0,
            1,
            Arc::new(|| DELEG_CACHE.len() as u64 * DELEG_ENTRY_BYTES),
            Arc::new(|_| {
                DELEG_CACHE.retain_sync(|_, _| false);
            }),
        ));
    });
}

/// The cache's entry cap: the token cache's derivation verbatim (same
/// budget share, same floor) — one law, not two.
fn deleg_cache_cap() -> usize {
    cache_cap()
}

/// One bounded second-chance pass (the token-cache sweep's law).
fn deleg_sweep() {
    let mut retired = 0u64;
    DELEG_CACHE.retain_sync(|_, e| {
        if e.used.swap(false, Ordering::Relaxed) {
            true
        } else {
            retired += 1;
            false
        }
    });
    if retired == 0 {
        DELEG_CACHE.retain_sync(|_, _| false);
    }
}

/// Owner face: a delegation grant rode a reply.
pub(crate) fn note_deleg_grant_issued() {
    DELEG_GRANTS_ISSUED.fetch_add(1, Ordering::Relaxed);
}

/// Owner face: a grant was declined (in-flight mutation, vanished object).
pub(crate) fn note_deleg_decline() {
    DELEG_DECLINES.fetch_add(1, Ordering::Relaxed);
}

/// Owner face: a delegation recall timed out (LOUD; the caller escalates
/// to membership eviction — the rung-11 residual #2 discharged).
pub(crate) fn note_deleg_recall_timeout() {
    DELEG_RECALL_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

/// Owner face: a timed-out holder was evicted from the membership plane.
pub(crate) fn note_deleg_eviction() {
    DELEG_EVICTIONS.fetch_add(1, Ordering::Relaxed);
}

/// Client face: a re-assertion round completed against a (successor)
/// owner.
pub(crate) fn note_deleg_reassert_round() {
    DELEG_REASSERTS.fetch_add(1, Ordering::Relaxed);
}

/// Install a piggybacked grant. Tombstoned, capped, monotone per object
/// (an older or equal `(term, seq)` never displaces a newer entry).
pub fn install_delegation(endpoint: &str, grant: &DelegGrant) {
    ensure_deleg_r5();
    if let Some((t_term, t_seq)) = DELEG_TOMBSTONES.read_sync(&grant.ino, |_, v| *v) {
        // The fence: a grant at or below its object's floor is dead on
        // arrival (it was minted before a recall that already retired it
        // owner-side — installing it would serve with no recall coming).
        if grant.term < t_term || (grant.term == t_term && grant.seq <= t_seq) {
            DELEG_TOMBSTONE_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let _ = DELEG_TOMBSTONES.remove_sync(&grant.ino);
    }
    if DELEG_CACHE.len() >= deleg_cache_cap() {
        deleg_sweep();
    }
    // The grant rode an authenticated reply, so it IS era evidence: the
    // channel adopts it monotonically (the lane's `learn_term` law), which
    // is what lets a grant serve during the channel's first round's
    // flight. A failover's higher term still invalidates every older
    // entry — `fetch_max` only ever rises.
    deleg_channel(endpoint)
        .term
        .fetch_max(grant.term, Ordering::AcqRel);
    let fresh = Arc::new(DelegEntry {
        endpoint: Arc::from(endpoint),
        class: grant.class,
        dir: grant.dir,
        seq: grant.seq,
        term: grant.term,
        stamp: grant.stamp,
        state: AtomicU8::new(DELEG_LIVE),
        inflight: AtomicU64::new(0),
        used: std::sync::atomic::AtomicBool::new(true),
        drain: squeezefs_ipc::sqz_notify::Notify::new(),
    });
    match DELEG_CACHE.entry_sync(grant.ino) {
        scc::hash_map::Entry::Occupied(mut o) => {
            let cur = o.get();
            if grant.term > cur.term || (grant.term == cur.term && grant.seq > cur.seq) {
                *o.get_mut() = fresh;
                DELEG_INSTALLS.fetch_add(1, Ordering::Relaxed);
            }
        }
        scc::hash_map::Entry::Vacant(v) => {
            v.insert_entry(fresh);
            DELEG_INSTALLS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The channel registry entry for `endpoint` (created on first use —
/// healthy from spawn so the grant-to-first-round window can serve; a
/// failed connect marks it down immediately).
pub(crate) fn deleg_channel(endpoint: &str) -> Arc<DelegChannel> {
    if let Some(ch) = DELEG_CHANNELS.read_sync(endpoint, |_, v| Arc::clone(v)) {
        return ch;
    }
    let fresh = Arc::new(DelegChannel {
        healthy: std::sync::atomic::AtomicBool::new(true),
        last_ok_ms: AtomicU64::new(now_ms()),
        park_ms: AtomicU64::new(DELEG_PARK_DEFAULT_MS),
        term: AtomicU64::new(0),
    });
    match DELEG_CHANNELS.insert_sync(endpoint.to_string(), Arc::clone(&fresh)) {
        Ok(()) => fresh,
        Err(_) => DELEG_CHANNELS
            .read_sync(endpoint, |_, v| Arc::clone(v))
            .unwrap_or(fresh),
    }
}

/// A recall-channel round completed: refresh the freshness clock, adopt
/// the owner's published park bound and era.
pub(crate) fn deleg_channel_mark_ok(endpoint: &str, park_ms: u64, term: u64) {
    let ch = deleg_channel(endpoint);
    ch.park_ms.store(park_ms.max(1), Ordering::Relaxed);
    let prev = ch.term.fetch_max(term, Ordering::AcqRel);
    if term > prev && prev > 0 {
        // Rung 13: a failover observed — un-flushed intents were minted
        // from the dead era's supply and can never apply (the era gate
        // refuses them whole); they die through the §8.2 error channel.
        super::intents::owner_era_moved(endpoint, term);
    }
    ch.last_ok_ms.store(now_ms(), Ordering::Release);
    ch.healthy.store(true, Ordering::Release);
    DELEG_CHANNEL_ROUNDS.fetch_add(1, Ordering::Relaxed);
}

/// The channel failed (connect refused, call error, fence): serves stop
/// NOW — fail-closed.
pub(crate) fn deleg_channel_mark_down(endpoint: &str) {
    if let Some(ch) = DELEG_CHANNELS.read_sync(endpoint, |_, v| Arc::clone(v)) {
        ch.healthy.store(false, Ordering::Release);
    }
}

/// Is the recall channel fresh enough to serve under? Healthy AND the
/// last completed round is within two park rounds + slack — a recall
/// issued while we can serve is deliverable within the window, and the
/// pathological remainder is bounded by the S6 eviction/T_self arithmetic
/// (the S9 custody plane's documented posture).
pub(crate) fn deleg_channel_fresh(endpoint: &str) -> bool {
    let Some(ch) = DELEG_CHANNELS.read_sync(endpoint, |_, v| Arc::clone(v)) else {
        return false;
    };
    if !ch.healthy.load(Ordering::Acquire) {
        return false;
    }
    let age = now_ms().saturating_sub(ch.last_ok_ms.load(Ordering::Acquire));
    age <= ch.park_ms.load(Ordering::Relaxed).saturating_mul(2) + DELEG_FRESH_SLACK_MS
}

/// An in-flight delegated serve: RAII over the entry's inflight count —
/// what the recall drain waits on, and what makes a serve-after-ack a
/// counted tripwire instead of a silent possibility.
pub struct DelegServeGuard {
    entry: Arc<DelegEntry>,
}

impl DelegServeGuard {
    /// Is the holder's view CURRENT for this grant — does its covered
    /// journal prefix reach the grant's commit watermark? (`>=`: a view
    /// past the mint can only contain transactions the coherence law has
    /// already recalled this grant for, or the grant's own era.)
    pub fn view_current(&self, view_watermark: u64) -> bool {
        view_watermark >= self.entry.stamp.watermark
    }

    /// Is the delegated object a directory (dentry reads servable)?
    pub fn dir(&self) -> bool {
        self.entry.dir
    }

    /// Count one delegated serve on the hits ledger.
    pub fn note_hit(&self) {
        DELEG_HITS.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for DelegServeGuard {
    fn drop(&mut self) {
        self.entry.inflight.fetch_sub(1, Ordering::AcqRel);
        if self.entry.state.load(Ordering::Acquire) == DELEG_ACKED {
            // Structurally unreachable (the ack waits for the drain); if
            // it ever fires it is the coherence bug the design's
            // must-stay-0 tripwire exists to make LOUD.
            DELEG_STALE_SERVES.fetch_add(1, Ordering::Relaxed);
            crate::note_invariant_tripwire(
                "meta_ship::deleg_stale_serve",
                "a delegated serve completed AFTER its recall was acked — the drain-before-ack \
                 law broke (dlm_delegation_stale_serves)",
            );
        }
        self.entry.drain.notify_waiters();
    }
}

/// Begin a delegated serve of `ino` against the owner at `endpoint`:
/// `None` unless the lever is on, the entry is LIVE and era-current, its
/// class covers LOOKUP, and the recall channel is fresh. The stamp gate is
/// the CALLER's (it owns the local view read).
pub fn deleg_serve_begin(ino: u64, endpoint: &str) -> Option<DelegServeGuard> {
    if !delegation_enabled() {
        return None;
    }
    let entry = DELEG_CACHE.read_sync(&ino, |_, e| Arc::clone(e))?;
    if entry.state.load(Ordering::Acquire) != DELEG_LIVE
        || entry.class & super::wire::DELEG_CLASS_LOOKUP == 0
        || entry.endpoint.as_ref() != endpoint
    {
        return None;
    }
    if !deleg_channel_fresh(endpoint)
        || entry.term != deleg_channel(endpoint).term.load(Ordering::Acquire)
    {
        return None;
    }
    entry.used.store(true, Ordering::Relaxed);
    entry.inflight.fetch_add(1, Ordering::AcqRel);
    // Re-check the state AFTER registering: a recall that revoked between
    // the check and the register would otherwise miss this serve in its
    // drain.
    if entry.state.load(Ordering::Acquire) != DELEG_LIVE {
        entry.inflight.fetch_sub(1, Ordering::AcqRel);
        entry.drain.notify_waiters();
        return None;
    }
    Some(DelegServeGuard { entry })
}

/// Why a set of delegations is being revoked (the counter it lands on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevokeKind {
    /// A recall frame from the owner's recall channel.
    Recall,
    /// A revocation riding the holder's own mutation reply (the
    /// self-conflict surrender's client half).
    Reply,
}

/// Revoke the named delegations: tombstone at the fence, stop new serves,
/// WAIT for in-flight serves to drain (the never-serve-after-ack law:
/// the caller queues the ack only after this returns), drop the entries,
/// and push the kernel invalidation for each (the TTL-stretch bound).
pub(crate) async fn revoke_delegations(
    endpoint: &str,
    inos: &[u64],
    fence_term: u64,
    fence_seq: u64,
    kind: RevokeKind,
) {
    // Rung 13 — **recall-forces-flush** (OQ-2's resolved form): a recall
    // naming a held UPDATE authority flushes its intent batch BEFORE the
    // drain/ack below, so the recaller's conflicting serve or mutation
    // orders strictly after every locally-acked intent (the O_EXCL
    // exactly-one-ack ordering proof). One relaxed load when no authority
    // exists.
    super::intents::revoke_update_authorities(endpoint, inos).await;
    revoke_delegations_no_intents(endpoint, inos, fence_term, fence_seq, kind).await;
}

/// [`revoke_delegations`] WITHOUT the intents hook — the arm the intent
/// flush's own reply-revoke absorption calls (a flush's reply must never
/// recurse into another flush; the apply's gate keeps the flusher's
/// UPDATE grants, so these revokes only ever name LOOKUP-class entries).
pub(crate) async fn revoke_delegations_no_intents(
    endpoint: &str,
    inos: &[u64],
    fence_term: u64,
    fence_seq: u64,
    kind: RevokeKind,
) {
    let t0 = Instant::now();
    for &ino in inos {
        // The fence floor: max-merge (a later recall never lowers it).
        match DELEG_TOMBSTONES.entry_sync(ino) {
            scc::hash_map::Entry::Occupied(mut o) => {
                let v = o.get_mut();
                if (fence_term, fence_seq) > *v {
                    *v = (fence_term, fence_seq);
                }
            }
            scc::hash_map::Entry::Vacant(v) => {
                v.insert_entry((fence_term, fence_seq));
            }
        }
        let Some(entry) = DELEG_CACHE.read_sync(&ino, |_, e| Arc::clone(e)) else {
            continue;
        };
        if entry.endpoint.as_ref() != endpoint {
            continue;
        }
        entry.state.store(DELEG_REVOKED, Ordering::Release);
        match kind {
            RevokeKind::Recall => DELEG_RECALLS.fetch_add(1, Ordering::Relaxed),
            RevokeKind::Reply => DELEG_REPLY_REVOKES.fetch_add(1, Ordering::Relaxed),
        };
        // Drain: no new serve begins (the state gate), and every
        // completing serve notifies.
        loop {
            let notified = entry.drain.notified();
            if entry.inflight.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
        entry.state.store(DELEG_ACKED, Ordering::Release);
        let _ = DELEG_CACHE.remove_sync(&ino);
        fire_inval_sink(ino);
    }
    super::deleg_phase_record(super::DelegPhase::HolderDrain, t0);
}

/// The channel died (transport error, listener restart): every entry from
/// that owner stops serving NOW and is dropped; the returned inos are the
/// re-assertion set the channel presents on reconnect (KD-MW-5's NFSv4
/// law). In-flight serves complete — they began inside the freshness
/// window — and their guards' drop sees `REVOKED`, never `ACKED` (there
/// is no ack to order against).
pub(crate) fn suspend_owner_delegations(endpoint: &str) -> Vec<u64> {
    let mut suspended = Vec::new();
    DELEG_CACHE.retain_sync(|ino, e| {
        if e.endpoint.as_ref() == endpoint {
            e.state.store(DELEG_REVOKED, Ordering::Release);
            suspended.push(*ino);
            false
        } else {
            true
        }
    });
    if !suspended.is_empty() {
        DELEG_CHANNEL_SUSPENDS.fetch_add(1, Ordering::Relaxed);
        for &ino in &suspended {
            fire_inval_sink(ino);
        }
    }
    deleg_channel_mark_down(endpoint);
    suspended
}

/// Drop the named delegations WITHOUT counting them as recalls or reply
/// revokes: the re-assertion answer's "gone" arm (the successor did not
/// re-admit them — NFSv4's un-reasserted-is-gone law). Serves stop
/// immediately; in-flight ones complete under the suspension semantics
/// (no ack to order against).
pub(crate) fn drop_delegations(endpoint: &str, inos: &[u64]) {
    for &ino in inos {
        let Some(entry) = DELEG_CACHE.read_sync(&ino, |_, e| Arc::clone(e)) else {
            continue;
        };
        if entry.endpoint.as_ref() != endpoint {
            continue;
        }
        entry.state.store(DELEG_REVOKED, Ordering::Release);
        let _ = DELEG_CACHE.remove_sync(&ino);
        fire_inval_sink(ino);
    }
}

/// The live entries held against `endpoint` — the re-assertion set's
/// still-held half.
pub(crate) fn held_delegation_inos(endpoint: &str) -> Vec<u64> {
    let mut inos = Vec::new();
    DELEG_CACHE.retain_sync(|ino, e| {
        if e.endpoint.as_ref() == endpoint && e.state.load(Ordering::Acquire) == DELEG_LIVE {
            inos.push(*ino);
        }
        true
    });
    inos
}

/// Absorb a re-assertion reply: entries NOT re-admitted are gone (the
/// NFSv4 law), re-admitted ones re-install with the successor's fresh
/// stamps and era. The tombstone floors from the PREDECESSOR's era are
/// cleared for re-admitted objects — the successor's sequence space is its
/// own.
pub(crate) fn absorb_reassert_reply(endpoint: &str, reply: &super::wire::DelegReassertReply) {
    for g in &reply.grants {
        let _ = DELEG_TOMBSTONES.remove_sync(&g.ino);
        install_delegation(endpoint, g);
    }
    // The round itself is channel evidence: adopt the successor's era and
    // refresh the freshness clock (park stays whatever the channel knew —
    // the next poll reply republishes the owner's derivation).
    let ch = deleg_channel(endpoint);
    let park = ch.park_ms.load(Ordering::Relaxed);
    deleg_channel_mark_ok(endpoint, park, reply.owner_term);
    note_deleg_reassert_round();
}

/// The kernel-TTL stretch (design §8.2: "kernel TTLs under a delegation
/// stretch"): the remaining serve-validity horizon of a live, era-current,
/// channel-fresh delegation — the recall (which pushes the invalidation
/// sink) is what bounds the stretched staleness. `None` = no stretch (the
/// per-class default governs). ATTR face only in this rung; the entry
/// face needs per-name tracking and belongs to rung 13's per-directory
/// machinery (stated in the rung-12 evidence note).
pub fn deleg_kernel_ttl_stretch(ino: u64) -> Option<Duration> {
    if !delegation_enabled() {
        return None;
    }
    let entry = DELEG_CACHE.read_sync(&ino, |_, e| Arc::clone(e))?;
    if entry.state.load(Ordering::Acquire) != DELEG_LIVE {
        return None;
    }
    let ch = DELEG_CHANNELS.read_sync(entry.endpoint.as_ref(), |_, v| Arc::clone(v))?;
    if !ch.healthy.load(Ordering::Acquire) || entry.term != ch.term.load(Ordering::Acquire) {
        return None;
    }
    let window = ch.park_ms.load(Ordering::Relaxed).saturating_mul(2) + DELEG_FRESH_SLACK_MS;
    let age = now_ms().saturating_sub(ch.last_ok_ms.load(Ordering::Acquire));
    let remaining = window.saturating_sub(age);
    if remaining == 0 {
        None
    } else {
        Some(Duration::from_millis(remaining))
    }
}

/// **Test seam**: drop every delegation, tombstone and channel record so a
/// suite can exercise the cold arms deliberately. Production never calls
/// it.
pub fn test_clear_delegations() {
    DELEG_CACHE.retain_sync(|_, _| false);
    DELEG_TOMBSTONES.retain_sync(|_, _| false);
    DELEG_CHANNELS.retain_sync(|_, _| false);
}

/// The `dlm_delegation` family snapshot (design §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DelegationStats {
    /// Owner face: grants issued onto replies.
    pub grants: u64,
    /// Owner face: grants declined (in-flight mutation, vanished object).
    pub declines: u64,
    /// Client face: grants installed into the cache.
    pub installs: u64,
    /// Client face: delegated local serves (one per verb).
    pub hits: u64,
    /// Client face: delegations dropped by wire recalls.
    pub recalls: u64,
    /// Client face: re-assertion rounds completed.
    pub reasserts: u64,
    /// Client face: delegations dropped by reply-ridden revocations (the
    /// self-conflict surrender's client half).
    pub reply_revokes: u64,
    /// **Must stay 0**: a serve completed after its recall was acked.
    pub stale_serves: u64,
    /// Owner face, LOUD: delegation recalls declared dead at the deadline
    /// (each escalates toward membership eviction).
    pub recall_timeouts: u64,
    /// Owner face: timed-out holders actually evicted from an armed
    /// membership plane (≤ recall_timeouts; the plane may be off).
    pub evictions: u64,
    /// Client face: in-flight grants dropped at the reordering fence.
    pub tombstone_drops: u64,
    /// Client face: channel failures that suspended an owner's entries.
    pub channel_suspends: u64,
    /// Client face: completed recall-channel rounds.
    pub channel_rounds: u64,
    /// GAUGE: cached delegations.
    pub entries: u64,
    /// GAUGE: their byte charge (the R5 component's reading).
    pub bytes: u64,
}

/// Read the family.
pub fn delegation_stats() -> DelegationStats {
    let entries = DELEG_CACHE.len() as u64;
    DelegationStats {
        grants: DELEG_GRANTS_ISSUED.load(Ordering::Relaxed),
        declines: DELEG_DECLINES.load(Ordering::Relaxed),
        installs: DELEG_INSTALLS.load(Ordering::Relaxed),
        hits: DELEG_HITS.load(Ordering::Relaxed),
        recalls: DELEG_RECALLS.load(Ordering::Relaxed),
        reasserts: DELEG_REASSERTS.load(Ordering::Relaxed),
        reply_revokes: DELEG_REPLY_REVOKES.load(Ordering::Relaxed),
        stale_serves: DELEG_STALE_SERVES.load(Ordering::Relaxed),
        recall_timeouts: DELEG_RECALL_TIMEOUTS.load(Ordering::Relaxed),
        evictions: DELEG_EVICTIONS.load(Ordering::Relaxed),
        tombstone_drops: DELEG_TOMBSTONE_DROPS.load(Ordering::Relaxed),
        channel_suspends: DELEG_CHANNEL_SUSPENDS.load(Ordering::Relaxed),
        channel_rounds: DELEG_CHANNEL_ROUNDS.load(Ordering::Relaxed),
        entries,
        bytes: entries * DELEG_ENTRY_BYTES,
    }
}

/// The `dlm_delegation` stats-inode object (design §13's spellings; the
/// extra engagement gauges are additive).
pub fn delegation_stats_json() -> serde_json::Value {
    let s = delegation_stats();
    serde_json::json!({
        "dlm_delegation_grants": s.grants,
        "dlm_delegation_declines": s.declines,
        "dlm_delegation_installs": s.installs,
        "dlm_delegation_hits": s.hits,
        "dlm_delegation_recalls": s.recalls,
        "dlm_delegation_reasserts": s.reasserts,
        "dlm_delegation_reply_revokes": s.reply_revokes,
        "dlm_delegation_stale_serves": s.stale_serves,
        "dlm_delegation_recall_timeouts": s.recall_timeouts,
        "dlm_delegation_evictions": s.evictions,
        "dlm_delegation_tombstone_drops": s.tombstone_drops,
        "dlm_delegation_channel_suspends": s.channel_suspends,
        "dlm_delegation_channel_rounds": s.channel_rounds,
        "dlm_delegation_entries": s.entries,
        "dlm_delegation_bytes": s.bytes,
    })
}
