//! Target discovery and the device census over sysfs (charter §1.4
//! "probe, never kernel version"; `docs/porting/ops_zcrx.md` §1.3). Zero
//! new configuration: the lane rides the kernel initiator's OWN attachment
//! (`/sys/class/nvme/nvme<C>/{transport, address, subsysnqn}`, `/sys/block/
//! nvme<C>n<N>/{nsid, queue/logical_block_size, queue/max_hw_sectors_kb}`).
//! Portable over an injectable sysfs root (fixture trees in tests) and an
//! injectable route function.
//!
//! Eligible device path shape: `/dev/nvme<C>n<N>` ONLY — digits both sides
//! of `n`; multipath `nvme<C>c<P>n<N>` and partitions are ineligible. The
//! census counts BOTH child shapes (`nvme<C>n<N>`, `nvme<C>c<P>n<N>`):
//! counting only the plain shape read 0 on the fleet and collapsed the fair
//! want (finding D); over-counting multipath c-paths is the safe direction
//! (it widens the spread).

const std = @import("std");
const Io = std.Io;
const area = @import("area.zig");

pub const Target = struct {
    traddr: [64]u8 = @splat(0),
    traddr_len: u8 = 0,
    trsvcid: u16 = 0,
    subnqn: [256]u8 = @splat(0),
    subnqn_len: u16 = 0,
    nsid: u32 = 0,
    lba_shift: u6 = 9,
    /// Per-command cap (`area.maxXferBytes`).
    max_xfer: u32 = 0,
    io_queues: u32 = 1,
    queue_depth: u32 = 4,

    pub fn traddrText(self: *const Target) []const u8 {
        return self.traddr[0..self.traddr_len];
    }

    pub fn subnqnText(self: *const Target) []const u8 {
        return self.subnqn[0..self.subnqn_len];
    }
};

pub const Refusal = error{
    /// Not `/dev/nvme<C>n<N>`.
    PathShape,
    /// The controller's transport is not tcp (or unreadable).
    NotTcp,
    /// `address` lacks `traddr=`/`trsvcid=`.
    AddressMalformed,
    /// A block attribute is missing or malformed.
    BlockAttrs,
    /// The logical block size is not a power of two.
    LbaNotPow2,
    SubnqnTooLong,
};

pub const Parsed = struct { ctrl: u32, ns: u32 };

/// `nvme<C>n<N>` with digits on both sides, nothing else.
pub fn parsePlainNamespace(name: []const u8) ?Parsed {
    if (!std.mem.startsWith(u8, name, "nvme")) return null;
    const rest = name[4..];
    const n = std.mem.indexOfScalar(u8, rest, 'n') orelse return null;
    if (n == 0 or n + 1 >= rest.len) return null;
    const ctrl = std.fmt.parseInt(u32, rest[0..n], 10) catch return null;
    const ns = std.fmt.parseInt(u32, rest[n + 1 ..], 10) catch return null;
    return .{ .ctrl = ctrl, .ns = ns };
}

/// Both census shapes: `nvme<C>n<N>` and `nvme<C>c<P>n<N>` under controller C.
pub fn isNamespaceOf(name: []const u8, ctrl: u32) bool {
    if (!std.mem.startsWith(u8, name, "nvme")) return false;
    var rest = name[4..];
    var i: usize = 0;
    while (i < rest.len and std.ascii.isDigit(rest[i])) : (i += 1) {}
    if (i == 0) return false;
    const c = std.fmt.parseInt(u32, rest[0..i], 10) catch return false;
    if (c != ctrl) return false;
    rest = rest[i..];
    if (rest.len > 0 and rest[0] == 'c') {
        var j: usize = 1;
        while (j < rest.len and std.ascii.isDigit(rest[j])) : (j += 1) {}
        if (j == 1) return false;
        rest = rest[j..];
    }
    if (rest.len < 2 or rest[0] != 'n') return false;
    for (rest[1..]) |ch| if (!std.ascii.isDigit(ch)) return false;
    return true;
}

pub const Address = struct { traddr: []const u8, trsvcid: u16 };

