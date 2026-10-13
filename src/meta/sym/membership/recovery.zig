//! Dead-appender recovery — the §5.9 procedure as ONE driver (charter
//! §5.5 "Death and recovery (§5.9)"; `docs/porting/sym_wire_membership_
//! recovery.md` §3): per `Live`/`Recovering` page the ledger names dead
//! on a volume this mount manages, under the handover mutex and — from the
//! root install on — the SMO mutex:
//!
//!  0. decide on a FRESH read (the page still Live/Recovering under the
//!     same identity + term; the record still standing; a member the owner
//!     lists LIVE again retires the record and leaves the region alone);
//!  1. PREEMPT-AND-ABORT the dead registrant's key (the own-key guard for a
//!     co-located appender; a no-op on a file substrate: the loud posture)
//!     — `pr_fenced ≡ the meta preempt landed`;
//!  2. page `Live → Recovering{recovered_by_term = writer_term}` to both
//!     directory slots, barriered; the lessee's rule 4 SUSPENDED;
//!  3. read the ring ≥ `ledger_tail_seq`; tree 0's leases of the dead id
//!     and its grant record; slots the window names that tree 0 already
//!     holds Unleased are ABSORBED (a re-run past its tree-0 step); the
//!     three violation classes refuse loud;
//!  4. the RAM lease table marks the dead slots `Releasing{dead}` (first
//!     touches refused; the structural class the manager's from here);
//!     under the SMO mutex with the RAII rollback declared: the root =
//!     the page's entry when `root ≠ 0 ∧ g ≥ lease.g` else tree 0's —
//!     NEVER by node seq — installed writer-legal after DROPPING every
//!     cached node of the slot; cursor = `max(recorded, page, window max
//!     + 1)`; `seq_floor = max(recorded, dead frontier)`;
//!  5. two-phase replay (interior `(level DESC, seq)`, then content by
//!     key) in the structural class + the data-bitmap deltas;
//!  6. cycle until no recovered slot holds a dirty node (`COVER_CYCLES_MAX`);
//!  6b. the tails: EVERY leaf on a non-PR substrate (the full tail scan),
//!     the resident ones only under a device fence;
//!  7. tree 0, ONE control entry per slot: `Unleased{root as flushed,
//!     cursor, g, extents, last_written, seq_floor}` + `slot_tails` +
//!     `custody_quarantine` iff the record is EARLY + the grant record
//!     minus the trees' images;
//!  7b. the RAM table `Unleased` LAST (the rollback disarmed from here);
//!     ring 0's seq floor raised; the quarantine installed in RAM;
//!  7c. the unclaimed grant and the orphan images returned;
//!  8. page `Recovered{ledger_tail_seq = head}`, slots cleared, the
//!     suspension lifted — a `Recovered` ring is NEVER replayed;
//!  9. `recovered:{id, vol}` on volume 0; the open intents rolled forward
//!     by the set-level poll.
//!
//! The RAII rollback (§3.1): on ANY error between step 4 and 7b, per slot
//! begun, the table's `Releasing{dead} → Leased{dead}`, the stashed
//! structure put back, the slot's RAM tree DISCARDED and restored to its
//! pre-install `(root, floor)` (or removed when adopted fresh — an
//! installed floor published nowhere would pin ring 0's tail for ever);
//! the page stays `Recovering` for the re-run. PORTABLE over a `Volume`
//! seam; the contracts run over RAM images for every crash window.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const log = @import("../../../core/log.zig");
const checkpoint = @import("../../checkpoint.zig");
const cr = @import("../control_records.zig");
const page = @import("../appender_page.zig");
const manager = @import("../manager.zig");
const tails_spill = @import("../slots/tails_spill.zig");
const lt = @import("../slots/lease_table.zig");
const inos = @import("../../inos.zig");
const custody = @import("../../../data/sym_planes/custody.zig");
const records = @import("../../../data/sym_planes/records.zig");
const census_view = @import("census_view.zig");
const death_ledger = @import("death_ledger.zig");
const fencing = @import("fencing.zig");
const shard = @import("shard.zig");
const pr = @import("../../guard/pr.zig");

pub const WriterIdentity = records.WriterIdentity;
pub const LeaseTable = lt.LeaseTable;
pub const GrantRun = cr.GrantRun;

pub const stats = struct {
    pub var appender_recoveries: counters.ShardedCounter = .{};
    /// Exact-sum `preempt / read / replay / flush / tails / tree0 / total`.
    pub var appender_recovery_phase_ns: [Phase.count]counters.ShardedLatencyHistogram = @splat(.{});
    /// The derived bound in force (ring bytes ÷ 230 B; 800 files/leaf ×
    /// 120 µs × three passes; + one landing ceiling).
    pub var appender_recovery_bound_ms: counters.Gauge = .{};
    pub var recovery_full_tail_scan_bytes: counters.ShardedCounter = .{};
    /// A recovery that failed and rolled back (the re-run resumes).
    pub var appender_recovery_rollbacks: counters.ShardedCounter = .{};
    /// A page that moved under the poll's snapshot — skipped, never recovered.
    pub var appender_recovery_skipped: counters.ShardedCounter = .{};
    /// Slots a re-run found already Unleased in tree 0 (judged legal).
    pub var appender_recovery_absorbed: counters.ShardedCounter = .{};
    /// Page entries tree 0 no longer leases to the appender: dropped.
    pub var slot_lease_stale_entries: counters.ShardedCounter = .{};
    /// Images the dead appender wrote that no root reaches (fsck C13 for a
    /// dead appender), returned to the heap.
    pub var dead_appender_orphans: counters.ShardedCounter = .{};
    /// Extents of the dead grant its page never named as unclaimed and it
    /// never wrote (a refill after its last page write, a RAM claim, a run
    /// past the page's cap), returned to the heap — never orphan images.
    pub var dead_appender_unwritten_returned: counters.ShardedCounter = .{};
    pub var appender_clear_runs: counters.ShardedCounter = .{};
    pub var slot_custody_quarantines_installed: counters.ShardedCounter = .{};
    /// The window's three violation classes (must-stay-0).
    pub var meta_kv_replay_key_violations: counters.Tripwire = .init("meta_kv_replay_key_violations");
    pub var meta_kv_replay_lease_violations: counters.Tripwire = .init("meta_kv_replay_lease_violations");
    pub var meta_kv_replay_extent_violations: counters.Tripwire = .init("meta_kv_replay_extent_violations");
};

pub const Phase = enum(u8) {
    preempt,
    read,
    replay,
    flush,
    tails,
    tree0,
    total,

    pub const count = @typeInfo(Phase).@"enum".field_names.len;
};

// ---------------------------------------------------------------------------
// The bound (§3.2), derived.
// ---------------------------------------------------------------------------

/// The measured ≈ 230 B journal entry per create (design §1.4).
pub const ENTRY_BYTES: u64 = 230;
/// ≈ 330 B per inode + dentry + layout: 800 files per 256 KiB leaf.
pub const FILES_PER_256K_LEAF: u64 = 800;
/// The upper end of the measured 85–120 µs cold leaf read.
pub const LEAF_LOAD_US: u64 = 120;
/// The flush's cold loads, the tail scan's extent reads, the orphan
/// census's reads.
pub const LEAF_PASSES: u64 = 3;
/// A per-key LWW insert.
pub const FOLD_US_PER_ENTRY: u64 = 1;

/// The successor's coherence hold past a death record's ts (decision 0003
/// §U.2): a joiner drops the tokens a dead holder granted it only at the
/// census commit that retires the holder, and every member pulls the
/// owner's census once per renew interval with a view answering for
/// `VALIDITY_RENEWALS` of them — so by `ts + VALIDITY_RENEWALS × renew`
/// every member whose view stands has committed one that names the death
/// (a pull starts ≤ renew + one beat after the last and walks in RTTs:
/// two intervals cover one late pull); `2 × skew_max` absorbs the
/// recorder's wall clock against ours (the custody quarantine's term).
pub fn coherenceHoldMs(renew_ms: u64, skew_max_ms: u64) u64 {
    return census_view.VALIDITY_RENEWALS * renew_ms + 2 * skew_max_ms;
}

pub fn recoveryBoundMs(ring_bytes: u64, node_size: u64) u64 {
    const entries = ring_bytes / ENTRY_BYTES;
    const files_per_leaf = @max(FILES_PER_256K_LEAF * node_size / (256 << 10), 1);
    const leaves = std.math.divCeil(u64, entries, files_per_leaf) catch unreachable;
    const load_ms = std.math.divCeil(u64, leaves * LEAF_LOAD_US * LEAF_PASSES, 1000) catch unreachable;
    const fold_ms = std.math.divCeil(u64, entries * FOLD_US_PER_ENTRY, 1000) catch unreachable;
    return load_ms + fold_ms + shard.CHECKPOINT_LANDING_CEILING_MS;
}

// ---------------------------------------------------------------------------
// The volume seam.
// ---------------------------------------------------------------------------

/// The window's record classes: the three slot-tree kinds (a `delta` is
/// content exactly like a `put` — it folds into the key's newest base at
/// replay), the ring's extent claims, the data-bitmap deltas.
pub const Kind = enum(u8) {
    put,
    delta,
    delete,
    alloc,
    data_delta,

    /// A slot-tree record (level 0: content; > 0: structural).
    pub fn isTreeRecord(self: Kind) bool {
        return self == .put or self == .delta or self == .delete;
    }
};

/// One record of the dead ring's window as the driver judges it.
pub const WindowRecord = struct {
    slot: u16,
    /// 0 = content; > 0 = an interior (structural) record.
    level: u8,
    seq: u64,
    kind: Kind,
    /// The slot-tree record's local ino (level 0, put/delete).
    local_ino: u64 = 0,
    /// The record sits at the slot's INTENT HOME (`xv/intent.homeIno`:
    /// local 0, which no inode ever owns) — a cross-owner intent of the
    /// dead appender, legal at local 0.
    home: bool = false,
    /// The extent an `alloc` claims.
    extent: u64 = 0,
    /// An opaque handle the volume resolves at replay (the frame's bytes).
    handle: u64 = 0,
};

pub const RingRead = struct {
    /// The head the ring was read to (the page's `ledger_tail_seq` after).
    head: u64,
    /// The highest stamp the window carried (the slot's `seq_floor` term).
    seq_frontier: u64,
    /// The ring's seq offset in force.
    seq_offset: u64,
};

pub const TreeState = struct { root: cr.Root, floor: u64 };
pub const FlushedRoot = struct { root: cr.Root, extents: u32 };

pub const Error = error{ Io, Corrupt, OutOfMemory, Deferred, Violation, Refused };

