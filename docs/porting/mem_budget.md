# R5 memory-budget authority — porting spec (`src/mem_budget.rs`)

Source of truth: `/Users/joberdorf/Source/squeezefs/src/mem_budget.rs` (1251 lines; "R5", design-read-path.md §5.7).
Tests: `tests/mem_budget_tests.rs` (1205), `tests/mem_budget_sampler_venue_tests.rs` (83), mem_budget tie tests in `tests/derivation_sweep_tests.rs`.
Line refs below are `mem_budget.rs:N` unless prefixed.

One budget, one pressure signal, three levels. RAM consumers register a `Component` {gauge closure, floor, weight, shed closure}
into a lock-free registry; a 1 Hz sampler resolves the budget, samples RSS + cgroup-v2 unreclaimable bytes into two 5-slot
max-of-window buffers, computes `pressure = max(Σ non-reclaimable gauges, rss_window_max, unreclaim_window_max)`, runs a
hysteresis level machine, and on Red ticks sheds ∝ weight over floors. Enforcement elsewhere is advisory-at-admission:
growth paths call `level()` (one relaxed atomic load) and apply their own response (`:19-25`).

---

## 1. Budget derivation

### 1.1 Resolution order (pure form `:953-969`)
```
resolve_budget_from(flag, env, cgroup_max, ram):
    if flag       -> flag                 // --mem-budget, verbatim
    if env        -> env                  // SQUEEZEFS_MEM_BUDGET_MB × 1 MiB, verbatim
    if cgroup_max -> cgroup_max / 10 * 8  // integer: divide first, then × 8 (NOT ×0.8 float)
    else          -> ram / 10 * 7         // 70 % RAM, same integer shape
```
Flag/env are "explicit tiers"; cgroup/RAM are the "derived tier".

### 1.2 Fleet share (KD-MW-14, `:978-980`, `:1005-1018`)
```
fleet_shared_root(value, share) = ceil(value / max(share,1))      // div_ceil; share ≤ 1 is identity; no overflow
resolve_budget_from_shared(flag, env, cgroup_max, ram, share) =
    resolve_budget_from(flag, env, cgroup_max.map(|c| fleet_shared_root(c, share)), fleet_shared_root(ram, share))
```
Division is applied to the SYSTEM roots (cgroup cage, RAM) BEFORE the ×0.8/×0.7 laws; flag/env pass through undivided.
`share` comes from `crate::cpu::fleet_share()` = `SQUEEZEFS_FLEET_SHARE` (int, default 1, `.max(1)`, cached once per process, `cpu.rs:44-47`).

### 1.3 Live resolution (`:541-557`)
```
MemBudget::resolve_budget_now():
    flag = flag_budget.load(Relaxed); 0 => None
    env  = getenv("SQUEEZEFS_MEM_BUDGET_MB").trim().parse::<u64>().ok().map(mb * 1024*1024)   // garbage => None (falls through)
    resolve_budget_from_shared(flag, env, read_cgroup_memory_max(), system_ram_bytes(), fleet_share())
```
Called EVERY tick (`:578-579`) so a runtime-lowered cgroup cage tightens the budget within one tick; also called by
mount-time sizers (`fuse_client.rs:26633`, `config_ops.rs:2887`).

`--mem-budget` flag: `main.rs:3301-3307` parses with `parse_size_string(s, shared_system_ram_bytes())` (accepts `6G`, `512MB`,
`50%` — a percent resolves against the FLEET-SHARED RAM), then `MEM_BUDGET.set_flag_budget(bytes)`; parse failure = mount bootstrap failure.

### 1.4 cgroup reading — **v2 ONLY** (`:1112-1122`, `:1156-1161`)
```
read_cgroup_memory_max() -> Option<u64>:
    cg   = read_to_string("/proc/self/cgroup")?                          // any io error => None
    path = first line with prefix "0::" , stripped, trimmed              // the v2 unified line; no "0::" line => None
    max  = read_to_string("/sys/fs/cgroup{path}/memory.max")?.trim()
    if max == "max" => None                                              // uncapped cage == no cgroup tier => fall to 70 % RAM
    max.parse::<u64>().ok()                                              // unparsable => None
```
There is NO cgroup-v1 support (no `memory.limit_in_bytes`, no `/proc/self/mountinfo` walk). Off-cgroup / macOS => `None` => RAM tier.

### 1.5 System RAM (`:1185-1189`)
`sysinfo::System::new(); refresh_memory(); total_memory()` — total physical bytes. Zig: `sysconf(_SC_PHYS_PAGES) × pagesize` on Linux / `hw.memsize` on macOS is equivalent.

### 1.6 What the sampler measures per tick (`:577-585`)
| Arm | Source | Function | Semantics |
|---|---|---|---|
| RSS | `/proc/self/statm` field 2 (resident pages) × `sysconf(_SC_PAGESIZE)` (fallback 4096 if ≤ 0) | `read_rss_bytes` `:1164-1172`, `page_size` `:1174-1183` | read failure => 0. Per-process, NEVER divided by fleet share |
| Unreclaimable | `/sys/fs/cgroup{path}/memory.stat` (same `0::` path) | `read_cgroup_unreclaimable` `:1156` → `parse_unreclaimable_memory_stat` `:1131-1151` | `None` off-cgroup → `unwrap_or(0)`; divided by fleet share at the root |
| Gauges | every registered `Component.current()` | in `tick_inner` | logical bytes |

`parse_unreclaimable_memory_stat(stat: &str) -> u64`: for each line, split on whitespace, take `(key, val)`; if key ∈
{`anon`, `file_dirty`, `file_writeback`, `shmem`, `unevictable`, `slab_unreclaimable`} add `val.parse::<u64>().unwrap_or(0)`.
Lines without two fields are skipped; unknown keys ignored; empty input => 0. Clean file cache is DELIBERATELY excluded
("counting it would pin healthy warm mounts — 5 GiB of clean tier mmap by design — in permanent Red", `:1127-1130`).

Input composition (pure, `:996-998`):
```
tick_inputs(budget, rss, raw_unreclaimable, share) -> (budget, rss, fleet_shared_root(raw_unreclaimable, share))
```
Why: N co-located daemons read ONE `memory.stat`; comparing the cage's collective residue against a 1/N budget over-fires
Red/backstop ~N× ("the rung-7 S6-a N=32 row caught it live: all 32 daemons in Red with hard backstops at 0.5 GB of own ledger each", `:992-993`).

