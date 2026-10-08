//! Order-independent mount discovery of a metadata set (charter §5.4
//! "order-independent mount discovery via per-volume membership stamps
//! with URI-disagreement refusals"; `docs/porting/dynamic_routing.md` §1.5,
//! §3.5 the crash table, §7 Q1). PORTABLE over `Observation`s (what each
//! volume's superblock + newest valid ledger record say); the Linux
//! `observe` reads them off the devices WITHOUT mounting (the probe class:
//! never blocked by a live writer).
//!
//! `discover` assembles the set in the reference's order — stamped census,
//! retired tombstones, pairwise identity, the URI count, positions, the
//! per-slot HIGHEST-EPOCH-WINS claim resolution, coverage — refusing each
//! disagreement by name (the nine texts verbatim, plus Z/'s own: the hash
//! seed and the node size must be set-wide, and the native keyspace rides
//! with slot 0's host). Every slot two stamps claim at different epochs is
//! reported as a DUAL CLAIM so a writable open converges it before serving
//! (§7 Q1: the reference left the loser's stale claim in place, where an
//! unrelated later flip could resurrect it); a same-epoch dual claim is the
//! "impossible by construction" tripwire and a refusal.
//!
//! The canonical member order is `member_position` ascending, so the
//! discovered map and every derived identity are independent of how the
//! operator ordered the URI.

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const key = @import("../key.zig");
const ledger = @import("../ledger.zig");
const route_table = @import("route_table.zig");
const rstats = @import("stats.zig");
const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");

const W = route_table.W;
const VolumeIndex = route_table.VolumeIndex;

/// One volume as observed off its device (the ledger's newest valid
/// record decoded; `stamp == null` = no valid record = stampless).
pub const Observation = struct {
    path: []const u8,
    uuid: [16]u8,
    hash_seed: u64,
    node_size: u32,
    /// The newest valid ledger record's stamp (owned by the observer).
    stamp: ?ledger.MembershipStamp,
    /// The newest valid record's checkpoint seq (0 when stampless).
    ledger_seq: u64 = 0,
};

/// A slot two stamps claim at different epochs: the higher epoch hosts
/// it; the loser's stale claim is converged by a writable open.
pub const DualClaim = struct {
    slot: u16,
    /// Canonical indices.
    winner: VolumeIndex,
    loser: VolumeIndex,
    winner_epoch: u64,
    loser_epoch: u64,
};

pub const Discovery = struct {
    gpa: Allocator,
    /// `order[i]` = the observation index of canonical member `i`.
    order: []usize,
    /// Canonical index → `member_position`.
    positions: []u16,
    set_uuid: [16]u8,
    hash_seed: u64,
    node_size: u32,
    /// `max` over the members (epochs legitimately differ at rest).
    set_epoch: u64,
    member_count: u16,
    slot_to_volume: *[W]VolumeIndex,
    /// The canonical index of the member carrying the native keyspace
    /// (slot 0's host).
    native_volume: VolumeIndex,
    dual_claims: []DualClaim,

    pub fn deinit(self: *Discovery) void {
        self.gpa.free(self.order);
        self.gpa.free(self.positions);
        self.gpa.destroy(self.slot_to_volume);
        self.gpa.free(self.dual_claims);
        self.* = undefined;
    }

    pub fn table(self: *const Discovery) route_table.RouteTable {
        return route_table.RouteTable.init(self.member_count, self.slot_to_volume);
    }

    /// The observation of canonical member `v`.
    pub fn observation(self: *const Discovery, obs: []const Observation, v: VolumeIndex) *const Observation {
        return &obs[self.order[v]];
    }
};

