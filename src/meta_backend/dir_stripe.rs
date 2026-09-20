//! **Directory STRIPING** — symmetric metadata PR 7b
//! (docs/design-symmetric-metadata.md §5.6.5, KD-SYM-20; owner ruling
//! R-SYM-5). Dark: every arm below is reached only on a volume whose
//! symmetric plane is ARMED (bit 17 + `SQUEEZEFS_SYMMETRIC_META=1`); an
//! unarmed or bit-17-absent mount pays one `Option` test per routed op
//! and takes the shipped path verbatim.
//!
//! A hot shared directory `D` spreads its dentries over `K` **stripes**:
//! `K` stripe inos `D_0 … D_{K-1}` — ordinary `S_IFDIR` inode records
//! with no name, each minted in a DIFFERENT node's slot on `D`'s volume —
//! with `stripe(name) = hash54(name) % K` and stripe `i`'s dentries keyed
//! under `D_i` (`D_i ‖ 02 ‖ hash54 ‖ coll` in `D_i`'s slot tree). Stripe
//! ownership IS slot ownership: a create into `D` is PR 6's shipped
//! `InsertDentry` with a different key ino, so `N` creators spread over
//! `K` holders and a create recalls only its stripe's readers.
//!
//! **The stripe map is a set of RESERVED-NAME dentries in `D`'s own
//! tree** (as built — the design's "`stripes` on the directory record"):
//! a name whose first byte is NUL can never arrive through FUSE (the VFS
//! refuses NUL inside a path component), so `\0sqz.stripe:NN → D_i`,
//! `\0sqz.striped → D_0` (the COMMIT marker: the map is live iff it
//! exists and names stripe 0) and `\0sqz.migrating → D_0` are records no
//! user op can name, filtered from `readdir` at the ONE routed pager. No
//! marker names `D` itself — a self-reference is the reverse dentry
//! scan's first hit whenever `D` sorts below its parent, and `..` would
//! answer `D` (Issue 3). Why dentries and not a new record codec: (1)
//! the map then RIDES `D`'s token verbatim — PR 5's grant pages a
//! directory's dentry set — with no token-API change; (2) the flip is
//! `K + 2` `InsertDentry` steps of ONE PR 6 intent, so the whole §5.6
//! protocol (intent, roll-forward, the kill seams) is reused with no new
//! step kind and no new applier; (3) C9's dentry pass marks every
//! marker's TARGET, so a stripe ino named by its map is REFERENCED and an
//! orphan stripe is C9's class exactly as §5.6.5 states. **No slot-tree
//! KEY kind is added**: bit 17's kind set stays PR 1 + PR 7's; what this
//! rung adds to the bit's meaning is the reserved NUL name space inside
//! kind `0x02` (design §7.1). **A stripe never carries a map** (Issue 1):
//! a `DIR_STRIPE` ino is no flip candidate at the trigger, the xattr or
//! the `-o stripe_dirs` mkdir — the census skips a known stripe, and the
//! flip's belt is the reverse scan (`is_stripe`), run once per stripe.
//!
//! **Where a name IS** (Issue 4 — the mutation paths made migration-
//! safe): `D`'s own tree is a PERMANENT secondary home of user names for
//! every READ (`stripe_locate`, the `readdir` merge — the stripe wins
//! where both hold a name), and every MUTATION keys the STRIPE: a name
//! found in `D`'s own tree is re-homed first (`rehome_name`, the same
//! `migrate_one` the background migration runs, under the same guards),
//! then the op revalidates under its guards against the stripe alone.
//! The `migrating` marker is the background migration's durable cursor,
//! not a routing switch, and its clear re-verifies `D`'s tree EMPTY of
//! user names under the exclusive guard.
//!
//! **Where the stripes live**: on `D`'s VOLUME (so every stripe's dentry
//! keys carry `D`'s volume's hash seed and the K-way `readdir` merge has
//! ONE order — `(hash54, coll)` — with the shipped cookie as its exact
//! continuation), in the slots of the `K` heaviest foreign creators the
//! holder observed (`SupplyStripeIno`), the remainder in the holder's
//! own slots.
//!
//! **`nlink` / `Δtime` of `D`** (the design's "batched per stripe per
//! flush cadence"): as built, a stripe's own record IS the durable delta
//! — every insert into stripe `i` moves `D_i`'s own `nlink`/times in the
//! same transaction as the dentry (the shipped parent update, exact and
//! idempotent) — and `D`'s VIEW is the FOLD `nlink = D.nlink + Σ (D_i.nlink
//! − 2)`, `mtime/ctime = max`, read off the `K` stripe records (RAM under
//! the tokens `readdir` needs anyway). The fold is PERSISTED onto `D`'s
//! own record once per cadence when it moved (`dir_stripe_time_batches`),
//! so an offline probe reads a near-current value; nothing ships per
//! insert, and no delta can be lost or double-applied.
//!
//! **`rmdir D`** (§5.6.5 "K `DestroyStripe` ships + `D`'s destroy in ONE
//! intent"; R26): under the S10 mutation permit and ONE guard set held
//! for the whole act — `I{parent}` + `D{parent, D}`, `I{D}` exclusive,
//! `I{D_i}` EXCLUSIVE on every stripe (travelling to a foreign holder),
//! `D{D, marker}` on every marker — the holder probes `D`'s own tree
//! (markers only), every stripe's count (`nlink 2` — a subdirectory is
//! `> 2`) and every stripe `IsEmpty` (exact under the exclusive guards:
//! every insert that took a stripe's guard has landed, none can be in
//! flight), refuses `ENOTEMPTY` having written NOTHING, and otherwise
//! writes ONE intent: `D`'s dentry out of its parent, the markers out of
//! `D` (the commit marker first — a reader between steps sees no map),
//! `SetNlink 2 → 0` on every stripe (the DYING mark) and on `D`. A kill
//! at any step rolls FORWARD to the whole removal at the next lessee, so
//! no window leaves `nlink 0` stripes under a LIVE map for the corpse
//! sweep to destroy (review round 1, Issue 2). An insert that parked on a
//! stripe's guard behind the rmdir resumes to a parent whose record reads
//! `nlink 0` and refuses `ENOENT` (`refuse_dying_parent`, under the
//! insert's own guard — every insert path: create, link, rename's
//! destination, the served step; Issue 6). The stripes' records are then
//! destroyed as hygiene (`DestroyStripe`; a stripe left is an `nlink 0`
//! corpse named by no map — the sweep's).

use super::crossvol_tx::{self, XvOp, XvPlan, XvStep};
use super::dlm;
use super::kv::backend::{KvMetaBackend, RoutedParentUpdate};
use super::kv::record::{dentry_name_hash54, forest_slot_of_ino, ForestSlot};
use super::{DirEntry, Ino, Inode, RoutedMetaBackend, MINT_SPREAD};
use crate::error::{Result, SqueezefsError};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// The knob, the xattr, the marker codec.
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_SYM_DIR_STRIPES` (design §6.1): the stripe count `K` at a
/// directory's flip; `1` = striping OFF (no flip ever runs — the A/B
/// control). Registered in `env_knobs.rs`; default derived
/// [`MINT_SPREAD`] = 64, the shipped granularity constant.
pub const DIR_STRIPES_ENV: &str = "SQUEEZEFS_SYM_DIR_STRIPES";

/// The stripe count the knob admits at most — the shipped granularity
/// constant (64 stripe inos per directory is cheap; `MINT_SPREAD` is the
/// number every other spread in the program derives from).
pub const STRIPES_MAX: u16 = MINT_SPREAD as u16;

/// The user-facing opt-in xattr: `setfattr -n user.squeezefs.stripes -v
/// <K> <dir>` flips the directory NOW (`K` clamped to `2..=STRIPES_MAX`;
/// any other value flips at the knob's `K`); `getfattr` answers the `K`
/// in force on a striped directory. Intercepted at the FUSE boundary
/// BEFORE the VAL-2 allowlist — a COMMAND, never a stored value.
pub const STRIPES_XATTR: &str = "user.squeezefs.stripes";

/// `\0sqz.stripe:NN → D_i` — stripe `i`'s ino. Every marker's leading
/// NUL byte is unreachable from FUSE (the VFS refuses NUL inside a
/// component), so no user name can ever collide with one or read one.
const STRIPE_MARKER: &str = "\u{0}sqz.stripe:";
/// `\0sqz.striped → D_0` — the COMMIT marker; the map is live iff present
/// AND it names stripe 0's ino (a torn or planted marker names nothing
/// the entries agree with). **Never `D` itself**: a dentry under `D`
/// naming `D` is the FIRST hit of the reverse dentry scan whenever `D`'s
/// key sorts below its real parent's, and `..` of `D` would resolve to
/// `D` on the reconnect path (review round 1, Issue 3) — so no marker
/// self-references.
pub const STRIPED_MARKER: &str = "\u{0}sqz.striped";
/// `\0sqz.migrating → D_0` — the lazy re-homing has not finished (the
/// resume's durable cursor; the same non-self-reference law).
pub const MIGRATING_MARKER: &str = "\u{0}sqz.migrating";
/// The stripe index the commit and migrating markers NAME.
pub const MARKER_TARGET_INDEX: u16 = 0;

/// A decoded marker name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// `\0sqz.stripe:NN` — the entry naming stripe `NN`.
    Stripe(u16),
    /// `\0sqz.striped` — the commit marker.
    Striped,
    /// `\0sqz.migrating` — the migration flag.
    Migrating,
}

/// The marker name of stripe `i` — exactly two decimal digits
/// (`STRIPES_MAX` is 64). The decoder is the encoder's inverse and
/// nothing wider: a wider `K` needs a wider encoder, and the codec's
/// law is `parse_marker(stripe_marker_name(i)) == Stripe(i)` AND
/// `stripe_marker_name(i) == name` for every name that decodes.
pub fn stripe_marker_name(i: u16) -> String {
    format!("{STRIPE_MARKER}{i:02}")
}

/// The width the encoder writes (the decoder refuses every other).
const STRIPE_MARKER_DIGITS: usize = 2;

/// Is `name` in the reserved marker space (any NUL-led name — every
/// marker starts with NUL and nothing user-reachable does)?
#[inline]
pub fn is_marker_name(name: &str) -> bool {
    name.as_bytes().first() == Some(&0)
}

/// Decode a marker name; `None` for every name that is not one of the
/// three shapes IN THE FORM THE ENCODER WRITES (a NUL-led name outside
/// the codec is filtered from `readdir` like a marker but names nothing:
/// `\0sqz.stripe:5` and `\0sqz.stripe:005` are not markers — a canonical
/// codec is what lets fsck's census and the router agree about one
/// tree). Total: never panics.
pub fn parse_marker(name: &[u8]) -> Option<Marker> {
    let name = std::str::from_utf8(name).ok()?;
    if name == STRIPED_MARKER {
        return Some(Marker::Striped);
    }
    if name == MIGRATING_MARKER {
        return Some(Marker::Migrating);
    }
    let digits = name.strip_prefix(STRIPE_MARKER)?;
    if digits.len() != STRIPE_MARKER_DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let i: u16 = digits.parse().ok()?;
    (i < STRIPES_MAX).then_some(Marker::Stripe(i))
}

/// `stripe(name) = hash54(name) % K` (§5.6.5) over the SEEDED dentry hash
/// of `D`'s volume — the same hash the stripe's dentry key carries, so the
/// merged `readdir` order across stripes is the shipped `(hash54, coll)`
/// order and the shipped cookie is its exact continuation.
#[inline]
pub fn stripe_of(hash54: u64, k: u16) -> u16 {
    (hash54 % u64::from(k.max(1))) as u16
}

/// The knob's `K`: `SQUEEZEFS_SYM_DIR_STRIPES` explicit, else
/// [`STRIPES_MAX`]. `1` = striping off.
pub fn stripe_count() -> u16 {
    crate::env_knobs::int_knob(DIR_STRIPES_ENV, i64::from(STRIPES_MAX))
        .clamp(1, i64::from(STRIPES_MAX)) as u16
}

/// Clamp an explicit `K` (the xattr's value) into the admissible flip
/// range: `2..=STRIPES_MAX` (a one-stripe directory is the unstriped
/// directory); an unparseable value takes the knob's.
pub fn clamp_explicit_k(value: &[u8]) -> u16 {
    std::str::from_utf8(value)
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .map_or_else(stripe_count, |k| k.clamp(2, STRIPES_MAX))
}

// ---------------------------------------------------------------------------
// Gauges (design §11, the Striping family) — process-wide, 0 unarmed.
// ---------------------------------------------------------------------------

pub static DIR_STRIPED_DIRS: AtomicU64 = AtomicU64::new(0);
pub static DIR_STRIPE_FLIPS: AtomicU64 = AtomicU64::new(0);
pub static DIR_STRIPE_SUPPLY_RPCS: AtomicU64 = AtomicU64::new(0);
pub static DIR_STRIPE_SHIPS: AtomicU64 = AtomicU64::new(0);
pub static DIR_STRIPE_READDIR_MERGES: AtomicU64 = AtomicU64::new(0);
pub static DIR_STRIPE_MIGRATED_NAMES: AtomicU64 = AtomicU64::new(0);
pub static DIR_STRIPE_TIME_BATCHES: AtomicU64 = AtomicU64::new(0);
/// Inserts refused because their stripe was DYING (an `rmdir` in
/// flight) — the R26 closer engaging; legal, never a tripwire.
pub static DIR_STRIPE_DYING_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// `rmdir`s of striped directories completed.
pub static DIR_STRIPE_RMDIRS: AtomicU64 = AtomicU64::new(0);

