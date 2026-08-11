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
//! ## The mechanism (the hold, plus an opt-in ahead lane; one lever)
//!
//! 1. **The hold** ([`ReadLaneHold`]) — **the shipped default win**:
//!    completed whole-block fills (demand primaries AND pinned-lane
//!    fetches; > 256 KiB, validated-fill window only) park in a
//!    retention-ledger-invisible, purge-integrated, coverage-retired
//!    holding store — RETENTION never publishes, never mutates
//!    admission state, never exerts eviction pressure. SERVES split by
//!    deposit provenance (the 2026-07-31 ledger-visibility fix):
//!    a demand-deposited entry's serve stands in for the pre-lane
//!    device refetch and carries the R1b admission ledger with it
//!    (ghost touch → second-touch publish → protected hot landing —
//!    `DataRouter::hold_serve_admission`); lane-fetch deposits stay
//!    invisible end to end (the scan-resistance verdict).
//!    Foreground readers serve from it (binding-rechecked
//!    exactly like hot-tier hits) and every consumption path credits
//!    consumed bytes; full coverage retires the entry, so memory
//!    converges by consumption. This is the deep-qd cohort stability
//!    fix: a straggling same-block sub-read that missed the
//!    single-flight window and lost the hot-probation clock race hits
//!    the hold instead of refetching 4 MiB. Field bracket (same
//!    binary, A-B-B-A vs the A0 lever): qd8 **+11 %** (25.98 vs 23.50
//!    GB/s), qd32 **+12 %** (20.91 vs 18.60) with read_amp 1.404 →
//!    1.147 — every serve/credit path measured engagement-exact.
//! 2. **The ahead lane** (probe-governed since 2026-08-05;
//!    `SQUEEZEFS_READ_LANE_DEPTH=N` pins, `0` = off): when a lane is
//!    classified streaming and R2 declines — a resident share below
//!    its AIMD start window, the whole sub-retention regime — issue
//!    whole-block fetches at the ENGAGE-GOVERNOR's depth —
//!    **ledger-invisible**: no ghost recording, no governor
//!    arbitration, no hot/NVMe-tier publication, no admission-waste
//!    accounting — the 2026-07-26 scan-resistance verdict STANDS.
//!    Bounded by the reader-tied horizon, a per-file single issue
//!    owner, the R5 budget cap ([`READ_LANE_BUDGET_DIVISOR`]) and
//!    clamped to zero under Red (in-flight bytes converge by
//!    completion — the `write_pipeline_inflight` pattern). The depth
//!    derives CLOSED-LOOP (probe-adopt-retreat, the write-side
//!    [`crate::write_pipeline::ProbeCore`] reused verbatim —
//!    the follow-on the 2026-08-01 note §8 named): probes launch only
//!    on saturated epochs with R5 headroom, adopt only when measured
//!    fill delivery responds, retreat + cool down on dead gain, and
//!    bleed to zero on unsaturated epochs — so the falsified
//!    demand-covered venues are a duty-cycle-bounded RETREAT arm
//!    (see [`read_lane_depth_blocks`] / [`probe_governed_depth`]).
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
//! * `SQUEEZEFS_READ_LANE_DEPTH=N` — per-stream depth pin (bracket
//!   lever, the `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS` pattern);
//!   unset = the engage-governor derives the depth at runtime; `0` =
//!   no lane issue (the hold stays armed — the hold-only A/B control).
//!   Red is senior to the pin AND the governor (the write-pipeline
//!   Red-clamp precedent).

use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};

/// The R5 bound: lane in-flight fetch bytes (and the hold's insert-time
/// budget) never exceed `MEM_BUDGET / this`. Deliberately junior to the
/// write pipeline's `/4` — read speculation must never crowd out write
/// custody.
pub const READ_LANE_BUDGET_DIVISOR: u64 = 8;

