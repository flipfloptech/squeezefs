//! The structure-modification task: compaction, split, root grow, the
//! writeback/flush steps, and the journal seam they reserve from (charter
//! §5.3 "SMOs": build successors from a frozen snapshot (no locks) →
//! barrier the images → parent-then-child locks → re-freeze the
//! build-window delta into the successor → swap the cache mapping →
//! RESERVE the interior-pointer + alloc/free records inside the lock
//! window from the reserve → release → write the entry; root swaps journal
//! no pointer record and pin a dying floor; §5.3 "Writeback"; §2 lock order
//! 4b — interior locks belong to this serialized task, parent-then-child,
//! never across device I/O or ring-space waits; survey
//! `docs/porting/kv_tree_cache_commit_survey.md` §2.9, §2.11, §3.4, §7 row
//! 7; the reference's `tree.rs` `SmoContext`/`smo_replace`).
//!
//! Every SMO entry point takes `*SmoContext` — ONE per volume, owned by the
//! checkpoint/SMO task — so SMO-vs-SMO races are structurally impossible;
//! the only locks it ever holds are node locks, briefly, RAM-only.
//!
//! ## `smoReplace`, the P0–P5 order (`§3.4`)
//!
//! | phase | locks | obligation |
//! |---|---|---|
//! | P0 freeze the source | the node's, briefly (the caller's) | — |
//! | P1 build images | none | successors written to FRESH extents; any error releases every claim |
//! | P2 barrier + admit | none | images durable BEFORE any pointer record can exist; `PendingFreeFull` / `JournalReserveExhausted` abort clean |
//! | P3 lock window | parent, then child | reserve INSIDE; leftovers moved with exact per-record floors; **publish → flip → retire**; a root swap pins a dying floor at `res.start` |
//! | P4 write the entry | none | after release; a failed write is the §5.2 crash-equivalent hole, loud not fatal |
//! | P5 park the old extent | none | gated on the free record's own POSITION; forced ⇒ the unbounded overflow |
//!
//! ## Publish → flip → retire
//!
//! Inside P3 the successors are published FIRST, the route flipped SECOND
//! and the old object retired LAST. Retiring before the flip opened a
//! reader-visible window in which the parent's current snapshot routed to
//! an already-retired extent; with the SMO task descheduled mid-window
//! under storm load readers burned the whole restart budget on one reason
//! (`[0,0,0,256,0]`, all child-retired — the K7 1M-storm exhaustion). With
//! the flip first, stragglers on the old route still resolve the
//! still-mapped pre-SMO object (readers get its intact snapshot, writers
//! fail revalidation and retry) and the retired state begins only once the
//! new route is reader-visible. Eviction cannot reopen the window:
//! `tryEvict` is a clean-only CAS and the superseded object is never clean.

const std = @import("std");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const key = @import("key.zig");
const rec = @import("record.zig");
const node = @import("node.zig");
const node_cache = @import("node_cache.zig");
const node_seq = @import("node_seq.zig");
const snapshot = @import("node_snapshot.zig");
const journal = @import("journal.zig");
const jc = @import("journal_core.zig");
const ledger = @import("ledger.zig");
const alloc_ext = @import("alloc_ext.zig");
const heap_law = @import("heap_law.zig");
const checkpoint_cycle = @import("checkpoint_cycle.zig");
const tree = @import("tree.zig");
const leaf_merge = @import("leaf_merge.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
/// This file's namespace (the harness inside `TestVolume` names the
/// module-level SMO drivers past its own methods of the same name).
const smo = @This();

pub const KvTree = tree.KvTree;
pub const CachedNode = node_cache.CachedNode;
pub const NodeCache = node_cache.NodeCache;
pub const OwnedRec = node_cache.OwnedRec;
pub const Record = rec.Record;
pub const RecordRef = rec.RecordRef;
pub const Reservation = jc.Reservation;
pub const Admission = jc.Admission;

// ---------------------------------------------------------------------------
// Observability (survey §5.5 names; `meta_kv_root_grows` is the one new
// face — the reference folds a root grow into `splits`).
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// 1:1 CoW rewrites (log fold). `compactions : appends` past ≈ 1:8 ⇒
    /// node_size/cadence mistuned.
    pub var meta_kv_node_compactions: counters.ShardedCounter = .{};
    /// Node splits (2-way and wider), the root's included.
    pub var meta_kv_node_splits: counters.ShardedCounter = .{};
    /// Root splits that minted a new root above the parts (height + 1).
    pub var meta_kv_root_grows: counters.ShardedCounter = .{};
    /// An SMO entry wider than the checkpoint class's kept claim (0 on a
    /// healthy mount — see `admitSmoEntry`).
    pub var meta_kv_smo_entries_over_keep: counters.ShardedCounter = .{};
    /// SMO entry writes that failed after the swap: the §5.2 crash-
    /// equivalent hole, loud and counted, never fatal.
    pub var meta_kv_smo_entry_write_failures: counters.ShardedCounter = .{};
    /// Pre-lock image barriers that failed: the SMO is abandoned whole
    /// (nothing published) and the failure counts on the FAILED latch.
    pub var meta_kv_smo_barrier_failures: counters.ShardedCounter = .{};
    /// A dead grant's image extent retired twice under a recovery's hold
    /// (must-stay-0: the held bit stays set, no tree can claim it back).
    pub var meta_kv_retire_hold_breaches: counters.Tripwire = .init("meta_kv_retire_hold_breaches");
    /// A grant-fed context retired an image its grant does not hold (0003
    /// §AD.1: a lease's custody move carries every image of a leased tree
    /// into the lessee's grant) — kept allocated, never recycled into the
    /// pool: a leaked extent, never one two custodians hand out.
    pub var extent_grant_foreign_retirements: counters.Tripwire = .init("extent_grant_foreign_retirements");
};

pub const Error = tree.Error || node_cache.FreezeError || node_cache.AppendError || journal.Error || error{
    /// The heap cannot claim an image extent (internal class: a genuinely
    /// exhausted heap; the merge's compaction-floor refusal).
    NoSpace,
    /// The checkpoint-class ring reserve refused the SMO's entry: the
    /// caller runs a minimal drain and retries (`SmoContext.last_refusal`).
    JournalReserveExhausted,
    /// The pending-free FIFO has no room for this SMO's retirement(s) under
    /// the threshold posture: the caller forces a cycle and retries.
    PendingFreeFull,
    /// The images-before-pointers barrier failed: the SMO is abandoned
    /// whole (its fresh extents released, nothing published) and the node
    /// stays dirty for the retry; counts toward the FAILED latch.
    BarrierFailed,
};

// ---------------------------------------------------------------------------
// The journal seam.
// ---------------------------------------------------------------------------

/// The device binding of the checkpoint-class entry path (SMO entries, the
/// rollback's compensation): `write` lands one entry's aligned runs on the
/// device and returns once they LANDED — on a mounted volume a STRUCTURAL
/// submission through the journal lane (`journal_lane.submitStructural`:
/// the same aligned-run path as a user window, no group, no ack, no
/// barrier; a lane-thread caller parks as a fiber and the lane's own CQE
/// answers it, so a compensation inside a verdict never waits on itself);
/// `completed` runs right after the reservation completes (the lane's kick
/// — a stage-B prefix wait may sit behind a reservation the lane's groups
/// never saw).
pub const DeviceHooks = struct {
    ctx: *anyopaque,
    write: *const fn (ctx: *anyopaque, ops: []const journal.WriteOp) error{Io}!void,
    completed: *const fn (ctx: *anyopaque) void,
};

/// The SMO task's journal: a ring image + the reservation core + the
/// in-flight registry, admission from the CHECKPOINT class, the entry write
/// (`journal.buildEntryWrite` landed on the image, or on the device through
/// `DeviceHooks`), and the barrier hook the device binding installs (the
/// in-RAM image is durable by construction: the default hook is a counted
/// no-op).
pub const SmoJournal = struct {
    gpa: Allocator,
    layout: journal.RingLayout,
    geo: jc.CoreGeometry,
    core: *jc.JournalCore,
    reg: *jc.InflightRegistry,
    /// The ring extent's image; `layout.base` is its first byte's offset.
    /// Empty on a device-bound journal (`device` set): the device is the
    /// image.
    image: []u8,
    /// The appender every page header of this ring names (the fixed ring's
    /// is appender 0 until rung 6 hands rings to joiners).
    appender_id: u32 = journal.FIXED_RING_APPENDER,
    /// The historical §4.7 retire tag stamped into free-record VALUES
    /// (byte-for-byte format compat; the gate is the record's position).
    retire_seq: std.atomic.Value(u64) = .init(0),
    /// The device barrier (the backend binds the metadata device's
    /// `fdatasync`); null on the in-RAM image, durable by construction.
    barrier_hook: ?checkpoint_cycle.BarrierSink = null,
    /// The volume's FAILED latch: the barrier's class counts here (the same
    /// run the checkpointer's `syncDevice` feeds); null in the standalone tests.
    latch: ?*journal.FailureLatch = null,
    device: ?DeviceHooks = null,
    barriers: std.atomic.Value(u64) = .init(0),
    stats: journal.RingStats = .{},
    /// The last SMO entry this seam committed (the tear harness's target).
    last_entry: ?Reservation = null,
    /// Test seam: fail the next N SMO entry writes after their reservation
    /// — the §5.2 pt 4 crash-equivalent hole (`writeEntryAfterRelease`).
    test_fail_writes: std.atomic.Value(u64) = .init(0),

    /// Successor images durable BEFORE any pointer record to them can
    /// exist in the ring: a replayed pointer must never route to a torn
    /// image. A failure is the SMO's: counted, on the latch's barrier
    /// class, and returned so the caller abandons the build (charter §1.8:
    /// never a silent fallback to publishing pointers to unbarriered images).
    pub fn barrier(self: *SmoJournal) error{BarrierFailed}!void {
        if (self.barrier_hook) |h| h.barrier(h.ctx) catch {
            stats.meta_kv_smo_barrier_failures.inc();
            if (self.latch) |l| _ = l.noteFailure(.barrier);
            return error.BarrierFailed;
        };
        if (self.latch) |l| l.noteSuccess(.barrier);
        _ = self.barriers.fetchAdd(1, .monotonic);
    }

    /// Transfer admitted budget to the head and register the reservation
    /// — INSIDE the lock window.
    pub fn reserve(self: *SmoJournal, adm: Admission) Allocator.Error!jc.InflightRegistry.Registered {
        return self.reg.reserveRegistered(self.core, adm);
    }

    /// Write one entry after release and complete its reservation on
    /// either outcome (an unwritten range still completes for the
    /// watermark; replay's checksum walk drops it). A device write failure
    /// is `error.Corrupt` to the caller's crash-equivalent-hole arm.
    pub fn commitEntry(self: *SmoJournal, res: Reservation, frames: []const journal.RecordFrame) journal.Error!void {
        const written = self.writeEntry(res, frames);
        self.reg.complete(self.core, res);
        if (self.device) |d| d.completed(d.ctx);
        return written;
    }

    fn writeEntry(self: *SmoJournal, res: Reservation, frames: []const journal.RecordFrame) journal.Error!void {
        var img = try journal.buildEntryWrite(self.gpa, self.layout, self.geo, self.appender_id, res, frames);
        defer img.deinit();
        if (self.device) |d| {
            d.write(d.ctx, img.ops) catch return error.Corrupt;
        } else {
            journal.applyOps(self.image, self.layout.base, img.ops);
        }
        self.stats.noteWritten(img.acct, 1);
        self.last_entry = res;
    }

    /// Abandon a reservation whose write did not happen: it still completes
    /// for the watermark (replay's checksum walk drops the hole).
    pub fn abandonEntry(self: *SmoJournal, res: Reservation) void {
        self.reg.complete(self.core, res);
        if (self.device) |d| d.completed(d.ctx);
    }

    fn testFailArmed(self: *SmoJournal) bool {
        var v = self.test_fail_writes.load(.monotonic);
        while (v > 0) {
            v = self.test_fail_writes.cmpxchgWeak(v, v - 1, .monotonic, .monotonic) orelse return true;
        }
        return false;
    }
};

// ---------------------------------------------------------------------------
// The serialized context.
// ---------------------------------------------------------------------------

/// Why the last SMO was refused (Zig errors carry no payload; the sweep's
/// `SweepRefusal` and the maintenance arms read it).
pub const Refusal = union(enum) {
    none,
    journal_reserve: struct { needed: u64 },
    pending_free_full: struct { pending: u64 },
};

/// A dead appender's grant under recovery (charter §5.9 steps 4–7b;
/// decision 0003 §U.1): every image extent of that grant an SMO on this
/// volume retires while the hold is installed is HELD here — its bit stays
/// set, it never enters the coverage clock — so the extent leaves the
/// grant by ONE path: the recovery's tree-0 step names it gone from the
/// record, then the table release parks it in the FIFO (`SmoContext.
/// freePending` consults the hold first). A rollback drops the hold and
/// the extents stay allocated: the restored tree may still reach them.
/// Installed, read and removed under the volume's SMO lock — the FIFO's
/// own producer law — so no retirement slips between a reading and a
/// removal.
pub const RetireHold = struct {
    gpa: Allocator,
    /// The grant's runs, sorted by start, disjoint.
    runs: []const Run,
    /// The held extents; capacity = the runs' total, reserved at `init`:
    /// a held extent's bit stays set, so it is never claimed into a tree
    /// again and is retired at most once — `take` never allocates (an SMO
    /// allocates nothing after its swap).
    held: std.ArrayList(u64),

    pub const Run = struct { start: u64, len: u64 };

    pub fn init(gpa: Allocator, runs: []const Run) Allocator.Error!RetireHold {
        const owned = try gpa.dupe(Run, runs);
        errdefer gpa.free(owned);
        std.mem.sort(Run, owned, {}, struct {
            fn lt(_: void, a: Run, b: Run) bool {
                return a.start < b.start;
            }
        }.lt);
        var total: usize = 0;
        for (owned) |r| total += @intCast(r.len);
        var held: std.ArrayList(u64) = .empty;
        try held.ensureTotalCapacity(gpa, total);
        return .{ .gpa = gpa, .runs = owned, .held = held };
    }

    pub fn deinit(self: *RetireHold) void {
        self.held.deinit(self.gpa);
        self.gpa.free(self.runs);
        self.* = undefined;
    }

    pub fn covers(self: *const RetireHold, extent: u64) bool {
        var lo: usize = 0;
        var hi: usize = self.runs.len;
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            const r = self.runs[mid];
            if (extent < r.start) {
                hi = mid;
            } else if (extent >= r.start + r.len) {
                lo = mid + 1;
            } else return true;
        }
        return false;
    }

    /// The SMO's retirement of `extent`: held iff the grant covers it.
    /// A grant extent retired twice is a protocol breach (the bit stayed
    /// set, so no tree could have claimed it back): counted, and the
    /// second retirement parks in the FIFO as any other would.
    fn take(self: *RetireHold, extent: u64) bool {
        if (!self.covers(extent)) return false;
        if (std.mem.indexOfScalar(u64, self.held.items, extent) != null or self.held.items.len == self.held.capacity) {
            stats.meta_kv_retire_hold_breaches.fire("a dead grant's image extent retired twice under the recovery's hold");
            return false;
        }
        self.held.appendAssumeCapacity(extent);
        return true;
    }
};

/// Whether a grant-fed context's grant holds `extent` (the JOINED
/// appender's census of its grant: `sym/joined.zig` `grantHolds`).
pub const GrantHolds = struct {
    ctx: *anyopaque,
    holds: *const fn (ctx: *anyopaque, extent: u64) bool,
};

