//! The 14-gate arm ladder (charter §1.4 "probe, never kernel version", §6.3;
//! `docs/porting/ops_zcrx.md` §1.1): a PURE state machine over injected
//! answers — every gate refuses with a typed verdict and leaves the kernel
//! path byte-identical; only steering mutates the NIC and it is the LAST
//! step and rolls itself back. The Linux half collects the answers from
//! the live host (`io_uring_setup + REGISTER_PROBE`, the route probe, the
//! genetlink HDS verdict, the ethtool controls, the sysfs census).
//!
//! Two strata only: `present` (the plan: a backend, the NIC facts, the
//! queue lease) or an `Absent` verdict whose tag is published on
//! `zcrx_lane_absent_reason` and logged ONCE per device per mount. On the
//! venue (loopback target, a 1-queue virtio NIC, no HDS attribute) the
//! ladder's own verdict is `no_hds` — exactly the stock-graceful stratum;
//! the test seam `SQUEEZEFS_TEST_ZCRX_AREA_SIM` arms the `RECV` leg at gate
//! 4 so the association runs for real against nvmet-tcp without a NIC.
//!
//! Verdict classes: TERMINAL (no `RECV_ZC`, no HDS, too narrow, not an IP
//! literal, ineligible target) latch for the mount; TRANSIENT (R5 Red, rxq
//! exhausted, a connect failure) are retried by the volume's health tick
//! (QUIRK 13 taken: a transient refusal at mount time no longer disables
//! the lane forever).

const std = @import("std");
const builtin = @import("builtin");
const derive = @import("../../core/derive.zig");
const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const area = @import("area.zig");
const steering = @import("steering.zig");
const nic_census = @import("nic_census.zig");
const stats = @import("stats.zig");

pub const Backend = enum(u8) {
    /// Multishot `RECV_ZC` into the registered area (the NIC leg).
    zcrx,
    /// Plain `RECV` into granted chunks (the venue correctness leg).
    sim,
};

pub const Plan = struct {
    backend: Backend,
    target: nic_census.Target,
    io_queues: u32,
    depth: u32,
    /// NIC facts (zcrx only; zero/null under sim).
    ifindex: u32 = 0,
    ifname: [16]u8 = @splat(0),
    ifname_len: u8 = 0,
    rx_queues: [steering.MAX_FLOWS]u32 = @splat(0),
    numa_node: ?u16 = null,
    mtu: ?u32 = null,
    rx_descs: u32 = 0,
    /// The queue lease to release at teardown (zcrx only).
    lease: ?steering.Registry.Lease = null,
};

pub const Verdict = union(enum) {
    /// Gate 1: the opt-in is off (silent; the default).
    off,
    /// Gate 2: R5 Red blocks NEW arms.
    r5_red,
    /// Gate 3: not an eligible nvme-tcp namespace.
    target_ineligible: nic_census.Refusal,
    /// Gate 5: the kernel lacks `RECV_ZC`.
    no_recv_zc,
    /// Gate 6: `traddr` is not an IP literal.
    traddr_not_ip,
    /// Gate 7: no route to the target.
    no_route,
    /// Gate 8: header/data split is not ENABLED on the NIC.
    no_hds,
    /// Gate 9: the interface has no index.
    no_ifindex,
    /// Gate 10: the capacity gate (ntuple off, no rule slots).
    capacity_refused,
    /// Gate 11: `channels/4 == 0` or a zero fair want.
    too_narrow,
    /// Gate 12: the rxq arbiter had nothing to lease.
    rxq_exhausted,
    /// Gate 14: the association refused.
    connect_failed,
    present: Plan,

    pub const Tag = std.meta.Tag(Verdict);

    pub fn code(self: Verdict) u8 {
        return @backingInt(std.meta.activeTag(self));
    }

    /// TERMINAL verdicts latch for the mount; the rest are retried.
    pub fn terminal(self: Verdict) bool {
        return switch (self) {
            .off, .target_ineligible, .no_recv_zc, .traddr_not_ip, .no_route, .no_hds, .no_ifindex, .capacity_refused, .too_narrow => true,
            .r5_red, .rxq_exhausted, .connect_failed, .present => false,
        };
    }

    pub fn describe(self: Verdict) []const u8 {
        return switch (self) {
            .off => "SQUEEZEFS_ZCRX_LANE is off",
            .r5_red => "the R5 memory budget is Red: new arms are blocked (sheds nothing)",
            .target_ineligible => "the device is not an eligible nvme-tcp namespace (/dev/nvme<C>n<N> on a tcp controller)",
            .no_recv_zc => "this kernel's io_uring has no RECV_ZC (CONFIG_IO_URING_ZCRX)",
            .traddr_not_ip => "the controller's traddr is not an IP literal",
            .no_route => "no interface routes to the target address",
            .no_hds => "the NIC does not split headers from data (ethtool -G <if> tcp-data-split on)",
            .no_ifindex => "the routed interface has no index",
            .capacity_refused => "the NIC's ntuple steering capacity refused (ethtool -K <if> ntuple on)",
            .too_narrow => "the NIC has fewer than 4 RX queues: clamp(devices, queues/4, queues/2) is empty (ethtool -L <if> combined <N>)",
            .rxq_exhausted => "every lane-eligible RX queue is leased",
            .connect_failed => "the userspace association refused (see the log)",
            .present => "present",
        };
    }
};

