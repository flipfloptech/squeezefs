//! The daemon-owned data-plane connect at `mount` and its release at the
//! clean `umount` (charter §8 "NVMe-oF targets"; `docs/porting/
//! ops_nvmet_keys.md` §3.7 KD-MW-15; the rung-8a3 owed row). The set
//! config's `FabricEndpoint` record per data volume is the operator's
//! declaration that THIS daemon owns the volume's fabric association: at
//! mount, before discovery opens any data device, the daemon resolves its
//! own host identity (the explicit pair → `/etc/nvme` both → the
//! machine-id derivation SALTED BY THE MOUNTPOINT, so co-located mounts
//! never share a controller and a remount at the same mountpoint presents
//! the same identity — the reservation's identity stamp never reads a
//! change), reuses a controller already riding that identity for the
//! record's subsystem (a crashed predecessor's association) or connects
//! one (`initiator.connect`: the uevent-driven readiness wait), probes the
//! node's DMA grain (`align_probe`) and hands discovery the LIVE node path
//! — the data superblock read right after is the identity law (`vol_tag`,
//! `device_uuid`, `set_uuid`): a wrong namespace refuses by name and the
//! association is released with the mount.
//!
//! The §5.2 ladder (`initiator.fabricLadderVerdict`) judges every other
//! volume: with an explicit pair a META volume's controller must carry
//! the pair (rule 2) and a DATA volume without a record refuses UNIFORMLY
//! naming `config set-fabric-endpoints`; with no explicit pair everything
//! accepts unchanged. The one departure from the reference (decision 0002
//! row 486): a record-covered data volume is a daemon connect UNDER THE
//! RESOLVED identity even with no explicit pair — the record IS the
//! operator's intent, and opening the recorded path "as-is" would bind a
//! renumbered node.
//!
//! Ownership = identity (the reference's `--identity-only` law): at the
//! teardown, after every device fd closed, the daemon deletes exactly the
//! controllers of its records' subsystems that carry ITS pair — a
//! co-located mount's (another pair) or the operator's (the host default)
//! association is never touched, and `/sys/class/nvme` reads as the mount
//! found it. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const align_probe = @import("../../io/align_probe.zig");
const pr = @import("../../meta/guard/pr.zig");
const proc = @import("../../meta/guard/proc.zig");
const set_config = @import("../../data/set_config.zig");
const initiator = @import("initiator.zig");
const configfs = @import("configfs.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/mount_fabric is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// One association this mount owns: the record's subsystem and the
/// controller instance the connect (or the reuse) resolved.
pub const Bound = struct {
    vol_tag: u64,
    instance: u32,
    reused: bool,
    subnqn_buf: [configfs.NVMF_NQN_SIZE]u8 = undefined,
    subnqn_len: usize = 0,
    device_buf: [40]u8 = undefined,
    device_len: usize = 0,

    pub fn subnqn(self: *const Bound) []const u8 {
        return self.subnqn_buf[0..self.subnqn_len];
    }
    pub fn device(self: *const Bound) []const u8 {
        return self.device_buf[0..self.device_len];
    }
};

/// Why the mount refused (the operator text names the volume and the
/// ladder's remedy).
pub const Refusal = union(enum) {
    identity: initiator.IdentityRefusal,
    ladder: struct { vol_tag: u64, path: []const u8, verdict: initiator.Verdict },
    connect: struct { vol_tag: u64, subnqn: []const u8, err: initiator.ConnectError, spec: initiator.ConnectSpec },
    /// The connected node's DMA grain is past what the planes admit.
    grain: struct { vol_tag: u64, path: []const u8, grain: u64 },
    out_of_memory,

    pub fn write(self: Refusal, w: *Io.Writer) Io.Writer.Error!void {
        switch (self) {
            .identity => |r| {
                try w.writeAll("fabric host identity: ");
                try r.write(w);
            },
            .ladder => |l| {
                if (l.vol_tag != 0) try w.print("data volume vol-{x:0>16} ({s}): ", .{ l.vol_tag, l.path }) else try w.print("metadata volume {s}: ", .{l.path});
                try l.verdict.write(w);
            },
            .connect => |c| {
                try w.print("data volume vol-{x:0>16}: the daemon's connect of {s} failed: ", .{ c.vol_tag, c.subnqn });
                try initiator.writeConnectRefusal(w, c.err, c.spec);
            },
            .grain => |g| try w.print("data volume vol-{x:0>16}: the connected namespace {s} reports a DMA grain of {d} bytes, past the {d} the data plane admits", .{ g.vol_tag, g.path, g.grain, align_probe.max_grain }),
            .out_of_memory => try w.writeAll("out of memory binding the fabric endpoints"),
        }
    }
};

/// The mount's fabric plan: the identity it connects under and what it
/// bound; `release` is the teardown's.
pub const Plan = struct {
    gpa: Allocator,
    identity: ?initiator.Identity = null,
    bound: std.ArrayList(Bound) = .empty,

    pub fn init(gpa: Allocator) Plan {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *Plan) void {
        self.bound.deinit(self.gpa);
    }

    /// The pair as the ladder and the disconnect filter compare it.
    pub fn pair(self: *const Plan) ?initiator.HostPair {
        const id = &(self.identity orelse return null);
        return .{ .hostnqn = id.hostnqn(), .hostid = id.hostid() };
    }

    /// Bind every data volume of `cfg` (in place: a bound volume's `uri`
    /// becomes the live node path) and judge the META paths. `mountpoint`
    /// salts the derived identity.
    pub fn bind(self: *Plan, env: *const std.process.Environ.Map, cfg: *set_config.Config, meta_paths: []const []const u8, mountpoint: []const u8, diag: *Refusal) error{Refused}!void {
        var any_record = false;
        for (cfg.data) |d| if (d.state != .retired and d.fabric != null) {
            any_record = true;
        };
        const explicit: ?initiator.HostPair = switch (pr.resolveExplicitIdentity(env, .{})) {
            .refusal => |r| {
                diag.* = .{ .identity = .{ .pair = r } };
                return error.Refused;
            },
            .pair => |p| .{ .hostnqn = p.hostnqn, .hostid = p.hostid },
            .none => null,
        };
        // Nothing to connect and nothing to judge: a local set, or the
        // operator's connects under the host default.
        if (!any_record and explicit == null) return;
        if (any_record) {
            switch (resolveMountIdentity(env, mountpoint)) {
                .identity => |id| self.identity = id,
                .refusal => |r| {
                    diag.* = .{ .identity = r };
                    return error.Refused;
                },
            }
            const id = &self.identity.?;
            log.info("mount: fabric host identity hostnqn={s} hostid={s} ({s}); {s}", .{ id.hostnqn(), id.hostid(), @tagName(id.source), initiator.readHostScoped().announce() });
        }
        // The META plane: rule 2 under an explicit pair.
        if (explicit) |want| for (meta_paths) |path| {
            var shape = Shape.of(self.gpa, path);
            const v = initiator.fabricLadderVerdict(shape.volume(.meta, false), want);
            switch (v) {
                .accept => {},
                .accept_unverified => {
                    var vb: [512]u8 = undefined;
                    var vw: Io.Writer = .fixed(&vb);
                    v.write(&vw) catch {};
                    log.warn("mount: metadata volume {s}: {s}", .{ path, vw.buffered() });
                },
                else => {
                    stats.fabric_ladder_refusals.inc();
                    diag.* = .{ .ladder = .{ .vol_tag = 0, .path = path, .verdict = v } };
                    return error.Refused;
                },
            }
        };
        // The DATA plane.
        for (cfg.data) |*d| {
            if (d.state == .retired) continue;
            if (d.fabric) |f| {
                try self.bindVolume(d, f, diag);
                continue;
            }
            const want = explicit orelse continue;
            var shape = Shape.of(self.gpa, d.uri);
            const v = initiator.fabricLadderVerdict(shape.volume(.data, false), want);
            if (v != .accept) {
                stats.fabric_ladder_refusals.inc();
                diag.* = .{ .ladder = .{ .vol_tag = d.vol_tag, .path = d.uri, .verdict = v } };
                return error.Refused;
            }
        }
    }

    /// The reuse-or-connect of one record; the volume's `uri` becomes the
    /// live node.
    fn bindVolume(self: *Plan, d: *set_config.DataVolume, f: set_config.FabricEndpoint, diag: *Refusal) error{Refused}!void {
        const id = &self.identity.?;
        var b: Bound = .{ .vol_tag = d.vol_tag, .instance = 0, .reused = false };
        @memcpy(b.subnqn_buf[0..f.subnqn.len], f.subnqn);
        b.subnqn_len = f.subnqn.len;
        var ab: [48]u8 = undefined;
        const spec: initiator.ConnectSpec = .{ .trtype = switch (f.trtype) {
            .tcp => .tcp,
            .loop => .loop,
        }, .traddr = f.traddrText(&ab), .trsvcid = f.trsvcid, .subnqn = f.subnqn, .hostnqn = id.hostnqn(), .hostid = id.hostid() };
        var dev_path: [40]u8 = undefined;
        var dev: []const u8 = "";
        if (self.findOurs(f.subnqn)) |ours| {
            b.instance = ours.instance;
            b.reused = true;
            dev = ours.ns.blockPath(&dev_path);
            stats.fabric_daemon_reuses.inc();
            log.warn("mount: data volume vol-{x:0>16}: reusing controller nvme{d} ({s}) already riding this mount's identity for {s} — a previous incarnation's association; it is released with this mount", .{ d.vol_tag, ours.instance, dev, f.subnqn });
        } else {
            const c = initiator.connect(self.gpa, spec, null) catch |e| {
                stats.fabric_daemon_connect_failures.inc();
                diag.* = .{ .connect = .{ .vol_tag = d.vol_tag, .subnqn = f.subnqn, .err = e, .spec = spec } };
                return error.Refused;
            };
            b.instance = c.instance;
            dev = c.ns.blockPath(&dev_path);
            stats.fabric_daemon_connects.inc();
            log.info("mount: data volume vol-{x:0>16}: connected {s} as nvme{d} (cntlid {d}) → {s} (nsid {d})", .{ d.vol_tag, f.subnqn, c.instance, c.cntlid, dev, c.ns.nsid });
        }
        @memcpy(b.device_buf[0..dev.len], dev);
        b.device_len = dev.len;
        // The connect is owned from here: a refusal below releases it.
        self.bound.append(self.gpa, b) catch {
            initiator.deleteController(b.instance) catch {};
            diag.* = .out_of_memory;
            return error.Refused;
        };
        // The grain the planes will DMA at (O_DIRECT on every data device).
        const probe = align_probe.probeGrain(dev);
        _ = align_probe.admitMetaIoGrain(probe.grain) catch {
            diag.* = .{ .grain = .{ .vol_tag = d.vol_tag, .path = b.device(), .grain = probe.grain } };
            return error.Refused;
        };
        log.info("mount: data volume vol-{x:0>16}: {s} DMA grain {d} ({s})", .{ d.vol_tag, dev, probe.grain, @tagName(probe.source) });
        const live = self.gpa.dupe(u8, dev) catch {
            diag.* = .out_of_memory;
            return error.Refused;
        };
        self.gpa.free(d.uri);
        d.uri = live;
    }

    /// A live controller of `subnqn` carrying our pair with a namespace
    /// node under it (the reuse arm).
    fn findOurs(self: *Plan, subnqn: []const u8) ?initiator.Connected {
        const p = self.pair() orelse return null;
        const ctrls = initiator.listControllers(self.gpa, initiator.SYSFS_ROOT) catch return null;
        defer self.gpa.free(ctrls);
        for (ctrls) |c| {
            if (!std.mem.eql(u8, c.subsysnqnOf(), subnqn) or !c.hasIdentity(p.hostnqn, p.hostid)) continue;
            if (!c.isLive()) continue;
            const ns = (initiator.controllerNamespace(self.gpa, initiator.SYSFS_ROOT, c.instance) catch null) orelse continue;
            if (!ns.char_present) continue;
            return .{ .instance = c.instance, .cntlid = 0, .ns = ns };
        }
        return null;
    }

    /// The teardown: every controller of a bound subsystem under our pair
    /// (`--identity-only`), each awaited gone. After the device fds closed.
    pub fn release(self: *Plan) void {
        const p = self.pair() orelse return;
        for (self.bound.items) |b| {
            const n = initiator.disconnect(self.gpa, .{ .subsysnqn = b.subnqn(), .identity = .{ .hostnqn = p.hostnqn, .hostid = p.hostid } }) catch |e| {
                log.err("umount: data volume vol-{x:0>16}: the disconnect of {s} failed ({s}) — `squeezefs nvmeof disconnect --identity-only {s}`", .{ b.vol_tag, b.subnqn(), @errorName(e), b.subnqn() });
                continue;
            };
            stats.fabric_daemon_disconnects.add(n);
            _ = initiator.awaitControllerGone(b.instance);
            log.info("umount: data volume vol-{x:0>16}: {d} controller(s) of {s} under this mount's identity released", .{ b.vol_tag, n, b.subnqn() });
        }
        self.bound.clearRetainingCapacity();
    }

    /// The `.stats` rows: the bound volumes.
    pub fn writeStatsJson(self: *const Plan, w: *Io.Writer) Io.Writer.Error!void {
        try w.writeAll("\"fabric_daemon_bound\":[");
        for (self.bound.items, 0..) |b, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"vol_tag\":\"vol-{x:0>16}\",\"subnqn\":\"{s}\",\"instance\":{d},\"device\":\"{s}\",\"reused\":{s}}}", .{ b.vol_tag, b.subnqn(), b.instance, b.device(), if (b.reused) "true" else "false" });
        }
        try w.writeByte(']');
    }
};