/// The serialized SMO execution context: ONE per volume, passed `*` into
/// every SMO entry point by the task that owns it.
pub const SmoContext = struct {
    alloc: *const alloc_ext.ExtentAllocator,
    /// null = the standalone shape (minted seqs, no journaling, no barrier);
    /// the production shape reserves from the checkpoint class.
    journal: ?*SmoJournal = null,
    /// The opportunistic merges' yield predicate (the heap law's hold).
    space_retry: ?*const heap_law.SpaceRetryHold = null,
    /// Fresh images this context's SMOs wrote — THIS volume's (the flush
    /// pass's per-image unit reads its delta per node).
    images_written: u64 = 0,
    /// The next SMO entry's admission may take the class's kept space:
    /// set by the flush pass for the OLDEST dirty node of the ring — the
    /// one node whose flush moves the tail — and consumed by that node's
    /// own SMO; every other checkpoint-class admission keeps.
    keep_exempt: bool = false,
    last_refusal: Refusal = .none,
    /// The allocator is a JOINED appender's grant mirror: an exhausted
    /// claim is the manager dependency (`GrantExhausted`, EAGAIN-class,
    /// refilled over the wire), never the heap's `NoSpace`.
    grant_fed: bool = false,
    /// A recovery's hold over a dead grant's retirements (`RetireHold`);
    /// set and cleared under the SMO lock.
    retire_hold: ?*RetireHold = null,
    /// A JOINED appender's grant (the mirror's free set is its pool): a
    /// retired image recycles only if the grant holds it.
    grant_holds: ?GrantHolds = null,

    pub fn init(alloc: *const alloc_ext.ExtentAllocator) SmoContext {
        return .{ .alloc = alloc };
    }

    pub fn withJournal(alloc: *const alloc_ext.ExtentAllocator, j: *SmoJournal) SmoContext {
        return .{ .alloc = alloc, .journal = j };
    }

    pub fn imagesWritten(self: *const SmoContext) u64 {
        return self.images_written;
    }

    pub fn setKeepExempt(self: *SmoContext, exempt: bool) void {
        self.keep_exempt = exempt;
    }

    /// The SMO entry's checkpoint-class admission: keeps `smoKeep` of class
    /// space unless the flush pass exempted this node as its ring's oldest
    /// — the liveness law: the class can never be consumed below what the
    /// tail-pinning node needs at the next cycle's start.
    ///
    /// The keep's bound is a CENSUS here, never a refusal: the keep holds
    /// the widest entry the tail-pinning node can need (a quarter of the
    /// class), and what makes a wider entry unreachable is the fold's
    /// arithmetic — a fold partitions one node's log (≤ `foldCapacity`)
    /// plus its frozen delta, whose records are journaled and UNCOVERED
    /// (the node's floor keeps the tail below them, so `delta ≤ ring_len`);
    /// `parts ≤ ⌈(foldCapacity + ring_len) / splitPartCapacity⌉`, one
    /// pointer and one alloc record each, fits `smoKeep − maxPad` at every
    /// format geometry (`smoKeepCoversWidestEntry`). An entry past the keep
    /// is admitted like any other and counted, loud once — the first rail
    /// refused a 62-part split's 4.6 KiB entry (a 2.3 MiB fold under a fill
    /// storm) and failed every later checkpoint of the volume.
    pub fn admitSmoEntry(self: *SmoContext, j: *SmoJournal, len: u64) ?Admission {
        const geo = j.geo;
        if (len +| geo.maxPad() > geo.smoKeep()) {
            stats.meta_kv_smo_entries_over_keep.inc();
            if (stats.meta_kv_smo_entries_over_keep.load() == 1) {
                log.warn("an SMO entry of {d} B (+ {d} B pad slack) exceeds the checkpoint class's kept claim of {d} B — a split wider than the fold's bound on this ring; admitted without the keep's guarantee", .{ len, geo.maxPad(), geo.smoKeep() });
            }
        }
        return if (self.keep_exempt) j.core.tryAdmit(len, .checkpoint) else j.core.tryAdmitKeeping(len, .checkpoint, geo.smoKeep());
    }

    /// Claim one image extent (internal class — may draw the reserve).
    pub fn claimInternal(self: *const SmoContext) error{NoSpace}!u64 {
        return self.alloc.claimInternal();
    }

    /// Return a claimed-but-unpublished extent (no checkpoint references
    /// it: straight back to the pool).
    pub fn releaseUnpublished(self: *const SmoContext, extent: u64) void {
        self.alloc.releaseUnpublished(extent);
    }

    /// Park a retired image's extent gated on `gate_seq` — the free
    /// record's POSITION in the ring. `forced` (the flush pass) parks
    /// unconditionally through the unbounded overflow (the §4.7 cycle-
    /// break); otherwise the pre-swap headroom check makes a refusal here
    /// unreachable on the serialized task (a second producer would be a
    /// protocol bug, surfaced typed). A recovery's hold takes a dead
    /// grant's extent first (`RetireHold`).
    pub fn freePending(self: *SmoContext, extent: u64, gate_seq: u64, forced: bool) Error!void {
        if (self.retire_hold) |h| if (h.take(extent)) return;
        if (self.grant_holds) |g| if (!g.holds(g.ctx, extent)) {
            stats.extent_grant_foreign_retirements.fire("a joiner's SMO retired an image its grant does not hold — kept allocated, never recycled into its pool");
            return;
        };
        if (forced) return self.alloc.freePendingForced(extent, gate_seq);
        self.alloc.freePending(extent, gate_seq) catch {
            self.last_refusal = .{ .pending_free_full = .{ .pending = self.alloc.pendingCount() } };
            return error.PendingFreeFull;
        };
    }

    /// The §4.7 at-cap admission valve (producer-side headroom; sound for
    /// the FIFO's only producer).
    pub fn pendingHasRoomFor(self: *const SmoContext, n: u64) bool {
        return if (n <= 1) self.alloc.pendingHasRoom() else self.alloc.pendingHasRoomFor(n);
    }

    pub fn pendingCount(self: *const SmoContext) u64 {
        return self.alloc.pendingCount();
    }

    pub fn refusePendingFull(self: *SmoContext) Error {
        self.last_refusal = .{ .pending_free_full = .{ .pending = self.pendingCount() } };
        return error.PendingFreeFull;
    }

    pub fn refuseReserve(self: *SmoContext, needed: u64) Error {
        self.last_refusal = .{ .journal_reserve = .{ .needed = needed } };
        return error.JournalReserveExhausted;
    }
};

/// Counts of the maintenance work one pass performed (tests assert against
/// these; the global counters aggregate across passes).
pub const MaintenanceOutcome = struct {
    /// Frozen deltas appended in place.
    appends: u64 = 0,
    compactions: u64 = 0,
    splits: u64 = 0,
    root_grows: u64 = 0,
    /// §4.6a sibling merges (two nodes → one).
    merges: u64 = 0,
    /// The subset of `merges` at level ≥ 1 (interior recursion).
    interior_merges: u64 = 0,
    /// Root collapses (height − 1).
    root_collapses: u64 = 0,

    pub fn add(self: *MaintenanceOutcome, o: MaintenanceOutcome) void {
        self.appends += o.appends;
        self.compactions += o.compactions;
        self.splits += o.splits;
        self.root_grows += o.root_grows;
        self.merges += o.merges;
        self.interior_merges += o.interior_merges;
        self.root_collapses += o.root_collapses;
    }
};

// ---------------------------------------------------------------------------
// Derivations with their ties.
// ---------------------------------------------------------------------------

/// Interior fanout of a layout for `key_len`-byte separators at the split
/// fill: one separator record is `15 + key_len + 16` bytes. 8-byte inode
/// keys: 1,180 at 64 KiB, 4,961 at 256 KiB (the charter's "≈ 7,000" is the
/// FULL fold's `foldCapacity / 39 = 6,615`); `fanout² ≫ leaves` is what
/// puts 100 M records at depth ≤ 3 with < 3 MiB of interior nodes.
pub fn interiorFanout(layout: node.NodeLayout, key_len: usize) usize {
    return layout.splitPartCapacity() / (rec.RECORD_HEADER_LEN + key_len + key.InteriorValue.LEN);
}

/// The widest SMO entry a node of `layout` can need on a ring of
/// `ring_len` logical bytes: `parts = ⌈(foldCapacity + ring_len) /
/// splitPartCapacity⌉` pointer records under the widest journal key (a
/// slot tree's `slot ‖ KEY_SPACE_MAX`) + alloc records of 8-byte keys,
/// plus the free record, plus the entry header — the bound
/// `admitSmoEntry`'s keep is sized against.
pub fn widestSmoEntryLen(layout: node.NodeLayout, ring_len: u64) u64 {
    const parts: u64 = @intCast((layout.foldCapacity() + ring_len + layout.splitPartCapacity() - 1) / layout.splitPartCapacity());
    const ptr = journal.recordFrameLen(key.KEY_BUF_CAP, key.InteriorValue.LEN);
    const alloc = journal.recordFrameLen(alloc_ext.extent_key_len, 1);
    const free = journal.recordFrameLen(alloc_ext.extent_key_len, 9);
    return journal.ENTRY_HDR_LEN + parts * (ptr + alloc) + free;
}

/// `smoKeep − maxPad ≥ widestSmoEntryLen` — the keep covers the widest
/// entry at this geometry.
pub fn smoKeepCoversWidestEntry(layout: node.NodeLayout, geo: jc.CoreGeometry) bool {
    return geo.smoKeep() - geo.maxPad() >= widestSmoEntryLen(layout, geo.logicalLen());
}

// ---------------------------------------------------------------------------
// The SMO entry's records.
// ---------------------------------------------------------------------------

/// One journal record of an SMO entry, owned until the write.
const EntryRecord = struct {
    tag: u8,
    key: []u8,
    kind: rec.RecordKind,
    seq: u64 = 0,
    value: []u8,
};

/// The SMO entry under construction: pointer records first (they are the
/// entry's records `0..n`, so the first flip's seq IS the entry start and
/// SMO floors pin at the §1b boundary), then alloc records, then the free
/// record(s) — the LAST record's position is the pending-free gate.
pub const SmoEntry = struct {
    gpa: Allocator,
    records: std.ArrayList(EntryRecord) = .empty,

    pub fn init(gpa: Allocator) SmoEntry {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *SmoEntry) void {
        for (self.records.items) |r| {
            self.gpa.free(r.key);
            self.gpa.free(r.value);
        }
        self.records.deinit(self.gpa);
    }

    fn push(self: *SmoEntry, tag: u8, k: []const u8, kind: rec.RecordKind, value: []const u8) Allocator.Error!void {
        const kc = try self.gpa.dupe(u8, k);
        errdefer self.gpa.free(kc);
        const vc = try self.gpa.dupe(u8, value);
        errdefer self.gpa.free(vc);
        try self.records.append(self.gpa, .{ .tag = tag, .key = kc, .kind = kind, .value = vc });
    }

    /// `Put(separator → (addr, node_seq))` at `level` of `t`.
    pub fn putInterior(self: *SmoEntry, t: *const KvTree, level: u8, separator: []const u8, addr: u64, ns: u64) Allocator.Error!void {
        const jk = t.interiorJournalKey(separator);
        try self.push(key.tagFor(t.tree_id, level), jk.slice(), .put, &key.encodeInteriorValue(addr, ns));
    }

    /// `Delete(separator)` at `level` of `t`.
    pub fn deleteInterior(self: *SmoEntry, t: *const KvTree, level: u8, separator: []const u8) Allocator.Error!void {
        const jk = t.interiorJournalKey(separator);
        try self.push(key.tagFor(t.tree_id, level), jk.slice(), .delete, &.{});
    }

    pub fn allocRecord(self: *SmoEntry, extent: u64) Allocator.Error!void {
        try self.push(key.tagFor(alloc_ext.tree_alloc_reserved, 0), &alloc_ext.extentKey(extent), .put, &alloc_ext.allocRecordValue());
    }

    pub fn freeRecord(self: *SmoEntry, extent: u64, retire_tag: u64) Allocator.Error!void {
        try self.push(key.tagFor(alloc_ext.tree_alloc_reserved, 0), &alloc_ext.extentKey(extent), .put, &alloc_ext.freeRecordValue(retire_tag));
    }

    pub fn len(self: *const SmoEntry) usize {
        return self.records.items.len;
    }

    /// Stamp `seq_base + i` on every record (inside the lock window).
    pub fn stampFrom(self: *SmoEntry, seq_base: u64) void {
        for (self.records.items, 0..) |*r, i| r.seq = seq_base + i;
    }

    pub fn seqAt(self: *const SmoEntry, i: usize) u64 {
        return self.records.items[i].seq;
    }

    pub fn frames(self: *const SmoEntry, gpa: Allocator) Allocator.Error![]journal.RecordFrame {
        const out = try gpa.alloc(journal.RecordFrame, self.records.items.len);
        for (self.records.items, out) |r, *f| f.* = .{ .tag = r.tag, .key = r.key, .kind = @fromBackingInt(@intCast(@backingInt(r.kind))), .seq = r.seq, .value = r.value };
        return out;
    }
};

/// An admitted-but-unreserved SMO entry: admission from the reserve, the
/// records it will carry, and their wire frames (built BEFORE any lock, so
/// nothing after the swap allocates — a post-swap failure would leave the
/// old extent unparked).
pub const PreparedEntry = struct {
    adm: Admission,
    recs: SmoEntry,
    frames: []journal.RecordFrame,

    /// Build the entry's frames and admit it from the checkpoint class
    /// (takes `recs` on every outcome).
    pub fn prepare(ctx: *SmoContext, j: *SmoJournal, recs: SmoEntry) Error!PreparedEntry {
        var owned = recs;
        errdefer owned.deinit();
        const frames = try owned.frames(owned.gpa);
        errdefer owned.gpa.free(frames);
        const len = try journal.entryLenFor(frames);
        const adm = ctx.admitSmoEntry(j, len) orelse return ctx.refuseReserve(len);
        return .{ .adm = adm, .recs = owned, .frames = frames };
    }

    pub fn deinit(self: *PreparedEntry) void {
        self.recs.gpa.free(self.frames);
        self.recs.deinit();
    }
};

/// Reserve `prep` inside the lock window: the records and their frames are
/// stamped from the reservation's seq base.
pub fn reserveInWindow(j: *SmoJournal, prep: *PreparedEntry) Allocator.Error!jc.InflightRegistry.Registered {
    const r = try j.reserve(prep.adm);
    prep.recs.stampFrom(r.seq_base);
    for (prep.frames, 0..) |*f, i| f.seq = r.seq_base + i;
    return r;
}

/// P4: write the entry after release; the gate is the LAST record's
/// position. A failed write is the crash-equivalent hole — the swap
/// already happened and RAM is authoritative; replay folds to the pre-SMO
/// state plus the by-key window, which is consistent. Infallible: the
/// frames were built before the lock window.
pub fn writeEntryAfterRelease(j: *SmoJournal, r: jc.InflightRegistry.Registered, prep: *const PreparedEntry, what: []const u8) u64 {
    const gate = r.res.recordGate(prep.recs.len() - 1);
    const written: journal.Error!void = if (j.testFailArmed()) blk: {
        j.abandonEntry(r.res);
        break :blk error.Corrupt;
    } else j.commitEntry(r.res, prep.frames);
    written catch |e| {
        stats.meta_kv_smo_entry_write_failures.inc();
        log.warn("{s} SMO journal entry write failed at position {d} ({s}): a crash-equivalent hole; state stays RAM-consistent", .{ what, r.res.start, @errorName(e) });
    };
    return gate;
}

// ---------------------------------------------------------------------------
// Test seam: the build-window pause.
// ---------------------------------------------------------------------------

/// Arm (`testSmoBuildPauseArm`) to park the next `smoReplace`/`smoMerge`
/// of a tree in its BUILD WINDOW — after the successor images are written
/// and loaded back, before the lock window — so a test can inject racing
/// commits whose reservations land BELOW the SMO's in-lock flip reservation
/// (the stranding shape). Holds `header id + 1` (a slot tree's header id
/// is 0); `0` = off; unarmed cost is one monotonic load per SMO.
pub var test_smo_build_pause_tree: std.atomic.Value(u64) = .init(0);

fn pauseArmFor(t: *const KvTree) u64 {
    return @as(u64, t.tree_id) + 1;
}

pub fn testSmoBuildPauseArm(t: *const KvTree) void {
    test_smo_build_pause_tree.store(pauseArmFor(t), .seq_cst);
}
/// Whether an SMO is parked right now (release pairs with the test's
/// acquire; `test_smo_pause_info` is valid while true).
pub var test_smo_build_paused: std.atomic.Value(bool) = .init(false);
pub var test_smo_pause_info: TestSmoPauseInfo = .{};

pub const TestSmoPauseInfo = struct {
    tree_id: u8 = 0,
    level: u8 = 0,
    /// A root swap journals no pointer records — the stranding contract
    /// requires `false`.
    is_root: bool = false,
    is_merge: bool = false,
    min_key: key.KeyBuf = .{},
    max_key: key.KeyBuf = .{},
};

/// Release a parked SMO (disarms first).
pub fn testSmoBuildPauseRelease() void {
    test_smo_build_pause_tree.store(0, .seq_cst);
}

