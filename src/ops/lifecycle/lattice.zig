//! The volume lifecycle's LATTICE and the durability order of every
//! lifecycle commit (charter §8 "`vol-{hex}` ids never reused; `Active →
//! Draining → Retired`"; spec §2.1–§2.3, §2.6), as a PORTABLE machine over
//! a `World` seam so a crash after EVERY step is injected in RAM and the
//! remount's convergence asserted on every host. `volume_ops.zig` binds
//! the same steps to the writer door, the superblocks, the data device
//! and volume 0's conveyor.
//!
//! The seven-step order (`set_config.zig`'s module doc; every verb):
//!   1. the live-writer gate on EVERY meta volume (offline) / the manager
//!      predicate (online);
//!   2. read + decode `set_config` (undecodable ⇒ refuse by name);
//!   3. preflight (device identity, the capacity closed form, the denylist);
//!   4. stamp `VOLUME_LIFECYCLE` (bit 3) on every member, barriered, iff
//!      `lifecycle_bit_needed` — BEFORE the record tx (bit-before-record);
//!   5. `add-data` only: the data device's superblock is written BEFORE the
//!      record tx (device-before-record);
//!   6. ONE conveyor tx: `Put(set_config)` at `generation + 1` (the offline
//!      add's queued rebalance record rides the SAME tx);
//!   7. runtime: placement enabled strictly AFTER the ack (placement-after-
//!      ack); the retire deregisters the runtime AFTER the durable commit
//!      (retire-then-deregister).
//!
//! Identity: `vol-{16 hex}` minted once, never reused — a Retired record
//! is kept FOREVER with its uri cleared, so a stale key fails loud instead
//! of resolving to a re-added device (`set_config.addDataVolume` refuses
//! a taken tag/uuid/uri).

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const set_config = @import("../../data/set_config.zig");
const router = @import("../../data/router.zig");
const records = @import("../jobs/records.zig");
const rebalance = @import("../jobs/rebalance.zig");
const preflight = @import("../jobs/preflight.zig");

pub const State = set_config.State;
pub const Config = set_config.Config;
pub const DataVolume = set_config.DataVolume;

/// The verb's steps, in order; a crash seam names the step AFTER which the
/// process dies.
pub const Step = enum(u8) {
    gate = 1,
    decode = 2,
    preflight = 3,
    stamp_bit = 4,
    device_superblock = 5,
    record_tx = 6,
    runtime = 7,
};

pub const Error = error{
    /// A live writer holds a member (offline) / not the manager (online).
    LiveWriter,
    /// The `set_config` record does not decode.
    ConfigUndecodable,
    /// The device is already a member (by tag, uuid or canonical path), a
    /// meta volume, unsized, or failed the rw probe.
    DeviceRefused,
    /// The capacity closed form refused (the text is the preflight's).
    CapacityRefused,
    /// The transition the lattice forbids (`undrain` of a non-draining
    /// volume: "retired volumes never come back: ids are permanent").
    IllegalTransition,
    UnknownVolume,
    /// A durable step failed (the crash seam, a device error).
    Io,
    OutOfMemory,
};

/// The identity of a device the verb names.
pub const Device = struct {
    uri: []const u8,
    uuid: [16]u8,
    capacity_blocks: u64,
    volume_len: u64,
    /// The rw probe's verdict (a read-only or vanished device fails HERE,
    /// before any durable record names it).
    writable: bool = true,
    /// Already a META volume of this set.
    is_meta: bool = false,
};