pub const Volume = struct {
    ctx: *anyopaque,
    vt: *const VTable,

    pub const VTable = struct {
        vol: *const fn (ctx: *anyopaque) u16,
        /// The newest valid page of `appender_id` (a FRESH read).
        readPage: *const fn (ctx: *anyopaque, appender_id: u32) Error!?page.Page,
        /// Both directory slots + barrier (`write_foreign_page`).
        writePage: *const fn (ctx: *anyopaque, p: *const page.Page) Error!void,
        /// The window ≥ the page's tail; torn records past the head dropped.
        readRing: *const fn (ctx: *anyopaque, gpa: Allocator, p: *const page.Page, out: *std.ArrayList(WindowRecord)) Error!RingRead,
        slotState: *const fn (ctx: *anyopaque, slot: u16) Error!?cr.SlotState,
        /// Slots tree 0 leases to `appender_id` (`leased` or `releasing`).
        leasedTo: *const fn (ctx: *anyopaque, gpa: Allocator, appender_id: u32, out: *std.ArrayList(u16)) Error!void,
        grantRecord: *const fn (ctx: *anyopaque, gpa: Allocator, appender_id: u32, out: *std.ArrayList(GrantRun)) Error!void,
        /// ONE control entry, admitted DRAIN-AND-RETRY under the SMO hold.
        controlEntry: *const fn (ctx: *anyopaque, recs: []const manager.ControlRecord) Error!void,
        /// Step 7 landed `Unleased` for the slot: the volume's own mirrors
        /// of the published root and cursor follow (the C1b publisher must
        /// not rewrite the record with older words).
        noteUnleased: *const fn (ctx: *anyopaque, slot: u16, state: cr.SlotState) Error!void,
        table: *const fn (ctx: *anyopaque) *LeaseTable,
        /// The slot tree's RAM state (null = this mount holds no tree).
        treeState: *const fn (ctx: *anyopaque, slot: u16) ?TreeState,
        /// The cache barrier + the install: every cached node of the slot
        /// dropped, the root installed WRITER-LEGAL at `floor` (the root
        /// node's seq verified against the pointer).
        installRoot: *const fn (ctx: *anyopaque, slot: u16, root: cr.Root, floor: u64) Error!void,
        /// The rollback's arm: every cached node discarded (dirty ones
        /// included), the tree restored to `prior` or removed.
        restoreTree: *const fn (ctx: *anyopaque, slot: u16, prior: ?TreeState) void,
        /// The record applied in the structural class under the stamp.
        replay: *const fn (ctx: *anyopaque, rec: WindowRecord, stamp: fencing.FrameStamp) Error!void,
        /// The volume's SMO lock held (`true`) or released, from the first
        /// root install to the replay's last record: no SMO of the
        /// volume's own (the checkpoint task's maintenance and merge sweep,
        /// the D4 arm) reshapes a recovered tree in between. `installRoot`
        /// and `restoreTree` run inside it or take it themselves.
        holdStructure: *const fn (ctx: *anyopaque, held: bool) void,
        /// `data_bitmap.replay` for the dead identity's leases homed here.
        replayDataDeltas: *const fn (ctx: *anyopaque, identity: WriterIdentity, recs: []const WindowRecord) Error!void,
        /// Barriered cycles until no named slot holds a dirty node; the
        /// cycles run (`Corrupt` past `COVER_CYCLES_MAX`).
        cycleUntilClean: *const fn (ctx: *anyopaque, slots: []const u16) Error!u32,
        /// The current root as FLUSHED and the tree's image count.
        flushedRoot: *const fn (ctx: *anyopaque, slot: u16) Error!FlushedRoot,
        /// Every leaf's tail (non-PR: every leaf, a non-resident one costs
        /// one extent read — the bytes answered; PR: the resident ones).
        leafTails: *const fn (ctx: *anyopaque, gpa: Allocator, slot: u16, pr_fenced: bool, out: *std.ArrayList(cr.TailEntry)) Error!u64,
        /// From the first root install to the table release (steps 4–7b;
        /// decision 0003 §U.1): every image extent of `grant` an SMO on
        /// this volume retires is HELD by the recovery — its bit set, never
        /// parked in the coverage clock (`smo.RetireHold`) — installed
        /// under the SMO lock, so no retirement slips past it.
        holdRetirements: *const fn (ctx: *anyopaque, grant: []const GrantRun) Error!void,
        /// The hold ends. `park` (7b, nothing rolls back from here): every
        /// held extent parks in the coverage clock, gated on ring 0's head
        /// — at or past its own free record, and never below a gate the
        /// FIFO already took. A rollback drops the hold instead: the
        /// extents stay allocated (the restored tree may reach them).
        endRetirementHold: *const fn (ctx: *anyopaque, park: bool) void,
        /// Every extent of `grant` the volume still holds, read in ONE SMO
        /// lock window: the images ANY tree on the volume reaches (the
        /// recovered slots' and every other — the record keeps naming
        /// the images of a slot the dead appender released before dying),
        /// and the hold's retirements. The record keeps none of them and
        /// the census returns none of them.
        keptExtents: *const fn (ctx: *anyopaque, gpa: Allocator, grant: []const GrantRun, out: *std.ArrayList(u64)) Error!void,
        returnExtents: *const fn (ctx: *anyopaque, extents: []const u64) Error!void,
        /// A released region's ring segments back to the heap (RAM bits
        /// + `free` deltas), AFTER its page went Free.
        freeRing: *const fn (ctx: *anyopaque, segments: []const page.Extent) Error!void,
        /// The census of the grant's remainder outside `kept` (sorted:
        /// `keptExtents`'s answer) — every
        /// extent read once, the node-seq handle floored above every stamp
        /// it carries), judged by CONTENT: `orphans` = the extents carrying
        /// an image `appender_id` wrote (fsck C13 for a dead appender);
        /// `unwritten` = the rest — extents the appender never drew on (a
        /// grant after its last page write, a RAM claim, a run past the
        /// page's cap). Both are the caller's to return.
        orphanImages: *const fn (ctx: *anyopaque, gpa: Allocator, appender_id: u32, grant: []const GrantRun, kept: []const u64, orphans: *std.ArrayList(u64), unwritten: *std.ArrayList(u64)) Error!void,
        raiseSeqFloor: *const fn (ctx: *anyopaque, seq_floor: u64) void,
        ring0Head: *const fn (ctx: *anyopaque) u64,
        writerTerm: *const fn (ctx: *anyopaque) u64,
        /// The RAM custody quarantine (keyed by slot on this volume).
        quarantineCustody: *const fn (ctx: *anyopaque, slot: u16, until_realtime_ns: u64) void,
        /// The coherence hold (0003 §U.2): no first touch of the recovered
        /// slot before `until_realtime_ns` (the realtime deadline the
        /// record's clock set; the volume holds it on its own clock).
        holdCoherence: *const fn (ctx: *anyopaque, slot: u16, until_realtime_ns: u64) Error!void,
        /// The parked doors woken (`handover_done`).
        notifyHandoverDone: *const fn (ctx: *anyopaque) void,
        /// A slot's tails past the inline cap SPILLED into claimed extents
        /// (`slots/tails_spill`): the record and the `alloc` deltas the
        /// entry carries. Null = no spill home (the RAM fixtures): such a
        /// set is `Corrupt` here.
        spillTails: ?*const fn (ctx: *anyopaque, gpa: Allocator, tails: []const cr.TailEntry) Error!tails_spill.Spilled = null,
    };
};

/// What the driver reads of its mount.
pub const Mount = struct {
    identity: WriterIdentity,
    /// The metadata namespace's PR client (null on a file substrate).
    meta_pr: ?*pr.Client,
    /// The data namespaces this process holds WERO on.
    data_pr: []const *pr.Client = &.{},
    screen: *fencing.ScreenSource,
    stamps: *fencing.FrameStamps,
    ledger: *death_ledger.Ledger,
    /// The home shard's owner (step 0's "listed LIVE again").
    census: ?*const shard.Owner = null,
    skew_max_ms: u64,
    /// `coherenceHoldMs` of the shard's clocks: the recovered slots take
    /// no write before the record's ts + this.
    coherence_hold_ms: u64,
};

/// The crash-window seam: the driver fails right AFTER the named step's
/// durable effects (the re-run must converge from them).
pub const Step = enum(u8) { decide, preempt, page_recovering, read_ring, mark_dead, replay, flush, tails, tree0, table_release, returns, page_recovered, recovered_record };

pub const Seams = struct {
    fail_at: ?Step = null,
};

pub const Outcome = enum { acted, skipped, failed };

const SlotPlan = struct {
    slot: u16,
    g: u32,
    lease: cr.SlotState,
    /// The page entry (null = the page names no live entry for the slot).
    entry: ?page.SlotEntry,
    prior: ?TreeState,
    root: cr.Root,
    cursor: u64,
    extents: u32,
    seq_floor: u64,
    window_max_ino: u64 = 0,
    begun: bool = false,
};

