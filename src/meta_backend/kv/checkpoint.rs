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
/// cycle's own writes, a device-time term the reader cannot derive and
/// that the published S5 staleness bound leaves unstated too (the
/// writer-advertised ceiling of adjudication item 4 is where a measured
/// cycle term belongs). On the shipped 50 ms flush this is 1,100 ms; on a
/// slow-flush venue the tick IS the landing term (5 s ⇒ 11,000 ms), which
/// the retired `staleness + skew` window (P + 1 s) never covered.
pub fn checkpoint_landing_ceiling_ms(flush_interval_ms: u64) -> u64 {
    CHECKPOINT_MAX_AGE_MS as u64 + 2 * checkpoint_tick_period_ms(flush_interval_ms)
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
/// audit is centralized in [`KvMetaBackend::checkpoint_cycle`] (P2
/// 2026-07-26 §9: the pre-fix audit lived only on the maintenance arms,
/// so direct-cycle callers livelocked silently). A healthy convergence
/// needs at most a couple of cycles (the first discharges dying floors,
/// the second's tail covers the parked frees), and since the §4.7
/// cycle-break the RESOLVABLE pinned-floor shape always converges that
/// way — what remains for the terminal is a tail pinned by something no
/// flush pass can discharge (a stuck in-flight reservation).
const PENDING_FREE_FORCE_CYCLES: u64 = 8;

/// Spawn the per-volume checkpoint/writeback task (called by
/// `KvMetaBackend::open`). The task holds a `Weak` backend reference —
/// dropping the backend without `shutdown` reaps it on its next tick (the
/// v2 flusher's sentinel discipline; pinned by
/// `tests/dismount_teardown_tests.rs`) — plus an owned liveness token the
/// teardown tests probe through `checkpoint_alive_probe`.
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
        if be.is_shutting_down() || be.all_trees().into_iter().any(|t| t.maintenance_pending()) {
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
    for tree in be.all_trees() {
        loop {
            let r = tree.run_maintenance_until(&mut smo, deadline).await;
            match r {
                Ok(_) => break,
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
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// One task tick: maintenance (within `drain_deadline`) → deferred flush
/// barrier → checkpoint when due. `final_cycle` (shutdown) drains
/// in-flight commits first and forces a full cycle with an immediate
/// post-ledger barrier, leaving `tail == head` — an empty replay window
/// for the next mount. `elastic_ceiling` is the freed-offset composite's
/// checkpoint ceiling in force (`None` = the shipped `CHECKPOINT_MAX_AGE_MS`).
async fn tick(
    be: &Arc<KvMetaBackend>,
    last_checkpoint: &mut std::time::Instant,
    final_cycle: bool,
    drain_deadline: Option<std::time::Instant>,
    elastic_ceiling: Option<u64>,
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
        for _ in 0..64 {
            let head = be.journal_ring().core().head();
            be.journal_ring().wait_completed_upto(head).await;
            if be.journal_ring().core().head() == head {
                break;
            }
        }
    }
    let mut smo = be.smo.lock().await;

    // 1. Threshold maintenance (appends + SMOs, serialized here — §4.6),
    //    within the drain budget (finding 49: the checkpoint decision
    //    below must not sit behind an unbounded drain under a storm).
    //    Reserve exhaustion runs a drain cycle and retries; a pending-
    //    free-FIFO refusal (SMO admission headroom, design-smo-replay-
    //    currency PR 4 clause a) forces the same cycle — its flush pass
    //    discharges the pinning floor (the §4.7 cycle-break) and its
    //    centralized progress audit (clause b) bounds genuine wedges.
    for tree in be.all_trees() {
        loop {
            match tree.run_maintenance_until(&mut smo, drain_deadline).await {
                Ok(_) => break,
                Err(KvError::JournalReserveExhausted { .. } | KvError::PendingFreeFull { .. }) => {
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

    // 2. The deferred-mode flush barrier (the v2 flusher tick). Also
    //    drains the §4.6 pt 3 pending-reclaim for previously-written
    //    ledger records.
    if be.take_needs_flush() {
        crate::fuse_client::METRICS
            .meta_flush_deferred
            .fetch_add(1, Ordering::Relaxed);
        be.sync_device().await.map_err(KvError::Io)?;
    }

    // 3. Checkpoint decision (§4.6 pt 2): cadence, journal distance,
    //    dirty-node cap, shutdown.
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
    let mut dirty_nodes = 0u64;
    be.node_cache().for_each_node(|n| {
        if n.dirty_floor() != u64::MAX {
            dirty_nodes += 1;
        }
    });
    // The freed-offset composite's ceiling in force replaces the constant
    // while a reader ask is live (`P/2` — every reader pass finds a new
    // root); `None` is the shipped decision verbatim. Compared against the
    // ELAPSED time, so a ceiling that tightens mid-interval fires at once
    // — what lets an advertised ceiling be a promise about commits that
    // preceded the grant, not only about the ones that follow it.
    let ceiling_ms = elastic_ceiling.map_or(CHECKPOINT_MAX_AGE_MS, u128::from);
    let due = final_cycle
        || ring_pressure
        // The cap is resolved ONCE at open (`KvMetaBackend::dirty_node_cap`
        // — the backend-knob convention): this branch runs on every 50 ms
        // cadence tick, and re-deriving here (env `CString`s + cgroup/
        // sysinfo probes in `resolve_budget_now`) was an allocation stream
        // that broke the op-economy allocation-free contract (2026-08-02).
        || dirty_nodes > be.dirty_node_cap()
        || last_checkpoint.elapsed().as_millis() >= ceiling_ms;
    // A declared region's uncovered ring counts as "something to cover"
    // exactly as ring 0's `distance > 0` does (its tail lands a cycle
    // after its flush, like the ledger's).
    let regions_uncovered = be.appenders().is_some_and(|a| a.rings_uncovered());
    if due && (final_cycle || dirty_nodes > 0 || distance > 0 || regions_uncovered) {
        // Immediate post-ledger barrier under pressure or at shutdown:
        // reclamation must not lag a cycle when parkers wait on it.
        if region_pressure {
            if let Some(a) = be.appenders() {
                a.pressure_cycles.fetch_add(1, Ordering::Relaxed);
            }
        }
        be.checkpoint_cycle(&mut smo, ring_pressure || final_cycle)
            .await?;
        *last_checkpoint = std::time::Instant::now();
        if elastic_ceiling.is_some() {
            crate::free_grace::note_elastic_checkpoint_cycle();
        }
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
        for _ in 0..16 {
            let core = be.journal_ring().core();
            if core.head() == core.reusable_upto() {
                return Ok(());
            }
            be.checkpoint_cycle(&mut smo, true).await?;
            *last_checkpoint = std::time::Instant::now();
        }
        let core = be.journal_ring().core();
        if core.head() != core.reusable_upto() {
            log::warn!(
                "shutdown checkpoint did not converge to an empty replay window \
                 (head={}, reusable_upto={}): the next mount will replay the residue \
                 (sound, but the shutdown tail==head guarantee was missed)",
                core.head(),
                core.reusable_upto()
            );
        }
    }
    Ok(())
}

impl KvMetaBackend {
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
        let cycle_started = std::time::Instant::now();
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
        let mut had_dirty: Vec<(u32, u64)> = Vec::new();
        let region_aware = self.appenders().is_some();
        self.node_cache().for_each_node(|n| {
            if n.dirty_floor() != u64::MAX && !n.state().is_superseded() {
                if region_aware && n.level() == 0 {
                    let r = self.region_of_node(n);
                    let since = n.dirty_since_ns();
                    match had_dirty.iter_mut().find(|(id, _)| *id == r) {
                        Some((_, oldest)) => *oldest = (*oldest).min(since),
                        None => had_dirty.push((r, since)),
                    }
                }
                dirty.push(Arc::clone(n));
            }
        });
        // Nodes this pass could not flush because the allocator answered
        // `NoSpace` — the wedged-tail audit's class discriminator below.
        let mut deferred_for_space = 0u64;
        for node in dirty {
            let addr = node.addr();
            let tree = self.tree_of_node(&node)?;
            match tree.checkpoint_flush_node(smo, addr).await {
                Ok(()) => {}
                Err(KvError::JournalReserveExhausted { needed }) => {
                    log::debug!(
                        "checkpoint: SMO reserve exhausted ({needed} B) at node {addr:#x}; \
                         deferred to the next cycle"
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

        // ---- Barrier #1: node appends + bitmap pages + every completed
        // journal write + any previously-written ledger record become
        // durable (the §4.6 pt 3 pending-reclaim drains inside).
        self.sync_device().await.map_err(KvError::Io)?;
        self.note_flush_ceiling(&had_dirty, crate::mono_core::monotonic_ns_u64());

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
        let mut tail = h
            .min(self.journal_ring().min_inflight_start())
            .min(dying_floors)
            // The forest's clamp: a guest root tree 0 does not yet name
            // (a deferred or failed publication above) keeps every record
            // applied under it in the window — `u64::MAX` when none.
            .min(self.unpublished_root_floor());
        let mut live_leaf_floors: std::collections::BTreeMap<super::record::ForestSlot, u64> =
            std::collections::BTreeMap::new();
        for (slot, f) in &dying_leaf_floors {
            if !partitioned || self.region_of_slot(*slot) == 0 {
                tail = tail.min(*f);
            }
        }
        self.node_cache().for_each_node(|n| {
            let floor = n.dirty_floor();
            if floor == u64::MAX {
                return;
            }
            if partitioned && n.level() == 0 {
                if let Some(slot) = n.forest_slot() {
                    if self.region_of_slot(slot) != 0 {
                        let e = live_leaf_floors.entry(slot).or_insert(u64::MAX);
                        *e = (*e).min(floor);
                        return;
                    }
                }
            }
            tail = tail.min(floor);
        });
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
        // ---- The appender pages (§5.3.2): region 0's mirrors the record
        // just written; a declared region's names ITS tail — one page
        // write per region per checkpoint. A no-op on a flat volume. The
        // leaf floors a failed page write leaves uncovered are the
        // region's own: its page keeps its previous (lower) tail, so
        // nothing under them is reclaimed.
        if let Err(e) = self
            .write_appender_pages(tail, ckpt_seq, h, &region_tails)
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
