//! Arming the lane for a data volume at volume open (charter §6.3;
//! `docs/porting/ops_zcrx.md` §1.1, §8; QUIRK 11 taken: the arm runs at
//! open on the opening thread — a multi-second control-plane bring-up never
//! rides a data op, and the read path only loads the published session).
//! Linux only.
//!
//! `armVolume` = the knobs → the target (the `SQUEEZEFS_TEST_ZCRX_TARGET`
//! seam or the sysfs probe of the device node) → the ladder's live answers
//! → the verdict announced ONCE per device (`zcrx_lane_absent_reason`) →
//! on `present`, the association connected under the mount's identity pair
//! and published to the funnel as a `Lane`. Every refusal leaves the kernel
//! path byte-identical.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;

const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const derive = @import("../../core/derive.zig");
const mem_budget = @import("../../mem/budget.zig");
const initiator = @import("../nvmet/initiator.zig");
const probe = @import("probe.zig");
const nic_census = @import("nic_census.zig");
const steering = @import("steering.zig");
const association = @import("association.zig");
const ethtool = @import("ethtool.zig");
const funnel = @import("funnel.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("zcrx/arm is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// A published session and its funnel face; owned by the data plane.
pub const Bound = struct {
    gpa: Allocator,
    session: *association.Session,
    lane: funnel.Lane,
    nic: ?struct { nic: *ethtool.EthtoolNic, reg: *steering.Registry, idx: u8 } = null,

    pub fn deinit(self: *Bound) void {
        self.session.deinit();
        if (self.nic) |n| {
            n.nic.close();
            self.gpa.destroy(n.nic);
        }
        self.gpa.destroy(self);
    }

    const vt: funnel.Lane.VTable = .{ .poisoned = &poisoned, .structural = &structural, .degraded = &degraded, .read = &read, .retire = &retire };

    fn me(ctx: *anyopaque) *Bound {
        return @ptrCast(@alignCast(ctx));
    }
    fn poisoned(ctx: *anyopaque) bool {
        return me(ctx).session.isPoisoned();
    }
    fn structural(ctx: *anyopaque) bool {
        return me(ctx).session.isStructurallyStarved();
    }
    fn degraded(ctx: *anyopaque) bool {
        return me(ctx).session.isDegraded();
    }
    fn read(ctx: *anyopaque, off: u64, dest: []u8, registered: bool) funnel.ReadError!void {
        return me(ctx).session.readInto(off, dest, registered);
    }
    /// Retirement: the session tears down (idempotent); the plane frees the
    /// `Bound` at its own teardown.
    fn retire(ctx: *anyopaque) void {
        me(ctx).session.teardown();
    }
};

/// The lane for `device_path` (an `/dev/nvme<C>n<N>` node), or null with
/// the verdict announced. `env` carries the knobs and the identity pair.
pub fn armVolume(gpa: Allocator, io: Io, env: *const std.process.Environ.Map, reg: *steering.Registry, device_path: []const u8, facts: derive.Facts, budget_level: mem_budget.Level) ?*Bound {
    const k = probe.Knobs.read(env);
    if (!k.lane_enabled) {
        probe.announce(.off, device_path);
        return null;
    }
    if (k.target_override_malformed) {
        log.warn("zcrx lane: {s}: SQUEEZEFS_TEST_ZCRX_TARGET is malformed — the lane stays Absent (never a guessed coordinate)", .{device_path});
        probe.announce(.{ .target_ineligible = error.AddressMalformed }, device_path);
        return null;
    }
    const target: nic_census.Refusal!nic_census.Target = if (k.target_override) |t| t else nic_census.nvmeTcpTargetFor(io, "/sys", device_path, facts.cpus);
    var answers: probe.Answers = .{ .lane_enabled = true, .r5_red = budget_level == .red, .target = target, .area_sim = k.area_sim, .cpus = facts.cpus };
    if (target) |t| {
        if (!k.area_sim) probe.linux_probe.collect(&answers, reg, io, t);
    } else |_| {}
    const verdict = probe.arm(answers);
    probe.announce(verdict, device_path);
    const plan = switch (verdict) {
        .present => |p| p,
        else => return null,
    };
    const identity = resolveIdentity(env) orelse {
        log.warn("zcrx lane: {s}: no host identity pair (SQUEEZEFS_HOSTNQN/HOSTID, /etc/nvme or the machine id) — the lane stays Absent", .{device_path});
        probe.announce(.connect_failed, device_path);
        if (plan.lease) |*l| reg.release(l);
        return null;
    };
    const bound = gpa.create(Bound) catch return null;
    bound.* = .{ .gpa = gpa, .session = undefined, .lane = .{ .ctx = bound, .vtable = &Bound.vt } };
    var nic_binding: ?association.NicBinding = null;
    if (plan.backend == .zcrx) {
        const nic = gpa.create(ethtool.EthtoolNic) catch {
            gpa.destroy(bound);
            return null;
        };
        nic.* = ethtool.EthtoolNic.open(plan.ifname[0..plan.ifname_len]) catch {
            gpa.destroy(nic);
            gpa.destroy(bound);
            if (plan.lease) |*l| reg.release(l);
            return null;
        };
        const idx = reg.nic(plan.ifindex) orelse 0;
        bound.nic = .{ .nic = nic, .reg = reg, .idx = idx };
        nic_binding = .{ .reg = reg, .nic = idx, .ctl = nic.control() };
    }
    bound.session = association.Session.connect(gpa, io, plan, identity, k.sim_chunk, nic_binding) catch |e| {
        stats.zcrx_conn_errors.inc();
        log.err("zcrx lane: {s}: the association refused ({s}); the kernel path serves the device", .{ device_path, @errorName(e) });
        probe.announce(.connect_failed, device_path);
        if (bound.nic) |n| {
            n.nic.close();
            gpa.destroy(n.nic);
        }
        if (plan.lease) |*l| reg.release(l);
        gpa.destroy(bound);
        return null;
    };
    if (nic_binding) |b| b.reg.noteArmed(b.nic);
    return bound;
}

/// The mount's identity pair (explicit > /etc/nvme > machine id), as the
/// association needs it.
fn resolveIdentity(env: *const std.process.Environ.Map) ?association.Identity {
    return switch (initiator.resolveHostIdentity(env, .{})) {
        .identity => |id| association.Identity.fromPair(id.hostnqn(), id.hostid()),
        .refusal => null,
    };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "armVolume: the lever off is silent-Absent (code 0, no refusal counted); a malformed target seam is loud-Absent; an ineligible node announces its verdict" {
    const gpa = testing.allocator;
    var reg = steering.Registry.init(gpa);
    defer reg.deinit();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const facts = derive.Facts.synthetic(8, 16 << 30);
    const refusals0 = stats.zcrx_probe_refusals.load();
    try testing.expect(armVolume(gpa, testing.io, &env, &reg, "/dev/nvme0n1", facts, .green) == null);
    try testing.expectEqual(@as(u64, 0), stats.zcrx_lane_absent_reason.load());
    try testing.expectEqual(refusals0, stats.zcrx_probe_refusals.load());
    try env.put("SQUEEZEFS_ZCRX_LANE", "1");
    try env.put("SQUEEZEFS_TEST_ZCRX_TARGET", "garbage");
    try testing.expect(armVolume(gpa, testing.io, &env, &reg, "/dev/nvme0n1", facts, .green) == null);
    try testing.expectEqual(refusals0 + 1, stats.zcrx_probe_refusals.load());
    try testing.expectEqual(@as(u64, (probe.Verdict{ .target_ineligible = error.AddressMalformed }).code()), stats.zcrx_lane_absent_reason.load());
    _ = env.swapRemove("SQUEEZEFS_TEST_ZCRX_TARGET");
    // A node that is not an nvme-tcp namespace on this host.
    try testing.expect(armVolume(gpa, testing.io, &env, &reg, "/dev/sda", facts, .green) == null);
    try testing.expectEqual(refusals0 + 2, stats.zcrx_probe_refusals.load());
    // R5 Red blocks new arms before any probe.
    try env.put("SQUEEZEFS_TEST_ZCRX_TARGET", "127.0.0.1,4420,nqn.x,1,9,1048576");
    try testing.expect(armVolume(gpa, testing.io, &env, &reg, "/dev/nvme0n1", facts, .red) == null);
    try testing.expectEqual(@as(u64, (probe.Verdict{ .r5_red = {} }).code()), stats.zcrx_lane_absent_reason.load());
}
