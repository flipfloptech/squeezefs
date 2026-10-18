//! The kernel's native NVMe multipath under the daemon's data-plane
//! associations (charter §3 "Host-scoped NVMe fabric subsystems", §8;
//! `docs/porting/ops_nvmet_keys.md` §3.7): a data volume's
//! `fabric_endpoint` record names ONE subsystem over SEVERAL paths (one
//! per target listener — several Ethernet links); the daemon connects
//! every path under ONE host identity and the kernel (`CONFIG_NVME_
//! MULTIPATH=y`, `nvme_core.multipath=Y`; patch 0025 keeps same-identity
//! merging) merges the controllers into one namespace head `nvme<K>n<Y>`
//! in the `nvme-subsys<K>` dir that links them. This module is the sysfs
//! side of that: which subsystem dir lists a controller, its serving set
//! judged EXACTLY OURS as a pure verdict (every path ours, no foreign
//! one — a head shared with another identity round-robins across a
//! per-mount fence), the hidden per-path node `nvme<K>c<N>n<Y>` and its
//! `stat` (the proof that a link carried bytes), the subsystem's
//! `iopolicy` written and read back (the kernel's `numa` default sends
//! every I/O of a NUMA node down one path; `SQUEEZEFS_FABRIC_IOPOLICY`),
//! and the `nvmeof status` walk. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const knobs = @import("../../core/knobs.zig");
const proc = @import("../../meta/guard/proc.zig");
const claim = @import("../../meta/guard/claim.zig");
const configfs = @import("configfs.zig");
const initiator = @import("initiator.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/multipath is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// `/sys/class/nvme-subsystem/<SUBSYS_PREFIX><K>`: the kernel's name.
pub const SUBSYS_PREFIX = "nvme-subsys";
pub const MULTIPATH_PARAM = "/sys/module/nvme_core/parameters/multipath";

/// `nvme_core.multipath`: `Y`, `N`, or absent (`CONFIG_NVME_MULTIPATH`
/// unset — the VM's kernel).
pub const MultipathParam = enum {
    on,
    off,
    absent,

    pub fn text(self: MultipathParam) []const u8 {
        return switch (self) {
            .on => "Y",
            .off => "N",
            .absent => "absent (CONFIG_NVME_MULTIPATH unset)",
        };
    }
};

pub fn readMultipathParam() MultipathParam {
    var buf: [16]u8 = undefined;
    const t = proc.readSmall(MULTIPATH_PARAM, &buf) orelse return .absent;
    return if (std.mem.eql(u8, std.mem.trim(u8, t, " \t\r\n"), "Y")) .on else .off;
}

// ---------------------------------------------------------------------------
// The I/O policy.
// ---------------------------------------------------------------------------

/// The kernel's `nvme_iopolicy_names`, spelled as sysfs takes them.
pub const Iopolicy = enum {
    numa,
    @"round-robin",
    @"queue-depth",

    pub fn name(self: Iopolicy) []const u8 {
        return @tagName(self);
    }

    pub fn parse(text: []const u8) ?Iopolicy {
        return std.meta.stringToEnum(Iopolicy, text);
    }
};

comptime {
    // Every value the registry admits is one the kernel spells.
    for (knobs.row("SQUEEZEFS_FABRIC_IOPOLICY").kind.enumeration.allowed) |a| {
        if (Iopolicy.parse(a) == null) @compileError("SQUEEZEFS_FABRIC_IOPOLICY admits '" ++ a ++ "', which is not a kernel iopolicy");
    }
}

/// The policy the mount writes: the knob through the ONE parser.
pub fn iopolicyKnob(env: *const std.process.Environ.Map) Iopolicy {
    return Iopolicy.parse(knobs.enumKnob("SQUEEZEFS_FABRIC_IOPOLICY", env)) orelse unreachable; // the registry's values all parse (the comptime check above)
}

pub const IopolicyOutcome = union(enum) {
    /// Written and read back as asked.
    set,
    /// Written without error but read back otherwise: a kernel without
    /// the policy keeps its own (named).
    mismatch: []const u8,
    /// The write refused (EINVAL: a policy this kernel does not know).
    write_refused: linux.E,
    /// No `iopolicy` attribute: a non-multipath kernel.
    absent,

    pub fn write(self: IopolicyOutcome, w: *Io.Writer, want: Iopolicy) Io.Writer.Error!void {
        switch (self) {
            .set => try w.print("iopolicy {s}", .{want.name()}),
            .mismatch => |got| try w.print("iopolicy stays '{s}' after writing '{s}' — this kernel has no such policy (queue-depth needs ≥ 6.11); I/O follows the kernel's policy", .{ got, want.name() }),
            .write_refused => |e| try w.print("iopolicy write of '{s}' refused ({t}) — this kernel does not know the policy; I/O follows the kernel's", .{ want.name(), e }),
            .absent => try w.print("no iopolicy attribute (a kernel without native NVMe multipath) — '{s}' not applied", .{want.name()}),
        }
    }
};

/// Read `<root>/class/nvme-subsystem/<subsys>/iopolicy`, trimmed.
pub fn readIopolicy(root: []const u8, subsys: []const u8, buf: []u8) ?[]const u8 {
    return initiator.readSysfs(buf, "{s}/class/nvme-subsystem/{s}/iopolicy", .{ root, subsys });
}

/// Write the policy and read it back; `got` holds the read-back text the
/// mismatch arm names.
pub fn setIopolicy(root: []const u8, subsys: []const u8, policy: Iopolicy, got: *[32]u8) IopolicyOutcome {
    var pb: [linux.PATH_MAX]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "{s}/class/nvme-subsystem/{s}/iopolicy", .{ root, subsys }, 0) catch return .absent;
    const rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return if (linux.errno(rc) == .NOENT) .absent else .{ .write_refused = linux.errno(rc) };
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    const text = policy.name();
    const n = linux.write(fd, text.ptr, text.len);
    if (linux.errno(n) != .SUCCESS) return .{ .write_refused = linux.errno(n) };
    const back = readIopolicy(root, subsys, got) orelse return .absent;
    return if (std.mem.eql(u8, back, text)) .set else .{ .mismatch = back };
}

