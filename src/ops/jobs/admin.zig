//! The ADMIN LANE's handlers (charter §8; `admin_frames.zig`): the
//! operator's verbs served on the coordinator's `wire/service.Service`
//! beside the job block, behind the storage-trust ladder (an admitted
//! peer IS the credential — the listener admitted nobody who could not
//! read the set's `job:enroll` record). Portable over the fabric and an
//! `AdminWorld` seam the Linux coordinator binds (the lattice over the
//! ONLINE set for `volume {add-data, remove-data, undrain}`, the fsck
//! job's last bounded report); tests run it over the loopback hub with a
//! RAM world.
//!
//! What a verb may do is exactly what the fabric's own control surface
//! does: `list` is the live table (adopted + submitted); `control` is
//! `pause / resume / cancel / setThrottle` with their typed outcomes;
//! `submit` is a coordinator-class act (durable Put first — refused
//! `not_coordinator` elsewhere); `volume` runs the lattice's seven steps
//! through the world (the daemon's gate is the manager predicate). Every
//! outcome is a VERDICT byte, never a string.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const service = @import("../../wire/service.zig");
const verbs = @import("../../wire/verbs.zig");
const records = @import("records.zig");
const fabric_mod = @import("fabric.zig");
const frames = @import("admin_frames.zig");
const lattice = @import("../lifecycle/lattice.zig");
const preflight = @import("preflight.zig");

const Fabric = fabric_mod.Fabric;
const Verdict = frames.Verdict;
const stats = @import("stats.zig").stats;

/// The lattice's outcome a volume verb reports.
pub const VolumeOutcome = struct {
    vol_tag: u64,
    job_id: u64 = 0,
    config_generation: u64 = 0,
    preflight: preflight.CapacityPreflight = .{ .needed = 0, .avail = 0, .transient = 0, .headroom = 0 },
    cancelled: u32 = 0,
    capacity_blocks: u32 = 0,
};

/// What the lane needs of the daemon beyond the fabric.
pub const AdminWorld = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// `volume add-data <uri>` online: probe, format, record, bind, the
        /// rebalance SUBMITTED through the fabric (never a queued record
        /// nobody adopts).
        addData: *const fn (ctx: *anyopaque, gpa: Allocator, uri: []const u8, rebalance: bool) lattice.Error!VolumeOutcome,
        /// `volume remove-data`: the preflight over the live plane, the
        /// `Draining` tx, the evacuation submitted.
        removeData: *const fn (ctx: *anyopaque, gpa: Allocator, vol_tag: u64) lattice.Error!VolumeOutcome,
        /// `volume undrain`: Active FIRST, then the live cancels.
        undrain: *const fn (ctx: *anyopaque, gpa: Allocator, vol_tag: u64) lattice.Error!VolumeOutcome,
        /// The fsck job's last bounded report (owned by `gpa`), or null.
        fsckReport: *const fn (ctx: *anyopaque, gpa: Allocator, job_id: u64) Allocator.Error!?[]u8,
        /// `defrag --rekey`: resolve the next key (the key dir), open the
        /// window durably, arm the plane's wrap, submit the sweep.
        rekey: *const fn (ctx: *anyopaque, gpa: Allocator, req: frames.AdminRekeyReq) RekeyError!RekeyOutcome,
    };
};

pub const RekeyError = error{ OutOfMemory, Io, NotCoordinator, Plaintext, WindowOpen, KeyUnresolved, SameKey, Plane, TableFull };
pub const RekeyOutcome = struct { job_id: u64, config_generation: u64 };

