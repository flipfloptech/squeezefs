//! GPFS-strict read tokens — the ONLY foreign-read method (charter §5.5
//! "Tokens"; `docs/porting/sym_tokens_intents_striping.md` §1): the
//! holder's grant table with the register-then-check gate and the recall
//! the conveyor pass runs at A1½ (`token_table`), the reader's cache with
//! the DMA-drain acks (`token_cache`), the read divert and the wire
//! binding (`read_divert`), `-o ro` and the epoch-poll control plane
//! (`ro_mount`), the models (`grant_gate_model`, `serve_stamp_model`).
//! Every registry is a field of the mount's objects (decision 0002 row
//! 259) — never a process static; the instruments alone are module
//! globals, folded over every plane as the reference's stats did.
//!
//! The contracts below run IN-PROCESS over the loopback hub with a real
//! wire session per pair: two members, A the holder of slot `s`, B a
//! reader (a writer's divert or a `-o ro` member — the same client), and
//! the broadcast shape.

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../core/counters.zig");
const conveyor = @import("../conveyor.zig");

pub const table = @import("tokens/token_table.zig");
pub const cache = @import("tokens/token_cache.zig");
pub const divert = @import("tokens/read_divert.zig");
pub const ro = @import("tokens/ro_mount.zig");
pub const grant_gate_model = @import("tokens/grant_gate_model.zig");
pub const serve_stamp_model = @import("tokens/serve_stamp_model.zig");
pub const smoke = if (builtin.os.tag == .linux) @import("tokens/smoke_linux.zig") else struct {};

pub const Holder = table.Holder;
pub const Reader = cache.Reader;
pub const Divert = divert.Divert;

/// The holder plane's binding onto a volume's conveyor (`conveyor.
/// TokenHook`): A1½ is `recallAndWait` over the batch's union with the
/// plane's pusher; the terminal outcome is `settle`. Armed with
/// `Conveyor.armTokens`; the binding outlives every window in flight.
pub const ConveyorBinding = struct {
    io: Io,
    holder: *Holder,
    pusher: table.Pusher,

    pub fn hook(self: *ConveyorBinding) conveyor.TokenHook {
        return .{ .ctx = self, .recall = &recall, .settle = &settle };
    }

    fn recall(ctx: *anyopaque, objects: []const u64) Allocator.Error!void {
        const self: *ConveyorBinding = @ptrCast(@alignCast(ctx));
        _ = try self.holder.recallAndWait(self.io, objects, self.pusher);
    }

    fn settle(ctx: *anyopaque, objects: []const u64) void {
        const self: *ConveyorBinding = @ptrCast(@alignCast(ctx));
        self.holder.settle(self.io, objects);
    }
};

/// The `.stats` `meta.sym.tokens` family: the holder side, the reader
/// side, the divert, the `-o ro` reader and the control-plane poll.
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeByte('{');
    inline for (.{
        .{ "holder", table.stats },
        .{ "reader", cache.stats },
        .{ "divert", divert.stats },
        .{ "ro", ro.stats },
    }, 0..) |m, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{m[0]});
        try counters.writeNamespaceJson(m[1], w);
    }
    try w.writeByte('}');
}

// ---------------------------------------------------------------------------
// Tests: the in-process two-member contracts over the loopback hub.
// ---------------------------------------------------------------------------

const testing = std.testing;
const errors = @import("../../core/errors.zig");
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const verbs = @import("../../wire/verbs.zig");
const service = @import("../../wire/service.zig");
const loopback = @import("../../wire/loopback.zig");
const auth = @import("../../wire/auth.zig");
const record = @import("../record.zig");
const key = @import("../key.zig");
const inos = @import("../inos.zig");
const route_table = @import("../routing/route_table.zig");
const jc = @import("../journal_core.zig");
const census_view = @import("membership/census_view.zig");

test {
    _ = table;
    _ = cache;
    _ = divert;
    _ = ro;
    _ = grant_gate_model;
    _ = serve_stamp_model;
    if (builtin.os.tag == .linux) _ = smoke;
}

