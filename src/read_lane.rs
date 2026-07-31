//! The cold-stream read lane (2026-08-01 read-lane campaign,
//! `.benchmarks/2026-08-01-read-lane.md`; design amendment
//! `docs/design-read-path.md` §5.5 / §Observability).
//!
//! ## The convicted mechanism (fio gap accounting, 2026-07-31 §6.2 +
//! this campaign's instrumentation rows I1/I2)
//!
//! Beyond-budget O_DIRECT cold streams — the EXA validation read shape
//! (libaio bs=1M qd8 nproc-wide over a TB-class set) — ride the R2
//! pipeline's **zero-resident-share regime**: `resident_share = share% ×
//! hot_budget / block / active_streams` truncates to 0 at the default
//! budgets (128 MiB hot tier, 32+ streams), so `prefetch_issue_admits`
//! never speculates (measured: `prefetch_issued` = 114 against 1.5 M
//! ops, `prefetch_active_streams` = 37) and every stream serializes on
//! one whole-block fabric RTT per block — the 21.5-vs-44 GB/s 0.49×
//! plateau. Deeper client qd makes it WORSE by breaking single-flight
//! cohorts: at qd32 the same-block sub-reads spread past the flight's
//! lifetime, the hot-probation landing churns (32-block budget vs 256+
//! live blocks), and stragglers refetch whole blocks (measured
//! `read_amp` 1.05 → 1.41 from qd8 → qd32).
//!
//! ## The mechanism (two coordinated pieces, one lever)
//!
//! 1. **The lane fetch pipeline**: when a lane is classified streaming
//!    and R2 declines with a zero resident share, the read lane issues
//!    pipelined whole-block fetches — **ledger-invisible**: no ghost
//!    recording, no governor arbitration, no hot/NVMe-tier publication,
//!    no admission-waste accounting — the 2026-07-26 scan-resistance
//!    verdict STANDS; the lane adds fetch concurrency, never tier
//!    residency. Depth is **derived at runtime** (BDP: measured fetch
//!    service floor × delivered fetch bandwidth — the write-pipeline
//!    governor's estimate shape, reusing its pure window rolls), never a
//!    constant; floor [`READ_LANE_FLOOR_BLOCKS`] per stream, bounded by
//!    the R5 budget cap ([`READ_LANE_BUDGET_DIVISOR`]) and clamped to
//!    zero speculation under Red (in-flight bytes converge by
//!    completion — the `write_pipeline_inflight` pattern).
//! 2. **The hold** ([`ReadLaneHold`]): completed whole-block fills —
//!    lane fetches AND demand primaries (> 256 KiB, validated-fill
//!    window only) — park in a ledger-invisible, purge-integrated,
//!    coverage-retired holding store. Foreground readers serve from it
//!    (binding-rechecked exactly like hot-tier hits) and every serve
//!    credits consumed bytes; full coverage retires the entry, so
//!    memory converges by consumption. This is the deep-qd cohort
//!    stability fix: a straggling same-block sub-read that missed the
//!    single-flight window and lost the hot-probation clock race hits
//!    the hold instead of refetching 4 MiB.
//!
//! ## Correctness posture (nothing new is proven here)
//!
//! Hold entries hold current-incarnation bytes by the SAME argument as
//! hot-tier entries: deposits happen only inside the validated-fill
//! publishable window (incarnation stable before the device read,
//! still-checked after; movement purges), and
//! [`crate::cache::TieredCache::purge_block_key`] — the only legal
//! block-key purge — gained the hold arm, so displaced/freed keys can
//! never serve stale from it (the R-6 law: a fifth block-key store
//! joins the unified purge or it does not exist). Every serve path
//! keeps its existing proof obligation: block-serving callers recheck
//! the CURRENT binding after bytes-in-hand.
//!
//! ## Concurrency class
//!
//! Single-word atomics + scc, racy-tolerant (the GhostTable /
//! `StreamLane` class): a lost insert is a missed hold (the reader goes
//! to the device — always correctness-safe); coverage retirement is
//! serialized per entry by `fetch_add` boundary-crossing (exactly one
//! server observes the crossing) and gauge accounting by scc removal
//! ownership (exactly one remover subtracts). No cross-word invariant ⇒
//! no loom model owed (documented per the house mandate).
//!
//! ## Levers
//!
//! * `SQUEEZEFS_READ_LANE=0` — the A0 attribution control: disables the
//!   lane AND the hold (deposits, probes, credits, issue) — exact prior
//!   behavior; the counters stay wired and read 0.
//! * `SQUEEZEFS_READ_LANE_DEPTH=N` — measurement-only per-stream depth
//!   pin (bracket lever, the `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS`
//!   pattern); `0` = no lane issue (the hold stays armed). Red is
//!   senior to the pin (the write-pipeline Red-clamp precedent).