pub const AdminService = struct {
    io: Io,
    fabric: *Fabric,
    world: ?AdminWorld,

    const all_verbs = .{ .admin_job_list, .admin_job_control, .admin_job_submit, .admin_fsck_report, .admin_volume, .admin_rekey };

    pub fn register(self: *AdminService, svc: *service.Service) void {
        const h: service.Handler = .{ .ctx = self, .serve = &serve };
        inline for (all_verbs) |v| svc.register(v, h);
    }

    pub fn unregister(_: *AdminService, svc: *service.Service) void {
        inline for (all_verbs) |v| svc.unregister(v);
    }

    fn serve(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *AdminService = @ptrCast(@alignCast(ctx));
        if (req.peer == null) return service.Answer.rejected("admin verbs need an admitted peer");
        stats.admin_verbs.inc();
        const answer: service.Answer = switch (req.payload) {
            .admin_job_list => self.list(scratch),
            .admin_job_control => |p| self.control(p),
            .admin_job_submit => |p| self.submit(p),
            .admin_fsck_report => |p| self.report(p, scratch),
            .admin_volume => |p| self.volume(p),
            .admin_rekey => |p| self.rekey(p),
            else => unreachable,
        };
        return answer;
    }

    fn refusedCount(v: Verdict) Verdict {
        if (v != .ok) stats.admin_refusals.inc();
        return v;
    }

    /// The live table in id order, every record's wire bytes in the
    /// session's scratch (`MAX_JOBS × 96 B`, inside the control cap).
    fn list(self: *AdminService, scratch: []u8) service.Answer {
        var out: std.ArrayList(records.JobRecord) = .empty;
        defer out.deinit(self.fabric.gpa);
        self.fabric.listJobs(self.io, self.fabric.gpa, &out) catch return service.Answer.refused(error.OutOfMemory, "out of memory listing jobs");
        const n = @min(out.items.len, frames.LIST_ROWS_MAX);
        if (scratch.len < n * @sizeOf(frames.JobRow)) return service.Answer.refused(error.ValueTooLarge, "the job list does not fit the session's scratch");
        const rows = std.mem.bytesAsSlice(frames.JobRow, scratch[0 .. n * @sizeOf(frames.JobRow)]);
        for (out.items[0..n], rows) |r, *row| row.* = .{ .bytes = r.encode() };
        return .{ .ok = .{ .admin_job_list = .{ .head = .{ .n = @intCast(n), .verdict = @backingInt(Verdict.ok), .reserved = @splat(0) }, .rows = rows } } };
    }

    fn control(self: *AdminService, p: frames.AdminJobControlReq) service.Answer {
        const io = self.io;
        const r: fabric_mod.ControlError!void = switch (p.operation()) {
            .pause => self.fabric.pause(io, p.job_id),
            .@"resume" => self.fabric.resumeJob(io, p.job_id),
            .cancel => self.fabric.cancel(io, p.job_id),
            .throttle => self.fabric.setThrottle(io, p.job_id, p.pct),
        };
        const v: Verdict = if (r) .ok else |e| switch (e) {
            error.UnknownJob => .unknown_job,
            error.Terminal => .terminal,
            error.Io => .io,
            error.OutOfMemory => return service.Answer.refused(error.OutOfMemory, "out of memory"),
        };
        return .{ .ok = .{ .admin_job_control = frames.AdminVerdictAck.of(refusedCount(v)) } };
    }

    fn submit(self: *AdminService, p: frames.AdminJobSubmitReq) service.Answer {
        // The rotation sweep is minted by `admin_rekey` alone (the window
        // must be open and the plane's wrap swapped before any pass).
        if (p.jobType() == .rekey) return .{ .ok = .{ .admin_job_submit = .{ .job_id = 0, .verdict = @backingInt(refusedCount(.refused)), .reserved = @splat(0) } } };
        const id = self.fabric.submit(self.io, .{ .job_type = p.jobType(), .throttle_pct = p.throttle_pct, .arg0 = p.arg0, .arg1 = p.arg1, .paused = p.paused == 1 }) catch |e| {
            const v: Verdict = switch (e) {
                error.NotCoordinator => .not_coordinator,
                error.TableFull => .table_full,
                error.Io => .io,
                error.OutOfMemory => return service.Answer.refused(error.OutOfMemory, "out of memory"),
            };
            return .{ .ok = .{ .admin_job_submit = .{ .job_id = 0, .verdict = @backingInt(refusedCount(v)), .reserved = @splat(0) } } };
        };
        stats.admin_submissions.inc();
        log.info("admin lane: {s} job-{x:0>16} submitted (throttle {d} %, arg0 {x}, arg1 {x}{s})", .{ p.jobType().name(), id, p.throttle_pct, p.arg0, p.arg1, if (p.paused == 1) ", paused" else "" });
        return .{ .ok = .{ .admin_job_submit = .{ .job_id = id, .verdict = @backingInt(Verdict.ok), .reserved = @splat(0) } } };
    }

    fn report(self: *AdminService, p: frames.AdminFsckReportReq, scratch: []u8) service.Answer {
        const none: service.Answer = .{ .ok = .{ .admin_fsck_report = .{ .head = .{ .len = 0, .verdict = @backingInt(refusedCount(.no_report)), .reserved = @splat(0) }, .json = "" } } };
        const w = self.world orelse return none;
        const json = (w.vtable.fsckReport(w.ctx, self.fabric.gpa, p.job_id) catch return service.Answer.refused(error.OutOfMemory, "out of memory")) orelse return none;
        defer self.fabric.gpa.free(json);
        if (json.len > frames.REPORT_BYTES_MAX or json.len > scratch.len) return service.Answer.refused(error.ValueTooLarge, "the fsck report exceeds the admin cap");
        @memcpy(scratch[0..json.len], json);
        return .{ .ok = .{ .admin_fsck_report = .{ .head = .{ .len = @intCast(json.len), .verdict = @backingInt(Verdict.ok), .reserved = @splat(0) }, .json = scratch[0..json.len] } } };
    }

    fn volume(self: *AdminService, p: frames.AdminVolumeReq) service.Answer {
        const w = self.world orelse return .{ .ok = .{ .admin_volume = frames.AdminVolumeReply.refusal(refusedCount(.not_coordinator)) } };
        const gpa = self.fabric.gpa;
        const r: lattice.Error!VolumeOutcome = switch (p.head.operation()) {
            .add_data => w.vtable.addData(w.ctx, gpa, p.uri, p.head.no_rebalance == 0),
            .remove_data => w.vtable.removeData(w.ctx, gpa, p.head.vol_tag),
            .undrain => w.vtable.undrain(w.ctx, gpa, p.head.vol_tag),
        };
        const out = r catch |e| {
            const v: Verdict = switch (e) {
                error.LiveWriter => .not_coordinator,
                error.UnknownVolume, error.IllegalTransition, error.DeviceRefused, error.CapacityRefused, error.ConfigUndecodable => .refused,
                error.Io => .io,
                error.OutOfMemory => return service.Answer.refused(error.OutOfMemory, "out of memory"),
            };
            log.warn("admin lane: volume {s} refused ({s})", .{ @tagName(p.head.operation()), @errorName(e) });
            var rep = frames.AdminVolumeReply.refusal(refusedCount(v));
            rep.vol_tag = p.head.vol_tag;
            // The lattice's reason rides the reserved-free words the CLI
            // prints: the preflight terms are 0 unless the refusal was the
            // capacity closed form (then `needed > avail` tells the story).
            return .{ .ok = .{ .admin_volume = rep } };
        };
        return .{ .ok = .{ .admin_volume = .{
            .verdict = @backingInt(Verdict.ok),
            .reserved = @splat(0),
            .vol_tag = out.vol_tag,
            .job_id = out.job_id,
            .config_generation = out.config_generation,
            .needed = out.preflight.needed,
            .avail = out.preflight.avail,
            .transient = out.preflight.transient,
            .headroom = out.preflight.headroom,
            .cancelled = out.cancelled,
            .capacity_blocks = out.capacity_blocks,
        } } };
    }

    fn rekey(self: *AdminService, p: frames.AdminRekeyReq) service.Answer {
        const w = self.world orelse return .{ .ok = .{ .admin_rekey = frames.AdminRekeyReply.refusal(refusedCount(.not_coordinator), .none) } };
        const out = w.vtable.rekey(w.ctx, self.fabric.gpa, p) catch |e| {
            const reply: frames.AdminRekeyReply = switch (e) {
                error.OutOfMemory => return service.Answer.refused(error.OutOfMemory, "out of memory"),
                error.NotCoordinator => .refusal(.not_coordinator, .none),
                error.TableFull => .refusal(.table_full, .none),
                error.Io => .refusal(.io, .none),
                error.Plaintext => .refusal(.refused, .plaintext),
                error.WindowOpen => .refusal(.refused, .window_open),
                error.KeyUnresolved => .refusal(.refused, .key_unresolved),
                error.SameKey => .refusal(.refused, .same_key),
                error.Plane => .refusal(.io, .plane),
            };
            log.warn("admin lane: defrag --rekey refused ({s})", .{@errorName(e)});
            _ = refusedCount(reply.get());
            return .{ .ok = .{ .admin_rekey = reply } };
        };
        stats.admin_submissions.inc();
        log.info("admin lane: rekey job-{x:0>16} submitted (next key id {x}, throttle {d} %, config generation {d})", .{ out.job_id, &p.key_id, p.throttle_pct, out.config_generation });
        return .{ .ok = .{ .admin_rekey = .{ .verdict = @backingInt(Verdict.ok), .reason = 0, .reserved = @splat(0), .job_id = out.job_id, .config_generation = out.config_generation } } };
    }
};

