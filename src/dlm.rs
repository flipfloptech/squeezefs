//! The cluster lock manager — process-local backend (DLM stages S0–S2,
//! S11's single-node half; `docs/pre-rc-engineering-spec.md` §6).
//!
//! **Custody model.** One `LOCK_MAP` entry per FILE identity
//! ([`ObjectKey`]) holds that file's live grants (`FileCustody`): the
//! whole-inode slots plus a sorted interval list of byte-range grants.
//! Whole-inode custody conflicts with every span; two spans conflict iff
//! they overlap AND their [`LockMode`]s are incompatible. So **disjoint
//! byte ranges of one file proceed in parallel** — execution-plan ruling
//! D8's target ("a file especially a large one could be getting
//! read/written to different blocks by different applications and want
//! locks on them") for applications sharing ONE mount, and the arbitration
//! S11 later ships remotely. There is deliberately **no wire, no remote
//! grant and no revocation here**: that is S3/S4/S9.
//!
//! **Fencing (the decision, spec §6.7 decision 4 + the S1/S2 census).**
//! Byte ranges keep **sharing the FILE's generator**: one global mint
//! (`GRANT_SEQ`, composed with the durable term), every grant —
//! whole-file or range — raises the file's readable generation and its
//! stripe floor. That is what keeps the ~24 fencing-read sites correct
//! without teaching any of them about spans: `presented < current` fences
//! still reject genuinely superseded stamps (a crashed writer's
//! range-stamped staged/extent records included), `==` coherence memos
//! still miss-and-refetch, and `.max()` folds stay order-safe. The
//! corollary, which S11's remote half must carry: once **two or more**
//! grants are live on one file, the earlier holder's token is no longer
//! the file's newest, so a range writer must fence on its own lease
//! snapshot (or a per-span currency read) — never on the file-wide read.
//! Today no product verb issues a range lease, so no live fence site is
//! affected; [`span_range_shared`] is the one production consumer of span
//! custody (W1's seventh ineligibility clause).

use crate::error::Result;
use crate::stripe_locks::StripeLocks;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use xxhash_rust::xxh3::xxh3_64;

/// Typed lock/fencing object key — the **file identity**.
///
/// The hot path (`inode_{N}` objects) is pure binary — no `format!`, no
/// digit parsing on reads, no heap allocation, integer hashing. String
/// forms exist only at the API boundary (callers pass `&str` paths) and
/// for the rare non-inode object; a networked DLM backend would render
/// these to wire bytes at the transport edge, never on the local path.
///
/// **Byte ranges are deliberately NOT part of this key** (S11 — spec
/// §6.9): one map entry per FILE carries the whole-file slot *and* the
/// file's live byte-range grants (`FileCustody`), so every
/// whole-file/range and range/range conflict is decided in one place under
/// one entry lock. Pre-S11 the span rode the key, which made each distinct
/// span a distinct lock **object**: overlapping spans both granted, and a
/// range was granted straight through a held whole-file lease.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ObjectKey {
    /// Lock/fencing identity of `inode_{0}`.
    Ino(u64),
    /// Lock/fencing identity of a non-inode path (rare).
    Path(Box<str>),
}

impl ObjectKey {
    /// Parse a caller path into its binary form without allocating for the
    /// `inode_{N}` fast path.
    fn from_path(file_path: &str) -> Self {
        match ino_of_path(file_path) {
            Some(ino) => Self::Ino(ino),
            None => Self::Path(file_path.into()),
        }
    }

    /// Stripe selector for the waiter-notify array. Keyed on the FILE
    /// identity, so a release on ANY span of a file wakes every waiter on
    /// that file (whole-file waiters included — they conflict with every
    /// span). Collisions are benign (spurious wakeups re-check and
    /// re-wait); correctness never depends on this hash.
    fn stripe_seed(&self) -> u64 {
        match self {
            Self::Ino(i) => *i,
            Self::Path(p) => xxh3_64(p.as_bytes()),
        }
    }
}

