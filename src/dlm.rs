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
use crate::range_custody_core::{publish_floor_then_admit, FileCustody, Grant, RangePlan};
use crate::stripe_locks::StripeLocks;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use xxhash_rust::xxh3::xxh3_64;

// The interval algebra itself — `LockMode`, the `FileCustody` sorted
// interval list, the §9.2 required/desired plan with admit-time
// coalescing, and the alignment/cap arithmetic — lives in
// [`crate::range_custody_core`] (spec §6.9's fourth named loom
// obligation, KD-MW-10; `#[path]`-included by `loom-models`). This module
// keeps the POLICY: the lock table, the mint, the waiter protocol, the
// R5 byte-budget ceiling and the geometry cap enforcement.
pub use crate::range_custody_core::{block_align_out, range_span_cap, LockMode, ShrinkResolution};

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

/// DLM **S9**: adopt a grant an OWNER issued to this client into the local
/// custody table, and hand back the ordinary [`LockLease`] every call site
/// already understands.
///
/// Three things happen here, in this order, and each one is load-bearing:
///
/// 1. **The era is adopted** ([`adopt_durable_term`] on the token's term).
///    A grant is minted by the authority in ITS durable era, and a client
///    that had not learned that era would compose lower tokens and read
///    lower floors — the §6.11 inversion. Monotone, so a stale grant can
///    never lower the era.
/// 2. **The stripe floor is raised** before the grant becomes visible —
///    the same publication order [`LocalLockManager::acquire_lock_mode`]
///    uses, so no reader can observe a grant the floor does not cover.
/// 3. **The grant is admitted with the OWNER's token**, not a locally
///    minted one. That is what makes every local consumer — the fencing
///    census's `read_identity`, W1's [`span_range_shared`], the lease's own
///    `is_held` — answer from the authority's decision. Nothing on the
///    client mints custody, which is precisely the property S4's
///    foreign-home refusal protected until this stage existed.
///
/// A conflicting local adoption is impossible by construction *if the
/// owner arbitrates correctly*, and if it does not, the local table says
/// so: adoption REFUSES a span the local table already holds
/// incompatibly rather than recording two live grants for the same bytes.
pub fn adopt_remote_grant(
    ino: u64,
    span: Option<(u64, u64)>,
    token: u64,
    mode: LockMode,
    handle: Arc<dyn RemoteGrant>,
) -> Result<LockLease> {
    if let Some((start, end)) = span {
        if start >= end {
            return Err(crate::error::SqueezefsError::LockFailed {
                reason: format!(
                    "S9: refusing to adopt a malformed span [{start},{end}) on inode_{ino} — \
                     spans are [start,end) with end EXCLUSIVE and must be non-empty"
                ),
            });
        }
    }
    adopt_durable_term(token_term(token));
    let key = ObjectKey::Ino(ino);
    let client_nonce = CLIENT_NONCE.fetch_add(1, Ordering::Relaxed);
    let grant = Grant {
        start: span.map(|(s, _)| s).unwrap_or(0),
        end: span.map(|(_, e)| e).unwrap_or(u64::MAX),
        owner_nonce: client_nonce,
        token,
        mode,
        // §9.3a: an adopted record repeats the AUTHORITY's decision — the
        // client's table never arbitrates a shrink, so span-as-required
        // is the honest identity here (no local tail exists).
        required: (
            span.map(|(s, _)| s).unwrap_or(0),
            span.map(|(_, e)| e).unwrap_or(u64::MAX),
        ),
        required_segments: vec![(
            span.map(|(s, _)| s).unwrap_or(0),
            span.map(|(_, e)| e).unwrap_or(u64::MAX),
        )],
    };
    grant_floor(&key).fetch_max(token, Ordering::AcqRel);
    let mut conflicted = false;
    match LOCK_MAP.entry_sync(key.clone()) {
        scc::hash_map::Entry::Occupied(mut occ) => {
            let custody = occ.get_mut();
            if custody.holds_token(token) {
                // **The same grant, seen from the grantee's side.** A token
                // is globally unique per authority mint, so an identical
                // live token cannot be a different custody: this is the
                // authority's own record, and the authority shares this
                // process (a single-host deployment, and the suite's
                // two-nodes-in-one-process shape). ATTACHING rather than
                // duplicating is the only correct answer — a second record
                // for one grant would make the authority's release and the
                // grantee's release each look partial.
            } else if custody.conflicts(span, mode) {
                conflicted = true;
            } else {
                custody.admit(span, grant);
                note_record_admitted(span);
            }
        }
        scc::hash_map::Entry::Vacant(vac) => {
            let _ = vac.insert_entry(FileCustody::opened(span, grant));
            note_record_admitted(span);
        }
    }
    if conflicted {
        // The owner granted bytes this client believes are already held.
        // Refusing (and releasing the owner's grant) is the only answer
        // that cannot produce two live records for one span.
        handle.release();
        let reason = format!(
            "S9: the owner granted {span:?} on inode_{ino} but this client's own custody \
             table already holds an incompatible grant on those bytes — refusing the \
             adoption and releasing the grant rather than recording two live custodies \
             (the owner's arbitration and this client's view disagree, which is a bug worth \
             a loud failure)"
        );
        log::error!("{reason}");
        return Err(crate::error::SqueezefsError::LockFailed { reason });
    }
    Ok(LockLease {
        inner: Arc::new(LockLeaseInner {
            key,
            span,
            client_nonce,
            fencing_token: token,
            released: AtomicBool::new(false),
            remote: Some(handle),
        }),
    })
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
/// The generation of an object with **live local custody**, or `None` when
/// nothing is held on it.
///
/// DLM **S9** needs the distinction the general fencing read deliberately
/// hides: with an adopted remote grant the entry's `max_token` is the
/// OWNER's own answer (exact), while with no entry the read is the stripe
/// floor (a monotone over-approximation) — and on a foreign-home object the
/// sound fallback is not the floor but S8's cached grant.
pub fn live_custody_generation(ino: u64) -> Option<u64> {
    LOCK_MAP.read_sync(&ObjectKey::Ino(ino), |_, custody| custody.max_token())
}

/// Finding 27: the range PENDING-MARK hook — installed by the custody
/// authority ([`crate::data_grant::WriteCustodyOwner::arm`]) so the §9.3
/// demotion barrier's pending marks wake the STANDING notice polls the
/// instant an incumbent owes an ack (this module sits below `data_grant`
/// and cannot name the owner). Wake-all semantics; absent = no pollers.
static RANGE_PENDING_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn() + Send + Sync>>> =
    parking_lot::RwLock::new(None);

/// Install the pending-mark hook (the custody authority's arm; a re-arm
/// replaces — the newest authority owns the wake).
pub fn install_range_pending_hook(hook: Arc<dyn Fn() + Send + Sync>) {
    *RANGE_PENDING_HOOK.write() = Some(hook);
}

/// Fire the pending-mark hook (called OUTSIDE the custody entry lock —
/// a cold path: one read-lock per barrier park round).
pub(crate) fn note_range_pending() {
    let hook = RANGE_PENDING_HOOK.read().clone();
    if let Some(hook) = hook {
        hook();
    }
}

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

/// Finding 26: the ARBITER-side form of [`span_range_shared`] — TRUE only
/// when grants from two or more DISTINCT holders overlap the span. The
/// fast-path clauses consult it exclusively under the authority's
/// fold-of-shipped-assembly scope
/// ([`crate::meta_ship::publish::arbiter_fold_active`]): there the fold
/// executes the single overlapping holder's own bytes by proxy, and a
/// demoted region is the fold's own vehicle — declining either blocked
/// the demotion machinery's designed publisher on every pass (attempt 8's
/// engagement failure).
pub fn span_range_shared_for_arbiter(ino: u64, start: u64, end: u64) -> bool {
    if start >= end {
        return false;
    }
    LOCK_MAP
        .read_sync(&ObjectKey::Ino(ino), |_, custody| {
            custody.span_is_range_shared_beyond_one_holder(start, end)
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// DLM S11 rung 15 — the §9.2 bounds/R5 policy (KD-MW-7,
// docs/design-full-multi-writer.md; PR-plan row 15)
// ---------------------------------------------------------------------------

/// Bytes charged per live grant record for the `dlm_grant_table_bytes`
/// gauge: the 40-byte [`Grant`] plus its amortized `Vec` slot — the §9.2
/// arithmetic's "~48 B" (*"spans are ~48 B, so even the 1 TiB block-cyclic
/// shape is ~262,144 spans ≈ 12 MiB"*). A billing estimate for the R5
/// authority, not a byte-exact malloc trace (the metadata_cache precedent:
/// the VALUE is the ceiling, not the accounting).
pub const RANGE_GRANT_RECORD_BYTES: u64 = 48;

/// Live grant records across the process's custody table — wholes AND
/// ranges (§9.2: the gauge is "`FileCustody` wholes + ranges"), issued and
/// adopted alike: every record is RAM this process holds.
static GRANT_RECORDS: AtomicU64 = AtomicU64::new(0);

// The S11 range-custody ledger (design §13's named family).
static RANGE_GRANTS: AtomicU64 = AtomicU64::new(0);
static RANGE_EXTENSIONS: AtomicU64 = AtomicU64::new(0);
static RANGE_COVERED_SERVES: AtomicU64 = AtomicU64::new(0);
static RANGE_RELEASES: AtomicU64 = AtomicU64::new(0);
static RANGE_ACTIVE: AtomicU64 = AtomicU64::new(0);
static RANGE_CONFLICTS: AtomicU64 = AtomicU64::new(0);
static RANGE_WAITS: AtomicU64 = AtomicU64::new(0);
static RANGE_DESIRED_TRIMS: AtomicU64 = AtomicU64::new(0);
static RANGE_CAP_REFUSALS: AtomicU64 = AtomicU64::new(0);
// Rung 17 — the §9.3 demotion-barrier family. The CLOSED LEDGER LAW is
// `demotions ≡ acks + fence_resolves` on EVERY posture (healthy fleet:
// fence_resolves == 0; kill rows close through the fence column), and
// `demotion_fenced_publishes` is the ≈0 loser-law tripwire (a
// version-gate-fenced direct publish on a demoted block is the
// crash-window path, never steady state).
static RANGE_DEMOTIONS: AtomicU64 = AtomicU64::new(0);
static RANGE_DEMOTION_ACKS: AtomicU64 = AtomicU64::new(0);
static RANGE_DEMOTION_FENCE_RESOLVES: AtomicU64 = AtomicU64::new(0);
static RANGE_DEMOTION_FENCED_PUBLISHES: AtomicU64 = AtomicU64::new(0);
static RANGE_DEMOTION_WAIT: Lazy<crate::fuse_client::LatencyHistogram> =
    Lazy::new(crate::fuse_client::LatencyHistogram::default);
// §9.3a — the tail-shrink family (residual board item 7's fix). The CLOSED
// LEDGER LAW is `tail_shrinks ≡ tail_shrink_acks + tail_shrink_fence_resolves`
// on EVERY posture (healthy fleet: fence_resolves == 0; kill rows close
// through the fence column — the demotion ledger's law verbatim), and
// `shrink_demotions` is the expected-≈0-on-disjoint-workloads escalation
// gauge: a shrink that resolved [`ShrinkResolution::Demoted`] because the
// incumbent HAD written into the contested tail — sticky demotion's only
// remaining fabrication-adjacent path. `stretch_ceiling_clamps` is the
// CLIENT half: sequential-doubling stretches clamped by a learned ceiling
// (a prior shrink on the ino), the counter that says the repeat-collision
// amplification is being prevented at the source.
static RANGE_TAIL_SHRINKS: AtomicU64 = AtomicU64::new(0);
static RANGE_TAIL_SHRINK_ACKS: AtomicU64 = AtomicU64::new(0);
static RANGE_TAIL_SHRINK_FENCE_RESOLVES: AtomicU64 = AtomicU64::new(0);
static RANGE_SHRINK_DEMOTIONS: AtomicU64 = AtomicU64::new(0);
static RANGE_STRETCH_CEILING_CLAMPS: AtomicU64 = AtomicU64::new(0);

/// A grant record became live (any admit site — issue or adoption).
fn note_record_admitted(span: Option<(u64, u64)>) {
    GRANT_RECORDS.fetch_add(1, Ordering::Relaxed);
    if span.is_some() {
        RANGE_ACTIVE.fetch_add(1, Ordering::Relaxed);
    }
}

/// A grant record retired (the one unlock path).
fn note_record_retired(span: Option<(u64, u64)>) {
    GRANT_RECORDS.fetch_sub(1, Ordering::Relaxed);
    if span.is_some() {
        RANGE_ACTIVE.fetch_sub(1, Ordering::Relaxed);
        RANGE_RELEASES.fetch_add(1, Ordering::Relaxed);
    }
}

/// The `dlm_grant_table_bytes` gauge (§9.2's spec-named R5 component):
/// live grant records × the per-record estimate.
pub fn grant_table_bytes() -> u64 {
    GRANT_RECORDS.load(Ordering::Relaxed) * RANGE_GRANT_RECORD_BYTES
}

/// Live byte-range grant records on one inode — the per-file face of the
/// gauge (race-free per ino, which is what the convergence contracts
/// assert against).
pub fn live_range_records(ino: u64) -> usize {
    LOCK_MAP
        .read_sync(&ObjectKey::Ino(ino), |_, custody| custody.ranges_len())
        .unwrap_or(0)
}

/// The budget seam (the `test_swap_grant_seq` precedent): `Some(bytes)`
/// replaces the derived share, `None` restores it. Production never calls
/// it — the derived share cannot be reached deterministically by a test
/// without weeks of grants. (`Some(0)` is not expressible; 0 is the
/// "derived" sentinel, and a zero share would refuse every grant — a
/// configuration no test wants.)
static RANGE_BUDGET_OVERRIDE: AtomicU64 = AtomicU64::new(0);

/// **Test seam**: swap the range grant-table byte budget.
pub fn test_swap_range_table_budget(bytes: Option<u64>) -> Option<u64> {
    let prev = RANGE_BUDGET_OVERRIDE.swap(bytes.unwrap_or(0), Ordering::AcqRel);
    (prev != 0).then_some(prev)
}

/// Is `reason` a §9.2 capacity-class refusal (geometry cap, byte budget,
/// Red clamp — as opposed to a conflicting-custody one)? The wire's
/// status classifier (`CUSTODY_AT_CAPACITY` vs `CUSTODY_CONFLICT`),
/// matching the three refusal messages' own distinctive phrases so no
/// second error taxonomy exists — the reason text IS the operator
/// surface, and each phrase has exactly one producer in
/// [`LocalLockManager::acquire_lock_range_scoped`].
pub fn is_range_capacity_refusal(reason: &str) -> bool {
    reason.contains("geometry-derived span cap")
        || reason.contains("dlm_grant_table_bytes")
        || reason.contains("R5 authority is RED")
}

/// The grant table's byte budget — the `dlm_grant_table_bytes` R5 share,
/// enforced at admission (§9.2: *"the real ceiling is the byte budget …
/// At budget the acquire refuses loud … never a silent trim of
/// required"*).
///
/// Derived: `R5 budget / 256`, floor 16 MiB. The divisor prices custody
/// bookkeeping as a sliver of the memory budget (64 MiB at the 16 GiB
/// reference — ~1.4 M live spans); the floor's reason: §9.2's own
/// arithmetic prices the 1 TiB block-cyclic decomposition at ~262,144
/// spans ≈ 12 MiB, and a share below it would refuse the workload S11
/// exists for — the Issue-19 class (a constant refusing a legitimate
/// shape). No env knob: the seam above is the only override, and it is a
/// test seam.
pub fn range_table_budget_bytes() -> u64 {
    let over = RANGE_BUDGET_OVERRIDE.load(Ordering::Acquire);
    if over != 0 {
        return over;
    }
    (crate::mem_budget::MEM_BUDGET.budget_bytes() / 256).max(16 * 1024 * 1024)
}

/// Register `dlm_grant_table_bytes` with the R5 authority (§9.2: both
/// gauges are R5 components). Floor 0, weight 0: live custody can never
/// be SHED — dropping a grant a holder still presents would be silent
/// revocation — so the pressure response is at ADMISSION (the ceiling
/// above refuses loud, and a Red level clamps new admits; converge by
/// release, never OOM — the write_pipeline_inflight law).
pub fn ensure_grant_table_r5() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::mem_budget::MEM_BUDGET.register(crate::mem_budget::Component::new(
            "dlm_grant_table_bytes",
            0,
            0, // weight 0: never shed — admission refuses instead (§9.2)
            Arc::new(grant_table_bytes),
            Arc::new(|_| {}),
        ));
    });
}

/// The S11 range-custody ledger snapshot (design §13's family).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RangeCustodyStats {
    /// NEW range grants admitted (issue-side: local acquires and the
    /// authority's arbiter — a client's adoption repeats its authority's
    /// issue and is deliberately not double-counted).
    pub grants: u64,
    /// Admit-time merges: asks answered by WIDENING an existing grant
    /// (the §9.2 coalescing engaging — the adversarial shape's converge).
    pub extensions: u64,
    /// Asks answered by an existing covering grant (idempotent re-asks —
    /// a healthy client's cache serves these without a round trip).
    pub covered_serves: u64,
    /// Range grant records retired.
    pub releases: u64,
    /// Live range grant records (gauge).
    pub active: u64,
    /// Ranged acquires refused for conflicting custody.
    pub conflicts: u64,
    /// Ranged acquires that parked at least once before resolving.
    pub waits: u64,
    /// Grants whose DESIRED was clipped against live custody (required is
    /// structurally never trimmed).
    pub desired_trims: u64,
    /// Admissions refused by the geometry cap or the R5 byte budget —
    /// the refuse-loud arm (≈ 0 on legitimate shapes below budget: the
    /// Issue-19 falsifier).
    pub cap_refusals: u64,
    /// §9.3 demotion episodes MARKED (a block-sharing acquire parked
    /// behind the grant-issuance barrier).
    pub demotions: u64,
    /// Demotions resolved by the incumbent's ACK (reply-carried notice —
    /// every custody-channel reply since finding 16 half (a) → quiesce →
    /// client-initiated ack RPC) — the clean path.
    pub demotion_acks: u64,
    /// Demotions resolved by the incumbent's grant DEATH (release /
    /// revocation / lease expiry on the OWNER's clock) — the fence
    /// column; 0 on a healthy fleet.
    pub demotion_fence_resolves: u64,
    /// The loser-law tripwire (≈ 0): a version-gate-fenced direct
    /// publish observed on a demoted block — the crash-window path.
    pub demotion_fenced_publishes: u64,
    /// §9.3a shrink episodes MARKED (a foreign REQUIRED overlapping ONLY
    /// a grant's desired-minted stretch tail parked behind the shrink
    /// barrier instead of fabricating a demotion) — the fix's engagement
    /// gauge. Closed ledger: `tail_shrinks ≡ acks + fence_resolves`.
    pub tail_shrinks: u64,
    /// Shrinks resolved by the incumbent's ACK (reply-carried notice —
    /// every custody-channel reply since finding 16 half (a) → cache
    /// shrink → watermark answer) — the clean path, whichever resolution
    /// the watermark selected.
    pub tail_shrink_acks: u64,
    /// Shrinks resolved by the incumbent's grant DEATH (release /
    /// revocation / lease expiry) — the fence column; 0 on a healthy
    /// fleet.
    pub tail_shrink_fence_resolves: u64,
    /// Shrinks whose ack ESCALATED to the honest demotion barrier because
    /// the incumbent HAD written into the contested tail
    /// ([`ShrinkResolution::Demoted`]) — expected ≈ 0 on disjoint
    /// workloads; sticky demotion's only remaining fabrication-adjacent
    /// path.
    pub shrink_demotions: u64,
    /// CLIENT side: sequential-doubling stretches clamped by a learned
    /// ceiling (a prior shrink on the ino) — the repeat-collision
    /// prevention engaging.
    pub stretch_ceiling_clamps: u64,
}

