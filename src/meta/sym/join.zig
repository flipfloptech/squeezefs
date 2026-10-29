//! The join ladder of a LATER RW mount (charter §5.7 "Under the symmetric
//! default the D0 winner IS the manager and a later RW mount JOINS it";
//! §5.5 "Appenders"; `docs/porting/sym_appenders_manager.md` §3.1 "the
//! joiner's side", §3.4): present identity → `JoinAppender` (in-process
//! when the manager is this process; the loopback/wire otherwise) → the
//! reply's words SCREENED (`node_seq_base` before anything is installed)
//! → the region core from the reply (its page slots, its ring table, its
//! seq continuation) → the RAM grant from the REPLY's grant word, never
//! its own page read → `NodeSeqHandle.incarnation(base)` → `Live`.
//! PORTABLE over a `Door` seam (the manager's `joinAppender` in-process;
//! the wire's client otherwise); the device-bound joiner runtime (the
//! lane over the ring) is `bind.zig`'s.
//!
//! Identity (`appender_page.Identity`): `node_token = hash64(boot_id ‖
//! hostname_hash)` — ONE per host, so N daemons on one box share it and
//! differ by `mount_slot` (the production shape: `0x5000 + n` for the
//! in-process harness, the mount's lane index for a daemon); `writer_id`
//! = the claim's per-incarnation derivation (0002 row 185). A page is
//! OURS iff it carries this exact `(node, mount slot)`.

const std = @import("std");
const counters = @import("../../core/counters.zig");
const checksum = @import("../../core/checksum.zig");
const log = @import("../../core/log.zig");
const errors = @import("../../core/errors.zig");
const claim = @import("../guard/claim.zig");
const node_seq = @import("../node_seq.zig");
const page = @import("appender_page.zig");
const region = @import("region.zig");
const manager = @import("manager.zig");
const grant = @import("extent_grant.zig");

const Allocator = std.mem.Allocator;
pub const Identity = page.Identity;

pub const stats = struct {
    /// A `Joined` word the screen refused: must stay 0 (a buggy or hostile
    /// manager frame).
    pub var joined_wire_words_rejected: counters.Tripwire = .init("joined_wire_words_rejected");
    pub var joined_rejoins: counters.ShardedCounter = .{};
    pub var joined_capacity_refusals: counters.ShardedCounter = .{};
};

/// The in-process harness's mount-slot base (the production shape of N
/// daemons on one box: the manager's node token at `0x5000 + n`).
pub const HARNESS_MOUNT_SLOT_BASE: u32 = 0x5000;

/// ONE token per host.
pub fn nodeToken(id: claim.Identity) u64 {
    var h: checksum.Hasher = .init();
    h.update(&id.boot_id);
    h.update(std.mem.asBytes(&std.mem.nativeToLittle(u64, id.hostname_hash)));
    return h.finalLow64();
}

/// The appender identity a mount presents at `mount_slot`.
pub fn identityFor(id: claim.Identity, mount_slot: u32) Identity {
    return .{ .node_token = nodeToken(id), .mount_slot = mount_slot, .writer_id = id.writerId(@truncate(mount_slot)) };
}

/// The manager's door as the joiner sees it.
pub const Door = struct {
    ctx: *anyopaque,
    join: *const fn (ctx: *anyopaque, req: manager.JoinAppender) manager.VerbError!manager.Joined,
    leave: *const fn (ctx: *anyopaque, req: manager.LeaveAppender) manager.VerbError!manager.Left,
    /// The door is the manager's own process (the N-daemon harness shape).
    in_process: bool = false,
};

pub const JoinError = error{
    /// The manager named the volume count as the lever.
    Capacity,
    /// A `Joined` word failed the screen; nothing installed.
    Rejected,
    /// The manager answered `deferred`: retry.
    Deferred,
    Io,
    Corrupt,
    OutOfMemory,
};