**Divergence (Zig, 2026-10-24, decision 0002 row 556; `rung-10-idle-rss.md`): the arm reads the cage only when the cage is the
fleet's.** The census that counts the share also counts the cage's FOREIGN live processes — neither this process's comm nor a
supervisor's (`sqz-supervise`). With none (a systemd service: the daemon and its supervisor; a pod of daemons) the arm reads
`memory.stat` as above; with any (a desktop session's scope, a CI runner's, the container's root cgroup) the sample is 0 — the
cage's set is THEIRS, and the daemon answers for its own: its gauges and its RSS (`statm`, kept over PSS: the two differ by the
shared text, ≤ 14 MiB, and PSS walks every VMA). An unreadable census is off too. The cage's `memory.max`/`memory.high` still bound
the budget in both cases. The edge is one info line.

---

## 2. Component model

### 2.1 Struct (`:111-134`)
```rust
pub struct Component {
    pub name: &'static str,
    pub current: Arc<dyn Fn() -> u64 + Send + Sync>,  // logical bytes now
    pub floor: u64,       // bytes it may always keep (clamped proportionally when Σ floors > 90 % budget)
    pub weight: u64,      // Red-shed share; 0 => never shed
    pub shed: Arc<dyn Fn(u64 /*target bytes*/) + Send + Sync>,  // never-lossy mechanism (early flush, drop probation, trim)
    kernel_reclaimable: bool,          // private; builder-set; attribution-only, excluded from pressure
    sheds: AtomicU64,                  // shed calls received
    shed_target_bytes: AtomicU64,      // Σ bytes requested (internal only; not exported)
}
Component::new(name, floor, weight, current, shed) -> Self      // kernel_reclaimable=false, counters 0  (:137-154)
Component::kernel_reclaimable(self) -> Self                      // builder flag                            (:158-161)
Component::sheds(&self) -> u64                                   // Relaxed load                           (:163-165)
```
Zig shape: gauge/shed are `(ctx: *anyopaque, fn)` pairs or a vtable; they must be callable from the sampler thread without locks.

### 2.2 Registration (`:231-238`)
`MemBudget::register(&self, c: Component)` wraps in `Arc`, then `registry.rcu(|cur| cur.clone() + push(c))` — copy-on-write
append on an `ArcSwap<Vec<Arc<Component>>>`. Readers do one guarded `load()`; registration is mount-time, append-only,
never removed. The production registration block is guarded by a `static REGISTERED: AtomicBool` so in-process remounts do
not duplicate (`fuse_client.rs:26644-26646`); some modules register via `Once`/`Lazy` (`dlm.rs:548`, `op_trace.rs:144`).

### 2.3 Sheddable vs non-sheddable
A component takes part in a shed only if `weight > 0` AND `current() > effective_floor` (`:497-503`). Non-sheddable
(`weight 0`, no-op shed) components are attribution-only: `write_pipeline_inflight`, `read_lane_inflight` (in-flight DMAs drain by
completion — weight 1 but no-op shed), `transport_payload_buffers` (0), `staging_mmap` (0, kernel_reclaimable),
`read_tier_mmap` (0, kernel_reclaimable), `dlm_grant_table_bytes` (0, "admission refuses instead"), `op_trace_rings` (1, no-op).

### 2.4 Floor law (`:310-326`)
```
effective_floors(&self, budget) -> Vec<u64>:           // index-aligned with the registry
    sum = Σ floor; cap = budget * 90 / 100
    if sum <= cap || sum == 0: floors_clamped.store(false); return floors verbatim
    if !floors_clamped.swap(true): log::warn!("mem_budget: component floors sum to {sum} B > 90% of the {budget} B budget
         — floors proportionally clamped (raise --mem-budget or lower component floors)")
    return floor_i * cap / sum  for each       // integer, proportional, NOT truncation-ordered
```
Violation is reported by (a) the `floors_clamped` flag (stats `mem_budget_floors_clamped`) and (b) ONE warn line on the
false→true edge. It is NEVER a mount failure. Recomputed on every shed pass (and by callers), so the flag tracks the live budget.
With `budget == 0`, `cap == 0`, so any non-zero Σ floors clamps everything to 0.

### 2.5 Production registry (for parity; `fuse_client.rs:26663-26914` unless noted)
| name | floor | weight | gauge | shed |
|---|---|---|---|---|
| parked_write_buffers | 32×4 MiB = 128 MiB | 4 | `METRICS.parked_full_buffer_bytes` | `drain_target.fetch_min(target)` + notify drain worker |
| write_pipeline_inflight | 0 | 1 | pipeline inflight bytes | no-op (drains by completion) |
| parked_extent_bytes | 0 | 2 | `METRICS.parked_extent_bytes` | same drain plumbing |
| hot_block_tier | 64 MiB | 4 | `hot_block.current_bytes()` | `shed_to(target)` |
| read_lane_hold | 0 | 2 | `hold.bytes()` | `trim_to(target)` (oldest-first) |
| read_lane_inflight | 0 | 1 | inflight bytes | no-op |
| read_lru / write_lru | 32 MiB each | 2 | `current_bytes()` | `shed_to(target)` |
| read_lru_evict_channel / hot_block_evict_channel (`:921-940`) | 0 | 1 | `evict_channel_bytes()` | `shed_evict_channel(target)` (send-side clamp) |
| prefetch_inflight | 0 | 1 | `METRICS.prefetch_inflight_bytes` | `lanes.invalidate_all()` (ignores target) |
| staging_mmap | 0 | 0 | staged write bytes | no-op; `.kernel_reclaimable()` |
| read_tier_mmap | 0 | 0 | read tier bytes | no-op; `.kernel_reclaimable()` |
| metadata_cache | 0 | 1 | `entry_count() × 1024` | `invalidate_all()` |
| kv_node_cache | 64 MiB | 1 | Σ node_cache_gauge | `kick_checkpoint()` per volume |
| transport_payload_buffers (linux) | 0 | 0 | uring geometry bytes | no-op |
| block_map_window | 0 | 2 | `METRICS.block_map_window_bytes` | `kvmap_windows_shed(target)` |
| buffer_pool / aligned_buf_pool / zc_fill_pool | 16×4 MiB = 64 MiB | 2 | `allocated_bytes()` | `trim_to(target)` |
| ranged_buf_pool | 4 MiB | 2 | `allocated_bytes()` | `trim_to(target)` |
| ipc_session_arenas (`:898-904`) | 0 | 1 | caller | caller (refuse new sessions + reap idle) |
| dlm_grant_table_bytes (`dlm.rs:548`) | 0 | 0 | grant table | no-op |
| dlm_token_cache_bytes, dlm_delegation_entries, dlm_token_records_bytes, zcrx_area, op_trace_rings | 0 | 1 | various | clear / no-op |