/// Read the process's range-custody ledger.
pub fn range_custody_stats() -> RangeCustodyStats {
    RangeCustodyStats {
        grants: RANGE_GRANTS.load(Ordering::Relaxed),
        extensions: RANGE_EXTENSIONS.load(Ordering::Relaxed),
        covered_serves: RANGE_COVERED_SERVES.load(Ordering::Relaxed),
        releases: RANGE_RELEASES.load(Ordering::Relaxed),
        active: RANGE_ACTIVE.load(Ordering::Relaxed),
        conflicts: RANGE_CONFLICTS.load(Ordering::Relaxed),
        waits: RANGE_WAITS.load(Ordering::Relaxed),
        desired_trims: RANGE_DESIRED_TRIMS.load(Ordering::Relaxed),
        cap_refusals: RANGE_CAP_REFUSALS.load(Ordering::Relaxed),
        demotions: RANGE_DEMOTIONS.load(Ordering::Relaxed),
        demotion_acks: RANGE_DEMOTION_ACKS.load(Ordering::Relaxed),
        demotion_fence_resolves: RANGE_DEMOTION_FENCE_RESOLVES.load(Ordering::Relaxed),
        demotion_fenced_publishes: RANGE_DEMOTION_FENCED_PUBLISHES.load(Ordering::Relaxed),
        tail_shrinks: RANGE_TAIL_SHRINKS.load(Ordering::Relaxed),
        tail_shrink_acks: RANGE_TAIL_SHRINK_ACKS.load(Ordering::Relaxed),
        tail_shrink_fence_resolves: RANGE_TAIL_SHRINK_FENCE_RESOLVES.load(Ordering::Relaxed),
        shrink_demotions: RANGE_SHRINK_DEMOTIONS.load(Ordering::Relaxed),
        stretch_ceiling_clamps: RANGE_STRETCH_CEILING_CLAMPS.load(Ordering::Relaxed),
    }
}