pub const Refusal = union(enum) {
    all_stampless: struct { paths: []const Observation },
    mixed_stampless: struct { stamped: usize, total: usize, obs: []const Observation },
    retired: struct { path: []const u8 },
    different_sets: struct { a: []const u8, b: []const u8, ua: [16]u8, ub: [16]u8 },
    hash_seed: struct { a: []const u8, b: []const u8 },
    node_size: struct { a: []const u8, b: []const u8, na: u32, nb: u32 },
    member_count: struct { a: []const u8, b: []const u8, ma: u16, mb: u16 },
    count_mismatch: struct { declared: u16, listed: usize, obs: []const Observation },
    duplicate_position: struct { a: []const u8, b: []const u8, position: u16 },
    same_epoch_dual_claim: struct { a: []const u8, b: []const u8, slot: u16, epoch: u64 },
    unclaimed: struct { slot: u32 },
    two_native: struct { a: []const u8, b: []const u8 },
    native_not_zero: struct { path: []const u8, slot: u16 },
    slot_zero_host_without_native: struct { path: []const u8 },
    native_not_slot_zero_host: struct { native: []const u8, host: []const u8 },
    slot_map: route_table.Refusal,

    fn writePaths(w: *std.Io.Writer, obs: []const Observation, stampless_only: bool) std.Io.Writer.Error!void {
        try w.writeByte('[');
        var first = true;
        for (obs) |o| {
            if (stampless_only and o.stamp != null) continue;
            if (!first) try w.writeAll(", ");
            try w.print("\"{s}\"", .{o.path});
            first = false;
        }
        try w.writeByte(']');
    }

    fn writeUuid4(w: *std.Io.Writer, u: [16]u8) std.Io.Writer.Error!void {
        try w.print("{x:0>2}{x:0>2}{x:0>2}{x:0>2}", .{ u[0], u[1], u[2], u[3] });
    }

    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .all_stampless => |r| {
                try w.writeAll("metadata volumes ");
                try writePaths(w, r.paths, false);
                try w.writeAll(" carry NO §5.5.1a membership stamps — every dynamic-routing format stamps its members at format time, so these ledgers are torn or foreign; reformat required (`squeezefs format --force`, destroys the old contents)");
            },
            .mixed_stampless => |r| {
                try w.print("metadata set mixes stamped and stampless members: {d} of {d} volumes carry a §5.5.1a membership stamp but ", .{ r.stamped, r.total });
                try writePaths(w, r.obs, true);
                try w.writeAll(" carry none — a slot-mapped set's members are all stamped; if a repair was interrupted, run `squeezefs volume repair-set <sqmeta-uri>`");
            },
            .retired => |r| try w.print("metadata volume {s} is a RETIRED member (remove-meta tombstone) — remove it from the URI; it carries no live slots", .{r.path}),
            .different_sets => |r| {
                try w.print("metadata volumes belong to DIFFERENT sets: {s} and {s} carry different set uuids (", .{ r.a, r.b });
                try writeUuid4(w, r.ua);
                try w.writeAll(" vs ");
                try writeUuid4(w, r.ub);
                try w.writeAll(") — one of them is from another filesystem");
            },
            .hash_seed => |r| try w.print("metadata volumes {s} and {s} carry different hash seeds — not one set", .{ r.a, r.b }),
            .node_size => |r| try w.print("metadata volumes {s} and {s} carry different node sizes ({d} vs {d} B) — not one set", .{ r.a, r.b, r.na, r.nb }),
            .member_count => |r| try w.print("metadata set stamps disagree on membership (an interrupted add-meta/remove-meta): {s} declares {d} members while {s} declares {d} — re-run the interrupted `squeezefs volume add-meta`/`remove-meta` with the same arguments (idempotent), or `squeezefs volume repair-set`", .{ r.a, r.ma, r.b, r.mb }),
            .count_mismatch => |r| {
                try w.print("metadata set stamps declare {d} members but the URI lists {d} (", .{ r.declared, r.listed });
                try writePaths(w, r.obs, false);
                try w.print(") — a member is {s}; every member of the set must be listed", .{if (r.declared > r.listed) "missing from the URI" else "listed that the stamps do not name"});
            },
            .duplicate_position => |r| try w.print("metadata volumes {s} and {s} BOTH stamp member position {d} — duplicate membership; one of them is stale or foreign", .{ r.a, r.b, r.position }),
            .same_epoch_dual_claim => |r| try w.print("metadata volumes {s} and {s} BOTH host routing slot {d} at the SAME epoch {d} — impossible by construction (one coordinator, one flip per slot); treat as corruption", .{ r.a, r.b, r.slot, r.epoch }),
            .unclaimed => |r| try w.print("routing slot {d} of width {d} is hosted by NO listed volume — the durable slot map is incomplete; run `squeezefs volume repair-set`", .{ r.slot, W }),
            .two_native => |r| try w.print("metadata volumes {s} and {s} BOTH carry a native keyspace — a v4 set keeps ONE (slot 0's host); one of them is stale or foreign", .{ r.a, r.b }),
            .native_not_zero => |r| try w.print("metadata volume {s} stamps native slot {d} — a v4 native keyspace is slot 0's (the root pin) alone", .{ r.path, r.slot }),
            .slot_zero_host_without_native => |r| try w.print("metadata volume {s} hosts routing slot 0 but stamps no native keyspace — slot 0's records live un-namespaced on their host; run `squeezefs volume repair-set`", .{r.path}),
            .native_not_slot_zero_host => |r| try w.print("metadata volume {s} carries the native keyspace but {s} hosts routing slot 0 — a v4 set keeps the native keyspace with slot 0's host", .{ r.native, r.host }),
            .slot_map => |r| try r.write(w),
        }
    }
};