test "the stats family names every instrument of the four namespaces" {
    var buf: [65536]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    for ([_][]const u8{ "\"holder\":{", "\"dlm_token_recall_timeouts_live\":", "\"dlm_token_recall_rtt_total\":", "\"reader\":{", "\"dlm_token_cached_bytes\":", "\"reader_serve_slot_overruns\":", "\"divert\":{", "\"dlm_token_reader_holder_planes\":", "\"ro\":{", "\"reader_staleness_bound_ms\":", "\"meta_kv_revalidate_gap_scans\":" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}

/// The holder's records in RAM (the backend's trees in the product).
const RamRecords = struct {
    gpa: Allocator,
    lock: jc.SpinLock = .{},
    inodes: std.AutoHashMapUnmanaged(u64, record.InodeValue) = .empty,
    xattrs: std.AutoHashMapUnmanaged(u64, std.ArrayListUnmanaged(Pair)) = .empty,
    dentries: std.AutoHashMapUnmanaged(u64, std.ArrayListUnmanaged(Dent)) = .empty,

    const Pair = struct { name: []const u8, value: []const u8 };
    const Dent = struct { cookie: u64, child: u64, name: []const u8 };

    fn deinit(self: *RamRecords) void {
        self.inodes.deinit(self.gpa);
        var xi = self.xattrs.valueIterator();
        while (xi.next()) |l| l.deinit(self.gpa);
        self.xattrs.deinit(self.gpa);
        var di = self.dentries.valueIterator();
        while (di.next()) |l| l.deinit(self.gpa);
        self.dentries.deinit(self.gpa);
    }

    fn putInode(self: *RamRecords, ino: u64, v: record.InodeValue) !void {
        self.lock.lock();
        defer self.lock.unlock();
        try self.inodes.put(self.gpa, ino, v);
    }

    fn removeInode(self: *RamRecords, ino: u64) void {
        self.lock.lock();
        defer self.lock.unlock();
        _ = self.inodes.remove(ino);
    }

    fn putXattr(self: *RamRecords, ino: u64, name: []const u8, value: []const u8) !void {
        self.lock.lock();
        defer self.lock.unlock();
        const gop = try self.xattrs.getOrPut(self.gpa, ino);
        if (!gop.found_existing) gop.value_ptr.* = .empty;
        for (gop.value_ptr.items) |*p| if (std.mem.eql(u8, p.name, name)) {
            p.value = value;
            return;
        };
        try gop.value_ptr.append(self.gpa, .{ .name = name, .value = value });
        std.mem.sort(Pair, gop.value_ptr.items, {}, struct {
            fn lt(_: void, a: Pair, b: Pair) bool {
                return std.mem.lessThan(u8, a.name, b.name);
            }
        }.lt);
    }

    fn putDentry(self: *RamRecords, dir: u64, cookie: u64, child: u64, name: []const u8) !void {
        self.lock.lock();
        defer self.lock.unlock();
        const gop = try self.dentries.getOrPut(self.gpa, dir);
        if (!gop.found_existing) gop.value_ptr.* = .empty;
        try gop.value_ptr.append(self.gpa, .{ .cookie = cookie, .child = child, .name = name });
        std.mem.sort(Dent, gop.value_ptr.items, {}, struct {
            fn lt(_: void, a: Dent, b: Dent) bool {
                return a.cookie < b.cookie;
            }
        }.lt);
    }

    fn inode(ctx: *anyopaque, object: u64) table.SourceError!?record.InodeValue {
        const self: *RamRecords = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        return self.inodes.get(object);
    }

    fn xattrsOf(ctx: *anyopaque, arena: Allocator, object: u64, out: *std.ArrayList(table.XattrRec)) table.SourceError!void {
        const self: *RamRecords = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        const l = self.xattrs.get(object) orelse return;
        for (l.items) |p| try out.append(arena, .{ .name = try arena.dupe(u8, p.name), .value = try arena.dupe(u8, p.value) });
    }

    fn dentriesOf(ctx: *anyopaque, arena: Allocator, object: u64, after: u64, max: usize, out: *std.ArrayList(table.DirRec)) table.SourceError!bool {
        const self: *RamRecords = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        const l = self.dentries.get(object) orelse return false;
        var n: usize = 0;
        for (l.items) |d| {
            if (d.cookie <= after) continue;
            if (n == max) return true;
            try out.append(arena, .{ .cookie = d.cookie, .child_ino = d.child, .file_type = 8, .name = try arena.dupe(u8, d.name) });
            n += 1;
        }
        return false;
    }

    fn source(self: *RamRecords) table.RecordSource {
        return .{ .ctx = self, .inode = &inode, .xattrs = &xattrsOf, .dentries = &dentriesOf };
    }
};

fn linkCall(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
    const c: *loopback.Conn = @ptrCast(@alignCast(ctx));
    return c.call(req);
}

fn linkOf(c: *loopback.Conn) cache.Link {
    return .{ .ctx = c, .call = &linkCall };
}

const SLOT: u16 = 5;
const A_ID: u64 = 0xA;
const A_APPENDER: u32 = 2;
const MAX_READERS = 8;

/// Slot `SLOT`'s guest key ino of local `local` (the objects A holds).
fn objectOf(local: u64) u64 {
    return route_table.effectiveIno(inos.makeGlobalIno(local, SLOT, inos.ROUTING_WIDTH));
}

/// B's resolver: slot `SLOT` is A's; everything else local; the dial is
/// the loopback session B → A.
const ReaderResolver = struct {
    conn_to_a: *loopback.Conn,
    refreshes: u32 = 0,

    fn homeOf(ctx: *anyopaque, slot: u16) divert.Home {
        _ = ctx;
        return if (slot == SLOT) .{ .foreign = .{ .holder = A_APPENDER, .identity = A_ID } } else .local;
    }
    fn refresh(ctx: *anyopaque, slot: u16, _: u32) divert.Home {
        const self: *ReaderResolver = @ptrCast(@alignCast(ctx));
        self.refreshes += 1;
        return homeOf(ctx, slot);
    }
    fn dial(ctx: *anyopaque, _: u64, _: u32) ?cache.Link {
        const self: *ReaderResolver = @ptrCast(@alignCast(ctx));
        return linkOf(self.conn_to_a);
    }
    fn resolver(self: *ReaderResolver) divert.Resolver {
        return .{ .ctx = self, .homeOf = &homeOf, .refresh = &refresh, .dial = &dial };
    }
};

/// A's resolver: it leases `SLOT`.
const HolderResolver = struct {
    /// A holds `SLOT` and slot 0 (the root's: the manager's own).
    fn homeOf(_: *anyopaque, slot: u16) divert.Home {
        return if (slot == SLOT or slot == 0) .local else .{ .foreign = .{ .holder = 9, .identity = 0x9 } };
    }
    fn refresh(_: *anyopaque, _: u16, _: u32) divert.Home {
        return .local;
    }
    fn dial(_: *anyopaque, _: u64, _: u32) ?cache.Link {
        return null;
    }
    var marker: u8 = 0;
    fn resolver() divert.Resolver {
        return .{ .ctx = &marker, .homeOf = &homeOf, .refresh = &refresh, .dial = &dial };
    }
};

/// A's view of its readers' listeners.
const ReaderPeers = struct {
    conns: [MAX_READERS]?*loopback.Conn = @splat(null),
    /// A reader the holder cannot reach (its endpoint unbound).
    unreachable_: [MAX_READERS]bool = @splat(false),

    fn linkFor(ctx: *anyopaque, client: u64) ?cache.Link {
        const self: *ReaderPeers = @ptrCast(@alignCast(ctx));
        const i: usize = @intCast(client - 1);
        if (i >= MAX_READERS or self.unreachable_[i]) return null;
        return linkOf(self.conns[i] orelse return null);
    }
    fn peers(self: *ReaderPeers) divert.Peers {
        return .{ .ctx = self, .linkFor = &linkFor };
    }
};

/// A reader's data-plane sink that records what the recall asked of it.
const RecordingSink = struct {
    discards: u32 = 0,
    census: u32 = 0,
    noted: u32 = 0,
    /// The object whose layout entry is DIRTY (the writer's acked bytes).
    dirty_object: u64 = 0,
    keys_per_entry: u32 = 3,

    fn discardLayout(ctx: *anyopaque, object: u64) cache.DiscardVerdict {
        const self: *RecordingSink = @ptrCast(@alignCast(ctx));
        if (object == self.dirty_object) return .dirty_kept;
        self.discards += 1;
        return .{ .discarded = self.keys_per_entry };
    }
    fn purgeCensus(ctx: *anyopaque) void {
        const self: *RecordingSink = @ptrCast(@alignCast(ctx));
        self.census += 1;
    }
    fn noteRecalled(ctx: *anyopaque, _: u64) void {
        const self: *RecordingSink = @ptrCast(@alignCast(ctx));
        self.noted += 1;
    }
    fn sink(self: *RecordingSink) cache.RecallSink {
        return .{ .ctx = self, .discardLayout = &discardLayout, .purgeCensus = &purgeCensus, .noteRecalled = &noteRecalled };
    }
};

const GrantingCustody = struct {
    acquired: u32 = 0,
    released: u32 = 0,
    fn acquire(ctx: *anyopaque, _: u64, _: u64, _: u64, _: u64, _: bool, _: u64) divert.CustodyVerdict {
        const self: *GrantingCustody = @ptrCast(@alignCast(ctx));
        self.acquired += 1;
        return .{ .granted = .{ .epoch = 7 } };
    }
    fn release(ctx: *anyopaque, _: u64, _: u64, _: u64) void {
        const self: *GrantingCustody = @ptrCast(@alignCast(ctx));
        self.released += 1;
    }
    fn seam(self: *GrantingCustody) divert.CustodySeam {
        return .{ .ctx = self, .acquire = &acquire, .release = &release };
    }
};

/// Two members over the hub: A (daemon 0) the holder of `SLOT`, readers
/// B1..Bn (daemons 1..n) each with its own divert and sessions both ways.
const TwoMembers = struct {
    gpa: Allocator,
    io: Io,
    hub: loopback.LoopbackHub,
    services: [1 + MAX_READERS]service.Service = @splat(.init),
    records: RamRecords,
    leases: ro.OwnerLeases,
    holder: table.Holder,
    holder_svc: divert.HolderService,
    custody: GrantingCustody = .{},
    peers: ReaderPeers = .{},
    pusher: divert.LinkPusher,
    n_readers: usize,
    // Per reader: the two sessions, the resolver, the divert, its service.
    a_to_b: [MAX_READERS]loopback.Conn = undefined,
    b_to_a: [MAX_READERS]loopback.Conn = undefined,
    resolvers: [MAX_READERS]ReaderResolver = undefined,
    sinks: [MAX_READERS]RecordingSink = @splat(.{}),
    member_leases: [MAX_READERS]ro.MemberLease = @splat(.{}),
    diverts: [MAX_READERS]divert.Divert = undefined,
    reader_svcs: [MAX_READERS]divert.ReaderService = undefined,

    fn identityOf(n: u64) verbs.WireIdentity {
        return .{ .node_token = 0x1000 + n, .writer_id = 0x2000 + n, .mount_slot = @intCast(n), .reserved = 0 };
    }

    fn init(gpa: Allocator, io: Io, n_readers: usize) !*TwoMembers {
        const self = try gpa.create(TwoMembers);
        self.* = .{
            .gpa = gpa,
            .io = io,
            .hub = loopback.LoopbackHub.init(gpa, io),
            .records = .{ .gpa = gpa },
            .leases = ro.OwnerLeases.init(gpa),
            .holder = undefined,
            .holder_svc = undefined,
            .pusher = undefined,
            .n_readers = n_readers,
        };
        const secret: auth.Secret = @splat(0x5A);
        self.holder = try table.Holder.init(gpa, io, A_ID, self.leases.oracle(), 1024, 64);
        self.holder_svc = .{ .io = io, .holder = &self.holder, .source = self.records.source(), .resolver = HolderResolver.resolver(), .custody = self.custody.seam() };
        self.holder_svc.register(&self.services[0]);
        self.pusher = .{ .volume = 0, .identity = A_ID, .peers = self.peers.peers() };
        _ = try self.hub.addDaemon(&self.services[0], secret, identityOf(0));
        const now = clock.monotonicNs();
        for (0..n_readers) |i| {
            _ = try self.hub.addDaemon(&self.services[i + 1], secret, identityOf(i + 1));
            self.a_to_b[i] = try self.hub.connect(0, i + 1);
            self.b_to_a[i] = try self.hub.connect(i + 1, 0);
            self.peers.conns[i] = &self.a_to_b[i];
            self.resolvers[i] = .{ .conn_to_a = &self.b_to_a[i] };
            self.member_leases[i].renew(now, table.T_OWNER_NS);
            try self.leases.grant(@intCast(i + 1), now);
            self.diverts[i] = divert.Divert.init(gpa, io, self.resolvers[i].resolver(), @intCast(i + 1), 0, self.sinks[i].sink(), self.member_leases[i].selfLease(), cache.recordsBudgetBytes(0));
            self.reader_svcs[i] = .{ .io = io, .divert = &self.diverts[i] };
            self.reader_svcs[i].register(&self.services[i + 1]);
        }
        // A's objects: a directory with two children, a file with two
        // carried xattrs and one control xattr.
        try self.records.putInode(objectOf(2), .{ .mode = 0o040755, .nlink = 2, .mtime = 100 });
        try self.records.putInode(objectOf(3), .{ .mode = 0o100644, .nlink = 1, .size = 10, .mtime = 100 });
        try self.records.putInode(objectOf(4), .{ .mode = 0o100644, .nlink = 1, .size = 20, .mtime = 100 });
        try self.records.putDentry(objectOf(2), key.encodeReaddirCookie(0x10, 0), inos.makeGlobalIno(3, SLOT, inos.ROUTING_WIDTH), "three");
        try self.records.putDentry(objectOf(2), key.encodeReaddirCookie(0x20, 0), inos.makeGlobalIno(4, SLOT, inos.ROUTING_WIDTH), "four");
        try self.records.putXattr(objectOf(3), "user.b", "bee");
        try self.records.putXattr(objectOf(3), "user.a", "ay");
        try self.records.putXattr(objectOf(3), "user.squeezefs.gather", "1");
        return self;
    }

    fn deinit(self: *TwoMembers) void {
        for (0..self.n_readers) |i| {
            self.diverts[i].deinit();
            self.a_to_b[i].deinit();
            self.b_to_a[i].deinit();
        }
        self.holder.deinit();
        self.leases.deinit();
        self.records.deinit();
        self.hub.deinit();
        self.gpa.destroy(self);
    }

    fn reader(self: *TwoMembers, i: usize) *divert.Divert {
        return &self.diverts[i];
    }

    /// The holder's commit of `objects`: the A1½ recall, the apply, the
    /// settle at the terminal outcome.
    fn commit(self: *TwoMembers, objects: []const u64, apply: *const fn (*TwoMembers) anyerror!void) !table.RecallSummary {
        const s = try self.holder.recallAndWait(self.io, objects, self.pusher.pusher());
        try apply(self);
        self.holder.settle(self.io, objects);
        return s;
    }

    /// The per-holder closure: every grant served is released, recalled,
    /// expired or still outstanding.
    fn closureHolds(self: *TwoMembers, grants_before: u64, recalled_before: u64, releases_before: u64, expired_before: u64) !void {
        const served = table.stats.dlm_token_grants_served.load() - grants_before;
        const recalled = table.stats.dlm_token_recalls.load() - recalled_before;
        const released = table.stats.dlm_token_releases.load() - releases_before;
        const expired = table.stats.dlm_token_recall_expired_with_lease.load() - expired_before;
        _ = expired;
        try testing.expectEqual(served - recalled - released, self.holder.outstandingGrants(self.io));
    }
};

fn bumpMtime(f: *TwoMembers) anyerror!void {
    try f.records.putInode(objectOf(3), .{ .mode = 0o100644, .nlink = 1, .size = 11, .mtime = 200 });
}

test "grant → cached read 0 RPCs → the holder's commit recalls → the reader acks → the write proceeds → the reader re-grants and sees the new value; a voluntary release; the closure" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    const b = f.reader(0);
    const grants0 = table.stats.dlm_token_grants_served.load();
    const recalls0 = table.stats.dlm_token_recalls.load();
    const releases0 = table.stats.dlm_token_releases.load();
    const expired0 = table.stats.dlm_token_recall_expired_with_lease.load();
    const rgrants0 = cache.stats.dlm_token_grants.load();
    // First touch: 1 RPC; the entry carries the folded inode and the
    // CARRIED xattrs in name order — never the control one.
    var s = (try b.serve(io, objectOf(3), .{})).?;
    try testing.expect(!s.gone());
    try testing.expectEqual(@as(u64, 100), s.entry.?.attrs.mtime);
    try testing.expectEqual(@as(usize, 2), s.entry.?.xattrs.len);
    try testing.expectEqualStrings("user.a", s.entry.?.xattrs[0].name);
    try testing.expectEqualStrings("bee", s.entry.?.xattr("user.b").?);
    try testing.expect(s.entry.?.xattr("user.squeezefs.gather") == null);
    s.release(io);
    try testing.expectEqual(rgrants0 + 1, cache.stats.dlm_token_grants.load());
    // Cached: 0 RPCs, 0 device reads.
    const hits0 = cache.stats.dlm_token_hits.load();
    s = (try b.serve(io, objectOf(3), .{})).?;
    s.release(io);
    try testing.expectEqual(rgrants0 + 1, cache.stats.dlm_token_grants.load());
    try testing.expectEqual(hits0 + 1, cache.stats.dlm_token_hits.load());
    try testing.expect(f.holder.holdsGrant(io, objectOf(3), 1));
    // The holder commits a setattr: the recall reaches B before the apply,
    // B acks inline (nothing in flight), the write proceeds.
    const batches0 = table.stats.dlm_token_recall_batches.load();
    const sum = try f.commit(&.{objectOf(3)}, &bumpMtime);
    try testing.expectEqual(@as(u32, 1), sum.recalled);
    try testing.expectEqual(@as(u32, 1), sum.acked);
    try testing.expectEqual(@as(u32, 0), sum.expired_with_lease + sum.timeouts_live);
    try testing.expectEqual(batches0 + 1, table.stats.dlm_token_recall_batches.load());
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 1));
    try testing.expectEqual(@as(u32, 1), f.sinks[0].noted);
    try testing.expectEqual(@as(usize, 0), b.planeOf(io, A_ID).?.cache.count(io));
    // B's very next read is fresh (exact, never bounded — the `-o ro`
    // reader's staleness bound is 0).
    s = (try b.serve(io, objectOf(3), .{})).?;
    try testing.expectEqual(@as(u64, 200), s.entry.?.attrs.mtime);
    try testing.expectEqual(@as(u64, 11), s.entry.?.attrs.size);
    s.release(io);
    try testing.expectEqual(rgrants0 + 2, cache.stats.dlm_token_grants.load());
    // A voluntary release drains, purges, then tells the holder.
    b.planeOf(io, A_ID).?.release(io, objectOf(3));
    try testing.expectEqual(releases0 + 1, table.stats.dlm_token_releases.load());
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 1));
    try f.closureHolds(grants0, recalls0, releases0, expired0);
    // The exact-sum law per batch: send + drain + ack ≡ total.
    const send = table.stats.dlm_token_recall_rtt_send.snapshot().sum_ns;
    const drain = table.stats.dlm_token_recall_rtt_drain.snapshot().sum_ns;
    const ack = table.stats.dlm_token_recall_rtt_ack.snapshot().sum_ns;
    try testing.expectEqual(table.stats.dlm_token_recall_rtt_total.snapshot().sum_ns, send + drain + ack);
}