/// The data-plane connect identity: the explicit pair → `/etc/nvme` both
/// → the machine-id derivation salted by the mountpoint (stable per mount
/// slot: a remount at the same mountpoint presents the same pair; two
/// mountpoints on one host never share a controller).
pub fn resolveMountIdentity(env: *const std.process.Environ.Map, mountpoint: []const u8) initiator.Resolved {
    const etc = proc.EtcNvme.read();
    var mb: [128]u8 = undefined;
    const mid = proc.readSmall(initiator.MACHINE_ID_PATH, &mb);
    const r = resolveMountIdentityWith(env, &etc, mid, mountpoint);
    if (r == .identity) stats.fabric_host_identity_source.set(@backingInt(r.identity.source));
    return r;
}

/// Pure over the sources (the fixture tests).
pub fn resolveMountIdentityWith(env: *const std.process.Environ.Map, etc: *const proc.EtcNvme, machine_id: ?[]const u8, mountpoint: []const u8) initiator.Resolved {
    const mid = std.mem.trim(u8, machine_id orelse "", " \t\r\n");
    var salted: [128 + linux.PATH_MAX]u8 = undefined;
    const salted_id: ?[]const u8 = if (mid.len == 0) null else std.fmt.bufPrint(&salted, "{s}\x00{s}", .{ mid, mountpoint }) catch null;
    return initiator.resolveIdentity(env, .{}, etc, salted_id);
}