Σ production floors ≈ 128+64+32+32+64+64+64+64+4 (+64 zc) MiB ≈ 516–580 MiB ⇒ clamp engages below ≈ 575–645 MiB budgets.

---

## 3. Pressure, windows, level machine, escalations

### 3.1 `tick_inner(budget, rss_sample, unreclaimable_sample)` — exact order (`:331-473`)
1. `budget.store(budget)`.
2. RSS window: `idx = rss_idx.fetch_add(1) % 5; rss_window[idx] = rss_sample; rss_max = max(window)`. Slots start at 0.
3. Unreclaimable window: identical with `unreclaim_idx/unreclaim_window`; `unreclaimable.store(unreclaim_max)`.
   Windows are **max over the last 5 samples** — a decaying window, "explicitly NOT ratchets" (`:13`): a spike ages out on the
   5th subsequent tick.
4. `reg = registry.load()`; for each c: `cur = c.current()`; `gauge_sum += cur`; `if !c.kernel_reclaimable { pressure_gauges += cur }`.
   `gauge_sum.store(gauge_sum)` (attribution surface, full sum).
5. `pressure = max(pressure_gauges, rss_max, unreclaim_max)`; `pressure.store`.
6. **budget == 0 short-circuit** (`:373-379`): level := Green, tier_paused := false, backstop_over_ticks := 0, backstop_on := false; RETURN
   (no events, no shed). Level/pressure/gauge_sum are still published.
7. **Tier-publish pause** on the unreclaimable arm ALONE (`:387-397`):
   `upct = unreclaim_max.saturating_mul(100) / budget`;
   `if upct >= 95 { if !tier_paused.swap(true) { warn!("mem_budget: unreclaimable {u} B >= 95% of the {b} B budget — disk-tier publishes paused") } }`
   `else if upct < 91 && tier_paused.swap(false) { info!("mem_budget: unreclaimable pressure receded — disk-tier publishes resume") }`
   (91 ≤ upct < 95: hold.) Gauge- or RSS-driven Red must NOT pause publishes (`:381-386`).
8. **Hard backstop** (`:404-419`):
   ```
   if unreclaim_max >= budget:                      // ≥ 100 %, on the WINDOWED max
       over = backstop_over_ticks.fetch_add(1) + 1
       if over >= 3 && !backstop_on.swap(true):     // entry edge only
           hard_backstops += 1
           warn!("mem_budget: HARD BACKSTOP — unreclaimable {u} B >= the full {b} B budget for {over} consecutive ticks;
                  Red sheds escalate to component floors until the pressure recedes")
   else:
       backstop_over_ticks.store(0)                 // any sub-budget tick resets the run
       if upct < 91 && backstop_on.swap(false): info!("mem_budget: hard backstop released")
   ```
   Note: because the window holds the max for 5 ticks, `unreclaim_max >= budget` persists ≥ 5 ticks after a single over-budget
   sample, so one over-budget SAMPLE followed by low samples still trips the backstop on the 3rd tick. The tests confirm this
   (`hard_backstop_escalates_to_floors_after_sustained_overage` feeds 1100 three times; `red_convergence_under_admission_storm` relies on the window).
   Release requires `upct < 91` (window decayed), not merely `< 100`. The backstop is NOT reset by level changes.
9. **Level machine** (`:421-467`), `pct = pressure.saturating_mul(100) / budget`:
   ```
   Green : pct>=95 -> Red ; pct>=80 -> Yellow ; else Green
   Yellow: pct>=95 -> Red ; pct<76  -> Green  ; else Yellow
   Red   : pct>=91 -> Red ; pct>=76 -> Yellow ; else Green         // Red can drop straight to Green
   ```
   Events on transition only: Green→Yellow: `yellow_events += 1`. →Red: `red_events += 1`, and if prev == Green ALSO
   `yellow_events += 1` ("Entering Red from Green counts both edges", `:458`). Yellow→Red: red only. Any downward move: nothing.
   `level.store(next)`.
10. `if next == Red { shed_to_weights(budget, pressure, &reg); purge_allocator_retained(); }` — EVERY Red tick, not just the entry edge.

### 3.2 Level semantics (§5.7 table, design doc `:402-406`) — the responses live at the consumers, not in this module
| Level | Band | Response (advisory, consumers read `level()`) |
|---|---|---|
| Green | < 80 % | none |
| Yellow | enter ≥ 80, exit < 76 | stop growth: prefetch window growth frozen; hot-tier inserts evict-first; dehydration paused entirely (`cache/mod.rs:249,330`, counter `mem_budget_dehydrate_paused`); `lru.rs:171,315` freeze |
| Red | enter ≥ 95, exit < 91 | shed-to-weights pass each tick (this module); prefetch plans cleared; parked-buffer cap halves and becomes a hard bound (writers await drain → self-flush; `fuse_client.rs:24089,24320`); write pipeline/read lane depth clamp to floor; IPC refuses new sessions; fleet/zcrx jobs pause |

`effective_parked_cap(cap, level)` (`:1103-1108`): `Red => max(cap/2, 1)`, else `cap`.

### 3.3 Shed algorithm `shed_to_weights(budget, pressure, reg)` (`:481-532`) — exact
```
floors = effective_floors(budget)
target_total = backstop_on ? Σ floors : budget * 85 / 100
excess = pressure - target_total; if underflow or 0 -> return
sheddable = [(i, headroom = cur_i - floors[i] (saturating), weight_i)  for i where headroom > 0 && weight_i > 0]  // re-reads current()
weight_sum = Σ weight over sheddable; if 0 -> return
excess = min(excess, Σ headroom)                       // never ask for more than exists over floors
remaining = excess; n = len(sheddable)
for k, (i, headroom, weight) in enumerate(sheddable):
    share = (k+1 == n) ? remaining : min(excess * weight / weight_sum, headroom)
    share = min(share, headroom, remaining)
    remaining -= share
    if share == 0: continue                              // at-target components get NO call
    cur = c.current()                                    // re-read again
    target = cur.saturating_sub(share)
    c.sheds += 1; c.shed_target_bytes += share
    (c.shed)(target)                                     // callback invoked with no lock held
```
Properties the tests pin: single pass; floor-clamped; last sheddable component absorbs the integer remainder (so cuts sum
exactly to the excess when headroom allows); a component at/below its floor is never called; Yellow never sheds; shortfall
reappears as pressure next tick. Note `pressure` may be RSS/unreclaimable-driven while gauges are small: the excess is then
capped by Σ headroom and distributed anyway.