/// The caller side (the CLI's `admin_lane.zig`, the tests): typed calls
/// over any `wire.Caller`-shaped function.
pub const Client = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, req: verbs.Request) @import("../../core/errors.zig").Error!verbs.Reply,

    pub const Error = @import("../../core/errors.zig").Error;

    pub fn list(self: Client, gpa: Allocator, out: *std.ArrayList(records.JobRecord)) Error!void {
        const rep = try self.call(self.ctx, .{ .admin_job_list = .{ .reserved = 0 } });
        for (rep.admin_job_list.rows) |row| {
            const rec = records.JobRecordV1.decode(&row.bytes) catch return error.WireMalformed;
            out.append(gpa, rec) catch return error.OutOfMemory;
        }
    }

    pub fn control(self: Client, job_id: u64, op: frames.ControlOp, pct: u8) Error!Verdict {
        const rep = try self.call(self.ctx, .{ .admin_job_control = .{ .job_id = job_id, .op = @backingInt(op), .pct = pct, .reserved = @splat(0) } });
        return rep.admin_job_control.get();
    }

    pub const Submitted = struct { job_id: u64, verdict: Verdict };

    pub fn submit(self: Client, s: fabric_mod.Submission) Error!Submitted {
        const rep = try self.call(self.ctx, .{ .admin_job_submit = .{ .job_type = @backingInt(s.job_type), .throttle_pct = s.throttle_pct, .paused = @intFromBool(s.paused), .reserved = @splat(0), .arg0 = s.arg0, .arg1 = s.arg1 } });
        return .{ .job_id = rep.admin_job_submit.job_id, .verdict = rep.admin_job_submit.get() };
    }

    /// The report JSON (owned by `gpa`) or null (`no_report`).
    pub fn fsckReport(self: Client, gpa: Allocator, job_id: u64) Error!?[]u8 {
        const rep = try self.call(self.ctx, .{ .admin_fsck_report = .{ .job_id = job_id } });
        const r = rep.admin_fsck_report;
        if (r.head.verdict != @backingInt(Verdict.ok)) return null;
        return gpa.dupe(u8, r.json) catch return error.OutOfMemory;
    }

    pub fn volume(self: Client, op: frames.VolumeOp, vol_tag: u64, uri: []const u8, no_rebalance: bool) Error!frames.AdminVolumeReply {
        const rep = try self.call(self.ctx, .{ .admin_volume = .{ .head = .{ .op = @backingInt(op), .no_rebalance = @intFromBool(no_rebalance), .reserved = @splat(0), .vol_tag = vol_tag, .uri_len = @intCast(uri.len), .reserved2 = 0 }, .uri = uri } });
        return rep.admin_volume;
    }

    pub fn rekey(self: Client, kdf_salt: [32]u8, key_id: [8]u8, throttle_pct: u8) Error!frames.AdminRekeyReply {
        const rep = try self.call(self.ctx, .{ .admin_rekey = .{ .kdf_salt = kdf_salt, .key_id = key_id, .throttle_pct = throttle_pct, .reserved = @splat(0) } });
        return rep.admin_rekey;
    }
};

