//! Root-ledger records (PR K3) **and the per-volume checkpoint/writeback
//! task (PR K6b)** — design §4.1 "Root ledger", §4.6.
//!
//! ## The K6b checkpoint task (§4.6)
//!
//! One background task per mounted v3 volume, riding the **existing
//! flusher cadence** (`SQUEEZEFS_META_FLUSH_INTERVAL_MS`; the v2 deferred
//! flusher's knob, reused as-is per §5.1). Each tick it:
//!
//! 1. drains the trees' maintenance queues (threshold writebacks +
//!    SMOs — all structure modifications run here, serialized on this
//!    task's `SmoContext`, §4.6);
//! 2. issues the deferred-mode flush barrier when a commit flagged one
//!    (the v2 `needs_flush` discipline, via the volume's `SyncCoalescer`);
//! 3. runs a **checkpoint cycle** when due — every ≤ 1 s, or journal
//!    distance > ring/2, or dirty-node count >
//!    `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (default derived:
//!    `max(4096, budget/32 ÷ node_size)` — the §3 mount-replay bound,
//!    budget-scaled since the 2026-08-04 derivation sweep), or on
//!    shutdown.
//!
//! ## One checkpoint cycle (§4.6 pt 2, pinned order)
//!
//! `H = head` → flush every dirty node once (freeze under the node lock,
//! take its `oldest_dirty_seq` floor, append **outside** the lock; full
//! logs compact/split through the SMO path) → write dirty bitmap pages
//! (generation = this checkpoint's seq) → **barrier** (everything above +
//! every completed journal write + any previously-written ledger record
//! is now durable — the §4.6 pt 3 pending-reclaim drains here) → compute
//! **`tail = min(H, min in-flight reservation start, min dirty floor)`**
//! → write the ledger slot naming the synced roots + that tail → the
//! record's own durability rides the next barrier (or an immediate one
//! under ring pressure / shutdown), after which `reusable_upto` advances
//! to its tail and admission parkers wake.
//!
//! Why the three-way tail min is exact: a reservation `r < H` was taken
//! inside its node-lock window, so the flush pass (which takes the same
//! write locks) either blocked until its records were applied — flushing
//! them — or the tx is still in-window and `min_inflight_start ≤ r` holds
//! the tail back; records applied after the pass visited their node carry
//! reservations ≥ H by the same argument, and their floors re-lower the
//! min. The tail is always an entry boundary (H is the next reservation
//! start; inflight starts are entry starts; and floors round DOWN to
//! entry starts — FIND-SMO-TAIL, docs/design-smo-replay-currency.md §1b:
//! record seqs are `entry_start + i` across a multi-leaf tx, so folding
//! raw seqs let a leaf holding only `rec[j>0]` — its sibling flushed or
//! reserve-skip floor-restored — pin the tail STRICTLY INSIDE the entry,
//! which replay's chain walk (parsing AT the tail) dropped with
//! collateral entries to the resync point. Every `apply_locked` now
//! folds the member's entry start; SMO flips already pinned at
//! `res.start`).
//!
//! ## The cadence is never starved by its own threshold drain (finding 49)
//!
//! The cadence tick is the ONLY path to a ring-pressure checkpoint cycle
//! — the only thing that ever advances `reusable_upto` for a committer
//! parked on ring admission (§4.4 pt 5). Two laws keep it live under a
//! storm whose every pass crosses the writeback threshold (so the
//! maintenance wake is never silent): (1) the cadence deadline is read
//! off the CLOCK after every wake — `timeout_at` polls the wake before the
//! sleep, so a wake returning past the deadline IS the deadline; (2) every
//! threshold drain is BOUNDED by one cadence period
//! (`KvTree::run_maintenance_until`; at least one item per pass so it
//! always progresses), leftovers re-arming the wake. Together: the tick
//! runs every ≤ period + one maintenance item's service time. The shipped
//! form (wake-first select + pop-until-empty drain) let a storm whose
//! items were slower than its arrivals hold the drain open for minutes —
//! no checkpoint, no reclaim, a 32 MiB ring full, the parked pass
//! escalating to fail-stop at 3 × `SQUEEZEFS_TIMEOUT` (the `kv_scale`
//! million-entry storm under the all-features gate: the directory
//! inode's 35 k-record same-key Δtime run folded in 44 s through the
//! O(run²) `bset::MergeIter::run_end` rescan — memoized in the same fix —
//! and the drain never emptied; latent on every venue where SMO service
//! time ≥ the threshold-crossing interval). Contracts: `tests/
//! conveyor_two_stage_tests.rs` §3. What the drain bound does NOT bound is
//! one item's own service time (an SMO is serialized here by design) —
//! the reclaim latency floor under a storm is one SMO + one flush pass.
//!
//! Acyclicity (the D-2 / C-2 lanes): the checkpoint task runs on the
//! `sqz-meta` pool and waits on nothing the conveyor holds. Its barrier is
//! `sync_device` → the `uring_fs` PROCESS pool's fdatasync (never the
//! volume's journal lane); its tail inputs are `head`, the dirty floors
//! and `min_inflight_start` — a window's reservation, registered by stage
//! A and released when stage B observes the write outcome, which needs
//! only the device (the C-2 lane reaps its own CQEs; a ring-space park is
//! an async `Notify` wait on the parked committer's task, so it yields the
//! lane executor to stage B rather than holding the thread). The
//! conveyor's one dependency on this task is `advance_reusable_upto`,
//! which nothing here waits behind. Checkpoint → device; conveyor → device
//! + checkpoint: no cycle — so the only way a parked committer is never
//! released is this task not RUNNING its cycle, which is exactly what the
//! two laws above rule out. The ring-capacity half of that argument is
//! model-checked where it is a word protocol (`loom-models`' `journal_core`
//! models: admission past `reusable_upto` refused, released by
//! `advance_reusable_upto`); the cadence half is a liveness property under
//! a real clock — a select arm never reached — which loom's interleaving
//! model cannot express, so the cargo contract is its pin.
//!
//! ## R10: the drain always makes progress
//!
//! The cycle consumes **zero ring bytes** for everything except SMO
//! records, which draw from the checkpoint-task reserve
//! ([`super::journal::checkpoint_reserve_bytes`]) and **never park**: an
//! exhausted reserve surfaces as
//! [`KvError::JournalReserveExhausted`], the cycle skips that node for
//! this pass (restoring its floor — the tail keeps respecting it),
//! finishes, advances `reusable_upto`, and retries next tick with freed
//! budget. No task ever blocks on ring space while holding a node lock;
//! parked user commits hold nothing the drain needs (§4.4 pt 5).
//!
//! The §4.7 pending-free cap is a pressure valve for THRESHOLD SMOs only
//! (design-smo-replay-currency PR 4 clause a): one refused at the
//! FIFO-headroom admission check surfaces as
//! [`KvError::PendingFreeFull`] and the two `run_maintenance` arms force
//! a barriered checkpoint cycle exactly like reserve exhaustion. The
//! **flush pass itself is exempt** (the §4.7 cycle-break, P2 2026-07-26
//! §9): its compactions run with forced retirement — at cap the old
//! extent parks in the allocator's unbounded overflow against the coming
//! tail instead of refusing — because the flush pass's SMOs are
//! precisely what discharge tail-pinning dirty floors, and refusing one
//! (the retired clause-c "skip-and-defer") closed a dependency cycle
//! {parked frees ↔ pinned tail ↔ refused compaction} that no schedule
//! could exit (progress theorem: `KvTree::smo_replace`). Every barriered
//! cycle rides the centralized clause-b progress audit in
//! `checkpoint_cycle` — retirements parked with neither a release nor a
//! tail advance for `PENDING_FREE_FORCE_CYCLES` consecutive barriered
//! cycles fail the volume loud (the genuinely-wedged-tail terminal — a
//! tail pinned by something no flush can discharge, e.g. a stuck
//! in-flight reservation — never an unbounded retry loop, and never the
//! resolvable pinned-floor shape). The audit judges TWO classes
//! (2026-09-11, design §4.7 amended): a cycle whose flush pass deferred
//! a node because the allocator answered `NoSpace` is a SPACE standstill
//! — counted on `heap_full_cycles`, latched loud once on the volume's
//! `heap_full` posture, neither advancing nor resetting the wedge rung,
//! never terminal — because the tail is legitimately pinned by a node
//! the pass could not flush for want of an extent; the commit pass's
//! heap admission (`KvMetaBackend::admit_heap_locked`) promises every
//! projected SMO's extents, so this class is the residual. The FAILED
//! terminal is the WEDGE class alone.
//!
//! ---
//!
//! **PR K3's half** — slot encode/decode, round-robin slot placement,
//! durable write, and newest-valid-wins selection with torn-slot
//! fallback:
//!
//! ## Layout
//!
//! 32 round-robin 4 KiB slots (128 KiB total). A record with checkpoint
//! seq `s` lives in slot `s % 32`; mount picks the newest slot whose
//! checksum verifies. A torn newest slot (a checkpoint racing power loss)
//! therefore falls back to its predecessor — correct by the pending-free
//! rule (§4.7) and its ring twin (§4.6 pt 3): nothing either record
//! references has been overwritten.
//!
//! ## Append partitioning (pre-RC engineering spec §6.2 item 4)
//!
//! `slot = seq % 32` is a **single-checkpointer** placement: two
//! checkpointers overwrite each other's tree state, and newest-valid-wins
//! then picks between two divergent views of one volume. The partitioned
//! form gives every appender a contiguous **slot range** of
//! [`ledger_slots_per_writer`] slots and round-robins inside it
//! ([`ledger_slot_for`]), which preserves the property the fallback rests
//! on *per appender*: a torn newest slot still has ITS OWN predecessor
//! behind it. That is why the appender count is capped at
//! [`super::journal::MAX_APPENDERS`] — 16 appenders is 2 slots each, and
//! one slot each would leave a torn checkpoint with no fallback at all.
//!
//! Attribution rides an optional 4-byte payload suffix
//! ([`AppendPartition`]); a record without it is a **pre-partition**
//! record, byte-identical to today's, and every un-stamped volume writes
//! only those. Selection becomes per-appender
//! ([`read_partitioned_ledger`]) with three loud refusals — a record in a
//! slot range it does not own, a record from a differently-partitioned era,
//! and a non-authority record carrying tree roots (two structural
//! authorities being the §6.2 item 4 hazard itself).
//!
//! Per-appender selection also means the **checkpoint seq space is
//! per-appender**: seqs are only ever compared within one appender's
//! range, and the fields that must stay volume-global (`next_ino`,
//! `node_seq_watermark`, the tree roots) live only on the authority's
//! records. `alloc_bitmap_generation` is per-appender for the same reason
//! the bitmap pages are: A/B resolution is per page, and a page has one
//! owner.
//!
//! ## Slot format (little-endian; §4.1's field list)
//!
//! ```text
//! [0..4)   magic: u32      ROOT_LEDGER_MAGIC
//! [4..8)   len: u32        payload byte length
//! [8..16)  seq: u64        checkpoint sequence (slot = seq % 32)
//! [16..24) xxh3_64: u64    over the slot header (checksum zeroed) + payload
//! payload:
//!   journal_tail_seq: u64
//!   next_ino: u64
//!   alloc_bitmap_generation: u64
//!   n_roots: u16
//!   n_roots × { tree_id: u8, node_addr: u64, node_seq: u64 }
//!   [membership stamp — OPTIONAL suffix; stride-run wire, dynamic meta
//!    routing (docs/design-dynamic-meta-routing.md §5.2), behind incompat
//!    bit 6]:
//!     set_uuid: [u8; 16]
//!     set_epoch: u64
//!     member_position: u16
//!     member_count: u16
//!     routing_width: u32
//!     n_runs: u16              (≤ STAMP_MAX_RUNS)
//!     n_runs × { start: u16, stride: u16, count: u32 }  hosted-slot runs
//!     native_slot_plus1: u32   (0 = no legacy-keyspace slot; u32 —
//!                               a u16 plus1 overflows at slot 65535)
//!     n_cursors: u16           (≤ STAMP_MAX_CURSORS)
//!     n_cursors × { slot: u16, next_local_ino: u64 }  per-slot cursors
//!   [append partition — OPTIONAL suffix; multi-writer partitioned append
//!    (pre-RC engineering spec §6.2 item 4), behind incompat bit 8]:
//!     writer_id: u16
//!     writer_count: u16
//! ```
//!
//! Every length is bounds-checked against its container before use (§9);
//! slots are written as full zero-padded 4 KiB images so a shorter record
//! can never leave stale bytes of a longer predecessor parseable.
//!
//! Format note (forward-only, design-dynamic-meta-routing §5.2): this is
//! the ONE stamp wire this binary reads or writes. The retired dense
//! slot-list encodings (VL5a's `n_slots × u16` and the VL5b optional
//! extension) belonged to frozen-routing-width volumes, which the
//! superblock gate refuses BEFORE any ledger slot is read (bit 6 —
//! [`super::superblock::FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING`] — is
//! presence-required, and every bit-6 format stamps this wire from
//! birth), so the decoder never meets them. Old binaries refuse bit-6
//! volumes at their own superblock gates — no crash prefix exists in
//! which either side silently misparses the other's stamps.

use super::backend::KvMetaBackend;
use super::journal::AppendPartition;
use super::node_cache::CachedNode;
use super::tree::SmoContext;
use super::KvError;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Number of round-robin ledger slots (§4.1).
pub const ROOT_LEDGER_SLOTS: u64 = 32;
/// Slot size in bytes.
pub const ROOT_LEDGER_SLOT_LEN: u64 = 4096;
/// Total ledger extent length (128 KiB — the §3 mount-read unit).
pub const ROOT_LEDGER_LEN: u64 = ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN;
/// Ledger slot magic (`"KVRL"`).
pub const ROOT_LEDGER_MAGIC: u32 = 0x4B56_524C;
/// Fixed slot header length (`magic | len | seq | xxh3`).
pub const ROOT_LEDGER_HDR_LEN: usize = 24;

/// Fixed payload prefix: `journal_tail_seq | next_ino |
/// alloc_bitmap_generation | node_seq_watermark | n_roots`.
///
/// Format note (forward-only): the watermark widened this prefix by 8
/// bytes; pre-watermark slots fail the `n_roots` length-consistency
/// check and decode as absent. The superblock incompat bit
/// [`super::superblock::FEATURE_INCOMPAT_NODE_SEQ_WATERMARK`] refuses
/// pre-watermark volumes loud (reformat required) before any slot is
/// read, per the standing no-backwards-compatibility directive.
const PAYLOAD_FIXED_LEN: usize = 8 + 8 + 8 + 8 + 2;
/// Encoded size of one tree root: `tree_id | node_addr | node_seq`.
const ROOT_ENC_LEN: usize = 1 + 8 + 8;

/// The stamp encoding budget's RUN cap (design-dynamic-meta-routing
/// §5.3 — what replaced the retired 64-hosted-slot cap): a volume may
/// host tens of thousands of slots as ONE stride run; what the 4096-B
/// ledger slot bounds is encoding COMPLEXITY. Worst-case record:
/// 24 (header) + 34 (fixed prefix) + 5 roots × 17 (85) +
/// stamp (34 + 128 runs × 8 + 4 + 256 cursors × 10) = 3 765 < 4 096,
/// with 19 spare roots of headroom. Enforced loud here at encode and by
/// the migration/add-meta preflights (a refusal names the consolidation
/// remedy before any copy starts).
pub const STAMP_MAX_RUNS: usize = 128;
/// The stamp encoding budget's CURSOR cap (same budget equation):
/// cursors exist only for ever-minted guest slots — `MINT_SPREAD` (64)
/// fresh mint cursors consume ¼ of it, leaving ¾ for cursors travelling
/// in with migrated slots.
pub const STAMP_MAX_CURSORS: usize = 256;
/// Fixed stamp prefix: `set_uuid | set_epoch | member_position |
/// member_count | routing_width | n_runs`.
const STAMP_FIXED_LEN: usize = 16 + 8 + 2 + 2 + 4 + 2;
/// Encoded size of one hosted-slot run: `start | stride | count`.
const RUN_ENC_LEN: usize = 2 + 2 + 4;

/// The per-volume set-membership stamp (PR VL5a, design-volume-lifecycle
/// §5.5.1a): rides the root-ledger record payload — the one structure
/// that is already A/B-rotating, checksummed, and rewritable without
/// relocation on a densely-packed v3 volume. Solves the slot-0 bootstrap
/// chicken-and-egg (mount reads stamps off raw superblock+ledger reads
/// before any tree routing) and makes volume ORDER durable:
/// `member_position` defines the canonical order for
/// `volume_set_generation`, not the operator's URI ordering. The stamp
/// is authoritative; the `FormatConfig` mirror (`meta_routing_width` /
/// `meta_slot_map` / `meta_volumes`) is human-readable only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipStamp {
    /// The SET identity (minted once per `squeezefs format` invocation;
    /// distinct from the per-volume superblock uuid).
    pub set_uuid: [u8; 16],
    /// Membership epoch (§5.5.2b protocol; format mints 1). VL5a refuses
    /// mixed epochs loud — the highest-complete-epoch resolution is
    /// VL5b's.
    pub set_epoch: u64,
    /// This volume's position in the canonical set order (assigned at
    /// format = format order; preserved across add/remove).
    pub member_position: u16,
    /// Members in this epoch's set (mount cross-checks completeness).
    pub member_count: u16,
    /// The DERIVED routing width W (design-dynamic-meta-routing §5.1;
    /// `crate::meta_backend::DERIVED_ROUTING_WIDTH` at format):
    /// `route_ino`/`make_global_ino` modulo, eternally stable across set
    /// changes. Stored — never re-derived at mount — so a future
    /// derivation change can never re-route an existing set.
    pub routing_width: u32,
    /// The slots this volume hosts, as stride runs (≤
    /// [`STAMP_MAX_RUNS`] of them — the §5.3 encoding budget).
    pub slots_hosted: super::slot_set::SlotSet,
    /// The slot whose records live in this volume's LEGACY
    /// (un-namespaced) keyspace — the volume's format-time native slot.
    /// `None` on fresh `add-meta` members (all their slots are guests).
    pub native_slot: Option<u16>,
    /// Per-slot GUEST ino-allocation cursors — `(slot, next_local_ino)`
    /// for every hosted slot whose guest keyspace has minted (the
    /// MINT_SPREAD rotation's durable half) or whose cursor travelled in
    /// with a migrated slot. ≤ [`STAMP_MAX_CURSORS`].
    pub slot_cursors: Vec<(u16, u64)>,
}