### 3.4 Allocator purge (`:1201-1218`)
`purge_allocator_retained()`: Linux && !dhat ⇒ `mallctl("arena.4096.purge")` (MALLCTL_ARENAS_ALL = 4096) — jemalloc returns
retained dirty pages NOW. Fired only on Red ticks. Why: "after the component sheds, the daemon sat at ~5 GiB anon with every
registered gauge at ~130 MiB and NO VMA over 200 MiB: fragmented allocator-retained pages from metadata-node churn (11 M+
node-cache hits)" (`:1193-1197`). Zig: whatever the chosen allocator's "release free pages to OS" call is; no-op elsewhere.

### 3.5 Tick cadence (`:1222-1251`)
```
spawn_sampler():  static STARTED: AtomicBool; if swap(true) return         // idempotent, one task per process
    spawn_meta("mem_budget_sampler", async { let t = interval(1 s); loop { t.tick().await; tick_off_thread().await } })
tick_off_thread(): run_blocking(|| MEM_BUDGET.tick()).await               // on the blocking pool, NOT an async worker
```
`interval(1 s).tick()` is `sleep(period)` — the next tick starts 1 s after the previous tick RETURNED (first tick after one
full second; no immediate fire; no catch-up bursts; `sqz_time.rs:282-299`). Ticks never overlap because the loop awaits the
blocking job. The "1 Hz" is nominal: real period = 1 s + tick duration. Why off-thread (RES-11, `:1236-1248`): the tick does
procfs reads + every gauge closure + the all-arena purge ("tens of milliseconds of uninterruptible syscall-heavy work"), and
ran on a tokio worker precisely under memory pressure.

---

## 4. Public API (Rust pseudocode; mirror the shape in Zig)

```rust
#[repr(u8)] pub enum Level { Green = 0, Yellow = 1, Red = 2 }   // Ord: Green < Yellow < Red; from_u8: 2→Red, 1→Yellow, _→Green (:77-85)

pub const YELLOW_ENTER_PCT: u64 = 80;  pub const YELLOW_EXIT_PCT: u64 = 76;
pub const RED_ENTER_PCT: u64 = 95;     pub const RED_EXIT_PCT: u64 = 91;
pub const BACKSTOP_SUSTAIN_TICKS: u64 = 3;
pub const IPC_ARENA_CAP_DEFAULT_PCT: f64 = 12.5;
pub const IPC_ARENA_FLOOR_BYTES: u64 = 64 MiB;  pub const IPC_ARENA_DMA_ALIGN_BYTES: u64 = 4 MiB;  pub const EXA_CANON_QD: u64 = 8;

pub struct Component { .. }                                   // §2.1
impl Component { new(name, floor, weight, current, shed); kernel_reclaimable(self) -> Self; sheds(&self) -> u64 }

pub struct MemBudget { .. }                                   // fields §8
impl MemBudget {
    pub fn new_for_test() -> Self;                            // private instance, all zero, level Green
    pub fn register(&self, c: Component);                     // rcu append
    pub fn set_flag_budget(&self, bytes: u64);                // 0 = unset
    pub fn level(&self) -> Level;                             // relaxed load
    pub fn budget_bytes(&self) -> u64;   pub fn pressure_bytes(&self) -> u64;   pub fn gauge_sum_bytes(&self) -> u64;
    pub fn yellow_events(&self) -> u64;  pub fn red_events(&self) -> u64;       pub fn floors_clamped(&self) -> bool;
    pub fn unreclaimable_bytes(&self) -> u64;                 // last windowed max
    pub fn hard_backstops(&self) -> u64; pub fn backstop_active(&self) -> bool; pub fn tier_publish_paused(&self) -> bool;
    pub fn force_level_for_test(&self, l: Level);             // test seam
    pub fn force_tier_publish_paused_for_test(&self, paused: bool);
    pub fn effective_floors(&self, budget: u64) -> Vec<u64>;  // §2.4; side effect: floors_clamped
    pub fn tick_inner(&self, budget: u64, rss_sample: u64, unreclaimable_sample: u64);  // §3.1 (test seam)
    pub fn resolve_budget_now(&self) -> u64;                  // §1.3
    pub fn tick(&self);                                       // resolve + probe + tick_inputs + tick_inner
    pub fn stats_components(&self) -> Vec<(&'static str, u64 /*current*/, u64 /*floor*/, u64 /*weight*/, u64 /*sheds*/)>;
    fn explicit_budget_engaged(&self) -> bool;                // flag != 0 || env parses
    fn shed_to_weights(&self, budget, pressure, reg: &[Arc<Component>]);
}
pub static MEM_BUDGET: Lazy<MemBudget>;                       // process singleton (:600)
pub fn level() -> Level;                                      // MEM_BUDGET.level()      (:604)
pub fn tier_publish_paused() -> bool;                         // MEM_BUDGET.tier_publish_paused() (:947)
pub fn effective_parked_cap(cap: usize, level: Level) -> usize;          // Red => max(cap/2,1)
pub fn resolve_budget_from(flag: Option<u64>, env: Option<u64>, cgroup_max: Option<u64>, ram: u64) -> u64;
pub fn fleet_shared_root(value: u64, share: usize) -> u64;               // div_ceil(max(share,1))
pub fn resolve_budget_from_shared(flag, env, cgroup_max, ram, share: usize) -> u64;
pub fn tick_inputs(budget: u64, rss: u64, raw_unreclaimable: u64, share: usize) -> (u64, u64, u64);
pub fn shared_system_ram_bytes() -> u64;                                 // fleet_shared_root(system_ram, share)
pub fn parse_unreclaimable_memory_stat(stat: &str) -> u64;
pub fn fleet_share_floor_refusal(share: usize, divided_budget: u64, possible_cpus: usize) -> Option<String>;
pub fn fleet_share_mount_refusal() -> Option<String>;                    // live face; None = mount proceeds
pub fn spawn_sampler();  pub async fn tick_off_thread();
pub fn register_ipc_session_arena_component(mb: &MemBudget, current, shed);  // ("ipc_session_arenas", 0, 1)
pub fn register_lru_evict_channel_components(mb: &MemBudget, read_lru: &LruCache, hot_block: &LruCache);
// Derived-sizing helpers that live in this file (pure; tie-tested in derivation_sweep_tests):
pub fn transport_buffer_cap(budget: u64) -> u64;                         // budget / 8, no ceiling
pub fn resolve_transport_buffer_cap(budget: u64, mem_max_mib: Option<&str>, mem_pct: Option<&str>) -> u64;
pub fn ipc_arena_cap(budget: u64) -> u64;                                // budget / 8
pub fn resolve_ipc_arena_cap(budget: u64, mem_max_mib: Option<&str>, mem_pct: Option<&str>) -> u64;
pub fn ipc_session_population_target(cpus: usize) -> u64;               // max(128, cpus × 8)
pub fn resolve_ipc_arena_bytes(arena_mb_env: Option<&str>, arena_cap_bytes: u64, cpus: usize) -> u64;
pub fn ipc_per_uid_session_cap(arena_cap_bytes: u64, arena_bytes: u64) -> usize;   // clamp(cap / max(arena,1), 64, 4096)
pub fn dir_entry_capacity(total_memory: u64) -> u64;                     // max(50_000, total / 200_000)
```

