//! `squeezefs storage {list, validate}` (charter §8; `docs/porting/
//! ops_nvmet_keys.md` §6, quirk Q10): the host's NVMe inventory and the
//! backing-device classifier — never an LVM wrapper (the charter's data
//! plane stripes whole namespaces itself; an LVM layer under a PR-fenced
//! namespace would hide the `ng` char node and the reservation from the
//! daemon; `pvcreate -ff` re-initializes whatever disk is named).
//!
//! `storage list`: every controller of `/sys/class/nvme` (transport,
//! state, subsysnqn, address, host identity) and every namespace node
//! (block + `ng` twin, nsid, uuid, size, RESCAP via Identify where root)
//! with the squeezefs CLASS read off sector 0 — the `pv_is_squeezefs_
//! backed` question answered by the superblock, never by an NQN prefix.
//! `storage validate <path>`: the `validate_backing_device` ladder (ST:
//! 438-502) — missing, a zero-size file, a zero-capacity block device
//! ("an unattached loop device"), a CHAR device ("pass the namespace
//! block node, not the controller") — plus the class and, on an NVMe
//! namespace, the char twin and RESCAP. Linux only.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const align_probe = @import("../../io/align_probe.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const superblock = @import("../../meta/superblock.zig");
const data_superblock = @import("../../data/data_superblock.zig");
const pr = @import("../../meta/guard/pr.zig");
const pr_linux = @import("../../meta/guard/pr_linux.zig");
const configfs = @import("configfs.zig");
const initiator = @import("initiator.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/storage is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// What sector 0 says a device is.
pub const Class = union(enum) {
    /// `SQZMETLV` v4: a metadata member of the set.
    meta: struct { set_uuid: [16]u8, member_uuid: [16]u8 },
    /// A pre-v5 metadata image.
    meta_legacy: u32,
    /// A data volume: `vol_tag` + the set.
    data: struct { vol_tag: u64, set_uuid: [16]u8, device_uuid: [16]u8 },
    blank,
    /// A magic neither format owns.
    foreign,
    unreadable,

    pub fn write(self: Class, w: *Io.Writer) Io.Writer.Error!void {
        switch (self) {
            .meta => |m| {
                try w.writeAll("squeezefs metadata member of set ");
                try configfs.writeUuid(w, m.set_uuid);
                try w.writeAll(" (member ");
                try configfs.writeUuid(w, m.member_uuid);
                try w.writeByte(')');
            },
            .meta_legacy => |v| try w.print("squeezefs pre-v5 metadata image (version {d}; reformat required)", .{v}),
            .data => |d| {
                try w.print("squeezefs data volume vol-{x:0>16} of set ", .{d.vol_tag});
                try configfs.writeUuid(w, d.set_uuid);
            },
            .blank => try w.writeAll("blank (zeroed sector 0)"),
            .foreign => try w.writeAll("foreign (a magic neither squeezefs format owns)"),
            .unreadable => try w.writeAll("unreadable"),
        }
    }

    pub fn tag(self: Class) []const u8 {
        return switch (self) {
            .meta => "meta",
            .meta_legacy => "meta_legacy",
            .data => "data",
            .blank => "blank",
            .foreign => "foreign",
            .unreadable => "unreadable",
        };
    }
};

/// Sector 0 through the io_uring file layer; the classifiers of both
/// formats in turn.
pub fn classify(pool: *uring_fs.Pool, path: []const u8) Class {
    // The pool's positional read opens O_CREAT: an absent path is judged
    // here so a census never creates a file.
    switch (validate(path)) {
        .ok => {},
        else => return .unreadable,
    }
    var res = pool.readAt(path, 0, superblock.sector_len) catch return .unreadable;
    defer res.deinit();
    const sector = res.bytes();
    if (sector.len != superblock.sector_len) return .unreadable;
    if (superblock.classifySector0(sector, null)) |f| switch (f) {
        .blank => return .blank,
        .legacy => |v| return .{ .meta_legacy = v },
        .v5 => |sb| return .{ .meta = .{ .set_uuid = sb.set_uuid, .member_uuid = sb.uuid } },
    } else |_| {}
    if (data_superblock.classifySector0(sector, null)) |f| switch (f) {
        .blank => return .blank,
        .v2 => |sb| return .{ .data = .{ .vol_tag = sb.vol_tag, .set_uuid = sb.set_uuid, .device_uuid = sb.device_uuid } },
    } else |_| {}
    return .foreign;
}

/// The `validate_backing_device` ladder (ST:438-502).
pub const Verdict = union(enum) {
    ok: struct { is_bdev: bool, size: u64 },
    missing,
    zero_size_file,
    /// `/sys/class/block/<n>/size == 0`: an unattached loop device.
    zero_capacity_block,
    char_device,
    other_kind,

    pub fn write(self: Verdict, w: *Io.Writer, path: []const u8) Io.Writer.Error!void {
        switch (self) {
            .ok => |o| try w.print("{s}: {s} of {d} bytes", .{ path, if (o.is_bdev) "block device" else "regular file", o.size }),
            .missing => try w.print("{s} does not exist", .{path}),
            .zero_size_file => try w.print("{s} is a zero-size file — `truncate -s <size>` it first", .{path}),
            .zero_capacity_block => try w.print("{s} is a block device of zero capacity (an unattached loop device?)", .{path}),
            .char_device => try w.print("{s} is a CHARACTER device — pass the namespace block node (/dev/nvme0n1), not the controller (/dev/nvme0) or the generic node (/dev/ng0n1)", .{path}),
            .other_kind => try w.print("{s} is neither a regular file nor a block device", .{path}),
        }
    }
};

pub fn validate(path: []const u8) Verdict {
    var zb: [linux.PATH_MAX]u8 = undefined;
    const z = align_probe.pathZ(&zb, path) catch return .missing;
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, z, 0, .{ .TYPE = true, .SIZE = true }, &st)) != .SUCCESS) return .missing;
    if (linux.S.ISCHR(st.mode)) return .char_device;
    if (linux.S.ISREG(st.mode)) return if (st.size == 0) .zero_size_file else .{ .ok = .{ .is_bdev = false, .size = st.size } };
    if (!linux.S.ISBLK(st.mode)) return .other_kind;
    const size = blockSize(st.rdev_major, st.rdev_minor) orelse 0;
    if (size == 0) return .zero_capacity_block;
    return .{ .ok = .{ .is_bdev = true, .size = size } };
}