test "a directory's token carries its dentry set paged by cookie: lookup by name, a readdir page after a cookie, and a dentry-bearing entry survives a bare re-fetch" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    const b = f.reader(0);
    var s = (try b.serve(io, objectOf(2), .{ .dentries = true })).?;
    const e = s.entry.?;
    try testing.expectEqual(@as(usize, 2), e.dir.?.len);
    try testing.expectEqual(inos.makeGlobalIno(4, SLOT, inos.ROUTING_WIDTH), e.lookupName("four").?.child_ino);
    try testing.expectEqual(@as(usize, 1), e.pageAfter(key.encodeReaddirCookie(0x10, 0)).len);
    s.release(io);
    // A bare serve of the same object is a hit on the richer entry.
    const g0 = cache.stats.dlm_token_grants.load();
    s = (try b.serve(io, objectOf(2), .{})).?;
    try testing.expect(s.entry.?.dir != null);
    s.release(io);
    try testing.expectEqual(g0, cache.stats.dlm_token_grants.load());
    // Gone at its holder: served as gone, nothing cached, nothing granted.
    s = (try b.serve(io, objectOf(99), .{})).?;
    try testing.expect(s.gone());
    s.release(io);
    try testing.expect(!f.holder.holdsGrant(io, objectOf(99), 1));
}

