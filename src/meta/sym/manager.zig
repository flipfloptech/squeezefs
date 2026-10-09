//! The manager lease (charter §5.5 "The manager lease"; `docs/porting/
//! sym_appenders_manager.md` §3–§5): per volume, the D0 ladder's winner —
//! an ordinary appender with `is_manager` on its page (there is NO
//! separate election; `bind.zig` stamps the bit at the join); volume 0's
//! manager is the set-wide role holder. It writes tree 0 and the fixed
//! ledger, allocates appender pages, grants/returns extents and
//! maintains the UNLEASED slot trees. ZERO per-op work: resolving a holder
//! is a projection of tree 0 (KD-SYM-17).
//!
//! PORTABLE core over a `Volume` seam (tree 0, the directory pages, the
//! bitmap, ring 0's head, the checkpoint kick) the backend binds and the
//! tests satisfy in RAM. The verbs of THIS wave: `joinAppender`,
//! `leaveAppender`, `extentGrant`, `returnExtents`, `growRing`,
//! `publishRoots`, `publishEndpoint`, `resolveEndpoint`. The slot, token,
//! dir-rename, shared-block and alloc-lease verbs are typed members of the
//! vocabulary with EMPTY handler slots: a frame naming one answers
//! `rejected{verb_unserved}` — the wire never guesses at a grant-bearing
//! frame.
//!
//! Every verb is idempotent against DURABLE state (a `Live` page under the
//! identity IS the join; a `Free` page IS the leave; the page word ∩ the
//! record covering `want` IS the grant; a return of extents outside the
//! record is `already`), never a RAM dedup window. Every wire word is
//! SCREENED before any effect ("bounded codec = bounded execution"): a
//! wire integer is a claim, never allocation authority. Control entries
//! admit in the USER class — a verb storm competes with user commits and
//! can never eat the checkpoint reserve; a full window answers `deferred`
//! and kicks the checkpoint. The reply classifier is ONE function over
//! every verb's error.
//!
//! Lock order: `smo` (the per-volume SMO mutex) BEFORE `manager_verbs` for
//! any verb that moves a lease or consumes a checkpoint seq (the
//! checkpoint task's own order); within a region `grant` BEFORE `page`.
//! The structural door takes `smo` first and cycles ring 0's window clear
//! of the granted slots, bounded by `COVER_CYCLES_MAX`.

const std = @import("std");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const checkpoint = @import("../checkpoint.zig");
const geometry = @import("../geometry.zig");
const journal = @import("../journal.zig");
const key = @import("../key.zig");
const node_seq = @import("../node_seq.zig");
const cr = @import("control_records.zig");
const page = @import("appender_page.zig");
const dir = @import("appender_dir.zig");
const region = @import("region.zig");
const grant = @import("extent_grant.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
pub const Extent = page.Extent;
pub const GrantRun = cr.GrantRun;
pub const Identity = page.Identity;

// ---------------------------------------------------------------------------
// Instruments (charter §5.5 "Gauges to publish": the manager family).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var manager_verbs: counters.ShardedCounter = .{};
    /// `manager_verbs_per_s` and `manager_load_pct` — MEASURED (Σ service
    /// ÷ wall from the first verb), never declared.
    pub var manager_verbs_per_s: counters.Gauge = .{};
    pub var manager_load_pct: counters.Gauge = .{};
    /// Exact-sum `admit / execute / reply / total`.
    pub var manager_service_ns: [Phase.count]counters.ShardedLatencyHistogram = @splat(.{});
    /// A verb answered from durable state it had already produced.
    pub var manager_verb_replays: counters.ShardedCounter = .{};
    /// The durable witness contradicts the caller: must stay 0.
    pub var manager_verb_refusals: counters.Tripwire = .init("manager_verb_refusals");
    /// A wire integer names what durable state cannot — the buggy/hostile
    /// peer class, kept OFF the must-stay-0 gauge.
    pub var manager_verb_rejected: counters.ShardedCounter = .{};
    /// A verb answered `deferred` (a full USER window, the door's moving
    /// tail): nothing written, the caller retries.
    pub var manager_verb_deferrals: counters.ShardedCounter = .{};
    /// A join past the ring budget's remainder: the lever is the metadata
    /// volume COUNT (0002 row 232 — a counter, not a cannot-happen).
    pub var manager_capacity_refusals: counters.ShardedCounter = .{};
    pub var appender_joins: counters.ShardedCounter = .{};
    pub var appender_leaves: counters.ShardedCounter = .{};
    /// Whole-directory walks (joins only — spec Q-14).
    pub var appender_directory_reads: counters.ShardedCounter = .{};
    pub var appender_directory_links_grown: counters.ShardedCounter = .{};
    pub var appender_join_residue_returned: counters.ShardedCounter = .{};
    pub var appender_pending_segments_returned: counters.ShardedCounter = .{};
    pub var appender_grow_ring_short_declines: counters.ShardedCounter = .{};
    pub var appender_ring_grows: counters.ShardedCounter = .{};
    pub var extent_grants: counters.ShardedCounter = .{};
    pub var extent_grant_extents: counters.ShardedCounter = .{};
    pub var extent_returns: counters.ShardedCounter = .{};
    pub var extent_grant_returned: counters.ShardedCounter = .{};
    /// The page word named an extent the record no longer grants.
    pub var extent_grant_stale_page_words: counters.ShardedCounter = .{};
    /// Two custodians of one extent: must stay 0.
    pub var extent_grant_conflicts: counters.Tripwire = .init("extent_grant_conflicts");
    /// A return that would split a record run past the cap (capacity class).
    pub var extent_return_run_cap_refusals: counters.ShardedCounter = .{};
    /// Cycles the structural door ran; doors deferred on a moving tail.
    pub var slot_grant_door_cycles: counters.ShardedCounter = .{};
    pub var slot_grant_deferrals: counters.ShardedCounter = .{};
    pub var meta_kv_forest_root_publishes: counters.ShardedCounter = .{};
};

pub const Phase = enum(u8) {
    admit,
    execute,
    reply,
    total,

    pub const count = @typeInfo(Phase).@"enum".field_names.len;
};

/// Per-manager measured load: `verbs_per_s` and `load_pct = Σ service ÷
/// wall from the first verb`.
pub const VerbLedger = struct {
    first_ns: u64 = 0,
    verbs: u64 = 0,
    service_ns: u64 = 0,

    pub fn record(self: *VerbLedger, now_ns: u64, admit_ns: u64, execute_ns: u64, reply_ns: u64) void {
        if (self.first_ns == 0) self.first_ns = now_ns -| (admit_ns + execute_ns + reply_ns);
        self.verbs += 1;
        const total = admit_ns + execute_ns + reply_ns;
        self.service_ns += total;
        stats.manager_service_ns[@backingInt(Phase.admit)].record(admit_ns);
        stats.manager_service_ns[@backingInt(Phase.execute)].record(execute_ns);
        stats.manager_service_ns[@backingInt(Phase.reply)].record(reply_ns);
        stats.manager_service_ns[@backingInt(Phase.total)].record(total);
        stats.manager_verbs.inc();
        const wall = now_ns -| self.first_ns;
        if (wall > 0) {
            stats.manager_verbs_per_s.set(self.verbs * std.time.ns_per_s / wall);
            stats.manager_load_pct.set(@min(self.service_ns * 100 / wall, 100));
        }
    }
};

// ---------------------------------------------------------------------------
// The vocabulary.
// ---------------------------------------------------------------------------

/// Explicit u16 codes: the wire never carries a declaration index.
pub const Verb = enum(u16) {
    join_appender = 1,
    leave_appender = 2,
    extent_grant = 3,
    return_extents = 4,
    grow_ring = 5,
    publish_roots = 6,
    publish_endpoint = 7,
    resolve_endpoint = 8,
    // The slot-lease wave.
    acquire_slots = 16,
    acquire_slot = 17,
    offer_slot = 18,
    release_slot = 19,
    resolve_slot = 20,
    // The cross-owner wave.
    dir_rename_lock = 32,
    dir_rename_unlock = 33,
    // The data-plane wave.
    mark_shared = 48,
    share_block = 49,
    release_shared = 50,
    block_grant = 51,
    return_blocks = 52,
    alloc_lease_acquire = 53,
    alloc_lease_bitmap = 54,
    alloc_lease_release = 55,
    // The recovery wave.
    record_recovered = 64,
    record_death = 65,
};

/// The frame statuses (the reply classifier's codomain).
pub const Status = enum(u16) {
    ok = 0,
    /// Another vocabulary version.
    schema = 1,
    malformed = 2,
    /// This node holds no manager lease for the volume named.
    not_manager = 3,
    /// The durable witness contradicts the caller (`manager_verb_refusals`).
    refused = 48,
    /// A wire integer names what durable state cannot (`manager_verb_rejected`).
    rejected = 49,
    /// "Not now", nothing written, retry.
    deferred = 50,
};

/// Typed reasons the client renders (never a `String` on a control
/// frame) — the classifier's codomain.
pub const Reason = enum(u16) {
    none = 0,
    /// The verb is in the vocabulary but has no executor in this binary.
    verb_unserved,
    /// The join's remainder cannot reach a floor ring (the volume count).
    capacity,
    /// The heap's class: a carve could not serve one extent.
    no_space,
    /// The USER window is full; the checkpoint was kicked.
    window_full,
    /// The structural door's tail moved but did not clear at the bound.
    door_moving_tail,
    /// A wire word the screen refused before any effect.
    screen_rejected,
    /// The durable witness contradicts the caller.
    durable_witness,
    io,
    corrupt,
};

pub const JoinAppender = struct {
    identity: Identity,
    /// 0 = derive (the hint, else the floor).
    ring_want_bytes: u64 = 0,
    /// The joiner's HOME volume ordinal (the membership shard key).
    home_volume: u16 = 0,
};

pub const Joined = struct {
    appender_id: u32,
    /// Device offset of the directory slot A.
    page_addr: u64,
    /// The page's term and generation as the manager wrote them: the
    /// joiner's own page writes continue the rotation from here.
    term: u64,
    generation: u64,
    /// The seq-space continuation the page carries (`head_hint`).
    head_hint: u64,
    ring_segments: page.SegmentTable,
    /// The RAM grant: the reply's word, never the page read.
    grant: [page.GRANT_RUNS_MAX * 4]GrantRun,
    n_grant: u8,
    already: bool,
    node_seq_base: u64,

    pub fn grantRuns(self: *const Joined) []const GrantRun {
        return self.grant[0..self.n_grant];
    }
};

pub const LeaveAppender = struct {
    identity: Identity,
    appender_id: u32,
    unclaimed: []const GrantRun,
    /// The manager's own region leaving (the in-process door): the same
    /// law per region; a WIRE leave of an own region is rejected.
    in_process: bool = false,
};

pub const Left = struct { already: bool };

pub const ExtentGrant = struct {
    appender_id: u32,
    /// 0 = the derived size.
    want: u64,
    /// The page's unclaimed word as the caller holds it (the remainder is
    /// its intersection with the record).
    page_word: []const GrantRun = &.{},
};

pub const Granted = struct {
    runs: []GrantRun,
    /// Nothing carved: the remainder covered `want`.
    replayed: bool,
};

pub const ReturnExtents = struct {
    appender_id: u32,
    runs: []GrantRun,
};

pub const Returned = struct { cleared: u64, already: u64 };

pub const GrowRing = struct {
    appender_id: u32,
    want_bytes: u64,
};

pub const RingGrown = struct { segment: ?Extent };

pub const SlotRootWord = struct {
    slot: u16,
    g: u32,
    root: cr.Root,
    cursor: u64,
    slot_tree_extents: u32,
};

pub const PublishRoots = struct {
    appender_id: u32,
    roots: []const SlotRootWord,
};

pub const RootsPublished = struct { published: u64, already: u64 };

/// A `sockaddr_storage` image.
pub const Endpoint = struct {
    bytes: [128]u8 = @splat(0),
    len: u8 = 0,

    pub fn slice(self: *const Endpoint) []const u8 {
        return self.bytes[0..self.len];
    }

    pub fn eql(a: Endpoint, b: Endpoint) bool {
        return std.mem.eql(u8, a.slice(), b.slice());
    }
};

pub const PublishEndpoint = struct {
    identity: Identity,
    appender_id: u32,
    endpoint: Endpoint,
    pr_key: u64,
};

pub const Published = struct { already: bool };

pub const ResolveEndpoint = struct { appender_id: u32 };

pub const EndpointReply = struct { endpoint: ?Endpoint };

/// The verb error set — ONE classifier below.
pub const VerbError = error{
    /// The screen refused a wire word.
    Rejected,
    /// The durable witness contradicts the caller.
    Refused,
    /// Nothing written; retry (the door's moving tail, a full window).
    Deferred,
    /// The USER window is full: kick the checkpoint, then `Deferred`.
    JournalReserveExhausted,
    NoSpace,
    Capacity,
    Io,
    Corrupt,
    OutOfMemory,
};

pub const Classified = struct { status: Status, reason: Reason };