pub const Error = error{ Refused, OutOfMemory };

/// Assemble the set (module doc). `obs` is the URI's order; the result's
/// `order` is canonical.
pub fn discover(gpa: Allocator, obs: []const Observation, diag: *Refusal) Error!Discovery {
    const n = obs.len;
    // 1–2. The stamped census.
    var stamped: usize = 0;
    for (obs) |o| stamped += @intFromBool(o.stamp != null);
    if (stamped == 0) {
        diag.* = .{ .all_stampless = .{ .paths = obs } };
        return error.Refused;
    }
    if (stamped < n) {
        diag.* = .{ .mixed_stampless = .{ .stamped = stamped, .total = n, .obs = obs } };
        return error.Refused;
    }
    // 3. Tombstones.
    for (obs) |o| if (o.stamp.?.member_count == 0) {
        diag.* = .{ .retired = .{ .path = o.path } };
        return error.Refused;
    };
    // 4. Pairwise identity against the first.
    const first = obs[0];
    for (obs[1..]) |o| {
        const a = first.stamp.?;
        const b = o.stamp.?;
        if (!std.mem.eql(u8, &a.set_uuid, &b.set_uuid)) {
            diag.* = .{ .different_sets = .{ .a = first.path, .b = o.path, .ua = a.set_uuid, .ub = b.set_uuid } };
            return error.Refused;
        }
        if (first.hash_seed != o.hash_seed) {
            diag.* = .{ .hash_seed = .{ .a = first.path, .b = o.path } };
            return error.Refused;
        }
        if (first.node_size != o.node_size) {
            diag.* = .{ .node_size = .{ .a = first.path, .b = o.path, .na = first.node_size, .nb = o.node_size } };
            return error.Refused;
        }
        if (a.member_count != b.member_count) {
            diag.* = .{ .member_count = .{ .a = first.path, .b = o.path, .ma = a.member_count, .mb = b.member_count } };
            return error.Refused;
        }
    }
    // 5. The URI count.
    const declared = first.stamp.?.member_count;
    if (declared != n) {
        diag.* = .{ .count_mismatch = .{ .declared = declared, .listed = n, .obs = obs } };
        return error.Refused;
    }
    // 6. Canonical order by position; duplicates refuse.
    const order = try gpa.alloc(usize, n);
    errdefer gpa.free(order);
    for (order, 0..) |*o, i| o.* = i;
    std.mem.sort(usize, order, obs, struct {
        fn lt(o: []const Observation, a: usize, b: usize) bool {
            return o[a].stamp.?.member_position < o[b].stamp.?.member_position;
        }
    }.lt);
    const positions = try gpa.alloc(u16, n);
    errdefer gpa.free(positions);
    for (order, positions) |i, *p| p.* = obs[i].stamp.?.member_position;
    for (positions[1..], 1..) |p, i| if (p == positions[i - 1]) {
        diag.* = .{ .duplicate_position = .{ .a = obs[order[i - 1]].path, .b = obs[order[i]].path, .position = p } };
        return error.Refused;
    };
    // 7. Claims, per slot highest-epoch-wins.
    const Claim = struct { volume: VolumeIndex, epoch: u64 };
    const claims = try gpa.alloc(?Claim, W);
    defer gpa.free(claims);
    @memset(claims, null);
    var duals: std.ArrayList(DualClaim) = .empty;
    errdefer duals.deinit(gpa);
    var set_epoch: u64 = 0;
    for (order, 0..) |oi, vi| {
        const v: VolumeIndex = @intCast(vi);
        const st = obs[oi].stamp.?;
        set_epoch = @max(set_epoch, st.set_epoch);
        for (st.slots_hosted.runs) |r| {
            var k: u32 = 0;
            while (k < r.count) : (k += 1) {
                const s: usize = @intCast(@as(u64, r.start) + @as(u64, k) * @as(u64, r.stride));
                if (claims[s]) |c| {
                    if (c.epoch == st.set_epoch) {
                        rstats.stats.routing_same_epoch_dual_claim.fire("two stamps claim one slot at one epoch");
                        diag.* = .{ .same_epoch_dual_claim = .{ .a = obs[order[c.volume]].path, .b = obs[oi].path, .slot = @intCast(s), .epoch = c.epoch } };
                        return error.Refused;
                    }
                    if (st.set_epoch > c.epoch) {
                        try duals.append(gpa, .{ .slot = @intCast(s), .winner = v, .loser = c.volume, .winner_epoch = st.set_epoch, .loser_epoch = c.epoch });
                        claims[s] = .{ .volume = v, .epoch = st.set_epoch };
                    } else {
                        try duals.append(gpa, .{ .slot = @intCast(s), .winner = c.volume, .loser = v, .winner_epoch = c.epoch, .loser_epoch = st.set_epoch });
                    }
                } else claims[s] = .{ .volume = v, .epoch = st.set_epoch };
            }
        }
    }
    // 8. Coverage.
    const map = try gpa.create([W]VolumeIndex);
    errdefer gpa.destroy(map);
    for (claims, 0..) |c, s| {
        map[s] = (c orelse {
            diag.* = .{ .unclaimed = .{ .slot = @intCast(s) } };
            return error.Refused;
        }).volume;
    }
    // 9. The native keyspace rides with slot 0's host.
    var native: ?VolumeIndex = null;
    for (order, 0..) |oi, vi| {
        const st = obs[oi].stamp.?;
        if (st.native_slot) |ns| {
            if (ns != 0) {
                diag.* = .{ .native_not_zero = .{ .path = obs[oi].path, .slot = ns } };
                return error.Refused;
            }
            if (native) |prev| {
                diag.* = .{ .two_native = .{ .a = obs[order[prev]].path, .b = obs[oi].path } };
                return error.Refused;
            }
            native = @intCast(vi);
        }
    }
    const host0 = map[0];
    const nv = native orelse {
        diag.* = .{ .slot_zero_host_without_native = .{ .path = obs[order[host0]].path } };
        return error.Refused;
    };
    if (nv != host0) {
        diag.* = .{ .native_not_slot_zero_host = .{ .native = obs[order[nv]].path, .host = obs[order[host0]].path } };
        return error.Refused;
    }
    var mdiag: route_table.Refusal = undefined;
    route_table.validate(@intCast(n), map, nv, &mdiag) catch {
        diag.* = .{ .slot_map = mdiag };
        return error.Refused;
    };
    return .{
        .gpa = gpa,
        .order = order,
        .positions = positions,
        .set_uuid = first.stamp.?.set_uuid,
        .hash_seed = first.hash_seed,
        .node_size = first.node_size,
        .set_epoch = set_epoch,
        .member_count = @intCast(n),
        .slot_to_volume = map,
        .native_volume = nv,
        .dual_claims = try duals.toOwnedSlice(gpa),
    };
}