impl MembershipStamp {
    /// The slot whose records live in this volume's LEGACY keyspace.
    /// (The pre-dynamic-routing derivation — "unextended stamps default
    /// to `member_position`" — died with the dense wire; the field is
    /// always explicit now.)
    pub fn resolved_native_slot(&self) -> Option<u16> {
        self.native_slot
    }

    /// The travelling cursor recorded for `slot`, if any.
    pub fn cursor_for(&self, slot: u16) -> Option<u64> {
        self.slot_cursors
            .iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, n)| *n)
    }

    fn encoded_len(&self) -> usize {
        STAMP_FIXED_LEN
            + self.slots_hosted.runs().len() * RUN_ENC_LEN
            + 4 // native_slot_plus1 (u32)
            + 2 // n_cursors
            + self.slot_cursors.len() * 10
    }
}

/// One tree root named by a ledger record: `(tree_id, node_addr, node_seq)`
/// (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRoot {
    pub tree_id: u8,
    pub node_addr: u64,
    pub node_seq: u64,
}

/// A root-ledger record (§4.1): the per-tree roots plus the journal tail,
/// the monotonic ino watermark (§4.8), and the allocator-bitmap generation
/// (§4.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRecord {
    /// Checkpoint sequence; strictly monotonic per volume. Slot = `seq % 32`.
    pub seq: u64,
    /// Per-tree roots.
    pub tree_roots: Vec<TreeRoot>,
    /// Replay starts here (§4.6 pt 2 tail rule — computed by K6b).
    pub journal_tail_seq: u64,
    /// Monotonic ino allocation watermark (§4.8).
    pub next_ino: u64,
    /// Allocator bitmap generation (§4.7 — consumed by K4).
    pub alloc_bitmap_generation: u64,
    /// Node-seq mint watermark: the per-volume mint counter at record
    /// build time. A mount reseeds the counter **at or above** this
    /// value, so node incarnation seqs never repeat within a generation
    /// — the invariant the §4.5 `node_seq_at_write == node_seq` frame
    /// admission relies on. Before this field existed, a clean shutdown
    /// (empty replay window) collapsed the reseed floor to the ROOT
    /// seqs and re-minted every non-root seq; recycled extents still
    /// holding frames stamped with a re-minted seq then chained the
    /// previous incarnation's checksummed records into the new node
    /// (the 2026-07-13 release-gate Finding A).
    pub node_seq_watermark: u64,
    /// The §5.5.1a set-membership stamp (PR VL5a): `Some` on every
    /// ledger record of a slot-mapped volume (written under the
    /// bit-before-first-stamp invariant), `None` forever on legacy
    /// volumes — whose slots stay byte-identical to the pre-VL5a
    /// encoding.
    pub membership_stamp: Option<MembershipStamp>,
    /// Which appender wrote this record, and how many the volume's
    /// structures are partitioned for (spec §6.2 item 4). `None` on every
    /// un-stamped volume — such a record encodes byte-identically to the
    /// pre-partitioning image and places itself with `slot = seq % 32`
    /// (ruling D9: built, not stamped).
    pub append_partition: Option<AppendPartition>,
}

/// xxh3 over a slot image with the checksum field (bytes 16..24) zeroed —
/// the header-then-payload discipline the superblock and bsets use (§4.3).
fn slot_checksum(image: &[u8], payload_len: usize) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&image[..16]);
    h.update(&[0u8; 8]);
    h.update(&image[ROOT_LEDGER_HDR_LEN..ROOT_LEDGER_HDR_LEN + payload_len]);
    h.digest()
}

/// Encoded size of the append-partition suffix (`writer_id ‖
/// writer_count`) — 4 bytes against the §5.3 encoding budget's 331 spare
/// (the worst-case stamped record is 3,765 of 4,096).
const APPEND_PARTITION_ENC_LEN: usize = 4;

/// Ledger slots each appender owns under a `writers`-way partition. The
/// floor of 2 is normative, not cosmetic: newest-valid-wins with a torn
/// newest slot must fall back to a predecessor **of the same appender**
/// (§4.1), which one slot cannot provide. [`super::journal::MAX_APPENDERS`]
/// is derived from exactly this.
pub fn ledger_slots_per_writer(writers: u16) -> u64 {
    ROOT_LEDGER_SLOTS / u64::from(writers)
}

/// The slot a record with checkpoint seq `seq` occupies under `part`:
/// solo is `seq % 32` verbatim; a partitioned appender round-robins inside
/// its own contiguous range (spec §6.2 item 4).
pub fn ledger_slot_for(seq: u64, part: AppendPartition) -> u64 {
    if part.is_solo() {
        return seq % ROOT_LEDGER_SLOTS;
    }
    let per = ledger_slots_per_writer(part.writers());
    u64::from(part.writer_id()) * per + seq % per
}

impl LedgerRecord {
    /// The slot index this record occupies: `seq % 32` for an
    /// un-partitioned record (every un-stamped volume), its appender's
    /// round-robin range otherwise ([`ledger_slot_for`]).
    pub fn slot_index(&self) -> u64 {
        match self.append_partition {
            None => self.seq % ROOT_LEDGER_SLOTS,
            Some(part) => ledger_slot_for(self.seq, part),
        }
    }

    /// Encode into a full zero-padded 4 KiB slot image. Errors when the
    /// record cannot fit a slot — the encoding-budget checks are
    /// load-bearing here: a stamp past [`STAMP_MAX_RUNS`] hosted-slot
    /// runs or [`STAMP_MAX_CURSORS`] cursors refuses LOUD (the caps
    /// exist precisely so the worst-case record fits), and the
    /// total-size check stays as defense in depth for the root count.
    pub fn encode_slot(&self) -> Result<Vec<u8>, KvError> {
        if let Some(stamp) = &self.membership_stamp {
            if stamp.slots_hosted.runs().len() > STAMP_MAX_RUNS {
                return Err(KvError::Corrupt(format!(
                    "membership stamp needs {} hosted-slot runs — at most {STAMP_MAX_RUNS} \
                     (the design-dynamic-meta-routing §5.3 encoding budget; consolidate \
                     the volume's hosted slots via stride-preserving migration)",
                    stamp.slots_hosted.runs().len()
                )));
            }
            if stamp.slot_cursors.len() > STAMP_MAX_CURSORS {
                return Err(KvError::Corrupt(format!(
                    "membership stamp carries {} slot cursors — at most \
                     {STAMP_MAX_CURSORS} (the same §5.3 encoding budget; migrate \
                     cursor-bearing slots off this volume)",
                    stamp.slot_cursors.len()
                )));
            }
        }
        let stamp_len = self
            .membership_stamp
            .as_ref()
            .map_or(0, MembershipStamp::encoded_len);
        let part_len = if self.append_partition.is_some() {
            APPEND_PARTITION_ENC_LEN
        } else {
            0
        };
        let payload_len =
            PAYLOAD_FIXED_LEN + self.tree_roots.len() * ROOT_ENC_LEN + stamp_len + part_len;
        if ROOT_LEDGER_HDR_LEN + payload_len > ROOT_LEDGER_SLOT_LEN as usize
            || self.tree_roots.len() > usize::from(u16::MAX)
        {
            return Err(KvError::Corrupt(format!(
                "ledger record with {} tree roots does not fit a {}-byte slot",
                self.tree_roots.len(),
                ROOT_LEDGER_SLOT_LEN
            )));
        }
        let mut image = vec![0u8; ROOT_LEDGER_SLOT_LEN as usize];
        image[0..4].copy_from_slice(&ROOT_LEDGER_MAGIC.to_le_bytes());
        image[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
        image[8..16].copy_from_slice(&self.seq.to_le_bytes());
        // Checksum stamped below.
        let mut pos = ROOT_LEDGER_HDR_LEN;
        image[pos..pos + 8].copy_from_slice(&self.journal_tail_seq.to_le_bytes());
        pos += 8;
        image[pos..pos + 8].copy_from_slice(&self.next_ino.to_le_bytes());
        pos += 8;
        image[pos..pos + 8].copy_from_slice(&self.alloc_bitmap_generation.to_le_bytes());
        pos += 8;
        image[pos..pos + 8].copy_from_slice(&self.node_seq_watermark.to_le_bytes());
        pos += 8;
        image[pos..pos + 2].copy_from_slice(&(self.tree_roots.len() as u16).to_le_bytes());
        pos += 2;
        for root in &self.tree_roots {
            image[pos] = root.tree_id;
            image[pos + 1..pos + 9].copy_from_slice(&root.node_addr.to_le_bytes());
            image[pos + 9..pos + 17].copy_from_slice(&root.node_seq.to_le_bytes());
            pos += ROOT_ENC_LEN;
        }
        if let Some(stamp) = &self.membership_stamp {
            image[pos..pos + 16].copy_from_slice(&stamp.set_uuid);
            pos += 16;
            image[pos..pos + 8].copy_from_slice(&stamp.set_epoch.to_le_bytes());
            pos += 8;
            image[pos..pos + 2].copy_from_slice(&stamp.member_position.to_le_bytes());
            pos += 2;
            image[pos..pos + 2].copy_from_slice(&stamp.member_count.to_le_bytes());
            pos += 2;
            image[pos..pos + 4].copy_from_slice(&stamp.routing_width.to_le_bytes());
            pos += 4;
            let runs = stamp.slots_hosted.runs();
            image[pos..pos + 2].copy_from_slice(&(runs.len() as u16).to_le_bytes());
            pos += 2;
            for run in runs {
                image[pos..pos + 2].copy_from_slice(&run.start.to_le_bytes());
                pos += 2;
                image[pos..pos + 2].copy_from_slice(&run.stride.to_le_bytes());
                pos += 2;
                image[pos..pos + 4].copy_from_slice(&run.count.to_le_bytes());
                pos += 4;
            }
            // u32 on the wire: a u16 `plus1` would overflow at slot
            // 65535, which the derived width makes reachable.
            let native_plus1 = stamp.native_slot.map_or(0u32, |s| u32::from(s) + 1);
            image[pos..pos + 4].copy_from_slice(&native_plus1.to_le_bytes());
            pos += 4;
            image[pos..pos + 2].copy_from_slice(&(stamp.slot_cursors.len() as u16).to_le_bytes());
            pos += 2;
            for (slot, next) in &stamp.slot_cursors {
                image[pos..pos + 2].copy_from_slice(&slot.to_le_bytes());
                pos += 2;
                image[pos..pos + 8].copy_from_slice(&next.to_le_bytes());
                pos += 8;
            }
        }
        // The append-partition suffix rides AFTER the membership stamp:
        // the stamp's own length equation closes exactly, so the decoder
        // can tell "no suffix" (payload ends) from "one suffix" (exactly
        // four more bytes) without ambiguity, and an un-partitioned record
        // adds nothing at all (ruling D9).
        if let Some(part) = self.append_partition {
            image[pos..pos + 2].copy_from_slice(&part.writer_id().to_le_bytes());
            image[pos + 2..pos + 4].copy_from_slice(&part.writers().to_le_bytes());
        }
        let sum = slot_checksum(&image, payload_len);
        image[16..24].copy_from_slice(&sum.to_le_bytes());
        Ok(image)
    }

    /// Decode + verify one slot image: magic, bounds-checked lengths (§9),
    /// checksum. Any failure means "this slot holds no valid record" —
    /// the selection logic treats it as absent, never loud.
    pub fn decode_slot(buf: &[u8]) -> Result<Self, KvError> {
        if buf.len() < ROOT_LEDGER_HDR_LEN {
            return Err(KvError::Corrupt(format!(
                "truncated ledger slot: {} of {ROOT_LEDGER_HDR_LEN} header bytes",
                buf.len()
            )));
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != ROOT_LEDGER_MAGIC {
            return Err(KvError::Corrupt(format!(
                "bad ledger slot magic {magic:#010x} (expected {ROOT_LEDGER_MAGIC:#010x})"
            )));
        }
        let payload_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        // §9: the length is validated against the container before any
        // byte it governs is touched.
        if payload_len < PAYLOAD_FIXED_LEN || ROOT_LEDGER_HDR_LEN + payload_len > buf.len() {
            return Err(KvError::Corrupt(format!(
                "ledger payload length {payload_len} does not fit its slot"
            )));
        }
        let seq = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let stored = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let computed = slot_checksum(buf, payload_len);
        if stored != computed {
            return Err(KvError::ChecksumMismatch { stored, computed });
        }
        let payload = &buf[ROOT_LEDGER_HDR_LEN..ROOT_LEDGER_HDR_LEN + payload_len];
        let journal_tail_seq = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let next_ino = u64::from_le_bytes(payload[8..16].try_into().unwrap());
        let alloc_bitmap_generation = u64::from_le_bytes(payload[16..24].try_into().unwrap());
        let node_seq_watermark = u64::from_le_bytes(payload[24..32].try_into().unwrap());
        let n_roots = usize::from(u16::from_le_bytes(payload[32..34].try_into().unwrap()));
        // §9: the roots region must fit BEFORE it is walked; what follows
        // it is one of exactly four shapes — nothing (the historical
        // pre-VL5a encoding), one §5.5.1a membership stamp, one 4-byte
        // append-partition suffix, or a stamp followed by that suffix.
        // The shapes are unambiguous by length: the fixed stamp prefix is
        // 34 bytes, so a 4-byte tail can only be a partition suffix, and a
        // stamp's own length equation must close to within 0 or exactly 4
        // bytes of the payload end. (The pre-VL5a decoder required exact
        // equality — which is why stamped slots decode as absent to old
        // binaries; see the module-docs format note. Partitioned records
        // ride incompat bit 8, so the same one-way gate applies to them.)
        let roots_end = PAYLOAD_FIXED_LEN + n_roots * ROOT_ENC_LEN;
        if roots_end > payload_len {
            return Err(KvError::Corrupt(format!(
                "ledger n_roots {n_roots} inconsistent with payload length {payload_len}"
            )));
        }
        let mut tree_roots = Vec::with_capacity(n_roots);
        let mut pos = PAYLOAD_FIXED_LEN;
        for _ in 0..n_roots {
            tree_roots.push(TreeRoot {
                tree_id: payload[pos],
                node_addr: u64::from_le_bytes(payload[pos + 1..pos + 9].try_into().unwrap()),
                node_seq: u64::from_le_bytes(payload[pos + 9..pos + 17].try_into().unwrap()),
            });
            pos += ROOT_ENC_LEN;
        }
        let mut append_partition: Option<AppendPartition> = None;
        let membership_stamp = if pos == payload_len {
            None
        } else if payload_len - pos == APPEND_PARTITION_ENC_LEN {
            // An append-partition suffix with no membership stamp.
            append_partition = Some(decode_append_partition(&payload[pos..])?);
            None
        } else {
            if pos + STAMP_FIXED_LEN > payload_len {
                return Err(KvError::Corrupt(format!(
                    "ledger payload tail of {} bytes is neither empty nor a membership \
                     stamp (fixed stamp prefix is {STAMP_FIXED_LEN} bytes)",
                    payload_len - pos
                )));
            }
            let set_uuid: [u8; 16] = payload[pos..pos + 16].try_into().unwrap();
            pos += 16;
            let set_epoch = u64::from_le_bytes(payload[pos..pos + 8].try_into().unwrap());
            pos += 8;
            let member_position = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap());
            pos += 2;
            let member_count = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap());
            pos += 2;
            let routing_width = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let n_runs = usize::from(u16::from_le_bytes(
                payload[pos..pos + 2].try_into().unwrap(),
            ));
            pos += 2;
            if n_runs > STAMP_MAX_RUNS {
                return Err(KvError::Corrupt(format!(
                    "membership stamp claims {n_runs} hosted-slot runs (cap \
                     {STAMP_MAX_RUNS})"
                )));
            }
            if pos + n_runs * RUN_ENC_LEN > payload_len {
                return Err(KvError::Corrupt(format!(
                    "membership stamp n_runs {n_runs} inconsistent with payload \
                     length {payload_len}"
                )));
            }
            let mut runs = Vec::with_capacity(n_runs);
            for _ in 0..n_runs {
                runs.push(super::slot_set::SlotRun {
                    start: u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap()),
                    stride: u16::from_le_bytes(payload[pos + 2..pos + 4].try_into().unwrap()),
                    count: u32::from_le_bytes(payload[pos + 4..pos + 8].try_into().unwrap()),
                });
                pos += RUN_ENC_LEN;
            }
            // Validate-before-trust (§9): malformed run geometry —
            // overlaps, zero counts, namespace escapes — refuses loud
            // here, never reaches routing.
            let slots_hosted = super::slot_set::SlotSet::from_runs(runs)?;
            if pos + 4 + 2 > payload_len {
                return Err(KvError::Corrupt(format!(
                    "membership stamp tail of {} bytes truncates the native/cursor \
                     suffix",
                    payload_len - pos
                )));
            }
            let native_plus1 = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let n_cursors = usize::from(u16::from_le_bytes(
                payload[pos..pos + 2].try_into().unwrap(),
            ));
            pos += 2;
            if n_cursors > STAMP_MAX_CURSORS {
                return Err(KvError::Corrupt(format!(
                    "membership stamp claims {n_cursors} slot cursors (cap \
                     {STAMP_MAX_CURSORS})"
                )));
            }
            let cursors_end = pos + n_cursors * 10;
            if cursors_end != payload_len && cursors_end + APPEND_PARTITION_ENC_LEN != payload_len {
                return Err(KvError::Corrupt(format!(
                    "membership stamp n_cursors {n_cursors} inconsistent with payload \
                     length {payload_len} (the only legal tail past the cursors is one \
                     {APPEND_PARTITION_ENC_LEN}-byte append-partition suffix)"
                )));
            }
            let mut cursors = Vec::with_capacity(n_cursors);
            for _ in 0..n_cursors {
                let slot = u16::from_le_bytes(payload[pos..pos + 2].try_into().unwrap());
                pos += 2;
                let next = u64::from_le_bytes(payload[pos..pos + 8].try_into().unwrap());
                pos += 8;
                cursors.push((slot, next));
            }
            let native_slot = match native_plus1 {
                0 => None,
                p1 => Some(u16::try_from(p1 - 1).map_err(|_| {
                    KvError::Corrupt(format!(
                        "membership stamp native slot {} escapes the u16 slot-id namespace",
                        p1 - 1
                    ))
                })?),
            };
            if pos < payload_len {
                append_partition = Some(decode_append_partition(&payload[pos..])?);
            }
            let slot_cursors = cursors;
            Some(MembershipStamp {
                set_uuid,
                set_epoch,
                member_position,
                member_count,
                routing_width,
                slots_hosted,
                native_slot,
                slot_cursors,
            })
        };
        Ok(Self {
            seq,
            tree_roots,
            journal_tail_seq,
            next_ino,
            alloc_bitmap_generation,
            node_seq_watermark,
            membership_stamp,
            append_partition,
        })
    }
}