/// `traddr=…,trsvcid=…[,src_addr=…]` (the kernel's `address` attribute).
pub fn parseAddress(text: []const u8) ?Address {
    var traddr: ?[]const u8 = null;
    var svc: ?u16 = null;
    var it = std.mem.splitScalar(u8, std.mem.trim(u8, text, " \r\n\x00"), ',');
    while (it.next()) |kv| {
        if (std.mem.startsWith(u8, kv, "traddr=")) traddr = kv["traddr=".len..];
        if (std.mem.startsWith(u8, kv, "trsvcid=")) svc = std.fmt.parseInt(u16, kv["trsvcid=".len..], 10) catch return null;
    }
    return .{ .traddr = traddr orelse return null, .trsvcid = svc orelse return null };
}

fn readAttr(io: Io, root: []const u8, buf: []u8, comptime fmt: []const u8, args: anytype) ?[]const u8 {
    var pb: [Io.Dir.max_path_bytes]u8 = undefined;
    const rel = std.fmt.bufPrint(&pb, fmt, args) catch return null;
    var full: [Io.Dir.max_path_bytes]u8 = undefined;
    const path = std.fmt.bufPrint(&full, "{s}/{s}", .{ root, rel }) catch return null;
    const text = Io.Dir.cwd().readFile(io, path, buf) catch return null;
    return std.mem.trim(u8, text, " \r\n\x00");
}

/// The target behind `/dev/nvme<C>n<N>` when it is an nvme-tcp namespace.
pub fn nvmeTcpTargetFor(io: Io, root: []const u8, dev_path: []const u8, cpus: u32) Refusal!Target {
    const name = std.fs.path.basename(dev_path);
    if (!std.mem.startsWith(u8, dev_path, "/dev/")) return error.PathShape;
    const p = parsePlainNamespace(name) orelse return error.PathShape;
    var b1: [512]u8 = undefined;
    const transport = readAttr(io, root, &b1, "class/nvme/nvme{d}/transport", .{p.ctrl}) orelse return error.NotTcp;
    if (!std.mem.eql(u8, transport, "tcp")) return error.NotTcp;
    var b2: [512]u8 = undefined;
    const addr = parseAddress(readAttr(io, root, &b2, "class/nvme/nvme{d}/address", .{p.ctrl}) orelse return error.AddressMalformed) orelse return error.AddressMalformed;
    var t: Target = .{};
    if (addr.traddr.len > t.traddr.len) return error.AddressMalformed;
    @memcpy(t.traddr[0..addr.traddr.len], addr.traddr);
    t.traddr_len = @intCast(addr.traddr.len);
    t.trsvcid = addr.trsvcid;
    var b3: [512]u8 = undefined;
    const nqn = readAttr(io, root, &b3, "class/nvme/nvme{d}/subsysnqn", .{p.ctrl}) orelse return error.BlockAttrs;
    if (nqn.len > t.subnqn.len) return error.SubnqnTooLong;
    @memcpy(t.subnqn[0..nqn.len], nqn);
    t.subnqn_len = @intCast(nqn.len);
    var b4: [64]u8 = undefined;
    t.nsid = std.fmt.parseInt(u32, readAttr(io, root, &b4, "block/{s}/nsid", .{name}) orelse return error.BlockAttrs, 10) catch return error.BlockAttrs;
    var b5: [64]u8 = undefined;
    const lbs = std.fmt.parseInt(u32, readAttr(io, root, &b5, "block/{s}/queue/logical_block_size", .{name}) orelse return error.BlockAttrs, 10) catch return error.BlockAttrs;
    if (lbs == 0 or !std.math.isPowerOfTwo(lbs)) return error.LbaNotPow2;
    t.lba_shift = @intCast(std.math.log2_int(u32, lbs));
    var b6: [64]u8 = undefined;
    const max_kb = std.fmt.parseInt(u64, readAttr(io, root, &b6, "block/{s}/queue/max_hw_sectors_kb", .{name}) orelse return error.BlockAttrs, 10) catch return error.BlockAttrs;
    t.max_xfer = @intCast(area.maxXferBytes(max_kb, t.lba_shift));
    const geo = area.laneGeometry(cpus);
    t.io_queues = geo.io_queues;
    t.queue_depth = geo.depth;
    return t;
}