use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};

/// Cold-start / minimum useful pipeline: blocks in flight per stream
/// while the BDP estimates learn (the campaign charter's "≥ 2 blocks in
/// flight per stream" floor — one consuming, one fetching).
pub const READ_LANE_FLOOR_BLOCKS: u32 = 2;

/// BDP multiplier: absorbs fetch-service variance and the serve-side
/// legs so the DEVICE leg stays at ≥ 1×BDP occupancy (the
/// write-pipeline `HEADROOM` shape).
pub const READ_LANE_HEADROOM: u64 = 3;

/// The R5 bound: lane in-flight fetch bytes (and the hold's insert-time
/// budget) never exceed `MEM_BUDGET / this`. Deliberately junior to the
/// write pipeline's `/4` — read speculation must never crowd out write
/// custody.
pub const READ_LANE_BUDGET_DIVISOR: u64 = 8;

/// The R1b size boundary, mirrored: only fills above this participate
/// in the lane/hold (the ≤ 256 KiB population keeps today's behavior
/// verbatim — it already has a RAM tier).
pub const READ_LANE_MIN_FILL_BYTES: usize = 256 * 1024;

/// Per-stream lane depth, in blocks (pure — pinned by
/// `tests/read_lane_tests.rs` depth tables). NO fixed depth anywhere:
/// the adaptive value is `max(floor, BDP × HEADROOM / streams)` with
/// `BDP = bw_peak × lat_floor / block_size`, bounded by the per-stream
/// share of the R5 budget cap. Red returns 0 — speculation stops, the
/// in-flight gauge converges by completion (Red is senior to the
/// measurement pin, the write-pipeline precedent). A budget cap that
/// cannot hold even one block per stream never speculates (share-0
/// rationale, preserved from §5.5).
pub fn read_lane_depth_blocks(
    override_depth: Option<u32>,
    bw_peak_bps: u64,
    lat_floor_ns: u64,
    block_size: u64,
    active_streams: u32,
    red: bool,
    budget_cap_bytes: u64,
) -> u32 {
    if red {
        return 0;
    }
    if let Some(d) = override_depth {
        return d;
    }
    let bs = block_size.max(1);
    let streams = u64::from(active_streams.max(1));
    let cap = (budget_cap_bytes / bs / streams).min(u64::from(u32::MAX)) as u32;
    if cap == 0 {
        return 0;
    }
    let bdp_blocks = ((bw_peak_bps as u128 * lat_floor_ns as u128) / 1_000_000_000u128 / bs as u128)
        .min(u128::from(u32::MAX)) as u64;
    let per_stream =
        (bdp_blocks.saturating_mul(READ_LANE_HEADROOM) / streams).min(u64::from(u32::MAX)) as u32;
    per_stream.max(READ_LANE_FLOOR_BLOCKS).min(cap)
}

/// One lane-issue admission decision (pure): per-lane depth bound AND
/// the aggregate R5 cap on in-flight fetch bytes.
pub fn lane_issue_admits(
    rl_inflight: u32,
    depth: u32,
    inflight_bytes: u64,
    block_size: u64,
    budget_cap_bytes: u64,
) -> bool {
    rl_inflight < depth && inflight_bytes.saturating_add(block_size) <= budget_cap_bytes
}