/// `/sys/dev/block/M:m/size` × 512.
fn blockSize(major: u32, minor: u32) ?u64 {
    var pb: [96]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/sys/dev/block/{d}:{d}/size", .{ major, minor }, 0) catch return null;
    var buf: [32]u8 = undefined;
    const t = @import("../../meta/guard/proc.zig").readSmall(path.ptr, &buf) orelse return null;
    const sectors = std.fmt.parseInt(u64, std.mem.trim(u8, t, " \t\r\n"), 10) catch return null;
    return sectors * 512;
}

/// An NVMe namespace's facts for the census: the char twin and RESCAP
/// (Identify through the guard's own device face; null when not root or
/// no twin).
pub const NvmeFacts = struct { char_node: [32]u8 = undefined, char_len: usize = 0, nsid: u32 = 0, rescap: ?u8 = null };

pub fn nvmeFacts(gpa: Allocator, block_path: []const u8) ?NvmeFacts {
    switch (pr_linux.resolve(gpa, block_path)) {
        .none => return null,
        .device => |dev| {
            defer dev.deinit();
            var f: NvmeFacts = .{ .nsid = dev.nsid };
            const name = dev.nodeName();
            @memcpy(f.char_node[0..name.len], name);
            f.char_len = name.len;
            var client = pr.Client.init(gpa, dev.face(), dev.nsid, 0) catch return f;
            defer client.deinit();
            f.rescap = client.probeRescap() catch null;
            return f;
        },
    }
}