pub fn testSmoBuildPause(t: *const KvTree, level: u8, is_root: bool, is_merge: bool, min_key: []const u8, max_key: []const u8) void {
    if (test_smo_build_pause_tree.load(.monotonic) != pauseArmFor(t)) return;
    test_smo_pause_info = .{ .tree_id = t.tree_id, .level = level, .is_root = is_root, .is_merge = is_merge, .min_key = key.KeyBuf.fromSlice(min_key), .max_key = key.KeyBuf.fromSlice(max_key) };
    test_smo_build_paused.store(true, .release);
    while (test_smo_build_pause_tree.load(.acquire) == pauseArmFor(t)) std.Thread.yield() catch std.atomic.spinLoopHint();
    test_smo_build_paused.store(false, .release);
}

// ---------------------------------------------------------------------------
// Shared SMO plumbing.
// ---------------------------------------------------------------------------

/// Read `n`'s extent fresh from the device and verify it under
/// `durable_tail` and `slot`'s frame screen — the fold's on-disk source.
/// The buffer outlives the loaded view (the caller releases both).
pub const Source = struct {
    buf: *snapshot.SharedBuf,
    loaded: node.LoadedNode,

    pub fn load(cache: *NodeCache, addr: u64, durable_tail: u64, slot: ?node_cache.ForestSlot) node_cache.LoadError!Source {
        const buf = try snapshot.SharedBuf.createExtent(cache.gpa, cache.cfg.layout.node_size);
        errdefer buf.release();
        try cache.loader.readExtent(cache.loader.ctx, addr, buf.data);
        const loaded = try node.verifyNodeExtentScreened(cache.gpa, buf.data, cache.cfg.layout, addr, durable_tail, cache.screenFor(slot, addr), null);
        return .{ .buf = buf, .loaded = loaded };
    }

    pub fn deinit(self: *Source, gpa: Allocator) void {
        self.loaded.deinit(gpa);
        self.buf.release();
        self.* = undefined;
    }
};

/// FIND-VS-A fold-source guard: an SMO's successors are folded from THIS
/// extent's on-disk log + THIS object's frozen delta. A disk image of
/// another incarnation (extent reuse racing a stale maintenance address)
/// or a log view disagreeing with the object's append cursor would build
/// successors missing acked records — refused loud; the caller's floor
/// stands and the next cycle retries. Returns the frozen delta records
/// (the newest fold source), owned.
pub fn foldSourceExtra(cache: *NodeCache, n: *CachedNode, src: *const node.LoadedNode) Error![]Record {
    const gpa = cache.gpa;
    const d = n.lockRead(cache.io);
    defer n.unlockRead(cache.io);
    if (src.header.node_seq != n.node_seq) {
        log.err("SMO fold-source incarnation mismatch at 0x{x}: disk image has node_seq {d}, the live object is {d} — refusing to fold a stale source (acked records would be dropped)", .{ n.addr, src.header.node_seq, n.node_seq });
        return error.Corrupt;
    }
    if (src.tail_offset != d.tailOffset()) {
        log.err("SMO fold-source log-view mismatch at 0x{x}: disk walk ends at {d}, the live object's append cursor is at {d} — refusing to fold a diverged source", .{ n.addr, src.tail_offset, d.tailOffset() });
        return error.Corrupt;
    }
    const frozen = d.frozenRecords();
    const out = try gpa.alloc(Record, frozen.len);
    var made: usize = 0;
    errdefer {
        rec.deinitRecords(gpa, out[0..made]);
        gpa.free(out);
    }
    for (frozen, out) |*f, *o| {
        o.* = try f.ref().toRecord(gpa);
        made += 1;
    }
    return out;
}

pub fn refsOf(gpa: Allocator, records: []const Record) Allocator.Error![]RecordRef {
    const out = try gpa.alloc(RecordRef, records.len);
    for (records, out) |*r, *o| o.* = r.ref();
    return out;
}

/// Greedy partition of key-ascending folded records into chunks of ≤
/// `budget` encoded bytes (each non-empty). Finding 41: a part boundary
/// may only land on a KEY boundary — same-key fold groups (the
/// `compactFold` lineage pair) land whole in ONE part; the byte budget
/// yields, the key law does not.
pub fn partitionRecords(gpa: Allocator, folded: []const Record, budget: usize) Allocator.Error![]const []const Record {
    var parts: std.ArrayList([]const Record) = .empty;
    errdefer parts.deinit(gpa);
    var start: usize = 0;
    var acc: usize = 0;
    for (folded, 0..) |*r, i| {
        const l = r.ref().encodedLen();
        if (acc + l > budget and i > start and !std.mem.eql(u8, folded[i].key, folded[i - 1].key)) {
            try parts.append(gpa, folded[start..i]);
            start = i;
            acc = 0;
        }
        acc += l;
    }
    try parts.append(gpa, folded[start..]);
    return parts.toOwnedSlice(gpa);
}

/// A node frozen for an SMO: the §4.6 pt 1 freeze-swap when it holds an
/// open delta, else the FORCED transition into FREEZING (an empty frozen
/// delta) so the swap's supersede/`endFreeze` bookkeeping sees a freeze-
/// borne source either way. The guard owns the forced bit (the W-B drop
/// guard): `release` ends a forced freeze the SMO did not consume, so an
/// error mid-SMO never leaves FREEZING latched on a live node.
pub const ForcedFreezeGuard = struct {
    n: *CachedNode,
    armed: bool,

    pub fn release(self: *ForcedFreezeGuard) void {
        if (self.armed and !self.n.state.isSuperseded()) _ = self.n.state.endFreeze();
        self.armed = false;
    }
};

/// `null` = the node was superseded / a freeze is racing — the caller's
/// census was stale, an idempotent no-op.
pub fn freezeForSmo(cache: *NodeCache, n: *CachedNode) Error!?ForcedFreezeGuard {
    const d = n.lockWrite(cache.io);
    defer n.unlockWrite(cache.io);
    const frozen = try n.freezeLocked(d, cache.cfg.layout);
    if (frozen != null) return .{ .n = n, .armed = false };
    n.state.beginForcedFreeze() catch return null;
    return .{ .n = n, .armed = true };
}

/// D4's compaction threshold (`defrag --meta`, `ops_jobs_lifecycle.md`
/// §3.1 "dead_bset_ratio = 1 − live/indexed"): a leaf compacts when at
/// least HALF of its serialized log is dead — the one law's node face
/// (the pack face's `live ≤ CHUNK/2`): the rewrite costs `live` bytes and
/// reclaims `log − live`, which pays iff `live ≤ log/2`.
pub const DEFRAG_COMPACT_DEAD_PERMILLE: u64 = 500;

/// The D4 nudge: a resident LEAF whose log is mostly dead folds into ONE
/// fresh image through the §4.6 replacement — `freezeForSmo` (a clean
/// node takes the forced freeze; a dirty one freezes its open delta) then
/// `smoReplace` with the FIFO valve KEPT (never the flush pass's forced
/// posture: a maintenance job is not what discharges a tail-pinning
/// floor, so it refuses pre-swap and the caller's cycle remedies). False =
/// the candidate went stale (superseded, a racing freeze) — a no-op.
pub fn compactForDefrag(t: *KvTree, ctx: *SmoContext, n: *CachedNode, out: *MaintenanceOutcome) Error!bool {
    var g = (try freezeForSmo(t.cache, n)) orelse return false;
    defer g.release();
    try smoReplace(t, ctx, n, out, false);
    return true;
}

/// Move the records of `leftovers` whose keys fall in `[min, max]` into
/// `succ`'s open delta with EXACT per-record floors (each carries its own
/// entry start from its original apply, so every successor gets exactly
/// `min` over the records it receives — the former predecessor-floor
/// inheritance chained an ancient floor through every compaction and
/// pinned the tail), and re-promise a successor whose inherited delta
/// overflows its log (the leftovers were admitted against the
/// predecessor's promise, released by `takeOverlay`, but this SMO folded
/// only the frozen delta — they still owe their flush).
pub fn applyLeftovers(cache: *NodeCache, succ: *CachedNode, leftovers: []OwnedRec, taken: []bool) Error!void {
    const gpa = cache.gpa;
    const layout = cache.cfg.layout;
    var mine: std.ArrayList(OwnedRec) = .empty;
    defer mine.deinit(gpa);
    var floor: u64 = node_cache.no_floor;
    for (leftovers, taken) |*r, *t| {
        if (t.*) continue;
        const k = r.key.slice();
        if (key.lessThan(k, succ.min_key) or key.lessThan(succ.max_key, k)) continue;
        floor = @min(floor, @min(r.entry_floor, r.seq));
        // The element moves: `taken` tells the caller not to free it; a
        // failed append leaves it with the caller.
        try mine.append(gpa, r.*);
        t.* = true;
    }
    if (mine.items.len == 0) return;
    const sd = succ.lockWrite(cache.io);
    defer succ.unlockWrite(cache.io);
    // Consumes `mine`'s elements on every outcome.
    succ.applyLockedStructural(sd, mine.items, floor) catch |e| return mapApply(e);
    if (sd.projectedLogEnd(layout, 0) > layout.node_size) {
        const snap = succ.snapshotLoad();
        defer snap.release();
        var none: [0]snapshot.Extra = .{};
        const p = snap.value.foldBytesUpperWith(&none, layout.splitPartCapacity(), 0);
        sd.promise(layout.smoExtentsForParts(p.bytes, p.parts), p.bytes);
    }
}

pub fn mapApply(e: node_cache.ApplyError) Error {
    return switch (e) {
        error.Superseded => error.Corrupt,
        error.ReaderMayNotMutate, error.LeafLeaseRefused => error.PartitionRefused,
        error.OutOfMemory => error.OutOfMemory,
    };
}

/// Re-enqueue `parent` if the flips pushed its open delta over the
/// threshold (the interior recursion's trigger).
pub fn requeueIfOverThreshold(t: *KvTree, parent: *CachedNode) void {
    const d = parent.lockRead(t.cache.io);
    const re = d.overlayBytes() >= t.cache.cfg.writeback_delta_bytes;
    parent.unlockRead(t.cache.io);
    if (re) t.enqueueMaintenance(parent.addr);
}

// ---------------------------------------------------------------------------
// smoReplace — compaction / split / root grow.
// ---------------------------------------------------------------------------

/// The build's products: claimed extents, loaded-back successors, the
/// optional new root. `abandon` releases every claim and reference on an
/// error before publication (nothing routes to them yet).
const Build = struct {
    gpa: Allocator,
    claimed: std.ArrayList(u64) = .empty,
    successors: std.ArrayList(*CachedNode) = .empty,
    new_root: ?*CachedNode = null,

    fn abandon(self: *Build, ctx: *SmoContext) void {
        for (self.claimed.items) |e| ctx.releaseUnpublished(e);
        self.releaseRefs();
    }

    fn releaseRefs(self: *Build) void {
        for (self.successors.items) |s| s.release();
        if (self.new_root) |r| r.release();
        self.claimed.deinit(self.gpa);
        self.successors.deinit(self.gpa);
        self.* = undefined;
    }

    fn claim(self: *Build, ctx: *SmoContext, cache: *NodeCache) Error!u64 {
        const extent = try ctx.claimInternal();
        errdefer ctx.releaseUnpublished(extent);
        try self.claimed.append(self.gpa, extent);
        return cache.extentAddr(extent);
    }
};

/// Write one successor image at `addr` — its base frame stamped for
/// `slot` — and load it back as an unpublished node of the slot.
pub fn writeSuccessor(cache: *NodeCache, params: node.NodeWriteParams, records: []const RecordRef, horizon: u64, pinned: bool, slot: ?node_cache.ForestSlot) Error!*CachedNode {
    var w = try node.writeNode(cache.gpa, cache.layoutFor(slot), params, records, horizon, null);
    defer w.deinit(cache.gpa);
    try cache.loader.writeImage(cache.loader.ctx, params.node_addr, w.image);
    return tree.loadFresh(cache, params.node_addr, pinned, slot);
}

