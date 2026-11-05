//! The real NVMe namespace behind the PR primitive (Linux; spec
//! `guard_claims.md` §1.1 "Port note — the char device", quirk Q2): the
//! metadata block node is classified as an NVMe namespace by its sysfs
//! `nsid` (present only on NVMe namespaces; its value IS the nsid — no
//! `NVME_IOCTL_ID`), its generic CHAR twin `/dev/ngXnY` is derived by name
//! and verified through `/sys/class/nvme-generic/<ng>/dev`, and every
//! command goes through `IORING_OP_URING_CMD` with `NVME_URING_CMD_{IO,
//! ADMIN}` on a dedicated `SQE128|CQE32` ring the guard owns for its life.
//! Linux exposes NVMe passthru through uring_cmd only on the char nodes
//! (`nvme_ns_chr_uring_cmd`); the block node's uring_cmd is the block
//! layer's own (`BLOCK_URING_CMD_DISCARD`). The NAMESPACE node answers
//! `NVME_URING_CMD_IO` alone (`nvme_ns_uring_cmd`: anything else is
//! `-ENOTTY`); ADMIN commands (Identify, Get Features FID 0x81) go to the
//! CONTROLLER node `/dev/nvme<N>` (`nvme_dev_uring_cmd`) — measured on
//! 7.2.6-sqz against nvmet-loop by the fidelity tier (the rung-5 design
//! put both on the ns node; the reference's ioctl path hid the split
//! because `nvme_ioctl` on a namespace accepts admin commands). So a
//! `Device` holds both fds and routes by queue.
//!
//! The real-device test runs where a `/dev/ng*` exists (the fidelity
//! tier's substrate) and skips LOUD otherwise.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;