test "the paged-resident dentry set (rung 10 item 5c, spec Q13): a directory larger than the reader's window is never refused — the entry holds one cookie WINDOW from the serve's `after`, a readdir pages window after window (each a fetch, dir_complete false until the last), a lookup by name fetches the window around the name's cookies and judges absence only inside it (a cut mid-run re-serves from the window's end); a cached window is a hit; oversize refusals stay 0" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    // A's directory 5 with 5,000 children (several wire pages) whose
    // cookies ARE their names' hashes (the filesystem's law), under the
    // fixture's name key.
    const name_key = key.NameKey.init(0xABCD);
    const n: usize = 5000;
    const names = try gpa.alloc([5]u8, n);
    defer gpa.free(names);
    for (names, 0..) |*nm, i| _ = std.fmt.bufPrint(nm, "n{d:0>4}", .{i}) catch unreachable;
    try f.records.putInode(objectOf(5), .{ .mode = 0o040755, .nlink = 2, .mtime = 100 });
    for (names, 0..) |*nm, i| try f.records.putDentry(objectOf(5), key.encodeReaddirCookie(key.dentryNameHash54(nm, &name_key), 0), inos.makeGlobalIno(100 + i, SLOT, inos.ROUTING_WIDTH), nm);
    const b = f.reader(0);
    // Bind the plane, then shrink its window to ~1,500 records (a wire
    // page carries ~2,000: the cut lands at a page boundary).
    var s0 = (try b.serve(io, objectOf(3), .{})).?;
    s0.release(io);
    const plane = b.planeOf(io, A_ID).?;
    plane.dir_window_bytes = table.TOKEN_ENTRY_BYTES + 1500 * (table.DIR_RECORD_BYTES + 5);
    const oversize0 = cache.stats.dlm_token_oversize_refusals.load();
    const windows0 = cache.stats.dlm_token_dir_windows.load();
    // The readdir walk: windows until complete, every record once, in order.
    var seen: usize = 0;
    var after: u64 = 0;
    var windows: u32 = 0;
    var last_cookie: u64 = 0;
    while (true) {
        var s = (try b.serve(io, objectOf(5), .{ .dentries = true, .after = after })).?;
        defer s.release(io);
        const e = s.entry.?;
        try testing.expect(e.coversAfter(after));
        const page = e.pageAfter(after);
        for (page) |r| {
            try testing.expect(r.cookie > last_cookie);
            last_cookie = r.cookie;
            seen += 1;
        }
        windows += 1;
        if (e.dir_complete) break;
        try testing.expect(page.len >= 1);
        after = e.lastCookie();
    }
    try testing.expectEqual(n, seen);
    try testing.expect(windows >= 2);
    try testing.expectEqual(windows0 + windows - 1, cache.stats.dlm_token_dir_windows.load());
    // A lookup by name: the window around its cookies, found; a second
    // lookup inside the same window is a hit (no fetch).
    const h35 = key.dentryNameHash54("n3500", &name_key);
    var s = (try b.serve(io, objectOf(5), cache.Wants.forName(h35))).?;
    try testing.expectEqual(inos.makeGlobalIno(3600, SLOT, inos.ROUTING_WIDTH), s.entry.?.lookupName("n3500").?.child_ino);
    try testing.expect(s.entry.?.coversName(h35));
    s.release(io);
    const g0 = cache.stats.dlm_token_grants.load();
    s = (try b.serve(io, objectOf(5), cache.Wants.forName(h35))).?;
    s.release(io);
    try testing.expectEqual(g0, cache.stats.dlm_token_grants.load());
    // An absent name: null, and the window judges the absence.
    const hzz = key.dentryNameHash54("zz", &name_key);
    s = (try b.serve(io, objectOf(5), cache.Wants.forName(hzz))).?;
    try testing.expect(s.entry.?.lookupName("zz") == null);
    try testing.expect(s.entry.?.coversName(hzz) or s.entry.?.lastCookie() < key.encodeReaddirCookie(hzz, 0));
    s.release(io);
    try testing.expectEqual(oversize0, cache.stats.dlm_token_oversize_refusals.load());
}

/// A recall pushed while a DMA read is in flight on the reader.
const InFlightRig = struct {
    f: *TwoMembers,
    summary: table.RecallSummary = .{},
    fn run(self: *InFlightRig) void {
        self.summary = self.f.commit(&.{objectOf(3)}, &bumpMtime) catch unreachable;
    }
};

test "a recall during an in-flight DMA read is acked only after it lands: the push is DEFERRED, the holder waits, the drain lane acks after the last serve ends" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    const b = f.reader(0);
    var s = (try b.serve(io, objectOf(3), .{})).?;
    s.release(io);
    const plane = b.planeOf(io, A_ID).?;
    // A read serve in flight on the ino (its stamp held).
    const stamp = plane.stamps.begin();
    var rig: InFlightRig = .{ .f = f };
    const t = try std.Thread.spawn(.{}, InFlightRig.run, .{&rig});
    // The push arrives, the entry is revoked, the frame is deferred: the
    // holder's grant stands under recall until the ack.
    var spins: u32 = 0;
    while (plane.deferredCount(io) == 0) : (spins += 1) {
        try testing.expect(spins < 1_000_000);
        std.Thread.yield() catch {};
    }
    try testing.expect(f.holder.holdsGrant(io, objectOf(3), 1));
    try testing.expectEqual(@as(usize, 0), plane.cache.count(io));
    // The DMA lands: the drain completes, the lane purges and acks.
    plane.stamps.end(io, stamp);
    const deferred0 = cache.stats.dlm_token_recall_acks_deferred.load();
    try testing.expectEqual(@as(u32, 1), plane.pumpDeferred(io));
    try testing.expectEqual(deferred0 + 1, cache.stats.dlm_token_recall_acks_deferred.load());
    t.join();
    try testing.expectEqual(@as(u32, 1), rig.summary.recalled);
    try testing.expectEqual(@as(u32, 1), rig.summary.acked);
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 1));
    try testing.expectEqual(@as(u32, 1), f.sinks[0].discards);
    s = (try b.serve(io, objectOf(3), .{})).?;
    try testing.expectEqual(@as(u64, 200), s.entry.?.attrs.mtime);
    s.release(io);
}

