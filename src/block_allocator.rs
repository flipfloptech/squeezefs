use crate::error::Result;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// RES-19 (pre-RC spec §7): retention of the free-forensics tape.
///
/// The tape held one FULL `Backtrace` string (kilobytes) per distinct
/// offset ever freed, forever, in an insert-only global map — and the
/// insert runs on the write path whenever the env knob is set. A
/// double-release investigation only ever needs the recent frees, so the
/// tape is a bounded ring: N entries, oldest evicted.
pub const FREE_FORENSICS_TAPE_CAP: usize = 4096;

/// FIND-RW5-A double-release forensics tape (env-gated by
/// `SQUEEZEFS_FREE_FORENSICS`, diagnostic-only): the last recorded free
/// backtrace per offset, shared by `finish_free` (records + pairs a
/// DOUBLE FREE) and `begin_free`'s refusal arm (pairs a REFUSED release
/// with the first free that emptied the refcount).
///
/// RES-19: a bounded ring — the map holds at most
/// [`FREE_FORENSICS_TAPE_CAP`] entries and `order` is the FIFO that names
/// the eviction victim.
struct FreeForensicsTape {
    by_offset: std::collections::HashMap<u64, String>,
    order: std::collections::VecDeque<u64>,
}

fn free_forensics_tape() -> &'static std::sync::Mutex<FreeForensicsTape> {
    static TAPE: std::sync::OnceLock<std::sync::Mutex<FreeForensicsTape>> =
        std::sync::OnceLock::new();
    TAPE.get_or_init(|| {
        std::sync::Mutex::new(FreeForensicsTape {
            by_offset: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
        })
    })
}

/// Record `offset`'s free capture, evicting the oldest entry once the ring
/// is full (RES-19). Returns the PREVIOUS capture for this offset, which
/// is the double-free pairing `finish_free` reports.
fn record_free_forensics(offset: u64, capture: String) -> Option<String> {
    let mut tape = free_forensics_tape()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prior = tape.by_offset.insert(offset, capture);
    if prior.is_none() {
        tape.order.push_back(offset);
        while tape.order.len() > FREE_FORENSICS_TAPE_CAP {
            if let Some(evict) = tape.order.pop_front() {
                tape.by_offset.remove(&evict);
            }
        }
    }
    prior
}

/// The recorded capture for `offset`, if it is still in the ring.
fn lookup_free_forensics(offset: u64) -> Option<String> {
    free_forensics_tape()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_offset
        .get(&offset)
        .cloned()
}

/// RES-19 test seam: record a capture without a live allocator (the ring's
/// bound is the contract — `tests/res_tail_tests.rs`).
pub fn record_free_forensics_for_test(offset: u64, capture: &str) {
    record_free_forensics(offset, capture.to_string());
}

/// RES-19 test seam: the ring's current occupancy.
pub fn free_forensics_tape_len_for_test() -> usize {
    free_forensics_tape()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_offset
        .len()
}

/// RES-19 test seam: whether `offset` is still retained.
pub fn lookup_free_forensics_for_test(offset: u64) -> Option<String> {
    lookup_free_forensics(offset)
}

/// Whether the forensics tape is armed — **memoized** (PERF-14).
///
/// `std::env::var` is an allocation plus the process-global environ lock
/// on EVERY call, and this gate sits on the terminal-free path
/// (`finish_free`, hundreds of thousands of calls per benchmark row) and
/// on the publish path (`DataRouter::merge_block_mappings`). Derived
/// defaults resolve once at first use — never on a per-op path (the
/// standing derivation law).
pub fn free_forensics_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // The ENG-10 registry accessor owns the CONVENTION (1/true/yes/on vs
    // 0/false/no/off, loud refusal at startup); the `OnceLock` owns the
    // COST — `bool_knob` reads the environment on every call, which is an
    // allocation plus the process-global environ lock, and this gate sits on
    // the terminal-free and publish paths.
    *ON.get_or_init(|| crate::env_knobs::bool_knob("SQUEEZEFS_FREE_FORENSICS", false))
}

/// One warn-level line per co-writer abandon BURST at most (the storm
/// shape is hundreds of in-flight cleanups landing together after a
/// custody loss — the 2026-08-19 field capture logged one ERROR per
/// in-flight block): suppress repeat warns inside a 5 s window; the
/// per-offset detail rides debug. Race-benign — a lost CAS at the window
/// edge costs at most one extra warn, never a storm.
fn abandon_warn_due() -> bool {
    static LAST_WARN_MS: AtomicU64 = AtomicU64::new(0);
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let now_ms = EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1; // +1: 0 stays the never-warned sentinel
    let last = LAST_WARN_MS.load(Ordering::Relaxed);
    if last != 0 && now_ms.saturating_sub(last) < 5_000 {
        return false;
    }
    LAST_WARN_MS
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Physical allocation stride of every data-volume allocator: block
/// offsets are minted as `block_idx * CHUNK_SIZE`, so a stored block
/// image longer than this tramples the NEXT chunk's bytes on the device
/// (the FIND-RW4-A neighbor-corruption mechanism). Every producer of a
/// stored block image must satisfy `image_len <= CHUNK_SIZE` — see
/// [`ensure_stored_block_image_fits`].
pub const CHUNK_SIZE: u64 = 4 * 1024 * 1024;

/// FIND-RW4-A defense in depth: refuse — loudly, never by silent
/// truncation or a silent overflow write — any stored block image that
/// would exceed its allocator chunk. Post-fix geometry (the store-raw
/// escape + the format-time block-size headroom clamp + the mount
/// geometry gate) makes this unreachable on in-contract volumes; if it
/// fires, a write path produced an image the volume geometry cannot hold
/// and the write MUST fail rather than corrupt the neighboring chunk.
/// Deliberately an error, not an assert: a mis-geometried volume must
/// degrade to a loud EIO, never abort the daemon.
pub fn ensure_stored_block_image_fits(
    stored_len: usize,
    chunk_size: u64,
    context: &str,
) -> crate::error::Result<()> {
    if stored_len as u64 > chunk_size {
        let msg = format!(
            "stored block image ({stored_len} B) exceeds the {chunk_size} B allocator \
             chunk at {context}: refusing the write — landing it would corrupt the \
             neighboring chunk (FIND-RW4-A guard)"
        );
        log::error!("{msg}");
        return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            msg,
        )));
    }
    Ok(())
}

/// `true` ⇔ `e` is the allocator's StorageFull refusal (the ENOSPC
/// pressure-valve trigger).
/// The ENOSPC pressure valve's async body (see the `space_valve` field):
/// boxed so the wiring closure can be stored latch-free in a `OnceLock`.
pub type SpaceValve =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// Reclaimable-supply predicate (see `space_pending`): `true` while the
/// background reclaimer still owes finish_frees (queued or in-flight).
pub type SpacePending = Arc<dyn Fn() -> bool + Send + Sync>;

/// RES-14/RES-15 liveness floor: drain-and-retry passes
/// [`BlockAllocator::allocate_block`]'s ENOSPC pressure valve will make
/// before the `StorageFull` verdict stands. Each pass awaits a FULL
/// queue drain, so this is a generous ceiling on "the reclaimer is
/// gaining on the demand", not a tuning knob — the pre-fix loop had no
/// bound at all and spun whenever concurrent reclaim traffic kept
/// `pending` true.
const ENOSPC_VALVE_MAX_ATTEMPTS: u32 = 32;

pub(crate) fn is_storage_full(e: &crate::error::SqueezefsError) -> bool {
    matches!(e, crate::error::SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull)
}

/// Outcome of [`BlockAllocator::pin_block_validated`] (§5.1 clone
/// validate-after-pin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// Reference taken and the incarnation observed stable — the pin holds.
    Pinned,
    /// Reference taken but the incarnation is UNSTABLE (a patch may be
    /// mid-flight): the caller must unpin this block too, refetch the
    /// authoritative map, and retry.
    PinnedUnstable,
    /// Reference NOT taken (freed/untracked offset) — the caller must
    /// re-resolve, never proceed unpinned.
    Refused,
}

/// Outcome of [`BlockAllocator::apply_lane_free_notice`] (publish schema
/// 16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneFreeNotice {
    /// The offset's local tracking released (`cowriter.lane_free_notices`).
    Released,
    /// The offset was re-minted here from a grant served AFTER the notice
    /// was queued — the live lifetime is untouched
    /// (`cowriter.lane_free_notices_reminted`).
    ReMinted,
    /// No local tracking existed (a foreign predecessor, or a lifetime some
    /// other arm already retired).
    Untracked,
}

pub struct BlockAllocator {
    _volume_id: Box<str>,
    chunk_size: u64,
    free_blocks: LaneCountedSet,
    highest_block: AtomicU64,
    /// Device capacity in whole chunks (0 = unbounded: offline tools /
    /// tests without a real device). Set at mount registration from the
    /// backing device/file size. `allocate_block` refuses to mint offsets
    /// past it: on a real block device the write would fail EIO/ENOSPC at
    /// DMA time; on a FILE-backed volume it silently GREW the file past
    /// its provisioned size — both discovered by the multi-volume bench
    /// EIO investigation.
    capacity_blocks: AtomicU64,
    refcounts: scc::HashMap<u64, AtomicU32>,
    /// Symmetric metadata PR 7 (design §5.4.3 / §5.4.4): the per-block
    /// **SHARED mark** — the RAM projection of the shared-block index for
    /// THIS data volume (`crate::shared_ref_core`'s word; an absent entry
    /// reads unshared). Seeded from the index at an armed forest mount's
    /// open, set by `MarkShared`, cleared by the index home's `Freed`
    /// verdict; read by the W1 predicate after the §5.1 fence and by the
    /// terminal-free decision. Empty for the life of every flat / unarmed
    /// mount (nothing marks) — those paths read one absent-key probe.
    shared: scc::HashMap<u64, AtomicU32>,
    /// PR VL6a (design-volume-lifecycle §5.6): the fsck **allocation
    /// epoch** — one monotonic atomic, bumped by the fsck coordinator at
    /// scan start and again at re-check. Explicitly a SEPARATE side map
    /// (below), never packed into the loom-verified incarnation seqlock.
    fsck_scan_epoch: AtomicU64,
    /// The scan latch: while set, `allocate_block` records each minted
    /// offset in the epoch side map (one relaxed load on the hot path;
    /// when no scan runs — zero stores, zero cost).
    fsck_scan_active: std::sync::atomic::AtomicBool,
    /// The scan-latched allocation-epoch side map (`offset → epoch at
    /// allocation`); exists only for the duration of a scan
    /// (`fsck_end_scan` drains it). C2/C3 checkers exempt any offset
    /// present here (allocation younger than the scan epoch).
    fsck_epoch_map: scc::HashMap<u64, u64>,
    /// PR VL6a (§5.6): the **in-flight allocation registry** — the
    /// enumerable live owners of allocated-but-unpublished offsets
    /// (writeback/flush units, active-block uploads, R5-parked writes,
    /// mover destinations, wire shard destinations). Fed by
    /// [`Self::inflight_register`] RAII guards at the owner paths; an
    /// owner's guard drops only AFTER its publish is durable and visible
    /// to the tree/refcount reads fsck performs (the natural scope: the
    /// guard is a local of the allocate→DMA→merge function). A crashed /
    /// aborted owner drops its guard on unwind, so a genuinely leaked
    /// offset is never shielded — only LIVE owners are.
    inflight: scc::HashMap<u64, u32>,
    /// The ENOSPC **pressure valve** (async block-reclaim,
    /// `tests/async_block_reclaim_tests.rs` contract 2): terminal frees
    /// queue their device reclaim to `crate::block_reclaim`, and the
    /// freed offsets are not reallocatable until the (background)
    /// reclaim completes — so an allocation that would refuse for space
    /// must first force a drain of the queued reclaims. A full volume
    /// can never be wedged by lazily-queued space. The valve is ASYNC
    /// (contract 2b, probe-up campaign 2026-07-29): the allocating task
    /// awaits the drain, the device reclaim work runs on the blocking
    /// pool — an engagement must never freeze an executor thread (the
    /// old inline drain froze whole fuse3 tpc lanes: the write-funnel
    /// conviction). Wired by `BackendRouter` at
    /// construction/registration; allocators used standalone (offline
    /// tools, unit fixtures) simply have no valve.
    space_valve: std::sync::OnceLock<SpaceValve>,
    /// Reclaimable-supply predicate (paired with the valve at wiring).
    space_pending: std::sync::OnceLock<SpacePending>,
    /// Idea 4 — discard elision (design-rewrite-program §3): unreturned
    /// discard DEBT per elided terminal free (`offset → bytes`). Debt is
    /// RAM-only and strictly ⊆ the free list; allocation CANCELS a
    /// reused offset's entry ([`Self::cancel_elided_debt`] in
    /// `claim_block_idx` — claim-cancels-debt, KD-4.3) and the trim
    /// venues drain it (`crate::block_reclaim::drain_debt_sync`).
    elided_debt: scc::HashMap<u64, u64>,
    /// Local mirror of the outstanding debt bytes (the per-target
    /// watermark input; the process-wide gauge is
    /// `METRICS.block_free_elided_debt_bytes`).
    elided_debt_bytes: AtomicU64,
    /// Offsets currently inside a trim CLAIM WINDOW (KD-4.4:
    /// [`Self::claim_free_for_trim`] … [`Self::return_from_trim`]) — free
    /// supply that has left the free list for the duration of one device
    /// command and comes back. The allocation funnel reads it after an
    /// empty scan: a full store whose whole free list sits inside a
    /// window is NOT full, and refusing it was the spurious-ENOSPC race
    /// (`.benchmarks/2026-09-07-overlay-enospc-convergence-flake.md`).
    trim_claimed: AtomicU64,
    /// The claim window's return edge (`return_from_trim`) — what a parked
    /// allocation waits on. `notify_waiters` only: no permit accumulates.
    trim_returned: squeezefs_ipc::sqz_notify::Notify,
    /// Per-offset incarnation seqlock: `gen << 1 | stable`.
    ///
    /// Block keys are plain offset strings, so when an offset is freed and
    /// reallocated the *same key string* names a new incarnation. A reader that
    /// resolved the key through a slightly stale block map can device-read the
    /// offset mid-transition and then publish those bytes into the shared block
    /// caches — poisoning the key for its new owner (the block then reads as
    /// zeros or another block's data until remount, while durable data is
    /// correct). The seqlock makes cache fills validated: `allocate_block`
    /// marks the offset in-flight (gen+1, stable=0), the writer calls
    /// [`Self::publish_block`] after its device write (stable=1), and
    /// `free_block` retires it (gen+1, stable=0). A fill may publish into the
    /// caches only if the word was stable before its device read and unchanged
    /// after — anything else returns bytes to the caller uncached.
    ///
    /// **RES-13 recorded ceiling** (pre-RC engineering spec §7): one
    /// entry per distinct device OFFSET this volume has ever allocated,
    /// so the map is bounded by the volume GEOMETRY — `capacity /
    /// chunk_size` entries, ~40 B each (a 100 TB volume at the shipped
    /// 4 MiB block: ~25 M offsets ≈ 1 GB at full lifetime coverage,
    /// reached only by a volume that has written every block). It grows
    /// with the address space, never with time or op count, and it
    /// deliberately has NO removal path: the incarnation counter must
    /// stay monotone per offset for the seqlock's whole point — a
    /// reclaimed entry would restart at generation 0 and let a stale
    /// in-flight fill pass its after-check against a reused offset (the
    /// generic/074 stale-fill family). Bound it by shrinking the address
    /// space (larger blocks), never by evicting entries.
    ///
    /// Since spec §6.2 item 6 each cell also carries the offset's **live
    /// lifetime stamp** — the durable-era-composed word a persisted key
    /// names (`IncarnationCell::stamp`, +8 B on the same recorded
    /// ceiling). Deliberately a second word beside the seqlock and never
    /// packed INTO it: the seqlock word is loom-verified
    /// ([`crate::incarnation_core`]) and its bit layout is load-bearing,
    /// the same reason PR VL6a kept the fsck allocation epoch in a
    /// separate side map.
    incarnations: scc::HashMap<u64, IncarnationCell>,
    /// Spec §6.2 item 6: `true` once ANY lifetime stamp exists on this
    /// allocator (minted at engagement, or seeded from a walked key).
    ///
    /// Without it [`Self::live_incarnation`] would run a hashed `scc` probe
    /// for every key mint on a mount that can hold no lifetime at all —
    /// which ruling D9 makes the ONLY mount that exists today. One relaxed
    /// load of an owned word replaces the probe whenever no lifetime can
    /// exist. **Predicted, not measured** (ruling D11 defers benches and
    /// brackets): the `write_block_key/persist_bare_default_slot` row holds
    /// its pre-item-6 value with this latch and regresses without it;
    /// falsification is that row moving either way past the bench group
    /// threshold once measurement is allowed.
    ///
    /// The latch is set BEFORE the stamp it covers becomes visible (both at
    /// engagement and at seeding), so the only reordering a relaxed reader
    /// can observe is `false` while a stamp already exists — which reads as
    /// [`crate::routing::INCARNATION_NONE`], i.e. §6.3's honest "unknown"
    /// degradation (serve, counted). It can never manufacture a refusal.
    stamps_present: std::sync::atomic::AtomicBool,
    /// Spec §6.2 item 6 (incompat bit 13): the mount's lifetime-stamp
    /// minter — `Some` only when incarnation keys are engaged, which
    /// requires every mounted meta volume to carry bit 13 and a durable
    /// writer term (bit 7). Since the rung-10b flip the DEFAULT format
    /// stamps bit 13, so every plain write mount engages (PR 6a's item-1
    /// verdict); `None` on `--single-writer`/pre-flip volumes and probe
    /// mounts, where every minted key stays the bare offset form.
    incarnation_minter: std::sync::OnceLock<IncarnationMinter>,
    /// DLM **S7** (pre-RC engineering spec §6.7 "Recovery"): this volume's
    /// dead-epoch **do-not-reallocate** quarantine —
    /// [`crate::data_custody::BlockQuarantine`]. Empty on every
    /// single-writer mount; the algorithm lives in `data_custody` so the
    /// law has one home.
    quarantine: crate::data_custody::BlockQuarantine,
    /// Spec **§6.8 item 3** (`crate::free_grace`): this volume's
    /// freed-offset **grace ring** — terminally-freed offsets held out of
    /// the free list until every live reader has acknowledged passing
    /// them. Untouched (one relaxed load) on every mount without a reader
    /// plane, which is every mount by default.
    grace: crate::free_grace::GraceRing,
    // --- The ahead-of-stall lane refill (design-free-grace-sustain §5.5,
    // PR 4) — all zero-cost words on unpartitioned mounts. ---
    /// **The explicit-ship arm's face**: blocks this mount's SHIPPED
    /// displaced frees left on the authority's list — +1 per `Freed`
    /// verdict `crate::cowriter::ship_displaced_frees` brings back, −1 per
    /// harvest adoption (clamped). Per-allocator BY CONSTRUCTION (the
    /// two-volume law: a global number cannot say which volume's authority
    /// holds supply); published as the sum `alloc_lane_owed_blocks`.
    ///
    /// NOT the refill gate (2026-09-07,
    /// `.benchmarks/2026-09-07-lane-refill-hint-gate.md`): a displaced
    /// block the authority RECOMPUTES when it serves this mount's layout
    /// publish reaches its list with nothing noted here — ≈ 90 % of a
    /// rewriting co-writer's displaced blocks on the s11 fleet — so this
    /// word is a strict subset of the authority's own count, which the
    /// renewal grant carries (`crate::free_grace::lane_supply_hint`). The
    /// proactive refills gate on either witness
    /// (`crate::free_grace::lane_supply_witnessed`); this word alone would
    /// leave every recomputed block reachable only from an ENOSPC park.
    lane_owed: AtomicU64,
    /// Publish schema 16: per harvested block index, the authority's
    /// per-client grant sequence it was adopted under
    /// ([`Self::adopt_lane_free_grant_at`]) — the ordering witness a
    /// lane-free notice's `after_grants` is compared against
    /// ([`Self::apply_lane_free_notice`]). An entry lives as long as the
    /// lifetime it tags: pruned when the local tracking retires, releases
    /// to zero, or recycles. Fresh mints carry no tag (a fresh offset was
    /// never handed out, so no notice can be ordered after its grant).
    harvest_grants: scc::HashMap<u64, u64>,
    /// Owed-ledger ARRIVALS (+1 per [`Self::note_owed_freed`]) — this
    /// allocator's half of the single-flight harvest's decline witness
    /// ([`Self::supply_witness_gen`]): the owed word itself falls at
    /// adoption, so a monotonic arrival count is what "the owed ledger
    /// moved" compares on.
    owed_arrivals: AtomicU64,
    /// The COMPOSED measured refill horizon (`hint + RTT + one refresh
    /// floor`), 0 = use the member-local derivation (OQ 2's fallback —
    /// the pre-first-reply state, a zero hint, and a refused reply all
    /// land here).
    horizon_composed_ms: AtomicU64,
    /// Allocation claims (the rate EWMA's input — every funnel exit).
    alloc_claims: AtomicU64,
    rate_last_sample_ms: AtomicU64,
    rate_last_claims: AtomicU64,
    /// EWMA allocation rate, milli-blocks/s.
    rate_mblk_per_s: AtomicU64,
    /// `ceil(rate × horizon)` capped at lane-share/4 — derived, never a
    /// knob (the A/B lever disarms the mechanism, not the number).
    harvest_watermark: AtomicU64,
    /// The capacity law's two published faces (hold-time campaign):
    /// `share_needed = ceil(rate × horizon) + live` and the headroom
    /// percentage against this lane's share; re-derived with the
    /// watermark at every rate sample.
    share_needed_blocks: AtomicU64,
    headroom_pct: AtomicU64,
    /// DLM **S9** blocker #3: this volume's **allocation partition**
    /// ([`crate::data_alloc_lane`]) — `None` on every mount today AND on
    /// every solo mount forever, which is what makes single-writer
    /// allocation not merely equivalent to the shipped path but literally
    /// it ([`Self::engage_alloc_lanes`] installs nothing at `writers == 1`).
    lanes: std::sync::OnceLock<LanePartition>,
    /// PR 8 (design-symmetric-metadata §5.5, KD-SYM-9): the ARMED
    /// symmetric plane's fresh-mint source — ranged block grants from
    /// this data volume's allocation-lease holder (`crate::block_grant`).
    /// `None` on every unarmed mount, where `next_fresh_block` is the
    /// shipped loop verbatim (one `OnceLock` probe).
    block_grant: std::sync::OnceLock<BlockGrantArm>,
}

/// The allocator's derived allocation truth at one instant (PR 8 —
/// [`BlockAllocator::derived_allocation_snapshot`]): the dense cursor,
/// the blocks below it that are NOT reallocatable now as ONE BIT PER
/// BLOCK (`set_words` — 8 B per 64 blocks: 512 KiB per 4 M-block volume,
/// the same order as the bitmap it seeds; review round 3, Issue 29a), and
/// the free list's blocks below it (`free` — bounded by the list).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedAllocation {
    pub highest: u64,
    set_words: Vec<u64>,
    pub free: Vec<u64>,
}

impl DerivedAllocation {
    /// `true` ⇔ `block` is below the cursor and not on the free list.
    pub fn is_set(&self, block: u64) -> bool {
        block < self.highest
            && self
                .set_words
                .get((block / 64) as usize)
                .is_some_and(|w| w & (1u64 << (block % 64)) != 0)
    }

    /// The SET blocks, ascending.
    pub fn set_blocks(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.highest).filter(move |b| self.is_set(*b))
    }

    /// The SET population.
    pub fn population(&self) -> u64 {
        self.set_words
            .iter()
            .map(|w| u64::from(w.count_ones()))
            .sum()
    }
}

/// The writer's grant arm on one data volume (see
/// [`BlockAllocator::install_block_grant_arm`]).
struct BlockGrantArm {
    vol_tag: u64,
    window: crate::block_grant::GrantWindow,
    /// The top-up ask (the holder in-process, `ManagerCall::BlockGrant`
    /// over the wire).
    sink: crate::block_grant::BlockGrantSink,
    topups: AtomicU64,
    /// One proactive ask in flight at a time.
    topup_inflight: std::sync::atomic::AtomicBool,
    /// Wakes an exhausted mint waiting on the proactive ask in flight.
    topup_done: squeezefs_ipc::sqz_notify::Notify,
    /// The allocator, for the detached proactive ask.
    me: std::sync::Weak<BlockAllocator>,
}

/// One mount's data-plane allocation partition (see
/// [`BlockAllocator::engage_alloc_lanes`]).
struct LanePartition {
    /// The appender descriptor — the SAME identity incompat bit 8's
    /// partitioned journal/bitmap/ledger and §6.2 item 5's ino lanes carry,
    /// so a volume never holds two disagreeing notions of "who is writer 2".
    part: crate::meta_backend::kv::journal::AppendPartition,
    /// Lane bitmask this mount may mint in: its own lane always, plus any
    /// lane [`BlockAllocator::adopt_lane`] has adopted under a drain proof.
    owned: AtomicU64,
    /// The durable reservation frontier (exclusive, **dense** block index) —
    /// raised through [`Self::sink`] for every owned lane before any mint
    /// reaches it, so a successor's
    /// [`crate::data_alloc_lane::recover_lane_floor`] starts above every
    /// index this mount could have minted.
    reserved_upto: AtomicU64,
    /// Fresh blocks one raise covers
    /// ([`crate::data_alloc_lane::reserve_grain_blocks`], resolved once at
    /// engagement — never per allocation).
    grain: u64,
    /// The durable sink. Absent on offline tools and unit fixtures: the
    /// reservation is then RAM-only and recovery falls back to the derived
    /// floor, which is exactly the pre-partition posture.
    sink: std::sync::OnceLock<crate::data_alloc_lane::LaneReserveSink>,
    /// The lane free HARVEST sink (rung 10, residual 2) — wired only on
    /// co-writer engagements ([`crate::alloc_lane_grant`]): at lane
    /// exhaustion, ask the authority for this lane's freed supply. Absent
    /// on the authority (its own free list already carries lane-0's
    /// supply) and everywhere unpartitioned.
    harvest: std::sync::OnceLock<crate::data_alloc_lane::LaneHarvestSink>,
    /// The supply-coupled rewrite-epoch close (finding 15's parked-supply
    /// term, design-rewrite-program §5.3) — installed only where a
    /// harvest sink is (a co-writer's lane): the refill tick closes the
    /// mount's open epochs when this lane's reachable supply sits below
    /// the watermark, so the parked A keys enter the recycle loop ahead
    /// of the `StorageFull` instead of at the iteration boundary.
    supply_close: std::sync::OnceLock<crate::data_alloc_lane::SupplyCloseSink>,
    /// The single-flight harvest rendezvous (finding 15 phase B1,
    /// `.benchmarks/2026-09-07-lane-harvest-single-flight.md`): one
    /// in-flight harvest RPC per allocator; every concurrent caller joins
    /// its outcome. Zero-cost words until a harvest runs.
    flight: HarvestFlight,
}

/// **The single-flight harvest rendezvous** — one per laned allocator
/// (finding 15 phase B1: on the 8-co-writer fleet every parked
/// allocation issued its OWN harvest per park slice — 124k RPCs in 9.5
/// min, half empty — and the authority's liveness serves queued behind
/// them). Latch-free: the leader is whoever wins `inflight`'s CAS; joiners
/// register on `notify` BEFORE re-reading `done_gen` (the enable-then-check
/// ordering — a leader completing between the failed CAS and the
/// registration is caught by the generation, one after it by the wake),
/// and read the outcome from `last_adopted` after the generation moved.
/// No lock is held across the RPC.
///
/// `empty_at_witness` is the DECLINE: the supply witness generation
/// ([`BlockAllocator::supply_witness_gen`]) at which the last reply came
/// back EMPTY (`u64::MAX` = none). While the witness has not moved —
/// no grant, no wake, no owed `Freed` — a fresh empty answer is the
/// answer, and a caller declines without a wire trip; the next grant
/// (the renewal cadence, ≤ 500 ms under an ask) ends the window for
/// exactly one RPC. An RPC FAILURE stamps nothing (it is not an answer).
struct HarvestFlight {
    inflight: AtomicBool,
    /// +1 at every flight's terminal outcome.
    done_gen: AtomicU64,
    /// The last flight's adopted count, published before `done_gen` moves.
    last_adopted: AtomicU64,
    empty_at_witness: AtomicU64,
    notify: squeezefs_ipc::sqz_notify::Notify,
}