/// The answers the ladder consumes, in gate order.
pub const Answers = struct {
    lane_enabled: bool,
    r5_red: bool = false,
    target: nic_census.Refusal!nic_census.Target,
    /// Gate 4: the test seam arms the sim backend here.
    area_sim: bool = false,
    recv_zc_supported: bool = false,
    traddr_is_ip: bool = false,
    /// The routed interface, OWNED (a probe's stack buffer must not outlive it).
    route_ifname_buf: [16]u8 = @splat(0),
    route_ifname_len: u8 = 0,
    hds_on: bool = false,
    ifindex: u32 = 0,
    channels: u32 = 0,
    ntuple_enabled: bool = false,
    rx_descs: u32 = 0,
    mtu: ?u32 = null,
    numa_node: ?u16 = null,
    devices_via_nic: u32 = 1,
    cpus: u32 = 1,
    /// Gate 12: the arbiter's grant (null = exhausted).
    lease: ?steering.Registry.Lease = null,
    /// Gate 14.
    connect_ok: bool = true,

    pub fn routeIfname(self: *const Answers) ?[]const u8 {
        return if (self.route_ifname_len == 0) null else self.route_ifname_buf[0..self.route_ifname_len];
    }

    pub fn setRoute(self: *Answers, name: []const u8) void {
        std.debug.assert(name.len > 0 and name.len <= self.route_ifname_buf.len);
        @memcpy(self.route_ifname_buf[0..name.len], name);
        self.route_ifname_len = @intCast(name.len);
    }
};

/// THE ladder. Pure.
pub fn arm(a: Answers) Verdict {
    if (!a.lane_enabled) return .off;
    if (a.r5_red) return .r5_red;
    const target = a.target catch |e| return .{ .target_ineligible = e };
    const geo = area.laneGeometry(a.cpus);
    if (a.area_sim) {
        return .{ .present = .{ .backend = .sim, .target = target, .io_queues = @min(target.io_queues, geo.io_queues), .depth = target.queue_depth } };
    }
    if (!a.recv_zc_supported) return .no_recv_zc;
    if (!a.traddr_is_ip) return .traddr_not_ip;
    const ifname = a.routeIfname() orelse return .no_route;
    if (!a.hds_on) return .no_hds;
    if (a.ifindex == 0) return .no_ifindex;
    if (!a.ntuple_enabled) return .capacity_refused;
    const pool = steering.laneEligibleQueues(a.channels, a.devices_via_nic) orelse return .too_narrow;
    const want = steering.fairQueueWant(pool.hi - pool.lo, a.devices_via_nic, geo.io_queues);
    if (want == 0) return .too_narrow;
    const lease = a.lease orelse return .rxq_exhausted;
    if (lease.n == 0) return .rxq_exhausted;
    if (!a.connect_ok) return .connect_failed;
    var plan: Plan = .{ .backend = .zcrx, .target = target, .io_queues = lease.n, .depth = target.queue_depth, .ifindex = a.ifindex, .numa_node = a.numa_node, .mtu = a.mtu, .rx_descs = a.rx_descs, .lease = lease };
    @memcpy(plan.ifname[0..ifname.len], ifname);
    plan.ifname_len = @intCast(ifname.len);
    @memcpy(plan.rx_queues[0..lease.n], lease.slice());
    return .{ .present = plan };
}

