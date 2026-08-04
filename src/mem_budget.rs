//! R5 — the joint memory authority (docs/design-read-path.md §5.7).
//!
//! One budget, one pressure signal, three levels. The daemon's RAM
//! consumers (parked write buffers, staging/tier mmaps, the RAM LRUs, the
//! hot-block tier, the prefetch in-flight gauge, the buffer pools) each
//! register a `Component` — a gauge closure, a floor, a weight, and a shed
//! closure — into a latch-free `ArcSwap` registry. A 1 Hz sampler
//! resolves the budget (flag → env → cgroup `memory.max` × 0.8 re-read
//! every tick → 70 % RAM), samples `/proc/self/statm` RSS **and** the
//! cgroup-v2 UNRECLAIMABLE set (`memory.stat`: anon + dirty + writeback +
//! shmem + unevictable + unreclaimable slab — the bytes the OOM killer
//! cannot reclaim its way out of; clean cache deliberately excluded) into
//! two 5-slot decaying windows (max-of-window, explicitly NOT ratchets),
//! computes `pressure = max(Σ gauges, windowed_rss_max,
//! windowed_unreclaimable_max)`, and drives the level machine with
//! hysteresis. Red ticks shed to weights over floors through the
//! components' own never-lossy mechanisms.
//!
//! Enforcement is ADVISORY-AT-ADMISSION: growth paths call [`level`] —
//! one relaxed atomic load, no lock, nothing blocking — and apply their
//! own documented response (§5.7 table: Yellow stops growth + pauses
//! dehydration entirely; Red sheds, halves the parked-buffer cap, stops
//! prefetch issue). Write-side durability is untouched: shedding parked
//! buffers means flushing them through the existing durable paths sooner,
//! never dropping them.
//!
//! Two escalations beyond advisory (the 2026-07-12 saturation-suite
//! cage-OOM finding #2 — Red fired but shedding did not converge against
//! 16-stream admission; `.benchmarks/2026-07-12-saturation-suite-oom-
//! finding2.md`):
//! - **Disk-tier publish pause** ([`tier_publish_paused`]): while the
//!   unreclaimable arm sits in the Red band (enter ≥ 95 %, release
//!   < 91 % — same 4-point hysteresis), read-tier publishes are skipped
//!   at [`crate::cache::NvmeStaging::cache_read_block`] (never-lossy —
//!   it is a read cache). Keyed on the unreclaimable arm, NOT the level:
//!   gauge-driven Red over clean (kernel-reclaimable) tier bytes must
//!   keep publishing or warm-up would starve (the PR 3 knee scenario).
//! - **Hard backstop** ([`MemBudget::hard_backstops`]): the unreclaimable
//!   arm ≥ 100 % of budget for [`BACKSTOP_SUSTAIN_TICKS`] consecutive
//!   ticks — Red demonstrably not converging — collapses the shed target
//!   from 85 % to the FLOORS until the window decays below the Red-exit
//!   edge. This holds even when a component gauge undercounts: the arm
//!   reads the kernel's accounting, not ours.
//!
//! The third Red response lives at the parked-buffer admission site
//! (`fuse_client::insert_active_block_buffer`): at Red the halved cap is
//! a real bound — writers await the never-lossy drain (bounded, async)
//! instead of parking past it. See the §4.4-pt-5 ring-admission
//! precedent; measured pre-fix: 1,937 parked buffers = 7.6 GiB anon
//! against a 256 cap while staging refused every spill.
//!
//! Concurrency: single-word relaxed atomics throughout (level, budget,
//! pressure, event counters) — racy-tolerant by design (a stale level
//! read costs one admission decision, never correctness); the registry is
//! `ArcSwap<Vec<Component>>` (read = one guarded load). No cross-word
//! invariant ⇒ no loom model required (the doc's loom-scope note).
//!
//! jemalloc watch stays OUT per approved OQ #4: the RSS sampler covers
//! allocator-held memory; the recorded follow-up triggers only if
//! `windowed_rss_max` sustains ≳ 10 % above the gauge sum on quiet
//! workloads (allocator drift), observable from the exported gauges.

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

/// Pressure levels (§5.7 table). Encoded in one `AtomicU8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Green = 0,
    Yellow = 1,
    Red = 2,
}

impl Level {
    fn from_u8(v: u8) -> Self {
        match v {
            2 => Level::Red,
            1 => Level::Yellow,
            _ => Level::Green,
        }
    }
}

/// Hysteresis bands (percent of budget). Enter Yellow at ≥ 80 %, leave
/// below 76 %; enter Red at ≥ 95 %, leave below 91 % — 4-point gaps so a
/// pressure hovering AT a boundary cannot flap the level (R-7).
pub const YELLOW_ENTER_PCT: u64 = 80;
pub const YELLOW_EXIT_PCT: u64 = 76;
pub const RED_ENTER_PCT: u64 = 95;
pub const RED_EXIT_PCT: u64 = 91;
/// Red sheds aim the total at this fraction of the budget (mid-Yellow —
/// below the Yellow-exit edge would oscillate the level itself).
const SHED_TARGET_PCT: u64 = 85;
/// Floors may claim at most this share of the budget before the
/// proportional clamp engages (§5.7 floor validation).
const FLOOR_CAP_PCT: u64 = 90;
/// RSS window length (1 Hz samples) — the decay horizon.
const RSS_WINDOW: usize = 5;
/// Consecutive ticks the windowed unreclaimable arm must sit at/over the
/// FULL budget before the hard backstop escalates Red sheds to the floors
/// (§5.7 Red semantics). One or two ticks is a spike the normal Red
/// response absorbs; three is a convergence failure.
pub const BACKSTOP_SUSTAIN_TICKS: u64 = 3;