/// The durable + runtime world the steps act on.
pub const World = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        gate: *const fn (ctx: *anyopaque) Error!void,
        loadConfig: *const fn (ctx: *anyopaque, gpa: Allocator) Error!Config,
        stampLifecycleBit: *const fn (ctx: *anyopaque) Error!void,
        /// Write the data superblock naming `(set_uuid, vol_tag)` on the device.
        formatDevice: *const fn (ctx: *anyopaque, dev: Device, vol_tag: u64) Error!void,
        /// ONE tx: the config record (+ the optional queued job record).
        commit: *const fn (ctx: *anyopaque, cfg: *const Config, job: ?records.JobRecord) Error!void,
        /// Placement: enable `vol_tag` (after the ack) / retire the runtime
        /// (after the durable commit).
        publishPlacement: *const fn (ctx: *anyopaque, vol_tag: u64, state: State) Error!void,
        /// Cancel every non-terminal evacuate job naming `vol_tag` (undrain).
        cancelEvacuations: *const fn (ctx: *anyopaque, vol_tag: u64) Error!void,
        now_s: *const fn (ctx: *anyopaque) u64,
        random: *const fn (ctx: *anyopaque) u64,
    };
};

pub const AddOutcome = struct {
    vol_tag: u64,
    config_generation: u64,
    /// The queued rebalance job's id (0 when `--no-rebalance`).
    rebalance_job: u64,
};

/// `volume add-data`: the seven steps. `queue_rebalance` writes a durable
/// queued `rebalance` record at `REBALANCE_DEFAULT_THROTTLE_PCT` in the
/// SAME tx as the config so the next mount's fabric adopts it (offline);
/// online the caller submits through the fabric instead.
pub fn addData(gpa: Allocator, w: World, dev: Device, queue_rebalance: bool) Error!AddOutcome {
    try w.vtable.gate(w.ctx); // 1
    var cfg = try w.vtable.loadConfig(w.ctx, gpa); // 2
    defer cfg.deinit(gpa);
    // 3: identity — by canonical uri, by uuid, a meta volume, unsized, the rw probe.
    if (dev.is_meta or !dev.writable or dev.capacity_blocks == 0 or dev.uri.len == 0) return error.DeviceRefused;
    for (cfg.data) |d| {
        if (std.mem.eql(u8, &d.device_uuid, &dev.uuid)) return error.DeviceRefused;
        if (d.uri.len != 0 and std.mem.eql(u8, d.uri, dev.uri)) return error.DeviceRefused;
    }
    var tag = w.vtable.random(w.ctx);
    while (tag == 0 or cfg.find(tag) != null) tag = w.vtable.random(w.ctx);
    const mutation = set_config.addDataVolume(gpa, &cfg, .{ .vol_tag = tag, .state = .active, .device_uuid = dev.uuid, .added_ts = w.vtable.now_s(w.ctx), .capacity_blocks = dev.capacity_blocks, .volume_len = dev.volume_len, .uri = dev.uri }) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => error.DeviceRefused,
    };
    var next = mutation.next;
    defer next.deinit(gpa);
    if (mutation.lifecycle_bit_needed) try w.vtable.stampLifecycleBit(w.ctx); // 4
    try w.vtable.formatDevice(w.ctx, dev, tag); // 5
    var job: ?records.JobRecord = null;
    var job_id: u64 = 0;
    if (queue_rebalance) {
        job_id = w.vtable.random(w.ctx);
        while (job_id == 0) job_id = w.vtable.random(w.ctx);
        const now = w.vtable.now_s(w.ctx);
        job = .{ .job_id = job_id, .job_type = .rebalance, .state = .queued, .throttle_pct = rebalance.REBALANCE_DEFAULT_THROTTLE_PCT, .created_realtime_s = now, .created_by = .{ .node_token = 0, .mount_slot = 0 }, .created_pid = 0, .updated_realtime_s = now };
    }
    try w.vtable.commit(w.ctx, &next, job); // 6
    try w.vtable.publishPlacement(w.ctx, tag, .active); // 7
    return .{ .vol_tag = tag, .config_generation = next.config_generation, .rebalance_job = job_id };
}