/// The `range_custody` stats-inode object (design §13) — 0 in every field
/// on a single-writer mount BY CONSTRUCTION (no product verb issues a
/// range grant without the armed co-writer path).
pub fn range_custody_stats_json() -> serde_json::Value {
    let s = range_custody_stats();
    serde_json::json!({
        "range_custody_grants": s.grants,
        "range_custody_extensions": s.extensions,
        "range_custody_covered_serves": s.covered_serves,
        "range_custody_releases": s.releases,
        "range_custody_active": s.active,
        "range_custody_conflicts": s.conflicts,
        "range_custody_waits": s.waits,
        "range_custody_desired_trims": s.desired_trims,
        "range_custody_cap_refusals": s.cap_refusals,
        "range_custody_demotions": s.demotions,
        "range_custody_demotion_acks": s.demotion_acks,
        "range_custody_demotion_fence_resolves": s.demotion_fence_resolves,
        "range_custody_demotion_fenced_publishes": s.demotion_fenced_publishes,
        "range_custody_tail_shrinks": s.tail_shrinks,
        "range_custody_tail_shrink_acks": s.tail_shrink_acks,
        "range_custody_tail_shrink_fence_resolves": s.tail_shrink_fence_resolves,
        "range_custody_shrink_demotions": s.shrink_demotions,
        "range_custody_stretch_ceiling_clamps": s.stretch_ceiling_clamps,
        "range_custody_demotion_wait_ns": RANGE_DEMOTION_WAIT.to_json(),
        "dlm_grant_table_bytes": grant_table_bytes(),
        "dlm_grant_table_budget_bytes": range_table_budget_bytes(),
    })
}

