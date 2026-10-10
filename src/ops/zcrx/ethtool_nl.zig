//! The HDS probe over generic netlink (`docs/porting/ops_zcrx.md` §7.4):
//! two round trips on `AF_NETLINK/SOCK_RAW/NETLINK_GENERIC` —
//! `CTRL_CMD_GETFAMILY("ethtool")` for the family id, then
//! `ETHTOOL_MSG_RINGS_GET` with the nested header naming the device; the
//! verdict compares the ATTRIBUTE `ETHTOOL_A_RINGS_TCP_DATA_SPLIT ==
//! ETHTOOL_TCP_DATA_SPLIT_ENABLED` (absent ⇒ false: the driver predates
//! HDS; `NLMSG_ERROR` ≠ 0 ⇒ error). The 2026-08-02 lesson: a RENDERED
//! string compared ("TCP data split: on" under ethtool 7.1) mislabelled a
//! whole cell driver-blocked. Linux only; the message layouts are pinned.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const log = @import("../../core/log.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("zcrx/ethtool_nl is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const GENL_ID_CTRL: u16 = 16;
pub const CTRL_CMD_GETFAMILY: u8 = 3;
pub const CTRL_ATTR_FAMILY_ID: u16 = 1;
pub const CTRL_ATTR_FAMILY_NAME: u16 = 2;
pub const ETHTOOL_GENL_NAME = "ethtool";
pub const ETHTOOL_MSG_RINGS_GET: u8 = 15;
pub const ETHTOOL_A_RINGS_HEADER: u16 = 1;
pub const ETHTOOL_A_HEADER_DEV_NAME: u16 = 2;
pub const ETHTOOL_A_RINGS_TCP_DATA_SPLIT: u16 = 11;
pub const ETHTOOL_TCP_DATA_SPLIT_ENABLED: u8 = 2;
pub const NLA_F_NESTED: u16 = 1 << 15;
pub const NLA_ALIGNTO: usize = 4;
pub const NLM_F_REQUEST: u16 = 1;
pub const NLM_F_ACK: u16 = 4;
pub const NLMSG_ERROR: u16 = 2;

pub const NlMsgHdr = extern struct {
    len: u32,
    type: u16,
    flags: u16,
    seq: u32,
    pid: u32,
};

pub const GenlMsgHdr = extern struct {
    cmd: u8,
    version: u8 = 1,
    reserved: u16 = 0,
};

pub const NlAttr = extern struct {
    len: u16,
    type: u16,
};

comptime {
    std.debug.assert(@sizeOf(NlMsgHdr) == 16);
    std.debug.assert(@sizeOf(GenlMsgHdr) == 4);
    std.debug.assert(@sizeOf(NlAttr) == 4);
}

pub const Error = error{ Socket, Send, Recv, Malformed, Refused, NoFamily };

fn nlaAlign(n: usize) usize {
    return (n + NLA_ALIGNTO - 1) & ~(NLA_ALIGNTO - 1);
}

