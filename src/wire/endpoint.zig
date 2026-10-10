//! The endpoint on the wire and in text (`docs/porting/sym_wire_membership_
//! recovery.md` §1.4 "THE endpoint law", §1.6 `EndpointWire`): a 20 B
//! binary `{family, port, addr}` in every frame that carries one
//! (`PublishEndpoint`, `ResolveEndpoint`'s reply, the membership `Join`
//! and census rows), and ONE text codec — `host:port` / `[v6]:port` — for
//! the bind knob, the logs and the CLI. The text form is a rendering; the
//! identity words are binary (decision 0002 row 225: never `ip:port`
//! strings parsed per use).
//!
//! Portable (`std.Io.net.IpAddress`'s parsers take no `Io`).

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const net = std.Io.net;

pub const FAMILY_NONE: u8 = 0;
pub const FAMILY_IP4: u8 = 4;
pub const FAMILY_IP6: u8 = 6;

/// 20 B: IPv4 in the low 4 bytes of `addr`, the rest zero.
pub const EndpointWire = extern struct {
    family: u8,
    reserved: u8,
    port: u16,
    addr: [16]u8,

    pub const none: EndpointWire = .{ .family = FAMILY_NONE, .reserved = 0, .port = 0, .addr = @splat(0) };

    pub fn check(self: EndpointWire) bool {
        return switch (self.family) {
            FAMILY_NONE => self.port == 0 and std.mem.allEqual(u8, &self.addr, 0),
            FAMILY_IP4 => std.mem.allEqual(u8, self.addr[4..], 0),
            FAMILY_IP6 => true,
            else => false,
        };
    }

    pub fn isNone(self: EndpointWire) bool {
        return self.family == FAMILY_NONE;
    }

    pub fn fromIp(a: net.IpAddress) EndpointWire {
        var e: EndpointWire = .none;
        switch (a) {
            .ip4 => |v4| {
                e.family = FAMILY_IP4;
                e.port = v4.port;
                @memcpy(e.addr[0..4], &v4.bytes);
            },
            .ip6 => |v6| {
                e.family = FAMILY_IP6;
                e.port = v6.port;
                e.addr = v6.bytes;
            },
        }
        return e;
    }

    pub fn toIp(self: EndpointWire) ?net.IpAddress {
        return switch (self.family) {
            FAMILY_IP4 => .{ .ip4 = .{ .bytes = self.addr[0..4].*, .port = self.port } },
            FAMILY_IP6 => .{ .ip6 = .{ .bytes = self.addr, .port = self.port, .interface = .none } },
            else => null,
        };
    }

    pub fn eql(a: EndpointWire, b: EndpointWire) bool {
        return a.family == b.family and a.port == b.port and std.mem.eql(u8, &a.addr, &b.addr);
    }

    /// `a.b.c.d:port`, `[v6]:port`, or `-` for none.
    pub fn format(self: EndpointWire, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self.family) {
            FAMILY_IP4 => try w.print("{d}.{d}.{d}.{d}:{d}", .{ self.addr[0], self.addr[1], self.addr[2], self.addr[3], self.port }),
            FAMILY_IP6 => {
                const v6: net.Ip6Address = .{ .bytes = self.addr, .port = self.port, .interface = .none };
                try v6.format(w);
            },
            else => try w.writeAll("-"),
        }
    }
};

comptime {
    std.debug.assert(@sizeOf(EndpointWire) == 20);
}

pub const ParseError = error{ MalformedEndpoint, BadPort };

/// `host:port` or `[v6]:port` (an IPv4/IPv6 LITERAL — the wire carries no
/// names; a name resolves at the dialer through `IpAddress.resolve`).
pub fn parseEndpoint(text: []const u8) ParseError!EndpointWire {
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (t.len == 0) return error.MalformedEndpoint;
    var host: []const u8 = undefined;
    var port_text: []const u8 = undefined;
    if (t[0] == '[') {
        const close = std.mem.indexOfScalar(u8, t, ']') orelse return error.MalformedEndpoint;
        host = t[1..close];
        if (close + 1 >= t.len or t[close + 1] != ':') return error.MalformedEndpoint;
        port_text = t[close + 2 ..];
    } else {
        const colon = std.mem.lastIndexOfScalar(u8, t, ':') orelse return error.MalformedEndpoint;
        host = t[0..colon];
        port_text = t[colon + 1 ..];
        if (std.mem.indexOfScalar(u8, host, ':') != null) return error.MalformedEndpoint; // a bare v6 needs brackets
    }
    const port = std.fmt.parseInt(u16, port_text, 10) catch return error.BadPort;
    if (net.Ip4Address.parse(host, port)) |v4| return EndpointWire.fromIp(.{ .ip4 = v4 }) else |_| {}
    if (net.Ip6Address.parse(host, port)) |v6| return EndpointWire.fromIp(.{ .ip6 = v6 }) else |_| {}
    return error.MalformedEndpoint;
}

