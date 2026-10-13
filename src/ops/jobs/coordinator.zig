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
const write_seams = @import("../../data/write_seams.zig");
const records = @import("records.zig");
const fabric_mod = @import("fabric.zig");
const lease_mod = @import("lease.zig");
const wire_mod = @import("wire.zig");
const wire_tls = @import("../../wire/tls.zig");
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
    /// The plane's volume table (each row's allocator).
    vols: *const write_seams.Volumes,
    epochs: custody.DeadEpochSequence = .{},
    wero: ?*DataWero,
    /// The enrolled table (the holder's `pr_key`).
    coord: ?*wire_mod.Coordinator = null,

    pub fn sink(self: *AllocSink) lease_mod.QuarantineSink {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: lease_mod.QuarantineSink.VTable = .{ .mintEpoch = &mintEpoch, .admit = &admit, .preempt = &preemptFn, .release = &releaseFn, .isQuarantined = &isQuarantined };

    fn allocOf(self: *AllocSink, vol_tag: u64) ?*block_alloc.BlockAllocator {
        const v = self.vols.byTag(vol_tag) orelse return null;
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
        for (self.vols.rows[0..self.vols.n()]) |*r| {
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
    /// A reference release THROUGH THE LADDER (the mover's law: the pin is
    /// the last source reference iff every referencer published, and a
    /// refused referencer's raised destination reference leaves the same
    /// way) — `reclaimer.freeBlock` decrements and, at 0, runs the terminal
    /// free; the allocator's bare `unpin` fires `block_live_free_refusals`
    /// on a terminal release by design.
    fn releaseThroughLadder(self: *PlaneMover, vol_tag: u64, idx: u64) void {
        const v = self.plane.vols.byTag(vol_tag) orelse return;
        _ = self.plane.reclaimer.freeBlock(self.io, @intCast(v.ordinal), idx, v.alloc.stampOf(idx));
    }
    fn unpin(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        self_(ctx).releaseThroughLadder(vol_tag, idx);
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
        self_(ctx).releaseThroughLadder(vol_tag, idx);
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
const lifecycle_stats = @import("../lifecycle/stats.zig").stats;
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
        // The generation the membership grants carry from now on (a
        // joiner behind it pulls the record at its next renewal).
        lifecycle_stats.volume_config_generation.set(cfg.config_generation);
    }
    fn publish(ctx: *anyopaque, vol_tag: u64, state: set_config.State) lattice.Error!void {
        const self = self_(ctx);
        const to: router.VolumeState = switch (state) {
            .active => .active,
            .draining => .draining,
            .retired => .retired,
            .disabled => .disabled,
        };
        self.plane.setVolumeState(vol_tag, to) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.UnknownVolume,
        };
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
    rate: WriteRate,

    /// The preflight's foreground write-rate term (the node's estimator).
    pub const WriteRate = struct {
        ctx: *anyopaque,
        bps: *const fn (ctx: *anyopaque) u64,
    };

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
    /// The victim's allocated blocks MINUS the one the plane reserves
    /// forever (index 0, the data superblock chunk: `recoverBlock(0)` at
    /// open) — the drain's convergence word is "no FILE block left";
    /// counting the reserved chunk re-planned forever against a census
    /// that can never name it (rung 8: 7,050 re-plans in 25 minutes).
    fn usedBlocks(ctx: *anyopaque, vol_tag: u64) u64 {
        const v = self_(ctx).plane.vols.byTag(vol_tag) orelse return 0;
        return v.alloc.usedBlocks() -| plane_mod.DataPlane.RESERVED_BLOCKS;
    }
    fn rows(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]preflight.SurvivorRow {
        const self = self_(ctx);
        try self.plane.refreshCensus();
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
        const r = self_(ctx).rate;
        return r.bps(r.ctx);
    }
    fn workersFn(ctx: *anyopaque) u64 {
        return self_(ctx).workers;
    }
};

// ---------------------------------------------------------------------------
// The defrag seams over the plane: the data arms (D2/D1 through the ONE
// mover) and the `--pack` compactor over the packer.
// ---------------------------------------------------------------------------

const defrag_bodies = @import("../defrag/bodies.zig");
const defrag_plan = @import("../defrag/plan.zig");
const pack_compact = @import("../defrag/pack_compact.zig");
const pack = @import("../../data/pack.zig");
const rekey_mod = @import("../defrag/rekey.zig");
const transform = @import("../../data/transform.zig");
const keys = @import("../keys.zig");

/// `pack_compact.PackSeam` over the plane + its packer: a window the
/// compactor prepared lands at once (`Packer.landWindow` — the slot is a
/// committed tenant whose references are the referencers' records); the
/// raises and releases keep the pack's committed count beside the
/// allocator's refcount so the seal's pin arithmetic holds; the windows in
/// flight are a bounded table (one per worker's current victim).
pub const PlanePack = struct {
    plane: *plane_mod.DataPlane,
    io: Io,
    mover_: *PlaneMover,
    lock: jc.SpinLock = .{},
    /// `(vol_tag, base)` → the open pack a prepared window landed in.
    windows: [WINDOWS_MAX]?Window = @splat(null),

    /// One window in flight per worker at the compactor's pipeline width
    /// 1 (`throttle.MOVER_PIPELINE_WIDTH` bounds the movers; the compactor
    /// is order-dependent), so the shipped worker ceiling (8) × the
    /// pipeline width (4) bounds the table with room for the victims a
    /// stopped pass left landed.
    pub const WINDOWS_MAX: usize = 8 * 4 * 2;

    const Window = struct { vol_tag: u64, base: u64, tenant: pack.Tenant };
    const jc = @import("../../meta/journal_core.zig");

    pub fn seam(self: *PlanePack) pack_compact.PackSeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: pack_compact.PackSeam.VTable = .{ .chunk = &chunk, .isOpenPack = &isOpenPack, .quiescent = &quiescent, .pin = &pin, .unpin = &unpin, .readWindow = &readWindow, .prepare = &prepare, .abandonWindow = &abandonWindow, .incrementRefcount = &incRef, .releaseRef = &releaseRef, .republish = &republish, .freeBinding = &freeBinding };

    fn self_(ctx: *anyopaque) *PlanePack {
        return @ptrCast(@alignCast(ctx));
    }
    fn chunk(ctx: *anyopaque) u64 {
        return self_(ctx).plane.chunk;
    }
    fn isOpenPack(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
        return PlaneMover.vt.isOpenPack(self_(ctx).mover_, vol_tag, idx);
    }
    fn quiescent(ctx: *anyopaque, ino: u64, block_index: u32) bool {
        return self_(ctx).plane.moverQuiescent(ino, block_index);
    }
    fn pin(ctx: *anyopaque, vol_tag: u64, idx: u64) mover.PinVerdict {
        return PlaneMover.vt.pin(self_(ctx).mover_, vol_tag, idx);
    }
    fn unpin(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        PlaneMover.vt.unpin(self_(ctx).mover_, vol_tag, idx);
    }
    fn readWindow(ctx: *anyopaque, m: layout.Mapping, out: []u8) pack_compact.SeamError!usize {
        const self = self_(ctx);
        const bytes = self.plane.readMappingImage(m, out) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        return bytes.len;
    }
    fn windowOf(self: *PlanePack, vol_tag: u64, base: u64) ?Window {
        self.lock.lock();
        defer self.lock.unlock();
        for (self.windows) |w| if (w) |x| if (x.vol_tag == vol_tag and x.base == base) return x;
        return null;
    }
    fn forget(self: *PlanePack, vol_tag: u64, base: u64) void {
        self.lock.lock();
        defer self.lock.unlock();
        for (&self.windows) |*w| if (w.*) |x| if (x.vol_tag == vol_tag and x.base == base) {
            w.* = null;
            return;
        };
    }
    fn prepare(ctx: *anyopaque, image: []const u8) pack_compact.SeamError!pack_compact.Placed {
        const self = self_(ctx);
        const plane = self.plane;
        const p = &(plane.packer orelse return error.Io);
        const tag = (plane.rt.pickVolume() catch return error.OutOfMemory) orelse return error.StorageFull;
        const stored = plane.transformWrite(tag, image) catch return error.Io;
        defer stored.release(plane);
        const prepared = (p.prepare(self.io, .{ .vol_tag = tag, .scope = plane.scope }, stored.bytes, clock.monotonicNs()) catch |e| return switch (e) {
            error.StorageFull => error.StorageFull,
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        }) orelse return error.StorageFull;
        const t = prepared.tenant;
        {
            self.lock.lock();
            defer self.lock.unlock();
            var slot: ?*?Window = null;
            for (&self.windows) |*w| if (w.* == null) {
                slot = w;
                break;
            };
            const s = slot orelse {
                self.lock.unlock();
                p.abandonLandedWindow(t);
                self.lock.lock();
                return error.Io;
            };
            s.* = .{ .vol_tag = t.pack.vol_tag, .base = t.pack.base, .tenant = t };
        }
        p.landWindow(t);
        return .{ .vol_tag = t.pack.vol_tag, .block_idx = t.pack.base / plane.chunk, .offset = t.pack.base, .incarnation = t.pack.incarnation, .rel_off = @intCast(t.off) };
    }
    fn abandonWindow(ctx: *anyopaque, placed: pack_compact.Placed) void {
        const self = self_(ctx);
        const p = &(self.plane.packer orelse return);
        const w = self.windowOf(placed.vol_tag, placed.offset) orelse return;
        self.forget(placed.vol_tag, placed.offset);
        p.abandonLandedWindow(w.tenant);
    }
    /// A reference step on a landed window's block: the packer pairs it
    /// with the pack's tenant word under the pin lock while the pack is
    /// open (the seal's arithmetic), else it runs alone.
    const RefStep = struct {
        self: *PlanePack,
        vol_tag: u64,
        idx: u64,
        fn raise(c: *anyopaque) bool {
            const s: *RefStep = @ptrCast(@alignCast(c));
            return PlaneMover.vt.incrementRefcount(s.self.mover_, s.vol_tag, s.idx);
        }
        fn release(c: *anyopaque) void {
            const s: *RefStep = @ptrCast(@alignCast(c));
            PlaneMover.vt.releaseRef(s.self.mover_, s.vol_tag, s.idx);
        }
    };
    fn incRef(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
        const self = self_(ctx);
        var step: RefStep = .{ .self = self, .vol_tag = vol_tag, .idx = idx };
        if (self.plane.packer) |*p| if (self.windowOf(vol_tag, idx * self.plane.chunk)) |w| return p.tenantRaised(w.tenant.pack, &step, RefStep.raise);
        return RefStep.raise(&step);
    }
    fn releaseRef(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        const self = self_(ctx);
        var step: RefStep = .{ .self = self, .vol_tag = vol_tag, .idx = idx };
        if (self.plane.packer) |*p| if (self.windowOf(vol_tag, idx * self.plane.chunk)) |w| return p.tenantReleased(w.tenant.pack, &step, RefStep.release);
        RefStep.release(&step);
    }
    fn republish(ctx: *anyopaque, ino: u64, block_index: u32, expected: layout.Mapping, new: layout.Mapping) mover.RepublishError!void {
        return PlaneMover.vt.republish(self_(ctx).mover_, ino, block_index, expected, new);
    }
    fn freeBinding(ctx: *anyopaque, old: layout.Mapping) void {
        PlaneMover.vt.freeBinding(self_(ctx).mover_, old);
    }
};

/// `bodies.DataSeam` over the plane (the D2/D1 planner's rows and refs,
/// the mover, the pack face, the lever, the open packs).
pub const PlaneDefrag = struct {
    plane: *plane_mod.DataPlane,
    io: Io,
    mover_: *PlaneMover,
    pack_: PlanePack,

    pub fn seam(self: *PlaneDefrag) defrag_bodies.DataSeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: defrag_bodies.DataSeam.VTable = .{ .rows = &rows, .refs = &refs, .mover = &moverFn, .pack = &packFn, .chunk = &chunk, .packingOn = &packingOn, .packMaxSlot = &packMaxSlot, .openPacks = &openPacks, .openRoomBytes = &openRoomBytes, .drainReclaim = &drainReclaim };

    fn self_(ctx: *anyopaque) *PlaneDefrag {
        return @ptrCast(@alignCast(ctx));
    }
    fn rows(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]defrag_plan.VolumeRow {
        const self = self_(ctx);
        try self.plane.refreshCensus();
        const live = self.plane.rt.volumesLive();
        const out = try gpa.alloc(defrag_plan.VolumeRow, live.len);
        for (live, out) |*v, *r| {
            const a = self.plane.vols.byTag(v.id.tag);
            r.* = .{ .vol_tag = v.id.tag, .eligible = v.placementEligible(), .frontier = if (a) |x| x.alloc.highestBlockIndex() else 0 };
        }
        return out;
    }
    fn refs(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]census.Ref {
        const self = self_(ctx);
        var list: std.ArrayList(census.Ref) = .empty;
        errdefer list.deinit(gpa);
        const Collect = struct {
            gpa: Allocator,
            list: *std.ArrayList(census.Ref),
            fn visit(c: *@This(), ino: u64, bi: u32, m: layout.Mapping) plane_mod.Error!void {
                try c.list.append(c.gpa, .{ .ino = ino, .block_index = bi, .mapping = m });
            }
        };
        var col: Collect = .{ .gpa = gpa, .list = &list };
        self.plane.forEachBinding(*Collect, &col, Collect.visit) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            // A torn walk plans nothing this pass (the next pass re-plans).
            else => blk: {
                log.warn("defrag: the layout walk failed ({s}) — the pass plans nothing", .{@errorName(e)});
                list.clearRetainingCapacity();
                break :blk list.toOwnedSlice(gpa);
            },
        };
        return list.toOwnedSlice(gpa);
    }
    fn moverFn(ctx: *anyopaque) mover.Seam {
        return self_(ctx).mover_.seam();
    }
    fn packFn(ctx: *anyopaque) pack_compact.PackSeam {
        return self_(ctx).pack_.seam();
    }
    fn chunk(ctx: *anyopaque) u64 {
        return self_(ctx).plane.chunk;
    }
    fn packingOn(ctx: *anyopaque) bool {
        const p = &(self_(ctx).plane.packer orelse return false);
        return p.policy.enabled;
    }
    fn packMaxSlot(ctx: *anyopaque) u64 {
        const self = self_(ctx);
        const p = &(self.plane.packer orelse return self.plane.chunk / 2);
        return p.policy.max_slot;
    }
    fn openPacks(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]defrag_plan.BlockKey {
        const self = self_(ctx);
        const p = &(self.plane.packer orelse return try gpa.alloc(defrag_plan.BlockKey, 0));
        var placed: std.ArrayList(pack.Placed) = .empty;
        defer placed.deinit(gpa);
        _ = try p.openBlocks(gpa, &placed);
        const out = try gpa.alloc(defrag_plan.BlockKey, placed.items.len);
        for (placed.items, out) |pl, *o| o.* = .{ .vol_tag = pl.vol_tag, .block_idx = pl.offset / self.plane.chunk };
        return out;
    }
    fn openRoomBytes(ctx: *anyopaque) u64 {
        const self = self_(ctx);
        const p = &(self.plane.packer orelse return 0);
        var placed: std.ArrayList(pack.Placed) = .empty;
        defer placed.deinit(self.plane.gpa);
        return p.openBlocks(self.plane.gpa, &placed) catch 0;
    }
    fn drainReclaim(ctx: *anyopaque) void {
        const self = self_(ctx);
        _ = self.plane.reclaimer.drainSync(self.io);
    }
};

/// `bodies.FoldSeam` over the plane (D3, `defrag --fold`): the write
/// side's parked W2 extents are the targets; the kick is the write side's
/// own fold (`plane.foldKick` → `accumulate.foldExtentBlock`).
pub const PlaneFold = struct {
    plane: *plane_mod.DataPlane,

    pub fn seam(self: *PlaneFold) defrag_bodies.FoldSeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: defrag_bodies.FoldSeam.VTable = .{ .targets = &targets, .kick = &kick };

    fn targets(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]u64 {
        const self: *PlaneFold = @ptrCast(@alignCast(ctx));
        return self.plane.foldTargets(gpa);
    }

    fn kick(ctx: *anyopaque, io: Io, ino: u64) bool {
        const self: *PlaneFold = @ptrCast(@alignCast(ctx));
        return self.plane.foldKick(io, ino) > 0;
    }
};

/// `bodies.MetaSeam` over the meta set (D4, `defrag --meta`): per volume
/// the backend's compaction nudge and merge lap, each under the volume's
/// SMO lock (`backend.defragNudgeCompactions` / `defragMergeLap`).
pub const PlaneMeta = struct {
    set: *meta_set.MetaSet,
    gpa: Allocator,

    pub fn seam(self: *PlaneMeta) defrag_bodies.MetaSeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: defrag_bodies.MetaSeam.VTable = .{ .volumes = &volumes, .nudge = &nudge, .mergeLap = &mergeLap };

    fn volumes(ctx: *anyopaque) u32 {
        const self: *PlaneMeta = @ptrCast(@alignCast(ctx));
        return @intCast(self.set.volumes.len);
    }

    fn nudge(ctx: *anyopaque, _: Io, v: u32, n: u32) u32 {
        const self: *PlaneMeta = @ptrCast(@alignCast(ctx));
        const be = self.set.volumes[v];
        if (be.posture == .reader) return 0;
        return be.defragNudgeCompactions(self.gpa, n) catch 0;
    }

    fn mergeLap(ctx: *anyopaque, _: Io, v: u32) u64 {
        const self: *PlaneMeta = @ptrCast(@alignCast(ctx));
        const be = self.set.volumes[v];
        if (be.posture == .reader) return 0;
        return be.defragMergeLap();
    }
};

/// `rekey.RekeySeam` over the plane: the mover's table with the plane's
/// rewrite hook bound, the classifier off the device, the confirming
/// census's universe, the plane's quiescence, and the CLOSE — ONE config
/// tx flipping the primary to the next key (durable, fsync'd) and the
/// plane's wrap closed behind it.
pub const PlaneRekey = struct {
    plane: *plane_mod.DataPlane,
    io: Io,
    be: *KvBackend,
    mover_: *PlaneMover,
    /// The mover's table + `rewrite`.
    vt_: mover.Seam.VTable,

    pub fn init(plane: *plane_mod.DataPlane, io: Io, be: *KvBackend, m: *PlaneMover) PlaneRekey {
        var vt_ = PlaneMover.vt;
        vt_.rewrite = &rewrite;
        return .{ .plane = plane, .io = io, .be = be, .mover_ = m, .vt_ = vt_ };
    }

    pub fn seam(self: *PlaneRekey) rekey_mod.RekeySeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: rekey_mod.RekeySeam.VTable = .{ .mover = &moverFn, .windowKeyId = &windowKeyId, .refs = &refs, .allocatedBlocks = &allocatedBlocks, .classify = &classify, .quiesce = &quiesce, .drainReclaim = &drainReclaim, .close = &close, .chunk = &chunk };

    fn self_(ctx: *anyopaque) *PlaneRekey {
        return @ptrCast(@alignCast(ctx));
    }
    fn moverFn(ctx: *anyopaque) mover.Seam {
        const self = self_(ctx);
        return .{ .ctx = self.mover_, .vtable = &self.vt_ };
    }
    /// The mover's ctx is the `PlaneMover`; the plane is its.
    fn rewrite(ctx: *anyopaque, task: mover.Task, image: []u8, len: usize) mover.SeamError!?usize {
        const m: *PlaneMover = @ptrCast(@alignCast(ctx));
        return m.plane.rekeyRewrite(task, image, len);
    }
    fn windowKeyId(ctx: *anyopaque) ?[transform.KEY_ID_LEN]u8 {
        const self = self_(ctx);
        const value = (self.be.lookup(self.plane.gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch return null) orelse return null;
        defer self.plane.gpa.free(value);
        var diag: set_config.Refusal = undefined;
        var cfg = set_config.decode(self.plane.gpa, value, &diag) catch return null;
        defer cfg.deinit(self.plane.gpa);
        const nk = cfg.next_key orelse return null;
        return nk.key_id;
    }
    fn refs(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]census.Ref {
        const self = self_(ctx);
        var list: std.ArrayList(census.Ref) = .empty;
        errdefer list.deinit(gpa);
        const Collect = struct {
            gpa: Allocator,
            list: *std.ArrayList(census.Ref),
            fn visit(c: *@This(), ino: u64, bi: u32, m: layout.Mapping) plane_mod.Error!void {
                try c.list.append(c.gpa, .{ .ino = ino, .block_index = bi, .mapping = m });
            }
        };
        var col: Collect = .{ .gpa = gpa, .list = &list };
        self.plane.forEachBinding(*Collect, &col, Collect.visit) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => blk: {
                log.warn("defrag --rekey: the layout walk failed ({s}) — the pass plans nothing", .{@errorName(e)});
                list.clearRetainingCapacity();
                break :blk list.toOwnedSlice(gpa);
            },
        };
        return list.toOwnedSlice(gpa);
    }
    fn allocatedBlocks(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]rekey_mod.BlockKey {
        return self_(ctx).plane.rekeyAllocatedBlocks(gpa);
    }
    fn classify(ctx: *anyopaque, vol_tag: u64, block_idx: u64, rel_off: u32) rekey_mod.Class {
        return self_(ctx).plane.rekeyClassify(vol_tag, block_idx, rel_off);
    }
    fn quiesce(ctx: *anyopaque, io: Io) bool {
        return self_(ctx).plane.rekeyQuiesce(io);
    }
    fn drainReclaim(ctx: *anyopaque) void {
        const self = self_(ctx);
        _ = self.plane.reclaimer.drainSync(self.io);
    }
    /// THE LAST STEP: `closeRekey` on the record in ONE tx + the barrier,
    /// then the plane's wrap closed.
    fn close(ctx: *anyopaque) error{Io}!void {
        const self = self_(ctx);
        const gpa = self.plane.gpa;
        const value = (self.be.lookup(gpa, key.TREE_CONTROL, set_config.CONTROL_KEY) catch return error.Io) orelse return error.Io;
        defer gpa.free(value);
        var diag: set_config.Refusal = undefined;
        var cfg = set_config.decode(gpa, value, &diag) catch return error.Io;
        defer cfg.deinit(gpa);
        var m = set_config.closeRekey(gpa, &cfg) catch return error.Io;
        defer m.next.deinit(gpa);
        const bytes = set_config.encode(gpa, &m.next, @intCast(geometry.recordValueCap(self.be.nodeSize()))) catch return error.Io;
        defer gpa.free(bytes);
        const t = self.be.begin(@src()) catch return error.Io;
        t.stagePut(key.TREE_CONTROL, set_config.CONTROL_KEY, bytes) catch {
            self.be.abandon(t);
            return error.Io;
        };
        self.be.commit(t) catch return error.Io;
        self.be.fsync() catch return error.Io;
        lifecycle_stats.volume_config_generation.set(m.next.config_generation);
        self.plane.closeRekey(self.io) catch |e| log.err("defrag --rekey: the record flipped to key id {x} (generation {d}) but the plane's wrap did not close ({s}) — the previous key stays an unwrap arm until the next mount", .{ &m.next.key_id, m.next.config_generation, @errorName(e) });
        log.info("defrag --rekey: the primary key is now id {x} (config generation {d})", .{ &m.next.key_id, m.next.config_generation });
    }
    fn chunk(ctx: *anyopaque) u64 {
        return self_(ctx).plane.chunk;
    }
};

// ---------------------------------------------------------------------------
// The admin lane's world over the ONLINE set (`admin.AdminWorld`): the
// lattice through `PlaneWorld` with the plane attach + the grant-plane arm
// between the record and the placement; the fsck job's last report.
// ---------------------------------------------------------------------------

const admin_mod = @import("admin.zig");
const fsck_runner = @import("../fsck/runner.zig");

/// The online add's seventh step needs the wire plane (the allocation
/// lease + the holder's row for the new volume): a seam the mount binds.
pub const DataArm = struct {
    ctx: *anyopaque,
    arm: *const fn (ctx: *anyopaque, plane: *plane_mod.DataPlane, ordinal: u16) error{Io}!void,
};

pub const PlaneAdmin = struct {
    node: *Node,
    world_: PlaneWorld,
    arm: ?DataArm,
    /// `volume add-data` online: the attach + the grant-plane arm run
    /// between the record tx and the placement publish — `publishPlacement`
    /// is the lattice's step 7, so the add binds its own publish hook.
    pending_attach: ?set_config.DataVolume = null,

    pub fn world(self: *PlaneAdmin) admin_mod.AdminWorld {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: admin_mod.AdminWorld.VTable = .{ .addData = &addData, .removeData = &removeData, .undrain = &undrain, .fsckReport = &fsckReport, .rekey = &rekey };

    fn self_(ctx: *anyopaque) *PlaneAdmin {
        return @ptrCast(@alignCast(ctx));
    }

    /// The seven steps over an ONLINE world whose `publishPlacement` for
    /// the new volume first attaches it to the plane and arms the grant
    /// plane over it (device → record → attach → arm → Active), then the
    /// rebalance SUBMITTED through the fabric at the shipped 25 %.
    fn addData(ctx: *anyopaque, gpa: Allocator, uri: []const u8, rebalance: bool) lattice.Error!admin_mod.VolumeOutcome {
        const self = self_(ctx);
        const node = self.node;
        const plane = node.plane;
        const dev = volume_ops.probeDevice(gpa, node.io, uri, node.store.be.setUuid(), @intCast(plane.chunk)) catch |e| return e;
        var online: OnlineAdd = .{ .admin = self, .dev = dev, .cfg_after = null };
        defer if (online.cfg_after) |*c| c.deinit(gpa);
        const out = try lattice.addData(gpa, online.world(), dev, false);
        var job_id: u64 = 0;
        if (rebalance) {
            job_id = node.fabric.submit(node.io, .{ .job_type = .rebalance, .throttle_pct = rebalance_mod.REBALANCE_DEFAULT_THROTTLE_PCT }) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => blk: {
                    log.warn("admin lane: the add's rebalance could not be submitted ({s}) — `defrag --rebalance` queues it later", .{@errorName(e)});
                    break :blk 0;
                },
            };
        }
        return .{ .vol_tag = out.vol_tag, .job_id = job_id, .config_generation = out.config_generation, .capacity_blocks = @intCast(@min(dev.capacity_blocks, std.math.maxInt(u32))) };
    }

    /// `lattice.World` for the online add: `PlaneWorld` with the attach +
    /// arm folded into the placement publish of the NEW volume.
    const OnlineAdd = struct {
        admin: *PlaneAdmin,
        dev: lattice.Device,
        cfg_after: ?set_config.Config,

        fn world(self: *OnlineAdd) lattice.World {
            return .{ .ctx = self, .vtable = &ovt };
        }
        const ovt: lattice.World.VTable = .{ .gate = &gate, .loadConfig = &loadConfig, .stampLifecycleBit = &stamp, .formatDevice = &formatDevice, .commit = &commit, .publishPlacement = &publish, .cancelEvacuations = &cancelEvac, .now_s = &nowS, .random = &randomFn };
        fn inner(ctx: *anyopaque) *PlaneWorld {
            const self: *OnlineAdd = @ptrCast(@alignCast(ctx));
            return &self.admin.world_;
        }
        fn gate(ctx: *anyopaque) lattice.Error!void {
            return PlaneWorld.vt.gate(inner(ctx));
        }
        fn loadConfig(ctx: *anyopaque, gpa: Allocator) lattice.Error!set_config.Config {
            return PlaneWorld.vt.loadConfig(inner(ctx), gpa);
        }
        fn stamp(ctx: *anyopaque) lattice.Error!void {
            return PlaneWorld.vt.stampLifecycleBit(inner(ctx));
        }
        fn formatDevice(ctx: *anyopaque, dev: lattice.Device, vol_tag: u64) lattice.Error!void {
            return PlaneWorld.vt.formatDevice(inner(ctx), dev, vol_tag);
        }
        /// The record lands; the config AFTER is kept for the attach.
        fn commit(ctx: *anyopaque, cfg: *const set_config.Config, job: ?records.JobRecord) lattice.Error!void {
            const self: *OnlineAdd = @ptrCast(@alignCast(ctx));
            try PlaneWorld.vt.commit(inner(ctx), cfg, job);
            const gpa = self.admin.node.plane.gpa;
            self.cfg_after = cfg.clone(gpa) catch return error.OutOfMemory;
        }
        /// Step 7 for the NEW volume: attach to the plane (router row
        /// DISABLED), arm the grant plane, THEN Active.
        fn publish(ctx: *anyopaque, vol_tag: u64, state: set_config.State) lattice.Error!void {
            const self: *OnlineAdd = @ptrCast(@alignCast(ctx));
            const plane = self.admin.node.plane;
            const cfg = self.cfg_after orelse return error.Io;
            const d = cfg.find(vol_tag) orelse return error.UnknownVolume;
            const ordinal = plane.attachVolume(d.*) catch |e| {
                log.err("admin lane: vol {x} was recorded but could not be attached to the live plane ({s}) — it joins at the next mount", .{ vol_tag, @errorName(e) });
                return error.Io;
            };
            if (self.admin.arm) |a| a.arm(a.ctx, plane, ordinal) catch {
                log.err("admin lane: vol {x} attached but the grant plane could not arm over it — no placement on it until the next mount", .{vol_tag});
                return error.Io;
            };
            return PlaneWorld.vt.publishPlacement(inner(ctx), vol_tag, state);
        }
        fn cancelEvac(ctx: *anyopaque, vol_tag: u64) lattice.Error!void {
            return PlaneWorld.vt.cancelEvacuations(inner(ctx), vol_tag);
        }
        fn nowS(ctx: *anyopaque) u64 {
            return PlaneWorld.vt.now_s(inner(ctx));
        }
        fn randomFn(ctx: *anyopaque) u64 {
            return PlaneWorld.vt.random(inner(ctx));
        }
    };

    /// `remove-data`: the preflight over the live plane at the ACTUAL
    /// worker count, the `Draining` tx + placement, the evacuation
    /// SUBMITTED (the fabric's claim runs it at once).
    fn removeData(ctx: *anyopaque, gpa: Allocator, vol_tag: u64) lattice.Error!admin_mod.VolumeOutcome {
        const self = self_(ctx);
        const node = self.node;
        const pf = volume_ops.preflightDrain(gpa, node.plane, vol_tag, node.opts.workers) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
        _ = try lattice.removeData(gpa, self.world_.world(), vol_tag, pf, false);
        const job_id = node.fabric.submit(node.io, .{ .job_type = .evacuate_volume, .throttle_pct = 100, .arg0 = vol_tag }) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => blk: {
                log.err("admin lane: vol {x} is Draining but the evacuation could not be submitted ({s}) — `volume undrain` or retry", .{ vol_tag, @errorName(e) });
                break :blk 0;
            },
        };
        return .{ .vol_tag = vol_tag, .job_id = job_id, .preflight = pf };
    }

    fn undrain(ctx: *anyopaque, gpa: Allocator, vol_tag: u64) lattice.Error!admin_mod.VolumeOutcome {
        const self = self_(ctx);
        const before = self.node.fabric.liveNonTerminal(self.node.io);
        try lattice.undrain(gpa, self.world_.world(), vol_tag);
        const after = self.node.fabric.liveNonTerminal(self.node.io);
        return .{ .vol_tag = vol_tag, .cancelled = @intCast(before -| after) };
    }

    fn fsckReport(ctx: *anyopaque, gpa: Allocator, job_id: u64) Allocator.Error!?[]u8 {
        const self = self_(ctx);
        const node = self.node;
        const snap = node.fabric.snapshot(node.io, job_id) orelse return null;
        if (snap.job_type != .fsck) return null;
        const body = node.fsck_body orelse return null;
        return body.lastReportJson(gpa);
    }

    /// `defrag --rekey`: the next key resolved from the key dir and
    /// verified → the window opened in ONE config tx (durable BEFORE any
    /// block is written under it) → the plane's wrap swapped behind its
    /// barriers → the sweep submitted. A window already open with the same
    /// next key resumes (a new sweep record); another next key refuses.
    fn rekey(ctx: *anyopaque, gpa: Allocator, req: frames_mod.AdminRekeyReq) admin_mod.RekeyError!admin_mod.RekeyOutcome {
        const self = self_(ctx);
        const node = self.node;
        if (!node.fabric.is_coordinator.load(.acquire)) return error.NotCoordinator;
        var cfg = PlaneWorld.vt.loadConfig(&self.world_, gpa) catch return error.Io;
        defer cfg.deinit(gpa);
        if (cfg.encryption == .none) return error.Plaintext;
        if (std.mem.eql(u8, &cfg.key_id, &req.key_id)) return error.SameKey;
        if (cfg.next_key) |nk| if (!std.mem.eql(u8, &nk.key_id, &req.key_id) or !std.mem.eql(u8, &nk.kdf_salt, &req.kdf_salt)) return error.WindowOpen;
        var derived = keys.linux_verbs.resolveNextKey(&req.kdf_salt, req.key_id) catch return error.KeyUnresolved;
        defer derived.zeroize();
        var generation = cfg.config_generation;
        if (cfg.next_key == null) {
            var m = set_config.openRekey(gpa, &cfg, .{ .kdf_salt = req.kdf_salt, .key_id = req.key_id }) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.WindowOpen,
            };
            defer m.next.deinit(gpa);
            PlaneWorld.vt.commit(&self.world_, &m.next, null) catch return error.Io;
            generation = m.next.config_generation;
        }
        // The plane's wrap: a window already open in RAM (a resumed verb on
        // the same mount) is fine; a crashed mount's window armed at mount.
        if (!node.plane.rekeyWindowOpen()) node.plane.beginRekey(node.io, derived) catch |e| switch (e) {
            error.RotationOpen => {},
            error.OutOfMemory => return error.OutOfMemory,
            error.NotEncrypted, error.RandomFailed => return error.Plane,
        };
        const job_id = node.fabric.submit(node.io, .{ .job_type = .rekey, .throttle_pct = req.throttle_pct, .arg0 = std.mem.readInt(u64, &req.key_id, .little) }) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.NotCoordinator => error.NotCoordinator,
            error.TableFull => error.TableFull,
            error.Io => error.Io,
        };
        return .{ .job_id = job_id, .config_generation = generation };
    }
};