const errors = @import("../../core/errors.zig");
const log = @import("../../core/log.zig");
const counters = @import("../../core/counters.zig");
const align_probe = @import("../../io/align_probe.zig");
const uring128 = @import("../../io/uring128.zig");
const futex = @import("../../sync/futex.zig");
const nvme = @import("nvme_cmd.zig");
const pr = @import("pr.zig");
const proc = @import("proc.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/pr_linux is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// Eight entries: one command in flight at a time (`Device.issue_lock`
/// serializes the issuers), the ring floor a `Ring128` admits with the 2× CQ
/// law.
const ring_entries: u16 = 8;
const ud_cmd: u64 = 1;
var issue_wait_failures: counters.Tripwire = .init("pr_issue_wait_failures");
var issue_cqe_mismatches: counters.Tripwire = .init("pr_issue_cqe_mismatches");

/// What the resolution found for a metadata path.
pub const Resolved = union(enum) {
    none: pr.NoPrReason,
    /// An NVMe namespace with its char twin open.
    device: *Device,
};

/// The two char nodes + the ring: the `nvme_cmd.Namespace` face.
pub const Device = struct {
    gpa: Allocator,
    /// `/dev/ngXnY`: the I/O queue (reservation commands).
    fd: linux.fd_t,
    /// `/dev/nvmeX`: the admin queue (Identify, Get Features).
    ctrl_fd: linux.fd_t,
    ring: uring128.Ring128,
    nsid: u32,
    /// `/dev/ngXnY` (for the log).
    node: [32]u8,
    node_len: usize,
    /// One command on the ring at a time: the guard's heartbeat, recovery's
    /// preempts and reports and a lapse's re-register share a Device, and
    /// each command rides its own tag — an issuer that reaped another's
    /// completion would return before the kernel filled its buffer (review
    /// M2 of decision 0003 §DG).
    issue_lock: IssueLock = .{},
    next_cmd: u64 = ud_cmd,
    /// Test seam: fired after a command's submit, before its completion is
    /// reaped (the window two issuers on one ring meet in).
    test_after_submit: if (builtin.is_test) ?TestHook else void = if (builtin.is_test) null else {},

    pub const TestHook = struct { ctx: *anyopaque, fire: *const fn (ctx: *anyopaque) void };

    pub fn face(self: *Device) nvme.Namespace {
        return .{ .ctx = self, .issue = issueFn };
    }

    pub fn nodeName(self: *const Device) []const u8 {
        return self.node[0..self.node_len];
    }

    pub fn deinit(self: *Device) void {
        self.ring.deinit();
        _ = linux.close(self.fd);
        _ = linux.close(self.ctrl_fd);
        self.gpa.destroy(self);
    }

    /// One passthru command: the 72 B `nvme_uring_cmd` at the head of the
    /// SQE128 command area, `cmd_op` and the fd by queue, a submit-and-wait
    /// for its CQE32. `res` ≥ 0 is the NVMe status, < 0 a transport
    /// `-errno`; `big_cqe[0]` carries CDW0/CDW1.
    fn issueFn(ctx: *anyopaque, queue: nvme.Queue, cmd: *const nvme.UringCmd) errors.Error!nvme.Completion {
        const self: *Device = @ptrCast(@alignCast(ctx));
        self.issue_lock.lock();
        defer self.issue_lock.unlock();
        const sqe = self.ring.getSqe() orelse return error.SubmissionQueueOverflow;
        const cmd_op: u32 = switch (queue) {
            .admin => nvme.NVME_URING_CMD_ADMIN,
            .io => nvme.NVME_URING_CMD_IO,
        };
        const fd = switch (queue) {
            .admin => self.ctrl_fd,
            .io => self.fd,
        };
        uring128.prepUringCmd(sqe, .{ .raw = fd }, cmd_op, &cmd.toCmdArea());
        const tag = self.next_cmd;
        self.next_cmd +%= 1;
        sqe.user_data = tag;
        _ = self.ring.submit() catch |e| blk: {
            if (self.ring.sqReady() != 0) {
                // Device has no SQPOLL: after a rejected enter no kernel
                // reader can take this unconsumed payload-bearing SQE.
                const head = @atomicLoad(u32, self.ring.sq.head, .acquire);
                self.ring.sq.sqe_head = head;
                self.ring.sq.sqe_tail = head;
                @atomicStore(u32, self.ring.sq.tail, head, .release);
                return e;
            }
            // A consumed SQE retains its payload until its CQE, even when
            // the syscall supplied no submission answer.
            break :blk 0;
        };
        if (builtin.is_test) if (self.test_after_submit) |h| h.fire(h.ctx);
        var cqes: [1]uring128.Cqe32 = undefined;
        var wait_failed = false;
        while (true) {
            const n = self.ring.copyCqes(&cqes, 1) catch {
                if (!wait_failed) issue_wait_failures.fire("native PR wait failed: retaining the accepted command until its CQE");
                wait_failed = true;
                _ = linux.sched_yield();
                continue;
            };
            if (n == 0) continue;
            if (cqes[0].user_data != tag) {
                issue_cqe_mismatches.fire("native PR CQE is not the command holding the issue lock");
                continue;
            }
            break;
        }
        const cqe = cqes[0];
        if (cqe.res < 0) return errors.fromCqeRes(cqe.res);
        return .{ .status = nvme.statusOf(cqe.res), .result = cqe.big_cqe[0] };
    }
};

/// The issuers' lock: `std.Io.Mutex`'s three-state algorithm over the raw
/// process-private futex (the Device holds no `Io`; a PR command is no hot
/// path). 0 unlocked, 1 locked, 2 locked with waiters. The fencing
/// registrant's sequence lock is one too (`fencer.zig`).
pub const IssueLock = struct {
    word: futex.Word = .init(0),

    pub fn lock(self: *IssueLock) void {
        // Acquire pairs with `unlock`'s release swap: the previous holder's
        // ring state (its SQE written, its CQE reaped) is visible here.
        if (self.word.cmpxchgStrong(0, 1, .acquire, .monotonic) == null) return;
        while (self.word.swap(2, .acquire) != 0) futex.wait(&self.word, 2);
    }

    pub fn unlock(self: *IssueLock) void {
        if (self.word.swap(0, .release) == 2) futex.wake(&self.word, 1);
    }

    /// The `pr.Client.OpLock` face.
    pub fn opLock(self: *IssueLock) pr.Client.OpLock {
        return .{ .ctx = self, .lock = lockFn, .unlock = unlockFn };
    }

    fn lockFn(ctx: *anyopaque) void {
        const self: *IssueLock = @ptrCast(@alignCast(ctx));
        self.lock();
    }

    fn unlockFn(ctx: *anyopaque) void {
        const self: *IssueLock = @ptrCast(@alignCast(ctx));
        self.unlock();
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

const DevNum = struct { major: u32, minor: u32 };

/// `<class dir>/<name>/dev` → `major:minor` of a char node.
fn sysfsClassDev(class: []const u8, name: []const u8) ?DevNum {
    var pb: [96]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/sys/class/{s}/{s}/dev", .{ class, name }, 0) catch return null;
    var buf: [32]u8 = undefined;
    const text = std.mem.trim(u8, proc.readSmall(path.ptr, &buf) orelse return null, " \t\r\n");
    const colon = std.mem.indexOfScalar(u8, text, ':') orelse return null;
    return .{
        .major = std.fmt.parseInt(u32, text[0..colon], 10) catch return null,
        .minor = std.fmt.parseInt(u32, text[colon + 1 ..], 10) catch return null,
    };
}

fn sysfsCharDev(ng: []const u8) ?DevNum {
    return sysfsClassDev("nvme-generic", ng);
}

/// `ngXnY → nvmeX`: the controller's char node.
pub fn controllerName(ng: []const u8, out: *[32]u8) ?[]const u8 {
    if (!std.mem.startsWith(u8, ng, "ng")) return null;
    const rest = ng[2..];
    const n_at = std.mem.indexOfScalar(u8, rest, 'n') orelse return null;
    if (n_at == 0 or 4 + n_at > out.len) return null;
    @memcpy(out[0..4], "nvme");
    @memcpy(out[4 .. 4 + n_at], rest[0..n_at]);
    return out[0 .. 4 + n_at];
}

/// Open a char node and verify it is the device sysfs names (a stale
/// `/dev` entry is another device's); null = refused (logged by the caller).
fn openVerifiedChar(node: [:0]const u8, want: DevNum) ?linux.fd_t {
    const rc = linux.open(node.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(rc);
    var stx: linux.Statx = undefined;
    if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true }, &stx)) != .SUCCESS or !linux.S.ISCHR(stx.mode) or stx.rdev_major != want.major or stx.rdev_minor != want.minor) {
        _ = linux.close(fd);
        return null;
    }
    return fd;
}