// ---------------------------------------------------------------------------
// The device observer (Linux): superblock + newest ledger record, no mount.
// ---------------------------------------------------------------------------

pub const linux_dev = if (builtin.os.tag == .linux) struct {
    const superblock = @import("../superblock.zig");
    const device_loader = @import("../device_loader.zig");
    const uring_fs = @import("../../io/uring_fs.zig");

    pub const ObserveError = error{ DeviceRefused, NotFormatted, LegacyFormat, Corrupt, Io, OutOfMemory };

    /// What `observe` owns per volume (the decoded record's slices).
    pub const Owned = struct {
        obs: Observation,
        record: ?ledger.LedgerRecord,

        pub fn deinit(self: *Owned, gpa: Allocator) void {
            if (self.record) |*r| r.deinit(gpa);
        }
    };

    /// One volume: both superblock copies classified (blank and pre-v4
    /// refuse by name), the newest valid ledger record (a ledger with no
    /// valid record observes as STAMPLESS — the repair's input, never a
    /// refusal here).
    pub fn observe(gpa: Allocator, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8) ObserveError!Owned {
        const volume_len = device_loader.volumeLength(path) catch return error.DeviceRefused;
        const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
        var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };
        const sector0 = try dev.readRange(gpa, 0, superblock.sector_len);
        defer gpa.free(sector0);
        const backup: ?[]u8 = if (superblock.backupOffsetFor(volume_len)) |off| try dev.readRange(gpa, off, superblock.sector_len) else null;
        defer if (backup) |b| gpa.free(b);
        var diag: superblock.Refusal = undefined;
        const classified = superblock.classifyVolumeImages(sector0, backup, volume_len, &diag) catch return error.Corrupt;
        // Typed, never loud here: a blank device is the `add-meta` joiner's
        // expected state; the mount names its refusal.
        const sb = switch (classified.format) {
            .blank => return error.NotFormatted,
            .legacy => return error.LegacyFormat,
            .v4 => |s| s,
        };
        const ledger_img = try dev.readRange(gpa, sb.root_ledger.start, @intCast(sb.root_ledger.len));
        defer gpa.free(ledger_img);
        const record = try ledger.newestValid(gpa, ledger_img);
        return .{
            .obs = .{
                .path = path,
                .uuid = sb.uuid,
                .hash_seed = sb.hash_seed,
                .node_size = sb.node_size,
                .stamp = if (record) |r| r.membership_stamp else null,
                .ledger_seq = if (record) |r| r.seq else 0,
            },
            .record = record,
        };
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests: synthetic stamps.
// ---------------------------------------------------------------------------

const testing = std.testing;

const uuid_a: [16]u8 = "set-a-uuid-fixtu".*;
const uuid_b: [16]u8 = "set-b-uuid-fixtu".*;

const Fixture = struct {
    runs: [8][2]ledger.SlotRun = undefined,
    stamps: [8]ledger.MembershipStamp = undefined,
    obs: [8]Observation = undefined,
    n: usize = 0,

    fn member(self: *Fixture, path: []const u8, position: u16, count: u16, epoch: u64, run_list: []const ledger.SlotRun, native: ?u16) void {
        const i = self.n;
        for (run_list, 0..) |r, k| self.runs[i][k] = r;
        self.stamps[i] = .{ .set_uuid = uuid_a, .set_epoch = epoch, .member_position = position, .member_count = count, .slots_hosted = .{ .runs = self.runs[i][0..run_list.len] }, .native_slot = native, .slot_cursors = &.{} };
        self.obs[i] = .{ .path = path, .uuid = @splat(@intCast(i)), .hash_seed = 77, .node_size = 65536, .stamp = self.stamps[i] };
        self.n += 1;
    }

    fn fresh(self: *Fixture, count: u16) void {
        var p: u16 = 0;
        const names = [_][]const u8{ "/a", "/b", "/c", "/d", "/e", "/f", "/g", "/h" };
        while (p < count) : (p += 1) self.member(names[p], p, count, 1, &.{ledger.memberRun(p, count)}, ledger.nativeSlotOf(p == 0));
    }

    fn slice(self: *Fixture) []Observation {
        return self.obs[0..self.n];
    }
};

fn expectRefusal(obs: []const Observation, comptime tag: std.meta.Tag(Refusal), needle: []const u8) !void {
    var diag: Refusal = undefined;
    try testing.expectError(error.Refused, discover(testing.allocator, obs, &diag));
    try testing.expectEqual(tag, std.meta.activeTag(diag));
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try diag.write(&w);
    if (std.mem.indexOf(u8, w.buffered(), needle) == null) {
        std.debug.print("refusal text {s}\n  lacks {s}\n", .{ w.buffered(), needle });
        return error.TestUnexpectedResult;
    }
}

test "a fresh 3-member set discovers to the identity map in canonical order whatever the URI order; the native keyspace is slot 0's host; no dual claims" {
    const gpa = testing.allocator;
    var f: Fixture = .{};
    f.fresh(3);
    // Reverse the URI order.
    std.mem.reverse(Observation, f.slice());
    var diag: Refusal = undefined;
    var d = try discover(gpa, f.slice(), &diag);
    defer d.deinit();
    try testing.expectEqualSlices(usize, &.{ 2, 1, 0 }, d.order);
    try testing.expectEqualSlices(u16, &.{ 0, 1, 2 }, d.positions);
    try testing.expectEqual(@as(u64, 1), d.set_epoch);
    try testing.expectEqual(@as(u16, 3), d.member_count);
    try testing.expectEqual(@as(VolumeIndex, 0), d.native_volume);
    try testing.expectEqual(@as(usize, 0), d.dual_claims.len);
    const t = d.table();
    for (0..W) |s| try testing.expectEqual(@as(VolumeIndex, @intCast(s % 3)), t.volumeOf(@intCast(s)));
    try testing.expectEqualStrings("/a", d.observation(f.slice(), 0).path);
    try testing.expectEqualStrings("/c", d.observation(f.slice(), 2).path);
}

test "discovery refusals: all stampless, mixed, retired, different sets, hash seed, node size, member count, URI count (both directions), duplicate position, same-epoch dual claim, unclaimed, the native-keyspace laws" {
    var f: Fixture = .{};
    f.fresh(2);
    // All stampless.
    var stampless = f;
    stampless.obs[0].stamp = null;
    stampless.obs[1].stamp = null;
    try expectRefusal(stampless.slice(), .all_stampless, "carry NO §5.5.1a membership stamps");
    // Mixed.
    var mixed = f;
    mixed.obs[1].stamp = null;
    try expectRefusal(mixed.slice(), .mixed_stampless, "1 of 2 volumes carry a §5.5.1a membership stamp but [\"/b\"] carry none");
    // Retired tombstone.
    var retired = f;
    retired.stamps[1].member_count = 0;
    retired.obs[1].stamp = retired.stamps[1];
    try expectRefusal(retired.slice(), .retired, "is a RETIRED member (remove-meta tombstone)");
    // Different sets.
    var foreign = f;
    foreign.stamps[1].set_uuid = uuid_b;
    foreign.obs[1].stamp = foreign.stamps[1];
    try expectRefusal(foreign.slice(), .different_sets, "belong to DIFFERENT sets: /a and /b carry different set uuids (7365742d vs 7365742d)");
    // Hash seed / node size (Z/'s additions).
    var seed = f;
    seed.obs[1].hash_seed = 78;
    try expectRefusal(seed.slice(), .hash_seed, "carry different hash seeds — not one set");
    var ns = f;
    ns.obs[1].node_size = 131072;
    try expectRefusal(ns.slice(), .node_size, "different node sizes (65536 vs 131072 B)");
    // Member count disagreement.
    var mc = f;
    mc.stamps[1].member_count = 3;
    mc.obs[1].stamp = mc.stamps[1];
    try expectRefusal(mc.slice(), .member_count, "/a declares 2 members while /b declares 3 — re-run the interrupted");
    // URI count: a member missing; a member listed the stamps do not name.
    try expectRefusal(f.slice()[0..1], .count_mismatch, "declare 2 members but the URI lists 1 ([\"/a\"]) — a member is missing from the URI");
    var three: Fixture = .{};
    three.fresh(3);
    for (&three.stamps) |*s| s.member_count = 2;
    for (three.slice(), 0..) |*o, i| o.stamp = three.stamps[i];
    try expectRefusal(three.slice(), .count_mismatch, "declare 2 members but the URI lists 3");
    // Duplicate position.
    var dup = f;
    dup.stamps[1].member_position = 0;
    dup.obs[1].stamp = dup.stamps[1];
    try expectRefusal(dup.slice(), .duplicate_position, "/a and /b BOTH stamp member position 0");
    // Same-epoch dual claim: member 1 also claims slot 0 at epoch 1.
    var same: Fixture = .{};
    same.member("/a", 0, 2, 1, &.{ledger.memberRun(0, 2)}, 0);
    same.member("/b", 1, 2, 1, &.{ ledger.memberRun(1, 2), .{ .start = 0, .stride = 1, .count = 1 } }, null);
    const tw0 = counters.tripwireCount("routing_same_epoch_dual_claim");
    log.testing.expectErrorLogs(1);
    try expectRefusal(same.slice(), .same_epoch_dual_claim, "BOTH host routing slot 0 at the SAME epoch 1 — impossible by construction");
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 1, counters.tripwireCount("routing_same_epoch_dual_claim"));
    // Unclaimed: member 1's run is short by one.
    var hole: Fixture = .{};
    hole.member("/a", 0, 2, 1, &.{ledger.memberRun(0, 2)}, 0);
    hole.member("/b", 1, 2, 1, &.{.{ .start = 1, .stride = 2, .count = 32767 }}, null);
    try expectRefusal(hole.slice(), .unclaimed, "routing slot 65535 of width 65536 is hosted by NO listed volume");
    // Native laws: two natives; a non-zero native slot; slot 0's host
    // without a native keyspace.
    var two_native = f;
    two_native.stamps[1].native_slot = 0;
    two_native.obs[1].stamp = two_native.stamps[1];
    try expectRefusal(two_native.slice(), .two_native, "BOTH carry a native keyspace");
    var nz = f;
    nz.stamps[1].native_slot = 1;
    nz.obs[1].stamp = nz.stamps[1];
    try expectRefusal(nz.slice(), .native_not_zero, "stamps native slot 1");
    var no_native = f;
    no_native.stamps[0].native_slot = null;
    no_native.obs[0].stamp = no_native.stamps[0];
    try expectRefusal(no_native.slice(), .slot_zero_host_without_native, "hosts routing slot 0 but stamps no native keyspace");
}

