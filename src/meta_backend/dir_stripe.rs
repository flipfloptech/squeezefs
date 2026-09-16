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
//! `\0sqz.striped → D` (the COMMIT marker: the map is live iff it exists)
//! and `\0sqz.migrating → D` are records no user op can name, filtered
//! from `readdir` at the ONE routed pager. Why dentries and not a new
//! record codec: (1) the map then RIDES `D`'s token verbatim — PR 5's
//! grant pages a directory's dentry set — with no token-API change; (2)
//! the flip is `K + 2` `InsertDentry` steps of ONE PR 6 intent, so the
//! whole §5.6 protocol (intent, roll-forward, the kill seams) is reused
//! with no new step kind and no new applier; (3) C9's dentry pass marks
//! every marker's TARGET, so a stripe ino named by its map is REFERENCED
//! and an orphan stripe is C9's class exactly as §5.6.5 states. **No
//! slot-tree KEY kind is added**: bit 17's kind set stays PR 1 + PR 7's;
//! what this rung adds to the bit's meaning is the reserved NUL name
//! space inside kind `0x02` (design §7.1).
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
//! **`rmdir D`**: under `D`'s exclusive 4a guard and the S10 mutation
//! permit (the "exclusive token": every reader's token on `D` is recalled
//! and no new grant issues while it is held), the holder MARKS every
//! stripe dying (`SetNlink 2 → 0`, one intent), then probes each stripe
//! `IsEmpty` UNDER the mark — an insert into a dying stripe is refused
//! `ENOENT` at its holder, so a create that raced the first probe is
//! either seen by the second or refused — then removes the markers,
//! unlinks `D` from its parent and destroys the stripes (`DestroyStripe`;
//! a stripe whose destroy the process did not reach is an `nlink == 0`
//! corpse the next mount's sweep reclaims). A non-empty stripe under the
//! mark REVIVES the set and answers `ENOTEMPTY`.

use super::crossvol_tx::{self, XvOp, XvPlan, XvStep, XvStepStatus};
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
/// `\0sqz.striped → D` — the COMMIT marker; the map is live iff present.
pub const STRIPED_MARKER: &str = "\u{0}sqz.striped";
/// `\0sqz.migrating → D` — names may still live in `D`'s own tree.
pub const MIGRATING_MARKER: &str = "\u{0}sqz.migrating";

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

/// The marker name of stripe `i` (two decimal digits — `STRIPES_MAX` is
/// 64; the decoder accepts up to three so a wider future `K` decodes).
pub fn stripe_marker_name(i: u16) -> String {
    format!("{STRIPE_MARKER}{i:02}")
}

/// Is `name` in the reserved marker space (any NUL-led name — every
/// marker starts with NUL and nothing user-reachable does)?
#[inline]
pub fn is_marker_name(name: &str) -> bool {
    name.as_bytes().first() == Some(&0)
}

/// Decode a marker name; `None` for every name that is not one of the
/// three shapes (a NUL-led name outside the codec is filtered from
/// `readdir` like a marker but names nothing). Total: never panics.
pub fn parse_marker(name: &[u8]) -> Option<Marker> {
    let name = std::str::from_utf8(name).ok()?;
    if name == STRIPED_MARKER {
        return Some(Marker::Striped);
    }
    if name == MIGRATING_MARKER {
        return Some(Marker::Migrating);
    }
    let digits = name.strip_prefix(STRIPE_MARKER)?;
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
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

/// Test seam: hold the BACKGROUND migration a flip would kick, so a
/// contract observes the `migrating` state (names in both homes, the
/// fallback lookup, the LWW rule) and drives [`RoutedMetaBackend::
/// migrate_dir`] itself. Never set in production.
pub static TEST_STRIPE_HOLD_MIGRATION: AtomicBool = AtomicBool::new(false);

/// The family as the stats inode publishes it.
pub fn stats_json() -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    let put = |m: &mut serde_json::Map<String, serde_json::Value>, k: &str, v: &AtomicU64| {
        m.insert(k.to_string(), serde_json::json!(v.load(Ordering::Relaxed)));
    };
    put(&mut m, "dir_striped_dirs", &DIR_STRIPED_DIRS);
    put(&mut m, "dir_stripe_flips", &DIR_STRIPE_FLIPS);
    put(&mut m, "dir_stripe_supply_rpcs", &DIR_STRIPE_SUPPLY_RPCS);
    put(&mut m, "dir_stripe_ships", &DIR_STRIPE_SHIPS);
    put(
        &mut m,
        "dir_stripe_readdir_merges",
        &DIR_STRIPE_READDIR_MERGES,
    );
    put(
        &mut m,
        "dir_stripe_migrated_names",
        &DIR_STRIPE_MIGRATED_NAMES,
    );
    put(&mut m, "dir_stripe_time_batches", &DIR_STRIPE_TIME_BATCHES);
    put(
        &mut m,
        "dir_stripe_dying_refusals",
        &DIR_STRIPE_DYING_REFUSALS,
    );
    put(&mut m, "dir_stripe_rmdirs", &DIR_STRIPE_RMDIRS);
    m
}