// ---------------------------------------------------------------------------
// DLM S11 rung 17 — the §9.3 demotion barrier's public surface (KD-MW-8)
// ---------------------------------------------------------------------------

/// The UNACKED demotion notices naming `incumbent_token` on `ino` — the
/// reply-carried notice's read (every custody-channel reply since finding
/// 16 half (a): acquire/release/renewal), composed under the SAME
/// `FileCustody` entry serialization that parked the waiter (the
/// in-flight-renewal race pin: a reply composed after the pending-mark
/// always observes it).
pub fn demotion_notices_for(ino: u64, incumbent_token: u64) -> Vec<(u64, u64)> {
    LOCK_MAP
        .read_sync(&ObjectKey::Ino(ino), |_, custody| {
            custody.demotion_notices_for_token(incumbent_token)
        })
        .unwrap_or_default()
}

/// The incumbent's ACK (a client-initiated RPC's landing point): mark
/// the pending acked, adopt the region as DEMOTED (authority-assembled),
/// count it, and wake the file's stripe so the parked waiter re-plans.
pub fn ack_demotion(ino: u64, incumbent_token: u64, region: (u64, u64)) -> bool {
    if region.0 >= region.1 {
        return false;
    }
    let key = ObjectKey::Ino(ino);
    let acked = LOCK_MAP
        .update_sync(&key, |_, custody| {
            custody.ack_demotion(incumbent_token, region)
        })
        .unwrap_or(false);
    if acked {
        RANGE_DEMOTION_ACKS.fetch_add(1, Ordering::Relaxed);
        LOCK_WAITERS
            .get_inode_lock(key.stripe_seed())
            .notify_waiters();
    }
    acked
}

// ---------------------------------------------------------------------------
// DLM S11 §9.3a — the tail-shrink arm's public surface (residual item 7)
// ---------------------------------------------------------------------------

/// The pending SHRINK notice naming `incumbent_token` on `ino` — the
/// reply-carried notice's read (every custody-channel reply since finding
/// 16 half (a); composed under the same `FileCustody` entry serialization
/// that parked the asker, the demotion notice's in-flight-renewal race
/// pin verbatim). The answer is the block-hulled FLOOR the incumbent's
/// tail is asked to release back to.
pub fn shrink_notice_for(ino: u64, incumbent_token: u64) -> Option<u64> {
    LOCK_MAP
        .read_sync(&ObjectKey::Ino(ino), |_, custody| {
            custody.shrink_notice_for_token(incumbent_token)
        })
        .flatten()
}

