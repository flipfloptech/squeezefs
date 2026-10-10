//! The live `NicControl` over ONE `AF_INET/SOCK_DGRAM` socket and
//! `SIOCETHTOOL` (`docs/porting/ops_zcrx.md` §7.3), plus the sysfs NIC
//! facts (mtu, ifindex, NUMA node, rx ring descriptors) and the route
//! probe (`routeIfnameFor`: a connected UDP socket, port 9, no packet ⇒ the
//! local address ⇒ the interface that owns it). Linux only; compiled on
//! every Linux host, exercised against a real steerable NIC only in the
//! field — on the venue (`lo`, a 1-queue virtio NIC) every control answers
//! its refusal, which IS the probe ladder's verdict.
//!
//! Every uapi struct carries its size/offset pins (`ethtool_value` 8,
//! `ringparam` 36, `channels` 36, `rx_flow_spec` 168 with `ring_cookie`
//! @152 and `location` @160, `rxnfc` 192 with `fs` @16, `rule_cnt` @184,
//! `rule_locs` @188, `rxfh` head 24).

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const log = @import("../../core/log.zig");
const steering = @import("steering.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("zcrx/ethtool is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const ETHTOOL_GRINGPARAM: u32 = 0x10;
pub const ETHTOOL_GFLAGS: u32 = 0x25;
pub const ETHTOOL_GRXCLSRLCNT: u32 = 0x2e;
pub const ETHTOOL_GRXCLSRLALL: u32 = 0x30;
pub const ETHTOOL_SRXCLSRLDEL: u32 = 0x31;
pub const ETHTOOL_SRXCLSRLINS: u32 = 0x32;
pub const ETHTOOL_GCHANNELS: u32 = 0x3c;
pub const ETHTOOL_GRSSH: u32 = 0x46;
pub const ETHTOOL_SRSSH: u32 = 0x47;
pub const ETH_FLAG_NTUPLE: u32 = 1 << 27;
pub const RX_CLS_LOC_SPECIAL: u32 = 0x8000_0000;
pub const TCP_V4_FLOW: u32 = 1;
pub const TCP_V6_FLOW: u32 = 5;

pub const EthtoolValue = extern struct { cmd: u32, data: u32 };

pub const RingParam = extern struct {
    cmd: u32,
    rx_max_pending: u32 = 0,
    rx_mini_max_pending: u32 = 0,
    rx_jumbo_max_pending: u32 = 0,
    tx_max_pending: u32 = 0,
    rx_pending: u32 = 0,
    rx_mini_pending: u32 = 0,
    rx_jumbo_pending: u32 = 0,
    tx_pending: u32 = 0,
};

pub const Channels = extern struct {
    cmd: u32,
    max_rx: u32 = 0,
    max_tx: u32 = 0,
    max_other: u32 = 0,
    max_combined: u32 = 0,
    rx_count: u32 = 0,
    tx_count: u32 = 0,
    other_count: u32 = 0,
    combined_count: u32 = 0,
};

/// `struct ethtool_flow_ext` (20 B).
pub const FlowExt = extern struct {
    padding: [2]u8 = @splat(0),
    h_dest: [6]u8 = @splat(0),
    vlan_etype: u16 = 0,
    vlan_tci: u16 = 0,
    data: [2]u32 = @splat(0),
};

/// `struct ethtool_rx_flow_spec` (168 B); `h_u`/`m_u` as 52 raw bytes
/// (`tcp_ip4_spec`: ip4src @0, ip4dst @4, psrc @8, pdst @10; `tcp_ip6_spec`:
/// ip6src @0, ip6dst @16, psrc @32, pdst @34).
pub const RxFlowSpec = extern struct {
    flow_type: u32,
    h_u: [52]u8 = @splat(0),
    h_ext: FlowExt = .{},
    m_u: [52]u8 = @splat(0),
    m_ext: FlowExt = .{},
    ring_cookie: u64 align(8),
    location: u32,
    _pad: u32 = 0,
};

/// `struct ethtool_rxnfc` with room for `rule_locs[]` behind it.
pub const Rxnfc = extern struct {
    cmd: u32,
    flow_type: u32 = 0,
    data: u64 = 0,
    fs: RxFlowSpec,
    rule_cnt: u32 = 0,
    rule_locs: [1]u32 = @splat(0),
};

pub const RxfhHead = extern struct {
    cmd: u32,
    rss_context: u32 = 0,
    indir_size: u32 = 0,
    key_size: u32 = 0,
    hfunc: u8 = 0,
    input_xfrm: u8 = 0,
    rsvd8: [2]u8 = @splat(0),
    rsvd32: u32 = 0,
};

comptime {
    std.debug.assert(@sizeOf(EthtoolValue) == 8);
    std.debug.assert(@sizeOf(RingParam) == 36);
    std.debug.assert(@offsetOf(RingParam, "rx_pending") == 20);
    std.debug.assert(@sizeOf(Channels) == 36);
    std.debug.assert(@offsetOf(Channels, "combined_count") == 32);
    std.debug.assert(@sizeOf(FlowExt) == 20);
    std.debug.assert(@sizeOf(RxFlowSpec) == 168);
    std.debug.assert(@offsetOf(RxFlowSpec, "h_u") == 4);
    std.debug.assert(@offsetOf(RxFlowSpec, "ring_cookie") == 152);
    std.debug.assert(@offsetOf(RxFlowSpec, "location") == 160);
    std.debug.assert(@sizeOf(Rxnfc) == 192);
    std.debug.assert(@offsetOf(Rxnfc, "fs") == 16);
    std.debug.assert(@offsetOf(Rxnfc, "rule_cnt") == 184);
    std.debug.assert(@offsetOf(Rxnfc, "rule_locs") == 188);
    std.debug.assert(@sizeOf(RxfhHead) == 24);
}

/// Encode a lane flow as the kernel's flow spec with all-ones masks on the
/// matched fields and the queue as the ring cookie.
pub fn flowSpec(rule: steering.FlowRule, location: u32) RxFlowSpec {
    var fs: RxFlowSpec = .{ .flow_type = if (rule.v6) TCP_V6_FLOW else TCP_V4_FLOW, .ring_cookie = rule.queue, .location = location };
    if (rule.v6) {
        @memcpy(fs.h_u[0..16], &rule.src);
        @memcpy(fs.h_u[16..32], &rule.dst);
        std.mem.writeInt(u16, fs.h_u[32..34], rule.sport, .big);
        std.mem.writeInt(u16, fs.h_u[34..36], rule.dport, .big);
        @memset(fs.m_u[0..36], 0xFF);
    } else {
        @memcpy(fs.h_u[0..4], rule.src[0..4]);
        @memcpy(fs.h_u[4..8], rule.dst[0..4]);
        std.mem.writeInt(u16, fs.h_u[8..10], rule.sport, .big);
        std.mem.writeInt(u16, fs.h_u[10..12], rule.dport, .big);
        @memset(fs.m_u[0..12], 0xFF);
    }
    return fs;
}

pub const EthtoolNic = struct {
    fd: linux.fd_t,
    name: [linux.IFNAMESIZE]u8,
    name_len: u8,

    pub fn open(name: []const u8) error{ SocketFailed, NameTooLong }!EthtoolNic {
        if (name.len >= linux.IFNAMESIZE) return error.NameTooLong;
        const rc = linux.socket(linux.AF.INET, linux.SOCK.DGRAM | linux.SOCK.CLOEXEC, 0);
        if (linux.errno(rc) != .SUCCESS) return error.SocketFailed;
        var self: EthtoolNic = .{ .fd = @intCast(rc), .name = @splat(0), .name_len = @intCast(name.len) };
        @memcpy(self.name[0..name.len], name);
        return self;
    }

    pub fn close(self: *EthtoolNic) void {
        _ = linux.close(self.fd);
    }

    pub fn control(self: *EthtoolNic) steering.NicControl {
        return .{ .ctx = self, .vtable = &vtable };
    }

    /// ONE ioctl: `ifreq{name, data = cmd}`.
    fn call(self: *EthtoolNic, cmd: *anyopaque) steering.Error!void {
        var req: linux.ifreq = undefined;
        req.ifrn.name = self.name;
        req.ifru.data = @ptrCast(cmd);
        const rc = linux.ioctl(self.fd, linux.SIOCETHTOOL, @intFromPtr(&req));
        return switch (linux.errno(rc)) {
            .SUCCESS => {},
            .OPNOTSUPP => error.NotSupported,
            .NOSPC => error.NoSpace,
            else => error.NicRefused,
        };
    }

    /// `ETHTOOL_GRINGPARAM.rx_pending` (frames on mlx5); a refusal reads 0
    /// LOUD once — sizing flies blind, the park governor bounds the damage.
    pub fn rxRingDescriptors(self: *EthtoolNic) u32 {
        var rp: RingParam = .{ .cmd = ETHTOOL_GRINGPARAM };
        self.call(&rp) catch |e| {
            log.warn("zcrx: {s}: ETHTOOL_GRINGPARAM refused ({s}) — the ring's standing pool demand is unknown; sizing flies blind, the park governor bounds the damage", .{ self.name[0..self.name_len], @errorName(e) });
            return 0;
        };
        return rp.rx_pending;
    }

    const vtable: steering.NicControl.VTable = .{
        .ifname = &ifname,
        .combinedChannels = &combinedChannels,
        .tcpDataSplitOn = &tcpDataSplitOn,
        .rxfhIndir = &rxfhIndir,
        .setRxfhIndir = &setRxfhIndir,
        .ntupleEnabled = &ntupleEnabled,
        .ntupleTableSize = &ntupleTableSize,
        .specialLocSupported = &specialLocSupported,
        .ntupleTableSizeHint = &ntupleTableSizeHint,
        .ntupleLocs = &ntupleLocs,
        .insertNtuple = &insertNtuple,
        .deleteNtuple = &deleteNtuple,
    };

    fn me(ctx: *anyopaque) *EthtoolNic {
        return @ptrCast(@alignCast(ctx));
    }

    fn ifname(ctx: *anyopaque) []const u8 {
        const s = me(ctx);
        return s.name[0..s.name_len];
    }

    /// `max(combined_count, rx_count)`.
    fn combinedChannels(ctx: *anyopaque) steering.Error!u32 {
        var ch: Channels = .{ .cmd = ETHTOOL_GCHANNELS };
        try me(ctx).call(&ch);
        return @max(ch.combined_count, ch.rx_count);
    }

    fn tcpDataSplitOn(ctx: *anyopaque) steering.Error!bool {
        const ethtool_nl = @import("ethtool_nl.zig");
        return ethtool_nl.tcpDataSplitOn(ifname(ctx)) catch error.NicRefused;
    }

    fn rxfhIndir(ctx: *anyopaque, out: []u32) steering.Error![]u32 {
        const s = me(ctx);
        var sizes: RxfhHead = .{ .cmd = ETHTOOL_GRSSH };
        try s.call(&sizes);
        const n = sizes.indir_size;
        if (n == 0 or n > out.len) return error.NotSupported;
        var buf: [@sizeOf(RxfhHead) + 2048 * 4]u8 align(8) = undefined;
        const head: *RxfhHead = @ptrCast(&buf);
        head.* = .{ .cmd = ETHTOOL_GRSSH, .indir_size = n };
        try s.call(head);
        const indir: [*]const u32 = @ptrCast(@alignCast(buf[@sizeOf(RxfhHead)..].ptr));
        @memcpy(out[0..n], indir[0..n]);
        return out[0..n];
    }

    /// `hfunc 0, key_size 0` = keep both.
    fn setRxfhIndir(ctx: *anyopaque, table: []const u32) steering.Error!void {
        if (table.len > 2048) return error.NicRefused;
        var buf: [@sizeOf(RxfhHead) + 2048 * 4]u8 align(8) = undefined;
        const head: *RxfhHead = @ptrCast(&buf);
        head.* = .{ .cmd = ETHTOOL_SRSSH, .indir_size = @intCast(table.len) };
        const indir: [*]u32 = @ptrCast(@alignCast(buf[@sizeOf(RxfhHead)..].ptr));
        @memcpy(indir[0..table.len], table);
        try me(ctx).call(head);
    }

    fn ntupleEnabled(ctx: *anyopaque) steering.Error!bool {
        var v: EthtoolValue = .{ .cmd = ETHTOOL_GFLAGS, .data = 0 };
        try me(ctx).call(&v);
        return v.data & ETH_FLAG_NTUPLE != 0;
    }

    fn rxclsCount(s: *EthtoolNic) steering.Error!Rxnfc {
        var nfc: Rxnfc = .{ .cmd = ETHTOOL_GRXCLSRLCNT, .fs = std.mem.zeroes(RxFlowSpec) };
        try s.call(&nfc);
        return nfc;
    }

    fn ntupleTableSize(ctx: *anyopaque) steering.Error!u32 {
        const nfc = try rxclsCount(me(ctx));
        return @intCast(nfc.data & ~@as(u64, RX_CLS_LOC_SPECIAL));
    }

    fn specialLocSupported(ctx: *anyopaque) steering.Error!bool {
        const nfc = try rxclsCount(me(ctx));
        return nfc.data & RX_CLS_LOC_SPECIAL != 0;
    }

    /// `GRXCLSRLALL`: `data` = the table size the driver really has (mlx5
    /// reports 1024 here while `GRXCLSRLCNT` says 0); the ids follow.
    fn rxclsAll(s: *EthtoolNic, buf: []align(8) u8) steering.Error!*Rxnfc {
        const cnt = try rxclsCount(s);
        const rules = cnt.rule_cnt;
        if (@sizeOf(Rxnfc) + @as(usize, rules) * 4 > buf.len) return error.NicRefused;
        @memset(buf, 0);
        const nfc: *Rxnfc = @ptrCast(buf.ptr);
        nfc.cmd = ETHTOOL_GRXCLSRLALL;
        nfc.rule_cnt = rules;
        try s.call(nfc);
        return nfc;
    }

    fn ntupleTableSizeHint(ctx: *anyopaque) steering.Error!u32 {
        var buf: [@sizeOf(Rxnfc) + 4096 * 4]u8 align(8) = undefined;
        const nfc = try rxclsAll(me(ctx), &buf);
        return @intCast(nfc.data);
    }

    fn ntupleLocs(ctx: *anyopaque, out: []u32) steering.Error![]u32 {
        var buf: [@sizeOf(Rxnfc) + 4096 * 4]u8 align(8) = undefined;
        const nfc = try rxclsAll(me(ctx), &buf);
        const n = @min(nfc.rule_cnt, out.len);
        const locs: [*]const u32 = @ptrCast(@alignCast(buf[@offsetOf(Rxnfc, "rule_locs")..].ptr));
        @memcpy(out[0..n], locs[0..n]);
        return out[0..n];
    }

    fn insertNtuple(ctx: *anyopaque, loc: u32, rule: steering.FlowRule) steering.Error!u32 {
        var nfc: Rxnfc = .{ .cmd = ETHTOOL_SRXCLSRLINS, .fs = flowSpec(rule, loc) };
        try me(ctx).call(&nfc);
        // The kernel echoes the EFFECTIVE location.
        return nfc.fs.location;
    }

    fn deleteNtuple(ctx: *anyopaque, loc: u32) steering.Error!void {
        var nfc: Rxnfc = .{ .cmd = ETHTOOL_SRXCLSRLDEL, .fs = std.mem.zeroes(RxFlowSpec) };
        nfc.fs.location = loc;
        try me(ctx).call(&nfc);
    }
};

// ---------------------------------------------------------------------------
// sysfs NIC facts and the route probe.
// ---------------------------------------------------------------------------

fn readNetAttr(ifname: []const u8, attr: []const u8, buf: []u8) ?[]const u8 {
    var pb: [128]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/sys/class/net/{s}/{s}", .{ ifname, attr }, 0) catch return null;
    const rc = linux.open(path.ptr, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    const n = linux.read(fd, buf.ptr, buf.len);
    if (linux.errno(n) != .SUCCESS) return null;
    return std.mem.trim(u8, buf[0..n], " \r\n\x00");
}

pub fn nicMtu(ifname: []const u8) ?u32 {
    var b: [32]u8 = undefined;
    return std.fmt.parseInt(u32, readNetAttr(ifname, "mtu", &b) orelse return null, 10) catch null;
}

pub fn nicIfindex(ifname: []const u8) u32 {
    var b: [32]u8 = undefined;
    return std.fmt.parseInt(u32, readNetAttr(ifname, "ifindex", &b) orelse return 0, 10) catch 0;
}

/// `device/numa_node`: absent (a virtual NIC) or -1 ⇒ null.
pub fn nicNumaNode(ifname: []const u8) ?u16 {
    var b: [32]u8 = undefined;
    const v = std.fmt.parseInt(i32, readNetAttr(ifname, "device/numa_node", &b) orelse return null, 10) catch return null;
    if (v < 0) return null;
    return @intCast(v);
}

pub const Ip = union(enum) {
    v4: [4]u8,
    v6: [16]u8,

    /// An IP LITERAL (the probe's gate 6); a hostname is not one.
    pub fn parse(text: []const u8) ?Ip {
        if (std.mem.indexOfScalar(u8, text, ':') != null) {
            const a = std.Io.net.Ip6Address.parse(text, 0) catch return null;
            return .{ .v6 = a.bytes };
        }
        const a = std.Io.net.Ip4Address.parse(text, 0) catch return null;
        return .{ .v4 = a.bytes };
    }
};

/// The interface a packet to `ip` would leave through: connect a UDP
/// socket (port 9, nothing is sent), read the local address, find the
/// interface owning it (v4: `SIOCGIFADDR` per `/sys/class/net/*`; v6:
/// `/proc/net/if_inet6`).
pub fn routeIfnameFor(ip: Ip, out: []u8) ?[]const u8 {
    switch (ip) {
        .v4 => |v4| {
            const rc = linux.socket(linux.AF.INET, linux.SOCK.DGRAM | linux.SOCK.CLOEXEC, 0);
            if (linux.errno(rc) != .SUCCESS) return null;
            const fd: linux.fd_t = @intCast(rc);
            defer _ = linux.close(fd);
            var dst: linux.sockaddr.in = .{ .port = std.mem.nativeToBig(u16, 9), .addr = @bitCast(v4) };
            if (linux.errno(linux.connect(fd, @ptrCast(&dst), @sizeOf(linux.sockaddr.in))) != .SUCCESS) return null;
            var local: linux.sockaddr.in = undefined;
            var len: linux.socklen_t = @sizeOf(linux.sockaddr.in);
            if (linux.errno(linux.getsockname(fd, @ptrCast(&local), &len)) != .SUCCESS) return null;
            return ifnameOwningV4(fd, local.addr, out);
        },
        .v6 => |v6| {
            const rc = linux.socket(linux.AF.INET6, linux.SOCK.DGRAM | linux.SOCK.CLOEXEC, 0);
            if (linux.errno(rc) != .SUCCESS) return null;
            const fd: linux.fd_t = @intCast(rc);
            defer _ = linux.close(fd);
            var dst: linux.sockaddr.in6 = .{ .port = std.mem.nativeToBig(u16, 9), .flowinfo = 0, .addr = v6, .scope_id = 0 };
            if (linux.errno(linux.connect(fd, @ptrCast(&dst), @sizeOf(linux.sockaddr.in6))) != .SUCCESS) return null;
            var local: linux.sockaddr.in6 = undefined;
            var len: linux.socklen_t = @sizeOf(linux.sockaddr.in6);
            if (linux.errno(linux.getsockname(fd, @ptrCast(&local), &len)) != .SUCCESS) return null;
            return ifnameOwningV6(local.addr, out);
        },
    }
}

fn ifnameOwningV4(fd: linux.fd_t, addr: u32, out: []u8) ?[]const u8 {
    const rc = linux.open("/sys/class/net", .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    const dfd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(dfd);
    var buf: [4096]u8 align(8) = undefined;
    while (true) {
        const n = linux.getdents64(dfd, &buf, buf.len);
        if (linux.errno(n) != .SUCCESS or n == 0) return null;
        var off: usize = 0;
        while (off < n) {
            const ent: *align(1) const linux.dirent64 = @ptrCast(&buf[off]);
            const name = std.mem.sliceTo(@as([*:0]const u8, @ptrCast(&ent.name)), 0);
            off += ent.reclen;
            if (name.len == 0 or name[0] == '.' or name.len >= linux.IFNAMESIZE) continue;
            var req: linux.ifreq = undefined;
            req.ifrn.name = @splat(0);
            @memcpy(req.ifrn.name[0..name.len], name);
            if (linux.errno(linux.ioctl(fd, linux.SIOCGIFADDR, @intFromPtr(&req))) != .SUCCESS) continue;
            const sa: *const linux.sockaddr.in = @ptrCast(@alignCast(&req.ifru.addr));
            if (sa.addr == addr) {
                @memcpy(out[0..name.len], name);
                return out[0..name.len];
            }
        }
    }
}

/// `/proc/net/if_inet6`: `<32 hex> <ifindex> <plen> <scope> <flags> <ifname>`.
fn ifnameOwningV6(addr: [16]u8, out: []u8) ?[]const u8 {
    const rc = linux.open("/proc/net/if_inet6", .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var buf: [16384]u8 = undefined;
    const n = linux.read(fd, &buf, buf.len);
    if (linux.errno(n) != .SUCCESS) return null;
    const hex = std.fmt.bytesToHex(addr, .lower);
    var lines = std.mem.splitScalar(u8, buf[0..n], '\n');
    while (lines.next()) |line| {
        var it = std.mem.tokenizeScalar(u8, line, ' ');
        const a = it.next() orelse continue;
        if (!std.mem.eql(u8, a, &hex)) continue;
        _ = it.next();
        _ = it.next();
        _ = it.next();
        _ = it.next();
        const name = it.next() orelse continue;
        @memcpy(out[0..name.len], name);
        return out[0..name.len];
    }
    return null;
}

// ---------------------------------------------------------------------------
// Tests (venue: the loopback and the 1-queue virtio NIC answer their refusals).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the flow spec encodes a v4 lane flow with big-endian ports, all-ones masks and the queue as the ring cookie" {
    var src: [16]u8 = @splat(0);
    src[0..4].* = .{ 10, 0, 0, 1 };
    var dst: [16]u8 = @splat(0);
    dst[0..4].* = .{ 10, 0, 0, 9 };
    const fs = flowSpec(.{ .v6 = false, .src = src, .dst = dst, .sport = 40000, .dport = 4420, .queue = 31 }, 112);
    try testing.expectEqual(TCP_V4_FLOW, fs.flow_type);
    try testing.expectEqualSlices(u8, &.{ 10, 0, 0, 1, 10, 0, 0, 9, 0x9c, 0x40, 0x11, 0x44 }, fs.h_u[0..12]);
    for (fs.m_u[0..12]) |m| try testing.expectEqual(@as(u8, 0xFF), m);
    try testing.expectEqual(@as(u64, 31), fs.ring_cookie);
    try testing.expectEqual(@as(u32, 112), fs.location);
    const bytes = std.mem.asBytes(&fs);
    try testing.expectEqual(@as(u64, 31), std.mem.readInt(u64, bytes[152..160], .little));
    try testing.expectEqual(@as(u32, 112), std.mem.readInt(u32, bytes[160..164], .little));
}

test "on the venue: 127.0.0.1 routes through lo; lo has no channels control (the refusal); the facts read; the IP-literal gate" {
    var out: [16]u8 = undefined;
    const ifn = routeIfnameFor(Ip.parse("127.0.0.1").?, &out) orelse return testkit.skip(@src(), .no_nic, "no route for 127.0.0.1 readable on this host");
    try testing.expectEqualStrings("lo", ifn);
    try testing.expect(nicIfindex("lo") >= 1);
    try testing.expect(nicMtu("lo").? >= 1280);
    try testing.expect(nicNumaNode("lo") == null);
    var nic = try EthtoolNic.open("lo");
    defer nic.close();
    const ctl = nic.control();
    try testing.expectEqualStrings("lo", ctl.ifname());
    try testing.expectError(error.NotSupported, ctl.vtable.combinedChannels(ctl.ctx));
    try testing.expectEqual(@as(u32, 0), nic.rxRingDescriptors());
    try testing.expect(Ip.parse("target.example") == null);
    try testing.expect(Ip.parse("::1") != null);
    try testing.expect(Ip.parse("10.0.0.300") == null);
}

fn ipLiteralFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [64]u8 = undefined;
    const len = smith.slice(&buf);
    _ = Ip.parse(buf[0..len]);
}

test "fuzz: the IP-literal parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, ipLiteralFuzz, .{ .corpus = &.{ fuzzkit.slice("127.0.0.1"), fuzzkit.slice("::ffff:1.2.3.4"), fuzzkit.slice("1.2.3") } });
}