/// Lock mode (spec §6.7 "Lock modes": *"Four modes plus capability bits
/// (NL / CR / CW / EX with LOOKUP, UPDATE, PERM, LAYOUT, XATTR, DATA
/// bits) covers every shape in this filesystem … CW should ship disabled
/// until a verb issues it, per the no-dead-code rule"*).
///
/// **Shipped subset, and why it is a subset.** The no-dead-code law
/// admits a public surface the stage above needs; it does not admit modes
/// with no issuer, no observable semantic and no test:
///
/// | Mode | State | Issuer |
/// |------|-------|--------|
/// | **EX** — exclusive | shipped, default | every production acquire (`acquire_lock`) |
/// | **CW** — concurrent write | **shipped DISABLED** ([`test_arm_cw_mode`]) | none yet — S9/S11 issue it; §6.7 requires the mode to exist and to be unreachable until then |
/// | *CR* — concurrent read | absent | nothing takes a READ lease (§6.2 census: readers take no leases at all). It lands with **S5** read-only coherent mounts, which is the verb that gives it meaning |
/// | *NL* — null | absent | NL exists to park a resource handle across a *conversion*; this manager has no conversion verb and no client-side handle to park, so an NL variant would be unconstructible-and-unobservable. It lands with **S9**'s conversion protocol |
/// | capability bits | absent | LOOKUP/UPDATE/PERM/XATTR partition *metadata* locks, and metadata ops take no cluster lease today (§6.2). They land with **S8** function-shipped metadata, whose verbs are the issuers |
///
/// The compatibility matrix over the shipped modes:
///
/// ```text
///        EX   CW
///   EX    N    N
///   CW    N    Y
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockMode {
    /// EX — exclusive custody of the span: compatible with nothing.
    Exclusive,
    /// CW — concurrent write: two CW holders may cover the same span and
    /// coordinate at a finer grain themselves (the classic DLM semantic).
    /// **Ships disabled** — [`LocalLockManager::acquire_lock_mode`]
    /// refuses it until [`test_arm_cw_mode`] arms it.
    ConcurrentWrite,
}

impl LockMode {
    /// The §6.7 compatibility matrix: `true` ⇔ two grants in these modes
    /// may cover overlapping bytes at the same time.
    pub const fn compatible_with(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::ConcurrentWrite, Self::ConcurrentWrite)
        )
    }
}

/// One live grant in a file's custody table: the span it protects, its
/// owner, its mode, and the fencing token minted at grant. The token is
/// globally unique (S1's single mint), which makes it the grant's exact
/// removal identity — two grants of the same span by the same client
/// (possible only under compatible modes) can never be confused.
#[derive(Clone, Copy, Debug)]
struct Grant {
    start: u64,
    end: u64,
    owner_nonce: u64,
    token: u64,
    mode: LockMode,
}

impl Grant {
    #[inline]
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.end > start && self.start < end
    }

    #[inline]
    fn covers(&self, start: u64, end: u64) -> bool {
        self.start <= start && self.end >= end
    }

    #[inline]
    fn len(&self) -> u64 {
        self.end - self.start
    }
}

/// A file's custody: the whole-file slot plus the live byte-range grants,
/// as a **sorted interval list** (S11 — spec §6.9).
///
/// **Why a sorted `Vec` and not a tree.** Acquisition is per
/// *open-for-write episode*, not per op (`get_or_acquire_lease` caches in
/// `active_leases` — §6.2), so this structure is not on the op hot path;
/// what it must be is cheap at the population it actually sees and free
/// for the whole-file path that ships today. The population bound is
/// **live grants on ONE file** = concurrently held leases on it (entries
/// are retired at release, never accumulated per span ever locked) —
/// one per writing application/rank, so tens, not millions. At that size
/// a contiguous 40-byte-element array beats every pointer structure on
/// constants, and it costs ZERO when empty (`Vec::new` does not
/// allocate), which is what keeps the shipped whole-file path unchanged.
///
/// Costs:
///
/// | Operation | Cost |
/// |---|---|
/// | whole-file EX acquire / release (the shipped path) | O(1) — two `is_empty` probes |
/// | range acquire (conflict probe) | O(log n + c), `c` = grants in the stab window |
/// | range acquire (admit) | O(n) memmove |
/// | range release | O(log n) locate + O(n) memmove (+ O(n) only when the widest grant leaves) |
/// | fencing read (`read_identity`) | O(1) — `max_token` |
/// | [`span_range_shared`] (the W1 clause) | O(log n + c) |
///
/// The stab window: any grant overlapping `[s,e)` has `start < e` and
/// `start + len > s`, and `len ≤ widest`, so the candidates are exactly
/// the grants with `start ∈ (s − widest, e)` — two `partition_point`s.
/// `widest` is a monotone over-approximation on admit (max) and exact on
/// release (recomputed only when the widest grant is the one leaving), so
/// it can never shrink below a live grant's length and can never miss a
/// conflict.
struct FileCustody {
    /// Live `range: None` grants — whole-inode custody. A list, not an
    /// `Option`: under a compatible mode (CW) two whole-file grants may
    /// coexist, and silently overwriting one would strand its release.
    /// EX — every shipped acquire — keeps this at most one deep, and the
    /// fast path only ever asks `is_empty()`.
    wholes: Vec<Grant>,
    /// Live byte-range grants, sorted by `(start, token)`.
    ranges: Vec<Grant>,
    /// `max(end − start)` over `ranges` — the stab-window bound.
    widest: u64,
    /// The newest fencing token granted while this entry has been live:
    /// the object's exact generation read (see [`read_identity`]).
    max_token: u64,
}