/// Decode the 4-byte append-partition suffix, validating it through
/// [`AppendPartition::new`] (an illegal appender count or id is structural
/// corruption — the checksum already verified, so this is a writer-bug
/// screen, and it must never reach placement arithmetic).
fn decode_append_partition(tail: &[u8]) -> Result<AppendPartition, KvError> {
    debug_assert!(tail.len() >= APPEND_PARTITION_ENC_LEN);
    let writer_id = u16::from_le_bytes(tail[0..2].try_into().unwrap());
    let writers = u16::from_le_bytes(tail[2..4].try_into().unwrap());
    AppendPartition::new(writers, writer_id).map_err(|e| {
        KvError::Corrupt(format!(
            "ledger record carries an illegal append partition: {e}"
        ))
    })
}

/// Write `rec` to its round-robin slot at `ledger_base` in `path` via
/// `uring_fs` (one full-slot `write_at`). Durability rides the caller's
/// barrier (§4.6 pt 2: "the root record's own durability rides the next
/// barrier").
pub async fn write_ledger_slot(
    path: &Path,
    ledger_base: u64,
    rec: &LedgerRecord,
) -> Result<(), KvError> {
    let image = rec.encode_slot()?;
    let off = ledger_base + rec.slot_index() * ROOT_LEDGER_SLOT_LEN;
    crate::uring_fs::write_at(path, off, image).await?;
    Ok(())
}

/// Read all 32 slots (one 128 KiB `uring_fs::read_at`) and return the
/// newest-seq record whose checksum verifies, or `None` when no slot holds
/// a valid record (fresh volume — the all-slots-invalid *loud* policy on a
/// non-fresh volume is mount wiring, PR K6a). A torn newest slot simply
/// loses the race to its intact predecessor (§4.1). `Err` is real device
/// I/O failure only ([`KvError::Io`]) — ledger *contents* never fail loud
/// here.
pub async fn read_newest_ledger(
    path: &Path,
    ledger_base: u64,
) -> Result<Option<LedgerRecord>, KvError> {
    let got = crate::uring_fs::read_at(path, ledger_base, ROOT_LEDGER_LEN as usize).await?;
    let mut newest: Option<LedgerRecord> = None;
    for slot in 0..ROOT_LEDGER_SLOTS as usize {
        let start = slot * ROOT_LEDGER_SLOT_LEN as usize;
        if start >= got.len() {
            break; // short extent: the rest reads as absent
        }
        let end = (start + ROOT_LEDGER_SLOT_LEN as usize).min(got.len());
        if let Ok(rec) = LedgerRecord::decode_slot(&got[start..end]) {
            if newest.as_ref().is_none_or(|n| rec.seq > n.seq) {
                newest = Some(rec);
            }
        }
    }
    Ok(newest)
}

/// Every appender's newest valid ledger record on a partitioned volume
/// (spec §6.2 item 4) — the multi-appender face of
/// [`read_newest_ledger`].
#[derive(Debug)]
pub struct PartitionedLedger {
    /// Appenders the volume's structures are partitioned for.
    pub writers: u16,
    /// Newest valid record per appender, indexed by writer id (`None` =
    /// that appender has never checkpointed).
    pub per_writer: Vec<Option<LedgerRecord>>,
    /// Each appender's durable journal tail — index = writer id, absent
    /// records contributing 0 (the conservative seed: replay that ring
    /// from its start; the fold is idempotent).
    pub tails: Vec<u64>,
    /// Pre-partition (suffix-less) records found — the Phase-8 transition
    /// signal. Such a record was written under the solo placement law and
    /// belongs to the root authority wherever it sits.
    pub pre_partition_records: u64,
}

impl PartitionedLedger {
    /// The root-authority appender's newest record: tree roots, `next_ino`,
    /// and `node_seq_watermark` come from here and nowhere else.
    pub fn authority(&self) -> Option<&LedgerRecord> {
        self.per_writer
            .get(usize::from(super::journal::ROOT_AUTHORITY_WRITER))
            .and_then(|r| r.as_ref())
    }
}

/// Read all 32 slots and resolve **per appender**: newest valid record in
/// each appender's own slot range (spec §6.2 item 4). `writers` is the
/// mount's appender count.
///
/// Three classes refuse the mount LOUD, because each one means the
/// partition is not what this mount believes:
///
/// 1. a partitioned record sitting in a slot range it does not own (a
///    misdirected write, or an appender using the wrong placement);
/// 2. a record whose `writer_count` disagrees with `writers` (a
///    differently-partitioned era — its slot arithmetic is not this
///    mount's);
/// 3. a **non-authority** record carrying tree roots — two structural
///    authorities is §6.2 item 4's hazard itself, so it is refused at the
///    format level rather than resolved by guessing.
///
/// Pre-partition records (no suffix) are the Phase-8 transition and are
/// accepted as the authority's **wherever they sit**: they were placed by
/// `slot = seq % 32`, and ignoring a newer valid record would fall back an
/// unbounded distance — only the immediately-preceding record's replay
/// window is protected (§4.6 pt 3).
///
/// `Err` on I/O is real device failure; slot *contents* that merely fail
/// to verify read as absent, exactly like [`read_newest_ledger`].
pub async fn read_partitioned_ledger(
    path: &Path,
    ledger_base: u64,
    writers: u16,
) -> Result<PartitionedLedger, KvError> {
    let per = ledger_slots_per_writer(writers);
    let got = crate::uring_fs::read_at(path, ledger_base, ROOT_LEDGER_LEN as usize).await?;
    let mut per_writer: Vec<Option<LedgerRecord>> = vec![None; usize::from(writers)];
    let mut pre_partition_records = 0u64;
    for slot in 0..ROOT_LEDGER_SLOTS as usize {
        let start = slot * ROOT_LEDGER_SLOT_LEN as usize;
        if start >= got.len() {
            break; // short extent: the rest reads as absent
        }
        let end = (start + ROOT_LEDGER_SLOT_LEN as usize).min(got.len());
        let Ok(rec) = LedgerRecord::decode_slot(&got[start..end]) else {
            continue; // unverifiable slot: absent, never loud
        };
        let writer = match rec.append_partition {
            None => {
                // A pre-partition record: the authority's, by the solo
                // placement law it was written under.
                pre_partition_records += 1;
                super::journal::ROOT_AUTHORITY_WRITER
            }
            Some(part) => {
                if part.writers() != writers {
                    return Err(KvError::Corrupt(format!(
                        "root-ledger slot {slot} carries writer_count {} but this mount is \
                         partitioned for {writers} appenders — the volume was last written \
                         by a differently-partitioned era, whose slot arithmetic is not \
                         this one's (spec §6.2 item 4)",
                        part.writers()
                    )));
                }
                let lo = u64::from(part.writer_id()) * per;
                if !(lo..lo + per).contains(&(slot as u64)) {
                    return Err(KvError::Corrupt(format!(
                        "root-ledger slot {slot} holds a record from writer {} whose slot \
                         range is [{lo}, {}) — a misdirected write or an appender using \
                         foreign placement (spec §6.2 item 4)",
                        part.writer_id(),
                        lo + per
                    )));
                }
                part.writer_id()
            }
        };
        if writer != super::journal::ROOT_AUTHORITY_WRITER && !rec.tree_roots.is_empty() {
            return Err(KvError::Corrupt(format!(
                "root-ledger slot {slot}: writer {writer} published {} tree roots, but only \
                 writer {} owns the volume's structural state — two checkpointers naming \
                 roots is exactly the overwrite this partitioning prevents (spec §6.2 \
                 item 4)",
                rec.tree_roots.len(),
                super::journal::ROOT_AUTHORITY_WRITER
            )));
        }
        let cell = &mut per_writer[usize::from(writer)];
        if cell.as_ref().is_none_or(|cur| rec.seq > cur.seq) {
            *cell = Some(rec);
        }
    }
    let tails = per_writer
        .iter()
        .map(|r| r.as_ref().map_or(0, |r| r.journal_tail_seq))
        .collect();
    Ok(PartitionedLedger {
        writers,
        per_writer,
        tails,
        pre_partition_records,
    })
}

// ---------------------------------------------------------------------------
// PR K6b: the checkpoint/writeback task (module docs above).
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (§5.1; the §3
/// mount-replay working-set bound). Since the 2026-08-04 derivation
/// sweep the default derives from the machine
/// ([`resolve_max_dirty_nodes`]); the env stays absolute-verbatim.
/// Resolved ONCE at `open` into `KvMetaBackend::dirty_node_cap` (the
/// backend-knob convention) — never re-derived on the cadence tick.
pub const CHECKPOINT_MAX_DIRTY_NODES_ENV: &str = "SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES";

/// The shipped dirty-node cap — the derived default's FLOOR since the
/// 2026-08-04 derivation sweep (never-regress-below-shipped: a lower
/// cap checkpoints more often than any box ever shipped — pure
/// overhead, no RAM won).
pub const CHECKPOINT_MAX_DIRTY_NODES_FLOOR: u64 = 4096;

/// Dirty-node cap resolution, pure (2026-08-04 derivation sweep): env
/// wins verbatim; derived default = `max(4096, budget/32 ÷ node_size)`
/// — the cap bounds RAM pinned by dirty nodes AND the mount-replay
/// working set, so it scales with the resolved R5 budget instead of a
/// 512 MiB-era constant (a zero `node_size` is defensive-floored).
pub fn resolve_max_dirty_nodes(budget_bytes: u64, node_size: u64, env: Option<&str>) -> u64 {
    if let Some(raw) = env {
        match raw.trim().parse::<u64>() {
            Ok(n) => return n,
            Err(e) => log::warn!(
                "{CHECKPOINT_MAX_DIRTY_NODES_ENV}={raw:?} is not an integer ({e}) — ignored"
            ),
        }
    }
    if node_size == 0 {
        return CHECKPOINT_MAX_DIRTY_NODES_FLOOR;
    }
    (budget_bytes / 32 / node_size).max(CHECKPOINT_MAX_DIRTY_NODES_FLOOR)
}

/// Checkpoint cadence ceiling: a cycle runs at least this often even when
/// no other trigger fires (§4.6 pt 2 "every ≤ 1 s"). Public because it is
/// also the **reader's** guarantee: a coherent reader cannot see records
/// faster than the writer mints them, so the §6.8 item-2 poll cadence
/// derives from this ceiling (`super::revalidate`).
pub const CHECKPOINT_MAX_AGE_MS: u128 = 1000;

/// The checkpoint task's tick period for a flush interval, ms: the knob
/// verbatim, strict mode (`0`) reading as the task's own 100 ms tick. The
/// ONE derivation the checkpoint task's spawn and the reader's poll cadence
/// (`super::revalidate::resolve_revalidate_interval_ms`) both ride, so the
/// two cannot drift.
pub fn checkpoint_tick_period_ms(flush_interval_ms: u64) -> u64 {
    match flush_interval_ms {
        0 => 100,
        ms => ms,
    }
}

/// The §4.6a merge sweep's per-cycle WORK BUDGET, ms (design §4.6a (e),
/// finalized): one checkpoint tick period — the SAME law finding 49 gave
/// the threshold drain ("every threshold drain is BOUNDED by one cadence
/// period; at least one item per pass, leftovers re-arm"). The sweep's
/// projection walk is O(resident nodes) per lap (≈ 2,048 × the bench's
/// ns/leaf on the shipped 512 MiB / 256 KiB geometry), and it runs on the
/// checkpoint task INSIDE the cycle a full-cache volume's ring reclamation
/// depends on — so a cycle may spend at most one tick on it, the lap
/// resumes from its cursor next cycle, and the cadence runs every ≤
/// period + one item's service time exactly as the drain's law states.
/// Never a constant of its own: tie-tested against
/// [`checkpoint_tick_period_ms`] (`tests/kv_leaf_merge_tests.rs`).
pub fn merge_sweep_budget_ms(flush_interval_ms: u64) -> u64 {
    checkpoint_tick_period_ms(flush_interval_ms)
}

/// **The writer's checkpoint LANDING ceiling for a commit, ms** — the
/// reader ack ladder's qualify term (spec §6.8 item 3; ladder
/// re-derivation item 1, `.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`).
///
/// [`CHECKPOINT_MAX_AGE_MS`] is the cadence TRIGGER, evaluated in `tick`
/// behind two tick-granularity terms: the tick wait (the decision is
/// taken once per period) and the bounded maintenance drain that precedes
/// the decision in the same tick (finding 49: ≤ one period). A commit
/// acked at `t` therefore has its checkpoint decided by
/// `t + CHECKPOINT_MAX_AGE_MS + 2 × period`; the record lands after the
/// cycle's own writes — a device-time term the reader cannot derive, and
/// which the writer therefore PRICES INTO ITS TRIGGER on a forest volume
/// (PR 13e, F-B1: the cadence fires `checkpoint_trigger_ms` — the ceiling
/// minus the cycle's measured TERM, `checkpoint_cycle_term_ns` — from the
/// last cycle's collection, so the landing stays inside this number on a
/// stationary term; a flat volume keeps the constant trigger verbatim). On the
/// shipped 50 ms flush this is 1,100 ms; on a slow-flush venue the tick
/// IS the landing term (5 s ⇒ 11,000 ms), which the retired `staleness +
/// skew` window (P + 1 s) never covered.
pub fn checkpoint_landing_ceiling_ms(flush_interval_ms: u64) -> u64 {
    CHECKPOINT_MAX_AGE_MS as u64 + 2 * checkpoint_tick_period_ms(flush_interval_ms)
}

/// **The cadence TRIGGER in force for a MAX AGE, ms** (PR 13e, F-B1 —
/// record §7 item 3, the margin derived from the MEASURED cycle term):
/// `max_age − anticipated_term` (saturating). The input is the age the
/// tick fires AT — [`CHECKPOINT_MAX_AGE_MS`] on the routine cadence, the
/// free-grace composite's elastic ceiling while a reader ask is live —
/// and NEVER the landing ceiling: the landing ceiling is `max_age + 2 ×
/// tick` ([`checkpoint_landing_ceiling_ms`]), a PROMISE about when a
/// commit's checkpoint LANDS — the free-grace qualify term and the
/// reader's staleness bound read it as the writer's landing bound — whose
/// two ticks price the tick wait and one period of the tick's own work; a
/// caller feeding it the landing ceiling would land the cycle `2 × tick`
/// past the promise on every cycle, the margin silently eaten (review
/// round 1, Issue 7 — the parameter is named for its input, and the tie
/// `trigger(max_age, term) + 2 × tick == landing ceiling − term` pins
/// it). The cycle's TERM — its pre-barrier wall (the flush pass, the
/// bitmap pages, barrier #1 — at N regions their page writes, at a full
/// cache the appends) plus the tick's own pre-decision work beyond that
/// one period (the deferred-flush barrier and a maintenance item's device
/// time run BEFORE the tick decides) — sat OUTSIDE the two ticks, so a
/// leaf dirtied right after a cycle's collection aged `trigger + late +
/// wall` at the next covering barrier and the audit read the excess as an
/// overrun (16–106 ms past 1,100 on the box, `excused_ns` 0: no other
/// actor's hold, the cadence's own term). The decision anticipates the
/// term it has measured ([`CycleTermWindow`] — the maximum of
/// [`checkpoint_cycle_term_ns`] over the last [`TERM_HORIZON_CYCLES`]
/// cycles, the same interval the audit measures) so the LANDING stays
/// inside the published ceiling on a stationary term; a cycle SLOWER than
/// the measured one still trips the audit — the tripwire keeps its teeth,
/// the published number never widens. A term at or past the max age makes
/// a cycle due every tick, the honest response to a device that cannot
/// land the promise. Tie-tested (`derivation_sweep_tests`).
pub fn checkpoint_trigger_ms(max_age_ms: u64, anticipated_term_ms: u64) -> u64 {
    max_age_ms.saturating_sub(anticipated_term_ms)
}

/// **One cycle's landing TERM**, ns — the interval between the trigger
/// firing and the covering barrier that the ceiling's `2 × tick` does not
/// price (PR 13e, F-B1): the cycle's wall from its age DECISION to barrier
/// #1's completion (PR 13h — the decision, not the cycle's start: the
/// tick's deferred-flush barrier runs between the two since PR 13g read
/// the decision ahead of the drain, and its ≈ 40 ms was the fourth box
/// pass's one trip; a cycle no age decision fired measures from its own
/// start) plus the age decision's lateness past the trigger BEYOND one
/// tick — `wall + (late − tick)⁺`. One tick of lateness is the cadence's wake
/// quantization, the ceiling's first tick; the excess is the tick's own
/// device work ahead of its decision (the deferred-flush barrier, a
/// maintenance item's SMO barrier past the drain deadline — the shape the
/// pin's parked device makes 130 ms of), which the ceiling's second tick
/// prices at one period and no further. It is the LATENESS that is
/// sampled, never the tick's whole pre-decision work: work that ran
/// before the trigger fired cost the leaf nothing, and anticipating it
/// would spend the ceiling's second tick on every cycle and leave a burst
/// one decayed step above the mark nothing to land in. The tick's WAIT for
/// the SMO mutex is not in `late` (the caller subtracts it): a wait behind
/// another holder is that holder's hold — a structural hold the audit
/// excuses (`StructuralHolds`), a census or a service it judges — and
/// never a term for the cadence to anticipate (the fleet's manager read a
/// 7 s wait behind its own online fsck as a term, and a trigger of 0 for
/// the mark's memory). `late` is CAPPED at `late_cap_ns` — one landing
/// ceiling (`AppenderSet::flush_ceiling_ms`), the belt of review round 1,
/// Issue 1: a decision later than the whole ceiling is a stall the audit
/// counts on the cycle it happens, never a term the next 64 cycles
/// anticipate. Tie-tested (`derivation_sweep_tests`).
pub fn checkpoint_cycle_term_ns(
    prebarrier_wall_ns: u64,
    late_ns: u64,
    tick_ns: u64,
    late_cap_ns: u64,
) -> u64 {
    prebarrier_wall_ns.saturating_add(late_ns.min(late_cap_ns).saturating_sub(tick_ns))
}

/// **One flush pass's measured work, split by its two cost classes** (PR
/// 13g, F-B1): the wall spent flushing nodes whose flush wrote NO fresh
/// image (an append of the frozen bset — `node_ns` over `nodes`) and the
/// wall spent on the nodes whose flush ran an SMO (compaction / split /
/// merge — `image_ns` over the fresh IMAGES those SMOs wrote, the unit
/// §4.7's promise ledger counts in: a promised extent is one image).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlushPassSample {
    pub node_ns: u64,
    pub nodes: u64,
    pub image_ns: u64,
    pub images: u64,
    /// The nodes whose flush ran an SMO (the images' sources).
    pub smo_nodes: u64,
}