/// ONE classifier over every verb's error: a screen's `Rejected` →
/// `rejected`; `Deferred` → `deferred`; a full window → the checkpoint kick
/// + `deferred`; everything else → `refused` with its reason.
pub fn classify(e: VerbError, kick: ?*const fn (ctx: *anyopaque) void, kick_ctx: ?*anyopaque) Classified {
    return switch (e) {
        error.Rejected => blk: {
            stats.manager_verb_rejected.inc();
            break :blk .{ .status = .rejected, .reason = .screen_rejected };
        },
        error.Deferred => blk: {
            stats.manager_verb_deferrals.inc();
            break :blk .{ .status = .deferred, .reason = .door_moving_tail };
        },
        error.JournalReserveExhausted => blk: {
            if (kick) |k| k(kick_ctx.?);
            stats.manager_verb_deferrals.inc();
            break :blk .{ .status = .deferred, .reason = .window_full };
        },
        error.Capacity => .{ .status = .refused, .reason = .capacity },
        error.NoSpace => .{ .status = .refused, .reason = .no_space },
        error.Refused => .{ .status = .refused, .reason = .durable_witness },
        error.Io => .{ .status = .refused, .reason = .io },
        error.Corrupt => .{ .status = .refused, .reason = .corrupt },
        error.OutOfMemory => .{ .status = .refused, .reason = .io },
    };
}

// ---------------------------------------------------------------------------
// The volume seam.
// ---------------------------------------------------------------------------

pub const Geometry = struct {
    node_size: u64,
    volume_len: u64,
    heap: Extent,
    total_extents: u64,
    journal: Extent,
    appender_dir: Extent,
    /// `nodeSeqBase(uuid)`.
    volume_base: u64,
    grain: u64,
    /// `SQUEEZEFS_SYM_MINT_SLOTS` resolved (the pool floor's `M`).
    mint_slots: u64,
};

/// One tree-0 mutation or allocator delta of a control entry.
pub const ControlRecord = union(enum) {
    put: struct { key: []const u8, value: []const u8 },
    delete: []const u8,
    /// A kind-4 `alloc(extent)` delta (the bit is already set in RAM).
    alloc: u64,
    /// A kind-4 `free(extent, retire 0)` delta — immediately reusable.
    free: u64,
};

pub const KeyValue = struct { key: []u8, value: []u8 };

pub const ControlError = error{ JournalReserveExhausted, ValueTooLarge, Io, Corrupt, OutOfMemory };

pub const PageWriteOp = struct { offset: u64, image: *const [page.PAGE_LEN]u8 };