// ---------------------------------------------------------------------------
// The path-loss clocks of a daemon connect.
// ---------------------------------------------------------------------------

/// The derived `ConnectTimeouts`, each a pure function of the single-writer
/// guard's clock (`claim.zig`): `reconnect_delay` = the claim's beat
/// (10 s: a path retries as often as the claim beats, so a path back within
/// one beat is seen before the claim ages); `fast_io_fail_tmo` = the claim's
/// staleness TTL (45 s: with every path down that long the holder's claim
/// is preemptable, so its held I/O must fail THEN and fail-stop the volume
/// — and well under the FUSE 135 s request timeout that would abort the
/// connection instead); `ctrl_loss_tmo` = 2 × the TTL (90 s: a path keeps
/// reconnecting one more window past fail-fast, so an outage that spans it
/// still brings the association back for the FAILED daemon's teardown to
/// release — never the kernel's 600 s, which pins a dead association for
/// ten minutes).
pub fn deriveConnectTimeouts() initiator.ConnectTimeouts {
    return .{
        .reconnect_delay = @intCast(claim.HEARTBEAT_INTERVAL_S),
        .fast_io_fail_tmo = @intCast(claim.STALE_TTL_S),
        .ctrl_loss_tmo = @intCast(2 * claim.STALE_TTL_S),
    };
}

/// `explicit > derived` per field; never refuses (the startup gate did).
pub fn resolveConnectTimeouts(env: *const std.process.Environ.Map) initiator.ConnectTimeouts {
    const d = deriveConnectTimeouts();
    return .{
        .fast_io_fail_tmo = knobs.optIntKnob("SQUEEZEFS_FABRIC_FAST_IO_FAIL_TMO_S", u32, env) orelse d.fast_io_fail_tmo,
        .ctrl_loss_tmo = knobs.optIntKnob("SQUEEZEFS_FABRIC_CTRL_LOSS_TMO_S", u32, env) orelse d.ctrl_loss_tmo,
        .reconnect_delay = knobs.optIntKnob("SQUEEZEFS_FABRIC_RECONNECT_DELAY_S", u32, env) orelse d.reconnect_delay,
    };
}

// ---------------------------------------------------------------------------
// The head's serving set: EXACTLY OURS as a pure verdict.
// ---------------------------------------------------------------------------

pub const HeadVerdict = union(enum) {
    /// Every serving controller carries the pair and every path we
    /// connected is a serving controller.
    exactly_ours,
    /// A serving controller under another identity, or one whose dir is
    /// gone (never provably ours): the head would round-robin across a
    /// foreign per-mount fence.
    foreign: u32,
    /// One of our connected paths is not a serving controller of the
    /// dir: the kernel grouped it elsewhere (two heads, no merge).
    missing: u32,
    /// No subsystem dir lists our first controller: a kernel without
    /// native multipath (each path is its own `nvme<N>n<Y>`).
    no_head,

    pub fn write(self: HeadVerdict, w: *Io.Writer) Io.Writer.Error!void {
        switch (self) {
            .exactly_ours => try w.writeAll("every serving controller of the head is this mount's"),
            .foreign => |n| try w.print("controller nvme{d} serves the same head under ANOTHER host identity (or is gone): a shared head round-robins across a per-mount fence — boot the sqz kernel with nvme_core.fabrics_host_scoped_subsystems=Y, or disconnect it", .{n}),
            .missing => |n| try w.print("controller nvme{d} (one of this mount's paths) is not a serving controller of the head's subsystem: the kernel did not merge the paths", .{n}),
            .no_head => try w.writeAll("no multipath head: this kernel has no native NVMe multipath (CONFIG_NVME_MULTIPATH / nvme_core.multipath=Y), so several paths would be several disks"),
        }
    }
};

/// Pure: `serving` are the controllers the subsystem dir links, `ours`
/// the instances this mount connected (≥ 1), `pair` its identity.
pub fn judgeServing(serving: []const initiator.Controller, ours: []const u32, pair: initiator.HostPair) HeadVerdict {
    for (serving) |*c| if (!c.hasIdentity(pair.hostnqn, pair.hostid)) return .{ .foreign = c.instance };
    for (ours) |inst| {
        var listed = false;
        for (serving) |*c| listed = listed or c.instance == inst;
        if (!listed) return .{ .missing = inst };
    }
    return .exactly_ours;
}

/// The instance of a controller link `nvme<N>` in a subsystem dir; null
/// for a namespace head, the attributes, anything else.
pub fn controllerLinkInstance(name: []const u8) ?u32 {
    if (!std.mem.startsWith(u8, name, "nvme")) return null;
    const rest = name["nvme".len..];
    if (rest.len == 0) return null;
    for (rest) |c| if (c < '0' or c > '9') return null;
    return std.fmt.parseInt(u32, rest, 10) catch null; // decimal, as sysfs names the controller
}

/// The `nvme-subsys<K>` dir that lists controller `nvme<N>` (into `out`);
/// null on a kernel that made no head for it.
pub fn subsystemOf(gpa: Allocator, root: []const u8, instance: u32, out: *[32]u8) !?[]const u8 {
    const subs = initiator.listDir(gpa, "{s}/class/nvme-subsystem", .{root}) catch |e| switch (e) {
        error.FileNotFound => return null,
        else => return e,
    };
    defer configfs.freeNames(gpa, subs);
    for (subs) |sub| {
        if (!std.mem.startsWith(u8, sub, SUBSYS_PREFIX) or sub.len > out.len) continue;
        const children = initiator.listDir(gpa, "{s}/class/nvme-subsystem/{s}", .{ root, sub }) catch continue;
        defer configfs.freeNames(gpa, children);
        for (children) |ch| {
            if (controllerLinkInstance(ch) == instance) {
                @memcpy(out[0..sub.len], sub);
                return out[0..sub.len];
            }
        }
    }
    return null;
}

