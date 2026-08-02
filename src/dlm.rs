use crate::error::Result;
use crate::stripe_locks::StripeLocks;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use xxhash_rust::xxh3::xxh3_64;

/// Typed lock/fencing object key.
///
/// The hot path (`inode_{N}` objects) is pure binary — no `format!`, no
/// digit parsing on reads, no heap allocation, integer hashing. String
/// forms exist only at the API boundary (callers pass `&str` paths) and
/// for the rare non-inode object; a networked DLM backend would render
/// these to wire bytes at the transport edge, never on the local path.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ObjectKey {
    /// Whole-file lock on `inode_{0}`.
    Ino(u64),
    /// Byte-range lock on `inode_{0}`: `(ino, start, end)`.
    InoRange(u64, u64, u64),
    /// Whole-object lock on a non-inode path (rare).
    Path(Box<str>),
    /// Byte-range lock on a non-inode path (rare).
    PathRange(Box<str>, u64, u64),
}

impl ObjectKey {
    /// Parse a caller path into its binary form without allocating for the
    /// `inode_{N}` fast path.
    fn from_path(file_path: &str, range: Option<(u64, u64)>) -> Self {
        match (ino_of_path(file_path), range) {
            (Some(ino), None) => Self::Ino(ino),
            (Some(ino), Some((s, e))) => Self::InoRange(ino, s, e),
            (None, None) => Self::Path(file_path.into()),
            (None, Some((s, e))) => Self::PathRange(file_path.into(), s, e),
        }
    }

    /// The fencing generator identity: per *file* object (ranges share the
    /// file's generator, matching the historical `fencing_generator:{path}`
    /// keyspace).
    fn fencing_identity(&self) -> ObjectKey {
        match self {
            Self::Ino(i) | Self::InoRange(i, _, _) => Self::Ino(*i),
            Self::Path(p) | Self::PathRange(p, _, _) => Self::Path(p.clone()),
        }
    }

    /// Stripe selector for the waiter-notify array. Collisions are benign
    /// (spurious wakeups re-check and re-wait); correctness never depends on
    /// this hash.
    fn stripe_seed(&self) -> u64 {
        match self {
            Self::Ino(i) => *i,
            Self::InoRange(i, s, e) => i ^ s.rotate_left(16) ^ e.rotate_left(32),
            Self::Path(p) => xxh3_64(p.as_bytes()),
            Self::PathRange(p, s, e) => {
                xxh3_64(p.as_bytes()) ^ s.rotate_left(16) ^ e.rotate_left(32)
            }
        }
    }
}

