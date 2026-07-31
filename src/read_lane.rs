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
/// the adaptive value is the lane's live §5.5 AIMD `window` —
/// foreground-wait growth (the reader demonstrably caught the
/// pipeline: too shallow) / evicted-unconsumed collapse + quiescence
/// (demonstrated waste), already Green-gated and capped by the derived
/// `prefetch_window_cap` upstream — floored at
/// [`READ_LANE_FLOOR_BLOCKS`] and bounded by the per-stream share of
/// the R5 budget cap.
///
/// A pure-BDP derivation (measured lane-fetch bandwidth × latency
/// floor, the write-pipeline estimate shape) was built first and
/// FALSIFIED by the field bracket (2026-08-01 round 2, the campaign
/// note §round-2): it is the same self-fulfilling equilibrium the
/// 2026-07-29 probe-up campaign convicted on writes — it targets the
/// CURRENT delivery point (the plateau bandwidth × its own service
/// floor ⇒ floor depth at 32+ streams) and discovers nothing. The
/// AIMD window is the read side's honest probe: it grows only on
/// reader-wait evidence and collapses on demonstrated waste.
///
/// Red returns 0 — speculation stops, the in-flight gauge converges
/// by completion (Red is senior to the measurement pin, the
/// write-pipeline precedent). A budget cap that cannot hold even one
/// block per stream never speculates (share-0 rationale, preserved
/// from §5.5).
pub fn read_lane_depth_blocks(
    override_depth: Option<u32>,
    window: u32,
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
    window.max(READ_LANE_FLOOR_BLOCKS).min(cap)
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

/// The hold's insert-time byte budget (pure): sized to the live
/// CONSUME-BEHIND WINDOW — 2× the aggregate pipeline (streams × depth
/// × block: one being consumed + one landing per stream, doubled for
/// cohort stragglers) — floored at 4 blocks and capped by the R5 share.
/// Round-2 field lesson (2026-08-01): a flat `mem/8` budget pinned
/// 23.6 GiB of FIFO churn on a looping beyond-budget row (~50 % of
/// deposits evicted unconsumed — RAM spent on entries whose reader
/// was minutes away); the consume-window sizing keeps the hold at the
/// working set the pipeline actually needs and lets the
/// `hold_evicted_unconsumed` detector mean starvation again.
pub fn hold_budget_bytes(
    mem_budget_bytes: u64,
    block_size: u64,
    active_streams: u32,
    depth: u32,
) -> u64 {
    let bs = block_size.max(1);
    // 4× the aggregate pipeline: the ahead window itself + the
    // straggler cohort + the global-FIFO skew margin (round-4 field
    // lesson: 2× left half the deposits evicted-unconsumed under
    // 38-stream FIFO skew — each such eviction is a paid-for device
    // fetch thrown away).
    let window = 4u64
        .saturating_mul(u64::from(active_streams.max(1)))
        .saturating_mul(u64::from(depth))
        .saturating_mul(bs);
    window
        .max(4 * bs)
        .min(mem_budget_bytes / READ_LANE_BUDGET_DIVISOR)
        .max(4 * bs)
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

/// Per-mount read-lane authority: the arm/disarm lever, the depth pin,
/// and the aggregate in-flight gauge (the R5 `read_lane_inflight`
/// component source).
pub struct ReadLaneGovernor {
    enabled: bool,
    depth_override: Option<u32>,
    inflight_bytes: AtomicU64,
    /// The live consume-behind hold budget (bytes), cached by the issue
    /// path (which knows streams × depth) for the deposit sites (which
    /// do not). 0 = never derived yet (deposit sites fall back to the
    /// floor-shaped derivation).
    hold_budget: AtomicU64,
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
            inflight_bytes: AtomicU64::new(0),
            hold_budget: AtomicU64::new(0),
        }
    }

    /// The A0 lever (`SQUEEZEFS_READ_LANE=0` ⇒ false): gates issue,
    /// deposits, probes and credits — exact prior behavior when off.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Live per-stream depth (blocks) — [`read_lane_depth_blocks`] over
    /// the lane's AIMD window.
    pub fn depth_blocks(
        &self,
        window: u32,
        block_size: u64,
        active_streams: u32,
        red: bool,
        budget_cap_bytes: u64,
    ) -> u32 {
        read_lane_depth_blocks(
            self.depth_override,
            window,
            block_size,
            active_streams,
            red,
            budget_cap_bytes,
        )
    }

    /// Cache the issue path's consume-window hold-budget derivation for
    /// the deposit sites.
    pub fn set_hold_budget(&self, bytes: u64) {
        self.hold_budget.store(bytes, Ordering::Relaxed);
    }

    /// The cached consume-window hold budget (0 = never derived).
    pub fn hold_budget(&self) -> u64 {
        self.hold_budget.load(Ordering::Relaxed)
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
