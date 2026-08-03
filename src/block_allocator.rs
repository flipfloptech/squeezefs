use crate::error::Result;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

fn is_storage_full(e: &crate::error::SqueezefsError) -> bool {
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

pub struct BlockAllocator {
    _volume_id: Box<str>,
    chunk_size: u64,
    free_blocks: dashmap::DashSet<u64>,
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
    /// writer term (bit 7). `None` on every volume today (ruling D9:
    /// build the bit, do not stamp it), and then every minted key is the
    /// bare offset form, byte-identical to the shipped one.
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
    /// DLM **S9** blocker #3: this volume's **allocation partition**
    /// ([`crate::data_alloc_lane`]) — `None` on every mount today AND on
    /// every solo mount forever, which is what makes single-writer
    /// allocation not merely equivalent to the shipped path but literally
    /// it ([`Self::engage_alloc_lanes`] installs nothing at `writers == 1`).
    lanes: std::sync::OnceLock<LanePartition>,
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
            free_blocks: dashmap::DashSet::new(),
            highest_block: AtomicU64::new(0),
            capacity_blocks: AtomicU64::new(0),
            refcounts: scc::HashMap::new(),
            fsck_scan_epoch: AtomicU64::new(0),
            fsck_scan_active: std::sync::atomic::AtomicBool::new(false),
            fsck_epoch_map: scc::HashMap::new(),
            inflight: scc::HashMap::new(),
            space_valve: std::sync::OnceLock::new(),
            space_pending: std::sync::OnceLock::new(),
            elided_debt: scc::HashMap::new(),
            elided_debt_bytes: AtomicU64::new(0),
            incarnations: scc::HashMap::new(),
            stamps_present: std::sync::atomic::AtomicBool::new(false),
            incarnation_minter: std::sync::OnceLock::new(),
            quarantine: crate::data_custody::BlockQuarantine::new(),
            grace: crate::free_grace::GraceRing::derived(),
            lanes: std::sync::OnceLock::new(),
        })
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
        };
        let grain = installed.grain;
        if self.lanes.set(installed).is_err() {
            return Ok(());
        }
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
    /// not the holder: [`Self::reserve_lane_frontier`] declares the dense
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
    /// `0` when unpartitioned.
    pub fn foreign_lane_free_blocks(&self) -> u64 {
        let Some(lanes) = self.lanes.get() else {
            return 0;
        };
        let owned = lanes.owned.load(Ordering::Acquire);
        let writers = lanes.part.writers();
        self.free_blocks
            .iter()
            .filter(|idx| {
                owned & (1u64 << crate::data_alloc_lane::block_lane_of(**idx, writers)) == 0
            })
            .count() as u64
    }

    /// `true` ⇔ `block_idx` is in a lane this mount may mint in (always
    /// `true` when unpartitioned — the shipped answer).
    fn lane_is_ours(&self, block_idx: u64) -> bool {
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
    /// Associated (not `&self`) deliberately: the posture is the MOUNT's,
    /// never one allocator's, so no per-volume state can drift out of
    /// agreement with it.
    #[inline]
    fn plane_gate(what: &str) -> Result<()> {
        if crate::fuse_client::read_only_mount() {
            let e = crate::fuse_client::read_only_refusal(what);
            log::error!("{e}");
            return Err(e);
        }
        if crate::fuse_client::co_writer_mount() {
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
                    self.publish_free_list(*offset);
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
    pub fn claim_free_for_trim(self: &Arc<Self>, offset: u64) -> Option<InflightAllocGuard> {
        let idx = offset / self.chunk_size;
        self.free_blocks
            .remove(&idx)
            .map(|_| self.inflight_register(offset))
    }

    /// Return a trim-claimed offset to the free list (the claim window
    /// ends; the caller drops the in-flight guard after this).
    pub fn return_from_trim(&self, offset: u64) {
        self.free_blocks.insert(offset / self.chunk_size);
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
    /// walks ([`Self::owned_offset`]), so an offset laid down by a previous
    /// mount presents its real era instead of "unknown".
    ///
    /// Never overwrites a stamp: an allocation ([`Self::claim_block_idx`])
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

    /// Snapshot the incarnation word for a fill. `None` while unstable
    /// (in-flight write or retired/free) — the fill must not publish. Offsets
    /// with no recorded incarnation (written before this process / by another
    /// node) are treated as stable.
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
        // door (EROFS) and at the allocator, so this arm is structurally
        // unreachable in production — a counter that can only ever read 0
        // is exactly the dead weight the ledger's rot-detection value
        // depends on not having.
        if Self::plane_gate("W1 in-place sub-block patch").is_err() {
            return false;
        }
        self.mark_incarnation_unstable(offset);
        crate::patch_clone_core::cross_word_fence();
        self.refcount(offset) == Some(1)
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
        if !crate::dlm::span_range_shared(ino, block_start, block_end, holder_token) {
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
        log::error!("{msg}");
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
            let _ = self.free_block(offset).await;
            return Err(e);
        }
        Ok(offset)
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
        if let Err(e) = Self::plane_gate("block allocation") {
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
        if !is_storage_full(&e) || self.space_valve.get().is_none() {
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
        // RES-1 5 (pre-RC engineering spec §7): the `pending` exit alone
        // is not a bound. Under concurrent reclaim traffic OTHER writers
        // keep the queue non-empty, so every pass observes `pending ==
        // true` and the honest-refusal exit is never reached — the
        // allocating task spins on a genuinely full store instead of
        // returning `StorageFull`. The attempt cap below is the
        // liveness floor: past it the verdict stands, loudly.
        for attempt in 0..ENOSPC_VALVE_MAX_ATTEMPTS {
            if let ok @ Ok(_) = self.try_allocate_block() {
                return self.hand_out_reserved(ok).await;
            }
            let pending = self.space_pending.get().map(|p| p()).unwrap_or(false);
            if let Some(valve) = self.space_valve.get() {
                valve().await;
            }
            if !pending {
                // Nothing was owed before the final drain: the verdict
                // stands (genuine fullness refuses StorageFull).
                let last = self.try_allocate_block();
                return self.hand_out_reserved(last).await;
            }
            let _ = attempt;
        }
        log::error!(
            "allocate_block: the ENOSPC pressure valve ran {ENOSPC_VALVE_MAX_ATTEMPTS} \
             drain-and-retry passes with reclaims still pending and never freed a \
             block — refusing StorageFull rather than spinning (the store is full \
             and the reclaimer is not gaining on it; check \
             block_free_reclaim_queue_bytes and block_free_reclaim_fence_halts)"
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
                return Ok(self.claim_block_idx(idx));
            }
            // Lost the claim race: rescan for the next candidate.
        }
        let block_idx = self.next_fresh_block()?;
        log::debug!(
            "allocate_block: offset {} (fresh)",
            block_idx * self.chunk_size
        );
        Ok(self.claim_block_idx(block_idx))
    }

    /// The shared post-claim tail: refcount 1, incarnation unstable (cache
    /// fills must not publish until the owner's `publish_block`), and the
    /// PR VL6a fsck epoch-latch record. Returns the byte offset.
    fn claim_block_idx(&self, block_idx: u64) -> u64 {
        let offset = block_idx * self.chunk_size;
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
            log::error!(
                "CLAIM ANOMALY: offset {offset} claimed from the free list while a \
                 refcount entry lingers (count={:?})",
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
        Self::plane_gate("block allocation (contiguity pick)").ok()?;
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
        Self::plane_gate("block allocation (ascending pick)")?;
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
        if Self::plane_gate("terminal block free").is_err() {
            return false;
        }
        let should_free = if let Some(terminal) = self
            .refcounts
            .read_sync(&offset, |_, v| crate::refcount_core::release(v))
        {
            if terminal {
                self.refcounts.remove_sync(&offset);
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
    /// (`GraceRing::harvest` returns on an empty ring's published length).
    #[inline]
    fn harvest_grace(&self) {
        for offset in self.grace.harvest(crate::free_grace::HARVEST_BATCH) {
            self.publish_free_list(offset);
        }
    }

    /// The PRESSURE harvest: what allocation runs when it is about to
    /// refuse `StorageFull`. Same act, earlier deadline (one honest
    /// acknowledgement cycle instead of the routine bound) — it never
    /// releases an unacknowledged offset without evicting the member
    /// responsible. Returns the number published.
    fn harvest_grace_pressure(&self) -> usize {
        let released = self
            .grace
            .harvest_pressure(crate::free_grace::HARVEST_BATCH);
        let n = released.len();
        for offset in released {
            self.publish_free_list(offset);
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
        Self::plane_gate("block free")?;
        if self.begin_free(offset) {
            self.finish_free(offset);
        }
        Ok(())
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
        Self::plane_gate("specific block allocation")?;
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        if block_idx >= cur_highest {
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
        self.walk_live_layouts(kv, |layout| {
            let (mut owned, mut blobs) = self.layout_owned_blocks(backend_router, layout);
            for idx in owned.drain(..) {
                *census.entry(idx).or_insert(0) += 1;
            }
            indirect.append(&mut blobs);
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
        Self::plane_gate("block-ownership recovery walk")?;
        // Two passes' worth of work in one: collect the owned indices
        // under the walk (which borrows `self` immutably), then seed.
        // `recover_block` is `async`, so it cannot run inside the
        // synchronous visitor.
        let mut indices: Vec<u64> = Vec::new();
        let mut indirect: Vec<String> = Vec::new();
        let summary = self
            .walk_live_layouts(kv, |layout| {
                let (owned, mut blobs) = self.layout_owned_blocks(backend_router, layout);
                indices.extend(owned);
                indirect.append(&mut blobs);
            })
            .await?;
        for key in indirect {
            indices.extend(self.indirect_owned_blocks(backend_router, &key).await);
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

    /// [`walk_live_layouts`] bound to this allocator (the visitor sees only
    /// the layout — the seed and the oracle key on blocks, not inodes).
    async fn walk_live_layouts<F>(
        &self,
        kv: &crate::meta_backend::kv::backend::KvMetaBackend,
        mut visit: F,
    ) -> Result<LayoutWalkSummary>
    where
        F: FnMut(&crate::routing::LayoutMetadata),
    {
        walk_live_layouts(kv, |_ino, layout| visit(layout)).await
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

/// Walk every LIVE inode's decoded `"layout"` xattr on one metadata volume
/// (paged range scans, `nlink == 0` corpses skipped — the reclaim
/// contract), handing each `(ino, layout)` to `visit`.
///
/// The shared engine of three things that must never disagree: the
/// mount-time refcount seed, its durable-vs-derived **oracle**, and the
/// §6.2-item-1 **backfill**. A free function precisely because it touches
/// no allocator state — an allocator-bound copy would invite one of the
/// three to drift.
pub(crate) async fn walk_live_layouts<F>(
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
    mut visit: F,
) -> Result<LayoutWalkSummary>
where
    F: FnMut(u64, &crate::routing::LayoutMetadata),
{
    use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
    let inodes = kv.trees()[0];
    let mut summary = LayoutWalkSummary::default();
    let mut cursor: Vec<u8> = inode_key(1).to_vec();
    let end = inode_key(u64::MAX - 1);
    loop {
        let page = inodes.range(&cursor, &end, 512).await.map_err(|e| {
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
            let Ok(val) = InodeValue::decode(v) else {
                continue;
            };
            summary.checked += 1;
            if val.nlink == 0 {
                continue;
            }
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
