//! The OFFLINE data-volume verbs (charter §8 `volume {add-data, remove-
//! data, undrain, list}`; spec §2.3, §2.6): `lattice.World` bound to the
//! writer-door set `cli/offline.withSet` opens — the gate IS the door (a
//! live writer refused the open by name), the record is volume 0's
//! `set_config`, the bit is stamped on EVERY member barriered, the data
//! superblock is written through the same writer `format` uses, and the
//! verbs' job records (the add's queued `rebalance` at 25 %, the
//! remove's queued `evacuate_volume`) ride the SAME tx so the next mount's
//! fabric adopts them. The offline set has no placement runtime: the
//! `publishPlacement` step is the next mount's discovery. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const key = @import("../../meta/key.zig");
const tree = @import("../../meta/tree.zig");
const geometry = @import("../../meta/geometry.zig");
const meta_set = @import("../../meta/meta_set.zig");
const set_config = @import("../../data/set_config.zig");
const data_superblock = @import("../../data/data_superblock.zig");
const transform = @import("../../data/transform.zig");
const plane_mod = @import("../../data/plane.zig");
const records = @import("../jobs/records.zig");
const preflight = @import("../jobs/preflight.zig");
const lattice = @import("lattice.zig");
const census = @import("census.zig");
const stats_mod = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/lifecycle/volume_ops is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const stats = stats_mod.stats;