/// `volume remove-data`: gate, decode, the capacity closed form, the bit,
/// the `Draining` record, the runtime (weight 0 is a consequence of
/// `placementEligible`). The drain itself is the mover job the caller
/// submits (online) or writes queued (offline).
pub fn removeData(gpa: Allocator, w: World, vol_tag: u64, pf: preflight.CapacityPreflight, queue_drain: bool) Error!u64 {
    try w.vtable.gate(w.ctx);
    var cfg = try w.vtable.loadConfig(w.ctx, gpa);
    defer cfg.deinit(gpa);
    const cur = cfg.find(vol_tag) orelse return error.UnknownVolume;
    if (cur.state != .active) return error.IllegalTransition;
    if (pf.refuses()) return error.CapacityRefused;
    const mutation = set_config.setState(gpa, &cfg, vol_tag, .draining) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.IllegalTransition => error.IllegalTransition,
        error.UnknownVolume => error.UnknownVolume,
        else => error.Io,
    };
    var next = mutation.next;
    defer next.deinit(gpa);
    if (mutation.lifecycle_bit_needed) try w.vtable.stampLifecycleBit(w.ctx);
    var job: ?records.JobRecord = null;
    var job_id: u64 = 0;
    if (queue_drain) {
        job_id = w.vtable.random(w.ctx);
        while (job_id == 0) job_id = w.vtable.random(w.ctx);
        const now = w.vtable.now_s(w.ctx);
        job = .{ .job_id = job_id, .job_type = .evacuate_volume, .state = .queued, .throttle_pct = 100, .created_realtime_s = now, .created_by = .{ .node_token = 0, .mount_slot = 0 }, .created_pid = 0, .arg0 = vol_tag, .updated_realtime_s = now };
    }
    try w.vtable.commit(w.ctx, &next, job);
    try w.vtable.publishPlacement(w.ctx, vol_tag, .draining);
    return job_id;
}

/// The drain converged (census empty ∧ used 0): `Retired`, uri cleared,
/// ONE tx; the runtime deregisters AFTER the durable commit.
pub fn retire(gpa: Allocator, w: World, vol_tag: u64) Error!void {
    try w.vtable.gate(w.ctx);
    var cfg = try w.vtable.loadConfig(w.ctx, gpa);
    defer cfg.deinit(gpa);
    const cur = cfg.find(vol_tag) orelse return error.UnknownVolume;
    if (cur.state != .draining) return error.IllegalTransition;
    const mutation = set_config.setState(gpa, &cfg, vol_tag, .retired) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => error.IllegalTransition,
    };
    var next = mutation.next;
    defer next.deinit(gpa);
    if (mutation.lifecycle_bit_needed) try w.vtable.stampLifecycleBit(w.ctx);
    try w.vtable.commit(w.ctx, &next, null);
    try w.vtable.publishPlacement(w.ctx, vol_tag, .retired);
}

/// `volume undrain`: refuse unless `Draining`; the DURABLE flip to Active
/// FIRST (a mover pass observing non-draining self-cancels even if the
/// explicit cancel races it), THEN cancel every evacuate job naming it.
pub fn undrain(gpa: Allocator, w: World, vol_tag: u64) Error!void {
    try w.vtable.gate(w.ctx);
    var cfg = try w.vtable.loadConfig(w.ctx, gpa);
    defer cfg.deinit(gpa);
    const cur = cfg.find(vol_tag) orelse return error.UnknownVolume;
    if (cur.state != .draining) return error.IllegalTransition;
    const mutation = set_config.setState(gpa, &cfg, vol_tag, .active) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => error.IllegalTransition,
    };
    var next = mutation.next;
    defer next.deinit(gpa);
    if (mutation.lifecycle_bit_needed) try w.vtable.stampLifecycleBit(w.ctx);
    try w.vtable.commit(w.ctx, &next, null);
    try w.vtable.publishPlacement(w.ctx, vol_tag, .active);
    try w.vtable.cancelEvacuations(w.ctx, vol_tag);
}