impl HarvestFlight {
    const fn new() -> Self {
        Self {
            inflight: AtomicBool::new(false),
            done_gen: AtomicU64::new(0),
            last_adopted: AtomicU64::new(0),
            empty_at_witness: AtomicU64::new(u64::MAX),
            notify: squeezefs_ipc::sqz_notify::Notify::new(),
        }
    }
}

/// The `SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT` lever's latch (the
/// `HARVEST_AHEAD` shape): one in-flight harvest RPC per allocator with
/// the fresh-empty decline. `0` = one RPC per caller, the shipped shape —
/// the fleet A/B control.
static HARVEST_SINGLE_FLIGHT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn harvest_single_flight_enabled() -> bool {
    match HARVEST_SINGLE_FLIGHT.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on =
                crate::env_knobs::bool_knob("SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT", true);
            HARVEST_SINGLE_FLIGHT.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_harvest_ahead` shape).
pub fn test_set_harvest_single_flight(on: Option<bool>) {
    HARVEST_SINGLE_FLIGHT.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_harvest_single_flight() -> bool {
    HARVEST_SINGLE_FLIGHT.swap(0, Ordering::Relaxed) != 0
}

/// The `SQUEEZEFS_ALLOC_LANE_HARVEST_AHEAD` lever's latch (the free-grace
/// campaign's `ACK_PIPELINE` pattern): the ahead-of-stall lane refill
/// (design-free-grace-sustain §5.5). `0` = ENOSPC-triggered harvests
/// only, the shipped shape verbatim.
static HARVEST_AHEAD: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn harvest_ahead_enabled() -> bool {
    match HARVEST_AHEAD.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_ALLOC_LANE_HARVEST_AHEAD", true);
            HARVEST_AHEAD.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `free_grace::test_set_ack_pipeline` shape).
pub fn test_set_harvest_ahead(on: Option<bool>) {
    HARVEST_AHEAD.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_harvest_ahead() -> bool {
    HARVEST_AHEAD.swap(0, Ordering::Relaxed) != 0
}

/// The `SQUEEZEFS_COWRITER_LANE_PLACEMENT` lever's latch (same shape):
/// lane-aware write placement + allocation failover on a laned co-writer
/// (`.benchmarks/2026-09-07-cowriter-lane-aware-placement.md`). `0` = the
/// shipped device-fill pick and no failover, the fleet A/B control.
static COWRITER_LANE_PLACEMENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether a laned co-writer's placement is lane-governed (one relaxed
/// load after the first read).
pub fn cowriter_lane_placement_enabled() -> bool {
    match COWRITER_LANE_PLACEMENT.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob("SQUEEZEFS_COWRITER_LANE_PLACEMENT", true);
            COWRITER_LANE_PLACEMENT.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Test seam (the `test_set_harvest_ahead` shape).
pub fn test_set_cowriter_lane_placement(on: Option<bool>) {
    COWRITER_LANE_PLACEMENT.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// Test seam: `true` ⇔ a preset was in force.
pub fn test_clear_cowriter_lane_placement() -> bool {
    COWRITER_LANE_PLACEMENT.swap(0, Ordering::Relaxed) != 0
}

/// **The lane-counted free set** (sustain campaign KD-FG-10,
/// design-free-grace-sustain §5.4): the free list plus a lane-owned
/// population maintained INSIDE insert/remove — one modulo per mutation —
/// so the LANE-REACHABLE supply (the quantity that actually troughs on a
/// recycle-bound stream, where the global count accumulates foreign-lane
/// releases nobody here can consume) is correct by construction across
/// the full ten-site mutation census (the `pending_block_refs`
/// deferred-op-accumulator precedent; an eleventh site cannot drift).
/// Unpartitioned mounts (`writers == 0`) count everything, so the two
/// quantities coincide there. The fsck C6 per-lane recount is the drift
/// tripwire (`tests/mw_cowriter_free_tests.rs`).
///
/// The two trim edges (KD-4.4 — `remove_for_trim` … `insert_from_trim`)
/// move MEMBERSHIP but not the reachable count: a claimed offset leaves
/// the list for one device command and comes back, and the allocation
/// funnel parks on that return edge rather than refusing, so it is
/// pending supply, never a deficit. Before this (`.benchmarks/2026-09-08-
/// placement-refresh-race.md`) the count dipped by the trim batch for the
/// command's duration and a §5.9 refresh inside the window banded the
/// restocked volume's sibling alone.
#[derive(Debug, Default)]
struct LaneCountedSet {
    set: dashmap::DashSet<u64>,
    /// Blocks in lanes this mount owns that the allocation funnel can
    /// REACH: free-listed, or inside an open trim claim window. One load
    /// serves `lane_reachable_blocks`; the membership-exact lane-owned
    /// count is derived (`lane_owned`) by subtracting the owned windows.
    reachable_owned: AtomicU64,
    /// The open trim claim windows' block indices — the recount's input
    /// (a windowed block is a member of nothing else) and the return
    /// edge's witness that a claim preceded it.
    trim_windowed: dashmap::DashSet<u64>,
    /// Free-listed blocks PER LANE (finding 15 term 2 — the lane-supply
    /// hint's O(1) input: a co-writer's renewal grant carries its lane's
    /// count, so this is what keeps the count out of the renewal hot op
    /// and off the free-list scan). Membership-exact — the trim edges move
    /// it; meaningful only when `writers > 0`.
    per_lane: [AtomicU64; crate::alloc_lane_grant::MAX_LANES as usize],
    /// Partition width in force (0 = unpartitioned — everything is ours).
    writers: AtomicU64,
    /// Owned-lane bitmask (meaningful only when `writers > 0`).
    owned_mask: AtomicU64,
}

impl LaneCountedSet {
    /// The lane of `idx` under the partition in force (`None` when
    /// unpartitioned).
    #[inline]
    fn lane_of(&self, idx: u64) -> Option<usize> {
        let writers = self.writers.load(Ordering::Acquire);
        if writers == 0 {
            return None;
        }
        Some(crate::data_alloc_lane::block_lane_of(idx, writers as u16) as usize)
    }

    #[inline]
    fn is_ours(&self, idx: u64) -> bool {
        match self.lane_of(idx) {
            None => true,
            Some(lane) => self.owned_mask.load(Ordering::Acquire) & (1u64 << lane) != 0,
        }
    }

    /// `DashSet::insert` shape: `true` ⇔ newly inserted (and then, and only
    /// then, the lane counts move — each mutator adjusts by exactly its own
    /// membership delta, so the counts stay exact under races).
    fn insert(&self, idx: u64) -> bool {
        let new = self.set.insert(idx);
        if new {
            if let Some(lane) = self.lane_of(idx) {
                self.per_lane[lane].fetch_add(1, Ordering::AcqRel);
            }
            if self.is_ours(idx) {
                self.reachable_owned.fetch_add(1, Ordering::AcqRel);
            }
        }
        new
    }

    /// `DashSet::remove` shape: `Some` ⇔ this caller removed it.
    fn remove(&self, idx: &u64) -> Option<u64> {
        let out = self.set.remove(idx);
        if out.is_some() {
            if let Some(lane) = self.lane_of(*idx) {
                self.per_lane[lane].fetch_sub(1, Ordering::AcqRel);
            }
            if self.is_ours(*idx) {
                self.reachable_owned.fetch_sub(1, Ordering::AcqRel);
            }
        }
        out
    }

    /// The trim CLAIM (KD-4.4): membership out, the reachable count
    /// untouched — the offset is inside a window the funnel parks for.
    /// The set's `remove` arbitrates the claim; only the winner records
    /// the window, so a racing venue's lost claim never erases it.
    fn remove_for_trim(&self, idx: &u64) -> Option<u64> {
        let out = self.set.remove(idx);
        if out.is_some() {
            if let Some(lane) = self.lane_of(*idx) {
                self.per_lane[lane].fetch_sub(1, Ordering::AcqRel);
            }
            self.trim_windowed.insert(*idx);
        }
        out
    }

    /// The trim RETURN: membership back, the reachable count untouched
    /// for a block the window (or the recount that ran across it) already
    /// counts. `true` ⇔ a claim window closed. Without a preceding claim
    /// this is a plain `insert` — the accumulation shape the seeding
    /// contracts use — and no window ends.
    fn insert_from_trim(&self, idx: u64) -> bool {
        let windowed = self.trim_windowed.remove(&idx).is_some();
        let new = self.set.insert(idx);
        if new {
            if let Some(lane) = self.lane_of(idx) {
                self.per_lane[lane].fetch_add(1, Ordering::AcqRel);
            }
        }
        if self.is_ours(idx) {
            if !windowed && new {
                self.reachable_owned.fetch_add(1, Ordering::AcqRel);
            } else if windowed && !new {
                // Re-listed mid-window (a plain insert already counted the
                // member): the window's share leaves with the window.
                self.reachable_owned.fetch_sub(1, Ordering::AcqRel);
            }
        }
        windowed
    }

    /// The free-listed population of one lane (0 unpartitioned).
    #[inline]
    fn lane_count(&self, lane: u16) -> u64 {
        self.per_lane
            .get(lane as usize)
            .map(|c| c.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    #[inline]
    fn contains(&self, idx: &u64) -> bool {
        self.set.contains(idx)
    }

    #[inline]
    fn len(&self) -> usize {
        self.set.len()
    }

    #[inline]
    fn iter(&self) -> impl Iterator<Item = dashmap::setref::multiple::RefMulti<'_, u64>> {
        self.set.iter()
    }

    /// (Re)declare the partition in force and RECOUNT — the two
    /// control-plane sites (`engage_alloc_lanes`, `adopt_lane`) call this
    /// after moving the mask; mutations racing the recount converge on the
    /// next one (both sites are rare, and the tripwire contract pins the
    /// quiescent equality).
    fn set_partition(&self, writers: u16, owned_mask: u64) {
        self.writers.store(u64::from(writers), Ordering::Release);
        self.owned_mask.store(owned_mask, Ordering::Release);
        let mut per_lane = [0u64; crate::alloc_lane_grant::MAX_LANES as usize];
        let mut owned = 0u64;
        for idx in self.set.iter() {
            if let Some(lane) = self.lane_of(*idx) {
                per_lane[lane] += 1;
            }
            if self.is_ours(*idx) {
                owned += 1;
            }
        }
        for (c, n) in self.per_lane.iter().zip(per_lane) {
            c.store(n, Ordering::Release);
        }
        // A windowed block is on its way back to the list under the mask
        // in force NOW (an adoption mid-trim owns it from here), and its
        // return moves nothing — so the recount counts it here.
        let windowed_owned = self.windowed_owned();
        self.reachable_owned
            .store(owned + windowed_owned, Ordering::Release);
    }

    /// The owned blocks inside open trim claim windows (the recount's and
    /// the derived membership count's term; a scan of ≤ one trim batch).
    fn windowed_owned(&self) -> u64 {
        self.trim_windowed
            .iter()
            .filter(|idx| self.is_ours(**idx))
            .count() as u64
    }

    /// The lane-owned REACHABLE population (free-listed + windowed) — the
    /// counting half of the lane-reachable supply, one load.
    #[inline]
    fn reachable_owned(&self) -> u64 {
        self.reachable_owned.load(Ordering::Acquire)
    }

    /// The lane-owned FREE-LISTED population (membership-exact — the
    /// KD-FG-10 drift tripwire's quantity): the reachable count less the
    /// owned windows. A diagnostic read, never on the pick path.
    fn lane_owned(&self) -> u64 {
        self.reachable_owned().saturating_sub(self.windowed_owned())
    }
}

/// One offset's incarnation state: the loom-verified seqlock word plus,
/// since spec §6.2 item 6, its live lifetime stamp (0 = none recorded).
#[derive(Debug)]
struct IncarnationCell {
    word: AtomicU64,
    stamp: AtomicU64,
}

impl IncarnationCell {
    fn new(word: u64, stamp: u64) -> Self {
        Self {
            word: AtomicU64::new(word),
            stamp: AtomicU64::new(stamp),
        }
    }
}

/// Spec §6.2 item 6: the per-mount lifetime-stamp minter — the durable
/// era (writer term) plus this appender's lane cursor over the sequence
/// space. Composition and its rationale: [`crate::routing::compose_incarnation`].
#[derive(Debug)]
struct IncarnationMinter {
    era: u64,
    seq: crate::lane_core::LaneCursor,
}

impl BlockAllocator {
    pub async fn new(volume_id: &str) -> Result<Self> {
        Ok(Self {
            _volume_id: volume_id.to_string().into_boxed_str(),
            chunk_size: CHUNK_SIZE,
            free_blocks: LaneCountedSet::default(),
            highest_block: AtomicU64::new(0),
            capacity_blocks: AtomicU64::new(0),
            refcounts: scc::HashMap::new(),
            shared: scc::HashMap::new(),
            fsck_scan_epoch: AtomicU64::new(0),
            fsck_scan_active: std::sync::atomic::AtomicBool::new(false),
            fsck_epoch_map: scc::HashMap::new(),
            inflight: scc::HashMap::new(),
            space_valve: std::sync::OnceLock::new(),
            space_pending: std::sync::OnceLock::new(),
            elided_debt: scc::HashMap::new(),
            elided_debt_bytes: AtomicU64::new(0),
            trim_claimed: AtomicU64::new(0),
            trim_returned: squeezefs_ipc::sqz_notify::Notify::new(),
            incarnations: scc::HashMap::new(),
            stamps_present: std::sync::atomic::AtomicBool::new(false),
            incarnation_minter: std::sync::OnceLock::new(),
            quarantine: crate::data_custody::BlockQuarantine::new(),
            grace: crate::free_grace::GraceRing::derived(),
            lane_owed: AtomicU64::new(0),
            harvest_grants: scc::HashMap::new(),
            owed_arrivals: AtomicU64::new(0),
            horizon_composed_ms: AtomicU64::new(0),
            alloc_claims: AtomicU64::new(0),
            rate_last_sample_ms: AtomicU64::new(0),
            rate_last_claims: AtomicU64::new(0),
            rate_mblk_per_s: AtomicU64::new(0),
            harvest_watermark: AtomicU64::new(0),
            share_needed_blocks: AtomicU64::new(0),
            headroom_pct: AtomicU64::new(0),
            lanes: std::sync::OnceLock::new(),
            block_grant: std::sync::OnceLock::new(),
        })
    }

    // -----------------------------------------------------------------
    // PR 8 — ranged block grants (the armed symmetric plane's fresh mint;
    // `crate::block_grant`, contracts in tests/sym_block_grant_tests.rs).
    // The unarmed path is byte-identical: without an arm every method
    // below is one `OnceLock` probe.
    // -----------------------------------------------------------------

    /// Arm this allocator to mint from ranged block grants of data volume
    /// `vol_tag`, topped up through `sink`. The first arm wins (a mount's
    /// grant source never moves under live offsets).
    pub fn install_block_grant_arm(
        self: &Arc<Self>,
        vol_tag: u64,
        sink: crate::block_grant::BlockGrantSink,
    ) -> bool {
        self.block_grant
            .set(BlockGrantArm {
                vol_tag,
                window: crate::block_grant::GrantWindow::new(),
                sink,
                topups: AtomicU64::new(0),
                topup_inflight: std::sync::atomic::AtomicBool::new(false),
                topup_done: squeezefs_ipc::sqz_notify::Notify::new(),
                me: Arc::downgrade(self),
            })
            .is_ok()
    }

    /// Install a grant into the window (the initial grant, a top-up).
    /// `false` ⇔ no arm, or the grant is already installed.
    pub fn install_block_grant(&self, grant: crate::block_grant::BlockGrant) -> bool {
        self.block_grant
            .get()
            .is_some_and(|arm| arm.window.install(grant))
    }

    /// `true` ⇔ this allocator mints from block grants.
    pub fn block_grant_armed(&self) -> bool {
        self.block_grant.get().is_some()
    }

    /// The data volume tag the grant arm was installed for (`None` unarmed)
    /// — fsck's key into the allocation holding (the C6 bitmap oracle).
    pub fn block_grant_vol_tag(&self) -> Option<u64> {
        self.block_grant.get().map(|a| a.vol_tag)
    }

    /// The window's unconsumed blocks (0 unarmed).
    pub fn block_grant_remaining(&self) -> u64 {
        self.block_grant.get().map_or(0, |a| a.window.remaining())
    }

    /// The window's unconsumed ranges (what a clean leave returns).
    pub fn block_grant_unconsumed(&self) -> Vec<crate::block_grant::BlockGrant> {
        self.block_grant
            .get()
            .map(|a| a.window.unconsumed())
            .unwrap_or_default()
    }

    /// Top-ups asked (`block_grant_topups`).
    pub fn block_grant_topups(&self) -> u64 {
        self.block_grant
            .get()
            .map_or(0, |a| a.topups.load(Ordering::Relaxed))
    }

    /// Mints refused because the window was empty.
    pub fn block_grant_exhausted(&self) -> u64 {
        self.block_grant.get().map_or(0, |a| a.window.exhausted())
    }

    /// **The top-up** (the extent grant's 50 % law, §5.3.3, applied to
    /// blocks — refreshed as carriage, never expired): ask the sink for a
    /// derived-size grant when the window wants one. `true` ⇔ a grant was
    /// installed. The refill cadence's tick and the exhausted mint both
    /// run it.
    pub async fn block_grant_topup(&self) -> bool {
        let Some(arm) = self.block_grant.get() else {
            return false;
        };
        if !arm.window.wants_topup() {
            return false;
        }
        arm.topups.fetch_add(1, Ordering::Relaxed);
        let held = arm.window.remaining();
        match (arm.sink)(0, held).await {
            Some(grants) => grants
                .into_iter()
                .fold(false, |any, g| arm.window.install(g) || any),
            None => false,
        }
    }

    /// Wait for a PROACTIVE top-up in flight to land (bounded by the ask's
    /// own completion — the task always clears the latch). `true` ⇔ one
    /// was in flight and has completed.
    async fn block_grant_topup_join(&self) -> bool {
        let Some(arm) = self.block_grant.get() else {
            return false;
        };
        if !arm.topup_inflight.load(Ordering::Acquire) {
            return false;
        }
        loop {
            let done = arm.topup_done.notified();
            if !arm.topup_inflight.load(Ordering::Acquire) {
                return true;
            }
            done.await;
        }
    }

    /// The PROACTIVE half of the 50 % law (review round 1, Issue 7): a
    /// mint that left the window below its refill point asks for the
    /// top-up off the write path — one detached ask in flight per
    /// allocator, so a burst of mints costs one wire round trip, and the
    /// exhausted mint's inline ask stays the belt.
    fn kick_block_grant_topup(&self) {
        let Some(arm) = self.block_grant.get() else {
            return;
        };
        if !arm.window.wants_topup()
            || arm
                .topup_inflight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let Some(me) = arm.me.upgrade() else {
            arm.topup_inflight.store(false, Ordering::Release);
            arm.topup_done.notify_waiters();
            return;
        };
        crate::meta_exec::spawn_meta("block_grant_topup", async move {
            let _ = me.block_grant_topup().await;
            if let Some(arm) = me.block_grant.get() {
                arm.topup_inflight.store(false, Ordering::Release);
                arm.topup_done.notify_waiters();
            }
        });
    }

    /// **The allocator's derived truth for the allocation bitmap's FIRST
    /// hold** (PR 8 review round 2, Issue 23): every block below the dense
    /// cursor that is NOT on the local free list — the refcount map's
    /// population plus every offset in limbo between `begin_free` and
    /// `finish_free`, quarantined under a dead epoch, held in the grace
    /// ring or inside a trim window — is "not reallocatable now" and
    /// reads SET; the free list's blocks read CLEAR. The complement of the
    /// free list below the cursor is exactly the free-list definition the
    /// durable-refs seed / the derived walk already produced at mount.
    /// Taken at the arm, before FUSE serves — quiescent by construction.
    pub fn derived_allocation_snapshot(&self) -> DerivedAllocation {
        let highest = self.highest_block.load(Ordering::Acquire);
        let free: std::collections::BTreeSet<u64> = self
            .free_blocks
            .iter()
            .map(|item| *item)
            .filter(|b| *b < highest)
            .collect();
        // 1 bit per block below the cursor (Issue 29a): every bit set,
        // then the free list's cleared — O(highest / 64) words.
        let words = highest.div_ceil(64) as usize;
        let mut set_words = vec![u64::MAX; words];
        if highest % 64 != 0 {
            if let Some(last) = set_words.last_mut() {
                *last = (1u64 << (highest % 64)) - 1;
            }
        }
        for b in &free {
            set_words[(*b / 64) as usize] &= !(1u64 << (*b % 64));
        }
        DerivedAllocation {
            highest,
            set_words,
            free: free.into_iter().collect(),
        }
    }

    /// **Drain the local free list into the grant plane** (Issue 23): the
    /// list's blocks read CLEAR in the seeded bitmap and return only
    /// through a carve — the list is emptied so no path can hand them out
    /// beside the window. Returns the blocks drained.
    pub fn drain_free_list_into_grants(&self) -> usize {
        let idxs: Vec<u64> = self.free_blocks.iter().map(|item| *item).collect();
        let mut drained = 0;
        for idx in idxs {
            if self.free_blocks.remove(&idx).is_some() {
                drained += 1;
            }
        }
        drained
    }

    /// TEST seam (Issue 23's gate pin): plant `block_idx` on the LOCAL free
    /// list the way the mount-time gap fill or a pre-arm free would — an
    /// armed allocator must never mint it.
    pub fn test_plant_free_list(&self, block_idx: u64) {
        self.free_blocks.insert(block_idx);
    }

    // -----------------------------------------------------------------
    // DLM S9 blocker #3 — the data-plane allocation partition
    // (`crate::data_alloc_lane`; contracts in
    // tests/mw_data_alloc_lane_tests.rs). Fresh allocation is a residue
    // class per writer; FREES ARE UNTOUCHED — the owning lane is derivable
    // from the offset, so a writer frees a peer's block with no ownership
    // lookup, no message and no record.
    // -----------------------------------------------------------------

    /// Engage the allocation partition: this mount mints only lane
    /// `part.writer_id()` of `part.writers()`.
    ///
    /// **A solo partition installs nothing** and returns `Ok(())`: lane 0
    /// of 1 owns every index at stride 1, so installing state would only
    /// create a way for the shipped path to differ from itself. That is the
    /// single-writer byte-identity proof, made structural rather than
    /// argued.
    ///
    /// Idempotent — the first non-solo call wins, so a re-registration can
    /// never move a live mount's lane out from under offsets it has minted.
    pub fn engage_alloc_lanes(
        &self,
        part: crate::meta_backend::kv::journal::AppendPartition,
    ) -> Result<()> {
        if part.is_solo() {
            return Ok(());
        }
        if u32::from(part.writers()) > u64::BITS {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "allocation partition width {} exceeds the {} lanes a lane mask can hold",
                part.writers(),
                u64::BITS
            )));
        }
        let cap = self.capacity_blocks.load(Ordering::Relaxed);
        let installed = LanePartition {
            part,
            owned: AtomicU64::new(1u64 << part.writer_id()),
            // The frontier a fresh engagement starts from is the derived
            // one: `install_lane_floor` raises it from the durable records
            // when recovery has read them.
            reserved_upto: AtomicU64::new(self.highest_block.load(Ordering::Relaxed)),
            grain: crate::data_alloc_lane::reserve_grain_blocks(cap, part.writers()),
            sink: std::sync::OnceLock::new(),
            harvest: std::sync::OnceLock::new(),
            supply_close: std::sync::OnceLock::new(),
            flight: HarvestFlight::new(),
        };
        let grain = installed.grain;
        if self.lanes.set(installed).is_err() {
            return Ok(());
        }
        // KD-FG-10: the counting-set learns the partition and recounts —
        // from here the lane-owned population is the lane-reachable
        // supply's free-list half.
        self.free_blocks
            .set_partition(part.writers(), 1u64 << part.writer_id());
        let metrics = &crate::fuse_client::METRICS;
        metrics
            .alloc_lane_writers
            .store(u64::from(part.writers()), Ordering::Relaxed);
        metrics
            .alloc_lane_id
            .store(u64::from(part.writer_id()), Ordering::Relaxed);
        metrics.alloc_lanes_owned.store(1, Ordering::Relaxed);
        // Summed across this mount's data volumes (each engages once);
        // `adopt_lane` subtracts the lane share it reclaims.
        metrics.alloc_lane_stranded_bytes.fetch_add(
            crate::data_alloc_lane::stranded_blocks_bound(
                cap,
                part.writers(),
                1u64 << part.writer_id(),
            )
            .saturating_mul(self.chunk_size),
            Ordering::Relaxed,
        );
        log::info!(
            "data-plane allocation partition engaged on volume '{}': lane {} of {}, reservation \
             grain {grain} blocks, {} block(s) of this device belong to other lanes \
             (alloc_lane_stranded_bytes)",
            self._volume_id,
            part.writer_id(),
            part.writers(),
            crate::data_alloc_lane::stranded_blocks_bound(
                cap,
                part.writers(),
                1u64 << part.writer_id()
            ),
        );
        Ok(())
    }

    /// Wire the durable reservation sink (set once by the owning
    /// `BackendRouter`, exactly like [`Self::set_space_pressure_valve`]).
    /// Without it the reservation is RAM-only — the offline-tool posture.
    pub fn set_lane_reserve_sink(&self, sink: crate::data_alloc_lane::LaneReserveSink) {
        if let Some(lanes) = self.lanes.get() {
            let _ = lanes.sink.set(sink);
        }
    }

    /// Wire the lane free HARVEST sink (rung 10 — co-writer engagements
    /// only; see `LanePartition::harvest`).
    pub fn set_lane_harvest_sink(&self, sink: crate::data_alloc_lane::LaneHarvestSink) {
        if let Some(lanes) = self.lanes.get() {
            let _ = lanes.harvest.set(sink);
        }
    }

    /// Wire the supply-coupled rewrite-epoch close
    /// ([`crate::routing::DataRouter::arm_rewrite_supply_close`]). Installs
    /// ONLY on a harvesting (co-writer) lane and returns whether it did:
    /// the authority's own lane and every unpartitioned mount keep the
    /// shipped KD-1.6/1.7 triggers exactly — the scope is enforced here,
    /// in one place, not at each caller.
    pub fn set_lane_supply_close_sink(
        &self,
        sink: crate::data_alloc_lane::SupplyCloseSink,
    ) -> bool {
        let Some(lanes) = self.lanes.get() else {
            return false;
        };
        if lanes.harvest.get().is_none() {
            return false;
        }
        lanes.supply_close.set(sink).is_ok()
    }

    /// Whether a supply-coupled close is wired on this allocator (the
    /// scope contract's instrument).
    pub fn lane_supply_close_installed(&self) -> bool {
        self.lanes
            .get()
            .is_some_and(|l| l.supply_close.get().is_some())
    }

    /// This mount's partition, `None` ⇔ unpartitioned (every mount today).
    pub fn lane_partition(&self) -> Option<crate::meta_backend::kv::journal::AppendPartition> {
        self.lanes.get().map(|l| l.part)
    }

    /// The lane mask this mount may mint in (own + adopted). `None` ⇔
    /// unpartitioned.
    pub fn owned_lane_mask(&self) -> Option<u64> {
        self.lanes.get().map(|l| l.owned.load(Ordering::Acquire))
    }

    /// The durable reservation frontier (exclusive, dense block index).
    pub fn lane_reserved_upto(&self) -> Option<u64> {
        self.lanes
            .get()
            .map(|l| l.reserved_upto.load(Ordering::Acquire))
    }

    /// Raise the mint floor from recovery's answer
    /// ([`crate::data_alloc_lane::recover_lane_floor`]): monotone
    /// (`fetch_max`, so a stale floor can never regress a fresher mint) and
    /// idempotent under re-seeding.
    ///
    /// Both halves move together — the dense cursor (so no index below the
    /// floor is minted) and the reservation frontier (so the floor a
    /// predecessor durably claimed is not re-reserved).
    pub fn install_lane_floor(&self, floor: u64) {
        let Some(lanes) = self.lanes.get() else {
            return;
        };
        self.highest_block.fetch_max(floor, Ordering::AcqRel);
        lanes.reserved_upto.fetch_max(floor, Ordering::AcqRel);
    }

    /// **Adopt a lane whose holder is proven dead** — the ENOSPC/fairness
    /// answer (see the module docs and `docs/operations.md`): the adopted
    /// lane's free blocks and virgin share become allocatable here.
    ///
    /// The witness is S7's [`crate::data_custody::DeadEpoch`], i.e. the
    /// SAME drain proof [`Self::release_quarantine`] demands — a landed
    /// WERO preempt of the dead lane's host on a PR substrate, recovery's
    /// proof of death otherwise. Adoption needs **no new durable
    /// structure** because the reservation watermark is keyed on the LANE,
    /// not the holder: `Self::reserve_lane_frontier` declares the dense
    /// frontier for every owned lane, so any future holder of an adopted
    /// lane recovers above the indices we minted in it.
    ///
    /// The adopted lane's frontier starts from OUR dense cursor rather than
    /// from the dead holder's record, which can write that lane's record
    /// backwards. That is sound precisely because adoption demands a proof
    /// of death: nothing the dead holder minted can still be written, and
    /// our own subsequent raises dominate our own mints. It is also why a
    /// LIVE peer's lane is never adoptable.
    ///
    /// `false` ⇔ nothing changed (unpartitioned, own lane, out of range, or
    /// already adopted).
    pub fn adopt_lane(&self, lane: u16, proof: crate::data_custody::DeadEpoch) -> bool {
        let Some(lanes) = self.lanes.get() else {
            log::error!(
                "refusing to adopt lane {lane} on volume '{}': no allocation partition is \
                 engaged, so there are no lanes to adopt",
                self._volume_id
            );
            return false;
        };
        if lane >= lanes.part.writers() || lane == lanes.part.writer_id() {
            return false;
        }
        let bit = 1u64 << lane;
        let prev = lanes.owned.fetch_or(bit, Ordering::AcqRel);
        if prev & bit != 0 {
            return false;
        }
        let owned = prev | bit;
        // KD-FG-10: the adopted lane's free blocks join the lane-owned
        // population — recount under the new mask.
        self.free_blocks.set_partition(lanes.part.writers(), owned);
        let metrics = &crate::fuse_client::METRICS;
        metrics
            .alloc_lanes_owned
            .store(u64::from(owned.count_ones()), Ordering::Relaxed);
        metrics.alloc_lane_adoptions.fetch_add(1, Ordering::Relaxed);
        // The adopted lane's share stops being stranded (exact, so the
        // gauge stays closed across volumes and adoptions).
        metrics.alloc_lane_stranded_bytes.fetch_sub(
            crate::data_alloc_lane::lane_capacity_blocks(
                self.capacity_blocks.load(Ordering::Relaxed),
                lanes.part.writers(),
                lane,
            )
            .saturating_mul(self.chunk_size),
            Ordering::Relaxed,
        );
        log::warn!(
            "lane {lane} ADOPTED on volume '{}' under {proof} (its holder is proven dead): its \
             free blocks and virgin share are now allocatable here — alloc_lanes_owned={}",
            self._volume_id,
            owned.count_ones()
        );
        true
    }

    /// Free blocks this mount can never hand out because they belong to
    /// lanes it does not own — the number the ENOSPC refusal prints, and
    /// what an operator reads when "the device has space but writes fail".
    /// `0` when unpartitioned. On an authority this is also the supply it
    /// holds FOR its co-writers (`alloc_lane_supply_blocks`) — read off the
    /// free set's maintained counts (plus the ≤ one-trim-batch window scan
    /// the membership-exact owned count subtracts), never a free-list scan.
    pub fn foreign_lane_free_blocks(&self) -> u64 {
        if self.lanes.get().is_none() {
            return 0;
        }
        (self.free_blocks.len() as u64).saturating_sub(self.free_blocks.lane_owned())
    }

    /// `true` ⇔ `block_idx` is in a lane this mount may mint in (always
    /// `true` when unpartitioned — the shipped answer).
    pub(crate) fn lane_is_ours(&self, block_idx: u64) -> bool {
        match self.lanes.get() {
            None => true,
            Some(lanes) => {
                let lane = crate::data_alloc_lane::block_lane_of(block_idx, lanes.part.writers());
                lanes.owned.load(Ordering::Acquire) & (1u64 << lane) != 0
            }
        }
    }

    /// Raise the durable reservation so it covers `block_idx`, **before that
    /// offset is handed to a caller**. One `await`ed commit per
    /// [`crate::data_alloc_lane::reserve_grain_blocks`] fresh blocks **per
    /// owned lane**; free-list reuse never reaches here (a freed index is
    /// dominated by the derived floor, so it needs no new watermark) — which
    /// is what keeps the rewrite hot path at zero reservation work.
    ///
    /// The frontier is a **dense** index bound, and a raise declares the same
    /// bound for **every lane this mount owns** — so an adopted lane's future
    /// holder also recovers above the indices we minted in it. One commit on
    /// the shipped shape (a mount owns exactly its own lane); an adopting
    /// mount pays one per adopted lane per grain, which is the price of
    /// reaching a dead writer's space.
    async fn reserve_lane_frontier(&self, block_idx: u64) -> Result<()> {
        let Some(lanes) = self.lanes.get() else {
            return Ok(());
        };
        if block_idx < lanes.reserved_upto.load(Ordering::Acquire) {
            return Ok(());
        }
        let want = block_idx.saturating_add(lanes.grain).saturating_add(1);
        let owned = lanes.owned.load(Ordering::Acquire);
        match lanes.sink.get() {
            Some(sink) => {
                for lane in 0..lanes.part.writers() {
                    if owned & (1u64 << lane) == 0 {
                        continue;
                    }
                    sink(lane, want).await?;
                    crate::fuse_client::METRICS
                        .alloc_lane_reservations
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            None => {
                // No durable sink (offline tools, unit fixtures): the
                // frontier is RAM-only and recovery falls back to the
                // derived floor — the pre-partition posture, stated rather
                // than pretended.
                log::debug!(
                    "lane reservation for volume '{}' lane {} raised to {want} in RAM only (no \
                     durable sink wired)",
                    self._volume_id,
                    lanes.part.writer_id()
                );
            }
        }
        lanes.reserved_upto.fetch_max(want, Ordering::AcqRel);
        Ok(())
    }

    // -----------------------------------------------------------------
    // DLM S5 + S9 — the OWNERSHIP-ACCOUNTING gate (pre-RC engineering spec
    // §6.8 item 1: "the write gate extended past metadata to cover the
    // block allocator, the reclaim queue, W1 and in-place overwrite, and
    // `recover_active_blocks_v3`'s free-completing arm"; §6.2 item 7's
    // co-writer consumer half)
    // -----------------------------------------------------------------

    /// Refuse a mutation of this mount's **ownership accounting** for a
    /// device offset when the mount holds no authority over it.
    ///
    /// Every arm of this allocator that changes ownership of a device
    /// offset — mint, terminal free, specific claim, the recovery walk's
    /// free completion, the W1 patch's incarnation retire — routes through
    /// here. On a WRITE mount this is two relaxed atomic loads feeding a
    /// never-taken branch (`benches/write_path_bench.rs`, group
    /// `ro_gate`); the whole point of the latches' shape is that neither a
    /// reader nor a co-writer feature costs writers anything measurable.
    ///
    /// **Two refusal CLASSES, not one condition** (DLM S9 — the split the
    /// co-writer posture needs, and the reason this is no longer called
    /// `reader_gate`):
    ///
    /// | Posture | Data (DMA) authority | Accounting authority | This gate |
    /// |---|---|---|---|
    /// | writer | local | local | passes |
    /// | reader (S5) | none — EROFS at the FUSE door | none | refuses, `read_only_refusal` (unchanged text, unchanged sites) |
    /// | co-writer (S9) | **yes**, under a granted custody lease — authorized at [`crate::data_custody::authorize_dma`], NOT here | none: the durable answer is the authority's `TREE_BLOCK_REFS` | refuses, `co_writer_refusal` — naming the data-plane allocation partition |
    ///
    /// The distinction that makes this coherent: a device offset's
    /// OWNERSHIP is metadata, and metadata authority is what a co-writer
    /// lacks. Writing bytes into an offset it was granted is a different
    /// question, asked at a different door.
    ///
    /// **Under the ARMED symmetric plane the gate keys on a held LEASE**
    /// (design-symmetric-metadata §7.3, PR 12 — "`plane_gate` keys on 'do
    /// I hold this plane's lease'"): a grant-armed allocator (PR 8's
    /// `install_block_grant_arm` — the armed writer's fresh mint) answers
    /// for ITS data volume, and the ownership-accounting plane of that
    /// volume is the ALLOCATION LEASE — the bitmap, the terminal free, the
    /// grace ring, the quarantine live with its holder (§5.5). This mount
    /// passes iff it holds that lease (`alloc_lease::holding`) or is
    /// running the holder's own served-free act; a non-holder's terminal
    /// frees SHIP to the holder (`block_grant::free_target_for`), never
    /// run here. Every unarmed allocator takes the two shipped arms above
    /// verbatim — the posture-word arm never reads a lease, so `=0` and a
    /// bit-17-absent mount stay byte-identical.
    ///
    /// `&self` since PR 12 for exactly that reason: the armed question is
    /// per VOLUME (which lease), where the shipped one was the mount's
    /// (which posture).
    #[inline]
    fn plane_gate(&self, what: &str) -> Result<()> {
        if crate::fuse_client::read_only_mount() {
            let e = crate::fuse_client::read_only_refusal(what);
            log::error!("{e}");
            return Err(e);
        }
        if let Some(vol_tag) = self.block_grant_vol_tag() {
            if crate::meta_backend::kv::alloc_lease::holding(vol_tag).is_none()
                && !crate::cowriter::authority_accounting_scope_active()
            {
                let e = crate::fuse_client::lease_refusal(what, vol_tag);
                log::error!("{e}");
                return Err(e);
            }
            return Ok(());
        }
        // DLM S9 free path: the scope probe runs only INSIDE the co-writer
        // branch (a write mount never pays it). It marks the ONE venue that
        // legitimately runs this gate's arms while the process latch says
        // co-writer — the shipped-free EXECUTOR, i.e. the authority's own
        // accounting act for the set (`crate::cowriter::
        // with_authority_accounting`; in production the authority's posture
        // is `writer` and the probe never fires).
        if crate::fuse_client::co_writer_mount()
            && !crate::cowriter::authority_accounting_scope_active()
        {
            let e = crate::fuse_client::co_writer_refusal(what);
            log::error!("{e}");
            return Err(e);
        }
        Ok(())
    }

    /// [`Self::plane_gate`] for the **ALLOCATION** arms only (DLM S9 blocker
    /// #3's admission — `docs/design-mw-data-alloc-partition.md`,
    /// `crate::alloc_lane_grant`).
    ///
    /// A reader is refused exactly as before, with its own text and at every
    /// site. A **co-writer with an engaged lane** now PASSES, and nothing else
    /// about its posture moved:
    ///
    /// | Arm | Co-writer | Why |
    /// |---|---|---|
    /// | fresh allocation / free-list reuse / the two picks | **allowed with a lane** | its lane is a residue class no other writer mints in, and the durable reservation covering the offset is committed by the authority BEFORE the hand-out ([`Self::hand_out_reserved`]) |
    /// | the same, with NO lane | refused, unchanged | without a lane there is no disjointness: this mount's cursor and free list are a private opinion about shared hardware |
    /// | terminal free / `free_block` | refused, unchanged | a free's durable effect is the authority's `TREE_BLOCK_REFS` delete, and the device reclaim that follows is ceased on this posture |
    /// | `allocate_specific_block` | refused, unchanged | deliberately lane-BLIND (a clone/recovery path naming an index it already owns durably), which on a co-writer means claiming an offset in a lane it may not hold |
    /// | the W1 incarnation retire | refused, unchanged | it retires a LIFETIME, which is durable ownership state (§6.2 item 6) |
    /// | the ownership recovery walk | refused, unchanged | it declares gaps free from the tree it happened to see |
    ///
    /// Keyed on the ALLOCATOR's lane rather than on a mount-wide word, and
    /// that is not a drift from `plane_gate`'s "the posture is the MOUNT's"
    /// discipline: the mount-wide partition
    /// ([`crate::data_alloc_lane::mount_partition`]) is what a lane is
    /// engaged FROM, and this asks the narrower question the allocation arm
    /// actually needs — *is THIS volume's index space partitioned for me* —
    /// which is per-volume state by construction (each volume engages its
    /// own).
    ///
    /// **Armed (PR 12)**: the allocation plane's lease is the GRANT WINDOW
    /// — a grant-armed allocator mints from ranges its data volume's
    /// holder carved (PR 8), so the arm passes on the arm's existence and
    /// asks the posture word nothing; the posture arms below are the
    /// unarmed allocator's, byte-identical.
    #[inline]
    fn alloc_plane_gate(&self, what: &str) -> Result<()> {
        if crate::fuse_client::read_only_mount() {
            let e = crate::fuse_client::read_only_refusal(what);
            log::error!("{e}");
            return Err(e);
        }
        if self.block_grant_armed() {
            return Ok(());
        }
        if crate::fuse_client::co_writer_mount() && self.lanes.get().is_none() {
            let e = crate::fuse_client::co_writer_refusal(what);
            log::error!("{e}");
            return Err(e);
        }
        Ok(())
    }

    // DLM S7 — the dead-epoch allocation quarantine (pre-RC engineering
    // spec §6.7 "Recovery"; contracts in tests/dlm_data_fence_tests.rs).
    // The job wire's expired-lease destination quarantine applied
    // verbatim, ENFORCED here instead of asserted after the allocator has
    // already answered.
    // -----------------------------------------------------------------

    /// Admit `offset` to `epoch`'s do-not-reallocate cohort. `true` ⇔
    /// newly admitted.
    ///
    /// Admission **claims the offset out of the free list** (the
    /// [`Self::claim_free_for_trim`] protocol), so no allocation path —
    /// free-list claim, contiguity pick, ascending pick — can hand it out
    /// while the epoch is unproven. An offset a LIVE owner still holds is
    /// admitted too and stays with its owner: the entry then only gates
    /// the future free ([`Self::finish_free`] defers the publish), which is
    /// the shape the job wire's pre-allocated shard destinations take.
    ///
    /// Releasing needs a **drain proof** — [`Self::release_quarantine`].
    pub fn quarantine_offset(&self, offset: u64, epoch: crate::data_custody::DeadEpoch) -> bool {
        let admitted = self.quarantine.admit(offset, epoch);
        if admitted {
            // Out of the free list if it was in it: a quarantined offset
            // must be unreachable from every allocation path, not merely
            // filtered by one of them.
            if self
                .free_blocks
                .remove(&(offset / self.chunk_size))
                .is_some()
            {
                // It was free, so its free-list publish is what release
                // owes back (the terminal free already completed).
                self.quarantine.defer_free(offset);
            }
            log::warn!(
                "block {offset} quarantined under {epoch} (do-not-reallocate until the epoch \
                 is proven drained; dlm_quarantined_offsets)"
            );
        }
        admitted
    }

    /// `true` ⇔ `offset` is in the dead-epoch quarantine.
    pub fn is_quarantined(&self, offset: u64) -> bool {
        self.quarantine.contains(offset)
    }

    /// Live quarantined offsets on this volume.
    pub fn quarantined_count(&self) -> usize {
        self.quarantine.len()
    }

    /// **The drain proof**: release `epoch`'s whole cohort — the caller
    /// states the dead epoch can no longer submit DMA (the job wire's PR
    /// preempt of the victim host is exactly such a proof; on a
    /// detection-grade substrate it is recovery's proof of death). Offsets
    /// whose terminal free completed while quarantined are published to
    /// the free list HERE and only here. Returns the count released.
    pub fn release_quarantine(&self, epoch: crate::data_custody::DeadEpoch) -> usize {
        let released = self.quarantine.release(epoch);
        for (offset, free_pending) in &released {
            if *free_pending {
                // The drain proof clears the CUSTODY gate; the reader
                // coherence gate (spec §6.8 item 3) is separate and still
                // applies, so the publish routes through the grace period
                // exactly as a fresh terminal free's does. Unarmed, this is
                // the same free-list insert it always was.
                if self.grace.defer(*offset, self.chunk_size) {
                    self.harvest_grace();
                } else {
                    self.publish_grace_release(*offset);
                }
            }
        }
        if !released.is_empty() {
            log::info!(
                "{epoch} proven drained: {} quarantined offset(s) released ({} returned to the \
                 free list)",
                released.len(),
                released.iter().filter(|(_, pending)| *pending).count()
            );
        }
        released.len()
    }

    // -----------------------------------------------------------------
    // Idea 4 — discard-elision debt (design-rewrite-program §3; contracts
    // in tests/discard_elision_tests.rs)
    // -----------------------------------------------------------------

    /// Record one elided terminal free's unreturned discard debt.
    /// Called BEFORE `finish_free` (so a racing claim always observes the
    /// entry it must cancel).
    pub fn record_elided_debt(&self, offset: u64, bytes: u64) {
        let prev = match self.elided_debt.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(mut occ) => std::mem::replace(occ.get_mut(), bytes),
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(bytes);
                0
            }
        };
        if prev != 0 {
            // Replaced a lingering entry (re-free without an intervening
            // claim — cannot happen at steady state, reconciled anyway).
            self.elided_debt_bytes.fetch_sub(prev, Ordering::Relaxed);
            crate::fuse_client::METRICS
                .block_free_elided_debt_bytes
                .fetch_sub(prev, Ordering::Relaxed);
        }
        self.elided_debt_bytes.fetch_add(bytes, Ordering::Relaxed);
        crate::fuse_client::METRICS
            .block_free_elided_debt_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Claim-cancels-debt (KD-4.3): a reused offset owes no discard.
    /// One lock-free probe on a mostly-empty map when elision is idle.
    pub(crate) fn cancel_elided_debt(&self, offset: u64) {
        if let Some((_, bytes)) = self.elided_debt.remove_sync(&offset) {
            self.elided_debt_bytes.fetch_sub(bytes, Ordering::Relaxed);
            crate::fuse_client::METRICS
                .block_free_elided_debt_bytes
                .fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    /// Outstanding elided-debt bytes on THIS allocator (the per-target
    /// watermark input).
    pub fn elided_debt_bytes_local(&self) -> u64 {
        self.elided_debt_bytes.load(Ordering::Relaxed)
    }

    /// Take up to `max` debt entries for a trim pass (removed from the
    /// tracker — the pop is the ownership transfer; the trim's
    /// claim-from-free-list decides per offset whether anything issues).
    pub fn take_debt_batch(&self, max: usize) -> Vec<(u64, u64)> {
        let mut keys = Vec::new();
        // Spec §6.8 item 3: an offset held in the grace period is not on
        // the free list, so the trim's claim would LOSE and the taken debt
        // entry would evaporate with a discard still owed. Leaving it in the
        // tracker costs nothing (the trim venue is idle/pressure-driven and
        // comes back) and keeps the debt ledger honest. One relaxed load
        // when nothing is held.
        let graced = !self.grace.is_empty();
        self.elided_debt.iter_sync(|k, _| {
            if !graced || !self.grace.holds(*k) {
                keys.push(*k);
            }
            keys.len() < max
        });
        let mut out = Vec::new();
        for k in keys {
            if let Some((offset, bytes)) = self.elided_debt.remove_sync(&k) {
                self.elided_debt_bytes.fetch_sub(bytes, Ordering::Relaxed);
                crate::fuse_client::METRICS
                    .block_free_elided_debt_bytes
                    .fetch_sub(bytes, Ordering::Relaxed);
                out.push((offset, bytes));
            }
        }
        out
    }

    /// The never-minted (virgin) tail in bytes — the KD-4.6 watermark's
    /// derivation input. Unbounded allocators (capacity 0: offline
    /// tools / tests) report an infinite tail: pressure never fires.
    ///
    /// DLM S9: under an engaged partition only the OWNED lanes' share of
    /// that tail is this mount's to mint, so the answer is scaled by the
    /// owned-lane count. Reporting the dense tail would tell the discard
    /// watermark there is `W`× more virgin supply than this writer can
    /// reach — the same class of lie the stranded-capacity gauge exists to
    /// prevent.
    pub fn virgin_bytes(&self) -> u64 {
        let cap = self.capacity_blocks.load(Ordering::Relaxed);
        if cap == 0 {
            return u64::MAX;
        }
        let cursor = self.highest_block.load(Ordering::Relaxed).min(cap);
        let tail = cap - cursor;
        let tail = match self.lanes.get() {
            None => tail,
            Some(lanes) => {
                let owned = lanes.owned.load(Ordering::Acquire).count_ones() as u64;
                tail / u64::from(lanes.part.writers()) * owned
            }
        };
        tail.saturating_mul(self.chunk_size)
    }

    /// Trim claim (KD-4.4): take `offset` OUT of the free list — the
    /// allocation claim protocol — so no discard can ever race a new
    /// owner's DMA. The returned in-flight registration shields the
    /// claim window from fsck C6's limbo reconciliation (a mid-trim
    /// offset must never be "completed" back onto the free list). `None`
    /// = lost the claim (reused / racing trim): the offset owes nothing.
    ///
    /// The window is COUNTED before the remove: an allocation that scans
    /// the list empty and then reads `trim_claimed` must see the claim
    /// that emptied it (`await_trim_return` — the funnel's park on the
    /// window's return edge, `alloc_trim_window_parks`). The claim moves
    /// membership only: the offset stays in [`Self::lane_reachable_blocks`]
    /// (`LaneCountedSet::remove_for_trim`) because the funnel reaches it —
    /// a §5.9 refresh or a watermark tick inside the window must not read
    /// the batch as a deficit (`.benchmarks/2026-09-08-placement-refresh-race.md`).
    pub fn claim_free_for_trim(self: &Arc<Self>, offset: u64) -> Option<InflightAllocGuard> {
        let idx = offset / self.chunk_size;
        self.trim_claimed.fetch_add(1, Ordering::SeqCst);
        match self.free_blocks.remove_for_trim(&idx) {
            Some(_) => Some(self.inflight_register(offset)),
            None => {
                self.end_trim_claim();
                None
            }
        }
    }

    /// Return a trim-claimed offset to the free list (the claim window
    /// ends; the caller drops the in-flight guard after this). The
    /// insert precedes the count's release so a parked allocation that
    /// reads the window closed finds the offset on its rescan. A return
    /// no claim preceded is a plain free-list insert (the accumulation
    /// shape the seeding contracts use) and ends no window.
    pub fn return_from_trim(&self, offset: u64) {
        if self.free_blocks.insert_from_trim(offset / self.chunk_size) {
            self.end_trim_claim();
        }
    }

    fn end_trim_claim(&self) {
        self.trim_claimed.fetch_sub(1, Ordering::SeqCst);
        self.trim_returned.notify_waiters();
    }

    /// Park for an open trim claim window's return edge (contract 9,
    /// `tests/discard_elision_tests.rs`). `false` = no window is open —
    /// nothing to wait for, the caller's verdict stands. Registers BEFORE
    /// the re-check (`notified` registers at creation), so a return edge
    /// between the empty scan and the park is never lost. Bounded by the
    /// reclaimer's shipped park ceiling: a window is one batch of device
    /// commands, and one that outlives that ceiling is a STALLED trim —
    /// the caller's attempt cap then owns the verdict.
    async fn await_trim_return(&self) -> bool {
        let returned = self.trim_returned.notified();
        if self.trim_claimed.load(Ordering::SeqCst) == 0 {
            return false;
        }
        crate::fuse_client::METRICS
            .alloc_trim_window_parks
            .fetch_add(1, Ordering::Relaxed);
        let _ = squeezefs_ipc::sqz_time::timeout(
            std::time::Duration::from_millis(crate::block_reclaim::PARK_BOUND_CEILING_MS),
            returned,
        )
        .await;
        true
    }

    /// Wire the ENOSPC pressure valve (see the field doc). Set once by
    /// the owning `BackendRouter`; later calls are no-ops.
    pub fn set_space_pressure_valve(&self, valve: SpaceValve, pending: SpacePending) {
        let _ = self.space_valve.set(valve);
        let _ = self.space_pending.set(pending);
    }

    // -----------------------------------------------------------------
    // PR VL6a — fsck allocation-epoch side map + in-flight registry
    // (design-volume-lifecycle §5.6; the incarnation seqlock above is
    // deliberately untouched)
    // -----------------------------------------------------------------

    /// Arm the scan latch and bump the allocation epoch (fsck scan
    /// start). Returns the scan epoch.
    pub fn fsck_begin_scan(&self) -> u64 {
        let epoch = self.fsck_scan_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.fsck_scan_active.store(true, Ordering::SeqCst);
        epoch
    }

    /// Bump the epoch again at re-check (the two-epoch survival gate).
    pub fn fsck_bump_epoch(&self) -> u64 {
        self.fsck_scan_epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Disarm the latch and drain the side map (scan end).
    pub fn fsck_end_scan(&self) {
        self.fsck_scan_active.store(false, Ordering::SeqCst);
        self.fsck_epoch_map.clear_sync();
    }

    /// The epoch recorded for `offset` in the side map (`Some` ⇔ the
    /// offset was allocated while a scan was active — younger than the
    /// scan epoch, exempt from C2/C3 promotion).
    pub fn allocation_epoch_of(&self, offset: u64) -> Option<u64> {
        self.fsck_epoch_map.read_sync(&offset, |_, v| *v)
    }

    /// Register a live owner of an allocated-but-unpublished offset.
    /// The returned guard MUST be held until the owner's publish is
    /// durable and visible to the tree/refcount reads fsck performs
    /// (deregister-after-publish-visible — §5.6 registry contract), or
    /// until the owner's failure path frees the offset; dropping it on
    /// unwind is exactly right (a dead owner shields nothing).
    pub fn inflight_register(self: &Arc<Self>, offset: u64) -> InflightAllocGuard {
        match self.inflight.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(mut occ) => *occ.get_mut() += 1,
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(1);
            }
        }
        InflightAllocGuard {
            alloc: Arc::clone(self),
            offset,
        }
    }

    /// `true` ⇔ a live owner currently holds `offset` in flight.
    pub fn inflight_contains(&self, offset: u64) -> bool {
        self.inflight.read_sync(&offset, |_, _| ()).is_some()
    }

    /// Snapshot of the in-flight-registered offsets — fsck's C6
    /// aggregate-census exemption source (the same §5.6 live-owner
    /// shield `inflight_contains` serves per offset; since the
    /// write-wall manners law the begin_free→reclaim limbo legitimately
    /// spans whole foreground-busy periods as the deferred reclaim
    /// backlog).
    pub fn inflight_offsets(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.inflight.iter_sync(|k, _| {
            out.push(*k);
            true
        });
        out
    }

    /// Snapshot of the tracked (refcounted) population — fsck's C2/C3
    /// allocator-side ground truth.
    pub fn tracked_offsets(&self) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        self.refcounts.iter_sync(|k, v| {
            out.push((*k, crate::refcount_core::peek(v)));
            true
        });
        out
    }

    /// The free-list population (fsck C6 accounting).
    pub fn free_blocks_count(&self) -> u64 {
        self.free_blocks.len() as u64
    }

    /// The LANE-OWNED free-list population (sustain campaign KD-FG-10 —
    /// membership-exact: a block inside a trim claim window is not
    /// listed; equals `free_blocks_count` on unpartitioned mounts). The
    /// drift contract asserts it against the C6-style per-lane recount.
    pub fn lane_owned_free_blocks(&self) -> u64 {
        self.free_blocks.lane_owned()
    }

    /// The free-listed population of one lane (finding 15 term 2 — the
    /// lane-supply hint's input): on an authority, lane `w`'s count is the
    /// supply released to co-writer `w` that its next harvest RPC will
    /// take. 0 unpartitioned.
    pub fn lane_free_count(&self, lane: u16) -> u64 {
        self.free_blocks.lane_count(lane)
    }

    /// This volume's durable tag (the lane-visible ledger's key half).
    fn vol_tag(&self) -> u64 {
        crate::meta_backend::kv::block_refs::volume_tag(&self._volume_id)
    }

    /// Publish one grace-released offset to the free list and, when it is
    /// a co-writer's (a lane this mount does not own), mark its release
    /// for the lane-visible ledger (`alloc_lane_visible_phase_ns`): the
    /// block now waits on THIS list for that co-writer's harvest RPC.
    fn publish_grace_release(&self, offset: u64) {
        self.publish_free_list(offset);
        let idx = offset / self.chunk_size;
        if self.lanes.get().is_some() && !self.lane_is_ours(idx) {
            crate::free_grace::mark_lane_release(self.vol_tag(), idx);
        }
    }

    /// **The lane-reachable supply** (design-free-grace-sustain §5.4/§8):
    /// exactly `allocate_block`'s own reachable set — the lane-owned
    /// free-list population, the lane-owned blocks inside an open trim
    /// claim window (KD-4.4: the funnel parks on the window's return edge,
    /// so they are pending supply — `.benchmarks/2026-09-08-placement-
    /// refresh-race.md`), plus the lane-scoped virgin remainder
    /// (`virgin_bytes` already divides by the partition width). This is
    /// the quantity that troughs on a recycle-bound stream; the
    /// passed-global `free_supply_blocks` accumulates foreign-lane
    /// releases and provably never did on the motivating row.
    /// `u64::MAX` on an unbounded allocator (space is not a constraint).
    pub fn lane_reachable_blocks(&self) -> u64 {
        if let Some(armed) = self.grant_supply_blocks() {
            return armed;
        }
        let virgin = self.virgin_bytes();
        if virgin == u64::MAX {
            return u64::MAX;
        }
        (virgin / self.chunk_size).saturating_add(self.free_blocks.reachable_owned())
    }

    /// **Lane-governed placement** (`.benchmarks/2026-09-07-cowriter-lane-
    /// aware-placement.md`): `true` ⇔ this is a laned CO-WRITER's allocator
    /// — a partition engaged AND the lane free harvest wired (only the
    /// co-writer engagement wires it, `alloc_lane_grant::
    /// engage_allocator_lane`) — with `SQUEEZEFS_COWRITER_LANE_PLACEMENT`
    /// on. The §5.9 placement table then weighs this volume by its
    /// lane-reachable supply instead of the device fill, and the write
    /// path's allocation fails over to a sibling before it parks. Every
    /// single-writer and authority allocator answers `false` in one
    /// `OnceLock` probe, which is what keeps their placement byte-identical.
    pub fn lane_placement_governed(&self) -> bool {
        match self.lanes.get() {
            Some(lanes) if lanes.harvest.get().is_some() => cowriter_lane_placement_enabled(),
            _ => false,
        }
    }

    /// The blocks of this device the owned lanes hold in total (the
    /// lane-reachable supply's denominator — `lane_reachable_blocks ×
    /// 1000 ÷ lane_share_blocks` is the lane-governed placement weight).
    /// 0 unpartitioned or unbounded.
    pub fn lane_share_blocks(&self) -> u64 {
        let Some(lanes) = self.lanes.get() else {
            return 0;
        };
        let cap = self.capacity_blocks.load(Ordering::Relaxed);
        let owned = lanes.owned.load(Ordering::Acquire);
        (0..lanes.part.writers())
            .filter(|lane| owned & (1u64 << lane) != 0)
            .map(|lane| {
                crate::data_alloc_lane::lane_capacity_blocks(cap, lanes.part.writers(), lane)
            })
            .fold(0u64, u64::saturating_add)
    }

    /// PR VL6b (design-volume-lifecycle §5.6a, C3 **recount-and-set** /
    /// the C2-lost allocator-repair tail): set `offset`'s refcount to the
    /// verified counted-reference value. Repair-only seam — callers hold
    /// every referencing ino's DLM lease (ascending order) across the
    /// recount and this store; `count == 0` is refused (an unreferenced
    /// tracked offset is C2-leaked's business, freed through the full
    /// begin→purge→punch→finish law, never zeroed in place).
    pub fn fsck_set_refcount(&self, offset: u64, count: u32) -> bool {
        if count == 0 {
            return false;
        }
        match self.refcounts.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                occ.get().store(count, Ordering::SeqCst);
                true
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU32::new(count));
                true
            }
        }
    }

    /// PR VL6b (§5.6a, C6 **recompute-and-republish**): reconcile the
    /// derived used/free accounting with the tracked refcount population
    /// (both are mount-session RAM, rebuilt at mount — pure derived
    /// state). Two drift shapes are healed: a begin_free-limbo offset
    /// (untracked, not free-listed, no live in-flight owner — the wedged
    /// freer whose `finish_free` never came, provably dead after the
    /// finding survived two scan epochs + repair-time re-verification)
    /// gets its free completed; a tracked offset sitting on the free list
    /// (double-owned accounting) is pulled off it. Returns
    /// `(frees_completed, free_list_evictions)`.
    pub fn fsck_reconcile_accounting(&self) -> (u64, u64) {
        let highest = self.highest_block.load(Ordering::Relaxed);
        let mut frees_completed = 0u64;
        let mut free_list_evictions = 0u64;
        for idx in 0..highest {
            // DLM S9: a foreign lane's index is not this writer's to
            // reconcile. Under a partition the dense cursor spans peers'
            // indices, and "untracked and not free-listed" is the NORMAL
            // state of a peer's live block here — completing its free would
            // publish another writer's block into this one's free list.
            if !self.lane_is_ours(idx) {
                continue;
            }
            let offset = idx * self.chunk_size;
            let tracked = self.refcounts.read_sync(&offset, |_, _| ()).is_some();
            let free_listed = self.free_blocks.contains(&idx);
            // Spec §6.8 item 3: an offset held in the grace period is
            // untracked and deliberately not free-listed — its publish is
            // OWED to the readers' acknowledgement. "Completing" its free
            // here would both break the coherence promise and double-publish
            // it at the next harvest, so the ring is an exemption exactly
            // like the in-flight registry is.
            if !tracked
                && !free_listed
                && !self.inflight_contains(offset)
                && !self.grace.holds(offset)
            {
                self.free_blocks.insert(idx);
                frees_completed += 1;
            } else if tracked && free_listed {
                self.free_blocks.remove(&idx);
                free_list_evictions += 1;
            }
        }
        (frees_completed, free_list_evictions)
    }

    /// The fresh-block cursor (fsck C6 accounting: `used = highest −
    /// free`, the same arithmetic [`Self::get_used_blocks`] runs).
    pub fn highest_block_index(&self) -> u64 {
        self.highest_block.load(Ordering::Relaxed)
    }

    /// gen+1, stable=0 — offset owned by a writer whose data is not yet on the
    /// device (or retired by a free). Cache fills must not publish. Protocol
    /// core: [`crate::incarnation_core`] (loom-model-checked).
    fn mark_incarnation_unstable(&self, offset: u64) {
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                crate::incarnation_core::retire(&occ.get().word);
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(IncarnationCell::new(
                    crate::incarnation_core::UNSTABLE_FIRST,
                    crate::routing::INCARNATION_NONE,
                ));
            }
        }
    }

    /// Writer's durable device write for this incarnation completed — cache
    /// fills that observe an unchanged stable word may publish.
    pub fn publish_block(&self, offset: u64) {
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                crate::incarnation_core::publish(&occ.get().word);
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(IncarnationCell::new(
                    crate::incarnation_core::STABLE_FIRST,
                    crate::routing::INCARNATION_NONE,
                ));
            }
        }
    }

    // -----------------------------------------------------------------
    // Spec §6.2 item 6 — `offset ‖ incarnation` block keys (incompat bit
    // 11, ruling D9: built, NOT stamped). Design:
    // `docs/design-mw-cursors-and-incarnation.md`.
    // -----------------------------------------------------------------

    /// Engage lifetime stamping on this allocator: `era` is the mount's
    /// durable writer term (incompat bit 7) and `part` its appender lane.
    /// Idempotent — the first call wins, so a re-registration cannot
    /// restart the sequence space mid-mount.
    pub fn engage_incarnations(
        &self,
        era: u64,
        part: crate::meta_backend::kv::journal::AppendPartition,
    ) {
        self.stamps_present.store(true, Ordering::Release);
        let _ = self.incarnation_minter.set(IncarnationMinter {
            era,
            // Base 1: stamp sequence 0 is reserved for
            // `INCARNATION_NONE` ("this key names no lifetime").
            seq: crate::lane_core::LaneCursor::new(
                1,
                u64::from(part.writers()),
                u64::from(part.writer_id()),
                1,
            ),
        });
    }

    /// `true` ⇔ this allocator stamps lifetimes into the keys of the
    /// blocks it hands out (the item-6 engagement gauge; `false` on every
    /// volume today).
    pub fn incarnations_engaged(&self) -> bool {
        self.incarnation_minter.get().is_some()
    }

    /// Mint the next lifetime stamp, or [`crate::routing::INCARNATION_NONE`]
    /// when stamping is disengaged (the shipped path) or the per-mount
    /// sequence space is exhausted.
    ///
    /// Exhaustion degrades to NONE — loudly and counted — never to a
    /// wrapped stamp: losing detection is recoverable by a remount (the
    /// era advances), while a wrapped stamp would ALIAS a live lifetime,
    /// which is the exact failure this structure exists to prevent. The
    /// space is 2^40 allocations per mount; at the shipped 4 MiB block
    /// that is 4 EiB of fresh blocks in one mount.
    fn mint_incarnation(&self) -> u64 {
        let Some(minter) = self.incarnation_minter.get() else {
            return crate::routing::INCARNATION_NONE;
        };
        let seq = minter.seq.mint();
        match crate::routing::compose_incarnation(minter.era, seq) {
            Some(stamp) => stamp,
            None => {
                log::error!(
                    "block-key incarnation space exhausted on volume '{}' (era {}, seq {}) — \
                     degrading to unstamped keys: stale-binding DETECTION is lost until \
                     remount (which advances the durable era), but no lifetime is ever \
                     aliased (spec §6.2 item 6)",
                    self._volume_id,
                    minter.era,
                    seq
                );
                crate::fuse_client::METRICS
                    .block_key_incarnation_exhausted
                    .fetch_add(1, Ordering::Relaxed);
                crate::routing::INCARNATION_NONE
            }
        }
    }

    /// The offset's live lifetime stamp — what
    /// [`crate::routing::BackendRouter::persist_block_key`] writes into a
    /// key and what the read/free paths validate against.
    /// [`crate::routing::INCARNATION_NONE`] = no lifetime recorded for this
    /// offset (never allocated by this mount, or stamping disengaged),
    /// which the validators treat exactly as pre-item-6 behavior.
    pub fn live_incarnation(&self, offset: u64) -> u64 {
        // The shipped path: no lifetime can exist on this allocator, so the
        // answer is NONE without touching the map (see `stamps_present`,
        // whose cost claim is a D11-deferred prediction, not a measurement).
        if !self.stamps_present.load(Ordering::Relaxed) {
            return crate::routing::INCARNATION_NONE;
        }
        self.incarnations
            .read_sync(&offset, |_, v| v.stamp.load(Ordering::Acquire))
            .unwrap_or(crate::routing::INCARNATION_NONE)
    }

    /// Record the lifetime a persisted key names for `offset` **without**
    /// minting one — first-touch seeding from the keys mount recovery
    /// walks (`Self::owned_offset`), so an offset laid down by a previous
    /// mount presents its real era instead of "unknown".
    ///
    /// Never overwrites a stamp: an allocation (`Self::claim_block_idx`)
    /// is the only event that changes an offset's lifetime, and a walked
    /// key must not be able to talk this mount out of the lifetime it just
    /// minted.
    pub fn seed_incarnation(&self, offset: u64, inc: u64) {
        if inc == crate::routing::INCARNATION_NONE {
            return;
        }
        // A seeded stamp is a live lifetime even on a mount that mints none
        // (a read-only mount of a stamped volume), so the fast-path latch
        // must cover it.
        self.stamps_present.store(true, Ordering::Release);
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                let _ = occ.get().stamp.compare_exchange(
                    crate::routing::INCARNATION_NONE,
                    inc,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            scc::hash_map::Entry::Vacant(vac) => {
                // No seqlock history for this offset yet: a walked key
                // names a durably-written block, so the word starts
                // STABLE exactly as `publish_block` would leave it.
                let _ = vac.insert_entry(IncarnationCell::new(
                    crate::incarnation_core::STABLE_FIRST,
                    inc,
                ));
            }
        }
    }

    /// **The served publish's DMA witness for a FOREIGN-lane offset**
    /// (finding 51, `.benchmarks/2026-09-07-read-settle-lost-serialized-
    /// authority.md`): the authority just committed a peer's layout publish
    /// that binds `block_idx`, so the peer's device write behind it is
    /// complete (a co-writer publishes strictly after its DMA) — publish
    /// this allocator's word for the offset exactly as the local writer's
    /// DMA-complete [`Self::publish_block`] would. Returns `true` ⇔ the
    /// word was published.
    ///
    /// Why the authority needs it: the incarnation seqlock is per PROCESS.
    /// A co-writer's displaced block is freed THROUGH the authority
    /// ([`Self::begin_free`] retires the authority's word), returns to the
    /// lane's supply, is harvested by the co-writer and minted again — and
    /// the co-writer's own `publish_block` stabilizes the co-writer's word,
    /// never this one. The authority can never claim a foreign-lane
    /// offset, so without this witness its word stayed retired for ever
    /// and every later fill of the recycled key failed validation: the
    /// s11-mpiio row's 326 `read_settle_lost_serialized` tripwires and 101
    /// fsync EIOs, every one a co-writer block the authority had freed once.
    ///
    /// The two edges it keeps: between the authority's free of the offset
    /// and this witness the word stays RETIRED (a straggler fill of the
    /// dead lifetime during the co-writer's DMA must not publish into the
    /// authority's tiers — the seqlock's whole purpose), and an OWN-lane
    /// offset is never touched (`false`): its word belongs to the local
    /// claim → DMA → publish protocol, and a peer's publish naming one is
    /// either a clone of a block already stable or a stale view the compose
    /// dropped. Unpartitioned allocators own every lane, so a solo mount
    /// never reaches the publish.
    ///
    /// What it does NOT touch — the cell's lifetime STAMP (`live_incarnation`,
    /// the number `incarnation_ok` compares a key's `@stamp` against). The
    /// seqlock word and the stamp are two fields: this publishes the word's
    /// stable bit and leaves the stamp exactly as the authority's own mints
    /// (`claim_block_idx`) or the mount walk (`seed_incarnation`) left it —
    /// for a foreign-lane offset on a fresh fleet that is `INCARNATION_NONE`,
    /// which `incarnation_ok` reads as "unknown, accept" (§6.3) before and
    /// after this witness. The phase-B1 `STALE BLOCK-KEY BINDING` storm
    /// (`.benchmarks/2026-09-07-read-settle-lost-serialized-authority.md`
    /// §8) was therefore never this function's: every refused offset was
    /// LANE 0 — the authority's own mints, whose stamps this function never
    /// reaches (`lane_is_ours` returns before the publish).
    pub fn witness_served_binding(&self, block_idx: u64) -> bool {
        if self.lane_is_ours(block_idx) {
            return false;
        }
        self.publish_block(block_idx.saturating_mul(self.chunk_size));
        true
    }

    /// Snapshot the incarnation word for a fill. `None` while unstable
    /// (in-flight write or retired/free) — the fill must not publish. Offsets
    /// with no recorded incarnation (written before this process / by another
    /// node) are treated as stable.
    ///
    /// On an AUTHORITY the recorded incarnations include every foreign-lane
    /// offset it ever freed for a peer; those words are re-published by
    /// the served publish that re-adopts the offset
    /// ([`Self::witness_served_binding`]), so "unstable" here means
    /// mid-transition on SOME node, never "this node cannot know".
    pub fn fill_incarnation(&self, offset: u64) -> Option<u64> {
        match self
            .incarnations
            .read_sync(&offset, |_, v| crate::incarnation_core::snapshot(&v.word))
        {
            Some(snap) => snap,
            None => Some(crate::incarnation_core::UNKNOWN_STABLE),
        }
    }

    /// True if the incarnation word is unchanged since [`Self::fill_incarnation`]
    /// (no allocate/publish/free transitioned the offset during the fill's
    /// device read).
    pub fn fill_incarnation_still(&self, offset: u64, before: u64) -> bool {
        match self.incarnations.read_sync(&offset, |_, v| {
            crate::incarnation_core::still(&v.word, before)
        }) {
            Some(unchanged) => unchanged,
            None => before == crate::incarnation_core::UNKNOWN_STABLE,
        }
    }

    /// Take one reference on `offset`'s block. Returns `false` (loudly) when
    /// the reference was NOT taken: the count already hit its terminal zero
    /// (racing free) or the offset is untracked — callers must treat the
    /// block as gone and re-resolve, never proceed unpinned.
    #[must_use]
    pub fn increment_refcount(&self, offset: u64) -> bool {
        match self.refcounts.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                // Acquire-from-nonzero (loom-modeled, crate::refcount_core):
                // a plain fetch_add could resurrect a count a concurrent
                // free_block just took to zero, leaving this clone holding a
                // freed (reallocatable) offset.
                let taken = crate::refcount_core::try_acquire(occ.get());
                if !taken {
                    log::warn!(
                        "increment_refcount raced a free for offset {offset}: reference not taken"
                    );
                }
                taken
            }
            scc::hash_map::Entry::Vacant(_) => {
                // Unknown offset: it was never allocated by this process or
                // its count already hit zero and was removed. Fabricating a
                // fresh count for a possibly free-listed offset would alias
                // a future allocation — refuse loudly instead.
                log::warn!("increment_refcount on untracked offset {offset}: reference not taken");
                false
            }
        }
    }

    /// Read-only refcount accessor (design-random-small-writes §5.1
    /// predicate 4 / review Issue 10 — RW2 adds it; only
    /// [`Self::increment_refcount`] existed before). `None` = untracked
    /// offset (never allocated by this process / already freed) — callers
    /// must treat it as NOT provably sole-owned.
    pub fn refcount(&self, offset: u64) -> Option<u32> {
        self.refcounts
            .read_sync(&offset, |_, v| crate::refcount_core::peek(v))
    }

    // -----------------------------------------------------------------
    // DLM S9 — the co-writer FREE path's two allocator seams
    // (`crate::cowriter`; contracts tests/mw_cowriter_free_tests.rs).
    // -----------------------------------------------------------------

    /// `true` ⇔ `block_idx` is on the free list — the shipped-free
    /// executor's already-free probe (one lock-free contains, beside
    /// [`Self::grace_holds`] / [`Self::is_quarantined`] /
    /// [`Self::inflight_contains`], the other three states a
    /// double-released offset can be found in).
    pub fn free_list_contains(&self, block_idx: u64) -> bool {
        self.free_blocks.contains(&block_idx)
    }

    /// **Seed ONE reference for a peer-minted block whose terminal free
    /// this authority is about to execute** (the shipped-free executor's
    /// untracked arm): the co-writer minted the offset in its own lane, so
    /// this allocator never tracked it — and `begin_free`'s untracked
    /// refusal (correct everywhere else: it is the double-release
    /// tripwire) would otherwise refuse a legitimate first release.
    ///
    /// Deliberately NOT [`Self::recover_block`]: its gap-filling arm
    /// free-lists every index between the cursor and the target, and on a
    /// partitioned device those gaps are LIVE PEERS' residue classes —
    /// declaring a live co-writer's minted-but-unpublished tail "free" is
    /// exactly the §3.1 zombie window the lane reservation exists to
    /// close. This seeds the one entry and nothing else: no cursor move,
    /// no gap fill.
    ///
    /// `true` ⇔ seeded; `false` ⇔ an entry already exists (a racing seed
    /// or a live count — the caller's `begin_free` arbitrates).
    pub fn seed_shipped_free_reference(&self, offset: u64) -> bool {
        self.refcounts
            .insert_sync(offset, AtomicU32::new(1))
            .is_ok()
    }

    /// **Claim one free-listed block for a lane-harvest handout** (rung 10,
    /// residual 2 — the AUTHORITY side of
    /// [`crate::cowriter::execute_lane_harvest`]): remove it from this
    /// allocator's free list so no path here can ever hand it out again —
    /// the receiving lane holder is its one next owner. Free-list
    /// membership already implies not-quarantined / not-graced /
    /// not-inflight (quarantine admission and the grace ring both pull
    /// offsets OUT of the list), which is the same invariant
    /// `try_allocate_block` stands on. The claim-cancels-debt law applies
    /// (KD-4.3): the new owner's write-before-publish rewrites the range,
    /// so it owes no discard. `true` ⇔ this call won the removal.
    pub fn take_free_for_lane_grant(&self, block_idx: u64) -> bool {
        if self.free_blocks.remove(&block_idx).is_none() {
            return false;
        }
        self.cancel_elided_debt(block_idx * self.chunk_size);
        true
    }

    /// **Adopt a lane-free grant** (rung 10 — the CO-WRITER side): insert
    /// harvested block indices into this mount's own free list, where the
    /// ordinary free-list-first allocation funnel serves them back with the
    /// full claim discipline (refcount, fresh incarnation — the co-writer's
    /// own lane-partitioned minter, so the new lifetime stamp can collide
    /// with nobody's). A foreign-lane index is refused loud (the authority
    /// mis-serving a lane is exactly the collision the partition forbids);
    /// a duplicate is refused loud (a double handout is the two-owners
    /// lineage). Returns the count adopted. The untagged form (grant
    /// sequence 0 — a lifetime no lane-free notice can ever be ordered
    /// after); the harvest RPC adopts through
    /// [`Self::adopt_lane_free_grant_at`].
    pub fn adopt_lane_free_grant(&self, block_idxs: &[u64]) -> u64 {
        self.adopt_lane_free_grant_at(block_idxs, 0)
    }

    /// [`Self::adopt_lane_free_grant`] under the authority's per-client
    /// `grant_seq` (publish schema 16): every adopted block is tagged with
    /// it BEFORE it reaches the free list, so a lane-free notice whose
    /// `after_grants` is below the tag is recognised as naming the offset's
    /// PREVIOUS lifetime — the one this grant replaced — and touches
    /// nothing ([`Self::apply_lane_free_notice`]). A tag of 0 records
    /// nothing.
    pub fn adopt_lane_free_grant_at(&self, block_idxs: &[u64], grant_seq: u64) -> u64 {
        let mut adopted = 0u64;
        for idx in block_idxs {
            if !self.lane_is_ours(*idx) {
                log::error!(
                    "refusing to adopt harvested block {idx} on volume '{}': its lane is not \
                     this mount's — a mis-served harvest would mint two owners for one offset",
                    self._volume_id
                );
                continue;
            }
            if grant_seq != 0 {
                match self.harvest_grants.entry_sync(*idx) {
                    scc::hash_map::Entry::Occupied(mut occ) => *occ.get_mut() = grant_seq,
                    scc::hash_map::Entry::Vacant(vac) => {
                        let _ = vac.insert_entry(grant_seq);
                    }
                }
            }
            if !self.free_blocks.insert(*idx) {
                log::error!(
                    "refusing to adopt harvested block {idx} on volume '{}': it is already on \
                     this mount's free list — the double-handout lineage",
                    self._volume_id
                );
                continue;
            }
            adopted += 1;
        }
        if adopted > 0 {
            let m = &crate::fuse_client::METRICS;
            m.alloc_lane_harvested_blocks
                .fetch_add(adopted, Ordering::Relaxed);
            // §5.5: the adoption pays the owed ledger down (clamped — the
            // ENOSPC path can adopt supply the ledger never counted, e.g.
            // frees shipped before this binary), mirroring the actually
            // subtracted amount into the process sum gauge.
            let paid = loop {
                let owed = self.lane_owed.load(Ordering::Acquire);
                let pay = owed.min(adopted);
                if self
                    .lane_owed
                    .compare_exchange(owed, owed - pay, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    break pay;
                }
            };
            if paid > 0 {
                m.alloc_lane_owed_blocks.fetch_sub(paid, Ordering::Relaxed);
            }
        }
        adopted
    }

    /// §5.5 (the owed ledger's increment): `n` of this mount's shipped
    /// displaced frees came back `Freed` — the authority's list now holds
    /// supply this mount is owed. Called by
    /// `crate::cowriter::ship_displaced_frees` per acknowledged group —
    /// the explicit-ship arm ONLY (see the `lane_owed` field doc).
    pub fn note_owed_freed(&self, n: u64) {
        if n == 0 {
            return;
        }
        self.lane_owed.fetch_add(n, Ordering::AcqRel);
        self.owed_arrivals.fetch_add(1, Ordering::Release);
        crate::fuse_client::METRICS
            .alloc_lane_owed_blocks
            .fetch_add(n, Ordering::Relaxed);
    }

    /// **The supply witness generation** — the single-flight harvest's
    /// decline compares on it: the sum of two monotonic arrival counts,
    /// the grants that moved the authority's advertisement of THIS
    /// VOLUME's lane supply ([`crate::free_grace::lane_supply_hint_gen_for`]
    /// — a nonzero advertisement of it, or a grant that did not name it;
    /// the mount-wide arrival count under `SQUEEZEFS_ALLOC_LANE_VOLUME_HINT=0`)
    /// and this allocator's owed `Freed` arrivals. Either moving moves the
    /// sum; neither moving means the authority has told this mount nothing
    /// new about its lane's supply ON THIS VOLUME, so a fresh empty
    /// harvest reply still stands — a grant that only moved the sibling's
    /// share re-arms no RPC here
    /// (`.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md`).
    fn supply_witness_gen(&self) -> u64 {
        crate::free_grace::lane_supply_hint_gen_for(self.vol_tag())
            .wrapping_add(self.owed_arrivals.load(Ordering::Acquire))
    }

    /// Blocks this mount's SHIPPED frees left on the authority's free list
    /// (§5.5 — the explicit-ship arm's face, see the `lane_owed` field doc).
    pub fn lane_owed_blocks(&self) -> u64 {
        self.lane_owed.load(Ordering::Acquire)
    }

    /// The authority's advertised lane supply ON THIS VOLUME — the grant
    /// vector's entry for it ([`crate::free_grace::lane_supply_hint_for`]),
    /// the mount sum when no vector names it or under
    /// `SQUEEZEFS_ALLOC_LANE_VOLUME_HINT=0`.
    fn lane_supply_hint(&self) -> u64 {
        crate::free_grace::lane_supply_hint_for(self.vol_tag())
    }

    /// The refill's supply witness for this allocator: the authority's
    /// advertised lane supply on this volume (`lane_supply_hint_for`)
    /// or this allocator's owed word
    /// ([`crate::free_grace::lane_supply_witnessed`]).
    fn lane_supply_witnessed(&self) -> bool {
        crate::free_grace::lane_supply_witnessed(
            self.lane_supply_hint(),
            self.lane_owed.load(Ordering::Acquire),
        )
    }

    /// The hint-refill ledger (`alloc_lane_hint_refills`): a proactive
    /// harvest fired with the owed word at 0 is one the retired owed gate
    /// would have declined — the hint armed it.
    fn note_proactive_harvest_witness(&self) {
        if self.lane_owed.load(Ordering::Acquire) == 0 {
            crate::fuse_client::METRICS
                .alloc_lane_hint_refills
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One refresh-floor beat, ms — the horizon's slack term (the reader's
    /// own pass-cadence floor; §5.5's `+ one ack_refresh_floor`).
    pub fn horizon_floor_ms(&self) -> u64 {
        (crate::ro_coherence::reader_revalidate_interval().as_millis() as u64).max(1)
    }

    /// The member-local horizon derivation — OQ 2's FALLBACK (the
    /// pre-first-reply state, a zero hint, and every refused reply):
    /// `qualify_lag + drain_lag + 2 × refresh floor`, from the same
    /// published numbers the ack ladder runs on (session clock terms when
    /// a member session exists; the staleness bound alone otherwise).
    fn fallback_horizon_ms(&self) -> u64 {
        let staleness = crate::ro_coherence::reader_staleness_bound().as_millis() as u64;
        let (skew, d_purge) = match crate::membership::installed_member() {
            Some(s) => (s.skew_max_ms(), s.d_purge_ms()),
            None => (0, staleness),
        };
        (staleness + skew) + (staleness + d_purge) + 2 * self.horizon_floor_ms()
    }

    /// §5.5 (OQ 2): deposit a harvest reply's horizon inputs. A nonzero
    /// hint composes `hint + RTT + one refresh floor` and is COUNTED
    /// (`alloc_lane_horizon_hints`); a zero hint (nothing held) clears the
    /// word back to the derivation — a quiet ring is never over-trusted.
    pub fn note_harvest_hint(&self, bound_age_hint_ms: u64, rtt_ms: u64) {
        if bound_age_hint_ms == 0 {
            self.horizon_composed_ms.store(0, Ordering::Relaxed);
            return;
        }
        let composed = bound_age_hint_ms
            .saturating_add(rtt_ms)
            .saturating_add(self.horizon_floor_ms());
        self.horizon_composed_ms.store(composed, Ordering::Relaxed);
        let m = &crate::fuse_client::METRICS;
        m.alloc_lane_horizon_hints.fetch_add(1, Ordering::Relaxed);
        m.alloc_lane_harvest_horizon_ms
            .store(composed, Ordering::Relaxed);
    }

    /// The refill horizon in force, ms: the composed measurement when a
    /// hinted reply set one, the member-local derivation otherwise.
    pub fn harvest_horizon_ms(&self) -> u64 {
        match self.horizon_composed_ms.load(Ordering::Relaxed) {
            0 => self.fallback_horizon_ms(),
            v => v,
        }
    }

    /// §5.5: fold one rate sample (claims since the last sample ÷ elapsed)
    /// into the EWMA and re-derive the watermark —
    /// `ceil(rate × horizon)` capped at lane-share/4. Called by the
    /// ahead-refill task each tick (and by the contracts directly).
    pub fn sample_alloc_rate(&self, now_ms: u64) {
        let claims = self.alloc_claims.load(Ordering::Relaxed);
        let last_ms = self.rate_last_sample_ms.swap(now_ms, Ordering::Relaxed);
        let last_claims = self.rate_last_claims.swap(claims, Ordering::Relaxed);
        if last_ms == 0 || now_ms <= last_ms {
            return; // the first sample only seeds the snapshots
        }
        let dt_ms = now_ms - last_ms;
        let inst_mblk = claims.saturating_sub(last_claims).saturating_mul(1_000_000) / dt_ms;
        // EWMA α = 1/4: three parts memory, one part instant. The decay
        // divides CEILING-wise so a quiet writer's rate reaches exactly 0
        // (a floor division wedges the integer EWMA at 3 forever).
        let old = self.rate_mblk_per_s.load(Ordering::Relaxed);
        let ewma = old.saturating_sub(old.div_ceil(4)) + inst_mblk / 4;
        self.rate_mblk_per_s.store(ewma, Ordering::Relaxed);
        // watermark = ceil(rate × horizon), capped at lane-share/4 (the
        // partitioned share; 0 unpartitioned — the task never runs there).
        let Some(lanes) = self.lanes.get() else {
            self.harvest_watermark.store(0, Ordering::Relaxed);
            return;
        };
        let share = crate::data_alloc_lane::lane_capacity_blocks(
            self.capacity_blocks.load(Ordering::Relaxed),
            lanes.part.writers(),
            lanes.part.writer_id(),
        );
        // blocks = (milli-blocks/s × ms) / 1e6; ceil so a slow-but-live
        // writer keeps a nonzero watermark. A zero rate derives zero.
        let horizon = self.harvest_horizon_ms();
        let inflight = if ewma == 0 {
            0
        } else {
            ewma.saturating_mul(horizon).div_ceil(1_000_000)
        };
        let want = inflight.min(share / 4);
        self.harvest_watermark.store(want, Ordering::Relaxed);
        let m = &crate::fuse_client::METRICS;
        m.alloc_lane_harvest_watermark
            .store(want, Ordering::Relaxed);
        // The capacity law (hold-time campaign): the lane holds `live`
        // blocks in use (its share less what is reachable here or owed
        // back from the authority) and needs `churn × hold` more in flight
        // through the grace loop; it exhausts exactly when the sum exceeds
        // the share — the headroom is the published distance from that.
        // The owed word is the explicit-ship arm's face (a per-allocator
        // number); the mount-summed hint cannot enter a per-volume law, so
        // `live` over-reads by the recomputed supply the authority holds.
        let live = share.saturating_sub(self.lane_reachable_blocks() + self.lane_owed_blocks());
        let needed = inflight.saturating_add(live);
        let headroom_pct = if share == 0 {
            0
        } else {
            share.saturating_sub(needed).saturating_mul(100) / share
        };
        self.share_needed_blocks.store(needed, Ordering::Relaxed);
        self.headroom_pct.store(headroom_pct, Ordering::Relaxed);
        m.alloc_lane_share_needed_blocks
            .store(needed, Ordering::Relaxed);
        m.alloc_lane_headroom_pct
            .store(headroom_pct, Ordering::Relaxed);
    }

    /// The blocks this lane needs to sustain its churn through the grace
    /// loop plus what it holds live (`alloc_lane_share_needed_blocks`;
    /// 0 unpartitioned).
    pub fn lane_share_needed_blocks(&self) -> u64 {
        self.share_needed_blocks.load(Ordering::Relaxed)
    }

    /// `(share − needed) ÷ share` in percent, saturating at 0
    /// (`alloc_lane_headroom_pct`; 0 unpartitioned).
    pub fn lane_headroom_pct(&self) -> u64 {
        self.headroom_pct.load(Ordering::Relaxed)
    }

    /// The derived watermark in force (blocks).
    pub fn watermark_blocks(&self) -> u64 {
        self.harvest_watermark.load(Ordering::Relaxed)
    }

    /// §5.5's decision: harvest ahead ⇔ the lever is armed, supply of this
    /// lane is WITNESSED on the authority (the renewal grant's hint, or
    /// this allocator's owed word — `lane_supply_witnessed`; never
    /// the owed word alone, `.benchmarks/2026-09-07-lane-refill-hint-gate.md`),
    /// and its lane-reachable stock sits below the watermark. `Some(ask)`
    /// = the grain batch to request — the grain, never fewer (the serve is
    /// bounded by what the authority holds, so a smaller ask against a
    /// one-cadence-stale, mount-summed hint could only under-harvest);
    /// `None` = nothing to do (no witness / stocked / quiet / lever off).
    ///
    /// The witness is THIS VOLUME's advertised share (the grant vector's
    /// entry — `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md`), so
    /// a low volume the authority holds nothing for does not ask; under
    /// `SQUEEZEFS_ALLOC_LANE_VOLUME_HINT=0` the mount sum stands in and
    /// every low volume asks — the volume that got nothing has paid one
    /// RTT, and its stock stays where the lane-aware placement can see it
    /// (`.benchmarks/2026-09-07-cowriter-lane-aware-placement.md`).
    pub fn should_harvest_ahead(&self) -> Option<u64> {
        if !harvest_ahead_enabled() {
            return None;
        }
        let lanes = self.lanes.get()?;
        if !self.lane_supply_witnessed() {
            return None;
        }
        let watermark = self.harvest_watermark.load(Ordering::Relaxed);
        if watermark == 0 || self.lane_reachable_blocks() >= watermark {
            return None;
        }
        Some(lanes.grain.max(1))
    }

    /// One ahead-refill tick (§5.5's background single-flight task body —
    /// `crate::alloc_lane_grant` spawns one per laned co-writer
    /// engagement): sample the rate, and when the decision fires run the
    /// SAME harvest the ENOSPC arm runs (sink → adopt → hint deposit).
    pub async fn ahead_refill_tick(&self, now_ms: u64) -> u64 {
        self.sample_alloc_rate(now_ms);
        let adopted = if self.should_harvest_ahead().is_some() {
            crate::fuse_client::METRICS
                .alloc_lane_ahead_harvests
                .fetch_add(1, Ordering::Relaxed);
            self.note_proactive_harvest_witness();
            self.harvest_lane_supply().await
        } else {
            0
        };
        // After the harvest, so a grant that just restocked the lane reads
        // as covered.
        self.supply_close_tick().await;
        adopted
    }

    /// **The supply-coupled epoch close's decision** (finding 15's
    /// parked-supply term, design-rewrite-program §5.3 — the KD-1.7
    /// early-close made AHEAD of the `StorageFull`): `Some(deficit)` when
    /// the lever is on, this is a co-writer's lane with the close wired,
    /// and the lane's reachable supply sits below the watermark — the same
    /// `rate × horizon` threshold the ahead harvest fires on, i.e. the
    /// blocks one loop transit consumes. The deficit `watermark −
    /// reachable` is what the closes must yield to restore the lane to one
    /// transit's cover (at exhaustion it IS the watermark), and it is the
    /// per-tick bound: every close yields ≥ 1 block, so a tick never
    /// publishes more epochs than blocks it is short (≤ share/4).
    ///
    /// `None` with `rewrite_shadow_supply_close_declined_covered` counted
    /// when the lane covers its transit (a quiet writer's watermark decays
    /// to 0 — nothing to protect); `None` uncounted when the lever is off
    /// or the close is not wired (the shipped shape, byte-identical).
    pub fn supply_close_deficit(&self) -> Option<u64> {
        if !crate::routing::rewrite_supply_close_enabled() {
            return None;
        }
        self.lanes.get()?.supply_close.get()?;
        let watermark = self.harvest_watermark.load(Ordering::Relaxed);
        let reachable = self.lane_reachable_blocks();
        if watermark == 0 || reachable >= watermark {
            crate::fuse_client::METRICS
                .rewrite_shadow_supply_close_declined_covered
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(watermark - reachable)
    }

    /// Run the supply-coupled close for this tick's deficit (the sink
    /// closes largest-first until the yield covers it). Returns the parked
    /// blocks released; 0 = declined or nothing wired.
    async fn supply_close_tick(&self) -> u64 {
        let Some(deficit) = self.supply_close_deficit() else {
            return 0;
        };
        let Some(sink) = self.lanes.get().and_then(|l| l.supply_close.get()) else {
            return 0;
        };
        sink(deficit).await
    }

    /// **The PUSHED refill** (finding 15 term 2, the lane-push lever's
    /// co-writer half): the authority's renewal grant said this lane has
    /// supply on its list — harvest NOW, on the wake, instead of at the
    /// next watermark tick (which a quiet lane's decayed rate turns dark)
    /// or the next ENOSPC. The SAME harvest (sink → adopt → hint deposit);
    /// the decision is
    /// [`crate::free_grace::lane_push_wants_harvest_on_volume`] over THIS
    /// VOLUME's advertised share (`lane_supply_hint_for` on its `vol_tag`): the hint
    /// alone suffices (the owed word is the retired gate's witness), and a
    /// DRY volume asks even when owed nothing — but a volume the authority
    /// advertises 0 for does not, however large the sibling's share made
    /// the mount sum (`alloc_lane_volume_hint_skips` counts the decisions
    /// the sum would have fired: the empty RPCs the vector saves). Returns
    /// the count adopted (0 = the decision declined, or an empty grant).
    pub async fn pushed_refill_tick(&self, now_ms: u64) -> u64 {
        self.sample_alloc_rate(now_ms);
        if self.lanes.get().and_then(|l| l.harvest.get()).is_none() {
            return 0;
        }
        let (owed, reachable, watermark) = (
            self.lane_owed_blocks(),
            self.lane_reachable_blocks(),
            self.harvest_watermark.load(Ordering::Relaxed),
        );
        let adopted = if crate::free_grace::lane_push_wants_harvest_on_volume(
            self.lane_supply_hint(),
            owed,
            reachable,
            watermark,
        ) {
            crate::fuse_client::METRICS
                .alloc_lane_pushed_harvests
                .fetch_add(1, Ordering::Relaxed);
            self.note_proactive_harvest_witness();
            self.harvest_lane_supply().await
        } else {
            if crate::free_grace::lane_push_wants_harvest_on_volume(
                crate::free_grace::lane_supply_hint(),
                owed,
                reachable,
                watermark,
            ) {
                crate::fuse_client::METRICS
                    .alloc_lane_volume_hint_skips
                    .fetch_add(1, Ordering::Relaxed);
            }
            0
        };
        self.supply_close_tick().await;
        adopted
    }

    /// **The lane free harvest** (rung 10): when this mount's lane is
    /// exhausted, ask the authority for the lane's freed supply — the
    /// offsets this mount's own shipped frees returned to "the free supply
    /// of lane `b % W`", which live on the AUTHORITY's free list and are
    /// reachable by nobody else. One sink call per exhaustion episode
    /// (bounded by the reservation grain), adopted straight into the local
    /// free list. Returns the count adopted; `0` when no partition, no
    /// sink, or an empty supply — and errors are ABSORBED into `0` (the
    /// caller's honest `StorageFull` stands; a harvest failure must never
    /// mask the diagnosis).
    /// Finding 29: does a RECLAIMABLE supply exist that a bounded
    /// allocation should wait for? Two shapes: the local grace ring
    /// (the authority's own displaced offsets), and — on a co-writer —
    /// the REMOTE lane supply behind the harvest sink (its shipped
    /// frees land on the authority's list/ring; each bounded retry's
    /// `allocate_block` re-runs the harvest, whose serve-side pass 3
    /// evaluates the pressure fence at the authority — once per grant
    /// under the single-flight decline, not once per slice per parked
    /// allocation). The field re-measure convicted the local-ring-only
    /// condition: co-writer stalls parked ZERO times while their supply
    /// sat remote.
    ///
    /// The remote shape needs EVIDENCE, not a wire: the harvest reply's
    /// `bound_age_hint_ms` is the authority's live bound age — nonzero iff
    /// its ring holds offsets a fence can still release (`0` = nothing
    /// held, no plane, or nothing owed). `harvest_lane_supply` deposits
    /// every reply's hint before the verdict, so the word read here is
    /// this pass's. The sink's mere presence made every exhausted
    /// co-writer allocation park (the 2026-09-05 s11 fleet wedge).
    pub(crate) fn reclaimable_supply_exists(&self) -> bool {
        if !self.grace.is_empty() {
            return true;
        }
        self.lanes.get().is_some_and(|l| l.harvest.get().is_some())
            && self.horizon_composed_ms.load(Ordering::Relaxed) != 0
    }

    ///
    /// **Single-flight per allocator** (finding 15 phase B1,
    /// `SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT`, default on): every
    /// caller — an ENOSPC-path allocation, a parked retry, the ahead or
    /// pushed tick — goes through [`HarvestFlight`]: one RPC in flight,
    /// concurrent callers join its outcome (`alloc_lane_harvest_coalesced`)
    /// and retry their own funnel against the refilled list, and a fresh
    /// EMPTY reply declines re-issue (`alloc_lane_harvest_declined_stale`)
    /// until the supply witness moves ([`Self::supply_witness_gen`]). A
    /// joiner's wait is bounded by the park wall
    /// ([`crate::free_grace::pressure_park_wall_ms`]): past it the joiner
    /// takes its verdict with `0`, so no parked allocation waits longer
    /// than it would have on its own RPC. `alloc_lane_harvests` stays the
    /// RPC count, so `harvests + coalesced + declined_stale` accounts for
    /// every would-be call. Lever off = [`Self::issue_lane_harvest`] per
    /// caller, the shipped shape verbatim.
    async fn harvest_lane_supply(&self) -> u64 {
        let Some(lanes) = self.lanes.get() else {
            return 0;
        };
        let Some(sink) = lanes.harvest.get() else {
            return 0;
        };
        if !harvest_single_flight_enabled() {
            return self
                .issue_lane_harvest(lanes.grain, sink)
                .await
                .unwrap_or(0);
        }
        let flight = &lanes.flight;
        let m = &crate::fuse_client::METRICS;
        // The decline: the last reply was EMPTY and nothing the authority
        // advertises has moved since — a wire trip would return the same
        // answer. Read BEFORE the CAS so a leader's stamp names the witness
        // its reply answered; a grant landing mid-flight makes the stamp
        // stale at once (one conservative extra RPC, never a missed one).
        let witness = self.supply_witness_gen();
        if flight.empty_at_witness.load(Ordering::Acquire) == witness {
            m.alloc_lane_harvest_declined_stale
                .fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        let done0 = flight.done_gen.load(Ordering::Acquire);
        if flight
            .inflight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // The leader: one RPC, the outcome published to every joiner.
            let outcome = self.issue_lane_harvest(lanes.grain, sink).await;
            let adopted = outcome.unwrap_or(0);
            flight.empty_at_witness.store(
                if outcome == Some(0) {
                    witness
                } else {
                    u64::MAX
                },
                Ordering::Release,
            );
            flight.last_adopted.store(adopted, Ordering::Release);
            flight.done_gen.fetch_add(1, Ordering::AcqRel);
            flight.inflight.store(false, Ordering::Release);
            flight.notify.notify_waiters();
            return adopted;
        }
        // A joiner: register FIRST (`notified()` registers at creation),
        // then re-read the generation — a leader that finished between
        // the failed CAS and the registration already moved it; one that
        // finishes later wakes the registered waiter.
        m.alloc_lane_harvest_coalesced
            .fetch_add(1, Ordering::Relaxed);
        let woken = flight.notify.notified();
        if flight.done_gen.load(Ordering::Acquire) == done0 {
            let wall = crate::free_grace::pressure_park_wall_ms();
            if squeezefs_ipc::sqz_time::timeout(std::time::Duration::from_millis(wall), woken)
                .await
                .is_err()
            {
                // The leader's RPC outlived the wall: take the verdict now
                // (the caller's park refuses at the same wall it always
                // did); the leader's own outcome is untouched.
                return 0;
            }
        }
        flight.last_adopted.load(Ordering::Acquire)
    }

    /// **One harvest RPC** (the wire act [`Self::harvest_lane_supply`]
    /// single-flights): ask the sink for a grain, deposit the reply's
    /// horizon hint, adopt the blocks, stamp the lane-visible ledger.
    /// `Some(adopted)` on a reply (`Some(0)` = the authority holds nothing
    /// of this lane right now — an ANSWER, the decline's input); `None` on
    /// a failure, which is absorbed into the caller's `0` (the honest
    /// `StorageFull` stands; a harvest failure must never mask the
    /// diagnosis) and stamps no decline.
    async fn issue_lane_harvest(
        &self,
        grain: u64,
        sink: &crate::data_alloc_lane::LaneHarvestSink,
    ) -> Option<u64> {
        crate::fuse_client::METRICS
            .alloc_lane_harvests
            .fetch_add(1, Ordering::Relaxed);
        match sink(grain.max(1)).await {
            Ok(harvest) => {
                // OQ 2: every harvest reply refreshes the refill horizon —
                // the measured loop latency plus this trip's own RTT.
                self.note_harvest_hint(harvest.bound_age_hint_ms, harvest.rtt_ms);
                let adopted = self.adopt_lane_free_grant_at(&harvest.blocks, harvest.grant_seq);
                // The lane-visible ledger's co-writer half (finding 15
                // term 2): each adopted block's wait on the authority's
                // list (as the authority measured it) beside this trip's
                // own round trip — the instant of adoption is the instant
                // the allocator can mint it.
                for age in &harvest.release_ages_ms {
                    crate::free_grace::note_lane_visible(*age, harvest.rtt_ms);
                }
                Some(adopted)
            }
            Err(e) => {
                log::warn!(
                    "lane free harvest failed on volume '{}' ({e}) — the allocation verdict \
                     stands (StorageFull if nothing else frees); the lane's supply stays on \
                     the authority's list",
                    self._volume_id
                );
                None
            }
        }
    }

    /// **Retire a co-writer's LOCAL view of a displaced block whose free
    /// SHIPPED and was answered `Freed`**
    /// (`crate::cowriter::ship_displaced_frees`, after the authority's
    /// acknowledgement): drop the local refcount entry (the accounting
    /// lives on the authority now) and retire the local incarnation word,
    /// so a straggler validated fill of the dead binding fails its seqlock
    /// re-check here exactly as it would on the authority. Touches NO free
    /// list and NO device — this is the free's local *hygiene*, never its
    /// accounting.
    ///
    /// Finding 19/20: `Freed` verdicts ONLY. A `Refused` entry (the
    /// double-release lineage) must touch nothing — the offset may be live
    /// custody again, and destabilizing a live owner's word is the
    /// `read_settle_lost_serialized` tripwire. A `NonTerminal` entry rides
    /// [`Self::release_shipped_free_tracking`].
    pub fn retire_shipped_free_tracking(&self, offset: u64) {
        let _ = self.refcounts.remove_sync(&offset);
        let _ = self
            .harvest_grants
            .remove_sync(&(offset / self.chunk_size.max(1)));
        self.mark_incarnation_unstable(offset);
        // Finding 30: restore stability under a NEW generation (the W1
        // patch-fence idiom) — the poison above already invalidated every
        // racing fill's `still()` re-check (f19's law), and a co-writer
        // never re-claims a foreign offset, so a word LEFT unstable is
        // orphaned forever: when the authority re-mints this offset (its
        // fold of this very mount's shipped extents), every fetch here
        // loses its fill validation to the orphan, the settle exhausts,
        // and the fsync barrier EIOs (probes 2-4's MPI_ABORT).
        self.publish_block(offset);
    }

    /// **Release ONE local reference of a shipped displaced block whose
    /// free was answered `NonTerminal`** (finding 19): the durable ledger
    /// still holds references — a clone sibling, possibly on this very
    /// mount, keeps the block alive — so this mount's displaced reference
    /// releases (decrement, entry removed at zero) and the incarnation
    /// word is NEVER touched: an unstable-without-republish word would
    /// poison every later fill of the still-live block and moves a
    /// mid-settle owner's word (the finding-20 hazard). W1's sole-owner
    /// probe degrades safely either way (an untracked or >1 count
    /// refuses the patch).
    pub fn release_shipped_free_tracking(&self, offset: u64) {
        let drop_entry = self
            .refcounts
            .read_sync(&offset, |_, c| {
                c.fetch_sub(1, Ordering::AcqRel).saturating_sub(1) == 0
            })
            .unwrap_or(false);
        if drop_entry {
            let _ = self.refcounts.remove_sync(&offset);
            let _ = self
                .harvest_grants
                .remove_sync(&(offset / self.chunk_size.max(1)));
        }
    }

    /// **Apply one lane-free notice** (publish schema 16 — the AUTHORITY
    /// freed a block of this mount's lane through its OWN publish, a free no
    /// served reply names: the assembler's fold of this mount's shipped
    /// slices on the fleet). The offset's reference this mount held is gone
    /// on the authority, so its local tracking releases — the
    /// `retire_displaced_locally` decrement, never the incarnation word
    /// (the notice precedes the ladder's verdict, and a NonTerminal block's
    /// word must stay). The one guard is the LIFETIME: an offset whose
    /// harvest-grant tag is ABOVE `after_grants` was re-minted here from a
    /// grant the authority served after queuing the notice (the reply
    /// carrying the notice was reordered behind the grant's) — the live
    /// lifetime is touched by nobody.
    pub fn apply_lane_free_notice(&self, block_idx: u64, after_grants: u64) -> LaneFreeNotice {
        let offset = block_idx.saturating_mul(self.chunk_size);
        let tag = self
            .harvest_grants
            .read_sync(&block_idx, |_, g| *g)
            .unwrap_or(0);
        if tag > after_grants {
            log::debug!(
                "lane-free notice for block {block_idx} on volume '{}' names a previous \
                 lifetime (re-minted under grant {tag} > notice's {after_grants}); the live \
                 lifetime is untouched",
                self._volume_id
            );
            return LaneFreeNotice::ReMinted;
        }
        if self.refcount(offset).is_none() {
            return LaneFreeNotice::Untracked;
        }
        self.release_shipped_free_tracking(offset);
        LaneFreeNotice::Released
    }

    /// The harvest-grant tag block `block_idx`'s live lifetime carries
    /// (`None` = a fresh mint or an untagged adoption) — the instrument the
    /// notice pins read.
    pub fn harvest_grant_tag(&self, block_idx: u64) -> Option<u64> {
        self.harvest_grants.read_sync(&block_idx, |_, g| *g)
    }

    /// W1 patch fence, steps 1a+1b of the §5.1 mechanism (the normative
    /// clone/patch fence — docs/design-random-small-writes.md): retire the
    /// offset's incarnation word (racing validated read-tier fills of this
    /// key now fail their seqlock re-check instead of publishing mid-patch
    /// bytes), interpose the cross-word `SeqCst` fence (the store-buffering
    /// closure against `clone_file`'s pin-CAS → fence → snapshot side),
    /// then re-read the refcount. `true` ⇔ this writer is provably the
    /// sole owner and the in-place DMA may proceed.
    ///
    /// On `false` — the count grew (a clone pinned the block) or the
    /// offset is untracked — the caller MUST [`Self::publish_block`] to
    /// re-stabilize (content never changed) and fall back to the CoW path.
    ///
    /// Unstable-for-existing-offsets audit: `mark_incarnation_unstable`
    /// was built for *ownership transitions* (allocate / free) of an
    /// offset; the patch reuses it for a *content transition* of a LIVE
    /// mapped offset. That reuse is sound because the word's contract is
    /// "content is changing under this key — fills must not publish", not
    /// "the key is dead": `fill_incarnation` returns `None` while
    /// unstable, `fill_incarnation_still` fails across the retire→publish
    /// generation bump, and `publish_block` (after the DMA, or on the
    /// back-off/error paths) restores stability under a NEW generation so
    /// no fill that snapshotted the old generation can validate.
    pub fn begin_patch_sole_owner(&self, offset: u64) -> bool {
        // DLM S5 (spec §6.8 item 1 / §6.3's W1 paragraph): this predicate
        // reads a PROCESS-LOCAL refcount map, so on a reader it can prove
        // nothing about a block a live writer may have cloned — and a
        // reader has no business rewriting bytes in place at all. Refuse
        // before the incarnation word is even retired (the refusal must
        // not perturb a live writer's fill validation).
        //
        // No `patch_ineligible_*` counter joins the §5.4 decision ledger
        // here on purpose: a reader's write path is closed at the FUSE
        // door (EROFS) and at the allocator, and a CO-WRITER's W1 probes
        // decline UPSTREAM as the counted `patch_ineligible_posture`
        // decision (the `try_sole_owner_patch` posture clause, its
        // whole-block twin in `try_inplace_rewrite`, and the dd shape
        // probe — the 2026-08-19 mw-fleet storm fix: this gate's
        // ERROR-per-attempt refusal fired per eligible overwrite and
        // moved the `cowriter_accounting_refusals` tripwire). So this arm
        // stays DEFENSE-IN-DEPTH — structurally unreachable from the
        // product ladders on both postures — and a counter that can only
        // ever read 0 is exactly the dead weight the ledger's
        // rot-detection value depends on not having.
        if self.plane_gate("W1 in-place sub-block patch").is_err() {
            return false;
        }
        self.mark_incarnation_unstable(offset);
        crate::patch_clone_core::cross_word_fence();
        // The SHARED clause (symmetric PR 7, §5.4.4): a clone protocol
        // marks the source's block BEFORE the cloner holds a pin, so a
        // count of 1 is not sole ownership while the mark stands — read
        // after the same fence (`shared_ref_core`'s composed model).
        self.refcount(offset) == Some(1) && !self.is_shared(offset)
    }

    // -----------------------------------------------------------------
    // Symmetric metadata PR 7 — the SHARED mark (design §5.4.3 / §5.4.4;
    // `crate::shared_ref_core`).
    // -----------------------------------------------------------------

    /// Set the SHARED mark on `offset` (`MarkShared` landed at this
    /// process, or the index named the block at the armed forest's open).
    pub fn mark_shared(&self, offset: u64) {
        match self.shared.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => crate::shared_ref_core::mark(occ.get()),
            scc::hash_map::Entry::Vacant(vac) => {
                let word = crate::shared_ref_core::new_word();
                crate::shared_ref_core::mark(&word);
                let _ = vac.insert_entry(word);
            }
        }
    }

    /// Clear the mark — the index home's `Freed` verdict (no clone
    /// remains) or the block's terminal free.
    pub fn clear_shared(&self, offset: u64) {
        if let Some(word) = self.shared.remove_sync(&offset) {
            crate::shared_ref_core::clear(&word.1);
        }
    }

    /// Is `offset` SHARED? One absent-key probe on every mount that never
    /// marked (the flat / unarmed posture).
    pub fn is_shared(&self, offset: u64) -> bool {
        self.shared
            .read_sync(&offset, |_, w| crate::shared_ref_core::is_shared(w))
            .unwrap_or(false)
    }

    /// `shared_blocks`: the marked population on this data volume.
    pub fn shared_blocks(&self) -> u64 {
        self.shared.len() as u64
    }

    /// Release one RAM reference of a SHARED block whose terminal verdict
    /// the index home answered `Held`: the count decrements only while
    /// above 1 — the entry never reaches 0 under a standing index entry
    /// (a holder in a slot tree this process's RAM map never seeded, e.g.
    /// a handed-over tree, has no RAM reference here; the 1 stands for
    /// it). `true` ⇔ a reference was released.
    pub fn release_shared_held(&self, offset: u64) -> bool {
        self.refcounts
            .read_sync(&offset, |_, v| {
                let cur = crate::refcount_core::peek(v);
                if cur > 1 {
                    v.compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                } else {
                    false
                }
            })
            .unwrap_or(false)
    }

    /// W1 **clause 7** — whole-inode exclusive custody (DLM stage S11;
    /// pre-rc spec §6.7 lock modes, §6.3's W1 paragraph).
    ///
    /// [`Self::begin_patch_sole_owner`] proves the block is not
    /// CLONE-shared (refcount == 1). It proves nothing about a second
    /// writer holding custody of some of the block's BYTES — under
    /// byte-range custody that writer exists, and an in-place mutation of
    /// the block would touch bytes it owns (and race its CoW republish of
    /// the same block). This is the seventh clause of the §5.4 decision
    /// ledger, and the counter is its rot instrument.
    ///
    /// Callers pass the **whole block's** logical span, not the written
    /// sub-range: the patch retires the block's incarnation word, purges
    /// every tier under the block key, and (the in-place-rewrite arm)
    /// rewrites the whole block, so the custody the predicate demands is
    /// custody of the block. v1 is deliberately conservative there —
    /// refining to sub-block custody is S11's remote half, and this
    /// counter is what will show it is needed.
    ///
    /// `true` ⇒ ineligible (counted); `false` ⇒ the shipped shapes: no
    /// live custody, or a whole-file lease (whole-inode custody — what the
    /// write path's `get_or_acquire_lease` takes, so the clause is inert
    /// on every shipped mount), or a range grant this writer solely owns
    /// that covers the block.
    pub fn patch_range_shared(
        ino: u64,
        block_start: u64,
        block_end: u64,
        holder_token: u64,
    ) -> bool {
        // Finding 26: under the authority's fold-of-shipped-assembly
        // scope the question is the ARBITER's — two-or-more distinct
        // holders — because the fold executes the single holder's own
        // bytes by proxy and a demoted region is this fold's own vehicle.
        let shared = if crate::meta_ship::publish::arbiter_fold_active() {
            crate::dlm::span_range_shared_for_arbiter(ino, block_start, block_end)
        } else {
            crate::dlm::span_range_shared(ino, block_start, block_end, holder_token)
        };
        if !shared {
            return false;
        }
        crate::fuse_client::METRICS
            .patch_ineligible_range_shared
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// §5.1 clone amendment (a): pin + **validate-after-pin**. Takes one
    /// reference exactly like [`Self::increment_refcount`]; on success it
    /// interposes the cross-word `SeqCst` fence and snapshots the offset's
    /// incarnation word. An UNSTABLE word means a patch may be mid-flight
    /// on the pinned block: the caller must unpin (free_block = one
    /// decrement), refetch the authoritative map, and retry — the exact
    /// shape of the existing refused-pin retry loop, extended from "pin
    /// refused" to "pin unvalidated" (bounded by the same `attempt >= 3`
    /// loud refusal).
    ///
    /// The fence sits before the word *lookup* (not inside a
    /// found-the-word arm) so the absent-word case — an offset whose first
    /// in-process patch races this pin after a remount — is ordered too;
    /// absent words are otherwise stable-by-definition (written before
    /// this process, `UNKNOWN_STABLE`).
    pub fn pin_block_validated(&self, offset: u64) -> PinOutcome {
        if !self.increment_refcount(offset) {
            return PinOutcome::Refused;
        }
        crate::patch_clone_core::cross_word_fence();
        let stable = self
            .incarnations
            .read_sync(&offset, |_, v| {
                crate::incarnation_core::snapshot(&v.word).is_some()
            })
            .unwrap_or(true);
        if stable {
            PinOutcome::Pinned
        } else {
            PinOutcome::PinnedUnstable
        }
    }

    pub fn volume_id(&self) -> &str {
        &self._volume_id
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// Bound this allocator to a device of `bytes` capacity (whole chunks).
    /// Zero leaves it unbounded (offline tools / tests).
    pub fn set_capacity_bytes(&self, bytes: u64) {
        self.capacity_blocks
            .store(bytes / self.chunk_size, Ordering::Relaxed);
    }

    /// The bound capacity in bytes (`0` = unbounded) — the
    /// `volume_states` capacity gauge source.
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_blocks
            .load(Ordering::Relaxed)
            .saturating_mul(self.chunk_size)
    }

    /// Advance the fresh-block cursor by one, refusing to mint an offset at
    /// or past the device capacity (when bounded). CAS loop: a refused
    /// racer must not bump the cursor.
    ///
    /// **Under an engaged partition** the step is to the next index in a
    /// lane this mount owns (`cur` itself whenever `cur`'s lane is ours),
    /// and the cursor still publishes the DENSE frontier — the skipped
    /// indices belong to peers and are neither minted nor free-listed here.
    /// At `writers == 1` (and on every mount today, where `lanes` is `None`)
    /// the lane step is `cur` and this is the shipped loop, instruction for
    /// instruction.
    fn next_fresh_block(&self) -> Result<u64> {
        // PR 8: a grant-armed allocator mints ONLY inside its grants — the
        // holder's bitmap already set the bits before the grant reached
        // us, so a fresh mint here is never a dense-cursor claim.
        if let Some(arm) = self.block_grant.get() {
            return match arm.window.mint() {
                Some(idx) => {
                    self.highest_block.fetch_max(idx + 1, Ordering::AcqRel);
                    self.kick_block_grant_topup();
                    Ok(idx)
                }
                None => {
                    arm.window.note_exhausted();
                    Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::StorageFull,
                        format!(
                            "data volume '{}': block grant window empty (vol_tag {:#018x}) — a \
                             top-up from the allocation holder is owed",
                            self._volume_id, arm.vol_tag
                        ),
                    )))
                }
            };
        }
        let cap = self.capacity_blocks.load(Ordering::Relaxed);
        loop {
            let cur = self.highest_block.load(Ordering::Relaxed);
            let idx = match self.lanes.get() {
                None => cur,
                Some(lanes) => crate::data_alloc_lane::next_owned_index_at_or_above(
                    cur,
                    lanes.owned.load(Ordering::Acquire),
                    lanes.part.writers(),
                )
                .unwrap_or(cur),
            };
            if cap != 0 && idx >= cap {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    format!(
                        "data volume '{}' full: {} of {} blocks allocated",
                        self._volume_id, cur, cap
                    ),
                )));
            }
            if self
                .highest_block
                .compare_exchange(cur, idx + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(idx);
            }
        }
    }

    /// Enrich a `StorageFull` refusal with the lane diagnosis — *this lane is
    /// out of blocks, N free blocks belong to other lanes, and the way to
    /// reach them is a proven-dead lane adoption* (`docs/operations.md`
    /// §Multi-writer capacity planning) — and count it.
    ///
    /// Runs **once per refused allocation**, at the single exit of
    /// [`Self::allocate_block`], never inside `next_fresh_block`: the ENOSPC
    /// pressure valve retries up to `ENOSPC_VALVE_MAX_ATTEMPTS` times, and
    /// the free-list scan this performs is exactly the wrong thing to repeat
    /// on a store whose free supply is large but foreign. The
    /// [`std::io::ErrorKind::StorageFull`] class is preserved verbatim, so
    /// the valve, the reclaim ladder and every caller behave identically.
    fn refuse_lane_enospc(&self, e: crate::error::SqueezefsError) -> crate::error::SqueezefsError {
        let Some(lanes) = self.lanes.get() else {
            return e;
        };
        let foreign = self.foreign_lane_free_blocks();
        crate::fuse_client::METRICS
            .alloc_lane_enospc_refusals
            .fetch_add(1, Ordering::Relaxed);
        let msg = format!(
            "{e} — lane {} of {} is exhausted while {foreign} free block(s) belong to lanes this \
             mount does not own (alloc_lane_enospc_refusals; reach them by adopting a lane whose \
             holder is proven dead, or grow the volume set — docs/operations.md §Multi-writer \
             capacity planning)",
            lanes.part.writer_id(),
            lanes.part.writers()
        );
        // Finding 29: the bounded-allocation retries hit this refusal once
        // per park slice — a storm wrote 30k identical lines per row. One
        // loud line per second per allocator; the counter keeps the rate.
        static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = LAST_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) >= 1_000
            && LAST_LOG_MS
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            log::error!("{msg}");
        } else {
            log::debug!("{msg}");
        }
        crate::error::SqueezefsError::Io(std::io::Error::new(std::io::ErrorKind::StorageFull, msg))
    }

    /// The reservation gate between a claimed index and its caller (DLM S9
    /// blocker #3): an offset is handed out only once the own lane's durable
    /// watermark covers it, so no successor of this lane can re-mint it.
    ///
    /// `None` partition (every mount today) ⇒ one `OnceLock` probe and the
    /// value straight through. A failed reservation **gives the offset
    /// back** rather than handing out an un-covered one: nothing durable and
    /// no device byte has touched it yet, which is exactly the
    /// begin+finish-with-nothing-between contract [`Self::free_block`]
    /// states.
    async fn hand_out_reserved(&self, claimed: Result<u64>) -> Result<u64> {
        let Ok(offset) = claimed else { return claimed };
        if self.lanes.get().is_none() {
            return Ok(offset);
        }
        if let Err(e) = self.reserve_lane_frontier(offset / self.chunk_size).await {
            log::error!(
                "returning freshly claimed offset {offset} on volume '{}': its lane reservation \
                 could not be made durable ({e}) — handing out an unreserved offset would let a \
                 successor of this lane mint it again (DLM S9)",
                self._volume_id
            );
            // The give-back is co-writer-aware: a laned mount IS a
            // co-writer, whose local free would refuse at the plane gate
            // (loud, per offset). The un-covered offset abandons quietly —
            // exactly the recovery the failed reservation implies: the
            // frontier never covered it, so a successor may re-mint it.
            let _ = self.abandon_unpublished_offset(offset).await;
            return Err(e);
        }
        Ok(offset)
    }

    /// Finding 29: the WRITE PATH's allocation — [`Self::allocate_block`]
    /// plus the wait the pressure ruling promises. A `StorageFull` whose
    /// store still holds offsets in the freed-offset grace period is
    /// bounded-transient BY THE RULING ("this wait always ends by itself,
    /// because past the deadline the laggard is fenced") — but no caller
    /// ever performed the wait, so the field's fsync ladder surfaced EIO
    /// seconds into a storm with the fence gauges at 0 and reclaimable
    /// space in the ring. This form parks on the plane's own numbers
    /// (slice = [`crate::free_grace::pressure_park_slice_ms`]), re-runs
    /// the pressure harvest each pass (whose deadline evaluation fences
    /// the laggard past the tightened bound — release-or-evict), and
    /// stops honestly: an EMPTY ring refuses immediately (genuine
    /// exhaustion), and a frozen plane refuses at the wall backstop
    /// ([`crate::free_grace::pressure_park_wall_ms`]). On every unarmed
    /// mount the ring is structurally empty, so this is byte-identical
    /// to [`Self::allocate_block`] — one branch.
    ///
    /// The wall is measured in WALL time (the harvest RPC and the ENOSPC
    /// valve's passes inside `allocate_block` count — a co-writer's pass
    /// is one authority round trip, so a slice-sum accumulator under-read
    /// the park by the RTT per pass), and every caller holds this park
    /// under its `BLOCK_FLUSH_LOCKS` guard: past the wall the refusal is
    /// TERMINAL for that write (`.benchmarks/2026-09-06-cowriter-enospc-wedge.md`).
    ///
    /// The retries' harvests are SINGLE-FLIGHT per allocator with a
    /// fresh-empty decline (`HarvestFlight`): N parked allocations on
    /// one volume cost the authority one RPC per grant, not N per slice
    /// (finding 15 phase B1 — the storm the liveness plane queued behind).
    pub async fn allocate_block_grace_bounded(&self) -> Result<u64> {
        let started = std::time::Instant::now();
        loop {
            match self.allocate_block().await {
                Err(e) if is_storage_full(&e) => {
                    self.park_for_reclaimable_supply(e, started, self.reclaimable_supply_exists())
                        .await?;
                }
                other => return other,
            }
        }
    }

    /// One park decision of the bounded allocation
    /// ([`Self::allocate_block_grace_bounded`]), shared with the
    /// router-level placed allocation (`BackendRouter::allocate_placed_block`
    /// — which parks only after EVERY eligible volume refused, and passes the
    /// set's `reclaimable` verdict). `Err(e)` = the refusal is terminal (no
    /// reclaimable supply, or the wall passed); `Ok(())` = one slice parked,
    /// the caller retries.
    pub(crate) async fn park_for_reclaimable_supply(
        &self,
        e: crate::error::SqueezefsError,
        started: std::time::Instant,
        reclaimable: bool,
    ) -> Result<()> {
        if !reclaimable {
            return Err(e);
        }
        let wall = crate::free_grace::pressure_park_wall_ms();
        let waited_ms = started.elapsed().as_millis() as u64;
        if waited_ms >= wall {
            log::error!(
                "bounded allocation refusing StorageFull after {waited_ms} ms parked with {} \
                 offset(s) still in grace locally (lane harvest horizon {} ms): the pressure \
                 deadline never fenced within the wall backstop ({wall} ms). The refusal is \
                 honest and terminal for this write; investigate the membership plane \
                 (finding 29)",
                self.grace.len(),
                self.horizon_composed_ms.load(Ordering::Relaxed),
            );
            return Err(e);
        }
        let slice = crate::free_grace::pressure_park_slice_ms();
        crate::free_grace::note_pressure_park();
        // The lane-push lever (finding 15 term 2): a renewal grant's
        // lane-supply hint ends the slice at once, so the retry's harvest
        // runs on the hint's heels rather than at the slice's own cadence.
        // Inert with the lever off (nothing ever notifies) and on every
        // mount that is not a co-writer.
        let _ = squeezefs_ipc::sqz_time::timeout(
            std::time::Duration::from_millis(slice),
            crate::free_grace::lane_supply_wake().notified(),
        )
        .await;
        Ok(())
    }

    pub async fn allocate_block(&self) -> Result<u64> {
        match self.allocate_block_inner().await {
            // DLM S9: the lane diagnosis is attached ONCE, here, at the
            // single exit — never inside the valve's retry loop.
            Err(e) if is_storage_full(&e) => Err(self.refuse_lane_enospc(e)),
            other => other,
        }
    }

    async fn allocate_block_inner(&self) -> Result<u64> {
        if let Err(e) = self.alloc_plane_gate("block allocation") {
            return Err(e);
        }
        let first = self.try_allocate_block();
        if first.is_ok() {
            return self.hand_out_reserved(first).await;
        }
        let Err(e) = first else { return first };
        // Spec §6.8 item 3's pressure arm, BEFORE the reclaim valve's early
        // exits: a store whose free list is entirely in the grace period is
        // full only in the sense that its space is owed to readers. The
        // pressure deadline (one honest acknowledgement cycle) is evaluated
        // here — and if it has not expired the verdict STANDS: reallocating
        // an offset a reader may still resolve serves another file's bytes,
        // silently on a passthrough volume. ENOSPC is a bounded
        // availability cost; this wait always ends by itself, because past
        // the deadline the laggard is fenced.
        if is_storage_full(&e) && !self.grace.is_empty() {
            if self.harvest_grace_pressure() > 0 {
                if let ok @ Ok(_) = self.try_allocate_block() {
                    return ok;
                }
            }
            if !self.grace.is_empty() {
                crate::free_grace::note_alloc_stall(self.grace.len(), self.grace.bytes());
            }
        }
        // Rung 10 (residual 2) — the lane free HARVEST, before the verdict:
        // on a co-writer the lane's freed supply lives on the AUTHORITY's
        // free list (its shipped frees put it there, and the authority's
        // own lane-filtered funnel can never reach it), so lane exhaustion
        // asks for it back — grain-batched, adopted into the local free
        // list, retried through the ordinary funnel. Absent sink/partition
        // = one OnceLock probe and fall through (every mount today).
        if is_storage_full(&e) && self.harvest_lane_supply().await > 0 {
            if let ok @ Ok(_) = self.try_allocate_block() {
                return self.hand_out_reserved(ok).await;
            }
        }
        // PR 8: a grant-armed writer's exhausted window asks its holder
        // for a top-up and retries; a holder with nothing (full,
        // unreachable) leaves the refusal in the `StorageFull` class —
        // never a poison, never `note_journal_failure`. The retry runs
        // whatever the inline ask answered, and once more after a
        // PROACTIVE ask in flight lands: at the volume's last grant the
        // two asks race and the holder answers one of them `Full` while
        // the other carries the grant — the window, not the answer, is
        // the truth (found by the full-cursor supply pin).
        if is_storage_full(&e) && self.block_grant_armed() {
            let _ = self.block_grant_topup().await;
            if let ok @ Ok(_) = self.try_allocate_block() {
                return ok;
            }
            if self.block_grant_topup_join().await {
                if let ok @ Ok(_) = self.try_allocate_block() {
                    return ok;
                }
            }
        }
        if !is_storage_full(&e) {
            return Err(e);
        }
        // ENOSPC pressure: freed-but-queued reclaims own free space this
        // allocation is entitled to — drain them OFF-THREAD and retry
        // (contract 2b, probe-up campaign 2026-07-29): the allocating
        // task awaits the drain (honest backpressure on exactly the
        // starved task), the ioctl work runs on the blocking pool, and
        // racing engagements drain CONCURRENTLY (width derives from
        // allocation demand — no single-flight mutex, no park ticks:
        // both alternatives were counted on the 4-wide storm bracket
        // and measured −9 % / −12 % — where reclaim work is cheap,
        // added per-allocation latency is pipeline lifetime in a
        // depth-limited regime). The loop terminates: each pass either
        // allocates, or drains supply someone allocated (system-wide
        // progress, the try_allocate_block retry argument), or observes
        // nothing pending and refuses StorageFull honestly.
        //
        // The trim venue's CLAIM WINDOW is the other pending supply
        // (KD-4.4 — contract 9, tests/discard_elision_tests.rs): a debt
        // offset leaves the free list for one device command and comes
        // back, on no queue the valve can drain. A full store under
        // rewrite (virgin tail 0 ⇒ the pressure venue drains on every
        // pass) puts its ENTIRE free list inside that window at times,
        // and the empty-scan verdict was then a spurious, TERMINAL
        // StorageFull. So an open window parks this allocation on its
        // return edge — before the valve, and whether or not a valve is
        // wired (the window is the allocator's own protocol).
        //
        // RES-1 5 (pre-RC engineering spec §7): the `pending` exit alone
        // is not a bound. Under concurrent reclaim traffic OTHER writers
        // keep the queue non-empty, so every pass observes `pending ==
        // true` and the honest-refusal exit is never reached — the
        // allocating task spins on a genuinely full store instead of
        // returning `StorageFull`. The attempt cap below is the
        // liveness floor: past it the verdict stands, loudly.
        for _attempt in 0..ENOSPC_VALVE_MAX_ATTEMPTS {
            if let ok @ Ok(_) = self.try_allocate_block() {
                return self.hand_out_reserved(ok).await;
            }
            if self.await_trim_return().await {
                continue;
            }
            let Some(valve) = self.space_valve.get() else {
                return Err(e);
            };
            let pending = self.space_pending.get().map(|p| p()).unwrap_or(false);
            valve().await;
            if !pending {
                // Nothing was owed before the final drain: the verdict
                // stands (genuine fullness refuses StorageFull) — unless
                // a claim window opened DURING the drain, whose return
                // edge is the next pass's to await.
                let last = self.try_allocate_block();
                if last.is_err() && self.trim_claimed.load(Ordering::SeqCst) > 0 {
                    continue;
                }
                return self.hand_out_reserved(last).await;
            }
        }
        log::error!(
            "allocate_block: the ENOSPC pressure valve ran {ENOSPC_VALVE_MAX_ATTEMPTS} \
             drain-and-retry passes with reclaims still pending (queued, or inside a \
             trim claim window) and never freed a block — refusing StorageFull rather \
             than spinning (the store is full and the reclaimer is not gaining on it; \
             check block_free_reclaim_queue_bytes, alloc_trim_window_parks and \
             block_free_reclaim_fence_halts)"
        );
        let last = self.try_allocate_block();
        self.hand_out_reserved(last).await
    }

    /// One allocation attempt (free list, then fresh mint) — the body
    /// [`Self::allocate_block`] wraps with the ENOSPC pressure valve.
    ///
    /// The free-list claim RETRIES until a `remove` wins or the list is
    /// observed empty (field ledger inversion, 2026-07-27 — contract 7 of
    /// `tests/async_block_reclaim_tests.rs`): concurrent allocators all
    /// read the same list head, and the old lost-race arm fell straight
    /// to `next_fresh_block()` — which refuses `StorageFull` on any
    /// cursor-at-cap store (any store that has EVER been full;
    /// `highest_block` never shrinks). Every lost race then fired the
    /// ENOSPC valve: a spurious `sync_drains` count plus a synchronous
    /// whole-queue reclaim drain ON THE WRITE PATH, at ANY fill (the
    /// field's per-allocation valve storm and −33 % rewrite tax). Each
    /// retry means another thread claimed that candidate — system-wide
    /// progress — so the loop is livelock-free and terminates when the
    /// list empties.
    fn try_allocate_block(&self) -> Result<u64> {
        // Spec §6.8 item 3: acknowledged offsets re-enter the free list
        // HERE, at the head of the funnel, so the existing free-list-first
        // preference is unchanged and no allocation path can observe a
        // released offset late. One relaxed load when nothing is held.
        self.harvest_grace();
        // PR 8 (review round 2, Issue 23): on a grant-armed allocator the
        // bitmap IS the free list — the flat list is never a mint source
        // (the mount-time gap fill, a pre-arm free, a trim window's return
        // would otherwise be handed out with their bits CLEAR, or SET
        // inside another writer's grant); the window is the one source.
        // One `OnceLock` probe on every unarmed mount.
        if self.block_grant.get().is_some() {
            let block_idx = self.next_fresh_block()?;
            crate::fuse_client::METRICS
                .alloc_fresh_mints
                .fetch_add(1, Ordering::Relaxed);
            return Ok(self.claim_block_idx(block_idx));
        }
        loop {
            // DLM S9: reuse obeys the same residue class as a fresh mint —
            // a free block in a peer's lane is that peer's to reuse, which
            // is what makes reuse arbitration-free (and is the source of
            // the stranding bound). Unpartitioned mounts take the first
            // candidate, unchanged.
            let Some(idx) = self
                .free_blocks
                .iter()
                .map(|item| *item)
                .find(|idx| self.lane_is_ours(*idx))
            else {
                break;
            };
            if self.free_blocks.remove(&idx).is_some() {
                log::debug!(
                    "allocate_block: offset {} (freelist)",
                    idx * self.chunk_size
                );
                // The attribution split PR 1 exists for (sustain §8):
                // which stream is recycle-bound.
                crate::fuse_client::METRICS
                    .alloc_from_freelist
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(self.claim_block_idx(idx));
            }
            // Lost the claim race: rescan for the next candidate.
        }
        let block_idx = self.next_fresh_block()?;
        log::debug!(
            "allocate_block: offset {} (fresh)",
            block_idx * self.chunk_size
        );
        crate::fuse_client::METRICS
            .alloc_fresh_mints
            .fetch_add(1, Ordering::Relaxed);
        Ok(self.claim_block_idx(block_idx))
    }

    /// The shared post-claim tail: refcount 1, incarnation unstable (cache
    /// fills must not publish until the owner's `publish_block`), and the
    /// PR VL6a fsck epoch-latch record. Returns the byte offset.
    fn claim_block_idx(&self, block_idx: u64) -> u64 {
        let offset = block_idx * self.chunk_size;
        // The ahead-refill rate EWMA's input (§5.5): one relaxed add per
        // funnel exit.
        self.alloc_claims.fetch_add(1, Ordering::Relaxed);
        // Claim-cancels-debt (Idea 4, KD-4.3): the new owner's
        // write-before-publish rewrites the range — it owes no discard.
        self.cancel_elided_debt(offset);
        if self
            .refcounts
            .insert_sync(offset, AtomicU32::new(1))
            .is_err()
        {
            // A lingering refcount entry at claim time means the offset was
            // free-listed while a tracked owner existed — the double-owner
            // mint observed from the OTHER side. Loud: this is never legal.
            crate::fuse_client::METRICS
                .block_claim_anomalies
                .fetch_add(1, Ordering::Relaxed);
            log::error!(
                "CLAIM ANOMALY: offset {offset} claimed from the free list while a \
                 refcount entry lingers (count={:?}) (block_claim_anomalies)",
                self.refcount(offset)
            );
        }
        // New incarnation, not yet durable: cache fills must not publish until
        // the owner calls `publish_block` after its device write.
        self.mark_incarnation_unstable(offset);
        // Spec §6.2 item 6: an allocation is the ONLY event that starts a
        // new lifetime of an offset, so this is the one place a lifetime
        // stamp is minted. Every other site (`persist_block_key`, fsck's
        // reconciliation, the movers' census) READS the live stamp — a
        // second minting site would let a key disagree with the offset's
        // recorded lifetime and turn honest reads into refusals.
        // Disengaged (every mount today): one relaxed `OnceLock` probe.
        let stamp = self.mint_incarnation();
        if stamp != crate::routing::INCARNATION_NONE {
            match self.incarnations.entry_sync(offset) {
                scc::hash_map::Entry::Occupied(occ) => {
                    occ.get().stamp.store(stamp, Ordering::Release)
                }
                scc::hash_map::Entry::Vacant(vac) => {
                    let _ = vac.insert_entry(IncarnationCell::new(
                        crate::incarnation_core::UNSTABLE_FIRST,
                        stamp,
                    ));
                }
            }
        }
        // PR VL6a (§5.6): while an fsck scan is latched, record the
        // minting epoch in the side map — one relaxed load when idle.
        if self.fsck_scan_active.load(Ordering::Relaxed) {
            let epoch = self.fsck_scan_epoch.load(Ordering::Relaxed);
            match self.fsck_epoch_map.entry_sync(offset) {
                scc::hash_map::Entry::Occupied(mut occ) => *occ.get_mut() = epoch,
                scc::hash_map::Entry::Vacant(vac) => {
                    let _ = vac.insert_entry(epoch);
                }
            }
        }
        offset
    }

    /// PR VL7 (design-volume-lifecycle §5.7 D1/D2): the **contiguity-aware
    /// pick** — claim the LOWEST free block whose index is `< below_idx`,
    /// with the full `allocate_block` discipline (refcount, incarnation,
    /// fsck epoch latch). `None` = no free block below the bound (the
    /// mover defers; it never mints fresh tail blocks for a compaction —
    /// that would grow the very tail it is reclaiming). Lock-free: the
    /// `DashSet::remove` is the atomic claim; a lost race falls through to
    /// the next candidate.
    ///
    /// DLM S9: under an engaged partition the candidates are filtered to
    /// lanes this mount owns, so **contiguity picks keep working WITHIN a
    /// lane** — a lane's indices are `w, w+W, …`, so "the lowest free block
    /// below the bound" still converges the mover, one stride coarser. The
    /// cost is stated in the design record: physical contiguity of a run of
    /// blocks is `W`-strided rather than dense, which is why the partition
    /// width is a capacity/fragmentation trade and not free.
    pub fn allocate_block_below(&self, below_idx: u64) -> Option<u64> {
        self.alloc_plane_gate("block allocation (contiguity pick)")
            .ok()?;
        // Spec §6.8 item 3: acknowledged offsets re-enter the free list
        // before the pick reads it, so a mover never defers for space that
        // is actually available (one relaxed load when nothing is held).
        self.harvest_grace();
        let mut cands: Vec<u64> = self
            .free_blocks
            .iter()
            .map(|i| *i)
            .filter(|i| *i < below_idx && self.lane_is_ours(*i))
            .collect();
        cands.sort_unstable();
        for idx in cands {
            if self.free_blocks.remove(&idx).is_some() {
                return Some(self.claim_block_idx(idx));
            }
        }
        None
    }

    /// PR VL7 (§5.7 D2): the **ascending pick** — claim the lowest free
    /// block whose index is `≥ min_idx`; none free ⇒ a fresh tail mint
    /// (strictly above every existing index, so successive calls with a
    /// caller-maintained floor are guaranteed ascending — the locality
    /// rewrite's convergence invariant). Full `allocate_block`
    /// discipline; `StorageFull` propagates from the fresh-mint path.
    pub fn allocate_block_at_or_above(&self, min_idx: u64) -> Result<u64> {
        self.alloc_plane_gate("block allocation (ascending pick)")?;
        // Spec §6.8 item 3, as in the contiguity pick above.
        self.harvest_grace();
        let mut cands: Vec<u64> = self
            .free_blocks
            .iter()
            .map(|i| *i)
            .filter(|i| *i >= min_idx && self.lane_is_ours(*i))
            .collect();
        cands.sort_unstable();
        for idx in cands {
            if self.free_blocks.remove(&idx).is_some() {
                return Ok(self.claim_block_idx(idx));
            }
        }
        let idx = self.next_fresh_block()?;
        // DLM S9: this pick is SYNCHRONOUS, so it cannot await a
        // reservation raise. A fresh mint past the durable frontier is
        // therefore refused loud rather than handed out uncovered — the
        // caller (a VL4/VL7 mover) defers, and the async write path's next
        // allocation raises the frontier. Unpartitioned mounts never reach
        // the branch.
        if let Some(lanes) = self.lanes.get() {
            if idx >= lanes.reserved_upto.load(Ordering::Acquire) {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "volume '{}': the ascending pick would mint fresh block {idx}, past this \
                     lane's durable reservation frontier {} — refusing (a synchronous pick \
                     cannot raise the frontier; the write path's next allocation will, and this \
                     mover should defer)",
                    self._volume_id,
                    lanes.reserved_upto.load(Ordering::Acquire)
                )));
            }
        }
        Ok(self.claim_block_idx(idx))
    }

    /// Release one reference. On the TERMINAL release (count hit zero) the
    /// offset's incarnation is retired and `true` is returned — but the
    /// offset is **not yet reallocatable**: the freer owns it until
    /// [`Self::finish_free`], which is what makes destructive post-free
    /// device work (the router's hole punch) safe to run in between — it
    /// strictly happens-before any new owner's DMA. Non-terminal releases
    /// return `false` and release nothing else.
    ///
    /// FIND-RW5-A face 6: a free of an UNTRACKED offset is **refused**
    /// (`false`), loudly and counted. At steady state every legitimately
    /// freeable offset carries a refcount entry — allocation seeds it
    /// (`claim_block_idx`, [`Self::allocate_specific_block`]) and
    /// the mount recovery walk seeds every live reference
    /// ([`Self::recover_block`]) — so an untracked free is the second half
    /// of a double-release: the lineage that, interleaved with two
    /// allocations, mints ONE device offset to TWO live owners (the
    /// generic/464 rebind-exhaustion EIO engine). Refusal is the leak-safe
    /// direction: the offset never re-enters the free list on a stale
    /// release (fsck C6 reconciles a genuine limbo). The historical
    /// untracked-frees-anyway arm predates recovery seeding and had no
    /// remaining legitimate caller (fsck's C2Leaked apply refuses untracked
    /// offsets itself before freeing).
    pub fn begin_free(&self, offset: u64) -> bool {
        if self.plane_gate("terminal block free").is_err() {
            return false;
        }
        let should_free = if let Some(terminal) = self
            .refcounts
            .read_sync(&offset, |_, v| crate::refcount_core::release(v))
        {
            if terminal {
                self.refcounts.remove_sync(&offset);
                // A block that frees leaves no mark behind (the index home
                // decided `Freed` before a SHARED block reaches here).
                self.clear_shared(offset);
                log::debug!("begin_free terminal: offset {offset}");
                true
            } else {
                log::debug!("begin_free nonterminal: offset {offset}");
                false
            }
        } else {
            log::error!(
                "begin_free REFUSED untracked offset {offset}: no refcount entry — \
                 double-release lineage (see block_untracked_free_refusals)"
            );
            // Forensics (env-gated): pair the refused release's backtrace with
            // the recorded FIRST free of this offset — names both halves of
            // the double-release lineage even though the refusal never
            // reaches finish_free's tape.
            if free_forensics_enabled() {
                let bt = std::backtrace::Backtrace::force_capture().to_string();
                let first = lookup_free_forensics(offset).unwrap_or_else(|| {
                    "<no recorded first free (or aged out of the ring)>".to_string()
                });
                log::error!(
                    "REFUSED FREE FORENSICS offset {offset}:\n--- first free ---\n{first}\n--- refused release ---\n{bt}"
                );
            }
            crate::fuse_client::METRICS
                .block_untracked_free_refusals
                .fetch_add(1, Ordering::Relaxed);
            false
        };

        if should_free {
            // Retire this incarnation before the offset becomes reallocatable so
            // any in-flight cache fill that resolved a stale map to this key
            // fails its seqlock validation instead of poisoning the next owner.
            self.mark_incarnation_unstable(offset);
        }
        should_free
    }

    /// Publish a [`Self::begin_free`]-retired offset for reuse. Only after
    /// this can `allocate_block` hand the offset to a new owner.
    pub fn finish_free(&self, offset: u64) {
        // DLM S7: a dead epoch's block must not become reallocatable when
        // its free completes — recovery, an fsck C2 repair or the
        // reclaimer finishing a queued free would otherwise hand it to a
        // new owner while the (possibly live) zombie can still DMA into
        // it. The publish is OWED to the drain proof
        // ([`Self::release_quarantine`]). One lock-free probe on an empty
        // map when nothing is quarantined, which is every single-writer
        // mount.
        if !self.quarantine.is_empty() && self.quarantine.defer_free(offset) {
            log::warn!(
                "finish_free of quarantined block {offset}: free-list publish DEFERRED until \
                 the dead epoch is proven drained (DLM S7; dlm_quarantined_offsets)"
            );
            return;
        }
        // Spec §6.8 item 3 — the freed-offset grace period. The two gates
        // compose in ONE order: custody proof (S7, above) first, reader
        // coherence second, because an offset the dead epoch may still DMA
        // into must not become a reader's problem, and an offset a reader
        // may still resolve must not become a new owner's. Unarmed (no
        // reader plane — the shipped default) this is one relaxed load.
        //
        // Under GPFS-strict tokens (design-symmetric-metadata §5.7.3, PR
        // 5) the freeing publish recalled every reader's token before it
        // committed — the recall IS the qualification — so the free
        // publishes directly; the ring is the timeout path for a live
        // member's unacked recall alone. One relaxed load unarmed.
        if crate::free_grace::recall_gate_verdict() == crate::free_grace::RecallGate::Gated {
            self.publish_free_list(offset);
            crate::fuse_client::METRICS
                .del_obj
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.grace.defer(offset, self.chunk_size) {
            // The free COMPLETED — only its free-LIST publish waits on the
            // acknowledgements, so the delete is counted here exactly as it
            // would have been.
            crate::fuse_client::METRICS
                .del_obj
                .fetch_add(1, Ordering::Relaxed);
            self.harvest_grace();
            return;
        }
        self.publish_free_list(offset);
        log::debug!("finish_free: offset {offset}");
        crate::fuse_client::METRICS
            .del_obj
            .fetch_add(1, Ordering::Relaxed);
    }

    /// The free list's ONE publish — the tail `finish_free` used to inline,
    /// now shared with the two deferred paths (the §6.8 item-3 harvest and
    /// S7's drain-proof release), so the double-free tripwire and the
    /// free-forensics tape cover every arm identically.
    fn publish_free_list(&self, offset: u64) {
        let block_idx = offset / self.chunk_size;
        // PR 8 (§5.5.1): on the volume's allocation HOLDER a terminal free
        // clears the block's bit here — after quarantine and grace, so a
        // clear bit means "reallocatable now" — and its CLEAR delta is
        // journaled at once, in order with every other delta of the
        // holding (review round 1, Issue 2). One acquire load when no
        // lease is held.
        if crate::meta_backend::kv::alloc_lease::holds_any() {
            let cleared = crate::meta_backend::kv::alloc_lease::note_finish_free(
                crate::meta_backend::kv::block_refs::volume_tag(&self._volume_id),
                block_idx,
            );
            // On a grant-armed allocator the bitmap IS the free list
            // (review round 1, Issue 7): a freed block is reallocated only
            // through a GRANT — whose carve SETS its bit again — never off
            // a local list with its bit clear (the double allocation a
            // successor's re-grant of that clear bit would be). The
            // holder's `false` here is the double-free shape the list's
            // tripwire below reports on the unarmed path.
            if self.block_grant_armed() {
                if !cleared {
                    log::error!(
                        "DOUBLE FREE: offset {offset} (block {block_idx}) was already clear on \
                         the allocation holder's bitmap; see block_double_frees"
                    );
                    crate::fuse_client::METRICS
                        .block_double_frees
                        .fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
        }
        // FIND-RW5-A forensics (env-gated, diagnostic-only): record every
        // free's capture so a DOUBLE FREE names BOTH call sites.
        if free_forensics_enabled() {
            let bt = std::backtrace::Backtrace::force_capture().to_string();
            // RES-19: one bounded-ring insert (which also HANDS BACK the
            // prior capture for this offset), instead of holding the global
            // mutex across a probe + an unbounded insert.
            let prior = record_free_forensics(offset, bt.clone());
            if let Some(first) = prior {
                if self.free_blocks.contains(&block_idx) {
                    log::error!(
                        "DOUBLE FREE FORENSICS offset {offset}:\n--- free #1 ---\n{first}\n--- free #2 ---\n{bt}"
                    );
                }
            }
        }
        if !self.free_blocks.insert(block_idx) {
            // FIND-RW5-A forensics tripwire: a second release of an offset
            // already on the free list is the double-free family the
            // release_superseded_staged doc warns about — the lineage that
            // mints TWO live owners for one device offset once the next
            // two allocations both receive it.
            log::error!(
                "DOUBLE FREE: offset {offset} (block {block_idx}) was already free —                  double-release lineage; see block_double_frees"
            );
            crate::fuse_client::METRICS
                .block_double_frees
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // -----------------------------------------------------------------
    // Spec §6.8 item 3 — the freed-offset grace period's allocator half
    // (`crate::free_grace`; contracts in tests/reader_free_grace_tests.rs)
    // -----------------------------------------------------------------

    /// Release every acknowledged offset back to the free list — the
    /// ROUTINE harvest, run at every terminal free and at the head of the
    /// allocation funnel.
    ///
    /// Cost on a mount with no reader plane: one relaxed load
    /// (the empty-ring probe below), and nothing else — the free supply is
    /// computed only once something is actually held.
    /// `pub(crate)`: the shipped LANE HARVEST is a remote allocation
    /// funnel and runs the same head (finding 15 —
    /// `crate::cowriter::execute_lane_harvest`).
    #[inline]
    pub(crate) fn harvest_grace(&self) {
        if self.grace.is_empty() {
            return;
        }
        // KD-FG-10 (PR 3): under the `DEMAND` lever the runway's supply
        // input is the LANE-REACHABLE number — the quantity that troughs
        // on a recycle-bound stream, where the passed-global one
        // accumulates foreign-lane releases and provably never did on the
        // motivating row. `DEMAND=0` restores the passed-global input
        // verbatim (the lever's restore-exactly contract). Site 0's
        // conjunct always reads the lane-reachable number.
        let supply = self.grace_supply_blocks();
        let lane_reachable = self.lane_reachable_blocks();
        for offset in
            self.grace
                .harvest_with_supply(crate::free_grace::HARVEST_BATCH, supply, lane_reachable)
        {
            self.publish_grace_release(offset);
        }
    }

    /// **The release-on-ack harvest** (finding 15 term 2, the lane-push
    /// lever's authority half — `crate::free_grace::ReleaseHook`): the
    /// routine harvest repeated until the ring's front is uncovered, so a
    /// binding acknowledgement releases EVERY offset it covered, not the
    /// first batch. Bounded by the covered population (each pass pops ≤
    /// `HARVEST_BATCH`, and a pass that pops fewer has reached the front).
    pub fn harvest_grace_to_front(&self) {
        loop {
            if self.grace.is_empty() {
                return;
            }
            let supply = self.grace_supply_blocks();
            let lane_reachable = self.lane_reachable_blocks();
            let released = self.grace.harvest_with_supply(
                crate::free_grace::HARVEST_BATCH,
                supply,
                lane_reachable,
            );
            let n = released.len();
            for offset in released {
                self.publish_grace_release(offset);
            }
            if n < crate::free_grace::HARVEST_BATCH {
                return;
            }
        }
    }

    /// **Take up to `max` free-listed blocks of lane `lane`/`writers` for a
    /// lane-harvest handout** (the AUTHORITY side of
    /// `crate::cowriter::execute_lane_harvest`, one pass): lowest-first
    /// (deterministic, and as dense as a strided lane allows), each removed
    /// through [`Self::take_free_for_lane_grant`] (exactly-once), each with
    /// its release AGE for the lane-visible ledger — ms the block sat on
    /// this list since its grace release, or
    /// [`crate::free_grace::LANE_RELEASE_AGE_UNPLACED`] for a block that
    /// reached the list some other way.
    pub fn take_lane_free_blocks(&self, lane: u16, writers: u16, max: usize) -> Vec<(u64, u64)> {
        let mut candidates: Vec<u64> = self
            .free_blocks
            .iter()
            .map(|item| *item)
            .filter(|idx| crate::data_alloc_lane::block_lane_of(*idx, writers) == u64::from(lane))
            .collect();
        candidates.sort_unstable();
        let vol_tag = self.vol_tag();
        let mut out = Vec::new();
        for idx in candidates {
            if out.len() >= max {
                break;
            }
            if self.take_free_for_lane_grant(idx) {
                let age = crate::free_grace::take_lane_release(vol_tag, idx)
                    .unwrap_or(crate::free_grace::LANE_RELEASE_AGE_UNPLACED);
                out.push((idx, age));
            }
        }
        out
    }

    /// The supply number the grace runway reads (KD-FG-10's re-base):
    /// lane-reachable under the `DEMAND` lever, the passed-global
    /// pre-campaign number under `DEMAND=0`.
    pub fn grace_supply_blocks(&self) -> u64 {
        if crate::free_grace::demand_enabled() {
            self.lane_reachable_blocks()
        } else {
            self.free_supply_blocks()
        }
    }

    /// Blocks this allocator could hand out right now — the SPACE half of
    /// the §6.8 item-3 pressure signal (rung-20 residual 6): the free list
    /// plus the virgin tail, which on a lane-partitioned volume is already
    /// this writer's LANE share ([`Self::virgin_bytes`] divides by the
    /// partition width), because lane-share exhaustion is the shape the
    /// field convicted.
    ///
    /// `u64::MAX` on an unbounded allocator (offline tools, tests): space
    /// is then not a constraint and only the ring's own headroom is.
    /// `pub`: the PASSED-GLOBAL supply — the pre-campaign runway input
    /// (`grace_supply_blocks` resolves the `DEMAND` lever between this and
    /// the lane-reachable number; the restore-exactly contract reads both).
    pub fn free_supply_blocks(&self) -> u64 {
        if let Some(armed) = self.grant_supply_blocks() {
            return armed;
        }
        let virgin = self.virgin_bytes();
        if virgin == u64::MAX {
            return u64::MAX;
        }
        (virgin / self.chunk_size).saturating_add(self.free_blocks_count())
    }

    /// The supply a GRANT-ARMED allocator can hand out right now (PR 8
    /// review round 3, Issue 28): the window's unconsumed remainder plus,
    /// when this process HOLDS the volume's allocation lease, the bitmap's
    /// clear population (what the holder can still carve). The flat inputs
    /// — the virgin tail and the local free list — read 0 on an armed
    /// allocator at a full cursor (the list is drained at the arm, the
    /// tail is re-granted holes), which the free-grace valve would read as
    /// a permanent trough and prod / tighten / fence readers for. `None`
    /// unarmed (one `OnceLock` probe).
    fn grant_supply_blocks(&self) -> Option<u64> {
        let arm = self.block_grant.get()?;
        let held = crate::meta_backend::kv::alloc_lease::holding(arm.vol_tag).map_or(0, |h| {
            h.bitmap.blocks().saturating_sub(h.bitmap.population())
        });
        Some(arm.window.remaining().saturating_add(held))
    }

    /// The PRESSURE harvest: what allocation runs when it is about to
    /// refuse `StorageFull`. Same act, earlier deadline (one honest
    /// acknowledgement cycle instead of the routine bound) — it never
    /// releases an unacknowledged offset without evicting the member
    /// responsible. Returns the number published.
    /// `pub(crate)`: an EMPTY lane harvest is `StorageFull`-imminent on
    /// the remote writer, so it evaluates this same deadline (finding 15).
    pub(crate) fn harvest_grace_pressure(&self) -> usize {
        let released = self
            .grace
            .harvest_pressure(crate::free_grace::HARVEST_BATCH);
        let n = released.len();
        for offset in released {
            self.publish_grace_release(offset);
        }
        n
    }

    /// Offsets this volume holds in the grace period (the stats/df face:
    /// they are genuinely unavailable, so [`Self::get_used_blocks`] counts
    /// them as used).
    pub fn grace_len(&self) -> usize {
        self.grace.len()
    }

    /// Device bytes this volume holds in the grace period.
    pub fn grace_bytes(&self) -> u64 {
        self.grace.bytes()
    }

    /// PR 8: this volume's grace ring — the per-volume closure
    /// `deferrals ≡ releases + offsets` and the timeout-path gauge live on
    /// the ring itself (one ring per data volume, one clock).
    pub fn grace_ring(&self) -> &crate::free_grace::GraceRing {
        &self.grace
    }

    /// `true` ⇔ `offset` is held in this volume's grace period — the
    /// exemption probe for every path that reconciles "untracked and not
    /// free-listed" state (fsck C6, the elided-debt trim), and the
    /// contract tests' invariant.
    pub fn grace_holds(&self, offset: u64) -> bool {
        self.grace.holds(offset)
    }

    /// The oldest held label (`None` = nothing held) — the deadline
    /// instrument, and what an acknowledgement is measured against.
    pub fn grace_oldest_label(&self) -> Option<u64> {
        self.grace.oldest_label()
    }

    /// Release one reference and, when terminal, immediately publish the
    /// offset for reuse (begin + finish with no destructive work between).
    pub async fn free_block(&self, offset: u64) -> Result<()> {
        // DLM S5: gated explicitly rather than left to `begin_free`'s
        // `false`, because a `false` here is indistinguishable from a
        // legitimate NON-TERMINAL release — and a reader reaching this path
        // at all is a bug worth a loud error, not a silent `Ok`.
        self.plane_gate("block free")?;
        if self.begin_free(offset) {
            self.finish_free(offset);
        }
        Ok(())
    }

    /// **The co-writer-aware cleanup arm for a minted-but-NEVER-PUBLISHED
    /// offset** (DLM S9 — the 2026-08-19 post-fence storm fix). The
    /// error-cleanup class — a pipeline upload whose DMA or publish
    /// failed, the RES-9 mint guard, the lane-reservation give-back, the
    /// mover's destination undo, and (the follow-up sweep of d575be03's
    /// named residual) the staged legs: the fsync/dismount flush funnel
    /// (`upload_active_block_bytes` / `fold_upload_block` /
    /// `flush_one_active_block`), the staged promotion's commit undo, the
    /// rider-fold / staged-clone durable-spill undos, the truncate
    /// durable-clip undo and `write_striped`'s stored-image-fits refusal
    /// — holds an offset NO map names, and calls the allocator directly
    /// by offset (the router's ship seam routes KEYS, which these offsets
    /// never earned).
    ///
    /// On the **authority / solo** (and reader) postures this is
    /// [`Self::free_block`] verbatim. On a **CO-WRITER** it ABANDONS the
    /// offset leak-safe instead of storming the plane gate: the design's
    /// answer ([`crate::data_grant`]'s `check_free` refusal text, the
    /// `free_ship_failures` pattern) is that a co-writer's unfreeable
    /// offset *stays durably unreferenced and the next derivation (mount
    /// recovery / fsck C6) returns it to the free supply* — so the arm
    /// counts `cowriter_unpublished_abandons` (expected nonzero ONLY
    /// around custody loss, when every in-flight upload's publish refuses
    /// at once), logs at most ONE warn per burst (per-offset detail at
    /// debug), and touches neither the free list nor the refusal
    /// tripwire. Never wire the HAPPY displaced-free path through this —
    /// published keys ship via
    /// [`crate::cowriter::ship_displaced_frees`].
    ///
    /// **Finding 15 (the recycle arm):** on a LIVE co-writer whose engaged
    /// lane OWNS the offset, a never-published mint goes straight back to
    /// this mount's own free list. Nothing durable ever named it, so no
    /// ledger on any node moves — the act is the co-writer's private view
    /// of its own lane supply, exactly what [`Self::adopt_lane_free_grant`]
    /// performs for a harvested offset — and the next allocation serves it
    /// with the full claim discipline (fresh refcount, fresh lifetime
    /// stamp). Before it, every superseded overlay destination and every
    /// failed-publish upload on a healthy co-writer was "abandoned to the
    /// next derivation" — one block per event gone from the lane until a
    /// remount, a steady leak on a fleet that never remounts (counted
    /// `cowriter_unpublished_recycles`). The quiet abandon stays for the
    /// shapes it was built for: a poisoned/fenced custody era (the
    /// 2026-08-19 post-fence storm) and an offset outside this mount's
    /// lanes.
    pub async fn abandon_unpublished_offset(&self, offset: u64) -> Result<()> {
        if crate::fuse_client::co_writer_mount()
            && !crate::cowriter::authority_accounting_scope_active()
        {
            let idx = offset / self.chunk_size;
            if self.lanes.get().is_some()
                && self.lane_is_ours(idx)
                && !crate::data_custody::poisoned()
            {
                let _ = self.refcounts.remove_sync(&offset);
                let _ = self.harvest_grants.remove_sync(&idx);
                self.mark_incarnation_unstable(offset);
                if self.free_blocks.insert(idx) {
                    crate::fuse_client::METRICS
                        .cowriter_unpublished_recycles
                        .fetch_add(1, Ordering::Relaxed);
                    log::debug!(
                        "co-writer recycle: never-published offset {offset} on volume '{}' back \
                         on this mount's own lane free list",
                        self._volume_id
                    );
                } else {
                    log::error!(
                        "co-writer recycle REFUSED: never-published offset {offset} on volume \
                         '{}' is already on this mount's free list — the double-handout lineage \
                         (cowriter_unpublished_abandons)",
                        self._volume_id
                    );
                    crate::fuse_client::METRICS
                        .cowriter_unpublished_abandons
                        .fetch_add(1, Ordering::Relaxed);
                }
                return Ok(());
            }
            crate::fuse_client::METRICS
                .cowriter_unpublished_abandons
                .fetch_add(1, Ordering::Relaxed);
            if abandon_warn_due() {
                log::warn!(
                    "co-writer: abandoning never-published block offset(s) leak-safe (volume \
                     '{}', first offset {offset}): the publish that would have named them \
                     failed or refused, and a co-writer may not mutate ownership accounting — \
                     the next derivation (mount recovery / fsck C6) returns them to the free \
                     supply (cowriter_unpublished_abandons; further offsets at debug level)",
                    self._volume_id
                );
            } else {
                log::debug!(
                    "co-writer abandon: never-published offset {offset} on volume '{}' left \
                     to the next derivation",
                    self._volume_id
                );
            }
            return Ok(());
        }
        self.free_block(offset).await
    }

    /// [`Self::abandon_unpublished_offset`] keyed on the publish OUTCOME
    /// class (design-small-file-packing §5.3, FIND-PK-4 — PK4): a
    /// never-published offset whose shipped publish had a KNOWN outcome
    /// (refused, or never sent) takes the lane recycle; one whose transport
    /// failed against an owner that MAY have applied it (`Unknown`) is
    /// abandoned WITHOUT recycle — durable layouts may name it there, and
    /// recycling it into this lane would mint a double owner. On every
    /// posture but a live co-writer's the class is moot (the free is local).
    pub async fn abandon_unpublished_offset_with(
        &self,
        offset: u64,
        outcome: PackPublishOutcome,
    ) -> Result<()> {
        if outcome == PackPublishOutcome::Unknown
            && crate::fuse_client::co_writer_mount()
            && !crate::cowriter::authority_accounting_scope_active()
        {
            self.abandon_without_recycle(offset);
            return Ok(());
        }
        self.abandon_unpublished_offset(offset).await
    }

    /// The quiet leak-safe abandon arm of [`Self::abandon_unpublished_offset`]
    /// WITHOUT the finding-15 lane recycle: the private entry is dropped,
    /// the word left unstable, and the offset stays out of every local
    /// allocation path until a harvest or remount re-derives it from the
    /// ledger. The disposition for a co-writer pack whose publishes have an
    /// UNKNOWN outcome (§5.3 — FIND-PK-4's closure): durable layouts MAY
    /// name the block on the owner, so recycling it into this mount's lane
    /// free list would mint a double owner. Counted
    /// `cowriter_unpublished_abandons`.
    fn abandon_without_recycle(&self, offset: u64) {
        let idx = offset / self.chunk_size;
        let _ = self.refcounts.remove_sync(&offset);
        let _ = self.harvest_grants.remove_sync(&idx);
        self.mark_incarnation_unstable(offset);
        crate::fuse_client::METRICS
            .cowriter_unpublished_abandons
            .fetch_add(1, Ordering::Relaxed);
        log::debug!(
            "co-writer abandon (outcome unknown): offset {offset} on volume '{}' left to the \
             next derivation, never recycled",
            self._volume_id
        );
    }

    /// **The pack block's ONE release primitive**
    /// (design-small-file-packing §5.3): the packer's pin at the seal, a
    /// refused tenant's reference and a failed DMA's reference all release
    /// through here. Runs OUTSIDE every 3.5 guard (RES-1 — the terminal
    /// arm's reclaim enqueue parks at the reclaim cap) and dispatches on
    /// posture, on the entry's state and on whether the pack's publishes
    /// have a KNOWN outcome:
    ///
    /// * **authority / solo**: the ROUTER ladder
    ///   ([`crate::routing::BackendRouter::free_block`]) for nonterminal
    ///   and terminal alike — a pack block's end of life is byte-identical
    ///   to a striped block's (read-tier purge, incarnation retire,
    ///   reclaim-queue enqueue, discard-elision debt, grace ring and S7
    ///   quarantine composed); an authority's outcome is always known.
    ///   Untracked is unreachable here (the pin keeps the entry alive until
    ///   this release) — counted `pack_release_untracked_noops`, logged
    ///   ERROR.
    /// * **co-writer**: nonterminal → a PRIVATE release of the mount's own
    ///   view (nothing ships — nothing durable ever named the pin);
    ///   terminal + KNOWN → [`Self::abandon_unpublished_offset`] (the
    ///   never-published arm: the lane recycle); terminal + UNKNOWN →
    ///   `abandon_without_recycle`; untracked → a counted no-op
    ///   (the last committed tenant's shipped `Freed` already ran
    ///   `retire_shipped_free_tracking` — the authority owns the offset
    ///   now, and a terminal arm here would hand it to this lane a SECOND
    ///   time). The co-writer's batch-scoped pack (PK4,
    ///   `DataRouter::promote_staged_batch`) seals through these arms at
    ///   its frame reply with the frame's typed outcome class.
    /// * a **reader** promotes nothing and never reaches this.
    pub async fn release_pack_reference(
        &self,
        router: &crate::routing::BackendRouter,
        block_key: &str,
        outcome: PackPublishOutcome,
    ) -> Result<PackRelease> {
        let offset = router.block_key_offset(block_key)?;
        let Some(count) = self.refcount(offset) else {
            crate::fuse_client::METRICS
                .pack_release_untracked_noops
                .fetch_add(1, Ordering::Relaxed);
            if crate::fuse_client::co_writer_mount()
                && !crate::cowriter::authority_accounting_scope_active()
            {
                log::debug!(
                    "packing: release of untracked pack block {block_key} on a co-writer — the \
                     last committed tenant's shipped free retired it; no-op"
                );
            } else {
                log::error!(
                    "packing: release of UNTRACKED pack block {block_key} on the authority — \
                     the pin's own reference should have kept the entry alive; no-op \
                     (pack_release_untracked_noops)"
                );
            }
            return Ok(PackRelease::UntrackedNoop);
        };
        if crate::fuse_client::co_writer_mount()
            && !crate::cowriter::authority_accounting_scope_active()
        {
            if count > 1 {
                self.release_shipped_free_tracking(offset);
                return Ok(PackRelease::Nonterminal);
            }
            match outcome {
                PackPublishOutcome::Known => {
                    self.abandon_unpublished_offset(offset).await?;
                }
                PackPublishOutcome::Unknown => self.abandon_without_recycle(offset),
            }
            return Ok(PackRelease::Terminal);
        }
        let terminal = router.free_block_verdict(block_key).await?;
        Ok(if terminal {
            PackRelease::Terminal
        } else {
            PackRelease::Nonterminal
        })
    }

    pub fn get_used_blocks(&self) -> u64 {
        let highest = self.highest_block.load(Ordering::Relaxed);
        let free = self.free_blocks.len() as u64;
        highest.saturating_sub(free)
    }

    pub async fn calculate_fragmentation(&self) -> Result<(u64, u64, u64, f64)> {
        let highest_block = self.highest_block.load(Ordering::Relaxed);
        let free_blocks = self.free_blocks.len() as u64;
        let used_blocks = highest_block.saturating_sub(free_blocks);
        let frag_percent = if highest_block > 0 {
            (free_blocks as f64 / highest_block as f64) * 100.0
        } else {
            0.0
        };

        Ok((highest_block, used_blocks, free_blocks, frag_percent))
    }

    pub async fn allocate_specific_block(&self, block_idx: u64) -> Result<()> {
        self.plane_gate("specific block allocation")?;
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        if block_idx >= cur_highest {
            // Rev 2 correction C (device-overlay KD-OV-14): the honest
            // GENERIC recovery census — offsets free-listed by this
            // gap-completing arm. The census structurally cannot tell an
            // overlay's abandoned destination from any other
            // allocated-but-unpublished offset (or from ordinary free
            // space below the cursor); overlay-specific attribution
            // lives only in the fault-injection suites.
            if block_idx > cur_highest {
                crate::fuse_client::METRICS
                    .unpublished_offsets_recovered
                    .fetch_add(
                        block_idx - cur_highest,
                        std::sync::atomic::Ordering::Relaxed,
                    );
            }
            for idx in cur_highest..block_idx {
                self.free_blocks.insert(idx);
            }
            self.highest_block.store(block_idx + 1, Ordering::Relaxed);
        } else {
            if !self.free_blocks.remove(&block_idx).is_some() {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Block {} is not free or does not exist",
                    block_idx
                )));
            }
        }
        let offset = block_idx * self.chunk_size;
        // Claim-cancels-debt (Idea 4): a specifically-claimed offset is
        // owned again — its lingering elided debt dies with the claim.
        self.cancel_elided_debt(offset);
        let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
        Ok(())
    }

    pub async fn get_free_blocks(&self) -> Result<Vec<u64>> {
        Ok(self.free_block_indices())
    }

    /// Sorted snapshot of the free-list indices — the sync twin of
    /// [`Self::get_free_blocks`] (PR VL7: the D1 contiguity census reads
    /// it latch-free from sync contexts).
    pub fn free_block_indices(&self) -> Vec<u64> {
        let mut list: Vec<u64> = self.free_blocks.iter().map(|item| *item).collect();
        list.sort_unstable();
        list
    }

    pub async fn recover_block(&self, block_idx: u64) -> Result<()> {
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        let offset = block_idx * self.chunk_size;

        if block_idx >= cur_highest {
            for idx in cur_highest..block_idx {
                self.free_blocks.insert(idx);
            }
            self.highest_block.store(block_idx + 1, Ordering::Relaxed);
            let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
        } else {
            if self.free_blocks.remove(&block_idx).is_some() {
                let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
            } else {
                match self.refcounts.entry_sync(offset) {
                    scc::hash_map::Entry::Occupied(mut occ) => {
                        occ.get_mut().fetch_add(1, Ordering::SeqCst);
                    }
                    scc::hash_map::Entry::Vacant(vac) => {
                        let _ = vac.insert_entry(AtomicU32::new(1));
                    }
                }
            }
        }
        Ok(())
    }

    /// **The derived census — the verification oracle** (pre-RC
    /// engineering spec §6.2 item 1).
    ///
    /// The same walk [`Self::recover_active_blocks_v3`] runs, but folded
    /// into a `block_idx → reference count` map instead of mutated into
    /// the allocator. Once block ownership is DURABLE, this walk stops
    /// being the source of truth and becomes the check: a mount (or fsck)
    /// that compares the durable census against this one is the strongest
    /// available test of the accounting, because the two are the same
    /// multiset by construction — one durable record per layout map
    /// entry.
    ///
    /// Deliberately shares `layout_owned_blocks` with the
    /// recovery path: an oracle that re-implemented the extraction would
    /// only ever test the re-implementation.
    pub async fn derived_block_census(
        &self,
        kv: &crate::meta_backend::kv::backend::KvMetaBackend,
        backend_router: &crate::routing::BackendRouter,
    ) -> Result<std::collections::BTreeMap<u64, u32>> {
        let mut census: std::collections::BTreeMap<u64, u32> = std::collections::BTreeMap::new();
        let mut indirect: Vec<String> = Vec::new();
        let mut kvmap: Vec<u64> = Vec::new();
        self.walk_live_layouts(kv, |ino, layout| {
            let (mut owned, mut blobs) = self.layout_owned_blocks(backend_router, layout);
            for idx in owned.drain(..) {
                *census.entry(idx).or_insert(0) += 1;
            }
            indirect.append(&mut blobs);
            if layout_is_kvmap(layout) {
                kvmap.push(ino);
            }
        })
        .await?;
        // The indirect blobs' ENTRIES need device reads, which cannot run
        // inside the synchronous visitor (never `block_on` a device read
        // from an async context — the write-funnel conviction).
        for key in indirect {
            for idx in self.indirect_owned_blocks(backend_router, &key).await {
                *census.entry(idx).or_insert(0) += 1;
            }
        }
        // PR 2 (kvmap, Rev 1.1 #3): a `kvmap:` head's entries live in
        // tree 7 — the SHARED extraction resolves them, so the oracle's
        // derived side and the fsck census cannot drift apart.
        for ino in kvmap {
            for (_b, key) in backend_router.kvmap_layout_entries(kv, ino).await {
                if let Some(offset) = self.owned_offset(backend_router, &key) {
                    *census.entry(offset / self.chunk_size).or_insert(0) += 1;
                }
            }
        }
        Ok(census)
    }

    /// Block refcount recovery over a metadata volume — walk the live
    /// inode tree (paged range scans) and seed the RAM refcount map + free
    /// list from each live ino's `"layout"` xattr.
    ///
    /// On a volume carrying incompat bit 8 this is no longer the mount
    /// path (see [`crate::routing::BackendRouter::recover_durable_block_refs`]
    /// — durable records replace the walk); it remains the oracle's engine
    /// (via [`Self::derived_block_census`]) and the un-stamped volume's
    /// only accounting, byte-identical to its pre-item-1 behavior.
    pub async fn recover_active_blocks_v3(
        &self,
        kv: &crate::meta_backend::kv::backend::KvMetaBackend,
        backend_router: &crate::routing::BackendRouter,
    ) -> Result<()> {
        // DLM S5 (spec §6.8 item 1, the walk's **free-completing arm**):
        // `recover_block` declares every gap below the cursor FREE, so the
        // walk does not merely *observe* ownership — it manufactures a
        // free list from the tree it happened to see. On a reader that
        // tree is a snapshot of one checkpoint, and the offsets a live
        // writer allocated after it would be free-listed here. A reader
        // allocates nothing and frees nothing, so the whole pass is
        // refused rather than made "harmless".
        self.plane_gate("block-ownership recovery walk")?;
        // Two passes' worth of work in one: collect the owned indices
        // under the walk (which borrows `self` immutably), then seed.
        // `recover_block` is `async`, so it cannot run inside the
        // synchronous visitor.
        let mut indices: Vec<u64> = Vec::new();
        let mut indirect: Vec<String> = Vec::new();
        let mut kvmap: Vec<u64> = Vec::new();
        let summary = self
            .walk_live_layouts(kv, |ino, layout| {
                let (owned, mut blobs) = self.layout_owned_blocks(backend_router, layout);
                indices.extend(owned);
                indirect.append(&mut blobs);
                if layout_is_kvmap(layout) {
                    kvmap.push(ino);
                }
            })
            .await?;
        for key in indirect {
            indices.extend(self.indirect_owned_blocks(backend_router, &key).await);
        }
        // PR 2 (kvmap, Rev 1.1 #2 — pulled-forward safety): a `kvmap:`
        // head yielding ZERO owned blocks would let gap-completion
        // free-list LIVE data on a derived (bit-9-absent) mount — the
        // tree-7 arm is mandatory HERE, not in the walkers PR.
        for ino in kvmap {
            for (_b, key) in backend_router.kvmap_layout_entries(kv, ino).await {
                if let Some(offset) = self.owned_offset(backend_router, &key) {
                    indices.push(offset / self.chunk_size);
                }
            }
        }
        for idx in indices {
            let _ = self.recover_block(idx).await;
        }
        // log, not stdout: this walk also runs under `squeezefs df`, whose
        // `--json` output must stay machine-parseable.
        log::info!(
            "Recovery scan summary (v3): checked={}, valid_inodes={}, layouts_found={}",
            summary.checked,
            summary.valid_inodes,
            summary.layouts_found
        );
        Ok(())
    }

    /// [`walk_live_layouts`] bound to this allocator. The visitor gets the
    /// volume-LOCAL ino too (PR 2): a `kvmap:` head's entries live in
    /// tree-7 records keyed on it, so the kvmap async pass needs the ino
    /// the layout alone no longer carries.
    async fn walk_live_layouts<F>(
        &self,
        kv: &crate::meta_backend::kv::backend::KvMetaBackend,
        visit: F,
    ) -> Result<LayoutWalkSummary>
    where
        F: FnMut(u64, &crate::routing::LayoutMetadata),
    {
        walk_live_layouts(kv, visit).await
    }

    /// Every block index on THIS volume that `layout` durably references
    /// — the map entries plus, for an indirect map, the blob block itself
    /// and every entry the blob names.
    ///
    /// FIND-RW5-A remount face: STAGED files carry durable `block_map`
    /// entries too — the staged truncate-clip (`bk:0:len`) and the
    /// StorageFull durable spills. Gating this on `"striped"` left those
    /// live offsets untracked AND free-listed on a fresh allocator (the
    /// gap-fill claims nothing for them), so `allocate_block` minted the
    /// SAME offset to a second owner while the staged file's map still
    /// bound it — the generic/464 post-remount never-settles EIO +
    /// refused-untracked-free storm. Every layout that carries durable
    /// block references counts, regardless of `file_type`.
    ///
    /// Stored values are backend-true key strings with an optional
    /// `:extra` trailer after the offset — stripped with
    /// [`crate::routing::clean_block_key`] (NEVER a bare `split(':')`,
    /// which mangles prefixed keys: `oss2://123` → `oss2`).
    fn layout_owned_blocks(
        &self,
        backend_router: &crate::routing::BackendRouter,
        layout: &crate::routing::LayoutMetadata,
    ) -> (Vec<u64>, Vec<String>) {
        let mut out = Vec::new();
        let mut indirect = Vec::new();
        if layout.file_type != "striped" && layout.file_type != "staged" {
            return (out, indirect);
        }
        // 1. Indirect block map: the blob block itself now; its ENTRIES
        //    need a device read, so the key is handed back for the
        //    caller's async pass ([`Self::indirect_owned_blocks`]).
        if let Some(indirect_key) = layout
            .block_map_id
            .as_deref()
            .and_then(|id| id.strip_prefix("indirect:"))
        {
            if let Some(offset) = self.owned_offset(backend_router, indirect_key) {
                out.push(offset / self.chunk_size);
            }
            indirect.push(indirect_key.to_string());
        }
        // 2. Inline block map.
        if let Some(bm) = layout.block_map.as_ref() {
            for offset_str in bm.values() {
                if let Some(offset) = self.owned_offset(backend_router, offset_str) {
                    out.push(offset / self.chunk_size);
                }
            }
        }
        (out, indirect)
    }

    /// The block indices THIS volume owns among an indirect block map's
    /// entries. The blob carries backend-true key strings (versioned v1
    /// blob): reducing them to bare offsets — the retired pre-versioned
    /// shape — lost the owning volume, so every rehydrated non-first-volume
    /// block was read/freed from the wrong device.
    async fn indirect_owned_blocks(
        &self,
        backend_router: &crate::routing::BackendRouter,
        indirect_key: &str,
    ) -> Vec<u64> {
        let mut out = Vec::new();
        let block_size = backend_router.block_size.load(Ordering::Relaxed) as usize;
        match backend_router.read_block(indirect_key, block_size).await {
            Ok(raw_bytes) => match crate::routing::decode_indirect_block_map(&raw_bytes) {
                Ok(entries) => {
                    for (_b, key) in entries {
                        if let Some(offset) = self.owned_offset(backend_router, &key) {
                            out.push(offset / self.chunk_size);
                        }
                    }
                }
                Err(e) => log::warn!(
                    "refcount recovery: undecodable indirect block map at \
                     '{indirect_key}': {e}"
                ),
            },
            Err(e) => log::warn!(
                "refcount recovery: unreadable indirect block map at '{indirect_key}': {e}"
            ),
        }
        out
    }

    /// `Some(offset)` ⇔ `block_key` names an offset on THIS volume.
    ///
    /// Alias-aware exactly as the historical walk was: the unprefixed
    /// first-volume forms (`123`) and the `backend_0` / `squeezefs`
    /// aliases all resolve to the router's default allocator, which is
    /// what keeps pre-VL3 layouts accountable.
    fn owned_offset(
        &self,
        backend_router: &crate::routing::BackendRouter,
        block_key: &str,
    ) -> Option<u64> {
        let cleaned = crate::routing::clean_block_key(block_key);
        let parts = backend_router.parse_block_key_parts(&cleaned).ok()?;
        let (be_id, offset) = (parts.be_id.as_str(), parts.offset);
        let default_id = backend_router.default_allocator.volume_id();
        let mine = self._volume_id.as_ref();
        let owned = be_id == mine
            || ((be_id == "backend_0" || be_id == "squeezefs")
                && (mine == "squeezefs" || mine == default_id));
        // Spec §6.2 item 6: the walk already parsed the key, so seeding the
        // offset's lifetime here is free — and it is what lets a block laid
        // down by a PREVIOUS mount present its real era instead of reading
        // as "unknown". Not a correctness dependency: the dangerous case is
        // a REALLOCATION, and every reallocation mints through
        // `claim_block_idx` in this process.
        if owned {
            self.seed_incarnation(offset, parts.incarnation);
        }
        owned.then_some(offset)
    }

    /// Seed the RAM refcount map + free list + allocation cursor from
    /// **durable** block-reference records (pre-RC engineering spec §6.2
    /// item 1) — the mount path that replaces the inode-tree walk.
    ///
    /// `refs` is this volume's whole durable reference set
    /// ([`crate::meta_backend::kv::backend::KvMetaBackend::block_ref_scan`],
    /// summed across the mounted meta volumes). Seeding runs the SAME
    /// [`Self::recover_block`] protocol the derived walk runs — once per
    /// reference — so the resulting refcounts, gap-filled free list, and
    /// `highest_block` cursor are identical to the derived answer by
    /// construction, and the free list stays what it has always been: the
    /// complement of the referenced set below the cursor.
    ///
    /// Ordering: references are seeded in ascending block order so the
    /// cursor advances monotonically and the gap fill runs once per gap.
    pub async fn seed_from_durable_refs(&self, refs: &[u64]) -> u64 {
        let mut sorted: Vec<u64> = refs.to_vec();
        sorted.sort_unstable();
        let seeded = sorted.len() as u64;
        for idx in sorted {
            let _ = self.recover_block(idx).await;
        }
        crate::meta_backend::kv::META_KV_BLOCK_REFS_RECOVERED.fetch_add(seeded, Ordering::Relaxed);
        seeded
    }
}

/// Collect every `nlink == 0` CORPSE ino on one metadata volume — the
/// complement of [`walk_live_layouts`]'s skip, over the same paged range
/// scan.
///
/// The population this names (POSIX-15's missing half, found by PR 8's
/// acceptance oracle, 2026-08-23): unlinked inodes whose final kernel
/// FORGET never arrived before the mount died — connection-abort unmount,
/// kill -9, or plain kernel cache retention. The FORGET path is the ONLY
/// live reclaimer, the census walk skips the shape, and fsck C9
/// deliberately declines it, so every such corpse leaked its blocks and
/// (on a bit-9 volume) its durable reference records FOREVER — the C2+C8
/// pair per block the tar-x oracle went red on.
/// [`crate::routing::DataRouter::sweep_unlinked_corpses`] is the consumer.
pub(crate) async fn collect_corpse_inos(
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
) -> Result<Vec<u64>> {
    use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
    let mut out = Vec::new();
    let mut cursor: Vec<u8> = inode_key(2).to_vec();
    let end = inode_key(u64::MAX - 1);
    loop {
        let page = kv
            .range_kind(
                crate::meta_backend::kv::record::TREE_INODES,
                &cursor,
                &end,
                512,
            )
            .await
            .map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "v3 corpse-sweep inode walk failed: {e}"
                ))
            })?;
        let Some((last_key, _)) = page.last() else {
            break;
        };
        cursor = crate::meta_backend::kv::node::key_successor(last_key);
        for (k, v) in &page {
            let Ok(ino) = decode_inode_key(k) else {
                continue;
            };
            let Ok(val) = InodeValue::decode(v) else {
                continue;
            };
            if val.nlink == 0 {
                out.push(ino);
            }
        }
    }
    Ok(out)
}