test "highest-epoch-wins: a slot flip crashed after write 1 leaves a dual claim; discovery hosts the slot at the higher epoch, mounts with the epoch spread, and reports the loser for convergence" {
    const gpa = testing.allocator;
    // Member 0 @1 claims {0,2,4,…}; member 1 @2 claims {1,3,5,…} + slot 4
    // (flip 4: 0 → 1, write 1 landed, write 2 did not).
    var f: Fixture = .{};
    f.member("/a", 0, 2, 1, &.{ledger.memberRun(0, 2)}, 0);
    f.member("/b", 1, 2, 2, &.{ ledger.memberRun(1, 2), .{ .start = 4, .stride = 1, .count = 1 } }, null);
    var diag: Refusal = undefined;
    var d = try discover(gpa, f.slice(), &diag);
    defer d.deinit();
    try testing.expectEqual(@as(u64, 2), d.set_epoch);
    try testing.expectEqual(@as(VolumeIndex, 1), d.table().volumeOf(4));
    try testing.expectEqual(@as(VolumeIndex, 0), d.table().volumeOf(6));
    try testing.expectEqualSlices(DualClaim, &.{.{ .slot = 4, .winner = 1, .loser = 0, .winner_epoch = 2, .loser_epoch = 1 }}, d.dual_claims);
    // The same set with the URI reversed resolves identically.
    std.mem.reverse(Observation, f.slice());
    var d2 = try discover(gpa, f.slice(), &diag);
    defer d2.deinit();
    try testing.expectEqual(@as(VolumeIndex, 1), d2.table().volumeOf(4));
    try testing.expectEqualSlices(DualClaim, d.dual_claims, d2.dual_claims);
    // The loser at the higher epoch (the stale claimant observed first).
    var g: Fixture = .{};
    g.member("/a", 0, 2, 3, &.{ ledger.memberRun(0, 2), .{ .start = 7, .stride = 1, .count = 1 } }, 0);
    g.member("/b", 1, 2, 1, &.{ledger.memberRun(1, 2)}, null);
    var d3 = try discover(gpa, g.slice(), &diag);
    defer d3.deinit();
    try testing.expectEqual(@as(VolumeIndex, 0), d3.table().volumeOf(7));
    try testing.expectEqualSlices(DualClaim, &.{.{ .slot = 7, .winner = 0, .loser = 1, .winner_epoch = 3, .loser_epoch = 1 }}, d3.dual_claims);
}