/// What a join installs: the region core, the RAM grant, the node-seq
/// handle, the words the reply carried.
pub const JoinedRegion = struct {
    core: region.RegionCore,
    grant: grant.RegionGrant,
    seq: node_seq.NodeSeqHandle,
    identity: Identity,
    already: bool,

    pub fn deinit(self: *JoinedRegion) void {
        self.core.deinit();
        self.grant.deinit();
    }

    pub fn appenderId(self: *const JoinedRegion) u32 {
        return self.core.appender_id;
    }
};

pub const JoinInputs = struct {
    identity: Identity,
    /// 0 = derive.
    ring_want_bytes: u64 = 0,
    home_volume: u16 = 0,
    /// `nodeSeqBase(uuid)` — the screen's base.
    volume_base: u64,
    grain: u64,
    /// The page words the region resumes from (a rejoin reads its own
    /// page's `head_hint`; a fresh join takes the reply's continuation).
    resume_page: ?page.Page = null,
    /// Ring 0's head as the seq continuation's other term.
    ring0_head: u64,
};

/// The ladder. Every refusal is typed; the capacity refusal's message is
/// `writeCapacityRefusal`'s.
pub fn join(gpa: Allocator, door: Door, in: JoinInputs) JoinError!JoinedRegion {
    const reply = door.join(door.ctx, .{ .identity = in.identity, .ring_want_bytes = in.ring_want_bytes, .home_volume = in.home_volume }) catch |e| return switch (e) {
        error.Capacity => blk: {
            stats.joined_capacity_refusals.inc();
            break :blk error.Capacity;
        },
        error.Rejected => error.Rejected,
        error.Deferred, error.JournalReserveExhausted => error.Deferred,
        error.NoSpace => error.Io,
        error.Refused => error.Corrupt,
        error.Io => error.Io,
        error.Corrupt => error.Corrupt,
        error.OutOfMemory => error.OutOfMemory,
    };
    // The screen BEFORE anything is installed.
    var diag: node_seq.ScreenRefusal = undefined;
    const base = node_seq.screenIncarnationBase(in.volume_base, reply.node_seq_base, &diag) catch {
        var buf: [512]u8 = undefined;
        var w: std.Io.Writer = .fixed(&buf);
        node_seq.writeScreenRefusal(&w, in.volume_base, reply.node_seq_base, diag) catch {};
        stats.joined_wire_words_rejected.fire(w.buffered());
        return error.Rejected;
    };
    if (reply.ring_segments.len == 0) {
        stats.joined_wire_words_rejected.fire("Joined.ring_segments is empty");
        return error.Rejected;
    }
    for (reply.grantRuns()) |r| if (r.len == 0) {
        stats.joined_wire_words_rejected.fire("Joined.grant names an empty run");
        return error.Rejected;
    };
    // The region from the reply's words: the manager's page is the truth
    // (term, generation, the seq continuation); a rejoin resumes its own
    // page's head.
    var words: page.Page = in.resume_page orelse .{ .appender_id = reply.appender_id, .generation = reply.generation };
    words.appender_id = reply.appender_id;
    words.term = reply.term;
    words.generation = @max(words.generation, reply.generation);
    words.state = .live;
    words.identity = in.identity;
    words.segments = reply.ring_segments;
    if (in.resume_page == null) {
        const cont = region.seqContinuation(reply.head_hint, in.ring0_head);
        words.head_hint = cont;
        words.ledger_tail_seq = cont;
    }
    var core = region.RegionCore.init(gpa, .{
        .appender_id = reply.appender_id,
        .table = reply.ring_segments,
        .dir_slots = .{ reply.page_addr, reply.page_addr + page.PAGE_LEN },
        .grain = in.grain,
        .head_pos = words.head_hint,
        .tail_pos = words.ledger_tail_seq,
        .page_words = words,
    }) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => |other| errors.narrow(JoinError, other, "join.join"),
    };
    errdefer core.deinit();
    var g = grant.RegionGrant.init(gpa);
    errdefer g.deinit();
    _ = try g.absorb(reply.grantRuns());
    if (reply.already) stats.joined_rejoins.inc();
    return .{ .core = core, .grant = g, .seq = node_seq.NodeSeqHandle.incarnation(base), .identity = in.identity, .already = reply.already };
}