/// Publish the verdict ONCE per device: the gauge carries the code, the
/// log names the remedy. `present` clears the gauge.
pub fn announce(v: Verdict, device: []const u8) void {
    switch (v) {
        .present => stats.zcrx_lane_absent_reason.set(0),
        .off => stats.zcrx_lane_absent_reason.set(v.code()),
        else => {
            stats.zcrx_lane_absent_reason.set(v.code());
            stats.zcrx_probe_refusals.inc();
            log.warn("zcrx lane: {s}: Absent — {s} (verdict {s}, code {d}); the kernel path serves this device byte-identical", .{ device, v.describe(), @tagName(std.meta.activeTag(v)), v.code() });
        },
    }
}

/// `traddr,trsvcid,subnqn,nsid,lba_shift,max_xfer[,io_queues,queue_depth]`
/// (the `SQUEEZEFS_TEST_ZCRX_TARGET` seam); malformed ⇒ null (loud at the caller).
pub fn parseTargetOverride(text: []const u8) ?nic_census.Target {
    var t: nic_census.Target = .{};
    var it = std.mem.splitScalar(u8, text, ',');
    const traddr = it.next() orelse return null;
    if (traddr.len == 0 or traddr.len > t.traddr.len) return null;
    @memcpy(t.traddr[0..traddr.len], traddr);
    t.traddr_len = @intCast(traddr.len);
    t.trsvcid = std.fmt.parseInt(u16, it.next() orelse return null, 10) catch return null;
    const nqn = it.next() orelse return null;
    if (nqn.len == 0 or nqn.len > t.subnqn.len) return null;
    @memcpy(t.subnqn[0..nqn.len], nqn);
    t.subnqn_len = @intCast(nqn.len);
    t.nsid = std.fmt.parseInt(u32, it.next() orelse return null, 10) catch return null;
    t.lba_shift = std.fmt.parseInt(u6, it.next() orelse return null, 10) catch return null;
    t.max_xfer = std.fmt.parseInt(u32, it.next() orelse return null, 10) catch return null;
    if (t.lba_shift > 31) return null;
    if (t.max_xfer == 0 or t.max_xfer % (@as(u32, 1) << @as(u5, @intCast(t.lba_shift))) != 0) return null;
    if (it.next()) |q| t.io_queues = std.fmt.parseInt(u32, q, 10) catch return null;
    if (it.next()) |d| t.queue_depth = std.fmt.parseInt(u32, d, 10) catch return null;
    if (it.next() != null) return null;
    if (t.io_queues == 0 or t.io_queues > steering.MAX_FLOWS or t.queue_depth < area.DEPTH_FLOOR) return null;
    return t;
}

// ---------------------------------------------------------------------------
// The Linux half: the live answers.
// ---------------------------------------------------------------------------

pub const linux_probe = if (builtin.os.tag == .linux) struct {
    const linux = std.os.linux;
    const uring128 = @import("../../io/uring128.zig");
    const ethtool = @import("ethtool.zig");

    /// `io_uring_setup(2)` + `REGISTER_PROBE`: `is_supported(RECV_ZC)`.
    pub fn recvZcSupported() bool {
        var ring = uring128.Ring128.init(2, 0) catch return false;
        defer ring.deinit();
        const probe = ring.getProbe() catch return false;
        return probe.is_supported(.RECV_ZC);
    }

    /// Gates 5–11 against the live host for `target`; the arbiter lease is
    /// the registry's (gate 12) and the connect (gate 14) the caller's.
    pub fn collect(a: *Answers, reg: *steering.Registry, io: std.Io, target: nic_census.Target) void {
        a.recv_zc_supported = recvZcSupported();
        const ip = ethtool.Ip.parse(target.traddrText()) orelse return;
        a.traddr_is_ip = true;
        const ifname = ethtool.routeIfnameFor(ip, &a.route_ifname_buf) orelse return;
        a.route_ifname_len = @intCast(ifname.len);
        var nic = ethtool.EthtoolNic.open(ifname) catch return;
        defer nic.close();
        const ctl = nic.control();
        a.hds_on = ctl.vtable.tcpDataSplitOn(ctl.ctx) catch false;
        a.ifindex = ethtool.nicIfindex(ifname);
        a.channels = ctl.vtable.combinedChannels(ctl.ctx) catch 0;
        a.ntuple_enabled = ctl.vtable.ntupleEnabled(ctl.ctx) catch false;
        a.rx_descs = nic.rxRingDescriptors();
        a.mtu = ethtool.nicMtu(ifname);
        a.numa_node = ethtool.nicNumaNode(ifname);
        var dummy: u8 = 0;
        a.devices_via_nic = nic_census.tcpDevicesViaNic(io, "/sys", ifname, .{ .ctx = &dummy, .call = &routeCall });
        if (!a.hds_on or a.ifindex == 0 or !a.ntuple_enabled) return;
        const geo = area.laneGeometry(a.cpus);
        const pool = steering.laneEligibleQueues(a.channels, a.devices_via_nic) orelse return;
        const want = steering.fairQueueWant(pool.hi - pool.lo, a.devices_via_nic, geo.io_queues);
        const idx = reg.nic(a.ifindex) orelse return;
        a.lease = reg.acquire(idx, ifname, a.channels, a.devices_via_nic, want) catch null;
    }

    fn routeCall(_: *anyopaque, ip_text: []const u8, out: []u8) ?[]const u8 {
        const ip = ethtool.Ip.parse(ip_text) orelse return null;
        return ethtool.routeIfnameFor(ip, out);
    }
} else struct {};