test "a DIRTY local layout entry survives a recall: the writer's acked bytes are kept, only clean entries are discarded, the ack still travels" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    const b = f.reader(0);
    f.sinks[0].dirty_object = objectOf(3);
    var s = (try b.serve(io, objectOf(3), .{})).?;
    s.release(io);
    s = (try b.serve(io, objectOf(4), .{})).?;
    s.release(io);
    const kept0 = cache.stats.dlm_token_recall_dirty_kept.load();
    const purged0 = cache.stats.dlm_token_recall_purged_keys.load();
    const sum = try f.commit(&.{ objectOf(3), objectOf(4) }, &bumpMtime);
    try testing.expectEqual(@as(u32, 2), sum.recalled);
    try testing.expectEqual(@as(u32, 2), sum.acked);
    try testing.expectEqual(kept0 + 1, cache.stats.dlm_token_recall_dirty_kept.load());
    try testing.expectEqual(purged0 + 3, cache.stats.dlm_token_recall_purged_keys.load());
    try testing.expectEqual(@as(u32, 1), f.sinks[0].discards);
}

/// A first-touch grant landing inside the pass's window.
const ParkedGrantRig = struct {
    b: *divert.Divert,
    io: Io,
    mtime: u64 = 0,
    fn run(self: *ParkedGrantRig) void {
        const s = (self.b.serve(self.io, objectOf(4), .{}) catch unreachable).?;
        self.mtime = s.entry.?.attrs.mtime;
        s.release(self.io);
    }
};

fn bumpFour(f: *TwoMembers) anyerror!void {
    try f.records.putInode(objectOf(4), .{ .mode = 0o100644, .nlink = 1, .size = 21, .mtime = 300 });
}

const HoldingCommitRig = struct {
    f: *TwoMembers,
    summary: table.RecallSummary = .{},
    fn run(self: *HoldingCommitRig) void {
        // The recall (nobody holds object 4 yet), then the seam's hold
        // BEFORE the apply: the window a first touch must park in.
        self.summary = self.f.holder.recallAndWait(self.f.io, &.{objectOf(4)}, self.f.pusher.pusher()) catch unreachable;
    }
};

test "a first-touch grant inside the pass window PARKS to the settle and serves the committed records (register-then-check, the seam's hold)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    f.holder.seams.conveyor_hold_post_recall = true;
    var commit_rig: HoldingCommitRig = .{ .f = f };
    const tc = try std.Thread.spawn(.{}, HoldingCommitRig.run, .{&commit_rig});
    var spins: u32 = 0;
    while (!f.holder.isHolding()) : (spins += 1) {
        try testing.expect(spins < 1_000_000);
        std.Thread.yield() catch {};
    }
    // The object is in flight: B's first touch parks on the gate.
    const parks0 = table.stats.dlm_token_grant_parks.load();
    var grant_rig: ParkedGrantRig = .{ .b = f.reader(0), .io = io };
    const tg = try std.Thread.spawn(.{}, ParkedGrantRig.run, .{&grant_rig});
    spins = 0;
    while (table.stats.dlm_token_grant_parks.load() == parks0) : (spins += 1) {
        try testing.expect(spins < 1_000_000);
        std.Thread.yield() catch {};
    }
    try testing.expect(f.holder.holdsGrant(io, objectOf(4), 1));
    // The apply lands, the pass proceeds, the window settles: the parked
    // grant wakes and reads the COMMITTED records.
    try bumpFour(f);
    f.holder.releaseHold(io);
    tc.join();
    f.holder.settle(io, &.{objectOf(4)});
    tg.join();
    try testing.expectEqual(@as(u64, 300), grant_rig.mtime);
    try testing.expectEqual(@as(u32, 0), commit_rig.summary.recalled);
}

test "an unacked recall completes at the reader's lease expiry (expired_with_lease, timeouts_live 0); a departed member's grants are swept at the owner's word" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 2);
    defer f.deinit();
    var s = (try f.reader(0).serve(io, objectOf(3), .{})).?;
    s.release(io);
    s = (try f.reader(1).serve(io, objectOf(3), .{})).?;
    s.release(io);
    // B1 is unreachable and its lease is 20 ms from expiry: the pass waits
    // for nobody past that; B2 acks inline.
    f.peers.unreachable_[0] = true;
    // Saturating: a venue booted under `T_OWNER` ago has a monotonic clock
    // below it (the fixture's grant time is then the epoch + 20 ms).
    try f.leases.grant(1, (clock.monotonicNs() -| table.T_OWNER_NS) + 20 * std.time.ns_per_ms);
    const live0 = table.stats.dlm_token_recall_timeouts_live.load();
    const t0 = clock.monotonicNs();
    const sum = try f.commit(&.{objectOf(3)}, &bumpMtime);
    const waited = clock.monotonicNs() - t0;
    try testing.expectEqual(@as(u32, 2), sum.recalled);
    try testing.expectEqual(@as(u32, 1), sum.expired_with_lease);
    try testing.expectEqual(@as(u32, 1), sum.acked);
    try testing.expectEqual(live0, table.stats.dlm_token_recall_timeouts_live.load());
    try testing.expect(waited < table.T_OWNER_NS / 2);
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 1));
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 2));
    // The departure sweep: B2 holds object 4; the owner evicts it.
    s = (try f.reader(1).serve(io, objectOf(4), .{})).?;
    s.release(io);
    const swept0 = table.stats.dlm_token_lease_swept_grants.load();
    f.leases.remove(2);
    f.holder.noteMemberDeparted(io, 2);
    try testing.expectEqual(swept0 + 1, table.stats.dlm_token_lease_swept_grants.load());
    try testing.expectEqual(@as(u64, 0), f.holder.outstandingGrants(io));
}

/// The joiner's lease face as `wire_plane.LeaseOracle` binds it: every
/// member but the manager is judged by the owner's census as the beat
/// last pulled it.
const CensusOracle = struct {
    view: census_view.CensusView,

    fn oracle(self: *CensusOracle) table.LeaseOracle {
        return .{ .ctx = self, .verdict = &verdictFn, .remaining = &noRemaining };
    }
    fn verdictFn(ctx: ?*anyopaque, client: table.ClientId, since_ns: u64, now_ns: u64) table.LeaseVerdict {
        const self: *CensusOracle = @ptrCast(@alignCast(ctx.?));
        return self.view.verdict(client, since_ns, now_ns);
    }
    fn noRemaining(_: ?*anyopaque, _: table.ClientId, _: u64) ?u64 {
        return null;
    }
};

/// A pass on its own thread (the conveyor's A1½ under a served step).
const PassThread = struct {
    f: *TwoMembers,
    object: u64,
    sum: ?table.RecallSummary = null,
    waited_ns: u64 = 0,
    fn run(self: *PassThread) void {
        const t0 = clock.monotonicNs();
        self.sum = self.f.commit(&.{self.object}, &bumpMtime) catch null;
        self.waited_ns = clock.monotonicNs() - t0;
    }
};