impl FileCustody {
    /// A fresh entry holding exactly `grant`.
    fn opened(span: Option<(u64, u64)>, grant: Grant) -> Self {
        let mut custody = Self {
            wholes: Vec::new(),
            ranges: Vec::new(),
            widest: 0,
            max_token: 0,
        };
        custody.admit(span, grant);
        custody
    }

    /// The grants that could overlap `[start, end)` — the stab window.
    fn candidates(&self, start: u64, end: u64) -> &[Grant] {
        if self.ranges.is_empty() {
            return &[];
        }
        let lo = self
            .ranges
            .partition_point(|g| g.start.saturating_add(self.widest) <= start);
        let hi = self.ranges.partition_point(|g| g.start < end);
        &self.ranges[lo..hi.max(lo)]
    }

    /// Would a `span`/`mode` request conflict with what is live?
    fn conflicts(&self, span: Option<(u64, u64)>, mode: LockMode) -> bool {
        // Whole-inode custody covers every span, so it is checked first
        // whatever the request is. EX (the shipped mode) makes this
        // `!is_empty()`.
        if self.wholes.iter().any(|w| !w.mode.compatible_with(mode)) {
            return true;
        }
        match span {
            // A whole-file request covers every span on the file, so any
            // incompatible live range conflicts. EX — every shipped
            // acquire — short-circuits to `is_empty()`: O(1), the
            // pre-S11 cost.
            None => match mode {
                LockMode::Exclusive => !self.ranges.is_empty(),
                _ => self.ranges.iter().any(|g| !g.mode.compatible_with(mode)),
            },
            Some((start, end)) => self
                .candidates(start, end)
                .iter()
                .any(|g| g.overlaps(start, end) && !g.mode.compatible_with(mode)),
        }
    }

    /// Record a granted lock. Callers must have cleared [`Self::conflicts`].
    fn admit(&mut self, span: Option<(u64, u64)>, grant: Grant) {
        self.max_token = self.max_token.max(grant.token);
        match span {
            None => self.wholes.push(grant),
            Some(_) => {
                let at = self
                    .ranges
                    .partition_point(|g| (g.start, g.token) < (grant.start, grant.token));
                self.widest = self.widest.max(grant.len());
                self.ranges.insert(at, grant);
            }
        }
    }

    /// Retire one grant, nonce- and token-conditional (the pre-S11
    /// `remove_if_sync(|held| held.owner_nonce == nonce)` discipline,
    /// strengthened by the globally unique token). `true` ⇔ something was
    /// retired, which is what licenses the waiter wake.
    fn retire(&mut self, span: Option<(u64, u64)>, token: u64, owner_nonce: u64) -> bool {
        let mine = |g: &Grant| g.token == token && g.owner_nonce == owner_nonce;
        match span {
            None => match self.wholes.iter().position(mine) {
                Some(at) => {
                    self.wholes.remove(at);
                    true
                }
                None => false,
            },
            Some((start, _)) => {
                // The list is sorted by `(start, token)` and that pair is
                // exactly this grant's insertion key, so the search lands
                // on it or on a stranger.
                let at = self
                    .ranges
                    .partition_point(|g| (g.start, g.token) < (start, token));
                match self.ranges.get(at) {
                    Some(g) if mine(g) => {
                        let len = g.len();
                        self.ranges.remove(at);
                        if self.ranges.is_empty() {
                            self.widest = 0;
                        } else if len == self.widest {
                            self.widest =
                                self.ranges.iter().map(Grant::len).max().unwrap_or_default();
                        }
                        true
                    }
                    _ => false,
                }
            }
        }
    }

    /// Does this client still hold the grant a lease names?
    fn holds(&self, token: u64, owner_nonce: u64) -> bool {
        self.wholes
            .iter()
            .chain(self.ranges.iter())
            .any(|g| g.token == token && g.owner_nonce == owner_nonce)
    }

    fn is_vacant(&self) -> bool {
        self.wholes.is_empty() && self.ranges.is_empty()
    }

    /// Raise the object's readable generation without taking custody
    /// (the [`test_bump_fencing_generation`] seam).
    fn bump_generation(&mut self, token: u64) {
        self.max_token = self.max_token.max(token);
    }

    /// W1 clause 7's question: is `[start, end)` under byte-range custody
    /// that `holder_token`'s writer does not solely own?
    ///
    /// A live WHOLE-FILE grant is whole-inode custody by definition and is
    /// therefore never range-shared — which is why the clause is inert on
    /// the shipped write path (it takes exactly that lease).
    fn span_is_range_shared(&self, start: u64, end: u64, holder_token: u64) -> bool {
        self.candidates(start, end)
            .iter()
            .any(|g| g.overlaps(start, end) && (g.token != holder_token || !g.covers(start, end)))
    }
}

