//! The fabrics initiator (charter §3 host-scoped subsystems, §8; `docs/
//! porting/ops_nvmet_keys.md` §3; decision 0003 §G): the connect string
//! written DIRECTLY to `/dev/nvme-fabrics` (no nvme-cli — the venue has
//! none and the product owns its wire), the kernel's `instance=N,cntlid=M`
//! answer read back on the SAME fd (the ONE exact way to learn WHICH
//! controller a connect created — quirk Q7: the reference discarded it and
//! sleep-polled sysfs by `subsysnqn`, which a second association on the
//! same NQN satisfies instantly), the namespace found UNDER
//! `/sys/class/nvme/nvme<N>` and VERIFIED by its `uuid` (never by name),
//! readiness awaited on a `NETLINK_KOBJECT_UEVENT` socket opened BEFORE
//! the write (never a sleep-poll) with the `nvme_core.admin_timeout`
//! deadline (a namespace scan is a sequence of admin commands each bounded
//! by it), the host identity PAIR-OR-NEITHER with a DERIVED default from
//! `/etc/machine-id` (quirk Q8: `/etc/nvme/*` is read-only; the kernel's
//! `nvmf_default_host` is a RANDOM uuid per module load, so an un-named
//! connect would register under a different host identity every boot and
//! the guard's identity-stability stamp would read a change at every
//! reboot's first beat), `nvme_core.fabrics_host_scoped_subsystems` read
//! once and announced, the sysfs discovery over an injectable root (the
//! strict `^nvme\d+n\d+$` shape; a merged multipath head is never handed
//! out), disconnect by `delete_controller` (every controller of the NQN,
//! or `--identity-only` ours), the `fabric_*` sampler (a SAMPLED-transition
//! reconnect counter that undercounts flaps faster than its cadence — by
//! design) and the §5.2 refusal ladder as a pure verdict.
//!
//! On the VM's kernel `CONFIG_NVME_MULTIPATH` is unset: two identities on
//! one subsystem yield two controllers with their own `nvme<N>n1` nodes (the
//! kernel warns "multipathing not supported" and proceeds) — the merged
//! shape cannot arise and `fabrics_host_scoped_subsystems=N` is inert; on
//! a multipath kernel (a stock distro's, kamrui's) the controller holds only
//! a hidden path node and the namespace is its subsystem's head, handed out
//! only while every serving controller is ours (`controllerNamespace`); with
//! `N` two per-mount identities would merge under one head whose round-robin
//! voids per-mount fencing, which is what the announcement at the first
//! connect says.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const errors = @import("../../core/errors.zig");
const checksum = @import("../../core/checksum.zig");
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const pr = @import("../../meta/guard/pr.zig");
const proc = @import("../../meta/guard/proc.zig");
const configfs = @import("configfs.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/initiator is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const FABRICS_DEV = "/dev/nvme-fabrics";
pub const SYSFS_ROOT = "/sys";
pub const MACHINE_ID_PATH = "/etc/machine-id";
pub const HOST_SCOPED_PARAM = "/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems";
pub const ADMIN_TIMEOUT_PARAM = "/sys/module/nvme_core/parameters/admin_timeout";
/// The kernel's own default when the parameter is unreadable.
pub const ADMIN_TIMEOUT_DEFAULT_S: u64 = 60;
/// `nvmf_hostnqn_generate`'s prefix — the same family libnvme derives.
pub const HOSTNQN_UUID_PREFIX = "nqn.2014-08.org.nvmexpress:uuid:";
/// `delete_controller` returns before the controller dir vanishes; the
/// disconnect verb waits for the dir with the same deadline class as a
/// connect (the teardown is the mirror of the admin sequence).
pub const DISCONNECT_WAIT_S: u64 = 30;

pub const Trtype = configfs.Trtype;

// ---------------------------------------------------------------------------
// §3.1 The connect string and the read-back.
// ---------------------------------------------------------------------------

pub const ConnectSpec = struct {
    trtype: Trtype,
    /// Empty on loop (`traddr` optional: the first loop port).
    traddr: []const u8 = "",
    trsvcid: u16 = configfs.DEFAULT_TRSVCID,
    subnqn: []const u8,
    hostnqn: []const u8,
    hostid: []const u8,
    host_traddr: ?[]const u8 = null,
    host_iface: ?[]const u8 = null,
    /// 0 refuses BEFORE any side effect (I:924-930).
    nr_io_queues: ?u32 = null,

    /// The exact option string (`fabrics_connect_string` I:846-868 + the
    /// loop shape): an all-absent option set is byte-identical to the base.
    pub fn write(self: ConnectSpec, w: *Io.Writer) Io.Writer.Error!void {
        switch (self.trtype) {
            .tcp => try w.print("transport=tcp,traddr={s},trsvcid={d},nqn={s},hostnqn={s},hostid={s}", .{ self.traddr, self.trsvcid, self.subnqn, self.hostnqn, self.hostid }),
            .loop => {
                try w.print("transport=loop,nqn={s},hostnqn={s},hostid={s}", .{ self.subnqn, self.hostnqn, self.hostid });
                if (self.traddr.len != 0) try w.print(",traddr={s}", .{self.traddr});
            },
        }
        if (self.host_traddr) |h| try w.print(",host_traddr={s}", .{h});
        if (self.host_iface) |i| try w.print(",host_iface={s}", .{i});
        if (self.nr_io_queues) |n| try w.print(",nr_io_queues={d}", .{n});
    }

    pub fn text(self: ConnectSpec, buf: []u8) []const u8 {
        var w: Io.Writer = .fixed(buf);
        self.write(&w) catch unreachable;
        return w.buffered();
    }
};

pub const ReadBack = struct { instance: u32, cntlid: u16 };

/// `instance=%d,cntlid=%d\n` (`nvmf_dev_read`).
pub fn parseReadBack(text: []const u8) ?ReadBack {
    const t = std.mem.trim(u8, text, " \r\n\x00");
    var it = std.mem.splitScalar(u8, t, ',');
    var inst: ?u32 = null;
    var cnt: ?u16 = null;
    while (it.next()) |kv| {
        const eq = std.mem.indexOfScalar(u8, kv, '=') orelse return null;
        const k = kv[0..eq];
        const v = kv[eq + 1 ..];
        if (std.mem.eql(u8, k, "instance")) {
            inst = std.fmt.parseInt(u32, v, 10) catch return null;
        } else if (std.mem.eql(u8, k, "cntlid")) {
            cnt = std.fmt.parseInt(u16, v, 10) catch return null;
        } else return null;
    }
    return .{ .instance = inst orelse return null, .cntlid = cnt orelse return null };
}

/// The write's errno table (§3.1), typed through the ONE table.
pub const ConnectError = error{
    /// `/dev/nvme-fabrics` absent: `CONFIG_NVME_FABRICS`/`CONFIG_NVME_TCP`.
    FabricsDeviceAbsent,
    /// A malformed option or an unknown transport after autoload (EINVAL).
    InvalidOptions,
    /// EADDRNOTAVAIL / ECONNREFUSED / EHOSTUNREACH / ENETUNREACH.
    Network,
    /// An identical `(transport, traddr, trsvcid, nqn, hostnqn)` controller
    /// exists (EALREADY, `nvmf_ip_options_match`).
    AlreadyConnected,
    /// The target refused the subsystem or the host (ENOENT / EIO: the
    /// allow-list).
    TargetRefused,
    Busy,
    /// `nr_io_queues=0`.
    ZeroQueues,
    /// The kernel's answer did not parse.
    ReadBackMalformed,
    /// No namespace materialized under the controller before the deadline.
    NamespaceTimeout,
    /// The namespace's uuid is not the expected one (a renumbered node
    /// binding the wrong disk — decision 0002 row 125).
    UuidMismatch,
    /// The controller dropped to a dead state during the scan.
    ControllerDead,
    /// The uevent socket could not be opened (CAP_NET_ADMIN is not needed
    /// for the uevent group; ENOBUFS-class failures are).
    UeventSocket,
    Unexpected,
    OutOfMemory,
};

pub fn writeConnectRefusal(w: *Io.Writer, e: ConnectError, spec: ConnectSpec) Io.Writer.Error!void {
    switch (e) {
        error.FabricsDeviceAbsent => try w.print("{s} is absent: the kernel has no NVMe over Fabrics host (CONFIG_NVME_FABRICS / CONFIG_NVME_TCP / CONFIG_NVME_TARGET_LOOP)", .{FABRICS_DEV}),
        error.InvalidOptions => try w.print("the kernel refused the connect options (EINVAL): transport '{s}' is not loaded or an option is malformed — check `dmesg`", .{spec.trtype.name()}),
        error.Network => try w.print("no route to the target at {s}:{d} (EADDRNOTAVAIL/ECONNREFUSED/EHOSTUNREACH) — is the share listening there? (`squeezefs nvmeof list`)", .{ spec.traddr, spec.trsvcid }),
        error.AlreadyConnected => try w.print("a controller for {s} at {s}:{d} under hostnqn {s} already exists (EALREADY) — `squeezefs nvmeof disconnect {s}` first, or connect under another identity pair", .{ spec.subnqn, spec.traddr, spec.trsvcid, spec.hostnqn, spec.subnqn }),
        error.TargetRefused => try w.print("the target refused {s} for host {s} (ENOENT/EIO): the subsystem is not served there or the host is not on its allow-list", .{ spec.subnqn, spec.hostnqn }),
        error.Busy => try w.writeAll("the fabrics device is busy (EBUSY) — retry"),
        error.ZeroQueues => try w.writeAll("--nr-io-queues 0 asks for no I/O queue: refused before any side effect (1 is the minimum)"),
        error.ReadBackMalformed => try w.print("{s} answered something other than `instance=N,cntlid=M` — a kernel this binary does not know", .{FABRICS_DEV}),
        error.NamespaceTimeout => try w.writeAll("the controller connected but no namespace appeared under it within nvme_core.admin_timeout — the target serves no enabled namespace to this host"),
        error.UuidMismatch => try w.writeAll("the namespace that appeared carries a uuid other than the recorded one — refusing to hand out a disk that is not the share's (never a guessed coordinate)"),
        error.ControllerDead => try w.writeAll("the controller died during its namespace scan (see `dmesg`)"),
        error.UeventSocket => try w.writeAll("the NETLINK_KOBJECT_UEVENT socket could not be opened — the connect's readiness wait has no event source"),
        error.Unexpected => try w.writeAll("an errno outside the connect table (see the log)"),
        error.OutOfMemory => try w.writeAll("out of memory"),
    }
}

/// The write's errno through the ONE table, folded into the connect classes.
fn connectErrnoClass(e: linux.E) ConnectError {
    return switch (errors.fromErrno(e)) {
        error.InvalidArgument => error.InvalidOptions,
        error.AddressNotAvailable, error.ConnectionRefused, error.HostUnreachable => error.Network,
        error.AlreadyConnected => error.AlreadyConnected,
        error.FileNotFound, error.InputOutput => error.TargetRefused,
        error.Busy => error.Busy,
        error.OutOfMemory => error.OutOfMemory,
        else => error.Unexpected,
    };
}

// ---------------------------------------------------------------------------
// §3.2 Identity: pair-or-neither; /etc/nvme both-or-neither; the derived
// default from the machine id (never written anywhere).
// ---------------------------------------------------------------------------

pub const IdentitySource = enum(u8) {
    explicit = 0,
    etc_nvme = 1,
    machine_id = 2,
};

pub const Identity = struct {
    hostnqn_buf: [256]u8 = undefined,
    hostnqn_len: usize = 0,
    hostid_buf: [64]u8 = undefined,
    hostid_len: usize = 0,
    source: IdentitySource,

    pub fn hostnqn(self: *const Identity) []const u8 {
        return self.hostnqn_buf[0..self.hostnqn_len];
    }

    pub fn hostid(self: *const Identity) []const u8 {
        return self.hostid_buf[0..self.hostid_len];
    }

    fn of(nqn: []const u8, id: []const u8, source: IdentitySource) Identity {
        var out: Identity = .{ .source = source };
        @memcpy(out.hostnqn_buf[0..nqn.len], nqn);
        out.hostnqn_len = nqn.len;
        @memcpy(out.hostid_buf[0..id.len], id);
        out.hostid_len = id.len;
        return out;
    }
};

pub const IdentityRefusal = union(enum) {
    /// Exactly one of the pair configured.
    pair: pr.HostIdentityRefusal,
    /// Exactly one of `/etc/nvme/{hostnqn,hostid}` present.
    etc_half: enum { nqn_only, id_only },
    /// A hostid that is not a uuid (the kernel's `hostid=` wants one) or
    /// an NQN past the grammar.
    malformed: enum { hostid, hostnqn },
    /// No explicit pair, no `/etc/nvme` pair, no `/etc/machine-id`.
    no_source,

    pub fn write(self: IdentityRefusal, w: *Io.Writer) Io.Writer.Error!void {
        switch (self) {
            .pair => |p| try p.write(w),
            .etc_half => |h| try w.print("/etc/nvme/{s} is present without /etc/nvme/{s}: the pair law applies to the files too (libnvme would mint the missing half and alias registrants) — provide both or neither", .{ if (h == .nqn_only) "hostnqn" else "hostid", if (h == .nqn_only) "hostid" else "hostnqn" }),
            .malformed => |m| switch (m) {
                .hostid => try w.writeAll("hostid is not a uuid (8-4-4-4-12 hex): the kernel's `hostid=` option refuses it"),
                .hostnqn => try w.print("hostnqn is not an NQN (nqn.…, ≤ {d} bytes, no whitespace)", .{configfs.NVMF_NQN_SIZE}),
            },
            .no_source => try w.print("no host identity: set SQUEEZEFS_HOSTNQN + SQUEEZEFS_HOSTID (pair), or provide /etc/nvme/hostnqn + /etc/nvme/hostid (both), or give the host an {s}", .{MACHINE_ID_PATH}),
        }
    }
};

pub const Resolved = union(enum) {
    identity: Identity,
    refusal: IdentityRefusal,
};

/// `uuid(sha256("squeezefs:nvme-hostid:" ‖ machine_id)[0..16])` with the
/// RFC 4122 version 4 / variant 10 bits set: deterministic per host,
/// never written anywhere.
pub fn deriveHostId(machine_id: []const u8) [16]u8 {
    var h = std.crypto.hash.sha2.Sha256.init(.{});
    h.update("squeezefs:nvme-hostid:");
    h.update(machine_id);
    var digest: [32]u8 = undefined;
    h.final(&digest);
    var out: [16]u8 = digest[0..16].*;
    out[6] = (out[6] & 0x0f) | 0x40;
    out[8] = (out[8] & 0x3f) | 0x80;
    return out;
}

fn checkPair(nqn: []const u8, id: []const u8) ?IdentityRefusal {
    if (proc.parseUuid(id) == null or id.len > 63) return .{ .malformed = .hostid };
    configfs.validateNqn(nqn) catch return .{ .malformed = .hostnqn };
    return null;
}

/// The explicit pair → `/etc/nvme` both → the machine-id derivation.
/// `machine_id` is the file's content (null = absent).
pub fn resolveIdentity(env: *const std.process.Environ.Map, opts: pr.IdentityOptions, etc: *const proc.EtcNvme, machine_id: ?[]const u8) Resolved {
    switch (pr.resolveExplicitIdentity(env, opts)) {
        .refusal => |r| return .{ .refusal = .{ .pair = r } },
        .pair => |p| {
            if (checkPair(p.hostnqn, p.hostid)) |r| return .{ .refusal = r };
            return .{ .identity = Identity.of(p.hostnqn, p.hostid, .explicit) };
        },
        .none => {},
    }
    const etc_nqn = etc.hostnqn();
    const etc_id = etc.hostid();
    if (etc_nqn.len != 0 and etc_id.len != 0) {
        if (checkPair(etc_nqn, etc_id)) |r| return .{ .refusal = r };
        return .{ .identity = Identity.of(etc_nqn, etc_id, .etc_nvme) };
    }
    if (etc_nqn.len != 0) return .{ .refusal = .{ .etc_half = .nqn_only } };
    if (etc_id.len != 0) return .{ .refusal = .{ .etc_half = .id_only } };
    const mid = std.mem.trim(u8, machine_id orelse "", " \t\r\n");
    if (mid.len == 0) return .{ .refusal = .no_source };
    var out: Identity = .{ .source = .machine_id };
    var w: Io.Writer = .fixed(&out.hostid_buf);
    configfs.writeUuid(&w, deriveHostId(mid)) catch unreachable;
    out.hostid_len = w.buffered().len;
    const n = std.fmt.bufPrint(&out.hostnqn_buf, "{s}{s}", .{ HOSTNQN_UUID_PREFIX, out.hostid() }) catch unreachable;
    out.hostnqn_len = n.len;
    return .{ .identity = out };
}

/// The host's resolution with the real sources (`/etc/nvme`, the machine
/// id); the source is published on `fabric_host_identity_source`.
pub fn resolveHostIdentity(env: *const std.process.Environ.Map, opts: pr.IdentityOptions) Resolved {
    const etc = proc.EtcNvme.read();
    var mb: [128]u8 = undefined;
    const mid = proc.readSmall(MACHINE_ID_PATH, &mb);
    const r = resolveIdentity(env, opts, &etc, mid);
    if (r == .identity) stats.fabric_host_identity_source.set(@backingInt(r.identity.source));
    return r;
}

// ---------------------------------------------------------------------------
// §3.3 `nvme_core.fabrics_host_scoped_subsystems`.
// ---------------------------------------------------------------------------

pub const HostScoped = enum(u8) {
    off = 0,
    on = 1,
    /// A stock kernel without the sqz patch.
    absent = 2,

    pub fn announce(self: HostScoped) []const u8 {
        return switch (self) {
            .on => "host-scoped fabric subsystems: ON (co-located per-mount identities on this host get their own subsystem heads)",
            .off => "host-scoped fabric subsystems: OFF (nvme_core.fabrics_host_scoped_subsystems=N — on a multipath kernel two per-mount identities on one subsystem would MERGE under one head whose round-robin voids per-mount fencing; this kernel has no multipath, so each identity gets its own nvme<N>n<Y> and the merge cannot arise)",
            .absent => "host-scoped fabric subsystems: stock kernel (no nvme_core.fabrics_host_scoped_subsystems) — co-located per-mount identities merge under one multipath head unless nvme_core.multipath=N",
        };
    }
};

/// Published on `fabric_host_scoped` on every path — a stock kernel's
/// `absent` too, never left reading the gauge's zero (`off`).
pub fn readHostScoped() HostScoped {
    var buf: [16]u8 = undefined;
    const hs: HostScoped = if (proc.readSmall(HOST_SCOPED_PARAM, &buf)) |t|
        (if (std.mem.eql(u8, std.mem.trim(u8, t, " \t\r\n"), "Y")) .on else .off)
    else
        .absent;
    stats.fabric_host_scoped.set(@backingInt(hs));
    return hs;
}

pub fn adminTimeoutS() u64 {
    var buf: [32]u8 = undefined;
    const t = proc.readSmall(ADMIN_TIMEOUT_PARAM, &buf) orelse return ADMIN_TIMEOUT_DEFAULT_S;
    return std.fmt.parseInt(u64, std.mem.trim(u8, t, " \t\r\n"), 10) catch ADMIN_TIMEOUT_DEFAULT_S;
}

// ---------------------------------------------------------------------------
// §3.4 Discovery over sysfs (an injectable root for the fixture tests).
// ---------------------------------------------------------------------------

/// Strict `^nvme\d+n\d+$` (F:157-163): hidden path nodes `nvme<X>c<C>n<Y>`
/// and partitions never match.
pub fn splitNamespaceName(name: []const u8) ?struct { ctrl: u32, ns: u32 } {
    if (!std.mem.startsWith(u8, name, "nvme")) return null;
    const rest = name[4..];
    const n_at = std.mem.indexOfScalar(u8, rest, 'n') orelse return null;
    if (n_at == 0 or n_at + 1 >= rest.len) return null;
    const ctrl = std.fmt.parseInt(u32, rest[0..n_at], 10) catch return null;
    const ns = std.fmt.parseInt(u32, rest[n_at + 1 ..], 10) catch return null;
    for (rest[0..n_at]) |c| if (c < '0' or c > '9') return null;
    for (rest[n_at + 1 ..]) |c| if (c < '0' or c > '9') return null;
    return .{ .ctrl = ctrl, .ns = ns };
}

fn readSysfs(buf: []u8, comptime fmt: []const u8, args: anytype) ?[]const u8 {
    var pb: [linux.PATH_MAX]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, fmt, args, 0) catch return null;
    const t = proc.readSmall(path.ptr, buf) orelse return null;
    return std.mem.trim(u8, t, " \t\r\n");
}