/// What a data volume's node is, by facts a reconnect does not change
/// (decision 0003 §DJ, review MAJOR 1): only a stable fact may leave a data
/// volume without a device fence.
pub const NodeFact = enum {
    /// A regular file or another non-block node, or a block node sysfs knows
    /// with no `nsid` (loop, brd, null_blk): never an NVMe namespace.
    not_nvme,
    nvme,
    /// The node is absent or unreadable, or sysfs does not know its device
    /// right now (a controller reconnecting): no fact.
    unknown,
};

pub fn classifyNode(path: []const u8) NodeFact {
    const st = (align_probe.statPath(path) catch return .unknown) orelse return .unknown;
    if (!st.is_bdev) return .not_nvme;
    if (!sysfsBlockDev(st.rdev_major, st.rdev_minor)) return .unknown;
    if (sysfsNsid(st.rdev_major, st.rdev_minor) != null) return .nvme;
    // No `nsid`: a device sysfs still knows is not a namespace; one that
    // went meanwhile proves nothing.
    return if (sysfsBlockDev(st.rdev_major, st.rdev_minor)) .not_nvme else .unknown;
}

/// `/sys/dev/block/M:m` exists: the kernel knows the device now.
fn sysfsBlockDev(major: u32, minor: u32) bool {
    var pb: [64]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/sys/dev/block/{d}:{d}", .{ major, minor }, 0) catch return false;
    var st: linux.Statx = undefined;
    return linux.errno(linux.statx(linux.AT.FDCWD, path.ptr, 0, .{ .TYPE = true }, &st)) == .SUCCESS;
}