test "a joiner's holder and a reader the owner evicted (found by the shared hot directory row: C's served step parked 45 s on a recall to the killed B): the census pulled after the reader's last grant no longer lists it ⇒ the pass sweeps its grant before recalling anyone (lease_swept, timeouts_live 0), nothing waits the deadline; a recall ALREADY waiting completes at the census commit's sweep, not at the pass's next expiry tick; an asking client a stale census does not list is never refused" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 2);
    defer f.deinit();
    var co: CensusOracle = .{ .view = census_view.CensusView.init(gpa, io) };
    defer co.view.deinit();
    // A view that answers for the whole test (two renew intervals).
    co.view.setRenewInterval(table.T_OWNER_NS);
    f.holder.oracle = co.oracle();
    // The deadline a corpse used to cost, shortened so the red run ends.
    f.holder.seams.recall_deadline_ns = 4 * std.time.ns_per_s;
    // The view before the grants lists both readers.
    try co.view.stage(1, .live);
    try co.view.stage(2, .live);
    co.view.commit(false, clock.monotonicNs());
    var s = (try f.reader(0).serve(io, objectOf(3), .{})).?;
    s.release(io);
    // B1 dies: unreachable, and the next census (newer than its grant,
    // the owner's grace window closed) no longer lists it.
    f.peers.unreachable_[0] = true;
    try co.view.stage(2, .live);
    co.view.commit(false, clock.monotonicNs() + 1);
    const live0 = table.stats.dlm_token_recall_timeouts_live.load();
    const swept0 = table.stats.dlm_token_lease_swept_grants.load();
    const t0 = clock.monotonicNs();
    const sum = try f.commit(&.{objectOf(3)}, &bumpMtime);
    const waited = clock.monotonicNs() - t0;
    // The pass's A1½ sweeps the corpse's grant before it recalls anyone.
    try testing.expectEqual(@as(u32, 0), sum.recalled);
    try testing.expectEqual(swept0 + 1, table.stats.dlm_token_lease_swept_grants.load());
    try testing.expectEqual(live0, table.stats.dlm_token_recall_timeouts_live.load());
    try testing.expect(waited < f.holder.recallDeadlineNs() / 2);
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 1));
    // A reader the census has never listed (it joined after the pull)
    // asks: in contact now, so the stale view's silence is no refusal.
    s = (try f.reader(0).serve(io, objectOf(4), .{})).?;
    s.release(io);
    try testing.expect(f.holder.holdsGrant(io, objectOf(4), 1));
    // B2 holds object 3, then dies while a pass recalls it: the pass waits
    // (the census still lists B2), the next census drops it, and the
    // commit's sweep wakes the pass — far inside the 45 s deadline's
    // 11.25 s tick.
    f.holder.seams.recall_deadline_ns = 0;
    s = (try f.reader(1).serve(io, objectOf(3), .{})).?;
    s.release(io);
    f.peers.unreachable_[1] = true;
    const batches0 = table.stats.dlm_token_recall_batches.load();
    var pass: PassThread = .{ .f = f, .object = objectOf(3) };
    const t = try std.Thread.spawn(.{}, PassThread.run, .{&pass});
    var spins: u32 = 0;
    while (table.stats.dlm_token_recall_batches.load() == batches0) : (spins += 1) {
        try testing.expect(spins < 10_000_000);
        std.Thread.yield() catch {};
    }
    try testing.expectEqual(@as(u32, 0), f.holder.sweepExpired(io, clock.monotonicNs()));
    co.view.commit(false, clock.monotonicNs() + 1);
    try testing.expect(f.holder.sweepExpired(io, clock.monotonicNs()) >= 1);
    t.join();
    try testing.expectEqual(@as(u32, 1), pass.sum.?.expired_with_lease);
    try testing.expect(pass.waited_ns < table.expiryTickNs(table.RECALL_DEADLINE_NS) / 2);
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 2));
    try testing.expectEqual(live0, table.stats.dlm_token_recall_timeouts_live.load());
}

test "a recall unacked by a LIVE member past the deadline is the must-stay-0 (dlm_token_recall_timeouts_live fires once); the token is retired and the commit proceeds" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    var s = (try f.reader(0).serve(io, objectOf(3), .{})).?;
    s.release(io);
    f.peers.unreachable_[0] = true;
    // The seam: the deadline at 20 ms instead of T_owner.
    f.holder.seams.recall_deadline_ns = 20 * std.time.ns_per_ms;
    const live0 = table.stats.dlm_token_recall_timeouts_live.load();
    table.stats.dlm_token_recall_timeouts_live.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    const sum = try f.commit(&.{objectOf(3)}, &bumpMtime);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u32, 1), sum.timeouts_live);
    try testing.expectEqual(live0 + 1, table.stats.dlm_token_recall_timeouts_live.load());
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 1));
}

test "the broadcast shape: every reader recalled ONCE per publish, the fan-out recorded, 16 co-queued commits on one object are ONE batch, the exact-sum pinned" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, MAX_READERS);
    defer f.deinit();
    for (0..MAX_READERS) |i| {
        const s = (try f.reader(i).serve(io, objectOf(3), .{})).?;
        s.release(io);
    }
    try testing.expectEqual(@as(u32, MAX_READERS), f.holder.grantsOf(io, objectOf(3)));
    // 16 co-queued txs on the object: the union is ONE object, the pass
    // recalls it ONCE.
    var u: table.ObjectUnion = .{};
    defer u.deinit(gpa);
    var txs: [16]@import("../tx.zig").KvTx = undefined;
    for (&txs) |*t| {
        t.* = @import("../tx.zig").KvTx.init(gpa, @src());
        try t.stagePut(key.TREE_INODES, &key.inodeKey(objectOf(3)), "v");
    }
    var qs: [16]@import("../tx.zig").QueuedTx = undefined;
    for (&qs, &txs) |*q, *t| q.* = try @import("../tx.zig").buildQueuedTx(t, 65536, null);
    defer for (&qs) |*q| q.deinit();
    for (&qs) |*q| try u.addMember(gpa, q);
    const objects = u.finish();
    try testing.expectEqual(@as(usize, 1), objects.len);
    const batches0 = table.stats.dlm_token_recall_batches.load();
    const fan0 = table.stats.dlm_token_recall_fanout[table.fanoutBucket(MAX_READERS)].load();
    const sum = try f.commit(objects, &bumpMtime);
    try testing.expectEqual(@as(u32, MAX_READERS), sum.recalled);
    try testing.expectEqual(@as(u32, MAX_READERS), sum.acked);
    try testing.expectEqual(@as(u32, MAX_READERS), sum.frames);
    try testing.expectEqual(batches0 + 1, table.stats.dlm_token_recall_batches.load());
    try testing.expectEqual(fan0 + 1, table.stats.dlm_token_recall_fanout[table.fanoutBucket(MAX_READERS)].load());
    try testing.expectEqual(@as(u64, 0), f.holder.outstandingGrants(io));
    const send = table.stats.dlm_token_recall_rtt_send.snapshot().sum_ns;
    const drain = table.stats.dlm_token_recall_rtt_drain.snapshot().sum_ns;
    const ack = table.stats.dlm_token_recall_rtt_ack.snapshot().sum_ns;
    try testing.expectEqual(table.stats.dlm_token_recall_rtt_total.snapshot().sum_ns, send + drain + ack);
    for (0..MAX_READERS) |i| {
        const s = (try f.reader(i).serve(io, objectOf(3), .{})).?;
        try testing.expectEqual(@as(u64, 200), s.entry.?.attrs.mtime);
        s.release(io);
    }
}