/// A directory's entries straight off `getdents64` into a stack buffer:
/// no allocation, directory order, `.`/`..` skipped. The fabric sampler
/// walks `class/nvme` through it (the `.stats` render's one-allocation
/// law); `listDir` sorts a duped copy for the callers that keep names.
const DirWalk = struct {
    fd: linux.fd_t,
    buf: [8192]u8 = undefined,
    len: usize = 0,
    off: usize = 0,

    fn open(comptime fmt: []const u8, args: anytype) !DirWalk {
        var pb: [linux.PATH_MAX]u8 = undefined;
        const path = std.fmt.bufPrintSentinel(&pb, fmt, args, 0) catch return error.NameTooLong;
        const rc = linux.open(path.ptr, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return errors.fromErrno(linux.errno(rc));
        return .{ .fd = @intCast(rc) };
    }

    fn close(self: *DirWalk) void {
        _ = linux.close(self.fd);
    }

    /// The next entry's name (valid until the next call), null at the end.
    fn next(self: *DirWalk) !?[]const u8 {
        while (true) {
            if (self.off >= self.len) {
                const n = linux.getdents64(self.fd, &self.buf, self.buf.len);
                if (linux.errno(n) != .SUCCESS) return errors.fromErrno(linux.errno(n));
                if (n == 0) return null;
                self.len = n;
                self.off = 0;
            }
            const d: *align(1) linux.dirent64 = @ptrCast(&self.buf[self.off]);
            const name = std.mem.sliceTo(@as([*:0]u8, @ptrCast(&d.name)), 0);
            self.off += d.reclen;
            if (std.mem.eql(u8, name, ".") or std.mem.eql(u8, name, "..")) continue;
            return name;
        }
    }
};

fn listDir(gpa: Allocator, comptime fmt: []const u8, args: anytype) ![][]u8 {
    var walk = try DirWalk.open(fmt, args);
    defer walk.close();
    var out: std.ArrayList([]u8) = .empty;
    errdefer configfs.freeNames(gpa, out.toOwnedSlice(gpa) catch &.{});
    while (try walk.next()) |name| try out.append(gpa, try gpa.dupe(u8, name));
    std.mem.sort([]u8, out.items, {}, struct {
        fn lt(_: void, a: []u8, b: []u8) bool {
            return std.mem.lessThan(u8, a, b);
        }
    }.lt);
    return out.toOwnedSlice(gpa);
}

/// A controller's identity attrs (`/sys/class/nvme/nvme<N>/*`).
pub const Controller = struct {
    name: [16]u8 = undefined,
    name_len: usize = 0,
    instance: u32,
    transport: [16]u8 = undefined,
    transport_len: usize = 0,
    state: [16]u8 = undefined,
    state_len: usize = 0,
    subsysnqn: [256]u8 = undefined,
    subsysnqn_len: usize = 0,
    hostnqn: [256]u8 = undefined,
    hostnqn_len: usize = 0,
    hostid: [64]u8 = undefined,
    hostid_len: usize = 0,
    address: [256]u8 = undefined,
    address_len: usize = 0,

    pub fn nameOf(self: *const Controller) []const u8 {
        return self.name[0..self.name_len];
    }
    pub fn transportOf(self: *const Controller) []const u8 {
        return self.transport[0..self.transport_len];
    }
    pub fn stateOf(self: *const Controller) []const u8 {
        return self.state[0..self.state_len];
    }
    pub fn subsysnqnOf(self: *const Controller) []const u8 {
        return self.subsysnqn[0..self.subsysnqn_len];
    }
    pub fn hostnqnOf(self: *const Controller) []const u8 {
        return self.hostnqn[0..self.hostnqn_len];
    }
    pub fn hostidOf(self: *const Controller) []const u8 {
        return self.hostid[0..self.hostid_len];
    }
    pub fn addressOf(self: *const Controller) []const u8 {
        return self.address[0..self.address_len];
    }

    pub fn isFabric(self: *const Controller) bool {
        return !std.mem.eql(u8, self.transportOf(), "pcie");
    }

    /// `state == live`; a missing state file reads as not live (F:86-90).
    pub fn isLive(self: *const Controller) bool {
        return std.mem.eql(u8, self.stateOf(), "live");
    }

    pub fn hasIdentity(self: *const Controller, hostnqn: []const u8, hostid: []const u8) bool {
        return std.mem.eql(u8, self.hostnqnOf(), hostnqn) and std.mem.eql(u8, self.hostidOf(), hostid);
    }
};

fn fill(dst: []u8, len: *usize, src: ?[]const u8) void {
    const s = src orelse "";
    const n = @min(s.len, dst.len);
    @memcpy(dst[0..n], s[0..n]);
    len.* = n;
}

/// `/sys/class/nvme/<name>` → its attrs; null when the dir is gone.
pub fn readController(root: []const u8, name: []const u8) ?Controller {
    const instance = std.fmt.parseInt(u32, if (std.mem.startsWith(u8, name, "nvme")) name[4..] else return null, 10) catch return null;
    var c: Controller = .{ .instance = instance };
    fill(&c.name, &c.name_len, name);
    var b: [512]u8 = undefined;
    const tr = readSysfs(&b, "{s}/class/nvme/{s}/transport", .{ root, name }) orelse return null;
    fill(&c.transport, &c.transport_len, tr);
    fill(&c.state, &c.state_len, readSysfs(&b, "{s}/class/nvme/{s}/state", .{ root, name }) orelse "unknown");
    fill(&c.subsysnqn, &c.subsysnqn_len, readSysfs(&b, "{s}/class/nvme/{s}/subsysnqn", .{ root, name }));
    fill(&c.hostnqn, &c.hostnqn_len, readSysfs(&b, "{s}/class/nvme/{s}/hostnqn", .{ root, name }));
    fill(&c.hostid, &c.hostid_len, readSysfs(&b, "{s}/class/nvme/{s}/hostid", .{ root, name }));
    fill(&c.address, &c.address_len, readSysfs(&b, "{s}/class/nvme/{s}/address", .{ root, name }));
    return c;
}

/// Every controller under `<root>/class/nvme` (caller frees the slice).
pub fn listControllers(gpa: Allocator, root: []const u8) ![]Controller {
    const names = listDir(gpa, "{s}/class/nvme", .{root}) catch |e| switch (e) {
        error.FileNotFound => return try gpa.alloc(Controller, 0),
        else => return e,
    };
    defer configfs.freeNames(gpa, names);
    var out: std.ArrayList(Controller) = .empty;
    errdefer out.deinit(gpa);
    for (names) |n| if (readController(root, n)) |c| try out.append(gpa, c);
    return out.toOwnedSlice(gpa);
}

/// A namespace node found under a controller: the block node, its char
/// twin, nsid, uuid.
pub const Namespace = struct {
    block: [32]u8 = undefined,
    block_len: usize = 0,
    char: [32]u8 = undefined,
    char_len: usize = 0,
    nsid: u32,
    uuid: ?[16]u8,
    /// `/sys/class/nvme-generic/<ng>/dev` resolved.
    char_present: bool,

    pub fn blockName(self: *const Namespace) []const u8 {
        return self.block[0..self.block_len];
    }
    pub fn charName(self: *const Namespace) []const u8 {
        return self.char[0..self.char_len];
    }
    pub fn blockPath(self: *const Namespace, buf: *[40]u8) []const u8 {
        return std.fmt.bufPrint(buf, "/dev/{s}", .{self.blockName()}) catch unreachable;
    }
    pub fn charPath(self: *const Namespace, buf: *[40]u8) []const u8 {
        return std.fmt.bufPrint(buf, "/dev/{s}", .{self.charName()}) catch unreachable;
    }
};

/// The namespace controller `nvme<N>` serves, with its facts (spec §3.4):
/// (a) the FIRST strict-shape `nvme<N>n<Y>` child of `/sys/class/nvme/nvme<N>`
/// — the per-controller node of a non-multipath kernel (the VM's); else
/// (b) on a multipath kernel the head of the subsystem dir listing it
/// (`multipathHead`). Honest null: a name is never fabricated.
pub fn controllerNamespace(gpa: Allocator, root: []const u8, instance: u32) !?Namespace {
    const names = listDir(gpa, "{s}/class/nvme/nvme{d}", .{ root, instance }) catch |e| switch (e) {
        error.FileNotFound => return null,
        else => return e,
    };
    defer configfs.freeNames(gpa, names);
    for (names) |n| {
        const parts = splitNamespaceName(n) orelse continue;
        if (parts.ctrl != instance) continue;
        return namespaceFacts(root, n, parts.ns);
    }
    return multipathHead(gpa, root, instance);
}

/// (b) A multipath kernel (`CONFIG_NVME_MULTIPATH=y` with
/// `nvme_core.multipath=Y`, a stock distro's default): the controller holds
/// only the hidden path node `nvme<K>c<N>n<Y>`; the namespace is the head
/// `nvme<K>n<Y>` of the `nvme-subsys<K>` dir that lists `nvme<N>` — offered
/// only when every serving controller there carries controller N's identity
/// (a head shared with another identity round-robins across per-mount
/// fences: the merged shape, never handed out).
fn multipathHead(gpa: Allocator, root: []const u8, instance: u32) !?Namespace {
    var nb: [16]u8 = undefined;
    const ctrl_name = std.fmt.bufPrint(&nb, "nvme{d}", .{instance}) catch unreachable;
    const own = readController(root, ctrl_name) orelse return null;
    const subs = listDir(gpa, "{s}/class/nvme-subsystem", .{root}) catch |e| switch (e) {
        error.FileNotFound => return null,
        else => return e,
    };
    defer configfs.freeNames(gpa, subs);
    const prefix = "nvme-subsys";
    for (subs) |sub| {
        if (!std.mem.startsWith(u8, sub, prefix)) continue;
        const k = std.fmt.parseInt(u32, sub[prefix.len..], 10) catch continue; // decimal, as sysfs names the subsystem instance
        const children = listDir(gpa, "{s}/class/nvme-subsystem/{s}", .{ root, sub }) catch continue;
        defer configfs.freeNames(gpa, children);
        var listed = false;
        for (children) |ch| listed = listed or std.mem.eql(u8, ch, ctrl_name);
        if (!listed) continue;
        var head: ?usize = null;
        for (children, 0..) |ch, i| {
            if (splitNamespaceName(ch)) |parts| {
                if (parts.ctrl == k and head == null) head = i;
                continue;
            }
            if (!std.mem.startsWith(u8, ch, "nvme")) continue;
            _ = std.fmt.parseInt(u32, ch["nvme".len..], 10) catch continue; // decimal, as sysfs names the controller link
            // A serving controller not provably ours (gone, or another
            // identity) makes the head shared: never handed out.
            const c = readController(root, ch) orelse return null;
            if (!c.hasIdentity(own.hostnqnOf(), own.hostidOf())) return null;
        }
        const h = head orelse return null;
        return namespaceFacts(root, children[h], splitNamespaceName(children[h]).?.ns);
    }
    return null;
}

fn namespaceFacts(root: []const u8, block_name: []const u8, ns_index: u32) ?Namespace {
    var out: Namespace = .{ .nsid = 0, .uuid = null, .char_present = false };
    fill(&out.block, &out.block_len, block_name);
    var b: [128]u8 = undefined;
    out.nsid = std.fmt.parseInt(u32, readSysfs(&b, "{s}/class/block/{s}/nsid", .{ root, block_name }) orelse "0", 10) catch 0;
    if (out.nsid == 0) out.nsid = ns_index;
    out.uuid = configfs.parseUuidAttr(readSysfs(&b, "{s}/class/block/{s}/uuid", .{ root, block_name }) orelse "");
    var cb: [32]u8 = undefined;
    const ng = charTwinName(block_name, &cb) orelse return out;
    fill(&out.char, &out.char_len, ng);
    out.char_present = readSysfs(&b, "{s}/class/nvme-generic/{s}/dev", .{ root, ng }) != null;
    return out;
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

/// The multipath MERGED shape (I:450-491): a `nvme-subsystem` dir carrying
/// a strict-shape namespace whose serving controllers (the `nvme<N>`
/// entries the kernel linked into the dir) carry > 1 distinct hostnqn.
/// Serving membership is DIR-scoped; a link-less dir falls back to the
/// global `subsysnqn` match.
pub fn multipathMergedShape(gpa: Allocator, root: []const u8, subsysnqn: []const u8) !bool {
    const subs = listDir(gpa, "{s}/class/nvme-subsystem", .{root}) catch |e| switch (e) {
        error.FileNotFound => return false,
        else => return e,
    };
    defer configfs.freeNames(gpa, subs);
    for (subs) |sub| {
        var b: [512]u8 = undefined;
        const nqn = readSysfs(&b, "{s}/class/nvme-subsystem/{s}/subsysnqn", .{ root, sub }) orelse continue;
        if (!std.mem.eql(u8, nqn, subsysnqn)) continue;
        const children = listDir(gpa, "{s}/class/nvme-subsystem/{s}", .{ root, sub }) catch continue;
        defer configfs.freeNames(gpa, children);
        var has_head = false;
        var first_host: ?[256]u8 = null;
        var first_len: usize = 0;
        var distinct: u32 = 0;
        var any_ctrl = false;
        for (children) |ch| {
            if (splitNamespaceName(ch) != null) {
                has_head = true;
                continue;
            }
            if (!std.mem.startsWith(u8, ch, "nvme")) continue;
            _ = std.fmt.parseInt(u32, ch[4..], 10) catch continue;
            any_ctrl = true;
            const c = readController(root, ch) orelse continue;
            if (first_host == null) {
                first_host = c.hostnqn;
                first_len = c.hostnqn_len;
                distinct = 1;
            } else if (!std.mem.eql(u8, first_host.?[0..first_len], c.hostnqnOf())) distinct += 1;
        }
        if (!any_ctrl) {
            // Global fallback: every controller of the NQN.
            const ctrls = try listControllers(gpa, root);
            defer gpa.free(ctrls);
            for (ctrls) |c| {
                if (!std.mem.eql(u8, c.subsysnqnOf(), subsysnqn)) continue;
                if (first_host == null) {
                    first_host = c.hostnqn;
                    first_len = c.hostnqn_len;
                    distinct = 1;
                } else if (!std.mem.eql(u8, first_host.?[0..first_len], c.hostnqnOf())) distinct += 1;
            }
        }
        if (has_head and distinct > 1) return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// The uevent wait: a NETLINK_KOBJECT_UEVENT socket opened BEFORE the
// connect write; every event re-checks sysfs; the deadline bounds it.
// ---------------------------------------------------------------------------

pub const UeventSocket = struct {
    fd: linux.fd_t,

    pub fn open() ConnectError!UeventSocket {
        const rc = linux.socket(linux.AF.NETLINK, linux.SOCK.DGRAM | linux.SOCK.CLOEXEC | linux.SOCK.NONBLOCK, linux.NETLINK.KOBJECT_UEVENT);
        if (linux.errno(rc) != .SUCCESS) return error.UeventSocket;
        const fd: linux.fd_t = @intCast(rc);
        errdefer _ = linux.close(fd);
        // Group 1 = the kernel's uevent multicast group.
        var addr: linux.sockaddr.nl = .{ .pid = 0, .groups = 1 };
        if (linux.errno(linux.bind(fd, @ptrCast(&addr), @sizeOf(linux.sockaddr.nl))) != .SUCCESS) return error.UeventSocket;
        return .{ .fd = fd };
    }

    pub fn close(self: *UeventSocket) void {
        _ = linux.close(self.fd);
    }

    /// Park until an event arrives or `deadline_ns`; drains what arrived.
    /// Returns false at the deadline.
    pub fn waitEvent(self: *UeventSocket, deadline_ns: u64) bool {
        const now = clock.monotonicNs();
        if (now >= deadline_ns) return false;
        const left_ms: i32 = @intCast(@min((deadline_ns - now) / std.time.ns_per_ms + 1, std.math.maxInt(i32)));
        var pfd = [_]linux.pollfd{.{ .fd = self.fd, .events = linux.POLL.IN }};
        const rc = linux.poll(&pfd, 1, left_ms);
        if (linux.errno(rc) != .SUCCESS or rc == 0) return false;
        var buf: [8192]u8 = undefined;
        while (true) {
            const n = linux.recvfrom(self.fd, &buf, buf.len, 0, null, null);
            if (linux.errno(n) != .SUCCESS or n == 0) break;
        }
        return true;
    }
};

/// Is the namespace READY: the block node under the controller, its uuid
/// as expected (when known), its char twin's sysfs entry and both `/dev`
/// nodes present (devtmpfs).
fn namespaceReady(gpa: Allocator, instance: u32, expected_uuid: ?[16]u8) ConnectError!?Namespace {
    const ns = (controllerNamespace(gpa, SYSFS_ROOT, instance) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        else => return null,
    }) orelse return null;
    if (!ns.char_present) return null;
    var bp: [40]u8 = undefined;
    var cp: [40]u8 = undefined;
    var st: linux.Statx = undefined;
    var bz: [48]u8 = undefined;
    var cz: [48]u8 = undefined;
    const bzp = std.fmt.bufPrintSentinel(&bz, "{s}", .{ns.blockPath(&bp)}, 0) catch return null;
    const czp = std.fmt.bufPrintSentinel(&cz, "{s}", .{ns.charPath(&cp)}, 0) catch return null;
    if (linux.errno(linux.statx(linux.AT.FDCWD, bzp, 0, .{ .TYPE = true }, &st)) != .SUCCESS) return null;
    if (linux.errno(linux.statx(linux.AT.FDCWD, czp, 0, .{ .TYPE = true }, &st)) != .SUCCESS) return null;
    if (expected_uuid) |want| {
        const got = ns.uuid orelse return null;
        if (!std.mem.eql(u8, &got, &want)) return error.UuidMismatch;
    }
    return ns;
}

pub const Connected = struct {
    instance: u32,
    cntlid: u16,
    ns: Namespace,
};

/// §3.1 + §3.4: write the option string, read the instance back, await
/// the namespace by uevent + sysfs verification.
pub fn connect(gpa: Allocator, spec: ConnectSpec, expected_uuid: ?[16]u8) ConnectError!Connected {
    if (spec.nr_io_queues) |n| if (n == 0) return error.ZeroQueues;
    var uev = try UeventSocket.open();
    defer uev.close();
    const rc = linux.open(FABRICS_DEV, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) {
        stats.fabric_connect_failures.inc();
        return if (linux.errno(rc) == .NOENT) error.FabricsDeviceAbsent else connectErrnoClass(linux.errno(rc));
    }
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var sb: [1024]u8 = undefined;
    const text = spec.text(&sb);
    const wn = linux.write(fd, text.ptr, text.len);
    if (linux.errno(wn) != .SUCCESS) {
        stats.fabric_connect_failures.inc();
        const e = connectErrnoClass(linux.errno(wn));
        log.warn("nvmeof connect: write to {s} refused ({t}) for `{s}`", .{ FABRICS_DEV, linux.errno(wn), text });
        return e;
    }
    var rb: [128]u8 = undefined;
    const rn = linux.read(fd, &rb, rb.len);
    if (linux.errno(rn) != .SUCCESS) {
        stats.fabric_connect_failures.inc();
        return error.ReadBackMalformed;
    }
    const back = parseReadBack(rb[0..rn]) orelse {
        stats.fabric_connect_failures.inc();
        return error.ReadBackMalformed;
    };
    stats.fabric_connects.inc();
    // The controller exists from the read-back on: a failure below deletes
    // it, else it outlives the refusal and keeps reconnecting for
    // ctrl_loss_tmo (a kamrui gate left five such behind).
    errdefer deleteController(back.instance) catch {};
    const deadline = clock.monotonicNs() + adminTimeoutS() * std.time.ns_per_s;
    while (true) {
        if (try namespaceReady(gpa, back.instance, expected_uuid)) |ns| return .{ .instance = back.instance, .cntlid = back.cntlid, .ns = ns };
        if (readController(SYSFS_ROOT, blk: {
            var nb: [16]u8 = undefined;
            break :blk std.fmt.bufPrint(&nb, "nvme{d}", .{back.instance}) catch unreachable;
        })) |c| {
            if (std.mem.eql(u8, c.stateOf(), "dead") or std.mem.eql(u8, c.stateOf(), "deleting")) return error.ControllerDead;
        } else return error.ControllerDead;
        if (!uev.waitEvent(deadline)) {
            // One last look: the event may have landed as the deadline did.
            if (try namespaceReady(gpa, back.instance, expected_uuid)) |ns| return .{ .instance = back.instance, .cntlid = back.cntlid, .ns = ns };
            return error.NamespaceTimeout;
        }
    }
}

// ---------------------------------------------------------------------------
// §3.5 Disconnect.
// ---------------------------------------------------------------------------

pub const DisconnectFilter = struct {
    subsysnqn: []const u8,
    /// `--identity-only`: delete only the controllers carrying this pair.
    identity: ?struct { hostnqn: []const u8, hostid: []const u8 } = null,
};

/// `delete_controller ← 1` on every matching controller; the count.
pub fn disconnect(gpa: Allocator, filter: DisconnectFilter) !u32 {
    const ctrls = try listControllers(gpa, SYSFS_ROOT);
    defer gpa.free(ctrls);
    var n: u32 = 0;
    for (ctrls) |c| {
        if (!std.mem.eql(u8, c.subsysnqnOf(), filter.subsysnqn)) continue;
        if (filter.identity) |id| if (!c.hasIdentity(id.hostnqn, id.hostid)) continue;
        try deleteController(c.instance);
        n += 1;
    }
    if (n != 0) stats.fabric_disconnects.add(n);
    return n;
}

pub fn deleteController(instance: u32) !void {
    var pb: [96]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "{s}/class/nvme/nvme{d}/delete_controller", .{ SYSFS_ROOT, instance }, 0) catch unreachable;
    const rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return errors.fromErrno(linux.errno(rc));
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    const n = linux.write(fd, "1", 1);
    if (linux.errno(n) != .SUCCESS) return errors.fromErrno(linux.errno(n));
}

/// Await the controller dir's disappearance (uevent-driven, bounded).
pub fn awaitControllerGone(instance: u32) bool {
    var uev = UeventSocket.open() catch return controllerGone(instance);
    defer uev.close();
    const deadline = clock.monotonicNs() + DISCONNECT_WAIT_S * std.time.ns_per_s;
    while (true) {
        if (controllerGone(instance)) return true;
        if (!uev.waitEvent(deadline)) return controllerGone(instance);
    }
}

fn controllerGone(instance: u32) bool {
    var pb: [96]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "{s}/class/nvme/nvme{d}", .{ SYSFS_ROOT, instance }, 0) catch return true;
    var st: linux.Statx = undefined;
    return linux.errno(linux.statx(linux.AT.FDCWD, path.ptr, 0, .{ .TYPE = true }, &st)) != .SUCCESS;
}

// ---------------------------------------------------------------------------
// §3.6 The `fabric_*` sampler.
// ---------------------------------------------------------------------------

/// `stable_endpoint`: only `traddr=`/`trsvcid=` of the address string
/// (nvme-tcp appends `src_addr=` only while queue 0 is live — FIND-N6-A).
pub fn stableEndpoint(address: []const u8, out: *[128]u8) []const u8 {
    var w: Io.Writer = .fixed(out);
    var it = std.mem.splitScalar(u8, address, ',');
    var first = true;
    while (it.next()) |kv| {
        if (!std.mem.startsWith(u8, kv, "traddr=") and !std.mem.startsWith(u8, kv, "trsvcid=")) continue;
        if (!first) w.writeByte(',') catch break;
        w.writeAll(kv) catch break;
        first = false;
    }
    return w.buffered();
}

pub const Sampler = struct {
    const Ident = struct {
        key: u64,
        live: bool,
    };
    /// The retained identities (the reconnect law's state).
    seen: std.ArrayList(Ident) = .empty,
    /// The sample's working tables, RETAINED across samples (cleared,
    /// never freed): the identities this sample merged and, per retained
    /// identity, whether the sample met it. They grow only when the
    /// population grows, so a steady fabric samples with NO allocation —
    /// the `.stats` render's one-allocation law holds with controllers
    /// present (every render allocated and freed them before).
    current: std.ArrayList(Ident) = .empty,
    present: std.ArrayList(bool) = .empty,
    /// The sample in progress: fabric controllers seen, not-live among them.
    controllers: u64 = 0,
    not_live: u64 = 0,

    pub fn deinit(self: *Sampler, gpa: Allocator) void {
        self.seen.deinit(gpa);
        self.current.deinit(gpa);
        self.present.deinit(gpa);
    }

    fn keyOf(c: *const Controller) u64 {
        var eb: [128]u8 = undefined;
        var h: checksum.Hasher = .init();
        h.update(c.transportOf());
        h.update("\x00");
        h.update(c.subsysnqnOf());
        h.update("\x00");
        h.update(stableEndpoint(c.addressOf(), &eb));
        return h.finalLow64();
    }

    /// One sample over the controllers: the two gauges and the observed
    /// not-live → live transitions. State law (F:279-341): identities
    /// that vanish while not-live are RETAINED (a give-up + renumbered
    /// reappearance as live counts once); those that vanish while live are
    /// dropped; live wins on duplicates. `gpa` owns every table of this
    /// sampler (`deinit` takes the same one).
    pub fn sample(self: *Sampler, gpa: Allocator, ctrls: []const Controller) !void {
        try self.begin(gpa);
        for (ctrls) |*c| try self.observe(gpa, c);
        try self.finish(gpa);
    }

    /// One sample straight off `<root>/class/nvme`: every controller is
    /// read into a stack `Controller` and folded as it is met — nothing is
    /// listed, duped or sorted. An absent `class/nvme` is a zero sample.
    pub fn sampleSysfs(self: *Sampler, gpa: Allocator, root: []const u8) !void {
        try self.begin(gpa);
        var walk = DirWalk.open("{s}/class/nvme", .{root}) catch |e| switch (e) {
            error.FileNotFound => return self.finish(gpa),
            else => return e,
        };
        defer walk.close();
        while (try walk.next()) |name| {
            const c = readController(root, name) orelse continue;
            try self.observe(gpa, &c);
        }
        try self.finish(gpa);
    }

    fn begin(self: *Sampler, gpa: Allocator) !void {
        self.controllers = 0;
        self.not_live = 0;
        self.current.clearRetainingCapacity();
        self.present.clearRetainingCapacity();
        try self.present.appendNTimes(gpa, false, self.seen.items.len);
    }

    fn observe(self: *Sampler, gpa: Allocator, c: *const Controller) !void {
        if (!c.isFabric()) return;
        self.controllers += 1;
        const live = c.isLive();
        if (!live) self.not_live += 1;
        const k = keyOf(c);
        for (self.current.items) |*cur| if (cur.key == k) {
            cur.live = cur.live or live;
            return;
        };
        try self.current.append(gpa, .{ .key = k, .live = live });
    }

    fn finish(self: *Sampler, gpa: Allocator) !void {
        var reconnects: u64 = 0;
        const present = self.present.items;
        for (self.current.items) |cur| {
            var found = false;
            for (self.seen.items[0..present.len], 0..) |*prev, i| if (prev.key == cur.key) {
                found = true;
                present[i] = true;
                if (!prev.live and cur.live) reconnects += 1;
                prev.live = cur.live;
            };
            if (!found) try self.seen.append(gpa, cur);
        }
        // Drop identities that vanished while live; keep the not-live ones.
        var i: usize = 0;
        var kept: usize = 0;
        while (i < present.len) : (i += 1) {
            const s = self.seen.items[i];
            if (!present[i] and s.live) continue;
            self.seen.items[kept] = s;
            kept += 1;
        }
        // Appended (new) identities sit past `present.len`: keep them.
        var j: usize = present.len;
        while (j < self.seen.items.len) : (j += 1) {
            self.seen.items[kept] = self.seen.items[j];
            kept += 1;
        }
        self.seen.shrinkRetainingCapacity(kept);
        // The next sample's presence table is sized NOW, with the population
        // that grew it: a sample over a steady fabric allocates nothing.
        try self.present.ensureTotalCapacity(gpa, self.seen.items.len);
        stats.fabric_controllers.set(self.controllers);
        stats.fabric_ctrl_not_live.set(self.not_live);
        if (reconnects != 0) stats.fabric_ctrl_reconnects.add(reconnects);
    }
};

var process_sampler: Sampler = .{};
/// Two concurrent renders: the second skips its sample (acq_rel pairs the
/// release below with the next taker's acquire).
var process_sampler_busy: std.atomic.Value(bool) = .init(false);
/// The process sampler's tables outlive every render: the `seen`
/// identities are process-lifetime state (the retained not-live identities
/// ARE the reconnect law) and the working tables are reused sample after
/// sample, so they live on the process allocator, never a render's — under
/// the test runner each test's allocator dies with the test, and a list it
/// owned would be a dangling list on the next render (the segfault a venue
/// with live fabric controllers showed); and a render allocates ONE payload
/// and nothing else (`stats.zig`'s allocation law), controllers or not.
const process_sampler_gpa = std.heap.page_allocator;
/// Test seam: the process sampler's sysfs root (null = `SYSFS_ROOT`), so a
/// render can be measured over a fixture tree of controllers.
pub var test_sysfs_root: ?[]const u8 = null;

/// The process's sample off the real sysfs (the `.stats` render's cadence;
/// a cold path). Takes no allocator: the render has nothing to lend.
pub fn sampleFabric() void {
    if (process_sampler_busy.cmpxchgStrong(false, true, .acq_rel, .acquire) != null) return;
    defer process_sampler_busy.store(false, .release);
    process_sampler.sampleSysfs(process_sampler_gpa, test_sysfs_root orelse SYSFS_ROOT) catch {};
}

// ---------------------------------------------------------------------------
// §3.7 The refusal ladder (I:681-771), pure over a volume shape.
// ---------------------------------------------------------------------------

pub const Plane = enum { meta, data };

/// A `(hostnqn, hostid)` pair as the ladder compares them.
pub const HostPair = struct { hostnqn: []const u8, hostid: []const u8 };

pub const VolumeShape = struct {
    plane: Plane,
    /// A `fabric_endpoint` record exists for this data volume.
    has_record: bool,
    /// The identity of a controller already serving the device (null =
    /// no fabric controller / a local device).
    actual: ?HostPair,
    merged_shape: bool,
};

pub const Verdict = union(enum) {
    /// No explicit identity: everything accepts unchanged.
    accept,
    /// A meta volume with no fabric identity: the bootstrap exemption, loud.
    accept_unverified,
    /// A data volume with a record: the daemon connects under the pair.
    daemon_connect,
    /// The merged multipath shape: both remedies named.
    refuse_merged,
    /// A pre-connected controller whose identity ≠ the pair ("an explicit
    /// per-mount identity is never inert").
    refuse_identity,
    /// A data volume with no record: `set-fabric-endpoints` (uniform, even
    /// when the actual identity matches — one rule keeps the ladder
    /// decidable).
    refuse_no_record,

    pub fn write(self: Verdict, w: *Io.Writer) Io.Writer.Error!void {
        switch (self) {
            .accept => try w.writeAll("accepted"),
            .accept_unverified => try w.writeAll("accepted UNVERIFIED: a metadata volume served by no fabric controller carries no identity to check (the operator's bootstrap connect)"),
            .daemon_connect => try w.writeAll("the daemon connects this data volume itself under the mount's identity pair"),
            .refuse_merged => try w.writeAll("refused: two host identities are MERGED under one multipath head for this subsystem (its round-robin voids per-mount fencing) — boot the sqz kernel with nvme_core.fabrics_host_scoped_subsystems=Y, or a stock kernel with nvme_core.multipath=N"),
            .refuse_identity => try w.writeAll("refused: a controller already serves this device under ANOTHER host identity than the mount's pair — an explicit per-mount identity is never inert; disconnect it or drop the pair"),
            .refuse_no_record => try w.writeAll("refused: a data volume under an explicit identity needs a fabric_endpoint record (`squeezefs config set-fabric-endpoints`) — a pre-connected controller is refused even when its identity matches"),
        }
    }
};

pub fn fabricLadderVerdict(shape: VolumeShape, explicit: ?HostPair) Verdict {
    const pair = explicit orelse return .accept;
    if (shape.merged_shape) return .refuse_merged;
    if (shape.actual) |a| {
        if (!std.mem.eql(u8, a.hostnqn, pair.hostnqn) or !std.mem.eql(u8, a.hostid, pair.hostid)) return .refuse_identity;
    }
    switch (shape.plane) {
        .data => return if (shape.has_record) .daemon_connect else .refuse_no_record,
        .meta => return if (shape.actual != null) .accept else .accept_unverified,
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "connect string: the tcp base shape; each option singly and combined; the loop shape with and without traddr; an all-absent set is byte-identical to the base; the read-back parses and refuses garbage" {
    var buf: [512]u8 = undefined;
    const base: ConnectSpec = .{ .trtype = .tcp, .traddr = "10.0.0.1", .trsvcid = 4420, .subnqn = "nqn.x", .hostnqn = "nqn.h", .hostid = "0a0a0a0a-0a0a-4a0a-8a0a-0a0a0a0a0a0a" };
    try testing.expectEqualStrings("transport=tcp,traddr=10.0.0.1,trsvcid=4420,nqn=nqn.x,hostnqn=nqn.h,hostid=0a0a0a0a-0a0a-4a0a-8a0a-0a0a0a0a0a0a", base.text(&buf));
    var one = base;
    one.host_traddr = "10.0.0.9";
    try testing.expect(std.mem.endsWith(u8, one.text(&buf), ",host_traddr=10.0.0.9"));
    var two = base;
    two.host_iface = "eth1";
    try testing.expect(std.mem.endsWith(u8, two.text(&buf), ",host_iface=eth1"));
    var three = base;
    three.nr_io_queues = 1;
    try testing.expect(std.mem.endsWith(u8, three.text(&buf), ",nr_io_queues=1"));
    var all = base;
    all.host_traddr = "10.0.0.9";
    all.host_iface = "eth1";
    all.nr_io_queues = 4;
    try testing.expect(std.mem.endsWith(u8, all.text(&buf), ",host_traddr=10.0.0.9,host_iface=eth1,nr_io_queues=4"));
    const loop: ConnectSpec = .{ .trtype = .loop, .subnqn = "nqn.x", .hostnqn = "nqn.h", .hostid = "id" };
    try testing.expectEqualStrings("transport=loop,nqn=nqn.x,hostnqn=nqn.h,hostid=id", loop.text(&buf));
    var loop_addr = loop;
    loop_addr.traddr = "loopaddr";
    try testing.expectEqualStrings("transport=loop,nqn=nqn.x,hostnqn=nqn.h,hostid=id,traddr=loopaddr", loop_addr.text(&buf));
    try testing.expectEqual(ReadBack{ .instance = 7, .cntlid = 3 }, parseReadBack("instance=7,cntlid=3\n").?);
    try testing.expectEqual(ReadBack{ .instance = 1, .cntlid = 2 }, parseReadBack("instance=1,cntlid=2").?);
    try testing.expect(parseReadBack("instance=7") == null);
    try testing.expect(parseReadBack("foo=1,cntlid=2") == null);
    try testing.expect(parseReadBack("") == null);
    // Zero queues refuses before any side effect (no fabrics write).
    var zq = base;
    zq.nr_io_queues = 0;
    try testing.expectError(error.ZeroQueues, connect(testing.allocator, zq, null));
}

test "identity resolution: the explicit pair; one env half refuses; one /etc/nvme file refuses; both files serve; the machine-id derivation is deterministic, RFC 4122-shaped and named by the nvmexpress uuid prefix; no source refuses; a non-uuid hostid refuses" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var etc: proc.EtcNvme = .{};
    // Derived.
    const r1 = resolveIdentity(&env, .{}, &etc, "0123456789abcdef0123456789abcdef\n");
    try testing.expect(r1 == .identity);
    try testing.expectEqual(IdentitySource.machine_id, r1.identity.source);
    const id1 = r1.identity.hostid();
    try testing.expectEqual(@as(usize, 36), id1.len);
    try testing.expectEqual(@as(u8, '4'), id1[14]);
    try testing.expect(id1[19] == '8' or id1[19] == '9' or id1[19] == 'a' or id1[19] == 'b');
    try testing.expect(std.mem.startsWith(u8, r1.identity.hostnqn(), HOSTNQN_UUID_PREFIX));
    try testing.expectEqualStrings(id1, r1.identity.hostnqn()[HOSTNQN_UUID_PREFIX.len..]);
    const r1b = resolveIdentity(&env, .{}, &etc, "0123456789abcdef0123456789abcdef");
    try testing.expectEqualStrings(id1, r1b.identity.hostid());
    const r1c = resolveIdentity(&env, .{}, &etc, "ffffffffffffffffffffffffffffffff");
    try testing.expect(!std.mem.eql(u8, id1, r1c.identity.hostid()));
    try testing.expectEqual(deriveHostId("abc"), deriveHostId("abc"));
    // No source.
    try testing.expect(resolveIdentity(&env, .{}, &etc, null).refusal == .no_source);
    try testing.expect(resolveIdentity(&env, .{}, &etc, "  \n").refusal == .no_source);
    // /etc/nvme halves.
    const nqn_text = "nqn.2014-08.org.nvmexpress:uuid:1b1b1b1b-1b1b-4b1b-8b1b-1b1b1b1b1b1b";
    @memcpy(etc.nqn_buf[0..nqn_text.len], nqn_text);
    etc.nqn_len = nqn_text.len;
    try testing.expectEqual(IdentityRefusal{ .etc_half = .nqn_only }, resolveIdentity(&env, .{}, &etc, "abc").refusal);
    const id_text = "1b1b1b1b-1b1b-4b1b-8b1b-1b1b1b1b1b1b";
    @memcpy(etc.id_buf[0..id_text.len], id_text);
    etc.id_len = id_text.len;
    const r2 = resolveIdentity(&env, .{}, &etc, "abc");
    try testing.expectEqual(IdentitySource.etc_nvme, r2.identity.source);
    try testing.expectEqualStrings(nqn_text, r2.identity.hostnqn());
    etc.nqn_len = 0;
    try testing.expectEqual(IdentityRefusal{ .etc_half = .id_only }, resolveIdentity(&env, .{}, &etc, "abc").refusal);
    // The explicit pair wins over the files; one half refuses.
    try env.put("SQUEEZEFS_HOSTNQN", "nqn.2014-08.org.nvmexpress:uuid:2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c");
    try testing.expectEqual(IdentityRefusal{ .pair = .nqn_without_id }, resolveIdentity(&env, .{}, &etc, "abc").refusal);
    try env.put("SQUEEZEFS_HOSTID", "not-a-uuid");
    try testing.expectEqual(IdentityRefusal{ .malformed = .hostid }, resolveIdentity(&env, .{}, &etc, "abc").refusal);
    try env.put("SQUEEZEFS_HOSTID", "2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c");
    const r3 = resolveIdentity(&env, .{}, &etc, "abc");
    try testing.expectEqual(IdentitySource.explicit, r3.identity.source);
    try testing.expectEqualStrings("2c2c2c2c-2c2c-4c2c-8c2c-2c2c2c2c2c2c", r3.identity.hostid());
    const r4 = resolveIdentity(&env, .{ .hostid = "3d3d3d3d-3d3d-4d3d-8d3d-3d3d3d3d3d3d" }, &etc, "abc");
    try testing.expectEqualStrings("3d3d3d3d-3d3d-4d3d-8d3d-3d3d3d3d3d3d", r4.identity.hostid());
    var buf: [512]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    const ns: IdentityRefusal = .no_source;
    try ns.write(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), MACHINE_ID_PATH) != null);
}