pub const Volume = struct {
    ctx: *anyopaque,
    vt: *const VTable,

    pub const VTable = struct {
        geometry: *const fn (ctx: *anyopaque) Geometry,
        /// Tree 0 point lookup (caller frees).
        lookup: *const fn (ctx: *anyopaque, gpa: Allocator, k: []const u8) error{ Io, OutOfMemory }!?[]u8,
        /// Tree 0 inclusive range (caller frees each key/value).
        range: *const fn (ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, out: *std.ArrayList(KeyValue)) error{ Io, OutOfMemory }!void,
        /// ONE ring-0 control entry in the USER class, applied, written,
        /// BARRIERED (a verb answers only from durable state).
        controlEntry: *const fn (ctx: *anyopaque, recs: []const ControlRecord) ControlError!void,
        readPage: *const fn (ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void,
        /// The writes land, then ONE barrier.
        writePages: *const fn (ctx: *anyopaque, writes: []const PageWriteOp) error{Io}!void,
        zero: *const fn (ctx: *anyopaque, chunks: []const region.ZeroChunk) error{Io}!void,
        /// The internal class (the manager's structural claims).
        claimInternal: *const fn (ctx: *anyopaque) error{NoSpace}!u64,
        /// An extent no durable record ever named goes back at once.
        releaseUnpublished: *const fn (ctx: *anyopaque, extent: u64) void,
        freeExtents: *const fn (ctx: *anyopaque) u64,
        ring0Head: *const fn (ctx: *anyopaque) u64,
        /// One of THIS mount's own regions (its leave/grow are in-process).
        ownsRegion: *const fn (ctx: *anyopaque, appender_id: u32) bool,
        /// An own region's RAM grant holds the extent CLAIMED or PENDING.
        regionGrantHeld: *const fn (ctx: *anyopaque, appender_id: u32, extent: u64) bool,
        /// Ring 0's window (above the tail) names no record of these slots.
        ring0WindowClearOf: *const fn (ctx: *anyopaque, slots: []const u16) bool,
        /// One barriered checkpoint cycle under the SMO hold; the new tail.
        cycleCheckpoint: *const fn (ctx: *anyopaque) error{ Io, Corrupt }!u64,
        kickCheckpoint: *const fn (ctx: *anyopaque) void,
    };

    fn geo(self: Volume) Geometry {
        return self.vt.geometry(self.ctx);
    }

    fn reader(self: *const Volume) dir.PageReader {
        return .{ .ctx = @constCast(self), .read = readPageSeam };
    }

    fn readPageSeam(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self: *const Volume = @ptrCast(@alignCast(ctx));
        return self.vt.readPage(self.ctx, offset, out);
    }
};

// ---------------------------------------------------------------------------
// The manager.
// ---------------------------------------------------------------------------

pub const Lease = enum { held, peer, vacant };

pub const EndpointRow = struct { appender_id: u32, endpoint: Endpoint, pr_key: u64 };

pub const Manager = struct {
    gpa: Allocator,
    /// The verbs lock's executor (a lane fiber parks; `std.testing.io` in
    /// tests).
    io: std.Io,
    vol: Volume,
    /// The lease posture word: `held | peer:<node> | vacant`.
    lease: Lease = .vacant,
    peer_node: u64 = 0,
    /// The serving era: the manager's page term.
    term: u64 = 0,
    ledger: VerbLedger = .{},
    /// ONE verb at a time (`manager_verbs`, taken AFTER `smo`).
    verbs_lock: std.Io.Mutex = .init,
    /// The appender → endpoint table (the per-mount `SlotHolderCache`'s
    /// source; the durable home is the member row of the membership wave).
    endpoints: std.ArrayList(EndpointRow) = .empty,
    /// The directory's Live count as of the last join (the grant cap
    /// divides by it; `appenders_known`).
    appenders_known: u64 = 1,
    /// The slot-lease plane's verbs (`sym/slots/acquire.zig` `Verbs`),
    /// bound at the arm: the slot verbs' handlers recover it from the
    /// manager the table dispatches on. Opaque here so the manager never
    /// imports the plane it serves.
    slot_plane: ?*anyopaque = null,

    pub fn init(gpa: Allocator, io: std.Io, vol: Volume) Manager {
        return .{ .gpa = gpa, .io = io, .vol = vol };
    }

    pub fn deinit(self: *Manager) void {
        self.endpoints.deinit(self.gpa);
    }

    pub fn holdLease(self: *Manager, term: u64) void {
        self.lease = .held;
        self.term = term;
    }

    pub fn leaseWord(self: *const Manager, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self.lease) {
            .held => try w.writeAll("held"),
            .peer => try w.print("peer:0x{x}", .{self.peer_node}),
            .vacant => try w.writeAll("vacant"),
        }
    }

    fn requireLease(self: *const Manager) error{NotManager}!void {
        if (self.lease != .held) return error.NotManager;
    }

    // ---- tree 0 helpers ----

    fn readSlotState(self: *Manager, slot: u16) VerbError!?cr.SlotState {
        const raw = (try self.vol.vt.lookup(self.vol.ctx, self.gpa, cr.slotStateKey(slot).slice())) orelse return null;
        defer self.gpa.free(raw);
        return cr.SlotStateWire.decode(raw) catch return error.Corrupt;
    }

    const GrantRecord = struct {
        runs: std.ArrayList(GrantRun) = .empty,

        fn deinit(self: *GrantRecord, gpa: Allocator) void {
            self.runs.deinit(gpa);
        }

        fn contains(self: *const GrantRecord, extent: u64) bool {
            for (self.runs.items) |r| if (r.contains(extent)) return true;
            return false;
        }

        fn total(self: *const GrantRecord) u64 {
            var n: u64 = 0;
            for (self.runs.items) |r| n += r.len;
            return n;
        }
    };

    fn readGrantRecord(self: *Manager, appender_id: u32) VerbError!GrantRecord {
        var rec: GrantRecord = .{};
        errdefer rec.deinit(self.gpa);
        const raw = (try self.vol.vt.lookup(self.vol.ctx, self.gpa, cr.extentGrantKey(appender_id).slice())) orelse return rec;
        defer self.gpa.free(raw);
        const d = cr.decodeExtentGrant(raw) catch return error.Corrupt;
        try rec.runs.resize(self.gpa, d.n_runs);
        _ = d.collect(rec.runs.items);
        return rec;
    }

    fn readHint(self: *Manager, id: cr.StableIdentity) VerbError!cr.AppenderHint {
        const raw = (try self.vol.vt.lookup(self.vol.ctx, self.gpa, cr.appenderHintKey(id).slice())) orelse return .{};
        defer self.gpa.free(raw);
        return cr.AppenderHintWire.decode(raw) catch return error.Corrupt;
    }

    fn readIncarnations(self: *Manager) VerbError!u64 {
        const raw = (try self.vol.vt.lookup(self.vol.ctx, self.gpa, cr.nodeSeqIncarnationsKey().slice())) orelse return 0;
        defer self.gpa.free(raw);
        return cr.decodeNodeSeqIncarnations(raw) catch return error.Corrupt;
    }

    fn control(self: *Manager, recs: []const ControlRecord) VerbError!void {
        self.vol.vt.controlEntry(self.vol.ctx, recs) catch |e| return switch (e) {
            error.JournalReserveExhausted => error.JournalReserveExhausted,
            error.ValueTooLarge, error.Corrupt => error.Corrupt,
            error.Io => error.Io,
            error.OutOfMemory => error.OutOfMemory,
        };
    }

    /// The record's run cap under the volume's tree-0 value cap.
    fn runCap(self: *Manager) u32 {
        return cr.extentGrantMaxRuns(geometry.recordValueCap(self.vol.geo().node_size));
    }

    /// Every OTHER appender's record: a custodian of `extent` elsewhere is
    /// `extent_grant_conflicts`.
    fn heldElsewhere(self: *Manager, appender_id: u32, extents: []const u64) VerbError!bool {
        const r = cr.rangeOf(.extent_grant);
        var kvs: std.ArrayList(KeyValue) = .empty;
        defer freeKvs(self.gpa, &kvs);
        try self.vol.vt.range(self.vol.ctx, self.gpa, r.start.slice(), r.end.slice(), &kvs);
        for (kvs.items) |kv| {
            const id = cr.appenderOfKey(kv.key) orelse continue;
            if (id == appender_id) continue;
            const d = cr.decodeExtentGrant(kv.value) catch return error.Corrupt;
            for (extents) |e| if (d.contains(e)) return true;
        }
        return false;
    }

    fn freeKvs(gpa: Allocator, kvs: *std.ArrayList(KeyValue)) void {
        for (kvs.items) |kv| {
            gpa.free(kv.key);
            gpa.free(kv.value);
        }
        kvs.deinit(gpa);
    }

    // ---- the directory ----

    const Directory = struct {
        links: std.ArrayList(dir.Link) = .empty,
        /// Live pages, ring bytes in use, known count.
        live: u64 = 0,
        rings_in_use: u64 = 0,

        fn deinit(self: *Directory, gpa: Allocator) void {
            self.links.deinit(gpa);
        }
    };

    /// ONE whole-directory walk per JOIN (the census: live count, rings in
    /// use); the other verbs read the ONE page they name (spec Q-14).
    fn readDirectory(self: *Manager) VerbError!Directory {
        const g = self.vol.geo();
        var d: Directory = .{};
        errdefer d.deinit(self.gpa);
        dir.walkChain(self.gpa, self.vol.reader(), g.appender_dir, g.node_size, g.heap, g.total_extents, &d.links) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
        };
        // Appender 0: the fixed ring's page (always Live under a serving manager).
        const offs0 = page.appender0PageOffsets(g.journal.start);
        const p0 = try self.readFourSlots(0, .{ offs0[0], offs0[1] });
        if (p0) |p| if (p.state == .live) {
            d.live += 1;
            d.rings_in_use += p.ringBytes();
        };
        for (d.links.items) |l| {
            var pair: u32 = 0;
            while (pair < l.header.pairs) : (pair += 1) {
                const imgs = dir.readPair(self.vol.reader(), l, pair) catch return error.Io;
                if (imgs.foreign) return error.Corrupt;
                const p = imgs.newest() orelse continue;
                if (p.state == .live) {
                    d.live += 1;
                    d.rings_in_use += p.ringBytes();
                }
            }
        }
        stats.appender_directory_reads.inc();
        self.appenders_known = @max(d.live, 1);
        return d;
    }

    /// Newest-valid-wins over the four slots; the ring-side pair only once
    /// a directory image names a first segment.
    fn readFourSlots(self: *Manager, appender_id: u32, dir_slots: [2]u64) VerbError!?page.Page {
        var bufs: [page.PAGE_SLOTS][page.PAGE_LEN]u8 = undefined;
        var imgs: [page.PAGE_SLOTS]?[]const u8 = .{ null, null, null, null };
        for (dir_slots, 0..) |off, i| {
            self.vol.vt.readPage(self.vol.ctx, off, &bufs[i]) catch return error.Io;
            imgs[i] = &bufs[i];
        }
        const dir_best = page.newestValid(.{ imgs[0], imgs[1], null, null }) catch return error.Corrupt;
        if (dir_best) |p| if (p.segments.len > 0) {
            const first = p.segments.items[0];
            const side: [2]u64 = if (appender_id == journal.FIXED_RING_APPENDER) .{ page.appender0PageOffsets(first.start)[2], page.appender0PageOffsets(first.start)[3] } else page.ringSideOffsets(first);
            for (side, 2..) |off, i| {
                self.vol.vt.readPage(self.vol.ctx, off, &bufs[i]) catch return error.Io;
                imgs[i] = &bufs[i];
            }
        };
        return page.newestValid(imgs) catch return error.Corrupt;
    }

    /// The directory slots of `appender_id` (≥ 1) through the chain.
    fn dirSlotsOf(self: *Manager, appender_id: u32) VerbError!?[2]u64 {
        const g = self.vol.geo();
        if (appender_id == journal.FIXED_RING_APPENDER) return .{ page.appender0PageOffsets(g.journal.start)[0], page.appender0PageOffsets(g.journal.start)[1] };
        var links: std.ArrayList(dir.Link) = .empty;
        defer links.deinit(self.gpa);
        dir.walkChain(self.gpa, self.vol.reader(), g.appender_dir, g.node_size, g.heap, g.total_extents, &links) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
        };
        const loc = dir.locate(appender_id, dir.pairCount(g.node_size));
        if (loc.chain_index >= links.items.len) return null;
        return dir.pairOffsets(links.items[loc.chain_index].extent, loc.pair);
    }

    /// The page of `appender_id` as the device holds it.
    pub fn readPageOf(self: *Manager, appender_id: u32) VerbError!?page.Page {
        const slots = (try self.dirSlotsOf(appender_id)) orelse return null;
        return self.readFourSlots(appender_id, slots);
    }

    /// `write_foreign_page` (the recovery's `Recovering` / `Recovered`
    /// writes): ANOTHER appender's page into both of its directory slots,
    /// one barrier — the directory-first law for a page whose ring-side
    /// slots this mount never writes.
    pub fn writeForeignPage(self: *Manager, p: *page.Page) VerbError!void {
        const slots = (try self.dirSlotsOf(p.appender_id)) orelse return error.Rejected;
        return self.writeBothDirSlots(p, slots);
    }

    /// Write `p` into BOTH directory slots (generations +1, +2), one
    /// barrier — the join's and the leave's law.
    fn writeBothDirSlots(self: *Manager, p: *page.Page, dir_slots: [2]u64) VerbError!void {
        var imgs: [2][page.PAGE_LEN]u8 = undefined;
        var ops: [2]PageWriteOp = undefined;
        for (0..2) |i| {
            p.generation += 1;
            p.encode(&imgs[i]) catch return error.Corrupt;
            ops[i] = .{ .offset = dir_slots[i], .image = &imgs[i] };
        }
        self.vol.vt.writePages(self.vol.ctx, &ops) catch return error.Io;
    }

    /// Grow the chain by one extent: zero, header + barrier, relink + barrier.
    fn growDirectory(self: *Manager, d: *Directory) VerbError!void {
        const g = self.vol.geo();
        const ext_idx = self.vol.vt.claimInternal(self.vol.ctx) catch return error.NoSpace;
        const link: Extent = .{ .start = g.heap.start + ext_idx * g.node_size, .len = g.node_size };
        const chunks = try region.zeroPlan(self.gpa, &.{link});
        defer self.gpa.free(chunks);
        self.vol.vt.zero(self.vol.ctx, chunks) catch return error.Io;
        const last = d.links.items[d.links.items.len - 1];
        const steps = dir.growPlan(last, link);
        for (steps) |s| {
            self.vol.vt.writePages(self.vol.ctx, &.{.{ .offset = s.offset, .image = &s.image }}) catch return error.Io;
        }
        try self.control(&.{.{ .alloc = ext_idx }});
        try d.links.append(self.gpa, .{ .extent = link, .header = dir.Header.decode(&steps[0].image) catch unreachable });
        d.links.items[d.links.items.len - 2].header = dir.Header.decode(&steps[1].image) catch unreachable;
        stats.appender_directory_links_grown.inc();
    }

    // ---- the carve ----

    const Carve = struct {
        table: page.SegmentTable = .{},
        extents: std.ArrayList(u64) = .empty,

        fn deinit(self: *Carve, gpa: Allocator) void {
            self.extents.deinit(gpa);
        }
    };

    /// `ceil(bytes / node_size)` internal claims, coalesced; a fragmented
    /// heap keeps what the table can name; the rest released, never a
    /// refusal. The kept extents must sit in no appender's grant record.
    /// ZEROED before any page names them.
    fn carveRing(self: *Manager, bytes: u64, floor_bytes: u64) VerbError!Carve {
        const g = self.vol.geo();
        const want: u64 = (bytes + g.node_size - 1) / g.node_size;
        var claimed: std.ArrayList(u64) = .empty;
        defer claimed.deinit(self.gpa);
        var i: u64 = 0;
        while (i < want) : (i += 1) {
            const e = self.vol.vt.claimInternal(self.vol.ctx) catch break;
            try claimed.append(self.gpa, e);
        }
        if (claimed.items.len == 0) return error.NoSpace;
        const runs = try self.gpa.alloc(region.IndexRun, claimed.items.len);
        defer self.gpa.free(runs);
        const coalesced = region.coalesceIndices(claimed.items, runs);
        const fit = region.ringSegmentsThatFit(coalesced, (floor_bytes + g.node_size - 1) / g.node_size);
        for (fit.released) |r| {
            var e = r.start;
            while (e < r.end()) : (e += 1) self.vol.vt.releaseUnpublished(self.vol.ctx, e);
        }
        var carve: Carve = .{};
        errdefer carve.deinit(self.gpa);
        for (fit.keptSlice()) |r| {
            carve.table.push(region.runExtent(g.heap.start, g.node_size, r)) catch unreachable;
            var e = r.start;
            while (e < r.end()) : (e += 1) try carve.extents.append(self.gpa, e);
        }
        if (try self.heldElsewhere(std.math.maxInt(u32), carve.extents.items)) {
            stats.extent_grant_conflicts.fire("a ring carve landed inside another appender's extent grant");
            for (carve.extents.items) |e| self.vol.vt.releaseUnpublished(self.vol.ctx, e);
            return error.Refused;
        }
        const chunks = try region.zeroPlan(self.gpa, carve.table.slice());
        defer self.gpa.free(chunks);
        self.vol.vt.zero(self.vol.ctx, chunks) catch {
            for (carve.extents.items) |e| self.vol.vt.releaseUnpublished(self.vol.ctx, e);
            return error.Io;
        };
        return carve;
    }

    fn releaseCarve(self: *Manager, carve: *const Carve) void {
        for (carve.extents.items) |e| self.vol.vt.releaseUnpublished(self.vol.ctx, e);
    }

    /// The alloc deltas of a carve as ONE control entry (`Try`-admitted).
    fn journalCarve(self: *Manager, extents: []const u64, extra: []const ControlRecord) VerbError!void {
        var recs: std.ArrayList(ControlRecord) = .empty;
        defer recs.deinit(self.gpa);
        for (extents) |e| try recs.append(self.gpa, .{ .alloc = e });
        for (extra) |r| try recs.append(self.gpa, r);
        try self.control(recs.items);
    }

    // ---- JoinAppender (§3.1) ----

    pub fn joinAppender(self: *Manager, req: JoinAppender) VerbError!Joined {
        self.requireLease() catch return error.Refused;
        self.verbs_lock.lockUncancelable(self.io);
        defer self.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        const g = self.vol.geo();
        var d = try self.readDirectory();
        defer d.deinit(self.gpa);
        const t_admit = clock.monotonicNs();
        const stable: cr.StableIdentity = .{ .node_token = req.identity.node_token, .mount_slot = req.identity.mount_slot };
        const hint = try self.readHint(stable);

        // 2. Durable idempotency: a Live page under this identity IS the join.
        var existing: ?struct { p: page.Page, slots: [2]u64 } = null;
        for (d.links.items) |l| {
            var pair: u32 = 0;
            while (pair < l.header.pairs and existing == null) : (pair += 1) {
                const imgs = dir.readPair(self.vol.reader(), l, pair) catch return error.Io;
                const p = imgs.newest() orelse continue;
                if (p.state == .live and p.identity.sameMount(req.identity)) existing = .{ .p = p, .slots = imgs.offsets };
            }
        }
        if (existing) |ex| {
            const minted = try self.mintIncarnation();
            var out: Joined = .{ .appender_id = ex.p.appender_id, .page_addr = ex.slots[0], .term = ex.p.term, .generation = ex.p.generation, .head_hint = ex.p.head_hint, .ring_segments = ex.p.segments, .grant = undefined, .n_grant = 0, .already = true, .node_seq_base = minted.base };
            var rec = try self.readGrantRecord(ex.p.appender_id);
            defer rec.deinit(self.gpa);
            if (ex.p.grants.len == 0 and rec.runs.items.len == 0) {
                // The first join died before its grant: complete it here.
                const want = @max(grant.joinedPoolFloor(g.mint_slots), hint.grant_extents);
                const granted = try self.carveGrant(ex.p.appender_id, want, &rec);
                defer self.gpa.free(granted.runs);
                for (granted.runs) |r| if (out.n_grant < out.grant.len) {
                    out.grant[out.n_grant] = r;
                    out.n_grant += 1;
                };
            } else {
                for (ex.p.grants.slice()) |e| if (rec.contains(e.start) and out.n_grant < out.grant.len) {
                    out.grant[out.n_grant] = .{ .start = e.start, .len = @intCast(e.len) };
                    out.n_grant += 1;
                };
            }
            stats.manager_verb_replays.inc();
            self.ledger.record(clock.monotonicNs(), t_admit - t0, clock.monotonicNs() - t_admit, 0);
            return out;
        }

        // 3. Capacity: the remainder law (one law — spec Q-10).
        const budget = region.ringBudgetBytes(g.heap.len);
        const remaining = budget -| d.rings_in_use;
        const ask: u64 = if (req.ring_want_bytes == 0) region.ringBytesHinted(region.RING_FLOOR_BYTES, hint.ring_bytes, g.volume_len) else region.ringBytesHinted(req.ring_want_bytes, 0, g.volume_len);
        const ring_bytes = region.joinRingUnderBudget(ask, remaining, g.node_size) orelse {
            stats.manager_capacity_refusals.inc();
            log.warn("manager: join refused — {d} appenders are live and the ring budget admits {d} (appenders_capacity = heap/16 ÷ ring floor); the lever is the metadata volume COUNT, never a format-time client count", .{ d.live, region.appendersCapacity(budget) });
            return error.Capacity;
        };

        // 4. The page: the lowest Free pair, else grow the chain.
        var free_pair = dir.lowestFreePair(self.vol.reader(), d.links.items) catch |e| return switch (e) {
            error.Io => error.Io,
            error.ForeignLayout => error.Corrupt,
        };
        if (free_pair == null) {
            try self.growDirectory(&d);
            free_pair = dir.lowestFreePair(self.vol.reader(), d.links.items) catch return error.Corrupt;
        }
        const pair = free_pair.?;
        const appender_id = pair.appender_id;
        const prior = pair.newest();

        // 5. Settle the predecessor incarnation's pending GrowRing word.
        try self.settlePending(stable, hint, prior);

        // 7. Carve + zero + the alloc deltas.
        var carve = try self.carveRing(ring_bytes, region.RING_FLOOR_BYTES);
        defer carve.deinit(self.gpa);
        errdefer self.releaseCarve(&carve);
        try self.journalCarve(carve.extents.items, &.{});

        // 8. The node-seq incarnation, durable before the page goes Live.
        const minted = try self.mintIncarnation();

        // 9. The page Live into BOTH directory slots, one barrier.
        var p: page.Page = if (prior) |pp| pp else .{ .appender_id = appender_id, .generation = 0 };
        p.appender_id = appender_id;
        p.term += 1;
        p.state = .live;
        p.recovered_by_term = 0;
        p.identity = req.identity;
        p.is_manager = false;
        p.home_volume = req.home_volume;
        p.segments = carve.table;
        const cont = region.seqContinuation(p.head_hint, self.vol.vt.ring0Head(self.vol.ctx));
        p.head_hint = cont;
        p.ledger_tail_seq = cont;
        p.ckpt_seq = 0;
        p.grants = .{};
        p.n_slots = 0;
        try self.writeBothDirSlots(&p, pair.offsets);
        stats.appender_joins.inc();
        d.live += 1;
        self.appenders_known = @max(d.live, 1);

        // 10. A Free page's residue returned; the initial grant.
        var rec = try self.readGrantRecord(appender_id);
        defer rec.deinit(self.gpa);
        if (rec.runs.items.len != 0) {
            try self.returnWhole(appender_id, &rec);
            stats.appender_join_residue_returned.inc();
        }
        const want = @max(grant.joinedPoolFloor(g.mint_slots), hint.grant_extents);
        const granted = try self.carveGrant(appender_id, want, &rec);
        defer self.gpa.free(granted.runs);
        var out: Joined = .{ .appender_id = appender_id, .page_addr = pair.offsets[0], .term = p.term, .generation = p.generation, .head_hint = p.head_hint, .ring_segments = carve.table, .grant = undefined, .n_grant = 0, .already = false, .node_seq_base = minted.base };
        for (granted.runs) |r| if (out.n_grant < out.grant.len) {
            out.grant[out.n_grant] = r;
            out.n_grant += 1;
        };
        const t_done = clock.monotonicNs();
        self.ledger.record(t_done, t_admit - t0, t_done - t_admit, 0);
        return out;
    }

    /// The counter read → `minted + 1` written as ONE control entry →
    /// BARRIERED before the base is answered: a manager dying after the
    /// reply never re-mints the ordinal; dying before it burns one.
    fn mintIncarnation(self: *Manager) VerbError!region.Minted {
        const so_far = try self.readIncarnations();
        const m = region.mintIncarnation(self.vol.geo().volume_base, so_far) catch {
            log.err("manager: the volume's node-seq incarnations are exhausted (2^25 joins) — refusing the join", .{});
            return error.Refused;
        };
        const v = cr.encodeNodeSeqIncarnations(m.minted);
        try self.control(&.{.{ .put = .{ .key = cr.nodeSeqIncarnationsKey().slice(), .value = &v } }});
        return m;
    }

    /// A GrowRing segment the predecessor incarnation carved: one its last
    /// page names went with the ring; one no page names is returned.
    fn settlePending(self: *Manager, stable: cr.StableIdentity, hint: cr.AppenderHint, prior: ?page.Page) VerbError!void {
        const p = hint.pending orelse return;
        const g = self.vol.geo();
        var named = false;
        if (prior) |pp| for (pp.segments.slice()) |s| if (s.start == p.start and s.len == p.len) {
            named = true;
        };
        var cleared = hint;
        cleared.pending = null;
        const hv = cleared.encode();
        if (named) {
            try self.control(&.{.{ .put = .{ .key = cr.appenderHintKey(stable).slice(), .value = &hv } }});
            return;
        }
        var recs: std.ArrayList(ControlRecord) = .empty;
        defer recs.deinit(self.gpa);
        var e = (p.start - g.heap.start) / g.node_size;
        const end = e + p.len / g.node_size;
        while (e < end) : (e += 1) try recs.append(self.gpa, .{ .free = e });
        try recs.append(self.gpa, .{ .put = .{ .key = cr.appenderHintKey(stable).slice(), .value = &hv } });
        try self.control(recs.items);
        stats.appender_pending_segments_returned.inc();
    }

    // ---- LeaveAppender (§3.2) ----

    pub fn leaveAppender(self: *Manager, req: LeaveAppender) VerbError!Left {
        self.requireLease() catch return error.Refused;
        self.verbs_lock.lockUncancelable(self.io);
        defer self.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        if (req.appender_id == journal.FIXED_RING_APPENDER) return error.Rejected;
        if (!req.in_process and self.vol.vt.ownsRegion(self.vol.ctx, req.appender_id)) return error.Rejected;
        const slots = (try self.dirSlotsOf(req.appender_id)) orelse return error.Rejected;
        const p_opt = try self.readFourSlots(req.appender_id, slots);
        const t_admit = clock.monotonicNs();
        var p = p_opt orelse return error.Rejected;
        if (p.state == .free) {
            stats.manager_verb_replays.inc();
            self.ledger.record(clock.monotonicNs(), t_admit - t0, 0, 0);
            return .{ .already = true };
        }
        if (p.state != .live) return error.Rejected;
        if (!p.identity.sameMount(req.identity)) return error.Rejected;
        // Busy while the page names a slot or tree 0 leases it anything:
        // release them first (flush-then-transfer to nobody).
        if (p.n_slots != 0 or try self.leasesAnything(req.appender_id)) return error.Deferred;
        // The unclaimed runs through the screened return law.
        var rec = try self.readGrantRecord(req.appender_id);
        defer rec.deinit(self.gpa);
        if (req.unclaimed.len != 0) {
            const copy = try self.gpa.dupe(GrantRun, req.unclaimed);
            defer self.gpa.free(copy);
            _ = try self.returnInner(req.appender_id, copy, &rec);
        }
        // The pending GrowRing word settles with the page.
        const stable: cr.StableIdentity = .{ .node_token = p.identity.node_token, .mount_slot = p.identity.mount_slot };
        const hint = try self.readHint(stable);
        try self.settlePending(stable, hint, p);
        // The page Free into BOTH directory slots keeping id, term and the
        // head hint (the seq-space watermark the next carve continues from).
        const ring = p.segments;
        p.state = .free;
        p.identity = .none;
        p.ledger_tail_seq = p.head_hint;
        p.segments = .{};
        p.grants = .{};
        p.n_slots = 0;
        try self.writeBothDirSlots(&p, slots);
        // Only THEN the ring extents' bits clear (no durable page names
        // them any more, and no journal record ever did) — pages-barrier-
        // release-bitmap: a Live page naming freed extents across a barrier
        // is the double-custodian window; a crash between the two leaves
        // the extents claimed (C13's orphan class, never a loss).
        const g = self.vol.geo();
        var recs: std.ArrayList(ControlRecord) = .empty;
        defer recs.deinit(self.gpa);
        for (ring.slice()) |s| {
            var e = (s.start - g.heap.start) / g.node_size;
            const end = e + s.len / g.node_size;
            while (e < end) : (e += 1) {
                self.vol.vt.releaseUnpublished(self.vol.ctx, e);
                try recs.append(self.gpa, .{ .free = e });
            }
        }
        if (recs.items.len != 0) try self.control(recs.items);
        stats.appender_leaves.inc();
        self.appenders_known = @max(self.appenders_known -| 1, 1);
        const t_done = clock.monotonicNs();
        self.ledger.record(t_done, t_admit - t0, t_done - t_admit, 0);
        return .{ .already = false };
    }

    fn leasesAnything(self: *Manager, appender_id: u32) VerbError!bool {
        const r = cr.rangeOf(.slot_state);
        var kvs: std.ArrayList(KeyValue) = .empty;
        defer freeKvs(self.gpa, &kvs);
        try self.vol.vt.range(self.vol.ctx, self.gpa, r.start.slice(), r.end.slice(), &kvs);
        for (kvs.items) |kv| {
            const s = cr.SlotStateWire.decode(kv.value) catch return error.Corrupt;
            if (s.variant != .unleased and s.appender_id == appender_id) return true;
        }
        return false;
    }

    // ---- ExtentGrant / ReturnExtents (§5.3) ----

    /// Screen → remainder → replay or carve (TOPS UP to `want`, never an
    /// increment) → cut to the run cap → refuse if held elsewhere → ONE
    /// control entry per chunk with the merged record → the reply.
    pub fn extentGrant(self: *Manager, req: ExtentGrant) VerbError!Granted {
        self.requireLease() catch return error.Refused;
        self.verbs_lock.lockUncancelable(self.io);
        defer self.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        if (req.appender_id == journal.FIXED_RING_APPENDER) {
            // The manager claims from the bitmap it owns.
            return error.Rejected;
        }
        const cap = grant.grantWireCap(self.vol.vt.freeExtents(self.vol.ctx), self.appenders_known);
        const want = try grant.clampGrantWant(req.want, cap);
        var rec = try self.readGrantRecord(req.appender_id);
        defer rec.deinit(self.gpa);
        const t_admit = clock.monotonicNs();
        // The page word ∩ the record = the remainder.
        var remainder: u64 = 0;
        var stale: u64 = 0;
        for (req.page_word) |r| {
            var e = r.start;
            while (e < r.end()) : (e += 1) {
                if (rec.contains(e)) remainder += 1 else stale += 1;
            }
        }
        if (stale != 0) stats.extent_grant_stale_page_words.add(stale);
        if (remainder >= want and want > 0) {
            stats.manager_verb_replays.inc();
            var out: std.ArrayList(GrantRun) = .empty;
            errdefer out.deinit(self.gpa);
            const copy = try self.gpa.dupe(GrantRun, req.page_word);
            defer self.gpa.free(copy);
            try grant.intersectWithRecord(self.gpa, grant.coalesceRuns(copy), rec.runs.items, &out);
            self.ledger.record(clock.monotonicNs(), t_admit - t0, clock.monotonicNs() - t_admit, 0);
            return .{ .runs = try out.toOwnedSlice(self.gpa), .replayed = true };
        }
        const granted = try self.carveGrant(req.appender_id, want - remainder, &rec);
        const t_done = clock.monotonicNs();
        self.ledger.record(t_done, t_admit - t0, t_done - t_admit, 0);
        return granted;
    }

    /// Carve `want` extents in the internal class (partial on `NoSpace`;
    /// nothing ⇒ `NoSpace` — the SPACE class), cut to the record's run cap,
    /// refused if held elsewhere, ONE control entry with the alloc deltas
    /// and the merged record. The caller owns the returned runs.
    fn carveGrant(self: *Manager, appender_id: u32, want: u64, rec: *GrantRecord) VerbError!Granted {
        var claimed: std.ArrayList(u64) = .empty;
        defer claimed.deinit(self.gpa);
        var i: u64 = 0;
        while (i < want) : (i += 1) {
            const e = self.vol.vt.claimInternal(self.vol.ctx) catch break;
            try claimed.append(self.gpa, e);
        }
        if (claimed.items.len == 0) return error.NoSpace;
        errdefer for (claimed.items) |e| self.vol.vt.releaseUnpublished(self.vol.ctx, e);
        if (try self.heldElsewhere(appender_id, claimed.items)) {
            stats.extent_grant_conflicts.fire("a grant carve landed inside another appender's extent grant");
            return error.Refused;
        }
        // Merge into the record; cut fragments past the run cap.
        var merged: std.ArrayList(GrantRun) = .empty;
        defer merged.deinit(self.gpa);
        try merged.appendSlice(self.gpa, rec.runs.items);
        std.mem.sort(u64, claimed.items, {}, std.sort.asc(u64));
        for (claimed.items) |e| try merged.append(self.gpa, .{ .start = e, .len = 1 });
        const normalized = grant.coalesceRuns(merged.items);
        const cap = self.runCap();
        var kept: std.ArrayList(GrantRun) = .empty;
        defer kept.deinit(self.gpa);
        var dropped: std.ArrayList(u64) = .empty;
        defer dropped.deinit(self.gpa);
        for (normalized, 0..) |r, idx| {
            if (idx < cap) try kept.append(self.gpa, r) else {
                var e = r.start;
                while (e < r.end()) : (e += 1) try dropped.append(self.gpa, e);
            }
        }
        var granted: std.ArrayList(GrantRun) = .empty;
        errdefer granted.deinit(self.gpa);
        var final_claimed: std.ArrayList(u64) = .empty;
        defer final_claimed.deinit(self.gpa);
        for (claimed.items) |e| {
            var cut = false;
            for (dropped.items) |d| if (d == e) {
                cut = true;
            };
            if (cut) self.vol.vt.releaseUnpublished(self.vol.ctx, e) else try final_claimed.append(self.gpa, e);
        }
        if (final_claimed.items.len == 0) return error.NoSpace;
        // The record as it stands after the carve.
        const buf = try self.gpa.alloc(u8, cr.extentGrantEncodedLen(kept.items.len));
        defer self.gpa.free(buf);
        const enc = cr.encodeExtentGrant(kept.items, buf) catch return error.Corrupt;
        try self.journalCarve(final_claimed.items, &.{.{ .put = .{ .key = cr.extentGrantKey(appender_id).slice(), .value = enc } }});
        rec.runs.clearRetainingCapacity();
        try rec.runs.appendSlice(self.gpa, kept.items);
        for (final_claimed.items) |e| {
            if (granted.items.len > 0 and granted.items[granted.items.len - 1].end() == e) granted.items[granted.items.len - 1].len += 1 else try granted.append(self.gpa, .{ .start = e, .len = 1 });
        }
        stats.extent_grants.inc();
        stats.extent_grant_extents.add(final_claimed.items.len);
        return .{ .runs = try granted.toOwnedSlice(self.gpa), .replayed = false };
    }

    pub fn returnExtents(self: *Manager, req: ReturnExtents) VerbError!Returned {
        self.requireLease() catch return error.Refused;
        self.verbs_lock.lockUncancelable(self.io);
        defer self.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        const g = self.vol.geo();
        try grant.validateReturnRuns(req.runs, g.total_extents);
        var rec = try self.readGrantRecord(req.appender_id);
        defer rec.deinit(self.gpa);
        const t_admit = clock.monotonicNs();
        const out = try self.returnInner(req.appender_id, req.runs, &rec);
        const t_done = clock.monotonicNs();
        self.ledger.record(t_done, t_admit - t0, t_done - t_admit, 0);
        return out;
    }

    /// `coalesce` (bounded by the frame) → `∩ record` (bounded by the
    /// record) → the own-region RAM screen → the run-cap screen → ONE
    /// control entry: `free(extent, retire 0)` + the record WITHOUT them.
    /// `cleared + already ≡ named`.
    fn returnInner(self: *Manager, appender_id: u32, runs: []GrantRun, rec: *GrantRecord) VerbError!Returned {
        const coalesced = grant.coalesceRuns(runs);
        var named: u64 = 0;
        for (coalesced) |r| named += r.len;
        var hit: std.ArrayList(GrantRun) = .empty;
        defer hit.deinit(self.gpa);
        try grant.intersectWithRecord(self.gpa, coalesced, rec.runs.items, &hit);
        var cleared: u64 = 0;
        for (hit.items) |r| cleared += r.len;
        if (cleared == 0) {
            stats.manager_verb_replays.inc();
            return .{ .cleared = 0, .already = named };
        }
        // An in-process region's RAM grant must not disagree with the bitmap.
        if (self.vol.vt.ownsRegion(self.vol.ctx, appender_id)) {
            for (hit.items) |r| {
                var e = r.start;
                while (e < r.end()) : (e += 1) if (self.vol.vt.regionGrantHeld(self.vol.ctx, appender_id, e)) {
                    stats.manager_verb_refusals.fire("a return named an extent the region's RAM grant holds claimed or pending");
                    return error.Refused;
                };
            }
        }
        // The record without the hits.
        var rest: std.ArrayList(GrantRun) = .empty;
        defer rest.deinit(self.gpa);
        for (rec.runs.items) |r| {
            var cur: ?GrantRun = null;
            var e = r.start;
            while (e < r.end()) : (e += 1) {
                var returned = false;
                for (hit.items) |h| if (h.contains(e)) {
                    returned = true;
                };
                if (returned) {
                    if (cur) |c| try rest.append(self.gpa, c);
                    cur = null;
                } else if (cur) |*c| {
                    c.len += 1;
                } else cur = .{ .start = e, .len = 1 };
            }
            if (cur) |c| try rest.append(self.gpa, c);
        }
        if (rest.items.len > self.runCap()) {
            stats.extent_return_run_cap_refusals.inc();
            return error.Refused;
        }
        var recs: std.ArrayList(ControlRecord) = .empty;
        defer recs.deinit(self.gpa);
        for (hit.items) |r| {
            var e = r.start;
            while (e < r.end()) : (e += 1) try recs.append(self.gpa, .{ .free = e });
        }
        const buf = try self.gpa.alloc(u8, cr.extentGrantEncodedLen(rest.items.len));
        defer self.gpa.free(buf);
        const k = cr.extentGrantKey(appender_id);
        if (rest.items.len == 0) {
            try recs.append(self.gpa, .{ .delete = k.slice() });
        } else {
            const enc = cr.encodeExtentGrant(rest.items, buf) catch return error.Corrupt;
            try recs.append(self.gpa, .{ .put = .{ .key = k.slice(), .value = enc } });
        }
        try self.control(recs.items);
        rec.runs.clearRetainingCapacity();
        try rec.runs.appendSlice(self.gpa, rest.items);
        stats.extent_returns.inc();
        stats.extent_grant_returned.add(cleared);
        return .{ .cleared = cleared, .already = named - cleared };
    }

    fn returnWhole(self: *Manager, appender_id: u32, rec: *GrantRecord) VerbError!void {
        const copy = try self.gpa.dupe(GrantRun, rec.runs.items);
        defer self.gpa.free(copy);
        _ = try self.returnInner(appender_id, copy, rec);
    }

    // ---- GrowRing (§3.3) ----

    pub fn growRing(self: *Manager, req: GrowRing) VerbError!RingGrown {
        self.requireLease() catch return error.Refused;
        self.verbs_lock.lockUncancelable(self.io);
        defer self.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        if (req.appender_id == journal.FIXED_RING_APPENDER or self.vol.vt.ownsRegion(self.vol.ctx, req.appender_id)) return error.Rejected;
        const g = self.vol.geo();
        const slots = (try self.dirSlotsOf(req.appender_id)) orelse return error.Rejected;
        const p = (try self.readFourSlots(req.appender_id, slots)) orelse return error.Rejected;
        if (p.state != .live) return error.Rejected;
        const t_admit = clock.monotonicNs();
        const stable: cr.StableIdentity = .{ .node_token = p.identity.node_token, .mount_slot = p.identity.mount_slot };
        const hint = try self.readHint(stable);
        // 2. The durable witness FIRST: a pending segment this incarnation's
        // page does not name is the lost reply, answered VERBATIM.
        if (hint.pending) |pend| {
            var named = false;
            for (p.segments.slice()) |s| if (s.start == pend.start and s.len == pend.len) {
                named = true;
            };
            if (pend.appender_id == req.appender_id and pend.term == p.term and !named) {
                stats.manager_verb_replays.inc();
                return .{ .segment = .{ .start = pend.start, .len = pend.len } };
            }
            try self.settlePending(stable, hint, p);
        }
        if (p.segments.len >= page.RING_SEGMENTS_MAX) return .{ .segment = null };
        // 4. The room.
        var d = try self.readDirectory();
        defer d.deinit(self.gpa);
        const remaining = region.ringBudgetBytes(g.heap.len) -| d.rings_in_use;
        const room = region.growRoomBytes(region.ringCeiling(g.volume_len), p.ringBytes(), remaining);
        const want_extents = @min(req.want_bytes, room) / g.node_size;
        if (want_extents == 0) return .{ .segment = null };
        // 5. Carve; the LONGEST adjacent run; the segment floor.
        var claimed: std.ArrayList(u64) = .empty;
        defer claimed.deinit(self.gpa);
        var i: u64 = 0;
        while (i < want_extents) : (i += 1) {
            const e = self.vol.vt.claimInternal(self.vol.ctx) catch break;
            try claimed.append(self.gpa, e);
        }
        if (claimed.items.len == 0) return error.NoSpace;
        const runs = try self.gpa.alloc(region.IndexRun, claimed.items.len);
        defer self.gpa.free(runs);
        const coalesced = region.coalesceIndices(claimed.items, runs);
        var best = coalesced[0];
        for (coalesced) |r| if (r.len > best.len) {
            best = r;
        };
        for (coalesced) |r| if (r.start != best.start) {
            var e = r.start;
            while (e < r.end()) : (e += 1) self.vol.vt.releaseUnpublished(self.vol.ctx, e);
        };
        if (best.len < region.growSegmentFloorExtents(want_extents)) {
            var e = best.start;
            while (e < best.end()) : (e += 1) self.vol.vt.releaseUnpublished(self.vol.ctx, e);
            stats.appender_grow_ring_short_declines.inc();
            return .{ .segment = null };
        }
        var kept: std.ArrayList(u64) = .empty;
        defer kept.deinit(self.gpa);
        var e = best.start;
        while (e < best.end()) : (e += 1) try kept.append(self.gpa, e);
        errdefer for (kept.items) |x| self.vol.vt.releaseUnpublished(self.vol.ctx, x);
        if (try self.heldElsewhere(std.math.maxInt(u32), kept.items)) {
            stats.extent_grant_conflicts.fire("a ring growth landed inside an appender's extent grant");
            return error.Refused;
        }
        const seg = region.runExtent(g.heap.start, g.node_size, best);
        const chunks = try region.zeroPlan(self.gpa, &.{seg});
        defer self.gpa.free(chunks);
        self.vol.vt.zero(self.vol.ctx, chunks) catch return error.Io;
        // 6. The alloc deltas + the hint put with the pending witness.
        var h = hint;
        h.ring_bytes = @max(p.ringBytes() + seg.len, hint.ring_bytes);
        h.pending = .{ .appender_id = req.appender_id, .term = p.term, .start = seg.start, .len = seg.len };
        const hv = h.encode();
        try self.journalCarve(kept.items, &.{.{ .put = .{ .key = cr.appenderHintKey(stable).slice(), .value = &hv } }});
        stats.appender_ring_grows.inc();
        const t_done = clock.monotonicNs();
        self.ledger.record(t_done, t_admit - t0, t_done - t_admit, 0);
        return .{ .segment = seg };
    }

    // ---- PublishRoots (§4.3; the screens of §4.5 minus the seq-floor clauses) ----

    pub fn publishRoots(self: *Manager, req: PublishRoots) VerbError!RootsPublished {
        self.requireLease() catch return error.Refused;
        self.verbs_lock.lockUncancelable(self.io);
        defer self.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        if (req.appender_id == journal.FIXED_RING_APPENDER) return error.Rejected;
        const g = self.vol.geo();
        var rec = try self.readGrantRecord(req.appender_id);
        defer rec.deinit(self.gpa);
        const t_admit = clock.monotonicNs();
        var published: u64 = 0;
        var already: u64 = 0;
        for (req.roots) |w| {
            const cur = (try self.readSlotState(w.slot)) orelse return error.Rejected;
            if (cur.variant == .unleased or cur.appender_id != req.appender_id or cur.g != w.g) return error.Rejected;
            try screenRootWord(w, cur, &rec, g);
            if (cur.root.addr == w.root.addr and cur.root.seq == w.root.seq and cur.cursor == w.cursor and cur.slot_tree_extents == w.slot_tree_extents) {
                already += 1;
                continue;
            }
            var next = cur;
            next.root = w.root;
            next.cursor = @max(cur.cursor, w.cursor);
            next.slot_tree_extents = w.slot_tree_extents;
            const v = next.encode();
            try self.control(&.{.{ .put = .{ .key = cr.slotStateKey(w.slot).slice(), .value = &v } }});
            stats.meta_kv_forest_root_publishes.inc();
            published += 1;
        }
        if (published == 0 and already != 0) stats.manager_verb_replays.inc();
        const t_done = clock.monotonicNs();
        self.ledger.record(t_done, t_admit - t0, t_done - t_admit, 0);
        return .{ .published = published, .already = already };
    }

    /// The root word's screen: the cursor ≥ the recorded one and below the
    /// guest-namespace base; `slot_tree_extents` ≤ the volume; the root =
    /// the recorded root verbatim OR a node-aligned heap address whose
    /// extent the appender's grant record holds.
    fn screenRootWord(w: SlotRootWord, cur: cr.SlotState, rec: *const GrantRecord, g: Geometry) error{Rejected}!void {
        if (w.cursor < cur.cursor or w.cursor > key.GUEST_NS_BASE) return error.Rejected;
        if (w.slot_tree_extents > g.total_extents) return error.Rejected;
        if (w.root.addr == cur.root.addr and w.root.seq == cur.root.seq) return;
        if (w.root.addr < g.heap.start or w.root.addr >= g.heap.end() or (w.root.addr - g.heap.start) % g.node_size != 0) return error.Rejected;
        if (!rec.contains((w.root.addr - g.heap.start) / g.node_size)) return error.Rejected;
    }

    // ---- PublishEndpoint / ResolveEndpoint ----

    pub fn publishEndpoint(self: *Manager, req: PublishEndpoint) VerbError!Published {
        self.requireLease() catch return error.Refused;
        self.verbs_lock.lockUncancelable(self.io);
        defer self.verbs_lock.unlock(self.io);
        const t0 = clock.monotonicNs();
        if (req.endpoint.len == 0 or req.endpoint.len > req.endpoint.bytes.len) return error.Rejected;
        // Strict identity ↔ page binding.
        const p = (try self.readPageOf(req.appender_id)) orelse return error.Rejected;
        if (p.state != .live or !p.identity.sameMount(req.identity)) return error.Rejected;
        const t_admit = clock.monotonicNs();
        for (self.endpoints.items) |*row| if (row.appender_id == req.appender_id) {
            if (row.endpoint.eql(req.endpoint) and row.pr_key == req.pr_key) {
                stats.manager_verb_replays.inc();
                return .{ .already = true };
            }
            row.endpoint = req.endpoint;
            row.pr_key = req.pr_key;
            self.ledger.record(clock.monotonicNs(), t_admit - t0, clock.monotonicNs() - t_admit, 0);
            return .{ .already = false };
        };
        try self.endpoints.append(self.gpa, .{ .appender_id = req.appender_id, .endpoint = req.endpoint, .pr_key = req.pr_key });
        const t_done = clock.monotonicNs();
        self.ledger.record(t_done, t_admit - t0, t_done - t_admit, 0);
        return .{ .already = false };
    }

    pub fn resolveEndpoint(self: *Manager, req: ResolveEndpoint) VerbError!EndpointReply {
        self.requireLease() catch return error.Refused;
        self.verbs_lock.lockUncancelable(self.io);
        defer self.verbs_lock.unlock(self.io);
        for (self.endpoints.items) |row| if (row.appender_id == req.appender_id) return .{ .endpoint = row.endpoint };
        return .{ .endpoint = null };
    }

    /// Drop a left appender's endpoint row.
    pub fn forgetEndpoint(self: *Manager, appender_id: u32) void {
        for (self.endpoints.items, 0..) |row, i| if (row.appender_id == appender_id) {
            _ = self.endpoints.swapRemove(i);
            return;
        };
    }

    // ---- the structural door (§5.4) ----

    pub const Door = union(enum) {
        cleared,
        /// The tail MOVED but the window is not yet clear at the bound:
        /// `Deferred` (nothing written).
        deferred: struct { cycles: u32, tail_start: u64, tail: u64 },
    };

    /// A grant of UNLEASED slots to any appender but the manager: cycle the
    /// checkpoint under the SMO hold until ring 0's window is CLEAR of every
    /// named slot's records, bounded by `COVER_CYCLES_MAX`. At the bound a
    /// tail that moved answers `deferred`; one that did not move at all is
    /// `Corrupt` (the stuck-reservation class — a defect, never a longer
    /// wait). The caller holds `smo` BEFORE `manager_verbs`.
    pub fn structuralDoor(self: *Manager, slots: []const u16, tail_at_entry: u64) error{ Io, Corrupt }!Door {
        if (self.vol.vt.ring0WindowClearOf(self.vol.ctx, slots)) return .cleared;
        var tail = tail_at_entry;
        var cycles: u32 = 0;
        while (cycles < checkpoint.COVER_CYCLES_MAX) : (cycles += 1) {
            tail = try self.vol.vt.cycleCheckpoint(self.vol.ctx);
            stats.slot_grant_door_cycles.inc();
            if (self.vol.vt.ring0WindowClearOf(self.vol.ctx, slots)) return .cleared;
        }
        if (tail == tail_at_entry) {
            log.err("manager: the structural door cycled {d} times and ring 0's tail never moved from {d} — a stuck reservation pins the window (a defect, never a longer wait)", .{ cycles, tail });
            return error.Corrupt;
        }
        stats.slot_grant_deferrals.inc();
        return .{ .deferred = .{ .cycles = cycles, .tail_start = tail_at_entry, .tail = tail } };
    }
};