/// `inode_{N}` → `N` without allocating. Strict: the entire suffix must be
/// ASCII digits that parse into a `u64`, otherwise the path is treated as an
/// opaque string key (never a lossy alias of some inode).
///
/// `pub(crate)` for S4's [`crate::dlm_slot::lock_home_slot`]: lock homing must
/// derive an object's ino by EXACTLY this law, or a key this module treats as
/// opaque could home as some inode (and vice versa).
pub(crate) fn ino_of_path(path: &str) -> Option<u64> {
    let digits = path.strip_prefix("inode_")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One acquire attempt's mint outcome: granted (token), the span is held
/// incompatibly, or the era's grant space is exhausted (loud refusal — no
/// entry is inserted, no lock is taken).
enum Mint {
    Granted(u64),
    Held,
    Exhausted(u64),
}

/// Mint one grant sequence. `Err(seq)` = the era's 40-bit grant budget is
/// spent; nothing may be granted (a carry into the term field would forge
/// a newer era).
fn mint_token() -> std::result::Result<u64, u64> {
    // S1 mint: one global fetch_add — strictly monotone in grant order,
    // globally unique, gap-carrying. S2: composed with the durable era,
    // so the sequence is monotone ACROSS processes too.
    let seq = GRANT_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
    if seq > GRANT_SEQ_MAX {
        Err(seq)
    } else {
        Ok(compose_token(durable_term(), seq))
    }
}

/// Lock table: **file identity → live custody** ([`FileCustody`]: the
/// whole-file slot plus the sorted live byte-range grants). Grants are
/// retired at release (nonce- and token-conditional) and the entry is
/// dropped when the last one leaves, so this map is bounded by
/// CONCURRENTLY HELD leases — never by objects or spans ever locked.
static LOCK_MAP: Lazy<scc::HashMap<ObjectKey, FileCustody>> = Lazy::new(scc::HashMap::new);

/// §6.7: CW ships DISABLED until a verb issues it. Armed only by
/// [`test_arm_cw_mode`]; production never writes this word.
static CW_ENABLED: AtomicBool = AtomicBool::new(false);

fn cw_enabled() -> bool {
    CW_ENABLED.load(Ordering::Relaxed)
}

/// **Test seam** (the [`test_swap_grant_seq`] precedent): arm/disarm
/// [`LockMode::ConcurrentWrite`], returning the previous state.
///
/// §6.7 requires CW to EXIST so S9/S11 can issue it and to be
/// unreachable until one does. This seam is that reachability: the mode,
/// its matrix row and its arbitration are exercised by
/// `tests/dlm_range_custody_tests.rs`, and every product path takes the
/// refusal. Production never calls it.
pub fn test_arm_cw_mode(enabled: bool) -> bool {
    CW_ENABLED.swap(enabled, Ordering::AcqRel)
}

/// **Test seam**: advance an object's fencing generation WITHOUT taking
/// custody — mint one grant sequence, publish it to the object's read
/// surfaces (the stripe floor plus any live entry's generation), return it.
///
/// This is the transient-bump shape the router/write-path suites need
/// (FIND-M11-A): the daemon's cached whole-file lease must stay HELD while
/// its token goes stale underneath it. Pre-S11 those suites did it by
/// acquiring a byte-RANGE lock on the same ino — which is now, correctly,
/// a conflict with the held whole-file lease. Production never calls this;
/// the product-facing verb is S9's revoke/epoch bump.
pub fn test_bump_fencing_generation(file_path: &str) -> u64 {
    let key = ObjectKey::from_path(file_path);
    let Ok(token) = mint_token() else {
        return read_identity(&key);
    };
    grant_floor(&key).fetch_max(token, Ordering::AcqRel);
    let _ = LOCK_MAP.update_sync(&key, |_, custody| custody.bump_generation(token));
    token
}

/// W1 **clause 7**'s custody source (spec §6.7: *"the W1 patch predicate
/// requires whole-inode exclusive custody, so range-shared custody needs a
/// seventh clause in the existing decision ledger"*; §6.3's W1 paragraph
/// is the cross-node face of the same hazard).
///
/// `true` ⇔ a live byte-range grant overlaps `[start, end)` that
/// `holder_token`'s writer does not solely own — either a foreign grant,
/// or its own grant not covering the whole span. Then whole-inode
/// exclusive custody does not hold and an in-place mutation of that span
/// would touch bytes whose custody belongs to someone else.
///
/// `false` for the shipped shapes: no custody at all, or a live
/// whole-file lease (which IS whole-inode custody — the write path's own
/// `get_or_acquire_lease`), or a range grant that does not overlap.
pub fn span_range_shared(ino: u64, start: u64, end: u64, holder_token: u64) -> bool {
    if start >= end {
        return false;
    }
    LOCK_MAP
        .read_sync(&ObjectKey::Ino(ino), |_, custody| {
            custody.span_is_range_shared(start, end, holder_token)
        })
        .unwrap_or(false)
}

/// S1 (spec §6.7 decision 4): the SINGLE per-process fencing mint. Global
/// monotonicity implies per-object monotonicity, and every consumer
/// comparison is `<`, `==` or `.max()` — monotone-safe under globally
/// unique, gap-carrying tokens. Replaces the per-object `FENCING_MAP`
/// (RES-2: one immortal `Arc<AtomicU64>` per object ever locked, ~105 B
/// each, no removal path) with O(1) state. S2 composes it into
/// `(term << GRANT_SEQ_BITS) | grant_seq` for remount monotonicity.
static GRANT_SEQ: AtomicU64 = AtomicU64::new(0);

/// S2 (spec §6.7 decision 4): width of the grant field in a composed
/// fencing token — `token = (term << 40) | grant_seq`.
pub const GRANT_SEQ_BITS: u32 = 40;

/// The largest grant sequence a term can issue (40 bits). Past it the
/// mint REFUSES ([`LocalLockManager::acquire_lock`] errors loud): a
/// carry into the term field would forge a future era.
pub const GRANT_SEQ_MAX: u64 = (1u64 << GRANT_SEQ_BITS) - 1;

/// The largest writer term (24 bits). Past it the MOUNT refuses (the D0
/// gate, `KvMetaBackend::writer_guard_gate`): a term rollover would
/// alias a live era's tokens with a retired one's.
pub const TERM_MAX: u64 = (1u64 << (64 - GRANT_SEQ_BITS)) - 1;

/// S2: the process's adopted **durable writer term** — the high-order
/// component of every token this process mints. Published by the D0
/// mount gate AFTER the claim barrier (`crate::dlm::adopt_durable_term`),
/// so no token can ever name an era that is not on disk; a multi-volume
/// mount publishes each volume's term and the max wins (every write
/// mount claims every volume in the set, so the max is itself
/// remount-monotone).
///
/// **0 = no durable term** — an un-stamped volume (no incompat bit 7),
/// an offline tool that holds no claim, or a pure in-RAM test. Then
/// `(0 << 40) | grant_seq == grant_seq`: tokens are byte-identical to
/// S1's and every behavior is the pre-S2 behavior.
static DURABLE_TERM: AtomicU64 = AtomicU64::new(0);

/// Compose a fencing token from its durable era and its grant sequence.
pub const fn compose_token(term: u64, grant_seq: u64) -> u64 {
    (term << GRANT_SEQ_BITS) | grant_seq
}

/// The durable era a token was minted in (0 = an un-stamped volume's
/// era-less token).
pub const fn token_term(token: u64) -> u64 {
    token >> GRANT_SEQ_BITS
}

/// The grant sequence inside a token's era.
pub const fn token_grant_seq(token: u64) -> u64 {
    token & GRANT_SEQ_MAX
}

/// This process's adopted durable term (0 = none: an un-stamped volume, an offline tool holding no claim, or a pure in-RAM test).
pub fn durable_term() -> u64 {
    DURABLE_TERM.load(Ordering::Acquire)
}

/// The current era's floor: the value an object with no grant in this
/// era reads. Consumers that must distinguish "a real grant happened"
/// from "the era moved" compare against this (the mount-time extent
/// sweep does exactly that — `SqueezefsFilesystem::recover_extent_records`).
pub fn term_base() -> u64 {
    compose_token(durable_term(), 0)
}

/// Adopt a durable term published by the D0 mount gate (monotone
/// `fetch_max` — never regresses, so a stale/second volume cannot lower
/// the era). Returns the process term after adoption.
///
/// Call only AFTER the term is durable and barriered: a token naming an
/// era that a crash could lose would be the §6.11 inversion in reverse.
pub fn adopt_durable_term(term: u64) -> u64 {
    let prev = DURABLE_TERM.fetch_max(term, Ordering::AcqRel);
    if term > prev {
        log::info!(
            "fencing: durable writer term {term} adopted (was {prev}); every token this \
             process mints now dominates every earlier era's (spec §6.7 decision 4)"
        );
        term
    } else {
        prev
    }
}

/// **Test seam** (the `test_conveyor_hold_release` precedent): swap the
/// process mint's grant counter, returning the previous value. Exists
/// for the 40-bit exhaustion contract, which cannot be reached by
/// minting; production code never calls it. Callers restore the saved
/// value — the mint is process-global.
pub fn test_swap_grant_seq(value: u64) -> u64 {
    GRANT_SEQ.swap(value, Ordering::AcqRel)
}

/// Per-stripe RELEASED-generation floors, fetch_max'd at every mint with
/// the granted token (keyed by the FILE identity's stripe). An UNHELD
/// object's generation read serves from its stripe floor: never below
/// the identity's own newest grant (the mint bumped it — the property
/// the stale-reject `presented < current` arm and the recovery sweep
/// need), possibly above it via a stripe-mate's later mint (a monotone
/// over-approximation: harmless for `<` fences — the presenter re-reads
/// and converges, the FIND-M11-A transient class — and for the `==`
/// coherence memos, which miss and refetch). HELD objects never read the
/// floor: their entry token is exact, so live writers cannot be
/// spuriously fenced by stripe collisions. 1024 stripes = the existing
/// waiter-stripe constant below (fixed structural fan-out, 8 KiB total —
/// the whole point of S1 is O(1) fencing state).
static LAST_GRANT_FLOOR: Lazy<Vec<AtomicU64>> =
    Lazy::new(|| (0..1024).map(|_| AtomicU64::new(0)).collect());

fn grant_floor(identity: &ObjectKey) -> &'static AtomicU64 {
    &LAST_GRANT_FLOOR[(identity.stripe_seed() % 1024) as usize]
}