test "the strict namespace shape: nvme0n1 and nvme12n34 match; a hidden path node nvme0c0n1, a partition, a controller and a foreign name do not; the char twin derives by name" {
    try testing.expectEqual(@as(u32, 0), splitNamespaceName("nvme0n1").?.ctrl);
    try testing.expectEqual(@as(u32, 34), splitNamespaceName("nvme12n34").?.ns);
    try testing.expect(splitNamespaceName("nvme0c0n1") == null);
    try testing.expect(splitNamespaceName("nvme0n1p1") == null);
    try testing.expect(splitNamespaceName("nvme0") == null);
    try testing.expect(splitNamespaceName("sda") == null);
    try testing.expect(splitNamespaceName("nvmen1") == null);
    var out: [32]u8 = undefined;
    try testing.expectEqualStrings("ng3n2", charTwinName("nvme3n2", &out).?);
    var eb: [128]u8 = undefined;
    try testing.expectEqualStrings("traddr=10.0.0.1,trsvcid=4420", stableEndpoint("traddr=10.0.0.1,trsvcid=4420,src_addr=10.0.0.2", &eb));
    try testing.expectEqualStrings("", stableEndpoint("", &eb));
}

/// A sysfs fixture under `/build/tmp`: `class/nvme/<ctrl>/{transport,
/// state,subsysnqn,hostnqn,hostid,address,<ns>/}`, `class/block/<ns>/
/// {nsid,uuid}`, `class/nvme-generic/<ng>/dev`, `class/nvme-subsystem/
/// <sub>/{subsysnqn, nvme<N>/, <head>/}`.
pub const Fixture = struct {
    gpa: Allocator,
    io: Io,
    root: []u8,

    pub fn init(gpa: Allocator, io: Io, tag: []const u8) !Fixture {
        _ = linux.mkdir("/build/tmp", 0o755);
        const root = try std.fmt.allocPrint(gpa, "/build/tmp/sysfs-fixture-{s}-{d}", .{ tag, linux.getpid() });
        Io.Dir.cwd().deleteTree(io, root) catch {};
        try Io.Dir.cwd().createDirPath(io, root);
        return .{ .gpa = gpa, .io = io, .root = root };
    }

    pub fn deinit(self: *Fixture) void {
        Io.Dir.cwd().deleteTree(self.io, self.root) catch {};
        self.gpa.free(self.root);
    }

    pub fn file(self: *Fixture, comptime rel: []const u8, args: anytype, content: []const u8) !void {
        var pb: [512]u8 = undefined;
        const p = try std.fmt.bufPrint(&pb, "{s}/" ++ rel, .{self.root} ++ args);
        try Io.Dir.cwd().createDirPath(self.io, std.fs.path.dirname(p).?);
        try Io.Dir.cwd().writeFile(self.io, .{ .sub_path = p, .data = content });
    }

    pub fn dir(self: *Fixture, comptime rel: []const u8, args: anytype) !void {
        var pb: [512]u8 = undefined;
        const p = try std.fmt.bufPrint(&pb, "{s}/" ++ rel, .{self.root} ++ args);
        try Io.Dir.cwd().createDirPath(self.io, p);
    }

    pub fn controller(self: *Fixture, n: u32, transport: []const u8, state: []const u8, nqn: []const u8, hostnqn: []const u8, hostid: []const u8, address: []const u8) !void {
        try self.file("class/nvme/nvme{d}/transport", .{n}, transport);
        try self.file("class/nvme/nvme{d}/state", .{n}, state);
        try self.file("class/nvme/nvme{d}/subsysnqn", .{n}, nqn);
        try self.file("class/nvme/nvme{d}/hostnqn", .{n}, hostnqn);
        try self.file("class/nvme/nvme{d}/hostid", .{n}, hostid);
        try self.file("class/nvme/nvme{d}/address", .{n}, address);
    }

    pub fn namespace(self: *Fixture, ctrl: u32, ns: u32, uuid: []const u8, with_char: bool) !void {
        try self.dir("class/nvme/nvme{d}/nvme{d}n{d}", .{ ctrl, ctrl, ns });
        try self.file("class/block/nvme{d}n{d}/nsid", .{ ctrl, ns }, "1\n");
        try self.file("class/block/nvme{d}n{d}/uuid", .{ ctrl, ns }, uuid);
        if (with_char) try self.file("class/nvme-generic/ng{d}n{d}/dev", .{ ctrl, ns }, "245:0\n");
    }
};