pub const Ctx = struct {
    gpa: Allocator,
    pool: *uring_fs.Pool,
    stdout: *Io.Writer,
    stderr: *Io.Writer,
};

pub const Outcome = enum(u8) { ok = 0, failed = 1 };

/// `storage list [--json]`.
pub fn list(ctx: *Ctx, json: bool) Outcome {
    const gpa = ctx.gpa;
    const ctrls = initiator.listControllers(gpa, initiator.SYSFS_ROOT) catch {
        ctx.stderr.writeAll("squeezefs storage list: out of memory\n") catch {};
        return .failed;
    };
    defer gpa.free(ctrls);
    const w = ctx.stdout;
    if (json) w.writeAll("{\"controllers\":[") catch {} else w.print("NVMe controllers: {d}\n", .{ctrls.len}) catch {};
    for (ctrls, 0..) |*c, i| {
        if (json) {
            if (i != 0) w.writeByte(',') catch {};
            w.print("{{\"name\":\"{s}\",\"transport\":\"{s}\",\"state\":\"{s}\",\"subsysnqn\":\"{s}\",\"address\":\"{s}\",\"hostnqn\":\"{s}\",\"hostid\":\"{s}\"}}", .{ c.nameOf(), c.transportOf(), c.stateOf(), c.subsysnqnOf(), c.addressOf(), c.hostnqnOf(), c.hostidOf() }) catch {};
        } else {
            w.print("  {s}: {s} {s} {s} {s}\n", .{ c.nameOf(), c.transportOf(), c.stateOf(), c.subsysnqnOf(), c.addressOf() }) catch {};
            if (c.hostnqn_len != 0) w.print("    host {s} / {s}\n", .{ c.hostnqnOf(), c.hostidOf() }) catch {};
        }
    }
    if (json) w.writeAll("],\"namespaces\":[") catch {} else w.writeAll("NVMe namespaces:\n") catch {};
    var n_ns: usize = 0;
    for (ctrls) |*c| {
        const ns = (initiator.findNamespaceUnderController(gpa, initiator.SYSFS_ROOT, c.instance) catch null) orelse continue;
        var bp: [40]u8 = undefined;
        const block = ns.blockPath(&bp);
        const class = classify(ctx.pool, block);
        const facts = nvmeFacts(gpa, block);
        const verdict = validate(block);
        const size: u64 = switch (verdict) {
            .ok => |o| o.size,
            else => 0,
        };
        var ub: [36]u8 = undefined;
        const uuid_text: []const u8 = if (ns.uuid) |u| configfs.uuidText(&ub, u) else "";
        if (json) {
            if (n_ns != 0) w.writeByte(',') catch {};
            w.print("{{\"device\":\"{s}\",\"char\":\"/dev/{s}\",\"controller\":\"{s}\",\"nsid\":{d},\"uuid\":\"{s}\",\"size\":{d},\"rescap\":{?d},\"class\":\"{s}\"", .{ block, ns.charName(), c.nameOf(), ns.nsid, uuid_text, size, if (facts) |f| f.rescap else null, class.tag() }) catch {};
            switch (class) {
                .data => |d| w.print(",\"vol_tag\":\"vol-{x:0>16}\"", .{d.vol_tag}) catch {},
                else => {},
            }
            w.writeByte('}') catch {};
        } else {
            w.print("  {s} (/dev/{s}) on {s}: nsid {d}; uuid {s}; {d} bytes; RESCAP ", .{ block, ns.charName(), c.nameOf(), ns.nsid, uuid_text, size }) catch {};
            if (facts) |f| if (f.rescap) |r| w.print("0x{x:0>2}", .{r}) catch {} else w.writeAll("(unreadable)") catch {} else w.writeAll("(no char twin)") catch {};
            w.writeAll("; ") catch {};
            class.write(w) catch {};
            w.writeAll("\n") catch {};
        }
        n_ns += 1;
    }
    if (json) w.writeAll("]}\n") catch {} else if (n_ns == 0) w.writeAll("  (none)\n") catch {};
    w.flush() catch {};
    return .ok;
}

