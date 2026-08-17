//! DLM S10 rung 13 — the **client half of UPDATE intents** (KD-MW-13;
//! design-full-multi-writer §8.2 lever 1, the UPDATE arm).
//!
//! A co-writer holding the EXCLUSIVE UPDATE grant on directory D mints
//! children of D **locally**: the ino comes from the grant-carried
//! [`InoSupply`] (an owner cursor reservation — collision-free within the
//! owner's incarnation, era-fenced across it), the name decision comes
//! from the grant-carried dentry **census** (exact under exclusivity — the
//! owner recalls this grant before ANY foreign mutation of D publishes, so
//! a local negative is authoritative and `O_EXCL` is decidable locally),
//! and the FUSE reply is answered from the local intent. The intent then
//! rides an ordered per-owner batch flushed by:
//!
//! * **`fsync(dir)` / `fsync(file)`** — the synchronous CONTRACT POINT
//!   (the §8.2 crash law: an un-flushed batch dies with the client, the
//!   acked-un-fsynced class, disclosed);
//! * **recall** — a foreign reader's lookup/readdir or a foreign mutation
//!   recalls the grant, and the recall FORCES the flush before the ack
//!   (OQ-2's resolved form: coherence over latency on the foreign path);
//! * **the ordering barrier** — any SHIPPED verb naming pending state
//!   (a pending ino, a pending name, a dir with pending ops) flushes
//!   first, so owner-side application order always respects local
//!   causality (a publish or unlink can never overtake the create it
//!   names);
//! * **release / size bound** — asynchronous kicks that keep the
//!   acked-un-fsynced window small without serializing the minter.
//!
//! **The deferred-error law** (§8.2 law 2, the POSIX-16 errseq precedent):
//! an apply refusal latches onto the DIRECTORY, surfaces once at
//! `fsync(dir)`/close, destroys the local mint (`meta_ship_intent_
//! refusals` counts it), and poisons ops on the destroyed child with the
//! owner's errno — never a silent success, never a stale local name
//! surviving the refusal.
//!
//! Solo cost: every entry point gates on one relaxed load (armed) or one
//! relaxed counter load (pending/authority population) — the shipped
//! mount's shape is structurally unchanged (the dark-posture pin).

use super::wire::{
    InoSupply, IntentBatchFrame, IntentBatchReply, IntentCall, IntentGrant, IntentOp,
};
use crate::meta_backend::Inode;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// The S10 UPDATE-intent A/B lever (ENG-10 registry entry; `Kind::Bool`,
/// static default **on**, read only when the mw plane is armed — the
/// `SQUEEZEFS_DELEGATION` form verbatim). `=0` on an armed mount is the
/// tar-x row's control: creates ship exactly as rung 9's.
pub const UPDATE_INTENTS_ENV: &str = "SQUEEZEFS_UPDATE_INTENTS";

/// **Test seam** (the `TEST_DELEGATION_OVERRIDE` form): `0` = read the
/// env knob, `1` = force on, `2` = force off.
pub static TEST_INTENTS_OVERRIDE: AtomicU8 = AtomicU8::new(0);

/// Is the UPDATE-intent plane live on this process? Rides the delegation
/// plane (an UPDATE grant is a delegation): armed + delegation lever +
/// this lever.
pub fn update_intents_enabled() -> bool {
    match TEST_INTENTS_OVERRIDE.load(Ordering::Relaxed) {
        1 => return super::ownership_armed(),
        2 => return false,
        _ => {}
    }
    super::tokens::delegation_enabled() && crate::env_knobs::bool_knob(UPDATE_INTENTS_ENV, true)
}

// ---------------------------------------------------------------------------
// Counters (design §13 spellings + the rung's engagement gauges). The
// owner-face counters are incremented by the service through the
// `note_*` fns so the whole family assembles in ONE place (the
// delegation-stats pattern).
// ---------------------------------------------------------------------------

static BATCHES: AtomicU64 = AtomicU64::new(0);
static VERBS: AtomicU64 = AtomicU64::new(0);
static FLUSH_FORCES: AtomicU64 = AtomicU64::new(0);
static REFUSALS: AtomicU64 = AtomicU64::new(0);
static MINTS: AtomicU64 = AtomicU64::new(0);
static DECLINES: AtomicU64 = AtomicU64::new(0);
static LOCAL_EEXIST: AtomicU64 = AtomicU64::new(0);
static LOCAL_NEGATIVES: AtomicU64 = AtomicU64::new(0);
static DEFERRED_SETATTRS: AtomicU64 = AtomicU64::new(0);
static MINT_DESTROYS: AtomicU64 = AtomicU64::new(0);
static APPLIED: AtomicU64 = AtomicU64::new(0);
static REPLAYS: AtomicU64 = AtomicU64::new(0);
static STALE_REFUSALS: AtomicU64 = AtomicU64::new(0);
static READ_RECALLS: AtomicU64 = AtomicU64::new(0);
static UPDATE_GRANTS: AtomicU64 = AtomicU64::new(0);
static UPDATE_DECLINES: AtomicU64 = AtomicU64::new(0);