/// The §4.6 three-step node replacement (module doc). `n` is FROZEN (its
/// unappended delta rides the fold as the newest source) and stays mapped
/// until the swap. `forced_retirement` selects the §4.7 pending-free
/// posture: the flush pass's compactions are the SMOs that discharge
/// tail-pinning floors, so they park unconditionally (refusing one at cap
/// closed the cycle {parked frees await the tail} → {the tail awaits this
/// node's floor} → {the floor awaits this SMO} → {the SMO awaits FIFO
/// room} → {FIFO room awaits the parked frees}); threshold maintenance
/// keeps the valve and refuses PRE-swap.
pub fn smoReplace(t: *KvTree, ctx: *SmoContext, n: *CachedNode, out: *MaintenanceOutcome, forced_retirement: bool) Error!void {
    const cache = t.cache;
    const gpa = cache.gpa;
    const io = cache.io;
    const layout = cache.cfg.layout;
    const durable_tail = t.durableTail();
    const is_root = t.isRoot(n);

    // ---- P1: build successors from the frozen snapshot, no locks.
    var src = try Source.load(cache, n.addr, durable_tail, t.forest_slot);
    defer src.deinit(gpa);
    const extra = try foldSourceExtra(cache, n, &src.loaded);
    defer {
        rec.deinitRecords(gpa, extra);
        gpa.free(extra);
    }
    const extra_refs = try refsOf(gpa, extra);
    defer gpa.free(extra_refs);
    var folded = try node.foldNodeSources(gpa, &.{&src.loaded}, &.{extra_refs}, durable_tail);
    defer folded.deinit(gpa);
    const items = folded.records.items;
    const total = folded.encodedBytes();
    // One part = compaction (it fit before folding; folding only shrinks);
    // more = split, parts filled to ¾ so appends have headroom. Exactly two
    // parts cut at the byte midpoint (`splitCut`, the K2 rule); wider
    // splits take the greedy partition.
    const greedy = try partitionRecords(gpa, items, layout.splitPartCapacity());
    defer gpa.free(greedy);
    var one_part = [1][]const Record{items};
    var two_parts: [2][]const Record = undefined;
    const parts: []const []const Record = if (total <= layout.foldCapacity()) &one_part else if (greedy.len == 2) blk: {
        const cut = node.splitCut(items) orelse return error.Corrupt;
        two_parts = .{ items[0..cut], items[cut..] };
        break :blk &two_parts;
    } else greedy;

    var build: Build = .{ .gpa = gpa };
    var published = false;
    errdefer if (published) build.releaseRefs() else build.abandon(ctx);
    {
        const all_refs = try refsOf(gpa, items);
        defer gpa.free(all_refs);
        var min_key = key.KeyBuf.fromSlice(n.min_key);
        var off: usize = 0;
        for (parts, 0..) |part, i| {
            const dst = try build.claim(ctx, cache);
            const ns = try t.seq.mint();
            const last = i + 1 == parts.len;
            const max_key: []const u8 = if (last) n.max_key else part[part.len - 1].key;
            const pinned = n.level > 0 or (is_root and parts.len == 1);
            const succ = try writeSuccessor(cache, .{
                .node_addr = dst,
                .node_seq = ns,
                .tree_id = n.tree_id,
                .level = n.level,
                .min_key = min_key.slice(),
                .max_key = max_key,
            }, all_refs[off .. off + part.len], folded.horizon, pinned, t.forest_slot);
            errdefer succ.release();
            try build.successors.append(gpa, succ);
            off += part.len;
            if (!last) min_key = key.keySuccessor(max_key);
        }
    }
    // The build-window park (test seam): images fixed on disk, no locks
    // held, the flip's reservation not yet taken.
    testSmoBuildPause(t, n.level, is_root, false, n.min_key, n.max_key);
    // A multi-way replacement of the root needs a new root above the parts
    // — written before any lock is taken. Its separators carry RECORD seq
    // 0 (the builder's convention): record seqs are the journal-domain
    // fold order, node seqs another domain; a fresh root has no earlier
    // records for these keys, so 0 is exact and every later pointer flip
    // folds above it.
    if (is_root and parts.len > 1) {
        const dst = try build.claim(ctx, cache);
        const ns = try t.seq.mint();
        const seps = try gpa.alloc(Record, build.successors.items.len);
        var made: usize = 0;
        defer {
            rec.deinitRecords(gpa, seps[0..made]);
            gpa.free(seps);
        }
        for (build.successors.items, seps) |s, *sep| {
            sep.* = try Record.put(gpa, s.max_key, 0, &key.encodeInteriorValue(s.addr, s.node_seq));
            made += 1;
        }
        const sep_refs = try refsOf(gpa, seps);
        defer gpa.free(sep_refs);
        build.new_root = try writeSuccessor(cache, .{
            .node_addr = dst,
            .node_seq = ns,
            .tree_id = n.tree_id,
            .level = n.level + 1,
            .min_key = n.min_key,
            .max_key = n.max_key,
        }, sep_refs, folded.horizon, true, t.forest_slot);
    }
    ctx.images_written += build.successors.items.len + @intFromBool(build.new_root != null);

    // ---- P2: barrier the images, then admit the entry from the reserve
    // — before any lock. A refusal aborts clean: nothing was published.
    const old_extent = cache.addrExtent(n.addr);
    var prep: ?PreparedEntry = null;
    defer if (prep) |*p| p.deinit();
    if (ctx.journal) |j| {
        try j.barrier();
        // At-cap headroom (clause a): this SMO pushes exactly one pending
        // free at P5 and the serialized task is the FIFO's only producer,
        // so headroom observed here still holds post-swap. Refusing NOW
        // keeps `PendingFreeFull` a clean pre-swap abort.
        if (!forced_retirement and !ctx.pendingHasRoomFor(1)) return ctx.refusePendingFull();
        var recs = SmoEntry.init(gpa);
        errdefer recs.deinit();
        if (!is_root) {
            for (build.successors.items) |s| try recs.putInterior(t, n.level + 1, s.max_key, s.addr, s.node_seq);
        }
        for (build.claimed.items) |e| try recs.allocRecord(e);
        try recs.freeRecord(old_extent, j.retire_seq.load(.acquire));
        prep = try PreparedEntry.prepare(ctx, j, recs);
    }

    // ---- P3: parent-then-child locks; reserve inside; move the delta;
    // publish → flip → retire; release.
    const parent: ?*CachedNode = if (is_root) null else try t.resolveParent(n);
    defer if (parent) |p| p.release();
    var registered: ?jc.InflightRegistry.Registered = null;
    {
        const pd: ?*node_cache.NodeDirty = if (parent) |p| p.lockWrite(io) else null;
        errdefer if (parent) |p| p.unlockWrite(io);
        const nd = n.lockWrite(io);
        errdefer n.unlockWrite(io);
        // The reservation INSIDE the window: any commit that lands on a
        // successor after the swap reserves after this and carries higher
        // seqs, so replay applies the pointer record first.
        if (prep) |*p| registered = try reserveInWindow(ctx.journal.?, p);
        const leftovers = try nd.takeOverlay(gpa);
        const taken = gpa.alloc(bool, leftovers.len) catch |e| {
            for (leftovers) |*r| r.deinit();
            gpa.free(leftovers);
            return e;
        };
        @memset(taken, false);
        // Every leftover not moved into a successor dies here (an error
        // path); the moved ones were consumed by the successor's apply.
        defer {
            for (leftovers, taken) |*r, tk| if (!tk) r.deinit();
            gpa.free(taken);
            gpa.free(leftovers);
        }
        const outcome = n.state.supersede() catch {
            log.err("double supersede of node 0x{x} (SMO serialization violated)", .{n.addr});
            return error.Corrupt;
        };
        assert(outcome.was_freezing);
        // The bounded second merge: the build-window delta into the
        // successors (they partition the node's range, so every leftover
        // lands in exactly one).
        for (build.successors.items) |s| try applyLeftovers(cache, s, leftovers, taken);
        for (taken) |tk| assert(tk);
        // Publish → flip → retire (module doc).
        for (build.successors.items) |s| {
            t.stamp(s);
            try cache.publish(s);
        }
        if (build.new_root) |r| {
            t.stamp(r);
            try cache.publish(r);
        }
        published = true;
        if (parent) |p| {
            const flips = try gpa.alloc(OwnedRec, build.successors.items.len);
            defer gpa.free(flips);
            {
                var made: usize = 0;
                errdefer for (flips[0..made]) |*f| f.deinit();
                for (build.successors.items, flips) |s, *f| {
                    // The first `successors.len` journal records ARE the
                    // pointer records, in successor order — RAM apply and
                    // replay carry identical seqs.
                    const seq = if (prep) |*pp| pp.recs.seqAt(made) else try t.seq.mint();
                    f.* = try OwnedRec.fromRecordRef(gpa, .{ .key = s.max_key, .seq = seq, .kind = .put, .value = &key.encodeInteriorValue(s.addr, s.node_seq) });
                    made += 1;
                }
            }
            // The flips are the entry's records 0..n, so the first flip's
            // seq IS the entry start.
            const flip_floor = flips[0].seq;
            p.applyLockedStructural(pd.?, flips, flip_floor) catch |e| return mapApply(e);
        } else {
            // A root swap is the ONE routing change with no journaled
            // pointer record and no parent floor — its durable form is
            // exclusively the next ledger record's roots. Clamp the next
            // tail to this swap's position (the dying-floor fold) so every
            // commit applied under the new arrangement stays inside the
            // replay window until a ledger record naming the new root
            // covers them.
            const swap_floor: u64 = if (registered) |r| r.res.start else 0;
            cache.noteDyingFloor(swap_floor);
            t.setRootFloor(swap_floor);
            const new_root = build.new_root orelse build.successors.items[0];
            try t.storeRoot(.{ .addr = new_root.addr, .seq = new_root.node_seq });
        }
        try cache.retire(n);
        n.unlockWrite(io);
        if (parent) |p| p.unlockWrite(io);
    }
    _ = n.state.endFreeze();

    // ---- P4 / P5: the entry bytes after release, then the old extent to
    // pending-free gated on the free record's own position.
    const gate: u64 = if (registered) |r| writeEntryAfterRelease(ctx.journal.?, r, &prep.?, "replace") else try t.seq.mint();
    try ctx.freePending(old_extent, gate, forced_retirement);
    if (parent) |p| requeueIfOverThreshold(t, p);
    if (build.successors.items.len == 1) {
        stats.meta_kv_node_compactions.inc();
        out.compactions += 1;
    } else {
        stats.meta_kv_node_splits.inc();
        out.splits += 1;
        t.noteNodes(@as(i64, @intCast(build.successors.items.len)) - 1);
        if (build.new_root != null) {
            stats.meta_kv_root_grows.inc();
            out.root_grows += 1;
            t.noteNodes(1);
        }
    }
    build.releaseRefs();
}

// ---------------------------------------------------------------------------
// Writeback and the maintenance drain.
// ---------------------------------------------------------------------------

/// Writeback / SMO for one node: freeze under the lock, append outside it;
/// a full log compacts; an oversized fold splits. Threshold posture: the
/// §4.7 FIFO valve applies.
pub fn maintainNode(t: *KvTree, ctx: *SmoContext, addr: u64, out: *MaintenanceOutcome) Error!void {
    const cache = t.cache;
    const n = cache.tryGet(addr) orelse return; // evicted/retired since enqueue
    defer n.release();
    if (n.state.isSuperseded() or !t.owns(n)) return;
    const frozen = blk: {
        const d = n.lockWrite(cache.io);
        defer n.unlockWrite(cache.io);
        break :blk (try n.freezeLocked(d, cache.cfg.layout)) != null;
    };
    if (!frozen) return; // spurious enqueue
    if (try cache.appendFrozen(n)) {
        out.appends += 1;
        // Re-enqueue if commits re-dirtied past the threshold while the
        // append was in flight.
        requeueIfOverThreshold(t, n);
        return;
    }
    try smoReplace(t, ctx, n, out, false);
}

/// Drain the maintenance queue under a pass deadline (monotonic ns; null =
/// unbounded): the first item always runs (every pass makes progress),
/// every later one only inside the deadline — a storm that re-enqueues
/// faster than the drain pops would otherwise hold the cadence tick
/// behind it. Work left queued is the caller's to re-arm; a failed item
/// is dropped (the node stays dirty for the flush pass).
pub fn runMaintenanceUntil(t: *KvTree, ctx: *SmoContext, deadline: ?u64) Error!MaintenanceOutcome {
    var out: MaintenanceOutcome = .{};
    var progressed = false;
    while (t.popMaintenance()) |addr| {
        if (progressed) {
            if (deadline) |d| if (clock.monotonicNs() >= d) {
                t.enqueueMaintenance(addr);
                break;
            };
        }
        progressed = true;
        try maintainNode(t, ctx, addr, &out);
    }
    return out;
}

pub fn runMaintenance(t: *KvTree, ctx: *SmoContext) Error!MaintenanceOutcome {
    return runMaintenanceUntil(t, ctx, null);
}

const DirtyCollector = struct {
    t: *const KvTree,
    out: *std.ArrayList(u64),
    failed: bool = false,

    fn visit(self: *DirtyCollector, n: *CachedNode) void {
        if (self.t.owns(n) and (n.state.isDirty() or n.state.isFreezing()) and !n.state.isSuperseded()) {
            self.out.append(self.t.gpa, n.addr) catch {
                self.failed = true;
            };
        }
    }
};

/// Force-writeback every dirty node of this tree regardless of threshold —
/// the checkpoint's flush shape; bounded passes cover the interior
/// recursion (leaf writebacks enqueue parent pointer work).
pub fn flushDirty(t: *KvTree, ctx: *SmoContext) Error!MaintenanceOutcome {
    var out: MaintenanceOutcome = .{};
    var attempt: u32 = 0;
    while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
        var dirty: std.ArrayList(u64) = .empty;
        defer dirty.deinit(t.gpa);
        var c: DirtyCollector = .{ .t = t, .out = &dirty };
        t.cache.forEachNode(&c, DirtyCollector.visit);
        if (c.failed) return error.OutOfMemory;
        if (dirty.items.len == 0 and !t.maintenancePending()) return out;
        for (dirty.items) |a| t.enqueueMaintenance(a);
        out.add(try runMaintenance(t, ctx));
    }
    log.err("tree {d}: flushDirty never converged (writeback keeps re-dirtying — SMO bug)", .{t.tree_id});
    return error.RetryBudgetExhausted;
}

/// The checkpoint's per-node flush step: freeze the open delta and TAKE
/// the dirty floor in the same lock window (no applied record can slip
/// between them), then append outside the lock — a full log
/// compacts/splits through the SMO path with FORCED retirement. On failure
/// the floor is restored. `flushFreezeLocked` keeps the floor of the
/// records still in the open delta when it answers a PARKED frozen delta
/// (the eight-writer storm's "deleted stays deleted" loss). After a leaf's
/// visit the §4.6a prefilter runs (`leaf_merge.mergeAfterFlush`).
pub fn checkpointFlushNode(t: *KvTree, ctx: *SmoContext, addr: u64) Error!void {
    const cache = t.cache;
    const n = cache.tryGet(addr) orelse return; // evicted/retired since the dirty walk
    defer n.release();
    if (n.state.isSuperseded() or !t.owns(n)) return;
    const ff = blk: {
        const d = n.lockWrite(cache.io);
        defer n.unlockWrite(cache.io);
        break :blk try n.flushFreezeLocked(d, cache.cfg.layout);
    };
    // Nothing to write: clean, or already appended by threshold
    // maintenance — the caller's barrier covers those appends, so the
    // cleared floor is exact.
    if (!ff.frozen) return;
    const flushed: Error!void = blk: {
        const appended = cache.appendFrozen(n) catch |e| break :blk e;
        if (appended) break :blk {};
        // Log area full ⇒ compact / split. Restore the taken floor FIRST
        // (FIND-VS-A): the SMO retires this node, and `retire`'s dying-
        // floor fold is what keeps the tail clamped to the frozen delta's
        // records until a ledger record written after the swap covers
        // them — a floor still parked in this frame's local would die
        // silently with the object.
        n.restoreDirtyFloor(ff.floor);
        var o: MaintenanceOutcome = .{};
        break :blk smoReplace(t, ctx, n, &o, true);
    };
    // The keep exemption was this node's own SMO's: the opportunistic merge
    // below and every later SMO keep.
    ctx.setKeepExempt(false);
    flushed catch |e| {
        n.restoreDirtyFloor(ff.floor);
        return e;
    };
    if (n.level == 0) try leaf_merge.mergeAfterFlush(t, ctx, n.min_key);
}

// ---------------------------------------------------------------------------
// The checkpoint task's structural seam over this volume's trees.
// ---------------------------------------------------------------------------

/// Binds `smoReplace` and `leaf_merge.mergeUnderfull` into the checkpoint
/// task's `checkpoint_cycle.SmoSeam` hook. `maintain` is the structural
/// half of the flush step (the checkpoint's `CacheFlusher` froze and tried
/// the append): the tree owning the node runs its SMO under the pass's
/// keep exemption and retirement posture. `mergeUnderfull` is the cycle's
/// end-of-cycle sweep under FORCED retirement (the cycle's posture; the D4
/// arm keeps the valve), one volume lap across the trees: a tree's lap
/// completes before the next tree's starts, so a tree the budget keeps
/// cutting is never starved by the ones before it re-walking.
pub const CheckpointSeam = struct {
    ctx: *SmoContext,
    /// The volume's live tree set (a flush may meet a tree minted or
    /// migrated in since the mount).
    trees: *const tree.TreeTable,

    pub fn seam(self: *CheckpointSeam) checkpoint_cycle.SmoSeam {
        return .{ .ctx = self, .maintain = maintain, .mergeUnderfull = mergeUnderfull };
    }

    fn treeOf(self: *const CheckpointSeam, n: *const CachedNode) ?*KvTree {
        for (self.trees.load()) |t| if (t.owns(n)) return t;
        return null;
    }

    fn maintain(ctx: *anyopaque, n: *CachedNode, opts: checkpoint_cycle.SmoSeam.MaintainOpts) checkpoint_cycle.SmoOutcome {
        const self: *CheckpointSeam = @ptrCast(@alignCast(ctx));
        const t = self.treeOf(n) orelse return .declined;
        self.ctx.setKeepExempt(opts.keep_exempt);
        defer self.ctx.setKeepExempt(false);
        const before = self.ctx.images_written;
        var out: MaintenanceOutcome = .{};
        smoReplace(t, self.ctx, n, &out, opts.forced_retirement) catch |e| return switch (e) {
            error.JournalReserveExhausted => .journal_reserve_exhausted,
            error.PendingFreeFull => .pending_free_full,
            error.NoSpace => if (self.ctx.grant_fed) .grant_exhausted else .no_space,
            error.OutOfMemory => .{ .failed = .out_of_memory },
            // The device's classes: the cycle aborts and the tick retries.
            error.Io, error.BarrierFailed => .{ .failed = .io },
            else => .{ .failed = .corrupt },
        };
        return .{ .replaced = .{ .images = self.ctx.images_written - before } };
    }

    fn mergeUnderfull(ctx: *anyopaque, deadline_ns: u64) checkpoint_cycle.SweepReport {
        const self: *CheckpointSeam = @ptrCast(@alignCast(ctx));
        var report: checkpoint_cycle.SweepReport = .{};
        for (self.trees.load()) |t| {
            // A loud class (corruption / allocation) ends the sweep with the
            // lap incomplete; the cursor stands where it parked.
            const s = leaf_merge.mergeUnderfull(t, self.ctx, true, deadline_ns) catch {
                report.lap_complete = false;
                return report;
            };
            report.merges += s.outcome.merges;
            report.interior_merges += s.outcome.interior_merges;
            report.root_collapses += s.outcome.root_collapses;
            report.space_refused = report.space_refused or s.space_refused;
            if (!s.lap_complete) {
                report.lap_complete = false;
                return report;
            }
            report.candidates += s.candidates;
        }
        return report;
    }
};

// ---------------------------------------------------------------------------
// The contracts' volume: a RAM heap + cache + allocator + ring image + the
// serialized context, shared by `tree.zig`, `smo.zig` and `leaf_merge.zig`
// tests.
// ---------------------------------------------------------------------------