/// The identity's readable generation — the surface the ~24-site fencing
/// census reads (`presented < current` fences, `==` coherence memos,
/// `.max()` folds; every one of them names a FILE identity).
///
/// * **Any live custody** ⇒ the entry's `max_token`: the FILE's newest
///   grant. With a whole-file lease held that is the holder's own token
///   **exactly** (S1's live-writer property, byte-identical: a live
///   whole-file writer can never be fenced by a stripe collision or a
///   sibling's mint). This is also where **byte ranges keep sharing the
///   file's generator** (S11 decision, unchanged from S1 and S2): a range
///   grant raises the file's generation, so a crashed writer's
///   range-stamped staged/extent records are still superseded by the next
///   grant, and no census site needs to learn about spans. The corollary,
///   documented for S11: once **two or more** range grants are live, the
///   earlier holder's own token is no longer the file's newest, so a range
///   writer must fence on its lease snapshot / a per-span currency read —
///   never on this file-wide surface.
/// * **No live custody** ⇒ the stripe floor raised to the current era's
///   base (≥ the identity's own newest grant, because every mint
///   `fetch_max`es its floor before the grant is published; possibly
///   above it via a stripe-mate — the documented monotone
///   over-approximation). A never-locked identity reads [`term_base`]:
///   S1's plain 0 with no durable term, the CURRENT era's floor with one
///   (S2 — what makes every pre-crash stamp stale in a fresh process,
///   spec §6.11).
///
/// The read never regresses: the floor covers every token ever granted on
/// the identity, so retiring the last grant cannot lower it.
fn read_identity(identity: &ObjectKey) -> u64 {
    LOCK_MAP
        .read_sync(identity, |_, custody| custody.max_token)
        .unwrap_or_else(|| {
            grant_floor(identity)
                .load(Ordering::Acquire)
                .max(term_base())
        })
}
/// Per-stripe release notifications. A release wakes only its own stripe —
/// never every waiter in the process (the old single global `Notify` was a
/// thundering herd and let unrelated churn burn waiters' retry budgets).
static LOCK_WAITERS: Lazy<StripeLocks<tokio::sync::Notify, 1024>> = Lazy::new(StripeLocks::new);