/// `storage validate <path>`: the ladder, the class, the NVMe facts.
pub fn validateVerb(ctx: *Ctx, path: []const u8) Outcome {
    const v = validate(path);
    var buf: [1024]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    v.write(&w, path) catch {};
    if (v != .ok) {
        ctx.stderr.print("squeezefs storage validate: {s} — not a backing device\n", .{w.buffered()}) catch {};
        ctx.stderr.flush() catch {};
        return .failed;
    }
    ctx.stdout.print("{s}\n  class: ", .{w.buffered()}) catch {};
    classify(ctx.pool, path).write(ctx.stdout) catch {};
    ctx.stdout.writeAll("\n") catch {};
    if (nvmeFacts(ctx.gpa, path)) |f| {
        ctx.stdout.print("  NVMe namespace: nsid {d}; char node /dev/{s}; RESCAP ", .{ f.nsid, f.char_node[0..f.char_len] }) catch {};
        if (f.rescap) |r| ctx.stdout.print("0x{x:0>2} ({s})\n", .{ r, if (r == 0) "no reservations: the single-writer guard is detection-grade here" else "Persistent Reservations available: the guard arms PR" }) catch {} else ctx.stdout.writeAll("(unreadable)\n") catch {};
    } else ctx.stdout.writeAll("  not an NVMe namespace (a file or a non-NVMe block device): the single-writer guard is flock + claim\n") catch {};
    ctx.stdout.flush() catch {};
    return .ok;
}

const testing = std.testing;

test "validate: a missing path, a zero-size file, a character device and an ordinary file classify; sector 0 of a blank file is blank and of a text file is foreign" {
    _ = linux.mkdir("/build/tmp", 0o755);
    _ = linux.unlink("/build/tmp/storage-does-not-exist");
    try testing.expectEqual(Verdict.missing, validate("/build/tmp/storage-does-not-exist"));
    try testing.expectEqual(Verdict.char_device, validate("/dev/null"));
    const zero = "/build/tmp/storage-zero.img";
    try Io.Dir.cwd().writeFile(testing.io, .{ .sub_path = zero, .data = "" });
    defer _ = linux.unlink(zero);
    try testing.expectEqual(Verdict.zero_size_file, validate(zero));
    const blank = "/build/tmp/storage-blank.img";
    {
        const f = try Io.Dir.cwd().createFile(testing.io, blank, .{ .truncate = true });
        defer f.close(testing.io);
        try f.setLength(testing.io, 1 << 20);
    }
    defer _ = linux.unlink(blank);
    const v = validate(blank);
    try testing.expect(v == .ok and !v.ok.is_bdev and v.ok.size == 1 << 20);
    const pool = uring_fs.Pool.init(testing.allocator, uring_fs.Config.fixed(1, uring_fs.nofileSoftAsFound())) catch return testkit.skip(@src(), .no_kernel_feature, "the io_uring file layer's ring did not set up on this host");
    defer pool.deinit();
    try testing.expectEqual(Class.blank, classify(pool, blank));
    const text = "/build/tmp/storage-text.img";
    {
        const f = try Io.Dir.cwd().createFile(testing.io, text, .{ .truncate = true });
        defer f.close(testing.io);
        try f.writeStreamingAll(testing.io, "this is not a squeezefs volume");
        try f.setLength(testing.io, 1 << 20);
    }
    defer _ = linux.unlink(text);
    try testing.expectEqual(Class.foreign, classify(pool, text));
    try testing.expectEqual(Class.unreadable, classify(pool, "/build/tmp/storage-does-not-exist"));
    try testing.expectEqual(Verdict.missing, validate("/build/tmp/storage-does-not-exist"));
    try testing.expect(nvmeFacts(testing.allocator, blank) == null);
    var buf: [256]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    const cd: Verdict = .char_device;
    try cd.write(&w, "/dev/nvme0");
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "not the controller") != null);
}
