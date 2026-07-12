//! R5 — the joint memory authority (docs/design-read-path.md §5.7).
//!
//! One budget, one pressure signal, three levels. The daemon's RAM
//! consumers (parked write buffers, staging/tier mmaps, the RAM LRUs, the
//! hot-block tier, the prefetch in-flight gauge, the buffer pools) each
//! register a `Component` — a gauge closure, a floor, a weight, and a shed
//! closure — into a latch-free `ArcSwap` registry. A 1 Hz sampler
//! resolves the budget (flag → env → cgroup `memory.max` × 0.8 re-read
//! every tick → 70 % RAM), samples `/proc/self/statm` RSS into a 5-slot
//! decaying window (max-of-window, explicitly NOT a ratchet), computes
//! `pressure = max(Σ gauges, windowed_rss_max)`, and drives the level
//! machine with hysteresis. Red ticks shed to weights over floors through
//! the components' own never-lossy mechanisms.
//!
//! Enforcement is ADVISORY-AT-ADMISSION: growth paths call [`level`] —
//! one relaxed atomic load, no lock, nothing blocking — and apply their
//! own documented response (§5.7 table: Yellow stops growth + pauses
//! dehydration entirely; Red sheds, halves the parked-buffer cap, stops
//! prefetch issue). Write-side durability is untouched: shedding parked
//! buffers means flushing them through the existing durable paths sooner,
//! never dropping them.
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
            sheds: AtomicU64::new(0),
            shed_target_bytes: AtomicU64::new(0),
        }
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
    level: AtomicU8,
    yellow_events: AtomicU64,
    red_events: AtomicU64,
    floors_clamped: AtomicBool,
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
            level: AtomicU8::new(Level::Green as u8),
            yellow_events: AtomicU64::new(0),
            red_events: AtomicU64::new(0),
            floors_clamped: AtomicBool::new(false),
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

    /// Test seam: pin the process level (integration phases drive the
    /// advisory checks without a live sampler).
    pub fn force_level_for_test(&self, l: Level) {
        self.level.store(l as u8, Relaxed);
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
    /// [`Self::tick`] resolves budget + RSS and delegates here).
    pub fn tick_inner(&self, budget: u64, rss_sample: u64) {
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

        let reg = self.registry.load();
        let gauge_sum: u64 = reg.iter().map(|c| (c.current)()).sum();
        self.gauge_sum.store(gauge_sum, Relaxed);
        let pressure = gauge_sum.max(rss_max);
        self.pressure.store(pressure, Relaxed);

        if budget == 0 {
            self.level.store(Level::Green as u8, Relaxed);
            return;
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
        }
    }

    /// Red response (§5.7): distribute the excess over `SHED_TARGET_PCT`
    /// across components ∝ weight, floors respected (single pass, floor-
    /// clamped — a shortfall reappears as pressure on the next tick).
    /// Components already at/below their target get no call.
    fn shed_to_weights(&self, budget: u64, pressure: u64, reg: &[Arc<Component>]) {
        let target_total = budget * SHED_TARGET_PCT / 100;
        let Some(mut excess) = pressure.checked_sub(target_total) else {
            return;
        };
        if excess == 0 {
            return;
        }
        let floors = self.effective_floors(budget);
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

    /// Production tick: resolve the budget (flag → env → cgroup × 0.8
    /// re-read NOW → 70 % RAM), sample RSS, delegate.
    pub fn tick(&self) {
        let flag = match self.flag_budget.load(Relaxed) {
            0 => None,
            v => Some(v),
        };
        let env = std::env::var("SQUEEZEFS_MEM_BUDGET_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|mb| mb * 1024 * 1024);
        let budget = resolve_budget_from(flag, env, read_cgroup_memory_max(), system_ram_bytes());
        self.tick_inner(budget, read_rss_bytes());
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
            MEM_BUDGET.tick();
        }
    });
}