// ---------------------------------------------------------------------------
// The handler table the wire binds: `fn(verb, payload) → reply`.
// ---------------------------------------------------------------------------

/// A reply as bytes under a status (the wire frames it; the in-process
/// hub hands it over verbatim).
pub const Reply = struct {
    status: Status,
    reason: Reason = .none,
    /// Owned by the caller's allocator (empty on a non-ok status).
    payload: []u8 = &.{},
};

pub const Handler = *const fn (mgr: *Manager, gpa: Allocator, payload: []const u8) Reply;

/// One slot per verb of the vocabulary; `null` = unserved in this binary.
pub const HandlerTable = struct {
    slots: std.EnumArray(Verb, ?Handler) = .initFill(null),

    pub fn bind(self: *HandlerTable, verb: Verb, h: Handler) void {
        self.slots.set(verb, h);
    }

    pub fn served(self: *const HandlerTable, verb: Verb) bool {
        return self.slots.get(verb) != null;
    }

    /// Dispatch: an unserved verb is `rejected{verb_unserved}` (the wire
    /// never guesses at a grant-bearing frame); a manager without the lease
    /// is `not_manager`.
    pub fn dispatch(self: *const HandlerTable, mgr: *Manager, gpa: Allocator, verb: Verb, payload: []const u8) Reply {
        if (mgr.lease != .held) return .{ .status = .not_manager };
        const h = self.slots.get(verb) orelse {
            stats.manager_verb_rejected.inc();
            return .{ .status = .rejected, .reason = .verb_unserved };
        };
        return h(mgr, gpa, payload);
    }

    /// The verbs this wave serves, each one handler over the verb's
    /// `extern struct` body (the wire agent's codecs frame them).
    pub fn servedVerbs() []const Verb {
        return &.{ .join_appender, .leave_appender, .extent_grant, .return_extents, .grow_ring, .publish_roots, .publish_endpoint, .resolve_endpoint };
    }
};