static CLIENT_NONCE: AtomicU64 = AtomicU64::new(1);

/// The cluster lock-manager surface (DLM stage S0 —
/// docs/pre-rc-engineering-spec.md §6.9): exactly the operations the
/// production call sites use (§6.2 census: three `acquire_lock` sites,
/// one lease site, ~24 fencing reads). `LocalLockManager` is the
/// process-local backend; later stages add remote implementations
/// (S4 slot lock manager) behind this same trait. Deliberately NOT
/// dyn-compatible (RPITIT futures): stage consumers select statically.
pub trait LockManager: Clone + Send + Sync + 'static {
    /// Acquire an exclusive lease on `file_path` (optionally a byte
    /// range), waiting up to `ttl` for the current holder to release.
    fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> impl std::future::Future<Output = Result<LockLease>> + Send;
    /// Current fencing generation for a path-form object key.
    fn get_fencing_token(&self, file_path: &str) -> u64;
    /// Current fencing generation for an inode object.
    fn get_fencing_token_ino(&self, ino: u64) -> u64;
}

/// Process-local lock manager: the S0 extraction of the historical
/// `DlmClient` body, byte-identical semantics (same maps, same wait
/// protocol, same fencing mint/read). The vestigial redis/meta mock
/// family (`MetaClient`, `MetaConnection`, `BoundConnection`,
/// `MockPubSub`, `MockMessageStream`, `MockMessage`, `redis_url`) was
/// deleted with it — spec §6.1, zero callers.
#[derive(Clone)]
pub struct LocalLockManager {
    client_nonce: u64,
}

/// The product-facing handle name — since **S4** the slot-homed lock
/// authority ([`crate::dlm_slot::SlotLockManager`]), which homes every
/// acquire on the durable meta slot map, asks the ownership plane whether
/// this node owns that home, and — in solo mode, always — runs the
/// unchanged [`LocalLockManager`] acquire. Call sites keep the historical
/// name, so homing + ownership are universal without a call-site edit.
pub type DlmClient = crate::dlm_slot::SlotLockManager;