/// One registered RAM consumer (§5.7 authority). Closures, not fn
/// pointers: components carry state (cache handles, gauges) and the
/// registry is shared through the `ArcSwap`.
pub struct Component {
    pub name: &'static str,
    /// Current logical bytes (mmap tiers report logical; RSS drift is the
    /// sampler's job).
    pub current: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Bytes this component may always keep (clamped proportionally when
    /// Σ floors exceeds 90 % of a small budget).
    pub floor: u64,
    /// Red-shed share: excess is distributed ∝ weight across components
    /// above their floor.
    pub weight: u64,
    /// Shed toward `target` bytes through the component's own never-lossy
    /// mechanism (early flush, probation drop, plan clear, pool trim).
    pub shed: Arc<dyn Fn(u64) + Send + Sync>,
    /// This component's bytes are kernel-reclaimable page cache (the mmap
    /// tiers): registered for ATTRIBUTION (stats registry, full gauge
    /// sum) but EXCLUDED from the pressure basis — the f1 verification
    /// trace showed 5 GiB of already-reclaimed tier logical bytes pinning
    /// phantom Red through the rand passes. Their kill-relevant residue
    /// (dirty/writeback) is what the unreclaimable arm measures.
    kernel_reclaimable: bool,
    sheds: AtomicU64,
    shed_target_bytes: AtomicU64,
}

impl Component {
    pub fn new(
        name: &'static str,
        floor: u64,
        weight: u64,
        current: Arc<dyn Fn() -> u64 + Send + Sync>,
        shed: Arc<dyn Fn(u64) + Send + Sync>,
    ) -> Self {
        Self {
            name,
            current,
            floor,
            weight,
            shed,
            kernel_reclaimable: false,
            sheds: AtomicU64::new(0),
            shed_target_bytes: AtomicU64::new(0),
        }
    }

    /// Builder: mark this component's bytes as kernel-reclaimable page
    /// cache (see the field doc — attribution-only, never pressure).
    pub fn kernel_reclaimable(mut self) -> Self {
        self.kernel_reclaimable = true;
        self
    }

    pub fn sheds(&self) -> u64 {
        self.sheds.load(Relaxed)
    }
}

/// The authority. One per process ([`MEM_BUDGET`]); tests construct
/// private instances for the arithmetic contracts.
pub struct MemBudget {
    registry: ArcSwap<Vec<Arc<Component>>>,
    /// `--mem-budget` / env override (0 = unset).
    flag_budget: AtomicU64,
    budget: AtomicU64,
    pressure: AtomicU64,
    gauge_sum: AtomicU64,
    rss_window: [AtomicU64; RSS_WINDOW],
    rss_idx: AtomicUsize,
    /// Decaying window over the cgroup-v2 unreclaimable set (anon + dirty
    /// + writeback + shmem + unevictable + unreclaimable slab) — the
    /// kill-relevant arm the statm sampler cannot see (dirty page cache)
    /// and component gauges may undercount.
    unreclaim_window: [AtomicU64; RSS_WINDOW],
    unreclaim_idx: AtomicUsize,
    /// Last windowed unreclaimable max (stats surface).
    unreclaimable: AtomicU64,
    level: AtomicU8,
    yellow_events: AtomicU64,
    red_events: AtomicU64,
    floors_clamped: AtomicBool,
    /// Disk-tier publish pause (unreclaimable-arm Red band, hysteresis).
    tier_paused: AtomicBool,
    /// Hard-backstop state: consecutive at/over-budget ticks, active flag,
    /// entry-edge counter.
    backstop_over_ticks: AtomicU64,
    backstop_on: AtomicBool,
    hard_backstops: AtomicU64,
}

impl MemBudget {
    fn new() -> Self {
        Self {
            registry: ArcSwap::from_pointee(Vec::new()),
            flag_budget: AtomicU64::new(0),
            budget: AtomicU64::new(0),
            pressure: AtomicU64::new(0),
            gauge_sum: AtomicU64::new(0),
            rss_window: Default::default(),
            rss_idx: AtomicUsize::new(0),
            unreclaim_window: Default::default(),
            unreclaim_idx: AtomicUsize::new(0),
            unreclaimable: AtomicU64::new(0),
            level: AtomicU8::new(Level::Green as u8),
            yellow_events: AtomicU64::new(0),
            red_events: AtomicU64::new(0),
            floors_clamped: AtomicBool::new(false),
            tier_paused: AtomicBool::new(false),
            backstop_over_ticks: AtomicU64::new(0),
            backstop_on: AtomicBool::new(false),
            hard_backstops: AtomicU64::new(0),
        }
    }

    /// Test constructor (private instance — no process-global coupling).
    pub fn new_for_test() -> Self {
        Self::new()
    }