/// The offline world: the set behind the writer door; a plane when the
/// verb opened one (`remove-data`'s census).
pub const OfflineWorld = struct {
    gpa: Allocator,
    io: Io,
    be: *meta_set.MetaSet,
    plane: ?*plane_mod.DataPlane,
    /// The job records written in the verb's tx (what the verb prints).
    jobs_written: std.ArrayList(records.JobRecord) = .empty,
    cancelled: std.ArrayList(u64) = .empty,

    pub fn deinit(self: *OfflineWorld) void {
        self.jobs_written.deinit(self.gpa);
        self.cancelled.deinit(self.gpa);
    }

    pub fn world(self: *OfflineWorld) lattice.World {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: lattice.World.VTable = .{ .gate = &gate, .loadConfig = &loadConfig, .stampLifecycleBit = &stamp, .formatDevice = &formatDevice, .commit = &commit, .publishPlacement = &publish, .cancelEvacuations = &cancelEvac, .now_s = &nowS, .random = &random };

    fn self_(ctx: *anyopaque) *OfflineWorld {
        return @ptrCast(@alignCast(ctx));
    }
    /// The writer door was the gate: a held guard refused the open.
    fn gate(_: *anyopaque) lattice.Error!void {}
    fn loadConfig(ctx: *anyopaque, gpa: Allocator) lattice.Error!set_config.Config {
        const self = self_(ctx);
        const value = (self.be.lookup(gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch return error.Io) orelse return error.ConfigUndecodable;
        defer gpa.free(value);
        var diag: set_config.Refusal = undefined;
        return set_config.decode(gpa, value, &diag) catch return error.ConfigUndecodable;
    }
    fn stamp(ctx: *anyopaque) lattice.Error!void {
        self_(ctx).be.stampVolumeLifecycle() catch return error.Io;
    }
    fn formatDevice(ctx: *anyopaque, dev: lattice.Device, vol_tag: u64) lattice.Error!void {
        const self = self_(ctx);
        var cfg = try loadConfig(ctx, self.gpa);
        defer cfg.deinit(self.gpa);
        const sb: data_superblock.DataSuperblock = .{
            .block_size = cfg.block_size,
            .chunk_size = cfg.chunk_size,
            .compression = cfg.compression,
            .encryption = cfg.encryption,
            .volume_len = dev.volume_len,
            .vol_tag = vol_tag,
            .set_uuid = self.be.setUuid(),
            .device_uuid = dev.uuid,
            .features_incompat = data_superblock.featuresFor(.{ .compression = cfg.compression, .encryption = cfg.encryption, .block_size = cfg.block_size }),
        };
        data_superblock.linux_dev.format(self.gpa, dev.uri, sb, 1) catch |e| {
            log.err("volume add-data: {s}: the data superblock could not be written ({s})", .{ dev.uri, @errorName(e) });
            return error.Io;
        };
    }
    fn commit(ctx: *anyopaque, cfg: *const set_config.Config, job: ?records.JobRecord) lattice.Error!void {
        const self = self_(ctx);
        const bytes = set_config.encode(self.gpa, cfg, @intCast(geometry.recordValueCap(self.be.nodeSize()))) catch return error.Io;
        defer self.gpa.free(bytes);
        const t = self.be.begin(@src()) catch return error.OutOfMemory;
        t.stagePut(key.TREE_CONTROL, set_config.CONTROL_KEY, bytes) catch {
            self.be.abandon(t);
            return error.OutOfMemory;
        };
        if (job) |j| {
            const jk = records.jobKey(j.job_id);
            const jb = j.encode();
            t.stagePut(key.TREE_CONTROL, jk.slice(), &jb) catch {
                self.be.abandon(t);
                return error.OutOfMemory;
            };
            self.jobs_written.append(self.gpa, j) catch return error.OutOfMemory;
        }
        self.be.commit(t) catch return error.Io;
        self.be.fsync() catch return error.Io;
        stats.volume_lifecycle_commits.inc();
    }
    /// No runtime offline: the next mount's discovery is the placement.
    fn publish(_: *anyopaque, _: u64, _: set_config.State) lattice.Error!void {}
    fn cancelEvac(ctx: *anyopaque, vol_tag: u64) lattice.Error!void {
        const self = self_(ctx);
        var jobs = try listJobs(self.gpa, self.be);
        defer jobs.deinit(self.gpa);
        for (jobs.items) |*j| {
            if (j.job_type != .evacuate_volume or j.arg0 != vol_tag or j.isTerminal()) continue;
            j.state = .cancelled;
            j.pause_intent = false;
            j.updated_realtime_s = clock.realtimeCoarseS();
            const jk = records.jobKey(j.job_id);
            const jb = j.encode();
            const t = self.be.begin(@src()) catch return error.OutOfMemory;
            t.stagePut(key.TREE_CONTROL, jk.slice(), &jb) catch {
                self.be.abandon(t);
                return error.OutOfMemory;
            };
            self.be.commit(t) catch return error.Io;
            self.cancelled.append(self.gpa, j.job_id) catch return error.OutOfMemory;
        }
        self.be.fsync() catch return error.Io;
        stats.volume_undrained.inc();
    }
    fn nowS(_: *anyopaque) u64 {
        return clock.realtimeCoarseS();
    }
    fn random(ctx: *anyopaque) u64 {
        const self = self_(ctx);
        var b: [8]u8 = undefined;
        self.io.randomSecure(&b) catch return @bitCast(clock.monotonicNs());
        return std.mem.readInt(u64, &b, .little);
    }
};

/// Every durable job record of the set (the `job list` probe and the
/// undrain's cancel): decodable ones only, in id order.
pub fn listJobs(gpa: Allocator, be: *meta_set.MetaSet) lattice.Error!std.ArrayList(records.JobRecord) {
    const r = records.rangeOf(.job);
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), 4096, &kvs) catch return error.Io;
    var out: std.ArrayList(records.JobRecord) = .empty;
    errdefer out.deinit(gpa);
    for (kvs.items) |kv| {
        if (records.jobIdOfKey(kv.key) == null) continue;
        const rec = records.JobRecordV1.decode(kv.value) catch continue;
        try out.append(gpa, rec);
    }
    return out;
}

/// The device identity the offline add preflights: read the superblock
/// path (a member of THIS set — by `set_uuid` — is refused; a blank or
/// another set's device is admissible and re-formatted), size it, mint
/// the device uuid. The rw probe IS the superblock write + read-back the
/// format step runs (device-before-record).
pub fn probeDevice(gpa: Allocator, io: Io, path: []const u8, set_uuid: [16]u8, chunk_size: u32) lattice.Error!lattice.Device {
    const opened = data_superblock.linux_dev.open(gpa, path) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        error.Io, error.Unexpected => return error.Io,
        else => return error.DeviceRefused,
    };
    switch (opened.classified.format) {
        .v1 => |sb| if (std.mem.eql(u8, &sb.set_uuid, &set_uuid)) return error.DeviceRefused,
        .blank => {},
    }
    var uuid: [16]u8 = undefined;
    io.randomSecure(&uuid) catch return error.Io;
    const blocks = transform.capacityBlocks(opened.volume_len, chunk_size);
    if (blocks == 0) return error.DeviceRefused;
    return .{ .uri = path, .uuid = uuid, .capacity_blocks = blocks, .volume_len = opened.volume_len };
}