impl LockManager for LocalLockManager {
    fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> impl std::future::Future<Output = Result<LockLease>> + Send {
        // Inherent method (resolution prefers it over the trait).
        LocalLockManager::acquire_lock(self, file_path, range, ttl)
    }
    fn get_fencing_token(&self, file_path: &str) -> u64 {
        LocalLockManager::get_fencing_token(self, file_path)
    }
    fn get_fencing_token_ino(&self, ino: u64) -> u64 {
        LocalLockManager::get_fencing_token_ino(self, ino)
    }
}

impl LocalLockManager {
    pub fn new() -> Result<Self> {
        let client_nonce = CLIENT_NONCE.fetch_add(1, Ordering::Relaxed);
        Ok(Self { client_nonce })
    }

    /// Current fencing generation for a path-form object key (zero-alloc for
    /// `inode_{N}` paths).
    pub fn get_fencing_token(&self, file_path: &str) -> u64 {
        match ino_of_path(file_path) {
            Some(ino) => self.get_fencing_token_ino(ino),
            None => read_identity(&ObjectKey::Path(file_path.into())),
        }
    }

    /// Current fencing generation for an inode object — binary fast path.
    pub fn get_fencing_token_ino(&self, ino: u64) -> u64 {
        read_identity(&ObjectKey::Ino(ino))
    }

    /// Acquire an **exclusive** ([`LockMode::Exclusive`]) lease on
    /// `file_path` — the whole file (`range: None`) or one `[start, end)`
    /// byte span — waiting up to `ttl` for conflicting custody to release.
    /// Every production verb takes this entry point;
    /// [`Self::acquire_lock_mode`] is the moded form.
    pub async fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> Result<LockLease> {
        self.acquire_lock_mode(file_path, range, LockMode::Exclusive, ttl)
            .await
    }

    /// Acquire a lease in an explicit [`LockMode`] (spec §6.7).
    ///
    /// **Span semantics.** `range: None` is whole-inode custody and
    /// conflicts with every span; `Some((start, end))` is `[start, end)`
    /// with **end EXCLUSIVE** and must be non-empty (`start < end`) — a
    /// malformed span refuses loud rather than granting custody over zero
    /// bytes. Disjoint spans do not conflict (S11: two applications
    /// writing different regions of one large file hold their leases at
    /// the same time — execution-plan ruling D8); overlapping spans
    /// arbitrate by the mode matrix.
    ///
    /// Wait protocol (per attempt):
    /// 1. `enable()` this FILE's stripe notification **before** the
    ///    conflict check — a release landing between the check and the
    ///    wait is then still observed (tokio's documented lost-wakeup
    ///    discipline).
    /// 2. Under the file's entry lock: check conflicts, and only if the
    ///    span is grantable, mint and admit the grant. A refused attempt
    ///    mints nothing — the AGENTS.md law "never burn fencing tokens on
    ///    failed lock acquisition".
    /// 3. Otherwise wait for a release on this file or the deadline.
    ///    Stripe collisions only cause spurious re-checks, never missed
    ///    wakeups.
    ///
    /// The wait budget is **time** (`ttl`), not wakeup counts: unrelated
    /// churn cannot starve a waiter into a spurious failure, and a quiet
    /// system fails loudly at the deadline instead of hanging.
    ///
    /// Fairness is unchanged from S0: barging is unbounded and the budget
    /// is the fairness bound. A whole-file waiter is therefore not
    /// protected against an unbroken stream of range acquires (and vice
    /// versa) — the queued/converting grant lattice is S9 work, and the
    /// loud time-bounded refusal is what keeps that honest today.
    pub async fn acquire_lock_mode(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        mode: LockMode,
        ttl: Duration,
    ) -> Result<LockLease> {
        if let Some((start, end)) = range {
            if start >= end {
                let reason = format!(
                    "malformed byte range [{start},{end}) on {file_path}: spans are \
                     [start,end) with end EXCLUSIVE and must be non-empty — refusing to \
                     grant custody over zero bytes"
                );
                log::error!("{reason}");
                return Err(crate::error::SqueezefsError::LockFailed { reason });
            }
        }
        if mode == LockMode::ConcurrentWrite && !cw_enabled() {
            // §6.7: CW ships DISABLED until a verb issues it.
            let reason = format!(
                "CW (concurrent-write) mode is not enabled: the mode exists for DLM \
                 stages S9/S11 and no verb issues it yet (spec §6.7) — refusing \
                 {file_path} {range:?}"
            );
            log::error!("{reason}");
            return Err(crate::error::SqueezefsError::LockFailed { reason });
        }

        let key = ObjectKey::from_path(file_path);
        let notify = LOCK_WAITERS.get_inode_lock(key.stripe_seed());
        let deadline = tokio::time::Instant::now() + ttl;

        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            // Register interest BEFORE the availability check (lost-wakeup fix).
            notified.as_mut().enable();

            // One entry-lock critical section decides and records the
            // grant: conflict probe → mint → admit. Nothing awaits inside
            // it, and the floor is published before the grant becomes
            // visible, so no reader can observe a grant the floor does not
            // already cover.
            let acquired = match LOCK_MAP.entry_sync(key.clone()) {
                scc::hash_map::Entry::Occupied(mut occ) => {
                    let custody = occ.get_mut();
                    if custody.conflicts(range, mode) {
                        Mint::Held
                    } else {
                        match mint_token() {
                            Err(seq) => Mint::Exhausted(seq),
                            Ok(token) => {
                                grant_floor(&key).fetch_max(token, Ordering::AcqRel);
                                custody.admit(range, self.grant(range, token, mode));
                                Mint::Granted(token)
                            }
                        }
                    }
                }
                scc::hash_map::Entry::Vacant(vac) => match mint_token() {
                    // Bit-budget refusal: never carry into the term field
                    // (that would forge a future era). Nothing is
                    // inserted — the caller holds no lock.
                    Err(seq) => Mint::Exhausted(seq),
                    Ok(token) => {
                        grant_floor(&key).fetch_max(token, Ordering::AcqRel);
                        let _ = vac.insert_entry(FileCustody::opened(
                            range,
                            self.grant(range, token, mode),
                        ));
                        Mint::Granted(token)
                    }
                },
            };

            if let Mint::Exhausted(seq) = acquired {
                let reason = format!(
                    "fencing grant space exhausted: sequence {seq} exceeds the {}-bit budget \
                     ({GRANT_SEQ_MAX}) of writer term {} — refusing to mint (a carry into the \
                     term field would forge a newer era); remount to start a fresh term",
                    GRANT_SEQ_BITS,
                    durable_term()
                );
                log::error!("{reason}");
                return Err(crate::error::SqueezefsError::LockFailed { reason });
            }

            if let Mint::Granted(fencing_token) = acquired {
                return Ok(LockLease {
                    inner: Arc::new(LockLeaseInner {
                        key,
                        span: range,
                        client_nonce: self.client_nonce,
                        fencing_token,
                        released: AtomicBool::new(false),
                    }),
                });
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(crate::error::SqueezefsError::LockFailed {
                    reason: format!(
                        "lock {key:?} span {range:?} mode {mode:?} still held after {ttl:?} \
                         wait budget"
                    ),
                });
            }
        }
    }

    /// This client's grant record for a span (whole-file custody is
    /// recorded as the full `[0, u64::MAX)` interval so one shape serves
    /// both slots).
    fn grant(&self, range: Option<(u64, u64)>, token: u64, mode: LockMode) -> Grant {
        let (start, end) = range.unwrap_or((0, u64::MAX));
        Grant {
            start,
            end,
            owner_nonce: self.client_nonce,
            token,
            mode,
        }
    }
}