/// The route function: the interface a destination IP leaves through.
pub const RouteFn = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, ip: []const u8, out: []u8) ?[]const u8,
};

/// nvme-tcp NAMESPACES whose controller routes through `ifname` (both
/// child shapes), floor 1 (the asking device itself).
pub fn tcpDevicesViaNic(io: Io, root: []const u8, ifname: []const u8, route: RouteFn) u32 {
    var pb: [Io.Dir.max_path_bytes]u8 = undefined;
    const class = std.fmt.bufPrint(&pb, "{s}/class/nvme", .{root}) catch return 1;
    var dir = Io.Dir.cwd().openDir(io, class, .{ .iterate = true }) catch return 1;
    defer dir.close(io);
    var count: u32 = 0;
    var it = dir.iterate();
    while (it.next(io) catch null) |ent| {
        if (!std.mem.startsWith(u8, ent.name, "nvme")) continue;
        const ctrl = std.fmt.parseInt(u32, ent.name[4..], 10) catch continue;
        var b1: [64]u8 = undefined;
        const transport = readAttr(io, root, &b1, "class/nvme/{s}/transport", .{ent.name}) orelse continue;
        if (!std.mem.eql(u8, transport, "tcp")) continue;
        var b2: [512]u8 = undefined;
        const addr = parseAddress(readAttr(io, root, &b2, "class/nvme/{s}/address", .{ent.name}) orelse continue) orelse continue;
        var ib: [64]u8 = undefined;
        const via = route.call(route.ctx, addr.traddr, &ib) orelse continue;
        if (!std.mem.eql(u8, via, ifname)) continue;
        // Children of the controller dir: the namespaces (both shapes).
        var cb: [Io.Dir.max_path_bytes]u8 = undefined;
        const cpath = std.fmt.bufPrint(&cb, "{s}/class/nvme/{s}", .{ root, ent.name }) catch continue;
        var cdir = Io.Dir.cwd().openDir(io, cpath, .{ .iterate = true }) catch continue;
        defer cdir.close(io);
        var cit = cdir.iterate();
        while (cit.next(io) catch null) |child| if (isNamespaceOf(child.name, ctrl)) {
            count += 1;
        };
    }
    return @max(count, 1);
}

// ---------------------------------------------------------------------------
// Tests over a fixture sysfs tree.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the strict namespace shape: nvme0n1 / nvme12n34 parse; a c-path, a partition, a controller and a foreign name do not; both census shapes match their controller" {
    try testing.expectEqual(Parsed{ .ctrl = 0, .ns = 1 }, parsePlainNamespace("nvme0n1").?);
    try testing.expectEqual(Parsed{ .ctrl = 12, .ns = 34 }, parsePlainNamespace("nvme12n34").?);
    try testing.expect(parsePlainNamespace("nvme0c0n1") == null);
    try testing.expect(parsePlainNamespace("nvme0n1p1") == null);
    try testing.expect(parsePlainNamespace("nvme0") == null);
    try testing.expect(parsePlainNamespace("sda") == null);
    try testing.expect(isNamespaceOf("nvme3n1", 3));
    try testing.expect(isNamespaceOf("nvme3c1n1", 3));
    try testing.expect(!isNamespaceOf("nvme3n1", 4));
    try testing.expect(!isNamespaceOf("nvme3c1n1p1", 3));
    try testing.expect(!isNamespaceOf("nvme3", 3));
    try testing.expect(!isNamespaceOf("nvme3cn1", 3));
    const a = parseAddress("traddr=127.0.0.1,trsvcid=4420,src_addr=127.0.0.1\n").?;
    try testing.expectEqualStrings("127.0.0.1", a.traddr);
    try testing.expectEqual(@as(u16, 4420), a.trsvcid);
    try testing.expect(parseAddress("traddr=1.2.3.4") == null);
    try testing.expect(parseAddress("trsvcid=x,traddr=1") == null);
}

fn writeAttr(io: Io, dir: Io.Dir, path: []const u8, text: []const u8) !void {
    if (std.fs.path.dirname(path)) |d| try dir.createDirPath(io, d);
    try dir.writeFile(io, .{ .sub_path = path, .data = text });
}