test "CustodyGrant: ONE round trip carries the custody epoch and the file's read token; a directory is never a custody object (rejected, the tripwire); the ghost member is refused NotMember before it registers" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    const c = &f.b_to_a[0];
    const rep = try c.call(.{ .custody_grant = .{ .volume = 0, .concurrent_write = 0, .reserved = 0, .wait_ms = 0, .client_id = 1, .object = objectOf(3), .span_start = 0, .span_len = 1 << 20, .lease_epoch = 0, .reserved2 = 0 } });
    try testing.expectEqual(@as(u8, @backingInt(verbs.CustodyKind.granted)), rep.custody_grant.head.kind);
    try testing.expectEqual(@as(u64, 7), rep.custody_grant.head.grant_epoch);
    const page = rep.custody_grant.page().?;
    try testing.expectEqual(@as(u32, 2), page.head.n_xattrs);
    try testing.expect(f.holder.holdsGrant(io, objectOf(3), 1));
    try testing.expectEqual(@as(u32, 1), f.custody.acquired);
    // A directory: rejected, counted on the must-stay-0.
    const rejected0 = table.stats.dlm_token_custody_rejected.load();
    table.stats.dlm_token_custody_rejected.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Rejected, c.call(.{ .custody_grant = .{ .volume = 0, .concurrent_write = 0, .reserved = 0, .wait_ms = 0, .client_id = 1, .object = objectOf(2), .span_start = 0, .span_len = 1, .lease_epoch = 0, .reserved2 = 0 } }));
    try log.testing.expectAllFired();
    try testing.expectEqual(rejected0 + 1, table.stats.dlm_token_custody_rejected.load());
    // A member the owner does not list: NotMember, nothing registered.
    const ghost0 = table.stats.dlm_token_nonmember_refusals.load();
    table.stats.dlm_token_nonmember_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.MembershipPending, c.call(.{ .token_grant = .{ .head = .{ .volume = 0, .wants = verbs.WANT_RECORDS, .mode = 0, .xattr_after_len = 0, .client_id = 77, .object = objectOf(3), .after = 0 }, .bytes = "" } }));
    try log.testing.expectAllFired();
    try testing.expectEqual(ghost0 + 1, table.stats.dlm_token_nonmember_refusals.load());
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 77));
}

test "a grant on a slot another appender leases is answered NotHolder and the reader follows the redirect ONCE inside the op" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    // An object of slot 7 — A does not lease it.
    const foreign = route_table.effectiveIno(inos.makeGlobalIno(3, 7, inos.ROUTING_WIDTH));
    const c = &f.b_to_a[0];
    const redirects0 = table.stats.dlm_token_not_holder_redirects.load();
    const rep = try c.call(.{ .token_grant = .{ .head = .{ .volume = 0, .wants = verbs.WANT_RECORDS, .mode = 0, .xattr_after_len = 0, .client_id = 1, .object = foreign, .after = 0 }, .bytes = "" } });
    try testing.expectEqual(@as(u8, @backingInt(verbs.GrantPageKind.not_holder)), rep.token_grant.head.kind);
    try testing.expectEqual(@as(u32, 9), rep.token_grant.head.holder);
    try testing.expectEqual(redirects0 + 1, table.stats.dlm_token_not_holder_redirects.load());
    // Through the divert: B's resolver says slot 7 is local, so the divert
    // reads it locally — the redirect arm engages only when the view and
    // the holder disagree: force that view.
    const Disagree = struct {
        inner: *ReaderResolver,
        fn homeOf(_: *anyopaque, _: u16) divert.Home {
            return .{ .foreign = .{ .holder = A_APPENDER, .identity = A_ID } };
        }
        fn refresh(ctx: *anyopaque, _: u16, current: u32) divert.Home {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.inner.refreshes += 1;
            // The manager's word: slot 7's lessee is appender 9 — unbound.
            return .{ .unreachable_ = .{ .holder = current } };
        }
        fn dial(ctx: *anyopaque, id: u64, h: u32) ?cache.Link {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            return ReaderResolver.dial(self.inner, id, h);
        }
    };
    var dis: Disagree = .{ .inner = &f.resolvers[0] };
    var d = divert.Divert.init(gpa, io, .{ .ctx = &dis, .homeOf = &Disagree.homeOf, .refresh = &Disagree.refresh, .dial = &Disagree.dial }, 1, 0, cache.RecallSink.none, cache.SelfLease.always, cache.recordsBudgetBytes(0));
    defer d.deinit();
    const followed0 = divert.stats.dlm_token_reader_redirects_followed.load();
    try testing.expectError(error.HolderUnreachable, d.serve(io, foreign, .{}));
    try testing.expectEqual(followed0 + 1, divert.stats.dlm_token_reader_redirects_followed.load());
    try testing.expectEqual(@as(u32, 1), f.resolvers[0].refreshes);
}

test "the parked joiner's root (rung 10 item 5a): the divert remembers the root's attributes as last served under a token; past T_self the serve refuses MembershipPending and the cache is shed, yet `lastRoot` still answers them (counted) — the `getattr(1)` a parked joiner serves so `.stats` stays reachable; a divert that never served the root has none" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    // The root is A's object here (slot 0 is the manager's).
    try f.records.putInode(inos.ROOT_INO, .{ .mode = 0o040755, .nlink = 7, .mtime = 4242 });
    const RootIsA = struct {
        inner: *ReaderResolver,
        fn homeOf(_: *anyopaque, _: u16) divert.Home {
            return .{ .foreign = .{ .holder = A_APPENDER, .identity = A_ID } };
        }
        fn refresh(_: *anyopaque, _: u16, _: u32) divert.Home {
            return .{ .foreign = .{ .holder = A_APPENDER, .identity = A_ID } };
        }
        fn dial(ctx: *anyopaque, id: u64, h: u32) ?cache.Link {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            return ReaderResolver.dial(self.inner, id, h);
        }
    };
    var r: RootIsA = .{ .inner = &f.resolvers[0] };
    var d = divert.Divert.init(gpa, io, .{ .ctx = &r, .homeOf = &RootIsA.homeOf, .refresh = &RootIsA.refresh, .dial = &RootIsA.dial }, 1, 0, cache.RecallSink.none, f.member_leases[0].selfLease(), cache.recordsBudgetBytes(0));
    defer d.deinit();
    const served_last0 = divert.stats.dlm_token_root_served_last.load();
    // Never served: nothing to fall back on.
    try testing.expectEqual(@as(?record.InodeValue, null), d.lastRoot());
    var s = (try d.serve(io, inos.ROOT_INO, .{})).?;
    try testing.expectEqual(@as(u64, 4242), s.entry.?.attrs.mtime);
    s.release(io);
    // Another object's serve leaves the root memo alone.
    s = (try d.serve(io, objectOf(3), .{})).?;
    s.release(io);
    try testing.expectEqual(@as(u32, 7), d.lastRoot().?.nlink);
    // The manager dies: past T_self every serve refuses and the cache is
    // shed; the root's last attributes still answer.
    f.member_leases[0].deadline_ns.store(1, .release);
    try testing.expectError(error.MembershipPending, d.serve(io, inos.ROOT_INO, .{}));
    try testing.expectEqual(@as(usize, 0), d.planeOf(io, A_ID).?.cache.count(io));
    const last = d.lastRoot().?;
    try testing.expectEqual(@as(u64, 4242), last.mtime);
    try testing.expectEqual(@as(u32, 7), last.nlink);
    try testing.expectEqual(served_last0 + 2, divert.stats.dlm_token_root_served_last.load());
}