/// The hold's insert-time byte budget (pure): the R5 share, floored at
/// 4 blocks (below that the hold cannot even cover one stream's
/// consume-behind window plus one straggler block).
pub fn hold_budget_bytes(mem_budget_bytes: u64, block_size: u64) -> u64 {
    (mem_budget_bytes / READ_LANE_BUDGET_DIVISOR).max(4 * block_size.max(1))
}

/// The R5 budget the lane derives from: the live authority when the
/// sampler has resolved it, else a memoized one-shot resolution (the
/// same flag→env→cgroup→RAM order) — daemonless contexts (tests,
/// offline tools) must not read a zero budget and refuse to speculate
/// forever.
pub fn effective_mem_budget() -> u64 {
    let live = crate::mem_budget::MEM_BUDGET.budget_bytes();
    if live != 0 {
        return live;
    }
    static FALLBACK: once_cell::sync::Lazy<u64> =
        once_cell::sync::Lazy::new(|| crate::mem_budget::MEM_BUDGET.resolve_budget_now());
    *FALLBACK
}

struct HoldEntry {
    bytes: Bytes,
    /// Consumed-byte credit (serves + primary-slice credits). Crossing
    /// `bytes.len()` retires the entry — coverage retirement.
    served: AtomicU64,
    /// FIFO identity: a trim pop only removes the entry it enqueued
    /// (re-inserted keys carry a fresh seq; stale tombstones skip).
    seq: u64,
}

/// The ledger-invisible completed-fill holding store (module docs §2).
/// Keyed by block key like every cache tier; values are `Bytes`
/// refcount clones (an entry whose bytes also sit in the hot tier or a
/// caller's reply costs one allocation, counted once here — the R5
/// `read_lane_hold` component is deliberately conservative).
pub struct ReadLaneHold {
    entries: scc::HashMap<String, HoldEntry>,
    fifo: crossbeam::queue::SegQueue<(u64, String)>,
    seq: AtomicU64,
    bytes: AtomicU64,
}

