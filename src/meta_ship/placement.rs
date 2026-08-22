//! DLM S10 rung 14 — **client-owned-slot placement** (design-full-multi-writer
//! §8.2 lever 2, KD-MW-6; PR-plan row 14).
//!
//! Delegation (rung 12) and UPDATE intents (rung 13) remove round trips
//! for *existing* work; placement removes them **structurally** for new
//! work, in two composed halves:
//!
//! 1. **The mint-targeting hint** ([`client_mint_slot`], consulted by
//!    [`crate::meta_backend::RoutedMetaBackend::pick_mint_slot`]): a mint
//!    executing FOR a shipping client (the owner-side `SHIP_CLIENT` scope)
//!    lands in a slot **dedicated to that client** — chosen OUTSIDE the
//!    volume's mint set so the owner's own rotor never interleaves into
//!    it, stable per `(client, volume)`, distinct between clients. That is
//!    what makes "the client's slots" a real, migratable unit: the whole
//!    of a client's minted population is movable through the EXISTING
//!    online `migrate-meta-slot` engine as O(slots), not O(records
//!    scattered over the rotor). The hint composes **under** the mint
//!    constraint ([`super::constrain_mint_volume`] — one appender per
//!    volume, §6.2 items 2/3/4), never above it: the volume is constrained
//!    first, the slot picked within it.
//! 2. **The migration policy** ([`note_supply_event`]): a client whose
//!    supply consumption is SUSTAINED (a run of the rung-11
//!    pattern-vs-coincidence constant) *and* who **owns a metadata volume**
//!    (the ownership-map inversion — spec §6.10 R4's fleet-of-authorities
//!    recipe) gets its hot slots migrated toward its own volume, after
//!    which its metadata verbs on those inos run **locally** (it IS the
//!    S8 authority for them — zero wire). The vehicle is the existing
//!    engine (`slot_migration::migrate_slot`), installed as an executor by
//!    the multi-writer authority arm; the policy is rate-bounded (one
//!    migration in flight, one launch per trigger) and **valve-bounded**
//!    (the rung-11 engaged-at-N arithmetic: alternating clients can never
//!    ping-pong a shared directory's slot — it demotes to stay-put for the
//!    derived cooldown and re-promotes with the evidence reset).
//!
//! **The shipped-topology honesty statement (stated, not hidden):** on
//! every fleet the product can mount today exactly ONE node holds the D0
//! claim on every metadata volume, so no shipping client ever owns one and
//! the MIGRATION half is structurally dark (`migration_candidates` stays
//! 0) while the mint-targeting half engages. The half that goes live with
//! a future per-volume claim admission is fully pinned in
//! `tests/mw_slot_placement_tests.rs` against the in-process
//! fleet-of-authorities shape.
//!
//! Solo cost: every entry point gates on one relaxed load
//! ([`super::ownership_armed`]) and the absent `SHIP_CLIENT` task-local —
//! the shipped mount's mint path is structurally unchanged (the
//! dark-posture pin).

use crate::error::Result;
use crate::meta_backend::RoutedMetaBackend;
use arc_swap::ArcSwapOption;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The S10 placement A/B lever (ENG-10 registry entry; `Kind::Bool`,
/// static default **on**, read only when the mw plane is armed — the
/// `SQUEEZEFS_DELEGATION` form verbatim). `=0` on an armed mount is the
/// A/B control: mints ride the rotor exactly as rung 13 shipped them.
pub const PLACEMENT_ENV: &str = "SQUEEZEFS_SLOT_PLACEMENT";

/// **Test seam** (the `TEST_DELEGATION_OVERRIDE` form): `0` = read the
/// env knob, `1` = force on, `2` = force off.
pub static TEST_PLACEMENT_OVERRIDE: AtomicU8 = AtomicU8::new(0);

/// **Test seam for KD-PV-13's disarm**: `0` = the law (disarmed while a
/// multi-owner plane is armed), `1` = force disarmed, `2` = force ARMED.
///
/// Value `2` exists for the [`super::owners::arm_ownership`] reason —
/// reachable so the behaviour is TESTED rather than commented. The launch
/// machinery below (the executor, the one-in-flight bound and the
/// never-thrash valve) is what D19's named cross-owner-migration follow-on
/// inherits; under the law it can no longer be reached, because every
/// migration this policy can select is cross-owner and sweep row 13
/// refuses it.
pub static TEST_MIGRATION_DISARM_OVERRIDE: AtomicU8 = AtomicU8::new(0);

/// Is the placement plane live on this process? One relaxed load on every
/// unarmed mount — the dark-posture gate.
pub fn slot_placement_enabled() -> bool {
    if !super::ownership_armed() {
        return false;
    }
    match TEST_PLACEMENT_OVERRIDE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => crate::env_knobs::bool_knob(PLACEMENT_ENV, true),
    }
}