pub const Driver = struct {
    gpa: Allocator,
    vol: Volume,
    mount: Mount,
    seams: Seams = .{},
    /// One recovery at a time per volume (the handover mutex's role).
    busy: bool = false,

    fn fail(self: *const Driver, step: Step) Error!void {
        if (self.seams.fail_at) |f| if (f == step) return error.Io;
    }

    /// `recover_dead_appenders` for ONE death on this volume: a no-op on a
    /// non-writer or when no page under the identity stands.
    pub fn recoverDead(self: *Driver, death: death_ledger.Death) Outcome {
        if (self.busy) return .skipped;
        self.busy = true;
        defer self.busy = false;
        const snapshot = self.findPage(death.identity) catch return .failed;
        const p = snapshot orelse return .skipped;
        if (p.identity.sameMount(.{ .node_token = self.mount.identity.node_token, .mount_slot = self.mount.identity.mount_slot, .writer_id = 0 })) return .skipped;
        if (p.state == .recovered) {
            // The recoverer died between the page's `Recovered` and the
            // record: the record is written here, idempotent (`completed`).
            const vol_ordinal = self.vol.vt.vol(self.vol.ctx);
            const fresh = self.mount.ledger.recordRecovered(death.identity, vol_ordinal, p.recovered_by_term, @intCast(clock.realtimeCoarseNs())) catch return .failed;
            return if (fresh) .acted else .skipped;
        }
        return self.recoverRegion(p, death) catch |e| {
            stats.appender_recovery_rollbacks.inc();
            log.warn("recovery: appender {d} (node 0x{x} mount slot {d}) on volume {d} failed at {s} — the page stays Recovering, the next poll re-runs from the durable state", .{ p.appender_id, p.identity.node_token, p.identity.mount_slot, self.vol.vt.vol(self.vol.ctx), @errorName(e) });
            return .failed;
        };
    }

    /// The non-Free page under the identity (a directory walk; the seam
    /// answers null past the directory's last page).
    fn findPage(self: *Driver, identity: WriterIdentity) Error!?page.Page {
        var id: u32 = 1;
        while (id < APPENDER_SCAN_MAX) : (id += 1) {
            const p = (try self.vol.vt.readPage(self.vol.ctx, id)) orelse continue;
            if (p.state == .free) continue;
            if (p.identity.node_token == identity.node_token and p.identity.mount_slot == identity.mount_slot) return p;
        }
        return null;
    }

    /// `regionPending`: a Live/Recovering page under the identity.
    pub fn regionPending(self: *Driver, identity: WriterIdentity) bool {
        const p = (self.findPage(identity) catch return true) orelse return false;
        return p.state == .live or p.state == .recovering;
    }

    /// `release_recovered_regions`: a `Recovered` page whose identity no
    /// `alloc_lease:` names any more goes `Free` (ring to the heap); the
    /// count released.
    pub fn releaseRecovered(self: *Driver, named_as_holder: *const fn (ctx: *anyopaque, identity: WriterIdentity) bool, ctx: *anyopaque) u32 {
        var n: u32 = 0;
        var id: u32 = 1;
        while (id < APPENDER_SCAN_MAX) : (id += 1) {
            const p = (self.vol.vt.readPage(self.vol.ctx, id) catch return n) orelse continue;
            if (p.state != .recovered) continue;
            const identity: WriterIdentity = .{ .node_token = p.identity.node_token, .mount_slot = p.identity.mount_slot, .writer_id = p.identity.writer_id };
            if (named_as_holder(ctx, identity)) continue;
            var freed = page.Page.free(p.appender_id, p.generation + 1, .{});
            freed.identity = p.identity;
            freed.term = p.term;
            freed.head_hint = p.head_hint;
            freed.ledger_tail_seq = p.head_hint;
            self.vol.vt.writePage(self.vol.ctx, &freed) catch return n;
            // Only THEN the ring's extents return to the heap (no durable
            // page names them any more) — the leave's pages-barrier-
            // release order; a crash between leaves C13's orphan class.
            self.vol.vt.freeRing(self.vol.ctx, p.segments.slice()) catch |e| log.warn("recovery: appender {d}'s ring could not return to the heap ({s}) — fsck C13 reclaims", .{ p.appender_id, @errorName(e) });
            n += 1;
        }
        return n;
    }

    fn phase(p: Phase, since_ns: u64) void {
        stats.appender_recovery_phase_ns[@backingInt(p)].record(clock.monotonicNs() -| since_ns);
    }

    /// `recover_region` (REC:1281-2157).
    fn recoverRegion(self: *Driver, snapshot: page.Page, death: death_ledger.Death) Error!Outcome {
        const t_total = clock.monotonicNs();
        const gpa = self.gpa;
        const vol_ordinal = self.vol.vt.vol(self.vol.ctx);
        // ---- 0. decide on a FRESH read ----
        var p = (try self.vol.vt.readPage(self.vol.ctx, snapshot.appender_id)) orelse return .skipped;
        if ((p.state != .live and p.state != .recovering) or !p.identity.sameMount(snapshot.identity) or p.term != snapshot.term) {
            stats.appender_recovery_skipped.inc();
            return .skipped;
        }
        const standing = (try self.mount.ledger.readDeath(death.identity)) orelse return .skipped;
        if (self.mount.census) |o| if (o.memberIsLive(shard.writerMemberId(death_ledger.wireOf(death.identity)), 0)) {
            self.mount.ledger.retireForJoin(death.identity) catch return error.Io;
            return .skipped;
        };
        try self.fail(.decide);
        // ---- 1. preempt ----
        const t_preempt = clock.monotonicNs();
        const meta = fencing.preemptDeadRegistrant(self.mount.meta_pr, standing.pr_key);
        const pr_fenced = meta == .fenced;
        if (meta != .own_key_guard) {
            for (self.mount.data_pr) |c| _ = fencing.preemptDeadRegistrant(c, standing.pr_key);
        }
        phase(.preempt, t_preempt);
        try self.fail(.preempt);
        // ---- 2. page Live → Recovering ----
        if (p.state == .live) {
            p.state = .recovering;
            p.recovered_by_term = self.vol.vt.writerTerm(self.vol.ctx);
            p.generation += 1;
            try self.vol.vt.writePage(self.vol.ctx, &p);
        }
        try self.mount.screen.suspendLessee(p.appender_id);
        defer self.mount.screen.resumeLessee(p.appender_id);
        try self.fail(.page_recovering);
        // ---- 3. read the ring from the page's tail; tree 0's leases ----
        const t_read = clock.monotonicNs();
        var window: std.ArrayList(WindowRecord) = .empty;
        defer window.deinit(gpa);
        const ring = try self.vol.vt.readRing(self.vol.ctx, gpa, &p, &window);
        var leased: std.ArrayList(u16) = .empty;
        defer leased.deinit(gpa);
        try self.vol.vt.leasedTo(self.vol.ctx, gpa, p.appender_id, &leased);
        var grant: std.ArrayList(GrantRun) = .empty;
        defer grant.deinit(gpa);
        try self.vol.vt.grantRecord(self.vol.ctx, gpa, p.appender_id, &grant);
        try self.screenWindow(window.items, leased.items, grant.items);
        // The RAM table follows tree 0 for the dead id's slots: a re-run
        // past its tree-0 step finds slots the rollback left `Leased{dead}`
        // in RAM that tree 0 already holds `Unleased` (absorbed above) —
        // the RAM word is reconciled here, never left refusing first
        // touches until the next mount's arm.
        const table = self.vol.vt.table(self.vol.ctx);
        {
            var held_ram: std.ArrayList(u16) = .empty;
            defer held_ram.deinit(gpa);
            try table.heldBy(p.appender_id, &held_ram);
            for (held_ram.items) |slot| {
                var still = false;
                for (leased.items) |l| still = still or l == slot;
                if (still) continue;
                const st = (try self.vol.vt.slotState(self.vol.ctx, slot)) orelse continue;
                if (st.variant == .unleased) try table.load(slot, st);
            }
        }
        phase(.read, t_read);
        try self.fail(.read_ring);
        // ---- 4. the RAM table marks the dead slots; the plans ----
        const t_replay = clock.monotonicNs();
        var plans: std.ArrayList(SlotPlan) = .empty;
        defer plans.deinit(gpa);
        for (leased.items) |slot| {
            const lease = (try self.vol.vt.slotState(self.vol.ctx, slot)) orelse continue;
            if (lease.variant == .unleased) continue;
            var entry: ?page.SlotEntry = null;
            for (p.slotEntries()) |e| if (e.slot == slot) {
                entry = e;
            };
            try plans.append(gpa, .{ .slot = slot, .g = lease.g, .lease = lease, .entry = entry, .prior = self.vol.vt.treeState(self.vol.ctx, slot), .root = lease.root, .cursor = lease.cursor, .extents = lease.slot_tree_extents, .seq_floor = lease.seq_floor });
        }
        // Stale page entries: the page names a slot tree 0 no longer
        // leases to this appender.
        for (p.slotEntries()) |e| {
            var named = false;
            for (plans.items) |pl| named = named or pl.slot == e.slot;
            if (!named) stats.slot_lease_stale_entries.inc();
        }
        for (window.items) |r| if (r.level == 0 and r.kind.isTreeRecord()) {
            for (plans.items) |*pl| if (pl.slot == r.slot) {
                pl.window_max_ino = @max(pl.window_max_ino, r.local_ino);
            };
        };
        var rollback: Rollback = .{ .driver = self, .plans = plans.items };
        errdefer rollback.run();
        // The dead grant's retirements are the recovery's from the first
        // install on: an SMO the volume runs on a recovered tree (the
        // flush cycles' compactions, the checkpoint task's maintenance,
        // the D4 arm) never hands a grant extent to the coverage clock
        // behind the census's back.
        try self.vol.vt.holdRetirements(self.vol.ctx, grant.items);
        rollback.hold = true;
        // The structure is the recovery's from the first root install to
        // the replay's last record (the charter's "from the root install on
        // — the SMO mutex"): an SMO of the volume's own between the install
        // and the dead window's interior records would mix a pre-split
        // image with the split's children.
        self.vol.vt.holdStructure(self.vol.ctx, true);
        var structure_held = true;
        defer if (structure_held) self.vol.vt.holdStructure(self.vol.ctx, false);
        for (plans.items) |*pl| {
            if (table.beginRelease(pl.slot, p.appender_id, true) == null) {
                const v = table.view(pl.slot);
                if (!(v.state == .releasing and v.holder == p.appender_id)) {
                    try table.load(pl.slot, pl.lease);
                    _ = table.beginRelease(pl.slot, p.appender_id, true);
                }
            }
            pl.begun = true;
            // The root by `g`, never by seq; the floor = ring 0's head.
            if (pl.entry) |e| if (e.root_addr != 0 and e.g >= pl.lease.g) {
                pl.root = .{ .addr = e.root_addr, .seq = e.root_seq };
                pl.extents = e.slot_tree_extents;
            };
            try self.vol.vt.installRoot(self.vol.ctx, pl.slot, pl.root, self.vol.vt.ring0Head(self.vol.ctx));
            const page_cursor: u64 = if (pl.entry) |e| e.cursor else 0;
            pl.cursor = @max(@max(pl.lease.cursor, page_cursor), if (pl.window_max_ino != 0) pl.window_max_ino + 1 else 0);
            pl.cursor = @max(pl.cursor, inos.LOCAL_INO_BASE);
            pl.seq_floor = @max(pl.lease.seq_floor, ring.seq_frontier);
            // Ring 0 stamps above the dead frontier BEFORE the volume writes
            // the recovered tree: the replay applies the dead window's
            // records at their own seqs, and an SMO's pointer flip stamped
            // below them (the flush cycles', the checkpoint task's) would
            // lose the per-key LWW to the dead appender's interior record
            // and route to the image it retired. Monotone: a rollback keeps
            // it.
            self.vol.vt.raiseSeqFloor(self.vol.ctx, pl.seq_floor);
        }
        try self.fail(.mark_dead);
        // ---- 5. two-phase replay + the data deltas ----
        var slot_list: std.ArrayList(u16) = .empty;
        defer slot_list.deinit(gpa);
        for (plans.items) |pl| try slot_list.append(gpa, pl.slot);
        try self.replayWindow(window.items, slot_list.items);
        self.vol.vt.holdStructure(self.vol.ctx, false);
        structure_held = false;
        try self.vol.vt.replayDataDeltas(self.vol.ctx, death.identity, window.items);
        phase(.replay, t_replay);
        try self.fail(.replay);
        // ---- 6. flush until clean ----
        const t_flush = clock.monotonicNs();
        _ = try self.vol.vt.cycleUntilClean(self.vol.ctx, slot_list.items);
        phase(.flush, t_flush);
        try self.fail(.flush);
        // ---- 6b. the tails ----
        const t_tails = clock.monotonicNs();
        var tails_per_slot: std.ArrayList(std.ArrayList(cr.TailEntry)) = .empty;
        defer {
            for (tails_per_slot.items) |*t| t.deinit(gpa);
            tails_per_slot.deinit(gpa);
        }
        for (plans.items) |pl| {
            var tails: std.ArrayList(cr.TailEntry) = .empty;
            errdefer tails.deinit(gpa);
            const bytes = try self.vol.vt.leafTails(self.vol.ctx, gpa, pl.slot, pr_fenced, &tails);
            stats.recovery_full_tail_scan_bytes.add(bytes);
            try tails_per_slot.append(gpa, tails);
        }
        phase(.tails, t_tails);
        try self.fail(.tails);
        // ---- 7. tree 0: one entry per slot ----
        const t_tree0 = clock.monotonicNs();
        // What the grant no longer owns: every tree's images and the
        // hold's retirements, sorted for the census's probes.
        var kept: std.ArrayList(u64) = .empty;
        defer kept.deinit(gpa);
        try self.vol.vt.keptExtents(self.vol.ctx, gpa, grant.items, &kept);
        std.mem.sort(u64, kept.items, {}, std.sort.asc(u64));
        const quarantine_until: ?u64 = if (standing.early) custody.custodyQuarantineUntilNs(standing.ts_realtime_ns, self.mount.skew_max_ms * std.time.ns_per_ms) else null;
        const coherence_until = standing.ts_realtime_ns +| self.mount.coherence_hold_ms * std.time.ns_per_ms;
        var tails_buf: [TAILS_INLINE_MAX * cr.TailEntryWire.LEN + cr.SlotTailsHead.LEN]u8 = undefined;
        for (plans.items, 0..) |*pl, i| {
            const flushed = try self.vol.vt.flushedRoot(self.vol.ctx, pl.slot);
            pl.root = flushed.root;
            pl.extents = flushed.extents;
            const state: cr.SlotState = .{ .variant = .unleased, .g = pl.g, .root = pl.root, .cursor = pl.cursor, .slot_tree_extents = pl.extents, .last_written = table.lease_seq + 1, .seq_floor = pl.seq_floor };
            const sv = state.encode();
            const tails = tails_per_slot.items[i].items;
            var recs: std.ArrayList(manager.ControlRecord) = .empty;
            defer recs.deinit(gpa);
            try recs.append(gpa, .{ .put = .{ .key = cr.slotStateKey(pl.slot).slice(), .value = &sv } });
            // The tails inline under the cap, else spilled (the leave's
            // path, `slots/tails_spill`) with the extents' `alloc` deltas.
            var spilled: ?tails_spill.Spilled = null;
            defer if (spilled) |*sp| sp.deinit();
            if (tails.len > TAILS_INLINE_MAX) {
                const f = self.vol.vt.spillTails orelse return error.Corrupt;
                spilled = try f(self.vol.ctx, gpa, tails);
            }
            const tails_rec: cr.SlotTails = if (spilled) |sp| sp.record(pl.g) else .{ .inline_tails = .{ .g = pl.g, .tails = tails } };
            const tv = cr.encodeSlotTails(tails_rec, &tails_buf) catch return error.Corrupt;
            try recs.append(gpa, .{ .put = .{ .key = cr.slotTailsKey(pl.slot).slice(), .value = tv } });
            if (spilled) |sp| for (sp.extents) |e| try recs.append(gpa, .{ .alloc = e });
            var qv: [cr.CustodyQuarantineWire.LEN]u8 = undefined;
            if (quarantine_until) |u| {
                qv = cr.CustodyQuarantineWire.init(u).encode();
                try recs.append(gpa, .{ .put = .{ .key = cr.custodyQuarantineKey(pl.slot).slice(), .value = &qv } });
            }
            try self.vol.vt.controlEntry(self.vol.ctx, recs.items);
            try self.vol.vt.noteUnleased(self.vol.ctx, pl.slot, state);
            // The frame screen's rule 2 reads the tails just made durable.
            try table.recordTails(pl.slot, pl.g, tails);
        }
        // The grant record minus what it no longer owns.
        var remaining: std.ArrayList(GrantRun) = .empty;
        defer remaining.deinit(gpa);
        try subtractExtents(gpa, grant.items, kept.items, &remaining);
        var grant_buf: [GRANT_RUNS_INLINE_MAX * cr.GrantRunWire.LEN + cr.ExtentGrantHead.LEN]u8 = undefined;
        if (remaining.items.len > GRANT_RUNS_INLINE_MAX) return error.Corrupt;
        if (remaining.items.len == 0) {
            try self.vol.vt.controlEntry(self.vol.ctx, &.{.{ .delete = cr.extentGrantKey(p.appender_id).slice() }});
        } else {
            const gv = cr.encodeExtentGrant(remaining.items, &grant_buf) catch return error.Corrupt;
            try self.vol.vt.controlEntry(self.vol.ctx, &.{.{ .put = .{ .key = cr.extentGrantKey(p.appender_id).slice(), .value = gv } }});
        }
        try self.fail(.tree0);
        // ---- 7b. the RAM table LAST; the rollback disarmed ----
        for (plans.items) |pl| {
            const words: lt.SlotWords = .{ .root = pl.root, .cursor = pl.cursor, .extents = pl.extents, .seq_floor = pl.seq_floor };
            switch (try table.release(pl.slot, p.appender_id, pl.g, words, clock.monotonicNs())) {
                .released, .already => {},
                .refused => {
                    log.err("recovery: the RAM lease table and tree 0 disagree on slot {d} (g {d}) — refusing to continue", .{ pl.slot, pl.g });
                    return error.Corrupt;
                },
            }
            try self.vol.vt.holdCoherence(self.vol.ctx, pl.slot, coherence_until);
            if (quarantine_until) |u| {
                self.vol.vt.quarantineCustody(self.vol.ctx, pl.slot, u);
                stats.slot_custody_quarantines_installed.inc();
                custody.stats.slot_custody_quarantined.add(1);
            }
        }
        rollback.disarm();
        self.vol.vt.endRetirementHold(self.vol.ctx, true);
        self.vol.vt.notifyHandoverDone(self.vol.ctx);
        phase(.tree0, t_tree0);
        try self.fail(.table_release);
        // ---- 7c. the unclaimed grant and the orphans returned ----
        // Every return is an extent the record step 7 wrote still names:
        // the page's word is the appender's attestation at its last write,
        // never proof the extent is still the grant's.
        var unclaimed: std.ArrayList(u64) = .empty;
        defer unclaimed.deinit(gpa);
        try self.unclaimedOf(&p, window.items, remaining.items, &unclaimed);
        self.vol.vt.returnExtents(self.vol.ctx, unclaimed.items) catch |e| log.warn("recovery: {d} unclaimed extent(s) of appender {d} could not be returned ({s}) — the region release retries, fsck C13 reclaims", .{ unclaimed.items.len, p.appender_id, @errorName(e) });
        // The page's word is the appender's attestation of its unclaimed
        // pool AT ITS LAST WRITE; the record is the manager's attestation
        // of the whole grant. What the record holds beyond the page and the
        // trees is judged by content, never by the page's lag: an image the
        // dead appender wrote is an orphan (C13); an extent it never drew
        // on — a refill granted after that page write, a RAM claim never
        // written, a run past the page's four — is unclaimed and goes back
        // unnamed (0002 row 352).
        var orphans: std.ArrayList(u64) = .empty;
        defer orphans.deinit(gpa);
        var unwritten: std.ArrayList(u64) = .empty;
        defer unwritten.deinit(gpa);
        try self.vol.vt.orphanImages(self.vol.ctx, gpa, p.appender_id, remaining.items, kept.items, &orphans, &unwritten);
        if (orphans.items.len != 0) {
            stats.dead_appender_orphans.add(orphans.items.len);
            try self.vol.vt.returnExtents(self.vol.ctx, orphans.items);
        }
        if (unwritten.items.len != 0) {
            stats.dead_appender_unwritten_returned.add(unwritten.items.len);
            try self.vol.vt.returnExtents(self.vol.ctx, unwritten.items);
        }
        try self.fail(.returns);
        // ---- 8. page Recovered ----
        var fresh = (try self.vol.vt.readPage(self.vol.ctx, p.appender_id)) orelse return error.Corrupt;
        fresh.state = .recovered;
        fresh.recovered_by_term = self.vol.vt.writerTerm(self.vol.ctx);
        fresh.n_slots = 0;
        fresh.head_hint = ring.head;
        fresh.ledger_tail_seq = ring.head;
        fresh.generation += 1;
        try self.vol.vt.writePage(self.vol.ctx, &fresh);
        try self.fail(.page_recovered);
        // ---- 9. recovered:{id, vol} ----
        _ = self.mount.ledger.recordRecovered(death.identity, vol_ordinal, self.vol.vt.writerTerm(self.vol.ctx), @intCast(clock.realtimeCoarseNs())) catch return error.Io;
        try self.fail(.recovered_record);
        stats.appender_recoveries.inc();
        phase(.total, t_total);
        return .acted;
    }

    /// The three violation classes over the window (REC:1477-1517): a
    /// slot-tree record of a slot the lease map does not lease to the dead
    /// id (lease) — unless tree 0 already holds the slot `Unleased`: a
    /// RE-RUN past its tree-0 step ABSORBS it, judged legal and never
    /// re-applied —, an `alloc` of an extent its grant does not hold
    /// (extent), a record keyed outside the slot it claims (key — the
    /// seam's `local_ino` is 0 for a key that is not a slot-tree key; the
    /// slot's intent HOME is local 0 by law and is exempt: the kill rows'
    /// B, killed with a cross-owner mkdir's intent still inside its ring
    /// window, was refused every poll — "no local ino").
    fn screenWindow(self: *Driver, window: []const WindowRecord, leased: []const u16, grant: []const GrantRun) Error!void {
        var absorbed: [ABSORBED_MAX]u16 = undefined;
        var n_absorbed: usize = 0;
        for (window) |r| switch (r.kind) {
            .put, .delta, .delete => {
                var held = false;
                for (leased) |s| held = held or s == r.slot;
                if (!held) {
                    var seen = false;
                    for (absorbed[0..n_absorbed]) |a| seen = seen or a == r.slot;
                    if (!seen) {
                        const st = try self.vol.vt.slotState(self.vol.ctx, r.slot);
                        if (st == null or st.?.variant != .unleased or n_absorbed == absorbed.len) {
                            stats.meta_kv_replay_lease_violations.fire("the dead ring names a slot tree 0 never leased to the appender — no merge order is correct");
                            return error.Violation;
                        }
                        absorbed[n_absorbed] = r.slot;
                        n_absorbed += 1;
                        stats.appender_recovery_absorbed.inc();
                    }
                }
                if (r.level == 0 and r.local_ino == 0 and !r.home) {
                    stats.meta_kv_replay_key_violations.fire("a slot-tree record of the dead ring carries no local ino for its slot");
                    return error.Violation;
                }
            },
            .alloc => {
                var granted = false;
                for (grant) |g| granted = granted or g.contains(r.extent);
                if (!granted) {
                    stats.meta_kv_replay_extent_violations.fire("the dead ring claims an extent its grant record does not hold");
                    return error.Violation;
                }
            },
            .data_delta => {},
        };
    }

    /// Phase 1: interior records `(level DESC, seq)`; phase 2: content by
    /// seq. The recoverer's frames on the dead lessee's slots carry its
    /// own stamp (`(0, g)` for the manager).
    fn replayWindow(self: *Driver, window: []const WindowRecord, slots: []const u16) Error!void {
        const gpa = self.gpa;
        var ordered: std.ArrayList(WindowRecord) = .empty;
        defer ordered.deinit(gpa);
        for (window) |r| {
            if (!r.kind.isTreeRecord()) continue;
            var ours = false;
            for (slots) |s| ours = ours or s == r.slot;
            if (ours) try ordered.append(gpa, r);
        }
        std.mem.sort(WindowRecord, ordered.items, {}, struct {
            fn lt(_: void, a: WindowRecord, b: WindowRecord) bool {
                if (a.level != b.level) return a.level > b.level;
                return a.seq < b.seq;
            }
        }.lt);
        const table = self.vol.vt.table(self.vol.ctx);
        for (ordered.items) |r| try self.vol.vt.replay(self.vol.ctx, r, self.mount.stamps.stampFor(r.slot, table));
    }

    /// The page's runs minus every extent the window's `alloc` records
    /// claimed, within the record's `remaining` runs.
    fn unclaimedOf(self: *Driver, p: *const page.Page, window: []const WindowRecord, remaining: []const GrantRun, out: *std.ArrayList(u64)) Error!void {
        for (p.grants.slice()) |run| {
            var e = run.start;
            while (e < run.start + run.len) : (e += 1) {
                var claimed = false;
                for (window) |r| claimed = claimed or (r.kind == .alloc and r.extent == e);
                var named = false;
                for (remaining) |r| named = named or r.contains(e);
                if (!claimed and named) try out.append(self.gpa, e);
            }
        }
    }
};