Helper semantics (exact):
- `resolve_transport_buffer_cap` / `resolve_ipc_arena_cap` (`:643-679`, `:718-751`): if `mem_max_mib` parses as u64 ⇒ `mib × 1 MiB`
  verbatim (incl. 0); unparsable ⇒ warn, fall through. Else if `mem_pct` parses as finite f64 > 0 ⇒ `(budget as f64 × min(p,100)/100) as u64`
  (p > 100 warns + clamps to 100); ≤ 0/NaN/inf/unparsable ⇒ warn, fall through. Else `budget / 8`.
- `resolve_ipc_arena_bytes` (`:834-851`): env parses u64 > 0 ⇒ verbatim MiB (0 warns, falls through); else
  `max(64 MiB, (cap / population_target(cpus) / 4 MiB) × 4 MiB)`.
- `fleet_share_floor_refusal` (`:1051-1077`): `share ≤ 1 ⇒ None`; `floor = possible_cpus × Q_DEPTH_FLOOR(4) × PAYLOAD_BASE(1 MiB)`;
  `divided_budget ≥ floor ⇒ None`; else the message (must contain `SQUEEZEFS_FLEET_SHARE={share}`, the divided budget, the floor,
  `Q_DEPTH_FLOOR`, possible_cpus). `fleet_share_mount_refusal` (`:1084-1097`): None if share ≤ 1 or an explicit tier is engaged;
  else prices `resolve_budget_from_shared(None, None, cgroup, ram, share)` with `possible_cpus()`. Called from `main.rs:3314`; a `Some` fails the mount.

---

## 5. Stats surface and tripwires (`fuse_client.rs:11318-11354`; design doc `:510-511`)
| key | type | meaning |
|---|---|---|
| `mem_budget_bytes` | gauge | resolved budget at last tick (0 before first tick) |
| `mem_budget_pressure_bytes` | gauge | `max(Σ non-reclaimable gauges, rss_max, unreclaim_max)` |
| `mem_budget_gauge_sum_bytes` | gauge | FULL registry sum incl. kernel_reclaimable |
| `mem_budget_level` | 0/1/2 | current level |
| `mem_budget_yellow_events` / `mem_budget_red_events` | counters | entry edges only |
| `mem_budget_floors_clamped` | bool | Σ floors > 90 % budget — proportional clamp active |
| `mem_budget_unreclaimable_bytes` | gauge | windowed max of the (fleet-divided) cgroup unreclaimable set |
| `mem_budget_hard_backstops` | counter | backstop entry edges |
| `mem_budget_backstop_active` | bool | shed target currently collapsed to floors |
| `mem_budget_tier_publish_paused` | bool | unreclaimable arm in Red band |
| `mem_budget_components{name: {current, floor, weight, sheds}}` | map | `stats_components()` (floor is the RAW floor, not the clamped one) |
| `mem_budget_dehydrate_paused` | counter (consumer) | Yellow dropped a dehydration victim |
| `read_tier_publishes_paused` | counter (consumer) | publish skipped while paused |
| `parked_gate_waits` / `parked_gate_self_flushes` / `parked_gate_timeouts` | counters (consumer) | Red parked-admission gate |