/// `vol-{16 hex}` of a tag.
pub fn writeVolumeId(w: *std.Io.Writer, vol_tag: u64) std.Io.Writer.Error!void {
    try router.VolumeId.format(.{ .tag = vol_tag }, w);
}

/// The tag of a `vol-{16 hex}` word (the verbs' positional).
pub fn parseVolumeId(word: []const u8) ?u64 {
    const id = router.VolumeId.parse(word) orelse return null;
    return id.tag;
}

// ---------------------------------------------------------------------------
// The RAM world with a crash seam, and the remount that judges it.
// ---------------------------------------------------------------------------

pub const RamWorld = struct {
    gpa: Allocator,
    /// The durable set: the config record and the jobs volume 0 holds.
    config: Config,
    jobs: std.ArrayList(records.JobRecord) = .empty,
    /// Bit 3 on every member (one member here; the stamp is barriered).
    lifecycle_bit: bool = false,
    /// Data devices by uuid: formatted with which `vol_tag` (0 = blank).
    devices: std.AutoHashMapUnmanaged([16]u8, u64) = .empty,
    /// The RUNTIME (lost at a crash): placement rows.
    placement: std.AutoHashMapUnmanaged(u64, State) = .empty,
    live_writer: bool = false,
    /// Crash AFTER this step (the step's own effect lands).
    crash_after: ?Step = null,
    crashed: bool = false,
    commits: u64 = 0,
    seed: u64 = 0x1234_5678,
    cancelled: std.ArrayList(u64) = .empty,

    pub fn init(gpa: Allocator, cfg: Config) RamWorld {
        return .{ .gpa = gpa, .config = cfg };
    }

    pub fn deinit(self: *RamWorld) void {
        self.config.deinit(self.gpa);
        self.jobs.deinit(self.gpa);
        self.devices.deinit(self.gpa);
        self.placement.deinit(self.gpa);
        self.cancelled.deinit(self.gpa);
    }

    pub fn world(self: *RamWorld) World {
        return .{ .ctx = self, .vtable = &vt };
    }

    fn crashAt(self: *RamWorld, step: Step) Error!void {
        if (self.crash_after) |c| if (c == step) {
            self.crashed = true;
            return error.Io;
        };
    }

    const vt: World.VTable = .{ .gate = &gate, .loadConfig = &loadConfig, .stampLifecycleBit = &stamp, .formatDevice = &formatDevice, .commit = &commit, .publishPlacement = &publish, .cancelEvacuations = &cancelEvac, .now_s = &nowS, .random = &random };

    fn gate(ctx: *anyopaque) Error!void {
        const self: *RamWorld = @ptrCast(@alignCast(ctx));
        if (self.live_writer) return error.LiveWriter;
        try self.crashAt(.gate);
    }
    fn loadConfig(ctx: *anyopaque, gpa: Allocator) Error!Config {
        const self: *RamWorld = @ptrCast(@alignCast(ctx));
        try self.crashAt(.decode);
        return self.config.clone(gpa) catch return error.OutOfMemory;
    }
    fn stamp(ctx: *anyopaque) Error!void {
        const self: *RamWorld = @ptrCast(@alignCast(ctx));
        try self.crashAt(.preflight);
        self.lifecycle_bit = true;
        try self.crashAt(.stamp_bit);
    }
    fn formatDevice(ctx: *anyopaque, dev: Device, vol_tag: u64) Error!void {
        const self: *RamWorld = @ptrCast(@alignCast(ctx));
        self.devices.put(self.gpa, dev.uuid, vol_tag) catch return error.OutOfMemory;
        try self.crashAt(.device_superblock);
    }
    fn commit(ctx: *anyopaque, cfg: *const Config, job: ?records.JobRecord) Error!void {
        const self: *RamWorld = @ptrCast(@alignCast(ctx));
        // The bit-before-record law is the WORLD's invariant too: a record
        // that diverged from birth never lands on an unstamped member.
        std.debug.assert(!cfg.lifecycleDiverged() or self.lifecycle_bit);
        const next = cfg.clone(self.gpa) catch return error.OutOfMemory;
        self.config.deinit(self.gpa);
        self.config = next;
        if (job) |j| self.jobs.append(self.gpa, j) catch return error.OutOfMemory;
        self.commits += 1;
        try self.crashAt(.record_tx);
    }
    fn publish(ctx: *anyopaque, vol_tag: u64, state: State) Error!void {
        const self: *RamWorld = @ptrCast(@alignCast(ctx));
        if (state == .retired) _ = self.placement.remove(vol_tag) else self.placement.put(self.gpa, vol_tag, state) catch return error.OutOfMemory;
        try self.crashAt(.runtime);
    }
    fn cancelEvac(ctx: *anyopaque, vol_tag: u64) Error!void {
        const self: *RamWorld = @ptrCast(@alignCast(ctx));
        for (self.jobs.items) |*j| if (j.job_type == .evacuate_volume and j.arg0 == vol_tag and !j.isTerminal()) {
            j.state = .cancelled;
            self.cancelled.append(self.gpa, j.job_id) catch return error.OutOfMemory;
        };
    }
    fn nowS(_: *anyopaque) u64 {
        return 1_800_000_000;
    }
    fn random(ctx: *anyopaque) u64 {
        const self: *RamWorld = @ptrCast(@alignCast(ctx));
        self.seed = self.seed *% 6364136223846793005 +% 1442695040888963407;
        return self.seed >> 1;
    }

    /// "Remount": the runtime is rebuilt from durable state alone —
    /// placement for every active record; nothing else. Returns the
    /// discovery verdict of the durable set.
    pub fn remount(self: *RamWorld) !void {
        self.placement.clearRetainingCapacity();
        self.crashed = false;
        self.crash_after = null;
        for (self.config.data) |d| {
            if (d.state == .retired) {
                // A retired record keeps its tag forever and names no device.
                try std.testing.expect(d.uri.len == 0);
                continue;
            }
            // Device-before-record: a record names only a formatted device
            // carrying the record's tag.
            const formatted = self.devices.get(d.device_uuid) orelse return error.RecordWithoutDevice;
            if (formatted != d.vol_tag) return error.RecordTagMismatch;
            try self.placement.put(self.gpa, d.vol_tag, d.state);
        }
        // Bit-before-record.
        if (self.config.lifecycleDiverged() and !self.lifecycle_bit) return error.RecordWithoutBit;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const set_uuid: [16]u8 = "lifecycle-set-01".*;

fn birthWorld(gpa: Allocator) !RamWorld {
    const d0: DataVolume = .{ .vol_tag = 0x1111, .state = .active, .device_uuid = @splat(1), .added_ts = 1, .capacity_blocks = 1000, .volume_len = 1 << 32, .uri = "/dev/d0" };
    const cfg = try set_config.birth(gpa, set_uuid, .{ .compression = .none, .encryption = .none, .block_size = 4 << 20 }, null, &.{d0}, &.{});
    var w = RamWorld.init(gpa, cfg);
    try w.devices.put(gpa, @splat(1), 0x1111);
    try w.placement.put(gpa, 0x1111, .active);
    return w;
}

const dev2: Device = .{ .uri = "/dev/d2", .uuid = @splat(2), .capacity_blocks = 2000, .volume_len = 1 << 33 };

test "add-data: a fresh vol-{16 hex} tag; the bit stamped before the record; the device formatted before the record; placement after the ack; the queued rebalance rides the one tx at 25 %; a second add of the same device refuses by identity; a live writer refuses first" {
    const gpa = testing.allocator;
    var w = try birthWorld(gpa);
    defer w.deinit();
    try testing.expect(!w.config.lifecycleDiverged());
    const out = try addData(gpa, w.world(), dev2, true);
    try testing.expect(out.vol_tag != 0 and out.vol_tag != 0x1111);
    try testing.expectEqual(@as(u64, 2), out.config_generation);
    try testing.expect(w.lifecycle_bit);
    try testing.expectEqual(@as(?u64, out.vol_tag), w.devices.get(dev2.uuid));
    try testing.expectEqual(@as(?State, .active), w.placement.get(out.vol_tag));
    try testing.expectEqual(@as(usize, 1), w.jobs.items.len);
    try testing.expectEqual(records.JobType.rebalance, w.jobs.items[0].job_type);
    try testing.expectEqual(@as(u8, 25), w.jobs.items[0].throttle_pct);
    try testing.expectEqual(out.rebalance_job, w.jobs.items[0].job_id);
    var buf: [32]u8 = undefined;
    var wr: std.Io.Writer = .fixed(&buf);
    try writeVolumeId(&wr, out.vol_tag);
    try testing.expect(std.mem.startsWith(u8, wr.buffered(), "vol-") and wr.buffered().len == 20);
    try testing.expectError(error.DeviceRefused, addData(gpa, w.world(), dev2, true));
    var same_uri = dev2;
    same_uri.uuid = @splat(9);
    try testing.expectError(error.DeviceRefused, addData(gpa, w.world(), same_uri, false));
    var ro = dev2;
    ro.uuid = @splat(7);
    ro.uri = "/dev/d7";
    ro.writable = false;
    try testing.expectError(error.DeviceRefused, addData(gpa, w.world(), ro, false));
    w.live_writer = true;
    try testing.expectError(error.LiveWriter, addData(gpa, w.world(), ro, false));
    try testing.expectEqual(@as(u64, 1), w.commits);
}

test "the lattice's durability order over RAM seams: a crash after EVERY step of add-data leaves a set the remount converges — a record never names an unformatted device or an unstamped member; re-running the verb converges to one added volume" {
    const gpa = testing.allocator;
    inline for (@typeInfo(Step).@"enum".field_names) |name| {
        const step = @field(Step, name);
        var w = try birthWorld(gpa);
        defer w.deinit();
        w.crash_after = step;
        const r = addData(gpa, w.world(), dev2, true);
        try testing.expectError(error.Io, r);
        try testing.expect(w.crashed);
        // The remount judges the durable state alone.
        try w.remount();
        const landed = w.config.data.len == 2;
        // The record landed iff the crash came after the tx.
        try testing.expectEqual(@backingInt(step) >= @backingInt(Step.record_tx), landed);
        if (landed) {
            try testing.expect(w.lifecycle_bit);
            try testing.expect(w.devices.get(dev2.uuid) != null);
            try testing.expectEqual(@as(?State, .active), w.placement.get(w.config.data[1].vol_tag));
            try testing.expectEqual(@as(usize, 1), w.jobs.items.len);
            // A re-run refuses by identity; the set is unchanged.
            try testing.expectError(error.DeviceRefused, addData(gpa, w.world(), dev2, true));
            try testing.expectEqual(@as(usize, 2), w.config.data.len);
        } else {
            // Before the tx the device may be formatted (device-before-
            // record) and the bit may be stamped (bit-before-record): both
            // are harmless orphans; the re-run converges to ONE volume.
            try testing.expectEqual(@as(usize, 0), w.jobs.items.len);
            const again = try addData(gpa, w.world(), dev2, true);
            try testing.expectEqual(@as(usize, 2), w.config.data.len);
            try testing.expectEqual(@as(?u64, again.vol_tag), w.devices.get(dev2.uuid));
            try w.remount();
        }
    }
}

test "remove-data → draining (preflight refuses loud) → retire (uri cleared, tag kept forever, runtime deregistered after the commit); undrain flips Active FIRST then cancels the evacuation; retired never comes back; a crash at each retire step converges" {
    const gpa = testing.allocator;
    var w = try birthWorld(gpa);
    defer w.deinit();
    const out = try addData(gpa, w.world(), dev2, false);
    const ok: preflight.CapacityPreflight = .{ .avail = 1 << 40, .needed = 1 << 30, .transient = 1 << 28, .headroom = 1 << 30 };
    const bad: preflight.CapacityPreflight = .{ .avail = 1, .needed = 1 << 30, .transient = 0, .headroom = 0 };
    try testing.expectError(error.CapacityRefused, removeData(gpa, w.world(), 0x1111, bad, true));
    try testing.expectEqual(State.active, w.config.find(0x1111).?.state);
    const job = try removeData(gpa, w.world(), 0x1111, ok, true);
    try testing.expectEqual(State.draining, w.config.find(0x1111).?.state);
    try testing.expectEqual(@as(?State, .draining), w.placement.get(0x1111));
    try testing.expectEqual(records.JobType.evacuate_volume, w.jobs.items[0].job_type);
    try testing.expectEqual(@as(u64, 0x1111), w.jobs.items[0].arg0);
    try testing.expectError(error.IllegalTransition, removeData(gpa, w.world(), 0x1111, ok, true));
    try testing.expectError(error.UnknownVolume, removeData(gpa, w.world(), 0xdead, ok, true));
    // Undrain: the record flips first, the job is cancelled after.
    try undrain(gpa, w.world(), 0x1111);
    try testing.expectEqual(State.active, w.config.find(0x1111).?.state);
    try testing.expectEqualSlices(u64, &.{job}, w.cancelled.items);
    try testing.expectEqual(records.JobState.cancelled, w.jobs.items[0].state);
    try testing.expectError(error.IllegalTransition, undrain(gpa, w.world(), 0x1111));
    // Drain again and retire.
    _ = try removeData(gpa, w.world(), 0x1111, ok, true);
    try testing.expectError(error.IllegalTransition, retire(gpa, w.world(), out.vol_tag));
    inline for (@typeInfo(Step).@"enum".field_names) |name| {
        const step = @field(Step, name);
        // The retire writes no device superblock and the bit is already
        // stamped (the set diverged at the add): those steps never run.
        if (step == .device_superblock or step == .preflight or step == .stamp_bit) continue;
        w.crash_after = step;
        try testing.expectError(error.Io, retire(gpa, w.world(), 0x1111));
        try w.remount();
        const retired = w.config.find(0x1111).?.state == .retired;
        try testing.expectEqual(@backingInt(step) >= @backingInt(Step.record_tx), retired);
        if (retired) break;
    }
    if (w.config.find(0x1111).?.state != .retired) try retire(gpa, w.world(), 0x1111);
    const r = w.config.find(0x1111).?;
    try testing.expectEqual(State.retired, r.state);
    try testing.expectEqual(@as(usize, 0), r.uri.len);
    try testing.expectEqual(@as(?State, null), w.placement.get(0x1111));
    try w.remount();
    try testing.expectError(error.IllegalTransition, undrain(gpa, w.world(), 0x1111));
    try testing.expectError(error.IllegalTransition, removeData(gpa, w.world(), 0x1111, ok, true));
    // The id is never reused: a device re-added mints a NEW tag and the
    // old record stays.
    var d0_again = dev2;
    d0_again.uuid = @splat(1);
    d0_again.uri = "/dev/d0";
    try testing.expectError(error.DeviceRefused, addData(gpa, w.world(), d0_again, false)); // same device uuid is taken forever
    try testing.expectEqual(@as(usize, 2), w.config.data.len);
}

fn volumeIdWordFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [64]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseVolumeId(buf[0..len]);
}

test "fuzz: the volume-id word parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, volumeIdWordFuzz, .{ .corpus = &.{ fuzzkit.slice("0x1f"), fuzzkit.slice("18446744073709551616") } });
}