/// Every gauge of the family, for the contracts' before/after reads.
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
    }
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
/// directory and CREATOR appender id).
#[derive(Debug, Default)]
struct DirCensus {
    epoch: u64,
    cur: BTreeMap<u32, u64>,
    prev: BTreeMap<u32, u64>,
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
            self.epoch = epoch;
        }
    }

    /// Creators by ops over the window, heaviest first.
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

/// Directories the census tracks at most — one entry per hot shared
/// directory; past it the least recently touched is evicted (the
/// `REQUESTERS_PER_SLOT_MAX` posture one level up).
const CENSUS_DIRS_MAX: usize = 1024;

/// The mount's striping state, one per [`RoutedMetaBackend`].
pub struct StripeState {
    /// `D → its map`, for directories whose migration finished (a map
    /// still migrating is re-read per op — the migration is short-lived
    /// and its flag is the truth). Invalidated by the local rmdir.
    maps: scc::HashMap<Ino, Arc<StripeMap>>,
    /// The flip trigger's census: foreign creators per directory.
    census: Mutex<BTreeMap<Ino, DirCensus>>,
    /// Directories whose flip or migration this mount is running.
    in_flight: scc::HashSet<Ino>,
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
            census: Mutex::new(BTreeMap::new()),
            in_flight: scc::HashSet::new(),
            stripe_at_mkdir: AtomicBool::new(false),
            me: std::sync::OnceLock::new(),
        }
    }

    fn census(&self) -> std::sync::MutexGuard<'_, BTreeMap<Ino, DirCensus>> {
        self.census.lock().unwrap_or_else(|e| e.into_inner())
    }
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
            .is_some_and(|v| v.slot_lease_armed())
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

    /// Is GLOBAL `ino`'s slot served by THIS process (its own region or a
    /// declared one — the door's "leased here")?
    fn served_here(&self, ino: Ino) -> bool {
        let (v, local) = self.route_ino(ino);
        match self.volumes.get(v).and_then(|vol| vol.slot_leases()) {
            Some(plane) => plane.gate.is_leased(forest_slot_of_ino(local)),
            None => true,
        }
    }

    /// **The stripe map of `dir`**, `None` when `dir` is not striped (or
    /// the plane is unarmed on its volume). A finished map is cached; a
    /// migrating one is re-read (its flag is the truth of the moment) and
    /// its migration kicked single-flight when this mount holds `dir`.
    pub async fn stripe_map(&self, dir: Ino) -> Result<Option<Arc<StripeMap>>> {
        let (v, local) = self.route_ino(dir);
        if !self.stripes_armed(v) {
            return Ok(None);
        }
        if let Some(m) = self.dir_stripes.maps.read_sync(&dir, |_, m| Arc::clone(m)) {
            return Ok(Some(m));
        }
        let Some((commit, _)) = self.find_dentry_routed(v, local, STRIPED_MARKER).await? else {
            return Ok(None);
        };
        if commit != dir {
            // A commit marker naming another ino is a planted or torn
            // record — the map is not live, never guessed from.
            log::warn!(
                "directory {dir}: the stripe commit marker names ino {commit}, not the \
                 directory — the map is ignored (fsck C17 reports it)"
            );
            return Ok(None);
        }
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
        if stripes.len() < 2 {
            log::warn!(
                "directory {dir}: the stripe commit marker is present with {} stripe \
                 marker(s) — the map is ignored (fsck C17 reports it)",
                stripes.len()
            );
            return Ok(None);
        }
        let migrating = self
            .find_dentry_routed(v, local, MIGRATING_MARKER)
            .await?
            .is_some();
        let map = Arc::new(StripeMap {
            dir,
            stripes,
            migrating,
        });
        if migrating {
            if self.served_here(dir) {
                self.kick_migration(dir);
            }
        } else {
            let _ = self.dir_stripes.maps.insert_sync(dir, Arc::clone(&map));
        }
        Ok(Some(map))
    }

    /// Forget a cached map (the local rmdir / the migration's clear).
    fn forget_map(&self, dir: Ino) {
        self.dir_stripes.maps.remove_sync(&dir);
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
    /// when the stripe holds it, `D`'s own tree while `migrating` and the
    /// stripe misses (the LWW rule: the stripe wins). `None` when the
    /// name is nowhere.
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
        if route.map.migrating {
            let (pv, plocal) = self.route_ino(parent);
            if let Some((child, ft)) = self.find_dentry_routed(pv, plocal, name).await? {
                return Ok(Some((parent, child, ft)));
            }
        }
        Ok(None)
    }

    /// The KEY parent a NEW name in a striped directory is inserted under
    /// (always the stripe), after the EEXIST screen over BOTH homes while
    /// migrating.
    pub async fn stripe_insert_parent(
        &self,
        parent: Ino,
        name: &str,
        route: &StripeRoute,
    ) -> Result<Ino> {
        if route.map.migrating {
            let (pv, plocal) = self.route_ino(parent);
            if self.find_dentry_routed(pv, plocal, name).await?.is_some() {
                return Err(SqueezefsError::already_exists("File already exists"));
            }
        }
        Ok(route.stripe)
    }

    /// The `unlink`/`rmdir` prelude: the KEY parent the name is removed
    /// from (its stripe when `parent` is striped — where the name IS, the
    /// directory's own tree while migrating), and the map of the CHILD
    /// when it is a striped directory (the rmdir protocol's subject).
    /// Unguarded pre-reads: the removal revalidates under its guards.
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
            Some(route) => match self.stripe_locate(parent, name, &route).await? {
                Some((home, _, _)) => home,
                None => route.stripe,
            },
            None => parent,
        };
        let (kv_idx, klocal) = self.route_ino(key_parent);
        let child = match self.find_dentry_routed(kv_idx, klocal, name).await? {
            Some((child, ft)) if ft == libc::S_IFDIR => child,
            _ => return Ok((key_parent, None)),
        };
        let map = self.stripe_map(child).await?;
        if let Some(m) = &map {
            // The directory's OWN tree must hold nothing but markers (a
            // name not yet migrated is an entry): the stripes are probed
            // under the mark by the protocol proper.
            if !self.serve_is_empty_covered(child, true).await? {
                return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOTEMPTY,
                )));
            }
            log::debug!("rmdir of striped directory {} ({} stripes)", m.dir, m.k());
        }
        Ok((key_parent, map))
    }

    /// A stripe whose record reads `nlink == 0` is DYING (an `rmdir` in
    /// flight): an insert into it is refused `ENOENT` — the R26 closer.
    /// Read under the caller's held `I{stripe}` guard.
    pub async fn refuse_dying_stripe(&self, stripe: Ino) -> Result<()> {
        let (v, local) = self.route_ino(stripe);
        match self.volumes[v].read_inode_value_routed(local).await? {
            Some(rec) if rec.nlink == 0 => {
                DIR_STRIPE_DYING_REFUSALS.fetch_add(1, Ordering::Relaxed);
                Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("directory stripe {stripe} is being removed (rmdir in flight)"),
                )))
            }
            _ => Ok(()),
        }
    }

    /// The directory a STRIPE belongs to, for the `..` reconnect scan:
    /// the ONE dentry naming a stripe is its map entry in `D`, so the
    /// reverse scan of the stripe answers `D`. `None` = not a stripe.
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
        self.flip_dir(dir, k, &creators).await
    }

    /// [`Self::stripe_dir`] with the suppliers NAMED (the fleet-shape
    /// contracts: the holder asks these appenders, in order, before
    /// minting the remainder itself).
    pub async fn stripe_dir_with_suppliers(
        &self,
        dir: Ino,
        k: u16,
        suppliers: &[u32],
    ) -> Result<()> {
        self.flip_dir(dir, k, suppliers).await
    }

    /// The flip proper (§5.6.5): `K` `SupplyStripeIno`s (the creators
    /// first, the holder's own slots for the remainder), then ONE PR 6
    /// intent of `K + 2` marker inserts — every stripe entry, the
    /// `migrating` flag, the COMMIT marker LAST — so a reader sees
    /// either no map or the whole map, and a kill at any step rolls
    /// forward to the whole map; then the lazy migration, in the
    /// background.
    async fn flip_dir(&self, dir: Ino, k: u16, creators: &[u32]) -> Result<()> {
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
        let mut stripes = Vec::with_capacity(usize::from(k));
        let mut next_creator = creators.iter().copied().filter(|c| Some(*c) != holder);
        for i in 0..k {
            let mut supplied = None;
            // A creator that declines or is gone is replaced by the next.
            for supplier in next_creator.by_ref() {
                match self.supply_stripe_ino(dir, i, supplier, rec).await {
                    Ok(ino) => {
                        supplied = Some(ino);
                        break;
                    }
                    Err(e) => log::info!(
                        "stripe {i} of directory {dir}: appender {supplier} declined to \
                         supply a stripe ino ({e}) — asking the next creator"
                    ),
                }
            }
            let ino = match supplied {
                Some(ino) => ino,
                None => self.mint_stripe_locally(dir, v, holder, rec).await?,
            };
            stripes.push(ino);
        }
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
            child: dir,
            ft_bits: libc::S_IFDIR,
            parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::None),
        });
        steps.push(XvStep::InsertDentry {
            parent: dir,
            name: STRIPED_MARKER.to_string(),
            child: dir,
            ft_bits: libc::S_IFDIR,
            parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::ExclusiveTimes),
        });
        // The op's guard set covers EVERY key its steps touch — `I{dir}`
        // and each marker's `D{}` — so a foreign holder serving them under
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
        let mut scope = None;
        let guards: Arc<[dlm::DlmGuard]> = Arc::from(
            self.lock_many_leased(v, &mut scope, &[(local, dlm::LockMode::Exclusive)], &dents)
                .await?,
        );
        // Re-check under the guard: a concurrent explicit flip lost the
        // race and must not write a second map.
        if self
            .find_dentry_routed(v, local, STRIPED_MARKER)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let plan = XvPlan {
            op: XvOp::StripeDir,
            steps,
        };
        crossvol_tx::execute(self, &plan, guards).await?;
        DIR_STRIPE_FLIPS.fetch_add(1, Ordering::Relaxed);
        DIR_STRIPED_DIRS.fetch_add(1, Ordering::Relaxed);
        self.dir_stripes.census().remove(&dir);
        log::info!(
            "directory {dir} STRIPED into {k} stripes ({} supplied by creators {:?}, {} minted \
             by the holder) — names re-home lazily",
            stripes.len().min(creators.len()),
            creators,
            stripes.len().saturating_sub(creators.len())
        );
        Ok(())
    }

    /// Mint one stripe ino in a slot `holder` leases on `dir`'s volume
    /// (the flip's remainder): an `S_IFDIR` record with `dir`'s
    /// permission bits and owner, `nlink 2`, no name.
    async fn mint_stripe_locally(
        &self,
        dir: Ino,
        v: usize,
        holder: Option<u32>,
        rec: &Inode,
    ) -> Result<Ino> {
        let slot = match holder {
            Some(h) => self
                .slot_of_appender_on(v, h)
                .unwrap_or_else(|| self.pick_mint_slot(v)),
            None => self.pick_mint_slot(v),
        };
        self.mint_stripe_in_slot(dir, v, slot, rec).await
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

    async fn mint_stripe_in_slot(&self, dir: Ino, v: usize, slot: u64, rec: &Inode) -> Result<Ino> {
        let (local, global) = self.allocate_local_ino_in_slot(v, slot)?;
        let vol = &self.volumes[v];
        let guard: Arc<[dlm::DlmGuard]> =
            Arc::from(vec![vol.dlm().lock_inode_exclusive(local).await]);
        let out = vol
            .routed_mint_inode(
                local,
                libc::S_IFDIR | (rec.mode & 0o7777),
                rec.uid,
                rec.gid,
                0,
                0,
                None,
                guard,
            )
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v);
        }
        out?;
        // Every stripe mint is one supply — asked of a creator or the
        // holder's own (the served side of a wire ask counts here too).
        DIR_STRIPE_SUPPLY_RPCS.fetch_add(1, Ordering::Relaxed);
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
    /// insert into `dir` from `creator`. Counted over the `T_idle`
    /// half-window pair per directory; when the window's foreign ships
    /// exceed `N_floor` from ≥ 2 creators the flip is spawned (single-
    /// flight, off the serving path). ONE creator never flips — that is
    /// a handover candidate, not a striping one.
    pub fn note_served_insert(&self, dir: Ino, creator: u32) {
        let (v, _) = self.route_ino(dir);
        let Some(plane) = self.volumes.get(v).and_then(|vol| vol.slot_leases()) else {
            return;
        };
        if stripe_count() <= 1 {
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
            *c.cur.entry(creator).or_default() += 1;
            let heaviest = c.heaviest();
            flip_due(&heaviest, n_floor).then(|| FlipDecision {
                dir,
                creators: heaviest.into_iter().map(|(c, _)| c).collect(),
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
                if let Err(e) = me.flip_dir(d.dir, stripe_count(), &d.creators).await {
                    log::warn!("automatic stripe flip of directory {} declined: {e}", d.dir);
                }
            });
        }
    }

    /// Flips and migrations this mount is running right now (the
    /// contracts drain it before a shutdown).
    pub fn stripe_work_in_flight(&self) -> usize {
        self.dir_stripes.in_flight.len()
    }

    /// The census as the contracts read it: creators of `dir`, heaviest
    /// first.
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

    /// **The migration, synchronously** (the contracts' and the resume's
    /// face): every non-marker name in `dir`'s own tree moves to its
    /// stripe — insert into the stripe FIRST, remove from `dir` second,
    /// each pair one intent, so a kill between the two leaves the name in
    /// both places and the stripe wins — then the `migrating` marker is
    /// removed. Idempotent: names already in their stripe are dropped from
    /// `dir` alone. Returns the names moved by this call.
    pub async fn migrate_dir(&self, dir: Ino) -> Result<u64> {
        if self.dir_stripes.in_flight.insert_sync(dir).is_err() {
            return Ok(0);
        }
        let out = self.migrate_dir_inner(dir).await;
        self.dir_stripes.in_flight.remove_sync(&dir);
        out
    }

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
        let mut cursor = 0u64;
        loop {
            let page = self.volumes[v].readdir_page(local, cursor, 256).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = *last;
            for (_, entry) in page {
                if is_marker_name(&entry.name) {
                    continue;
                }
                self.migrate_one(dir, v, local, &map, &entry).await?;
                moved += 1;
                DIR_STRIPE_MIGRATED_NAMES.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Nothing but markers left: clear the flag (its own intent, so a
        // kill here re-scans an empty tree and clears it again).
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
                    expect_child: dir,
                    parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::None),
                }],
            };
            crossvol_tx::execute(self, &plan, guards).await?;
        }
        self.forget_map(dir);
        log::info!("directory {dir}: stripe migration complete — {moved} name(s) re-homed");
        Ok(moved)
    }

    /// Move one name into its stripe: `[InsertDentry @ stripe,
    /// RemoveDentry @ dir]` under the op's guards on both homes.
    async fn migrate_one(
        &self,
        dir: Ino,
        v: usize,
        local: Ino,
        map: &StripeMap,
        entry: &DirEntry,
    ) -> Result<()> {
        let (index, stripe) = map.stripe_for(self.hash54_on(v, &entry.name));
        let (sv, slocal) = self.route_ino(stripe);
        let is_dir = entry.file_type & libc::S_IFMT == libc::S_IFDIR;
        let update = if is_dir {
            RoutedParentUpdate::ExclusiveTimesBump
        } else {
            RoutedParentUpdate::None
        };
        let mut scope = None;
        let mut inos = vec![(local, dlm::LockMode::Exclusive)];
        let mut dents = vec![(local, entry.name.as_str(), dlm::LockMode::Exclusive)];
        if sv == v {
            inos.push((slocal, dlm::LockMode::Exclusive));
            dents.push((slocal, entry.name.as_str(), dlm::LockMode::Exclusive));
        }
        let mut guards = self.lock_many_leased(v, &mut scope, &inos, &dents).await?;
        if sv != v {
            guards.extend(
                self.lock_many_leased(
                    sv,
                    &mut scope,
                    &[(slocal, dlm::LockMode::Exclusive)],
                    &[(slocal, entry.name.as_str(), dlm::LockMode::Exclusive)],
                )
                .await?,
            );
        }
        let guards: Arc<[dlm::DlmGuard]> = Arc::from(guards);
        // Revalidate under the guards: the name may have been unlinked or
        // migrated by a concurrent op.
        let Some((cur_child, _)) = self.find_dentry_routed(v, local, &entry.name).await? else {
            return Ok(());
        };
        if cur_child != entry.ino {
            return Ok(());
        }
        let in_stripe = self
            .find_dentry_routed(sv, slocal, &entry.name)
            .await?
            .is_some();
        let mut steps = Vec::with_capacity(2);
        if !in_stripe {
            steps.push(XvStep::InsertDentry {
                parent: stripe,
                name: entry.name.clone(),
                child: entry.ino,
                ft_bits: entry.file_type & libc::S_IFMT,
                parent_update: crossvol_tx::parent_update_code(update),
            });
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
        Ok(())
    }

    /// The map straight from the markers (no cache) — the migration's
    /// and the rmdir's read.
    async fn read_map_uncached(&self, dir: Ino) -> Result<Option<Arc<StripeMap>>> {
        self.forget_map(dir);
        // `stripe_map` re-reads and re-caches a finished map; a migrating
        // one is never cached, and the kick it would spawn is a no-op
        // while this caller holds the in-flight slot.
        self.stripe_map(dir).await
    }

    // -----------------------------------------------------------------
    // readdir + the attribute fold.
    // -----------------------------------------------------------------

    /// **The K-way `readdir` merge** (§5.6.5): one page after `offset`
    /// from every stripe (and from `dir`'s own tree while migrating),
    /// merged in `(hash54, coll)` order — the shipped cookie order, since
    /// every stripe lives on `dir`'s volume and shares its seed — with a
    /// name present in both a stripe and `dir` served from the stripe.
    /// The cookie of the last entry is the exact continuation: every
    /// stream resumes strictly after it, so a name present for the whole
    /// scan is returned exactly once under concurrent creates/unlinks.
    pub async fn readdir_striped_page(
        &self,
        map: &StripeMap,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, DirEntry)>> {
        DIR_STRIPE_READDIR_MERGES.fetch_add(1, Ordering::Relaxed);
        let mut merged: BTreeMap<u64, DirEntry> = BTreeMap::new();
        let mut from_stripes: std::collections::HashSet<String> = std::collections::HashSet::new();
        for stripe in &map.stripes {
            for (cookie, entry) in self.page_names(*stripe, offset, max).await? {
                from_stripes.insert(entry.name.clone());
                merged.insert(cookie, entry);
            }
        }
        if map.migrating {
            for (cookie, entry) in self.page_names(map.dir, offset, max).await? {
                if from_stripes.contains(&entry.name) {
                    continue;
                }
                merged.entry(cookie).or_insert(entry);
            }
        }
        Ok(merged.into_iter().take(max).collect())
    }

    /// One stream's page: `max` NON-marker entries strictly after
    /// `offset` (a page whose marker entries counted against `max` would
    /// end short of the names below the merged page's last cookie, and
    /// the continuation would skip them).
    async fn page_names(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<(u64, DirEntry)>> {
        let (v, local) = self.route_ino(dir);
        self.check_volume_enabled(v)?;
        let mut out = Vec::with_capacity(max);
        let mut cursor = offset;
        while out.len() < max {
            let want = max - out.len();
            let page = self.volumes[v].readdir_page(local, cursor, want).await?;
            let short = page.len() < want;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = *last;
            out.extend(page.into_iter().filter(|(_, e)| !is_marker_name(&e.name)));
            if short {
                break;
            }
        }
        Ok(out)
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
    /// persist costs nothing but the next one). Only the holder writes.
    pub async fn persist_striped_times(&self, dir: Ino, mtime: u64, ctime: u64) {
        if !self.served_here(dir) {
            return;
        }
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

    /// Does stripe `stripe` hold any entry? The `rmdir`'s own guard set
    /// (`scope`) holds `I{stripe}` exclusive — in this table or parked at
    /// the stripe's holder — so the read needs no lock of its own where
    /// the scope covers it; a stripe outside the scope is read under an
    /// exclusive guard at its holder. Either way every insert that took
    /// the stripe's guard has landed (then it is seen) or arrives after
    /// the mark (then it is refused, dying).
    async fn stripe_is_empty(&self, stripe: Ino, scope: u64) -> Result<bool> {
        let (v, _) = self.route_ino(stripe);
        // PR 6's foreign predicate: the lease TABLE, not the gate — a
        // declared region's slot is another appender's for every
        // cross-owner decision (the two-holder model in one process).
        let holder = match self.holder_of(stripe) {
            Some(h) if h != self.volumes[v].own_appender_id() => h,
            _ => return self.serve_is_empty_covered(stripe, scope != 0).await,
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

    /// No non-marker entry under `dir`; `held` = the caller's guard set
    /// already holds `I{dir}` (never re-taken — it would wait behind
    /// itself).
    async fn serve_is_empty_covered(&self, dir: Ino, held: bool) -> Result<bool> {
        let (v, local) = self.route_ino(dir);
        self.check_volume_enabled(v)?;
        let _g = if held {
            None
        } else {
            Some(self.volumes[v].dlm().lock_inode_exclusive(local).await)
        };
        let page = self.volumes[v].readdir_page(local, 0, 8).await?;
        Ok(page.iter().all(|(_, e)| is_marker_name(&e.name)))
    }

    /// [`Self::serve_is_empty_covered`] taking its own guard — the
    /// destroy's precondition read.
    async fn serve_is_empty(&self, dir: Ino) -> Result<bool> {
        self.serve_is_empty_covered(dir, false).await
    }

    /// Destroy stripe `stripe` (its record + xattrs — `nlink` is 0 by the
    /// mark): here when served here, `DestroyStripe` over the wire
    /// otherwise. Best-effort: a stripe left undestroyed is an `nlink 0`
    /// corpse the next mount's sweep reclaims.
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
        if !self.serve_is_empty(stripe).await? {
            return Err(SqueezefsError::refused(
                libc::ENOTEMPTY,
                format!("DestroyStripe: stripe {stripe} still holds entries"),
            ));
        }
        vol.destroy_inodes(&[local]).await
    }

    /// **`rmdir` of a striped directory** (§5.6.5, R26). Under the
    /// caller's exclusive `I{dir}` guard and S10 permit: (1) every stripe
    /// must read `nlink 2` (a subdirectory makes it `> 2` — `ENOTEMPTY`
    /// without a mark); (2) MARK every stripe dying (`SetNlink 2 → 0`,
    /// one intent — a mkdir that raced fails the CAS and the plan reads
    /// it); (3) probe `IsEmpty` under the mark; a non-empty stripe
    /// REVIVES the set (`SetNlink 0 → 2`) and answers `ENOTEMPTY`. The
    /// caller then unlinks `dir` from its parent and calls
    /// [`Self::finish_striped_rmdir`] (the markers, the stripes).
    pub async fn prepare_striped_rmdir(&self, dir: Ino, map: &StripeMap) -> Result<()> {
        let (v, _) = self.route_ino(dir);
        // The stripes' exclusive guards for the whole mark-probe-revive
        // sequence: in this table for the stripes served here, parked at
        // their holders for the rest (PR 6's travelling guard). Every
        // stripe lives on `dir`'s volume by construction.
        let mut scope = None;
        let stripe_keys: Vec<(Ino, dlm::LockMode)> = map
            .stripes
            .iter()
            .map(|s| (self.route_ino(*s).1, dlm::LockMode::Exclusive))
            .collect();
        let guards: Arc<[dlm::DlmGuard]> = Arc::from(
            self.lock_many_leased(v, &mut scope, &stripe_keys, &[])
                .await?,
        );
        let scope_word = crossvol_tx::scope_of(&guards);
        // (1) live counts.
        let mut pre = Vec::with_capacity(map.stripes.len());
        for stripe in &map.stripes {
            let (sv, slocal) = self.route_ino(*stripe);
            match self.volumes[sv].read_inode_value_routed(slocal).await? {
                Some(r) if r.nlink == 2 || r.nlink == 0 => pre.push(r.nlink),
                Some(r) => {
                    log::debug!(
                        "rmdir {dir}: stripe {stripe} nlink {} — holds subdirectories",
                        r.nlink
                    );
                    return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                        libc::ENOTEMPTY,
                    )));
                }
                None => pre.push(0),
            }
        }
        // (2) the mark.
        let mark: Vec<XvStep> = map
            .stripes
            .iter()
            .zip(&pre)
            .filter(|(_, n)| **n == 2)
            .map(|(s, _)| XvStep::SetNlink {
                ino: *s,
                pre: 2,
                post: 0,
                ctime: None,
            })
            .collect();
        if !mark.is_empty() {
            let done = crossvol_tx::execute(
                self,
                &XvPlan {
                    op: XvOp::Rmdir,
                    steps: mark,
                },
                Arc::clone(&guards),
            )
            .await?;
            if done
                .outcomes
                .iter()
                .any(|o| o.status == XvStepStatus::ForeignSkipped)
            {
                // A mkdir raced the mark on some stripe: revive the marked
                // ones and refuse.
                self.revive_stripes(map, Arc::clone(&guards)).await?;
                return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOTEMPTY,
                )));
            }
        }
        // (3) probe under the mark.
        for stripe in &map.stripes {
            if !self.stripe_is_empty(*stripe, scope_word).await? {
                self.revive_stripes(map, Arc::clone(&guards)).await?;
                return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOTEMPTY,
                )));
            }
        }
        drop(guards);
        Ok(())
    }

    /// Remove `dir`'s stripe markers (one intent, rolled forward if
    /// killed) — the LAST act of a striped rmdir, after the directory's
    /// own name is gone: while they stand, a create into the removed
    /// directory still routes to a stripe and is refused there.
    async fn remove_stripe_markers(&self, dir: Ino, map: &StripeMap) -> Result<()> {
        let (v, local) = self.route_ino(dir);
        let mut steps: Vec<XvStep> = map
            .stripes
            .iter()
            .enumerate()
            .map(|(i, s)| XvStep::RemoveDentry {
                parent: dir,
                name: stripe_marker_name(i as u16),
                expect_child: *s,
                parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::None),
            })
            .collect();
        for marker in [MIGRATING_MARKER, STRIPED_MARKER] {
            steps.push(XvStep::RemoveDentry {
                parent: dir,
                name: marker.to_string(),
                expect_child: dir,
                parent_update: crossvol_tx::parent_update_code(RoutedParentUpdate::None),
            });
        }
        let dents: Vec<(Ino, String)> = (0..map.k())
            .map(stripe_marker_name)
            .chain([MIGRATING_MARKER.to_string(), STRIPED_MARKER.to_string()])
            .map(|n| (local, n))
            .collect();
        let dents_ref: Vec<(Ino, &str, dlm::LockMode)> = dents
            .iter()
            .map(|(p, n)| (*p, n.as_str(), dlm::LockMode::Exclusive))
            .collect();
        let mut scope = None;
        let guards: Arc<[dlm::DlmGuard]> = Arc::from(
            self.lock_many_leased(
                v,
                &mut scope,
                &[(local, dlm::LockMode::Exclusive)],
                &dents_ref,
            )
            .await?,
        );
        crossvol_tx::execute(
            self,
            &XvPlan {
                op: XvOp::Rmdir,
                steps,
            },
            guards,
        )
        .await?;
        self.forget_map(dir);
        Ok(())
    }

    /// A striped rmdir whose own removal FAILED after the mark: revive the
    /// stripes so the directory stays usable (best-effort — a mark left
    /// standing is resolved by the next `rmdir` attempt).
    pub async fn abort_striped_rmdir(&self, map: &StripeMap) {
        let (v, _) = self.route_ino(map.dir);
        let mut scope = None;
        let stripe_keys: Vec<(Ino, dlm::LockMode)> = map
            .stripes
            .iter()
            .map(|s| (self.route_ino(*s).1, dlm::LockMode::Exclusive))
            .collect();
        let Ok(guards) = self
            .lock_many_leased(v, &mut scope, &stripe_keys, &[])
            .await
        else {
            return;
        };
        if let Err(e) = self.revive_stripes(map, Arc::from(guards)).await {
            log::warn!("rmdir {}: the stripes stay marked dying ({e})", map.dir);
        }
    }

    /// After `dir`'s own removal: the markers go, then the (marked, empty)
    /// stripes are destroyed.
    pub async fn finish_striped_rmdir(&self, dir: Ino, map: &StripeMap) {
        if let Err(e) = self.remove_stripe_markers(dir, map).await {
            log::warn!(
                "rmdir {dir}: the stripe markers stay ({e}) — the removed directory's own \
                 tree keeps them until the next attempt; its stripes are left marked"
            );
            return;
        }
        for stripe in &map.stripes {
            if let Err(e) = self.destroy_stripe(*stripe).await {
                log::warn!(
                    "rmdir {dir}: stripe {stripe} not destroyed ({e}) — an nlink-0 corpse the \
                     next mount's sweep reclaims"
                );
            }
        }
        self.forget_map(dir);
        DIR_STRIPE_RMDIRS.fetch_add(1, Ordering::Relaxed);
        // Saturating: a directory another mount flipped is not in this
        // mount's count.
        let _ = DIR_STRIPED_DIRS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_sub(1))
        });
    }

    /// Undo the dying mark on every stripe that carries it (under the
    /// rmdir's held stripe guards).
    async fn revive_stripes(&self, map: &StripeMap, guards: Arc<[dlm::DlmGuard]>) -> Result<()> {
        let mut steps = Vec::new();
        for stripe in &map.stripes {
            let (sv, slocal) = self.route_ino(*stripe);
            if let Some(r) = self.volumes[sv].read_inode_value_routed(slocal).await? {
                if r.nlink == 0 {
                    steps.push(XvStep::SetNlink {
                        ino: *stripe,
                        pre: 0,
                        post: 2,
                        ctime: None,
                    });
                }
            }
        }
        if steps.is_empty() {
            return Ok(());
        }
        crossvol_tx::execute(
            self,
            &XvPlan {
                op: XvOp::Rmdir,
                steps,
            },
            guards,
        )
        .await?;
        Ok(())
    }

    /// The `mkdir`-time flip under `-o stripe_dirs`: stripe the new
    /// directory when the option is on and its volume is armed. Failure
    /// is logged, never the mkdir's error (the directory exists; the
    /// automatic trigger can still flip it later).
    pub async fn stripe_at_mkdir(&self, dir: Ino) {
        if !self.stripe_dirs_at_mkdir() {
            return;
        }
        let (v, _) = self.route_ino(dir);
        if !self.stripes_armed(v) || stripe_count() <= 1 {
            return;
        }
        if let Err(e) = self.flip_dir(dir, stripe_count(), &[]).await {
            log::warn!("-o stripe_dirs: directory {dir} not striped at mkdir ({e})");
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