/// The incumbent's shrink ACK (a client-initiated RPC's landing point):
/// resolve the pending against the client's written high-water, count the
/// resolution, and wake the file's stripe so the parked asker re-plans —
/// against the shrunk span ([`ShrinkResolution::Shrunk`]: exclusive
/// custody, no demotion) or into the EXISTING demotion barrier
/// ([`ShrinkResolution::Demoted`]: the client truly wrote there — sticky
/// demotion reserved for TRUE sharing, counted
/// `range_custody_shrink_demotions`).
pub fn ack_tail_shrink(ino: u64, incumbent_token: u64, watermark: u64) -> ShrinkResolution {
    let key = ObjectKey::Ino(ino);
    let (resolution, demotions_fence_resolved) = LOCK_MAP
        .update_sync(&key, |_, custody| {
            custody.ack_tail_shrink(incumbent_token, watermark)
        })
        .unwrap_or((ShrinkResolution::None, 0));
    match resolution {
        ShrinkResolution::None => {}
        ShrinkResolution::Shrunk { .. } => {
            RANGE_TAIL_SHRINK_ACKS.fetch_add(1, Ordering::Relaxed);
        }
        ShrinkResolution::Demoted { .. } => {
            // The escalating ack still CLOSES the shrink ledger (it is an
            // ack), and is additionally counted as the escalation gauge.
            RANGE_TAIL_SHRINK_ACKS.fetch_add(1, Ordering::Relaxed);
            RANGE_SHRINK_DEMOTIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
    if demotions_fence_resolved > 0 {
        // Demotion pendings made moot by the shrink (their region's
        // custody provably retired) close through the demotion ledger's
        // fence column — the `sweep_pendings_of` law.
        RANGE_DEMOTION_FENCE_RESOLVES.fetch_add(demotions_fence_resolved as u64, Ordering::Relaxed);
    }
    if resolution != ShrinkResolution::None {
        LOCK_WAITERS
            .get_inode_lock(key.stripe_seed())
            .notify_waiters();
    }
    resolution
}

/// The CLIENT half of a shrink: narrow this process's own adopted record
/// to `new_end` so every local consumer (`span_range_shared`, the W1/B4
/// clauses, covering probes through the custody table) stops claiming the
/// released tail. `false` ⇔ no local record carries `token` (a racing
/// lease loss retired it whole — nothing left to shrink).
pub fn shrink_adopted_grant(ino: u64, token: u64, new_end: u64) -> bool {
    LOCK_MAP
        .update_sync(&ObjectKey::Ino(ino), |_, custody| {
            custody.shrink_grant_tail(token, new_end)
        })
        .unwrap_or(false)
}

/// Count one CLIENT-side stretch clamp (`ranged_lease_attempt`'s learned
/// ceiling engaging — the repeat-collision prevention's gauge).
pub fn note_stretch_ceiling_clamp() {
    RANGE_STRETCH_CEILING_CLAMPS.fetch_add(1, Ordering::Relaxed);
}

/// The CLIENT half of a demotion: mark `region` demoted in this
/// process's own custody table, so its local `span_range_shared` probes
/// (the W1/B4 clauses, the write path's extent-ship classification)
/// answer TRUE for the region. `false` ⇔ no live entry (the caller holds
/// no custody on the ino — nothing to re-route).
pub fn adopt_demoted_region(ino: u64, region: (u64, u64)) -> bool {
    if region.0 >= region.1 {
        return false;
    }
    LOCK_MAP
        .update_sync(&ObjectKey::Ino(ino), |_, custody| {
            custody.adopt_demoted(region);
            true
        })
        .unwrap_or(false)
}

/// The live demoted regions on `ino` (the grant reply carries them so an
/// adopting co-writer marks its local table before its first write).
/// Since rung 18 also the custody-scoped Put's EXCLUSION input: inside a
/// demoted region the AUTHORITY is the single publisher (KD-MW-8), so no
/// holder's Put is truth there.
pub fn demoted_regions(ino: u64) -> Vec<(u64, u64)> {
    LOCK_MAP
        .read_sync(&ObjectKey::Ino(ino), |_, custody| custody.demoted_regions())
        .unwrap_or_default()
}

/// Does `ino` carry any LIVE byte-range grant (rung 18)? The local-
/// publish serve-window guard's predicate: an authority's OWN layout
/// publish of a range-granted ino must serialize with the served scoped
/// Puts (whose read→compose→commit spans the serve stripe) — one map
/// probe, `false` on every file without range custody.
pub fn ino_has_range_custody(ino: u64) -> bool {
    LOCK_MAP
        .read_sync(&ObjectKey::Ino(ino), |_, custody| custody.ranges_len() > 0)
        .unwrap_or(false)
}

/// The §9.3 loser-law tripwire: a version-gate-fenced direct publish
/// observed on an ino with demoted custody (`≈ 0` — the crash-window
/// path; the loser re-ships its own bytes as extents, never
/// retry-as-whole-block).
pub fn note_demotion_fenced_publish() {
    RANGE_DEMOTION_FENCED_PUBLISHES.fetch_add(1, Ordering::Relaxed);
}

/// One [`LocalLockManager::acquire_lock_range`] outcome.
#[derive(Debug)]
pub enum RangeAcquired {
    /// A fresh grant: hold the lease — dropping/releasing it retires the
    /// span.
    New {
        lease: LockLease,
        /// The granted span — the conflict-free desired-subset, ⊇ required.
        span: (u64, u64),
    },
    /// An existing same-scope grant was WIDENED in place (the §9.2
    /// admit-time merge): same token, same release identity, wider span.
    /// No new lease exists — the original holder's lease now covers
    /// `span`.
    Extended { token: u64, span: (u64, u64) },
    /// An existing same-scope grant already covers `required` (idempotent
    /// re-ask). Nothing was mutated and nothing was minted.
    Covered { token: u64, span: (u64, u64) },
}

/// The merge-scope namespace for an authority's per-lease grants: lease
/// epochs and process client nonces are both small integers minted from 1,
/// so the high bit keeps the two scope spaces disjoint (a process would
/// need 2^63 `DlmClient` constructions to collide — not a reachable
/// state).
pub fn range_scope_for_epoch(lease_epoch: u64) -> u64 {
    lease_epoch | (1 << 63)
}

/// DLM S9's client half of an EXTENSION: the authority widened a grant
/// this process had already adopted — widen the local record to match
/// (same token, same release identity; the local table is a repeat of the
/// authority's decision, so no local conflict probe re-arbitrates it).
/// `false` ⇔ no local record carries `token` (the grant died between the
/// reply and this call — the caller treats it as lease-lost).
///
/// `required` is the client's OWN never-trim ask that produced the
/// extension — NEVER the widened span (§9.3a): when the authority shares
/// this process the adopted record IS the authority's arbiter record (the
/// `adopt_remote_grant` attach case — every single-host deployment and
/// the in-process suites), so a span-as-required union here would stamp
/// the whole desired-minted stretch as honest custody and structurally
/// disable the tail-shrink arm on exactly those deployments.
pub fn widen_adopted_grant(ino: u64, token: u64, span: (u64, u64), required: (u64, u64)) -> bool {
    LOCK_MAP
        .update_sync(&ObjectKey::Ino(ino), |_, custody| {
            custody.widen_grant(token, span, required)
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

/// Spec §6.2 **item 9** — mint one durable **layout version**: the
/// era-composed stamp a layout delta record carries as its own chain
/// link name (`crate::layout_wire::LayoutDelta::version`).
///
/// Deliberately draws from the SAME [`GRANT_SEQ`] sequencer as the S1
/// fencing mint: `(term, seq)` uniqueness is then ONE invariant with
/// one owner instead of two counters that must never collide, and the
/// sequence is gap-carrying by the same law that makes gap-carrying
/// tokens legal everywhere (`<`, `==`, `.max()` consumers only). Two
/// layout versions can therefore never be equal within a process, and
/// the durable-term composition (S2, monotone `fetch_max` adoption)
/// makes them unique ACROSS processes and writer eras — which is
/// exactly what lets a delta chain name its base after a failover.
///
/// Returns `0` on 40-bit grant exhaustion (the same horizon at which
/// [`LocalLockManager`]'s own mint refuses lock grants loud): `0` is
/// the reserved "unversioned" value, and the publish path's disposition
/// for it is the always-correct full-`Put` re-base — never an error
/// minted here, because the lock mint is already the loud gate for a
/// process in that state.
pub fn mint_layout_version() -> u64 {
    let seq = GRANT_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
    if seq > GRANT_SEQ_MAX {
        0
    } else {
        compose_token(durable_term(), seq)
    }
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
        .read_sync(identity, |_, custody| custody.max_token())
        .unwrap_or_else(|| {
            grant_floor(identity)
                .load(Ordering::Acquire)
                .max(term_base())
        })
}
/// Per-stripe release notifications. A release wakes only its own stripe —
/// never every waiter in the process (the old single global `Notify` was a
/// thundering herd and let unrelated churn burn waiters' retry budgets).
static LOCK_WAITERS: Lazy<StripeLocks<squeezefs_ipc::sqz_notify::Notify, 1024>> =
    Lazy::new(StripeLocks::new);

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
        let deadline = std::time::Instant::now() + ttl;

        loop {
            let mut notified = notify.notified_raw();
            // Register interest BEFORE the availability check (lost-wakeup fix).
            notified.enable();

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
                                // The S1 publication order, through the
                                // core's own protocol fn (loom-modeled):
                                // floor BEFORE visibility.
                                publish_floor_then_admit(grant_floor(&key), token, || {
                                    custody.admit(range, self.grant(range, token, mode))
                                });
                                note_record_admitted(range);
                                if range.is_some() {
                                    RANGE_GRANTS.fetch_add(1, Ordering::Relaxed);
                                }
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
                        publish_floor_then_admit(grant_floor(&key), token, || {
                            let _ = vac.insert_entry(FileCustody::opened(
                                range,
                                self.grant(range, token, mode),
                            ));
                        });
                        note_record_admitted(range);
                        if range.is_some() {
                            RANGE_GRANTS.fetch_add(1, Ordering::Relaxed);
                        }
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
                        remote: None,
                    }),
                });
            }

            if squeezefs_ipc::sqz_time::timeout_at(deadline, notified)
                .await
                .is_err()
            {
                return Err(crate::error::SqueezefsError::LockFailed {
                    reason: format!(
                        "lock {key:?} span {range:?} mode {mode:?} still held after {ttl:?} \
                         wait budget"
                    ),
                });
            }
        }
    }

    /// Acquire an **EX byte-range** lease by the §9.2 required/desired
    /// law: grant the largest desired-subset that conflicts with nothing —
    /// **never less than `required`** (a foreign grant overlapping
    /// required waits/refuses; desired is always trimmable), with
    /// **admit-time coalescing** (an ask adjacent to or overlapping the
    /// same scope's live grant WIDENS that grant — same token, no new
    /// record — which is what converges the adversarial tiny-ranges shape
    /// to O(1) spans), the **geometry-derived per-file span cap**
    /// `max(16, ceil(size / block_size))` and the **`dlm_grant_table_bytes`
    /// R5 byte-budget ceiling**, both refuse-loud naming their arithmetic
    /// (no free span constants — Issue-19's law).
    ///
    /// EX-only by construction: KD-MW-9's v1 issuance law (*"v1 issues EX
    /// only"*) — the moded span path stays [`Self::acquire_lock_mode`],
    /// where CW ships disabled.
    ///
    /// `geometry` = `(file_size, block_size)` for the per-file cap;
    /// `None` (an authority with no geometry source) runs the byte budget
    /// alone — a conjured default size would be exactly the constant
    /// Issue-19 forbids.
    pub async fn acquire_lock_range(
        &self,
        file_path: &str,
        required: (u64, u64),
        desired: (u64, u64),
        ttl: Duration,
        geometry: Option<(u64, u64)>,
    ) -> Result<RangeAcquired> {
        self.acquire_lock_range_scoped(file_path, required, desired, ttl, geometry, None)
            .await
    }

    /// [`Self::acquire_lock_range`] with an explicit MERGE SCOPE — the
    /// holder identity grants coalesce under. The S9 authority passes
    /// [`range_scope_for_epoch`] of the client's lease epoch (one scope
    /// per client lease); `None` = this manager's own nonce (the local
    /// caller).
    pub async fn acquire_lock_range_scoped(
        &self,
        file_path: &str,
        required: (u64, u64),
        desired: (u64, u64),
        ttl: Duration,
        geometry: Option<(u64, u64)>,
        merge_scope: Option<u64>,
    ) -> Result<RangeAcquired> {
        if required.0 >= required.1 {
            let reason = format!(
                "malformed required range [{},{}) on {file_path}: spans are [start,end) with \
                 end EXCLUSIVE and must be non-empty — refusing to grant custody over zero \
                 bytes",
                required.0, required.1
            );
            log::error!("{reason}");
            return Err(crate::error::SqueezefsError::LockFailed { reason });
        }
        if desired.0 > required.0 || desired.1 < required.1 {
            let reason = format!(
                "malformed desired window [{},{}) on {file_path}: desired must CONTAIN \
                 required [{},{}) (desired is the best-effort stretch, required the \
                 never-trimmed floor — §9.2)",
                desired.0, desired.1, required.0, required.1
            );
            log::error!("{reason}");
            return Err(crate::error::SqueezefsError::LockFailed { reason });
        }
        let scope = merge_scope.unwrap_or(self.client_nonce);
        let span_cap = geometry.map(|(size, block)| range_span_cap(size, block));
        // Rung 17 (§9.3): the demotion barrier's block-sharing probe is
        // decidable only with a block size — no geometry, no barrier
        // (an ask then parks on the plain conflict law, unchanged).
        let block_size = geometry.map(|(_, block)| block).filter(|b| *b > 0);
        let key = ObjectKey::from_path(file_path);
        let notify = LOCK_WAITERS.get_inode_lock(key.stripe_seed());
        let deadline = std::time::Instant::now() + ttl;
        let mut parked = false;
        // Rung 17: set when THIS ask parks behind the demotion barrier —
        // the admit records the reply-bounded window it waited
        // (`range_custody_demotion_wait_ns`; one interaction on a
        // churn-shaped incumbent since finding 16 half (a), one renewal
        // cadence worst case).
        let mut barrier_parked_at: Option<std::time::Instant> = None;

        /// The one-critical-section outcome (plan AND apply under the
        /// entry lock — a plan can never go stale before its admit).
        enum RangeMint {
            New {
                token: u64,
                span: (u64, u64),
            },
            Extended {
                token: u64,
                span: (u64, u64),
            },
            Covered {
                token: u64,
                span: (u64, u64),
            },
            Held,
            /// Rung 17: parked behind the §9.3 grant-issuance demotion
            /// barrier (pendings marked; the incumbent's ack — or its
            /// lease death — wakes this waiter).
            HeldDemotion,
            Bridge,
            CapGeometry {
                live: usize,
                cap: u64,
            },
            CapBudget {
                bytes: u64,
                budget: u64,
            },
            Red {
                bytes: u64,
            },
            Exhausted(u64),
        }

        loop {
            let mut notified = notify.notified_raw();
            // Register interest BEFORE the availability check (the
            // lost-wakeup discipline).
            notified.enable();

            let mem_red = crate::mem_budget::level() == crate::mem_budget::Level::Red;
            let decide = |custody: &mut FileCustody| -> RangeMint {
                // The §9.2 bounds + mint + admit for a NEW record over
                // `span` (shared by the plan's New arm and the rung-17
                // licensed-coexistence admit).
                let admit_new =
                    |custody: &mut FileCustody, span: (u64, u64), trimmed: bool| -> RangeMint {
                        // The §9.2 bounds, in refusal order: the file's own
                        // geometry, then the R5 byte budget, then the Red
                        // clamp. All BEFORE the mint (never burn a token
                        // on a refused acquisition).
                        let live = custody.ranges_len();
                        if let Some(cap) = span_cap {
                            if (live as u64) >= cap {
                                return RangeMint::CapGeometry { live, cap };
                            }
                        }
                        let bytes = grant_table_bytes();
                        let budget = range_table_budget_bytes();
                        if bytes + RANGE_GRANT_RECORD_BYTES > budget {
                            return RangeMint::CapBudget { bytes, budget };
                        }
                        if mem_red {
                            return RangeMint::Red { bytes };
                        }
                        match mint_token() {
                            Err(seq) => RangeMint::Exhausted(seq),
                            Ok(token) => {
                                if trimmed {
                                    RANGE_DESIRED_TRIMS.fetch_add(1, Ordering::Relaxed);
                                }
                                publish_floor_then_admit(grant_floor(&key), token, || {
                                    custody.admit(
                                        Some(span),
                                        Grant {
                                            start: span.0,
                                            end: span.1,
                                            owner_nonce: scope,
                                            token,
                                            mode: LockMode::Exclusive,
                                            // §9.3a: the ask's never-trim
                                            // floor — everything beyond
                                            // its hull is shrinkable
                                            // stretch.
                                            required,
                                            required_segments: vec![required],
                                        },
                                    )
                                });
                                note_record_admitted(Some(span));
                                RANGE_GRANTS.fetch_add(1, Ordering::Relaxed);
                                RangeMint::New { token, span }
                            }
                        }
                    };
                // Rung 17 (§9.3): the grant-issuance DEMOTION BARRIER.
                // `ask` is the span whose issuance would share a block
                // with a live foreign grant: mark one pending per
                // un-acked sharer (the incumbent learns on its next
                // custody-channel reply — acquire/release/renewal since
                // finding 16 half (a)) and PARK — the grant is withheld until
                // every sharer acked (its region then reads demoted and
                // the sharer probe excludes it) or died (the retire
                // sweep resolves through the fence column).
                //
                // §9.3a (residual item 7's fix): a sharer whose shared
                // region lies wholly at-or-beyond its REQUIRED-union
                // hull is sharing only the desired-minted STRETCH TAIL —
                // the shape the fabric venue proved fabricates demotions
                // on fully-aligned disjoint rows. That sharer marks a
                // SHRINK pending (floor = the shared region's start, the
                // block boundary that frees this ask) instead: the asker
                // parks on the same machinery, and the incumbent's ack
                // releases the tail rather than demoting the block.
                let barrier = |custody: &mut FileCustody, ask: (u64, u64)| -> bool {
                    let Some(bs) = block_size else { return false };
                    let sharers = custody.foreign_block_sharers(ask, bs, scope);
                    if sharers.is_empty() {
                        return false;
                    }
                    for sharer in sharers {
                        if !sharer.shares_required && sharer.tail_share {
                            if custody.mark_shrink_pending(sharer.token, sharer.region.0, bs) {
                                RANGE_TAIL_SHRINKS.fetch_add(1, Ordering::Relaxed);
                            }
                        } else if custody.mark_demotion_pending(sharer.region, sharer.token) {
                            RANGE_DEMOTIONS.fetch_add(1, Ordering::Relaxed);
                            // Finding 31 forensics: on a block-aligned row
                            // NOTHING should genuinely share a block, so
                            // every demotion mark names its parties loud —
                            // the ask, the sharer's grant geometry, and the
                            // classifier inputs (region vs required hull).
                            log::warn!(
                                "S11 demotion marked on {file_path}: ask=[{},{}) required=[{},{}) \
                                 ask_scope {scope:#x} sharer token {} scope {:#x} \
                                 region=[{},{}) required_hull_end {} (region.0 < hull ⇒ \
                                 region-sharing; fabricated on aligned rows — finding 31) \
                                 claim segments {:?}",
                                ask.0,
                                ask.1,
                                required.0,
                                required.1,
                                sharer.token,
                                sharer.owner_nonce,
                                sharer.region.0,
                                sharer.region.1,
                                sharer.required_hull_end,
                                sharer.segments,
                            );
                        }
                    }
                    true
                };
                let decision =
                    custody.plan_range(required, desired, scope, block_size.unwrap_or(0));
                match decision.plan {
                    RangePlan::HeldForeign => {
                        // Rung 17: foreign custody overlaps REQUIRED. If
                        // every overlap lies inside a DEMOTED region the
                        // coexistence is licensed (nobody DMAs there —
                        // the authority assembles) and REQUIRED admits
                        // exactly; otherwise the barrier marks pendings
                        // (range sharers only — wholes keep the plain
                        // conflict law) and the ask parks.
                        if block_size.is_none() || custody.has_wholes() {
                            return RangeMint::Held;
                        }
                        if barrier(custody, required) {
                            return RangeMint::HeldDemotion;
                        }
                        if custody.required_overlaps_licensed(required, scope) {
                            // Convergence inside a demoted region: a
                            // stream's licensed asks must EXTEND the
                            // holder's own record (byte overlap with the
                            // demoted peers is licensed — nobody DMAs
                            // there), or the §9.2 geometry cap would
                            // refuse the very interleave the demotion
                            // exists to serve.
                            if let Some((token, span)) = custody.own_adjacent_grant(required, scope)
                            {
                                let widened = custody.widen_grant(token, span, required);
                                debug_assert!(widened, "plan and apply share one critical section");
                                RANGE_EXTENSIONS.fetch_add(1, Ordering::Relaxed);
                                if required != desired {
                                    RANGE_DESIRED_TRIMS.fetch_add(1, Ordering::Relaxed);
                                }
                                return RangeMint::Extended { token, span };
                            }
                            return admit_new(custody, required, required != desired);
                        }
                        RangeMint::Held
                    }
                    RangePlan::BridgeRefused => RangeMint::Bridge,
                    RangePlan::Covered { token, span } => {
                        // §9.3a: a COVERED serve is the holder actively
                        // claiming these bytes — union its required into
                        // the grant's watermark so a tail byte re-asked
                        // over the wire mid-shrink resolves the late ack
                        // as Demoted (honest custody), never as a release
                        // of bytes the holder may be DMAing.
                        custody.note_required(token, required);
                        RangeMint::Covered { token, span }
                    }
                    RangePlan::Extend { token, span } => {
                        // Rung 17: a widening that would NEWLY share a
                        // block with a live foreign grant is the same
                        // demotion shape as a fresh admit — barrier it
                        // (shares already inside demoted regions are
                        // licensed and excluded by the probe).
                        if barrier(custody, span) {
                            return RangeMint::HeldDemotion;
                        }
                        // The merge rides the admit: widen in place, keep
                        // the token, mint nothing, add no record — so the
                        // caps structurally cannot refuse a coalescing ask.
                        if decision.trimmed {
                            RANGE_DESIRED_TRIMS.fetch_add(1, Ordering::Relaxed);
                        }
                        let widened = custody.widen_grant(token, span, required);
                        debug_assert!(widened, "plan and apply share one critical section");
                        RANGE_EXTENSIONS.fetch_add(1, Ordering::Relaxed);
                        RangeMint::Extended { token, span }
                    }
                    RangePlan::New { span } => {
                        // Rung 17: a byte-disjoint window that would
                        // SHARE A BLOCK with a live foreign grant is the
                        // other demotion shape — barrier before admit.
                        if barrier(custody, span) {
                            return RangeMint::HeldDemotion;
                        }
                        admit_new(custody, span, decision.trimmed)
                    }
                }
            };

            let outcome = match LOCK_MAP.entry_sync(key.clone()) {
                scc::hash_map::Entry::Occupied(mut occ) => decide(occ.get_mut()),
                scc::hash_map::Entry::Vacant(vac) => {
                    let mut fresh = FileCustody::empty();
                    let outcome = decide(&mut fresh);
                    if !fresh.is_vacant() {
                        let _ = vac.insert_entry(fresh);
                    }
                    outcome
                }
            };

            match outcome {
                RangeMint::New { token, span } => {
                    if let Some(t0) = barrier_parked_at {
                        // The demotion barrier's reply-bounded window,
                        // priced (§13's `wait_ns`).
                        RANGE_DEMOTION_WAIT.record(t0.elapsed());
                    }
                    return Ok(RangeAcquired::New {
                        lease: LockLease {
                            inner: Arc::new(LockLeaseInner {
                                key,
                                span: Some(span),
                                client_nonce: scope,
                                fencing_token: token,
                                released: AtomicBool::new(false),
                                remote: None,
                            }),
                        },
                        span,
                    });
                }
                RangeMint::Extended { token, span } => {
                    return Ok(RangeAcquired::Extended { token, span });
                }
                RangeMint::Covered { token, span } => {
                    RANGE_COVERED_SERVES.fetch_add(1, Ordering::Relaxed);
                    return Ok(RangeAcquired::Covered { token, span });
                }
                RangeMint::Bridge => {
                    let reason = format!(
                        "range acquire [{},{}) on {file_path}: required overlaps TWO OR MORE \
                         of this holder's own live grants (the bridge ask) — merging would \
                         absorb a second live grant record whose release handle is \
                         outstanding, so the shape refuses loud; a client whose range cache \
                         answers covering probes never builds it (§9.2)",
                        required.0, required.1
                    );
                    log::error!("{reason}");
                    return Err(crate::error::SqueezefsError::LockFailed { reason });
                }
                RangeMint::CapGeometry { live, cap } => {
                    RANGE_CAP_REFUSALS.fetch_add(1, Ordering::Relaxed);
                    let reason = format!(
                        "range acquire [{},{}) on {file_path}: the file already carries \
                         {live} live range grants, at its geometry-derived span cap {cap} = \
                         max(16, ceil(size / block_size)) — a NEW span refuses loud (never a \
                         silent trim of required); release or let coalescible custody merge \
                         (§9.2; the cap is the file's own geometry, never a constant)",
                        required.0, required.1
                    );
                    log::error!("{reason}");
                    return Err(crate::error::SqueezefsError::LockFailed { reason });
                }
                RangeMint::CapBudget { bytes, budget } => {
                    RANGE_CAP_REFUSALS.fetch_add(1, Ordering::Relaxed);
                    let reason = format!(
                        "range acquire [{},{}) on {file_path}: the grant table is at its R5 \
                         byte budget — dlm_grant_table_bytes {bytes} B + {RANGE_GRANT_RECORD_BYTES} B \
                         for the new record would exceed the share {budget} B (derived: R5 \
                         budget / 256, floor 16 MiB) — refusing loud rather than trimming \
                         required; the table converges by RELEASE (§9.2)",
                        required.0, required.1
                    );
                    log::error!("{reason}");
                    return Err(crate::error::SqueezefsError::LockFailed { reason });
                }
                RangeMint::Red { bytes } => {
                    RANGE_CAP_REFUSALS.fetch_add(1, Ordering::Relaxed);
                    let reason = format!(
                        "range acquire [{},{}) on {file_path}: the R5 authority is RED — new \
                         grant admissions are clamped (dlm_grant_table_bytes {bytes} B; \
                         custody is never shed, so pressure answers at admission and \
                         converges by release — §9.2's Red law)",
                        required.0, required.1
                    );
                    log::error!("{reason}");
                    return Err(crate::error::SqueezefsError::LockFailed { reason });
                }
                RangeMint::Exhausted(seq) => {
                    let reason = format!(
                        "fencing grant space exhausted: sequence {seq} exceeds the {}-bit \
                         budget ({GRANT_SEQ_MAX}) of writer term {} — refusing to mint (a \
                         carry into the term field would forge a newer era); remount to \
                         start a fresh term",
                        GRANT_SEQ_BITS,
                        durable_term()
                    );
                    log::error!("{reason}");
                    return Err(crate::error::SqueezefsError::LockFailed { reason });
                }
                held @ (RangeMint::Held | RangeMint::HeldDemotion) => {
                    if matches!(held, RangeMint::HeldDemotion) {
                        // Finding 27: the barrier just marked (or re-holds)
                        // pendings — wake the standing notice polls so a
                        // QUIET incumbent hears NOW, not at its renewal.
                        note_range_pending();
                        if barrier_parked_at.is_none() {
                            barrier_parked_at = Some(std::time::Instant::now());
                        }
                    }
                    if !parked {
                        parked = true;
                        RANGE_WAITS.fetch_add(1, Ordering::Relaxed);
                    }
                    if squeezefs_ipc::sqz_time::timeout_at(deadline, notified)
                        .await
                        .is_err()
                    {
                        RANGE_CONFLICTS.fetch_add(1, Ordering::Relaxed);
                        return Err(crate::error::SqueezefsError::LockFailed {
                            reason: format!(
                                "range [{},{}) on {file_path} is held by foreign custody \
                                 overlapping REQUIRED after the {ttl:?} wait budget — \
                                 refusing loud (required is never silently trimmed to fit — \
                                 §9.2)",
                                required.0, required.1
                            ),
                        });
                    }
                }
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
            // Plain moded acquires carry no desired stretch: the whole
            // span IS the required ask (§9.3a's identity case — no
            // shrinkable tail exists, classifications stay byte-identical).
            required: (start, end),
            required_segments: vec![(start, end)],
        }
    }
}