pub const TestVolume = struct {
    pub const Config = struct {
        node_size: usize = node.MIN_NODE_SIZE,
        /// Heap extents (the RAM image is `extents × node_size`).
        extents: u64 = 512,
        /// Cache budget in nodes (default: everything cached).
        budget_nodes: ?u64 = null,
        writeback_delta_bytes: usize = node_cache.DEFAULT_WRITEBACK_DELTA_BYTES,
        /// Compaction reserve (0 = no reserve pressure).
        reserve: u64 = 0,
        pending_cap: usize = 4096,
        /// Ring pages (the ring image is `pages × 4 KiB`): the 8 MiB
        /// floor, whose reserve is the 256 KiB floor.
        ring_pages: u64 = journal.RING_MIN / journal.PAGE_LEN,
        /// The ring's sector grain: 1 = unpadded (the contracts' default);
        /// 4096 = the O_DIRECT page posture with the sector-pad law.
        ring_grain: u64 = 1,
        /// Journaled SMOs and commits (the production shape) or the
        /// standalone minted-seq shape.
        journaled: bool = false,
    };

    gpa: Allocator,
    cfg: Config,
    layout: node.NodeLayout,
    ram: *node_cache.RamLoader,
    cache: *NodeCache,
    alloc: *alloc_ext.ExtentAllocator,
    seq: *node_seq.NodeSeqHandle,
    ring_image: []u8,
    core: *jc.JournalCore,
    reg: *jc.InflightRegistry,
    journal: *SmoJournal,
    ctx: SmoContext,
    trees: std.ArrayList(*KvTree) = .empty,
    /// The live-set view the conveyor and the checkpoint seam route
    /// through (kept in step with `trees` by `createTree`).
    table: *tree.TreeTable,

    pub fn init(gpa: Allocator, cfg: Config) !TestVolume {
        const layout = try node.NodeLayout.new(cfg.node_size, null);
        const ram = try gpa.create(node_cache.RamLoader);
        errdefer gpa.destroy(ram);
        ram.* = try node_cache.RamLoader.init(gpa, @intCast(cfg.extents * cfg.node_size));
        errdefer ram.deinit(gpa);
        const budget_nodes = cfg.budget_nodes orelse cfg.extents;
        const cache = try NodeCache.init(gpa, std.testing.io, .{
            .layout = layout,
            .heap_base = 0,
            .budget_bytes = budget_nodes * cfg.node_size,
            .writeback_delta_bytes = cfg.writeback_delta_bytes,
        }, ram.loader());
        errdefer cache.deinit();
        const alloc = try gpa.create(alloc_ext.ExtentAllocator);
        errdefer gpa.destroy(alloc);
        alloc.* = try alloc_ext.ExtentAllocator.format(gpa, cfg.extents, cfg.reserve, cfg.pending_cap);
        errdefer alloc.deinit();
        const seq = try gpa.create(node_seq.NodeSeqHandle);
        errdefer gpa.destroy(seq);
        seq.* = node_seq.NodeSeqHandle.shared(0);
        const ring_image = try gpa.alloc(u8, @intCast(cfg.ring_pages * journal.PAGE_LEN));
        errdefer gpa.free(ring_image);
        @memset(ring_image, 0);
        const geo = journal.productionGeometry(cfg.ring_pages, cfg.ring_grain);
        const core = try gpa.create(jc.JournalCore);
        errdefer gpa.destroy(core);
        core.* = jc.JournalCore.init(geo, 0, 0);
        const reg = try gpa.create(jc.InflightRegistry);
        errdefer gpa.destroy(reg);
        reg.* = try jc.InflightRegistry.init(gpa, 0, 1024);
        errdefer reg.deinit();
        const j = try gpa.create(SmoJournal);
        errdefer gpa.destroy(j);
        j.* = .{ .gpa = gpa, .layout = .{ .base = 0, .pages = cfg.ring_pages }, .geo = geo, .core = core, .reg = reg, .image = ring_image };
        const table = try gpa.create(tree.TreeTable);
        errdefer gpa.destroy(table);
        table.* = try tree.TreeTable.init(gpa, &.{});
        return .{
            .gpa = gpa,
            .cfg = cfg,
            .layout = layout,
            .ram = ram,
            .cache = cache,
            .alloc = alloc,
            .seq = seq,
            .ring_image = ring_image,
            .core = core,
            .reg = reg,
            .journal = j,
            .ctx = if (cfg.journaled) SmoContext.withJournal(alloc, j) else SmoContext.init(alloc),
            .table = table,
        };
    }

    pub fn deinit(self: *TestVolume) void {
        self.table.deinit();
        self.gpa.destroy(self.table);
        for (self.trees.items) |t| t.destroy();
        self.trees.deinit(self.gpa);
        self.cache.deinit();
        self.gpa.destroy(self.journal);
        self.reg.deinit();
        self.gpa.destroy(self.reg);
        self.gpa.destroy(self.core);
        self.gpa.free(self.ring_image);
        self.gpa.destroy(self.seq);
        self.alloc.deinit();
        self.gpa.destroy(self.alloc);
        self.ram.deinit(self.gpa);
        self.gpa.destroy(self.ram);
        self.* = undefined;
    }

    pub fn createTree(self: *TestVolume, identity: tree.TreeIdentity) !*KvTree {
        const t = try KvTree.create(self.gpa, self.cache, self.alloc, identity, self.seq);
        errdefer t.destroy();
        try self.trees.append(self.gpa, t);
        try self.table.add(t);
        return t;
    }

    pub fn drain(self: *TestVolume, t: *KvTree) !MaintenanceOutcome {
        var out: MaintenanceOutcome = .{};
        while (t.maintenancePending()) out.add(try smo.runMaintenance(t, &self.ctx));
        return out;
    }

    pub fn flushDirty(self: *TestVolume, t: *KvTree) !void {
        _ = try smo.flushDirty(t, &self.ctx);
    }

    /// The checkpoint's flush pass over `t`: every dirty node through
    /// `checkpointFlushNode` (floors taken), repeated until the interior
    /// recursion settles.
    pub fn flushPass(self: *TestVolume, t: *KvTree) !void {
        var attempt: u32 = 0;
        while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
            var dirty: std.ArrayList(u64) = .empty;
            defer dirty.deinit(self.gpa);
            var c: DirtyCollector = .{ .t = t, .out = &dirty };
            self.cache.forEachNode(&c, DirtyCollector.visit);
            if (c.failed) return error.OutOfMemory;
            if (dirty.items.len == 0) return;
            for (dirty.items) |a| try smo.checkpointFlushNode(t, &self.ctx, a);
        }
        return error.RetryBudgetExhausted;
    }

    /// The position every record stamped so far lies below.
    pub fn frontier(self: *TestVolume) u64 {
        return if (self.cfg.journaled) self.core.headPos() else self.seq.load();
    }

    /// The stand-in for a covering checkpoint: every seq minted so far
    /// counts as durable — tombstones below elide at the next fold, parked
    /// retirements release, ring space reclaims.
    pub fn coverEverything(self: *TestVolume) void {
        const tail = self.frontier();
        self.cache.setDurableTail(tail);
        _ = self.alloc.advanceDurable(tail);
        if (self.cfg.journaled) self.core.advanceReusableUpto(@min(tail, self.reg.completedUpto()));
    }

    /// A second cache over the SAME device images (a crash-equivalent
    /// reopen's RAM).
    pub fn freshCache(self: *TestVolume) !*NodeCache {
        return NodeCache.init(self.gpa, std.testing.io, self.cache.cfg, self.ram.loader());
    }

    // ---- the journaled commit: admit → resolve → lock → reserve inside → apply → write ----

    const StampCtx = struct {
        j: *SmoJournal,
        adm: Admission,
        registered: ?jc.InflightRegistry.Registered = null,

        fn stamp(ctx: *anyopaque) Allocator.Error!tree.Stamp {
            const self: *StampCtx = @ptrCast(@alignCast(ctx));
            const r = try self.j.reserve(self.adm);
            self.registered = r;
            return .{ .seq = r.seq_base, .entry_start = r.res.start };
        }
    };

    /// One user commit through the tree's lock-window primitive with the
    /// reservation taken INSIDE the window (the conveyor's shape for one
    /// record), its entry written to the ring image.
    pub fn commit(self: *TestVolume, t: *KvTree, k: []const u8, kind: rec.RecordKind, value: []const u8) !void {
        assert(self.cfg.journaled);
        // The tag is the record's kind: the key's own on a slot tree.
        const tag_kind: u8 = switch (t.identity) {
            .control => key.TREE_CONTROL,
            .slot => try key.slotTreeKeyKind(k),
        };
        var frames = [_]journal.RecordFrame{.{ .tag = key.tagFor(tag_kind, 0), .key = k, .kind = @fromBackingInt(@intCast(@backingInt(kind))), .seq = 0, .value = value }};
        const len = try journal.entryLenFor(&frames);
        const adm = self.core.tryAdmit(len, .user) orelse return error.RingFull;
        var st: StampCtx = .{ .j = self.journal, .adm = adm };
        var attempt: u32 = 0;
        while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
            const leaf = try t.resolveLeaf(k);
            defer leaf.release();
            switch (try t.applyAtSeqClass(leaf, k, kind, value, .{ .reserve = .{ .ctx = &st, .stamp = StampCtx.stamp } }, false)) {
                .applied => {
                    const r = st.registered.?;
                    frames[0].seq = r.seq_base;
                    try self.journal.commitEntry(r.res, &frames);
                    return;
                },
                .stale => continue,
            }
        }
        self.core.release(adm);
        return error.RetryBudgetExhausted;
    }

    // ---- crash replay ----

    /// A ledger record's content: roots, the tail replay starts at, the
    /// node-seq watermark.
    pub const Ledger = struct {
        roots: [4]ledger.TreeRoot,
        n: usize,
        tail: u64,
        watermark: u64,
    };

    const FloorCollector = struct {
        min: u64 = node_cache.no_floor,
        fn visit(self: *FloorCollector, n: *CachedNode) void {
            self.min = @min(self.min, n.dirtyFloor());
        }
    };

    /// The checkpoint cycle's core as the harness runs it: the flush pass
    /// over every tree, then `tail = min(head, oldest in-flight, dying
    /// floors, every live dirty floor)` rounded to entry starts (every
    /// input is one), record the roots, and make the tail durable
    /// (elision, pending-free release, ring reclamation). The dying floors
    /// of this cycle's own SMOs pin the tail for ONE cycle: a second
    /// `checkpoint` on a quiet volume lands the tail at the head.
    pub fn checkpoint(self: *TestVolume) !Ledger {
        assert(self.cfg.journaled);
        for (self.trees.items) |t| try self.flushPass(t);
        const h = self.core.headPos();
        const dying = self.cache.takeDyingFloors();
        var fc: FloorCollector = .{};
        self.cache.forEachNode(&fc, FloorCollector.visit);
        const tail = ledger.checkpointTail(h, &.{ self.reg.minInflightStart(), dying, fc.min });
        var led: Ledger = .{ .roots = undefined, .n = self.trees.items.len, .tail = tail, .watermark = self.seq.load() };
        _ = tree.roots(self.trees.items, &led.roots);
        self.cache.setDurableTail(tail);
        _ = self.alloc.advanceDurable(tail);
        self.core.advanceReusableUpto(@min(tail, self.reg.completedUpto()));
        return led;
    }

    /// A crash-equivalent reopen from `led`: a fresh cache over the same
    /// device images, trees opened at the ledger's roots, the ring image
    /// replayed from the tail in two phases.
    pub const Reopened = struct {
        gpa: Allocator,
        cache: *NodeCache,
        seq: *node_seq.NodeSeqHandle,
        trees: std.ArrayList(*KvTree),
        recovery: journal.JournalRecovery,
        outcome: tree.ReplayOutcome,

        pub fn deinit(self: *Reopened) void {
            for (self.trees.items) |t| t.destroy();
            self.trees.deinit(self.gpa);
            self.recovery.deinit();
            self.cache.deinit();
            self.gpa.destroy(self.seq);
            self.* = undefined;
        }

        pub fn treeOf(self: *const Reopened, identity: tree.TreeIdentity) *KvTree {
            for (self.trees.items) |t| if (t.identity.eql(identity)) return t;
            unreachable;
        }
    };

    pub fn crashReplay(self: *TestVolume, led: Ledger) !Reopened {
        const gpa = self.gpa;
        const cache = try self.freshCache();
        errdefer cache.deinit();
        cache.setDurableTail(led.tail);
        const seq = try gpa.create(node_seq.NodeSeqHandle);
        errdefer gpa.destroy(seq);
        seq.* = node_seq.NodeSeqHandle.shared(led.watermark);
        var trees: std.ArrayList(*KvTree) = .empty;
        errdefer {
            for (trees.items) |t| t.destroy();
            trees.deinit(gpa);
        }
        for (led.roots[0..led.n]) |r| {
            const t = try KvTree.open(gpa, cache, .fromRootId(r.id), .{ .addr = r.node_addr, .seq = r.node_seq }, seq);
            errdefer t.destroy();
            seq.raiseTo(r.node_seq);
            try trees.append(gpa, t);
        }
        var recovery = try journal.replayScanImage(gpa, self.ring_image, self.journal.geo, led.tail, self.journal.appender_id);
        errdefer recovery.deinit();
        const outcome = try tree.replayEntries(gpa, trees.items, recovery.entries);
        return .{ .gpa = gpa, .cache = cache, .seq = seq, .trees = trees, .recovery = recovery, .outcome = outcome };
    }

    // ---- tears ----

    /// Corrupt one byte of the entry at `res` (its checksum fails: the
    /// entry is dropped whole; same-page successors of the dead chain are
    /// lost, later pages resync).
    pub fn tearEntry(self: *TestVolume, res: Reservation) void {
        const off: usize = @intCast(self.journal.layout.physicalOffsetOf(self.journal.geo, res.start + journal.ENTRY_HDR_LEN));
        self.ring_image[off] ^= 0xFF;
    }

    /// Lose every byte at logical positions ≥ `pos` (the crash landed
    /// nothing past it).
    pub fn cutAfter(self: *TestVolume, pos: u64) void {
        const geo = self.journal.geo;
        const end = self.core.headPos();
        if (end <= pos) return;
        var segs = geo.segments(pos, end - pos);
        while (segs.next()) |seg| {
            const at: usize = @intCast(seg.page * journal.PAGE_LEN + journal.PAGE_HDR_LEN + seg.data_off);
            @memset(self.ring_image[at .. at + @as(usize, @intCast(seg.len))], 0);
        }
    }

    // ---- fixtures ----

    /// Grow `t` with `value_len`-byte values under `key_len`-byte keys
    /// (big-endian counters, so insertion order is key order) until the
    /// root reaches `level`, draining maintenance every 8 inserts and
    /// covering as it goes (the stand-in for the checkpoint's release of
    /// the splits' retirements). Returns the key count.
    pub fn growToRootLevel(self: *TestVolume, t: *KvTree, level: u8, key_len: usize, value_len: usize, max_keys: u64) !u64 {
        const vbuf = try self.gpa.alloc(u8, value_len);
        defer self.gpa.free(vbuf);
        var n: u64 = 0;
        while (try t.rootLevel() < level) : (n += 1) {
            if (n >= max_keys) return error.FixtureNeverFormed;
            var kb: [32]u8 = undefined;
            try t.insert(wideKey(n, key_len, &kb), tree.testValue(n, value_len, vbuf));
            if (n % 8 == 7) {
                _ = try self.drain(t);
                self.coverEverything();
            }
        }
        _ = try self.drain(t);
        return n;
    }
};

/// A `len`-byte key carrying `i` big-endian in its last 8 bytes (zero
/// prefix): memcmp order equals numeric order, and the width sets the
/// interior fanout (`interiorFanout`).
pub fn wideKey(i: u64, len: usize, buf: *[32]u8) []u8 {
    assert(len >= 8 and len <= 32);
    @memset(buf[0..len], 0);
    std.mem.writeInt(u64, buf[len - 8 ..][0..8], i, .big);
    return buf[0..len];
}

// ---------------------------------------------------------------------------
// Tests — row 7 of the survey over `TestVolume`.
// ---------------------------------------------------------------------------

const testing = std.testing;
const KeyValue = tree.KeyValue;

fn ikey(i: u64) [key.INODE_KEY_LEN]u8 {
    return key.inodeKey(i);
}

fn expectLookup(t: *KvTree, k: []const u8, want: ?[]const u8) !void {
    const got = try t.lookup(testing.allocator, k);
    defer if (got) |g| testing.allocator.free(g);
    if (want) |w| {
        try testing.expectEqualSlices(u8, w, got orelse return error.TestExpectedLive);
    } else {
        try testing.expectEqual(@as(?[]u8, null), got);
    }
}