// ---------------------------------------------------------------------------
// Tests: a RAM volume behind the seam.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// The in-RAM volume: tree 0 as a sorted key list, the device as a byte
/// image over the heap + the fixed journal, a bitmap as a bitset, ring 0's
/// head as a counter the window-clear seam consults.
pub const TestVolume = struct {
    gpa: Allocator,
    g: Geometry,
    image: []u8,
    image_base: u64,
    tree0: std.ArrayList(KeyValue) = .empty,
    bits: []bool,
    ring0_head: u64 = 1000,
    control_entries: u64 = 0,
    barriers: u64 = 0,
    zeroed_bytes: u64 = 0,
    /// Seams: refuse the next N control entries with a full window; the
    /// slots ring 0's window still names; the cycles until clear; the
    /// own regions and their RAM-held extents.
    window_full_next: u32 = 0,
    window_names: std.ArrayList(u16) = .empty,
    cycles_until_clear: u32 = 0,
    tail_moves: bool = true,
    cycles_run: u32 = 0,
    own_regions: std.ArrayList(u32) = .empty,
    ram_held: std.ArrayList(u64) = .empty,
    kicks: u32 = 0,

    pub fn init(gpa: Allocator, volume_len: u64, node_size: u64, total_extents: u64) !*TestVolume {
        const self = try gpa.create(TestVolume);
        errdefer gpa.destroy(self);
        const journal_start: u64 = journal.JOURNAL_START;
        const journal_len: u64 = 1 << 20;
        const heap_start: u64 = 2 << 20;
        self.* = .{
            .gpa = gpa,
            .g = .{ .node_size = node_size, .volume_len = volume_len, .heap = .{ .start = heap_start, .len = total_extents * node_size }, .total_extents = total_extents, .journal = .{ .start = journal_start, .len = journal_len }, .appender_dir = .{ .start = heap_start, .len = node_size }, .volume_base = 0x1000, .grain = 4096, .mint_slots = 4 },
            .image = undefined,
            .image_base = journal_start,
            .bits = undefined,
        };
        self.image = try gpa.alloc(u8, @intCast(heap_start + total_extents * node_size - journal_start));
        @memset(self.image, 0);
        self.bits = try gpa.alloc(bool, total_extents);
        @memset(self.bits, false);
        // The format's acts: the directory header at extent 0 and
        // appender 0's Free page at the fixed ring's slot 1.
        self.bits[0] = true;
        dir.formatHeader(node_size, self.at(heap_start)[0..page.PAGE_LEN]);
        var p0 = page.Page.free(0, 1, page.SegmentTable.one(.{ .start = journal_start, .len = journal_len }));
        try p0.encode(self.at(journal_start + page.PAGE_LEN)[0..page.PAGE_LEN]);
        return self;
    }

    pub fn deinit(self: *TestVolume) void {
        for (self.tree0.items) |kv| {
            self.gpa.free(kv.key);
            self.gpa.free(kv.value);
        }
        self.tree0.deinit(self.gpa);
        self.window_names.deinit(self.gpa);
        self.own_regions.deinit(self.gpa);
        self.ram_held.deinit(self.gpa);
        self.gpa.free(self.image);
        self.gpa.free(self.bits);
        self.gpa.destroy(self);
    }

    fn at(self: *TestVolume, offset: u64) []u8 {
        return self.image[@intCast(offset - self.image_base)..];
    }

    pub fn volume(self: *TestVolume) Volume {
        return .{ .ctx = self, .vt = &vtable };
    }

    pub fn allocatedExtents(self: *const TestVolume) u64 {
        var n: u64 = 0;
        for (self.bits) |b| n += @intFromBool(b);
        return n;
    }

    pub fn get(self: *TestVolume, k: []const u8) ?[]const u8 {
        for (self.tree0.items) |kv| if (std.mem.eql(u8, kv.key, k)) return kv.value;
        return null;
    }

    /// The manager's own page made Live (what `bind.zig`'s join does).
    pub fn liveManagerPage(self: *TestVolume) !void {
        var p = page.Page.free(0, 1, page.SegmentTable.one(.{ .start = self.g.journal.start, .len = self.g.journal.len }));
        p.state = .live;
        p.is_manager = true;
        p.term = 1;
        p.identity = .{ .node_token = 0xAA, .mount_slot = 0, .writer_id = 0xBB };
        p.generation = 2;
        try p.encode(self.at(self.g.journal.start + 2 * page.PAGE_LEN)[0..page.PAGE_LEN]);
    }

    const vtable: Volume.VTable = .{
        .geometry = geometryFn,
        .lookup = lookupFn,
        .range = rangeFn,
        .controlEntry = controlFn,
        .readPage = readPageFn,
        .writePages = writePagesFn,
        .zero = zeroFn,
        .claimInternal = claimFn,
        .releaseUnpublished = releaseFn,
        .freeExtents = freeExtentsFn,
        .ring0Head = ring0HeadFn,
        .ownsRegion = ownsRegionFn,
        .regionGrantHeld = regionGrantHeldFn,
        .ring0WindowClearOf = windowClearFn,
        .cycleCheckpoint = cycleFn,
        .kickCheckpoint = kickFn,
    };

    fn self_(ctx: *anyopaque) *TestVolume {
        return @ptrCast(@alignCast(ctx));
    }

    fn geometryFn(ctx: *anyopaque) Geometry {
        return self_(ctx).g;
    }

    fn lookupFn(ctx: *anyopaque, gpa: Allocator, k: []const u8) error{ Io, OutOfMemory }!?[]u8 {
        const v = self_(ctx).get(k) orelse return null;
        return try gpa.dupe(u8, v);
    }

    fn rangeFn(ctx: *anyopaque, gpa: Allocator, start: []const u8, end: []const u8, out: *std.ArrayList(KeyValue)) error{ Io, OutOfMemory }!void {
        for (self_(ctx).tree0.items) |kv| {
            if (key.lessThan(kv.key, start) or key.lessThan(end, kv.key)) continue;
            try out.append(gpa, .{ .key = try gpa.dupe(u8, kv.key), .value = try gpa.dupe(u8, kv.value) });
        }
    }

    fn put(self: *TestVolume, k: []const u8, v: []const u8) !void {
        for (self.tree0.items) |*kv| if (std.mem.eql(u8, kv.key, k)) {
            self.gpa.free(kv.value);
            kv.value = try self.gpa.dupe(u8, v);
            return;
        };
        try self.tree0.append(self.gpa, .{ .key = try self.gpa.dupe(u8, k), .value = try self.gpa.dupe(u8, v) });
    }

    fn del(self: *TestVolume, k: []const u8) void {
        for (self.tree0.items, 0..) |kv, i| if (std.mem.eql(u8, kv.key, k)) {
            self.gpa.free(kv.key);
            self.gpa.free(kv.value);
            _ = self.tree0.swapRemove(i);
            return;
        };
    }

    fn controlFn(ctx: *anyopaque, recs: []const ControlRecord) ControlError!void {
        const self = self_(ctx);
        if (self.window_full_next > 0) {
            self.window_full_next -= 1;
            return error.JournalReserveExhausted;
        }
        for (recs) |r| switch (r) {
            .put => |p| try self.put(p.key, p.value),
            .delete => |k| self.del(k),
            .alloc => |e| self.bits[@intCast(e)] = true,
            .free => |e| self.bits[@intCast(e)] = false,
        };
        self.control_entries += 1;
        self.barriers += 1;
        self.ring0_head += 100;
    }

    fn readPageFn(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self = self_(ctx);
        if (offset < self.image_base or offset + page.PAGE_LEN > self.image_base + self.image.len) return error.Io;
        @memcpy(out, self.at(offset)[0..page.PAGE_LEN]);
    }

    fn writePagesFn(ctx: *anyopaque, writes: []const PageWriteOp) error{Io}!void {
        const self = self_(ctx);
        for (writes) |w| @memcpy(self.at(w.offset)[0..page.PAGE_LEN], w.image);
        self.barriers += 1;
    }

    fn zeroFn(ctx: *anyopaque, chunks: []const region.ZeroChunk) error{Io}!void {
        const self = self_(ctx);
        for (chunks) |c| {
            @memset(self.at(c.offset)[0..@intCast(c.len)], 0);
            self.zeroed_bytes += c.len;
        }
    }

    fn claimFn(ctx: *anyopaque) error{NoSpace}!u64 {
        const self = self_(ctx);
        for (self.bits, 0..) |b, i| if (!b) {
            self.bits[i] = true;
            return i;
        };
        return error.NoSpace;
    }

    fn releaseFn(ctx: *anyopaque, extent: u64) void {
        self_(ctx).bits[@intCast(extent)] = false;
    }

    fn freeExtentsFn(ctx: *anyopaque) u64 {
        const self = self_(ctx);
        return self.g.total_extents - self.allocatedExtents();
    }

    fn ring0HeadFn(ctx: *anyopaque) u64 {
        return self_(ctx).ring0_head;
    }

    fn ownsRegionFn(ctx: *anyopaque, id: u32) bool {
        for (self_(ctx).own_regions.items) |r| if (r == id) return true;
        return false;
    }

    fn regionGrantHeldFn(ctx: *anyopaque, _: u32, extent: u64) bool {
        for (self_(ctx).ram_held.items) |e| if (e == extent) return true;
        return false;
    }

    fn windowClearFn(ctx: *anyopaque, slots: []const u16) bool {
        const self = self_(ctx);
        if (self.cycles_until_clear > 0) return false;
        for (slots) |s| for (self.window_names.items) |n| if (n == s) return false;
        return true;
    }

    fn cycleFn(ctx: *anyopaque) error{ Io, Corrupt }!u64 {
        const self = self_(ctx);
        self.cycles_run += 1;
        if (self.cycles_until_clear > 0) {
            self.cycles_until_clear -= 1;
            if (self.cycles_until_clear == 0) self.window_names.clearRetainingCapacity();
        }
        if (self.tail_moves) self.ring0_head += 7;
        return self.ring0_head;
    }

    fn kickFn(ctx: *anyopaque) void {
        self_(ctx).kicks += 1;
    }
};

