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
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

struct Entry {
    token: AtomicU64,
    term: AtomicU64,
    /// Second-chance reference bit: set on every serve, cleared by a
    /// sweep, and its clearing is what earns the entry one more pass.
    used: AtomicBool,
}

static CACHE: Lazy<scc::HashMap<u64, Entry>> = Lazy::new(scc::HashMap::new);

/// The owner era the cache last learned — a miss's floor, and the reason a
/// miss degrades exactly as a fresh mount does rather than answering 0 on
/// a volume whose records name eras.
static OWNER_TERM: AtomicU64 = AtomicU64::new(0);

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static EVICTIONS: AtomicU64 = AtomicU64::new(0);
static GRANTS: AtomicU64 = AtomicU64::new(0);

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
    OWNER_TERM.fetch_max(term, Ordering::AcqRel);
}

/// The era the cache serves a miss in.
pub fn owner_term() -> u64 {
    OWNER_TERM.load(Ordering::Acquire)
}

/// Record a grant an owner sent us. Monotone per object: a reordered or
/// replayed reply can never lower an object's generation.
pub fn record_grant(grant: &TokenGrant) {
    record_owner_term(grant.term);
    GRANTS.fetch_add(1, Ordering::Relaxed);
    let updated = CACHE.read_sync(&grant.ino, |_, e| {
        e.token.fetch_max(grant.token, Ordering::AcqRel);
        e.term.fetch_max(grant.term, Ordering::AcqRel);
        e.used.store(true, Ordering::Relaxed);
    });
    if updated.is_some() {
        return;
    }
    if CACHE.len() >= cache_cap() {
        sweep();
    }
    let _ = CACHE.insert_sync(
        grant.ino,
        Entry {
            token: AtomicU64::new(grant.token),
            term: AtomicU64::new(grant.term),
            used: AtomicBool::new(true),
        },
    );
}

/// One bounded second-chance pass: entries used since the last sweep are
/// kept (and their bit cleared), the rest retire.
fn sweep() {
    let mut retired = 0u64;
    CACHE.retain_sync(|_, e| {
        if e.used.swap(false, Ordering::AcqRel) {
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
    if let Some(token) = CACHE.read_sync(&ino, |_, e| {
        e.used.store(true, Ordering::Relaxed);
        e.token.load(Ordering::Acquire)
    }) {
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
    pub bytes: u64,
    pub cap_entries: u64,
    pub owner_term: u64,
}

/// Read the cache's counters.
pub fn token_cache_stats() -> TokenCacheStats {
    let entries = CACHE.len() as u64;
    TokenCacheStats {
        hits: HITS.load(Ordering::Relaxed),
        misses: MISSES.load(Ordering::Relaxed),
        evictions: EVICTIONS.load(Ordering::Relaxed),
        grants: GRANTS.load(Ordering::Relaxed),
        entries,
        bytes: entries * ENTRY_BYTES,
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
