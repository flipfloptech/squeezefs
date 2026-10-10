//! The fabric's LINUX binding (spec §1.1, §1.6 "Port", §1.9): the durable
//! `job:` records on tree 0 of volume 0 (`TreeStore`), the knob-derived
//! options, the quarantine sink over the plane's allocators + the data
//! WERO (`guard/pr.zig`, rtype 3 on every PR-capable data namespace,
//! acquired at the first remote enrollment, released at the last
//! departure; else the deferred-reclaim class, loud, no bypass knob), the
//! ONE mover's seam over the plane (`PlaneMover`: `pinBlockValidated`,
//! the three mint primitives, the stored-image read, the device write +
//! read-back, `republishBinding`, `freeBindings`), the `job_copy_buffers`
//! sheddable budget component whose shed hook pauses jobs loudly, and the
//! worker fibers on the executor (held at mount until admission opens).
//! Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const knobs = @import("../../core/knobs.zig");
const derive = @import("../../core/derive.zig");
const counters = @import("../../core/counters.zig");
const sync = @import("../../sync/sync.zig");
const budget = @import("../../mem/budget.zig");
const key = @import("../../meta/key.zig");
const tree = @import("../../meta/tree.zig");
const backend_mod = @import("../../meta/backend.zig");
const pr = @import("../../meta/guard/pr.zig");
const pr_linux = @import("../../meta/guard/pr_linux.zig");
const nvme_cmd = @import("../../meta/guard/nvme_cmd.zig");
const block_alloc = @import("../../data/block_alloc.zig");
const custody = @import("../../data/sym_planes/custody.zig");
const layout = @import("../../data/layout.zig");
const plane_mod = @import("../../data/plane.zig");
const records = @import("records.zig");
const fabric_mod = @import("fabric.zig");
const lease_mod = @import("lease.zig");
const wire_mod = @import("wire.zig");
const seams = @import("seams.zig");
const stats_mod = @import("stats.zig");
const mover = @import("../lifecycle/mover.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/jobs/coordinator is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const stats = stats_mod.stats;
const KvBackend = backend_mod.KvBackend;
const Fabric = fabric_mod.Fabric;
const DestTuple = records.DestTuple;
const StableIdentity = records.StableIdentity;

// ---------------------------------------------------------------------------
// The durable records on tree 0 of volume 0.
// ---------------------------------------------------------------------------

pub const TreeStore = struct {
    be: *KvBackend,

    pub fn store(self: *TreeStore) fabric_mod.Store {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: fabric_mod.Store.VTable = .{ .put = &put, .list = &list };

    fn put(ctx: *anyopaque, job_id: u64, bytes: *const [records.JobRecordV1.LEN]u8) fabric_mod.Store.Error!void {
        const self: *TreeStore = @ptrCast(@alignCast(ctx));
        const k = records.jobKey(job_id);
        const t = self.be.begin(@src()) catch return error.OutOfMemory;
        t.stagePut(key.TREE_CONTROL, k.slice(), bytes) catch {
            self.be.abandon(t);
            return error.OutOfMemory;
        };
        self.be.commit(t) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
    }

    fn list(ctx: *anyopaque, gpa: Allocator, out: *std.ArrayList([]u8)) fabric_mod.Store.Error!void {
        const self: *TreeStore = @ptrCast(@alignCast(ctx));
        const r = records.rangeOf(.job);
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        self.be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), fabric_mod.MAX_JOBS, &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        for (kvs.items) |kv| {
            if (records.jobIdOfKey(kv.key) == null) continue;
            try out.append(gpa, try gpa.dupe(u8, kv.value));
        }
    }
};

// ---------------------------------------------------------------------------
// Options from the registry.
// ---------------------------------------------------------------------------

pub const Options = struct {
    workers: u32,
    cpu_limit: u8,
    verify_permille: u16,
    fleet_jobs: bool,
    park_delay_ns: u64,
    hold_submission: bool,
    pre_publish_hold: bool,

    pub fn fromEnv(env: *const std.process.Environ.Map, facts: derive.Facts) Options {
        return .{
            .workers = knobs.optIntKnob("SQUEEZEFS_JOB_WORKERS", u32, env) orelse fabric_mod.defaultWorkers(facts.cpus),
            .cpu_limit = knobs.intKnob("SQUEEZEFS_JOB_CPU_LIMIT", u8, env),
            .verify_permille = knobs.intKnob("SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE", u16, env),
            .fleet_jobs = knobs.boolKnob("SQUEEZEFS_FLEET_JOBS", env),
            .park_delay_ns = @as(u64, knobs.intKnob("SQUEEZEFS_TEST_JOB_PARK_DELAY_MS", u32, env)) * std.time.ns_per_ms,
            .hold_submission = knobs.boolKnob("SQUEEZEFS_TEST_JOB_HOLD_SUBMISSION", env),
            .pre_publish_hold = knobs.boolKnob("SQUEEZEFS_TEST_MOVER_PRE_PUBLISH_HOLD", env),
        };
    }

    /// The copy window the budget component charges: 64 blocks per
    /// ACTUAL worker.
    pub fn copyWindowBytes(self: Options, block_size: u64) u64 {
        return @as(u64, self.workers) * records.SHARD_DEST_MAX * block_size;
    }
};

// ---------------------------------------------------------------------------
// The data WERO (rtype 3) over the PR-capable data namespaces.
// ---------------------------------------------------------------------------

pub const DataWero = struct {
    gpa: Allocator,
    devices: std.ArrayList(*pr_linux.Device) = .empty,
    clients: std.ArrayList(pr.Client) = .empty,
    /// False when any configured data namespace is not PR-capable: the
    /// deferred-reclaim class.
    all_capable: bool = true,

    pub fn init(gpa: Allocator) DataWero {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *DataWero) void {
        for (self.clients.items) |*c| c.deinit();
        self.clients.deinit(self.gpa);
        for (self.devices.items) |d| d.deinit();
        self.devices.deinit(self.gpa);
    }

    /// Resolve one data device path; a non-NVMe or char-node-less device
    /// marks the whole fence deferred-reclaim.
    pub fn addDevice(self: *DataWero, path: []const u8, our_key: u64) Allocator.Error!void {
        switch (pr_linux.resolve(self.gpa, path)) {
            .none => |why| {
                self.all_capable = false;
                log.warn("job fabric: data volume {s}: no PR primitive ({s}) — the remote fence runs in the deferred-reclaim class", .{ path, why.mode() });
            },
            .device => |d| {
                var c = try pr.Client.init(self.gpa, d.face(), d.nsid, our_key);
                c.rtype = nvme_cmd.RTYPE_WERO;
                try self.devices.append(self.gpa, d);
                try self.clients.append(self.gpa, c);
            },
        }
    }

    pub fn wero(self: *DataWero) wire_mod.Coordinator.Wero {
        return .{ .ctx = self, .acquire = &acquire, .release = &release };
    }

    fn acquire(ctx: *anyopaque) bool {
        const self: *DataWero = @ptrCast(@alignCast(ctx));
        if (!self.all_capable or self.clients.items.len == 0) return false;
        var ok = true;
        for (self.clients.items) |*c| {
            _ = c.registerLadder() catch {
                ok = false;
                continue;
            };
            c.acquire() catch {
                ok = false;
            };
        }
        return ok;
    }

    fn release(ctx: *anyopaque) void {
        const self: *DataWero = @ptrCast(@alignCast(ctx));
        for (self.clients.items) |*c| c.release() catch {};
    }

    /// Preempt `victim_key` on every namespace; the count landed.
    pub fn preempt(self: *DataWero, victim_key: u64) u32 {
        if (victim_key == 0) return 0;
        var landed: u32 = 0;
        for (self.clients.items) |*c| {
            c.preempt(victim_key) catch continue;
            landed += 1;
        }
        return landed;
    }
};

// ---------------------------------------------------------------------------
// The quarantine sink over the plane's allocators.
// ---------------------------------------------------------------------------

pub const AllocSink = struct {
    plane: *plane_mod.DataPlane,
    epochs: custody.DeadEpochSequence = .{},
    wero: ?*DataWero,
    /// The enrolled table (the holder's `pr_key`).
    coord: ?*wire_mod.Coordinator = null,

    pub fn sink(self: *AllocSink) lease_mod.QuarantineSink {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: lease_mod.QuarantineSink.VTable = .{ .mintEpoch = &mintEpoch, .admit = &admit, .preempt = &preemptFn, .release = &releaseFn, .isQuarantined = &isQuarantined };

    fn allocOf(self: *AllocSink, vol_tag: u64) ?*block_alloc.BlockAllocator {
        const v = self.plane.vols.byTag(vol_tag) orelse return null;
        return v.alloc;
    }
    fn mintEpoch(ctx: *anyopaque) u64 {
        const self: *AllocSink = @ptrCast(@alignCast(ctx));
        return self.epochs.mint();
    }
    fn admit(ctx: *anyopaque, d: DestTuple, epoch: u64) void {
        const self: *AllocSink = @ptrCast(@alignCast(ctx));
        const a = self.allocOf(d.vol_tag) orelse return;
        _ = a.quarantineBlock(d.block_idx, epoch);
    }
    fn preemptFn(ctx: *anyopaque, holder: StableIdentity) u32 {
        const self: *AllocSink = @ptrCast(@alignCast(ctx));
        const w = self.wero orelse return 0;
        const c = self.coord orelse return 0;
        return w.preempt(c.prKeyOf(holder) orelse 0);
    }
    fn releaseFn(ctx: *anyopaque, epoch: u64, _: u32) void {
        const self: *AllocSink = @ptrCast(@alignCast(ctx));
        var out: [records.SHARD_DEST_MAX]u64 = undefined;
        for (self.plane.vols.rows[0..self.plane.vols.count]) |*r| {
            const a = r.*.?.alloc;
            _ = a.releaseQuarantine(epoch, &.{}, &out);
        }
    }
    fn isQuarantined(ctx: *anyopaque, d: DestTuple) bool {
        const self: *AllocSink = @ptrCast(@alignCast(ctx));
        const a = self.allocOf(d.vol_tag) orelse return false;
        return a.isQuarantined(d.block_idx);
    }
};

// ---------------------------------------------------------------------------
// The ONE mover's seam over the plane.
// ---------------------------------------------------------------------------

pub const PlaneMover = struct {
    plane: *plane_mod.DataPlane,
    io: Io,
    /// `SQUEEZEFS_TEST_MOVER_PRE_PUBLISH_HOLD`: park before the first
    /// publish until `release_hold` fires.
    pre_publish_hold: bool = false,
    release_hold: sync.Notify = .init,
    held: std.atomic.Value(bool) = .init(false),

    pub fn seam(self: *PlaneMover) mover.Seam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: mover.Seam.VTable = .{ .chunk = &chunk, .isOpenPack = &isOpenPack, .sealPack = &sealPack, .quiescent = &quiescent, .pin = &pin, .refcount = &refcount, .unpin = &unpin, .readImage = &readImage, .allocate = &allocate, .abandonUnpublished = &abandon, .write = &write, .readBack = &readBack, .publishBlock = &publishBlock, .incrementRefcount = &incRef, .releaseRef = &releaseRef, .republish = &republish, .freeBinding = &freeBinding, .kill = &kill };

    fn self_(ctx: *anyopaque) *PlaneMover {
        return @ptrCast(@alignCast(ctx));
    }
    fn allocOf(self: *PlaneMover, vol_tag: u64) ?*block_alloc.BlockAllocator {
        const v = self.plane.vols.byTag(vol_tag) orelse return null;
        return v.alloc;
    }
    fn chunk(ctx: *anyopaque) u64 {
        return self_(ctx).plane.chunk;
    }
    fn isOpenPack(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
        const self = self_(ctx);
        const p = &(self.plane.packer orelse return false);
        const op = p.openPack(.{ .vol_tag = vol_tag, .scope = self.plane.scope }) orelse return false;
        return op.base == idx * self.plane.chunk and !op.isSealed();
    }
    fn sealPack(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        const self = self_(ctx);
        if (self.plane.packer) |*p| _ = p.sealBlock(vol_tag, idx * self.plane.chunk);
    }
    fn quiescent(ctx: *anyopaque, ino: u64, block_index: u32) bool {
        return self_(ctx).plane.moverQuiescent(ino, block_index);
    }
    fn pin(ctx: *anyopaque, vol_tag: u64, idx: u64) mover.PinVerdict {
        const a = self_(ctx).allocOf(vol_tag) orelse return .refused;
        return switch (a.pinBlockValidated(idx)) {
            .pinned => .pinned,
            .pinned_unstable => .pinned_unstable,
            .refused => .refused,
        };
    }
    fn refcount(ctx: *anyopaque, vol_tag: u64, idx: u64) ?u32 {
        const a = self_(ctx).allocOf(vol_tag) orelse return null;
        return a.refcount(idx);
    }
    fn unpin(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        const a = self_(ctx).allocOf(vol_tag) orelse return;
        a.unpin(idx);
    }
    fn readImage(ctx: *anyopaque, vol_tag: u64, idx: u64, out: []u8) mover.SeamError!usize {
        const self = self_(ctx);
        const n: usize = @intCast(self.plane.chunk);
        self.plane.deviceSeam().read(vol_tag, idx * self.plane.chunk, out[0..n]) catch return error.Io;
        return n;
    }
    fn allocate(ctx: *anyopaque, pick: mover.DestPick, src_vol: u64, src_idx: u64) mover.SeamError!?mover.Minted {
        const self = self_(ctx);
        switch (pick) {
            .balance => |p| {
                var tag: ?u64 = null;
                if (p.dest_hint != 0 and p.dest_hint != src_vol) if (self.plane.rt.volume(p.dest_hint)) |v| if (v.placementEligible()) {
                    tag = p.dest_hint;
                };
                if (tag == null) {
                    var best: u64 = std.math.maxInt(u64);
                    for (self.plane.rt.volumesLive()) |*v| {
                        if (v.id.tag == src_vol or !v.placementEligible()) continue;
                        if (v.fill() < best) {
                            best = v.fill();
                            tag = v.id.tag;
                        }
                    }
                }
                const t = tag orelse return error.Io;
                const a = self.allocOf(t) orelse return error.Io;
                const m = a.allocateBlock() catch |e| return switch (e) {
                    error.StorageFull => null,
                    else => error.Io,
                };
                return .{ .vol_tag = t, .idx = m.idx, .offset = m.offset, .incarnation = m.stamp };
            },
            .compact_low => {
                const a = self.allocOf(src_vol) orelse return error.Io;
                const idx = (a.allocateBlockBelow(src_idx) catch return error.Io) orelse return null;
                return .{ .vol_tag = src_vol, .idx = idx, .offset = a.offsetOf(idx), .incarnation = a.stampOf(idx) };
            },
            .backend_ascending => |p| {
                const a = self.allocOf(p.vol_tag) orelse return error.Io;
                const m = a.allocateBlockAtOrAbove(p.floor.*) catch |e| return switch (e) {
                    error.StorageFull => null,
                    else => error.Io,
                };
                p.floor.* = m.idx + 1;
                return .{ .vol_tag = p.vol_tag, .idx = m.idx, .offset = m.offset, .incarnation = m.stamp };
            },
        }
    }
    fn abandon(ctx: *anyopaque, m: mover.Minted) void {
        const a = self_(ctx).allocOf(m.vol_tag) orelse return;
        a.abandonUnpublished(m.idx);
    }
    fn write(ctx: *anyopaque, m: mover.Minted, image: []const u8) mover.SeamError!void {
        _ = self_(ctx).plane.deviceSeam().write(m.vol_tag, m.offset, image) catch return error.Io;
    }
    fn readBack(ctx: *anyopaque, m: mover.Minted, out: []u8) mover.SeamError!usize {
        const self = self_(ctx);
        const n: usize = @intCast(self.plane.chunk);
        self.plane.deviceSeam().read(m.vol_tag, m.offset, out[0..n]) catch return error.Io;
        return n;
    }
    fn publishBlock(ctx: *anyopaque, m: mover.Minted) void {
        const a = self_(ctx).allocOf(m.vol_tag) orelse return;
        a.publishBlock(m.idx);
    }
    fn incRef(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
        const a = self_(ctx).allocOf(vol_tag) orelse return false;
        return a.incrementRefcount(idx);
    }
    fn releaseRef(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        const a = self_(ctx).allocOf(vol_tag) orelse return;
        a.unpin(idx);
    }
    fn republish(ctx: *anyopaque, ino: u64, block_index: u32, expected: layout.Mapping, new: layout.Mapping) mover.RepublishError!void {
        const self = self_(ctx);
        const proof: layout.DmaProof = .{ .vol_tag = new.vol_tag, .offset = new.offset, .len = self.plane.chunk, .completed_ns = clock.monotonicNs() };
        self.plane.republishBinding(self.io, ino, block_index, expected, new, proof) catch |e| return switch (e) {
            error.Superseded => error.Superseded,
            error.OutOfMemory => error.OutOfMemory,
            error.InputOutput => error.Io,
        };
    }
    fn freeBinding(ctx: *anyopaque, old: layout.Mapping) void {
        const self = self_(ctx);
        self.plane.freeBindings(self.io, &.{old});
    }
    /// The plane never crashes on cue; the pre-publish HOLD seam parks here.
    fn kill(ctx: *anyopaque, at: mover.KillPoint) mover.SeamError!void {
        const self = self_(ctx);
        if (at == .after_raise and self.pre_publish_hold) {
            self.held.store(true, .release);
            var n = self.release_hold.notified();
            n.waitUncancelable(self.io);
            self.held.store(false, .release);
        }
    }
};

// ---------------------------------------------------------------------------
// The census walk, the lifecycle world and the drain seam over the plane.
// ---------------------------------------------------------------------------

const census = @import("../lifecycle/census.zig");
const lattice = @import("../lifecycle/lattice.zig");
const drain_mod = @import("../lifecycle/drain.zig");
const preflight = @import("preflight.zig");
const set_config = @import("../../data/set_config.zig");
const router = @import("../../data/router.zig");
const data_superblock = @import("../../data/data_superblock.zig");
const geometry = @import("../../meta/geometry.zig");

/// `census.LayoutWalk` over the plane's binding walk.
pub const PlaneWalk = struct {
    plane: *plane_mod.DataPlane,

    pub fn walk(self: *PlaneWalk) census.LayoutWalk {
        return .{ .ctx = self, .walk = &run };
    }

    const Visit = struct {
        v: census.LayoutWalk.Visitor,
        fn visit(ctx: *@This(), ino: u64, block_index: u32, m: layout.Mapping) plane_mod.Error!void {
            ctx.v.visit(ctx.v.ctx, .{ .ino = ino, .block_index = block_index, .mapping = m }) catch return error.OutOfMemory;
        }
    };

    fn run(ctx: *anyopaque, v: census.LayoutWalk.Visitor) error{ Io, OutOfMemory }!void {
        const self: *PlaneWalk = @ptrCast(@alignCast(ctx));
        var vis: Visit = .{ .v = v };
        self.plane.forEachBinding(*Visit, &vis, Visit.visit) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
    }
};

/// `lattice.World` over the ONLINE set: the manager predicate is the
/// gate, tree 0 of volume 0 holds the record, the plane's router is the
/// placement runtime.
pub const PlaneWorld = struct {
    be: *KvBackend,
    plane: *plane_mod.DataPlane,
    fabric: *Fabric,
    io: Io,
    random: *const fn () u64,

    pub fn world(self: *PlaneWorld) lattice.World {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: lattice.World.VTable = .{ .gate = &gate, .loadConfig = &loadConfig, .stampLifecycleBit = &stamp, .formatDevice = &formatDevice, .commit = &commit, .publishPlacement = &publish, .cancelEvacuations = &cancelEvac, .now_s = &nowS, .random = &randomFn };

    fn self_(ctx: *anyopaque) *PlaneWorld {
        return @ptrCast(@alignCast(ctx));
    }
    fn gate(ctx: *anyopaque) lattice.Error!void {
        if (!self_(ctx).fabric.is_coordinator.load(.acquire)) return error.LiveWriter;
    }
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
        // The online add formats the device BEFORE the record through the
        // same writer the offline verb uses (device-before-record).
        const self = self_(ctx);
        const cfg = loadConfig(ctx, self.plane.gpa) catch return error.Io;
        var c = cfg;
        defer c.deinit(self.plane.gpa);
        const sb: data_superblock.DataSuperblock = .{
            .block_size = @intCast(self.plane.block_size),
            .chunk_size = c.chunk_size,
            .compression = c.compression,
            .encryption = c.encryption,
            .volume_len = dev.volume_len,
            .vol_tag = vol_tag,
            .set_uuid = self.be.setUuid(),
            .device_uuid = dev.uuid,
            .features_incompat = 0,
        };
        data_superblock.linux_dev.format(self.plane.gpa, dev.uri, sb, 1) catch return error.Io;
    }
    fn commit(ctx: *anyopaque, cfg: *const set_config.Config, job: ?records.JobRecord) lattice.Error!void {
        const self = self_(ctx);
        const gpa = self.plane.gpa;
        const bytes = set_config.encode(gpa, cfg, @intCast(geometry.recordValueCap(self.be.nodeSize()))) catch return error.Io;
        defer gpa.free(bytes);
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
        }
        self.be.commit(t) catch return error.Io;
        self.be.fsync() catch return error.Io;
    }
    fn publish(ctx: *anyopaque, vol_tag: u64, state: set_config.State) lattice.Error!void {
        const self = self_(ctx);
        const to: router.VolumeState = switch (state) {
            .active => .active,
            .draining => .draining,
            .retired => .retired,
            .disabled => .disabled,
        };
        self.plane.rt.setState(vol_tag, to) catch return error.UnknownVolume;
        self.plane.rt.refreshPlacement() catch return error.OutOfMemory;
    }
    fn cancelEvac(ctx: *anyopaque, vol_tag: u64) lattice.Error!void {
        const self = self_(ctx);
        var list: std.ArrayList(records.JobRecord) = .empty;
        defer list.deinit(self.plane.gpa);
        self.fabric.listJobs(self.io, self.plane.gpa, &list) catch return error.OutOfMemory;
        for (list.items) |j| if (j.job_type == .evacuate_volume and j.arg0 == vol_tag and !j.isTerminal()) {
            self.fabric.cancel(self.io, j.job_id) catch {};
        };
    }
    fn nowS(_: *anyopaque) u64 {
        return clock.realtimeCoarseS();
    }
    fn randomFn(ctx: *anyopaque) u64 {
        return self_(ctx).random();
    }
};

/// `drain.DrainSeam` over the plane: the census walk, the mover, the
/// router's rows, the reclaimer's synchronous drain, the retire through
/// the online world.
pub const PlaneDrain = struct {
    plane: *plane_mod.DataPlane,
    io: Io,
    walk_: PlaneWalk,
    mover_: PlaneMover,
    world_: PlaneWorld,
    workers: u64,
    rate: *const fn () u64,

    pub fn seam(self: *PlaneDrain) drain_mod.DrainSeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: drain_mod.DrainSeam.VTable = .{ .walk = &walk, .mover = &moverFn, .volumeState = &volumeState, .usedBlocks = &usedBlocks, .rows = &rows, .drainReclaim = &drainReclaim, .retire = &retire, .blockSize = &blockSize, .writeRateBps = &writeRate, .workers = &workersFn };

    fn self_(ctx: *anyopaque) *PlaneDrain {
        return @ptrCast(@alignCast(ctx));
    }
    fn walk(ctx: *anyopaque) census.LayoutWalk {
        return self_(ctx).walk_.walk();
    }
    fn moverFn(ctx: *anyopaque) mover.Seam {
        return self_(ctx).mover_.seam();
    }
    fn volumeState(ctx: *anyopaque, vol_tag: u64) ?set_config.State {
        const v = self_(ctx).plane.rt.volume(vol_tag) orelse return null;
        return switch (v.state) {
            .active => .active,
            .draining => .draining,
            .retired => .retired,
            .disabled => .disabled,
        };
    }
    fn usedBlocks(ctx: *anyopaque, vol_tag: u64) u64 {
        const v = self_(ctx).plane.vols.byTag(vol_tag) orelse return 0;
        return v.alloc.usedBlocks();
    }
    fn rows(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]preflight.SurvivorRow {
        const self = self_(ctx);
        const live = self.plane.rt.volumesLive();
        const out = try gpa.alloc(preflight.SurvivorRow, live.len);
        for (live, out) |*v, *r| r.* = .{ .vol_tag = v.id.tag, .eligible = v.placementEligible(), .capacity_bytes = v.capacity_bytes, .used_bytes = v.used_bytes };
        return out;
    }
    fn drainReclaim(ctx: *anyopaque) void {
        const self = self_(ctx);
        _ = self.plane.reclaimer.drainSync(self.io);
    }
    fn retire(ctx: *anyopaque, vol_tag: u64) error{Io}!void {
        const self = self_(ctx);
        lattice.retire(self.plane.gpa, self.world_.world(), vol_tag) catch return error.Io;
    }
    fn blockSize(ctx: *anyopaque) u64 {
        return self_(ctx).plane.chunk;
    }
    fn writeRate(ctx: *anyopaque) u64 {
        return self_(ctx).rate();
    }
    fn workersFn(ctx: *anyopaque) u64 {
        return self_(ctx).workers;
    }
};

// ---------------------------------------------------------------------------
// The mount's node: the fabric + the shard table + the wire side + the
// budget component + the worker fibers.
// ---------------------------------------------------------------------------

pub const Node = struct {
    gpa: Allocator,
    io: Io,
    opts: Options,
    store: TreeStore,
    fabric: Fabric,
    sink: AllocSink,
    table: lease_mod.ShardTable,
    coord: wire_mod.Coordinator,
    verify: PlaneVerify,
    workers: sync.TaskSet = .init,
    component_registered: bool = false,
    block_size: u64,

    const PlaneVerify = struct {
        plane: *plane_mod.DataPlane,
        fn vr(self: *PlaneVerify) wire_mod.VerifyRead {
            return .{ .ctx = self, .read_xxh3 = &read };
        }
        fn read(ctx: *anyopaque, d: DestTuple, len: u64) error{Io}!u64 {
            const self: *PlaneVerify = @ptrCast(@alignCast(ctx));
            const buf = self.plane.gpa.alloc(u8, @intCast(len)) catch return error.Io;
            defer self.plane.gpa.free(buf);
            self.plane.deviceSeam().read(d.vol_tag, d.block_idx * self.plane.chunk, buf) catch return error.Io;
            return @import("../../core/checksum.zig").xxh3(buf);
        }
    };

    pub const Init = struct {
        backend: *KvBackend,
        plane: *plane_mod.DataPlane,
        identity: StableIdentity,
        pid: u32,
        wero: ?*DataWero,
        /// Held at mount until the allocation lease armed over a quiescent
        /// allocator; offline verbs and tests start open.
        held: bool,
        random: *const fn () u64,
    };

    pub fn create(gpa: Allocator, io: Io, opts: Options, in: Init) Allocator.Error!*Node {
        const n = try gpa.create(Node);
        errdefer gpa.destroy(n);
        n.* = .{
            .gpa = gpa,
            .io = io,
            .opts = opts,
            .store = .{ .be = in.backend },
            .fabric = undefined,
            .sink = .{ .plane = in.plane, .wero = in.wero },
            .table = undefined,
            .coord = undefined,
            .verify = .{ .plane = in.plane },
            .block_size = in.plane.chunk,
        };
        n.fabric = Fabric.init(gpa, n.store.store(), .{ .identity = in.identity, .pid = in.pid, .default_throttle_pct = opts.cpu_limit, .park_delay_ns = opts.park_delay_ns, .held = in.held, .random = in.random });
        n.table = lease_mod.ShardTable.init(seams.LEASE_TTL_MS, n.sink.sink(), null);
        n.coord = wire_mod.Coordinator.init(gpa, io, &n.fabric, &n.table, .{ .verify = n.verify.vr(), .channel = .plaintext, .configured_permille = opts.verify_permille, .wero = if (in.wero) |w| w.wero() else null });
        n.sink.coord = &n.coord;
        return n;
    }

    /// The sheddable `job_copy_buffers` component: sized from the ACTUAL
    /// worker count; Red's shed pauses every running job loudly.
    pub fn budgetComponent(self: *Node) budget.Component {
        return budget.Component.init("job_copy_buffers", 0, self.opts.copyWindowBytes(self.block_size), .{ .gauge = &stats.job_copy_buffer_bytes }, .{ .ctx = self, .call = &shed });
    }

    fn shed(ctx: *anyopaque, target: u64) void {
        const self: *Node = @ptrCast(@alignCast(ctx));
        self.fabric.shedTo(self.io, target);
    }

    /// Adopt the durable records, register the job verbs on the manager's
    /// service and spawn the local pool (claims nothing while held).
    pub fn start(self: *Node, svc: ?*@import("../../wire/service.zig").Service) !void {
        _ = try self.fabric.adopt(self.io);
        if (svc) |s| self.coord.register(s);
        var i: u32 = 0;
        while (i < self.opts.workers) : (i += 1) try self.workers.spawn(self.io, workerMain, .{self});
        log.info("job fabric: {d} local worker(s), default throttle {d} %, verify {d} ‰ (plaintext pins 1000), fleet jobs {s}", .{ self.opts.workers, self.opts.cpu_limit, self.coord.permille(), if (self.opts.fleet_jobs) "on" else "off" });
    }

    fn workerMain(self: *Node) Io.Cancelable!void {
        self.fabric.serve(self.io, false);
    }

    pub fn openAdmission(self: *Node) void {
        self.fabric.openAdmission(self.io);
    }

    pub fn setManager(self: *Node, held: bool) void {
        self.fabric.setCoordinator(held);
    }

    pub fn admin(self: *Node) seams.AdminJob {
        return seams.AdminJob.forFabric(&self.fabric);
    }

    pub fn stop(self: *Node, svc: ?*@import("../../wire/service.zig").Service) void {
        if (svc) |s| self.coord.unregister(s);
        self.fabric.stop(self.io);
        self.workers.wait(self.io) catch {};
    }

    pub fn destroy(self: *Node) void {
        self.fabric.deinit();
        self.gpa.destroy(self);
    }
};

// ---------------------------------------------------------------------------
// Tests (Linux; the device-free parts).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "Options.fromEnv: the derived worker count, the defaults, the seams; the copy window is 64 blocks per actual worker" {
    var env: std.process.Environ.Map = .init(testing.allocator);
    defer env.deinit();
    const o = Options.fromEnv(&env, derive.Facts.synthetic(16, 64 << 30));
    try testing.expectEqual(@as(u32, 4), o.workers);
    try testing.expectEqual(@as(u8, 100), o.cpu_limit);
    try testing.expectEqual(@as(u16, 1000), o.verify_permille);
    try testing.expect(o.fleet_jobs and !o.hold_submission and !o.pre_publish_hold);
    try testing.expectEqual(@as(u64, 0), o.park_delay_ns);
    try testing.expectEqual(@as(u64, 4 * 64 * (4 << 20)), o.copyWindowBytes(4 << 20));
    try env.put("SQUEEZEFS_JOB_WORKERS", "3");
    try env.put("SQUEEZEFS_JOB_CPU_LIMIT", "25");
    try env.put("SQUEEZEFS_TEST_JOB_PARK_DELAY_MS", "250");
    const o2 = Options.fromEnv(&env, derive.Facts.synthetic(16, 64 << 30));
    try testing.expectEqual(@as(u32, 3), o2.workers);
    try testing.expectEqual(@as(u8, 25), o2.cpu_limit);
    try testing.expectEqual(@as(u64, 250 * std.time.ns_per_ms), o2.park_delay_ns);
}