impl FlushPassSample {
    /// Fold another pass fragment's work into this one (a threshold pass
    /// walks its trees one at a time; the unit is the whole pass's).
    pub fn fold(&mut self, other: &Self) {
        self.node_ns = self.node_ns.saturating_add(other.node_ns);
        self.nodes = self.nodes.saturating_add(other.nodes);
        self.image_ns = self.image_ns.saturating_add(other.image_ns);
        self.images = self.images.saturating_add(other.images);
        self.smo_nodes = self.smo_nodes.saturating_add(other.smo_nodes);
    }

    /// One node's flush: `images` fresh images written in `wall_ns` — SMO
    /// work when any was, an append otherwise.
    pub fn note(&mut self, wall_ns: u64, images: u64) {
        if images > 0 {
            self.image_ns = self.image_ns.saturating_add(wall_ns);
            self.images = self.images.saturating_add(images);
            self.smo_nodes += 1;
        } else {
            self.node_ns = self.node_ns.saturating_add(wall_ns);
            self.nodes += 1;
        }
    }
}

/// **One flush pass's unit for a class** (PR 13g, F-B1): `wall_ns /
/// count` when the pass ran the class, `None` when it did not — a pass
/// without the class measures nothing and the unit in force keeps what
/// the passes that ran it measured, which is what lets a storm's FIRST
/// cycle after a quiet horizon be priced from the units the last storm
/// measured. The unit in force is the MAXIMUM over the last
/// [`TERM_HORIZON_CYCLES`] passes that ran the class (a
/// [`CycleTermWindow`] per class — the term's own law: a ceiling is a
/// BOUND, so the units it anticipates with must be bounds; a mean unit
/// under-prices every above-mean pass, and a machine that slows under a
/// storm raises the bound at the first slow pass instead of an eighth per
/// pass). **The unit's noise bound** (review round 1, Issue 10b): a unit
/// is a MEAN over one pass's count, so a pass of ONE node with a device
/// hiccup would read that hiccup as the per-node cost of every node for
/// the horizon — the wall is spread over at least
/// [`FLUSH_UNIT_COUNT_FLOOR`] (one SMO's grain: below it a pass cannot
/// separate a per-node cost from a per-pass one), so a short pass moves
/// the unit by at most its wall over the grain, while a pass at or past
/// the grain measures exactly. The projection itself is NOT capped —
/// a projection at or past the max age saturates the trigger to 0, one
/// cycle per tick, the right act when the work is real and the cost when
/// the unit is noise: bounded to `1/grain` of a hiccup per node and
/// self-healing when the pass leaves the horizon.
pub fn flush_unit_ns(wall_ns: u64, count: u64) -> Option<u64> {
    (count > 0).then(|| wall_ns / count.max(FLUSH_UNIT_COUNT_FLOOR))
}

/// The count a flush pass's unit is measured over at least — one SMO's
/// grain, [`super::appender::SMO_IMAGES_MAX`] nodes / images (Issue 10b:
/// a unit read off fewer writes is one write's outlier, spread here).
pub const FLUSH_UNIT_COUNT_FLOOR: u64 = super::appender::SMO_IMAGES_MAX as u64;

/// **The LIVE projection of the next cycle's flush wall** (PR 13g,
/// F-B1): the dirty nodes the pass will write times the measured per-node
/// append wall, plus the images the pending commits already PROMISED
/// extents for (§4.7's admission — `heap_promised` on the manager's heap,
/// a region grant's `promised` for its leased leaves) times the measured
/// per-image SMO wall — the next cycle's own bound, read off its PENDING
/// work at every tick, before the cycle runs. The cadence anticipates
/// `max(horizon term, projection)`: the horizon carries what the last
/// cycles COST (the fixed part — publish, pages, ledger — and the lateness
/// law), the projection what the next one CARRIES; a storm's first cycle
/// after a quiet horizon (the box's class — 199 quiet cycles had emptied
/// the 64-cycle horizon of the previous row's 133 ms, the onset cycle
/// tripped at 1,127 ms with 11 ms anticipated), or a cycle whose interval
/// accumulated more work than any before it, is priced from its own
/// pending work instead of a past that never saw it. A unit no pass has
/// measured yet is 0 (a fresh mount's first storm cycle is the horizon's
/// alone — the shipped posture until its first pass with the class); a
/// measured unit is the horizon MAXIMUM ([`flush_unit_ns`]). The image
/// unit is FLOORED at the node unit: an image is one node write at least,
/// so a promised image no SMO pass has priced yet (the first storm after
/// a mount, the fresh-tree shape — every promise is a first) is priced as
/// the append it cannot cost less than, never at 0.
pub fn projected_flush_wall_ns(
    dirty_nodes: u64,
    node_unit_ns: u64,
    promised_extents: u64,
    image_unit_ns: u64,
) -> u64 {
    dirty_nodes
        .saturating_mul(node_unit_ns)
        .saturating_add(promised_extents.saturating_mul(image_unit_ns.max(node_unit_ns)))
}

/// **The covering barriers a due cycle pays between its age decision and
/// its landing** (PR 13h, F-B1's landing residue): barrier #1 (every
/// cycle), and on a DEFERRED-mode volume the tick's deferred-flush barrier
/// — `needs_flush`, set by every non-strict commit group, consumed by the
/// tick between its decision and its cycle: under any load a due tick
/// finds it set, and pricing it on a due tick that does not (one create,
/// then quiet) costs a trigger one barrier early, never a trip. A STRICT
/// volume's commits barrier themselves and never set it. A count, never a
/// clock: the projection multiplies it by the measured barrier unit.
pub fn covering_barriers(strict: bool) -> u64 {
    if strict {
        1
    } else {
        2
    }
}

/// **The LIVE projection of the next cycle's wall from its DECISION to its
/// LANDING** (PR 13h): the flush projection ([`projected_flush_wall_ns`])
/// plus the covering barriers ([`covering_barriers`]) at the measured
/// barrier unit — the horizon MAXIMUM of one barrier's wall on this
/// volume's checkpoint path (`meta_kv_checkpoint_barrier_ms`). The
/// fourth box pass's one trip (1,101 ms, no service, the storm's END)
/// was the deferred barrier's ≈ 40 ms sitting in neither the decision's
/// lateness nor the cycle's wall — the same instant the flush-ceiling
/// audit judges is where the term is measured now (`checkpoint_cycle_term_
/// ns` from the decision), and this is what the projection prices ahead
/// of it: a device whose barrier is slow at REST (the bring-up cycles
/// measure it) is priced before its first storm cycle. Saturating.
pub fn projected_cycle_wall_ns(flush_ns: u64, barriers: u64, barrier_unit_ns: u64) -> u64 {
    flush_ns.saturating_add(barriers.saturating_mul(barrier_unit_ns))
}

/// **The cycle terms a forest volume's cadence anticipates over** — the
/// last [`TERM_HORIZON_CYCLES`] samples; the anticipated term is their
/// MAXIMUM (PR 13e, F-B1). A ceiling is a BOUND, so the term it
/// anticipates must be one: a trigger set off the MEAN term lands past
/// the promise on every cycle whose term is above the mean — which under
/// a create storm is every cycle whose flush pass runs an SMO, because
/// each SMO barriers its successor images (§4.10) and the pass wall is
/// `(SMOs + 1) × barrier`, bursty by construction (the first build's mean
/// estimator left 3 of 6 cycles overrunning at a 150 ms barrier; a
/// DECAYED mark leaked one eighth per quiet cycle and let a burst one
/// step above it land a tick short). The window is the bound over its
/// horizon and forgets a burst exactly when it leaves it — a quieter
/// device earns its cadence back after the horizon; a burst larger than
/// every one in it still trips the audit (the tripwire keeps its teeth).
/// A fixed ring of `u64`s, RAM only, no allocation past the open.
#[derive(Debug)]
pub struct CycleTermWindow {
    samples: [u64; TERM_HORIZON_CYCLES],
    next: usize,
}

/// The horizon of [`CycleTermWindow`], in CYCLES: [`COVER_CYCLES_MAX`] —
/// the ONE cycle-count bound every "cycle until the tail covers X" loop
/// runs to, so a burst is remembered for as long as any cover loop would
/// wait on it (64 at the shipped constants). A count of cycles, never a
/// duration: at the full trigger 64 cycles is about a minute, at a
/// trigger of 0 (a term at the max age) or inside a handover's
/// `checkpoint_now` loop the horizon passes in seconds (review round 1,
/// Issue 8). Tie-tested (`derivation_sweep_tests`).
pub const TERM_HORIZON_CYCLES: usize = COVER_CYCLES_MAX as usize;

impl Default for CycleTermWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl CycleTermWindow {
    pub const fn new() -> Self {
        Self {
            samples: [0; TERM_HORIZON_CYCLES],
            next: 0,
        }
    }

    /// One more cycle's term, ns.
    pub fn push(&mut self, sample_ns: u64) {
        self.samples[self.next] = sample_ns;
        self.next = (self.next + 1) % TERM_HORIZON_CYCLES;
    }

    /// The anticipated term: the maximum over the horizon (0 before the
    /// first sample).
    pub fn anticipated_ns(&self) -> u64 {
        self.samples.iter().copied().max().unwrap_or(0)
    }
}

/// [`checkpoint_landing_ceiling_ms`] at the flush cadence in force — the
/// value a member session answers for its writer today (the fleet-config
/// assumption the reader's poll cadence already makes: one flush knob
/// across the set).
pub fn checkpoint_landing_ceiling_derived() -> u64 {
    checkpoint_landing_ceiling_ms(crate::meta_backend::resolve_flush_interval_ms())
}

/// The landing ceiling for an ELASTIC decision ceiling (the writer→member
/// checkpoint composite, adjudication item 4): the task compares elapsed
/// time against `decision_ms` and tightens its tick to `min(period,
/// decision)` (`checkpoint_task`), so the two tick-granularity terms are
/// bounded by that tightened tick — `decision + 2 × min(tick, decision)`.
/// This is what a grant ADVERTISES: the promise "every commit before this
/// grant is checkpointed within this many ms of it" is honest only with
/// the landing terms in it, and it is the qualify term the member's ack
/// ladder adds its skew to.
pub fn checkpoint_landing_ceiling_for_elastic(decision_ms: u64, flush_interval_ms: u64) -> u64 {
    decision_ms + 2 * checkpoint_tick_period_ms(flush_interval_ms).min(decision_ms)
}

/// §4.7 wedged-tail audit bound (design-smo-replay-currency PR 4
/// clause b; the [`KvMetaBackend::checkpoint_past`] precedent's shape):
/// consecutive barriered cycles with retirements parked, none released,
/// and the ledger tail not advancing, before the volume fails loud. The
/// audit is centralized in `KvMetaBackend::checkpoint_cycle` (P2
/// 2026-07-26 §9: the pre-fix audit lived only on the maintenance arms,
/// so direct-cycle callers livelocked silently). A healthy convergence
/// needs at most a couple of cycles (the first discharges dying floors,
/// the second's tail covers the parked frees), and since the §4.7
/// cycle-break the RESOLVABLE pinned-floor shape always converges that
/// way — what remains for the terminal is a tail pinned by something no
/// flush pass can discharge (a stuck in-flight reservation).
pub const PENDING_FREE_FORCE_CYCLES: u64 = 8;

/// The ONE bound every "cycle the checkpoint until the tail covers X"
/// loop runs to (the mount's bring-up cover, the mount gate's pre-claim
/// drain, the handover's flush-then-transfer post-condition, a grant's
/// ring-0 window clearing — four declarations of one law before PR 4
/// review round 6, Issue 30): **eight clause-b rounds**. A tail that does
/// not move at all fails the volume loud INSIDE the eighth barriered
/// cycle by the §4.7 audit itself ([`PENDING_FREE_FORCE_CYCLES`]), so a
/// loop that reaches this bound has a tail that MOVES and still has not
/// covered its target — a schedule (a slow device, a long in-flight
/// window), never a wedge; what each caller does at the bound is its own
/// class (a grant defers, the mount refuses). Tie-tested in
/// `tests/sym_slot_transfer_tests.rs`.
pub const COVER_CYCLES_MAX: u32 = 8 * PENDING_FREE_FORCE_CYCLES as u32;

/// The cycles a cover-to-FIXPOINT loop runs before it gives up — the
/// shutdown's "tail == head ∧ nothing dirty" convergence and (PR 13g) a
/// joined appender's ring growth under its closed gate. The fixpoint has
/// TWO convergent terms, each bounded by the §4.7 clause-b audit's own
/// law: the TAIL (a cycle's flush pass journals its SMOs past the head
/// its tail was computed from, the next cycle covers them, the parked
/// frees it releases are the audit's — a resolvable pinned floor
/// converges inside [`PENDING_FREE_FORCE_CYCLES`] or the volume fails
/// loud) and the BITMAP (the frees a covering cycle releases dirty pages
/// the next cycle writes — one more window of the same bound). Two
/// windows: a loop still open past them is waiting on what the audit
/// fail-stops, never on a legal schedule. Tie-tested in
/// `tests/derivation_sweep_tests.rs`.
pub const FIXPOINT_COVER_CYCLES_MAX: u64 = 2 * PENDING_FREE_FORCE_CYCLES;

/// Spawn the per-volume checkpoint/writeback task (called by
/// `KvMetaBackend::open`). The task holds a `Weak` backend reference —
/// dropping the backend without `shutdown` reaps it on its next tick (the
/// v2 flusher's sentinel discipline; pinned by
/// `tests/dismount_teardown_tests.rs`) — plus an owned liveness token the
/// teardown tests probe through `checkpoint_alive_probe`.
/// **One threshold-drain pass's budget** (finding 49; PR 13g, F-B1): the
/// first item of the PASS is admitted unconditionally — progress — and
/// every later one only while the deadline stands, whatever tree it
/// belongs to. The bound the cadence relies on is therefore `period +
/// ONE item's service time` per pass: the previous per-tree form
/// admitted one free item per tree and put a forest's tick `trees × one
/// item` late. `None` is the unbounded form (the shutdown tick, the
/// harnesses' `run_maintenance`).
#[derive(Debug, Clone, Copy)]
pub struct DrainBudget {
    deadline: Option<std::time::Instant>,
    progressed: bool,
}

impl DrainBudget {
    /// A pass bounded at `deadline` (`None` = unbounded).
    pub fn new(deadline: Option<std::time::Instant>) -> Self {
        Self {
            deadline,
            progressed: false,
        }
    }

    /// The unbounded pass.
    pub fn unbounded() -> Self {
        Self::new(None)
    }