const frames_mod = @import("admin_frames.zig");

const rebalance_mod = @import("rebalance.zig");
const volume_ops = @import("../lifecycle/volume_ops.zig");
const meta_set = @import("../../meta/meta_set.zig");
const listener_mod = @import("../../wire/listener.zig");

// ---------------------------------------------------------------------------
// The mount's node: the fabric + the shard table + the wire side + the
// budget component + the worker fibers.
// ---------------------------------------------------------------------------

pub const Node = struct {
    gpa: Allocator,
    io: Io,
    opts: Options,
    store: TreeStore,
    plane: *plane_mod.DataPlane,
    fabric: Fabric,
    sink: AllocSink,
    table: lease_mod.ShardTable,
    coord: wire_mod.Coordinator,
    verify: PlaneVerify,
    workers: sync.TaskSet = .init,
    block_size: u64,
    /// The bodies over the plane (`bindBodies`), one each.
    mover_: PlaneMover,
    walk_: PlaneWalk,
    world_: PlaneWorld,
    drain_: PlaneDrain,
    drain_body: drain_mod.DrainBody,
    rebalance_body: drain_mod.RebalanceBody,
    defrag_: PlaneDefrag,
    data_body: defrag_bodies.DefragDataBody,
    pack_body: defrag_bodies.DefragPackBody,
    rekey_: PlaneRekey,
    rekey_body: rekey_mod.RekeyBody,
    fold_: PlaneFold,
    fold_body: defrag_bodies.DefragFoldBody,
    /// D4 over the meta set (null on a node created without the set: the
    /// offline verbs' and tests' shape — `defrag --meta` then fails typed).
    meta_: ?PlaneMeta,
    meta_body: defrag_bodies.DefragMetaBody,
    noop_body: fabric_mod.NoopBody = .{},
    /// The fsck engine over the LIVE binding (null until `bindFsck`).
    fsck_body: ?*fsck_runner.linux.JobBody = null,
    /// The MEMBER's fleet worker (a joined mount with `SQUEEZEFS_FLEET_JOBS`
    /// on): enrolled read-capable at the coordinator over the member's own
    /// manager link, pulling READ shards for the fsck engine over this
    /// mount's binding. Null on the coordinator and on a wire-less mount.
    fleet_caller: ?wire_mod.Caller,
    bitmaps: ?fsck_runner.linux.BitmapFace,
    fleet_worker: ?*wire_mod.Worker = null,
    fleet_task: ?Io.Future(void) = null,
    /// The admin lane (registered beside the job block).
    admin_: PlaneAdmin,
    admin_svc: admin_mod.AdminService,
    /// The foreground write-rate estimator the preflight reads.
    rate: preflight.WriteRateEstimator = .{},
    /// The listener's departure binding (its address is the listener's).
    departure_: listener_mod.Listener.Departure = undefined,
    sweeper: ?Io.Future(void) = null,
    sweeper_stop: std.atomic.Value(bool) = .init(false),

    const PlaneVerify = struct {
        plane: *plane_mod.DataPlane,
        fn vr(self: *PlaneVerify) wire_mod.VerifyRead {
            return .{ .ctx = self, .read_csum = &read };
        }
        fn read(ctx: *anyopaque, d: DestTuple, len: u64) error{Io}!u64 {
            const self: *PlaneVerify = @ptrCast(@alignCast(ctx));
            const buf = self.plane.gpa.alloc(u8, @intCast(len)) catch return error.Io;
            defer self.plane.gpa.free(buf);
            self.plane.deviceSeam().read(d.vol_tag, d.block_idx * self.plane.chunk, buf) catch return error.Io;
            return @import("../../core/checksum.zig").hash64(buf);
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
        /// The online add's grant-plane arm (the wire plane's); null on a
        /// mount with no wire (then `volume add-data` on the live mount
        /// refuses `io` — the offline verb is the path).
        data_arm: ?DataArm = null,
        /// The meta set the D4 arm (`defrag --meta`) compacts and merges;
        /// null = the arm is not bound (the body fails typed).
        meta_set: ?*meta_set.MetaSet = null,
        /// A JOINED mount's dialed link to the manager (the member's own
        /// session, so the member's death expires its shards): the fleet
        /// worker enrolls over it. Null on the manager and without a wire.
        fleet_caller: ?wire_mod.Caller = null,
        /// The allocation holder's bitmaps for the online fsck (resolved
        /// at every pass; null without a wire plane).
        bitmaps: ?fsck_runner.linux.BitmapFace = null,
    };

    pub fn create(gpa: Allocator, io: Io, opts: Options, in: Init) Allocator.Error!*Node {
        const n = try gpa.create(Node);
        errdefer gpa.destroy(n);
        n.* = .{
            .gpa = gpa,
            .io = io,
            .opts = opts,
            .store = .{ .be = in.backend },
            .plane = in.plane,
            .fabric = undefined,
            .sink = .{ .vols = &in.plane.vols, .wero = in.wero },
            .table = undefined,
            .coord = undefined,
            .verify = .{ .plane = in.plane },
            .block_size = in.plane.chunk,
            .mover_ = .{ .plane = in.plane, .io = io, .pre_publish_hold = opts.pre_publish_hold },
            .walk_ = .{ .plane = in.plane },
            .world_ = undefined,
            .drain_ = undefined,
            .drain_body = undefined,
            .rebalance_body = undefined,
            .defrag_ = undefined,
            .data_body = undefined,
            .pack_body = undefined,
            .rekey_ = undefined,
            .rekey_body = undefined,
            .fold_ = .{ .plane = in.plane },
            .fold_body = undefined,
            .meta_ = if (in.meta_set) |s| .{ .set = s, .gpa = gpa } else null,
            .meta_body = undefined,
            .admin_ = undefined,
            .admin_svc = undefined,
            .fleet_caller = in.fleet_caller,
            .bitmaps = in.bitmaps,
        };
        n.fabric = Fabric.init(gpa, n.store.store(), .{ .identity = in.identity, .pid = in.pid, .default_throttle_pct = opts.cpu_limit, .park_delay_ns = opts.park_delay_ns, .held = in.held, .random = in.random });
        n.table = lease_mod.ShardTable.init(seams.LEASE_TTL_MS, n.sink.sink(), null);
        n.coord = wire_mod.Coordinator.init(gpa, io, &n.fabric, &n.table, .{ .verify = n.verify.vr(), .channel = wire_tls.posture().channel(), .configured_permille = opts.verify_permille, .wero = if (in.wero) |w| w.wero() else null });
        n.sink.coord = &n.coord;
        // The bodies over the plane (every job type this daemon can run).
        n.world_ = .{ .be = in.backend, .plane = in.plane, .fabric = &n.fabric, .io = io, .random = in.random };
        n.drain_ = .{ .plane = in.plane, .io = io, .walk_ = n.walk_, .mover_ = n.mover_, .world_ = n.world_, .workers = opts.workers, .rate = .{ .ctx = n, .bps = &writeRateFn } };
        n.drain_body = .{ .gpa = gpa, .seam = n.drain_.seam() };
        n.rebalance_body = .{ .gpa = gpa, .seam = n.drain_.seam() };
        n.defrag_ = .{ .plane = in.plane, .io = io, .mover_ = &n.mover_, .pack_ = .{ .plane = in.plane, .io = io, .mover_ = &n.mover_ } };
        n.data_body = .{ .gpa = gpa, .seam = n.defrag_.seam() };
        n.pack_body = .{ .gpa = gpa, .seam = n.defrag_.seam() };
        n.fabric.registerBody(.noop, n.noop_body.body());
        n.fabric.registerBody(.evacuate_volume, n.drain_body.body());
        n.fabric.registerBody(.rebalance, n.rebalance_body.body());
        n.fabric.registerBody(.defrag_data, n.data_body.body());
        n.fabric.registerBody(.defrag_pack, n.pack_body.body());
        n.rekey_ = PlaneRekey.init(in.plane, io, in.backend, &n.mover_);
        n.rekey_body = .{ .gpa = gpa, .seam = n.rekey_.seam() };
        n.fabric.registerBody(.rekey, n.rekey_body.body());
        // D3 over the plane's write side; D4 over the meta set when bound.
        n.fold_body = .{ .gpa = gpa, .seam = n.fold_.seam() };
        n.fabric.registerBody(.defrag_fold, n.fold_body.body());
        if (n.meta_) |*m| {
            n.meta_body = .{ .gpa = gpa, .seam = m.seam() };
            n.fabric.registerBody(.defrag_meta, n.meta_body.body());
        }
        n.admin_ = .{ .node = n, .world_ = n.world_, .arm = in.data_arm };
        n.admin_svc = .{ .io = io, .fabric = &n.fabric, .world = n.admin_.world() };
        return n;
    }

    fn writeRateFn(ctx: *anyopaque) u64 {
        const self: *Node = @ptrCast(@alignCast(ctx));
        return self.rate.rateBps();
    }

    /// The foreground write admission sampled at the fabric's checkpoint
    /// cadence (the preflight's headroom term): the cadence tick calls it.
    pub fn sampleWriteRate(self: *Node, admitted_bytes: u64) void {
        self.rate.sample(clock.monotonicNs(), admitted_bytes);
    }

    /// The fsck engine over the LIVE binding: the `JobType.fsck` body (one
    /// pass = one online run; the admin lane reads its last report). The
    /// filesystem's 4a manager is the ladder's exclusive lease.
    pub fn bindFsck(self: *Node, be: *meta_set.MetaSet, fs: ?*fsck_runner.linux.BackendFs, env: *const std.process.Environ.Map, mem_budget: u64) !void {
        const b = try fsck_runner.linux.bind(self.gpa, self.io, be, self.plane, .{ .env = env, .mode = .online, .mem_budget = mem_budget, .fs = fs, .bitmaps = self.bitmaps });
        errdefer fsck_runner.linux.destroy(b);
        const body = try self.gpa.create(fsck_runner.linux.JobBody);
        body.* = .{ .gpa = self.gpa, .be = be, .plane = self.plane, .env = env, .mem_budget = mem_budget, .fs = fs, .bound = b, .fleet = if (self.opts.fleet_jobs) self.coord.fleet() else seams.FleetDispatch.none, .bitmaps = self.bitmaps };
        self.fsck_body = body;
        self.fabric.registerBody(.fsck, body.body());
    }

    /// The member's fleet worker: enroll (retried at the heartbeat cadence
    /// while the coordinator's block is not yet served — a manager mid-
    /// bring-up or mid-failover), then pull until the stop; a dead link
    /// ends one round and the next re-enrols (the departure expired this
    /// member's shards at the coordinator). Never a sleep: the parks are
    /// the worker's own notify under a bound.
    fn fleetMain(self: *Node) void {
        const w = self.fleet_worker.?;
        var enrolled = false;
        while (!w.stop.load(.acquire)) {
            if (!enrolled) {
                if (w.enroll()) |en| {
                    enrolled = true;
                    log.info("job fabric: this member enrolled at the coordinator as a fleet READ worker (heartbeat {d} ms, lease TTL {d} ms)", .{ en.heartbeat_ms, en.lease_ttl_ms });
                } else |e| {
                    log.debug("job fabric: fleet enrollment refused ({s}); retrying at the heartbeat cadence", .{@errorName(e)});
                    w.parkUnlessStopped(clock.monotonicNs() + @as(u64, w.heartbeat_ms) * std.time.ns_per_ms);
                    continue;
                }
            }
            w.serve() catch |e| {
                if (w.stop.load(.acquire)) break;
                log.warn("job fabric: the fleet worker's link to the coordinator ended ({s}) after {d} read shard(s); re-enrolling", .{ @errorName(e), w.read_shards_run });
                enrolled = false;
                w.parkUnlessStopped(clock.monotonicNs() + @as(u64, w.heartbeat_ms) * std.time.ns_per_ms);
            };
        }
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

    /// Adopt the durable records, register the job block AND the admin
    /// lane on the manager's service, spawn the local pool (claims nothing
    /// while held) and the lease sweeper.
    /// The listener's departure hook → the coordinator's `sessionDeparted`.
    pub fn departure(self: *Node) *const listener_mod.Listener.Departure {
        self.departure_ = .{ .ctx = self, .call = &departed };
        return &self.departure_;
    }

    fn departed(ctx: *anyopaque, session_id: u64) void {
        const self: *Node = @ptrCast(@alignCast(ctx));
        self.coord.sessionDeparted(session_id);
    }

    pub fn start(self: *Node, svc: ?*@import("../../wire/service.zig").Service) !void {
        const adopted = try self.fabric.adopt(self.io);
        if (svc) |s| {
            self.coord.register(s);
            self.admin_svc.register(s);
        }
        var i: u32 = 0;
        while (i < self.opts.workers) : (i += 1) try self.workers.spawn(self.io, workerMain, .{self});
        self.sweeper = try self.io.concurrent(sweeperMain, .{self});
        // A JOINED member with the fleet on and the engine bound is a
        // read-capable worker of the coordinator's.
        if (self.opts.fleet_jobs and !self.fabric.is_coordinator.load(.acquire)) if (self.fleet_caller) |caller| if (self.fsck_body) |body| {
            const w = try self.gpa.create(wire_mod.Worker);
            w.* = .{ .io = self.io, .caller = caller, .gpa = self.gpa, .caps = @import("frames.zig").CAP_FLEET_READ, .read_runner = body.readRunner(), .hold_submission = self.opts.hold_submission };
            self.fleet_worker = w;
            self.fleet_task = try self.io.concurrent(fleetMain, .{self});
        };
        log.info("job fabric: {d} job(s) adopted; {d} local worker(s), default throttle {d} %, verify {d} ‰ (plaintext pins 1000), fleet jobs {s}{s}; the admin lane {s}", .{ adopted, self.opts.workers, self.opts.cpu_limit, self.coord.permille(), if (self.opts.fleet_jobs) "on" else "off", if (self.fleet_worker != null) " (this member is a READ worker)" else "", if (svc != null) "on the listener" else "unserved (no wire)" });
    }

    fn workerMain(self: *Node) Io.Cancelable!void {
        self.fabric.serve(self.io, false);
    }

    /// The shard-lease sweeper at `max(TTL/4, 25 ms)` (`lease.zig`'s law):
    /// expired remote shards requeue; a lost notify cannot strand it (the
    /// park is bounded by the cadence itself).
    fn sweeperMain(self: *Node) void {
        const period_ns = lease_mod.sweepTickNs(self.table.ttl_ms);
        while (!self.sweeper_stop.load(.acquire)) {
            _ = self.coord.sweep(clock.monotonicNs());
            var n = self.coord.fabric.pool_wake.notified();
            _ = n.waitUntilUncancelable(self.io, clock.monotonicNs() + period_ns);
        }
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
        if (svc) |s| {
            self.coord.unregister(s);
            self.admin_svc.unregister(s);
        }
        self.sweeper_stop.store(true, .release);
        self.fabric.stop(self.io);
        // The fleet worker FIRST: a read shard mid-walk ends at its next
        // step (the stop raises the cancel) and is abandoned over the live
        // link before the wire plane stops.
        if (self.fleet_worker) |w| w.requestStop();
        if (self.fleet_task) |*t| {
            t.cancel(self.io);
            self.fleet_task = null;
        }
        if (self.sweeper) |*f| {
            f.cancel(self.io);
            self.sweeper = null;
        }
        self.workers.wait(self.io) catch {};
    }

    pub fn destroy(self: *Node) void {
        if (self.fleet_worker) |w| self.gpa.destroy(w);
        if (self.fsck_body) |b| {
            b.deinit();
            self.gpa.destroy(b);
        }
        self.coord.deinit();
        self.data_body.deinit();
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

test "the quarantine sink returns an expired lease's cohort to the free list at the drain proof: two pre-allocated unpublished destinations (minted, never published — flagged alone) and a free-listed one (taken out, OWED) leave the quarantine and read free; nothing stays allocated, in limbo or flagged — fsck's leaked arm has nothing to find (0003 §Y.2)" {
    const wt = @import("../../data/write_testing.zig");
    const f = try wt.Fixture.init(testing.allocator, 32);
    defer f.deinit();
    const a = f.alloc_a;
    // Blocks 0..3 minted; 3 published and terminal-freed: on the free list.
    var mints: [4]block_alloc.Minted = undefined;
    for (&mints) |*m| m.* = try a.allocateBlock();
    a.publishBlock(mints[3].idx);
    try testing.expectEqual(block_alloc.BeginFree.terminal, try a.freeBlock(mints[3].idx));
    const free0 = a.freeBlocksCount();
    var sink: AllocSink = .{ .vols = &f.vols, .wero = null };
    const q = sink.sink();
    const dests = [_]DestTuple{ .{ .vol_tag = wt.VOL_A, .block_idx = mints[0].idx }, .{ .vol_tag = wt.VOL_A, .block_idx = mints[1].idx }, .{ .vol_tag = wt.VOL_A, .block_idx = mints[3].idx } };
    const epoch = q.vtable.mintEpoch(q.ctx);
    for (dests) |d| q.vtable.admit(q.ctx, d, epoch);
    for (dests) |d| try testing.expect(q.vtable.isQuarantined(q.ctx, d));
    try testing.expectEqual(free0 - 1, a.freeBlocksCount()); // the free-listed one left the list
    // The landed preempt is the drain proof.
    q.vtable.release(q.ctx, epoch, 1);
    for (dests) |d| {
        try testing.expect(!q.vtable.isQuarantined(q.ctx, d));
        try testing.expectEqual(block_alloc.State.free, a.stateOf(d.block_idx));
        try testing.expect(!a.bitIsSet(d.block_idx));
    }
    try testing.expectEqual(free0 + 2, a.freeBlocksCount());
    try testing.expectEqual(@as(u64, 0), a.quarantine_count.load(.acquire));
    // The block never admitted keeps its owner.
    try testing.expectEqual(block_alloc.State.allocated, a.stateOf(mints[2].idx));
}