/// The R1b size boundary — the SAME constant, not a mirror (derivation-
/// debt audit 2026-08-04: "mirrored" used to mean a retyped literal):
/// only fills above [`crate::routing::READ_SIZE_CLASS_BOUNDARY_BYTES`]
/// participate in the lane/hold (the at-or-below population keeps
/// today's behavior verbatim — it already has a RAM tier).
pub const READ_LANE_MIN_FILL_BYTES: usize = crate::routing::READ_SIZE_CLASS_BOUNDARY_BYTES;

/// The engage-governor's depth mapping (2026-08-05 read-throughput
/// campaign — the probe-adopt-retreat follow-on the 2026-08-01 note §8
/// named): the [`crate::write_pipeline::ProbeCore`] multiplier
/// (Q6 fixed point, ×1.0 = [`crate::write_pipeline::PROBE_MUL_ONE`])
/// maps to a per-stream ahead depth in blocks. ×1.0 — unprobed, fully
/// decayed, or retreated — is depth 0 (the exact hold-only prior
/// behavior); each +1/4 probe gain is one more block of pipeline, so
/// the FIRST adopted probe is the minimal one-block-per-stream ahead
/// window and adopted gains compound: 64→0, 80→1, 100→2, 125→3,
/// 156→5, 195→8 … (pinned table, `tests/read_lane_tests.rs`).
///
/// Dimensionless-multiplier law (the write-side probe-up precedent
/// verbatim): the governor never models the fabric — it PROBES.
/// Depth rises only while measured fill delivery responds; the
/// 2026-08-01 falsified venues (demand qd × streams already covering
/// the fabric BDP) become the RETREAT arm, duty-cycle-bounded, instead
/// of a constant 0 that leaves every under-offered shape (the field's
/// 16-job row: 21 in-flight fills vs the raw row's 160) at the demand
/// plateau.
pub fn probe_governed_depth(mul_q6: u64) -> u32 {
    (mul_q6.saturating_sub(crate::write_pipeline_core::PROBE_MUL_ONE) * 4
        / crate::write_pipeline_core::PROBE_MUL_ONE)
        .min(u64::from(u32::MAX)) as u32
}

/// Per-stream lane AHEAD-issue depth, in blocks (pure — pinned by
/// `tests/read_lane_tests.rs` depth tables). **Default = the
/// engage-governor's `governed_depth`** ([`probe_governed_depth`] over
/// the live probe multiplier — 0 until a probe epoch has MEASURED that
/// ahead depth buys delivery); an explicit `SQUEEZEFS_READ_LANE_DEPTH`
/// pin wins verbatim (`0` = ahead-issue off — the hold-only
/// measurement control). The 2026-08-01 campaign falsified both OPEN-
/// LOOP derivations on the reset-v3 venue — pure-BDP (the write
/// campaign's self-fulfilling-equilibrium lesson, reproduced:
/// floor-locked at 32+ streams) and the §5.5 AIMD window (engaged at
/// depth 2/4/16 alike, −19 % vs the hold alone: ahead-fetches died
/// FIFO-unconsumed racing a demand cohort that already covered the
/// fabric BDP) — which is exactly why the shipped derivation is
/// CLOSED-LOOP: probe, adopt on measured response, retreat + cool down
/// on dead gain (`.benchmarks/2026-08-01-read-lane.md` §8 item 2, the
/// named follow-on).
///
/// Red returns 0 — senior to the pin AND the governor (the
/// write-pipeline precedent). A budget cap that cannot hold one block
/// per stream never speculates.
pub fn read_lane_depth_blocks(
    override_depth: Option<u32>,
    governed_depth: u32,
    block_size: u64,
    active_streams: u32,
    red: bool,
    budget_cap_bytes: u64,
) -> u32 {
    if red {
        return 0;
    }
    let depth = override_depth.unwrap_or(governed_depth);
    if depth == 0 {
        return 0;
    }
    let bs = block_size.max(1);
    let streams = u64::from(active_streams.max(1));
    let cap = (budget_cap_bytes / bs / streams).min(u64::from(u32::MAX)) as u32;
    depth.min(cap)
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
    /// R1b ledger provenance (the 2026-07-31 ledger-visibility fix):
    /// `true` for DEMAND-primary deposits — a serve of such an entry
    /// after every tier probe missed stands in for exactly the device
    /// refetch the pre-lane read would have paid, so the serve sites
    /// run the ghost/second-touch admission ceremony on it
    /// (`DataRouter::hold_serve_admission`). `false` for lane-fetch
    /// deposits, which stay ledger-invisible end to end (the
    /// scan-resistance verdict — read_lane_tests contract 2).
    /// "Ledger-invisible" was always a claim about RETENTION (no tier
    /// publish, no admission mutation, no eviction pressure FROM the
    /// hold) — never a license for serves to hide re-read heat from
    /// the R1b convergence contract.
    ledger_visible: bool,
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
    /// Live entry count — the exact O(1) input to the RES-3 tombstone
    /// trigger (`scc::HashMap::len` walks buckets; this is one atomic).
    live: AtomicU64,
    /// Single-reclaimer latch: at most one [`Self::reclaim_fifo`] pass
    /// runs at a time (a second caller simply skips — the backlog is
    /// already being drained).
    reclaiming: std::sync::atomic::AtomicBool,
}