/// Walk every layout-OWNING inode's decoded `"layout"` xattr on one
/// metadata volume (paged range scans), handing each `(ino, layout)` to
/// `visit`.
///
/// **`nlink == 0` corpses are INCLUDED** (2026-08-23, the corpse-census
/// correction): an unlinked inode awaiting its kernel FORGET is legal
/// POSIX state that still OWNS its blocks until reclaim runs — the old
/// skip made the derived seed hand corpse blocks out as free, the oracle
/// read every corpse's ledger record as drift (the fsck C8 false positive
/// on healthy live mounts), and the backfill dropped them. The block
/// plane counts owners; WHO may destroy the owner is the inode plane's
/// question (fsck C9/C10 keep their own walks and their deliberate
/// `nlink == 0` exclusions).
///
/// The shared engine of three things that must never disagree: the
/// mount-time refcount seed, its durable-vs-derived **oracle**, and the
/// §6.2-item-1 **backfill**. A free function precisely because it touches
/// no allocator state — an allocator-bound copy would invite one of the
/// three to drift.
/// PR 2 (kvmap): does this layout's map live in the block-map tree (a
/// `kvmap:` head)? The `layout_owned_blocks` file-type gate applies to
/// the tree arm too — only striped/staged layouts carry durable block
/// references.
fn layout_is_kvmap(layout: &crate::routing::LayoutMetadata) -> bool {
    (layout.file_type == "striped" || layout.file_type == "staged")
        && layout
            .block_map_id
            .as_deref()
            .is_some_and(|id| id.starts_with(crate::meta_backend::kv::block_map::KVMAP_HEAD_PREFIX))
}