test "discovery over fixture trees: the plain per-controller node with its nsid, uuid and char twin; a path node and a partition never match; a missing controller is an honest null; a controller's attrs read; the merged shape is detected only with a head and > 1 hostnqn" {
    const gpa = testing.allocator;
    var fx = Fixture.init(gpa, testing.io, "disc") catch return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer fx.deinit();
    try fx.controller(3, "tcp", "live", "nqn.sub", "nqn.hostA", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "traddr=127.0.0.1,trsvcid=4420,src_addr=127.0.0.1");
    try fx.namespace(3, 1, "11111111-2222-3333-4444-555555555555\n", true);
    // Decoys under the controller: a path node and a partition.
    try fx.dir("class/nvme/nvme3/nvme3c3n1", .{});
    try fx.dir("class/nvme/nvme3/nvme3n1p1", .{});
    const ns = (try controllerNamespace(gpa, fx.root, 3)).?;
    try testing.expectEqualStrings("nvme3n1", ns.blockName());
    try testing.expectEqualStrings("ng3n1", ns.charName());
    try testing.expectEqual(@as(u32, 1), ns.nsid);
    try testing.expectEqual(@as(u8, 0x11), ns.uuid.?[0]);
    try testing.expect(ns.char_present);
    try testing.expect((try controllerNamespace(gpa, fx.root, 9)) == null);
    const c = readController(fx.root, "nvme3").?;
    try testing.expect(c.isFabric() and c.isLive());
    try testing.expectEqualStrings("nqn.sub", c.subsysnqnOf());
    try testing.expect(c.hasIdentity("nqn.hostA", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"));
    try testing.expect(readController(fx.root, "nvme7") == null);
    const ctrls = try listControllers(gpa, fx.root);
    defer gpa.free(ctrls);
    try testing.expectEqual(@as(usize, 1), ctrls.len);
    // A pcie controller is not fabric; a missing state reads not live.
    try fx.controller(4, "pcie", "live", "nqn.local", "", "", "");
    try fx.file("class/nvme/nvme5/transport", .{}, "tcp");
    try fx.file("class/nvme/nvme5/subsysnqn", .{}, "nqn.sub");
    const c4 = readController(fx.root, "nvme4").?;
    try testing.expect(!c4.isFabric());
    const c5 = readController(fx.root, "nvme5").?;
    try testing.expect(!c5.isLive());
    // The merged shape: a subsystem dir with a head and two controllers of distinct hostnqn.
    try fx.file("class/nvme-subsystem/nvme-subsys0/subsysnqn", .{}, "nqn.sub");
    try fx.dir("class/nvme-subsystem/nvme-subsys0/nvme3", .{});
    try testing.expect(!(try multipathMergedShape(gpa, fx.root, "nqn.sub")));
    try fx.dir("class/nvme-subsystem/nvme-subsys0/nvme0n1", .{});
    try testing.expect(!(try multipathMergedShape(gpa, fx.root, "nqn.sub")));
    try fx.controller(6, "tcp", "live", "nqn.sub", "nqn.hostB", "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "traddr=127.0.0.1,trsvcid=4420");
    try fx.dir("class/nvme-subsystem/nvme-subsys0/nvme6", .{});
    try testing.expect(try multipathMergedShape(gpa, fx.root, "nqn.sub"));
    try testing.expect(!(try multipathMergedShape(gpa, fx.root, "nqn.other")));
}

test "discovery on a multipath kernel: the controller holds only its hidden path node; the namespace is the head of the subsystem dir listing it, with its uuid and char twin — and never while another identity serves that subsystem" {
    const gpa = testing.allocator;
    var fx = Fixture.init(gpa, testing.io, "mpath") catch return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer fx.deinit();
    // kamrui's 7.0.0-38-generic after `connect` of a loop target as nvme7:
    // `class/nvme/nvme7/nvme2c7n1` (hidden), the head `nvme2n1` + `ng2n1`
    // and the `nvme7` link in `nvme-subsys2`.
    try fx.controller(7, "loop", "live", "nqn.sub", "nqn.hostA", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "");
    try fx.dir("class/nvme/nvme7/nvme2c7n1", .{});
    try fx.file("class/nvme-subsystem/nvme-subsys2/subsysnqn", .{}, "nqn.sub");
    try fx.dir("class/nvme-subsystem/nvme-subsys2/nvme7", .{});
    try fx.dir("class/nvme-subsystem/nvme-subsys2/nvme2n1", .{});
    try fx.dir("class/nvme-subsystem/nvme-subsys2/ng2n1", .{});
    try fx.file("class/block/nvme2n1/nsid", .{}, "1\n");
    try fx.file("class/block/nvme2n1/uuid", .{}, "66666666-2222-3333-4444-555555555555\n");
    try fx.file("class/nvme-generic/ng2n1/dev", .{}, "245:2\n");
    // A foreign subsystem's head is never ours.
    try fx.file("class/nvme-subsystem/nvme-subsys0/subsysnqn", .{}, "nqn.local");
    try fx.dir("class/nvme-subsystem/nvme-subsys0/nvme0n1", .{});
    const ns = (try controllerNamespace(gpa, fx.root, 7)).?;
    try testing.expectEqualStrings("nvme2n1", ns.blockName());
    try testing.expectEqualStrings("ng2n1", ns.charName());
    try testing.expect(ns.char_present);
    try testing.expectEqual(@as(u32, 1), ns.nsid);
    try testing.expectEqual(@as(u8, 0x66), ns.uuid.?[0]);
    // A second controller of OUR identity on the subsystem: still ours.
    try fx.controller(8, "loop", "live", "nqn.sub", "nqn.hostA", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "");
    try fx.dir("class/nvme-subsystem/nvme-subsys2/nvme8", .{});
    try testing.expect((try controllerNamespace(gpa, fx.root, 7)) != null);
    // Another identity serving it: the merged shape — never handed out.
    try fx.controller(9, "loop", "live", "nqn.sub", "nqn.hostB", "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "");
    try fx.dir("class/nvme-subsystem/nvme-subsys2/nvme9", .{});
    try testing.expect((try controllerNamespace(gpa, fx.root, 7)) == null);
}

/// An allocator that counts its calls (the sampler's zero-allocation pin).
const CallCount = struct {
    inner: Allocator,
    allocs: usize = 0,
    resizes: usize = 0,
    remaps: usize = 0,
    frees: usize = 0,

    fn allocator(self: *CallCount) Allocator {
        return .{ .ptr = self, .vtable = &.{ .alloc = alloc, .resize = resize, .remap = remap, .free = free } };
    }
    fn alloc(ctx: *anyopaque, len: usize, alignment: std.mem.Alignment, ret_addr: usize) ?[*]u8 {
        const self: *CallCount = @ptrCast(@alignCast(ctx));
        self.allocs += 1;
        return self.inner.rawAlloc(len, alignment, ret_addr);
    }
    fn resize(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, new_len: usize, ret_addr: usize) bool {
        const self: *CallCount = @ptrCast(@alignCast(ctx));
        self.resizes += 1;
        return self.inner.rawResize(memory, alignment, new_len, ret_addr);
    }
    fn remap(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, new_len: usize, ret_addr: usize) ?[*]u8 {
        const self: *CallCount = @ptrCast(@alignCast(ctx));
        self.remaps += 1;
        return self.inner.rawRemap(memory, alignment, new_len, ret_addr);
    }
    fn free(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, ret_addr: usize) void {
        const self: *CallCount = @ptrCast(@alignCast(ctx));
        self.frees += 1;
        self.inner.rawFree(memory, alignment, ret_addr);
    }

    fn total(self: *const CallCount) usize {
        return self.allocs + self.resizes + self.remaps + self.frees;
    }
};

test "the sampler's tables are retained across samples: over a fixture tree of three fabric controllers the first sysfs sample grows them, the second and every later sample of the same population touches the allocator NOT ONCE (no list, no dupe, no sort, no transient table); the reconnect law still reads across samples; the retained identities outlive the render" {
    const gpa = testing.allocator;
    var fx = Fixture.init(gpa, testing.io, "sampler") catch return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer fx.deinit();
    try fx.controller(0, "pcie", "live", "nqn.local", "", "", "");
    try fx.controller(1, "tcp", "connecting", "nqn.sub-a", "nqn.host", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "traddr=10.0.0.1,trsvcid=4420");
    try fx.controller(2, "loop", "live", "nqn.sub-b", "nqn.host", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "traddr=,trsvcid=");
    try fx.controller(3, "tcp", "live", "nqn.sub-c", "nqn.host", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "traddr=10.0.0.2,trsvcid=4420");
    var counting: CallCount = .{ .inner = gpa };
    var s: Sampler = .{};
    defer s.deinit(counting.allocator());
    try s.sampleSysfs(counting.allocator(), fx.root);
    try testing.expectEqual(@as(u64, 3), stats.fabric_controllers.load());
    try testing.expectEqual(@as(u64, 1), stats.fabric_ctrl_not_live.load());
    try testing.expectEqual(@as(usize, 3), s.seen.items.len);
    try testing.expect(counting.total() > 0);
    // Steady population: the tables are already sized — zero calls.
    const grown = counting.total();
    try s.sampleSysfs(counting.allocator(), fx.root);
    try s.sampleSysfs(counting.allocator(), fx.root);
    try testing.expectEqual(grown, counting.total());
    try testing.expectEqual(@as(u64, 3), stats.fabric_controllers.load());
    // A reconnect read across samples (the retained identity, now live).
    const base = stats.fabric_ctrl_reconnects.load();
    try fx.file("class/nvme/nvme1/state", .{}, "live");
    try s.sampleSysfs(counting.allocator(), fx.root);
    try testing.expectEqual(base + 1, stats.fabric_ctrl_reconnects.load());
    try testing.expectEqual(@as(u64, 0), stats.fabric_ctrl_not_live.load());
    try testing.expectEqual(grown, counting.total());
    // The controller list face folds through the same tables: still zero.
    var a: Controller = .{ .instance = 1 };
    fill(&a.transport, &a.transport_len, "tcp");
    fill(&a.state, &a.state_len, "live");
    fill(&a.subsysnqn, &a.subsysnqn_len, "nqn.sub-a");
    fill(&a.address, &a.address_len, "traddr=10.0.0.1,trsvcid=4420");
    try s.sample(counting.allocator(), &.{a});
    try testing.expectEqual(grown, counting.total());
    try testing.expectEqual(@as(u64, 1), stats.fabric_controllers.load());
    // A missing class/nvme is a zero sample, still without a call.
    try s.sampleSysfs(counting.allocator(), "/nonexistent-sysfs-root");
    try testing.expectEqual(@as(u64, 0), stats.fabric_controllers.load());
    try testing.expectEqual(grown, counting.total());
    // The process sampler reads the seam's root and lends nothing to a render.
    test_sysfs_root = fx.root;
    defer test_sysfs_root = null;
    sampleFabric();
    try testing.expectEqual(@as(u64, 3), stats.fabric_controllers.load());
}

test "the fabric sampler: pcie skipped; unknown state not live; a missing sysfs is a zero sample; a reconnect is an observed not-live → live; renumbering of a not-live identity is tracked once; src_addr churn is one identity; the same NQN on two addresses is two" {
    const gpa = testing.allocator;
    var s: Sampler = .{};
    defer s.deinit(gpa);
    const base_reconnects = stats.fabric_ctrl_reconnects.load();
    try s.sample(gpa, &.{});
    try testing.expectEqual(@as(u64, 0), stats.fabric_controllers.load());
    var pcie: Controller = .{ .instance = 0 };
    fill(&pcie.transport, &pcie.transport_len, "pcie");
    fill(&pcie.state, &pcie.state_len, "live");
    var a: Controller = .{ .instance = 1 };
    fill(&a.transport, &a.transport_len, "tcp");
    fill(&a.state, &a.state_len, "connecting");
    fill(&a.subsysnqn, &a.subsysnqn_len, "nqn.sub");
    fill(&a.address, &a.address_len, "traddr=10.0.0.1,trsvcid=4420");
    try s.sample(gpa, &.{ pcie, a });
    try testing.expectEqual(@as(u64, 1), stats.fabric_controllers.load());
    try testing.expectEqual(@as(u64, 1), stats.fabric_ctrl_not_live.load());
    // The same identity live, with src_addr appended: one reconnect.
    var a_live = a;
    fill(&a_live.state, &a_live.state_len, "live");
    fill(&a_live.address, &a_live.address_len, "traddr=10.0.0.1,trsvcid=4420,src_addr=10.0.0.2");
    try s.sample(gpa, &.{a_live});
    try testing.expectEqual(base_reconnects + 1, stats.fabric_ctrl_reconnects.load());
    try testing.expectEqual(@as(u64, 0), stats.fabric_ctrl_not_live.load());
    // Still live: no new reconnect.
    try s.sample(gpa, &.{a_live});
    try testing.expectEqual(base_reconnects + 1, stats.fabric_ctrl_reconnects.load());
    // Vanishes while NOT live (a give-up), then reappears renumbered as live: counts once.
    var a_dead = a;
    fill(&a_dead.state, &a_dead.state_len, "deleting");
    try s.sample(gpa, &.{a_dead});
    try s.sample(gpa, &.{});
    try testing.expectEqual(@as(usize, 1), s.seen.items.len);
    var a_back = a_live;
    a_back.instance = 9;
    try s.sample(gpa, &.{a_back});
    try testing.expectEqual(base_reconnects + 2, stats.fabric_ctrl_reconnects.load());
    // Vanishes while live: dropped; a later reappearance is a fresh identity (no reconnect).
    try s.sample(gpa, &.{});
    try testing.expectEqual(@as(usize, 0), s.seen.items.len);
    try s.sample(gpa, &.{a_back});
    try testing.expectEqual(base_reconnects + 2, stats.fabric_ctrl_reconnects.load());
    // The same NQN on another address: a distinct identity.
    var b = a_live;
    fill(&b.address, &b.address_len, "traddr=10.0.0.5,trsvcid=4420");
    try s.sample(gpa, &.{ a_back, b });
    try testing.expectEqual(@as(usize, 2), s.seen.items.len);
    try testing.expectEqual(@as(u64, 2), stats.fabric_controllers.load());
}

test "the ladder verdict: every (plane, record, actual, merged) cell under no identity and under a pair" {
    const pair: HostPair = .{ .hostnqn = "nqn.h", .hostid = "id" };
    const other: HostPair = .{ .hostnqn = "nqn.other", .hostid = "id2" };
    inline for (.{ Plane.meta, Plane.data }) |plane| {
        inline for (.{ false, true }) |rec| {
            try testing.expectEqual(Verdict.accept, fabricLadderVerdict(.{ .plane = plane, .has_record = rec, .actual = null, .merged_shape = true }, null));
        }
    }
    try testing.expectEqual(Verdict.refuse_merged, fabricLadderVerdict(.{ .plane = .data, .has_record = true, .actual = null, .merged_shape = true }, pair));
    try testing.expectEqual(Verdict.refuse_identity, fabricLadderVerdict(.{ .plane = .data, .has_record = true, .actual = other, .merged_shape = false }, pair));
    try testing.expectEqual(Verdict.refuse_identity, fabricLadderVerdict(.{ .plane = .meta, .has_record = false, .actual = other, .merged_shape = false }, pair));
    try testing.expectEqual(Verdict.daemon_connect, fabricLadderVerdict(.{ .plane = .data, .has_record = true, .actual = null, .merged_shape = false }, pair));
    try testing.expectEqual(Verdict.daemon_connect, fabricLadderVerdict(.{ .plane = .data, .has_record = true, .actual = pair, .merged_shape = false }, pair));
    try testing.expectEqual(Verdict.refuse_no_record, fabricLadderVerdict(.{ .plane = .data, .has_record = false, .actual = pair, .merged_shape = false }, pair));
    try testing.expectEqual(Verdict.refuse_no_record, fabricLadderVerdict(.{ .plane = .data, .has_record = false, .actual = null, .merged_shape = false }, pair));
    try testing.expectEqual(Verdict.accept, fabricLadderVerdict(.{ .plane = .meta, .has_record = false, .actual = pair, .merged_shape = false }, pair));
    try testing.expectEqual(Verdict.accept_unverified, fabricLadderVerdict(.{ .plane = .meta, .has_record = false, .actual = null, .merged_shape = false }, pair));
    var buf: [512]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    const rm: Verdict = .refuse_merged;
    try rm.write(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "fabrics_host_scoped_subsystems=Y") != null and std.mem.indexOf(u8, w.buffered(), "nvme_core.multipath=N") != null);
}

test "host-scoped parameter and admin_timeout read on the venue; absent elsewhere" {
    const hs = readHostScoped();
    _ = hs.announce();
    try testing.expect(adminTimeoutS() >= 1);
    try testing.expectEqual(@as(u64, @backingInt(hs)), stats.fabric_host_scoped.load());
}

fn readBackFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [128]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseReadBack(buf[0..len]);
}

test "fuzz: the nvmf_dev_read read-back parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, readBackFuzz, .{ .corpus = &.{ fuzzkit.slice("instance=3,cntlid=1\n"), fuzzkit.slice("instance=,cntlid=99999\n") } });
}