// ---------------------------------------------------------------------------
// Tests over the loopback hub.
// ---------------------------------------------------------------------------

const testing = std.testing;
const loopback = @import("../../wire/loopback.zig");
const auth = @import("../../wire/auth.zig");
const errors = @import("../../core/errors.zig");

var seed: std.atomic.Value(u64) = .init(0xadbeef);
fn rnd() u64 {
    return seed.fetchAdd(0x9E37_79B9_7F4A_7C15, .monotonic) | 1;
}

const RamAdminWorld = struct {
    adds: u32 = 0,
    removes: u32 = 0,
    undrains: u32 = 0,
    refuse_remove: bool = false,
    report: ?[]const u8 = null,
    rekeys: u32 = 0,
    rekey_refuse: ?RekeyError = null,

    fn world(self: *RamAdminWorld) AdminWorld {
        return .{ .ctx = self, .vtable = &vt };
    }
    const vt: AdminWorld.VTable = .{ .addData = &addData, .removeData = &removeData, .undrain = &undrain, .fsckReport = &fsckReport, .rekey = &rekeyFn };
    fn self_(ctx: *anyopaque) *RamAdminWorld {
        return @ptrCast(@alignCast(ctx));
    }
    fn addData(ctx: *anyopaque, _: Allocator, uri: []const u8, rebalance: bool) lattice.Error!VolumeOutcome {
        const self = self_(ctx);
        if (uri.len == 0) return error.DeviceRefused;
        self.adds += 1;
        return .{ .vol_tag = 0x77, .job_id = if (rebalance) 0x1234 else 0, .config_generation = 2, .capacity_blocks = 512 };
    }
    fn removeData(ctx: *anyopaque, _: Allocator, vol_tag: u64) lattice.Error!VolumeOutcome {
        const self = self_(ctx);
        if (self.refuse_remove) return error.CapacityRefused;
        self.removes += 1;
        return .{ .vol_tag = vol_tag, .job_id = 0x5678, .config_generation = 3, .preflight = .{ .needed = 1, .avail = 2, .transient = 3, .headroom = 4 } };
    }
    fn undrain(ctx: *anyopaque, _: Allocator, vol_tag: u64) lattice.Error!VolumeOutcome {
        const self = self_(ctx);
        self.undrains += 1;
        return .{ .vol_tag = vol_tag, .config_generation = 4, .cancelled = 1 };
    }
    fn fsckReport(ctx: *anyopaque, gpa: Allocator, _: u64) Allocator.Error!?[]u8 {
        const self = self_(ctx);
        const r = self.report orelse return null;
        return try gpa.dupe(u8, r);
    }
    fn rekeyFn(ctx: *anyopaque, _: Allocator, req: frames.AdminRekeyReq) RekeyError!RekeyOutcome {
        const self = self_(ctx);
        if (self.rekey_refuse) |e| return e;
        self.rekeys += 1;
        return .{ .job_id = std.mem.readInt(u64, &req.key_id, .little), .config_generation = 9 };
    }
};

