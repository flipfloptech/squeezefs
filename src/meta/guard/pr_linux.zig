//! The real NVMe namespace behind the PR primitive (Linux; spec
//! `guard_claims.md` §1.1 "Port note — the char device", quirk Q2): the
//! metadata block node is classified as an NVMe namespace by its sysfs
//! `nsid` (present only on NVMe namespaces; its value IS the nsid — no
//! `NVME_IOCTL_ID`), its generic CHAR twin `/dev/ngXnY` is derived by name
//! and verified through `/sys/class/nvme-generic/<ng>/dev`, and every
//! command goes through `IORING_OP_URING_CMD` with `NVME_URING_CMD_{IO,
//! ADMIN}` on a dedicated `SQE128|CQE32` ring the guard owns for its life.
//! Linux exposes NVMe passthru through uring_cmd only on the char node
//! (`nvme_ns_chr_uring_cmd`); the block node's uring_cmd is the block
//! layer's own (`BLOCK_URING_CMD_DISCARD`).
//!
//! The venue has no `/dev/ng*` (rung 8 brings nvmet-loop): the resolution
//! is exercised on the plain-file arm here and skips LOUD otherwise.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;

const errors = @import("../../core/errors.zig");
const log = @import("../../core/log.zig");
const align_probe = @import("../../io/align_probe.zig");
const uring128 = @import("../../io/uring128.zig");
const nvme = @import("nvme_cmd.zig");
const pr = @import("pr.zig");
const proc = @import("proc.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/pr_linux is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// Eight entries: one command in flight at a time (the primitive is
/// sequential), the ring floor a `Ring128` admits with the 2× CQ law.
const ring_entries: u16 = 8;
const ud_cmd: u64 = 1;

/// What the resolution found for a metadata path.
pub const Resolved = union(enum) {
    none: pr.NoPrReason,
    /// An NVMe namespace with its char twin open.
    device: *Device,
};

/// The char node + the ring: the `nvme_cmd.Namespace` face.
pub const Device = struct {
    gpa: Allocator,
    fd: linux.fd_t,
    ring: uring128.Ring128,
    nsid: u32,
    /// `/dev/ngXnY` (for the log).
    node: [32]u8,
    node_len: usize,

    pub fn face(self: *Device) nvme.Namespace {
        return .{ .ctx = self, .issue = issueFn };
    }

    pub fn nodeName(self: *const Device) []const u8 {
        return self.node[0..self.node_len];
    }

    pub fn deinit(self: *Device) void {
        self.ring.deinit();
        _ = linux.close(self.fd);
        self.gpa.destroy(self);
    }

    /// One passthru command: the 72 B `nvme_uring_cmd` at the head of the
    /// SQE128 command area, `cmd_op` by queue, a submit-and-wait for its
    /// CQE32. `res` ≥ 0 is the NVMe status, < 0 a transport `-errno`;
    /// `big_cqe[0]` carries CDW0/CDW1.
    fn issueFn(ctx: *anyopaque, queue: nvme.Queue, cmd: *const nvme.UringCmd) errors.Error!nvme.Completion {
        const self: *Device = @ptrCast(@alignCast(ctx));
        const sqe = self.ring.getSqe() orelse return error.SubmissionQueueOverflow;
        const cmd_op: u32 = switch (queue) {
            .admin => nvme.NVME_URING_CMD_ADMIN,
            .io => nvme.NVME_URING_CMD_IO,
        };
        uring128.prepUringCmd(sqe, .{ .raw = self.fd }, cmd_op, &cmd.toCmdArea());
        sqe.user_data = ud_cmd;
        _ = try self.ring.submitAndWait(1);
        var cqes: [1]uring128.Cqe32 = undefined;
        while (true) {
            const n = try self.ring.copyCqes(&cqes, 1);
            if (n == 0) continue;
            if (cqes[0].user_data != ud_cmd) continue;
            break;
        }
        const cqe = cqes[0];
        if (cqe.res < 0) return errors.fromCqeRes(cqe.res);
        return .{ .status = nvme.statusOf(cqe.res), .result = cqe.big_cqe[0] };
    }
};

/// `/sys/dev/block/M:m/nsid` → the nsid, null when absent (not an NVMe
/// namespace) or unparsable.
fn sysfsNsid(major: u32, minor: u32) ?u32 {
    var pb: [96]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/sys/dev/block/{d}:{d}/nsid", .{ major, minor }, 0) catch return null;
    var buf: [32]u8 = undefined;
    const text = proc.readSmall(path.ptr, &buf) orelse return null;
    return std.fmt.parseInt(u32, std.mem.trim(u8, text, " \t\r\n"), 10) catch null;
}

/// `/sys/dev/block/M:m/uevent` `DEVNAME=` → the block node's name.
fn sysfsDevName(major: u32, minor: u32, out: *[32]u8) ?[]const u8 {
    var pb: [96]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/sys/dev/block/{d}:{d}/uevent", .{ major, minor }, 0) catch return null;
    var buf: [1024]u8 = undefined;
    const text = proc.readSmall(path.ptr, &buf) orelse return null;
    var it = std.mem.splitScalar(u8, text, '\n');
    while (it.next()) |line| {
        if (!std.mem.startsWith(u8, line, "DEVNAME=")) continue;
        const name = std.mem.trim(u8, line["DEVNAME=".len..], " \t\r");
        if (name.len == 0 or name.len > out.len) return null;
        @memcpy(out[0..name.len], name);
        return out[0..name.len];
    }
    return null;
}

/// `nvmeXnY → ngXnY` (same instance numbers by kernel construction).
pub fn charTwinName(block_name: []const u8, out: *[32]u8) ?[]const u8 {
    if (!std.mem.startsWith(u8, block_name, "nvme")) return null;
    const rest = block_name["nvme".len..];
    if (rest.len + 2 > out.len) return null;
    out[0] = 'n';
    out[1] = 'g';
    @memcpy(out[2 .. 2 + rest.len], rest);
    return out[0 .. 2 + rest.len];
}

/// `/sys/class/nvme-generic/<ng>/dev` → `major:minor` of the char node.
fn sysfsCharDev(ng: []const u8) ?struct { major: u32, minor: u32 } {
    var pb: [96]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/sys/class/nvme-generic/{s}/dev", .{ng}, 0) catch return null;
    var buf: [32]u8 = undefined;
    const text = std.mem.trim(u8, proc.readSmall(path.ptr, &buf) orelse return null, " \t\r\n");
    const colon = std.mem.indexOfScalar(u8, text, ':') orelse return null;
    return .{
        .major = std.fmt.parseInt(u32, text[0..colon], 10) catch return null,
        .minor = std.fmt.parseInt(u32, text[colon + 1 ..], 10) catch return null,
    };
}

/// Classify `path` and open its char twin. Never fails the mount: every
/// refusal is a `NoPrReason` the posture logs.
pub fn resolve(gpa: Allocator, path: []const u8) Resolved {
    const st = (align_probe.statPath(path) catch return .{ .none = .not_nvme_namespace }) orelse return .{ .none = .not_nvme_namespace };
    if (!st.is_bdev) return .{ .none = .not_nvme_namespace };
    const nsid = sysfsNsid(st.rdev_major, st.rdev_minor) orelse return .{ .none = .not_nvme_namespace };
    var name_buf: [32]u8 = undefined;
    const block_name = sysfsDevName(st.rdev_major, st.rdev_minor, &name_buf) orelse return .{ .none = .no_char_node };
    var ng_buf: [32]u8 = undefined;
    const ng = charTwinName(block_name, &ng_buf) orelse return .{ .none = .no_char_node };
    const chr = sysfsCharDev(ng) orelse {
        log.warn("meta volume {s}: NVMe namespace {s} (nsid {d}) has no generic char node /dev/{s} (CONFIG_NVME generic nodes absent?) — the PR primitive needs uring_cmd passthru on the char node; single-writer guard degrades to detection grade", .{ path, block_name, nsid, ng });
        return .{ .none = .no_char_node };
    };
    var node_z: [40]u8 = undefined;
    const node = std.fmt.bufPrintSentinel(&node_z, "/dev/{s}", .{ng}, 0) catch return .{ .none = .no_char_node };
    const rc = linux.open(node.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) {
        log.warn("meta volume {s}: cannot open {s} ({t}) — single-writer guard degrades to detection grade", .{ path, node, linux.errno(rc) });
        return .{ .none = .no_char_node };
    }
    const fd: linux.fd_t = @intCast(rc);
    errdefer _ = linux.close(fd);
    // The node must be the char device sysfs named (a stale /dev entry is
    // another device's).
    var stx: linux.Statx = undefined;
    if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true }, &stx)) != .SUCCESS or !linux.S.ISCHR(stx.mode) or stx.rdev_major != chr.major or stx.rdev_minor != chr.minor) {
        log.warn("meta volume {s}: {s} is not the char node sysfs names ({d}:{d}) — single-writer guard degrades to detection grade", .{ path, node, chr.major, chr.minor });
        return .{ .none = .no_char_node };
    }
    const ring = uring128.Ring128.init(ring_entries, 0) catch |e| {
        log.warn("meta volume {s}: the PR ring could not be set up ({s}) — single-writer guard degrades to detection grade", .{ path, @errorName(e) });
        return .{ .none = .probe_error };
    };
    const dev = gpa.create(Device) catch {
        var r = ring;
        r.deinit();
        return .{ .none = .probe_error };
    };
    dev.* = .{ .gpa = gpa, .fd = fd, .ring = ring, .nsid = nsid, .node = undefined, .node_len = ng.len };
    @memcpy(dev.node[0..ng.len], ng);
    return .{ .device = dev };
}