/// The controllers a subsystem dir links; a link whose controller dir is
/// gone reads as its instance with no attributes (never provably ours).
pub fn servingControllers(gpa: Allocator, root: []const u8, subsys: []const u8) ![]initiator.Controller {
    const children = try initiator.listDir(gpa, "{s}/class/nvme-subsystem/{s}", .{ root, subsys });
    defer configfs.freeNames(gpa, children);
    var out: std.ArrayList(initiator.Controller) = .empty;
    errdefer out.deinit(gpa);
    for (children) |ch| {
        const inst = controllerLinkInstance(ch) orelse continue;
        try out.append(gpa, initiator.readController(root, ch) orelse .{ .instance = inst });
    }
    return out.toOwnedSlice(gpa);
}

/// The head's name in a subsystem dir (`nvme<K>n<Y>`, strict shape), or
/// null before the scan made one.
pub fn headOf(gpa: Allocator, root: []const u8, subsys: []const u8, out: *[32]u8) !?[]const u8 {
    const children = try initiator.listDir(gpa, "{s}/class/nvme-subsystem/{s}", .{ root, subsys });
    defer configfs.freeNames(gpa, children);
    for (children) |ch| {
        if (initiator.splitNamespaceName(ch) == null or ch.len > out.len) continue;
        @memcpy(out[0..ch.len], ch);
        return out[0..ch.len];
    }
    return null;
}

/// The `nvme-subsys<K>` dir whose head is the block node `nvme<K>n<Y>`
/// (into `out`); null for a per-controller node (no head) or a non-NVMe
/// name. The metadata device arrives as a path: this is how its paths
/// are found.
pub fn subsystemOfNode(gpa: Allocator, root: []const u8, block_name: []const u8, out: *[32]u8) !?[]const u8 {
    if (initiator.splitNamespaceName(block_name) == null) return null;
    const subs = initiator.listDir(gpa, "{s}/class/nvme-subsystem", .{root}) catch |e| switch (e) {
        error.FileNotFound => return null,
        else => return e,
    };
    defer configfs.freeNames(gpa, subs);
    for (subs) |sub| {
        if (!std.mem.startsWith(u8, sub, SUBSYS_PREFIX) or sub.len > out.len) continue;
        const children = initiator.listDir(gpa, "{s}/class/nvme-subsystem/{s}", .{ root, sub }) catch continue;
        defer configfs.freeNames(gpa, children);
        for (children) |ch| {
            if (std.mem.eql(u8, ch, block_name)) {
                @memcpy(out[0..sub.len], sub);
                return out[0..sub.len];
            }
        }
    }
    return null;
}

/// The §5.2 shape of a head's serving set (pure): how many fabric paths
/// serve it, whether any is a local (PCIe) controller — a local disk, no
/// fabric identity to judge —, whether the paths carry MORE THAN ONE
/// identity (the merged shape: a gone controller is an unknown identity),
/// and which controller's identity stands for the device (the first).
pub const ServingShape = struct {
    fabric_paths: usize,
    local: bool,
    merged: bool,
    first: ?usize,
};

pub fn servingShape(serving: []const initiator.Controller) ServingShape {
    var out: ServingShape = .{ .fabric_paths = 0, .local = false, .merged = false, .first = null };
    var first_nqn: ?[]const u8 = null;
    for (serving, 0..) |*c, i| {
        if (c.transport_len != 0 and !c.isFabric()) {
            out.local = true;
            continue;
        }
        out.fabric_paths += 1;
        if (out.first == null) out.first = i;
        if (c.transport_len == 0) {
            // A link whose controller dir is gone: an identity nobody can
            // read among the paths.
            out.merged = true;
            continue;
        }
        if (first_nqn) |f| {
            if (!std.mem.eql(u8, f, c.hostnqnOf())) out.merged = true;
        } else first_nqn = c.hostnqnOf();
    }
    return out;
}

/// Live controllers linked in a subsystem dir, under `pair` when given
/// (any identity otherwise), straight off the dir with no allocation (the
/// `.stats` render's law).
pub fn countLiveServing(root: []const u8, subsys: []const u8, pair: ?initiator.HostPair) u64 {
    var walk = initiator.DirWalk.open("{s}/class/nvme-subsystem/{s}", .{ root, subsys }) catch return 0;
    defer walk.close();
    var n: u64 = 0;
    while (walk.next() catch null) |name| {
        if (controllerLinkInstance(name) == null) continue;
        const c = initiator.readController(root, name) orelse continue;
        if (!c.isLive()) continue;
        if (pair) |p| if (!c.hasIdentity(p.hostnqn, p.hostid)) continue;
        n += 1;
    }
    return n;
}

/// The head's verdict and the subsystem dir name's length in `subsys_out`
/// (0 when no dir lists the first path).
pub const Head = struct { verdict: HeadVerdict, subsys_len: usize };

/// The head verdict for the paths this mount connected: the subsystem
/// dir of the first (named into `subsys_out`), its serving set, judged.
pub fn headVerdict(gpa: Allocator, root: []const u8, ours: []const u32, pair: initiator.HostPair, subsys_out: *[32]u8) !Head {
    const sub = (try subsystemOf(gpa, root, ours[0], subsys_out)) orelse return .{ .verdict = .no_head, .subsys_len = 0 };
    const serving = try servingControllers(gpa, root, sub);
    defer gpa.free(serving);
    return .{ .verdict = judgeServing(serving, ours, pair), .subsys_len = sub.len };
}