    /// Register a consumer (mount-time; rcu append keeps readers
    /// latch-free).
    pub fn register(&self, c: Component) {
        let c = Arc::new(c);
        self.registry.rcu(|cur| {
            let mut v: Vec<Arc<Component>> = (**cur).clone();
            v.push(c.clone());
            v
        });
    }

    /// `--mem-budget` (parsed bytes). Wins the resolution order.
    pub fn set_flag_budget(&self, bytes: u64) {
        self.flag_budget.store(bytes, Relaxed);
    }

    pub fn level(&self) -> Level {
        Level::from_u8(self.level.load(Relaxed))
    }

    pub fn budget_bytes(&self) -> u64 {
        self.budget.load(Relaxed)
    }

    pub fn pressure_bytes(&self) -> u64 {
        self.pressure.load(Relaxed)
    }

    pub fn gauge_sum_bytes(&self) -> u64 {
        self.gauge_sum.load(Relaxed)
    }

    pub fn yellow_events(&self) -> u64 {
        self.yellow_events.load(Relaxed)
    }

    pub fn red_events(&self) -> u64 {
        self.red_events.load(Relaxed)
    }

    pub fn floors_clamped(&self) -> bool {
        self.floors_clamped.load(Relaxed)
    }

    /// Windowed max of the cgroup unreclaimable arm (stats surface).
    pub fn unreclaimable_bytes(&self) -> u64 {
        self.unreclaimable.load(Relaxed)
    }

    /// Hard-backstop entry edges (§5.7 Red-semantics escalation).
    pub fn hard_backstops(&self) -> u64 {
        self.hard_backstops.load(Relaxed)
    }

    /// Whether the hard backstop is currently escalating Red sheds to the
    /// floors.
    pub fn backstop_active(&self) -> bool {
        self.backstop_on.load(Relaxed)
    }

    /// Whether disk-tier publishes are paused (unreclaimable-arm Red band).
    pub fn tier_publish_paused(&self) -> bool {
        self.tier_paused.load(Relaxed)
    }

    /// Test seam: pin the process level (integration phases drive the
    /// advisory checks without a live sampler).
    pub fn force_level_for_test(&self, l: Level) {
        self.level.store(l as u8, Relaxed);
    }

    /// Test seam: pin the tier-publish pause (integration phases drive the
    /// publish gate without a live sampler).
    pub fn force_tier_publish_paused_for_test(&self, paused: bool) {
        self.tier_paused.store(paused, Relaxed);
    }

    /// Effective floors for `budget` (§5.7 floor validation): proportional
    /// clamp when Σ floors > 90 % of the budget, observable via
    /// [`Self::floors_clamped`] and a loud log line — never a mount
    /// failure.
    pub fn effective_floors(&self, budget: u64) -> Vec<u64> {
        let reg = self.registry.load();
        let sum: u64 = reg.iter().map(|c| c.floor).sum();
        let cap = budget * FLOOR_CAP_PCT / 100;
        if sum <= cap || sum == 0 {
            self.floors_clamped.store(false, Relaxed);
            return reg.iter().map(|c| c.floor).collect();
        }
        if !self.floors_clamped.swap(true, Relaxed) {
            log::warn!(
                "mem_budget: component floors sum to {sum} B > {FLOOR_CAP_PCT}% of the \
                 {budget} B budget — floors proportionally clamped (raise --mem-budget \
                 or lower component floors)"
            );
        }
        reg.iter().map(|c| c.floor * cap / sum).collect()
    }