/// `remove-data`'s capacity preflight over the OPEN plane: the census is
/// deduped per block; the survivors are the router's rows.
pub fn preflightDrain(gpa: Allocator, plane: *plane_mod.DataPlane, victim: u64, workers: u64) !preflight.CapacityPreflight {
    const Walk = struct {
        plane: *plane_mod.DataPlane,
        fn walk(ctx: *anyopaque, v: census.LayoutWalk.Visitor) error{ Io, OutOfMemory }!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            const V = struct {
                v: census.LayoutWalk.Visitor,
                fn visit(c: *@This(), ino: u64, bi: u32, m: census.Mapping) plane_mod.Error!void {
                    c.v.visit(c.v.ctx, .{ .ino = ino, .block_index = bi, .mapping = m }) catch return error.OutOfMemory;
                }
            };
            var vv: V = .{ .v = v };
            self.plane.forEachBinding(*V, &vv, V.visit) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Io,
            };
        }
    };
    var w: Walk = .{ .plane = plane };
    var plan = try census.censusFor(gpa, .{ .ctx = &w, .walk = &Walk.walk }, &.{victim}, plane.chunk);
    defer plan.deinit(gpa);
    try plane.refreshCensus();
    const live = plane.rt.volumesLive();
    const rows = try gpa.alloc(preflight.SurvivorRow, live.len);
    defer gpa.free(rows);
    for (live, rows) |*v, *r| r.* = .{ .vol_tag = v.id.tag, .eligible = v.placementEligible(), .capacity_bytes = v.capacity_bytes, .used_bytes = v.used_bytes };
    return preflight.compute(.{ .needed = plan.distinct_blocks * plane.chunk, .rows = rows, .victim = victim, .workers = workers, .block_size = plane.chunk, .write_rate_bps = 0 });
}

/// `volume list`: the record rows joined with the allocator accounting
/// when a plane is open (`{id, state, capacity, used, free}`), the
/// durable job records after.
pub fn writeList(gpa: Allocator, be: *meta_set.MetaSet, plane: ?*plane_mod.DataPlane, w: *Io.Writer) !void {
    const value = (be.lookup(gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch return error.Io) orelse return error.ConfigUndecodable;
    defer gpa.free(value);
    var diag: set_config.Refusal = undefined;
    var cfg = set_config.decode(gpa, value, &diag) catch return error.ConfigUndecodable;
    defer cfg.deinit(gpa);
    try w.print("data volumes (config generation {d}):\n", .{cfg.config_generation});
    for (cfg.data) |d| {
        try w.writeAll("  ");
        try lattice.writeVolumeId(w, d.vol_tag);
        try w.print("  {s:<9} capacity {d} B", .{ d.state.name(), d.capacity_blocks * cfg.block_size });
        if (plane) |p| if (p.vols.byTag(d.vol_tag)) |v| {
            const used = v.alloc.usedBlocks() * p.chunk;
            try w.print("  used {d} B  free {d} B", .{ used, (d.capacity_blocks * cfg.block_size) -| used });
        };
        if (d.uri.len != 0) try w.print("  {s}", .{d.uri});
        try w.writeByte('\n');
    }
    var jobs = listJobs(gpa, be) catch return error.Io;
    defer jobs.deinit(gpa);
    if (jobs.items.len != 0) try w.writeAll("jobs:\n");
    for (jobs.items) |j| try w.print("  {f}  {s:<16} {s:<16} throttle {d} %  {d}/{d}\n", .{ j, j.job_type.name(), @tagName(j.state), j.throttle_pct, j.tasks_done, j.tasks_total });
}