/// The ladder's shape for a device path: the controller serving it (null
/// = a local device / a regular file) and the merged multipath shape of
/// its subsystem. The controller's attrs live in this value, so the
/// verdict's slices borrow from it.
const Shape = struct {
    ctrl: ?initiator.Controller,
    merged: bool,

    fn of(gpa: Allocator, path: []const u8) Shape {
        const c = controllerOfNode(path) orelse return .{ .ctrl = null, .merged = false };
        return .{ .ctrl = c, .merged = initiator.multipathMergedShape(gpa, initiator.SYSFS_ROOT, c.subsysnqnOf()) catch false };
    }

    fn volume(self: *Shape, plane: initiator.Plane, has_record: bool) initiator.VolumeShape {
        return .{ .plane = plane, .has_record = has_record, .actual = if (self.ctrl) |*c| .{ .hostnqn = c.hostnqnOf(), .hostid = c.hostidOf() } else null, .merged_shape = self.merged };
    }
};

/// `/dev/nvme<N>n<Y>` → the fabric controller `nvme<N>` (the per-
/// controller node of a non-multipath kernel); honest null for anything
/// else (a file, a PCIe namespace reads as a non-fabric controller → null).
pub fn controllerOfNode(path: []const u8) ?initiator.Controller {
    const base = std.fs.path.basename(path);
    const parts = initiator.splitNamespaceName(base) orelse return null;
    var nb: [16]u8 = undefined;
    const name = std.fmt.bufPrint(&nb, "nvme{d}", .{parts.ctrl}) catch return null;
    const c = initiator.readController(initiator.SYSFS_ROOT, name) orelse return null;
    if (!c.isFabric()) return null;
    return c;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the mount identity: the explicit pair wins; the derived default is salted by the mountpoint (two mountpoints differ, a remount at one agrees, the unsalted verb identity differs from both); no source refuses" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var etc: proc.EtcNvme = .{};
    const a1 = resolveMountIdentityWith(&env, &etc, "0123456789abcdef0123456789abcdef\n", "/mnt/a");
    const a2 = resolveMountIdentityWith(&env, &etc, "0123456789abcdef0123456789abcdef", "/mnt/a");
    const b = resolveMountIdentityWith(&env, &etc, "0123456789abcdef0123456789abcdef", "/mnt/b");
    try testing.expect(a1 == .identity and a2 == .identity and b == .identity);
    try testing.expectEqual(initiator.IdentitySource.machine_id, a1.identity.source);
    try testing.expectEqualStrings(a1.identity.hostid(), a2.identity.hostid());
    try testing.expect(!std.mem.eql(u8, a1.identity.hostid(), b.identity.hostid()));
    const verb = initiator.resolveIdentity(&env, .{}, &etc, "0123456789abcdef0123456789abcdef");
    try testing.expect(!std.mem.eql(u8, verb.identity.hostid(), a1.identity.hostid()));
    try testing.expect(std.mem.startsWith(u8, a1.identity.hostnqn(), initiator.HOSTNQN_UUID_PREFIX));
    try testing.expect(resolveMountIdentityWith(&env, &etc, null, "/mnt/a").refusal == .no_source);
    try env.put("SQUEEZEFS_HOSTNQN", "nqn.2014-08.org.nvmexpress:uuid:2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c");
    try env.put("SQUEEZEFS_HOSTID", "2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c");
    const e = resolveMountIdentityWith(&env, &etc, null, "/mnt/a");
    try testing.expectEqual(initiator.IdentitySource.explicit, e.identity.source);
    try testing.expectEqualStrings("2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c", e.identity.hostid());
}