// ---------------------------------------------------------------------------
// Counters (the rung's engagement gauges — all structurally 0 on every
// mount without an armed ownership plane).
// ---------------------------------------------------------------------------

static CLIENT_SLOT_MINTS: AtomicU64 = AtomicU64::new(0);
static ROTOR_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static SUPPLY_EVENTS: AtomicU64 = AtomicU64::new(0);
static MIGRATION_CANDIDATES: AtomicU64 = AtomicU64::new(0);
static MIGRATIONS_TRIGGERED: AtomicU64 = AtomicU64::new(0);
static MIGRATIONS_COMPLETED: AtomicU64 = AtomicU64::new(0);
static MIGRATIONS_FAILED: AtomicU64 = AtomicU64::new(0);
static THRASH_DEMOTIONS: AtomicU64 = AtomicU64::new(0);
static VALVE_HOLDS: AtomicU64 = AtomicU64::new(0);
static FENCES: AtomicU64 = AtomicU64::new(0);
/// The process-wide rate bound: at most ONE policy migration in flight
/// (the engine's own `migration_lock` serializes a second anyway — this
/// bound keeps the policy from queueing work behind it).
static MIGRATION_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// State (control-plane — the rung-11/13 lane's sanctioned Mutex class;
// the fast gates above are relaxed atomics and the absent task-local).
// ---------------------------------------------------------------------------

/// A client's live policy evidence.
#[derive(Default)]
struct ClientPolicy {
    /// Consecutive supply events since the last trigger (saturates at the
    /// sustain threshold — evidence is a RUN detector, never a backlog).
    evidence: u32,
    /// Hot directory slots associated to this client (UPDATE grants whose
    /// supplies it consumed) — the "subtree's hot slots" half of the
    /// policy's migration set. Bounded at [`crate::meta_backend::MINT_SPREAD`]
    /// (the same movable-slice constant), FIFO.
    dirs: Vec<u16>,
}

/// One slot's anti-thrash valve (the rung-11 arithmetic on migration
/// EPISODES: an episode = a policy-launched move of this slot).
#[derive(Default)]
struct SlotValve {
    last_move: Option<Instant>,
    cycles: u32,
    demoted_until: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// `(client, volume)` → the client's dedicated slot on that volume.
    assigns: HashMap<(String, usize), u16>,
    /// Per-client policy evidence.
    policy: HashMap<String, ClientPolicy>,
    /// Per-slot migration valve.
    valve: HashMap<u16, SlotValve>,
}

static STATE: Lazy<Mutex<State>> = Lazy::new(|| Mutex::new(State::default()));

// ---------------------------------------------------------------------------
// The policy configuration — every quantity derived, no free constants.
// ---------------------------------------------------------------------------

/// The placement policy's derived configuration. Public fields so tests
/// drive the valve with injected instants (the rung-11 storm-table
/// discipline); production callers use [`PolicyConfig::derived`].
#[derive(Debug, Clone, Copy)]
pub struct PolicyConfig {
    /// Supply events in a client's run before it counts as SUSTAINED:
    /// the rung-11 pattern-vs-coincidence constant (**3** — one event is
    /// any legitimate burst, two can be one burst's refill).
    pub sustain_runs: u32,
    /// Migration episodes of ONE slot inside [`Self::window`] before the
    /// valve demotes it: the same constant, same reasoning.
    pub thrash_cycles: u32,
    /// The cycle-detection window: the membership lease TTL — a slot that
    /// moves twice inside one lease period is cycling faster than clients
    /// re-home their custody; slower alternation is priced as legitimate
    /// re-placement (each move is then rarer than the custody re-home
    /// horizon).
    pub window: Duration,
    /// Demotion hold: `8 ×` window (the rung-11
    /// [`super::tokens::recall_cooldown_from`] arithmetic, reused).
    pub cooldown: Duration,
}