Tripwires: `mem_budget_hard_backstops` and `parked_gate_timeouts` growing on a quiet workload = convergence regression (doc `:511`);
`parked_gate_timeouts == 0` with a working backend (test Phase F); `red_events > 0` with no OOM-kill is the designed outcome.
Follow-up trigger (OQ #4): `windowed_rss_max` sustaining ≳ 10 % above the gauge sum on quiet workloads = allocator drift (`:58-61`).

---

## 6. Env knobs (registry `src/env_knobs.rs`; malformed/out-of-range values REFUSE the process at startup; mem_budget.rs itself re-parses leniently)
| name | kind | range | default | read at | purpose |
|---|---|---|---|---|---|
| `SQUEEZEFS_MEM_BUDGET_MB` | int | 1 ..= 2^30 (MiB) | derived | every tick (`:546-549`), `explicit_budget_engaged` | absolute R5 budget; wins over cgroup/RAM, undivided by fleet share; loses to `--mem-budget` |
| `SQUEEZEFS_FLEET_SHARE` | int | 1 ..= 4096 | 1 | once, cached (`cpu.rs:46`) | N co-located daemons; divides cgroup/RAM roots and the unreclaimable sample (ceil); 0 refuses at startup |
| `SQUEEZEFS_TRANSPORT_MEM_MAX` | int | 1 ..= 2^30 (MiB) | derived | caller passes string | transport payload-buffer cap, absolute (`2048` restores the retired 2 GiB ceiling) |
| `SQUEEZEFS_TRANSPORT_MEM_PCT` | int | 1 ..= 100 | derived | caller | transport cap as % of budget |
| `SQUEEZEFS_IPC_MEM_MAX` | int | 1 ..= 2^30 (MiB) | derived | caller | ipc_session_arenas admission cap, absolute |
| `SQUEEZEFS_IPC_MEM_PCT` | int | 1 ..= 100 | derived | caller | ipc admission cap as % of budget |
| `SQUEEZEFS_IPC_ARENA_MB` | int | 1 ..= 2^20 (MiB) | derived | caller | per-session arena size, verbatim |
| `SQUEEZEFS_PARKED_GATE_ASSIST_MS` | int | 0 ..= 600_000 | 200 (doc says 250; code `fuse_client.rs:24345` = 200) | consumer | Red parked-gate drain-assist window before self-flush |
| `--mem-budget` (CLI, `main.rs:460-465`) | size string | `6G`, `512MB`, `50%` of shared RAM | unset | mount | highest-precedence budget |

---

## 7. Numeric constants with their WHY (verbatim where short)
| constant | value | line | why |
|---|---|---|---|
| `YELLOW_ENTER/EXIT`, `RED_ENTER/EXIT` | 80/76, 95/91 | `:87-93` | "4-point gaps so a pressure hovering AT a boundary cannot flap the level (R-7)" |
| `SHED_TARGET_PCT` | 85 | `:94-96` | "mid-Yellow — below the Yellow-exit edge would oscillate the level itself" |
| `FLOOR_CAP_PCT` | 90 | `:97-99` | "Floors may claim at most this share of the budget before the proportional clamp engages" |
| `RSS_WINDOW` | 5 | `:100-101` | "RSS window length (1 Hz samples) — the decay horizon" |
| `BACKSTOP_SUSTAIN_TICKS` | 3 | `:102-106` | "One or two ticks is a spike the normal Red response absorbs; three is a convergence failure" |
| cgroup factor | ×0.8 as `/10*8` | `:965` | §5.7 resolution order |
| RAM factor | ×0.7 as `/10*7` | `:968` | 70 % RAM fallback |
| sampler period | 1 s | `:1228` | re-read cgroup "within a second"; 5-tick window = 5 s decay |
| page-size fallback | 4096 | `:1175-1182` | sysconf -1 |
| `arena.4096.purge` | 4096 | `:1205-1207` | MALLCTL_ARENAS_ALL |
| `transport_buffer_cap` | budget/8 | `:626-628` | "the same scale-free fraction as ipc_arena_cap"; no ceiling ("the former fixed 2 GiB ... degraded depth below the measured-best 32 on > 64-possible-CPU big-RAM boxes for no physical reason") |
| `IPC_ARENA_CAP_DEFAULT_PCT` | 12.5 (= /8) | `:681-686` | no ceiling: "on the 251 GB field box it clamped the pool to ~31 × 64 MiB sessions against a 48-HELLO fleet, ipc_bind_refused_budget 13/48" |
| `IPC_ARENA_FLOOR_BYTES` | 64 MiB | `:753-756` | "never-regress-below-shipped ... every box ran 64 MiB sessions" |
| `IPC_ARENA_DMA_ALIGN_BYTES` | 4 MiB | `:758-773` | slots (1024) × 4 KiB LBA; odd 2 MiB multiples make slab ≡ 2048 mod 4096 ⇒ "EXACTLY half of all round-robin direct-drive reads bounce" (−15.4 %, bounce rate 50.006 %) |
| `EXA_CANON_QD` | 8 | `:775-786` | external instrument dims (DDN exa-client kit: libaio, direct=1, qd=8) |
| population target | max(128, cpus×8) | `:788-811` | "32 cpus → 256 fio processes against the ~128-session budget ... ipc_admission_refusals ~125/pass, ~half the fleet silently on the kernel lane, engagement 0.697 ⇒ INVALID row"; floor 128 = retired /128 fraction |
| per-uid cap | clamp(cap/arena, 64, 4096) | `:853-891` | floor 64 shipped; rail 4096 = ctl-thread exhaustion rail; "22.5 GiB over 176 MiB derived arenas = a ~131-session budget" |
| `dir_entry_capacity` | max(50_000, total/200_000) | `:1031-1039` | one entry per 200 KB of shared RAM, floored at shipped 50 000 |
| Q_DEPTH_FLOOR / PAYLOAD_BASE (fuse3) | 4 / 1 MiB | tests `:1559-1560` | refusal floor = possible_cpus × 4 × 1 MiB (32 CPUs ⇒ 128 MiB) |
| Production floors | §2.5 | `fuse_client.rs` | "32 parked blocks at the default 4 MiB" etc. |
| `LruCache::EVICT_CHANNEL_BYTE_BOUND` | 256 MiB | `lru.rs:153` | per-channel send-side bound; "up to 512 MiB of live multi-MiB Bytes outside the authority" pre-RES-4 (`:910-911`) |

Measured numbers quoted in comments: 16-stream admission outran Red (`:28`); 1,937 parked buffers = 7.6 GiB anon against a 256
cap (`:49-50`); gauge_sum rode 2.1 GiB over budget 100+ s, kernel killed 7.94 GiB anon (tests `:24-25`); f1 trace: 5 GiB of
reclaimed tier logical bytes pinning phantom Red (`:128-129`); ~5 GiB anon with gauges ~130 MiB and no VMA > 200 MiB, 11 M+
node-cache hits (`:1194-1197`); RSS ramp 2.1 → 7.5 GiB in < 30 s with 16,384 channel payloads (tests `:281-284`); 36 ten-second
deadline stalls from drain convoys (tests `:1128-1129`); N=32 fleet all Red at 0.5 GB own ledger (`:992-993`); 316k-IOPS
geometry (`:614`); 1 MiB write ring vs 512 KiB entries = 100 % staging refusals (tests `:1123-1125`).

---

## 8. Concurrency (`:52-56`)
- **Registry**: `ArcSwap<Vec<Arc<Component>>>`. Read = one guarded `load()` (no lock, wait-free for readers); write = `rcu`
  clone-append (writers may retry under contention; mount-time only). Zig: an atomic pointer to an immutable, refcounted
  slice; append = copy + CAS; readers hold a ref for the tick's duration. No removal ever.
- **Every scalar is a single-word atomic with `Ordering::Relaxed`** — `level: AtomicU8`, `flag_budget/budget/pressure/gauge_sum/
  unreclaimable/yellow_events/red_events/hard_backstops/backstop_over_ticks: AtomicU64`, `rss_window/unreclaim_window: [AtomicU64; 5]`,
  `rss_idx/unreclaim_idx: AtomicUsize`, `floors_clamped/tier_paused/backstop_on: AtomicBool`; per-component `sheds/shed_target_bytes: AtomicU64`.
  "racy-tolerant by design (a stale level read costs one admission decision, never correctness) ... No cross-word invariant ⇒ no loom model".
- Edge detection uses `AtomicBool::swap` so log lines fire once per edge even if (hypothetically) two ticks raced; in practice
  ticks are serialized by the awaited sampler loop. `tick_inner` is only ever called from one thread at a time in production,
  but is `&self` and safe to call concurrently with `level()` readers from any thread (test `level_read_is_lock_free_and_immediately_visible`).
- **Shed callbacks are invoked with no lock held** (`:530`): the tick holds only the ArcSwap guard (a refcount), computes targets
  from fresh `current()` reads, then calls `shed(target)`. Shed closures must be synchronous and non-blocking (production ones
  post a target atomically + notify a worker, or trim a pool); a slow gauge/shed only delays the tick, never a reader.
- Gauge closures may be called twice per Red tick (sum pass + shed pass, `:363`, `:498`, `:526`) — they must be cheap and side-effect free.
- `spawn_sampler` guard: `static STARTED: AtomicBool` swap. Production registration guard: `static REGISTERED: AtomicBool`.
- `MEM_BUDGET` is a lazily-initialised process global; `new_for_test()` builds independent instances with no global coupling.

---

## 9. Tests → red-first list

### 9.1 `tests/mem_budget_tests.rs` (pure logic unless flagged; all use `MemBudget::new_for_test()` + `tick_inner`)
| test | asserts | needs |
|---|---|---|
| `budget_resolution_order_flag_env_cgroup_ram` (`:88`) | flag(2G) > env(3G) > cgroup 10G×0.8=8G > RAM 10G×0.7=7G | pure |
| `transport_buffer_cap_is_budget_fraction` (`:120`) | cap = budget/8 for 6 GiB+410 MiB and 76 GiB; cap(0)=0 | pure |
| `floor_sum_clamp_is_proportional_and_loud` (`:137`) | floors {600,300}: budget 1000 ⇒ verbatim, not clamped; budget 800 ⇒ {480,240}, `floors_clamped()` true | pure |
| `level_transitions_hysteresis_and_entry_events` (`:153`) | sequence 790 G, 800 Y, 940 Y, 950 R, 920 R, 905 Y, 770 Y, 759 G, 800 Y, 700 G at budget 1000; yellow_events == 2, red_events == 1 | pure |
| `windowed_rss_max_decays_not_ratchets` (`:185`) | rss 960 then 100: Red for the spike tick + 4 more ticks, Green on the 6th; pressure ≤ 100 | pure |
| `red_sheds_to_weights_over_floors_once_per_tick` (`:208`) | gauges a=500(f100,w3) b=400(f100,w1) c=80(f80,w1), budget 1000 ⇒ Red; exactly 2 shed calls (c none); targets ≥ floors; cut_a ≈ 3×cut_b (±3); cut_a+cut_b == 130; then budget 1200 ⇒ Yellow, zero shed calls | pure |
| `level_read_is_lock_free_and_immediately_visible` (`:253`) | 4 threads × 10 000 `level()` reads during 100 ticks never block/invalid; final level Red at 990/1000 | pure, threads |
| `dehydration_channel_is_byte_bounded_at_the_send_side` (`:279`) | `LruCache` evict channel: parked bytes ≤ 256 MiB bound, drops counted, admission resumes after drain | LruCache module, not mem_budget |
| `parked_cap_halves_under_red` (`:331`) | `effective_parked_cap(256, G/Y/R)` = 256/256/128 | pure |
| `pressure_counts_windowed_unreclaimable_arm` (`:346`) | unreclaimable 960 alone ⇒ Red; `pressure_bytes ≥ 960`; `unreclaimable_bytes == 960`; decays to Green after 5 samples of 100 | pure |
| `tier_publish_pause_tracks_unreclaimable_hysteresis` (`:379`) | gauge 990 + unreclaimable 100 ⇒ Red but NOT paused; unreclaimable 950 ⇒ paused; holds through 5×920; releases after 5×900 | pure |
| `hard_backstop_escalates_to_floors_after_sustained_overage` (`:429`) | gauge 500 (f100), unreclaimable 1100: after 2 ticks Red, hard_backstops 0, shed targets > 100; 3rd tick ⇒ hard_backstops 1, active, target == 100; 5×200 ⇒ released, count stays 1; gauge-only 1500 never trips | pure |
| `red_convergence_under_admission_storm` (`:499`) | gauge regrows +400 before each of 6 ticks, unreclaimable 1200; every post-shed value ≤ 850; hard_backstops ≥ 1; final gauge == floor 100 | pure |
| `kernel_reclaimable_components_do_not_drive_pressure` (`:547`) | 400 anon + 5000 reclaimable vs 1000 ⇒ Green, pressure 400, gauge_sum 5400; then unreclaimable 990 ⇒ Red | pure |
| `memory_stat_unreclaimable_parser` (`:598`) | fixture sums anon+file_dirty+file_writeback+shmem+unevictable+slab_unreclaimable only; "" ⇒ 0; "anon garbage\nfile_dirty 12\n" ⇒ 12 | pure |
| `advisory_integration_phases` (`:757`) | A: Yellow drops dehydration victims (counter) and victim never reaches tier; B: Red ⇒ 0 `prefetch_issued`, Green resumes; C: Yellow freezes `prefetch_window_hwm`; D: `drain_parked_toward(0)` ⇒ parked bytes 0, data byte-exact; E: forced tier pause ⇒ publish skipped + counted, resumes after; F: Red storm of 140 half-block writes keeps parked ≤ 128×BS, `parked_gate_waits` > 0, `parked_gate_self_flushes` > 0, `parked_gate_timeouts` == 0, data byte-exact | full filesystem stack (Linux-ish; not portable logic) |

### 9.2 `tests/mem_budget_sampler_venue_tests.rs`
| `sampler_tick_does_not_occupy_an_async_worker` (`:33`) | on a single-worker runtime, a probe task spawned after `tick_off_thread()` finishes BEFORE the 400 ms-slow tick ⇒ the tick body runs off the async workers | async runtime; registers a global component |

### 9.3 `tests/derivation_sweep_tests.rs` (mem_budget ties; all pure)
| test | asserts |
|---|---|
| `transport_buffer_cap_is_budget_fraction_no_fixed_ceiling` (`:45`) | budget/8 at 6.4 GiB, 76 GiB, 176 GiB ⇒ 22 GiB; 0 ⇒ 0 |
| `transport_cap_resolution_precedence_absolute_pct_default` (`:68`) | abs `"2048"` ⇒ 2 GiB over pct; `"0"` ⇒ 0; pct 50 ⇒ 4 GiB of 8; 12.5 ⇒ 1 GiB; none ⇒ 1 GiB; garbage abs falls to pct; both garbage ⇒ default; pct 250 ⇒ 100 %; `-3` ⇒ default |
| `dir_entry_capacity_is_one_derivation_with_the_shipped_floor` (`:154`) | max(50_000, total/200_000) incl. boundary 50_000×200_000 and +1 |
| `ipc_session_population_target_derives_from_cores` (`:737`) | 32⇒256; 1/8/16⇒128; 64⇒512; 192⇒1536 |
| `ipc_arena_default_derives_from_admission_cap` (`:779`) | cap 22 GiB: 32 cpus ⇒ 88 MiB, 8 cpus ⇒ 176 MiB, 512 cpus ⇒ 64 MiB; floor cap ⇒ 64 MiB; 129×128 MiB/8 ⇒ 128 MiB; 90×128 MiB/8 ⇒ 88 MiB (4 MiB round-down); every derived arena % 4 MiB == 0; env `"64"`, `"80"`, `"90"` verbatim; `"junk"` ⇒ derived |
| `ipc_per_uid_session_cap_derives_from_session_budget` (`:955`) | (22 GiB, 88 MiB) ⇒ 256; (22 GiB, 176 MiB) ⇒ 128; (90×128 MiB, 88 MiB) ⇒ 130; floor shape ⇒ 64; u64::MAX ⇒ 4096; arena 0 ⇒ 4096 |
| `ipc_per_uid_session_cap_monotone_in_arena` (`:1021`) | non-increasing in arena at fixed cap |
| `exa_canon_qd_has_one_definition_tied_to_the_fio_canon` (`:1053`) | `EXA_CANON_QD == 8`; population(32) == 32×8; parses fio shell files (repo-file dependent) |
| `fleet_share_divides_the_memory_root_derived_tier_only` (`:1372`) | 320 GiB RAM, share 4 ⇒ 56 GiB; cgroup 10 GiB share 4 ⇒ 2 GiB; flag/env verbatim; `fleet_shared_root(10,4)=3`, `(5,1)=5`, `(5,0)=5`, `(u64::MAX,2)=u64::MAX/2+1` |
| `fleet_share_quarters_every_divisible_derived_cap` (`:1446`) | whole 224 GiB vs shared 56 GiB; transport cap 7 GiB; ipc cap quarters (plus other modules) |
| `fleet_share_floors_hold_and_are_never_divided_at_every_share` (`:1515`) | population floor 128 at shares 2/4/32/4096; `Q_DEPTH_FLOOR == 4`, `PAYLOAD_BASE == 1 MiB` |
| `fleet_share_one_is_byte_identical_to_the_whole_machine_posture` (`:1566`) | `_shared(..,1)` == `resolve_budget_from(..)` for 3 RAM sizes × 3 tiers; `fleet_share_floor_refusal(1, 0, 4096) == None` |
| `fleet_share_unsatisfiable_floor_refuses_naming_the_arithmetic` (`:1594`) | floor 32×4×1 MiB = 134 217 728; `(2, floor)` and `(2, floor+1)` ⇒ None; `(1024, floor−1, 32)` ⇒ message containing `SQUEEZEFS_FLEET_SHARE=1024`, `134217727`, `134217728`, `Q_DEPTH_FLOOR`, `32`; share 1 never refuses |
| `fleet_share_exemption_list_is_pinned_to_the_kernel_mandated_set` (`:1639`) | source census: `src/mem_budget.rs` is one of exactly 5 files calling `possible_cpus(` (repo-structure test) |
| `fleet_share_knob_is_registered_and_zero_refuses_at_startup` (`:1757`) | knob Int lo=1, hi≥4096, default "1"; "0","-1","junk","1.5" refuse; "1","4","32" valid |
| `the_unreclaimable_arm_divides_by_the_fleet_share_like_every_system_root` (`:2411`) | `tick_inputs(2G,1G,5G,1)` passthrough; share 32 ⇒ budget & rss unchanged, unreclaimable 5G/32; 5G+1 ⇒ 5G/32+1 (ceil) |
| `sym_read_token_budget_page_and_deadline_derive_from_r5…` (`:3939`) | reads `MEM_BUDGET.budget_bytes()` (consumer tie; 0 before any tick in a test process) |

**Linux-only pieces**: `/proc/self/cgroup`, `/sys/fs/cgroup/**/memory.{max,stat}`, `/proc/self/statm`, jemalloc purge. Everything the
tests above exercise goes through `tick_inner`/pure fns and is platform-independent; `tick()`/`resolve_budget_now()` degrade
gracefully off-Linux (cgroup ⇒ None, RSS ⇒ 0, unreclaimable ⇒ 0 ⇒ budget = 70 % RAM, pressure = gauges only).

---

## 10. Porting checklist (behavioral invariants a Zig implementation must reproduce)
1. Integer arithmetic exactly as written: `c/10*8`, `ram/10*7`, `budget*85/100`, `budget*90/100`, `floor*cap/sum`, `excess*weight/weight_sum`, `pct = sat_mul(x,100)/budget`, `div_ceil` for fleet share.
2. Window slot overwrite via a monotonically increasing index mod 5; max over all 5 slots (zeros count).
3. `budget == 0` ⇒ Green + all escalation state cleared + early return BEFORE events/sheds.
4. Order inside a tick: budget → windows → gauges → pressure → (0-check) → tier pause → backstop → level+events → shed+purge.
5. Events: Green→Yellow +1Y; Green→Red +1Y +1R; Yellow→Red +1R; downward moves 0.
6. Shed every Red tick; target 85 % (or Σ effective floors under backstop); skip weight-0 and at/below-floor; remainder to last.
7. Tier pause and backstop key on the unreclaimable window max only; level keys on `max` of all three arms; kernel_reclaimable gauges count in `gauge_sum` but not pressure.
8. Backstop: run counter resets on any tick with `unreclaim_max < budget`; entry at run ≥ 3 (edge-counted); release only when `upct < 91`.
9. Sampler: idempotent spawn; 1 s sleep between ticks measured from tick completion; tick body on a blocking thread; never overlapping.
10. `level()` is a plain relaxed load with no allocation or locking, callable from any thread/hot path.