test "derivation ties: interior fanout; the SMO keep covers the widest entry at every format geometry and production ring" {
    const l64 = try node.NodeLayout.new(64 * 1024, null);
    const l256 = try node.NodeLayout.new(node.DEFAULT_NODE_SIZE, null);
    const l1m = try node.NodeLayout.new(node.MAX_NODE_SIZE, null);
    // v4: a separator record over an inode key is 16 + 9 + 16 = 41 B.
    try testing.expectEqual(@as(usize, 46_026 / 41), interiorFanout(l64, key.INODE_KEY_LEN));
    try testing.expectEqual(@as(usize, 1_122), interiorFanout(l64, key.INODE_KEY_LEN));
    try testing.expectEqual(@as(usize, 193_482 / 41), interiorFanout(l256, key.INODE_KEY_LEN));
    try testing.expectEqual(@as(usize, 4_719), interiorFanout(l256, key.INODE_KEY_LEN));
    // The charter's "≈ 7,000 at 256 KiB": the full fold's separators,
    // 257,976 / 41.
    try testing.expectEqual(@as(usize, 6_292), l256.foldCapacity() / 41);
    // 100 M records of ~100 B at the ¾ fill ≈ 52 k leaves ≪ fanout² ⇒ depth ≤ 3.
    const leaves_100m: u64 = 100_000_000 * 100 / l256.splitPartCapacity();
    try testing.expect(leaves_100m < @as(u64, interiorFanout(l256, key.INODE_KEY_LEN)) * interiorFanout(l256, key.INODE_KEY_LEN));
    for ([_]u64{ journal.RING_MIN, 16 * 1024 * 1024, journal.RING_MAX }) |ring| {
        for ([_]u64{ 1, 512, 4096 }) |grain| {
            const geo = journal.productionGeometry(ring / journal.PAGE_LEN, grain);
            for ([_]node.NodeLayout{ l64, l256, l1m }) |l| try testing.expect(smoKeepCoversWidestEntry(l, geo));
        }
    }
}

test "partitionRecords: greedy ¾-fill parts never split a same-key group (finding 41)" {
    const gpa = testing.allocator;
    var recs: [6]Record = undefined;
    recs[0] = try Record.put(gpa, "a", 1, "xxxxxxxxxx");
    recs[1] = try Record.put(gpa, "b", 2, "xxxxxxxxxx");
    recs[2] = try Record.put(gpa, "b", 3, "xxxxxxxxxx");
    recs[3] = try Record.put(gpa, "c", 4, "xxxxxxxxxx");
    recs[4] = try Record.put(gpa, "d", 5, "xxxxxxxxxx");
    recs[5] = try Record.put(gpa, "e", 6, "xxxxxxxxxx");
    defer rec.deinitRecords(gpa, &recs);
    const one = recs[0].ref().encodedLen();
    const parts = try partitionRecords(gpa, &recs, one * 2);
    defer gpa.free(parts);
    // [a, b, b] (the byte budget yields: the second `b` may not open a
    // part), [c, d], [e].
    try testing.expectEqual(@as(usize, 3), parts.len);
    try testing.expectEqual(@as(usize, 3), parts[0].len);
    try testing.expectEqualStrings("b", parts[0][1].key);
    try testing.expectEqualStrings("b", parts[0][2].key);
    try testing.expectEqual(@as(usize, 2), parts[1].len);
    try testing.expectEqualStrings("c", parts[1][0].key);
    try testing.expectEqual(@as(usize, 1), parts[2].len);
    try testing.expectEqualStrings("e", parts[2][0].key);
    // A budget below one record still never splits a key group.
    const tight = try partitionRecords(gpa, &recs, 1);
    defer gpa.free(tight);
    try testing.expectEqual(@as(usize, 5), tight.len);
    try testing.expectEqual(@as(usize, 2), tight[1].len);
}

test "writeback_appends_then_compacts_then_splits: threshold appends never move the node; a full log compacts CoW with the old extent pending-free until durable; unique keys split the root" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    const root0 = t.rootPtr();
    const c0 = stats.meta_kv_node_compactions.load();
    const s0 = stats.meta_kv_node_splits.load();
    var buf: [64]u8 = undefined;
    for (0..200) |i| try t.insert(&ikey(i), tree.testValue(i, 24, &buf));
    const a = try vol.drain(t);
    for (200..400) |i| try t.insert(&ikey(i), tree.testValue(i, 24, &buf));
    const b = try vol.drain(t);
    try testing.expect(a.appends >= 1 and b.appends >= 1);
    var src = try Source.load(vol.cache, t.rootPtr().addr, 0, t.forest_slot);
    defer src.deinit(gpa);
    try testing.expect(src.loaded.bsetCount() >= 2);
    try testing.expectEqual(root0, t.rootPtr());

    // Overwrite one key band until the log fills ⇒ compaction.
    const free_before = vol.alloc.freeExtents();
    var spins: u32 = 0;
    while (stats.meta_kv_node_compactions.load() == c0) : (spins += 1) {
        for (0..200) |i| try t.insert(&ikey(i), tree.testValue(i * 31, 24, &buf));
        _ = try vol.drain(t);
        try testing.expect(spins < 10_000);
    }
    const root1 = t.rootPtr();
    try testing.expect(root1.addr != root0.addr);
    try testing.expect(vol.alloc.pendingCount() >= 1);
    try testing.expect(vol.alloc.freeExtents() < free_before);
    try testing.expect(vol.alloc.advanceDurable(vol.seq.load()) >= 1);
    var i: u64 = 0;
    while (i < 200) : (i += 17) try expectLookup(t, &ikey(i), tree.testValue(i * 31, 24, &buf));

    // Unique keys until the root leaf splits ⇒ a new interior root.
    const g0 = stats.meta_kv_root_grows.load();
    var n: u64 = 1000;
    while (stats.meta_kv_node_splits.load() == s0) {
        for (0..64) |_| {
            try t.insert(&ikey(n), tree.testValue(n, 40, &buf));
            n += 1;
        }
        _ = try vol.drain(t);
        try testing.expect(n < 1_000_000);
    }
    try testing.expectEqual(@as(u8, 1), try t.rootLevel());
    try testing.expectEqual(g0 + 1, stats.meta_kv_root_grows.load());
    for ([_]u64{ 1000, 1100, n - 1 }) |p| try expectLookup(t, &ikey(p), tree.testValue(p, 40, &buf));
    try expectLookup(t, &ikey(3), tree.testValue(3 * 31, 24, &buf));
}

test "the D4 nudge (defrag --meta): a leaf whose serialized log is mostly dead (one key band overwritten across drains) measures past DEFRAG_COMPACT_DEAD_PERMILLE and `compactForDefrag` folds it into ONE fresh image — a compaction, never a split, the old extent pending-free, every key current, the occupancy dead share ~0 after; a fresh leaf below the threshold is not a candidate; a superseded node is the clean no-op" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    for (0..200) |i| try t.insert(&ikey(i), tree.testValue(i, 24, &buf));
    _ = try vol.drain(t);
    const root0 = t.rootPtr();
    {
        const n = try vol.cache.get(root0.addr);
        defer n.release();
        // One base frame: nothing dead yet.
        const occ = leaf_merge.leafOccupancy(t, n);
        try testing.expect(occ.log_bytes > 0);
        try testing.expect(occ.deadPermille() < DEFRAG_COMPACT_DEAD_PERMILLE);
    }
    // Overwrite the band across drains: every drain appends a frame that
    // shadows the previous one — the log grows, the fold does not. Stop
    // BEFORE the threshold maintenance's own compaction fires.
    const c0 = stats.meta_kv_node_compactions.load();
    var rounds: u32 = 0;
    while (rounds < 64) : (rounds += 1) {
        for (0..200) |i| try t.insert(&ikey(i), tree.testValue(i * 7 + rounds, 24, &buf));
        _ = try vol.drain(t);
        try testing.expectEqual(c0, stats.meta_kv_node_compactions.load());
        const n = try vol.cache.get(t.rootPtr().addr);
        defer n.release();
        if (leaf_merge.leafOccupancy(t, n).deadPermille() >= DEFRAG_COMPACT_DEAD_PERMILLE) break;
    }
    try testing.expect(rounds < 64);
    try testing.expectEqual(@as(u64, 500), DEFRAG_COMPACT_DEAD_PERMILLE);
    const pending0 = vol.alloc.pendingCount();
    {
        const n = try vol.cache.get(t.rootPtr().addr);
        defer n.release();
        var out: MaintenanceOutcome = .{};
        try testing.expect(try compactForDefrag(t, &vol.ctx, n, &out));
        try testing.expectEqual(@as(u64, 1), out.compactions);
        try testing.expectEqual(@as(u64, 0), out.splits);
        try testing.expect(n.state.isSuperseded());
        // A second nudge on the superseded object is the no-op.
        var again: MaintenanceOutcome = .{};
        try testing.expect(!try compactForDefrag(t, &vol.ctx, n, &again));
        try testing.expectEqual(@as(u64, 0), again.compactions);
    }
    try testing.expectEqual(c0 + 1, stats.meta_kv_node_compactions.load());
    try testing.expect(t.rootPtr().addr != root0.addr);
    try testing.expectEqual(pending0 + 1, vol.alloc.pendingCount());
    {
        const n = try vol.cache.get(t.rootPtr().addr);
        defer n.release();
        // A fresh image is ONE frame of its fold: 0 dead (the measure is
        // frame-rounded on both sides, so a leaf the nudge cannot shrink is
        // never a candidate again).
        const occ = leaf_merge.leafOccupancy(t, n);
        try testing.expectEqual(@as(u64, 0), occ.deadPermille());
        try testing.expectEqual(@as(u64, 0), occ.deadBytes());
    }
    var i: u64 = 0;
    while (i < 200) : (i += 13) try expectLookup(t, &ikey(i), tree.testValue(i * 7 + rounds, 24, &buf));
}

test "try-only reads (decision 0003 §AB.1): a lookup whose leaf the cache does not hold is WouldBlock — never a demand page — while a resident path serves; the blocking lookup then pages the leaf in and the try-only repeat serves it" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .extents = 512 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    const n = try vol.growToRootLevel(t, 1, 32, 4096, 4_000);
    try vol.flushDirty(t);
    const vbuf = try gpa.alloc(u8, 4096);
    defer gpa.free(vbuf);
    var fresh = try vol.freshCache();
    defer fresh.deinit();
    // The reopen pages in the root (an interior node) alone.
    const reopened = try KvTree.open(gpa, fresh, .native, t.rootPtr(), vol.seq);
    defer reopened.destroy();
    var kb: [32]u8 = undefined;
    const k = wideKey(n / 2, 32, &kb);
    {
        node_cache.try_only = true;
        defer node_cache.try_only = false;
        try testing.expectError(error.WouldBlock, reopened.lookup(gpa, k));
    }
    try expectLookup(reopened, k, tree.testValue(n / 2, 4096, vbuf));
    node_cache.try_only = true;
    defer node_cache.try_only = false;
    const got = (try reopened.lookup(gpa, k)).?;
    defer gpa.free(got);
    try testing.expectEqualSlices(u8, tree.testValue(n / 2, 4096, vbuf), got);
}

test "interior_recursion_grows_root_to_depth_three: the level-1 root splits itself; every key resolves through two interior levels and streams in order" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .extents = 2048 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    // 32-byte keys ⇒ 63 B separators: the level-1 root's fold passes
    // `foldCapacity` after 61,376 / 63 = 974 leaves of ~7–14 four-KiB
    // records, and it splits itself.
    const n = try vol.growToRootLevel(t, 2, 32, 4096, 60_000);
    try testing.expectEqual(@as(u8, 2), try t.rootLevel());
    const vbuf = try gpa.alloc(u8, 4096);
    defer gpa.free(vbuf);
    var p: u64 = 0;
    while (p < n) : (p += @max(n / 97, 1)) {
        var kb: [32]u8 = undefined;
        try expectLookup(t, wideKey(p, 32, &kb), tree.testValue(p, 4096, vbuf));
    }
    var all: std.ArrayList(KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &all);
    var lo: [32]u8 = undefined;
    var hi: [32]u8 = undefined;
    try t.range(gpa, wideKey(0, 32, &lo), wideKey(n, 32, &hi), std.math.maxInt(usize), &all);
    try testing.expectEqual(n, all.items.len);
    for (all.items, 0..) |kv, i| {
        var kb: [32]u8 = undefined;
        try testing.expectEqualSlices(u8, wideKey(i, 32, &kb), kv.key);
    }
}

test "stale_writer_revalidates_and_retries_after_smo: a leaf resolved before an SMO is superseded; applyAt answers stale and counts; the writer loop lands on the successor" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    for (0..200) |i| try t.insert(&ikey(i), tree.testValue(i, 24, &buf));
    _ = try vol.drain(t);
    const stale_leaf = try t.resolveLeaf(&ikey(50));
    defer stale_leaf.release();
    const c0 = stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load();
    var spins: u64 = 0;
    while (stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() == c0) : (spins += 1) {
        for (0..200) |i| try t.insert(&ikey(i), tree.testValue(i * 7 + spins, 24, &buf));
        _ = try vol.drain(t);
        try testing.expect(spins < 10_000);
    }
    try testing.expect(stale_leaf.state.isSuperseded());
    const r0 = tree.stats.meta_kv_commit_smo_retries.load();
    try testing.expectEqual(tree.ApplyOutcome.stale, try t.applyAt(stale_leaf, &ikey(50), .put, tree.testValue(4242, 24, &buf)));
    try testing.expectEqual(r0 + 1, tree.stats.meta_kv_commit_smo_retries.load());
    try t.insert(&ikey(50), tree.testValue(4242, 24, &buf));
    try expectLookup(t, &ikey(50), tree.testValue(4242, 24, &buf));
}

test "reopen_from_disk_after_flush_serves_everything: flushDirty makes the volume reopenable from the images alone (tombstones included)" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{});
    defer vol.deinit();
    const n: u64 = 800;
    var buf: [64]u8 = undefined;
    const root = blk: {
        const t = try vol.createTree(.native);
        for (0..n) |i| try t.insert(&ikey(i), tree.testValue(i, 48, &buf));
        var i: u64 = 0;
        while (i < n) : (i += 5) try t.delete(&ikey(i));
        try vol.flushDirty(t);
        break :blk t.rootPtr();
    };
    const cache = try vol.freshCache();
    defer cache.deinit();
    var seq2 = node_seq.NodeSeqHandle.shared(1 << 32);
    const t2 = try KvTree.open(gpa, cache, .native, root, &seq2);
    defer t2.destroy();
    for (0..n) |i| {
        if (i % 5 == 0) try expectLookup(t2, &ikey(i), null) else try expectLookup(t2, &ikey(i), tree.testValue(i, 48, &buf));
    }
    var all: std.ArrayList(KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &all);
    try t2.range(gpa, &ikey(0), &ikey(n), std.math.maxInt(usize), &all);
    try testing.expectEqual(n - (n + 4) / 5, all.items.len);
}

test "split_vs_commit_storm_loses_nothing: 8 writers against the serialized SMO task; every key present exactly once in order; the storm split" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .extents = 1024, .writeback_delta_bytes = 2048 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    const s0 = stats.meta_kv_node_splits.load();
    const WRITERS: u64 = 8;
    const PER_WRITER: u64 = 1500;
    const Shared = struct {
        done: std.atomic.Value(bool) = .init(false),
        failed: std.atomic.Value(bool) = .init(false),
    };
    var shared: Shared = .{};
    const Smo = struct {
        fn run(v: *TestVolume, tr: *KvTree, sh: *Shared) void {
            while (true) {
                _ = runMaintenance(tr, &v.ctx) catch {
                    sh.failed.store(true, .release);
                    return;
                };
                if (sh.done.load(.acquire) and !tr.maintenancePending()) return;
                if (!tr.maintenancePending()) std.Thread.yield() catch {};
            }
        }
    };
    const Writer = struct {
        fn run(tr: *KvTree, w: u64, sh: *Shared) void {
            var buf: [64]u8 = undefined;
            var got: [64]u8 = undefined;
            for (0..PER_WRITER) |i| {
                const n = w * PER_WRITER + i;
                tr.insert(&ikey(n), tree.testValue(n, 48, &buf)) catch {
                    sh.failed.store(true, .release);
                    return;
                };
                if (i % 7 == 0) {
                    const v = tr.lookup(std.heap.page_allocator, &ikey(n)) catch {
                        sh.failed.store(true, .release);
                        return;
                    };
                    defer if (v) |x| std.heap.page_allocator.free(x);
                    if (v == null or !std.mem.eql(u8, v.?, tree.testValue(n, 48, &got))) sh.failed.store(true, .release);
                }
            }
        }
    };
    const smo_thread = try std.Thread.spawn(.{}, Smo.run, .{ &vol, t, &shared });
    var ws: [WRITERS]std.Thread = undefined;
    for (&ws, 0..) |*w, i| w.* = try std.Thread.spawn(.{}, Writer.run, .{ t, @as(u64, i), &shared });
    for (ws) |w| w.join();
    shared.done.store(true, .release);
    smo_thread.join();
    try testing.expect(!shared.failed.load(.acquire));
    var all: std.ArrayList(KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &all);
    try t.range(gpa, &ikey(0), &ikey(WRITERS * PER_WRITER), std.math.maxInt(usize), &all);
    try testing.expectEqual(WRITERS * PER_WRITER, all.items.len);
    var buf: [64]u8 = undefined;
    for (all.items, 0..) |kv, n| {
        try testing.expectEqualSlices(u8, &ikey(n), kv.key);
        try testing.expectEqualSlices(u8, tree.testValue(n, 48, &buf), kv.value);
    }
    try testing.expect(stats.meta_kv_node_splits.load() > s0);
}