pub(crate) async fn walk_live_layouts<F>(
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
    mut visit: F,
) -> Result<LayoutWalkSummary>
where
    F: FnMut(u64, &crate::routing::LayoutMetadata),
{
    use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
    let mut summary = LayoutWalkSummary::default();
    let mut cursor: Vec<u8> = inode_key(1).to_vec();
    let end = inode_key(u64::MAX - 1);
    loop {
        let page = kv
            .range_kind(
                crate::meta_backend::kv::record::TREE_INODES,
                &cursor,
                &end,
                512,
            )
            .await
            .map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "v3 recovery inode walk failed: {e}"
                ))
            })?;
        let Some((last_key, _)) = page.last() else {
            break;
        };
        cursor = crate::meta_backend::kv::node::key_successor(last_key);
        for (k, v) in &page {
            let Ok(ino) = decode_inode_key(k) else {
                continue;
            };
            // A corpse (nlink == 0) still owns its blocks — see the
            // function doc. Decode success is the record-validity gate.
            let Ok(_val) = InodeValue::decode(v) else {
                continue;
            };
            summary.checked += 1;
            summary.valid_inodes += 1;
            if let Ok(Some(bytes)) = kv.getxattr(ino, "layout").await {
                summary.layouts_found += 1;
                let layout_opt: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{")
                {
                    serde_json::from_slice(&bytes).ok()
                } else {
                    bincode::deserialize(&bytes).ok()
                };
                if let Some(layout) = layout_opt {
                    visit(ino, &layout);
                }
            }
        }
    }
    Ok(summary)
}