/// A directory wider than this is past the ring budget on any volume the
/// charter sizes (`heap/16 ÷ ring`, 32 MiB floor rings on a 16 TiB heap).
pub const APPENDER_SCAN_MAX: u32 = 4096;
/// The inline tails a recovery writes per slot: 1,024 × 16 B + the 12 B
/// head = 16,396 B, under the smallest allowed node's value cap (64 KiB
/// → 16,640 B, `geometry.recordValueCap`) and the stack buffer's size; a
/// tree with more leaves spills (`slots/tails_spill`).
pub const TAILS_INLINE_MAX: usize = 1024;
pub const GRANT_RUNS_INLINE_MAX: usize = 256;
/// Slots one re-run can absorb: the page budget's entries + the overflow
/// a region may lease (the rotor cap is 128).
pub const ABSORBED_MAX: usize = 256;

/// `grant − kept` (`kept` sorted ascending), as normalized runs.
fn subtractExtents(gpa: Allocator, grant: []const GrantRun, kept: []const u64, out: *std.ArrayList(GrantRun)) Allocator.Error!void {
    for (grant) |run| {
        var start: ?u64 = null;
        var e = run.start;
        while (e <= run.end()) : (e += 1) {
            const hit = e == run.end() or sortedContains(kept, e);
            if (hit) {
                if (start) |s| {
                    try out.append(gpa, .{ .start = s, .len = @intCast(e - s) });
                    start = null;
                }
            } else if (start == null) start = e;
        }
    }
}

/// Whether the ascending `sorted` holds `e`.
pub fn sortedContains(sorted: []const u64, e: u64) bool {
    return std.sort.binarySearch(u64, sorted, e, struct {
        fn order(ctx: u64, item: u64) std.math.Order {
            return std.math.order(ctx, item);
        }
    }.order) != null;
}

/// §3.1: the RAII rollback's exact scope.
const Rollback = struct {
    driver: *Driver,
    plans: []SlotPlan,
    armed: bool = true,
    /// The retirement hold is installed (dropped, never parked, on a
    /// rollback).
    hold: bool = false,

    fn disarm(self: *Rollback) void {
        self.armed = false;
    }

    fn run(self: *Rollback) void {
        if (!self.armed) return;
        const table = self.driver.vol.vt.table(self.driver.vol.ctx);
        for (self.plans) |pl| {
            if (!pl.begun) continue;
            // (1) Releasing{dead} → Leased{dead}: a first touch is refused
            // in either state; the re-run begins the release again.
            table.abortRelease(pl.slot);
            // (4) the slot's RAM tree discarded and restored (or removed).
            self.driver.vol.vt.restoreTree(self.driver.vol.ctx, pl.slot, pl.prior);
        }
        // The held retirements stay allocated: the restored tree may reach
        // them, and the re-run's census judges them afresh.
        if (self.hold) self.driver.vol.vt.endRetirementHold(self.driver.vol.ctx, false);
        // (5) the parked doors woken.
        self.driver.vol.vt.notifyHandoverDone(self.driver.vol.ctx);
    }
};

// ---------------------------------------------------------------------------
// The poll's hooks over the driver.
// ---------------------------------------------------------------------------

pub const PollBinding = struct {
    driver: *Driver,
    named_ctx: *anyopaque,
    namedAsHolder: *const fn (ctx: *anyopaque, identity: WriterIdentity) bool,

    pub fn hook(self: *PollBinding) death_ledger.VolumeHook {
        return .{ .ctx = self, .vol = self.driver.vol.vt.vol(self.driver.vol.ctx), .releaseRecovered = &release, .recoverDead = &recover, .regionPending = &pending };
    }

    fn release(ctx: *anyopaque) u32 {
        const self: *PollBinding = @ptrCast(@alignCast(ctx));
        return self.driver.releaseRecovered(self.namedAsHolder, self.named_ctx);
    }

    fn recover(ctx: *anyopaque, d: death_ledger.Death) death_ledger.RecoverOutcome {
        const self: *PollBinding = @ptrCast(@alignCast(ctx));
        return switch (self.driver.recoverDead(d)) {
            .acted => .acted,
            .skipped => .skipped,
            .failed => .failed,
        };
    }

    fn pending(ctx: *anyopaque, identity: WriterIdentity) bool {
        const self: *PollBinding = @ptrCast(@alignCast(ctx));
        return self.driver.regionPending(identity);
    }
};

// ---------------------------------------------------------------------------
// `squeezefs appender clear <uri> <id>` (§3.4): the refusals in order, then
// the writes — over the OFFLINE seam (the CLI's Linux binding opens the
// volume and answers it).
// ---------------------------------------------------------------------------

pub const ClearRefusal = enum {
    /// 1. The Layer-A flock on the volume (or volume 0) is HELD.
    live_mounted,
    /// 2. A `writer_claim` heartbeated within the TTL on the volume or
    ///    volume 0 ("a live manager holds the set").
    fresh_claim,
    /// 3. The page is Free / Recovered / absent.
    nothing_to_clear,
    /// 4. The page is this node's own residue.
    own_residue,
    /// 5. `appender_id == 0`: the manager's region is the D0 successor's.
    appender_zero,
    /// 6. The manager's claim went stale less than `T_park_max` ago: a
    ///    parked joiner may still reclaim.
    parked_joiner_window,
    /// 7. A fresh `client:` registration names the appender's node.
    fresh_registration,
};

pub const ClearOutcome = union(enum) {
    refused: ClearRefusal,
    cleared: struct { was: page.State },
};

pub const ClearSeam = struct {
    ctx: *anyopaque,
    vt: *const VTable,

    pub const VTable = struct {
        flockHeld: *const fn (ctx: *anyopaque, vol0: bool) bool,
        /// The claim's age in ms on the volume / volume 0 (null = none).
        claimAgeMs: *const fn (ctx: *anyopaque, vol0: bool) ?u64,
        readPage: *const fn (ctx: *anyopaque, appender_id: u32) Error!?page.Page,
        pageIsOwn: *const fn (ctx: *anyopaque, p: *const page.Page) bool,
        freshRegistrationFor: *const fn (ctx: *anyopaque, node_token: u64) bool,
        /// Write (1): the page `Recovering{recovered_by_term 0}`, both slots
        /// + sync.
        writeRecoveringPage: *const fn (ctx: *anyopaque, p: *const page.Page) Error!void,
        /// Write (2): the writer bring-up (the frame fence primed, the own
        /// pools, the cover, the JOIN).
        bringUp: *const fn (ctx: *anyopaque) Error!void,
        /// Write (3): the EARLY death record on volume 0, barriered +
        /// `checkpoint_now`.
        recordEarlyDeath: *const fn (ctx: *anyopaque, identity: WriterIdentity) Error!void,
        /// Write (4): the clean leave.
        leave: *const fn (ctx: *anyopaque) Error!void,
    };
};