/// The bind posture of the ONE listener (`SQUEEZEFS_MW_BIND`): `auto`
/// (bind an ephemeral port on every interface and advertise the default
/// route's address), `off` (refused by the join ladder's rung 3 — "a
/// writer that cannot be SEEN cannot be EVICTED"), or an explicit address
/// advertised VERBATIM.
pub const Bind = union(enum) {
    auto,
    off,
    explicit: EndpointWire,

    pub fn parse(text: ?[]const u8) ParseError!Bind {
        const t = std.mem.trim(u8, text orelse "", " \t\r\n");
        if (t.len == 0 or std.ascii.eqlIgnoreCase(t, "auto")) return .auto;
        if (std.ascii.eqlIgnoreCase(t, "off")) return .off;
        return .{ .explicit = try parseEndpoint(t) };
    }
};

const testing = std.testing;

test "the wire form round-trips both families through the text codec; none renders as -" {
    const e4 = try parseEndpoint("10.1.2.3:7000");
    try testing.expectEqual(FAMILY_IP4, e4.family);
    try testing.expectEqual(@as(u16, 7000), e4.port);
    try testing.expect(e4.check());
    var buf: [64]u8 = undefined;
    try testing.expectEqualStrings("10.1.2.3:7000", try std.fmt.bufPrint(&buf, "{f}", .{e4}));
    const e6 = try parseEndpoint("[::1]:9");
    try testing.expectEqual(FAMILY_IP6, e6.family);
    try testing.expectEqual(@as(u16, 9), e6.port);
    try testing.expectEqual(@as(u8, 1), e6.addr[15]);
    try testing.expect(e6.toIp().? == .ip6);
    try testing.expectEqualStrings("-", try std.fmt.bufPrint(&buf, "{f}", .{EndpointWire.none}));
    try testing.expect(EndpointWire.fromIp(e4.toIp().?).eql(e4));
}

test "malformed endpoints refuse by name: no port, a bad port, a bare v6, a hostname" {
    try testing.expectError(error.MalformedEndpoint, parseEndpoint("10.0.0.1"));
    try testing.expectError(error.BadPort, parseEndpoint("10.0.0.1:99999"));
    try testing.expectError(error.BadPort, parseEndpoint("10.0.0.1:x"));
    try testing.expectError(error.MalformedEndpoint, parseEndpoint("::1:9"));
    try testing.expectError(error.MalformedEndpoint, parseEndpoint("example.org:9"));
    try testing.expectError(error.MalformedEndpoint, parseEndpoint(""));
    // The wire check: a family outside {0, 4, 6}, a v4 with high bytes set, a none with a port.
    var bad: EndpointWire = .none;
    bad.family = 5;
    try testing.expect(!bad.check());
    bad = .none;
    bad.port = 1;
    try testing.expect(!bad.check());
    bad = (try parseEndpoint("1.2.3.4:5"));
    bad.addr[9] = 1;
    try testing.expect(!bad.check());
}

test "the bind knob: absent/auto, off, explicit verbatim; malformed refuses" {
    try testing.expectEqual(Bind.auto, try Bind.parse(null));
    try testing.expectEqual(Bind.auto, try Bind.parse(" AUTO "));
    try testing.expectEqual(Bind.off, try Bind.parse("off"));
    const b = try Bind.parse("127.0.0.1:4242");
    try testing.expectEqual(@as(u16, 4242), b.explicit.port);
    try testing.expectError(error.MalformedEndpoint, Bind.parse("nonsense"));
}

fn endpointTextFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [128]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    if (parseEndpoint(text)) |e| {
        // What parses prints, and the printed form parses back to the same wire value.
        var out: [96]u8 = undefined;
        var w: std.Io.Writer = .fixed(&out);
        try e.format(&w);
        const back = try parseEndpoint(w.buffered());
        try std.testing.expectEqual(e, back);
    } else |_| {}
    _ = Bind.parse(text) catch {};
    _ = Bind.parse(null) catch {};
}

test "fuzz: the endpoint and bind text parsers are total and a parsed endpoint round-trips through its printed form" {
    try fuzzkit.fuzz({}, endpointTextFuzz, .{ .corpus = &.{ fuzzkit.slice("127.0.0.1:4420"), fuzzkit.slice("[::1]:65535"), fuzzkit.slice("auto"), fuzzkit.slice("off"), fuzzkit.slice("1.2.3.4:99999") } });
}