/// Counters from one live-layout walk (the log line's inputs).
#[derive(Default)]
pub(crate) struct LayoutWalkSummary {
    pub(crate) checked: u64,
    pub(crate) valid_inodes: u64,
    pub(crate) layouts_found: u64,
}

/// Whether a pack block's publishes have a KNOWN outcome when its
/// reference releases (design-small-file-packing §5.3): an authority's
/// commit is local — always `Known`; on a co-writer a shipped commit
/// whose transport failed past the resend ladder against an owner that
/// MAY have applied it is `Unknown` (FIND-PK-4 — minted from the typed
/// publish outcome `PublishFailureClass::TransportOutcomeUnknown`, never
/// from a message string), and the terminal arm then abandons WITHOUT
/// the lane recycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackPublishOutcome {
    Known,
    Unknown,
}

/// What [`BlockAllocator::release_pack_reference`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackRelease {
    /// References remain (tenants live): one reference released.
    Nonterminal,
    /// The block's last reference: the terminal free (the router ladder
    /// on the authority; the abandon arm on a co-writer).
    Terminal,
    /// No refcount entry: nothing to release (counted
    /// `pack_release_untracked_noops` — expected only on a co-writer whose
    /// tenants were all deleted between the group landing and the seal).
    UntrackedNoop,
}