/// RES-3 hysteresis floor: below this many nodes a backlog is not worth
/// a drain pass (an amortization floor, not a resource cap — the bound
/// that matters is the `2 × live` term, which scales with the hold).
const FIFO_RECLAIM_SLACK: u64 = 32;

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
            live: AtomicU64::new(0),
            reclaiming: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Held bytes (the R5 `read_lane_hold` gauge / `read_lane_hold_bytes`
    /// stats field).
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// FIFO node count — the RES-3 tombstone-backlog probe. The R5
    /// component gauges PAYLOAD bytes, so it cannot see the ordering
    /// queue's `(u64, String)` nodes; this is the instrument that keeps
    /// the reclamation invariant honest (`tests/read_lane_tests.rs`).
    pub fn fifo_len(&self) -> usize {
        self.fifo.len()
    }

    /// Residency probe (no serve, no credit) — the consume-time
    /// evicted-unconsumed detector's hold arm and the lane task's
    /// already-resident skip.
    pub fn contains(&self, block_key: &str) -> bool {
        self.entries.contains_sync(block_key)
    }

    /// Deposit a completed validated LANE fill (ledger-invisible serves
    /// — the scan-resistance posture). An existing entry for the key
    /// is kept (same incarnation ⇒ identical bytes; a changed incarnation
    /// purges first). Then trims oldest-first to `budget` — evictions of
    /// never-fully-consumed entries count
    /// `read_lane_hold_evicted_unconsumed` (the lane's refetch-spiral
    /// detector).
    pub fn insert(&self, block_key: &str, bytes: Bytes, budget: u64) {
        self.insert_class(block_key, bytes, budget, false);
    }

    /// [`Self::insert`] with DEMAND provenance: serves of this entry
    /// carry the R1b admission ledger (see `HoldEntry::ledger_visible`).
    /// Duplicate-insert keeps the existing entry's provenance — a lane
    /// deposit racing a demand primary at worst downgrades one ledger
    /// touch to a missed publish, which is always correctness-safe (the
    /// next reader goes to the device — the GhostTable's racy-tolerant
    /// class).
    pub fn insert_demand(&self, block_key: &str, bytes: Bytes, budget: u64) {
        self.insert_class(block_key, bytes, budget, true);
    }

    fn insert_class(&self, block_key: &str, bytes: Bytes, budget: u64, ledger_visible: bool) {
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
                    ledger_visible,
                },
            )
            .is_ok()
        {
            self.bytes.fetch_add(len, Ordering::Relaxed);
            self.live.fetch_add(1, Ordering::Relaxed);
            self.fifo.push((seq, block_key.to_string()));
            crate::fuse_client::METRICS
                .read_lane_holds
                .fetch_add(1, Ordering::Relaxed);
            self.trim_to(budget);
            // RES-3: `trim_to` runs only while `bytes > target`, so in
            // the healthy steady state (every entry retires on coverage)
            // it never pops and the ordering queue grows a node per
            // deposit — unbounded, and invisible to the R5 payload-byte
            // gauge. Reclaim the backlog here, amortized.
            self.reclaim_fifo();
        }
    }

    /// Serve the held block: `credit` consumed bytes (0 = anti-refetch
    /// serve only, e.g. the single-flight loop probe where the caller's
    /// slice length is unknown). Full coverage retires the entry.
    pub fn serve(&self, block_key: &str, credit: u64) -> Option<Bytes> {
        self.serve_with_provenance(block_key, credit)
            .map(|(b, _)| b)
    }

    /// [`Self::serve`] plus the entry's ledger provenance: `true` when
    /// the serve must run the R1b admission ceremony (demand-deposited
    /// entry — see `HoldEntry::ledger_visible`). The router serve
    /// sites are the only callers that act on the flag.
    pub fn serve_with_provenance(&self, block_key: &str, credit: u64) -> Option<(Bytes, bool)> {
        let mut retire = false;
        let out = self.entries.read_sync(block_key, |_, e| {
            if credit > 0 {
                let len = e.bytes.len() as u64;
                let prev = e.served.fetch_add(credit, Ordering::Relaxed);
                if prev < len && prev.saturating_add(credit) >= len {
                    retire = true;
                }
            }
            (e.bytes.clone(), e.ledger_visible)
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

    /// The entry's consumed-byte credit (tests + observability; `None`
    /// when the key is not held). The coverage-credit truth contract
    /// (`tests/read_lane_tests.rs` contract 12) reads it to pin that
    /// every serve arm credits its TRUE block coverage.
    pub fn served_bytes(&self, block_key: &str) -> Option<u64> {
        self.entries
            .read_sync(block_key, |_, e| e.served.load(Ordering::Relaxed))
    }

    /// Trim oldest-first to `target` bytes (insert-time budget and the
    /// R5 shed hook). Evicted-unconsumed entries are CLASSED (2026-08-05
    /// hold-churn campaign): an ahead-class (lane-fetch) eviction also
    /// counts `read_lane_hold_ahead_evictions` — the engage-governor's
    /// landing-zone-pressure signal (a probe launched into a hold that
    /// evicts its ahead deposits before their readers arrive measures
    /// its own thrash as dead gain forever). Oldest-first is itself the
    /// ahead-priority mechanism: ahead entries are the NEWEST deposits,
    /// so the flow-through demand population always evicts first.
    pub fn trim_to(&self, target: u64) {
        while self.bytes.load(Ordering::Relaxed) > target {
            let Some((seq, key)) = self.fifo.pop() else {
                return;
            };
            // Stale tombstone (entry retired/purged/re-inserted): skip.
            if let Some((_, e)) = self.entries.remove_if_sync(&key, |e| e.seq == seq) {
                let len = e.bytes.len() as u64;
                self.bytes.fetch_sub(len, Ordering::Relaxed);
                crate::gauge_core::sub_saturating(&self.live, 1);
                if e.served.load(Ordering::Relaxed) < len {
                    crate::fuse_client::METRICS
                        .read_lane_hold_evicted_unconsumed
                        .fetch_add(1, Ordering::Relaxed);
                    if !e.ledger_visible {
                        crate::fuse_client::METRICS
                            .read_lane_hold_ahead_evictions
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    fn retire(&self, block_key: &str) {
        if self.remove_entry(block_key) {
            crate::fuse_client::METRICS
                .read_lane_hold_retired
                .fetch_add(1, Ordering::Relaxed);
            // RES-3: retirement is what MINTS the tombstone; a hold that
            // stops taking deposits must still shed its backlog.
            self.reclaim_fifo();
        }
    }

    /// Exactly-once gauge accounting: the scc removal winner subtracts.
    fn remove_entry(&self, block_key: &str) -> bool {
        // Write-IOPS economy (2026-08-11): the R-6 unified purge calls
        // this per invalidated block, and on write-heavy shapes the key
        // is absent — a reader-lock `contains` probe instead of a
        // bucket-WRITER lock per absent key. (Deliberately NOT gated on
        // the `live` gauge: it is saturating/best-effort, and a stale 0
        // skipping a mandatory purge is the R-6 stale-serve class.) The
        // probe→remove race window equals today's remove→insert one (the
        // hold's seq/retire protocol owns it either way).
        if !self.entries.contains_sync(block_key) {
            return false;
        }
        if let Some((_, e)) = self.entries.remove_sync(block_key) {
            self.bytes
                .fetch_sub(e.bytes.len() as u64, Ordering::Relaxed);
            crate::gauge_core::sub_saturating(&self.live, 1);
            true
        } else {
            false
        }
    }

    /// RES-3: drop the FIFO nodes whose entry is gone (retired, purged,
    /// or re-inserted under a fresher seq).
    ///
    /// A `SegQueue` has no interior removal, so this is a drain-and-refill
    /// pass, gated on the backlog exceeding `2 × live + slack` — amortized
    /// O(1) per deposit, and it bounds the node count at that multiple of
    /// the live set instead of at "every deposit this mount ever made".
    /// A head-only skim cannot do the job: a long-lived unconsumed entry
    /// sits at the head while tombstones pile up behind it.
    ///
    /// Ordering: kept nodes are re-pushed in their original relative
    /// order. Nodes pushed CONCURRENTLY with a pass land ahead of them,
    /// so a trim in that window can pick a slightly-out-of-order victim
    /// — an LRU-hint imprecision over at most the handful of deposits
    /// made during one pass, whose cost is one refetch (the store's
    /// standing eviction posture), never correctness.
    fn reclaim_fifo(&self) {
        let backlog = self.fifo.len() as u64;
        if backlog <= self.live.load(Ordering::Relaxed) * 2 + FIFO_RECLAIM_SLACK {
            return;
        }
        if self
            .reclaiming
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let mut keep: Vec<(u64, String)> = Vec::new();
        for _ in 0..backlog {
            let Some((seq, key)) = self.fifo.pop() else {
                break;
            };
            if self
                .entries
                .read_sync(&key, |_, e| e.seq == seq)
                .unwrap_or(false)
            {
                keep.push((seq, key));
            }
        }
        for node in keep {
            self.fifo.push(node);
        }
        self.reclaiming
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Coarse monotonic milliseconds since process start (budget-decay
/// clock — the write-pipeline shape).
fn coarse_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

/// Per-mount read-lane authority: the arm/disarm lever, the depth pin,
/// the engage-governor probe layer, and the aggregate in-flight gauge
/// (the R5 `read_lane_inflight` component source).
pub struct ReadLaneGovernor {
    enabled: bool,
    depth_override: Option<u32>,
    /// The engage-governor (2026-08-05): the write-side BBR-flavored
    /// probe core REUSED verbatim (loom-modeled, weakening-verified).
    /// Delivery = completed whole-block fill bytes (demand primaries +
    /// lane fetches — total device fill throughput, so an ahead lane
    /// that merely displaces demand fetches reads as dead gain and
    /// retreats); saturation = the sat-mark snapshot pattern
    /// (`probe_waits_snap` precedent); headroom = below the R5 cap and
    /// not Red. [`probe_governed_depth`] maps the multiplier to the
    /// default ahead depth.
    probe: crate::write_pipeline_core::ProbeCore,
    /// Saturation marks (issue-path observations: a classified stream
    /// the lane declines at depth 0, a depth-bound issue loop, or a
    /// reader that caught an in-flight fill) and the last-roll
    /// snapshot — marks moved since the snapshot = a saturated epoch.
    probe_sat_marks: AtomicU64,
    probe_sat_snap: AtomicU64,
    /// `read_lane_hold_ahead_evictions` at the last epoch roll — the
    /// landing-zone pressure snapshot (2026-08-05): movement since the
    /// snapshot reads as zero headroom (see [`Self::probe_epoch_tick`]).
    probe_ahead_snap: AtomicU64,
    inflight_bytes: AtomicU64,
    /// The live consume-behind hold budget (bytes), cached by the issue
    /// path (which knows streams × depth) for the deposit sites (which
    /// do not). 0 = never derived yet (deposit sites fall back to the
    /// floor-shaped derivation). FAST-UP / SLOW-DOWN (round-4 field
    /// lesson): the raw derivation flaps with whichever lane touched
    /// last (a pass-boundary lane claim starts at window 2 ⇒ a 1.2 GiB
    /// derivation trimming a healthy 9.7 GiB hold — half the row's
    /// deposits evicted unconsumed, each a paid-for device fetch
    /// thrown away); shrink is decayed ⅛ per 2 s epoch.
    hold_budget: AtomicU64,
    hold_budget_decay_ms: AtomicU64,
}

impl ReadLaneGovernor {
    /// Env-resolved once per router (`SQUEEZEFS_READ_LANE`,
    /// `SQUEEZEFS_READ_LANE_DEPTH`); unrecognized values refuse loud,
    /// forward-only.
    pub fn from_env() -> Self {
        let enabled = crate::env_knobs::bool_knob("SQUEEZEFS_READ_LANE", true);
        // ENG-10: malformed values are refused by the startup gate, not by
        // a panic inside this constructor (which the release profile's
        // panic="abort" turns into an aborted mount over a typo).
        let depth_override = crate::env_knobs::opt_int_knob::<u32>("SQUEEZEFS_READ_LANE_DEPTH");
        Self {
            enabled,
            depth_override,
            probe: crate::write_pipeline_core::ProbeCore::new(),
            probe_sat_marks: AtomicU64::new(0),
            probe_sat_snap: AtomicU64::new(0),
            probe_ahead_snap: AtomicU64::new(0),
            inflight_bytes: AtomicU64::new(0),
            hold_budget: AtomicU64::new(0),
            hold_budget_decay_ms: AtomicU64::new(0),
        }
    }

    /// The A0 lever (`SQUEEZEFS_READ_LANE=0` ⇒ false): gates issue,
    /// deposits, probes and credits — exact prior behavior when off.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Live per-stream ahead depth (blocks) — [`read_lane_depth_blocks`]
    /// over the pin and the engage-governor's probed depth.
    pub fn depth_blocks(
        &self,
        block_size: u64,
        active_streams: u32,
        red: bool,
        budget_cap_bytes: u64,
    ) -> u32 {
        read_lane_depth_blocks(
            self.depth_override,
            self.governed_depth(),
            block_size,
            active_streams,
            red,
            budget_cap_bytes,
        )
    }

    /// The engage-governor's current depth contribution
    /// ([`probe_governed_depth`] over the live probe multiplier).
    pub fn governed_depth(&self) -> u32 {
        probe_governed_depth(self.probe.mul_q6())
    }

    /// `true` under an explicit `SQUEEZEFS_READ_LANE_DEPTH` pin — the
    /// probe layer goes dormant so the A/B lever stays verbatim (the
    /// write-pipeline depth-override precedent).
    pub fn depth_pinned(&self) -> bool {
        self.depth_override.is_some()
    }

    /// Count completed whole-block fill bytes into the running probe
    /// epoch (demand primaries + lane fetches — TOTAL fill delivery,
    /// the response signal probes are adjudicated against).
    pub fn probe_on_fill_bytes(&self, bytes: u64) {
        self.probe.on_bytes(bytes);
    }

    /// One issue-path saturation observation (see the field doc): the
    /// epoch that contains at least one mark is a saturated epoch.
    pub fn note_probe_saturation(&self) {
        self.probe_sat_marks.fetch_add(1, Ordering::Relaxed);
    }

    /// Roll the probe epoch from the issue path (production clock):
    /// saturation from the sat-mark snapshot, headroom from the caller
    /// (below the R5 cap and not Red) COMPOSED with the landing-zone
    /// pressure arm (2026-08-05 hold-churn campaign): an ahead-class
    /// hold eviction since the last epoch means the hold is evicting
    /// lane deposits before their readers arrive — probing into that
    /// is guaranteed dead gain (the probe measures its own thrash), so
    /// pressure reads as ZERO headroom until an eviction-free epoch.
    pub fn probe_epoch_tick(&self, headroom: bool) {
        let ahead_now = crate::fuse_client::METRICS
            .read_lane_hold_ahead_evictions
            .load(Ordering::Relaxed);
        let _ = self.probe_epoch_tick_at(coarse_ms(), headroom, ahead_now);
    }

    /// [`Self::probe_epoch_tick`] with an explicit clock and
    /// ahead-eviction reading (tests — the ProbeCore determinism
    /// contract). Returns `true` iff this call rolled the epoch.
    pub fn probe_epoch_tick_at(&self, now_ms: u64, headroom: bool, ahead_evictions: u64) -> bool {
        let marks = self.probe_sat_marks.load(Ordering::Relaxed);
        let saturated = marks != self.probe_sat_snap.load(Ordering::Relaxed);
        let pressure = ahead_evictions != self.probe_ahead_snap.load(Ordering::Relaxed);
        let rolled = self.probe.roll(now_ms, saturated, headroom && !pressure);
        if rolled {
            self.probe_sat_snap.store(marks, Ordering::Relaxed);
            self.probe_ahead_snap
                .store(ahead_evictions, Ordering::Relaxed);
        }
        rolled
    }

    /// [`crate::write_pipeline::ProbeCore::roll`] with an explicit
    /// clock and saturation verdict (tests — the write-side ProbeCore
    /// suite's determinism contract).
    pub fn probe_roll_at(&self, now_ms: u64, saturated: bool, headroom: bool) -> bool {
        self.probe.roll(now_ms, saturated, headroom)
    }

    /// Probes launched (`read_lane_depth_probe_ups`).
    pub fn probe_ups(&self) -> u64 {
        self.probe.probe_ups()
    }

    /// Retreats/step-downs (`read_lane_depth_probe_backoffs`).
    pub fn probe_backoffs(&self) -> u64 {
        self.probe.probe_backoffs()
    }

    /// Cache the issue path's consume-window hold-budget derivation for
    /// the deposit sites — fast-up, ⅛-per-2 s-epoch down (see the field
    /// doc; a flapping budget trims healthy deposits).
    pub fn set_hold_budget(&self, bytes: u64) {
        self.set_hold_budget_at(bytes, coarse_ms());
    }

    /// [`Self::set_hold_budget`] with an explicit clock (tests).
    pub fn set_hold_budget_at(&self, bytes: u64, now_ms: u64) {
        let cur = self.hold_budget.load(Ordering::Relaxed);
        if bytes >= cur {
            self.hold_budget.store(bytes, Ordering::Relaxed);
            return;
        }
        let last = self.hold_budget_decay_ms.load(Ordering::Relaxed);
        if last == 0 {
            // Anchor the first decay epoch at the first shrink
            // observation (a 0 anchor would let the first-ever shrink
            // fire immediately).
            let _ = self.hold_budget_decay_ms.compare_exchange(
                0,
                now_ms.max(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            return;
        }
        if now_ms.saturating_sub(last) >= 2_000
            && self
                .hold_budget_decay_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.hold_budget
                .store(bytes.max(cur - cur / 8), Ordering::Relaxed);
        }
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