test "the serve gate: a reader past T_self serves NOTHING from its cache; a holder that moved its listener is re-pointed in place, every token dropped, the channel fail-closed until a round completes" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    const b = f.reader(0);
    var s = (try b.serve(io, objectOf(3), .{})).?;
    s.release(io);
    const plane = b.planeOf(io, A_ID).?;
    // Past T_self: refused, the cache dropped.
    f.member_leases[0].deadline_ns.store(1, .release);
    const refusals0 = cache.stats.dlm_token_serve_refusals.load();
    try testing.expectError(error.MembershipPending, b.serve(io, objectOf(3), .{}));
    try testing.expectEqual(refusals0 + 1, cache.stats.dlm_token_serve_refusals.load());
    try testing.expectEqual(@as(usize, 0), plane.cache.count(io));
    f.member_leases[0].renew(clock.monotonicNs(), table.T_OWNER_NS);
    s = (try b.serve(io, objectOf(3), .{})).?;
    s.release(io);
    // The re-point onto a session that dies at its first frame: every token
    // dropped, the probe round fails, the channel stays fail-closed.
    var dead = try f.hub.connect(1, 0);
    defer dead.deinit();
    dead.tamper_next = true;
    const gen0 = plane.endpointGen();
    const repoints0 = cache.stats.dlm_token_holder_repoints.load();
    @import("../../wire/stats.zig").cluster_wire_mac_failures.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    plane.repoint(io, linkOf(&dead));
    try log.testing.expectAllFired();
    try testing.expectEqual(gen0 + 1, plane.endpointGen());
    try testing.expectEqual(repoints0 + 1, cache.stats.dlm_token_holder_repoints.load());
    try testing.expectEqual(@as(usize, 0), plane.cache.count(io));
    try testing.expect(!plane.channelOk());
    // The plane itself serves nothing while the channel is closed; the
    // divert's serve re-points it ONCE inside the same op through the
    // resolver's dial (the fixture's live session), the probe round
    // re-opens the channel and the serve lands on a fresh grant.
    try testing.expectError(error.TokenChannelStale, plane.serve(io, objectOf(3), .{}));
    const reprobes0 = divert.stats.dlm_token_reader_channel_reprobes.load();
    s = (try b.serve(io, objectOf(3), .{})).?;
    try testing.expectEqual(@as(u64, 100), s.entry.?.attrs.mtime);
    s.release(io);
    try testing.expectEqual(reprobes0 + 1, divert.stats.dlm_token_reader_channel_reprobes.load());
    try testing.expect(plane.channelOk());
    try testing.expectEqual(gen0 + 2, plane.endpointGen());
    // The holder's moved listener: the re-point's probe round completes and
    // the channel stays open; the next serve is a cache hit of the grant.
    var fresh = try f.hub.connect(1, 0);
    defer fresh.deinit();
    plane.repoint(io, linkOf(&fresh));
    try testing.expect(plane.channelOk());
    try testing.expectEqual(gen0 + 3, plane.endpointGen());
    s = (try b.serve(io, objectOf(3), .{})).?;
    try testing.expectEqual(@as(u64, 100), s.entry.?.attrs.mtime);
    s.release(io);
    // Back onto the fixture's session before the temporary ones die.
    plane.repoint(io, linkOf(&f.b_to_a[0]));
}

test "a CACHED token serves while the holder is unreachable: a round that fails closes the channel for FETCHES only — the cached object still hits (0 RPCs), an uncached one is the typed EAGAIN, the in-op re-probe whose dial dies keeps the cache; a re-probe that completes drops it (a live holder we lost a round with)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    const b = f.reader(0);
    // Object 3 cached under a token.
    var s = (try b.serve(io, objectOf(3), .{})).?;
    try testing.expectEqual(@as(u64, 100), s.entry.?.attrs.mtime);
    s.release(io);
    const plane = b.planeOf(io, A_ID).?;
    // The holder goes dark: the plane's link is a session that dies at
    // its first frame — the fetch of an UNCACHED object fails the round
    // and closes the channel.
    var dead = try f.hub.connect(1, 0);
    defer dead.deinit();
    dead.tamper_next = true;
    plane.link = linkOf(&dead);
    @import("../../wire/stats.zig").cluster_wire_mac_failures.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Wire, b.serve(io, objectOf(4), .{}));
    try log.testing.expectAllFired();
    try testing.expect(!plane.channelOk());
    // The cached token still serves — a hit, no round attempted.
    const hits0 = cache.stats.dlm_token_hits.load();
    const rounds0 = cache.stats.dlm_token_channel_rounds.load();
    s = try plane.serve(io, objectOf(3), .{});
    try testing.expectEqual(@as(u64, 100), s.entry.?.attrs.mtime);
    s.release(io);
    try testing.expectEqual(hits0 + 1, cache.stats.dlm_token_hits.load());
    try testing.expectEqual(rounds0, cache.stats.dlm_token_channel_rounds.load());
    try testing.expectEqual(@as(usize, 1), plane.cache.count(io));
    // An uncached object on the closed channel: the plane's typed EAGAIN.
    try testing.expectError(error.TokenChannelStale, plane.serve(io, objectOf(2), .{}));
    // The divert's in-op re-probe re-dials through the resolver: a probe
    // round that dies on the re-dialled link (a second session tampered
    // at its first frame) leaves the cache intact.
    var dead2 = try f.hub.connect(1, 0);
    defer dead2.deinit();
    dead2.tamper_next = true;
    f.resolvers[0].conn_to_a = &dead2;
    const reprobes0 = divert.stats.dlm_token_reader_channel_reprobes.load();
    @import("../../wire/stats.zig").cluster_wire_mac_failures.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.TokenChannelStale, b.serve(io, objectOf(2), .{}));
    try log.testing.expectAllFired();
    try testing.expectEqual(reprobes0 + 1, divert.stats.dlm_token_reader_channel_reprobes.load());
    try testing.expectEqual(@as(usize, 1), plane.cache.count(io));
    s = (try b.serve(io, objectOf(3), .{})).?;
    s.release(io);
    // Back onto the fixture's live session for the re-dial.
    f.resolvers[0].conn_to_a = &f.b_to_a[0];
    // A re-probe whose round completes: the holder is live — the cache
    // dropped (it may have re-granted), the channel open, the fetch lands.
    s = (try b.serve(io, objectOf(2), .{})).?;
    s.release(io);
    try testing.expect(plane.channelOk());
    try testing.expectEqual(reprobes0 + 2, divert.stats.dlm_token_reader_channel_reprobes.load());
    // Object 3's old token was dropped by the re-probe; the serve is a fresh grant.
    const grants0 = cache.stats.dlm_token_grants.load();
    s = (try b.serve(io, objectOf(3), .{})).?;
    s.release(io);
    try testing.expectEqual(grants0 + 1, cache.stats.dlm_token_grants.load());
}

test "the seams read their registry rows once (default-inert); the conveyor binding recalls through the plane's pusher and settles the same union" {
    const gpa = testing.allocator;
    const io = testing.io;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const inert = table.Seams.fromEnv(&env);
    try testing.expectEqual(@as(u64, 0), inert.recall_deadline_ns);
    try testing.expect(!inert.conveyor_hold_post_recall and !inert.grant_hold_after_check);
    try env.put("SQUEEZEFS_TEST_TOKEN_RECALL_DEADLINE_MS", "20");
    try env.put("SQUEEZEFS_TEST_TOKEN_CONVEYOR_HOLD_POST_RECALL", "1");
    const armed = table.Seams.fromEnv(&env);
    try testing.expectEqual(@as(u64, 20 * std.time.ns_per_ms), armed.recall_deadline_ns);
    try testing.expect(armed.conveyor_hold_post_recall and !armed.grant_hold_after_check);

    const f = try TwoMembers.init(gpa, io, 1);
    defer f.deinit();
    var binding: ConveyorBinding = .{ .io = io, .holder = &f.holder, .pusher = f.pusher.pusher() };
    const hook = binding.hook();
    var s = (try f.reader(0).serve(io, objectOf(3), .{})).?;
    s.release(io);
    const objects = [_]u64{objectOf(3)};
    try hook.recall(hook.ctx, &objects);
    try testing.expect(!f.holder.holdsGrant(io, objectOf(3), 1));
    try testing.expectEqual(@as(usize, 1), f.holder.gate.inflightCount(io));
    hook.settle(hook.ctx, &objects);
    try testing.expectEqual(@as(usize, 0), f.holder.gate.inflightCount(io));
}