const ConnCaller = struct {
    conn: *loopback.Conn,
    fn client(self: *ConnCaller) Client {
        return .{ .ctx = self, .call = &call };
    }
    fn call(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
        const self: *ConnCaller = @ptrCast(@alignCast(ctx));
        return self.conn.call(req);
    }
};

const Rig = struct {
    hub: loopback.LoopbackHub,
    svc: service.Service = .init,
    store: fabric_mod.RamStore,
    fabric: Fabric,
    world: RamAdminWorld = .{},
    admin: AdminService,
    conn: loopback.Conn,
    cc: ConnCaller,

    fn init(gpa: Allocator, io: Io) !*Rig {
        const r = try gpa.create(Rig);
        r.* = .{ .hub = loopback.LoopbackHub.init(gpa, io), .store = fabric_mod.RamStore.init(gpa), .fabric = undefined, .admin = undefined, .conn = undefined, .cc = undefined };
        r.fabric = Fabric.init(gpa, r.store.store(), .{ .identity = .{ .node_token = 1, .mount_slot = 1 }, .pid = 1, .random = &rnd });
        r.admin = .{ .io = io, .fabric = &r.fabric, .world = r.world.world() };
        r.admin.register(&r.svc);
        const secret: auth.Secret = @splat(0x24);
        _ = try r.hub.addDaemon(&r.svc, secret, .{ .node_token = 1, .writer_id = 1, .mount_slot = 1, .reserved = 0 });
        _ = try r.hub.addDaemon(&r.svc, secret, .{ .node_token = 9, .writer_id = 9, .mount_slot = 0xFFFF_0001, .reserved = 0 });
        r.conn = try r.hub.connect(1, 0);
        r.cc = .{ .conn = &r.conn };
        return r;
    }

    fn deinit(self: *Rig, gpa: Allocator) void {
        self.conn.deinit();
        self.fabric.deinit();
        self.store.deinit();
        self.hub.deinit();
        gpa.destroy(self);
    }
};