impl PolicyConfig {
    /// The derived shipped configuration (no knobs — the valve is
    /// structural, the rung-11 law).
    pub fn derived() -> Self {
        let window = super::tokens::recall_lease_ttl();
        Self {
            sustain_runs: super::RECALL_THRASH_CYCLES,
            thrash_cycles: super::RECALL_THRASH_CYCLES,
            window,
            cooldown: super::tokens::recall_cooldown_from(None, window),
        }
    }
}

// ---------------------------------------------------------------------------
// The mint-targeting hint (half 1)
// ---------------------------------------------------------------------------

/// The per-client mint-slot pick, consulted by
/// [`RoutedMetaBackend::pick_mint_slot`] with the live route table's rows.
/// `None` = the rotor path (unarmed / lever off / no shipping client in
/// scope / no eligible slot — the last counted as a rotor fallback).
///
/// The pick is deterministic per client (stable hash into the volume's
/// hosted slots outside the mint set, linear-probed past slots already
/// dedicated to OTHER clients so distinctness is structural, never
/// probabilistic), cached per `(client, volume)`, and re-derived when the
/// cached slot no longer routes to the volume (a migration moved it —
/// which is the policy WORKING, not an error).
pub fn client_mint_slot(
    volume_idx: usize,
    slot_to_volume: &[usize],
    mint_set: &[u16],
) -> Option<u64> {
    if !slot_placement_enabled() {
        return None;
    }
    let client = super::service::current_ship_client()?;
    let mut st = STATE.lock();
    if let Some(&slot) = st.assigns.get(&(client.clone(), volume_idx)) {
        if slot_to_volume.get(usize::from(slot)) == Some(&volume_idx) {
            CLIENT_SLOT_MINTS.fetch_add(1, Ordering::Relaxed);
            return Some(u64::from(slot));
        }
        // The slot migrated away since the assignment: re-derive.
        st.assigns.remove(&(client.clone(), volume_idx));
    }
    // Eligible: hosted by this volume, not the root pin (slot 0 carries
    // ino 1), outside the mint set (rotor-clean — the owner's own mints
    // never interleave into the client's unit).
    let candidates: Vec<u16> = slot_to_volume
        .iter()
        .enumerate()
        .filter(|&(s, &v)| v == volume_idx && s != 0 && !mint_set.contains(&(s as u16)))
        .map(|(s, _)| s as u16)
        .collect();
    if candidates.is_empty() {
        // A volume hosting nothing beyond its mint set: the stated
        // fallback — the mint rides the rotor, counted.
        ROTOR_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut h = DefaultHasher::new();
    client.hash(&mut h);
    let start = (h.finish() as usize) % candidates.len();
    let taken: std::collections::HashSet<u16> = st.assigns.values().copied().collect();
    let slot = (0..candidates.len())
        .map(|i| candidates[(start + i) % candidates.len()])
        .find(|s| !taken.contains(s))
        // Every eligible slot dedicated elsewhere (more clients than
        // hosted slots): share the hashed one — stability over purity.
        .unwrap_or(candidates[start]);
    st.assigns.insert((client, volume_idx), slot);
    CLIENT_SLOT_MINTS.fetch_add(1, Ordering::Relaxed);
    Some(u64::from(slot))
}

// ---------------------------------------------------------------------------
// The migration policy (half 2)
// ---------------------------------------------------------------------------

/// The migration vehicle: `(slot, target volume index)` → the engine's
/// outcome. Installed by the multi-writer authority arm
/// ([`authority_migration_executor`]) or a test.
pub type MigrationExec = Arc<
    dyn Fn(u16, usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

static EXECUTOR: Lazy<ArcSwapOption<MigrationExec>> = Lazy::new(ArcSwapOption::empty);

/// Install the policy's migration vehicle (the authority arm's act).
pub fn install_migration_executor(exec: MigrationExec) {
    EXECUTOR.store(Some(Arc::new(exec)));
}

/// Remove the vehicle (disarm) — the policy then observes but never moves.
pub fn uninstall_migration_executor() {
    EXECUTOR.store(None);
}

/// The PRODUCTION vehicle: the existing online `migrate-meta-slot` engine
/// over the authority's own routed set, followed by the ownership re-arm
/// the migration-while-armed law demands (`owners.rs`: a slot migration
/// changes the derived local slot set, so the map must be republished at
/// cutover exactly as the routing table is).
pub fn authority_migration_executor(meta: Arc<RoutedMetaBackend>) -> MigrationExec {
    Arc::new(move |slot, target| {
        let meta = Arc::clone(&meta);
        Box::pin(async move {
            crate::meta_backend::slot_migration::migrate_slot(
                &meta,
                slot,
                target,
                &crate::meta_backend::slot_migration::MigrationOptions::default(),
                &crate::meta_backend::slot_migration::MigrationTestHooks::default(),
            )
            .await?;
            super::owners::rearm_ownership(&meta)
        })
    })
}

/// **KD-PV-13's predicate**: is the migration half inert on this mount?
///
/// True while the installed map names at least one PEER-owned volume,
/// which is exactly the condition under which `migrate_slot` refuses a
/// policy-selected move (its endpoints then have different owners — sweep
/// row 13). The A/B-style override exists so the launch machinery the
/// follow-on inherits stays under test.
fn migration_half_disarmed(map: &super::OwnerMap) -> bool {
    match TEST_MIGRATION_DISARM_OVERRIDE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => map.multi_owner(),
    }
}

/// Associate `client` with a hot directory slot (the owner records it at
/// UPDATE-grant issuance): the policy's migration set is the client's
/// dedicated mint slots PLUS these — "the subtree's hot slots".
pub fn note_client_dir(client: &str, dir_slot: u16) {
    if !slot_placement_enabled() {
        return;
    }
    let mut st = STATE.lock();
    let pol = st.policy.entry(client.to_string()).or_default();
    if !pol.dirs.contains(&dir_slot) {
        if pol.dirs.len() >= crate::meta_backend::MINT_SPREAD {
            pol.dirs.remove(0);
        }
        pol.dirs.push(dir_slot);
    }
}

/// One supply reservation ran for `client` (grant or refill) — the
/// policy's trigger event. At a SUSTAINED run, when the ownership map
/// names a volume `client` owns, launch (at most) ONE valve-admitted slot
/// migration toward it through the installed executor. `home_of` answers
/// a slot's current volume (the live route table — injectable so the
/// valve arms are testable with fabricated instants).
pub fn note_supply_event(
    client: &str,
    cfg: &PolicyConfig,
    now: Instant,
    home_of: impl Fn(u16) -> Option<usize>,
) {
    if !slot_placement_enabled() {
        return;
    }
    SUPPLY_EVENTS.fetch_add(1, Ordering::Relaxed);
    let Some(map) = super::owner_map() else {
        return;
    };
    let launch = {
        let mut st = STATE.lock();
        let pol = st.policy.entry(client.to_string()).or_default();
        pol.evidence = pol.evidence.saturating_add(1).min(cfg.sustain_runs);
        if pol.evidence < cfg.sustain_runs {
            return;
        }
        // The candidate inversion: does this client own a volume? On
        // every shipped fleet the answer is no — the policy stays dark
        // (evidence saturated, nothing counted, nothing moved).
        let owned = map.volumes_owned_by(client);
        if owned.is_empty() {
            return;
        }
        MIGRATION_CANDIDATES.fetch_add(1, Ordering::Relaxed);
        // **KD-PV-13 — the migration half is DISARMED under multi-owner.**
        // The target is a volume the CLIENT owns and the victim's home is
        // by construction not in that set, so every migration selectable
        // here is a CROSS-OWNER one, which `migrate_slot` refuses (sweep
        // row 13, D19's deferred two-party hand-off). Left armed the
        // composition is a permanent trigger → refuse → fail retry loop on
        // a healthy fleet. The candidate above still counts — it is the
        // follow-on's demand signal — and nothing else happens.
        if migration_half_disarmed(&map) {
            log::debug!(
                "S10 placement: client '{client}' is sustained toward volume {} it owns, but \
                 the migration half is DISARMED while a multi-owner plane is armed (KD-PV-13): \
                 the move would be cross-owner, which the engine refuses. Counted as demand, \
                 nothing triggered",
                owned[0]
            );
            return;
        }
        let target = owned[0];
        // The hot set: the client's dedicated mint slots, then its
        // associated directory slots — first one not already home.
        let mut hot: Vec<u16> = st
            .assigns
            .iter()
            .filter(|((c, _), _)| c == client)
            .map(|(_, &s)| s)
            .collect();
        hot.extend(
            st.policy
                .get(client)
                .map(|p| p.dirs.clone())
                .unwrap_or_default(),
        );
        let victim = hot.into_iter().find(|&s| match home_of(s) {
            Some(v) => !owned.contains(&v),
            None => false,
        });
        let Some(slot) = victim else {
            // Everything already home: the end state — evidence resets,
            // nothing to move.
            st.policy.entry(client.to_string()).or_default().evidence = 0;
            return;
        };
        // The executor must exist to count an episode: a valve cycle
        // charged for a move that could never launch would demote slots
        // on an unarmed vehicle.
        let Some(exec_slot) = EXECUTOR.load_full() else {
            log::debug!(
                "S10 placement: client '{client}' sustained toward volume {target} but no \
                 migration executor is installed — observing only"
            );
            return;
        };
        let exec: MigrationExec = MigrationExec::clone(&exec_slot);
        // One migration in flight, process-wide (evidence is kept so the
        // next event retries).
        if MIGRATION_IN_FLIGHT.load(Ordering::Acquire) {
            return;
        }
        // The valve (rung-11 engaged-at-N arithmetic on this SLOT's
        // migration episodes).
        let v = st.valve.entry(slot).or_default();
        if let Some(until) = v.demoted_until {
            if now < until {
                VALVE_HOLDS.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // Past the cooldown: re-promote with the evidence reset.
            v.demoted_until = None;
            v.cycles = 0;
            v.last_move = None;
        }
        match v.last_move {
            Some(t) if now.duration_since(t) < cfg.window => v.cycles += 1,
            _ => v.cycles = 1,
        }
        v.last_move = Some(now);
        if v.cycles >= cfg.thrash_cycles {
            // Engaged AT the Nth episode (the rung-11 storm table's
            // shape): this move still issues; every later attempt holds
            // for the cooldown.
            v.demoted_until = Some(now + cfg.cooldown);
            THRASH_DEMOTIONS.fetch_add(1, Ordering::Relaxed);
        }
        MIGRATION_IN_FLIGHT.store(true, Ordering::Release);
        MIGRATIONS_TRIGGERED.fetch_add(1, Ordering::Relaxed);
        st.policy.entry(client.to_string()).or_default().evidence = 0;
        (slot, target, exec)
    };
    let (slot, target, exec) = launch;
    let client = client.to_string();
    crate::meta_exec::spawn_meta("meta_ship_placement_migration", async move {
        log::warn!(
            "S10 placement: migrating slot {slot} toward volume {target} for sustained client \
             '{client}' (the client-owned-slot policy — its verbs on this slot's inos become \
             LOCAL to it at cutover)"
        );
        match (*exec)(slot, target).await {
            Ok(()) => {
                MIGRATIONS_COMPLETED.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                MIGRATIONS_FAILED.fetch_add(1, Ordering::Relaxed);
                log::error!(
                    "S10 placement: the policy migration of slot {slot} → volume {target} \
                     FAILED ({e}) — the engine is idempotent, a later sustained run retries"
                );
            }
        }
        MIGRATION_IN_FLIGHT.store(false, Ordering::Release);
    });
}

/// The era hook (rung 13's `note_client_incarnation` law): `client`'s
/// incarnation changed or it was fenced — its placement state dies, so a
/// zombie's half-run can never compose with its successor's into a
/// trigger. Counted only when state actually died.
pub fn fence_client(client: &str) {
    let mut st = STATE.lock();
    let had_assigns = {
        let before = st.assigns.len();
        st.assigns.retain(|(c, _), _| c != client);
        st.assigns.len() != before
    };
    let had_policy = st.policy.remove(client).is_some();
    if had_assigns || had_policy {
        FENCES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drop every assignment and policy record (the disarm path — placement
/// state is meaningless without an ownership plane). Counters survive;
/// [`test_clear_placement`] resets those too.
pub(crate) fn clear_runtime_state() {
    let mut st = STATE.lock();
    st.assigns.clear();
    st.policy.clear();
    st.valve.clear();
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// The rung's stats snapshot (the `meta_ship_placement` stats-inode
/// object).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementStats {
    /// Client-targeted mint picks (assignment hits + derivations).
    pub client_slot_mints: u64,
    /// GAUGE: live `(client, volume)` slot assignments.
    pub client_slots: u64,
    /// Mints that fell back to the rotor (no eligible slot).
    pub rotor_fallbacks: u64,
    /// Supply events observed by the policy (grant + refills).
    pub supply_events: u64,
    /// GAUGE: live per-client sustained-run evidence (sums; dies with a
    /// fence, resets on a trigger).
    pub sustain_evidence: u64,
    /// Sustained runs whose candidate inversion FOUND a client-owned
    /// volume — structurally 0 on every shipped (one-authority) fleet.
    pub migration_candidates: u64,
    /// Policy-launched migrations.
    pub migrations_triggered: u64,
    pub migrations_completed: u64,
    pub migrations_failed: u64,
    /// Valve engagements (the never-thrash law) + holds during cooldown.
    pub thrash_demotions: u64,
    pub valve_holds: u64,
    /// Fenced incarnations whose placement state died.
    pub fences: u64,
}

/// Snapshot the family.
pub fn placement_stats() -> PlacementStats {
    let (client_slots, sustain_evidence) = {
        let st = STATE.lock();
        (
            st.assigns.len() as u64,
            st.policy.values().map(|p| u64::from(p.evidence)).sum(),
        )
    };
    PlacementStats {
        client_slot_mints: CLIENT_SLOT_MINTS.load(Ordering::Relaxed),
        client_slots,
        rotor_fallbacks: ROTOR_FALLBACKS.load(Ordering::Relaxed),
        supply_events: SUPPLY_EVENTS.load(Ordering::Relaxed),
        sustain_evidence,
        migration_candidates: MIGRATION_CANDIDATES.load(Ordering::Relaxed),
        migrations_triggered: MIGRATIONS_TRIGGERED.load(Ordering::Relaxed),
        migrations_completed: MIGRATIONS_COMPLETED.load(Ordering::Relaxed),
        migrations_failed: MIGRATIONS_FAILED.load(Ordering::Relaxed),
        thrash_demotions: THRASH_DEMOTIONS.load(Ordering::Relaxed),
        valve_holds: VALVE_HOLDS.load(Ordering::Relaxed),
        fences: FENCES.load(Ordering::Relaxed),
    }
}

/// The `meta_ship_placement` stats-inode object.
pub fn placement_stats_json() -> serde_json::Value {
    let s = placement_stats();
    serde_json::json!({
        "meta_ship_placement_client_slot_mints": s.client_slot_mints,
        "meta_ship_placement_client_slots": s.client_slots,
        "meta_ship_placement_rotor_fallbacks": s.rotor_fallbacks,
        "meta_ship_placement_supply_events": s.supply_events,
        "meta_ship_placement_sustain_evidence": s.sustain_evidence,
        "meta_ship_placement_migration_candidates": s.migration_candidates,
        "meta_ship_placement_migrations_triggered": s.migrations_triggered,
        "meta_ship_placement_migrations_completed": s.migrations_completed,
        "meta_ship_placement_migrations_failed": s.migrations_failed,
        "meta_ship_placement_thrash_demotions": s.thrash_demotions,
        "meta_ship_placement_valve_holds": s.valve_holds,
        "meta_ship_placement_fences": s.fences,
    })
}

/// The slots currently dedicated to `client` (test/observability surface).
pub fn client_assigned_slots(client: &str) -> Vec<u16> {
    let st = STATE.lock();
    let mut out: Vec<u16> = st
        .assigns
        .iter()
        .filter(|((c, _), _)| c == client)
        .map(|(_, &s)| s)
        .collect();
    out.sort_unstable();
    out
}

/// Reset EVERYTHING (test isolation — the ArmGuard's arm).
pub fn test_clear_placement() {
    clear_runtime_state();
    CLIENT_SLOT_MINTS.store(0, Ordering::Relaxed);
    ROTOR_FALLBACKS.store(0, Ordering::Relaxed);
    SUPPLY_EVENTS.store(0, Ordering::Relaxed);
    MIGRATION_CANDIDATES.store(0, Ordering::Relaxed);
    MIGRATIONS_TRIGGERED.store(0, Ordering::Relaxed);
    MIGRATIONS_COMPLETED.store(0, Ordering::Relaxed);
    MIGRATIONS_FAILED.store(0, Ordering::Relaxed);
    THRASH_DEMOTIONS.store(0, Ordering::Relaxed);
    VALVE_HOLDS.store(0, Ordering::Relaxed);
    FENCES.store(0, Ordering::Relaxed);
    MIGRATION_IN_FLIGHT.store(false, Ordering::Release);
}