/// The clean leave: the pool's unclaimed runs return with the page.
pub fn leave(gpa: Allocator, door: Door, jr: *JoinedRegion) JoinError!manager.Left {
    var runs: std.ArrayList(grant.GrantRun) = .empty;
    defer runs.deinit(gpa);
    try jr.grant.unclaimedRuns(gpa, &runs);
    return door.leave(door.ctx, .{ .identity = jr.identity, .appender_id = jr.appenderId(), .unclaimed = runs.items, .in_process = door.in_process }) catch |e| switch (e) {
        error.Deferred, error.JournalReserveExhausted => error.Deferred,
        error.Rejected => error.Rejected,
        error.OutOfMemory => error.OutOfMemory,
        error.Io, error.NoSpace => error.Io,
        else => |other| errors.narrow(JoinError, other, "join.leave"),
    };
}

pub fn writeCapacityRefusal(w: *std.Io.Writer, live: u64, capacity: u64) std.Io.Writer.Error!void {
    try w.print("join refused: {d} appenders are live and the ring budget admits {d} (appenders_capacity = heap/16 ÷ ring) — the lever is the metadata volume COUNT, never a format-time client count", .{ live, capacity });
}

// ---------------------------------------------------------------------------
// Tests over the manager's RAM volume.
// ---------------------------------------------------------------------------

const testing = std.testing;

const InProcessDoor = struct {
    mgr: *manager.Manager,
    /// Seam: forge the reply's base.
    forge_base: ?u64 = null,

    fn door(self: *InProcessDoor) Door {
        return .{ .ctx = self, .join = joinFn, .leave = leaveFn };
    }

    fn joinFn(ctx: *anyopaque, req: manager.JoinAppender) manager.VerbError!manager.Joined {
        const self: *InProcessDoor = @ptrCast(@alignCast(ctx));
        var r = try self.mgr.joinAppender(req);
        if (self.forge_base) |b| r.node_seq_base = b;
        return r;
    }

    fn leaveFn(ctx: *anyopaque, req: manager.LeaveAppender) manager.VerbError!manager.Left {
        const self: *InProcessDoor = @ptrCast(@alignCast(ctx));
        return self.mgr.leaveAppender(req);
    }
};

fn testIdentity(mount_slot: u32) Identity {
    const id: claim.Identity = .{ .boot_id = "boot-id-fixture!".*, .pid = 4242, .start_ticks = 99, .hostname_hash = claim.hostnameHash("host-a") };
    return identityFor(id, mount_slot);
}

test "identity: one node token per host; mounts differ by slot; the writer id is the claim's derivation" {
    const a = testIdentity(HARNESS_MOUNT_SLOT_BASE);
    const b = testIdentity(HARNESS_MOUNT_SLOT_BASE + 1);
    try testing.expectEqual(a.node_token, b.node_token);
    try testing.expect(a.mount_slot != b.mount_slot and a.writer_id != b.writer_id);
    try testing.expect(a.sameMount(testIdentity(HARNESS_MOUNT_SLOT_BASE)) and !a.sameMount(b));
    const other: claim.Identity = .{ .boot_id = "another-boot-id!".*, .pid = 4242, .start_ticks = 99, .hostname_hash = claim.hostnameHash("host-a") };
    try testing.expect(nodeToken(other) != a.node_token);
}