/// The verb. `t_park_max_ms` and `t_owner_ms` are the derivations in
/// force (never knobs).
pub fn clearAppender(seam: ClearSeam, appender_id: u32, t_owner_ms: u64, t_park_max_ms: u64) Error!ClearOutcome {
    stats.appender_clear_runs.inc();
    if (seam.vt.flockHeld(seam.ctx, false) or seam.vt.flockHeld(seam.ctx, true)) return .{ .refused = .live_mounted };
    if (seam.vt.claimAgeMs(seam.ctx, false)) |age| if (age <= t_owner_ms) return .{ .refused = .fresh_claim };
    const vol0_age = seam.vt.claimAgeMs(seam.ctx, true);
    if (vol0_age) |age| if (age <= t_owner_ms) return .{ .refused = .fresh_claim };
    const p = (try seam.vt.readPage(seam.ctx, appender_id)) orelse return .{ .refused = .nothing_to_clear };
    if (p.state == .free or p.state == .recovered) return .{ .refused = .nothing_to_clear };
    if (seam.vt.pageIsOwn(seam.ctx, &p)) return .{ .refused = .own_residue };
    if (appender_id == 0) return .{ .refused = .appender_zero };
    if (vol0_age) |age| if (age - t_owner_ms < t_park_max_ms) return .{ .refused = .parked_joiner_window };
    if (seam.vt.freshRegistrationFor(seam.ctx, p.identity.node_token)) return .{ .refused = .fresh_registration };
    const was = p.state;
    var rec = p;
    rec.state = .recovering;
    rec.recovered_by_term = 0;
    rec.generation += 1;
    try seam.vt.writeRecoveringPage(seam.ctx, &rec);
    try seam.vt.bringUp(seam.ctx);
    try seam.vt.recordEarlyDeath(seam.ctx, .{ .node_token = p.identity.node_token, .mount_slot = p.identity.mount_slot, .writer_id = p.identity.writer_id });
    try seam.vt.leave(seam.ctx);
    return .{ .cleared = .{ .was = was } };
}

pub fn writeClearRefusal(w: *std.Io.Writer, r: ClearRefusal, appender_id: u32) std.Io.Writer.Error!void {
    switch (r) {
        .live_mounted => try w.writeAll("refusing — the volume is live-mounted on this host. Unmount it first"),
        .fresh_claim => try w.writeAll("refusing — a live manager holds the set (its writer claim heartbeated within the TTL)"),
        .nothing_to_clear => try w.print("appender {d}: nothing to clear (the page is Free, Recovered or absent)", .{appender_id}),
        .own_residue => try w.print("appender {d}: refusing — the page is this node's OWN residue; the next mount of this node recovers it itself", .{appender_id}),
        .appender_zero => try w.writeAll("appender 0's page is the MANAGER's — a dead manager's region is recovered by the D0 ladder's successor: mount the set writable once its writer claim aged past the TTL (or `claim clear` it). `appender clear` attests joined appenders' pages (ids ≥ 1) only"),
        .parked_joiner_window => try w.writeAll("refusing — the manager's claim went stale less than T_park_max ago: a LIVE joined appender parked at T_self reclaims against the successor for up to T_park_max, and a clear inside that window would recover a live writer's ring"),
        .fresh_registration => try w.print("appender {d}: refusing — a fresh registration names the appender's node", .{appender_id}),
    }
}

// ---------------------------------------------------------------------------
// Tests: the §5.9 procedure over RAM images, every crash window.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the recovery bound derives from the ring: ≈ 1,105 ms at the 512 KiB floor, ≈ 1,312 ms at the 32 MiB ceiling (the charter's ≈ 1.3 s); the gauge publishes it" {
    try testing.expectEqual(@as(u64, 2 + 3 + 1100), recoveryBoundMs(512 << 10, 256 << 10));
    try testing.expectEqual(@as(u64, 9 + 19 + 1100), recoveryBoundMs(4 << 20, 256 << 10));
    try testing.expectEqual(@as(u64, 66 + 146 + 1100), recoveryBoundMs(32 << 20, 256 << 10));
    stats.appender_recovery_bound_ms.set(recoveryBoundMs(32 << 20, 256 << 10));
    try testing.expectEqual(@as(u64, 1312), stats.appender_recovery_bound_ms.load());
}

test "the coherence hold derives from the census law: VALIDITY_RENEWALS × renew + 2 × skew_max past the record — 20,044 ms at the shipped T_owner of 45 s (renew 10 s, skew 22 ms), 534 ms at the smoke's 3 s (renew 266 ms, skew 1 ms)" {
    for ([_]u64{ 45_000, 3_000 }) |t_owner| {
        const c = try shard.LeaseClocks.derive(t_owner, 0, shard.CHECKPOINT_LANDING_CEILING_MS);
        try testing.expectEqual(census_view.VALIDITY_RENEWALS * c.renew_ms + 2 * c.skew_max_ms, coherenceHoldMs(c.renew_ms, c.skew_max_ms));
    }
    const shipped = try shard.LeaseClocks.derive(45_000, 0, shard.CHECKPOINT_LANDING_CEILING_MS);
    try testing.expectEqual(@as(u64, 20_044), coherenceHoldMs(shipped.renew_ms, shipped.skew_max_ms));
    const smoke = try shard.LeaseClocks.derive(3_000, 0, shard.CHECKPOINT_LANDING_CEILING_MS);
    try testing.expectEqual(@as(u64, 534), coherenceHoldMs(smoke.renew_ms, smoke.skew_max_ms));
}