fn newManager(gpa: Allocator, tv: *TestVolume) Manager {
    var m = Manager.init(gpa, testing.io, tv.volume());
    m.holdLease(1);
    return m;
}

const ident_a: Identity = .{ .node_token = 0xAA, .mount_slot = 0x5001, .writer_id = 0x1001 };
const ident_b: Identity = .{ .node_token = 0xAA, .mount_slot = 0x5002, .writer_id = 0x1002 };

test "join: allocates the lowest free page, carves + zeroes a ring at the floor, mints an incarnation, writes the page Live into both directory slots, grants the pool floor; the record and the page agree; a replay answers already with a fresh incarnation" {
    const gpa = testing.allocator;
    // 1,200 × 64 KiB extents = 75 MiB heap ⇒ budget 4.7 MiB ⇒ the manager's
    // 1 MiB ring + 7 floor rings.
    const tv = try TestVolume.init(gpa, 128 << 20, 65_536, 1200);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    const joins_before = stats.appender_joins.load();
    const j = try m.joinAppender(.{ .identity = ident_a, .home_volume = 2 });
    try testing.expect(!j.already);
    try testing.expectEqual(@as(u32, 1), j.appender_id);
    try testing.expectEqual(dir.pairOffsets(tv.g.appender_dir, 0)[0], j.page_addr);
    try testing.expectEqual(region.RING_FLOOR_BYTES, j.ring_segments.totalBytes());
    try testing.expectEqual(region.RING_FLOOR_BYTES, tv.zeroed_bytes);
    try testing.expectEqual(@as(u64, 0x1000 + node_seq.INCARNATION_SPACE), j.node_seq_base);
    // The pool floor 8 + M(4) = 12 extents granted.
    var total: u64 = 0;
    for (j.grantRuns()) |r| total += r.len;
    try testing.expectEqual(@as(u64, 12), total);
    // The page: Live, term 1, identity, home volume, segments, seq continuation at ring 0's head.
    const p = (try m.readPageOf(1)).?;
    try testing.expect(p.state == .live and !p.is_manager and p.term == 1 and p.identity.eql(ident_a) and p.home_volume == 2);
    try testing.expect(p.segments.eql(&j.ring_segments));
    try testing.expect(p.head_hint >= 1000 and p.ledger_tail_seq == p.head_hint and p.ckpt_seq == 0);
    try testing.expectEqual(@as(u64, 2), p.generation);
    // Tree 0: the record, the counter.
    const rec = cr.decodeExtentGrant(tv.get(cr.extentGrantKey(1).slice()).?) catch unreachable;
    try testing.expectEqual(@as(u64, 12), rec.totalExtents());
    try testing.expectEqual(@as(u64, 1), try cr.decodeNodeSeqIncarnations(tv.get(cr.nodeSeqIncarnationsKey().slice()).?));
    // Bits: dir + 8 ring + 12 grant.
    try testing.expectEqual(@as(u64, 1 + 8 + 12), tv.allocatedExtents());
    try testing.expectEqual(joins_before + 1, stats.appender_joins.load());
    // Idempotent: the Live page under the identity IS the join; a fresh incarnation anyway.
    const again = try m.joinAppender(.{ .identity = .{ .node_token = 0xAA, .mount_slot = 0x5001, .writer_id = 0x9999 } });
    try testing.expect(again.already);
    try testing.expectEqual(@as(u32, 1), again.appender_id);
    try testing.expectEqual(@as(u64, 0x1000 + 2 * node_seq.INCARNATION_SPACE), again.node_seq_base);
    try testing.expectEqual(@as(u64, 1 + 8 + 12), tv.allocatedExtents());
    // A second identity takes page 2; the rings in use are counted.
    const j2 = try m.joinAppender(.{ .identity = ident_b });
    try testing.expectEqual(@as(u32, 2), j2.appender_id);
    try testing.expectEqual(@as(u64, 3), m.appenders_known);
}