/// DLM **S9**: the client side of a grant an OWNER holds on our behalf.
///
/// A remote grant is adopted into this process's custody table
/// ([`adopt_remote_grant`]) so that every local consumer — the ~24 fencing
/// reads, W1's seventh clause ([`span_range_shared`]), the lease's own
/// `is_held` — answers from the owner's arbitrated decision instead of a
/// locally invented one. What the client cannot do is *retire* the grant by
/// itself: the authority's lease is the custody, so release must travel.
pub trait RemoteGrant: Send + Sync {
    /// Tell the owner this grant is released (must be non-blocking: it is
    /// called from `Drop`).
    fn release(&self);
    /// `false` once the owner has revoked/expired the grant — what makes
    /// `LockLease::is_held` honest on a client.
    fn live(&self) -> bool;
}

struct LockLeaseInner {
    key: ObjectKey,
    /// The span this lease holds — `None` = whole-inode custody. It is the
    /// retire key: the custody list is sorted by `(start, token)`.
    span: Option<(u64, u64)>,
    client_nonce: u64,
    fencing_token: u64,
    released: AtomicBool,
    /// `Some` ⇔ the grant is an owner's (DLM S9): release travels, and
    /// liveness is the owner's answer.
    remote: Option<Arc<dyn RemoteGrant>>,
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
        // S9: the owner's grant is the custody, so the owner is told first
        // — before the local entry disappears, so no local waiter can be
        // woken into an acquire the owner has not yet freed.
        if let Some(remote) = &self.remote {
            remote.release();
        }
        let mut retired = false;
        let mut fence_resolved = 0usize;
        let mut shrink_fence_resolved = 0usize;
        let _ = LOCK_MAP.remove_if_sync(&self.key, |custody| {
            retired = custody.retire(self.span, self.fencing_token, self.client_nonce);
            if retired {
                // Rung 17 (§9.3): a retiring INCUMBENT resolves its
                // un-acked demotion pendings through the FENCE column —
                // its custody over the shared blocks is provably retired
                // (release, revocation, or lease expiry on the owner's
                // clock), so the barrier's waiters may proceed.
                fence_resolved = custody.sweep_pendings_of(self.fencing_token);
                // §9.3a: its shrink pendings resolve the same way — the
                // whole grant died, so the contested tail is trivially
                // retired without an ack.
                shrink_fence_resolved = custody.sweep_shrink_pendings_of(self.fencing_token);
            }
            custody.is_vacant()
        });
        if retired {
            note_record_retired(self.span);
        }
        if fence_resolved > 0 {
            RANGE_DEMOTION_FENCE_RESOLVES.fetch_add(fence_resolved as u64, Ordering::Relaxed);
        }
        if shrink_fence_resolved > 0 {
            RANGE_TAIL_SHRINK_FENCE_RESOLVES
                .fetch_add(shrink_fence_resolved as u64, Ordering::Relaxed);
        }
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

impl std::fmt::Debug for LockLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockLease")
            .field("object", &self.inner.key)
            .field("span", &self.inner.span)
            .field("token", &format_args!("{:#x}", self.inner.fencing_token))
            .field("released", &self.inner.released.load(Ordering::Relaxed))
            // S9: whether the custody is an owner's grant, adopted here.
            .field("remote", &self.inner.remote.is_some())
            .finish()
    }
}

impl LockLease {
    /// Is THIS grant still live in the file's custody table?
    ///
    /// For a **remote** grant (DLM S9) the local entry is a repeat of the
    /// owner's decision, so both must hold: the owner's liveness is
    /// authoritative and a revoked grant reads `false` here the moment the
    /// client learns of it (at its renewal, bounded by `T_self`).
    pub async fn is_held(&self) -> bool {
        // A REMOTE grant's liveness is the OWNER's decision, full stop: the
        // local record is a repeat of it (and, when the authority shares
        // this process, it IS the authority's own record, attached to
        // rather than duplicated — see `adopt_remote_grant`). Consulting the
        // local table too would answer "not held" for a grant that is held,
        // which is the one wrong answer available here.
        if let Some(remote) = &self.inner.remote {
            return remote.live();
        }
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

    /// The span this lease was ACQUIRED over — `None` = whole-inode
    /// custody. NB: a grant widened by the admit-time merge covers MORE
    /// than this (the client range cache carries the current hull); the
    /// acquisition span is the release identity, not the coverage read.
    pub fn span(&self) -> Option<(u64, u64)> {
        self.inner.span
    }

    pub async fn release(self) -> Result<()> {
        self.inner.unlock();
        Ok(())
    }
}