test "scale: ≥ 100k keys at 64 KiB nodes ⇒ depth ≤ 3 (the fanout tie), every key found, the walk streams them all" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .extents = 1024 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    const KEYS: u64 = 100_000;
    var buf: [64]u8 = undefined;
    for (0..KEYS) |i| {
        try t.insert(&ikey(i), tree.testValue(i, 24, &buf));
        if (i % 512 == 511) _ = try vol.drain(t);
        if (i % 8192 == 8191) vol.coverEverything();
    }
    try vol.flushDirty(t);
    const level = try t.rootLevel();
    try testing.expect(level <= 2);
    // The tie: 100k records of 47 B at the ¾ fill ≈ 102 leaves ≪ fanout.
    const leaves_bound = KEYS * 47 / vol.layout.splitPartCapacity() + 1;
    try testing.expect(leaves_bound < interiorFanout(vol.layout, 8));
    try testing.expectEqual(@as(u8, 1), level);
    for (0..KEYS) |i| try expectLookup(t, &ikey(i), tree.testValue(i, 24, &buf));
    var all: std.ArrayList(KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &all);
    try t.range(gpa, &ikey(0), &ikey(KEYS), std.math.maxInt(usize), &all);
    try testing.expectEqual(KEYS, all.items.len);
}

test "the dying floor of a root swap pins the tail at the entry start; a root swap journals no pointer record; a non-root SMO journals Put(part.max → succ) per part and the alloc/free records" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    // Fill the root leaf until it splits (a root swap).
    var n: u64 = 0;
    while (try t.rootLevel() == 0) {
        for (0..64) |_| {
            try vol.commit(t, &ikey(n), .put, tree.testValue(n, 40, &buf));
            n += 1;
        }
        _ = try vol.drain(t);
    }
    const swap = vol.journal.last_entry.?;
    try testing.expectEqual(swap.start, t.rootFloor());
    // The accumulator holds the min of the swap's position and the retired
    // root's own floor (its first commit's entry start, position 0 here):
    // both clamp the next tail.
    const dying = vol.cache.takeDyingFloors();
    try testing.expect(dying <= swap.start);
    try testing.expectEqual(@as(u64, 0), dying);
    // The root-grow entry: alloc × 3 (two parts + the new root) + free, no
    // pointer record (level-0 tags only).
    var recovery = try journal.replayScanImage(gpa, vol.ring_image, vol.journal.geo, 0, 0);
    defer recovery.deinit();
    var found = false;
    for (recovery.entries) |e| {
        if (e.seq != swap.start) continue;
        found = true;
        var it = e.records();
        var allocs: u32 = 0;
        var frees: u32 = 0;
        while (it.next()) |r| {
            const tag = journal.untag(r.tag);
            try testing.expectEqual(@as(u8, 0), tag.level);
            try testing.expectEqual(alloc_ext.tree_alloc_reserved, tag.tree_id);
            const d = try alloc_ext.decodeAllocRecord(r.key, r.value, null);
            switch (d) {
                .allocated => allocs += 1,
                .freed => frees += 1,
            }
        }
        try testing.expectEqual(@as(u32, 3), allocs);
        try testing.expectEqual(@as(u32, 1), frees);
    }
    try testing.expect(found);
    // Now a LEAF compaction under the interior root journals its pointer.
    const c0 = stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load();
    var spins: u64 = 0;
    while (stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() == c0) : (spins += 1) {
        for (0..64) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i * 3 + spins, 40, &buf));
        _ = try vol.drain(t);
        try testing.expect(spins < 10_000);
    }
    const leaf_smo = vol.journal.last_entry.?;
    var recovery2 = try journal.replayScanImage(gpa, vol.ring_image, vol.journal.geo, 0, 0);
    defer recovery2.deinit();
    const interior = try journal.interiorRecordsSorted(gpa, recovery2.entries);
    defer gpa.free(interior);
    var ptrs: u32 = 0;
    for (interior) |r| {
        if (r.entry_seq != leaf_smo.start) continue;
        ptrs += 1;
        try testing.expectEqual(@as(u8, 1), r.level);
        try testing.expectEqual(key.KIND_INTERIOR, r.tree_id);
        try testing.expectEqual(journal.RecordKind.put, r.record.kind);
        try testing.expectEqual(@as(u32, 0), r.index); // the flips are records 0..n
        const iv = try key.InteriorValue.decode(r.record.value);
        const succ = try vol.cache.get(iv.childAddr());
        defer succ.release();
        try testing.expectEqual(iv.childSeq(), succ.node_seq);
        // A slot tree's pointer record rides `slot ‖ separator`.
        const jk = try key.splitInteriorJournalKey(r.record.key);
        try testing.expectEqual(key.NATIVE_FOREST_SLOT, jk.slot);
        try testing.expectEqualSlices(u8, succ.max_key, jk.separator);
    }
    try testing.expectEqual(@as(u32, 1), ptrs);
}

test "rightmost_separator_pointer_record_replays_clean: an SMO of the rightmost leaf journals KEY_SPACE_MAX and replay admits it" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() == 0) {
        for (0..64) |_| {
            try vol.commit(t, &ikey(n), .put, tree.testValue(n, 40, &buf));
            n += 1;
        }
        _ = try vol.drain(t);
    }
    const led = try vol.checkpoint();
    // Churn the RIGHTMOST leaf (the highest keys) until it compacts: its
    // pointer record's key is KEY_SPACE_MAX.
    const c0 = stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load();
    var spins: u64 = 0;
    while (stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() == c0) : (spins += 1) {
        for (n - 32..n) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i * 5 + spins, 40, &buf));
        _ = try vol.drain(t);
        try testing.expect(spins < 10_000);
    }
    const smo_entry = vol.journal.last_entry.?;
    var recovery = try journal.replayScanImage(gpa, vol.ring_image, vol.journal.geo, led.tail, 0);
    defer recovery.deinit();
    const interior = try journal.interiorRecordsSorted(gpa, recovery.entries);
    defer gpa.free(interior);
    var saw_top = false;
    for (interior) |r| {
        if (r.entry_seq == smo_entry.start and std.mem.eql(u8, r.record.key, t.interiorJournalKey(&key.KEY_SPACE_MAX).slice())) saw_top = true;
    }
    try testing.expect(saw_top);
    var re = try vol.crashReplay(led);
    defer re.deinit();
    try testing.expect(re.outcome.interior_applied >= 1);
    try testing.expectEqual(@as(u64, 0), re.outcome.interior_dropped);
    const t2 = re.treeOf(.native);
    for (n - 32..n) |i| try expectLookup(t2, &ikey(i), tree.testValue(i * 5 + spins - 1, 40, &buf));
    try testing.expectEqual(t.rootPtr(), t2.rootPtr());
}

/// The leaf a key routes to in `t`.
fn leafAddrOf(t: *KvTree, k: []const u8) !u64 {
    const leaf = try t.resolveLeaf(k);
    defer leaf.release();
    return leaf.addr;
}

test "SMO completeness: an SMO entry torn, or the ring cut right after it, replays to the OLD or the NEW routing — never a mix — with every acked key served" {
    const gpa = testing.allocator;
    var buf: [64]u8 = undefined;
    const Variant = enum { intact, torn_entry, cut_after };
    for ([_]Variant{ .intact, .torn_entry, .cut_after }) |variant| {
        var vol = try TestVolume.init(gpa, .{ .journaled = true });
        defer vol.deinit();
        const t = try vol.createTree(.native);
        var n: u64 = 0;
        while (try t.rootLevel() == 0) {
            for (0..64) |_| {
                try vol.commit(t, &ikey(n), .put, tree.testValue(n, 40, &buf));
                n += 1;
            }
            _ = try vol.drain(t);
        }
        // Two cycles: the first flushes and takes the floors, the second
        // drains the first's dying floors — the tail lands at the head, so
        // the window holds exactly what follows.
        _ = try vol.checkpoint();
        const led = try vol.checkpoint();
        try testing.expectEqual(vol.core.headPos(), led.tail);
        const root_at_ledger = t.rootPtr();
        // Churn one leaf's band until it compacts/splits: the SMO entry.
        const old_leaf = try leafAddrOf(t, &ikey(10));
        const c0 = stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load();
        var spins: u64 = 0;
        while (stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() == c0) : (spins += 1) {
            for (0..80) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i * 11 + spins, 40, &buf));
            _ = try vol.drain(t);
            try testing.expect(spins < 10_000);
        }
        const smo_entry = vol.journal.last_entry.?;
        const last_pre_smo = spins - 1;
        // Records AFTER the SMO: a few more puts into the moved band.
        for (0..8) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i * 1000 + 7, 40, &buf));
        try testing.expectEqual(root_at_ledger, t.rootPtr()); // no root swap since the ledger
        switch (variant) {
            .intact => {},
            .torn_entry => vol.tearEntry(smo_entry),
            .cut_after => vol.cutAfter(smo_entry.paddedEnd()),
        }
        var re = try vol.crashReplay(led);
        defer re.deinit();
        const t2 = re.treeOf(.native);
        const new_routing = re.outcome.interior_applied > 0;
        switch (variant) {
            .intact, .cut_after => try testing.expect(new_routing),
            .torn_entry => try testing.expect(!new_routing),
        }
        // Never a mix: every key of the band routes to the old leaf iff the
        // routing is old; the old leaf's extent is pending-free (never
        // reused), so the old route still serves.
        for (0..80) |i| {
            const leaf = try leafAddrOf(t2, &ikey(i));
            try testing.expectEqual(!new_routing, leaf == old_leaf);
        }
        // Every acked key: the pre-SMO value stands wherever the crash cut
        // the post-SMO put (`cut_after` loses them all; a torn entry loses
        // the same-page successors of the dead chain and resyncs on the
        // next page), the post-SMO value wherever its entry survived.
        for (0..80) |i| {
            const got = (try t2.lookup(gpa, &ikey(i))) orelse return error.TestExpectedLive;
            defer gpa.free(got);
            const pre = tree.testValue(i * 11 + last_pre_smo, 40, &buf);
            var buf2: [64]u8 = undefined;
            const post = tree.testValue(i * 1000 + 7, 40, &buf2);
            const is_pre = std.mem.eql(u8, got, pre);
            const is_post = i < 8 and std.mem.eql(u8, got, post);
            switch (variant) {
                .intact => try testing.expect(if (i < 8) is_post else is_pre),
                .cut_after => try testing.expect(is_pre),
                .torn_entry => try testing.expect(is_pre or is_post),
            }
        }
        for (80..n) |i| try expectLookup(t2, &ikey(i), tree.testValue(i, 40, &buf));
    }
}

test "stranded_build_window_commits_survive_crash: commits racing an SMO's build window reach the successors as leftovers and replay through the FINAL routing" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() == 0) {
        for (0..64) |_| {
            try vol.commit(t, &ikey(n), .put, tree.testValue(n, 40, &buf));
            n += 1;
        }
        _ = try vol.drain(t);
    }
    const led = try vol.checkpoint();
    // Dirty one band until its leaf's log is full and the next drain must
    // run its SMO — then arm the seam and run that drain on another thread.
    var spins: u64 = 0;
    while (true) : (spins += 1) {
        for (0..80) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i * 11 + spins, 40, &buf));
        const leaf = try t.resolveLeaf(&ikey(10));
        defer leaf.release();
        const full = blk: {
            const d = leaf.lockRead(testing.io);
            defer leaf.unlockRead(testing.io);
            break :blk d.projectedLogEnd(vol.layout, 0) > vol.layout.node_size;
        };
        if (full) break;
        _ = try vol.drain(t);
        try testing.expect(spins < 10_000);
    }
    const c0 = stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load();
    testSmoBuildPauseArm(t);
    defer testSmoBuildPauseRelease();
    const Smo = struct {
        fn run(v: *TestVolume, tr: *KvTree, failed: *std.atomic.Value(bool)) void {
            _ = v.drain(tr) catch failed.store(true, .release);
        }
    };
    var failed: std.atomic.Value(bool) = .init(false);
    const th = try std.Thread.spawn(.{}, Smo.run, .{ &vol, t, &failed });
    while (!test_smo_build_paused.load(.acquire)) std.Thread.yield() catch {};
    try testing.expectEqual(key.KIND_INTERIOR, test_smo_pause_info.tree_id);
    try testing.expect(!test_smo_pause_info.is_root);
    // The racing commits: ACKED puts inside the paused leaf's range, with
    // reservations BELOW the flip's.
    var racing: u64 = 0;
    for (0..80) |i| {
        const k = ikey(i);
        if (key.lessThan(&k, test_smo_pause_info.min_key.slice()) or key.lessThan(test_smo_pause_info.max_key.slice(), &k)) continue;
        try vol.commit(t, &k, .put, tree.testValue(i * 1000 + 99, 40, &buf));
        racing += 1;
    }
    try testing.expect(racing >= 8);
    testSmoBuildPauseRelease();
    th.join();
    try testing.expect(!failed.load(.acquire));
    try testing.expect(stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() > c0);
    // Live RAM serves the racing commits (the bounded second merge).
    for (0..80) |i| {
        const k = ikey(i);
        if (key.lessThan(&k, test_smo_pause_info.min_key.slice()) or key.lessThan(test_smo_pause_info.max_key.slice(), &k)) continue;
        try expectLookup(t, &k, tree.testValue(i * 1000 + 99, 40, &buf));
    }
    // Crash-equivalent reopen: two-phase replay routes them to the
    // successors (single-pass seq order would have stranded them in the
    // abandoned predecessor).
    var re = try vol.crashReplay(led);
    defer re.deinit();
    try testing.expect(re.outcome.interior_applied >= 1);
    const t2 = re.treeOf(.native);
    for (0..80) |i| {
        const k = ikey(i);
        if (key.lessThan(&k, test_smo_pause_info.min_key.slice()) or key.lessThan(test_smo_pause_info.max_key.slice(), &k)) continue;
        try expectLookup(t2, &k, tree.testValue(i * 1000 + 99, 40, &buf));
    }
}

test "replay_twice_digest_stable_across_smo_windows: two reopens of the same post-crash bytes fold to identical state" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() == 0) {
        for (0..64) |_| {
            try vol.commit(t, &ikey(n), .put, tree.testValue(n, 40, &buf));
            n += 1;
        }
        _ = try vol.drain(t);
    }
    const led = try vol.checkpoint();
    var prng = std.Random.DefaultPrng.init(0xD16E57);
    const rnd = prng.random();
    for (0..6) |round| {
        for (0..300) |_| {
            const i = rnd.uintLessThan(u64, n);
            if (rnd.uintLessThan(u8, 4) == 0) try vol.commit(t, &ikey(i), .delete, &.{}) else try vol.commit(t, &ikey(i), .put, tree.testValue(i + round * 7919, 40, &buf));
        }
        _ = try vol.drain(t);
    }
    var a = try vol.crashReplay(led);
    defer a.deinit();
    var b = try vol.crashReplay(led);
    defer b.deinit();
    try testing.expectEqual(a.outcome, b.outcome);
    try testing.expectEqual(a.treeOf(.native).rootPtr(), b.treeOf(.native).rootPtr());
    var la: std.ArrayList(KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &la);
    var lb: std.ArrayList(KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &lb);
    try a.treeOf(.native).range(gpa, &ikey(0), &ikey(n), std.math.maxInt(usize), &la);
    try b.treeOf(.native).range(gpa, &ikey(0), &ikey(n), std.math.maxInt(usize), &lb);
    try testing.expectEqual(la.items.len, lb.items.len);
    for (la.items, lb.items) |x, y| {
        try testing.expectEqualSlices(u8, x.key, y.key);
        try testing.expectEqualSlices(u8, x.value, y.value);
    }
}