test "join: a join past the ring budget's remainder refuses naming the volume count lever on its own counter (never the refusal tripwire); the chain grows when every pair is taken" {
    const gpa = testing.allocator;
    // 400 extents ⇒ 1.56 MiB budget ⇒ the manager's 1 MiB ring leaves one floor ring.
    const tv = try TestVolume.init(gpa, 64 << 20, 65_536, 400);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    _ = try m.joinAppender(.{ .identity = ident_a });
    const before = stats.manager_capacity_refusals.load();
    const refusals_before = stats.manager_verb_refusals.load();
    try testing.expectError(error.Capacity, m.joinAppender(.{ .identity = ident_b }));
    try testing.expectEqual(before + 1, stats.manager_capacity_refusals.load());
    try testing.expectEqual(refusals_before, stats.manager_verb_refusals.load());
    // A wide heap: fill every pair of the first link (7 at 64 KiB), the 8th join grows the chain.
    const wide = try TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer wide.deinit();
    try wide.liveManagerPage();
    var mw = newManager(gpa, wide);
    defer mw.deinit();
    const grown_before = stats.appender_directory_links_grown.load();
    for (0..8) |i| {
        const j = try mw.joinAppender(.{ .identity = .{ .node_token = 0xAA, .mount_slot = @intCast(0x6000 + i), .writer_id = i } });
        try testing.expectEqual(@as(u32, @intCast(i + 1)), j.appender_id);
    }
    try testing.expectEqual(grown_before + 1, stats.appender_directory_links_grown.load());
    const p8 = (try mw.readPageOf(8)).?;
    try testing.expect(p8.state == .live);
    try testing.expectEqual(@as(u64, 9), mw.appenders_known);
}

test "leave: screens (own region, unknown id, a foreign identity, a Free page = already); the page goes Free into both slots keeping id/term/head_hint, the ring's bits clear after the barrier; the unclaimed runs return through the law" {
    const gpa = testing.allocator;
    const tv = try TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    const j = try m.joinAppender(.{ .identity = ident_a });
    const bits_live = tv.allocatedExtents();
    try testing.expectError(error.Rejected, m.leaveAppender(.{ .identity = ident_a, .appender_id = 0, .unclaimed = &.{} }));
    try testing.expectError(error.Rejected, m.leaveAppender(.{ .identity = ident_a, .appender_id = 77, .unclaimed = &.{} }));
    try testing.expectError(error.Rejected, m.leaveAppender(.{ .identity = ident_b, .appender_id = 1, .unclaimed = &.{} }));
    try tv.own_regions.append(gpa, 1);
    try testing.expectError(error.Rejected, m.leaveAppender(.{ .identity = ident_a, .appender_id = 1, .unclaimed = &.{} }));
    tv.own_regions.clearRetainingCapacity();
    // Busy while tree 0 leases it anything.
    const leased: cr.SlotState = .{ .variant = .leased, .appender_id = 1, .g = 1, .page_addr = j.page_addr, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 };
    try tv.put(cr.slotStateKey(9).slice(), &leased.encode());
    try testing.expectError(error.Deferred, m.leaveAppender(.{ .identity = ident_a, .appender_id = 1, .unclaimed = &.{} }));
    tv.del(cr.slotStateKey(9).slice());
    // The clean leave returns the whole unclaimed grant.
    const left = try m.leaveAppender(.{ .identity = ident_a, .appender_id = 1, .unclaimed = j.grantRuns() });
    try testing.expect(!left.already);
    const p = (try m.readPageOf(1)).?;
    try testing.expect(p.state == .free and p.term == 1 and p.identity.eql(.none) and p.segments.len == 0 and p.ledger_tail_seq == p.head_hint);
    try testing.expectEqual(@as(u64, 4), p.generation);
    try testing.expectEqual(bits_live - 8 - 12, tv.allocatedExtents());
    try testing.expect(tv.get(cr.extentGrantKey(1).slice()) == null);
    // Replay: a Free page answers already, nothing written.
    const entries = tv.control_entries;
    const again = try m.leaveAppender(.{ .identity = ident_a, .appender_id = 1, .unclaimed = &.{} });
    try testing.expect(again.already);
    try testing.expectEqual(entries, tv.control_entries);
    // The id is reusable: the next join takes page 1 at term 2 and continues the seq space.
    const j2 = try m.joinAppender(.{ .identity = ident_b });
    try testing.expectEqual(@as(u32, 1), j2.appender_id);
    const p2 = (try m.readPageOf(1)).?;
    try testing.expectEqual(@as(u64, 2), p2.term);
    try testing.expect(p2.head_hint >= p.head_hint);
}

test "grants: the manager never grants itself; want is screened against the heap-share cap; a covered want replays verbatim; a top-up carves to the pool size; the record is normalized and the closure holds; returns are screened, idempotent and clear bits; a RAM-held extent's return is the refusal tripwire" {
    const gpa = testing.allocator;
    const tv = try TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    const j = try m.joinAppender(.{ .identity = ident_a });
    try testing.expectError(error.Rejected, m.extentGrant(.{ .appender_id = 0, .want = 8 }));
    try testing.expectError(error.Rejected, m.extentGrant(.{ .appender_id = 1, .want = 1 << 40 }));
    // The page word covers the want: verbatim.
    const rep = try m.extentGrant(.{ .appender_id = 1, .want = 12, .page_word = j.grantRuns() });
    defer gpa.free(rep.runs);
    try testing.expect(rep.replayed);
    var n: u64 = 0;
    for (rep.runs) |r| n += r.len;
    try testing.expectEqual(@as(u64, 12), n);
    // A top-up: 12 held, want 20 ⇒ 8 carved; the record names 20.
    const granted_before = stats.extent_grant_extents.load();
    const g1 = try m.extentGrant(.{ .appender_id = 1, .want = 20, .page_word = j.grantRuns() });
    defer gpa.free(g1.runs);
    try testing.expect(!g1.replayed);
    n = 0;
    for (g1.runs) |r| n += r.len;
    try testing.expectEqual(@as(u64, 8), n);
    const rec = cr.decodeExtentGrant(tv.get(cr.extentGrantKey(1).slice()).?) catch unreachable;
    try testing.expectEqual(@as(u64, 20), rec.totalExtents());
    try testing.expectEqual(granted_before + 8, stats.extent_grant_extents.load());
    // A stale page word (an extent the record never held) is counted, not granted.
    const stale_before = stats.extent_grant_stale_page_words.load();
    const g2 = try m.extentGrant(.{ .appender_id = 1, .want = 20, .page_word = &.{.{ .start = 3900, .len = 2 }} });
    defer gpa.free(g2.runs);
    try testing.expectEqual(stale_before + 2, stats.extent_grant_stale_page_words.load());
    // Returns: a run past the volume is rejected; a run outside the record is `already`.
    var past = [_]GrantRun{.{ .start = 3999, .len = 2 }};
    try testing.expectError(error.Rejected, m.returnExtents(.{ .appender_id = 1, .runs = &past }));
    var outside = [_]GrantRun{.{ .start = 3000, .len = 3 }};
    const r0 = try m.returnExtents(.{ .appender_id = 1, .runs = &outside });
    try testing.expectEqual(Returned{ .cleared = 0, .already = 3 }, r0);
    // Return the first granted run: cleared + already ≡ named.
    const first = j.grantRuns()[0];
    var runs = [_]GrantRun{ first, .{ .start = 3000, .len = 1 } };
    const bits_before = tv.allocatedExtents();
    const r1 = try m.returnExtents(.{ .appender_id = 1, .runs = &runs });
    try testing.expectEqual(@as(u64, first.len), r1.cleared);
    try testing.expectEqual(@as(u64, 1), r1.already);
    try testing.expectEqual(bits_before - first.len, tv.allocatedExtents());
    const rec2 = cr.decodeExtentGrant(tv.get(cr.extentGrantKey(1).slice()).?) catch unreachable;
    try testing.expect(!rec2.contains(first.start));
    // Replayed: nothing held ⇒ already.
    const r2 = try m.returnExtents(.{ .appender_id = 1, .runs = &runs });
    try testing.expectEqual(@as(u64, 0), r2.cleared);
    // The own-region RAM screen fires the refusal tripwire.
    try tv.own_regions.append(gpa, 1);
    const held = g1.runs[0].start;
    try tv.ram_held.append(gpa, held);
    var held_run = [_]GrantRun{.{ .start = held, .len = 1 }};
    const refusals_before = stats.manager_verb_refusals.load();
    stats.manager_verb_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Refused, m.returnExtents(.{ .appender_id = 1, .runs = &held_run }));
    try log.testing.expectAllFired();
    try testing.expectEqual(refusals_before + 1, stats.manager_verb_refusals.load());
}