/// Names a mutation found in `D`'s own tree and re-homed into their
/// stripe on the way (a stale-route insert's or a mid-migration name's
/// self-healing; Issue 4).
pub static DIR_STRIPE_REHOMED_ON_TOUCH: AtomicU64 = AtomicU64::new(0);

/// Test seam: hold the BACKGROUND migration a flip would kick, so a
/// contract observes the `migrating` state (names in both homes, the
/// fallback lookup, the LWW rule) and drives [`RoutedMetaBackend::
/// migrate_dir`] itself. Never set in production.
pub static TEST_STRIPE_HOLD_MIGRATION: AtomicBool = AtomicBool::new(false);

/// Test seam: a striped `rmdir` holds its whole guard set for this many
/// ms between the probes and its intent (0 = off), raising
/// [`TEST_STRIPE_RMDIR_GUARDS_HELD`] meanwhile — the window a racing
/// create / link / rename parks in, to be refused `ENOENT` after (the
/// R26 pin, Issue 6). Never set in production.
pub static TEST_STRIPE_RMDIR_HOLD_MS: AtomicU64 = AtomicU64::new(0);
pub static TEST_STRIPE_RMDIR_GUARDS_HELD: AtomicBool = AtomicBool::new(false);

/// Every gauge of the family (the stats inode's JSON and the contracts'
/// before/after reads are ONE read).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StripeStats {
    pub striped_dirs: u64,
    pub flips: u64,
    pub supply_rpcs: u64,
    pub ships: u64,
    pub readdir_merges: u64,
    pub migrated_names: u64,
    pub time_batches: u64,
    pub dying_refusals: u64,
    pub rmdirs: u64,
    pub rehomed_on_touch: u64,
}

pub fn stripe_stats() -> StripeStats {
    StripeStats {
        striped_dirs: DIR_STRIPED_DIRS.load(Ordering::Relaxed),
        flips: DIR_STRIPE_FLIPS.load(Ordering::Relaxed),
        supply_rpcs: DIR_STRIPE_SUPPLY_RPCS.load(Ordering::Relaxed),
        ships: DIR_STRIPE_SHIPS.load(Ordering::Relaxed),
        readdir_merges: DIR_STRIPE_READDIR_MERGES.load(Ordering::Relaxed),
        migrated_names: DIR_STRIPE_MIGRATED_NAMES.load(Ordering::Relaxed),
        time_batches: DIR_STRIPE_TIME_BATCHES.load(Ordering::Relaxed),
        dying_refusals: DIR_STRIPE_DYING_REFUSALS.load(Ordering::Relaxed),
        rmdirs: DIR_STRIPE_RMDIRS.load(Ordering::Relaxed),
        rehomed_on_touch: DIR_STRIPE_REHOMED_ON_TOUCH.load(Ordering::Relaxed),
    }
}

/// The family as the stats inode publishes it (design §11).
pub fn stats_json() -> serde_json::Map<String, serde_json::Value> {
    let s = stripe_stats();
    let mut m = serde_json::Map::new();
    for (k, v) in [
        ("dir_striped_dirs", s.striped_dirs),
        ("dir_stripe_flips", s.flips),
        ("dir_stripe_supply_rpcs", s.supply_rpcs),
        ("dir_stripe_ships", s.ships),
        ("dir_stripe_readdir_merges", s.readdir_merges),
        ("dir_stripe_migrated_names", s.migrated_names),
        ("dir_stripe_time_batches", s.time_batches),
        ("dir_stripe_dying_refusals", s.dying_refusals),
        ("dir_stripe_rmdirs", s.rmdirs),
        ("dir_stripe_rehomed_on_touch", s.rehomed_on_touch),
    ] {
        m.insert(k.to_string(), serde_json::json!(v));
    }
    m
}

// ---------------------------------------------------------------------------
// The per-mount state: the map cache, the flip census, the migrations.
// ---------------------------------------------------------------------------

/// The stripe map of one directory as its markers name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeMap {
    pub dir: Ino,
    /// `stripes[i]` = the GLOBAL ino of stripe `i`; `len() = K`.
    pub stripes: Vec<Ino>,
    /// Names may still live in `D`'s own tree (the flip's lazy re-homing
    /// has not finished): a miss in the stripe falls back to `D`.
    pub migrating: bool,
}

impl StripeMap {
    pub fn k(&self) -> u16 {
        self.stripes.len() as u16
    }

    /// The stripe ino `hash54` routes to.
    pub fn stripe_for(&self, hash54: u64) -> (u16, Ino) {
        let i = stripe_of(hash54, self.k());
        (i, self.stripes[usize::from(i)])
    }
}

/// The resolved home of one `(D, name)`: the stripe the name routes to.
#[derive(Debug, Clone)]
pub struct StripeRoute {
    pub map: Arc<StripeMap>,
    pub index: u16,
    /// The GLOBAL stripe ino — the KEY parent every dentry op uses.
    pub stripe: Ino,
}

/// One directory's foreign-creator census over the `T_idle` half-window
/// pair (the [`crate::slot_lease_core::DominanceWindow`] shape, keyed by
/// directory and CREATOR appender id). The served hot path pays one
/// `BTreeMap` increment and two O(1) reads (Issue 11); the heaviest-first
/// SORT runs once, when the decision is due.
#[derive(Debug, Default)]
struct DirCensus {
    epoch: u64,
    cur: BTreeMap<u32, u64>,
    prev: BTreeMap<u32, u64>,
    /// Σ over both halves.
    total: u64,
    /// Distinct creators over both halves.
    distinct: u64,
}

impl DirCensus {
    fn align(&mut self, epoch: u64) {
        if epoch > self.epoch {
            if epoch == self.epoch + 1 {
                self.prev = std::mem::take(&mut self.cur);
            } else {
                self.prev.clear();
                self.cur.clear();
            }
            self.total = self.prev.values().sum();
            self.distinct = self.prev.len() as u64;
            self.epoch = epoch;
        }
    }

    /// One served insert from `creator`.
    fn note(&mut self, creator: u32) {
        let fresh = !self.cur.contains_key(&creator) && !self.prev.contains_key(&creator);
        *self.cur.entry(creator).or_default() += 1;
        self.total += 1;
        if fresh {
            self.distinct += 1;
        }
    }

    /// The trigger's two inputs without a sort.
    fn due(&self, n_floor: u64) -> bool {
        self.distinct >= 2 && self.total > n_floor
    }

    /// Creators by ops over the window, heaviest first (the sort — run
    /// when the decision is due, and for the contracts' reads).
    fn heaviest(&self) -> Vec<(u32, u64)> {
        let mut by: BTreeMap<u32, u64> = self.prev.clone();
        for (c, n) in &self.cur {
            *by.entry(*c).or_default() += n;
        }
        let mut v: Vec<(u32, u64)> = by.into_iter().filter(|(_, n)| *n > 0).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
}

/// Directories the census tracks at most — one hot shared directory per
/// rotor-sized slice of the slot namespace (`W / MINT_SPREAD` = 1,024):
/// past it the least recently touched is evicted (the
/// `REQUESTERS_PER_SLOT_MAX` posture one level up). Tie-tested.
pub const CENSUS_DIRS_MAX: usize = super::DERIVED_ROUTING_WIDTH as usize / MINT_SPREAD;

/// The migration scan's raw page: one merge-floor page per stripe of the
/// widest map (`MERGE_STREAM_PAGE_FLOOR × MINT_SPREAD` = 512 — the KV
/// walkers' page) — one bounded read per step, every page re-read at the
/// next round. Tie-tested.
pub const MIGRATION_SCAN_PAGE: usize = MERGE_STREAM_PAGE_FLOOR * MINT_SPREAD;

/// The negative map cache's bound (the holder's "not striped" hints): one
/// hint per slot of the routing namespace (`W` = 65,536; 24 B each ≈
/// 1.5 MiB) — a HINT flushed whole at the cap, re-learnt at one tree
/// descent per directory. Tie-tested.
pub const UNSTRIPED_CACHE_MAX: usize = super::DERIVED_ROUTING_WIDTH as usize;

/// The mount's striping state, one per [`RoutedMetaBackend`].
pub struct StripeState {
    /// `D → its map` (finished OR migrating — a stale `migrating: true`
    /// costs one probe of `D`'s tree more, never a wrong answer; the
    /// clear invalidates it on the holder). Invalidated by the local
    /// rmdir.
    maps: scc::HashMap<Ino, Arc<StripeMap>>,
    /// The HOLDER's negative cache: directories this mount holds whose
    /// tree carries no commit marker, stamped with the slot's lease
    /// `(holder, g)` at the read. Only the holder flips a directory it
    /// holds, so its own flip is the one invalidation (Issue 12); a hint
    /// whose lease moved since (a handover round trip with a flip in
    /// between — review round 2, Issue 26) reads as no hint.
    unstriped: scc::HashMap<Ino, (u32, u32)>,
    /// Inos known to be STRIPES (learnt from every map read and every
    /// mint): never a flip candidate, never a census entry (Issue 1).
    known_stripes: scc::HashSet<Ino>,
    /// The flip trigger's census: foreign creators per directory.
    census: Mutex<BTreeMap<Ino, DirCensus>>,
    /// Directories whose flip or migration this mount is running.
    in_flight: scc::HashSet<Ino>,
    /// `D → the monotonic ns of its last times persist` — the fold's
    /// write-back runs once per checkpoint cadence per directory
    /// (Issue 8), never per `stat`.
    times_persisted_ns: scc::HashMap<Ino, u64>,
    /// `-o stripe_dirs`: every `mkdir` under an armed volume stripes at
    /// creation (the DNE-2 default).
    stripe_at_mkdir: AtomicBool,
    /// The owning set's handle, installed by `open_routed_meta_set` so
    /// the background flip/migration tasks can hold the set; a bare
    /// constructor leaves it empty and every background arm becomes a
    /// no-op (the synchronous faces stay).
    me: std::sync::OnceLock<std::sync::Weak<RoutedMetaBackend>>,
}

impl Default for StripeState {
    fn default() -> Self {
        Self::new()
    }
}

impl StripeState {
    pub fn new() -> Self {
        Self {
            maps: scc::HashMap::new(),
            unstriped: scc::HashMap::new(),
            known_stripes: scc::HashSet::new(),
            census: Mutex::new(BTreeMap::new()),
            in_flight: scc::HashSet::new(),
            times_persisted_ns: scc::HashMap::new(),
            stripe_at_mkdir: AtomicBool::new(false),
            me: std::sync::OnceLock::new(),
        }
    }

    fn census(&self) -> std::sync::MutexGuard<'_, BTreeMap<Ino, DirCensus>> {
        self.census.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn learn_stripes(&self, stripes: &[Ino]) {
        for s in stripes {
            let _ = self.known_stripes.insert_sync(*s);
        }
    }

    fn note_unstriped(&self, dir: Ino, lease: (u32, u32)) {
        if self.unstriped.len() >= UNSTRIPED_CACHE_MAX {
            self.unstriped.retain_sync(|_, _| false);
        }
        let _ = self.unstriped.insert_sync(dir, lease);
    }

    /// The hint for `dir` under the lease `(holder, g)` in force — `None`
    /// when absent or stale.
    fn unstriped_hint(&self, dir: Ino, lease: (u32, u32)) -> bool {
        self.unstriped
            .read_sync(&dir, |_, l| *l == lease)
            .unwrap_or(false)
    }
}

/// One stream of the K-way `readdir` merge: a stripe's (or `D`'s own)
/// filtered page and its resume cookie.
struct MergeStream {
    buf: std::collections::VecDeque<(u64, DirEntry)>,
    cursor: u64,
    exhausted: bool,
}

/// The smallest per-stream refill: the shipped readdir's smallest useful
/// page (a `getdents` of a few entries) — below it the per-refill call
/// overhead dominates the entries it returns. A SHAPE constant (the one
/// free number of the module), pinned by the tie test so a change is a
/// red test and a stated decision.
pub const MERGE_STREAM_PAGE_FLOOR: usize = 8;

/// The 54-bit dentry hash a real-entry readdir cookie was issued for
/// (the cookie is the dentry key suffix + the shipped bias; `0` for the
/// reserved offsets, which no merged entry carries).
fn readdir_cookie_hash54(cookie: u64) -> u64 {
    match super::kv::record::decode_readdir_cookie(cookie) {
        Ok(super::kv::record::ReaddirPos::AfterEntry { hash54, .. }) => hash54,
        _ => 0,
    }
}

/// Whether a flip must run the "is this a STRIPE?" belt (the reverse
/// dentry scan) before it mints anything: a caller that KNOWS the
/// directory — the `mkdir` that just named it under its parent, the
/// automatic trigger that ran the scan already — skips it; the explicit
/// xattr has no such knowledge and checks once (review round 2, Issue 24:
/// under `-o stripe_dirs` every mkdir paid a reverse scan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripeBelt {
    /// Run the reverse scan; refuse a stripe.
    Check,
    /// The caller established that `dir` is not a stripe.
    KnownDirectory,
}

/// The flip decision the census answers (§5.6.5 "when to stripe"):
/// foreign ships into `D` over the window exceed `N_floor` AND come from
/// more than one creator. `creators` = the heaviest first — the
/// `SupplyStripeIno` targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlipDecision {
    pub dir: Ino,
    pub creators: Vec<u32>,
}

/// The pure trigger law, for the census AND the contracts: `ships` ranked
/// heaviest-first over one window.
pub fn flip_due(ships: &[(u32, u64)], n_floor: u64) -> bool {
    let total: u64 = ships.iter().map(|(_, n)| n).sum();
    ships.len() >= 2 && total > n_floor
}