/// A message builder over a fixed buffer: header, genl header, attributes
/// (nested ones close by patching their length).
pub const Builder = struct {
    buf: [512]u8 align(4) = @splat(0),
    len: usize = 0,

    pub fn start(self: *Builder, nl_type: u16, flags: u16, seq: u32, cmd: u8) void {
        self.len = 0;
        const h: *NlMsgHdr = @ptrCast(@alignCast(&self.buf[0]));
        h.* = .{ .len = 0, .type = nl_type, .flags = flags, .seq = seq, .pid = 0 };
        self.len = @sizeOf(NlMsgHdr);
        const g: *GenlMsgHdr = @ptrCast(@alignCast(&self.buf[self.len]));
        g.* = .{ .cmd = cmd };
        self.len += @sizeOf(GenlMsgHdr);
    }

    pub fn attr(self: *Builder, t: u16, payload: []const u8) void {
        const a: *NlAttr = @ptrCast(@alignCast(&self.buf[self.len]));
        a.* = .{ .len = @intCast(@sizeOf(NlAttr) + payload.len), .type = t };
        @memcpy(self.buf[self.len + @sizeOf(NlAttr) ..][0..payload.len], payload);
        self.len += nlaAlign(@sizeOf(NlAttr) + payload.len);
    }

    /// Open a nested attribute; returns the offset to close with.
    pub fn nestStart(self: *Builder, t: u16) usize {
        const at = self.len;
        const a: *NlAttr = @ptrCast(@alignCast(&self.buf[self.len]));
        a.* = .{ .len = 0, .type = t | NLA_F_NESTED };
        self.len += @sizeOf(NlAttr);
        return at;
    }

    pub fn nestEnd(self: *Builder, at: usize) void {
        const a: *NlAttr = @ptrCast(@alignCast(&self.buf[at]));
        a.len = @intCast(self.len - at);
    }

    pub fn finish(self: *Builder) []const u8 {
        const h: *NlMsgHdr = @ptrCast(@alignCast(&self.buf[0]));
        h.len = @intCast(self.len);
        return self.buf[0..self.len];
    }
};

/// Walk top-level attributes of a genl reply (`nlmsghdr ‖ genlmsghdr ‖
/// attrs`); returns the payload of `want` or null.
pub fn findAttr(msg: []const u8, want: u16) ?[]const u8 {
    if (msg.len < @sizeOf(NlMsgHdr) + @sizeOf(GenlMsgHdr)) return null;
    var off: usize = @sizeOf(NlMsgHdr) + @sizeOf(GenlMsgHdr);
    while (off + @sizeOf(NlAttr) <= msg.len) {
        const len = std.mem.readInt(u16, msg[off..][0..2], .little);
        const t = std.mem.readInt(u16, msg[off + 2 ..][0..2], .little) & ~NLA_F_NESTED;
        if (len < @sizeOf(NlAttr) or off + len > msg.len) return null;
        if (t == want) return msg[off + @sizeOf(NlAttr) .. off + len];
        off += nlaAlign(len);
    }
    return null;
}

/// The reply's class: an error code (0 = an ACK), or a data message.
pub const Reply = union(enum) { err: i32, data: []const u8 };

pub fn classify(msg: []const u8) Error!Reply {
    if (msg.len < @sizeOf(NlMsgHdr)) return error.Malformed;
    const len = std.mem.readInt(u32, msg[0..4], .little);
    const t = std.mem.readInt(u16, msg[4..6], .little);
    if (len < @sizeOf(NlMsgHdr) or len > msg.len) return error.Malformed;
    if (t == NLMSG_ERROR) {
        if (len < @sizeOf(NlMsgHdr) + 4) return error.Malformed;
        return .{ .err = std.mem.readInt(i32, msg[16..20], .little) };
    }
    return .{ .data = msg[0..len] };
}

const Sock = struct {
    fd: linux.fd_t,
    seq: u32 = 1,

    fn open() Error!Sock {
        const rc = linux.socket(linux.AF.NETLINK, linux.SOCK.RAW | linux.SOCK.CLOEXEC, linux.NETLINK.GENERIC);
        if (linux.errno(rc) != .SUCCESS) return error.Socket;
        return .{ .fd = @intCast(rc) };
    }

    fn close(self: *Sock) void {
        _ = linux.close(self.fd);
    }

    fn roundTrip(self: *Sock, msg: []const u8, reply: []u8) Error![]const u8 {
        var dst: linux.sockaddr.nl = .{ .pid = 0, .groups = 0 };
        const sn = linux.sendto(self.fd, msg.ptr, msg.len, 0, @ptrCast(&dst), @sizeOf(linux.sockaddr.nl));
        if (linux.errno(sn) != .SUCCESS) return error.Send;
        var pfd = [_]linux.pollfd{.{ .fd = self.fd, .events = linux.POLL.IN, .revents = 0 }};
        const pr = linux.poll(&pfd, 1, 2000);
        if (linux.errno(pr) != .SUCCESS or pr == 0) return error.Recv;
        const rn = linux.recvfrom(self.fd, reply.ptr, reply.len, 0, null, null);
        if (linux.errno(rn) != .SUCCESS) return error.Recv;
        return reply[0..rn];
    }
};