test "an SMO entry write failure after the swap is the crash-equivalent hole: loud, counted, RAM stays authoritative, replay folds to the pre-SMO routing" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() == 0) {
        for (0..64) |_| {
            try vol.commit(t, &ikey(n), .put, tree.testValue(n, 40, &buf));
            n += 1;
        }
        _ = try vol.drain(t);
    }
    _ = try vol.checkpoint();
    const led = try vol.checkpoint();
    const old_leaf = try leafAddrOf(t, &ikey(10));
    vol.journal.test_fail_writes.store(1, .monotonic);
    const f0 = stats.meta_kv_smo_entry_write_failures.load();
    const c0 = stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load();
    var spins: u64 = 0;
    while (stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() == c0) : (spins += 1) {
        for (0..80) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i * 11 + spins, 40, &buf));
        _ = try vol.drain(t);
        try testing.expect(spins < 10_000);
    }
    try testing.expectEqual(f0 + 1, stats.meta_kv_smo_entry_write_failures.load());
    // RAM: the new routing serves everything.
    try testing.expect(try leafAddrOf(t, &ikey(10)) != old_leaf);
    for (0..80) |i| try expectLookup(t, &ikey(i), tree.testValue(i * 11 + spins - 1, 40, &buf));
    // Replay: the hole drops whole; the predecessor stays routed and serves.
    var re = try vol.crashReplay(led);
    defer re.deinit();
    try testing.expectEqual(@as(u64, 0), re.outcome.interior_applied);
    const t2 = re.treeOf(.native);
    try testing.expectEqual(old_leaf, try leafAddrOf(t2, &ikey(10)));
    for (0..80) |i| try expectLookup(t2, &ikey(i), tree.testValue(i * 11 + spins - 1, 40, &buf));
}

/// Interior (pointer) records in the ring image: every SMO that published
/// under an interior root wrote at least one.
fn interiorRecordCount(gpa: Allocator, vol: *TestVolume) !usize {
    var recovery = try journal.replayScanImage(gpa, vol.ring_image, vol.journal.geo, 0, 0);
    defer recovery.deinit();
    const interior = try journal.interiorRecordsSorted(gpa, recovery.entries);
    defer gpa.free(interior);
    return interior.len;
}

/// A barrier sink that fails its next `fail_next` calls (the SMO's
/// images-before-pointers barrier).
const FailingBarrier = struct {
    fail_next: u32 = 0,
    calls: u32 = 0,

    fn sink(self: *FailingBarrier) checkpoint_cycle.BarrierSink {
        return .{ .ctx = self, .barrier = barrier };
    }

    fn barrier(ctx: *anyopaque) checkpoint_cycle.IoError!void {
        const self: *FailingBarrier = @ptrCast(@alignCast(ctx));
        self.calls += 1;
        if (self.fail_next > 0) {
            self.fail_next -= 1;
            return error.Io;
        }
    }
};

test "the SMO's pre-lock barrier failure fails the SMO loud: BarrierFailed, no pointer record in the ring, the claimed extents released, the routing unchanged, one barrier failure on the latch; the retry completes it" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true });
    defer vol.deinit();
    var latch: journal.FailureLatch = .{};
    vol.journal.latch = &latch;
    var fb: FailingBarrier = .{};
    vol.journal.barrier_hook = fb.sink();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    var n: u64 = 0;
    while (try t.rootLevel() == 0) {
        for (0..64) |_| {
            try vol.commit(t, &ikey(n), .put, tree.testValue(n, 40, &buf));
            n += 1;
        }
        _ = try vol.drain(t);
    }
    _ = try vol.checkpoint();
    const old_leaf = try leafAddrOf(t, &ikey(10));
    const ptrs0 = try interiorRecordCount(gpa, &vol);
    const free0 = vol.alloc.freeExtents();
    const c0 = stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load();
    const failures0 = stats.meta_kv_smo_barrier_failures.load();
    // Commits onto the leaf until a drain needs an SMO; the SMO's barrier fails.
    fb.fail_next = 1;
    var spins: u64 = 0;
    var failed = false;
    while (!failed and stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() == c0) : (spins += 1) {
        for (0..80) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i * 11 + spins, 40, &buf));
        _ = vol.drain(t) catch |e| blk: {
            try testing.expectEqual(error.BarrierFailed, e);
            failed = true;
            break :blk MaintenanceOutcome{};
        };
        try testing.expect(spins < 10_000);
    }
    try testing.expect(failed);
    try testing.expectEqual(c0, stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load());
    try testing.expectEqual(failures0 + 1, stats.meta_kv_smo_barrier_failures.load());
    try testing.expectEqual(@as(u32, 1), latch.barrier_failures.load(.acquire));
    try testing.expect(!latch.isFailed());
    // Nothing published: no pointer record, the images' extents back, the
    // old routing.
    try testing.expectEqual(ptrs0, try interiorRecordCount(gpa, &vol));
    try testing.expectEqual(free0, vol.alloc.freeExtents());
    try testing.expectEqual(old_leaf, try leafAddrOf(t, &ikey(10)));
    for (0..80) |i| try expectLookup(t, &ikey(i), tree.testValue(i * 11 + spins - 1, 40, &buf));
    // The node stays dirty for the retry: the next drain's barrier succeeds
    // and the SMO completes, resetting the latch's barrier run.
    try vol.flushDirty(t);
    try testing.expect(stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() > c0);
    try testing.expect(try interiorRecordCount(gpa, &vol) > ptrs0);
    try testing.expectEqual(@as(u32, 0), latch.barrier_failures.load(.acquire));
    try testing.expect(fb.calls >= 2);
    for (0..80) |i| try expectLookup(t, &ikey(i), tree.testValue(i * 11 + spins - 1, 40, &buf));
}

test "property: random puts/deletes with SMOs interleaved ⇒ lookup ≡ an AutoHashMap oracle; replay of the ring over the images reproduces the final routing and the oracle" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true, .extents = 256 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var oracle = std.AutoHashMap(u64, u64).init(gpa);
    defer oracle.deinit();
    var buf: [256]u8 = undefined;
    var prng = std.Random.DefaultPrng.init(0x5EED_0FF);
    const rnd = prng.random();
    const KEYS: u64 = 900;
    const OPS: usize = 6000;
    var led: ?TestVolume.Ledger = null;
    var ops: usize = 0;
    while (ops < OPS) : (ops += 1) {
        const i = rnd.uintLessThan(u64, KEYS);
        if (rnd.uintLessThan(u8, 10) < 3) {
            try vol.commit(t, &ikey(i), .delete, &.{});
            _ = oracle.remove(i);
        } else {
            // A 56-bit seed: the oracle packs `seed << 8 | (len − 40)`.
            const seed = rnd.int(u64) >> 8;
            const len = 40 + rnd.uintLessThan(usize, 160);
            try vol.commit(t, &ikey(i), .put, tree.testValue(seed, len, &buf));
            try oracle.put(i, (seed << 8) | @as(u64, @intCast(len - 40)));
        }
        if (ops % 97 == 96) _ = try vol.drain(t);
        if (ops == OPS / 3) led = try vol.checkpoint();
        if (ops % 1500 == 1499) {
            for (0..KEYS) |k| try expectOracle(t, k, oracle.get(k), &buf);
        }
    }
    _ = try vol.drain(t);
    for (0..KEYS) |k| try expectOracle(t, k, oracle.get(k), &buf);
    try testing.expect(stats.meta_kv_node_compactions.load() + stats.meta_kv_node_splits.load() > 0);
    // The replay over the images from the mid-run ledger.
    var re = try vol.crashReplay(led.?);
    defer re.deinit();
    const t2 = re.treeOf(.native);
    for (0..KEYS) |k| try expectOracle(t2, k, oracle.get(k), &buf);
    if (t.rootPtr().addr == led.?.roots[0].node_addr) {
        // No root swap since the ledger: the FINAL routing is reproduced
        // exactly — every key resolves to the same leaf address.
        for (0..KEYS) |k| try testing.expectEqual(try leafAddrOf(t, &ikey(k)), try leafAddrOf(t2, &ikey(k)));
    }
}

fn expectOracle(t: *KvTree, k: u64, want: ?u64, buf: []u8) !void {
    const got = try t.lookup(testing.allocator, &ikey(k));
    defer if (got) |g| testing.allocator.free(g);
    if (want) |w| {
        const seed = w >> 8;
        const len: usize = @intCast((w & 0xFF) + 40);
        try testing.expectEqualSlices(u8, tree.testValue(seed, len, buf), got orelse return error.TestExpectedLive);
    } else {
        try testing.expectEqual(@as(?[]u8, null), got);
    }
}

test "checkpointFlushNode: freezes and takes the floor in one window, appends outside the lock, restores the floor when the append fails; a clean node is a no-op" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    const addr = t.rootPtr().addr;
    try checkpointFlushNode(t, &vol.ctx, addr); // clean: nothing to write
    for (0..20) |i| try t.insert(&ikey(i), tree.testValue(i, 24, &buf));
    const root_node = try vol.cache.get(addr);
    defer root_node.release();
    const floor0 = root_node.dirtyFloor();
    try testing.expect(floor0 != node_cache.no_floor);
    const appends0 = vol.ram.appends.load(.monotonic);
    try checkpointFlushNode(t, &vol.ctx, addr);
    try testing.expectEqual(appends0 + 1, vol.ram.appends.load(.monotonic));
    try testing.expectEqual(node_cache.no_floor, root_node.dirtyFloor());
    try testing.expect(!root_node.state.isDirty() and !root_node.state.isFreezing());
    // A failed append restores the floor and leaves the frozen delta parked.
    for (20..40) |i| try t.insert(&ikey(i), tree.testValue(i, 24, &buf));
    const floor1 = root_node.dirtyFloor();
    vol.ram.fail_appends.store(true, .monotonic);
    try testing.expectError(error.Io, checkpointFlushNode(t, &vol.ctx, addr));
    vol.ram.fail_appends.store(false, .monotonic);
    try testing.expectEqual(floor1, root_node.dirtyFloor());
    try testing.expect(root_node.state.isFreezing());
    try checkpointFlushNode(t, &vol.ctx, addr);
    try testing.expectEqual(node_cache.no_floor, root_node.dirtyFloor());
    for (0..40) |i| try expectLookup(t, &ikey(i), tree.testValue(i, 24, &buf));
}

test "runMaintenanceUntil: an expired deadline still runs the first item (the progress law) and hands the rest back" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{});
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    for (0..20) |i| try t.insert(&ikey(i), tree.testValue(i, 24, &buf));
    const addr = t.rootPtr().addr;
    t.enqueueMaintenance(addr);
    t.enqueueMaintenance(addr);
    t.enqueueMaintenance(addr);
    const out = try runMaintenanceUntil(t, &vol.ctx, clock.monotonicNs());
    try testing.expectEqual(@as(u64, 1), out.appends);
    try testing.expect(t.maintenancePending());
    const rest = try runMaintenance(t, &vol.ctx);
    try testing.expectEqual(@as(u64, 0), rest.appends); // spurious re-enqueues
    try testing.expect(!t.maintenancePending());
}

test "CheckpointSeam: the checkpoint's CacheFlusher appends in place, hands a full node to smoReplace (replaced with its image count, the floor taken), declines a foreign tree's node, and sweeps the volume lap" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var seam_ctx: CheckpointSeam = .{ .ctx = &vol.ctx, .trees = vol.table };
    var flusher: checkpoint_cycle.CacheFlusher = .{ .cache = vol.cache, .smo = seam_ctx.seam() };
    const sink = flusher.sink();
    var buf: [64]u8 = undefined;
    for (0..40) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i, 24, &buf));
    const root_node = try vol.cache.get(t.rootPtr().addr);
    defer root_node.release();
    // An in-place append; the checkpoint mode takes the floor.
    const r1 = sink.flush(sink.ctx, root_node, .{ .mode = .checkpoint });
    try testing.expectEqual(checkpoint_cycle.FlushDisposition.appended, r1.disposition);
    try testing.expectEqual(node_cache.no_floor, root_node.dirtyFloor());
    // Fill the log, then the flush step hands the node to the seam.
    var spins: u64 = 0;
    while (true) : (spins += 1) {
        for (0..80) |i| try vol.commit(t, &ikey(i), .put, tree.testValue(i * 11 + spins, 40, &buf));
        const d = root_node.lockRead(testing.io);
        const full = d.projectedLogEnd(vol.layout, 0) > vol.layout.node_size;
        root_node.unlockRead(testing.io);
        if (full) break;
        const r = sink.flush(sink.ctx, root_node, .{ .mode = .threshold });
        try testing.expectEqual(checkpoint_cycle.FlushDisposition.appended, r.disposition);
        try testing.expect(spins < 10_000);
    }
    const g0 = stats.meta_kv_root_grows.load() + stats.meta_kv_node_compactions.load();
    const r2 = sink.flush(sink.ctx, root_node, .{ .mode = .checkpoint, .keep_exempt = true });
    try testing.expectEqual(checkpoint_cycle.FlushDisposition.replaced, r2.disposition);
    try testing.expect(r2.images >= 1);
    try testing.expect(root_node.state.isSuperseded());
    try testing.expectEqual(g0 + 1, stats.meta_kv_root_grows.load() + stats.meta_kv_node_compactions.load());
    try testing.expect(!vol.ctx.keep_exempt);
    for (0..80) |i| try expectLookup(t, &ikey(i), tree.testValue(i * 11 + spins, 40, &buf));
    // A node of a tree the seam does not hold is declined.
    var empty_table = try tree.TreeTable.init(gpa, &.{});
    defer empty_table.deinit();
    var foreign: CheckpointSeam = .{ .ctx = &vol.ctx, .trees = &empty_table };
    const nl = try t.resolveLeaf(&ikey(0));
    defer nl.release();
    try testing.expectEqual(checkpoint_cycle.SmoOutcome.declined, foreign.seam().maintain(&foreign, nl, .{ .keep_exempt = false, .forced_retirement = true }));
    // The volume lap through the seam reaches a complete lap on a tree with
    // nothing to merge.
    const report = sweepThrough(&seam_ctx);
    try testing.expect(report.lap_complete);
    try testing.expectEqual(@as(u64, 0), report.merges);
}

fn sweepThrough(seam_ctx: *CheckpointSeam) checkpoint_cycle.SweepReport {
    const s = seam_ctx.seam();
    return s.mergeUnderfull(s.ctx, clock.monotonicNs() + 10 * std.time.ns_per_s);
}

test "threshold SMO under the FIFO valve refuses PendingFreeFull pre-swap with nothing published; the flush pass's forced retirement parks in the overflow" {
    const gpa = testing.allocator;
    var vol = try TestVolume.init(gpa, .{ .journaled = true, .pending_cap = 2 });
    defer vol.deinit();
    const t = try vol.createTree(.native);
    var buf: [64]u8 = undefined;
    const Fill = struct {
        fn run(v: *TestVolume, tr: *KvTree, b: []u8, salt: u64) !void {
            var spins: u64 = 0;
            while (true) : (spins += 1) {
                for (0..80) |i| try v.commit(tr, &ikey(i), .put, tree.testValue(i * 11 + spins + salt, 40, b));
                const leaf = try tr.resolveLeaf(&ikey(10));
                defer leaf.release();
                const d = leaf.lockRead(testing.io);
                const full = d.projectedLogEnd(v.layout, 0) > v.layout.node_size;
                leaf.unlockRead(testing.io);
                if (full) return;
                _ = try v.drain(tr);
                try testing.expect(spins < 10_000);
            }
        }
    };
    // Two compactions fill the 2-slot FIFO (nothing covers them).
    for (0..2) |r| {
        try Fill.run(&vol, t, &buf, r * 100_000);
        _ = try vol.drain(t);
    }
    try testing.expectEqual(@as(u64, 2), vol.alloc.pendingCount());
    try testing.expect(!vol.ctx.pendingHasRoomFor(1));
    // The third, under the valve: refused pre-swap, clean.
    try Fill.run(&vol, t, &buf, 300_000);
    const free0 = vol.alloc.freeExtents();
    const root0 = t.rootPtr();
    const addr = try leafAddrOf(t, &ikey(10));
    try testing.expectError(error.PendingFreeFull, runMaintenance(t, &vol.ctx));
    try testing.expectEqual(free0, vol.alloc.freeExtents());
    try testing.expectEqual(root0, t.rootPtr());
    try testing.expectEqual(addr, try leafAddrOf(t, &ikey(10)));
    try testing.expect(vol.ctx.last_refusal == .pending_free_full);
    // The flush pass: forced retirement overflows instead of refusing.
    const o0 = alloc_ext.meta_kv_pending_free_overflow.load();
    try checkpointFlushNode(t, &vol.ctx, addr);
    try testing.expectEqual(o0 + 1, alloc_ext.meta_kv_pending_free_overflow.load());
    try testing.expectEqual(@as(u64, 3), vol.alloc.pendingCount());
    // Coverage drains FIFO and overflow alike.
    vol.coverEverything();
    try testing.expectEqual(@as(u64, 0), vol.alloc.pendingCount());
}