test "the admin lane over the loopback: list is the live table; submit is coordinator-class (not_coordinator elsewhere); pause/resume/throttle/cancel answer their verdicts; a terminal job refuses control; an unknown id is unknown_job" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io);
    defer r.deinit(gpa);
    const c = r.cc.client();
    var list: std.ArrayList(records.JobRecord) = .empty;
    defer list.deinit(gpa);
    try c.list(gpa, &list);
    try testing.expectEqual(@as(usize, 0), list.items.len);
    // Not the coordinator (a joiner's fabric): submit refuses by verdict,
    // nothing written.
    r.fabric.setCoordinator(false);
    const refused = try c.submit(.{ .job_type = .noop, .arg0 = 4, .arg1 = 0 });
    try testing.expectEqual(Verdict.not_coordinator, refused.verdict);
    try testing.expectEqual(@as(u64, 0), refused.job_id);
    r.fabric.setCoordinator(true);
    const s = try c.submit(.{ .job_type = .noop, .throttle_pct = 50, .arg0 = 4, .arg1 = 0, .paused = true });
    try testing.expectEqual(Verdict.ok, s.verdict);
    try testing.expect(s.job_id != 0);
    try testing.expect(r.store.record(s.job_id) != null);
    list.clearRetainingCapacity();
    try c.list(gpa, &list);
    try testing.expectEqual(@as(usize, 1), list.items.len);
    try testing.expectEqual(records.JobState.paused, list.items[0].state);
    try testing.expectEqual(@as(u8, 50), list.items[0].throttle_pct);
    try testing.expectEqual(Verdict.ok, try c.control(s.job_id, .throttle, 25));
    try testing.expectEqual(@as(u8, 25), r.fabric.snapshot(io, s.job_id).?.throttle_pct);
    try testing.expectEqual(Verdict.ok, try c.control(s.job_id, .@"resume", 0));
    try testing.expectEqual(records.JobState.queued, r.fabric.snapshot(io, s.job_id).?.state);
    try testing.expectEqual(Verdict.ok, try c.control(s.job_id, .pause, 0));
    try testing.expectEqual(records.JobState.paused, r.fabric.snapshot(io, s.job_id).?.state);
    try testing.expectEqual(Verdict.ok, try c.control(s.job_id, .cancel, 0));
    try testing.expectEqual(records.JobState.cancelled, r.fabric.snapshot(io, s.job_id).?.state);
    try testing.expectEqual(Verdict.terminal, try c.control(s.job_id, .pause, 0));
    try testing.expectEqual(Verdict.unknown_job, try c.control(0xdead, .cancel, 0));
    // A malformed control (a pct on a pause) never reaches the handler.
    try testing.expectError(error.WireMalformed, r.conn.call(.{ .admin_job_control = .{ .job_id = s.job_id, .op = 0, .pct = 5, .reserved = @splat(0) } }));
}