/// `inode_{N}` → `N` without allocating. Strict: the entire suffix must be
/// ASCII digits that parse into a `u64`, otherwise the path is treated as an
/// opaque string key (never a lossy alias of some inode).
fn ino_of_path(path: &str) -> Option<u64> {
    let digits = path.strip_prefix("inode_")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One acquire attempt's mint outcome: granted (token), the object is
/// held by someone else, or the era's grant space is exhausted (loud
/// refusal — no entry is inserted, no lock is taken).
enum Mint {
    Granted(u64),
    Held,
    Exhausted(u64),
}

/// A held lock entry: the owner's process-unique nonce plus the fencing
/// token minted at grant. The token is atomic because a byte-range grant
/// on the same FILE identity bumps a live whole-file entry (ranges share
/// the file's generator — the historical `fencing_generator:{path}`
/// semantic the router's stale-token checks are written against).
struct HeldLock {
    owner_nonce: u64,
    token: AtomicU64,
}

/// Lock table: object → held entry. Entries are removed at release
/// (nonce-conditional), so this map is bounded by CONCURRENTLY HELD
/// leases — never by distinct objects ever locked.
static LOCK_MAP: Lazy<scc::HashMap<ObjectKey, HeldLock>> = Lazy::new(scc::HashMap::new);

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

/// This process's adopted durable term (0 = none — see [`DURABLE_TERM`]).
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

/// The product-facing handle name. `LocalLockManager` is the sole
/// implementation until the S4 slot lock manager lands; call sites keep
/// the historical name and S4 swaps this alias for its mode dispatch.
pub type DlmClient = LocalLockManager;

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
            None => Self::read_identity(&ObjectKey::Path(file_path.into())),
        }
    }

    /// Current fencing generation for an inode object — binary fast path.
    pub fn get_fencing_token_ino(&self, ino: u64) -> u64 {
        Self::read_identity(&ObjectKey::Ino(ino))
    }

    /// The identity's readable generation: EXACT while a whole-file lease
    /// is held (the entry token — live writers are never fenced by stripe
    /// collisions), otherwise the stripe floor raised to the current
    /// era's base (≥ the identity's own newest grant; see
    /// `LAST_GRANT_FLOOR`). A never-locked identity reads
    /// [`term_base`] — S1's plain 0 when no durable term is adopted, and
    /// the CURRENT era's floor when one is (S2: that is what makes every
    /// pre-crash stamp stale in a fresh process — spec §6.11).
    fn read_identity(identity: &ObjectKey) -> u64 {
        LOCK_MAP
            .read_sync(identity, |_, h| h.token.load(Ordering::Acquire))
            .unwrap_or_else(|| {
                grant_floor(identity)
                    .load(Ordering::Acquire)
                    .max(term_base())
            })
    }

    /// Acquire an exclusive lease on `file_path` (optionally a byte range),
    /// waiting up to `ttl` for the current holder to release.
    ///
    /// Wait protocol (per attempt):
    /// 1. `enable()` this key's stripe notification **before** checking the
    ///    table — a release landing between the check and the wait is then
    ///    still observed (tokio's documented lost-wakeup discipline).
    /// 2. Try to claim the vacant entry.
    /// 3. Otherwise wait for a stripe release or the deadline. Stripe
    ///    collisions only cause spurious re-checks, never missed wakeups.
    ///
    /// The wait budget is **time** (`ttl`), not wakeup counts: unrelated
    /// churn cannot starve a waiter into a spurious failure, and a quiet
    /// system fails loudly at the deadline instead of hanging.
    pub async fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> Result<LockLease> {
        let key = ObjectKey::from_path(file_path, range);
        let notify = LOCK_WAITERS.get_inode_lock(key.stripe_seed());
        let deadline = tokio::time::Instant::now() + ttl;

        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            // Register interest BEFORE the availability check (lost-wakeup fix).
            notified.as_mut().enable();

            let acquired = match LOCK_MAP.entry_sync(key.clone()) {
                scc::hash_map::Entry::Occupied(_) => Mint::Held,
                scc::hash_map::Entry::Vacant(vac) => {
                    // S1 mint: one global fetch_add — strictly monotone
                    // in grant order, globally unique, gap-carrying.
                    // S2: composed with the durable era, so the sequence
                    // is monotone ACROSS processes too.
                    let seq = GRANT_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
                    if seq > GRANT_SEQ_MAX {
                        // Bit-budget refusal: never carry into the term
                        // field (that would forge a future era). Nothing
                        // is inserted — the caller holds no lock.
                        Mint::Exhausted(seq)
                    } else {
                        let token = compose_token(durable_term(), seq);
                        let _ = vac.insert_entry(HeldLock {
                            owner_nonce: self.client_nonce,
                            token: AtomicU64::new(token),
                        });
                        Mint::Granted(token)
                    }
                }
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
                let identity = key.fencing_identity();
                // Publish the grant to the identity's read surfaces: the
                // stripe floor (unheld reads), and — for a range grant —
                // a live whole-file entry (ranges share the file's
                // generator; the router's stale-token checks construct
                // `stale = range_token - 1` against the whole-file read).
                grant_floor(&identity).fetch_max(fencing_token, Ordering::AcqRel);
                if identity != key {
                    LOCK_MAP.read_sync(&identity, |_, h| {
                        h.token.fetch_max(fencing_token, Ordering::AcqRel);
                    });
                }

                return Ok(LockLease {
                    inner: Arc::new(LockLeaseInner {
                        key,
                        client_nonce: self.client_nonce,
                        fencing_token,
                        released: AtomicBool::new(false),
                    }),
                });
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(crate::error::SqueezefsError::LockFailed {
                    reason: format!("lock {:?} still held after {:?} wait budget", key, ttl),
                });
            }
        }
    }
}

struct LockLeaseInner {
    key: ObjectKey,
    client_nonce: u64,
    fencing_token: u64,
    released: AtomicBool,
}

impl LockLeaseInner {
    /// Single-pass conditional unlock: remove the entry only if this lease's
    /// client still owns it, then wake this key's stripe. Idempotent across
    /// explicit `release()` + final-clone `Drop`.
    fn unlock(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let nonce = self.client_nonce;
        let removed = LOCK_MAP
            .remove_if_sync(&self.key, |held| held.owner_nonce == nonce)
            .is_some();
        if removed {
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
    pub async fn is_held(&self) -> bool {
        LOCK_MAP
            .read_sync(&self.inner.key, |_, held| {
                held.owner_nonce == self.inner.client_nonce
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