    /// One sampler tick with injected inputs (the test seam; the real
    /// [`Self::tick`] resolves budget + RSS + the cgroup unreclaimable
    /// set and delegates here).
    pub fn tick_inner(&self, budget: u64, rss_sample: u64, unreclaimable_sample: u64) {
        self.budget.store(budget, Relaxed);

        // Decaying RSS window: overwrite the oldest slot, take the max.
        let idx = self.rss_idx.fetch_add(1, Relaxed) % RSS_WINDOW;
        self.rss_window[idx].store(rss_sample, Relaxed);
        let rss_max = self
            .rss_window
            .iter()
            .map(|s| s.load(Relaxed))
            .max()
            .unwrap_or(0);

        // Same decay semantics for the unreclaimable arm.
        let uidx = self.unreclaim_idx.fetch_add(1, Relaxed) % RSS_WINDOW;
        self.unreclaim_window[uidx].store(unreclaimable_sample, Relaxed);
        let unreclaim_max = self
            .unreclaim_window
            .iter()
            .map(|s| s.load(Relaxed))
            .max()
            .unwrap_or(0);
        self.unreclaimable.store(unreclaim_max, Relaxed);

        let reg = self.registry.load();
        // Full registry sum: the ATTRIBUTION surface (stats). The pressure
        // basis excludes kernel-reclaimable components — their logical
        // bytes over reclaimed pages are phantom pressure (f1 trace), and
        // their kill-relevant residue arrives via the unreclaimable arm.
        let mut gauge_sum = 0u64;
        let mut pressure_gauges = 0u64;
        for c in reg.iter() {
            let cur = (c.current)();
            gauge_sum += cur;
            if !c.kernel_reclaimable {
                pressure_gauges += cur;
            }
        }
        self.gauge_sum.store(gauge_sum, Relaxed);
        let pressure = pressure_gauges.max(rss_max).max(unreclaim_max);
        self.pressure.store(pressure, Relaxed);

        if budget == 0 {
            self.level.store(Level::Green as u8, Relaxed);
            self.tier_paused.store(false, Relaxed);
            self.backstop_over_ticks.store(0, Relaxed);
            self.backstop_on.store(false, Relaxed);
            return;
        }

        // Disk-tier publish pause: the unreclaimable arm alone (the
        // dirty-flood / anon-balloon kill signature), Red band with the
        // same 4-point hysteresis. Gauge- or statm-driven Red must NOT
        // pause publishes: logical tier bytes over clean page cache are
        // kernel-reclaimable, and pausing warm-up on them would regress
        // the PR 3 oversubscription scenario.
        let upct = unreclaim_max.saturating_mul(100) / budget;
        if upct >= RED_ENTER_PCT {
            if !self.tier_paused.swap(true, Relaxed) {
                log::warn!(
                    "mem_budget: unreclaimable {unreclaim_max} B >= {RED_ENTER_PCT}% of the \
                     {budget} B budget — disk-tier publishes paused"
                );
            }
        } else if upct < RED_EXIT_PCT && self.tier_paused.swap(false, Relaxed) {
            log::info!("mem_budget: unreclaimable pressure receded — disk-tier publishes resume");
        }

        // Hard backstop (§5.7 Red semantics): sustained unreclaimable
        // at/over the FULL budget means Red shedding is not converging
        // against admission — escalate the shed target to the floors.
        // Kernel accounting, not component gauges: this holds even when a
        // gauge undercounts.
        if unreclaim_max >= budget {
            let over = self.backstop_over_ticks.fetch_add(1, Relaxed) + 1;
            if over >= BACKSTOP_SUSTAIN_TICKS && !self.backstop_on.swap(true, Relaxed) {
                self.hard_backstops.fetch_add(1, Relaxed);
                log::warn!(
                    "mem_budget: HARD BACKSTOP — unreclaimable {unreclaim_max} B >= the full \
                     {budget} B budget for {over} consecutive ticks; Red sheds escalate to \
                     component floors until the pressure recedes"
                );
            }
        } else {
            self.backstop_over_ticks.store(0, Relaxed);
            if upct < RED_EXIT_PCT && self.backstop_on.swap(false, Relaxed) {
                log::info!("mem_budget: hard backstop released");
            }
        }

        let pct = pressure.saturating_mul(100) / budget;
        let prev = Level::from_u8(self.level.load(Relaxed));
        let next = match prev {
            Level::Green => {
                if pct >= RED_ENTER_PCT {
                    Level::Red
                } else if pct >= YELLOW_ENTER_PCT {
                    Level::Yellow
                } else {
                    Level::Green
                }
            }
            Level::Yellow => {
                if pct >= RED_ENTER_PCT {
                    Level::Red
                } else if pct < YELLOW_EXIT_PCT {
                    Level::Green
                } else {
                    Level::Yellow
                }
            }
            Level::Red => {
                if pct >= RED_EXIT_PCT {
                    Level::Red
                } else if pct >= YELLOW_EXIT_PCT {
                    Level::Yellow
                } else {
                    Level::Green
                }
            }
        };
        if next != prev {
            match next {
                Level::Yellow if prev == Level::Green => {
                    self.yellow_events.fetch_add(1, Relaxed);
                }
                Level::Red => {
                    // Entering Red from Green counts both edges.
                    if prev == Level::Green {
                        self.yellow_events.fetch_add(1, Relaxed);
                    }
                    self.red_events.fetch_add(1, Relaxed);
                }
                _ => {}
            }
            self.level.store(next as u8, Relaxed);
        }

        if next == Level::Red {
            self.shed_to_weights(budget, pressure, &reg);
            purge_allocator_retained();
        }
    }

    /// Red response (§5.7): distribute the excess over `SHED_TARGET_PCT`
    /// across components ∝ weight, floors respected (single pass, floor-
    /// clamped — a shortfall reappears as pressure on the next tick).
    /// Components already at/below their target get no call. Under the
    /// hard backstop the target collapses to the floor sum: every
    /// over-floor component is shed to its floor.
    fn shed_to_weights(&self, budget: u64, pressure: u64, reg: &[Arc<Component>]) {
        let floors = self.effective_floors(budget);
        let target_total = if self.backstop_on.load(Relaxed) {
            floors.iter().sum()
        } else {
            budget * SHED_TARGET_PCT / 100
        };
        let Some(mut excess) = pressure.checked_sub(target_total) else {
            return;
        };
        if excess == 0 {
            return;
        }
        // Sheddable set: above-floor components with weight > 0.
        let mut sheddable: Vec<(usize, u64, u64)> = Vec::new(); // (idx, headroom, weight)
        let mut weight_sum = 0u64;
        for (i, c) in reg.iter().enumerate() {
            let cur = (c.current)();
            let headroom = cur.saturating_sub(floors[i]);
            if headroom > 0 && c.weight > 0 {
                weight_sum += c.weight;
                sheddable.push((i, headroom, c.weight));
            }
        }
        if weight_sum == 0 {
            return;
        }
        // Largest-weight-last rounding: hand the integer remainder to the
        // final component so cuts sum exactly to the excess when headroom
        // allows.
        excess = excess.min(sheddable.iter().map(|(_, h, _)| h).sum());
        let mut remaining = excess;
        let n = sheddable.len();
        for (k, (i, headroom, weight)) in sheddable.into_iter().enumerate() {
            let share = if k + 1 == n {
                remaining
            } else {
                (excess * weight / weight_sum).min(headroom)
            };
            let share = share.min(headroom).min(remaining);
            remaining -= share;
            if share == 0 {
                continue;
            }
            let c = &reg[i];
            let cur = (c.current)();
            let target = cur.saturating_sub(share);
            c.sheds.fetch_add(1, Relaxed);
            c.shed_target_bytes.fetch_add(share, Relaxed);
            (c.shed)(target);
        }
    }