test "the admin lane's volume verbs run the world; a lattice refusal is the `refused` verdict with the victim named; the fsck report rides whole or answers no_report" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io);
    defer r.deinit(gpa);
    const c = r.cc.client();
    const add = try c.volume(.add_data, 0, "/dev/d2", false);
    try testing.expectEqual(Verdict.ok, add.get());
    try testing.expectEqual(@as(u64, 0x77), add.vol_tag);
    try testing.expectEqual(@as(u64, 0x1234), add.job_id);
    try testing.expectEqual(@as(u32, 512), add.capacity_blocks);
    try testing.expectEqual(@as(u32, 1), r.world.adds);
    const rm = try c.volume(.remove_data, 0x77, "", false);
    try testing.expectEqual(Verdict.ok, rm.get());
    try testing.expectEqual(@as(u64, 4), rm.headroom);
    try testing.expectEqual(@as(u64, 0x5678), rm.job_id);
    r.world.refuse_remove = true;
    const rm2 = try c.volume(.remove_data, 0x77, "", false);
    try testing.expectEqual(Verdict.refused, rm2.get());
    try testing.expectEqual(@as(u64, 0x77), rm2.vol_tag);
    const un = try c.volume(.undrain, 0x77, "", false);
    try testing.expectEqual(Verdict.ok, un.get());
    try testing.expectEqual(@as(u32, 1), un.cancelled);
    try testing.expectEqual(@as(?[]u8, null), try c.fsckReport(gpa, 5));
    r.world.report = "{\"schema\":1}";
    const got = (try c.fsckReport(gpa, 5)).?;
    defer gpa.free(got);
    try testing.expectEqualStrings("{\"schema\":1}", got);
    // The rotation: the world's outcome rides whole; its typed refusals
    // name their class; a generic submit of the sweep type is refused.
    const rk = try c.rekey(@splat(3), .{ 8, 7, 6, 5, 4, 3, 2, 1 }, 25);
    try testing.expectEqual(Verdict.ok, rk.get());
    try testing.expectEqual(@as(u64, 9), rk.config_generation);
    try testing.expectEqual(@as(u32, 1), r.world.rekeys);
    r.world.rekey_refuse = error.KeyUnresolved;
    const rk2 = try c.rekey(@splat(3), .{ 8, 7, 6, 5, 4, 3, 2, 1 }, 25);
    try testing.expectEqual(Verdict.refused, rk2.get());
    try testing.expectEqual(frames.RekeyRefusal.key_unresolved, rk2.why());
    try testing.expectEqual(Verdict.refused, (try c.submit(.{ .job_type = .rekey })).verdict);
    // Without a world the volume verbs answer not_coordinator (a joiner).
    r.admin.world = null;
    try testing.expectEqual(Verdict.not_coordinator, (try c.volume(.undrain, 0x77, "", false)).get());
    try testing.expectEqual(Verdict.not_coordinator, (try c.rekey(@splat(3), @splat(1), 0)).get());
}