/// `path` is an NVMe namespace's block node (no open, no char twin).
pub fn isNvmeNamespace(path: []const u8) bool {
    const st = (align_probe.statPath(path) catch return false) orelse return false;
    if (!st.is_bdev) return false;
    return sysfsNsid(st.rdev_major, st.rdev_minor) != null;
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
    const fd = openVerifiedChar(node, chr) orelse {
        log.warn("meta volume {s}: cannot open {s} as the char node sysfs names ({d}:{d}) — single-writer guard degrades to detection grade", .{ path, node, chr.major, chr.minor });
        return .{ .none = .no_char_node };
    };
    errdefer _ = linux.close(fd);
    // The controller node for the admin queue.
    var cn_buf: [32]u8 = undefined;
    const ctrl = controllerName(ng, &cn_buf) orelse return .{ .none = .no_char_node };
    const ctrl_num = sysfsClassDev("nvme", ctrl) orelse {
        log.warn("meta volume {s}: controller {s} has no /sys/class/nvme entry — single-writer guard degrades to detection grade", .{ path, ctrl });
        return .{ .none = .no_char_node };
    };
    var ctrl_z: [40]u8 = undefined;
    const ctrl_node = std.fmt.bufPrintSentinel(&ctrl_z, "/dev/{s}", .{ctrl}, 0) catch return .{ .none = .no_char_node };
    const ctrl_fd = openVerifiedChar(ctrl_node, ctrl_num) orelse {
        log.warn("meta volume {s}: cannot open {s} as the controller node sysfs names ({d}:{d}) — the admin queue (Identify, FID 0x81) needs it; single-writer guard degrades to detection grade", .{ path, ctrl_node, ctrl_num.major, ctrl_num.minor });
        return .{ .none = .no_char_node };
    };
    errdefer _ = linux.close(ctrl_fd);
    const ring = uring128.Ring128.init(ring_entries, 0) catch |e| {
        log.warn("meta volume {s}: the PR ring could not be set up ({s}) — single-writer guard degrades to detection grade", .{ path, @errorName(e) });
        return .{ .none = .probe_error };
    };
    const dev = gpa.create(Device) catch {
        var r = ring;
        r.deinit();
        return .{ .none = .probe_error };
    };
    dev.* = .{ .gpa = gpa, .fd = fd, .ctrl_fd = ctrl_fd, .ring = ring, .nsid = nsid, .node = undefined, .node_len = ng.len };
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

test "the char twin and the controller node derive by name; a regular file resolves to the single-kernel class (no PR); the real-device leg runs only where a /dev/ng* exists (skipped loud here)" {
    var out: [32]u8 = undefined;
    try testing.expectEqualStrings("ng0n1", charTwinName("nvme0n1", &out).?);
    try testing.expectEqualStrings("ng12n34", charTwinName("nvme12n34", &out).?);
    try testing.expect(charTwinName("sda", &out) == null);
    try testing.expect(charTwinName("loop0", &out) == null);
    try testing.expectEqualStrings("nvme12", controllerName("ng12n34", &out).?);
    try testing.expectEqualStrings("nvme0", controllerName("ng0n1", &out).?);
    try testing.expect(controllerName("nvme0n1", &out) == null);
    try testing.expectEqual(Resolved{ .none = .not_nvme_namespace }, resolve(testing.allocator, "/proc/self/exe"));
    try testing.expectEqual(Resolved{ .none = .not_nvme_namespace }, resolve(testing.allocator, "/nonexistent/path"));
    // The data fence's classifier: a file is a stable fact, an absent node
    // none (review MAJOR 1 of decision 0003 §DJ).
    try testing.expectEqual(NodeFact.not_nvme, classifyNode("/proc/self/exe"));
    try testing.expectEqual(NodeFact.unknown, classifyNode("/nonexistent/path"));
    if (!anyCharNodePresent()) {
        return testkit.skip(@src(), .no_device, "no /dev/ng* on this venue — the real NVMe passthru leg needs the fidelity tier's nvmet-loop");
    }
    // A real namespace: Identify answers, RESCAP reads, the report decodes.
    const ng = firstCharNode().?;
    var block_name: [32]u8 = undefined;
    const ng_name = std.mem.sliceTo(&ng, 0);
    const bn = try std.fmt.bufPrint(&block_name, "/dev/nvme{s}", .{ng_name[2..]});
    const r = resolve(testing.allocator, bn);
    if (r != .device) return testkit.skip(@src(), .no_device, "the first /dev/ng* node did not resolve to an NVMe namespace device");
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

const InterruptedIssue = struct {
    dev: *Device,
    interrupted: bool = false,
    lock_retained: bool = false,
    cqe_errno: ?linux.E = null,

    fn enter(ctx: *anyopaque, ring: *uring128.Ring128, submit_n: u32, wait_n: u32, flags: u32, argp: usize, argsz: usize) usize {
        const self: *InterruptedIssue = @ptrCast(@alignCast(ctx));
        if (submit_n != 0) {
            const sqe = &ring.sq.sqes[ring.sq.head.* & ring.sq.mask];
            const ud = sqe.user_data;
            uring128.prepNop(sqe);
            sqe.user_data = ud;
        }
        if (!self.interrupted) {
            self.interrupted = true;
            self.lock_retained = self.dev.issue_lock.word.load(.acquire) != 0;
            _ = linux.syscall6(.io_uring_enter, @intCast(ring.fd), submit_n, 0, 0, 0, 0);
            return @bitCast(-@as(isize, @backingInt(linux.E.INTR)));
        }
        const rc = linux.syscall6(.io_uring_enter, @intCast(ring.fd), submit_n, wait_n, flags, argp, argsz);
        if (linux.errno(rc) == .SUCCESS and ring.cqReady() != 0) if (self.cqe_errno) |e| {
            ring.cq.cqes[ring.cq.head.* & ring.cq.mask].res = -@as(i32, @backingInt(e));
        };
        return rc;
    }
};

test "PR-2 C1: native PR issue keeps its lock through interrupted wait, reaps its own CQE, and distinguishes completed EINTR" {
    const gpa = testing.allocator;
    const dev = blk: {
        const d = try gpa.create(Device);
        errdefer gpa.destroy(d);
        d.* = .{ .gpa = gpa, .fd = -1, .ctrl_fd = -1, .ring = try uring128.Ring128.init(ring_entries, 0), .nsid = 1, .node = undefined, .node_len = 0 };
        break :blk d;
    };
    defer dev.deinit();
    var seam: InterruptedIssue = .{ .dev = dev };
    dev.ring.test_enter = .{ .ctx = &seam, .call = InterruptedIssue.enter };
    var cmd = nvme.UringCmd.zeroed();
    const answer = try dev.face().issue(dev, .io, &cmd);
    try testing.expectEqual(nvme.STATUS_SUCCESS, answer.status);
    try testing.expect(seam.lock_retained);
    try testing.expectEqual(@as(u32, 0), dev.ring.cqReady());
    seam.cqe_errno = .INTR;
    try testing.expectError(error.Interrupted, dev.face().issue(dev, .io, &cmd));
    try testing.expectEqual(@as(u32, 0), dev.ring.cqReady());
    try testing.expectEqual(@as(u32, 0), dev.issue_lock.word.load(.acquire));
}

const IssueError = struct {
    value: u64 = 0,
    fd: linux.fd_t = -1,
    phase: u8 = 0,
    fail_submit: bool,

    fn enter(ctx: *anyopaque, ring: *uring128.Ring128, submit_n: u32, wait_n: u32, flags: u32, argp: usize, argsz: usize) usize {
        const self: *IssueError = @ptrCast(@alignCast(ctx));
        if (self.phase == 0) {
            self.phase = 1;
            if (self.fail_submit) return @bitCast(-@as(isize, @backingInt(linux.E.NOMEM)));
            const sqe = &ring.sq.sqes[ring.sq.head.* & ring.sq.mask];
            const ud = sqe.user_data;
            uring128.prepRead(sqe, .{ .raw = self.fd }, std.mem.asBytes(&self.value), 0);
            sqe.user_data = ud;
            return linux.syscall6(.io_uring_enter, @intCast(ring.fd), submit_n, 0, 0, 0, 0);
        }
        if (self.phase == 1) {
            self.phase = 2;
            const one: u64 = 1;
            _ = linux.write(self.fd, @ptrCast(&one), @sizeOf(u64));
            return @bitCast(-@as(isize, @backingInt(linux.E.NOMEM)));
        }
        return linux.syscall6(.io_uring_enter, @intCast(ring.fd), submit_n, wait_n, flags, argp, argsz);
    }
};

fn testDevice() !*Device {
    const gpa = testing.allocator;
    const dev = try gpa.create(Device);
    errdefer gpa.destroy(dev);
    dev.* = .{ .gpa = gpa, .fd = -1, .ctrl_fd = -1, .ring = try uring128.Ring128.init(ring_entries, 0), .nsid = 1, .node = undefined, .node_len = 0 };
    return dev;
}

test "PR-2 C1: rejected native submission leaves no SQE retaining the caller's payload" {
    const dev = try testDevice();
    defer dev.deinit();
    var seam: IssueError = .{ .fail_submit = true };
    dev.ring.test_enter = .{ .ctx = &seam, .call = IssueError.enter };
    const cmd = nvme.UringCmd.zeroed();
    try testing.expectError(error.OutOfMemory, dev.face().issue(dev, .io, &cmd));
    try testing.expectEqual(@as(u32, 0), dev.ring.sqReady());
}

test "PR-2 C1: a native CQ wait failure retains the accepted request until its own CQE" {
    const dev = try testDevice();
    defer dev.deinit();
    const rc = linux.eventfd(0, linux.EFD.CLOEXEC);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var seam: IssueError = .{ .fail_submit = false, .fd = fd };
    dev.ring.test_enter = .{ .ctx = &seam, .call = IssueError.enter };
    const cmd = nvme.UringCmd.zeroed();
    log.testing.expectErrorLogs(1);
    const c = try dev.face().issue(dev, .io, &cmd);
    try testing.expectEqual(@as(nvme.Status, 8), c.status);
    try testing.expectEqual(@as(u64, 1), seam.value);
    try testing.expectEqual(@as(u32, 0), dev.ring.cqReady());
    try log.testing.expectAllFired();
}

test "PR-2 C1: an old native CQE is never accepted as the next command's answer" {
    const dev = try testDevice();
    defer dev.deinit();
    const cmd = nvme.UringCmd.zeroed();
    try testing.expectError(error.BadFileDescriptor, dev.face().issue(dev, .io, &cmd));
    const sqe = dev.ring.getSqe().?;
    uring128.prepNop(sqe);
    sqe.user_data = ud_cmd;
    _ = try dev.ring.submitAndWait(1);
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.BadFileDescriptor, dev.face().issue(dev, .io, &cmd));
    try testing.expectEqual(@as(u32, 0), dev.ring.cqReady());
    try log.testing.expectAllFired();
}

/// Two issuers on one Device (review M2 of decision 0003 §DG): A parks
/// after its submit until B is done — or, B shut out, for a bound.
const IssueRace = struct {
    fired: std.atomic.Value(bool) = .init(false),
    a_submitted: futex.Word = .init(0),
    b_done: futex.Word = .init(0),
    a: ?errors.Error = null,
    b: ?errors.Error = null,

    fn park(ctx: *anyopaque) void {
        const r: *IssueRace = @ptrCast(@alignCast(ctx));
        if (r.fired.swap(true, .acq_rel)) return;
        r.a_submitted.store(1, .release);
        futex.wake(&r.a_submitted, 1);
        const deadline = std.time.ns_per_s;
        var waited: u64 = 0;
        const step: u64 = 50 * std.time.ns_per_ms;
        while (r.b_done.load(.acquire) == 0 and waited < deadline) : (waited += step) {
            _ = futex.waitTimeout(&r.b_done, 0, step);
        }
    }

    fn runA(dev: *Device, r: *IssueRace) void {
        var cmd = std.mem.zeroes(nvme.UringCmd);
        cmd.opcode = 0x0E;
        cmd.nsid = 1;
        _ = Device.issueFn(dev, .io, &cmd) catch |e| {
            r.a = e;
        };
    }

    fn runB(dev: *Device, r: *IssueRace) void {
        while (r.a_submitted.load(.acquire) == 0) futex.wait(&r.a_submitted, 0);
        var cmd = std.mem.zeroes(nvme.UringCmd);
        cmd.opcode = 0x06;
        _ = Device.issueFn(dev, .admin, &cmd) catch |e| {
            r.b = e;
        };
        r.b_done.store(1, .release);
        futex.wake(&r.b_done, 1);
    }
};

test "review M2 (decision 0003 §DG): two threads issuing on ONE Device each reap their OWN completion — A's command (a uring_cmd on a regular file: the file system's refusal) submitted and parked before its reap, B's (no controller node: -EBADF) issued meanwhile; never one thread returning the other's answer (a stolen completion parsed as a report, its owner waiting on, or the kernel writing into a returned frame)" {
    const gpa = testing.allocator;
    const dev = blk: {
        const rc = linux.open("/proc/self/exe", .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
        errdefer _ = linux.close(@intCast(rc));
        const d = try gpa.create(Device);
        errdefer gpa.destroy(d);
        d.* = .{ .gpa = gpa, .fd = @intCast(rc), .ctrl_fd = -1, .ring = try uring128.Ring128.init(ring_entries, 0), .nsid = 1, .node = undefined, .node_len = 0 };
        break :blk d;
    };
    defer dev.deinit();
    // Each command's own answer, one at a time (a file system with no
    // uring_cmd answers -EOPNOTSUPP, btrfs -EINVAL): never -EBADF.
    var alone: IssueRace = .{};
    alone.fired.store(true, .release);
    alone.a_submitted.store(1, .release);
    IssueRace.runA(dev, &alone);
    IssueRace.runB(dev, &alone);
    try testing.expectEqual(@as(?errors.Error, error.BadFileDescriptor), alone.b);
    try testing.expect(alone.a != null and alone.a.? != error.BadFileDescriptor);
    var race: IssueRace = .{};
    dev.test_after_submit = .{ .ctx = &race, .fire = IssueRace.park };
    var ta: ?std.Thread = try std.Thread.spawn(.{}, IssueRace.runA, .{ dev, &race });
    defer if (ta) |t| t.join();
    const tb = try std.Thread.spawn(.{}, IssueRace.runB, .{ dev, &race });
    ta.?.join();
    ta = null;
    tb.join();
    try testing.expectEqual(alone.a, race.a);
    try testing.expectEqual(@as(?errors.Error, error.BadFileDescriptor), race.b);
}