test "sysfs discovery: a tcp namespace yields its target (traddr, trsvcid, subnqn, nsid, lba shift, the saturated xfer cap on the MDTS sentinel, the geometry); pcie and bad shapes refuse; the census counts both shapes behind the route" {
    const io = testing.io;
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    const d = tmp.dir;
    try writeAttr(io, d, "class/nvme/nvme0/transport", "tcp\n");
    try writeAttr(io, d, "class/nvme/nvme0/address", "traddr=10.0.0.9,trsvcid=4420\n");
    try writeAttr(io, d, "class/nvme/nvme0/subsysnqn", "nqn.2026-10.io.squeezefs:share-a\n");
    try writeAttr(io, d, "class/nvme/nvme0/nvme0n1/dummy", "");
    try writeAttr(io, d, "class/nvme/nvme0/nvme0c3n2/dummy", "");
    try writeAttr(io, d, "block/nvme0n1/nsid", "1\n");
    try writeAttr(io, d, "block/nvme0n1/queue/logical_block_size", "512\n");
    try writeAttr(io, d, "block/nvme0n1/queue/max_hw_sectors_kb", "2147483644\n");
    try writeAttr(io, d, "class/nvme/nvme1/transport", "pcie\n");
    try writeAttr(io, d, "class/nvme/nvme1/nvme1n1/dummy", "");
    try writeAttr(io, d, "class/nvme/nvme2/transport", "tcp\n");
    try writeAttr(io, d, "class/nvme/nvme2/address", "traddr=192.0.2.1,trsvcid=4420\n");
    try writeAttr(io, d, "class/nvme/nvme2/nvme2n1/dummy", "");
    var root_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const root = root_buf[0..try d.realPath(io, &root_buf)];
    const t = try nvmeTcpTargetFor(io, root, "/dev/nvme0n1", 32);
    try testing.expectEqualStrings("10.0.0.9", t.traddrText());
    try testing.expectEqual(@as(u16, 4420), t.trsvcid);
    try testing.expectEqualStrings("nqn.2026-10.io.squeezefs:share-a", t.subnqnText());
    try testing.expectEqual(@as(u32, 1), t.nsid);
    try testing.expectEqual(@as(u6, 9), t.lba_shift);
    try testing.expectEqual(@as(u32, 1 << 20), t.max_xfer);
    try testing.expectEqual(@as(u32, 4), t.io_queues);
    try testing.expectEqual(@as(u32, 64), t.queue_depth);
    try testing.expectError(error.NotTcp, nvmeTcpTargetFor(io, root, "/dev/nvme1n1", 32));
    try testing.expectError(error.PathShape, nvmeTcpTargetFor(io, root, "/dev/nvme0c0n1", 32));
    try testing.expectError(error.PathShape, nvmeTcpTargetFor(io, root, "/dev/nvme0n1p1", 32));
    try testing.expectError(error.PathShape, nvmeTcpTargetFor(io, root, "/tmp/nvme0n1", 32));
    try writeAttr(io, d, "block/nvme0n1/queue/logical_block_size", "500\n");
    try testing.expectError(error.LbaNotPow2, nvmeTcpTargetFor(io, root, "/dev/nvme0n1", 32));
    // The census: 10.0.0.9 routes via eth7 (two shapes under nvme0 = 2);
    // 192.0.2.1 via eth1 does not count; floor 1 on an empty route.
    const Route = struct {
        fn call(_: *anyopaque, ip: []const u8, out: []u8) ?[]const u8 {
            if (std.mem.eql(u8, ip, "10.0.0.9")) {
                @memcpy(out[0..4], "eth7");
                return out[0..4];
            }
            if (std.mem.eql(u8, ip, "192.0.2.1")) {
                @memcpy(out[0..4], "eth1");
                return out[0..4];
            }
            return null;
        }
    };
    var dummy: u8 = 0;
    const route: RouteFn = .{ .ctx = &dummy, .call = &Route.call };
    try testing.expectEqual(@as(u32, 2), tcpDevicesViaNic(io, root, "eth7", route));
    try testing.expectEqual(@as(u32, 1), tcpDevicesViaNic(io, root, "eth1", route));
    try testing.expectEqual(@as(u32, 1), tcpDevicesViaNic(io, root, "eth9", route));
}