/// GLOBAL pending-op population (queued + in a flush's flight) — the
/// one-relaxed-load fast gate every probe/barrier pays when idle.
static PENDING_OPS: AtomicU64 = AtomicU64::new(0);
/// GLOBAL held-authority population — the mint/census fast gate.
static AUTHORITIES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_intent_applied() {
    APPLIED.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn note_intent_replay() {
    REPLAYS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn note_intent_stale_refusal() {
    STALE_REFUSALS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn note_intent_read_recall() {
    READ_RECALLS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn note_update_grant() {
    UPDATE_GRANTS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn note_update_decline() {
    UPDATE_DECLINES.fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// The wire identity a lane flushes with (captured from the router at the
/// first grant absorb).
#[derive(Clone)]
pub(crate) struct LaneCtx {
    pub peer_id: Arc<str>,
    pub secret: Arc<Vec<u8>>,
    pub client_epoch: u64,
}

/// One held EXCLUSIVE UPDATE authority over a directory.
struct Authority {
    seq: u64,
    term: u64,
    /// D's attributes as this client believes them: the grant image plus
    /// this client's own folds (mint Δtimes, mkdir Δnlink). Exact under
    /// exclusivity — a foreign mutation of D recalls this grant first —
    /// and the LOCAL parent-attr serve built on it deletes the measured
    /// one-shipped-getattr-per-create (the FUSE D2.c parent refresh).
    attrs: Inode,
    /// D's name set as this client believes it: the grant census + every
    /// name this client applied since. Exact under exclusivity.
    census: std::collections::HashSet<String>,
}

/// One pre-reserved mint supply (per owner lane — pooled across that
/// owner's authorities; placement is PR-14's lever).
struct Supply {
    next: u64,
    end: u64,
    stride: u64,
    /// The owner era the reservation was minted under: the flush presents
    /// it as the frame's `owner_term`, so a successor's era gate refuses
    /// every stale-supply apply before its recovered cursor could collide.
    term: u64,
}

struct QueuedIntent {
    request_id: u64,
    /// The directory whose deferred-refusal latch a failure lands on.
    dir: u64,
    call: IntentCall,
}

/// One owner's intent lane.
struct Lane {
    ctx: Option<LaneCtx>,
    queue: VecDeque<QueuedIntent>,
    /// Pending images: minted ino → the answered inode.
    images: HashMap<u64, Inode>,
    /// Pending names: (dir, name) → minted ino.
    names: HashMap<(u64, String), u64>,
    /// Held authorities: dir → census + grant identity.
    authorities: HashMap<u64, Authority>,
    supply: Option<Supply>,
    /// The deferred-refusal latch (errseq: reported once at fsync(dir)).
    err_latch: HashMap<u64, i32>,
    /// Destroyed mints: ino → the owner's errno (poisons ops on the
    /// child; FIFO-bounded).
    destroyed: HashMap<u64, i32>,
    destroyed_order: VecDeque<u64>,
    /// One in-flight flush per lane (order preservation).
    flushing: bool,
    /// Terminal: the owner fenced this holder (era/custody/deleg fence) —
    /// pending intents were latched + destroyed; re-admission by remount.
    fenced: bool,
    /// A cached wire session the flushing winner takes/puts.
    session: Option<crate::cluster_wire::RpcClient>,
    /// One scheduled release-kick per lane at a time (the coalescer).
    kick_scheduled: bool,
}

impl Lane {
    fn new() -> Self {
        Self {
            ctx: None,
            queue: VecDeque::new(),
            images: HashMap::new(),
            names: HashMap::new(),
            authorities: HashMap::new(),
            supply: None,
            err_latch: HashMap::new(),
            destroyed: HashMap::new(),
            destroyed_order: VecDeque::new(),
            flushing: false,
            fenced: false,
            session: None,
            kick_scheduled: false,
        }
    }
}

struct IntentState {
    /// endpoint → lane.
    lanes: HashMap<String, Lane>,
}

static STATE: Lazy<Mutex<IntentState>> = Lazy::new(|| {
    Mutex::new(IntentState {
        lanes: HashMap::new(),
    })
});

/// Wakes flush waiters (a flush completed or the queue moved).
static FLUSH_NOTIFY: Lazy<squeezefs_ipc::sqz_notify::Notify> =
    Lazy::new(squeezefs_ipc::sqz_notify::Notify::new);

/// Destroyed-mint poison map bound: enough to cover any live handle
/// population; past it the oldest poison ages out (its ops then answer
/// ENOENT — the record never existed — which is still never a silent
/// success).
const DESTROYED_CAP: usize = 1024;

/// The flush batch cap: the ship lane's own derived frame cap (one law).
fn batch_cap() -> usize {
    super::router::batch_max()
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Grant absorption (the router's reply hook)
// ---------------------------------------------------------------------------

/// Absorb a piggybacked UPDATE grant (census + supply) from `endpoint`'s
/// reply. Monotone per directory: an older `(term, seq)` never displaces
/// a newer authority.
pub(crate) fn absorb_intent_grant(endpoint: &str, ctx: LaneCtx, grant: &IntentGrant) {
    if !update_intents_enabled() {
        return;
    }
    let mut st = STATE.lock();
    let lane = st
        .lanes
        .entry(endpoint.to_string())
        .or_insert_with(Lane::new);
    if lane.fenced {
        return;
    }
    lane.ctx.get_or_insert(ctx);
    match lane.authorities.get(&grant.dir) {
        Some(a) if (a.term, a.seq) >= (grant.term, grant.seq) => return,
        Some(_) => {}
        None => {
            AUTHORITIES.fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut attrs = Inode::from(grant.dir_attrs);
    attrs.ino = grant.dir;
    lane.authorities.insert(
        grant.dir,
        Authority {
            seq: grant.seq,
            term: grant.term,
            attrs,
            census: grant.census.iter().cloned().collect(),
        },
    );
    if let Some(s) = &grant.supply {
        absorb_supply_locked(lane, s, grant.term);
    }
}

fn absorb_supply_locked(lane: &mut Lane, s: &InoSupply, term: u64) {
    // One pool per owner: adopt the fresh chunk when the current pool is
    // drained or from an older era (never merge ranges — the drained
    // remainder burns, §4.8).
    let fresh = Supply {
        next: s.first_global,
        end: s.first_global + u64::from(s.count) * s.stride,
        stride: s.stride,
        term,
    };
    match &lane.supply {
        Some(cur) if cur.term == term && cur.next < cur.end => {}
        _ => lane.supply = Some(fresh),
    }
}

/// Is an authority LIVE right now — recall channel fresh (a recall can
/// reach us promptly, so exclusivity is enforceable) and era-current (a
/// failover's survivors never mint across a term)? The rung-12 serve
/// gates, applied to mint authority.
fn authority_live(endpoint: &str, auth: &Authority) -> bool {
    super::tokens::deleg_channel_fresh(endpoint)
        && auth.term
            == super::tokens::deleg_channel(endpoint)
                .term
                .load(Ordering::Acquire)
}

/// Does this process hold a LIVE UPDATE authority over `dir`? (Test +
/// engagement surface.)
pub fn holds_update_authority(dir: u64) -> bool {
    if AUTHORITIES.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let st = STATE.lock();
    st.lanes.iter().any(|(ep, l)| {
        l.authorities
            .get(&dir)
            .is_some_and(|a| authority_live(ep, a))
    })
}

// ---------------------------------------------------------------------------
// The local mint + probes (the router's serve hooks)
// ---------------------------------------------------------------------------

/// A local mint attempt's outcome.
pub(crate) enum MintOutcome {
    /// Not eligible (no authority / lever off / supply dry / flush
    /// needed): the caller ships as today — the priced fallback.
    NotEligible,
    /// The census (∪ pending) already names it — the authoritative local
    /// EEXIST (`O_EXCL` exactness).
    Exists,
    /// Minted: the FUSE reply's inode, acked locally, queued for the
    /// batch.
    Minted(Inode),
}

/// Try to mint `(parent, name)` locally under the UPDATE authority (an
/// authority implies an absorbed grant, which set the lane's ctx).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_mint_create(
    endpoint: &str,
    parent: u64,
    name: &str,
    mode: u32,
    uid: u32,
    gid: u32,
    rdev: u32,
    initial_size: u64,
) -> MintOutcome {
    if AUTHORITIES.load(Ordering::Relaxed) == 0 || !update_intents_enabled() {
        return MintOutcome::NotEligible;
    }
    // "." components never mint; the FUSE layer never sends them here —
    // defensive only.
    if name.is_empty() || name == "." || name == ".." {
        return MintOutcome::NotEligible;
    }
    let mut st = STATE.lock();
    let Some(lane) = st.lanes.get_mut(endpoint) else {
        return MintOutcome::NotEligible;
    };
    if lane.fenced {
        return MintOutcome::NotEligible;
    }
    let Some(auth) = lane.authorities.get(&parent) else {
        return MintOutcome::NotEligible;
    };
    if !authority_live(endpoint, auth) {
        DECLINES.fetch_add(1, Ordering::Relaxed);
        return MintOutcome::NotEligible;
    }
    // The O_EXCL decision (§8.2 law 1): census ∪ pending is D's EXACT
    // name set under exclusivity.
    if auth.census.contains(name) || lane.names.contains_key(&(parent, name.to_string())) {
        LOCAL_EEXIST.fetch_add(1, Ordering::Relaxed);
        return MintOutcome::Exists;
    }
    // The number: one supply draw. A dry pool declines (the flush's
    // supply_request refills it).
    let Some(supply) = lane.supply.as_mut() else {
        DECLINES.fetch_add(1, Ordering::Relaxed);
        return MintOutcome::NotEligible;
    };
    if supply.next >= supply.end || supply.term != auth.term {
        DECLINES.fetch_add(1, Ordering::Relaxed);
        return MintOutcome::NotEligible;
    }
    let ino = supply.next;
    supply.next += supply.stride;

    // The image: setgid inheritance computed from the grant-carried
    // parent attrs (exclusivity keeps them current; the apply re-computes
    // and matches).
    let is_dir = (mode & libc::S_IFMT) == libc::S_IFDIR;
    let mut final_mode = mode;
    let mut final_gid = gid;
    if (auth.attrs.mode & libc::S_ISGID) != 0 {
        final_gid = auth.attrs.gid;
        if is_dir {
            final_mode |= libc::S_ISGID;
        }
    }
    let ts = now_ns();
    let inode = Inode {
        ino,
        mode: final_mode,
        uid,
        gid: final_gid,
        size: initial_size,
        nlink: if is_dir { 2 } else { 1 },
        atime: ts,
        mtime: ts,
        ctime: ts,
        flags: 0,
        rdev,
    };
    lane.queue.push_back(QueuedIntent {
        request_id: next_request_id(),
        dir: parent,
        call: IntentCall::CreateAt {
            parent,
            name: name.to_string(),
            ino,
            mode,
            uid,
            gid,
            rdev,
            initial_size,
            ts_ns: ts,
        },
    });
    lane.images.insert(ino, inode.clone());
    lane.names.insert((parent, name.to_string()), ino);
    // Fold the mint into the parent image (the owner's apply stamps the
    // same Δ — times from the mint instant, nlink for a mkdir).
    if let Some(auth) = lane.authorities.get_mut(&parent) {
        auth.attrs.mtime = ts;
        auth.attrs.ctime = ts;
        if is_dir {
            auth.attrs.nlink += 1;
        }
    }
    PENDING_OPS.fetch_add(1, Ordering::Relaxed);
    MINTS.fetch_add(1, Ordering::Relaxed);
    if lane.queue.len() >= batch_cap() {
        // The size bound: an asynchronous kick (never blocks the minter).
        drop(st);
        kick_flush(endpoint);
    }
    MintOutcome::Minted(inode)
}

/// Try to DEFER a setattr on a PENDING ino into the batch (the tar
/// `utimensat` shape). `None` = the ino is not pending here — the caller
/// takes the normal path. Size changes never defer (data-plane act).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_defer_setattr(
    endpoint: &str,
    ino: u64,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    atime: Option<u64>,
    mtime: Option<u64>,
    ctime: Option<u64>,
) -> Option<Inode> {
    if PENDING_OPS.load(Ordering::Relaxed) == 0 || size.is_some() {
        return None;
    }
    let mut st = STATE.lock();
    let lane = st.lanes.get_mut(endpoint)?;
    let dir = lane
        .queue
        .iter()
        .find_map(|q| match &q.call {
            IntentCall::CreateAt { ino: i, parent, .. } if *i == ino => Some(*parent),
            _ => None,
        })
        .or_else(|| {
            lane.images.contains_key(&ino).then(|| {
                lane.names
                    .iter()
                    .find_map(|((d, _), i)| (*i == ino).then_some(*d))
                    .unwrap_or(1)
            })
        })?;
    let image = lane.images.get_mut(&ino)?;
    if let Some(m) = mode {
        image.mode = (image.mode & libc::S_IFMT) | (m & !(libc::S_IFMT));
    }
    if let Some(u) = uid {
        image.uid = u;
    }
    if let Some(g) = gid {
        image.gid = g;
    }
    if let Some(a) = atime {
        image.atime = a;
    }
    if let Some(m) = mtime {
        image.mtime = m;
    }
    if let Some(c) = ctime {
        image.ctime = c;
    }
    let out = image.clone();
    lane.queue.push_back(QueuedIntent {
        request_id: next_request_id(),
        dir,
        call: IntentCall::SetattrAt {
            ino,
            mode,
            uid,
            gid,
            atime,
            mtime,
            ctime,
        },
    });
    PENDING_OPS.fetch_add(1, Ordering::Relaxed);
    DEFERRED_SETATTRS.fetch_add(1, Ordering::Relaxed);
    Some(out)
}

/// A pending-name lookup probe's outcome.
pub(crate) enum LookupProbe {
    /// No authority over the parent (and no pending name): the caller's
    /// normal path.
    None,
    /// The pending image (a locally-acked, not-yet-applied child).
    Image(Inode),
    /// The census says the name EXISTS but it is not pending here (a
    /// pre-grant or applied name): SHIP the lookup (owner-current answer)
    /// — never serve a view that may lag our own applies.
    Ship,
    /// The census says the name does not exist: the authoritative local
    /// negative.
    Negative,
}

/// Probe `(parent, name)` against pending state + the census.
pub(crate) fn lookup_probe(endpoint: &str, parent: u64, name: &str) -> LookupProbe {
    if AUTHORITIES.load(Ordering::Relaxed) == 0 && PENDING_OPS.load(Ordering::Relaxed) == 0 {
        return LookupProbe::None;
    }
    let st = STATE.lock();
    let Some(lane) = st.lanes.get(endpoint) else {
        return LookupProbe::None;
    };
    if let Some(ino) = lane.names.get(&(parent, name.to_string())) {
        if let Some(image) = lane.images.get(ino) {
            return LookupProbe::Image(image.clone());
        }
    }
    let Some(auth) = lane.authorities.get(&parent) else {
        return LookupProbe::None;
    };
    if !authority_live(endpoint, auth) {
        return LookupProbe::None;
    }
    if auth.census.contains(name) {
        LookupProbe::Ship
    } else {
        LOCAL_NEGATIVES.fetch_add(1, Ordering::Relaxed);
        LookupProbe::Negative
    }
}

/// The LOCAL parent-attr serve (the measured live finding's fix): a
/// LIVE UPDATE authority answers its directory's getattr from the folded
/// grant image — the FUSE create handler's D2.c parent refresh would
/// otherwise ship one getattr per minted create.
pub(crate) fn authority_attr_probe(endpoint: &str, ino: u64) -> Option<Inode> {
    if AUTHORITIES.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let st = STATE.lock();
    let lane = st.lanes.get(endpoint)?;
    let auth = lane.authorities.get(&ino)?;
    if !authority_live(endpoint, auth) {
        return None;
    }
    Some(auth.attrs.clone())
}

/// The pending image of `ino`, if any (the getattr probe).
pub(crate) fn pending_image(ino: u64) -> Option<Inode> {
    if PENDING_OPS.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let st = STATE.lock();
    st.lanes.values().find_map(|l| l.images.get(&ino).cloned())
}

/// The poison errno of a DESTROYED mint (§8.2: ops on the child answer
/// the owner's errno).
pub(crate) fn destroyed_errno(ino: u64) -> Option<i32> {
    let st = STATE.lock();
    st.lanes
        .values()
        .find_map(|l| l.destroyed.get(&ino).copied())
}

/// Does the UPDATE authority over `dir` belong to this process, and is a
/// delegated-LOOKUP serve of `dir`'s dentries therefore FORBIDDEN (the
/// census governs names; the view may lag our own applies)?
pub(crate) fn authority_governs(endpoint: &str, dir: u64) -> bool {
    if AUTHORITIES.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let st = STATE.lock();
    // Deliberately liveness-BLIND: while ANY authority record exists for
    // `dir`, the reader-view delegated serve stays forbidden (the view
    // may lag our own applies) — a dead channel makes serves ship, never
    // regress to a possibly-stale view negative.
    st.lanes
        .get(endpoint)
        .is_some_and(|l| l.authorities.contains_key(&dir))
}

// ---------------------------------------------------------------------------
// Barriers (the ordering law) + the fsync contract points
// ---------------------------------------------------------------------------

/// Does any pending state name one of `inos` or the `(dir, name)` pairs —
/// i.e. must the caller flush before shipping? One relaxed load when
/// nothing is pending anywhere.
pub(crate) fn barrier_needed(inos: &[u64], pairs: &[(u64, &str)]) -> bool {
    if PENDING_OPS.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let st = STATE.lock();
    for lane in st.lanes.values() {
        for &ino in inos {
            if lane.images.contains_key(&ino)
                || lane.queue.iter().any(|q| q.dir == ino)
                || lane.names.keys().any(|(d, _)| *d == ino)
            {
                return true;
            }
        }
        for (dir, name) in pairs {
            if lane.names.contains_key(&(*dir, (*name).to_string())) {
                return true;
            }
        }
    }
    false
}

/// Flush EVERY lane with pending intents and wait (the synchronous
/// force). `reason` is the ledger row it lands on.
pub(crate) async fn flush_all(force: bool) -> Result<(), i32> {
    if PENDING_OPS.load(Ordering::Relaxed) == 0 {
        return Ok(());
    }
    if force {
        FLUSH_FORCES.fetch_add(1, Ordering::Relaxed);
    }
    let endpoints: Vec<String> = {
        let st = STATE.lock();
        st.lanes
            .iter()
            .filter(|(_, l)| !l.queue.is_empty() || l.flushing)
            .map(|(e, _)| e.clone())
            .collect()
    };
    let mut first_err: Option<i32> = None;
    for ep in endpoints {
        if let Err(e) = flush_owner(&ep).await {
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// `fsync(dir)` — the §8.2 CONTRACT POINT: flush everything, then report
/// (and consume — errseq) the directory's deferred-refusal latch.
pub async fn fsync_dir_barrier(dir: u64) -> Result<(), i32> {
    if PENDING_OPS.load(Ordering::Relaxed) > 0 {
        FLUSH_FORCES.fetch_add(1, Ordering::Relaxed);
        flush_all(false).await?;
    }
    let mut st = STATE.lock();
    for lane in st.lanes.values_mut() {
        if let Some(errno) = lane.err_latch.remove(&dir) {
            return Err(errno);
        }
    }
    Ok(())
}

/// `fsync(file)` on an ino that may be a pending/destroyed mint: flush
/// first (the file's existence must be durable for its data durability to
/// mean anything), then report a destroyed mint's poison.
pub async fn fsync_ino_barrier(ino: u64) -> Result<(), i32> {
    if PENDING_OPS.load(Ordering::Relaxed) > 0 && pending_image(ino).is_some() {
        FLUSH_FORCES.fetch_add(1, Ordering::Relaxed);
        flush_all(false).await?;
    }
    match destroyed_errno(ino) {
        Some(errno) => Err(errno),
        None => Ok(()),
    }
}

/// An asynchronous flush kick (the size bound): never blocks the caller;
/// the per-lane `flushing` flag self-coalesces bursts into one frame (the
/// conveyor shape).
pub fn kick_flush(endpoint: &str) {
    let ep = endpoint.to_string();
    crate::meta_exec::spawn_meta("meta_ship_intent_flush", async move {
        let _ = flush_owner(&ep).await;
    });
}

/// The RELEASE kick's coalescing delay: the §8.2 item-3 "batch-flush
/// latency" bound for the close-triggered flush. An IMMEDIATE per-release
/// flush was the first live-leg finding's shape — it raced ahead of tar's
/// post-close `utimensat`, so every explicit-time set arrived at an
/// ALREADY-APPLIED ino and shipped (2 wire verbs per file on a
/// zero-wire path), and it collapsed the batch to one frame per file.
/// The delay derives from the negative-TTL knob (the same term the
/// published visibility bound is built from — one law, no new constant);
/// foreign READS stay exact regardless (OQ-2 recalls force the flush),
/// so the delay only bounds the MW-8 acked-un-fsynced window.
fn release_kick_delay_ms() -> u64 {
    crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_FUSE_NEGATIVE_TTL_MS").unwrap_or(1000)
}

/// Kick every lane holding the given pending ino (the RELEASE hook) —
/// DELAYED and coalescing: one scheduled flush per lane at a time, so a
/// tar stream's closes fold into one frame per delay window instead of
/// one frame per file.
pub fn kick_if_pending(ino: u64) {
    if PENDING_OPS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let eps: Vec<String> = {
        let mut st = STATE.lock();
        let mut eps = Vec::new();
        for (ep, lane) in st.lanes.iter_mut() {
            if lane.images.contains_key(&ino) && !lane.kick_scheduled {
                lane.kick_scheduled = true;
                eps.push(ep.clone());
            }
        }
        eps
    };
    for ep in eps {
        let delay = release_kick_delay_ms();
        crate::meta_exec::spawn_meta("meta_ship_intent_release_kick", async move {
            squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(delay)).await;
            {
                let mut st = STATE.lock();
                if let Some(lane) = st.lanes.get_mut(&ep) {
                    lane.kick_scheduled = false;
                }
            }
            let _ = flush_owner(&ep).await;
        });
    }
}

// ---------------------------------------------------------------------------
// The flush (one in-flight frame per lane; order-preserving; witnessed)
// ---------------------------------------------------------------------------

/// Bounded resend budget for one witnessed intent frame — the
/// `PUBLISH_SHIP_ATTEMPTS` law verbatim: a protocol constant; each resend
/// is absorbed exactly-once by the owner's witness window.
const INTENT_SHIP_ATTEMPTS: u32 = 3;

/// The supply refill chunk (also the initial grant chunk owner-side —
/// [`super::service`] derives the same number, one law): 4 flush windows
/// of mints, floored for small trees, capped so a dead grant burns a
/// bounded range. Unused inos burn free (§4.8).
pub(crate) fn supply_chunk() -> u32 {
    let pinned = super::service::TEST_INTENT_SUPPLY_CHUNK.load(Ordering::Relaxed);
    if pinned > 0 {
        return pinned;
    }
    (batch_cap() as u32).saturating_mul(4).clamp(256, 65_536)
}

/// Flush one owner lane: drain the queue into ONE frame (order
/// preserved), send under the bounded witnessed retry, absorb per-op
/// outcomes (successes fold into the census; refusals latch + destroy).
pub(crate) async fn flush_owner(endpoint: &str) -> Result<(), i32> {
    loop {
        // The flush slot is single-occupancy per lane (order): wait out a
        // running flush WITHOUT holding the state lock across the await.
        enum Slot {
            Wait,
            Done,
            Fenced,
            Take,
        }
        let slot = {
            let st = STATE.lock();
            match st.lanes.get(endpoint) {
                None => Slot::Done,
                Some(l) if l.fenced => Slot::Fenced,
                Some(l) if l.flushing => Slot::Wait,
                Some(l) if l.queue.is_empty() || l.ctx.is_none() => Slot::Done,
                Some(_) => Slot::Take,
            }
        };
        match slot {
            Slot::Done => return Ok(()),
            Slot::Fenced => return Err(libc::EIO),
            Slot::Wait => {
                let notified = FLUSH_NOTIFY.notified();
                // Bounded: a wake that slipped past the registration only
                // costs one tick (the loop re-checks).
                let _ = squeezefs_ipc::sqz_time::timeout(
                    std::time::Duration::from_millis(50),
                    notified,
                )
                .await;
                continue;
            }
            Slot::Take => {}
        }
        // Take the batch (re-checked under the lock — a racer may have
        // taken the slot between the two scopes).
        let taken = {
            let mut st = STATE.lock();
            match st.lanes.get_mut(endpoint) {
                Some(lane) if !lane.flushing && !lane.queue.is_empty() && lane.ctx.is_some() => {
                    lane.flushing = true;
                    let cap = batch_cap();
                    let take = lane.queue.len().min(cap);
                    let batch: Vec<QueuedIntent> = lane.queue.drain(..take).collect();
                    // The frame's era = the supply's era (a dead era's
                    // numbers must never apply — see `InoSupply`);
                    // authorities and supply share the term by
                    // construction.
                    let term = lane
                        .supply
                        .as_ref()
                        .map(|s| s.term)
                        .or_else(|| lane.authorities.values().next().map(|a| a.term))
                        .unwrap_or(0);
                    let want = match &lane.supply {
                        Some(s)
                            if (s.end - s.next) / s.stride.max(1)
                                < u64::from(supply_chunk()) / 2 =>
                        {
                            supply_chunk()
                        }
                        None if !lane.authorities.is_empty() => supply_chunk(),
                        _ => 0,
                    };
                    let ctx = lane.ctx.clone().expect("checked above");
                    let session = lane.session.take();
                    Some((batch, ctx, term, want, session))
                }
                _ => None,
            }
        };
        let Some((batch, ctx, term, want_supply, mut session)) = taken else {
            continue;
        };
        let lease_epoch = crate::data_grant::custody_client()
            .map(|c| c.lease_epoch())
            .unwrap_or(0);

        let ops: Vec<IntentOp> = batch
            .iter()
            .map(|q| IntentOp {
                request_id: q.request_id,
                call: q.call.clone(),
            })
            .collect();
        let frame = IntentBatchFrame {
            schema: super::wire::META_SHIP_SCHEMA,
            client_epoch: ctx.client_epoch,
            client_id: ctx.peer_id.to_string(),
            owner_term: term,
            lease_epoch,
            supply_request: want_supply,
            ops,
        };
        let body = match super::wire::encode_intent_batch(&frame) {
            Ok(b) => b,
            Err(e) => {
                log::error!("S10 intents: frame encode failed ({e}) — latching the batch");
                fail_batch(endpoint, batch, libc::EIO, true, session);
                return Err(libc::EIO);
            }
        };

        // The bounded, epoch-stable witnessed resend (same ids, same
        // frame — the owner's window absorbs duplicates).
        let mut outcome: Option<cw::Reply> = None;
        let mut attempt = 0u32;
        while attempt < INTENT_SHIP_ATTEMPTS {
            if session.is_none() {
                match crate::cluster_wire::RpcClient::connect(
                    endpoint,
                    &ctx.secret,
                    &ctx.peer_id,
                    None,
                )
                .await
                {
                    Ok(c) => session = Some(c),
                    Err(e) => {
                        log::debug!("S10 intents: connect to {endpoint} failed ({e})");
                        attempt += 1;
                        continue;
                    }
                }
            }
            match session
                .as_mut()
                .expect("connected above")
                .call(super::wire::VERB_DELEG_INTENT, body.clone())
                .await
            {
                Ok(reply) => {
                    outcome = Some(cw::Reply {
                        status: reply.status,
                        body: reply.body,
                    });
                    break;
                }
                Err(e) => {
                    log::debug!("S10 intents: flush to {endpoint} failed ({e}) — resending");
                    session = None;
                    attempt += 1;
                }
            }
        }

        let Some(reply) = outcome else {
            // Transport-dead: the ops go BACK (order preserved) — a
            // transient outage must not destroy acked mints; the fsync
            // caller sees EIO and may retry, and the recall deadline is
            // the owner-side bound.
            let mut st = STATE.lock();
            if let Some(lane) = st.lanes.get_mut(endpoint) {
                for q in batch.into_iter().rev() {
                    lane.queue.push_front(q);
                }
                lane.flushing = false;
                lane.session = None;
            }
            FLUSH_NOTIFY.notify_waiters();
            return Err(libc::EIO);
        };

        match reply.status {
            s if s == crate::cluster_wire::RPC_OK => {
                let decoded = match super::wire::decode_intent_batch_reply(&reply.body) {
                    Ok(d) => d,
                    Err(e) => {
                        log::error!("S10 intents: undecodable flush reply ({e})");
                        fail_batch(endpoint, batch, libc::EIO, true, session);
                        return Err(libc::EIO);
                    }
                };
                BATCHES.fetch_add(1, Ordering::Relaxed);
                VERBS.fetch_add(batch.len() as u64, Ordering::Relaxed);
                absorb_flush_reply(endpoint, batch, &decoded, session).await;
                // Loop: more may have queued behind the frame.
                if PENDING_OPS.load(Ordering::Relaxed) == 0 {
                    return Ok(());
                }
                let more = {
                    let st = STATE.lock();
                    st.lanes.get(endpoint).is_some_and(|l| !l.queue.is_empty())
                };
                if !more {
                    return Ok(());
                }
            }
            s if s == super::wire::STATUS_STALE_TERM
                || s == super::wire::STATUS_INTENT_LEASE
                || s == super::wire::STATUS_DELEG_FENCED =>
            {
                // Terminal era/custody/deleg fence: the batch DIES (§8.2's
                // error channel — latched + destroyed, never silently
                // absorbed), authorities and supply die with it.
                log::error!(
                    "S10 intents: owner {endpoint} refused the batch with status {s} — the era \
                     is dead; latching {} intent(s) and dropping the mint authority",
                    batch.len()
                );
                fail_batch(endpoint, batch, libc::EIO, true, session);
                return Err(libc::EIO);
            }
            other => {
                log::error!(
                    "S10 intents: owner {endpoint} refused the batch (status {other}): {}",
                    String::from_utf8_lossy(&reply.body)
                );
                fail_batch(endpoint, batch, libc::EIO, true, session);
                return Err(libc::EIO);
            }
        }
    }
}

mod cw {
    /// A tiny local reply carrier (status + body) so the flush loop can
    /// treat transport and status outcomes uniformly.
    pub(super) struct Reply {
        pub status: u16,
        pub body: Vec<u8>,
    }
}

/// Absorb one flush reply: successes fold into the census (an applied
/// name IS a census name); refusals latch onto their directory and
/// destroy the mint; the supply refill and reply-revokes ride the same
/// pass.
async fn absorb_flush_reply(
    endpoint: &str,
    batch: Vec<QueuedIntent>,
    reply: &IntentBatchReply,
    session: Option<crate::cluster_wire::RpcClient>,
) {
    let mut revokes: Vec<(u64, u64)> = Vec::new(); // (ino, fence)
    {
        let mut st = STATE.lock();
        let Some(lane) = st.lanes.get_mut(endpoint) else {
            return;
        };
        let by_id: HashMap<u64, &super::wire::IntentResult> =
            reply.results.iter().map(|r| (r.request_id, r)).collect();
        for q in &batch {
            PENDING_OPS.fetch_sub(1, Ordering::Relaxed);
            let result = by_id.get(&q.request_id);
            let ok = result.map(|r| r.outcome.is_ok()).unwrap_or(false);
            if let Some(r) = result {
                for &ino in &r.revokes {
                    revokes.push((ino, r.revoke_fence));
                }
            }
            match &q.call {
                IntentCall::CreateAt {
                    parent, name, ino, ..
                } => {
                    lane.names.remove(&(*parent, name.clone()));
                    lane.images.remove(ino);
                    if ok {
                        if let Some(auth) = lane.authorities.get_mut(parent) {
                            auth.census.insert(name.clone());
                        }
                    } else {
                        let errno = result
                            .map(|r| match &r.outcome {
                                Err(e) => e.errno,
                                Ok(()) => libc::EIO,
                            })
                            .unwrap_or(libc::EIO);
                        REFUSALS.fetch_add(1, Ordering::Relaxed);
                        MINT_DESTROYS.fetch_add(1, Ordering::Relaxed);
                        lane.err_latch.entry(q.dir).or_insert(errno);
                        lane.destroyed.insert(*ino, errno);
                        lane.destroyed_order.push_back(*ino);
                        if lane.destroyed_order.len() > DESTROYED_CAP {
                            if let Some(old) = lane.destroyed_order.pop_front() {
                                lane.destroyed.remove(&old);
                            }
                        }
                        fire_destroy_inval(*ino);
                        log::error!(
                            "S10 intents: apply of '{name}' in dir {parent} REFUSED \
                             (errno {errno}) — deferred to fsync(dir) per the §8.2 error \
                             channel; the local mint (ino {ino}) is destroyed"
                        );
                    }
                }
                IntentCall::SetattrAt { .. } => {
                    if !ok {
                        let errno = result
                            .map(|r| match &r.outcome {
                                Err(e) => e.errno,
                                Ok(()) => libc::EIO,
                            })
                            .unwrap_or(libc::EIO);
                        REFUSALS.fetch_add(1, Ordering::Relaxed);
                        lane.err_latch.entry(q.dir).or_insert(errno);
                    }
                }
            }
        }
        if let Some(s) = &reply.supply {
            absorb_supply_locked(lane, s, reply.owner_term);
        }
        lane.flushing = false;
        lane.session = session;
    }
    FLUSH_NOTIFY.notify_waiters();
    // Reply-ridden revocations of this holder's own LOOKUP-class entries
    // (the apply's self-conflict surrenders) — dropped exactly as an S8
    // reply's revokes are, through the no-intents arm (a flush's reply
    // must never recurse into another flush).
    for (ino, fence) in revokes {
        super::tokens::revoke_delegations_no_intents(
            endpoint,
            &[ino],
            0,
            fence,
            super::tokens::RevokeKind::Reply,
        )
        .await;
    }
}

fn fire_destroy_inval(ino: u64) {
    // The kernel half of "destroys the local mint": the entry the create
    // reply cached ages out within the entry TTL; the inode invalidation
    // rides the delegation sink (installed by the mount).
    super::tokens::fire_deleg_inval(ino);
}

/// Fail a whole batch (a terminal refusal or an unencodable frame): every
/// op latches + destroys; TERMINAL fences also drop the lane's
/// authorities and supply.
fn fail_batch(
    endpoint: &str,
    batch: Vec<QueuedIntent>,
    errno: i32,
    terminal: bool,
    session: Option<crate::cluster_wire::RpcClient>,
) {
    let mut inval = Vec::new();
    {
        let mut st = STATE.lock();
        let Some(lane) = st.lanes.get_mut(endpoint) else {
            return;
        };
        for q in &batch {
            PENDING_OPS.fetch_sub(1, Ordering::Relaxed);
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            lane.err_latch.entry(q.dir).or_insert(errno);
            if let IntentCall::CreateAt {
                parent, name, ino, ..
            } = &q.call
            {
                MINT_DESTROYS.fetch_add(1, Ordering::Relaxed);
                lane.names.remove(&(*parent, name.clone()));
                lane.images.remove(ino);
                lane.destroyed.insert(*ino, errno);
                lane.destroyed_order.push_back(*ino);
                if lane.destroyed_order.len() > DESTROYED_CAP {
                    if let Some(old) = lane.destroyed_order.pop_front() {
                        lane.destroyed.remove(&old);
                    }
                }
                inval.push(*ino);
            }
        }
        if terminal {
            AUTHORITIES.fetch_sub(lane.authorities.len() as u64, Ordering::Relaxed);
            lane.authorities.clear();
            lane.supply = None;
            lane.fenced = true;
        }
        lane.flushing = false;
        lane.session = session;
    }
    FLUSH_NOTIFY.notify_waiters();
    for ino in inval {
        fire_destroy_inval(ino);
    }
}

// ---------------------------------------------------------------------------
// Revocation (the recall-forces-flush hook) + era movement
// ---------------------------------------------------------------------------

/// The recall hook (called by `tokens::revoke_delegations` BEFORE the ack
/// is queued): if any of `inos` is a held UPDATE authority, FLUSH the
/// lane first (OQ-2's resolved form — the batch applies before the
/// recaller's conflicting serve/mutation), then drop the authorities.
pub(crate) async fn revoke_update_authorities(endpoint: &str, inos: &[u64]) {
    if AUTHORITIES.load(Ordering::Relaxed) == 0 {
        return;
    }
    let held: Vec<u64> = {
        let st = STATE.lock();
        match st.lanes.get(endpoint) {
            Some(l) => inos
                .iter()
                .copied()
                .filter(|i| l.authorities.contains_key(i))
                .collect(),
            None => return,
        }
    };
    if held.is_empty() {
        return;
    }
    if PENDING_OPS.load(Ordering::Relaxed) > 0 {
        FLUSH_FORCES.fetch_add(1, Ordering::Relaxed);
        // A failed flush here leaves the recall un-acked; the owner's
        // deadline (and its eviction escalation) is the bound — never a
        // silent success.
        let _ = flush_owner(endpoint).await;
    }
    let mut st = STATE.lock();
    if let Some(lane) = st.lanes.get_mut(endpoint) {
        for ino in held {
            if lane.authorities.remove(&ino).is_some() {
                AUTHORITIES.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// The owner era moved under `endpoint` (failover observed): pending
/// intents were minted from a dead era's supply and can never apply —
/// latch + destroy them (§8.2's error channel), drop authorities + supply.
pub(crate) fn owner_era_moved(endpoint: &str, new_term: u64) {
    if PENDING_OPS.load(Ordering::Relaxed) == 0 && AUTHORITIES.load(Ordering::Relaxed) == 0 {
        return;
    }
    let stale: Vec<QueuedIntent> = {
        let mut st = STATE.lock();
        let Some(lane) = st.lanes.get_mut(endpoint) else {
            return;
        };
        let lane_term = lane.supply.as_ref().map(|s| s.term).unwrap_or(0);
        if lane_term >= new_term {
            return;
        }
        AUTHORITIES.fetch_sub(lane.authorities.len() as u64, Ordering::Relaxed);
        lane.authorities.clear();
        lane.supply = None;
        lane.queue.drain(..).collect()
    };
    if !stale.is_empty() {
        log::error!(
            "S10 intents: owner {endpoint} moved to era {new_term} with {} un-flushed intent(s) \
             — they die with the era (latched to their directories, mints destroyed; the \
             acked-un-fsynced disclosure)",
            stale.len()
        );
        fail_batch(endpoint, stale, libc::EIO, false, None);
    }
}

// ---------------------------------------------------------------------------
// Stats + seams
// ---------------------------------------------------------------------------

/// The `meta_ship_intent` family snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IntentStats {
    /// Flush frames shipped (the coalesce denominator).
    pub batches: u64,
    /// Intents those frames carried (`verbs / batches` = coalesce factor).
    pub verbs: u64,
    /// SYNCHRONOUS flush forces (fsync / recall / ordering barrier).
    pub flush_forces: u64,
    /// **Must stay ≈ 0**: deferred apply-refusals surfaced at the §8.2
    /// contract point (growth = capacity/quota pressure reaching the
    /// delegated create path).
    pub refusals: u64,
    /// Local mints (the engagement instrument: creates that never rode
    /// the wire).
    pub mints: u64,
    /// Mint declines (dry supply / mid-flush shapes) — each is one
    /// shipped create, the priced fallback.
    pub declines: u64,
    /// Authoritative local EEXIST answers (`O_EXCL` exactness).
    pub local_eexist: u64,
    /// Authoritative local NEGATIVE lookups (the census serving ENOENT).
    pub local_negatives: u64,
    /// Setattrs deferred into batches (the tar utimensat shape).
    pub deferred_setattrs: u64,
    /// Local mints destroyed by deferred refusals (≤ refusals).
    pub mint_destroys: u64,
    /// Owner face: intent ops applied.
    pub applied: u64,
    /// Owner face: replays answered from the witness window.
    pub replays: u64,
    /// Owner face: batches refused by the era/custody gates.
    pub stale_refusals: u64,
    /// Owner face: foreign reads that recalled an UPDATE grant (OQ-2's
    /// engagement — the storm row's instrument).
    pub read_recalls: u64,
    /// Owner face: UPDATE grants issued.
    pub update_grants: u64,
    /// Owner face: UPDATE grants declined (exclusivity / valve / census
    /// budget / in-flight mutation).
    pub update_declines: u64,
    /// GAUGE: pending intents (queued + in flight).
    pub pending: u64,
    /// GAUGE: held UPDATE authorities (client face — the live-leg
    /// absorption instrument).
    pub authorities: u64,
    /// GAUGE: supply inos remaining across lanes.
    pub supply_remaining: u64,
    /// GAUGE: the published §8.2 visibility bound, ms (post-fsync the
    /// batch-flush term is 0; this is the foreign kernel negative-TTL
    /// term — reader mounts add their own published
    /// `reader_staleness_bound_ms`).
    pub visibility_bound_ms: u64,
}

/// The published visibility bound's arithmetic (§8.2 law 3): foreign
/// visibility after `fsync(dir)` = 0 (the flush applied; owner serves
/// current) + the foreign kernel's negative-entry TTL, which the S5 TTL
/// law bounds by this published term.
fn visibility_bound_ms() -> u64 {
    crate::env_knobs::opt_int_knob::<u64>("SQUEEZEFS_FUSE_NEGATIVE_TTL_MS").unwrap_or(1000)
}

/// Read the family.
pub fn intent_stats() -> IntentStats {
    let (supply_remaining, pending) = {
        let st = STATE.lock();
        let mut rem = 0u64;
        for lane in st.lanes.values() {
            if let Some(s) = &lane.supply {
                rem += (s.end.saturating_sub(s.next)) / s.stride.max(1);
            }
        }
        (rem, PENDING_OPS.load(Ordering::Relaxed))
    };
    IntentStats {
        batches: BATCHES.load(Ordering::Relaxed),
        verbs: VERBS.load(Ordering::Relaxed),
        flush_forces: FLUSH_FORCES.load(Ordering::Relaxed),
        refusals: REFUSALS.load(Ordering::Relaxed),
        mints: MINTS.load(Ordering::Relaxed),
        declines: DECLINES.load(Ordering::Relaxed),
        local_eexist: LOCAL_EEXIST.load(Ordering::Relaxed),
        local_negatives: LOCAL_NEGATIVES.load(Ordering::Relaxed),
        deferred_setattrs: DEFERRED_SETATTRS.load(Ordering::Relaxed),
        mint_destroys: MINT_DESTROYS.load(Ordering::Relaxed),
        applied: APPLIED.load(Ordering::Relaxed),
        replays: REPLAYS.load(Ordering::Relaxed),
        stale_refusals: STALE_REFUSALS.load(Ordering::Relaxed),
        read_recalls: READ_RECALLS.load(Ordering::Relaxed),
        update_grants: UPDATE_GRANTS.load(Ordering::Relaxed),
        update_declines: UPDATE_DECLINES.load(Ordering::Relaxed),
        pending,
        authorities: AUTHORITIES.load(Ordering::Relaxed),
        supply_remaining,
        visibility_bound_ms: visibility_bound_ms(),
    }
}

/// The `meta_ship_intent` stats-inode object (design §13 spellings; the
/// engagement gauges are additive).
pub fn intent_stats_json() -> serde_json::Value {
    let s = intent_stats();
    serde_json::json!({
        "meta_ship_intent_batches": s.batches,
        "meta_ship_intent_verbs": s.verbs,
        "meta_ship_intent_flush_forces": s.flush_forces,
        "meta_ship_intent_refusals": s.refusals,
        "meta_ship_intent_mints": s.mints,
        "meta_ship_intent_declines": s.declines,
        "meta_ship_intent_local_eexist": s.local_eexist,
        "meta_ship_intent_local_negatives": s.local_negatives,
        "meta_ship_intent_deferred_setattrs": s.deferred_setattrs,
        "meta_ship_intent_mint_destroys": s.mint_destroys,
        "meta_ship_intent_applied": s.applied,
        "meta_ship_intent_replays": s.replays,
        "meta_ship_intent_stale_refusals": s.stale_refusals,
        "meta_ship_intent_read_recalls": s.read_recalls,
        "meta_ship_intent_update_grants": s.update_grants,
        "meta_ship_intent_update_declines": s.update_declines,
        "meta_ship_intent_pending": s.pending,
        "meta_ship_intent_authorities": s.authorities,
        "meta_ship_intent_supply_remaining": s.supply_remaining,
        "meta_ship_intent_visibility_bound_ms": s.visibility_bound_ms,
    })
}

/// **Test seam**: drop every lane, authority, image and latch (the
/// process-death analog the MW-8 cargo shape uses). Production never
/// calls it — a real death drops the process.
pub fn test_clear_intents() {
    let mut st = STATE.lock();
    st.lanes.clear();
    PENDING_OPS.store(0, Ordering::Relaxed);
    AUTHORITIES.store(0, Ordering::Relaxed);
}