    /// Whether the next item may run: the pass's first always, a later
    /// one iff the deadline has not passed. Marks the pass progressed.
    pub fn admits(&mut self) -> bool {
        let admit = !self.progressed || self.deadline.is_none_or(|d| std::time::Instant::now() < d);
        self.progressed = true;
        admit
    }

    /// Whether the pass has run an item.
    pub fn progressed(&self) -> bool {
        self.progressed
    }
}

pub(super) fn spawn_checkpoint_task(be: &Arc<KvMetaBackend>) {
    // PR M1 ordering pin (design-metadata-throughput §5.0 B2): the mount
    // gate committed + barriered the writer_claim BEFORE this call — the
    // trace event is the spawn hook the ordering assertion reads.
    be.trace_guard_event("checkpoint_task_spawned");
    let weak = Arc::downgrade(be);
    let alive = Arc::new(());
    let probe = Arc::downgrade(&alive);
    let wake = be.checkpoint_wake();
    // The EXISTING flusher cadence (§4.6): strict mode (interval 0) still
    // needs the background cycle for ring reclamation and SMO service —
    // it ticks at 100 ms; commits barrier themselves.
    let interval = checkpoint_tick_period_ms(crate::meta_backend::resolve_flush_interval_ms());
    // Stage 1c (design-sqz-sync): the checkpoint/SMO task is PLANE-
    // CRITICAL — a lost one stops journal reclamation and wedges every
    // committer at ring admission. It now runs on the sqz-meta lanes
    // (first-party delivery); the shutdown join rides a drop-guarded
    // completion channel instead of a runtime JoinHandle.
    let (done_tx, done_rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
    crate::meta_exec::spawn_meta("kv_checkpoint", async move {
        let mut done = crate::meta_exec::DoneGuard::new(done_tx);
        checkpoint_task(weak, alive, wake, interval).await;
        done.complete();
    });
    be.install_checkpoint_task(done_rx, probe);
}

/// Spawn the per-volume **pending-times drain** task (PR M6, design-
/// metadata-throughput §5.4 D4 — called by `KvMetaBackend::open` beside
/// the checkpoint task). It makes absorbed SETATTR-echo refinements
/// durable in batched transactions on the flush cadence (strict mode
/// ticks at 100 ms like the checkpoint task; commits inside the drain
/// barrier themselves there), waking early on cap crossings.
///
/// A DEDICATED task, deliberately not a checkpoint-tick step: the drain
/// commits through `commit_tx`, whose ring admission may park — parking
/// the checkpoint task on admission would deadlock the very drain that
/// frees ring space (§4.4 pt 5's liveness shape holds precisely because
/// parked committers and the checkpoint drain are different tasks). Same
/// `Weak` sentinel discipline: dropping the backend without `shutdown`
/// reaps it on its next tick; `shutdown` wakes and JOINS it (no leaked
/// tasks).
pub(super) fn spawn_times_drain_task(be: &Arc<KvMetaBackend>) {
    let weak = Arc::downgrade(be);
    let wake = be.times_drain_wake_handle();
    let interval = match crate::meta_backend::resolve_flush_interval_ms() {
        0 => 100,
        ms => ms,
    };
    let (done_tx, done_rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
    crate::meta_exec::spawn_meta("kv_times_drain", async move {
        let mut done = crate::meta_exec::DoneGuard::new(done_tx);
        times_drain_task(weak, wake, interval).await;
        done.complete();
    });
    be.install_times_drain_task(done_rx);
}

async fn times_drain_task(
    weak: std::sync::Weak<KvMetaBackend>,
    wake: Arc<squeezefs_ipc::sqz_notify::Notify>,
    interval_ms: u64,
) {
    let period = std::time::Duration::from_millis(interval_ms);
    // Fixed cadence deadline (the tokio-interval-in-select shape): a wake
    // never re-arms the tick — the deadline only advances when it fires.
    let mut next_tick = std::time::Instant::now() + period;
    loop {
        if squeezefs_ipc::sqz_time::timeout_at(next_tick, wake.notified())
            .await
            .is_err()
        {
            next_tick = std::time::Instant::now() + period;
        }
        let Some(be) = weak.upgrade() else {
            return; // backend dropped without shutdown: exit, leak nothing
        };
        if be.is_shutting_down() {
            return; // shutdown drained inline and joins us
        }
        if be.pending_times_len() == 0 {
            continue;
        }
        if let Err(e) = be.drain_pending_times_now().await {
            log::warn!(
                "kv pending-times drain failed on {:?}: {e} (refinements stay \
                 parked; the next tick retries)",
                be.device_path()
            );
        }
        // The checkpoint task's belt, verbatim: a shutdown that landed
        // WHILE the drain ran is re-armed as a permit, so the park above
        // never sleeps to the cadence deadline with the join waiting on it
        // (`shutdown()` signals with a permit; this covers a signaller that
        // stores the flag without one).
        if be.is_shutting_down() {
            wake.notify_one();
        }
    }
}

async fn checkpoint_task(
    weak: std::sync::Weak<KvMetaBackend>,
    alive: Arc<()>,
    wake: Arc<squeezefs_ipc::sqz_notify::Notify>,
    interval_ms: u64,
) {
    // Owned for the task's lifetime: `checkpoint_alive_probe` upgrades
    // iff this task is still running.
    let _alive = alive;
    let mut last_checkpoint = std::time::Instant::now();
    // A FIXED cadence deadline (not a per-wake re-armed sleep): §4.6 pt 1
    // threshold wakes must never starve the cadence's
    // barriers/checkpoints under a sustained storm — the deadline only
    // advances when it fires.
    //
    // Finding 49: the deadline is read off the CLOCK, not off the select
    // arm. `timeout_at` polls the wake before the sleep, so under a storm
    // that re-arms the maintenance wake on every pass the sleep arm was
    // never reached and the cadence tick — the only path to a ring-
    // pressure checkpoint cycle, i.e. the only thing that ever advances
    // `reusable_upto` for a parked committer — never ran: the ring
    // filled, the pass parked, and the D1.b escalation fail-stopped a
    // volume whose checkpoint could have reclaimed. A wake that returns
    // past the deadline IS the deadline; with the maintenance drain
    // bounded by the period (`run_maintenance_until`), the cadence runs
    // every ≤ period + one maintenance item's service time.
    let period = std::time::Duration::from_millis(interval_ms);
    let mut next_tick = std::time::Instant::now() + period;
    loop {
        let cadence = match squeezefs_ipc::sqz_time::timeout_at(next_tick, wake.notified()).await {
            Ok(()) => std::time::Instant::now() >= next_tick,
            Err(_) => true,
        };
        // The writer→member checkpoint composite (§6.8 item 3 adjudication
        // item 4): while the freed-offset valve is asking its readers to
        // answer sooner, the writer's checkpoint ceiling is the elastic one
        // (`P/2` against the reader's poll) — read once per iteration, one
        // `ArcSwap` load; `None` on every mount without an armed plane and
        // whenever no ask is in force, which leaves the shipped constant
        // and tick untouched. The tick tightens to the ceiling only where
        // the flush cadence is coarser than it (a slow-flush venue): the
        // decision below is evaluated per tick, so a ceiling finer than
        // the tick would otherwise be unreachable.
        let elastic_ceiling = crate::free_grace::checkpoint_ceiling_in_force_ms();
        let period_now =
            elastic_ceiling.map_or(period, |c| period.min(std::time::Duration::from_millis(c)));
        if cadence {
            next_tick = std::time::Instant::now() + period_now;
        }
        let Some(be) = weak.upgrade() else {
            return; // backend dropped without shutdown: exit, leak nothing
        };
        let shutting_down = be.is_shutting_down();
        // PR M1 (design-metadata-throughput §5.0, Issue 14): a FAILED
        // volume — fenced at a barrier (reservation conflict) or latched
        // by repeated write/barrier failures — is fail-stopped: mutations
        // already return EIO, and re-running maintenance + barriers every
        // tick against a fenced/dead device is pure log spam (and, post-
        // fence, writes past a foreign reservation). Idle until shutdown,
        // whose final attempt still runs loud. This applies to the
        // journal-durability paths ONLY — the data-path writeback ladder
        // lives elsewhere and stays retry-forever (never-lossy contract).
        if be.is_failed() && !shutting_down {
            continue;
        }
        // The threshold drain's budget: one cadence period (finding 49 —
        // never a constant; the strict cadence's `0` drains one item per
        // wake, which is still progress). The shutdown tick is unbounded:
        // its final cycle must see every queued append.
        let drain_deadline = (!shutting_down).then(|| std::time::Instant::now() + period_now);
        if cadence || shutting_down {
            if let Err(e) = tick(
                &be,
                &mut last_checkpoint,
                shutting_down,
                drain_deadline,
                elastic_ceiling,
                period_now.as_millis() as u64,
            )
            .await
            {
                // ENG-3 re-triage: error, not warn — a failed cycle can
                // carry a failed allocator-bitmap page write (DUR-4's
                // dirty-set exposure), and this line is the operator's
                // only signal until that item lands.
                log::error!(
                    "kv checkpoint tick failed on {:?}: {e} (state stays RAM-consistent; \
                     retrying next tick — barrier failures additionally escalate through \
                     the sync_device rungs, design-metadata-throughput §5.0)",
                    be.device_path()
                );
            }
            if shutting_down {
                return; // final checkpoint ran inside the tick
            }
            // The slot-lease cadence (design-symmetric-metadata §5.1.3 /
            // §5.1.4, PR 4) — on its OWN task, never inline: a release
            // drains the commit door, and a parked committer needs THIS
            // task's next tick to free ring space (review round 2, Issue
            // 6). Single-flight; a no-op unarmed.
            be.spawn_slot_lease_cadence();
        } else {
            // §4.6 pt 1's threshold trigger (a commit crossed a bset
            // worth of open delta): appends only — no barrier, no ledger,
            // both stay on the cadence. Commit RAM-apply cost is O(open
            // delta), so the drain must not wait out the tick.
            if let Err(e) = maintenance_pass(&be, drain_deadline).await {
                log::warn!(
                    "kv maintenance pass failed on {:?}: {e} (state stays RAM-consistent; \
                     the cadence tick retries)",
                    be.device_path()
                );
            }
        }
        // Work arrived while draining (or the budget / a reserve drain
        // deferred it): re-arm and return to the select so the ticker
        // still gets its turn — never spin the cadence out. The re-arm
        // can no longer starve the cadence: the deadline is read off the
        // clock above.
        //
        // A shutdown that landed WHILE this pass ran is re-armed the same
        // way: the flag is read only after a wake, so a task that parked
        // on it now would sleep to the cadence deadline with the final
        // checkpoint owed (review round 4, Issue 26 — `shutdown()` itself
        // now signals with a permit; this is the belt for any signaller
        // that stores the flag without one).
        if be.is_shutting_down()
            || be
                .maintainable_trees()
                .into_iter()
                .any(|t| t.maintenance_pending())
        {
            wake.notify_one();
        }
    }
}

/// The maintenance-only wake body: drain every tree's threshold queue
/// (bset appends + any compact/split the appends force) within
/// `deadline` (finding 49 — one cadence period; leftovers re-arm the
/// wake). Journal-reserve exhaustion runs one full checkpoint cycle — the
/// §4.4 pt 5 zero-ring-byte drain — exactly like the cadence tick's
/// maintenance step; a pending-free-FIFO refusal (the SMO admission
/// headroom check, design-smo-replay-currency PR 4 clause a) forces the
/// same cycle, whose flush pass now discharges the pinning floor itself
/// (the §4.7 cycle-break) and whose centralized progress audit (clause
/// b, inside `checkpoint_cycle`) bounds a genuinely wedged tail loud.
async fn maintenance_pass(
    be: &Arc<KvMetaBackend>,
    deadline: Option<std::time::Instant>,
) -> Result<(), KvError> {
    let mut smo = be.smo.lock().await;
    // The trees this mount MAINTAINS (never a projection or a foreign
    // lessee's — PR 13), in the pass's ROTATED order under ONE budget
    // (PR 13g, F-B1): the budget bounds the pass at `period + one item`
    // across every tree, and the rotation is what keeps a tree late in
    // the order from starving behind the trees the budget reaches first
    // under a storm that refills every queue.
    let mut budget = DrainBudget::new(deadline);
    let mut sample = FlushPassSample::default();
    for tree in be.maintainable_trees_rotated() {
        loop {
            let r = tree.run_maintenance_until(&mut smo, &mut budget).await;
            match r {
                Ok(out) => {
                    sample.fold(&out.sample);
                    be.note_region_smo_images(&tree, out.sample.images);
                    break;
                }
                Err(KvError::JournalReserveExhausted { .. } | KvError::PendingFreeFull { .. }) => {
                    be.checkpoint_cycle(&mut smo, true).await?;
                }
                Err(e @ KvError::NoSpace { .. }) => {
                    // §4.7 space class: the node stays dirty (its floor
                    // restored), the cadence cycle's flush pass owns the
                    // retry — never a per-tick WARN storm.
                    be.enter_heap_full(&format!("threshold maintenance: {e}"));
                    break;
                }
                Err(KvError::GrantExhausted {
                    appender, needed, ..
                }) => {
                    // The §5.3.3 reactive refill (PR 13 — the flush
                    // passes' arm on the threshold pass): the entry was
                    // handed back, so a refill that lands retries it;
                    // one the manager cannot answer hands the tree's
                    // queue to the cadence (the flush pass owns the
                    // retry and the stall count) — never a WARN per
                    // entry, never an ask per re-armed wake.
                    if be.maintenance_grant_refill(appender, needed, &smo).await {
                        continue;
                    }
                    let dropped = tree.drop_maintenance_queue();
                    log::debug!(
                        "threshold maintenance on {:?}: appender {appender}'s grant is exhausted \
                         ({needed} needed) and no refill landed; {dropped} queued entr(y/ies) \
                         left to the cadence's flush pass",
                        be.device_path()
                    );
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }
    be.note_flush_pass(sample);
    Ok(())
}

/// The cadence tick's checkpoint decision (§4.6 pt 2): what made a cycle
/// due — the ring's distance, a region's pressure, the dirty-node cap, the
/// age law with the decision's lateness — and whether there is anything
/// to cover.
struct CheckpointDecision {
    due: bool,
    covers_something: bool,
    ring_pressure: bool,
    region_pressure: bool,
    /// `Some(late)` = due by age on a forest volume (the lateness rides
    /// the cycle it runs); the flat arm carries `Some(0)`.
    age_late_ns: Option<u64>,
    /// The monotonic instant the decision was taken — the cycle it fires
    /// clocks its landing term from here (PR 13h: the deferred-flush
    /// barrier between the decision and the cycle is inside it).
    decided_at_ns: u64,
}

impl CheckpointDecision {
    fn runs_a_cycle(&self) -> bool {
        self.due && self.covers_something
    }
}

/// **The FLAT volume's age verdict — the shipped law verbatim** (PR 13g
/// review round 3, Issue 25): a cycle is due by age when the max age has
/// elapsed since the last cycle's END, and nothing else — the forest's
/// term horizon, its live projection and the decision's lateness are
/// never consulted on a bit-17-absent volume (`decide_checkpoint`'s flat
/// arm). Tie-tested (`derivation_sweep_tests`); the backend-level
/// contract (`kv_backend_tests`) reads the flat trigger word as the max
/// age with a non-zero term in the horizon beside it.
pub fn flat_age_due(elapsed_ms: u128, max_age_ms: u64) -> bool {
    elapsed_ms >= u128::from(max_age_ms)
}

/// Read the checkpoint decision's inputs and take it (PR 13g, F-B1 made
/// it a function the tick calls before AND after its threshold drain).
fn decide_checkpoint(
    be: &Arc<KvMetaBackend>,
    last_checkpoint: &std::time::Instant,
    mutex_wait_ns: u64,
    final_cycle: bool,
    elastic_ceiling: Option<u64>,
) -> CheckpointDecision {
    let core = be.journal_ring().core();
    let distance = core.head().saturating_sub(core.reusable_upto());
    // Ring 0 keeps the shipped law verbatim (a flat mount is byte-
    // identical); a DECLARED region's ring has its own (PR 2, `AppenderSet::
    // ring_pressure`): a committer parked at a full region ring holds no
    // node lock (§4.4 pt 5), so nothing is dirty and ring 0 is idle — read
    // alone, this tick would never make a cycle due and nobody would
    // advance that ring's `reusable_upto` (the wedge the pressure contract
    // pins).
    let region_pressure = be.appenders().is_some_and(|a| a.ring_pressure());
    let ring_pressure = distance > core.geometry().logical_len() / 2 || region_pressure;
    let dirty_nodes = be.dirty_node_count();
    // The freed-offset composite's ceiling in force replaces the constant
    // while a reader ask is live (`P/2` — every reader pass finds a new
    // root); `None` is the shipped decision verbatim. Compared against the
    // ELAPSED time, so a ceiling that tightens mid-interval fires at once
    // — what lets an advertised ceiling be a promise about commits that
    // preceded the grant, not only about the ones that follow it.
    // The age law on a FOREST volume (PR 13e, F-B1 — the population the
    // flush-ceiling audit judges): the elapsed time runs from the last
    // cycle's COLLECTION (a leaf dirtied after it is this cycle's — the
    // interval the landing ceiling bounds; a cycle's post-barrier work no
    // longer eats the margin), against the TRIGGER in force — the MAX AGE
    // minus the cycle's measured TERM (`checkpoint_trigger_ms`; the max
    // age is what the tick fires AT, the landing ceiling is `max_age + 2
    // × tick` — review round 1, Issue 7), so the covering barrier lands
    // inside the promise the ceiling's consumers read; the tick in force
    // is what one period of the decision's lateness is priced against. A
    // FLAT volume keeps the shipped law verbatim: the max age elapsed
    // since the last cycle's end.
    let max_age_ms = elastic_ceiling.map_or(CHECKPOINT_MAX_AGE_MS as u64, |c| c);
    // `Some(late)` = due by age on a forest volume, with the decision's
    // lateness for the cycle it runs; the flat arm carries no lateness.
    let decided_at_ns = crate::mono_core::monotonic_ns_u64();
    let age_late_ns = if be.appenders().is_some() {
        be.checkpoint_due_by_age(max_age_ms, mutex_wait_ns, decided_at_ns, dirty_nodes)
    } else {
        flat_age_due(last_checkpoint.elapsed().as_millis(), max_age_ms).then_some(0)
    };
    let due = final_cycle
        || ring_pressure
        // The cap is resolved ONCE at open (`KvMetaBackend::dirty_node_cap`
        // — the backend-knob convention): this branch runs on every 50 ms
        // cadence tick, and re-deriving here (env `CString`s + cgroup/
        // sysinfo probes in `resolve_budget_now`) was an allocation stream
        // that broke the op-economy allocation-free contract (2026-08-02).
        || dirty_nodes > be.dirty_node_cap()
        || age_late_ns.is_some();
    // A declared region's uncovered ring counts as "something to cover"
    // exactly as ring 0's `distance > 0` does (its tail lands a cycle
    // after its flush, like the ledger's).
    let regions_uncovered = be.appenders().is_some_and(|a| a.rings_uncovered());
    CheckpointDecision {
        due,
        covers_something: final_cycle || dirty_nodes > 0 || distance > 0 || regions_uncovered,
        ring_pressure,
        region_pressure,
        age_late_ns,
        decided_at_ns,
    }
}

/// One task tick: checkpoint decision → maintenance (within
/// `drain_deadline`, when no cycle is due) → deferred flush barrier →
/// checkpoint when due (decided again after a drain). `final_cycle` (shutdown) drains
/// in-flight commits first and forces a full cycle with an immediate
/// post-ledger barrier, leaving `tail == head` — an empty replay window
/// for the next mount. `elastic_ceiling` is the freed-offset composite's
/// checkpoint ceiling in force (`None` = the shipped `CHECKPOINT_MAX_AGE_MS`);
/// `tick_ms` the cadence period in force (the elastic composite may
/// tighten it below the routine tick).
async fn tick(
    be: &Arc<KvMetaBackend>,
    last_checkpoint: &mut std::time::Instant,
    final_cycle: bool,
    drain_deadline: Option<std::time::Instant>,
    elastic_ceiling: Option<u64>,
    tick_ms: u64,
) -> Result<(), KvError> {
    if final_cycle {
        // New mutations are already refused (`write_gate`); wait out the
        // in-flight ones so the final flush pass sees every applied
        // record and the tail lands exactly on the head. Drain UNTIL THE
        // HEAD IS STABLE, not one sampled head: a commit that passed the
        // gate before the flag can reserve ring space after a one-shot
        // sample, and its entry then sits past the final checkpoint's
        // tail — the next mount replays it (kill-9 soak, "clean shutdown
        // must leave an empty replay window", ~1/200 deep-churn rounds).
        // Gated writers are finite, so the head converges; the iteration
        // bound is a belt against a pathological writer, after which the
        // final cycle proceeds with the freshest head it saw.
        //
        // BEFORE taking the SMO mutex (D-2): in-flight reservations are
        // completed by the conveyor's durability lane, whose hole
        // checkpoints (`checkpoint_past`) take this mutex — a lane parked
        // on it while earlier windows of its own still hold open
        // reservations would never complete them, and this wait would
        // hold the mutex forever.
        //
        // EVERY ring gets this wait (review round 3, Issue 21): a declared
        // region's in-flight window — its committer passed the gate, its
        // write is on the device — would otherwise only be OBSERVED by the
        // all-rings fixpoint below (`min_inflight_start` keeps the region
        // uncovered, every iteration re-cycles), so a slow region write
        // could burn the fixpoint bound and land the leave on its belt
        // (page `Live`, own-residue recovery — sound, but the guarantee
        // missed) where waiting here lets the guarantee hold.
        let region_rings: Vec<Arc<super::journal::JournalRing>> = be
            .appenders()
            .map(|a| a.regions.iter().skip(1).map(|r| r.ring()).collect())
            .unwrap_or_default();
        for _ in 0..64 {
            let head0 = be.journal_ring().core().head();
            let heads: Vec<u64> = region_rings.iter().map(|r| r.core().head()).collect();
            be.journal_ring().wait_completed_upto(head0).await;
            for (ring, head) in region_rings.iter().zip(&heads) {
                ring.wait_completed_upto(*head).await;
            }
            if be.journal_ring().core().head() == head0
                && region_rings
                    .iter()
                    .zip(&heads)
                    .all(|(ring, head)| ring.core().head() == *head)
            {
                break;
            }
        }
    }
    // The tick's WAIT for the mutex is another holder's hold — measured so
    // the age decision below leaves it out of the lateness it anticipates
    // (PR 13e, F-B1).
    let lock_requested = std::time::Instant::now();
    let mut smo = be.smo.lock().await;
    let mutex_wait_ns = lock_requested.elapsed().as_nanos() as u64;

    // The checkpoint DECISION is read BEFORE the threshold drain (PR 13g,
    // F-B1): a cycle's flush pass appends every dirty node, so a drain
    // ahead of a DUE cycle is work the cycle repeats — and its bound
    // (`period + one item`) was a second period of decision lateness on
    // top of the maintenance wake's own pass just before the tick, past
    // the landing ceiling's two-tick margin by itself. A tick with no
    // cycle due drains (step 1) and decides again after it: a trigger the
    // drain crossed fires this tick, never the next. The shutdown tick
    // keeps the drain FIRST and unbounded — its final cycle must see
    // every queued append.
    let mut decision = if final_cycle {
        None
    } else {
        Some(decide_checkpoint(
            be,
            last_checkpoint,
            mutex_wait_ns,
            final_cycle,
            elastic_ceiling,
        ))
    };
    let run_drain = !decision.as_ref().is_some_and(|d| d.runs_a_cycle());

    // 1. Threshold maintenance (appends + SMOs, serialized here — §4.6),
    //    within the drain budget (finding 49: the checkpoint decision
    //    below must not sit behind an unbounded drain under a storm).
    //    Reserve exhaustion runs a drain cycle and retries; a pending-
    //    free-FIFO refusal (SMO admission headroom, design-smo-replay-
    //    currency PR 4 clause a) forces the same cycle — its flush pass
    //    discharges the pinning floor (the §4.7 cycle-break) and its
    //    centralized progress audit (clause b) bounds genuine wedges.
    //    Over the trees this mount MAINTAINS (PR 13): a joined appender's
    //    projections of the manager's trees are never appended to here.
    //    ONE budget across the trees, rotated (PR 13g, F-B1 — the age
    //    decision sits behind this drain when it runs; its lateness is
    //    bounded by the budget's `period + one item`, never by the
    //    forest's width).
    if run_drain {
        let mut budget = DrainBudget::new(drain_deadline);
        let mut sample = FlushPassSample::default();
        for tree in be.maintainable_trees_rotated() {
            loop {
                match tree.run_maintenance_until(&mut smo, &mut budget).await {
                    Ok(out) => {
                        sample.fold(&out.sample);
                        be.note_region_smo_images(&tree, out.sample.images);
                        break;
                    }
                    Err(
                        KvError::JournalReserveExhausted { .. } | KvError::PendingFreeFull { .. },
                    ) => {
                        be.checkpoint_cycle(&mut smo, true).await?;
                        *last_checkpoint = std::time::Instant::now();
                    }
                    Err(e @ KvError::NoSpace { .. }) => {
                        // §4.7 space class (see `maintenance_pass`): the
                        // cycle below owns the retry.
                        be.enter_heap_full(&format!("threshold maintenance: {e}"));
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        be.note_flush_pass(sample);
    }

    // 2. The deferred-mode flush barrier (the v2 flusher tick). Also
    //    drains the §4.6 pt 3 pending-reclaim for previously-written
    //    ledger records.
    if be.take_needs_flush() {
        crate::fuse_client::METRICS
            .meta_flush_deferred
            .fetch_add(1, Ordering::Relaxed);
        let barrier_started = std::time::Instant::now();
        be.sync_device().await.map_err(KvError::Io)?;
        // One covering barrier's wall — the unit the projection prices
        // the next cycle's barriers with (PR 13h).
        be.note_checkpoint_barrier(barrier_started.elapsed().as_nanos() as u64);
    }

    // 3. Checkpoint decision (§4.6 pt 2): cadence, journal distance,
    //    dirty-node cap, shutdown — read once more after a drain (or for
    //    the first time on the shutdown tick).
    if run_drain {
        decision = Some(decide_checkpoint(
            be,
            last_checkpoint,
            mutex_wait_ns,
            final_cycle,
            elastic_ceiling,
        ));
    }
    let d = decision.expect("a checkpoint decision was taken on every arm");
    if d.runs_a_cycle() {
        // Immediate post-ledger barrier under pressure or at shutdown:
        // reclamation must not lag a cycle when parkers wait on it.
        if d.region_pressure {
            if let Some(a) = be.appenders() {
                a.pressure_cycles.fetch_add(1, Ordering::Relaxed);
            }
        }
        // The age decision's lateness rides the cycle it RUNS, and only
        // that one (PR 13e review round 1, Issue 1): a due tick that runs
        // no cycle records nothing. The decision's INSTANT rides with it
        // (PR 13h): the cycle's landing term is clocked from the decision,
        // so the deferred-flush barrier step 2 ran between the two is in
        // the horizon the trigger anticipates — the fourth box pass's one
        // trip was that barrier's wall priced nowhere.
        if be.appenders().is_some() {
            if let Some(late_ns) = d.age_late_ns {
                be.note_checkpoint_decision(late_ns, tick_ms, d.decided_at_ns);
            }
        }
        be.checkpoint_cycle(&mut smo, d.ring_pressure || final_cycle)
            .await?;
        *last_checkpoint = std::time::Instant::now();
        if elastic_ceiling.is_some() {
            crate::free_grace::note_elastic_checkpoint_cycle();
        }
    } else if d.age_late_ns.is_some() && be.appenders().is_some() {
        // An idle due tick with nothing to cover — nothing dirty, the ring
        // covered, no region uncovered — is an EMPTY COLLECTION: every leaf
        // dirtied from here is bounded from here, so the age law's
        // reference advances and an idle volume never accrues lateness
        // (Issue 1: the first build let the idle span become the first
        // busy cycle's term, a trigger of 0 for the horizon).
        be.note_checkpoint_collected(crate::mono_core::monotonic_ns_u64());
    }
    if final_cycle {
        // Shutdown guarantee (`KvMetaBackend::shutdown`: "tail == head ⇒
        // an empty replay window"): a cycle's flush pass may itself
        // journal — a node whose log area filled compacts through the SMO
        // path, whose claim/free records land at positions PAST the `H`
        // that cycle's tail was computed from, and an SMO's parent-pointer
        // apply re-dirties an already-visited node. On the cadence path
        // the NEXT cycle covers them ("reclamation lags one cycle"), but
        // at shutdown there is no next cycle — the residue would replay at
        // the next mount. Iterate to the fixpoint: every extra cycle
        // flushes what the previous one dirtied and covers what it
        // journaled; a cycle only journals when it rewrites a full node
        // log (strictly consumed), so this converges within the SMO
        // cascade height. The bound is defensive.
        //
        // The fixpoint is over EVERY ring (review round 2, Issue 16):
        // since PR 3 a leased slot tree's SMO journals into its REGION's
        // ring and clamps the REGION's tail, and its moved root reaches
        // tree 0 only through the NEXT cycle's publication — so a cycle
        // that left ring 0 at `head == reusable_upto` can leave a region
        // uncovered, and the leave that followed wrote that region's page
        // `Free` over the uncovered window (acked records lost). A cycle
        // N+1 publishes what cycle N's flush pass moved and covers the
        // region's records; N+2 covers the publication itself.
        // The seam counts TOTAL final cycles (the one above included): `1`
        // = no fixpoint iteration at all — the ring-0-idle shape of the
        // defect.
        let bound = match crate::meta_backend::kv::backend::TEST_SHUTDOWN_FIXPOINT_CYCLES
            .load(Ordering::Relaxed)
        {
            0 => FIXPOINT_COVER_CYCLES_MAX,
            n => n - 1,
        };
        // Coverage alone is not convergence: the cycle's own barrier
        // releases the pending frees its tail covers AFTER the cycle wrote
        // its bitmap pages, so the release is a dirty page for the NEXT
        // cycle — and without one the retired images read CLAIMED at every
        // later mount (the clean-unmount leak PR 11's census found). One
        // more cycle persists them; with nothing dirty it journals no SMO,
        // so the term converges in that cycle.
        // A JOINED appender (PR 12b) is judged by ITS ring alone: ring 0
        // and the bitmap are the manager's — its projection of them holds
        // the manager's uncovered window and the replayed deltas' dirty
        // bits, which no cycle of a joiner may write (the first build
        // looped its bound on them and warned of a residue that was never
        // its own).
        let joined = be.is_joined_appender();
        let uncovered = |be: &KvMetaBackend| {
            let regions = be.appenders().is_some_and(|a| a.rings_uncovered());
            if joined {
                return regions;
            }
            let core = be.journal_ring().core();
            core.head() != core.reusable_upto() || regions || be.allocator().has_dirty_pages()
        };
        for _ in 0..bound {
            if !uncovered(be) {
                return Ok(());
            }
            be.checkpoint_cycle(&mut smo, true).await?;
            *last_checkpoint = std::time::Instant::now();
        }
        if uncovered(be) {
            let core = be.journal_ring().core();
            log::warn!(
                "shutdown checkpoint did not converge to an empty replay window \
                 (ring 0 head={}, reusable_upto={}; a declared region uncovered: {}; dirty \
                 bitmap pages: {}): the next mount will replay the residue (sound — an \
                 uncovered region's page stays Live at the leave — but the shutdown \
                 tail==head guarantee was missed)",
                core.head(),
                core.reusable_upto(),
                be.appenders().is_some_and(|a| a.rings_uncovered()),
                be.allocator().has_dirty_pages()
            );
        }
    }
    Ok(())
}

/// TEST seam ONLY (`false` in production, one relaxed load): halt the
/// checkpoint cycle right after its ledger record landed — the PR 8
/// crash-window pin for "pages before the tail-advancing record"
/// (`tests/sym_block_grant_tests.rs`).
pub static TEST_CHECKPOINT_HALT_AFTER_LEDGER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// TEST seam ONLY (`false` in production, one relaxed load): halt the
/// checkpoint cycle right BEFORE its ledger record — every SMO record the
/// flush pass journaled sits in the window UNCOVERED, the other crash
/// window of a cycle. PR 10 review round 4, Issue 31: a recoverer dying
/// there, inside its step-6 flush of a dead lessee's tree, leaves the
/// manager's interior records for a slot tree 0 leases to the dead
/// appender in ring 0 (`tests/sym_crash_matrix_tests.rs`).
pub static TEST_CHECKPOINT_HALT_BEFORE_LEDGER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// TEST seam ONLY (`false` in production, one relaxed load): PARK the
/// manager's checkpoint cycle after its ledger record and BEFORE its
/// appender page writes — the window between the cycle's tree-0
/// publication (`publish_forest_roots`, the cycle's first step) and the
/// page that names the region's roots (PR 13b review round 1, Issue 2: a
/// first touch landing here — the manager's own takes no SMO mutex — must
/// not evict a page-homed root from the page un-named; `tests/
/// sym_n_daemon_tests.rs`). Released by [`test_release_checkpoint_park`].
pub static TEST_CHECKPOINT_PARK_BEFORE_PAGES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Cycles that PARKED on [`TEST_CHECKPOINT_PARK_BEFORE_PAGES`] so far.
static TEST_CHECKPOINT_PARKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TEST_CHECKPOINT_PARK_NOTIFY: once_cell::sync::Lazy<squeezefs_ipc::sqz_notify::Notify> =
    once_cell::sync::Lazy::new(squeezefs_ipc::sqz_notify::Notify::new);

/// Cycles parked on [`TEST_CHECKPOINT_PARK_BEFORE_PAGES`] so far.
pub fn test_checkpoint_parked_count() -> u64 {
    TEST_CHECKPOINT_PARKED.load(Ordering::Acquire)
}

/// Release every cycle parked on [`TEST_CHECKPOINT_PARK_BEFORE_PAGES`]
/// (the flag cleared first).
pub fn test_release_checkpoint_park() {
    TEST_CHECKPOINT_PARK_BEFORE_PAGES.store(false, Ordering::Relaxed);
    TEST_CHECKPOINT_PARK_NOTIFY.notify_waiters();
}

impl KvMetaBackend {
    /// **A checkpoint-class durable step CONSUMES a checkpoint seq — and
    /// writes the ledger record that seq names** (symmetric PR 13, defect
    /// 25). PR 2's ring growth, the appender leave (in-process and the
    /// wire `LeaveAppender`) and PR 10's region release each write the
    /// allocation bitmap at a fresh `checkpoint_seq` so the next cycle's
    /// pages carry a strictly higher generation (DUR-4's raise stays a
    /// signal) — and left the LEDGER with a gap: no record at that seq.
    /// PR 5's predicted-slot poll reads slot `(adopted + 1) % 32` and
    /// stops on an OLDER record there ("the writer has not written that
    /// seq"), so after ONE gap every `-o ro` token reader stopped adopting
    /// until the writer's seq wrapped the whole ring — 32 checkpoints, 41 s
    /// on the fleet's `sym-storm` round (seven regions released at once;
    /// the reader's tree 0 named the dead lessees for the whole window and
    /// every read of a recovered slot failed at the dead address) and
    /// unbounded on a quiet volume. The law now: **a consumed seq is a
    /// ledger seq** — this writes the bitmap pages at `ckpt_seq`, then a
    /// record at `ckpt_seq` RESTATING the last cycle's word (the roots as
    /// they stand, the last record's tail — nothing became covered — the
    /// live `next_ino` and watermark), then the barrier. Content-equivalent
    /// to the record it follows: a crash after it replays exactly what a
    /// crash after the last cycle would. **The caller holds the SMO mutex**
    /// (no cycle mid-flight: the roots are the last record's) or is the
    /// shutdown's serialized tail. Returns the seq consumed.
    pub(crate) async fn consume_checkpoint_seq_for_bitmap(
        &self,
    ) -> std::result::Result<u64, KvError> {
        let ckpt_seq = self.checkpoint_seq.fetch_add(1, Ordering::AcqRel) + 1;
        self.allocator()
            .write_dirty_pages(
                self.device_path(),
                self.superblock().alloc_bitmap.start,
                ckpt_seq,
            )
            .await?;
        self.sync_device().await.map_err(KvError::Io)?;
        let tree_roots: Vec<TreeRoot> = self
            .ledger_root_trees()
            .into_iter()
            .map(|t| TreeRoot {
                tree_id: t.tree_id(),
                node_addr: t.root().addr,
                node_seq: t.root().seq,
            })
            .collect();
        let rec = LedgerRecord {
            seq: ckpt_seq,
            tree_roots,
            journal_tail_seq: self.last_ledger_tail.load(Ordering::Acquire),
            next_ino: self.next_ino(),
            alloc_bitmap_generation: ckpt_seq,
            node_seq_watermark: self.all_trees()[0].node_seq_snapshot(),
            membership_stamp: self.membership_stamp_for_ledger(),
            append_partition: None,
        };
        write_ledger_slot(
            self.device_path(),
            self.superblock().root_ledger.start,
            &rec,
        )
        .await?;
        self.sync_device().await.map_err(KvError::Io)?;
        super::META_KV_LEDGER_RESTATEMENTS.fetch_add(1, Ordering::Relaxed);
        Ok(ckpt_seq)
    }

    /// One §4.6 pt 2 checkpoint cycle (module docs pin the order).
    /// Serialized by the SMO mutex the caller holds. `barrier_now` makes
    /// the freshly-written ledger record durable inside this cycle
    /// (shutdown / ring pressure); otherwise its durability rides the
    /// next barrier and reclamation lags one cycle (§4.6 pt 2's "rides
    /// the next barrier" default).
    pub(super) async fn checkpoint_cycle(
        &self,
        smo: &mut SmoContext,
        barrier_now: bool,
    ) -> Result<(), KvError> {
        // PR 12b: a JOINED appender's cycle is over ITS ring, ITS slot
        // trees and ITS page alone — the ledger, the bitmap, tree 0 and
        // page 0 below are the manager's (`joined_checkpoint_cycle`).
        let r = if self.is_joined_appender() {
            self.joined_checkpoint_cycle(smo, barrier_now).await
        } else {
            self.checkpoint_cycle_managed(smo, barrier_now).await
        };
        // A cycle that fails BEFORE it folds the age decision's lateness
        // (`note_checkpoint_cycle_term` runs at barrier #1; an `Err` out
        // of the root publication or the flush pass precedes it) must not
        // leave the decision words for the NEXT cycle to fold as its own
        // term (review round 2, Issue 12).
        if r.is_err() {
            self.clear_checkpoint_decision();
        }
        r
    }

    /// The MANAGER's cycle — the volume's ledger, bitmap, tree 0 and page
    /// 0 are its own (a joined appender's is `joined_checkpoint_cycle`).
    async fn checkpoint_cycle_managed(
        &self,
        smo: &mut SmoContext,
        barrier_now: bool,
    ) -> Result<(), KvError> {
        let cycle_started = std::time::Instant::now();
        let cycle_started_ns = crate::mono_core::monotonic_ns_u64();
        let h = self.journal_ring().core().head();
        // The wedged-tail progress audit's inputs (see the barrier_now
        // block at the end): captured before the cycle mutates anything.
        let pending_before = self.allocator().pending_count();
        let tail_before = self.last_ledger_tail.load(Ordering::Acquire);

        // ---- Forest: publish every moved guest slot root into tree 0
        // FIRST, so the flush pass below carries tree 0's leaf and this
        // cycle's ledger record covers the publication (the slot-tree
        // root-swap floor's covering record — `publish_forest_roots`).
        // Reserve exhaustion defers to the next cycle; the unpublished
        // roots' floors then clamp this cycle's tail (below), so every
        // record applied under a root tree 0 does not yet name stays in
        // the replay window — the flush pass may clear those nodes' own
        // floors, the roots' floors it cannot.
        match self.publish_forest_roots().await {
            Ok(()) => {}
            Err(KvError::JournalReserveExhausted { needed }) => {
                log::debug!(
                    "checkpoint: SMO reserve exhausted ({needed} B) publishing forest roots; \
                     deferred to the next cycle (the unpublished roots clamp the tail)"
                );
            }
            Err(e) => return Err(e),
        }

        // ---- Flush pass: every dirty node once, snapshot-then-write.
        // SMO-reserve exhaustion skips the node (floor restored — the
        // tail keeps respecting it) and retries next cycle with the
        // budget this cycle frees.
        let mut dirty: Vec<Arc<CachedNode>> = Vec::new();
        // The regions whose LEAVES are dirty as the pass begins, each with
        // its OLDEST leaf's dirty-since instant — the flush-ceiling
        // audit's subjects (KD-SYM-10 bounds the AGE of a dirty leaf, from
        // the record that dirtied it; forest volumes only).
        let mut had_dirty: Vec<super::backend::DirtyLeafAge> = Vec::new();
        let region_aware = self.appenders().is_some();
        self.node_cache().for_each_node(|n| {
            if n.dirty_floor() != u64::MAX && !n.state().is_superseded() {
                if region_aware && n.level() == 0 {
                    // Only STAMPED leaves are the audit's subjects (a slot
                    // tree's; tree 0's leaf carries no stamp and no age).
                    let since = n.dirty_since_ns();
                    if since != 0 {
                        let r = self.region_of_node(n);
                        let held = n.dirty_since_held_ns();
                        match had_dirty.iter_mut().find(|d| d.region == r) {
                            Some(d) => d.fold_oldest(since, held),
                            None => had_dirty.push(super::backend::DirtyLeafAge {
                                region: r,
                                since_ns: since,
                                held_at_since_ns: held,
                            }),
                        }
                    }
                }
                dirty.push(Arc::clone(n));
            }
        });
        // The age law's reference (PR 13e, F-B1): every leaf dirtied from
        // here on is the NEXT cycle's, so the cadence measures its interval
        // from this instant — never from the cycle's end.
        self.note_checkpoint_collected(crate::mono_core::monotonic_ns_u64());
        let t_collected = std::time::Instant::now();
        let dirty_count = dirty.len();
        // Nodes this pass could not flush because the allocator answered
        // `NoSpace` — the wedged-tail audit's class discriminator below.
        let mut deferred_for_space = 0u64;
        // Regions whose SMO wanted an extent their exhausted grant could
        // not give (the manager's refill owed): deferred like the space
        // arm, counted on `manager_dependency_stalls`.
        let mut deferred_for_grant = 0u64;
        // The pass's work split by class — the cadence's live projection's
        // units (PR 13g, F-B1): a node whose flush wrote fresh images is
        // SMO work priced per image, every other node an append priced
        // per node. The same per-volume image count is a region's SMO-rate
        // input (review round 1, Issue 11 — the process-wide SMO counters
        // fold every volume's).
        let mut sample = FlushPassSample::default();
        for node in dirty {
            let addr = node.addr();
            let tree = self.tree_of_node(&node)?;
            // A leased slot tree's SMOs journal into its lessee's ring and
            // claim inside its grant (§5.2.3 / §5.3.3) — the tree scopes
            // itself; the region id here is the SMO-rate ledger's.
            let region_id = node
                .forest_slot()
                .map(|slot| self.region_of_slot(slot))
                .filter(|id| *id != 0);
            let images_before = smo.images_written();
            let node_started = std::time::Instant::now();
            let mut out = tree.checkpoint_flush_node(smo, addr).await;
            // A REACTIVE refill (§5.3.3): the flush pass that exhausts a
            // region's grant asks the manager — this process, in PR 3 —
            // for more and retries the node once; only a manager that
            // cannot answer leaves the SMO deferred and counts the stall.
            // The ask is a LADDER (PR 13g, F-R5): on a healthy heap the
            // DERIVED grant in the USER class — the SMO's own need at
            // least (a constant ask was re-answered verbatim once the
            // remainder held it while a fat overlay's split needed more;
            // review round 3) — so an exhausted region is re-supplied at
            // the steady-state grain, never one SMO at a time; only the
            // SPACE class (a USER carve the reserve refuses) falls to the
            // INTERNAL class for exactly ONE SMO's images
            // (`SMO_IMAGES_MAX`): the flush pass's SMOs are COMPACTIONS —
            // the SMOs that return extents — so that carve draws down to
            // the compaction floor like the manager's own (Issue 6: the
            // heap-full recovery must make progress on a leased slot tree
            // too), one image at a time, never the derived grant (the
            // reserve is the manager's recovery budget — Issue 18). A heap
            // that cannot serve even that answers `NoSpace`, the SPACE
            // class below — never the manager dependency.
            if let (
                Err(KvError::GrantExhausted {
                    appender, needed, ..
                }),
                Some(id),
            ) = (&out, region_id)
            {
                if *appender == id && !super::appender::test_manager_unreachable() {
                    let one_smo = u32::try_from(*needed)
                        .unwrap_or(u32::MAX)
                        .max(super::appender::SMO_IMAGES_MAX);
                    let derived = self
                        .appenders()
                        .map_or(u64::from(one_smo), |a| self.grant_extents_for(a, id));
                    let want = u32::try_from(derived).unwrap_or(u32::MAX).max(one_smo);
                    match self
                        .manager_extent_grant_class(
                            id,
                            want,
                            super::alloc_ext_core::AllocClass::User,
                        )
                        .await
                    {
                        Ok(runs) if !runs.is_empty() => {
                            out = tree.checkpoint_flush_node(smo, addr).await;
                        }
                        Ok(_) | Err(KvError::NoSpace { .. }) => {}
                        Err(e) => log::warn!(
                            "checkpoint: appender {id}'s reactive ExtentGrant deferred ({e})"
                        ),
                    }
                    // Still short — the space class, or a split wider than
                    // the clamped USER carve: one SMO's images, INTERNAL,
                    // sized by the need and unclamped.
                    if matches!(&out, Err(KvError::GrantExhausted { appender, .. }) if *appender == id)
                    {
                        match self
                            .manager_extent_grant_class(
                                id,
                                one_smo,
                                super::alloc_ext_core::AllocClass::Internal,
                            )
                            .await
                        {
                            Ok(runs) if !runs.is_empty() => {
                                out = tree.checkpoint_flush_node(smo, addr).await;
                            }
                            Ok(_) => {}
                            Err(KvError::NoSpace { free, reserve }) => {
                                out = Err(KvError::NoSpace { free, reserve });
                            }
                            Err(e) => log::warn!(
                                "checkpoint: appender {id}'s reactive ExtentGrant deferred ({e})"
                            ),
                        }
                    }
                }
            }
            let images = smo.images_written().saturating_sub(images_before);
            if let Some(id) = region_id {
                if images > 0 {
                    if let Some(r) = self.appenders().and_then(|a| a.region(id)) {
                        r.smos_this_cycle.fetch_add(images, Ordering::Relaxed);
                    }
                }
            }
            sample.note(node_started.elapsed().as_nanos() as u64, images);
            match out {
                Ok(()) => {}
                Err(KvError::JournalReserveExhausted { needed }) => {
                    log::debug!(
                        "checkpoint: SMO reserve exhausted ({needed} B) at node {addr:#x}; \
                         deferred to the next cycle"
                    );
                }
                Err(KvError::GrantExhausted {
                    appender,
                    unclaimed,
                    needed,
                }) => {
                    deferred_for_grant += 1;
                    if let Some(r) = self.appenders().and_then(|a| a.region(appender)) {
                        r.dependency_stalls.fetch_add(1, Ordering::Relaxed);
                    }
                    log::warn!(
                        "checkpoint: appender {appender}'s extent grant is exhausted ({unclaimed} \
                         unclaimed, {needed} needed) at node {addr:#x}; compaction deferred to \
                         the next cycle until the manager refills it (manager_dependency_stalls, \
                         bound manager_dependency_stall_bound_ms)"
                    );
                }
                Err(KvError::NoSpace { free, reserve }) => {
                    // ENOSPC-recovery ratchet (the preserved md-storm
                    // image's third face): a heap drained to zero cannot
                    // claim this compaction's successor extent — but
                    // aborting the WHOLE cycle here would also abandon
                    // the ledger + barrier that release parked
                    // retirements (whose held bits ARE the missing
                    // budget). Skip-and-defer exactly like the reserve
                    // arm (the floor was restored inside
                    // checkpoint_flush_node): the cycle completes,
                    // barriers, drains every retirement its tail covers,
                    // and the NEXT cycle's claim finds the returned
                    // budget. Since the §4.7 heap admission (2026-09-11)
                    // every acked record has its SMO's extents promised,
                    // so this arm is the RESIDUAL — a foreign claimant,
                    // an under-projection, an interior cascade past the
                    // reserve — and it is the SPACE standstill class:
                    // counted, latched loud once, never the terminal.
                    deferred_for_space += 1;
                    log::debug!(
                        "checkpoint: metadata heap exhausted (free={free}, \
                         reserve={reserve}) at node {addr:#x}; compaction deferred to \
                         the next cycle"
                    );
                }
                // NOTE: `KvError::PendingFreeFull` is structurally
                // unreachable here since the §4.7 cycle-break — the flush
                // pass runs its SMOs with `forced_retirement` (at-cap
                // retirements park in the allocator's overflow, never
                // refuse). The former clause-c "skip-and-defer" arm is
                // exactly what closed the pinned-floor dependency cycle
                // (P2 2026-07-26 §9): skipping THE compaction that
                // discharges the tail-pinning floor restored the ancient
                // floor every cycle, pinning the tail below every parked
                // gate forever. An unexpected `PendingFreeFull` now falls
                // through to the loud arm below — a protocol bug, never a
                // deferral.
                Err(e) => return Err(e),
            }
        }
        // §4.7 heap-full posture: a pass that deferred for space is the
        // space class (counted per cycle, latched loud once); a pass that
        // deferred nothing clears the latch once the growth floor is
        // clear again (a full volume whose deletes only free record space
        // stays latched — honest: no new leaf can be minted).
        if deferred_for_space > 0 {
            self.heap_full_cycles.fetch_add(1, Ordering::Relaxed);
            self.enter_heap_full(&format!(
                "the flush pass deferred {deferred_for_space} node(s) whose SMO could not \
                 claim an extent"
            ));
        } else if self.heap_growth_floor_clear() {
            self.leave_heap_full("a flush pass deferred nothing and the growth floor is clear");
        }

        let t_flushed = std::time::Instant::now();
        // The pass's measured work by class — the cadence's live
        // projection's units (PR 13g, F-B1).
        self.note_flush_pass(sample);
        // ---- Dirty bitmap pages, stamped with this checkpoint's seq
        // (§4.7: the generation the ledger record names).
        let ckpt_seq = self.checkpoint_seq.load(Ordering::Acquire) + 1;
        self.allocator()
            .write_dirty_pages(
                self.device_path(),
                self.superblock().alloc_bitmap.start,
                ckpt_seq,
            )
            .await?;
        // PR 8 (design-symmetric-metadata §5.5.1): the DATA allocation
        // bitmap pages of every allocation lease HOMED on this volume —
        // the SAME order as the meta bitmap's, and for the same reason
        // (review round 1, Issue 1): the ledger record below advances the
        // tail past every delta journaled under the cycle's head `h`, so
        // the pages that cover them must be on the device — and durable
        // by barrier #1 — BEFORE that record lands. A no-op on a mount
        // holding no lease.
        self.write_data_alloc_pages(ckpt_seq).await?;
        let t_pages = std::time::Instant::now();

        // ---- Barrier #1: node appends + bitmap pages + every completed
        // journal write + any previously-written ledger record become
        // durable (the §4.6 pt 3 pending-reclaim drains inside).
        self.sync_device().await.map_err(KvError::Io)?;
        let landed_ns = crate::mono_core::monotonic_ns_u64();
        self.note_flush_ceiling(&had_dirty, landed_ns);
        self.note_checkpoint_barrier(t_pages.elapsed().as_nanos() as u64);
        // The landing wall the audit just measured against — from the age
        // decision that fired this cycle (PR 13h), else its start — folded
        // with the decision's lateness into the term the cadence trigger
        // anticipates (PR 13e, F-B1).
        self.note_checkpoint_cycle_term(cycle_started_ns, landed_ns);
        log::debug!(
            "checkpoint: cycle on {:?} pre-barrier wall {} ms = publish {} + flush {} ({} dirty \
             nodes: {} appended in {} ms, {} SMO'd writing {} images in {} ms) + pages {} + \
             barrier {}; anticipated term {} ms, projected {} ms (units {} µs/node, {} µs/image)",
            self.device_path(),
            cycle_started.elapsed().as_millis(),
            (t_collected - cycle_started).as_millis(),
            (t_flushed - t_collected).as_millis(),
            dirty_count,
            sample.nodes,
            sample.node_ns / 1_000_000,
            sample.smo_nodes,
            sample.images,
            sample.image_ns / 1_000_000,
            (t_pages - t_flushed).as_millis(),
            t_pages.elapsed().as_millis(),
            self.checkpoint_term_ms(),
            self.checkpoint_projected_ms(),
            self.checkpoint_node_unit_ns() / 1_000,
            self.checkpoint_image_unit_ns() / 1_000
        );

        // ---- The tail rule (module docs; §4.6 pt 2), plus the FIND-VS-A
        // dying-floor clamp: floors of nodes whose mappings LEFT the cache
        // since the last drain (SMO retires, evictions, publish-replaces)
        // are invisible to the live walk below, but their records' durable
        // *reachability* is only tied down by a ledger record written
        // after the departure — so they clamp this record's tail, keeping
        // every such record inside the replay window. Drained here; folded
        // back on any failure past this point (the ledger slot never
        // landed, so the next cycle must still respect the floor).
        let dying_floors = self.node_cache().take_dying_floors();
        // The slot-stamped LEAF floors, per slot: on a partitioned forest
        // a leaf's floor is a position in ITS lessee's ring and clamps
        // THAT region's tail (`appender_region_tails`); on every other
        // volume every slot is region 0's and they fold into `tail`.
        let dying_leaf_floors = self.node_cache().take_dying_leaf_floors();
        let partitioned = self.appenders().is_some_and(|a| a.is_partitioned());
        let inflight_start = self.journal_ring().min_inflight_start();
        let mut tail = h.min(inflight_start).min(dying_floors);
        // The forest's clamp: a guest root tree 0 does not yet name (a
        // deferred or failed publication above) keeps every record
        // applied under it in the window. A root floor is a position in
        // the ring the slot's records journal into — the manager's slots
        // clamp the ledger's tail, a leased slot's clamps ITS region's.
        let mut live_leaf_floors: std::collections::BTreeMap<super::record::ForestSlot, u64> =
            std::collections::BTreeMap::new();
        let unpublished = self.unpublished_root_floors();
        for (slot, f) in &unpublished {
            if !partitioned || self.region_of_slot(*slot) == 0 {
                tail = tail.min(*f);
            } else {
                let e = live_leaf_floors.entry(*slot).or_insert(u64::MAX);
                *e = (*e).min(*f);
            }
        }
        for (slot, f) in &dying_leaf_floors {
            if !partitioned || self.region_of_slot(*slot) == 0 {
                tail = tail.min(*f);
            }
        }
        let mut dirty_floor_min = u64::MAX;
        self.node_cache().for_each_node(|n| {
            let floor = n.dirty_floor();
            if floor == u64::MAX {
                return;
            }
            // A slot-stamped node of a LEASED slot — leaf or interior,
            // since its flips journal in its lessee's ring too — clamps
            // that region's tail, never the ledger's.
            if partitioned {
                if let Some(slot) = n.forest_slot() {
                    if self.region_of_slot(slot) != 0 {
                        let e = live_leaf_floors.entry(slot).or_insert(u64::MAX);
                        *e = (*e).min(floor);
                        return;
                    }
                }
            }
            dirty_floor_min = dirty_floor_min.min(floor);
            tail = tail.min(floor);
        });
        if tail < h {
            // The tail's attribution: which clamp bound this cycle's
            // record (a stuck tail is read off this line).
            log::debug!(
                "checkpoint tail {tail} < head {h}: inflight {inflight_start}, dying floors \
                 {dying_floors}, unpublished roots {:?}, dying leaf floors {:?}, dirty node \
                 floor {dirty_floor_min}",
                unpublished
                    .iter()
                    .filter(|(s, _)| !partitioned || self.region_of_slot(**s) == 0)
                    .collect::<Vec<_>>(),
                dying_leaf_floors
                    .iter()
                    .filter(|(s, _)| !partitioned || self.region_of_slot(**s) == 0)
                    .collect::<Vec<_>>()
            );
        }
        let region_tails = self.appender_region_tails(&dying_leaf_floors, &live_leaf_floors);

        // ---- The ledger record naming the synced roots + that tail.
        // Every mounted tree names a root — the three §4.2 user trees
        // plus, on a bit-8 volume, the §6.2 item-1 block-reference tree
        // (the ledger payload's `n_roots` has been variable-length since
        // PR K3, with ~19 roots of headroom inside the 4 KiB slot, so
        // this is a payload the pre-item-1 DECODER still parses — old
        // binaries refuse the volume at the superblock gate instead). On
        // a forest volume: tree 0 and the native slot tree only — guest
        // slot roots were published into tree 0 above.
        let tree_roots: Vec<TreeRoot> = self
            .ledger_root_trees()
            .into_iter()
            .map(|t| TreeRoot {
                tree_id: t.tree_id(),
                node_addr: t.root().addr,
                node_seq: t.root().seq,
            })
            .collect();
        let rec = LedgerRecord {
            seq: ckpt_seq,
            tree_roots,
            journal_tail_seq: tail,
            next_ino: self.next_ino(),
            alloc_bitmap_generation: ckpt_seq,
            // Captured AFTER the flush loop (whose SMOs mint): every seq
            // stamped into a frame that an extent freed ≤ this record can
            // carry is ≤ this watermark; mints after capture are covered
            // by replay floors (journaled SMO pointers) until the NEXT
            // record's watermark — see the extent-reuse chain argument
            // on [`LedgerRecord::node_seq_watermark`].
            // (Every tree shares ONE mint counter — `KvTree::open`/`create`
            // clone the same `Arc<AtomicU64>` — so any tree's snapshot is
            // the volume's watermark.)
            node_seq_watermark: self.all_trees()[0].node_seq_snapshot(),
            // PR VL5a (§5.5.1a): the membership stamp rides EVERY ledger
            // record of a slot-mapped volume (seeded from the mounted
            // record at open; installed by format/repair-set). Legacy
            // volumes carry None forever — their slots stay byte-
            // identical to the pre-VL5a encoding. PR VL5b: the LIVE
            // per-slot guest cursors are folded in per record, so every
            // checkpoint publishes the current mint watermarks (the
            // loom-modeled slot_cursor_core publication edge).
            membership_stamp: self.membership_stamp_for_ledger(),
            // Ruling D9 — BUILT, NOT STAMPED: this mount is the volume's
            // only appender (the D0 writer guard enforces it), so its
            // records stay in the pre-partition form and place themselves
            // with `slot = seq % 32`. The partitioned placement engages
            // when spec §6.9 S4 hands the backend an appender set.
            append_partition: None,
        };
        // TEST seam (Issue 31 — the crash window BEFORE the record): the
        // cycle stops with its flush pass's SMO records journaled and the
        // covering record unwritten; the floors go back like a failed
        // record's.
        if TEST_CHECKPOINT_HALT_BEFORE_LEDGER.load(Ordering::Relaxed) {
            self.node_cache().restore_dying_floors(dying_floors);
            self.node_cache()
                .restore_dying_leaf_floors(dying_leaf_floors);
            return Err(KvError::Corrupt(
                "test seam: checkpoint halted before its ledger record".to_string(),
            ));
        }
        if let Err(e) = write_ledger_slot(
            self.device_path(),
            self.superblock().root_ledger.start,
            &rec,
        )
        .await
        {
            // The covering ledger record never landed: the drained dying
            // floors are still uncovered — fold them back so the next
            // cycle's tail keeps clamping to them (FIND-VS-A).
            self.node_cache().restore_dying_floors(dying_floors);
            self.node_cache()
                .restore_dying_leaf_floors(dying_leaf_floors);
            return Err(e);
        }
        self.checkpoint_seq.store(ckpt_seq, Ordering::Release);
        self.last_ledger_tail.store(tail, Ordering::Release);
        // TEST seam (PR 8 review round 1, Issue 1 — the crash window the
        // ordering law exists for): the cycle stops the instant its ledger
        // record landed, so nothing the cycle writes AFTER the record
        // reaches the device — a kill −9 in that window, made deterministic.
        if TEST_CHECKPOINT_HALT_AFTER_LEDGER.load(Ordering::Relaxed) {
            return Err(KvError::Corrupt(
                "test seam: checkpoint halted after its ledger record landed".to_string(),
            ));
        }
        // The NEXT record's seq still stamps the historical retire tag in
        // free-record VALUES (byte-for-byte format compat) — the release
        // GATE itself rides the tail (design-smo-replay-currency §2-A).
        self.retire_seq.store(ckpt_seq + 1, Ordering::Release);
        // DUR-3: stamp the push with the barrier epoch AS OF the moment
        // the record's write completed. Barriers already in flight cannot
        // have flushed it, so `after_durable_barrier` releases this entry
        // only once a barrier that started later completes — one cycle of
        // extra latency in the deferred case, never a released watermark
        // over an undurable record.
        self.pending_reclaim
            .lock()
            .unwrap()
            .push((tail, self.barrier_push_epoch()));
        super::META_KV_CHECKPOINTS.fetch_add(1, Ordering::Relaxed);
        // Test seam: PARK here (the ledger record is on the device, the
        // pages are not yet written) until released — the window a
        // first touch of the manager's own lands in (Issue 2's pin).
        if TEST_CHECKPOINT_PARK_BEFORE_PAGES.load(Ordering::Relaxed) {
            TEST_CHECKPOINT_PARKED.fetch_add(1, Ordering::AcqRel);
            while TEST_CHECKPOINT_PARK_BEFORE_PAGES.load(Ordering::Relaxed) {
                let notified = TEST_CHECKPOINT_PARK_NOTIFY.notified();
                if !TEST_CHECKPOINT_PARK_BEFORE_PAGES.load(Ordering::Relaxed) {
                    break;
                }
                notified.await;
            }
        }
        // ---- The appender pages (§5.3.2): region 0's mirrors the record
        // just written; a declared region's names ITS tail — one page
        // write per region per checkpoint. A no-op on a flat volume. The
        // leaf floors a failed page write leaves uncovered are the
        // region's own: its page keeps its previous (lower) tail, so
        // nothing under them is reclaimed.
        if let Err(e) = self
            .write_appender_pages(tail, ckpt_seq, h, &region_tails, smo)
            .await
        {
            self.node_cache()
                .restore_dying_leaf_floors(dying_leaf_floors);
            return Err(e);
        }
        // §6.8 item 3's hold ledger: the record naming the new roots is on
        // the device — a reader's next poll adopts it — so this is the
        // instant every dereference committed before the cycle became
        // observable (one `ArcSwap` load on a mount with no reader plane).
        crate::free_grace::note_checkpoint_completed(cycle_started.elapsed());

        if barrier_now {
            // Make THIS record durable now: reclamation (reusable_upto,
            // pending-free, cache durable tail) advances before we
            // return — the R10 drain shape and the shutdown guarantee.
            self.sync_device().await.map_err(KvError::Io)?;

            // The §4.7 wedged-tail progress audit (design-smo-replay-
            // currency PR 4 clause b, CENTRALIZED here so EVERY barriered
            // cycle rides it — the pre-fix audit lived only on the
            // `run_maintenance` arms, so `checkpoint_now`/`checkpoint_past`/
            // shutdown callers livelocked silently on a wedged tail, the
            // P2 2026-07-26 §9 finding). Progress is either a parked
            // retirement released or the ledger tail advancing: since the
            // flush pass discharges every pre-cycle dirty floor (forced
            // retirements — the smo_replace progress theorem), a
            // barriered cycle whose tail does NOT advance while
            // retirements stay parked means the tail is pinned by
            // something no flush can discharge (a stuck in-flight
            // reservation) — bounded cycles, then the volume fails loud.
            // Never an unbounded retry loop, and — post-fix — never
            // latched by the RESOLVABLE pinned-floor shape (its floor
            // discharges in cycle 1, its tail advances in cycle 2).
            //
            // TWO CLASSES (2026-09-11, the inline-raise sweep's P1): a
            // cycle that deferred a node for `NoSpace` has its tail
            // pinned by a node it could not flush for want of an EXTENT
            // — a SPACE standstill, capacity, not corruption. It says
            // nothing about a wedge (the tail is legitimately pinned), so
            // it neither advances nor resets the wedge rung; it is
            // counted on `heap_full_cycles`, latched loud once on
            // `heap_full`, and clears when budget returns (a delete's
            // compaction, a released claim). The FAILED terminal is
            // reserved for the WEDGE class: nothing deferred for space,
            // retirements parked, no release, no tail advance.
            let pending_after = self.allocator().pending_count();
            if pending_after == 0 || pending_after < pending_before || tail > tail_before {
                self.pending_free_stalled_cycles.store(0, Ordering::Release);
            } else if deferred_for_grant > 0 {
                // A tail pinned by a node whose lessee is out of GRANT is
                // the manager dependency, not a wedge: bounded by
                // `manager_dependency_stall_bound_ms`, counted above.
                log::debug!(
                    "checkpoint: {deferred_for_grant} node(s) deferred for an exhausted extent \
                     grant (tail {tail}); the manager's refill owns the retry"
                );
            } else if deferred_for_space > 0 {
                log::debug!(
                    "checkpoint: space standstill — {deferred_for_space} node(s) deferred for \
                     NoSpace, {pending_after} retirements parked behind their floors (tail \
                     {tail}); not a wedge, the volume stays writable for deletes"
                );
            } else {
                let stalled = self
                    .pending_free_stalled_cycles
                    .fetch_add(1, Ordering::AcqRel)
                    + 1;
                if stalled >= PENDING_FREE_FORCE_CYCLES {
                    let msg = format!(
                        "pending-free retirements wedged ({pending_after} parked) and \
                         {stalled} consecutive barriered checkpoint cycles neither \
                         released one nor advanced the ledger tail (stuck at {tail}) — \
                         the durable tail is pinned by something no flush pass can \
                         discharge (§4.7 wedged-tail bound, design-smo-replay-currency \
                         PR 4 clause b)"
                    );
                    self.fail_stop_loud(&msg);
                    return Err(KvError::Corrupt(msg));
                }
            }
        }

        // ---- Appender rings (design-symmetric-metadata §5.3.2, PR 2): a
        // declared region whose ring stalled since its last growth decision
        // and is DRAINED grows by one segment. Evaluated HERE, after this
        // cycle's barrier advanced every ring's `reusable_upto` — the first
        // instant a ring whose last records this cycle covered reads as
        // drained (a cycle earlier it still held them; FIND-VS-A's one-
        // cycle lag applies to the region's tail exactly as to the
        // ledger's). Its own bitmap + page writes and barriers are inside.
        // A no-op on every unpartitioned volume.
        self.grow_stalled_regions().await?;

        // ---- The grant cadence (design-symmetric-metadata §5.3.3, PR 3):
        // fold each region's SMO rate, ship the extents this cycle's
        // barrier released as `ReturnExtents`, refill at 50 % consumption.
        // A no-op on every unpartitioned volume.
        if let Some(set) = self.appenders().filter(|a| a.is_partitioned()) {
            let now = crate::mono_core::monotonic_ns_u64();
            let last = set.cadence_last_ns.swap(now, Ordering::AcqRel);
            let cycle_ms = if last == 0 {
                0
            } else {
                now.saturating_sub(last) / 1_000_000
            };
            self.grant_cadence(cycle_ms).await?;
        }

        // ---- §4.6a (e) the heap-full sweep: while the volume is full (or
        // a previous sweep left a backlog — candidates it could not admit
        // for space, or a lap the budget cut), merge underfull adjacent
        // nodes — net −1 extent per merge, admitted at the compaction
        // floor, both retirements parked forced like the flush pass's own
        // compactions. Runs at the END of the cycle, after the durable
        // tail advanced: the lap's count phase then reads the SAME tail a
        // census taken after the cycle reads, which is what makes
        // `meta_kv_merge_candidates` exact (a tail advance only elides
        // more tombstones — it can only ADD candidates, so a count taken
        // before the advance under-reads). The wave cadence is unchanged
        // by the placement: a merge's flips sit above this cycle's `H`
        // either way, so its extents return at the NEXT cycle's barrier.
        // BOUNDED to one tick period per cycle (`merge_sweep_budget_ms` —
        // finding 49's drain law) and cursor-resumed, so a full-cache
        // volume's lap completes across cycles without stalling the
        // cadence its ring reclamation rides. Zero cost in steady state:
        // the posture gates it.
        if self.heap_full() || self.merge_backlog.load(Ordering::Acquire) {
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(self.merge_sweep_budget_ms);
            match self.run_merge_sweep(smo, true, Some(deadline)).await {
                Ok(report) => {
                    // The backlog stands while the recovery is not at its
                    // fixed point: a lap the floor cut, a lap the budget or
                    // the ring reserve cut (`refusal` ⇒ `!lap_complete`, the
                    // cursor parked at the refused node), or a lap that
                    // MERGED — its successors and the parents it shrank can
                    // pair again, and the posture alone would let the
                    // recovery stall the moment its first returned extents
                    // clear the growth floor. A completed lap with no merge
                    // and no refusal is the fixed point: the sweep stops.
                    let backlog = report.space_refused
                        || !report.lap_complete
                        || report.merges + report.root_collapses > 0;
                    self.merge_backlog.store(backlog, Ordering::Release);
                    if report.merges + report.root_collapses > 0 || backlog {
                        log::debug!(
                            "checkpoint: merge sweep on {:?} — {} merges ({} interior), {} \
                             collapses, lap_complete={}, candidates={}, backlog={backlog} \
                             (free={} promised={} pending-free={})",
                            self.device_path(),
                            report.merges,
                            report.interior_merges,
                            report.root_collapses,
                            report.lap_complete,
                            report.candidates,
                            self.allocator().free_extents(),
                            self.heap_promised(),
                            self.allocator().pending_count()
                        );
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