// ---------------------------------------------------------------------------
// The routed arms.
// ---------------------------------------------------------------------------

/// The EAGAIN-class refusal a flip on a directory this mount does not
/// hold answers (the holder flips; a requester ships its creates).
fn not_holder(dir: Ino, holder: u32) -> SqueezefsError {
    SqueezefsError::refused(
        libc::EAGAIN,
        format!(
            "directory {dir} is leased by appender {holder}, not this mount — its holder \
             decides the stripe flip (the automatic trigger runs there; the explicit \
             `{STRIPES_XATTR}` opt-in is applied by the holder)"
        ),
    )
}

impl RoutedMetaBackend {
    /// Whether the striping arms are live for `volume_idx`: the plane
    /// armed on that volume. One `Option` test on every unarmed mount.
    #[inline]
    fn stripes_armed(&self, volume_idx: usize) -> bool {
        self.volumes
            .get(volume_idx)
            .is_some_and(|v| v.striping_plane_armed())
    }

    /// `stripes_armed` for GLOBAL `ino`'s volume — the FUSE
    /// boundary's gate on the `user.squeezefs.stripes` command (an
    /// unarmed mount treats the name as the reserved one it is).
    pub fn striping_armed(&self, ino: Ino) -> bool {
        self.stripes_armed(self.route_ino(ino).0)
    }

    /// Install the set's own handle for the background arms (once, at
    /// `open_routed_meta_set`).
    pub fn install_stripe_self(self: &Arc<Self>) {
        let _ = self.dir_stripes.me.set(Arc::downgrade(self));
    }

    fn self_arc(&self) -> Option<Arc<Self>> {
        self.dir_stripes.me.get().and_then(|w| w.upgrade())
    }

    /// `-o stripe_dirs`: stripe every new directory at its `mkdir`.
    pub fn set_stripe_dirs_at_mkdir(&self, on: bool) {
        self.dir_stripes
            .stripe_at_mkdir
            .store(on, Ordering::Relaxed);
    }

    pub fn stripe_dirs_at_mkdir(&self) -> bool {
        self.dir_stripes.stripe_at_mkdir.load(Ordering::Relaxed)
    }

    /// The seeded 54-bit dentry hash of `name` on `volume_idx` — the
    /// stripe function's input and the dentry key's.
    fn hash54_on(&self, volume_idx: usize, name: &str) -> u64 {
        dentry_name_hash54(
            name.as_bytes(),
            self.volumes[volume_idx].superblock().hash_seed,
        )
    }

    /// The appender that leases the slot of GLOBAL `ino` per tree 0
    /// (`None` unarmed / unleased).
    pub(super) fn holder_of(&self, ino: Ino) -> Option<u32> {
        let (v, local) = self.route_ino(ino);
        let plane = self.volumes.get(v)?.slot_leases()?;
        match plane.table.resolve(forest_slot_of_ino(local)) {
            crate::slot_lease_core::Resolved::Holder { holder, .. } => Some(holder),
            _ => None,
        }
    }

    /// The lease `(holder, g)` of GLOBAL `ino`'s slot per tree 0 — `(0, 0)`
    /// unarmed / unleased. The negative map cache's stamp.
    fn lease_of(&self, ino: Ino) -> (u32, u32) {
        let (v, local) = self.route_ino(ino);
        let Some(plane) = self.volumes.get(v).and_then(|vol| vol.slot_leases()) else {
            return (0, 0);
        };
        match plane.table.resolve(forest_slot_of_ino(local)) {
            crate::slot_lease_core::Resolved::Holder { holder, g } => (holder, g),
            _ => (0, 0),
        }
    }

    /// Is GLOBAL `ino`'s slot served by THIS process (its own region or a
    /// declared one — the door's "leased here")? A mount with no lease
    /// plane serves everything it WRITES (the unarmed writer — where the
    /// striping paths are off anyway) and nothing it only READS: a `-o ro`
    /// token reader holds no slot, so it is never a directory's holder
    /// (PR 13 — the fleet's `sym-crash` leg: the reader took the HOLDER's
    /// arms here, cached the negative "unstriped" hint at its first read
    /// of `/`, and the hint — invalidated only by the holder's OWN flip —
    /// outlived the successor's flip of `/`; the reader listed the root as
    /// EMPTY for the rest of the round).
    fn served_here(&self, ino: Ino) -> bool {
        let (v, local) = self.route_ino(ino);
        let Some(vol) = self.volumes.get(v) else {
            return false;
        };
        match vol.slot_leases() {
            Some(plane) => plane.gate.is_leased(forest_slot_of_ino(local)),
            None => !vol.is_read_only(),
        }
    }

    /// **The stripe map of `dir`**, `None` when `dir` is not striped (or
    /// the plane is unarmed on its volume). Cached — finished or
    /// migrating (Issue 12); a migrating map re-kicks its migration
    /// single-flight when this mount holds `dir` (a paused one resumes at
    /// the next access). The holder keeps a negative cache too, invalidated
    /// by its own flip — the one act that stripes a directory it holds.
    pub async fn stripe_map(&self, dir: Ino) -> Result<Option<Arc<StripeMap>>> {
        let (v, local) = self.route_ino(dir);
        if !self.stripes_armed(v) {
            return Ok(None);
        }
        if let Some(m) = self.dir_stripes.maps.read_sync(&dir, |_, m| Arc::clone(m)) {
            if m.migrating && self.served_here(dir) {
                self.kick_migration(dir);
            }
            return Ok(Some(m));
        }
        let holder = self.served_here(dir);
        let lease = self.lease_of(dir);
        if holder && self.dir_stripes.unstriped_hint(dir, lease) {
            return Ok(None);
        }
        let Some(map) = self.read_map_from_markers(dir, v, local).await? else {
            if holder {
                self.dir_stripes.note_unstriped(dir, lease);
            }
            return Ok(None);
        };
        let map = Arc::new(map);
        self.dir_stripes.learn_stripes(&map.stripes);
        let _ = self.dir_stripes.maps.insert_sync(dir, Arc::clone(&map));
        if map.migrating && holder {
            self.kick_migration(dir);
        }
        Ok(Some(map))
    }

    /// The map of `dir` as this mount already KNOWS it — the cache alone,
    /// no read (PR 13: a TOKEN READER's `stat D` folds over the stripes
    /// only once its own `readdir` / `lookup` — which fetch `D`'s dentry
    /// token anyway — learnt the map; a marker probe at every `getattr` of
    /// a directory cost a `stat`-only reader a second grant per directory,
    /// `sym_mount_posture_tests`' "one grant from B").
    pub fn stripe_map_cached(&self, dir: Ino) -> Option<Arc<StripeMap>> {
        self.dir_stripes.maps.read_sync(&dir, |_, m| Arc::clone(m))
    }

    /// The map as `dir`'s markers name it — `None` = not striped (no
    /// commit marker, a commit marker naming anything but stripe 0, or
    /// fewer than two entries: a torn or planted map is never guessed
    /// from; fsck C17 reports it).
    async fn read_map_from_markers(
        &self,
        dir: Ino,
        v: usize,
        local: Ino,
    ) -> Result<Option<StripeMap>> {
        let Some((commit, _)) = self.find_dentry_routed(v, local, STRIPED_MARKER).await? else {
            return Ok(None);
        };
        let mut stripes = Vec::new();
        for i in 0..STRIPES_MAX {
            match self
                .find_dentry_routed(v, local, &stripe_marker_name(i))
                .await?
            {
                Some((stripe, _)) => stripes.push(stripe),
                None => break,
            }
        }
        if stripes.len() < 2 || stripes[usize::from(MARKER_TARGET_INDEX)] != commit {
            log::warn!(
                "directory {dir}: the stripe commit marker names ino {commit} over {} stripe \
                 entr{} (a live map has ≥ 2 and its commit marker names stripe 0) — the map is \
                 ignored (fsck C17 reports it)",
                stripes.len(),
                if stripes.len() == 1 { "y" } else { "ies" }
            );
            return Ok(None);
        }
        let migrating = self
            .find_dentry_routed(v, local, MIGRATING_MARKER)
            .await?
            .is_some();
        Ok(Some(StripeMap {
            dir,
            stripes,
            migrating,
        }))
    }

    /// Forget a cached map (the local rmdir / the migration's clear / a
    /// flip's commit — the next read is the markers').
    fn forget_map(&self, dir: Ino) {
        self.dir_stripes.maps.remove_sync(&dir);
        self.dir_stripes.unstriped.remove_sync(&dir);
    }

    /// **The routing arm**: where `(parent, name)` lives when `parent`
    /// is striped — the stripe `hash54(name) % K` names. `None` = an
    /// ordinary directory (the shipped path).
    pub async fn stripe_route(&self, parent: Ino, name: &str) -> Result<Option<StripeRoute>> {
        if is_marker_name(name) {
            return Ok(None);
        }
        let Some(map) = self.stripe_map(parent).await? else {
            return Ok(None);
        };
        let (v, _) = self.route_ino(parent);
        let (index, stripe) = map.stripe_for(self.hash54_on(v, name));
        Ok(Some(StripeRoute { map, index, stripe }))
    }

    /// **Where an EXISTING name of a striped directory is** — the stripe
    /// when the stripe holds it, `D`'s own tree otherwise (the permanent
    /// secondary home: a name the migration has not moved, or one a
    /// stale route inserted after it; the LWW rule: the stripe wins).
    /// `None` when the name is nowhere.
    pub async fn stripe_locate(
        &self,
        parent: Ino,
        name: &str,
        route: &StripeRoute,
    ) -> Result<Option<(Ino, Ino, u32)>> {
        let (sv, slocal) = self.route_ino(route.stripe);
        if let Some((child, ft)) = self.find_dentry_routed(sv, slocal, name).await? {
            return Ok(Some((route.stripe, child, ft)));
        }
        let (pv, plocal) = self.route_ino(parent);
        if let Some((child, ft)) = self.find_dentry_routed(pv, plocal, name).await? {
            return Ok(Some((parent, child, ft)));
        }
        Ok(None)
    }

    /// The KEY parent a NEW name in a striped directory is inserted under
    /// (always the stripe), after the EEXIST screen over `D`'s own tree
    /// (the stripe's own dentry is screened under the insert's guard by
    /// the routed insert itself).
    pub async fn stripe_insert_parent(
        &self,
        parent: Ino,
        name: &str,
        route: &StripeRoute,
    ) -> Result<Ino> {
        let (pv, plocal) = self.route_ino(parent);
        if self.find_dentry_routed(pv, plocal, name).await?.is_some() {
            return Err(SqueezefsError::already_exists("File already exists"));
        }
        Ok(route.stripe)
    }

    /// **The KEY parent a MUTATION of an existing name runs against** —
    /// always the stripe: a name still in `D`'s own tree is re-homed
    /// FIRST (`rehome_name` — the migration's own move, under the same
    /// guards, so the two serialize and the second finds the work done),
    /// then the op revalidates under its guards against the stripe alone
    /// (Issue 4: a routing decision read once and never re-validated gave
    /// spurious `ENOENT`s racing the migration and stranded a rename's
    /// destination in `D`'s tree).
    pub async fn stripe_mutation_parent(
        &self,
        parent: Ino,
        name: &str,
        route: &StripeRoute,
    ) -> Result<Ino> {
        let (pv, plocal) = self.route_ino(parent);
        if let Some((child, ft)) = self.find_dentry_routed(pv, plocal, name).await? {
            let entry = DirEntry {
                ino: child,
                name: name.to_string(),
                file_type: ft,
            };
            if self
                .migrate_one(parent, pv, plocal, &route.map, &entry)
                .await?
            {
                DIR_STRIPE_REHOMED_ON_TOUCH.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(route.stripe)
    }

    /// The `unlink`/`rmdir` prelude: the KEY parent the name is removed
    /// from (its stripe when `parent` is striped — the name re-homed there
    /// first if it still sat in the directory's own tree), and the map of
    /// the CHILD when it is a striped directory (the rmdir protocol's
    /// subject — probed and removed by [`Self::rmdir_striped`] under ONE
    /// guard set). Unguarded pre-reads: the removal revalidates under its
    /// guards.
    pub async fn stripe_unlink_prelude(
        &self,
        parent: Ino,
        name: &str,
    ) -> Result<(Ino, Option<Arc<StripeMap>>)> {
        let (v, _) = self.route_ino(parent);
        if !self.stripes_armed(v) {
            return Ok((parent, None));
        }
        let key_parent = match self.stripe_route(parent, name).await? {
            Some(route) => self.stripe_mutation_parent(parent, name, &route).await?,
            None => parent,
        };
        let (kv_idx, klocal) = self.route_ino(key_parent);
        let child = match self.find_dentry_routed(kv_idx, klocal, name).await? {
            Some((child, ft)) if ft == libc::S_IFDIR => child,
            _ => return Ok((key_parent, None)),
        };
        Ok((key_parent, self.stripe_map(child).await?))
    }

    /// The KEY parent's `nlink` as its HOLDER states it — `None` for no
    /// record. A stripe is minted in ANOTHER appender's slot (the
    /// supplier's, or the holder's rotor), so at the initiator its record
    /// is a FOREIGN read: `getattr` rides the writer's read divert (PR
    /// 12b — the holder's token plane, exact until recalled), where the
    /// routed local read (`read_inode_value_routed`, the cross-volume
    /// plan's OWN-record witness) reads this mount's PROJECTION of the
    /// slot — loaded at its open, so a stripe minted after it read as "no
    /// record" and every foreign create into a striped directory was
    /// refused `ENOENT` (PR 13, the fleet's `sym-shared-dir`: 1–3 creates
    /// per foreign writer, then `dir_stripe_dying_refusals` +1 each). An
    /// own-slot parent and every unarmed mount take the local read
    /// verbatim (`token_serve` answers `None` for them).
    async fn key_parent_nlink(&self, parent: Ino) -> Result<Option<u32>> {
        let (v, local) = self.route_ino(parent);
        match self.volumes[v].getattr(local).await {
            Ok(rec) => Ok(Some(rec.nlink)),
            Err(SqueezefsError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A parent whose record reads `nlink == 0` — or has NO record (a
    /// stripe already destroyed behind its rmdir's intent) — is DYING or
    /// GONE, and an insert into it is refused `ENOENT`: the R26 closer,
    /// read under the insert's held `I{parent}` guard at EVERY insert
    /// path (create, link, rename's destination, the served step —
    /// Issue 6). An insert that parked on the stripe's guard behind the
    /// rmdir resumes to exactly one of the two shapes. The record is the
    /// HOLDER's word ([`Self::key_parent_nlink`]).
    pub async fn refuse_dying_parent(&self, parent: Ino) -> Result<()> {
        match self.key_parent_nlink(parent).await? {
            Some(nlink) if nlink != 0 => Ok(()),
            Some(_) => {
                DIR_STRIPE_DYING_REFUSALS.fetch_add(1, Ordering::Relaxed);
                Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("directory {parent} is being removed (rmdir in flight)"),
                )))
            }
            None => {
                DIR_STRIPE_DYING_REFUSALS.fetch_add(1, Ordering::Relaxed);
                Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("directory {parent} was removed (no record)"),
                )))
            }
        }
    }