/// RAII registration in the [`BlockAllocator::inflight_register`]
/// in-flight allocation registry (PR VL6a, design-volume-lifecycle
/// §5.6). Held by the live owner of an allocated-but-unpublished offset
/// across its allocate → DMA → meta-publish window; dropping it is the
/// deregistration (contract: only after the publish is durable and
/// visible, or on the owner's failure path where the offset is freed).
pub struct InflightAllocGuard {
    alloc: Arc<BlockAllocator>,
    offset: u64,
}

impl Drop for InflightAllocGuard {
    fn drop(&mut self) {
        let remove = self
            .alloc
            .inflight
            .update_sync(&self.offset, |_, c| {
                *c = c.saturating_sub(1);
                *c == 0
            })
            .unwrap_or(false);
        if remove {
            self.alloc.inflight.remove_sync(&self.offset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capacity contract (the multi-volume bench EIO investigation): a
    /// bounded allocator must refuse to mint offsets past its device end —
    /// on real block devices the write would EIO/ENOSPC at DMA time, and on
    /// FILE-backed volumes it silently grew the file past its provisioned
    /// size. Freed blocks make the offset pool reusable again; capacity 0
    /// stays unbounded for offline tools.
    #[tokio::test]
    async fn allocate_block_respects_device_capacity() {
        let a = BlockAllocator::new("cap_test").await.unwrap();
        a.set_capacity_bytes(3 * a.chunk_size());

        let o0 = a.allocate_block().await.expect("block 0");
        let o1 = a.allocate_block().await.expect("block 1");
        let o2 = a.allocate_block().await.expect("block 2");
        assert_eq!((o0, o1, o2), (0, a.chunk_size(), 2 * a.chunk_size()));

        let refused = a.allocate_block().await;
        match refused {
            Err(crate::error::SqueezefsError::Io(ref e))
                if e.kind() == std::io::ErrorKind::StorageFull => {}
            other => panic!("allocation past device capacity must fail StorageFull, got {other:?}"),
        }

        // A freed block re-opens exactly one slot.
        a.free_block(o1).await.unwrap();
        let again = a.allocate_block().await.expect("reuse freed block");
        assert_eq!(again, o1);
        assert!(
            a.allocate_block().await.is_err(),
            "pool exhausted again after the freed slot was reused"
        );
    }
}