test "grow ring: screens; a pending witness this incarnation's page does not name replays verbatim; a short run declines on its own counter; a real growth carves the longest run, zeroes it, writes the hint with the pending word; eight segments decline" {
    const gpa = testing.allocator;
    const tv = try TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    const j = try m.joinAppender(.{ .identity = ident_a });
    try testing.expectError(error.Rejected, m.growRing(.{ .appender_id = 0, .want_bytes = 1 << 20 }));
    try testing.expectError(error.Rejected, m.growRing(.{ .appender_id = 5, .want_bytes = 1 << 20 }));
    const zero_before = tv.zeroed_bytes;
    const grown = try m.growRing(.{ .appender_id = 1, .want_bytes = region.RING_FLOOR_BYTES });
    const seg = grown.segment.?;
    try testing.expectEqual(region.RING_FLOOR_BYTES, seg.len);
    try testing.expectEqual(zero_before + seg.len, tv.zeroed_bytes);
    const hint = cr.AppenderHintWire.decode(tv.get(cr.appenderHintKey(.{ .node_token = ident_a.node_token, .mount_slot = ident_a.mount_slot }).slice()).?) catch unreachable;
    try testing.expectEqual(2 * region.RING_FLOOR_BYTES, hint.ring_bytes);
    try testing.expectEqual(seg.start, hint.pending.?.start);
    try testing.expectEqual(@as(u64, 1), hint.pending.?.term);
    // The lost reply re-asked: verbatim, nothing carved.
    const bits = tv.allocatedExtents();
    const again = try m.growRing(.{ .appender_id = 1, .want_bytes = region.RING_FLOOR_BYTES });
    try testing.expectEqual(seg, again.segment.?);
    try testing.expectEqual(bits, tv.allocatedExtents());
    // The joiner's page names the segment: a later rejoin settles the hint clear.
    var p = (try m.readPageOf(1)).?;
    try p.segments.push(seg);
    var img: [page.PAGE_LEN]u8 = undefined;
    p.generation += 1;
    try p.encode(&img);
    @memcpy(tv.at(j.page_addr)[0..page.PAGE_LEN], &img);
    // Eight segments: declined.
    var p8 = (try m.readPageOf(1)).?;
    while (p8.segments.len < page.RING_SEGMENTS_MAX) try p8.segments.push(.{ .start = 0xF000000 + @as(u64, p8.segments.len) * 65_536, .len = 65_536 });
    p8.generation += 1;
    try p8.encode(&img);
    @memcpy(tv.at(j.page_addr)[0..page.PAGE_LEN], &img);
    const declined = try m.growRing(.{ .appender_id = 1, .want_bytes = region.RING_FLOOR_BYTES });
    try testing.expect(declined.segment == null);
    // The leave settles the pending word: a segment the page names goes with the ring.
    const left = try m.leaveAppender(.{ .identity = ident_a, .appender_id = 1, .unclaimed = j.grantRuns() });
    try testing.expect(!left.already);
    const hint2 = cr.AppenderHintWire.decode(tv.get(cr.appenderHintKey(.{ .node_token = ident_a.node_token, .mount_slot = ident_a.mount_slot }).slice()).?) catch unreachable;
    try testing.expect(hint2.pending == null);
    try testing.expectEqual(2 * region.RING_FLOOR_BYTES, hint2.ring_bytes);
    // A rejoin starts at the hinted ring size.
    const j2 = try m.joinAppender(.{ .identity = ident_a });
    try testing.expectEqual(2 * region.RING_FLOOR_BYTES, j2.ring_segments.totalBytes());
}

test "publish roots: a word for a slot the appender does not lease, a cursor below the recorded floor, a root outside the grant, or a wrong generation is rejected before any effect; a legal word lands in tree 0; the same word again is already" {
    const gpa = testing.allocator;
    const tv = try TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    const j = try m.joinAppender(.{ .identity = ident_a });
    const leased: cr.SlotState = .{ .variant = .leased, .appender_id = 1, .g = 3, .page_addr = j.page_addr, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 };
    try tv.put(cr.slotStateKey(5).slice(), &leased.encode());
    const in_grant = tv.g.heap.start + j.grantRuns()[0].start * tv.g.node_size;
    const entries = tv.control_entries;
    try testing.expectError(error.Rejected, m.publishRoots(.{ .appender_id = 1, .roots = &.{.{ .slot = 6, .g = 3, .root = .{ .addr = in_grant, .seq = 1 }, .cursor = 2, .slot_tree_extents = 1 }} }));
    try testing.expectError(error.Rejected, m.publishRoots(.{ .appender_id = 1, .roots = &.{.{ .slot = 5, .g = 4, .root = .{ .addr = in_grant, .seq = 1 }, .cursor = 2, .slot_tree_extents = 1 }} }));
    try testing.expectError(error.Rejected, m.publishRoots(.{ .appender_id = 1, .roots = &.{.{ .slot = 5, .g = 3, .root = .{ .addr = in_grant, .seq = 1 }, .cursor = 1, .slot_tree_extents = 1 }} }));
    try testing.expectError(error.Rejected, m.publishRoots(.{ .appender_id = 1, .roots = &.{.{ .slot = 5, .g = 3, .root = .{ .addr = tv.g.heap.start + 3999 * tv.g.node_size, .seq = 1 }, .cursor = 2, .slot_tree_extents = 1 }} }));
    try testing.expectError(error.Rejected, m.publishRoots(.{ .appender_id = 1, .roots = &.{.{ .slot = 5, .g = 3, .root = .{ .addr = in_grant + 1, .seq = 1 }, .cursor = 2, .slot_tree_extents = 1 }} }));
    try testing.expectError(error.Rejected, m.publishRoots(.{ .appender_id = 1, .roots = &.{.{ .slot = 5, .g = 3, .root = .{ .addr = in_grant, .seq = 1 }, .cursor = 2, .slot_tree_extents = 5000 }} }));
    try testing.expectError(error.Rejected, m.publishRoots(.{ .appender_id = 2, .roots = &.{.{ .slot = 5, .g = 3, .root = .{ .addr = in_grant, .seq = 1 }, .cursor = 2, .slot_tree_extents = 1 }} }));
    try testing.expectEqual(entries, tv.control_entries);
    const pub1 = try m.publishRoots(.{ .appender_id = 1, .roots = &.{.{ .slot = 5, .g = 3, .root = .{ .addr = in_grant, .seq = 44 }, .cursor = 9, .slot_tree_extents = 1 }} });
    try testing.expectEqual(RootsPublished{ .published = 1, .already = 0 }, pub1);
    const s = cr.SlotStateWire.decode(tv.get(cr.slotStateKey(5).slice()).?) catch unreachable;
    try testing.expectEqual(cr.Root{ .addr = in_grant, .seq = 44 }, s.root);
    try testing.expectEqual(@as(u64, 9), s.cursor);
    try testing.expect(s.variant == .leased and s.appender_id == 1);
    const pub2 = try m.publishRoots(.{ .appender_id = 1, .roots = &.{.{ .slot = 5, .g = 3, .root = .{ .addr = in_grant, .seq = 44 }, .cursor = 9, .slot_tree_extents = 1 }} });
    try testing.expectEqual(RootsPublished{ .published = 0, .already = 1 }, pub2);
}

test "endpoints: publish binds identity ↔ page strictly, is idempotent on the same address, re-points on a new one; resolve answers the table; forget drops it" {
    const gpa = testing.allocator;
    const tv = try TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    _ = try m.joinAppender(.{ .identity = ident_a });
    var ep: Endpoint = .{};
    @memcpy(ep.bytes[0..6], "\x02\x00\x1f\x90\x7f\x00");
    ep.len = 6;
    try testing.expectError(error.Rejected, m.publishEndpoint(.{ .identity = ident_b, .appender_id = 1, .endpoint = ep, .pr_key = 1 }));
    try testing.expectError(error.Rejected, m.publishEndpoint(.{ .identity = ident_a, .appender_id = 1, .endpoint = .{}, .pr_key = 1 }));
    try testing.expect(!(try m.publishEndpoint(.{ .identity = ident_a, .appender_id = 1, .endpoint = ep, .pr_key = 1 })).already);
    try testing.expect((try m.publishEndpoint(.{ .identity = ident_a, .appender_id = 1, .endpoint = ep, .pr_key = 1 })).already);
    try testing.expect((try m.resolveEndpoint(.{ .appender_id = 1 })).endpoint.?.eql(ep));
    try testing.expect((try m.resolveEndpoint(.{ .appender_id = 2 })).endpoint == null);
    ep.bytes[5] = 1;
    try testing.expect(!(try m.publishEndpoint(.{ .identity = ident_a, .appender_id = 1, .endpoint = ep, .pr_key = 1 })).already);
    m.forgetEndpoint(1);
    try testing.expect((try m.resolveEndpoint(.{ .appender_id = 1 })).endpoint == null);
}

test "the reply classifier and the dispatch table: Rejected → rejected, Deferred → deferred, a full USER window → deferred + the checkpoint kick, the rest refused with their reason; an unserved verb is rejected{verb_unserved}; no lease is not_manager; the served set" {
    const gpa = testing.allocator;
    const tv = try TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    try testing.expectEqual(Classified{ .status = .rejected, .reason = .screen_rejected }, classify(error.Rejected, null, null));
    try testing.expectEqual(Status.deferred, classify(error.Deferred, null, null).status);
    try testing.expectEqual(Classified{ .status = .refused, .reason = .capacity }, classify(error.Capacity, null, null));
    try testing.expectEqual(Classified{ .status = .refused, .reason = .no_space }, classify(error.NoSpace, null, null));
    const kicks = tv.kicks;
    const c = classify(error.JournalReserveExhausted, TestVolume.vtable.kickCheckpoint, tv);
    try testing.expectEqual(Classified{ .status = .deferred, .reason = .window_full }, c);
    try testing.expectEqual(kicks + 1, tv.kicks);
    // A verb whose control entry finds the window full surfaces the class.
    tv.window_full_next = 1;
    try testing.expectError(error.JournalReserveExhausted, m.joinAppender(.{ .identity = ident_a }));
    var table: HandlerTable = .{};
    try testing.expect(!table.served(.acquire_slots));
    const r = table.dispatch(&m, gpa, .acquire_slots, &.{});
    try testing.expectEqual(Status.rejected, r.status);
    try testing.expectEqual(Reason.verb_unserved, r.reason);
    const H = struct {
        fn ok(_: *Manager, _: Allocator, _: []const u8) Reply {
            return .{ .status = .ok };
        }
    };
    table.bind(.resolve_endpoint, H.ok);
    try testing.expectEqual(Status.ok, table.dispatch(&m, gpa, .resolve_endpoint, &.{}).status);
    m.lease = .vacant;
    try testing.expectEqual(Status.not_manager, table.dispatch(&m, gpa, .resolve_endpoint, &.{}).status);
    try testing.expectError(error.Refused, m.resolveEndpoint(.{ .appender_id = 1 }));
    try testing.expectEqual(@as(usize, 8), HandlerTable.servedVerbs().len);
    var buf: [64]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try m.leaseWord(&w);
    try testing.expectEqualStrings("vacant", w.buffered());
    m.lease = .peer;
    m.peer_node = 0xBEEF;
    w = .fixed(&buf);
    try m.leaseWord(&w);
    try testing.expectEqualStrings("peer:0xbeef", w.buffered());
}

test "the structural door: clear at entry costs nothing; a window that clears within the bound is cycled clear; a moving tail at the bound is deferred; a tail that never moves is Corrupt" {
    const gpa = testing.allocator;
    const tv = try TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = newManager(gpa, tv);
    defer m.deinit();
    try testing.expect((try m.structuralDoor(&.{ 1, 2 }, tv.ring0_head)) == .cleared);
    try testing.expectEqual(@as(u32, 0), tv.cycles_run);
    try tv.window_names.append(gpa, 2);
    tv.cycles_until_clear = 3;
    try testing.expect((try m.structuralDoor(&.{2}, tv.ring0_head)) == .cleared);
    try testing.expectEqual(@as(u32, 3), tv.cycles_run);
    // Never clears, the tail moves: deferred at the bound.
    try tv.window_names.append(gpa, 7);
    tv.cycles_run = 0;
    const d = try m.structuralDoor(&.{7}, tv.ring0_head);
    try testing.expect(d == .deferred);
    try testing.expectEqual(checkpoint.COVER_CYCLES_MAX, d.deferred.cycles);
    try testing.expect(d.deferred.tail > d.deferred.tail_start);
    // A tail that never moves is the defect class.
    tv.tail_moves = false;
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Corrupt, m.structuralDoor(&.{7}, tv.ring0_head));
    try log.testing.expectAllFired();
}