    /// The errno a SHIPPED insert's witness refusal means when the KEY
    /// parent is a stripe: the holder answers a dying parent as a witness
    /// refusal (`ForeignSkipped`, so a roll-forward retires the intent
    /// instead of erroring at every cadence — Issue 13), which the plan
    /// maps to `EEXIST`; the op's errno for "the directory is being
    /// removed" is `ENOENT`, decided here on one record read of the error
    /// path only.
    pub async fn dying_parent_errno(&self, key_parent: Ino, e: SqueezefsError) -> SqueezefsError {
        if e.to_errno() != libc::EEXIST {
            return e;
        }
        match self.key_parent_nlink(key_parent).await {
            Ok(Some(nlink)) if nlink != 0 => e,
            Ok(_) => SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("directory {key_parent} is being removed (rmdir in flight)"),
            )),
            Err(_) => e,
        }
    }

    /// Is GLOBAL `ino` a STRIPE (a nameless directory some map names)?
    /// The known set first (learnt from every map read and mint); else
    /// the reverse dentry scan — run ONCE per flip candidate the caller
    /// cannot vouch for (the automatic trigger's spawn, the explicit
    /// xattr), never on a per-op path and never at `mkdir`, and learnt.
    pub async fn is_stripe(&self, ino: Ino) -> Result<bool> {
        if self.dir_stripes.known_stripes.contains_sync(&ino) {
            return Ok(true);
        }
        Ok(self.stripe_parent_dir(ino).await?.is_some())
    }

    /// The directory a STRIPE belongs to, for the `..` reconnect scan:
    /// the ONLY dentries naming a stripe are its map entries in `D` (no
    /// marker self-references — Issue 3), so the reverse scan of the
    /// stripe answers `D`. `None` = not a stripe.
    pub async fn stripe_parent_dir(&self, maybe_stripe: Ino) -> Result<Option<Ino>> {
        let (v, _) = self.route_ino(maybe_stripe);
        if !self.stripes_armed(v) {
            return Ok(None);
        }
        // Dentry values carry GLOBAL child inos — the scan is by the
        // global ino.
        let Some(local_parent) = self.volumes[v].find_parent_of_child(maybe_stripe).await? else {
            return Ok(None);
        };
        let dir = self.make_global_ino(local_parent, v);
        // Confirm it is the map naming it, not an ordinary directory
        // holding an ordinary child.
        let Some(map) = self.stripe_map(dir).await? else {
            return Ok(None);
        };
        Ok(map.stripes.contains(&maybe_stripe).then_some(dir))
    }
    // -----------------------------------------------------------------
    // The flip.
    // -----------------------------------------------------------------

    /// **The explicit flip** (`setxattr user.squeezefs.stripes`, `-o
    /// stripe_dirs`): stripe `dir` into `k` stripes. Idempotent — an
    /// already-striped directory answers `Ok` and moves nothing; refused
    /// when the knob is `1` (striping off), when `dir` is not a
    /// directory, or when another appender holds it (`EAGAIN`, naming
    /// the holder). Suppliers = the census's heaviest creators of `dir`.
    pub async fn stripe_dir(&self, dir: Ino, k: u16) -> Result<()> {
        let creators: Vec<u32> = self
            .dir_stripes
            .census()
            .get(&dir)
            .map(|c| c.heaviest().into_iter().map(|(c, _)| c).collect())
            .unwrap_or_default();
        self.flip_dir(dir, k, &creators, StripeBelt::Check).await
    }

    /// [`Self::stripe_dir`] with the suppliers NAMED — a CONTRACT face
    /// (the fleet-shape contracts name the holders the trigger census
    /// would have picked; no production caller).
    #[doc(hidden)]
    pub async fn stripe_dir_with_suppliers(
        &self,
        dir: Ino,
        k: u16,
        suppliers: &[u32],
    ) -> Result<()> {
        self.flip_dir(dir, k, suppliers, StripeBelt::Check).await
    }

    /// The refusal every flip of a STRIPE answers (Issue 1: a stripe
    /// cannot carry a map — its directory's routing never consults one,
    /// so a nested map would strand every name of the stripe).
    fn stripe_cannot_carry_a_map(dir: Ino, of: Ino) -> SqueezefsError {
        SqueezefsError::InvalidOperation(format!(
            "ino {dir} is a directory STRIPE{} and cannot carry a stripe map of its own \
             (design-symmetric-metadata §5.6.5: a stripe's names route by its directory's map \
             alone)",
            if of == 0 {
                String::new()
            } else {
                format!(" of directory {of}")
            }
        ))
    }

    /// The flip proper (§5.6.5): `K` `SupplyStripeIno`s (the creators
    /// first, the holder's own slots for the remainder), then ONE PR 6
    /// intent of `K + 2` marker inserts — every stripe entry, the
    /// `migrating` flag, the COMMIT marker LAST — so a reader sees
    /// either no map or the whole map, and a kill at any step rolls
    /// forward to the whole map; then the lazy migration, in the
    /// background. A STRIPE is never a candidate: `belt` says whether
    /// this call must run the reverse scan to know (the explicit xattr) or
    /// the caller already knows (the mkdir that named the directory, the
    /// trigger that scanned).
    async fn flip_dir(&self, dir: Ino, k: u16, creators: &[u32], belt: StripeBelt) -> Result<()> {
        let (v, local) = self.route_ino(dir);
        self.check_volume_enabled(v)?;
        if !self.stripes_armed(v) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "directory striping needs the symmetric plane armed on directory {dir}'s \
                 volume (SQUEEZEFS_SYMMETRIC_META=1 on a bit-17 volume)"
            )));
        }
        if stripe_count() <= 1 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "directory striping is OFF ({DIR_STRIPES_ENV}=1 — the A/B control); \
                 directory {dir} stays unstriped"
            )));
        }
        let k = k.clamp(2, STRIPES_MAX);
        if let Some(h) = self.holder_of(dir) {
            if !self.served_here(dir) {
                return Err(not_holder(dir, h));
            }
        }
        let rec = self.read_inode_routed(v, local).await?;
        if rec.mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                libc::ENOTDIR,
            )));
        }
        if self.stripe_map(dir).await?.is_some() {
            return Ok(());
        }
        if self.dir_stripes.known_stripes.contains_sync(&dir) {
            self.dir_stripes.census().remove(&dir);
            return Err(Self::stripe_cannot_carry_a_map(dir, 0));
        }
        if belt == StripeBelt::Check {
            if let Some(of) = self.stripe_parent_dir(dir).await? {
                self.dir_stripes.learn_stripes(&[dir]);
                self.dir_stripes.census().remove(&dir);
                return Err(Self::stripe_cannot_carry_a_map(dir, of));
            }
        }
        // Single-flight per directory: a second trigger while the flip
        // runs (a storm of served inserts) joins nothing and does nothing.
        if self.dir_stripes.in_flight.insert_sync(dir).is_err() {
            return Ok(());
        }
        let out = self.flip_dir_inner(dir, v, local, &rec, k, creators).await;
        self.dir_stripes.in_flight.remove_sync(&dir);
        if out.is_ok() {
            // The lazy re-homing, in the background — kicked once the
            // flip's own single-flight slot is free (the kick takes it).
            self.kick_migration(dir);
        }
        out
    }

    async fn flip_dir_inner(
        &self,
        dir: Ino,
        v: usize,
        local: Ino,
        rec: &Inode,
        k: u16,
        creators: &[u32],
    ) -> Result<()> {
        let holder = self.holder_of(dir);
        // Belt 1 (guard-free, after the single-flight slot): a flip that
        // completed between the caller's map probe and our slot take —
        // in-process the ONLY way another flip of `dir` "wins" (its
        // commit is acked, so its marker is visible here) — is found
        // with nothing asked and nothing minted (Issue 20b).
        if self
            .find_dentry_routed(v, local, STRIPED_MARKER)
            .await?
            .is_some()
        {
            self.forget_map(dir);
            return Ok(());
        }
        // Phase A — the SUPPLIED stripes: another appender's mints, each
        // its own transaction under its own guard (an in-process holder
        // serves the ask on THIS table), asked while this op holds NO
        // guard. The 4a law (PR 4 round-1 / D-1c; PR 6's coverage law):
        // an op's guards are ONE canonical `lock_many` — a guard taken
        // while another of ours is held self-deadlocks on a stripe
        // collision, certain at `SQUEEZEFS_DLM_STRIPES=1` (PR 10 review,
        // Issue 28: the flip held `I{dir}` + the marker keys across `K`
        // single-key mints — a 64-stripe flip parked for ever ≈ 0.8 % of
        // the time at the shipped width).
        let k_us = usize::from(k);
        let mut stripes: Vec<Option<Ino>> = vec![None; k_us];
        let mut supplied_by: Vec<Option<u32>> = vec![None; k_us];
        let mut next_creator = creators.iter().copied().filter(|c| Some(*c) != holder);
        for i in 0..k {
            // A creator that declines or is gone is replaced by the next.
            for supplier in next_creator.by_ref() {
                match self.supply_stripe_ino(dir, i, supplier, rec).await {
                    Ok(ino) => {
                        stripes[usize::from(i)] = Some(ino);
                        supplied_by[usize::from(i)] = Some(supplier);
                        break;
                    }
                    Err(e) => log::info!(
                        "stripe {i} of directory {dir}: appender {supplier} declined to \
                         supply a stripe ino ({e}) — asking the next creator"
                    ),
                }
            }
        }
        // Phase B — the remainder's inos, allocated (no guard: the
        // §4.8 monotonic allocation; a refused flip burns them).
        let mut remainder: Vec<(usize, Ino, Ino, u64)> = Vec::new();
        for (i, s) in stripes.iter().enumerate() {
            if s.is_none() {
                let slot = self.remainder_mint_slot(v, holder);
                let (slocal, sglobal) = self.allocate_local_ino_in_slot(v, slot)?;
                remainder.push((i, slocal, sglobal, slot));
            }
        }
        // Phase C — the op's ONE guard set, covering EVERY key its steps
        // touch: `I{dir}`, `I{stripe}` for every stripe minted HERE, and
        // each marker's `D{}` — canonical order and stripe-deduped by
        // `lock_many`; a foreign holder serving the intent's steps under
        // the travelling scope takes nothing (a key outside the scope
        // would be taken at the holder while the initiator's parked
        // `I{dir}` is held: the deadlock PR 6's coverage law forbids).
        let marker_names: Vec<String> = (0..k)
            .map(stripe_marker_name)
            .chain([MIGRATING_MARKER.to_string(), STRIPED_MARKER.to_string()])
            .collect();
        let dents: Vec<(Ino, &str, dlm::LockMode)> = marker_names
            .iter()
            .map(|n| (local, n.as_str(), dlm::LockMode::Exclusive))
            .collect();
        let mut inos: Vec<(Ino, dlm::LockMode)> = std::iter::once(local)
            .chain(remainder.iter().map(|(_, slocal, _, _)| *slocal))
            .map(|l| (l, dlm::LockMode::Exclusive))
            .collect();
        inos.sort_unstable_by_key(|(l, _)| *l);
        inos.dedup_by_key(|(l, _)| *l);
        let mut scope = None;
        let guards: Arc<[dlm::DlmGuard]> =
            Arc::from(self.lock_many_leased(v, &mut scope, &inos, &dents).await?);
        // Belt 2 — under the guards: a map that landed meanwhile (no
        // in-process schedule reaches it past belt 1 — the belt for a
        // wire flip). The supplied mints are named loud, never silent.
        if self
            .find_dentry_routed(v, local, STRIPED_MARKER)
            .await?
            .is_some()
        {
            self.forget_map(dir);
            let supplied: Vec<Ino> = stripes.iter().flatten().copied().collect();
            let by: Vec<Option<u32>> = supplied_by
                .iter()
                .filter(|b| b.is_some())
                .copied()
                .collect();
            self.name_unmapped_stripes(dir, &supplied, &by);
            return Ok(());
        }
        // Phase D — the remainder's records, each minted under the ONE
        // set (its commit co-owns the `Arc` to its terminal outcome).
        for (i, slocal, sglobal, slot) in remainder {
            if let Err(e) = self
                .mint_stripe_record(dir, v, (slocal, sglobal), slot, rec, Arc::clone(&guards))
                .await
            {
                let minted: Vec<Ino> = stripes.iter().flatten().copied().collect();
                let by: Vec<Option<u32>> = stripes
                    .iter()
                    .zip(&supplied_by)
                    .filter(|(s, _)| s.is_some())
                    .map(|(_, b)| *b)
                    .collect();
                self.name_unmapped_stripes(dir, &minted, &by);
                return Err(e);
            }
            stripes[i] = Some(sglobal);
        }
        let stripes: Vec<Ino> = stripes.into_iter().flatten().collect();
        debug_assert_eq!(stripes.len(), k_us, "every stripe supplied or minted");
        self.dir_stripes.learn_stripes(&stripes);
        let target = stripes[usize::from(MARKER_TARGET_INDEX)];
        let mut steps: Vec<XvStep> = stripes
            .iter()
            .enumerate()
            .map(|(i, s)| XvStep::InsertDentry {
                parent: dir,
                name: stripe_marker_name(i as u16),
                child: *s,
                ft_bits: libc::S_IFDIR,
                parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::None),
            })
            .collect();
        steps.push(XvStep::InsertDentry {
            parent: dir,
            name: MIGRATING_MARKER.to_string(),
            child: target,
            ft_bits: libc::S_IFDIR,
            parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::None),
        });
        steps.push(XvStep::InsertDentry {
            parent: dir,
            name: STRIPED_MARKER.to_string(),
            child: target,
            ft_bits: libc::S_IFDIR,
            parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::ExclusiveTimes),
        });
        let plan = XvPlan {
            op: XvOp::StripeDir,
            steps,
        };
        // The negative cache goes BEFORE the write: a plan severed after
        // its commit marker landed must not leave "unstriped" cached.
        self.forget_map(dir);
        if let Err(e) = crossvol_tx::execute(self, &plan, guards).await {
            // The intent may or may not be durable (a failure at step 0
            // committed neither; a later one rolls forward and the
            // stripes become the map's) — the mints are never destroyed
            // here, they are named LOUD as C9's class (Issue 20b: an
            // orphaning that used to be silent).
            self.name_unmapped_stripes(dir, &stripes, &supplied_by);
            return Err(e);
        }
        self.forget_map(dir);
        DIR_STRIPE_FLIPS.fetch_add(1, Ordering::Relaxed);
        DIR_STRIPED_DIRS.fetch_add(1, Ordering::Relaxed);
        self.dir_stripes.census().remove(&dir);
        log::info!(
            "directory {dir} STRIPED into {k} stripes ({} supplied by creators {:?}, {} minted \
             by the holder) — names re-home lazily",
            supplied_by.iter().filter(|s| s.is_some()).count(),
            creators,
            supplied_by.iter().filter(|s| s.is_none()).count()
        );
        Ok(())
    }

    /// Stripes minted for a flip whose intent failed: the mount's own
    /// mints and every supplied one are nameless `nlink 2` directories
    /// until the intent rolls forward (durable) or for ever (never
    /// durable — fsck C9's unreferenced-inode class, reported and
    /// cleaned by the census). Named LOUD, never silent (Issue 20b).
    fn name_unmapped_stripes(&self, dir: Ino, stripes: &[Ino], supplied_by: &[Option<u32>]) {
        for (stripe, by) in stripes.iter().zip(supplied_by) {
            self.dir_stripes.known_stripes.remove_sync(stripe);
            log::warn!(
                "flip of directory {dir} did not commit its map: stripe {stripe} ({}) is an \
                 unreferenced nlink-2 directory until the flip's intent rolls forward — if \
                 the intent never landed it is fsck C9's class (unreferenced inode), \
                 reported and cleaned by the census",
                match by {
                    Some(s) => format!("supplied by appender {s}"),
                    None => "minted by this mount".to_string(),
                }
            );
        }
    }

    /// The slot a stripe the HOLDER mints lands in (the flip's
    /// remainder): a slot `holder` leases on `dir`'s volume `v`, else
    /// this mount's rotor.
    fn remainder_mint_slot(&self, v: usize, holder: Option<u32>) -> u64 {
        match holder {
            Some(h) => self
                .slot_of_appender_on(v, h)
                .unwrap_or_else(|| self.pick_mint_slot(v)),
            None => self.pick_mint_slot(v),
        }
    }

    /// A ROUTING slot appender `appender` leases on `v` — its rotor when
    /// it is this mount, else the first slot tree 0 says it holds.
    fn slot_of_appender_on(&self, v: usize, appender: u32) -> Option<u64> {
        let vol = self.volumes.get(v)?;
        let plane = vol.slot_leases()?;
        if appender == vol.own_appender_id() {
            return Some(self.pick_mint_slot(v));
        }
        let held = plane.table.held_by(appender);
        let forest = held
            .iter()
            .copied()
            .find(|s| *s != super::kv::record::NATIVE_FOREST_SLOT)
            .or_else(|| held.first().copied())?;
        vol.routing_slot_of_forest(forest).ok().map(u64::from)
    }

    /// Mint one stripe ino of `dir` in `slot` on its volume `v` as a
    /// STANDALONE act — the served `SupplyStripeIno` and this mount's own
    /// supply: ONE single-key acquisition (`I{new}`) while the caller
    /// holds no other guard on the table. The flip's own remainder never
    /// comes here: it mints under its ONE guard set
    /// ([`Self::mint_stripe_record`]).
    async fn mint_stripe_in_slot(&self, dir: Ino, v: usize, slot: u64, rec: &Inode) -> Result<Ino> {
        let (local, global) = self.allocate_local_ino_in_slot(v, slot)?;
        let guard: Arc<[dlm::DlmGuard]> = Arc::from(vec![
            self.volumes[v].dlm().lock_inode_exclusive(local).await,
        ]);
        self.mint_stripe_record(dir, v, (local, global), slot, rec, guard)
            .await
    }

    /// The stripe RECORD: an `S_IFDIR` with `dir`'s permission bits and
    /// owner, `nlink 2`, no name, committed under `guards` (a set that
    /// covers `I{local}` — the caller's ONE acquisition). `ino` = the
    /// allocated `(local, global)` pair.
    async fn mint_stripe_record(
        &self,
        dir: Ino,
        v: usize,
        ino: (Ino, Ino),
        slot: u64,
        rec: &Inode,
        guards: Arc<[dlm::DlmGuard]>,
    ) -> Result<Ino> {
        let (local, global) = ino;
        let out = self.volumes[v]
            .routed_mint_inode(
                local,
                libc::S_IFDIR | (rec.mode & 0o7777),
                rec.uid,
                rec.gid,
                0,
                0,
                None,
                guards,
            )
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v);
        }
        out?;
        // Every stripe mint is one supply — asked of a creator or the
        // holder's own (the served side of a wire ask counts here too).
        DIR_STRIPE_SUPPLY_RPCS.fetch_add(1, Ordering::Relaxed);
        self.dir_stripes.learn_stripes(&[global]);
        log::debug!("minted stripe ino {global} (slot {slot}) for directory {dir}");
        Ok(global)
    }

    /// `SupplyStripeIno { dir, i }` to `supplier` (§5.6.5 — the S10
    /// `InoSupply` pattern with the holder as the asker): this process's
    /// own identity mints directly; a declared region or a wire peer is
    /// asked over the S8 wire and mints in ITS slot. Counted
    /// `dir_stripe_supply_rpcs` either way.
    async fn supply_stripe_ino(&self, dir: Ino, i: u16, supplier: u32, rec: &Inode) -> Result<Ino> {
        let (v, _) = self.route_ino(dir);
        let vol = &self.volumes[v];
        if supplier == vol.own_appender_id() {
            let slot = self.pick_mint_slot(v);
            return self.mint_stripe_in_slot(dir, v, slot, rec).await;
        }
        let Some(plane) = vol.slot_leases() else {
            return Err(not_holder(dir, supplier));
        };
        let Some(endpoint) = plane.holders.endpoint(supplier) else {
            return Err(SqueezefsError::refused(
                libc::EAGAIN,
                format!("appender {supplier} has no endpoint bound on this mount"),
            ));
        };
        match crossvol_tx::ship_meta_call(
            &endpoint,
            supplier,
            crate::meta_ship::MetaCall::SupplyStripeIno {
                dir,
                index: u32::from(i),
                supplier,
            },
        )
        .await?
        {
            crate::meta_ship::MetaReply::StripeInoSupplied { ino } => Ok(ino),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "SupplyStripeIno: appender {supplier} answered {other:?} where an ino was due"
            ))),
        }
    }

    /// **The served `SupplyStripeIno`**: mint stripe `index` of `dir` in
    /// a slot `supplier` leases HERE. The wire words are screened first
    /// (`dir` a directory record on its volume; `supplier` an appender
    /// this process serves; `index` inside the stripe namespace).
    pub async fn serve_supply_stripe_ino(
        &self,
        dir: Ino,
        index: u32,
        supplier: u32,
    ) -> Result<Ino> {
        if index >= u32::from(STRIPES_MAX) {
            return Err(SqueezefsError::refused(
                libc::EINVAL,
                format!("SupplyStripeIno: stripe index {index} outside 0..{STRIPES_MAX}"),
            ));
        }
        let (v, local) = self.route_ino(dir);
        self.check_volume_enabled(v)?;
        let rec = self.read_inode_routed(v, local).await?;
        if rec.mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(SqueezefsError::refused(
                libc::ENOTDIR,
                format!("SupplyStripeIno: ino {dir} is not a directory"),
            ));
        }
        let vol = &self.volumes[v];
        let Some(plane) = vol.slot_leases() else {
            return Err(not_holder(dir, supplier));
        };
        let slot = if supplier == vol.own_appender_id() {
            self.pick_mint_slot(v)
        } else {
            let held: Vec<ForestSlot> = plane
                .table
                .held_by(supplier)
                .into_iter()
                .filter(|s| plane.gate.is_leased(*s))
                .collect();
            let Some(forest) = held
                .iter()
                .copied()
                .find(|s| *s != super::kv::record::NATIVE_FOREST_SLOT)
                .or_else(|| held.first().copied())
            else {
                return Err(SqueezefsError::refused(
                    libc::EAGAIN,
                    format!(
                        "SupplyStripeIno: appender {supplier} leases no slot this mount serves \
                         on directory {dir}'s volume"
                    ),
                ));
            };
            u64::from(vol.routing_slot_of_forest(forest)?)
        };
        self.mint_stripe_in_slot(dir, v, slot, &rec).await
    }

    /// **The automatic trigger** (§5.6.5): the holder served one foreign
    /// insert of `name` into `dir` from `creator`. Counted over the
    /// `T_idle` half-window pair per directory — one map increment and
    /// two O(1) reads on the served path (Issue 11); the heaviest-first
    /// sort runs once, when the decision is due. When the window's
    /// foreign ships exceed `N_floor` from ≥ 2 creators the flip is
    /// spawned (single-flight, off the serving path). ONE creator never
    /// flips — that is a handover candidate, not a striping one. A known
    /// STRIPE, an already-striped directory and a marker insert (the
    /// flip's own steps, shipped to the holder by their suppliers) feed
    /// nothing (Issue 1 / Issue 11).
    /// Is GLOBAL `dir` in the STRIPING mechanism's domain — a known stripe,
    /// or a directory whose map this mount has read? A ship into it is
    /// aggregate by construction (many creators into one directory, the
    /// flip's shape) and never feeds the handover's dominance window (PR
    /// 13, gate 3b: a stripe the manager supplied moved to the first
    /// requester whose few ships beat the manager's own few into that
    /// 1/K shard — a legal verdict of the law over a slot the striping
    /// already spread K ways; §5.6.5's split: one dominating creator is a
    /// handover candidate, MANY are a striping one). One `scc` probe each.
    pub(super) fn is_striping_domain(&self, dir: Ino) -> bool {
        self.dir_stripes.known_stripes.contains_sync(&dir)
            || self.dir_stripes.maps.contains_sync(&dir)
    }

    pub fn note_served_insert(&self, dir: Ino, name: &str, creator: u32) {
        let (v, _) = self.route_ino(dir);
        let Some(plane) = self.volumes.get(v).and_then(|vol| vol.slot_leases()) else {
            return;
        };
        if stripe_count() <= 1
            || is_marker_name(name)
            || self.dir_stripes.known_stripes.contains_sync(&dir)
            || self.dir_stripes.maps.contains_sync(&dir)
        {
            return;
        }
        let now = KvMetaBackend::now_ns_pub();
        let t_idle = plane.t_idle_ns();
        let n_floor = plane.n_floor();
        let epoch = crate::slot_lease_core::DominanceWindow::epoch(now, t_idle);
        let decision = {
            let mut census = self.dir_stripes.census();
            if census.len() >= CENSUS_DIRS_MAX && !census.contains_key(&dir) {
                if let Some(victim) = census.iter().min_by_key(|(_, c)| c.epoch).map(|(d, _)| *d) {
                    census.remove(&victim);
                }
            }
            let c = census.entry(dir).or_default();
            c.align(epoch);
            c.note(creator);
            c.due(n_floor).then(|| FlipDecision {
                dir,
                creators: c.heaviest().into_iter().map(|(c, _)| c).collect(),
            })
        };
        if let Some(d) = decision {
            if self.dir_stripes.in_flight.contains_sync(&dir) {
                return;
            }
            let Some(me) = self.self_arc() else {
                return;
            };
            crate::meta_exec::spawn_meta_contained("dir_stripe_flip", async move {
                // The belt before the flip: a stripe's census (a served
                // insert into `D_i` names `D_i` as its parent) must never
                // reach the flip — learnt once, quiet after.
                match me.is_stripe(d.dir).await {
                    Ok(true) => {
                        me.dir_stripes.learn_stripes(&[d.dir]);
                        me.dir_stripes.census().remove(&d.dir);
                        log::debug!(
                            "directory stripe {}: the trigger census is dropped — a stripe never \
                             carries a map",
                            d.dir
                        );
                        return;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        log::warn!(
                            "automatic stripe flip of directory {}: the stripe check failed \
                             ({e}) — declined this window",
                            d.dir
                        );
                        return;
                    }
                }
                if let Err(e) = me
                    .flip_dir(
                        d.dir,
                        stripe_count(),
                        &d.creators,
                        StripeBelt::KnownDirectory,
                    )
                    .await
                {
                    log::warn!("automatic stripe flip of directory {} declined: {e}", d.dir);
                }
            });
        }
    }

    /// Flips and migrations this mount is running right now — a CONTRACT
    /// face (the contracts drain it before a shutdown).
    #[doc(hidden)]
    pub fn stripe_work_in_flight(&self) -> usize {
        self.dir_stripes.in_flight.len()
    }

    /// The census as the contracts read it: creators of `dir`, heaviest
    /// first — a CONTRACT face.
    #[doc(hidden)]
    pub fn stripe_census(&self, dir: Ino) -> Vec<(u32, u64)> {
        self.dir_stripes
            .census()
            .get(&dir)
            .map(|c| c.heaviest())
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------
    // The lazy migration.
    // -----------------------------------------------------------------

    /// Run `dir`'s migration in the background, single-flight.
    fn kick_migration(&self, dir: Ino) {
        if TEST_STRIPE_HOLD_MIGRATION.load(Ordering::Relaxed) {
            log::debug!("directory {dir}: migration held by the test seam");
            return;
        }
        if self.dir_stripes.in_flight.insert_sync(dir).is_err() {
            log::debug!("directory {dir}: migration already in flight");
            return;
        }
        let Some(me) = self.self_arc() else {
            // A bare constructor (no set handle): the synchronous
            // `migrate_dir` is the only face; the next access re-kicks.
            log::debug!("directory {dir}: no set handle — the migration waits for a caller");
            self.dir_stripes.in_flight.remove_sync(&dir);
            return;
        };
        log::debug!("directory {dir}: stripe migration kicked in the background");
        crate::meta_exec::spawn_meta_contained("dir_stripe_migrate", async move {
            log::debug!("directory {dir}: stripe migration running");
            let out = me.migrate_dir_inner(dir).await;
            me.dir_stripes.in_flight.remove_sync(&dir);
            if let Err(e) = out {
                log::warn!(
                    "directory {dir}: stripe migration paused ({e}) — resumed at the next \
                     access by the holder"
                );
            }
        });
    }

    /// **The migration, synchronously** — a CONTRACT face (the resume
    /// runs [`Self::kick_migration`] → `migrate_dir_inner` in the
    /// background; a contract drives the same body and reads its count):
    /// every non-marker name in `dir`'s own tree moves to its stripe —
    /// insert into the stripe FIRST, remove from `dir` second, each pair
    /// one intent, so a kill between the two leaves the name in both
    /// places and the stripe wins — then the `migrating` marker is
    /// removed once `dir`'s tree is re-verified EMPTY of user names under
    /// the exclusive guard. Idempotent: names already in their stripe are
    /// dropped from `dir` alone. Returns the names moved by this call.
    #[doc(hidden)]
    pub async fn migrate_dir(&self, dir: Ino) -> Result<u64> {
        if self.dir_stripes.in_flight.insert_sync(dir).is_err() {
            return Ok(0);
        }
        let out = self.migrate_dir_inner(dir).await;
        self.dir_stripes.in_flight.remove_sync(&dir);
        out
    }

    /// The flag clear's re-verify rounds: a name that landed in `dir`'s
    /// own tree behind the scan (a create parked on `I{dir}` across the
    /// flip, a stale-route insert) is moved and the scan repeated; past
    /// the bound the flag stays (resumed at the next access) — a clear is
    /// never blind (Issue 4c). The bound every bounded cover loop of the
    /// program shares (`checkpoint::COVER_CYCLES_MAX` = 64). Tie-tested.
    pub const MIGRATION_ROUNDS_MAX: usize = super::kv::checkpoint::COVER_CYCLES_MAX as usize;

    async fn migrate_dir_inner(&self, dir: Ino) -> Result<u64> {
        let (v, local) = self.route_ino(dir);
        self.check_volume_enabled(v)?;
        let Some(map) = self.read_map_uncached(dir).await? else {
            return Ok(0);
        };
        if !map.migrating {
            return Ok(0);
        }
        let mut moved = 0u64;
        for round in 0..Self::MIGRATION_ROUNDS_MAX {
            moved += self.migrate_scan(dir, v, local, &map).await?;
            // Nothing but markers left? Decided UNDER the exclusive guard
            // the clear takes: a name inserted into `dir`'s tree after
            // its page was scanned would otherwise be stranded by the
            // clear (review round 1, Issue 4c).
            let mut scope = None;
            let guards: Arc<[dlm::DlmGuard]> = Arc::from(
                self.lock_many_leased(
                    v,
                    &mut scope,
                    &[(local, dlm::LockMode::Exclusive)],
                    &[(local, MIGRATING_MARKER, dlm::LockMode::Exclusive)],
                )
                .await?,
            );
            if !self.page_names(dir, 0, 1).await?.is_empty() {
                drop(guards);
                log::debug!(
                    "directory {dir}: a name landed in its own tree behind the migration's \
                     scan (round {round}) — re-scanning before the flag clears"
                );
                continue;
            }
            if self
                .find_dentry_routed(v, local, MIGRATING_MARKER)
                .await?
                .is_some()
            {
                let plan = XvPlan {
                    op: XvOp::StripeDir,
                    steps: vec![XvStep::RemoveDentry {
                        parent: dir,
                        name: MIGRATING_MARKER.to_string(),
                        expect_child: map.stripes[usize::from(MARKER_TARGET_INDEX)],
                        parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::None),
                    }],
                };
                crossvol_tx::execute(self, &plan, guards).await?;
            }
            self.forget_map(dir);
            log::info!("directory {dir}: stripe migration complete — {moved} name(s) re-homed");
            return Ok(moved);
        }
        log::warn!(
            "directory {dir}: names kept landing in its own tree across {} migration rounds — \
             the flag stays and the migration resumes at the next access ({moved} re-homed)",
            Self::MIGRATION_ROUNDS_MAX
        );
        Ok(moved)
    }

    /// One scan of `dir`'s own tree: every non-marker name moved.
    async fn migrate_scan(&self, dir: Ino, v: usize, local: Ino, map: &StripeMap) -> Result<u64> {
        let mut moved = 0u64;
        let mut cursor = 0u64;
        loop {
            let page = self.volumes[v]
                .readdir_page(local, cursor, MIGRATION_SCAN_PAGE)
                .await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = *last;
            for (_, entry) in page {
                if is_marker_name(&entry.name) {
                    continue;
                }
                // Counted only when THIS call moved it (a name a mutation
                // re-homed or unlinked meanwhile is nobody's move).
                if self.migrate_one(dir, v, local, map, &entry).await? {
                    moved += 1;
                    DIR_STRIPE_MIGRATED_NAMES.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(moved)
    }

    /// Move one name into its stripe: `[InsertDentry @ stripe,
    /// RemoveDentry @ dir]` under the op's guards on both homes — the ONE
    /// move the background migration and every mutation's re-home run
    /// (Issue 4), so the two serialize under the same guards and the
    /// second finds the work done. A stripe already holding the name
    /// with ANOTHER child is the LWW rule's verdict: the stripe's entry
    /// wins and `dir`'s is the shadowed loser — its name goes and its
    /// inode's count follows (a file: `nlink − 1`; a directory loser is
    /// LEFT and named loud — fsck C17's `unmigrated-name` — rather than
    /// unlinked over its contents).
    async fn migrate_one(
        &self,
        dir: Ino,
        v: usize,
        local: Ino,
        map: &StripeMap,
        entry: &DirEntry,
    ) -> Result<bool> {
        let (index, stripe) = map.stripe_for(self.hash54_on(v, &entry.name));
        let (sv, slocal) = self.route_ino(stripe);
        // Every stripe lives on `dir`'s volume by construction (both mint
        // sites mint on `route_ino(dir).0`): ONE volume's lock set.
        debug_assert_eq!(sv, v, "a stripe lives on its directory's volume");
        let is_dir = entry.file_type & libc::S_IFMT == libc::S_IFDIR;
        let update = if is_dir {
            RoutedParentUpdate::ExclusiveTimesBump
        } else {
            RoutedParentUpdate::None
        };
        let mut scope = None;
        let (cv, clocal) = self.route_ino(entry.ino);
        let mut inos = vec![
            (local, dlm::LockMode::Exclusive),
            (slocal, dlm::LockMode::Exclusive),
        ];
        if cv == v {
            // The loser's count step needs `I{child}` (the LWW arm).
            inos.push((clocal, dlm::LockMode::Exclusive));
        }
        inos.sort_unstable_by_key(|(l, _)| *l);
        inos.dedup_by_key(|(l, _)| *l);
        let dents = vec![
            (local, entry.name.as_str(), dlm::LockMode::Exclusive),
            (slocal, entry.name.as_str(), dlm::LockMode::Exclusive),
        ];
        let mut guards = self.lock_many_leased(v, &mut scope, &inos, &dents).await?;
        if cv != v {
            guards.extend(
                self.lock_many_leased(cv, &mut scope, &[(clocal, dlm::LockMode::Exclusive)], &[])
                    .await?,
            );
        }
        let guards: Arc<[dlm::DlmGuard]> = Arc::from(guards);
        // Revalidate under the guards: the name may have been unlinked or
        // migrated by a concurrent op.
        let Some((cur_child, _)) = self.find_dentry_routed(v, local, &entry.name).await? else {
            return Ok(false);
        };
        if cur_child != entry.ino {
            return Ok(false);
        }
        let in_stripe = self.find_dentry_routed(sv, slocal, &entry.name).await?;
        let mut steps = Vec::with_capacity(2);
        match in_stripe {
            None => steps.push(XvStep::InsertDentry {
                parent: stripe,
                name: entry.name.clone(),
                child: entry.ino,
                ft_bits: entry.file_type & libc::S_IFMT,
                parent_update: crossvol_tx::parent_update_code(update),
            }),
            Some((winner, _)) if winner == entry.ino => {}
            Some((winner, _)) => {
                // The LWW rule: `dir`'s entry is the shadowed loser.
                if is_dir {
                    log::warn!(
                        "directory {dir}: name {:?} names directory {} in its own tree and \
                         directory {winner} in stripe {index} — the stripe's wins; the loser \
                         is LEFT in place (fsck C17 reports it as unmigrated-name) rather \
                         than unlinked over its contents",
                        entry.name,
                        entry.ino
                    );
                    return Ok(false);
                }
                let Some(loser) = self.volumes[cv].read_inode_value_routed(clocal).await? else {
                    // No record: the dangling name alone goes.
                    steps.push(XvStep::RemoveDentry {
                        parent: dir,
                        name: entry.name.clone(),
                        expect_child: entry.ino,
                        parent_update: crossvol_tx::parent_update_code(update),
                    });
                    let plan = XvPlan {
                        op: XvOp::StripeDir,
                        steps,
                    };
                    crossvol_tx::execute(self, &plan, guards).await?;
                    return Ok(true);
                };
                log::warn!(
                    "directory {dir}: name {:?} names ino {} in its own tree and ino {winner} \
                     in stripe {index} — the stripe's wins (LWW); the shadowed entry is \
                     unlinked (its inode's nlink {} → {})",
                    entry.name,
                    entry.ino,
                    loser.nlink,
                    loser.nlink.saturating_sub(1)
                );
                steps.push(XvStep::SetNlink {
                    ino: entry.ino,
                    pre: loser.nlink,
                    post: loser.nlink.saturating_sub(1),
                    ctime: Some(KvMetaBackend::now_ns_pub()),
                });
            }
        }
        steps.push(XvStep::RemoveDentry {
            parent: dir,
            name: entry.name.clone(),
            expect_child: entry.ino,
            parent_update: crossvol_tx::parent_update_code(update),
        });
        let plan = XvPlan {
            op: XvOp::StripeDir,
            steps,
        };
        let done = crossvol_tx::execute(self, &plan, guards).await?;
        if is_dir {
            self.note_dir_parent(entry.ino, dir, &entry.name);
        }
        log::debug!(
            "directory {dir}: name {:?} re-homed into stripe {index} ({stripe}) — {:?}",
            entry.name,
            done.outcomes.iter().map(|o| o.status).collect::<Vec<_>>()
        );
        Ok(true)
    }

    /// The map straight from the markers (no cache) — the migration's
    /// and the rmdir's read.
    async fn read_map_uncached(&self, dir: Ino) -> Result<Option<Arc<StripeMap>>> {
        self.forget_map(dir);
        // `stripe_map` re-reads and re-caches; the kick it would spawn is
        // a no-op while this caller holds the in-flight slot.
        self.stripe_map(dir).await
    }

    // -----------------------------------------------------------------
    // readdir + the attribute fold.
    // -----------------------------------------------------------------

    /// **The K-way `readdir` merge** (§5.6.5): `max` entries strictly
    /// after `offset` merged from every stripe and from `dir`'s own tree
    /// (its permanent secondary home) in `(hash54, coll)` order — the
    /// shipped cookie order, since every stripe lives on `dir`'s volume
    /// and shares its seed — with a name present in both a stripe and
    /// `dir` served from the stripe. The cookie of the last entry is the
    /// exact continuation: every stream resumes strictly after it, so a
    /// name present for the whole scan is returned exactly once under
    /// concurrent creates/unlinks.
    ///
    /// STREAMED (Issue 10): each stream is refilled only when its head
    /// is consumed, in pages of `merge_stream_page` entries, so a
    /// merged page reads `≤ max + (K + 1) × page` raw entries whatever
    /// the stripes hold — never `K × max` (the first build materialized
    /// a full page per stripe and kept `max`).
    pub async fn readdir_striped_page(
        &self,
        map: &StripeMap,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, DirEntry)>> {
        DIR_STRIPE_READDIR_MERGES.fetch_add(1, Ordering::Relaxed);
        let page = Self::merge_stream_page(max, map.k());
        // Stream K is `dir`'s own tree.
        let homes: Vec<Ino> = map.stripes.iter().copied().chain([map.dir]).collect();
        let own = homes.len() - 1;
        let mut streams: Vec<MergeStream> = homes
            .iter()
            .map(|_| MergeStream {
                buf: std::collections::VecDeque::new(),
                cursor: offset,
                exhausted: false,
            })
            .collect();
        for (i, s) in streams.iter_mut().enumerate() {
            self.refill_stream(homes[i], s, page).await?;
        }
        let (v, _) = self.route_ino(map.dir);
        let mut out: Vec<(u64, DirEntry)> = Vec::with_capacity(max);
        while out.len() < max {
            // The smallest head across streams.
            let Some(next) = streams
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.buf.front().map(|(c, _)| (*c, i)))
                .min()
                .map(|(_, i)| i)
            else {
                break;
            };
            let Some((cookie, entry)) = streams[next].buf.pop_front() else {
                break;
            };
            if next == own {
                // `dir`'s own copy of a name its stripe holds too is the
                // LWW loser: the stripe's entry (same hash, so adjacent
                // in this order — refill that stream past the hash first)
                // is the one listed.
                let h = self.hash54_on(v, &entry.name);
                let si = usize::from(stripe_of(h, map.k()));
                while !streams[si].exhausted
                    && streams[si]
                        .buf
                        .back()
                        .is_none_or(|(c, _)| readdir_cookie_hash54(*c) <= h)
                {
                    self.refill_stream(homes[si], &mut streams[si], page)
                        .await?;
                }
                if streams[si].buf.iter().any(|(_, e)| e.name == entry.name)
                    || out
                        .iter()
                        .rev()
                        .take_while(|(c, _)| readdir_cookie_hash54(*c) == h)
                        .any(|(_, e)| e.name == entry.name)
                {
                    continue;
                }
            }
            out.push((cookie, entry));
            if streams[next].buf.is_empty() && !streams[next].exhausted {
                self.refill_stream(homes[next], &mut streams[next], page)
                    .await?;
            }
        }
        Ok(out)
    }

    /// The per-stream refill page: `max / (K + 1)` floored at
    /// [`MERGE_STREAM_PAGE_FLOOR`] — so a merged page's raw read is
    /// bounded by `max + (K + 1) × page ≈ 2 × max` at the floor's scale,
    /// and a FUSE page of 4,096 over 64 stripes reads ≈ 64 entries per
    /// stream per refill.
    fn merge_stream_page(max: usize, k: u16) -> usize {
        (max / (usize::from(k) + 1)).max(MERGE_STREAM_PAGE_FLOOR)
    }

    /// Refill one stream: the next `page` NON-marker entries strictly
    /// after its cursor (a page whose marker entries counted against the
    /// budget would end short of the names below the merged page's last
    /// cookie, and the continuation would skip them).
    async fn refill_stream(&self, home: Ino, s: &mut MergeStream, page: usize) -> Result<()> {
        let (v, local) = self.route_ino(home);
        self.check_volume_enabled(v)?;
        let mut got = 0usize;
        while got < page && !s.exhausted {
            let want = page - got;
            let raw = self.volumes[v].readdir_page(local, s.cursor, want).await?;
            if raw.len() < want {
                s.exhausted = true;
            }
            let Some((last, _)) = raw.last() else {
                break;
            };
            s.cursor = *last;
            for (c, e) in raw {
                if !is_marker_name(&e.name) {
                    s.buf.push_back((c, e));
                    got += 1;
                }
            }
        }
        Ok(())
    }

    /// One stream's page: the first `max` NON-marker entries strictly
    /// after `offset` (the emptiness probes' read — `max = 1` answers "any
    /// user name?" whatever the marker count, Issue 5).
    async fn page_names(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<(u64, DirEntry)>> {
        let mut s = MergeStream {
            buf: std::collections::VecDeque::new(),
            cursor: offset,
            exhausted: false,
        };
        self.refill_stream(dir, &mut s, max).await?;
        Ok(s.buf.into_iter().collect())
    }

    /// **The attribute fold** of a striped directory: `nlink = D.nlink +
    /// Σ (D_i.nlink − 2)`, `mtime`/`ctime` = the max over `D` and its
    /// stripes (a dying stripe — `nlink 0`, mid-`rmdir` — contributes
    /// nothing). Returns the `(mtime, ctime)` to PERSIST onto `D`'s record
    /// when a stripe's times passed `D`'s stored ones (the caller runs
    /// [`Self::persist_striped_times`] after its own guard drops — the
    /// fold is read under a SHARED `I{D}` guard, the persist needs the
    /// exclusive one).
    pub async fn fold_striped_attrs(
        &self,
        inode: &mut Inode,
        map: &StripeMap,
    ) -> Result<Option<(u64, u64)>> {
        let mut extra_links: u64 = 0;
        let (mut mtime, mut ctime) = (inode.mtime, inode.ctime);
        for stripe in &map.stripes {
            let (sv, slocal) = self.route_ino(*stripe);
            let Some(rec) = self.volumes[sv].read_inode_value_routed(slocal).await? else {
                continue;
            };
            if rec.nlink >= 2 {
                extra_links += u64::from(rec.nlink - 2);
            }
            mtime = mtime.max(rec.mtime);
            ctime = ctime.max(rec.ctime);
        }
        let persist = (mtime > inode.mtime || ctime > inode.ctime).then_some((mtime, ctime));
        inode.nlink = u32::try_from(u64::from(inode.nlink) + extra_links).unwrap_or(u32::MAX);
        inode.mtime = mtime;
        inode.ctime = ctime;
        Ok(persist)
    }

    /// Persist a fold's times onto `D`'s own record — hygiene, off the
    /// serve (the fold is exact from the stripes at every read; a failed
    /// persist costs nothing but the next one), ONCE PER CHECKPOINT
    /// CADENCE per directory (Issue 8: the write recalls every reader's
    /// token on `D`, so it must never run at the `stat` rate — the
    /// design's "batched per flush cadence"). Only the holder writes.
    pub async fn persist_striped_times(&self, dir: Ino, mtime: u64, ctime: u64) {
        if !self.served_here(dir) {
            return;
        }
        let now = crate::mono_core::monotonic_ns_u64();
        let cadence_ns = super::kv::checkpoint::checkpoint_landing_ceiling_derived() * 1_000_000;
        let due = self
            .dir_stripes
            .times_persisted_ns
            .read_sync(&dir, |_, last| now.saturating_sub(*last) >= cadence_ns)
            .unwrap_or(true);
        if !due {
            return;
        }
        let _ = self
            .dir_stripes
            .times_persisted_ns
            .entry_sync(dir)
            .and_modify(|t| *t = now)
            .or_insert(now);
        let (v, local) = self.route_ino(dir);
        let guards: Arc<[dlm::DlmGuard]> = Arc::from(vec![
            self.volumes[v].dlm().lock_inode_exclusive(local).await,
        ]);
        // The record may have moved past the fold meanwhile: never lower
        // a stored time.
        let Ok(Some(cur)) = self.volumes[v].read_inode_value_routed(local).await else {
            return;
        };
        let mtime = mtime.max(cur.mtime);
        let ctime = ctime.max(cur.ctime);
        if mtime == cur.mtime && ctime == cur.ctime {
            return;
        }
        if self.volumes[v]
            .setattr_locked(
                local,
                None,
                None,
                None,
                None,
                None,
                Some(mtime),
                Some(ctime),
                guards,
            )
            .await
            .is_ok()
        {
            DIR_STRIPE_TIME_BATCHES.fetch_add(1, Ordering::Relaxed);
        }
    }

    // -----------------------------------------------------------------
    // rmdir.
    // -----------------------------------------------------------------

    /// Does stripe `stripe` hold any user entry? Read under the rmdir's
    /// OWN exclusive guard on the stripe — exact: every insert that took
    /// the stripe's guard has landed and none can be in flight. A stripe
    /// another appender holds per tree 0 is asked `IsEmpty { dir, scope }`
    /// (the holder reads under the travelling scope — `Covered` — and
    /// never takes a lock of its own); a stripe served HERE by a caller
    /// with NO scope (the S8 owner-execute context, `executing_for_ship_
    /// client`) is read locally under the guard the caller holds — a
    /// served `Take` in this same table would wait behind itself (review
    /// round 1, Issue 9: held-ness is the caller's fact, never inferred
    /// from the scope word).
    async fn stripe_is_empty(&self, stripe: Ino, scope: u64) -> Result<bool> {
        let (v, _) = self.route_ino(stripe);
        let holder = match self.holder_of(stripe) {
            Some(h) if h != self.volumes[v].own_appender_id() => h,
            _ => return Ok(self.page_names(stripe, 0, 1).await?.is_empty()),
        };
        if scope == 0 && self.served_here(stripe) {
            return Ok(self.page_names(stripe, 0, 1).await?.is_empty());
        }
        let endpoint = self.volumes[v]
            .slot_leases()
            .and_then(|p| p.holders.endpoint(holder))
            .ok_or_else(|| {
                SqueezefsError::refused(
                    libc::EAGAIN,
                    format!("stripe {stripe}'s holder (appender {holder}) has no endpoint bound"),
                )
            })?;
        DIR_STRIPE_SHIPS.fetch_add(1, Ordering::Relaxed);
        match crossvol_tx::ship_meta_call(
            &endpoint,
            holder,
            crate::meta_ship::MetaCall::IsEmpty { dir: stripe, scope },
        )
        .await?
        {
            crate::meta_ship::MetaReply::Empty(empty) => Ok(empty),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "IsEmpty: appender {holder} answered {other:?} where a verdict was due"
            ))),
        }
    }

    /// The served `IsEmpty { dir, scope }` (the wire's door): read under
    /// the scope's parked guards when they cover `I{dir}`, else under an
    /// exclusive guard taken here.
    pub async fn serve_is_empty_scoped(
        &self,
        dir: Ino,
        scope: crossvol_tx::GuardScope<'_>,
    ) -> Result<bool> {
        let (v, local) = self.route_ino(dir);
        self.check_volume_enabled(v)?;
        let needed = crossvol_tx::step_stripes(
            self.volumes[v].dlm(),
            v,
            &crossvol_tx::XvLocalStep::TouchCtime {
                local_ino: local,
                ctime: 0,
            },
        );
        let covered = matches!(
            crossvol_tx::serve_guards_for(scope, &needed),
            crossvol_tx::ServeGuards::Covered
        );
        self.serve_is_empty_covered(dir, covered).await
    }

    /// No user entry under `dir` — markers are skipped whatever their
    /// count (Issue 5: an 8-entry read called a directory carrying 66
    /// markers empty); `held` = the caller's guard set already holds
    /// `I{dir}` (never re-taken — it would wait behind itself).
    async fn serve_is_empty_covered(&self, dir: Ino, held: bool) -> Result<bool> {
        let (v, local) = self.route_ino(dir);
        self.check_volume_enabled(v)?;
        let _g = if held {
            None
        } else {
            Some(self.volumes[v].dlm().lock_inode_exclusive(local).await)
        };
        Ok(self.page_names(dir, 0, 1).await?.is_empty())
    }

    /// Destroy stripe `stripe`'s record (`nlink` is 0 by the rmdir's
    /// intent): here when this appender holds it, `DestroyStripe` to its
    /// holder otherwise (the guards are dropped by now — the served side
    /// takes its own). Hygiene: a stripe left undestroyed is an `nlink 0`
    /// corpse named by no map — the next mount's sweep reclaims it.
    async fn destroy_stripe(&self, stripe: Ino) -> Result<()> {
        let (v, _) = self.route_ino(stripe);
        let holder = match self.holder_of(stripe) {
            Some(h) if h != self.volumes[v].own_appender_id() => h,
            _ => return self.serve_destroy_stripe(stripe).await,
        };
        let endpoint = self.volumes[v]
            .slot_leases()
            .and_then(|p| p.holders.endpoint(holder))
            .ok_or_else(|| {
                SqueezefsError::refused(
                    libc::EAGAIN,
                    format!("stripe {stripe}'s holder (appender {holder}) has no endpoint bound"),
                )
            })?;
        DIR_STRIPE_SHIPS.fetch_add(1, Ordering::Relaxed);
        match crossvol_tx::ship_meta_call(
            &endpoint,
            holder,
            crate::meta_ship::MetaCall::DestroyStripe { stripe },
        )
        .await?
        {
            crate::meta_ship::MetaReply::Unit => Ok(()),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "DestroyStripe: appender {holder} answered {other:?} where Unit was due"
            ))),
        }
    }

    /// The served `DestroyStripe { stripe }`: refuses (`EBUSY`) a stripe
    /// that still has entries or a live `nlink` — the wire word is judged
    /// by durable state, never trusted; destroys the record otherwise
    /// (idempotent: an absent record is `Ok`).
    pub async fn serve_destroy_stripe(&self, stripe: Ino) -> Result<()> {
        let (v, local) = self.route_ino(stripe);
        self.check_volume_enabled(v)?;
        let vol = &self.volumes[v];
        let Some(rec) = vol.read_inode_value_routed(local).await? else {
            return Ok(());
        };
        if rec.nlink != 0 || rec.mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(SqueezefsError::refused(
                libc::EBUSY,
                format!(
                    "DestroyStripe: ino {stripe} is live (nlink {}) — not a dying stripe",
                    rec.nlink
                ),
            ));
        }
        if !self.serve_is_empty_covered(stripe, false).await? {
            return Err(SqueezefsError::refused(
                libc::ENOTEMPTY,
                format!("DestroyStripe: stripe {stripe} still holds entries"),
            ));
        }
        self.dir_stripes.known_stripes.remove_sync(&stripe);
        vol.destroy_inodes(&[local]).await
    }

    /// **`rmdir` of a striped directory** (§5.6.5 — "K `DestroyStripe`
    /// ships + `D`'s destroy in ONE intent"; R26). `key_parent` is where
    /// `D`'s own dentry lives (its parent, or the parent's stripe). Under
    /// the S10 mutation permit and the §5.5.2a slot gate over every
    /// participant, ONE guard set held for the whole act: `I{key_parent}`
    /// + `D{key_parent, name}`, `I{D}` exclusive, `I{D_i}` EXCLUSIVE on
    /// every stripe (a foreign stripe's travels to its holder — PR 6),
    /// `D{D, marker}` on every marker. Then, having written NOTHING yet:
    /// the dentry revalidated (`ENOENT` if gone), the map re-read from the
    /// markers under the guard (it must be the caller's), `D`'s own tree
    /// probed for a user name, every stripe's count (`nlink 2` — a
    /// subdirectory is `> 2`) and every stripe `IsEmpty` — exact under the
    /// exclusive guards; `ENOTEMPTY` refuses with nothing to undo. Then
    /// ONE intent: the dentry out of its parent, the markers out of `D`
    /// (the commit marker FIRST — a reader between steps sees no map),
    /// `SetNlink 2 → 0` on every stripe (the dying mark) and on `D`. A kill
    /// at any step rolls forward whole at the next lessee (Issue 2). The
    /// stripes' records are destroyed after, as hygiene.
    pub async fn rmdir_striped(&self, key_parent: Ino, name: &str, map: &StripeMap) -> Result<Ino> {
        let dir = map.dir;
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[key_parent, dir]).await;
        let mut participants: Vec<Ino> = vec![key_parent, dir];
        participants.extend(map.stripes.iter().copied());
        let _gate = self.slot_gate_enter(&participants).await;
        let (pv, plocal) = self.route_ino(key_parent);
        let (dv, dlocal) = self.route_ino(dir);
        self.check_volume_enabled(pv)?;
        self.check_volume_enabled(dv)?;
        // The stripes live on `dir`'s volume by construction.
        let marker_names: Vec<String> = (0..map.k())
            .map(stripe_marker_name)
            .chain([MIGRATING_MARKER.to_string(), STRIPED_MARKER.to_string()])
            .collect();
        // Per-volume lock sets in ascending volume order, each canonical
        // (I before D, stripe-deduped by `lock_many`).
        let mut per_volume: BTreeMap<
            usize,
            (Vec<(Ino, dlm::LockMode)>, Vec<(Ino, &str, dlm::LockMode)>),
        > = BTreeMap::new();
        {
            let (inos, dents) = per_volume.entry(pv).or_default();
            inos.push((plocal, dlm::LockMode::Exclusive));
            dents.push((plocal, name, dlm::LockMode::Exclusive));
        }
        {
            let (inos, dents) = per_volume.entry(dv).or_default();
            inos.push((dlocal, dlm::LockMode::Exclusive));
            for s in &map.stripes {
                inos.push((self.route_ino(*s).1, dlm::LockMode::Exclusive));
            }
            for m in &marker_names {
                dents.push((dlocal, m.as_str(), dlm::LockMode::Exclusive));
            }
        }
        let mut guards = Vec::new();
        let mut scope = None;
        for (v_idx, (mut inos, dents)) in per_volume {
            inos.sort_unstable_by_key(|(l, _)| *l);
            inos.dedup_by_key(|(l, _)| *l);
            guards.extend(
                self.lock_many_leased(v_idx, &mut scope, &inos, &dents)
                    .await?,
            );
        }
        let guards: Arc<[dlm::DlmGuard]> = Arc::from(guards);
        let scope_word = crossvol_tx::scope_of(&guards);

        // ---- Everything below reads; nothing is written before the intent.
        match self.find_dentry_routed(pv, plocal, name).await? {
            Some((child, _)) if child == dir => {}
            Some((child, _)) => {
                return Err(SqueezefsError::refused(
                    libc::EAGAIN,
                    format!("rmdir {name:?}: the name now names ino {child}, not {dir} — retry"),
                ))
            }
            None => {
                return Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Dentry not found",
                )))
            }
        }
        let live = self.read_map_from_markers(dir, dv, dlocal).await?;
        let Some(live) = live.filter(|m| m.stripes == map.stripes) else {
            return Err(SqueezefsError::refused(
                libc::EAGAIN,
                format!("rmdir of directory {dir}: its stripe map changed under the op — retry"),
            ));
        };
        let dir_rec = self.read_inode_routed(dv, dlocal).await?;
        if dir_rec.nlink > 2 || !self.page_names(dir, 0, 1).await?.is_empty() {
            // A user name (or a subdirectory) still in `D`'s own tree: an
            // unmigrated one, or a stale-route insert — `ENOTEMPTY` (its
            // next touch re-homes it).
            return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                libc::ENOTEMPTY,
            )));
        }
        let mut stripe_counts = Vec::with_capacity(map.stripes.len());
        for stripe in &map.stripes {
            let (sv, slocal) = self.route_ino(*stripe);
            let n = match self.volumes[sv].read_inode_value_routed(slocal).await? {
                Some(r) if r.nlink == 2 || r.nlink == 0 => r.nlink,
                Some(r) => {
                    log::debug!(
                        "rmdir {dir}: stripe {stripe} nlink {} — holds subdirectories",
                        r.nlink
                    );
                    return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                        libc::ENOTEMPTY,
                    )));
                }
                None => 0,
            };
            stripe_counts.push(n);
        }
        for stripe in &map.stripes {
            if !self.stripe_is_empty(*stripe, scope_word).await? {
                return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOTEMPTY,
                )));
            }
        }
        let hold_ms = TEST_STRIPE_RMDIR_HOLD_MS.load(Ordering::Relaxed);
        if hold_ms > 0 {
            TEST_STRIPE_RMDIR_GUARDS_HELD.store(true, Ordering::SeqCst);
            squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(hold_ms)).await;
            TEST_STRIPE_RMDIR_GUARDS_HELD.store(false, Ordering::SeqCst);
        }

        // ---- ONE intent.
        let none = crossvol_tx::parent_update_code(RoutedParentUpdate::None);
        let target = map.stripes[usize::from(MARKER_TARGET_INDEX)];
        let mut steps: Vec<XvStep> = vec![XvStep::RemoveDentry {
            parent: key_parent,
            name: name.to_string(),
            expect_child: dir,
            parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::ExclusiveTimesBump),
        }];
        steps.push(XvStep::RemoveDentry {
            parent: dir,
            name: STRIPED_MARKER.to_string(),
            expect_child: target,
            parent_update: none,
        });
        if live.migrating {
            steps.push(XvStep::RemoveDentry {
                parent: dir,
                name: MIGRATING_MARKER.to_string(),
                expect_child: target,
                parent_update: none,
            });
        }
        for (i, s) in map.stripes.iter().enumerate() {
            steps.push(XvStep::RemoveDentry {
                parent: dir,
                name: stripe_marker_name(i as u16),
                expect_child: *s,
                parent_update: none,
            });
        }
        for (s, n) in map.stripes.iter().zip(&stripe_counts) {
            if *n == 2 {
                steps.push(XvStep::SetNlink {
                    ino: *s,
                    pre: 2,
                    post: 0,
                    ctime: None,
                });
            }
        }
        steps.push(XvStep::SetNlink {
            ino: dir,
            pre: dir_rec.nlink,
            post: 0,
            ctime: Some(KvMetaBackend::now_ns_pub()),
        });
        // The cached map goes BEFORE the write: a plan severed mid-way
        // must not leave the whole map cached over half its markers.
        self.forget_map(dir);
        crossvol_tx::execute(
            self,
            &XvPlan {
                op: XvOp::Rmdir,
                steps,
            },
            Arc::clone(&guards),
        )
        .await?;
        self.dir_stripes.census().remove(&dir);
        drop(guards);
        // Hygiene: the marked stripes' records — and the known-stripe set
        // forgets them whatever their holder (a foreign destroy never
        // reaches the local prune).
        for stripe in &map.stripes {
            self.dir_stripes.known_stripes.remove_sync(stripe);
            if let Err(e) = self.destroy_stripe(*stripe).await {
                log::warn!(
                    "rmdir {dir}: stripe {stripe} not destroyed ({e}) — an nlink-0 corpse named \
                     by no map; the next mount's sweep reclaims it"
                );
            }
        }
        DIR_STRIPE_RMDIRS.fetch_add(1, Ordering::Relaxed);
        // Saturating: a directory another mount flipped is not in this
        // mount's count.
        let _ = DIR_STRIPED_DIRS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_sub(1))
        });
        Ok(dir)
    }

    /// The `mkdir`-time flip under `-o stripe_dirs`: stripe the new
    /// directory when the option is on and its volume is armed. The mkdir
    /// JUST NAMED `dir` under `parent`, so it is a directory and never a
    /// stripe — no reverse scan runs here (Issue 24: the DNE-2 default paid
    /// one per mkdir). Failure is logged, never the mkdir's error (the
    /// directory exists; the automatic trigger can still flip it later).
    pub async fn stripe_at_mkdir(&self, dir: Ino, parent: Ino) {
        if !self.stripe_dirs_at_mkdir() {
            return;
        }
        let (v, _) = self.route_ino(dir);
        if !self.stripes_armed(v) || stripe_count() <= 1 {
            return;
        }
        if let Err(e) = self
            .flip_dir(dir, stripe_count(), &[], StripeBelt::KnownDirectory)
            .await
        {
            log::warn!(
                "-o stripe_dirs: directory {dir} (under {parent}) not striped at mkdir ({e})"
            );
        }
    }

    /// The `getxattr user.squeezefs.stripes` answer: `K` on a striped
    /// directory.
    pub async fn stripes_xattr_value(&self, dir: Ino) -> Result<Option<Vec<u8>>> {
        Ok(self
            .stripe_map(dir)
            .await?
            .map(|m| m.k().to_string().into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_codec_is_total_and_round_trips() {
        for i in 0..STRIPES_MAX {
            let n = stripe_marker_name(i);
            assert!(is_marker_name(&n));
            assert_eq!(parse_marker(n.as_bytes()), Some(Marker::Stripe(i)));
        }
        assert_eq!(
            parse_marker(STRIPED_MARKER.as_bytes()),
            Some(Marker::Striped)
        );
        assert_eq!(
            parse_marker(MIGRATING_MARKER.as_bytes()),
            Some(Marker::Migrating)
        );
        assert_eq!(parse_marker(b"\0sqz.stripe:64"), None);
        assert_eq!(parse_marker(b"\0sqz.stripe:"), None);
        assert_eq!(parse_marker(b"\0sqz.stripe:1234"), None);
        // Non-canonical widths decode to NOTHING (review round 1, Issue 7:
        // the fuzz law `stripe_marker_name(i) == name` for every decoded
        // name was false for these).
        assert_eq!(parse_marker(b"\0sqz.stripe:5"), None);
        assert_eq!(parse_marker(b"\0sqz.stripe:005"), None);
        for i in 0..STRIPES_MAX {
            let n = stripe_marker_name(i);
            assert_eq!(parse_marker(n.as_bytes()).map(|_| n.clone()), Some(n));
        }
        assert_eq!(parse_marker(b"plain"), None);
        assert_eq!(parse_marker(&[0xff, 0, 1]), None);
        assert!(!is_marker_name("file"));
        assert!(!is_marker_name(""));
        assert!(STRIPE_MARKER.starts_with('\0'));
    }

    #[test]
    fn stripe_function_is_hash_mod_k() {
        assert_eq!(stripe_of(130, 64), 2);
        assert_eq!(stripe_of(7, 1), 0);
        assert_eq!(stripe_of(u64::MAX, 3), (u64::MAX % 3) as u16);
    }

    #[test]
    fn the_trigger_needs_two_creators_over_the_floor() {
        assert!(!flip_due(&[(1, 100)], 2), "one creator never flips");
        assert!(!flip_due(&[(1, 1), (2, 1)], 2), "at the floor, not over it");
        assert!(flip_due(&[(1, 2), (2, 1)], 2));
        assert!(!flip_due(&[], 2));
    }
}