/// Our LIVE controllers of `subsysnqn` under `pair`, straight off
/// `class/nvme` with no allocation (the `.stats` render's law).
pub fn countOursLive(root: []const u8, subsysnqn: []const u8, pair: initiator.HostPair) u64 {
    var walk = initiator.DirWalk.open("{s}/class/nvme", .{root}) catch return 0;
    defer walk.close();
    var n: u64 = 0;
    while (walk.next() catch null) |name| {
        const c = initiator.readController(root, name) orelse continue;
        if (c.isLive() and std.mem.eql(u8, c.subsysnqnOf(), subsysnqn) and c.hasIdentity(pair.hostnqn, pair.hostid)) n += 1;
    }
    return n;
}

// ---------------------------------------------------------------------------
// Per-path nodes and the bytes they carried.
// ---------------------------------------------------------------------------

/// `nvme<K>c<N>n<Y>`: the hidden per-path node of controller N under
/// head K (the strict head shape never matches it, and the reverse).
pub fn splitPathNodeName(name: []const u8) ?struct { head: u32, ctrl: u32, ns: u32 } {
    if (!std.mem.startsWith(u8, name, "nvme")) return null;
    const rest = name["nvme".len..];
    const c_at = std.mem.indexOfScalar(u8, rest, 'c') orelse return null;
    const n_at = std.mem.indexOfScalarPos(u8, rest, c_at + 1, 'n') orelse return null;
    if (c_at == 0 or n_at == c_at + 1 or n_at + 1 >= rest.len) return null;
    for (rest[0..c_at]) |ch| if (ch < '0' or ch > '9') return null;
    for (rest[c_at + 1 .. n_at]) |ch| if (ch < '0' or ch > '9') return null;
    for (rest[n_at + 1 ..]) |ch| if (ch < '0' or ch > '9') return null;
    return .{
        .head = std.fmt.parseInt(u32, rest[0..c_at], 10) catch return null, // decimal, as sysfs names the head
        .ctrl = std.fmt.parseInt(u32, rest[c_at + 1 .. n_at], 10) catch return null, // decimal, the controller
        .ns = std.fmt.parseInt(u32, rest[n_at + 1 ..], 10) catch return null, // decimal, the namespace index
    };
}

/// The namespace node controller N holds (into `out`): the hidden path
/// node on a multipath kernel, the plain `nvme<N>n<Y>` otherwise; null
/// until the controller's scan made one.
pub fn pathNode(gpa: Allocator, root: []const u8, instance: u32, out: *[32]u8) !?[]const u8 {
    const names = initiator.listDir(gpa, "{s}/class/nvme/nvme{d}", .{ root, instance }) catch |e| switch (e) {
        error.FileNotFound => return null,
        else => return e,
    };
    defer configfs.freeNames(gpa, names);
    for (names) |n| {
        if (n.len > out.len) continue;
        const plain = initiator.splitNamespaceName(n);
        const hidden = splitPathNodeName(n);
        if ((plain != null and plain.?.ctrl == instance) or (hidden != null and hidden.?.ctrl == instance)) {
            @memcpy(out[0..n.len], n);
            return out[0..n.len];
        }
    }
    return null;
}

/// A block device's `stat` (Documentation/admin-guide/iostats.rst): the
/// read and write I/O and sector counts — the proof a path carried I/O.
pub const BlockStat = struct {
    read_ios: u64,
    read_sectors: u64,
    write_ios: u64,
    write_sectors: u64,

    pub fn sectors(self: BlockStat) u64 {
        return self.read_sectors + self.write_sectors;
    }
};

/// iostats.rst field order: 1 read I/Os, 2 read merges, 3 read sectors,
/// 4 read ticks, 5 write I/Os, 6 write merges, 7 write sectors, …
pub fn parseBlockStat(text: []const u8) ?BlockStat {
    var it = std.mem.tokenizeAny(u8, text, " \t\r\n");
    var fields: [7]u64 = undefined;
    for (&fields) |*f| f.* = std.fmt.parseInt(u64, it.next() orelse return null, 10) catch return null; // decimal counters
    return .{ .read_ios = fields[0], .read_sectors = fields[2], .write_ios = fields[4], .write_sectors = fields[6] };
}

/// `<root>/class/nvme/nvme<N>/<node>/stat`.
pub fn readNodeStat(root: []const u8, instance: u32, node: []const u8) ?BlockStat {
    var buf: [512]u8 = undefined;
    return parseBlockStat(initiator.readSysfs(&buf, "{s}/class/nvme/nvme{d}/{s}/stat", .{ root, instance, node }) orelse return null);
}

/// `traddr=` and `trsvcid=` of a controller's `address` attribute
/// (`stableEndpoint`'s two keys, split; empty when absent — loop).
pub const AddressParts = struct { traddr: []const u8, trsvcid: []const u8 };

pub fn addressParts(address: []const u8) AddressParts {
    var out: AddressParts = .{ .traddr = "", .trsvcid = "" };
    var it = std.mem.splitScalar(u8, address, ',');
    while (it.next()) |kv| {
        if (std.mem.startsWith(u8, kv, "traddr=")) out.traddr = kv["traddr=".len..];
        if (std.mem.startsWith(u8, kv, "trsvcid=")) out.trsvcid = kv["trsvcid=".len..];
    }
    return out;
}

// ---------------------------------------------------------------------------
// `nvmeof status`: every fabric subsystem of the host, its head, its
// iopolicy, its paths.
// ---------------------------------------------------------------------------