/// Is any generic NVMe char node present on this host? The real-device
/// legs run only then (skip loud otherwise).
pub fn anyCharNodePresent() bool {
    var st: linux.Statx = undefined;
    return linux.errno(linux.statx(linux.AT.FDCWD, "/sys/class/nvme-generic", 0, .{ .TYPE = true }, &st)) == .SUCCESS and firstCharNode() != null;
}

fn firstCharNode() ?[32]u8 {
    const rc = linux.open("/sys/class/nvme-generic", .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var buf: [4096]u8 = undefined;
    const n = linux.getdents64(fd, &buf, buf.len);
    if (linux.errno(n) != .SUCCESS) return null;
    var off: usize = 0;
    while (off < n) {
        const d: *align(1) linux.dirent64 = @ptrCast(&buf[off]);
        const name = std.mem.sliceTo(@as([*:0]u8, @ptrCast(&d.name)), 0);
        if (std.mem.startsWith(u8, name, "ng")) {
            var out: [32]u8 = @splat(0);
            @memcpy(out[0..@min(name.len, 31)], name[0..@min(name.len, 31)]);
            return out;
        }
        off += d.reclen;
    }
    return null;
}

const testing = std.testing;

test "the char twin derives by name; a regular file resolves to the single-kernel class (no PR); the real-device leg runs only where a /dev/ng* exists (skipped loud here)" {
    var out: [32]u8 = undefined;
    try testing.expectEqualStrings("ng0n1", charTwinName("nvme0n1", &out).?);
    try testing.expectEqualStrings("ng12n34", charTwinName("nvme12n34", &out).?);
    try testing.expect(charTwinName("sda", &out) == null);
    try testing.expect(charTwinName("loop0", &out) == null);
    try testing.expectEqual(Resolved{ .none = .not_nvme_namespace }, resolve(testing.allocator, "/proc/self/exe"));
    try testing.expectEqual(Resolved{ .none = .not_nvme_namespace }, resolve(testing.allocator, "/nonexistent/path"));
    if (!anyCharNodePresent()) {
        log.warn("guard/pr_linux: no /dev/ng* on this venue — the real NVMe passthru leg is skipped (rung 8 brings nvmet-loop)", .{});
        return error.SkipZigTest;
    }
    // A real namespace: Identify answers, RESCAP reads, the report decodes.
    const ng = firstCharNode().?;
    var block_name: [32]u8 = undefined;
    const ng_name = std.mem.sliceTo(&ng, 0);
    const bn = try std.fmt.bufPrint(&block_name, "/dev/nvme{s}", .{ng_name[2..]});
    const r = resolve(testing.allocator, bn);
    if (r != .device) return error.SkipZigTest;
    defer r.device.deinit();
    var client = try pr.Client.init(testing.allocator, r.device.face(), r.device.nsid, 0x5a5a);
    defer client.deinit();
    const rescap = try client.probeRescap();
    std.debug.print("\nreal NVMe {s} nsid {d}: RESCAP 0x{x}\n", .{ r.device.nodeName(), r.device.nsid, rescap });
    if (rescap == 0) return;
    var rep = try client.report();
    defer rep.deinit(testing.allocator);
    std.debug.print("report: rtype {d}, regctl {d}, ptpls {d}\n", .{ rep.rtype, rep.regctl, rep.ptpls });
}