/// The lane's knobs as the ladder reads them.
pub const Knobs = struct {
    lane_enabled: bool,
    area_sim: bool,
    sim_chunk: ?u64,
    target_override: ?nic_census.Target,
    target_override_malformed: bool,

    pub fn read(env: *const std.process.Environ.Map) Knobs {
        const raw = knobs.present(env.get("SQUEEZEFS_TEST_ZCRX_TARGET"));
        const parsed = if (raw) |r| parseTargetOverride(r) else null;
        return .{
            .lane_enabled = knobs.boolKnob("SQUEEZEFS_ZCRX_LANE", env),
            .area_sim = knobs.boolKnob("SQUEEZEFS_TEST_ZCRX_AREA_SIM", env),
            .sim_chunk = knobs.optIntKnob("SQUEEZEFS_TEST_ZCRX_SIM_CHUNK", u64, env),
            .target_override = parsed,
            .target_override_malformed = raw != null and parsed == null,
        };
    }
};

// ---------------------------------------------------------------------------
// Tests: the verdict table, and the venue's own verdict.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn okTarget() nic_census.Target {
    var t: nic_census.Target = .{};
    @memcpy(t.traddr[0..9], "127.0.0.1");
    t.traddr_len = 9;
    t.trsvcid = 4420;
    @memcpy(t.subnqn[0..5], "nqn.x");
    t.subnqn_len = 5;
    t.nsid = 1;
    t.max_xfer = 1 << 20;
    t.io_queues = 4;
    t.queue_depth = 64;
    return t;
}

fn fullAnswers() Answers {
    var lease: steering.Registry.Lease = .{ .nic = 0 };
    lease.queues[0] = 31;
    lease.n = 1;
    var a: Answers = .{
        .lane_enabled = true,
        .target = okTarget(),
        .recv_zc_supported = true,
        .traddr_is_ip = true,
        .hds_on = true,
        .ifindex = 6,
        .channels = 32,
        .ntuple_enabled = true,
        .rx_descs = 8192,
        .mtu = 9000,
        .devices_via_nic = 10,
        .cpus = 32,
        .lease = lease,
    };
    a.setRoute("eth7");
    return a;
}