struct LockLeaseInner {
    key: ObjectKey,
    /// The span this lease holds — `None` = whole-inode custody. It is the
    /// retire key: the custody list is sorted by `(start, token)`.
    span: Option<(u64, u64)>,
    client_nonce: u64,
    fencing_token: u64,
    released: AtomicBool,
}

impl LockLeaseInner {
    /// Single-pass conditional unlock: retire THIS grant from the file's
    /// custody (nonce- and token-conditional — another client's grant on
    /// the same span is never touched), drop the map entry when the file's
    /// last grant leaves, and wake the file's stripe. Idempotent across
    /// explicit `release()` + final-clone `Drop`.
    ///
    /// The wake is driven by RETIREMENT, not by entry removal: a released
    /// range on a file that still holds other spans must still wake the
    /// waiters it was blocking (a whole-file waiter, or an overlapping
    /// range's).
    fn unlock(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut retired = false;
        let _ = LOCK_MAP.remove_if_sync(&self.key, |custody| {
            retired = custody.retire(self.span, self.fencing_token, self.client_nonce);
            custody.is_vacant()
        });
        if retired {
            LOCK_WAITERS
                .get_inode_lock(self.key.stripe_seed())
                .notify_waiters();
        }
    }
}

impl Drop for LockLeaseInner {
    fn drop(&mut self) {
        self.unlock();
    }
}

#[derive(Clone)]
pub struct LockLease {
    inner: Arc<LockLeaseInner>,
}

impl LockLease {
    /// Is THIS grant still live in the file's custody table?
    pub async fn is_held(&self) -> bool {
        LOCK_MAP
            .read_sync(&self.inner.key, |_, custody| {
                custody.holds(self.inner.fencing_token, self.inner.client_nonce)
            })
            .unwrap_or(false)
    }

    /// The generation this lease was fenced at (snapshot at acquire).
    pub fn fencing_token(&self) -> u64 {
        self.inner.fencing_token
    }

    pub async fn release(self) -> Result<()> {
        self.inner.unlock();
        Ok(())
    }
}