test "a plan over a config with no records and no explicit pair binds nothing; a half pair refuses; a non-NVMe path has no controller" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const vols = [_]set_config.DataVolume{.{ .vol_tag = 0x1111_2222_3333_4444, .state = .active, .device_uuid = @splat(1), .added_ts = 1_700_000_000, .capacity_blocks = 254, .volume_len = 1 << 30, .uri = "/build/tmp/mount-fabric-d0.img" }};
    var cfg = try set_config.birth(gpa, @splat(7), .{ .compression = .none, .encryption = .none, .block_size = 4 << 20 }, null, &vols, &.{});
    defer cfg.deinit(gpa);
    var plan = Plan.init(gpa);
    defer plan.deinit();
    var diag: Refusal = undefined;
    try plan.bind(&env, &cfg, &.{"/build/tmp/not-a-device"}, "/mnt/x", &diag);
    try testing.expect(plan.identity == null and plan.bound.items.len == 0);
    try env.put("SQUEEZEFS_HOSTNQN", "nqn.2014-08.org.nvmexpress:uuid:2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c");
    try testing.expectError(error.Refused, plan.bind(&env, &cfg, &.{}, "/mnt/x", &diag));
    try testing.expect(diag == .identity);
    try testing.expect(controllerOfNode("/build/tmp/some.img") == null);
    try testing.expect(controllerOfNode("/dev/nvme99999n1") == null);
    var buf: [256]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try plan.writeStatsJson(&w);
    try testing.expectEqualStrings("\"fabric_daemon_bound\":[]", w.buffered());
}