/// `CTRL_CMD_GETFAMILY("ethtool")` → the family id.
pub fn ethtoolFamilyId(sock: *Sock) Error!u16 {
    var b: Builder = .{};
    b.start(GENL_ID_CTRL, NLM_F_REQUEST, sock.seq, CTRL_CMD_GETFAMILY);
    sock.seq += 1;
    b.attr(CTRL_ATTR_FAMILY_NAME, ETHTOOL_GENL_NAME ++ "\x00");
    var reply: [4096]u8 align(4) = undefined;
    const r = try sock.roundTrip(b.finish(), &reply);
    switch (try classify(r)) {
        .err => return error.NoFamily,
        .data => |d| {
            const id = findAttr(d, CTRL_ATTR_FAMILY_ID) orelse return error.NoFamily;
            if (id.len < 2) return error.Malformed;
            return std.mem.readInt(u16, id[0..2], .little);
        },
    }
}

/// The verdict for `ifname`: the attribute equals ENABLED.
pub fn tcpDataSplitOn(ifname: []const u8) Error!bool {
    var sock = try Sock.open();
    defer sock.close();
    const family = try ethtoolFamilyId(&sock);
    var b: Builder = .{};
    b.start(family, NLM_F_REQUEST | NLM_F_ACK, sock.seq, ETHTOOL_MSG_RINGS_GET);
    sock.seq += 1;
    const nest = b.nestStart(ETHTOOL_A_RINGS_HEADER);
    var name_z: [linux.IFNAMESIZE]u8 = @splat(0);
    if (ifname.len >= name_z.len) return error.Malformed;
    @memcpy(name_z[0..ifname.len], ifname);
    b.attr(ETHTOOL_A_HEADER_DEV_NAME, name_z[0 .. ifname.len + 1]);
    b.nestEnd(nest);
    var reply: [8192]u8 align(4) = undefined;
    const r = try sock.roundTrip(b.finish(), &reply);
    return switch (try classify(r)) {
        .err => |code| if (code == 0) false else error.Refused,
        .data => |d| blk: {
            const v = findAttr(d, ETHTOOL_A_RINGS_TCP_DATA_SPLIT) orelse break :blk false;
            break :blk v.len >= 1 and v[0] == ETHTOOL_TCP_DATA_SPLIT_ENABLED;
        },
    };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "message layout pins: GETFAMILY is 16 + 4 + nla(4 + 8) = 32 B; RINGS_GET nests the header with the device name; the attribute walk and the error class" {
    var b: Builder = .{};
    b.start(GENL_ID_CTRL, NLM_F_REQUEST, 7, CTRL_CMD_GETFAMILY);
    b.attr(CTRL_ATTR_FAMILY_NAME, "ethtool\x00");
    const m = b.finish();
    try testing.expectEqual(@as(usize, 32), m.len);
    try testing.expectEqual(@as(u32, 32), std.mem.readInt(u32, m[0..4], .little));
    try testing.expectEqual(GENL_ID_CTRL, std.mem.readInt(u16, m[4..6], .little));
    try testing.expectEqual(NLM_F_REQUEST, std.mem.readInt(u16, m[6..8], .little));
    try testing.expectEqual(@as(u32, 7), std.mem.readInt(u32, m[8..12], .little));
    try testing.expectEqual(CTRL_CMD_GETFAMILY, m[16]);
    try testing.expectEqual(@as(u8, 1), m[17]);
    try testing.expectEqual(@as(u16, 12), std.mem.readInt(u16, m[20..22], .little));
    try testing.expectEqual(CTRL_ATTR_FAMILY_NAME, std.mem.readInt(u16, m[22..24], .little));
    try testing.expectEqualStrings("ethtool\x00", m[24..32]);
    try testing.expectEqualStrings("ethtool\x00", findAttr(m, CTRL_ATTR_FAMILY_NAME).?);
    try testing.expect(findAttr(m, 9) == null);

    var r: Builder = .{};
    r.start(0x14, NLM_F_REQUEST | NLM_F_ACK, 8, ETHTOOL_MSG_RINGS_GET);
    const nest = r.nestStart(ETHTOOL_A_RINGS_HEADER);
    r.attr(ETHTOOL_A_HEADER_DEV_NAME, "eth0\x00");
    r.nestEnd(nest);
    const rm = r.finish();
    // 16 + 4 + nested(4 + attr(4 + 5 → 12)) = 36.
    try testing.expectEqual(@as(usize, 36), rm.len);
    try testing.expectEqual(ETHTOOL_A_RINGS_HEADER | NLA_F_NESTED, std.mem.readInt(u16, rm[22..24], .little));
    try testing.expectEqual(@as(u16, 16), std.mem.readInt(u16, rm[20..22], .little));
    try testing.expectEqual(ETHTOOL_A_HEADER_DEV_NAME, std.mem.readInt(u16, rm[26..28], .little));
    try testing.expectEqualStrings("eth0\x00", rm[28..33]);

    // A synthetic RINGS_GET reply carrying TCP_DATA_SPLIT = ENABLED.
    var rep: Builder = .{};
    rep.start(0x14, 0, 8, ETHTOOL_MSG_RINGS_GET);
    rep.attr(ETHTOOL_A_RINGS_TCP_DATA_SPLIT, &.{ETHTOOL_TCP_DATA_SPLIT_ENABLED});
    const d = rep.finish();
    try testing.expectEqual(ETHTOOL_TCP_DATA_SPLIT_ENABLED, findAttr(d, ETHTOOL_A_RINGS_TCP_DATA_SPLIT).?[0]);
    try testing.expectEqual(Reply{ .data = d }, try classify(d));
    // An NLMSG_ERROR with -EOPNOTSUPP.
    var e: [20]u8 align(4) = @splat(0);
    std.mem.writeInt(u32, e[0..4], 20, .little);
    std.mem.writeInt(u16, e[4..6], NLMSG_ERROR, .little);
    std.mem.writeInt(i32, e[16..20], -95, .little);
    try testing.expectEqual(Reply{ .err = -95 }, try classify(&e));
    try testing.expectError(error.Malformed, classify(e[0..8]));
}

test "on the venue: the ethtool genetlink family resolves (or the kernel lacks CONFIG_ETHTOOL_NETLINK — a ledgered capability skip; the probe's verdict is no_hds either way); lo and eth0 carry no tcp-data-split attribute" {
    var sock = Sock.open() catch return testkit.skip(@src(), .no_kernel_feature, "no generic netlink socket on this host");
    defer sock.close();
    const fam = ethtoolFamilyId(&sock) catch |e| {
        // 7.2.6-sqz ships `# CONFIG_ETHTOOL_NETLINK is not set`: GETFAMILY
        // answers NLMSG_ERROR and the HDS verdict is OFF on every NIC.
        try testing.expectEqual(error.NoFamily, e);
        try testing.expectError(error.NoFamily, tcpDataSplitOn("lo"));
        return testkit.skip(@src(), .no_kernel_feature, "this kernel has no ethtool genetlink family (CONFIG_ETHTOOL_NETLINK): the HDS verdict is OFF, the family round trip cannot run");
    };
    try testing.expect(fam != 0);
    // A driver without `get_ringparam` answers NLMSG_ERROR (-EOPNOTSUPP):
    // the same OFF verdict as an absent attribute.
    const lo = tcpDataSplitOn("lo") catch |e| switch (e) {
        error.Refused => false,
        else => return e,
    };
    try testing.expect(!lo);
    const eth0 = tcpDataSplitOn("eth0") catch |e| switch (e) {
        error.Refused => false,
        else => return e,
    };
    try testing.expect(!eth0);
}