test "grant − reached is normalized runs" {
    var out: std.ArrayList(GrantRun) = .empty;
    defer out.deinit(testing.allocator);
    try subtractExtents(testing.allocator, &.{ .{ .start = 10, .len = 6 }, .{ .start = 30, .len = 2 } }, &.{ 12, 13, 30, 31 }, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(@as(u64, 10), out.items[0].start);
    try testing.expectEqual(@as(u32, 2), out.items[0].len);
    try testing.expectEqual(@as(u64, 14), out.items[1].start);
    try testing.expectEqual(@as(u32, 2), out.items[1].len);
}

/// The RAM volume: pages, the dead ring's window, tree 0, the lease
/// table, slot trees as record sets with a flushed/dirty split.
const RamVolume = struct {
    gpa: Allocator,
    vol: u16,
    pages: std.AutoHashMapUnmanaged(u32, page.Page) = .empty,
    page_writes: u32 = 0,
    window: std.ArrayList(WindowRecord) = .empty,
    ring_head: u64 = 5000,
    seq_frontier: u64 = 4990,
    tree0: std.StringHashMapUnmanaged([]u8) = .empty,
    control_entries: u32 = 0,
    table: *LeaseTable,
    trees: std.AutoHashMapUnmanaged(u16, Tree) = .empty,
    grant_bits: std.AutoHashMapUnmanaged(u64, void) = .empty,
    /// Extents whose residue carries a frame the dead appender stamped.
    stamped: std.AutoHashMapUnmanaged(u64, void) = .empty,
    returned: std.ArrayList(u64) = .empty,
    /// Ring bytes a region release returned to the heap.
    rings_freed: u64 = 0,
    /// The data deltas folded (by handle: a re-fold is idempotent).
    data_deltas: std.AutoHashMapUnmanaged(u64, void) = .empty,
    cycles: u32 = 0,
    seq_floor_raised: u64 = 0,
    writer_term: u64 = 7,
    quarantined: std.ArrayList(u16) = .empty,
    /// The structural hold (the SMO lock) around the replay.
    structure_held: bool = false,
    /// The coherence holds installed: slot → realtime deadline.
    coherence: std.AutoHashMapUnmanaged(u16, u64) = .empty,
    handover_notifies: u32 = 0,
    /// Seam: the pointer's seq lies (the install must refuse).
    stale_pointer: bool = false,
    /// Seam: the volume's own SMO on a recovered tree during the flush
    /// cycles (the checkpoint task's maintenance, the D4 arm): `old`'s
    /// image replaced by `new` (claimed from the volume's pool), `old`
    /// retired.
    smo_on_cycle: ?struct { slot: u16, old: u64, new: u64 } = null,
    /// The coverage clock's parked retirements (the pending-free FIFO).
    fifo: std.ArrayList(u64) = .empty,
    /// The recovery's retirement hold: the grant's runs while installed,
    /// and what it took.
    hold: ?[]const GrantRun = null,
    held: std.ArrayList(u64) = .empty,

    const Tree = struct {
        root: cr.Root,
        floor: u64,
        /// Records applied in RAM (dirty until a cycle flushes them).
        dirty: std.ArrayList(u64) = .empty,
        flushed: std.ArrayList(u64) = .empty,
        images: std.ArrayList(u64) = .empty,
        /// Adopted fresh by a recovery (no prior).
        guest: bool = false,
        installs: u32 = 0,

        fn deinit(self: *Tree, gpa: Allocator) void {
            self.dirty.deinit(gpa);
            self.flushed.deinit(gpa);
            self.images.deinit(gpa);
        }
    };

    fn init(gpa: Allocator, vol: u16) !*RamVolume {
        const self = try gpa.create(RamVolume);
        self.* = .{ .gpa = gpa, .vol = vol, .table = try LeaseTable.init(gpa) };
        return self;
    }

    fn deinit(self: *RamVolume) void {
        self.pages.deinit(self.gpa);
        self.window.deinit(self.gpa);
        var it = self.tree0.iterator();
        while (it.next()) |kv| {
            self.gpa.free(kv.key_ptr.*);
            self.gpa.free(kv.value_ptr.*);
        }
        self.tree0.deinit(self.gpa);
        var ti = self.trees.valueIterator();
        while (ti.next()) |t| t.deinit(self.gpa);
        self.trees.deinit(self.gpa);
        self.grant_bits.deinit(self.gpa);
        self.stamped.deinit(self.gpa);
        self.returned.deinit(self.gpa);
        self.fifo.deinit(self.gpa);
        self.held.deinit(self.gpa);
        if (self.hold) |h| self.gpa.free(h);
        self.quarantined.deinit(self.gpa);
        self.coherence.deinit(self.gpa);
        self.data_deltas.deinit(self.gpa);
        self.table.deinit();
        self.gpa.destroy(self);
    }

    fn put0(self: *RamVolume, k: []const u8, v: []const u8) !void {
        const vd = try self.gpa.dupe(u8, v);
        const gop = try self.tree0.getOrPut(self.gpa, k);
        if (gop.found_existing) {
            self.gpa.free(gop.value_ptr.*);
        } else gop.key_ptr.* = try self.gpa.dupe(u8, k);
        gop.value_ptr.* = vd;
    }

    fn get0(self: *RamVolume, k: []const u8) ?[]const u8 {
        return self.tree0.get(k);
    }

    fn slotStateOf(self: *RamVolume, slot: u16) ?cr.SlotState {
        const v = self.get0(cr.slotStateKey(slot).slice()) orelse return null;
        return cr.SlotStateWire.decode(v) catch unreachable;
    }

    /// The fixture: appender 2 Live under `dead`, leasing slots 10 and 11
    /// (tree 0 `Leased`, the page naming slot 10 at a NEWER root), a grant
    /// RECORD of extents 100..110 of which 100..102 are its tree images and
    /// 103..107 the page's unclaimed word — 108..109 were granted AFTER the
    /// page's last write (the refill the page never named): 108 unwritten,
    /// 109 carrying a superseded image the appender stamped; the ring window
    /// with content for both slots, one interior record, one alloc.
    fn seedDeadAppender(self: *RamVolume, dead: WriterIdentity) !void {
        var p = page.Page.free(2, 3, page.SegmentTable.one(.{ .start = 1 << 20, .len = 512 << 10 }));
        p.state = .live;
        p.identity = .{ .node_token = dead.node_token, .mount_slot = dead.mount_slot, .writer_id = dead.writer_id };
        p.term = 4;
        p.head_hint = 4000;
        p.ledger_tail_seq = 3000;
        p.seq_offset = 2000;
        try p.grants.push(.{ .start = 103, .len = 5 });
        try p.setSlots(&.{ .{ .slot = 10, .state = .live, .g = 3, .slot_tree_extents = 2, .root_addr = 0x10000, .root_seq = 900, .cursor = 50 }, .{ .slot = 11, .state = .live, .g = 2, .slot_tree_extents = 1, .root_addr = 0, .root_seq = 0, .cursor = 20 } });
        try self.pages.put(self.gpa, 2, p);
        // Tree 0: both leased to appender 2 (slot 10's grant-time root is OLDER).
        const s10: cr.SlotState = .{ .variant = .leased, .appender_id = 2, .g = 3, .page_addr = 4096, .root = .{ .addr = 0x8000, .seq = 500 }, .cursor = 40, .slot_tree_extents = 1 };
        const s11: cr.SlotState = .{ .variant = .leased, .appender_id = 2, .g = 2, .page_addr = 4096, .root = .{ .addr = 0x9000, .seq = 600 }, .cursor = 20, .slot_tree_extents = 1, .seq_floor = 100 };
        try self.put0(cr.slotStateKey(10).slice(), &s10.encode());
        try self.put0(cr.slotStateKey(11).slice(), &s11.encode());
        try self.table.load(10, s10);
        try self.table.load(11, s11);
        var gbuf: [256]u8 = undefined;
        const g = try cr.encodeExtentGrant(&.{.{ .start = 100, .len = 10 }}, &gbuf);
        try self.put0(cr.extentGrantKey(2).slice(), g);
        var e: u64 = 100;
        while (e < 110) : (e += 1) try self.grant_bits.put(self.gpa, e, {});
        try self.stamped.put(self.gpa, 109, {});
        // The trees as the recoverer's cache holds them (stale: the manager's grant-time roots).
        var t10: Tree = .{ .root = .{ .addr = 0x8000, .seq = 500 }, .floor = 0 };
        try t10.images.appendSlice(self.gpa, &.{ 100, 101 });
        try self.trees.put(self.gpa, 10, t10);
        var t11: Tree = .{ .root = .{ .addr = 0x9000, .seq = 600 }, .floor = 0 };
        try t11.images.append(self.gpa, 102);
        try self.trees.put(self.gpa, 11, t11);
        // The window: content on both slots, an interior split on 10, an alloc of 104.
        try self.window.appendSlice(self.gpa, &.{
            .{ .slot = 10, .level = 0, .seq = 4001, .kind = .put, .local_ino = 70, .handle = 1 },
            .{ .slot = 11, .level = 0, .seq = 4002, .kind = .put, .local_ino = 25, .handle = 2 },
            .{ .slot = 10, .level = 1, .seq = 4003, .kind = .put, .handle = 3 },
            .{ .slot = 10, .level = 0, .seq = 4004, .kind = .delete, .local_ino = 71, .handle = 4 },
            .{ .slot = 10, .level = 0, .seq = 4005, .kind = .alloc, .extent = 104 },
            .{ .slot = 11, .level = 0, .seq = 4006, .kind = .data_delta, .handle = 5 },
        });
    }

    // ---- the seam ----

    fn self_(ctx: *anyopaque) *RamVolume {
        return @ptrCast(@alignCast(ctx));
    }
    fn volFn(ctx: *anyopaque) u16 {
        return self_(ctx).vol;
    }
    fn readPageFn(ctx: *anyopaque, id: u32) Error!?page.Page {
        return self_(ctx).pages.get(id);
    }
    fn writePageFn(ctx: *anyopaque, p: *const page.Page) Error!void {
        const self = self_(ctx);
        self.pages.put(self.gpa, p.appender_id, p.*) catch return error.OutOfMemory;
        self.page_writes += 1;
    }
    fn readRingFn(ctx: *anyopaque, gpa: Allocator, p: *const page.Page, out: *std.ArrayList(WindowRecord)) Error!RingRead {
        const self = self_(ctx);
        for (self.window.items) |r| if (r.seq >= p.ledger_tail_seq) out.append(gpa, r) catch return error.OutOfMemory;
        return .{ .head = self.ring_head, .seq_frontier = self.seq_frontier, .seq_offset = p.seq_offset };
    }
    fn slotStateFn(ctx: *anyopaque, slot: u16) Error!?cr.SlotState {
        return self_(ctx).slotStateOf(slot);
    }
    fn leasedToFn(ctx: *anyopaque, gpa: Allocator, id: u32, out: *std.ArrayList(u16)) Error!void {
        const self = self_(ctx);
        var it = self.tree0.iterator();
        while (it.next()) |kv| {
            const slot = cr.slotOfKey(kv.key_ptr.*) orelse continue;
            if (cr.tagOfKey(kv.key_ptr.*) != .slot_state) continue;
            const s = cr.SlotStateWire.decode(kv.value_ptr.*) catch return error.Corrupt;
            if (s.variant != .unleased and s.appender_id == id) out.append(gpa, slot) catch return error.OutOfMemory;
        }
        std.mem.sort(u16, out.items, {}, std.sort.asc(u16));
    }
    fn grantRecordFn(ctx: *anyopaque, gpa: Allocator, id: u32, out: *std.ArrayList(GrantRun)) Error!void {
        const self = self_(ctx);
        const v = self.get0(cr.extentGrantKey(id).slice()) orelse return;
        const d = cr.decodeExtentGrant(v) catch return error.Corrupt;
        out.resize(gpa, d.n_runs) catch return error.OutOfMemory;
        _ = d.collect(out.items);
    }
    fn controlEntryFn(ctx: *anyopaque, recs: []const manager.ControlRecord) Error!void {
        const self = self_(ctx);
        for (recs) |r| switch (r) {
            .put => |kv| self.put0(kv.key, kv.value) catch return error.OutOfMemory,
            .delete => |k| if (self.tree0.fetchRemove(k)) |kv| {
                self.gpa.free(kv.key);
                self.gpa.free(kv.value);
            },
            .alloc, .free, .data_delta => {},
        };
        self.control_entries += 1;
    }
    fn noteUnleasedFn(_: *anyopaque, _: u16, _: cr.SlotState) Error!void {}
    fn tableFn(ctx: *anyopaque) *LeaseTable {
        return self_(ctx).table;
    }
    fn treeStateFn(ctx: *anyopaque, slot: u16) ?TreeState {
        const t = self_(ctx).trees.get(slot) orelse return null;
        if (t.guest) return null;
        return .{ .root = t.root, .floor = t.floor };
    }
    /// The images each slot tree's root reaches on the device (the fixture).
    fn deviceImages(slot: u16) []const u64 {
        return switch (slot) {
            10 => &.{ 100, 101 },
            11 => &.{102},
            else => &.{},
        };
    }
    fn installRootFn(ctx: *anyopaque, slot: u16, root: cr.Root, floor: u64) Error!void {
        const self = self_(ctx);
        if (self.stale_pointer) return error.Corrupt;
        const gop = self.trees.getOrPut(self.gpa, slot) catch return error.OutOfMemory;
        if (!gop.found_existing) {
            // Adopted fresh (`open_unpublished_slot_tree`): the walk from
            // the installed root finds its images.
            gop.value_ptr.* = .{ .root = root, .floor = floor, .guest = true };
            gop.value_ptr.images.appendSlice(self.gpa, deviceImages(slot)) catch return error.OutOfMemory;
        }
        // The cache barrier: every cached node dropped.
        gop.value_ptr.dirty.clearRetainingCapacity();
        gop.value_ptr.root = root;
        gop.value_ptr.floor = floor;
        gop.value_ptr.installs += 1;
    }
    fn restoreTreeFn(ctx: *anyopaque, slot: u16, prior: ?TreeState) void {
        const self = self_(ctx);
        const t = self.trees.getPtr(slot) orelse return;
        t.dirty.clearRetainingCapacity();
        if (prior) |pr_state| {
            t.root = pr_state.root;
            t.floor = pr_state.floor;
        } else {
            var removed = self.trees.fetchRemove(slot).?;
            removed.value.deinit(self.gpa);
        }
    }
    fn replayFn(ctx: *anyopaque, rec: WindowRecord, stamp: fencing.FrameStamp) Error!void {
        const self = self_(ctx);
        // The recoverer's own frames on the dead lessee's slots: stamped (0, g).
        if (stamp.appender_id != 0 or stamp.g != self.table.gOf(rec.slot)) return error.Violation;
        if (!self.structure_held) return error.Corrupt;
        const t = self.trees.getPtr(rec.slot) orelse return error.Corrupt;
        // LWW by seq: a record the base already holds folds to itself.
        for (t.flushed.items) |h| if (h == rec.handle) return;
        t.dirty.append(self.gpa, rec.handle) catch return error.OutOfMemory;
    }
    fn replayDataFn(ctx: *anyopaque, _: WriterIdentity, recs: []const WindowRecord) Error!void {
        const self = self_(ctx);
        for (recs) |r| if (r.kind == .data_delta) self.data_deltas.put(self.gpa, r.handle, {}) catch return error.OutOfMemory;
    }
    fn cycleFn(ctx: *anyopaque, slots: []const u16) Error!u32 {
        const self = self_(ctx);
        self.cycles += 1;
        for (slots) |s| {
            const t = self.trees.getPtr(s) orelse continue;
            t.flushed.appendSlice(self.gpa, t.dirty.items) catch return error.OutOfMemory;
            t.dirty.clearRetainingCapacity();
            t.root.seq += 1;
        }
        if (self.smo_on_cycle) |op| {
            self.smo_on_cycle = null;
            const t = self.trees.getPtr(op.slot) orelse return error.Corrupt;
            const at = std.mem.indexOfScalar(u64, t.images.items, op.old) orelse return error.Corrupt;
            t.images.items[at] = op.new;
            self.retire(op.old) catch return error.OutOfMemory;
        }
        return 1;
    }
    /// An SMO's retirement: the hold takes a dead grant's extent, the
    /// coverage clock parks anything else.
    fn retire(self: *RamVolume, extent: u64) Allocator.Error!void {
        if (self.hold) |runs| for (runs) |r| if (r.contains(extent)) return self.held.append(self.gpa, extent);
        try self.fifo.append(self.gpa, extent);
    }
    fn holdFn(ctx: *anyopaque, grant: []const GrantRun) Error!void {
        const self = self_(ctx);
        if (self.hold) |h| self.gpa.free(h);
        self.hold = self.gpa.dupe(GrantRun, grant) catch return error.OutOfMemory;
        self.held.clearRetainingCapacity();
    }
    fn endHoldFn(ctx: *anyopaque, park: bool) void {
        const self = self_(ctx);
        const runs = self.hold orelse return;
        self.gpa.free(runs);
        self.hold = null;
        if (park) self.fifo.appendSlice(self.gpa, self.held.items) catch {};
        self.held.clearRetainingCapacity();
    }
    fn flushedRootFn(ctx: *anyopaque, slot: u16) Error!FlushedRoot {
        const t = self_(ctx).trees.get(slot) orelse return error.Corrupt;
        return .{ .root = t.root, .extents = @intCast(t.images.items.len) };
    }
    fn leafTailsFn(ctx: *anyopaque, gpa: Allocator, slot: u16, pr_fenced: bool, out: *std.ArrayList(cr.TailEntry)) Error!u64 {
        const t = self_(ctx).trees.get(slot) orelse return error.Corrupt;
        var bytes: u64 = 0;
        for (t.images.items) |img| {
            out.append(gpa, .{ .leaf_addr = img << 16, .tail = 8192 }) catch return error.OutOfMemory;
            // Under a device fence the resident ones only; the fixture's
            // leaves are all non-resident: a PR recovery scans none.
            if (!pr_fenced) bytes += 65_536;
        }
        if (pr_fenced) out.clearRetainingCapacity();
        return bytes;
    }
    fn keptFn(ctx: *anyopaque, gpa: Allocator, _: []const GrantRun, out: *std.ArrayList(u64)) Error!void {
        const self = self_(ctx);
        // Every root tree 0 names reaches its images, leased or not.
        for ([_]u16{ 10, 11 }) |s| {
            const imgs: []const u64 = if (self.trees.get(s)) |t| t.images.items else deviceImages(s);
            out.appendSlice(gpa, imgs) catch return error.OutOfMemory;
        }
        out.appendSlice(gpa, self.held.items) catch return error.OutOfMemory;
    }
    fn returnExtentsFn(ctx: *anyopaque, extents: []const u64) Error!void {
        const self = self_(ctx);
        for (extents) |e| {
            if (!self.grant_bits.remove(e)) return error.Corrupt;
            self.returned.append(self.gpa, e) catch return error.OutOfMemory;
        }
    }
    fn freeRingFn(ctx: *anyopaque, segments: []const page.Extent) Error!void {
        const self = self_(ctx);
        for (segments) |s| self.rings_freed += s.len;
    }
    fn orphanImagesFn(ctx: *anyopaque, gpa: Allocator, _: u32, grant: []const GrantRun, reached: []const u64, orphans: *std.ArrayList(u64), unwritten: *std.ArrayList(u64)) Error!void {
        const self = self_(ctx);
        // The census: still granted, still allocated, reached by no root,
        // and not an extent the window claimed (104 is claimed: kept);
        // judged by content — a stamped residue is an orphan image.
        for (grant) |run| {
            var e = run.start;
            while (e < run.end()) : (e += 1) {
                var hit = false;
                for (reached) |r| hit = hit or r == e;
                if (hit or !self.grant_bits.contains(e)) continue;
                var claimed = false;
                for (self.window.items) |w| claimed = claimed or (w.kind == .alloc and w.extent == e);
                if (claimed) continue;
                (if (self.stamped.contains(e)) orphans else unwritten).append(gpa, e) catch return error.OutOfMemory;
            }
        }
    }
    fn raiseFloorFn(ctx: *anyopaque, f: u64) void {
        const self = self_(ctx);
        self.seq_floor_raised = @max(self.seq_floor_raised, f);
    }
    fn ring0HeadFn(_: *anyopaque) u64 {
        return 777;
    }
    fn writerTermFn(ctx: *anyopaque) u64 {
        return self_(ctx).writer_term;
    }
    fn quarantineFn(ctx: *anyopaque, slot: u16, _: u64) void {
        const self = self_(ctx);
        self.quarantined.append(self.gpa, slot) catch {};
    }
    fn notifyFn(ctx: *anyopaque) void {
        self_(ctx).handover_notifies += 1;
    }
    fn holdStructureFn(ctx: *anyopaque, held: bool) void {
        const self = self_(ctx);
        std.debug.assert(self.structure_held != held);
        self.structure_held = held;
    }
    fn holdCoherenceFn(ctx: *anyopaque, slot: u16, until: u64) Error!void {
        const self = self_(ctx);
        self.coherence.put(self.gpa, slot, until) catch return error.OutOfMemory;
    }

    const vtable: Volume.VTable = .{ .vol = &volFn, .readPage = &readPageFn, .writePage = &writePageFn, .readRing = &readRingFn, .slotState = &slotStateFn, .leasedTo = &leasedToFn, .grantRecord = &grantRecordFn, .controlEntry = &controlEntryFn, .noteUnleased = &noteUnleasedFn, .table = &tableFn, .treeState = &treeStateFn, .installRoot = &installRootFn, .restoreTree = &restoreTreeFn, .replay = &replayFn, .holdStructure = &holdStructureFn, .replayDataDeltas = &replayDataFn, .cycleUntilClean = &cycleFn, .flushedRoot = &flushedRootFn, .leafTails = &leafTailsFn, .holdRetirements = &holdFn, .endRetirementHold = &endHoldFn, .keptExtents = &keptFn, .returnExtents = &returnExtentsFn, .freeRing = &freeRingFn, .orphanImages = &orphanImagesFn, .raiseSeqFloor = &raiseFloorFn, .ring0Head = &ring0HeadFn, .writerTerm = &writerTermFn, .quarantineCustody = &quarantineFn, .holdCoherence = &holdCoherenceFn, .notifyHandoverDone = &notifyFn };

    fn volume(self: *RamVolume) Volume {
        return .{ .ctx = self, .vt = &vtable };
    }
};

const DEAD: WriterIdentity = .{ .node_token = 0xD0D0, .mount_slot = 0x5001, .writer_id = 0xBEEF };
const ME: WriterIdentity = .{ .node_token = 0xAAAA, .mount_slot = 0, .writer_id = 0x1 };

const Fixture = struct {
    gpa: Allocator,
    rv: *RamVolume,
    ledger_store: death_ledger.RamLedgerStore,
    ledger: death_ledger.Ledger,
    screen: fencing.ScreenSource,
    stamps: fencing.FrameStamps,
    driver: Driver,

    fn init(gpa: Allocator, early: bool) !*Fixture {
        const f = try gpa.create(Fixture);
        errdefer gpa.destroy(f);
        f.gpa = gpa;
        f.rv = try RamVolume.init(gpa, 1);
        try f.rv.seedDeadAppender(DEAD);
        f.ledger_store = death_ledger.RamLedgerStore.init(gpa);
        f.ledger = death_ledger.Ledger.init(gpa, f.ledger_store.store(), 2, death_ledger.wireOf(ME), 45_000);
        f.ledger.recordDeath(DEAD, .{ .epoch = 9, .ts_realtime_ns = 1_000 * std.time.ns_per_ms, .pr_key = 0, .early = early });
        f.screen = .{ .gpa = gpa, .table = f.rv.table, .pr_fenced = false };
        f.stamps = fencing.FrameStamps.init(gpa, 0);
        // The recoverer (the manager, appender 0) stamps the dead slots at
        // their g off the table's `Releasing{dead}` mark — the structural
        // class — with nothing learned.
        try f.stamps.prime(f.rv.table, &.{}, &.{});
        f.driver = .{ .gpa = gpa, .vol = f.rv.volume(), .mount = .{ .identity = ME, .meta_pr = null, .screen = &f.screen, .stamps = &f.stamps, .ledger = &f.ledger, .skew_max_ms = 22, .coherence_hold_ms = coherenceHoldMs(300, 22) } };
        return f;
    }

    fn deinit(self: *Fixture) void {
        self.stamps.deinit();
        self.screen.deinit();
        self.ledger.deinit();
        self.ledger_store.deinit();
        self.rv.deinit();
        self.gpa.destroy(self);
    }

    fn death(self: *Fixture) death_ledger.Death {
        return .{ .identity = DEAD, .record = (self.ledger.readDeath(DEAD) catch unreachable).? };
    }

    /// The converged end state every path must reach.
    fn expectConverged(self: *Fixture) !void {
        const p = self.rv.pages.get(2).?;
        try testing.expectEqual(page.State.recovered, p.state);
        try testing.expectEqual(@as(u64, 7), p.recovered_by_term);
        try testing.expectEqual(@as(u64, 5000), p.ledger_tail_seq);
        try testing.expectEqual(@as(u16, 0), p.n_slots);
        for ([_]u16{ 10, 11 }) |s| {
            const st = self.rv.slotStateOf(s).?;
            try testing.expectEqual(cr.SlotVariant.unleased, st.variant);
            try testing.expectEqual(lt.State.unleased, self.rv.table.view(s).state);
            try testing.expectEqual(st.g, self.rv.table.view(s).g);
            try testing.expect(self.rv.get0(cr.slotTailsKey(s).slice()) != null);
        }
        const s10 = self.rv.slotStateOf(10).?;
        try testing.expectEqual(@as(u32, 3), s10.g);
        // Cursor = max(recorded 40, page 50, window max 71 + 1) = 72.
        try testing.expectEqual(@as(u64, 72), s10.cursor);
        // seq_floor = max(recorded 0, dead frontier 4990).
        try testing.expectEqual(@as(u64, 4990), s10.seq_floor);
        try testing.expectEqual(@as(u64, 4990), self.rv.slotStateOf(11).?.seq_floor);
        // The root by g: the page's NEWER root for slot 10 was installed (then flushed).
        try testing.expectEqual(@as(u64, 0x10000), self.rv.trees.get(10).?.root.addr);
        // Every window record of both slots is flushed, in order (interior first).
        const t10 = self.rv.trees.get(10).?;
        try testing.expectEqualSlices(u64, &.{ 3, 1, 4 }, t10.flushed.items);
        try testing.expectEqual(@as(usize, 0), t10.dirty.items.len);
        try testing.expect(try self.ledger.isRecovered(DEAD, 1));
        try testing.expect(!self.screen.isRecovering(2));
        try testing.expect(self.rv.seq_floor_raised >= 4990);
        // The coherence hold: no write before the record's ts + the hold.
        for ([_]u16{ 10, 11 }) |s| try testing.expectEqual(1_000 * std.time.ns_per_ms + coherenceHoldMs(300, 22) * std.time.ns_per_ms, self.rv.coherence.get(s).?);
        // The grant record minus the trees' images (100..102) = 103..109; the
        // unclaimed page runs minus the window's alloc (104) returned (103,
        // 105..107), then the record's remainder beyond the page by content:
        // 108 (never written) returned unnamed, 109 (the appender's stamped
        // residue) returned as the one orphan image.
        const g = cr.decodeExtentGrant(self.rv.get0(cr.extentGrantKey(2).slice()).?) catch unreachable;
        try testing.expectEqual(@as(u64, 7), g.totalExtents());
        std.mem.sort(u64, self.rv.returned.items, {}, std.sort.asc(u64));
        try testing.expectEqualSlices(u64, &.{ 103, 105, 106, 107, 108, 109 }, self.rv.returned.items);
        try testing.expectEqual(@as(u32, 1), self.rv.data_deltas.count());
    }
};

test "the §5.9 procedure end to end over RAM images: preempt (no device: the tail scan), Recovering, the window read + screened, the dead slots marked (first touch refused), the root by g installed after the cache barrier, cursor/seq_floor floors, two-phase replay, the flush, every leaf's tail, tree 0 per slot, the table LAST, returns, Recovered, recovered:" {
    const gpa = testing.allocator;
    const f = try Fixture.init(gpa, false);
    defer f.deinit();
    const recoveries0 = stats.appender_recoveries.load();
    const scan0 = stats.recovery_full_tail_scan_bytes.load();
    const orphans0 = stats.dead_appender_orphans.load();
    const unwritten0 = stats.dead_appender_unwritten_returned.load();
    try testing.expectEqual(Outcome.acted, f.driver.recoverDead(f.death()));
    try testing.expectEqual(recoveries0 + 1, stats.appender_recoveries.load());
    try f.expectConverged();
    // The record's remainder beyond the page's word, by content: the one
    // stamped residue is the orphan image; the refill never drawn on is not.
    try testing.expectEqual(orphans0 + 1, stats.dead_appender_orphans.load());
    try testing.expectEqual(unwritten0 + 1, stats.dead_appender_unwritten_returned.load());
    // Non-PR: every leaf scanned (3 images × 64 KiB).
    try testing.expectEqual(scan0 + 3 * 65_536, stats.recovery_full_tail_scan_bytes.load());
    // The page went Live → Recovering → Recovered: two writes.
    try testing.expectEqual(@as(u32, 2), f.rv.page_writes);
    // A Recovered ring is never replayed: a second poll skips it.
    try testing.expectEqual(Outcome.skipped, f.driver.recoverDead(f.death()));
    try testing.expectEqual(recoveries0 + 1, stats.appender_recoveries.load());
    // No quarantine for a non-early record.
    try testing.expectEqual(@as(usize, 0), f.rv.quarantined.items.len);
    try testing.expect(f.rv.get0(cr.custodyQuarantineKey(10).slice()) == null);
    // The phases exact-sum: total ≥ each.
    const total = stats.appender_recovery_phase_ns[@backingInt(Phase.total)].snapshot();
    try testing.expect(total.count >= 1);
}

test "every crash window: a recovery that fails right after each step's durable effects leaves a state the re-run converges from; a failure between the mark and the table release ROLLS BACK (the table back to leased{dead} — first touches still refused —, the slot's tree restored to its pre-install root or removed when adopted fresh, the page still Recovering)" {
    const gpa = testing.allocator;
    inline for (@typeInfo(Step).@"enum".field_values) |value| {
        const step: Step = @fromBackingInt(@intCast(value));
        const f = try Fixture.init(gpa, false);
        defer f.deinit();
        // Slot 11's tree is not in the recoverer's cache: adopted fresh.
        var removed = f.rv.trees.fetchRemove(11).?;
        removed.value.deinit(gpa);
        f.driver.seams.fail_at = step;
        const rollbacks0 = stats.appender_recovery_rollbacks.load();
        log.testing.expectErrorLogs(0);
        try testing.expectEqual(Outcome.failed, f.driver.recoverDead(f.death()));
        try testing.expectEqual(rollbacks0 + 1, stats.appender_recovery_rollbacks.load());
        const p = f.rv.pages.get(2).?;
        switch (step) {
            .decide, .preempt => try testing.expectEqual(page.State.live, p.state),
            .page_recovering, .read_ring, .mark_dead, .replay, .flush, .tails, .tree0, .table_release, .returns => try testing.expectEqual(page.State.recovering, p.state),
            .page_recovered, .recovered_record => try testing.expectEqual(page.State.recovered, p.state),
        }
        if (step != .page_recovered and step != .recovered_record) try testing.expect(!f.screen.isRecovering(2));
        switch (step) {
            .mark_dead, .replay, .flush, .tails, .tree0 => {
                // Rolled back: the table refuses a first touch, the stale
                // tree restored, the fresh guest removed, the page Recovering.
                for ([_]u16{ 10, 11 }) |s| {
                    const v = f.rv.table.view(s);
                    try testing.expect(v.state == .leased and v.holder == 2 and v.dead);
                    try testing.expect((try f.rv.table.acquire(s, 5, false)) == .refused);
                }
                try testing.expectEqual(@as(u64, 0x8000), f.rv.trees.get(10).?.root.addr);
                try testing.expect(f.rv.trees.get(11) == null);
                try testing.expect(f.rv.handover_notifies >= 1);
                if (step == .tree0) {
                    // Tree 0 already Unleased: the re-run ABSORBS both slots and
                    // owes only the page and the record.
                    try testing.expectEqual(cr.SlotVariant.unleased, f.rv.slotStateOf(10).?.variant);
                }
            },
            else => {},
        }
        // The re-run converges (an absorbed slot's RAM word follows tree 0
        // inside the run — nothing is reconciled by hand).
        f.driver.seams.fail_at = null;
        const outcome = f.driver.recoverDead(f.death());
        switch (step) {
            .recovered_record => {
                // Everything landed before the failure: nothing owed.
                try testing.expectEqual(Outcome.skipped, outcome);
                try testing.expect(try f.ledger.isRecovered(DEAD, 1));
            },
            .page_recovered => {
                // The recoverer died between the page and the record: the
                // poll completes the record off the Recovered page, once.
                try testing.expectEqual(Outcome.acted, outcome);
                try testing.expect(try f.ledger.isRecovered(DEAD, 1));
                try testing.expectEqual(Outcome.skipped, f.driver.recoverDead(f.death()));
            },
            .tree0 => {
                try testing.expectEqual(Outcome.acted, outcome);
                try testing.expectEqual(page.State.recovered, f.rv.pages.get(2).?.state);
                try testing.expect(try f.ledger.isRecovered(DEAD, 1));
                // The absorbed slots' RAM words followed tree 0.
                for ([_]u16{ 10, 11 }) |s| try testing.expectEqual(lt.State.unleased, f.rv.table.view(s).state);
            },
            else => {
                try testing.expectEqual(Outcome.acted, outcome);
                try f.expectConverged();
            },
        }
        try log.testing.expectAllFired();
    }
}

test "an EARLY record quarantines the recovered slots' custody for T_owner + 2 × skew_max past the record, durably beside the Unleased and in RAM; a rejoined member (listed live) is never recovered and its record is retired; a page that moved under the poll's snapshot is skipped; a window violation refuses loud" {
    const gpa = testing.allocator;
    const f = try Fixture.init(gpa, true);
    defer f.deinit();
    const q0 = stats.slot_custody_quarantines_installed.load();
    try testing.expectEqual(Outcome.acted, f.driver.recoverDead(f.death()));
    try testing.expectEqual(q0 + 2, stats.slot_custody_quarantines_installed.load());
    try testing.expectEqual(@as(usize, 2), f.rv.quarantined.items.len);
    const until = try cr.CustodyQuarantineWire.decode(f.rv.get0(cr.custodyQuarantineKey(10).slice()).?);
    try testing.expectEqual(custody.custodyQuarantineUntilNs(1_000 * std.time.ns_per_ms, 22 * std.time.ns_per_ms), until);
    try testing.expectEqual(@as(u64, (1_000 + 45_000 + 44) * std.time.ns_per_ms), until);
    // A rejoined member: the census lists it live — retired, skipped.
    const f2 = try Fixture.init(gpa, false);
    defer f2.deinit();
    const clocks = try shard.LeaseClocks.derive(45_000, 0, 100);
    var o = try shard.Owner.arm(gpa, clocks, 1, 1, 0, &.{}, 0);
    defer o.deinit();
    _ = o.join(.{ .id = shard.writerMemberId(death_ledger.wireOf(DEAD)), .role = .writer, .endpoint = .none, .pid = 1, .boot = @splat(0), .prior_epoch = null, .pr_key = 0, .mount = "/m", .row = null }, 0).granted;
    f2.driver.mount.census = &o;
    const d2 = f2.death();
    try testing.expectEqual(Outcome.skipped, f2.driver.recoverDead(d2));
    try testing.expect((try f2.ledger.readDeath(DEAD)) == null);
    try testing.expectEqual(page.State.live, f2.rv.pages.get(2).?.state);
    // A page that moved under the snapshot (a new term): skipped, counted.
    const f3 = try Fixture.init(gpa, false);
    defer f3.deinit();
    var moved = f3.rv.pages.get(2).?;
    moved.term += 1;
    const snap = f3.rv.pages.get(2).?;
    try f3.rv.pages.put(gpa, 2, moved);
    const skipped0 = stats.appender_recovery_skipped.load();
    try testing.expectEqual(Outcome.skipped, f3.driver.recoverRegion(snap, f3.death()) catch unreachable);
    try testing.expectEqual(skipped0 + 1, stats.appender_recovery_skipped.load());
    // A lease violation: the window names a slot never leased — refused loud.
    const f4 = try Fixture.init(gpa, false);
    defer f4.deinit();
    try f4.rv.window.append(gpa, .{ .slot = 99, .level = 0, .seq = 4100, .kind = .put, .local_ino = 5 });
    const tw0 = stats.meta_kv_replay_lease_violations.load();
    stats.meta_kv_replay_lease_violations.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(Outcome.failed, f4.driver.recoverDead(f4.death()));
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 1, stats.meta_kv_replay_lease_violations.load());
    try testing.expectEqual(page.State.recovering, f4.rv.pages.get(2).?.state);
    // An extent violation.
    const f5 = try Fixture.init(gpa, false);
    defer f5.deinit();
    try f5.rv.window.append(gpa, .{ .slot = 10, .level = 0, .seq = 4100, .kind = .alloc, .extent = 500 });
    stats.meta_kv_replay_extent_violations.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(Outcome.failed, f5.driver.recoverDead(f5.death()));
    try log.testing.expectAllFired();
    // A key violation: a leased slot's content record with no local ino.
    const f6 = try Fixture.init(gpa, false);
    defer f6.deinit();
    try f6.rv.window.append(gpa, .{ .slot = 10, .level = 0, .seq = 4100, .kind = .put, .local_ino = 0 });
    const kv0 = stats.meta_kv_replay_key_violations.load();
    stats.meta_kv_replay_key_violations.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(Outcome.failed, f6.driver.recoverDead(f6.death()));
    try log.testing.expectAllFired();
    try testing.expectEqual(kv0 + 1, stats.meta_kv_replay_key_violations.load());
    // NOT a violation: the slot's INTENT HOME (local 0 by the home law —
    // "guest cursors mint from 2, so no inode ever owns it"): a cross-owner
    // intent minted and retired inside the window is the dead appender's
    // own record, replayed like any other.
    const f7 = try Fixture.init(gpa, false);
    defer f7.deinit();
    try f7.rv.window.append(gpa, .{ .slot = 10, .level = 0, .seq = 4100, .kind = .put, .local_ino = 0, .home = true });
    try f7.rv.window.append(gpa, .{ .slot = 10, .level = 0, .seq = 4101, .kind = .delete, .local_ino = 0, .home = true });
    try testing.expectEqual(Outcome.acted, f7.driver.recoverDead(f7.death()));
    try testing.expectEqual(kv0 + 1, stats.meta_kv_replay_key_violations.load());
}