impl Default for ReadLaneHold {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadLaneHold {
    pub fn new() -> Self {
        Self {
            entries: scc::HashMap::new(),
            fifo: crossbeam::queue::SegQueue::new(),
            seq: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    /// Held bytes (the R5 `read_lane_hold` gauge / `read_lane_hold_bytes`
    /// stats field).
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Residency probe (no serve, no credit) — the consume-time
    /// evicted-unconsumed detector's hold arm and the lane task's
    /// already-resident skip.
    pub fn contains(&self, block_key: &str) -> bool {
        self.entries.contains_sync(block_key)
    }

    /// Deposit a completed validated fill. An existing entry for the key
    /// is kept (same incarnation ⇒ identical bytes; a changed incarnation
    /// purges first). Then trims oldest-first to `budget` — evictions of
    /// never-fully-consumed entries count
    /// `read_lane_hold_evicted_unconsumed` (the lane's refetch-spiral
    /// detector).
    pub fn insert(&self, block_key: &str, bytes: Bytes, budget: u64) {
        let len = bytes.len() as u64;
        if len == 0 || budget == 0 {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        if self
            .entries
            .insert_sync(
                block_key.to_string(),
                HoldEntry {
                    bytes,
                    served: AtomicU64::new(0),
                    seq,
                },
            )
            .is_ok()
        {
            self.bytes.fetch_add(len, Ordering::Relaxed);
            self.fifo.push((seq, block_key.to_string()));
            crate::fuse_client::METRICS
                .read_lane_holds
                .fetch_add(1, Ordering::Relaxed);
            self.trim_to(budget);
        }
    }

    /// Serve the held block: `credit` consumed bytes (0 = anti-refetch
    /// serve only, e.g. the single-flight loop probe where the caller's
    /// slice length is unknown). Full coverage retires the entry.
    pub fn serve(&self, block_key: &str, credit: u64) -> Option<Bytes> {
        let mut retire = false;
        let out = self.entries.read_sync(block_key, |_, e| {
            if credit > 0 {
                let len = e.bytes.len() as u64;
                let prev = e.served.fetch_add(credit, Ordering::Relaxed);
                if prev < len && prev.saturating_add(credit) >= len {
                    retire = true;
                }
            }
            e.bytes.clone()
        })?;
        if retire {
            self.retire(block_key);
        }
        Some(out)
    }

    /// Credit consumption without serving (the primary-slice / hot-tier
    /// / NVMe-tier serve sites: those bytes were consumed via another
    /// path, and the hold's copy is that much closer to retirement).
    /// No-op when the key is not held.
    pub fn credit(&self, block_key: &str, credit: u64) {
        if credit == 0 {
            return;
        }
        let mut retire = false;
        let present = self
            .entries
            .read_sync(block_key, |_, e| {
                let len = e.bytes.len() as u64;
                let prev = e.served.fetch_add(credit, Ordering::Relaxed);
                if prev < len && prev.saturating_add(credit) >= len {
                    retire = true;
                }
            })
            .is_some();
        if present && retire {
            self.retire(block_key);
        }
    }

    /// Unified-purge arm ([`crate::cache::TieredCache::purge_block_key`]):
    /// drop the key unconditionally.
    pub fn purge(&self, block_key: &str) {
        let _ = self.remove_entry(block_key);
    }

    /// Trim oldest-first to `target` bytes (insert-time budget and the
    /// R5 shed hook).
    pub fn trim_to(&self, target: u64) {
        while self.bytes.load(Ordering::Relaxed) > target {
            let Some((seq, key)) = self.fifo.pop() else {
                return;
            };
            // Stale tombstone (entry retired/purged/re-inserted): skip.
            if let Some((_, e)) = self.entries.remove_if_sync(&key, |e| e.seq == seq) {
                let len = e.bytes.len() as u64;
                self.bytes.fetch_sub(len, Ordering::Relaxed);
                if e.served.load(Ordering::Relaxed) < len {
                    crate::fuse_client::METRICS
                        .read_lane_hold_evicted_unconsumed
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn retire(&self, block_key: &str) {
        if self.remove_entry(block_key) {
            crate::fuse_client::METRICS
                .read_lane_hold_retired
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Exactly-once gauge accounting: the scc removal winner subtracts.
    fn remove_entry(&self, block_key: &str) -> bool {
        if let Some((_, e)) = self.entries.remove_sync(block_key) {
            self.bytes
                .fetch_sub(e.bytes.len() as u64, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/// Coarse monotonic milliseconds since process start (window clock —
/// the write-pipeline shape).
fn coarse_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

/// Per-mount read-lane authority: the arm/disarm lever, the BDP fetch
/// estimates (reusing the write-pipeline pure window rolls), and the
/// aggregate in-flight gauge (the R5 `read_lane_inflight` component
/// source). All fields latch-free approximate gauges — a torn window
/// roll self-heals within one window.
pub struct ReadLaneGovernor {
    enabled: bool,
    depth_override: Option<u32>,
    ewma_lat_ns: AtomicU64,
    lat_floor_ns: AtomicU64,
    bw_peak_bps: AtomicU64,
    win_start_ms: AtomicU64,
    win_bytes: AtomicU64,
    inflight_bytes: AtomicU64,
}

impl ReadLaneGovernor {
    /// Env-resolved once per router (`SQUEEZEFS_READ_LANE`,
    /// `SQUEEZEFS_READ_LANE_DEPTH`); unrecognized values refuse loud,
    /// forward-only.
    pub fn from_env() -> Self {
        let enabled = std::env::var("SQUEEZEFS_READ_LANE")
            .map(|v| v.trim() != "0")
            .unwrap_or(true);
        let depth_override = match std::env::var("SQUEEZEFS_READ_LANE_DEPTH") {
            Ok(v) => Some(v.trim().parse::<u32>().unwrap_or_else(|e| {
                panic!("SQUEEZEFS_READ_LANE_DEPTH must be an integer block count: {e}")
            })),
            Err(_) => None,
        };
        Self {
            enabled,
            depth_override,
            ewma_lat_ns: AtomicU64::new(0),
            lat_floor_ns: AtomicU64::new(0),
            bw_peak_bps: AtomicU64::new(0),
            win_start_ms: AtomicU64::new(0),
            win_bytes: AtomicU64::new(0),
            inflight_bytes: AtomicU64::new(0),
        }
    }

    /// The A0 lever (`SQUEEZEFS_READ_LANE=0` ⇒ false): gates issue,
    /// deposits, probes and credits — exact prior behavior when off.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Record one completed lane fetch: feeds the BDP estimates
    /// (write-pipeline `Lane::record` shape — min-tracking latency floor
    /// decayed upward bounded by the EWMA, windowed bandwidth peak with
    /// fast-up/slow-down decay; queueing inflation never feeds the BDP).
    pub fn record_fetch(&self, bytes: u64, dur_ns: u64) {
        self.record_fetch_at(bytes, dur_ns, coarse_ms());
    }

    /// [`Self::record_fetch`] with an explicit window clock (tests).
    pub fn record_fetch_at(&self, bytes: u64, dur_ns: u64, now_ms: u64) {
        let dur_ns = dur_ns.max(1);
        let prev = self.ewma_lat_ns.load(Ordering::Relaxed);
        let ewma = if prev == 0 {
            dur_ns
        } else {
            prev - prev / 8 + dur_ns / 8
        };
        self.ewma_lat_ns.store(ewma, Ordering::Relaxed);
        let floor = self.lat_floor_ns.load(Ordering::Relaxed);
        if floor == 0 || dur_ns < floor {
            self.lat_floor_ns.store(dur_ns, Ordering::Relaxed);
        }
        self.win_bytes.fetch_add(bytes, Ordering::Relaxed);

        let ws = self.win_start_ms.load(Ordering::Relaxed);
        if ws == 0 {
            let _ = self.win_start_ms.compare_exchange(
                0,
                now_ms.max(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
            return;
        }
        let elapsed = now_ms.saturating_sub(ws);
        if elapsed >= crate::write_pipeline::WINDOW_MS
            && self
                .win_start_ms
                .compare_exchange(ws, now_ms, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            let wb = self.win_bytes.swap(0, Ordering::Relaxed);
            let peak = crate::write_pipeline::rolled_bw_peak(
                self.bw_peak_bps.load(Ordering::Relaxed),
                wb,
                elapsed,
            );
            self.bw_peak_bps.store(peak, Ordering::Relaxed);
            let f = crate::write_pipeline::rolled_lat_floor(
                self.lat_floor_ns.load(Ordering::Relaxed),
                self.ewma_lat_ns.load(Ordering::Relaxed),
            );
            self.lat_floor_ns.store(f, Ordering::Relaxed);
        }
    }

    /// Live per-stream depth (blocks) — [`read_lane_depth_blocks`] over
    /// the current estimates.
    pub fn depth_blocks(
        &self,
        block_size: u64,
        active_streams: u32,
        red: bool,
        budget_cap_bytes: u64,
    ) -> u32 {
        read_lane_depth_blocks(
            self.depth_override,
            self.bw_peak_bps.load(Ordering::Relaxed),
            self.lat_floor_ns.load(Ordering::Relaxed),
            block_size,
            active_streams,
            red,
            budget_cap_bytes,
        )
    }

    /// Aggregate in-flight lane-fetch bytes (the R5 `read_lane_inflight`
    /// gauge — a non-sheddable drain: converges by completion).
    pub fn inflight_bytes(&self) -> u64 {
        self.inflight_bytes.load(Ordering::Relaxed)
    }

    pub fn add_inflight(&self, bytes: u64) {
        self.inflight_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn sub_inflight(&self, bytes: u64) {
        self.inflight_bytes.fetch_sub(bytes, Ordering::Relaxed);
    }
}