test "the ladder: join → screened reply → region core + RAM grant from the reply's word + an incarnation handle; a forged base is refused before anything is installed; a rejoin answers already and resumes its page's head; the clean leave" {
    const gpa = testing.allocator;
    const tv = try manager.TestVolume.init(gpa, 1 << 30, 65_536, 4000);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = manager.Manager.init(gpa, testing.io, tv.volume());
    defer m.deinit();
    m.holdLease(1);
    var d: InProcessDoor = .{ .mgr = &m };
    const ident = testIdentity(HARNESS_MOUNT_SLOT_BASE);
    var jr = try join(gpa, d.door(), .{ .identity = ident, .volume_base = tv.g.volume_base, .grain = 4096, .ring0_head = tv.ring0_head });
    defer jr.deinit();
    try testing.expect(!jr.already);
    try testing.expectEqual(@as(u32, 1), jr.appenderId());
    try testing.expectEqual(region.RING_FLOOR_BYTES, jr.core.ringBytes());
    try testing.expectEqual(@as(u64, 12), jr.grant.unclaimedCount());
    try testing.expect(jr.grant.closureHolds());
    try testing.expectEqual(tv.g.volume_base + node_seq.INCARNATION_SPACE + 1, try jr.seq.mint());
    try testing.expect(!jr.seq.raises);
    // The core's head continues the seq space at (at least) ring 0's head.
    try testing.expect(jr.core.core.headPos() >= 1000);
    try testing.expect(jr.core.page.state == .live and jr.core.page.identity.eql(ident));
    try testing.expectEqual(@as(u64, 1), jr.core.page.term);
    try testing.expectEqual(@as(u64, 2), jr.core.page.generation);
    // A forged base: refused, counted, nothing installed (the manager's
    // page is Live — the next honest join is a rejoin).
    const rejected_before = stats.joined_wire_words_rejected.load();
    const rejoins_before = stats.joined_rejoins.load();
    d.forge_base = tv.g.volume_base;
    stats.joined_wire_words_rejected.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Rejected, join(gpa, d.door(), .{ .identity = testIdentity(HARNESS_MOUNT_SLOT_BASE + 1), .volume_base = tv.g.volume_base, .grain = 4096, .ring0_head = tv.ring0_head }));
    try log.testing.expectAllFired();
    try testing.expectEqual(rejected_before + 1, stats.joined_wire_words_rejected.load());
    d.forge_base = null;
    // The rejoin presents the predecessor page's identity: already, the
    // page's head resumed, a FRESH incarnation.
    const own_page = (try m.readPageOf(1)).?;
    var re = try join(gpa, d.door(), .{ .identity = ident, .volume_base = tv.g.volume_base, .grain = 4096, .resume_page = own_page, .ring0_head = tv.ring0_head });
    defer re.deinit();
    try testing.expect(re.already);
    try testing.expectEqual(own_page.head_hint, re.core.core.headPos());
    try testing.expect(re.seq.load() > jr.seq.load());
    try testing.expectEqual(rejoins_before + 1, stats.joined_rejoins.load());
    // The clean leave returns the pool; a second leave is `already`.
    const left = try leave(gpa, d.door(), &re);
    try testing.expect(!left.already);
    try testing.expect((try leave(gpa, d.door(), &re)).already);
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeCapacityRefusal(&w, 3, 3);
    try testing.expectEqualStrings("join refused: 3 appenders are live and the ring budget admits 3 (appenders_capacity = heap/16 ÷ ring) — the lever is the metadata volume COUNT, never a format-time client count", w.buffered());
}

test "capacity: the ladder surfaces the manager's refusal as the typed class on its own counter" {
    const gpa = testing.allocator;
    const tv = try manager.TestVolume.init(gpa, 64 << 20, 65_536, 400);
    defer tv.deinit();
    try tv.liveManagerPage();
    var m = manager.Manager.init(gpa, testing.io, tv.volume());
    defer m.deinit();
    m.holdLease(1);
    var d: InProcessDoor = .{ .mgr = &m };
    var first = try join(gpa, d.door(), .{ .identity = testIdentity(HARNESS_MOUNT_SLOT_BASE), .volume_base = tv.g.volume_base, .grain = 4096, .ring0_head = tv.ring0_head });
    defer first.deinit();
    const before = stats.joined_capacity_refusals.load();
    log.testing.expectErrorLogs(0);
    try testing.expectError(error.Capacity, join(gpa, d.door(), .{ .identity = testIdentity(HARNESS_MOUNT_SLOT_BASE + 1), .volume_base = tv.g.volume_base, .grain = 4096, .ring0_head = tv.ring0_head }));
    try testing.expectEqual(before + 1, stats.joined_capacity_refusals.load());
}