test "the volume's own SMO retires a dead grant's image mid-recovery (found by the shared hot directory row: daemon A aborted in `alloc_core.freePending` recovering B): the extent leaves the grant by ONE path — never returned by the census, never named by the record, parked in the coverage clock exactly once" {
    const gpa = testing.allocator;
    const f = try Fixture.init(gpa, false);
    defer f.deinit();
    // Slot 10's image 101 (the dead grant's) is compacted into 120 (the
    // volume's own pool) by the flush cycle.
    f.rv.smo_on_cycle = .{ .slot = 10, .old = 101, .new = 120 };
    try testing.expectEqual(Outcome.acted, f.driver.recoverDead(f.death()));
    for (f.rv.returned.items) |e| try testing.expect(e != 101);
    try testing.expectEqual(@as(usize, 1), std.mem.count(u64, f.rv.fifo.items, &.{101}));
    const g = cr.decodeExtentGrant(f.rv.get0(cr.extentGrantKey(2).slice()).?) catch unreachable;
    var runs: [16]GrantRun = undefined;
    for (g.collect(&runs)) |r| try testing.expect(!r.contains(101));
}

test "a release of recovered regions frees a Recovered page no lease names (identity and term kept) and leaves one a lease still names; the poll binding composes the hooks" {
    const gpa = testing.allocator;
    const f = try Fixture.init(gpa, false);
    defer f.deinit();
    try testing.expectEqual(Outcome.acted, f.driver.recoverDead(f.death()));
    const Named = struct {
        named: bool,
        fn isNamed(ctx: *anyopaque, _: WriterIdentity) bool {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            return self.named;
        }
    };
    var named: Named = .{ .named = true };
    var pb: PollBinding = .{ .driver = &f.driver, .named_ctx = &named, .namedAsHolder = &Named.isNamed };
    const hook = pb.hook();
    try testing.expectEqual(@as(u32, 0), hook.releaseRecovered(hook.ctx));
    // A Recovered page is not a pending region (the retirement's word).
    try testing.expect(!hook.regionPending(hook.ctx, DEAD));
    named.named = false;
    try testing.expectEqual(@as(u32, 1), hook.releaseRecovered(hook.ctx));
    const p = f.rv.pages.get(2).?;
    try testing.expectEqual(page.State.free, p.state);
    try testing.expectEqual(@as(u64, 4), p.term);
    // The ring went back to the heap AFTER the page: 512 KiB.
    try testing.expectEqual(@as(u64, 512 << 10), f.rv.rings_freed);
    try testing.expectEqual(@as(u8, 0), p.segments.len);
    try testing.expect(p.identity.sameMount(.{ .node_token = DEAD.node_token, .mount_slot = DEAD.mount_slot }));
    try testing.expect(!hook.regionPending(hook.ctx, DEAD));
    try testing.expectEqual(death_ledger.RecoverOutcome.skipped, hook.recoverDead(hook.ctx, f.death()));
}