test "the verdict table: every gate refuses in order with its typed verdict; the full answer set is present with the plan; terminal vs transient" {
    var a = fullAnswers();
    const p = arm(a);
    try testing.expectEqual(Verdict.Tag.present, std.meta.activeTag(p));
    try testing.expectEqual(Backend.zcrx, p.present.backend);
    try testing.expectEqual(@as(u32, 1), p.present.io_queues);
    try testing.expectEqualSlices(u32, &.{31}, p.present.rx_queues[0..1]);
    try testing.expectEqualStrings("eth7", p.present.ifname[0..p.present.ifname_len]);
    try testing.expect(!p.terminal());

    a = fullAnswers();
    a.lane_enabled = false;
    try testing.expectEqual(Verdict.off, arm(a));
    a = fullAnswers();
    a.r5_red = true;
    try testing.expectEqual(Verdict.r5_red, arm(a));
    try testing.expect(!arm(a).terminal());
    a = fullAnswers();
    a.target = error.NotTcp;
    try testing.expectEqual(Verdict{ .target_ineligible = error.NotTcp }, arm(a));
    try testing.expect(arm(a).terminal());
    a = fullAnswers();
    a.recv_zc_supported = false;
    try testing.expectEqual(Verdict.no_recv_zc, arm(a));
    a = fullAnswers();
    a.traddr_is_ip = false;
    try testing.expectEqual(Verdict.traddr_not_ip, arm(a));
    a = fullAnswers();
    a.route_ifname_len = 0;
    try testing.expectEqual(Verdict.no_route, arm(a));
    a = fullAnswers();
    a.hds_on = false;
    try testing.expectEqual(Verdict.no_hds, arm(a));
    a = fullAnswers();
    a.ifindex = 0;
    try testing.expectEqual(Verdict.no_ifindex, arm(a));
    a = fullAnswers();
    a.ntuple_enabled = false;
    try testing.expectEqual(Verdict.capacity_refused, arm(a));
    a = fullAnswers();
    a.channels = 1;
    try testing.expectEqual(Verdict.too_narrow, arm(a));
    a = fullAnswers();
    a.lease = null;
    try testing.expectEqual(Verdict.rxq_exhausted, arm(a));
    try testing.expect(!arm(a).terminal());
    a = fullAnswers();
    a.connect_ok = false;
    try testing.expectEqual(Verdict.connect_failed, arm(a));
    // Gate 4: the sim seam arms before any NIC gate.
    a = fullAnswers();
    a.area_sim = true;
    a.recv_zc_supported = false;
    a.hds_on = false;
    const s = arm(a);
    try testing.expectEqual(Backend.sim, s.present.backend);
    try testing.expectEqual(@as(u32, 4), s.present.io_queues);
    // Codes are distinct and nonzero for every Absent verdict.
    try testing.expect((Verdict{ .off = {} }).code() != (Verdict{ .no_hds = {} }).code());
    try testing.expect((Verdict{ .no_hds = {} }).code() != 0);
    try testing.expectEqual(@as(u8, 0), @backingInt(Verdict.Tag.off));
}

test "the target override parses the 6-field and 8-field shapes; every malformed shape is null" {
    const t = parseTargetOverride("127.0.0.1,4420,nqn.2026-10.io.squeezefs:x,1,9,1048576").?;
    try testing.expectEqualStrings("127.0.0.1", t.traddrText());
    try testing.expectEqual(@as(u16, 4420), t.trsvcid);
    try testing.expectEqual(@as(u32, 1), t.nsid);
    try testing.expectEqual(@as(u6, 9), t.lba_shift);
    try testing.expectEqual(@as(u32, 1 << 20), t.max_xfer);
    try testing.expectEqual(@as(u32, 1), t.io_queues);
    try testing.expectEqual(@as(u32, 4), t.queue_depth);
    const u = parseTargetOverride("::1,4420,nqn.x,2,12,65536,2,8").?;
    try testing.expectEqual(@as(u32, 2), u.io_queues);
    try testing.expectEqual(@as(u32, 8), u.queue_depth);
    try testing.expect(parseTargetOverride("") == null);
    try testing.expect(parseTargetOverride("127.0.0.1,4420,nqn.x,1,9") == null);
    try testing.expect(parseTargetOverride("127.0.0.1,4420,nqn.x,1,9,1000") == null);
    try testing.expect(parseTargetOverride("127.0.0.1,4420,nqn.x,1,9,4096,0") == null);
    try testing.expect(parseTargetOverride("127.0.0.1,4420,nqn.x,1,9,4096,1,1") == null);
    try testing.expect(parseTargetOverride("127.0.0.1,4420,nqn.x,1,9,4096,1,4,extra") == null);
    try testing.expect(parseTargetOverride("127.0.0.1,x,nqn.x,1,9,4096") == null);
}

test "the venue's own verdict for a loopback nvme-tcp target: RECV_ZC is present on this kernel, the route is lo, and the ladder refuses at no_hds (or too_narrow) — LOUD, the gauge set" {
    if (builtin.os.tag != .linux) return error.SkipZigTest;
    var reg = steering.Registry.init(testing.allocator);
    defer reg.deinit();
    var a: Answers = .{ .lane_enabled = true, .target = okTarget(), .cpus = 8 };
    linux_probe.collect(&a, &reg, testing.io, okTarget());
    try testing.expect(a.recv_zc_supported);
    try testing.expect(a.traddr_is_ip);
    try testing.expectEqualStrings("lo", a.routeIfname().?);
    const v = arm(a);
    try testing.expect(v == .no_hds or v == .too_narrow);
    try testing.expect(v.terminal());
    announce(v, "/dev/nvme0n1");
    try testing.expectEqual(@as(u64, v.code()), stats.zcrx_lane_absent_reason.load());
    try testing.expect(stats.zcrx_probe_refusals.load() >= 1);
}