pub fn writeStatus(gpa: Allocator, root: []const u8, w: *Io.Writer, json: bool) !void {
    const mp = readMultipathParam();
    const subs = initiator.listDir(gpa, "{s}/class/nvme-subsystem", .{root}) catch |e| switch (e) {
        error.FileNotFound => try gpa.alloc([]u8, 0),
        else => return e,
    };
    defer configfs.freeNames(gpa, subs);
    if (json) try w.print("{{\"multipath\":\"{s}\",\"subsystems\":[", .{if (mp == .absent) "absent" else mp.text()}) else try w.print("nvme_core.multipath {s}\n", .{mp.text()});
    var first = true;
    var any = false;
    for (subs) |sub| {
        if (!std.mem.startsWith(u8, sub, SUBSYS_PREFIX)) continue;
        const serving = servingControllers(gpa, root, sub) catch continue;
        defer gpa.free(serving);
        var fabric = false;
        for (serving) |*c| fabric = fabric or (c.transport_len != 0 and c.isFabric());
        if (!fabric) continue;
        any = true;
        var nb: [512]u8 = undefined;
        const nqn = initiator.readSysfs(&nb, "{s}/class/nvme-subsystem/{s}/subsysnqn", .{ root, sub }) orelse "";
        var ib: [32]u8 = undefined;
        const policy = readIopolicy(root, sub, &ib) orelse "";
        var hb: [32]u8 = undefined;
        const head = headOf(gpa, root, sub, &hb) catch null;
        if (json) {
            if (!first) try w.writeByte(',');
            first = false;
            try w.print("{{\"subsystem\":\"{s}\",\"subsysnqn\":\"{s}\",\"iopolicy\":\"{s}\",\"head\":", .{ sub, nqn, policy });
            if (head) |h| try w.print("\"/dev/{s}\"", .{h}) else try w.writeAll("null");
            try w.writeAll(",\"paths\":[");
            for (serving, 0..) |*c, i| {
                if (i != 0) try w.writeByte(',');
                const a = addressParts(c.addressOf());
                try w.print("{{\"controller\":\"nvme{d}\",\"instance\":{d},\"transport\":\"{s}\",\"traddr\":\"{s}\",\"trsvcid\":\"{s}\",\"state\":\"{s}\",\"hostnqn\":\"{s}\",\"hostid\":\"{s}\"}}", .{ c.instance, c.instance, c.transportOf(), a.traddr, a.trsvcid, if (c.transport_len == 0) "gone" else c.stateOf(), c.hostnqnOf(), c.hostidOf() });
            }
            try w.writeAll("]}");
        } else {
            try w.print("{s} {s}\n  iopolicy {s}; head {s}; {d} path(s)\n", .{ sub, nqn, if (policy.len == 0) "(none)" else policy, head orelse "(none yet)", serving.len });
            for (serving) |*c| {
                const a = addressParts(c.addressOf());
                try w.print("  nvme{d}: {s} {s}:{s} {s} hostnqn {s} hostid {s}\n", .{ c.instance, c.transportOf(), a.traddr, a.trsvcid, if (c.transport_len == 0) "gone" else c.stateOf(), c.hostnqnOf(), c.hostidOf() });
            }
        }
    }
    if (json) try w.writeAll("]}\n") else if (!any) try w.writeAll("no fabric subsystems (no controller with transport != pcie)\n");
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const testkit = @import("../../core/testkit.zig");
const fuzzkit = @import("../../core/fuzzkit.zig");

test "the iopolicy enum spells the kernel's names and parses each; the knob's default is round-robin (owner ruling 2026-10-18); an outside word is null" {
    try testing.expectEqual(Iopolicy.@"queue-depth", Iopolicy.parse("queue-depth").?);
    try testing.expectEqual(Iopolicy.@"round-robin", Iopolicy.parse("round-robin").?);
    try testing.expectEqual(Iopolicy.numa, Iopolicy.parse("numa").?);
    try testing.expect(Iopolicy.parse("roundrobin") == null);
    try testing.expectEqualStrings("queue-depth", Iopolicy.@"queue-depth".name());
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(Iopolicy.@"round-robin", iopolicyKnob(&env));
    try env.put("SQUEEZEFS_FABRIC_IOPOLICY", "queue-depth");
    try testing.expectEqual(Iopolicy.@"queue-depth", iopolicyKnob(&env));
}

test "the connect timeouts restate the guard's clock (tie): reconnect_delay = the beat, fast_io_fail_tmo = the staleness TTL, ctrl_loss_tmo = 2 × TTL; fail-fast ≤ ctrl_loss; each knob overrides its field verbatim" {
    const d = deriveConnectTimeouts();
    try testing.expectEqual(@as(u32, @intCast(claim.HEARTBEAT_INTERVAL_S)), d.reconnect_delay);
    try testing.expectEqual(@as(u32, @intCast(claim.STALE_TTL_S)), d.fast_io_fail_tmo);
    try testing.expectEqual(@as(u32, @intCast(2 * claim.STALE_TTL_S)), d.ctrl_loss_tmo);
    try testing.expectEqual(@as(u32, 10), d.reconnect_delay);
    try testing.expectEqual(@as(u32, 45), d.fast_io_fail_tmo);
    try testing.expectEqual(@as(u32, 90), d.ctrl_loss_tmo);
    try testing.expect(d.fast_io_fail_tmo <= d.ctrl_loss_tmo);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(d, resolveConnectTimeouts(&env));
    try env.put("SQUEEZEFS_FABRIC_CTRL_LOSS_TMO_S", "300");
    const r = resolveConnectTimeouts(&env);
    try testing.expectEqual(@as(u32, 300), r.ctrl_loss_tmo);
    try testing.expectEqual(d.fast_io_fail_tmo, r.fast_io_fail_tmo);
    try testing.expectEqual(d.reconnect_delay, r.reconnect_delay);
}

test "the serving shape of a head (pure, the metadata device's §5.2 rule 2 per path): two paths of one identity are two fabric paths, not merged, the first standing for the device; a third under another identity is the merged shape; a gone link is an unknown identity (merged); a PCIe controller makes the head local; an empty set has no first" {
    var a: initiator.Controller = .{ .instance = 1 };
    initiator.fill(&a.transport, &a.transport_len, "tcp");
    initiator.fill(&a.hostnqn, &a.hostnqn_len, "nqn.h");
    initiator.fill(&a.hostid, &a.hostid_len, "id");
    var b = a;
    b.instance = 2;
    var other = a;
    other.instance = 3;
    initiator.fill(&other.hostnqn, &other.hostnqn_len, "nqn.other");
    const gone: initiator.Controller = .{ .instance = 4 };
    var pcie: initiator.Controller = .{ .instance = 0 };
    initiator.fill(&pcie.transport, &pcie.transport_len, "pcie");
    const two = servingShape(&.{ a, b });
    try testing.expectEqual(ServingShape{ .fabric_paths = 2, .local = false, .merged = false, .first = 0 }, two);
    try testing.expectEqual(ServingShape{ .fabric_paths = 3, .local = false, .merged = true, .first = 0 }, servingShape(&.{ a, b, other }));
    try testing.expectEqual(ServingShape{ .fabric_paths = 2, .local = false, .merged = true, .first = 0 }, servingShape(&.{ a, gone }));
    try testing.expectEqual(ServingShape{ .fabric_paths = 0, .local = true, .merged = false, .first = null }, servingShape(&.{pcie}));
    try testing.expectEqual(ServingShape{ .fabric_paths = 1, .local = false, .merged = false, .first = 0 }, servingShape(&.{other}));
    try testing.expectEqual(ServingShape{ .fabric_paths = 0, .local = false, .merged = false, .first = null }, servingShape(&.{}));
    // Through the ladder: two of ours accept; the merged shape refuses;
    // a lone foreign path refuses by identity.
    const pair: initiator.HostPair = .{ .hostnqn = "nqn.h", .hostid = "id" };
    const serving = [_]initiator.Controller{ a, b, other };
    const ok = servingShape(serving[0..2]);
    try testing.expectEqual(initiator.Verdict.accept, initiator.fabricLadderVerdict(.{ .plane = .meta, .has_record = false, .actual = .{ .hostnqn = serving[ok.first.?].hostnqnOf(), .hostid = serving[ok.first.?].hostidOf() }, .merged_shape = ok.merged }, pair));
    const mixed = servingShape(&serving);
    try testing.expectEqual(initiator.Verdict.refuse_merged, initiator.fabricLadderVerdict(.{ .plane = .meta, .has_record = false, .actual = .{ .hostnqn = serving[mixed.first.?].hostnqnOf(), .hostid = serving[mixed.first.?].hostidOf() }, .merged_shape = mixed.merged }, pair));
    const lone = servingShape(serving[2..3]);
    try testing.expectEqual(initiator.Verdict.refuse_identity, initiator.fabricLadderVerdict(.{ .plane = .meta, .has_record = false, .actual = .{ .hostnqn = serving[2 + lone.first.?].hostnqnOf(), .hostid = serving[2 + lone.first.?].hostidOf() }, .merged_shape = lone.merged }, pair));
}

test "the exactly-ours verdict (pure): every serving controller ours and every path listed; a foreign identity names its instance; a gone controller (no attributes) is foreign; a path the dir does not list is missing; the order of checks is foreign before missing" {
    const pair: initiator.HostPair = .{ .hostnqn = "nqn.h", .hostid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" };
    var a: initiator.Controller = .{ .instance = 1 };
    initiator.fill(&a.hostnqn, &a.hostnqn_len, pair.hostnqn);
    initiator.fill(&a.hostid, &a.hostid_len, pair.hostid);
    var b = a;
    b.instance = 2;
    var other = a;
    other.instance = 3;
    initiator.fill(&other.hostnqn, &other.hostnqn_len, "nqn.other");
    const gone: initiator.Controller = .{ .instance = 4 };
    try testing.expectEqual(HeadVerdict.exactly_ours, judgeServing(&.{ a, b }, &.{ 1, 2 }, pair));
    try testing.expectEqual(HeadVerdict.exactly_ours, judgeServing(&.{ a, b }, &.{1}, pair));
    try testing.expectEqual(HeadVerdict{ .foreign = 3 }, judgeServing(&.{ a, other }, &.{1}, pair));
    try testing.expectEqual(HeadVerdict{ .foreign = 4 }, judgeServing(&.{ a, gone }, &.{1}, pair));
    try testing.expectEqual(HeadVerdict{ .missing = 2 }, judgeServing(&.{a}, &.{ 1, 2 }, pair));
    try testing.expectEqual(HeadVerdict{ .foreign = 3 }, judgeServing(&.{ a, other }, &.{ 1, 2 }, pair));
    var buf: [512]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try (HeadVerdict{ .foreign = 3 }).write(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "nvme3") != null and std.mem.indexOf(u8, w.buffered(), "fabrics_host_scoped_subsystems=Y") != null);
}

test "name grammars: a controller link, a hidden path node (nvme1c2n1 → head 1, ctrl 2, ns 1) and the strict head shape never match each other; the address attribute splits into traddr/trsvcid (src_addr dropped, loop empty); a block stat parses its 4 counters and refuses a short line" {
    try testing.expectEqual(@as(?u32, 7), controllerLinkInstance("nvme7"));
    try testing.expect(controllerLinkInstance("nvme7n1") == null);
    try testing.expect(controllerLinkInstance("nvme") == null);
    try testing.expect(controllerLinkInstance("subsysnqn") == null);
    const p = splitPathNodeName("nvme1c2n1").?;
    try testing.expectEqual(@as(u32, 1), p.head);
    try testing.expectEqual(@as(u32, 2), p.ctrl);
    try testing.expectEqual(@as(u32, 1), p.ns);
    try testing.expectEqual(@as(u32, 34), splitPathNodeName("nvme12c34n56").?.ctrl);
    try testing.expect(splitPathNodeName("nvme1n1") == null);
    try testing.expect(splitPathNodeName("nvme1c2") == null);
    try testing.expect(splitPathNodeName("nvmec2n1") == null);
    try testing.expect(splitPathNodeName("nvme1cn1") == null);
    try testing.expect(initiator.splitNamespaceName("nvme1c2n1") == null);
    const a = addressParts("traddr=127.0.0.2,trsvcid=4499,src_addr=127.0.0.1");
    try testing.expectEqualStrings("127.0.0.2", a.traddr);
    try testing.expectEqualStrings("4499", a.trsvcid);
    const l = addressParts("");
    try testing.expectEqualStrings("", l.traddr);
    const st = parseBlockStat("      90        0     3880        0       81        0   264192     1442        0      201     1581        0        0        0        0    20003     5275\n").?;
    try testing.expectEqual(@as(u64, 90), st.read_ios);
    try testing.expectEqual(@as(u64, 3880), st.read_sectors);
    try testing.expectEqual(@as(u64, 81), st.write_ios);
    try testing.expectEqual(@as(u64, 264192), st.write_sectors);
    try testing.expectEqual(@as(u64, 268072), st.sectors());
    try testing.expect(parseBlockStat("1 2 3") == null);
    try testing.expect(parseBlockStat("a b c d e f g") == null);
}

test "over a sysfs fixture of a multipath kernel: the subsystem dir of each path, the serving set, the head, the exactly-ours verdict live (two of ours; a third identity makes it foreign; an unlisted path is missing); the hidden path nodes and their stat; iopolicy written and read back (a stale read-back is a mismatch); the live count; the status walk names every path in text and JSON" {
    const gpa = testing.allocator;
    var fx = initiator.Fixture.init(gpa, testing.io, "mpath2") catch return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer fx.deinit();
    const pair: initiator.HostPair = .{ .hostnqn = "nqn.hostA", .hostid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" };
    // 7.2.8-sqz after two tcp connects of one share under one identity:
    // nvme1 (127.0.0.1) and nvme2 (127.0.0.2) linked in nvme-subsys1 with
    // the head nvme1n1; each controller holds its hidden path node.
    try fx.controller(1, "tcp", "live", "nqn.sub", pair.hostnqn, pair.hostid, "traddr=127.0.0.1,trsvcid=4499,src_addr=127.0.0.1");
    try fx.controller(2, "tcp", "live", "nqn.sub", pair.hostnqn, pair.hostid, "traddr=127.0.0.2,trsvcid=4499,src_addr=127.0.0.1");
    try fx.file("class/nvme/nvme1/nvme1c1n1/stat", .{}, "35 0 1288 0 16 0 131072 0 0 0 0 0 0 0 0 0 0\n");
    try fx.file("class/nvme/nvme2/nvme1c2n1/stat", .{}, "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n");
    try fx.file("class/nvme-subsystem/nvme-subsys1/subsysnqn", .{}, "nqn.sub\n");
    try fx.file("class/nvme-subsystem/nvme-subsys1/iopolicy", .{}, "numa\n");
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme1", .{});
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme2", .{});
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme1n1", .{});
    try fx.dir("class/nvme-subsystem/nvme-subsys1/ng1n1", .{});
    // The host's PCIe disk: its own subsystem, never fabric.
    try fx.controller(0, "pcie", "live", "nqn.local", "", "", "0000:c2:00.0");
    try fx.file("class/nvme-subsystem/nvme-subsys0/subsysnqn", .{}, "nqn.local\n");
    try fx.file("class/nvme-subsystem/nvme-subsys0/iopolicy", .{}, "numa\n");
    try fx.dir("class/nvme-subsystem/nvme-subsys0/nvme0", .{});
    var sb: [32]u8 = undefined;
    try testing.expectEqualStrings("nvme-subsys1", (try subsystemOf(gpa, fx.root, 2, &sb)).?);
    try testing.expectEqualStrings("nvme-subsys0", (try subsystemOf(gpa, fx.root, 0, &sb)).?);
    try testing.expect((try subsystemOf(gpa, fx.root, 9, &sb)) == null);
    // The metadata device arrives as a node name: its head's dir.
    try testing.expectEqualStrings("nvme-subsys1", (try subsystemOfNode(gpa, fx.root, "nvme1n1", &sb)).?);
    try testing.expect((try subsystemOfNode(gpa, fx.root, "nvme0n1", &sb)) == null);
    try testing.expect((try subsystemOfNode(gpa, fx.root, "nvme1c1n1", &sb)) == null);
    try testing.expect((try subsystemOfNode(gpa, fx.root, "sda", &sb)) == null);
    try testing.expectEqual(@as(u64, 2), countLiveServing(fx.root, "nvme-subsys1", pair));
    try testing.expectEqual(@as(u64, 2), countLiveServing(fx.root, "nvme-subsys1", null));
    try testing.expectEqual(@as(u64, 0), countLiveServing(fx.root, "nvme-subsys1", .{ .hostnqn = "nqn.hostB", .hostid = "b" }));
    try testing.expectEqual(@as(u64, 0), countLiveServing(fx.root, "nvme-subsys9", null));
    const serving = try servingControllers(gpa, fx.root, "nvme-subsys1");
    defer gpa.free(serving);
    try testing.expectEqual(@as(usize, 2), serving.len);
    var hb: [32]u8 = undefined;
    try testing.expectEqualStrings("nvme1n1", (try headOf(gpa, fx.root, "nvme-subsys1", &hb)).?);
    try testing.expect((try headOf(gpa, fx.root, "nvme-subsys0", &hb)) == null);
    const h = try headVerdict(gpa, fx.root, &.{ 1, 2 }, pair, &sb);
    try testing.expectEqual(HeadVerdict.exactly_ours, h.verdict);
    try testing.expectEqualStrings("nvme-subsys1", sb[0..h.subsys_len]);
    try testing.expectEqual(HeadVerdict{ .missing = 5 }, (try headVerdict(gpa, fx.root, &.{ 1, 5 }, pair, &sb)).verdict);
    const none = try headVerdict(gpa, fx.root, &.{9}, pair, &sb);
    try testing.expectEqual(HeadVerdict.no_head, none.verdict);
    try testing.expectEqual(@as(usize, 0), none.subsys_len);
    // The path nodes and their stat.
    var nb: [32]u8 = undefined;
    try testing.expectEqualStrings("nvme1c2n1", (try pathNode(gpa, fx.root, 2, &nb)).?);
    try testing.expectEqualStrings("nvme1c1n1", (try pathNode(gpa, fx.root, 1, &nb)).?);
    try testing.expect((try pathNode(gpa, fx.root, 0, &nb)) == null);
    try testing.expectEqual(@as(u64, 131072), readNodeStat(fx.root, 1, "nvme1c1n1").?.write_sectors);
    try testing.expectEqual(@as(u64, 0), readNodeStat(fx.root, 2, "nvme1c2n1").?.sectors());
    try testing.expect(readNodeStat(fx.root, 2, "nvme9c9n9") == null);
    // iopolicy: the fixture file takes the write and reads it back.
    var got: [32]u8 = undefined;
    try testing.expectEqual(IopolicyOutcome.set, setIopolicy(fx.root, "nvme-subsys1", .@"queue-depth", &got));
    var ib: [32]u8 = undefined;
    try testing.expectEqualStrings("queue-depth", readIopolicy(fx.root, "nvme-subsys1", &ib).?);
    try testing.expectEqual(IopolicyOutcome.absent, setIopolicy(fx.root, "nvme-subsys7", .numa, &got));
    // A kernel keeping its own policy: the fixture's file is made
    // read-back-stale by a second, longer content the short write cannot
    // replace (the write lands at offset 0, the tail stays).
    try fx.file("class/nvme-subsystem/nvme-subsys0/iopolicy", .{}, "numa-is-what-this-kernel-keeps\n");
    const mm = setIopolicy(fx.root, "nvme-subsys0", .numa, &got);
    try testing.expect(mm == .mismatch);
    var vb: [256]u8 = undefined;
    var vw: Io.Writer = .fixed(&vb);
    try mm.write(&vw, .numa);
    try testing.expect(std.mem.indexOf(u8, vw.buffered(), "stays 'numa-is-what-this-kernel-keeps'") != null);
    // The live count: both ours live; a third identity's controller
    // raises nothing and makes the head foreign.
    try testing.expectEqual(@as(u64, 2), countOursLive(fx.root, "nqn.sub", pair));
    try testing.expectEqual(@as(u64, 0), countOursLive(fx.root, "nqn.sub", .{ .hostnqn = "nqn.hostB", .hostid = "b" }));
    try fx.controller(3, "tcp", "connecting", "nqn.sub", "nqn.hostB", "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "traddr=127.0.0.3,trsvcid=4499");
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme3", .{});
    try testing.expectEqual(HeadVerdict{ .foreign = 3 }, (try headVerdict(gpa, fx.root, &.{ 1, 2 }, pair, &sb)).verdict);
    try testing.expectEqual(@as(u64, 2), countOursLive(fx.root, "nqn.sub", pair));
    // The status walk: the fabric subsystem with its three paths, the
    // PCIe one skipped.
    var out: Io.Writer.Allocating = .init(gpa);
    defer out.deinit();
    try writeStatus(gpa, fx.root, &out.writer, true);
    try testing.expect(std.mem.indexOf(u8, out.written(), "\"subsystem\":\"nvme-subsys1\",\"subsysnqn\":\"nqn.sub\",\"iopolicy\":\"queue-depth\",\"head\":\"/dev/nvme1n1\",\"paths\":[{\"controller\":\"nvme1\",\"instance\":1,\"transport\":\"tcp\",\"traddr\":\"127.0.0.1\",\"trsvcid\":\"4499\",\"state\":\"live\"") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "\"controller\":\"nvme3\",\"instance\":3,\"transport\":\"tcp\",\"traddr\":\"127.0.0.3\",\"trsvcid\":\"4499\",\"state\":\"connecting\",\"hostnqn\":\"nqn.hostB\"") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "nvme-subsys0") == null);
    out.clearRetainingCapacity();
    try writeStatus(gpa, fx.root, &out.writer, false);
    try testing.expect(std.mem.indexOf(u8, out.written(), "nvme-subsys1 nqn.sub\n  iopolicy queue-depth; head nvme1n1; 3 path(s)\n  nvme1: tcp 127.0.0.1:4499 live hostnqn nqn.hostA") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "  nvme3: tcp 127.0.0.3:4499 connecting hostnqn nqn.hostB") != null);
    // A gone controller still linked: foreign, shown as gone.
    try fx.dir("class/nvme-subsystem/nvme-subsys1/nvme4", .{});
    try testing.expectEqual(HeadVerdict{ .foreign = 3 }, (try headVerdict(gpa, fx.root, &.{ 1, 2 }, pair, &sb)).verdict);
    out.clearRetainingCapacity();
    try writeStatus(gpa, fx.root, &out.writer, false);
    try testing.expect(std.mem.indexOf(u8, out.written(), "  nvme4:  : gone") != null);
}

fn sysfsTextFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [256]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    if (Iopolicy.parse(text)) |p| try std.testing.expect(Iopolicy.parse(p.name()) == p);
    _ = parseBlockStat(text);
    _ = splitPathNodeName(text);
    _ = controllerLinkInstance(text);
    _ = addressParts(text);
}

test "fuzz: the iopolicy, block-stat, path-node, controller-link and address parsers are total on arbitrary sysfs text" {
    try fuzzkit.fuzz({}, sysfsTextFuzz, .{ .corpus = &.{ fuzzkit.slice("queue-depth\n"), fuzzkit.slice("35 0 1288 0 16 0 131072 0 0 0 0 0 0 0 0 0 0\n"), fuzzkit.slice("nvme1c2n1"), fuzzkit.slice("traddr=127.0.0.2,trsvcid=4499,src_addr=127.0.0.1") } });
}