const ClearRam = struct {
    flock: [2]bool = .{ false, false },
    claim_age: [2]?u64 = .{ null, null },
    pages: std.AutoHashMapUnmanaged(u32, page.Page) = .empty,
    own: bool = false,
    fresh_registration: bool = false,
    writes: std.ArrayList(u8) = .empty,
    fail_bring_up: bool = false,
    gpa: Allocator,

    fn deinit(self: *ClearRam) void {
        self.pages.deinit(self.gpa);
        self.writes.deinit(self.gpa);
    }
    fn s(ctx: *anyopaque) *ClearRam {
        return @ptrCast(@alignCast(ctx));
    }
    fn flockHeld(ctx: *anyopaque, vol0: bool) bool {
        return s(ctx).flock[@intFromBool(vol0)];
    }
    fn claimAge(ctx: *anyopaque, vol0: bool) ?u64 {
        return s(ctx).claim_age[@intFromBool(vol0)];
    }
    fn readPage(ctx: *anyopaque, id: u32) Error!?page.Page {
        return s(ctx).pages.get(id);
    }
    fn pageIsOwn(ctx: *anyopaque, _: *const page.Page) bool {
        return s(ctx).own;
    }
    fn fresh(ctx: *anyopaque, _: u64) bool {
        return s(ctx).fresh_registration;
    }
    fn writeRec(ctx: *anyopaque, p: *const page.Page) Error!void {
        const self = s(ctx);
        try self.pages.put(self.gpa, p.appender_id, p.*);
        try self.writes.append(self.gpa, 1);
    }
    fn bringUp(ctx: *anyopaque) Error!void {
        const self = s(ctx);
        if (self.fail_bring_up) return error.Io;
        try self.writes.append(self.gpa, 2);
    }
    fn early(ctx: *anyopaque, _: WriterIdentity) Error!void {
        try s(ctx).writes.append(s(ctx).gpa, 3);
    }
    fn leave(ctx: *anyopaque) Error!void {
        const self = s(ctx);
        try self.writes.append(self.gpa, 4);
        var p = self.pages.get(2).?;
        p.state = .free;
        try self.pages.put(self.gpa, 2, p);
    }
    const vt: ClearSeam.VTable = .{ .flockHeld = &flockHeld, .claimAgeMs = &claimAge, .readPage = &readPage, .pageIsOwn = &pageIsOwn, .freshRegistrationFor = &fresh, .writeRecoveringPage = &writeRec, .bringUp = &bringUp, .recordEarlyDeath = &early, .leave = &leave };
    fn seam(self: *ClearRam) ClearSeam {
        return .{ .ctx = self, .vt = &vt };
    }
};

test "appender clear: the seven refusals in order (live mount, a fresh claim on either volume, nothing to clear, own residue, appender 0, the parked-joiner window, a fresh registration), then the four writes in order; a crash after write 1 leaves a Recovering page the re-run clears idempotently" {
    const gpa = testing.allocator;
    var c: ClearRam = .{ .gpa = gpa };
    defer c.deinit();
    var live = page.Page.free(2, 1, .{});
    live.state = .live;
    live.identity = .{ .node_token = 0xD0D0, .mount_slot = 1, .writer_id = 2 };
    try c.pages.put(gpa, 2, live);
    var p0 = page.Page.free(0, 1, .{});
    p0.state = .live;
    try c.pages.put(gpa, 0, p0);
    const t_owner: u64 = 45_000;
    const t_park: u64 = 91_500;
    const runs0 = stats.appender_clear_runs.load();
    c.flock[0] = true;
    try testing.expectEqual(ClearRefusal.live_mounted, (try clearAppender(c.seam(), 2, t_owner, t_park)).refused);
    c.flock[0] = false;
    c.flock[1] = true;
    try testing.expectEqual(ClearRefusal.live_mounted, (try clearAppender(c.seam(), 2, t_owner, t_park)).refused);
    c.flock[1] = false;
    c.claim_age[0] = 10_000;
    try testing.expectEqual(ClearRefusal.fresh_claim, (try clearAppender(c.seam(), 2, t_owner, t_park)).refused);
    c.claim_age[0] = null;
    c.claim_age[1] = 45_000;
    try testing.expectEqual(ClearRefusal.fresh_claim, (try clearAppender(c.seam(), 2, t_owner, t_park)).refused);
    c.claim_age[1] = null;
    try testing.expectEqual(ClearRefusal.nothing_to_clear, (try clearAppender(c.seam(), 3, t_owner, t_park)).refused);
    c.own = true;
    try testing.expectEqual(ClearRefusal.own_residue, (try clearAppender(c.seam(), 2, t_owner, t_park)).refused);
    c.own = false;
    try testing.expectEqual(ClearRefusal.appender_zero, (try clearAppender(c.seam(), 0, t_owner, t_park)).refused);
    c.claim_age[1] = t_owner + t_park - 1;
    try testing.expectEqual(ClearRefusal.parked_joiner_window, (try clearAppender(c.seam(), 2, t_owner, t_park)).refused);
    c.claim_age[1] = t_owner + t_park;
    c.fresh_registration = true;
    try testing.expectEqual(ClearRefusal.fresh_registration, (try clearAppender(c.seam(), 2, t_owner, t_park)).refused);
    c.fresh_registration = false;
    try testing.expectEqual(runs0 + 9, stats.appender_clear_runs.load());
    try testing.expectEqual(@as(usize, 0), c.writes.items.len);
    // A crash after write 1: the page is Recovering; the re-run clears it.
    c.fail_bring_up = true;
    try testing.expectError(error.Io, clearAppender(c.seam(), 2, t_owner, t_park));
    try testing.expectEqual(page.State.recovering, c.pages.get(2).?.state);
    try testing.expectEqual(@as(u64, 0), c.pages.get(2).?.recovered_by_term);
    c.fail_bring_up = false;
    const out = try clearAppender(c.seam(), 2, t_owner, t_park);
    try testing.expectEqual(page.State.recovering, out.cleared.was);
    try testing.expectEqualSlices(u8, &.{ 1, 1, 2, 3, 4 }, c.writes.items);
    try testing.expectEqual(page.State.free, c.pages.get(2).?.state);
    try testing.expectEqual(ClearRefusal.nothing_to_clear, (try clearAppender(c.seam(), 2, t_owner, t_park)).refused);
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeClearRefusal(&w, .appender_zero, 0);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "ids ≥ 1") != null);
}