    /// Resolve the budget NOW with the sampler's exact §5.7 order (flag →
    /// env → cgroup `memory.max` × 0.8 → 70 % RAM). Mount-time consumers
    /// (the L1 transport payload-buffer cap) size against this so their
    /// footprint agrees with what the 1 Hz sampler will enforce.
    pub fn resolve_budget_now(&self) -> u64 {
        let flag = match self.flag_budget.load(Relaxed) {
            0 => None,
            v => Some(v),
        };
        let env = std::env::var("SQUEEZEFS_MEM_BUDGET_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|mb| mb * 1024 * 1024);
        resolve_budget_from(flag, env, read_cgroup_memory_max(), system_ram_bytes())
    }

    /// Production tick: resolve the budget (flag → env → cgroup × 0.8
    /// re-read NOW → 70 % RAM), sample RSS + the cgroup unreclaimable
    /// set, delegate.
    pub fn tick(&self) {
        self.tick_inner(
            self.resolve_budget_now(),
            read_rss_bytes(),
            read_cgroup_unreclaimable().unwrap_or(0),
        );
    }

    /// Serialize the registry for the stats inode: `(name, current, floor,
    /// weight, sheds)` per component.
    pub fn stats_components(&self) -> Vec<(&'static str, u64, u64, u64, u64)> {
        self.registry
            .load()
            .iter()
            .map(|c| (c.name, (c.current)(), c.floor, c.weight, c.sheds()))
            .collect()
    }
}

/// The process authority. Consumers register at mount; the sampler task
/// ticks it at 1 Hz; growth paths read [`level`].
pub static MEM_BUDGET: Lazy<MemBudget> = Lazy::new(MemBudget::new);

/// The one-atomic-load admission check every growth path uses.
#[inline]
pub fn level() -> Level {
    MEM_BUDGET.level()
}

/// The L1 transport payload-buffer cap: an eighth of the resolved memory
/// budget — the same scale-free fraction as [`ipc_arena_cap`]. The
/// FUSE-over-io_uring geometry degrades its per-queue depth from the
/// desired 32 toward the pre-L1 floor of 4 to fit under this cap
/// (`TransportGeometry` in the vendored fuse3), so small-RAM boxes keep
/// (at worst) yesterday's shipped arena footprint while the measured
/// 316k-IOPS geometry ships by default everywhere else.
///
/// There is deliberately **no absolute byte ceiling** (2026-08-04
/// derivation sweep; user directive 2026-08-02 — the former fixed 2 GiB
/// `TRANSPORT_BUFFER_CAP_CEILING` degraded depth below the measured-best
/// 32 on > 64-possible-CPU big-RAM boxes for no physical reason): the
/// pinned-arena bound is STRUCTURAL — the geometry never registers more
/// than `nqueues × Q_DEPTH_DESIRED × payload_sz` (the demand cap; depth
/// is clamped to the desired 32 by construction), so the budget fraction
/// only decides how far below the demand the depth ladder degrades.
/// `SQUEEZEFS_TRANSPORT_MEM_MAX=2048` is the A0 lever that restores the
/// retired ceiling exactly ([`resolve_transport_buffer_cap`]).
pub fn transport_buffer_cap(budget_bytes: u64) -> u64 {
    budget_bytes / 8
}

/// Transport payload-buffer cap resolution, pure (env strings in, cap
/// out — the [`resolve_ipc_arena_cap`] pattern; never panics, never
/// refuses the mount): **absolute > percentage > derived default**.
///
/// - `mem_max_mib` (`SQUEEZEFS_TRANSPORT_MEM_MAX`, MiB): explicit wins
///   verbatim (including 0 — the depth ladder then floors at 4, the
///   pre-L1 shipped posture). Garbage warns and falls through.
/// - `mem_pct` (`SQUEEZEFS_TRANSPORT_MEM_PCT`, percent of the resolved
///   budget): clamped into (0, 100]; > 100 clamps to 100 with a
///   warning; non-positive / non-finite / unparseable warns and falls
///   through (the knob-family convention — a bad env string never
///   fails a mount).
/// - Neither: [`transport_buffer_cap`] (budget/8).
pub fn resolve_transport_buffer_cap(
    budget_bytes: u64,
    mem_max_mib: Option<&str>,
    mem_pct: Option<&str>,
) -> u64 {
    if let Some(raw) = mem_max_mib {
        match raw.trim().parse::<u64>() {
            Ok(mib) => return mib.saturating_mul(1024 * 1024),
            Err(e) => {
                log::warn!(
                    "SQUEEZEFS_TRANSPORT_MEM_MAX={raw:?} is not a MiB integer ({e}) — ignored"
                )
            }
        }
    }
    if let Some(raw) = mem_pct {
        match raw.trim().parse::<f64>() {
            Ok(p) if p.is_finite() && p > 0.0 => {
                let pct = if p > 100.0 {
                    log::warn!("SQUEEZEFS_TRANSPORT_MEM_PCT={raw:?} > 100 — clamped to 100");
                    100.0
                } else {
                    p
                };
                return (budget_bytes as f64 * (pct / 100.0)) as u64;
            }
            Ok(p) => log::warn!(
                "SQUEEZEFS_TRANSPORT_MEM_PCT={raw:?} must be a percent in (0, 100] (got {p}) \
                 — ignored"
            ),
            Err(e) => {
                log::warn!("SQUEEZEFS_TRANSPORT_MEM_PCT={raw:?} is not a number ({e}) — ignored")
            }
        }
    }
    transport_buffer_cap(budget_bytes)
}

/// L4 `ipc_session_arenas` default admission fraction (design-preload-
/// interception §5.7): 12.5 % of the resolved memory budget — the same
/// budget/8 fraction the transport payload cap uses, kept because the
/// budget itself is machine-derived (§5.7 resolution order), so the
/// fraction is scale-free on every box size.
pub const IPC_ARENA_CAP_DEFAULT_PCT: f64 = 12.5;

/// The derived L4 `ipc_session_arenas` admission cap:
/// [`IPC_ARENA_CAP_DEFAULT_PCT`] of the resolved memory budget, computed
/// exactly as `budget / 8`. There is deliberately **no absolute byte
/// ceiling** (2026-08-01 user ruling — the former fixed 2 GiB
/// `IPC_ARENA_CAP_CEILING` starved big-RAM fleet clients: on the 251 GB
/// field box it clamped the pool to ~31 × 64 MiB sessions against a
/// 48-HELLO fleet, `ipc_bind_refused_budget` 13/48). The pool is already
/// bounded without a constant: the R5 budget scales the cap, per-uid
/// session caps + idle reap bound the population, and Red shedding
/// refuses new sessions under pressure — a second absolute bound
/// duplicated R5's job with a number.
pub fn ipc_arena_cap(budget_bytes: u64) -> u64 {
    budget_bytes / 8
}

/// §5.7 `ipc_session_arenas` admission-cap resolution, pure (env strings
/// in, cap out — the [`resolve_budget_from`] pattern; never panics,
/// never refuses the mount): **absolute > percentage > derived
/// default**.
///
/// - `mem_max_mib` (`SQUEEZEFS_IPC_MEM_MAX`, MiB): explicit wins
///   verbatim — the compat spelling, semantics unchanged (including 0).
///   Garbage warns and falls through.
/// - `mem_pct` (`SQUEEZEFS_IPC_MEM_PCT`, percent of the resolved
///   budget — the preferred spelling): clamped into (0, 100]. Values
///   over 100 clamp to 100 with a warning; non-positive / non-finite /
///   unparseable values warn and fall through (the IPC knob-family
///   convention — a bad env string never fails a mount).
/// - Neither: [`ipc_arena_cap`] (budget/8 =
///   [`IPC_ARENA_CAP_DEFAULT_PCT`]).
pub fn resolve_ipc_arena_cap(
    budget_bytes: u64,
    mem_max_mib: Option<&str>,
    mem_pct: Option<&str>,
) -> u64 {
    if let Some(raw) = mem_max_mib {
        match raw.trim().parse::<u64>() {
            Ok(mib) => return mib.saturating_mul(1024 * 1024),
            Err(e) => {
                log::warn!("SQUEEZEFS_IPC_MEM_MAX={raw:?} is not a MiB integer ({e}) — ignored")
            }
        }
    }
    if let Some(raw) = mem_pct {
        match raw.trim().parse::<f64>() {
            Ok(p) if p.is_finite() && p > 0.0 => {
                let pct = if p > 100.0 {
                    log::warn!("SQUEEZEFS_IPC_MEM_PCT={raw:?} > 100 — clamped to 100");
                    100.0
                } else {
                    p
                };
                return (budget_bytes as f64 * (pct / 100.0)) as u64;
            }
            Ok(p) => log::warn!(
                "SQUEEZEFS_IPC_MEM_PCT={raw:?} must be a percent in (0, 100] (got {p}) — ignored"
            ),
            Err(e) => {
                log::warn!("SQUEEZEFS_IPC_MEM_PCT={raw:?} is not a number ({e}) — ignored")
            }
        }
    }
    ipc_arena_cap(budget_bytes)
}

/// The shipped per-session IPC arena size — the derived default's FLOOR
/// (never-regress-below-shipped, the `Q_DEPTH_FLOOR` house law: every
/// box ran 64 MiB sessions before the 2026-08-04 derivation sweep).
pub const IPC_ARENA_FLOOR_BYTES: u64 = 64 * 1024 * 1024;

/// The derived arena's alignment: the SLOT-SLAB DMA law — `slots (1024,
/// `Geometry::default_v1`) × the O_DIRECT LBA (4 KiB, `ipc_direct`'s
/// screen)` = 4 MiB, tie-tested against the geometry in
/// `tests/derivation_sweep_tests.rs`. Every 4 MiB multiple is also a
/// PMD (2 MiB) multiple, so the arena-THP collapse law
/// (`map_shared_pmd_aligned`) holds unchanged.
///
/// Why not PMD: the client slab is `arena / slots`; an ODD 2 MiB-multiple
/// arena makes it `≡ 2048 (mod 4096)`, so every odd slot's `slot × slab`
/// arena offset fails `ipc_direct`'s 4 KiB DMA screen — EXACTLY half of
/// all round-robin direct-drive reads bounce through the pooled-copy
/// path (the 2026-08-04 cluster randread-shim −15.4 %; bounce rate
/// 50.006 % measured where the acceptance records expect 0). The 64 MiB
/// floor masked this on small-budget boxes; the big-RAM fleet derived
/// odd multiples.
pub const IPC_ARENA_DMA_ALIGN_BYTES: u64 = 4 * 1024 * 1024;

/// Per-session IPC arena default resolution, pure (2026-08-04 derivation
/// sweep): `SQUEEZEFS_IPC_ARENA_MB` explicit (MiB, > 0) wins verbatim
/// (an odd explicit value stays DMA-eligible through the client-side
/// slab law — `Geometry::slot_slab` floors to the LBA); otherwise
/// `max(64 MiB, dma_align_down(admission_cap / 128))` — cap/128 is the
/// per-uid session cap (64) × 2 safety margin, so even a full per-uid
/// population of default-size arenas fits in HALF the admission cap;
/// the round-down never exceeds the cap fraction. Garbage warns and
/// falls through (knob-family convention).
pub fn resolve_ipc_arena_bytes(arena_mb_env: Option<&str>, arena_cap_bytes: u64) -> u64 {
    if let Some(raw) = arena_mb_env {
        match raw.trim().parse::<u64>() {
            Ok(mib) if mib > 0 => return mib.saturating_mul(1024 * 1024),
            Ok(_) => log::warn!("SQUEEZEFS_IPC_ARENA_MB must be > 0 — ignored"),
            Err(e) => {
                log::warn!("SQUEEZEFS_IPC_ARENA_MB={raw:?} is not a MiB integer ({e}) — ignored")
            }
        }
    }
    (arena_cap_bytes / 128 / IPC_ARENA_DMA_ALIGN_BYTES * IPC_ARENA_DMA_ALIGN_BYTES)
        .max(IPC_ARENA_FLOOR_BYTES)
}

/// Register the L4 `ipc_session_arenas` component (design-preload-
/// interception §5.7): floor 0, non-reclaimable (anon shm), shed =
/// refuse-new-sessions until the gauge is back under target + reap-idle
/// (PR L4-6) — never tearing live sessions (the R5 never-lossy
/// discipline; live arenas are bounded by admission).
pub fn register_ipc_session_arena_component(
    mb: &MemBudget,
    current: Arc<dyn Fn() -> u64 + Send + Sync>,
    shed: Arc<dyn Fn(u64) + Send + Sync>,
) {
    mb.register(Component::new("ipc_session_arenas", 0, 1, current, shed));
}

/// RES-4 (pre-RC engineering spec §7): register the two LRU eviction
/// channels that actually park payloads.
///
/// `LruCache` bounds each channel at `EVICT_CHANNEL_BYTE_BOUND` and
/// gauges it, but the bytes were never in the registry — up to 512 MiB
/// of live multi-MiB `Bytes` outside the authority, which is the exact
/// shape of the cage-OOM class the bound was added for. Only caches that
/// ARMED a dehydration receiver park anything (`write_lru` never takes
/// one, so its victims drop at the source and it has no channel to
/// register).
///
/// Floor 0 (warmth is the cheapest sacrifice), weight 1, and a real shed:
/// the closure clamps SEND-side admission — a channel's parked messages
/// belong to the worker draining them and are never torn (never-lossy),
/// but new victims stop parking until the gauge is back under target.
pub fn register_lru_evict_channel_components(
    mb: &MemBudget,
    read_lru: &crate::cache::lru::LruCache,
    hot_block: &crate::cache::lru::LruCache,
) {
    for (name, cache) in [
        ("read_lru_evict_channel", read_lru),
        ("hot_block_evict_channel", hot_block),
    ] {
        let gauge = cache.clone();
        let shed = cache.clone();
        mb.register(Component::new(
            name,
            0,
            1,
            Arc::new(move || gauge.evict_channel_bytes()),
            Arc::new(move |target| shed.shed_evict_channel(target)),
        ));
    }
}

/// One-atomic-load disk-tier publish gate (the finding-#2 escalation) —
/// read by [`crate::cache::NvmeStaging::cache_read_block`], the single
/// funnel every tier producer (fill path, dehydration) routes
/// through.
#[inline]
pub fn tier_publish_paused() -> bool {
    MEM_BUDGET.tier_publish_paused()
}

/// §5.7 budget resolution order, pure: flag → env → cgroup `memory.max`
/// × 0.8 → 70 % of system RAM.
pub fn resolve_budget_from(
    flag: Option<u64>,
    env: Option<u64>,
    cgroup_max: Option<u64>,
    ram: u64,
) -> u64 {
    if let Some(f) = flag {
        return f;
    }
    if let Some(e) = env {
        return e;
    }
    if let Some(c) = cgroup_max {
        return c / 10 * 8;
    }
    ram / 10 * 7
}

/// Red halves the parked-buffer spill threshold (§5.7 Red row — the early
/// `flush_memory_buffers_*` knob): the existing never-lossy staging spill
/// just engages at half the count.
#[inline]
pub fn effective_parked_cap(cap: usize, level: Level) -> usize {
    match level {
        Level::Red => (cap / 2).max(1),
        _ => cap,
    }
}

/// cgroup v2 `memory.max` for THIS process — re-read every tick so a
/// runtime-lowered cage tightens the budget within a second (§5.7).
fn read_cgroup_memory_max() -> Option<u64> {
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    // v2 unified line: `0::<path>`.
    let path = cg.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let max = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.max")).ok()?;
    let max = max.trim();
    if max == "max" {
        return None;
    }
    max.parse::<u64>().ok()
}

/// The kill-relevant subset of cgroup-v2 `memory.stat`: bytes the kernel
/// cannot reclaim without I/O or at all — `anon` (the OOM killer's prey)
/// + `file_dirty` + `file_writeback` (unreclaimable until writeback
/// completes) + `shmem` + `unevictable` + `slab_unreclaimable`. Clean
/// file cache is deliberately EXCLUDED: it is dropped, not killed over
/// (counting it would pin healthy warm mounts — 5 GiB of clean tier mmap
/// by design — in permanent Red).
pub fn parse_unreclaimable_memory_stat(stat: &str) -> u64 {
    let mut sum = 0u64;
    for line in stat.lines() {
        let mut it = line.split_whitespace();
        let (Some(key), Some(val)) = (it.next(), it.next()) else {
            continue;
        };
        if matches!(
            key,
            "anon"
                | "file_dirty"
                | "file_writeback"
                | "shmem"
                | "unevictable"
                | "slab_unreclaimable"
        ) {
            sum += val.parse::<u64>().unwrap_or(0);
        }
    }
    sum
}

/// cgroup v2 `memory.stat` unreclaimable sample for THIS process — one
/// small file read per tick, same cost class as the `memory.max` re-read.
/// `None` off-cgroup (the arm disengages; statm + gauges remain).
fn read_cgroup_unreclaimable() -> Option<u64> {
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cg.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let stat = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.stat")).ok()?;
    Some(parse_unreclaimable_memory_stat(&stat))
}

/// `/proc/self/statm` resident pages × page size.
fn read_rss_bytes() -> u64 {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let mut it = statm.split_whitespace();
    let _size = it.next();
    let resident: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    resident * page_size()
}

fn page_size() -> u64 {
    // SAFETY: sysconf(_SC_PAGESIZE) has no failure mode that matters here;
    // a -1 falls back to 4096.
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 {
        ps as u64
    } else {
        4096
    }
}

fn system_ram_bytes() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

/// Red-tick allocator purge: force jemalloc to return retained dirty
/// pages to the OS NOW (`arena.<all>.purge` via mallctl). The QUICK
/// cage-OOM residual was exactly this shape — after the component sheds,
/// the daemon sat at ~5 GiB anon with every registered gauge at ~130 MiB
/// and NO VMA over 200 MiB: fragmented allocator-retained pages from
/// metadata-node churn (11 M+ node-cache hits), which the 1 s decay only
/// trickles back while the cage kills in one burst. Purging is the
/// allocator-side twin of the pool trim; a no-op off-Linux/dhat. This is
/// NOT the OQ #4 jemalloc *watch* (still out — the RSS sampler covers
/// detection); it is a shed lever, fired only in Red.
fn purge_allocator_retained() {
    #[cfg(all(target_os = "linux", not(feature = "dhat-on")))]
    {
        // SAFETY: mallctl with a null oldp/newp and a static name string is
        // the documented no-argument command form; "arena.4096.purge"
        // (MALLCTL_ARENAS_ALL = 4096) purges every arena.
        unsafe {
            let name = b"arena.4096.purge\0";
            let _ = tikv_jemalloc_sys::mallctl(
                name.as_ptr() as *const _,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            );
        }
    }
}

/// Spawn the 1 Hz sampler (mount-time; one task per process — idempotent
/// via a static guard).
pub fn spawn_sampler() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Relaxed) {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            tick_off_thread().await;
        }
    });
}

/// RES-11 (pre-RC engineering spec §7): run one [`MemBudget::tick`] OFF
/// the async workers.
///
/// The tick reads procfs (`/proc/self/statm`, the cgroup memory files),
/// calls every registered gauge closure, and — on a Red tick — runs
/// `arena.4096.purge`, a full jemalloc all-arena purge. That is tens of
/// milliseconds of uninterruptible syscall-heavy work, once per second,
/// and the pre-fix sampler ran it on a tokio worker *precisely when the
/// daemon is under memory pressure* and every handler sharing that
/// worker is the thing being measured.
///
/// Awaited, so ticks never overlap: the hysteresis ladder, the backstop
/// sustain count and the shed pass are all single-tick state machines.
pub async fn tick_off_thread() {
    let _ = tokio::task::spawn_blocking(|| MEM_BUDGET.tick()).await;
}
