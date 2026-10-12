//! The kernel `nvmet` target driven through configfs DIRECTLY (charter §8
//! "NVMe-oF targets"; `docs/porting/ops_nvmet_keys.md` §1; decision 0003
//! §G): no `nvme-cli`, no `modprobe`, no `losetup` — the product mounts
//! configfs itself when absent, writes ONLY real kernel attributes, serves
//! a REGULAR FILE through nvmet's file backend (quirk Q3), and judges
//! readiness by the configfs `nvmet` directory (not `/sys/module/nvmet`,
//! which a built-in target never has — §0, quirk Q4).
//!
//! The recipe is ORDERED and every write is checked: `attr_allow_any_host`
//! (the first initiator must find the final posture) → `namespaces/1` →
//! `device_path` → `device_uuid` (the recorded identity, re-presented
//! verbatim by `restore`) → `resv_enable = 1` → `enable = 1` (the kernel
//! refuses `device_path`/`device_uuid` with EBUSY and `resv_enable` with
//! EINVAL once enabled — `AttrLocked`, measured on 7.2.6-sqz) → the port
//! object → the `ports/<id>/subsystems/<nqn>` link, which is what ENABLES
//! the port (an unbindable address fails HERE, after the namespace is
//! live — the crash-window leg). Teardown runs the mirror order: links →
//! `enable = 0` → `rmdir namespaces/1` → allowed-host links → `rmdir` the
//! subsystem → last-out `rmdir ports/<id>` → `rmdir hosts/<h>` iff no
//! other record names the host.
//!
//! Port ids ride a reserved u16 range `[base, base + 999]` (the kernel
//! names ports with `kstrtou16`): the first candidate is `base + hash64(key)
//! % 900` (900 hash slots; the last 100 are pure probe headroom), then a
//! linear probe wrapping through the whole range; an existing port is
//! OURS-AND-MATCHING iff its `(trtype, adrfam, traddr, trsvcid)` match and
//! every link under it names a LEDGERED nqn — "ownership is ledger
//! membership, never a prefix"; anything else is foreign and never touched.
//!
//! Every mutation goes through the `Fs` face: the real one over raw
//! syscalls, the FAKE an in-process tree with the kernel's semantics
//! (default groups, attribute locks, the first-link port enable, EBUSY on
//! a linked target), so the exact recipe is pinned without a kernel.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;

const errors = @import("../../core/errors.zig");
const checksum = @import("../../core/checksum.zig");
const log = @import("../../core/log.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/configfs is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Error = errors.Error;

/// The kernel's mount point and the nvmet subsystem's root (N:62).
pub const CONFIGFS_MOUNT = "/sys/kernel/config";
pub const NVMET_ROOT = CONFIGFS_MOUNT ++ "/nvmet";

/// One namespace per subsystem, index 1 STRUCTURALLY (D:359): the G2
/// same-nsid clause holds by construction.
pub const NAMESPACE_INDEX: u32 = 1;

/// The NVMe spec's NQN maximum (`NVMF_NQN_SIZE`; the kernel's field is
/// 256 with its NUL).
pub const NVMF_NQN_SIZE: usize = 223;

/// Port ids: WIDTH 1000 ids per base, 900 hash slots ("the hash spreads
/// over 900 slots; the last 100 are pure probe headroom", N:57-59).
pub const PORT_ID_RANGE: u16 = 1000;
pub const PORT_HASH_SLOTS: u16 = 900;
/// Default base 53000: disjoint from the reference dev-substrate's 52026
/// and the scoping rig's 52470/52471 (D:358); the fidelity tier relocates
/// to 54000 so a product range and a harness range never meet.
pub const PORT_ID_BASE_DEFAULT: u16 = 53000;
/// `base + 999 ≤ 65535`: ids are u16 (quirk Q6).
pub const PORT_ID_BASE_MAX: u16 = std.math.maxInt(u16) - (PORT_ID_RANGE - 1);

/// The IANA NVMe/TCP port.
pub const DEFAULT_TRSVCID: u16 = 4420;

/// Default share NQNs: CLASSIFICATION only (M:57-62).
pub const OWNERSHIP_NQN_PREFIX = "nqn.2026-10.io.squeezefs:share-";
/// `adopt` refuses the test fabric by name (M:789-795) and by port id:
/// the fidelity tier's range `[54000, 54999]` (`SQUEEZEFS_NVMET_PORT_ID_
/// BASE=54000`; SUB:33-37 relocated — the loop key hashes to ONE fixed
/// candidate, so the harness owns the whole relocated range), disjoint
/// from the product's default `[53000, 53999]`.
pub const HARNESS_NQN_MARKERS = [_][]const u8{ ":devsub-", ":fideli-" };
pub const HARNESS_PORT_ID_BASE: u16 = 54000;
pub const HARNESS_PORT_ID_SLICE: u16 = PORT_ID_RANGE;

pub fn isHarnessPortId(id: u16) bool {
    return id >= HARNESS_PORT_ID_BASE and id < HARNESS_PORT_ID_BASE + HARNESS_PORT_ID_SLICE;
}

pub const Trtype = enum(u8) {
    tcp = 1,
    loop = 2,

    pub fn name(self: Trtype) []const u8 {
        return switch (self) {
            .tcp => "tcp",
            .loop => "loop",
        };
    }

    pub fn fromWire(b: u8) ?Trtype {
        return switch (b) {
            1 => .tcp,
            2 => .loop,
            else => null,
        };
    }

    pub fn parse(text: []const u8) ?Trtype {
        if (std.mem.eql(u8, text, "tcp")) return .tcp;
        if (std.mem.eql(u8, text, "loop")) return .loop;
        return null;
    }
};

pub const Adrfam = enum(u8) {
    /// A loop port carries no address.
    none = 0,
    ipv4 = 1,
    ipv6 = 2,

    pub fn name(self: Adrfam) []const u8 {
        return switch (self) {
            .none => "loop",
            .ipv4 => "ipv4",
            .ipv6 => "ipv6",
        };
    }

    pub fn fromWire(b: u8) ?Adrfam {
        return switch (b) {
            0 => .none,
            1 => .ipv4,
            2 => .ipv6,
            else => null,
        };
    }
};

/// One listener of a share: a tcp `(ip, port)` or the one loop port.
pub const Listener = struct {
    trtype: Trtype,
    adrfam: Adrfam,
    /// The IPv4 address in the first 4 bytes, an IPv6 address whole.
    traddr: [16]u8 = @splat(0),
    trsvcid: u16 = 0,
    /// The configfs port object (allocated before the pending record).
    port_id: u16 = 0,

    pub const loop: Listener = .{ .trtype = .loop, .adrfam = .none };

    /// `--ip a.b.c.d` / `--ip [v6]` + the port (`adrfam_of`, N:96-105: a
    /// malformed address refuses before root).
    pub fn tcp(ip_text: []const u8, port: u16) error{BadAddress}!Listener {
        var l: Listener = .{ .trtype = .tcp, .adrfam = .ipv4, .trsvcid = port };
        if (std.Io.net.Ip4Address.parse(ip_text, 0)) |v4| {
            @memcpy(l.traddr[0..4], &v4.bytes);
            return l;
        } else |_| {}
        const v6 = std.Io.net.Ip6Address.parse(ip_text, 0) catch return error.BadAddress;
        l.adrfam = .ipv6;
        l.traddr = v6.bytes;
        return l;
    }

    /// The textual address (`addr_traddr`), into `buf`.
    pub fn traddrText(self: Listener, buf: *[64]u8) []const u8 {
        return switch (self.adrfam) {
            .none => "",
            .ipv4 => std.fmt.bufPrint(buf, "{d}.{d}.{d}.{d}", .{ self.traddr[0], self.traddr[1], self.traddr[2], self.traddr[3] }) catch unreachable,
            .ipv6 => blk: {
                var w: std.Io.Writer = .fixed(buf);
                const a: std.Io.net.Ip6Address = .{ .port = 0, .bytes = self.traddr };
                a.format(&w) catch unreachable;
                const text = w.buffered();
                // `Ip6Address.format` brackets and appends `:0`; the kernel wants the bare address.
                break :blk text[1 .. text.len - 3];
            },
        };
    }

    /// `"{trtype}:{ip}:{port}"` / `"loop"`: the hash key of the port id.
    pub fn hashKey(self: Listener, buf: *[96]u8) []const u8 {
        if (self.trtype == .loop) return "loop";
        var ab: [64]u8 = undefined;
        return std.fmt.bufPrint(buf, "tcp:{s}:{d}", .{ self.traddrText(&ab), self.trsvcid }) catch unreachable;
    }

    /// The same configfs port object serves two links only for the SAME
    /// `(trtype, adrfam, traddr, trsvcid)`.
    pub fn sameEndpoint(a: Listener, b: Listener) bool {
        return a.trtype == b.trtype and a.adrfam == b.adrfam and a.trsvcid == b.trsvcid and std.mem.eql(u8, &a.traddr, &b.traddr);
    }
};

// ---------------------------------------------------------------------------
// The filesystem face.
// ---------------------------------------------------------------------------

/// What the recipe needs of configfs. Paths are RELATIVE to the nvmet
/// root (`subsystems/<nqn>/namespaces/1/enable`); `symlink` targets are
/// relative too (the real face resolves both under the root).
pub const Fs = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        mkdir: *const fn (ctx: *anyopaque, path: []const u8) Error!void,
        rmdir: *const fn (ctx: *anyopaque, path: []const u8) Error!void,
        symlink: *const fn (ctx: *anyopaque, target: []const u8, link: []const u8) Error!void,
        unlink: *const fn (ctx: *anyopaque, path: []const u8) Error!void,
        writeAttr: *const fn (ctx: *anyopaque, path: []const u8, value: []const u8) Error!void,
        /// The attribute's text, trimmed of its trailing newline.
        readAttr: *const fn (ctx: *anyopaque, path: []const u8, buf: []u8) Error![]const u8,
        /// Directory entries (names; caller frees each and the slice).
        list: *const fn (ctx: *anyopaque, gpa: Allocator, path: []const u8) Error![][]u8,
        exists: *const fn (ctx: *anyopaque, path: []const u8) bool,
    };

    pub fn mkdir(self: Fs, path: []const u8) Error!void {
        return self.vtable.mkdir(self.ctx, path);
    }
    pub fn rmdir(self: Fs, path: []const u8) Error!void {
        return self.vtable.rmdir(self.ctx, path);
    }
    pub fn symlink(self: Fs, target: []const u8, link: []const u8) Error!void {
        return self.vtable.symlink(self.ctx, target, link);
    }
    pub fn unlink(self: Fs, path: []const u8) Error!void {
        return self.vtable.unlink(self.ctx, path);
    }
    pub fn writeAttr(self: Fs, path: []const u8, value: []const u8) Error!void {
        return self.vtable.writeAttr(self.ctx, path, value);
    }
    pub fn readAttr(self: Fs, path: []const u8, buf: []u8) Error![]const u8 {
        return self.vtable.readAttr(self.ctx, path, buf);
    }
    pub fn list(self: Fs, gpa: Allocator, path: []const u8) Error![][]u8 {
        return self.vtable.list(self.ctx, gpa, path);
    }
    pub fn exists(self: Fs, path: []const u8) bool {
        return self.vtable.exists(self.ctx, path);
    }
};

pub fn freeNames(gpa: Allocator, names: [][]u8) void {
    for (names) |n| gpa.free(n);
    gpa.free(names);
}

/// `/sys/kernel/config/nvmet/<rel>` as a NUL-terminated path.
fn rootedZ(buf: *[linux.PATH_MAX]u8, rel: []const u8) Error![:0]const u8 {
    return std.fmt.bufPrintSentinel(buf, "{s}/{s}", .{ NVMET_ROOT, rel }, 0) catch error.NameTooLong;
}

/// The REAL configfs: raw syscalls under the nvmet root. Attribute writes
/// are control writes to a pseudo-filesystem (never a product I/O path).
pub const RealFs = struct {
    fn mkdir(_: *anyopaque, path: []const u8) Error!void {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = try rootedZ(&zb, path);
        const e = linux.errno(linux.mkdir(z, 0o755));
        if (e != .SUCCESS) return errors.fromErrno(e);
    }

    fn rmdir(_: *anyopaque, path: []const u8) Error!void {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = try rootedZ(&zb, path);
        const e = linux.errno(linux.rmdir(z));
        if (e != .SUCCESS) return errors.fromErrno(e);
    }

    fn symlink(_: *anyopaque, target: []const u8, link: []const u8) Error!void {
        var tb: [linux.PATH_MAX]u8 = undefined;
        var lb: [linux.PATH_MAX]u8 = undefined;
        const t = try rootedZ(&tb, target);
        const l = try rootedZ(&lb, link);
        const e = linux.errno(linux.symlink(t, l));
        if (e != .SUCCESS) return errors.fromErrno(e);
    }

    fn unlink(_: *anyopaque, path: []const u8) Error!void {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = try rootedZ(&zb, path);
        const e = linux.errno(linux.unlink(z));
        if (e != .SUCCESS) return errors.fromErrno(e);
    }

    fn writeAttr(_: *anyopaque, path: []const u8, value: []const u8) Error!void {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = try rootedZ(&zb, path);
        const rc = linux.open(z, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return errors.fromErrno(linux.errno(rc));
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        // A configfs store consumes the whole page in one call.
        const n = linux.write(fd, value.ptr, value.len);
        if (linux.errno(n) != .SUCCESS) return errors.fromErrno(linux.errno(n));
        if (n != value.len) return error.ShortWrite;
    }

    fn readAttr(_: *anyopaque, path: []const u8, buf: []u8) Error![]const u8 {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = try rootedZ(&zb, path);
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return errors.fromErrno(linux.errno(rc));
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var total: usize = 0;
        while (total < buf.len) {
            const n = linux.read(fd, buf.ptr + total, buf.len - total);
            if (linux.errno(n) != .SUCCESS) return errors.fromErrno(linux.errno(n));
            if (n == 0) break;
            total += n;
        }
        return std.mem.trimEnd(u8, buf[0..total], "\n");
    }

    fn list(_: *anyopaque, gpa: Allocator, path: []const u8) Error![][]u8 {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = try rootedZ(&zb, path);
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return errors.fromErrno(linux.errno(rc));
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var out: std.ArrayList([]u8) = .empty;
        errdefer freeNames(gpa, out.toOwnedSlice(gpa) catch &.{});
        var buf: [8192]u8 = undefined;
        while (true) {
            const n = linux.getdents64(fd, &buf, buf.len);
            if (linux.errno(n) != .SUCCESS) return errors.fromErrno(linux.errno(n));
            if (n == 0) break;
            var off: usize = 0;
            while (off < n) {
                const d: *align(1) linux.dirent64 = @ptrCast(&buf[off]);
                const name = std.mem.sliceTo(@as([*:0]u8, @ptrCast(&d.name)), 0);
                off += d.reclen;
                if (std.mem.eql(u8, name, ".") or std.mem.eql(u8, name, "..")) continue;
                try out.append(gpa, try gpa.dupe(u8, name));
            }
        }
        return out.toOwnedSlice(gpa);
    }

    fn exists(_: *anyopaque, path: []const u8) bool {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = rootedZ(&zb, path) catch return false;
        var st: linux.Statx = undefined;
        return linux.errno(linux.statx(linux.AT.FDCWD, z, linux.AT.SYMLINK_NOFOLLOW, .{ .TYPE = true }, &st)) == .SUCCESS;
    }

    const vtable: Fs.VTable = .{ .mkdir = mkdir, .rmdir = rmdir, .symlink = symlink, .unlink = unlink, .writeAttr = writeAttr, .readAttr = readAttr, .list = list, .exists = exists };
    var token: u8 = 0;

    pub fn face() Fs {
        return .{ .ctx = @ptrCast(&token), .vtable = &vtable };
    }
};

// ---------------------------------------------------------------------------
// Preflight (§1.6): mount configfs when absent, judge readiness by the
// nvmet dir. The product loads no module: a missing transport surfaces at
// the port link as the kernel's own `request_module` outcome.
// ---------------------------------------------------------------------------

pub const Readiness = enum {
    ready,
    /// `mount(2)` of configfs refused (not root, or no CONFIGFS_FS).
    configfs_unmountable,
    /// configfs is mounted but carries no `nvmet` dir: CONFIG_NVME_TARGET
    /// is neither built in nor loaded.
    nvmet_absent,
};

pub fn writeReadinessRefusal(w: *std.Io.Writer, r: Readiness, e: ?linux.E) std.Io.Writer.Error!void {
    switch (r) {
        .ready => {},
        .configfs_unmountable => try w.print("configfs is not mounted at {s} and mount(2) refused ({t}): run as root on a kernel with CONFIG_CONFIGFS_FS, or `mount -t configfs none {s}`", .{ CONFIGFS_MOUNT, e orelse .SUCCESS, CONFIGFS_MOUNT }),
        .nvmet_absent => try w.print("{s} is missing: the kernel NVMe target is not present (CONFIG_NVME_TARGET=y, or load nvmet) — the product loads no module", .{NVMET_ROOT}),
    }
}

/// `statx(nvmet/subsystems)` present ⇒ ready; else `mount("none",
/// "/sys/kernel/config", "configfs")` (EBUSY = already mounted, fine);
/// re-probe. The errno of a refused mount rides `e`.
pub fn preflight(e: *?linux.E) Readiness {
    e.* = null;
    if (nvmetPresent()) return .ready;
    const rc = linux.mount("none", CONFIGFS_MOUNT, "configfs", 0, 0);
    const me = linux.errno(rc);
    if (me != .SUCCESS and me != .BUSY) {
        e.* = me;
        if (!configfsMounted()) return .configfs_unmountable;
    }
    if (nvmetPresent()) return .ready;
    return .nvmet_absent;
}

pub fn nvmetPresent() bool {
    var st: linux.Statx = undefined;
    return linux.errno(linux.statx(linux.AT.FDCWD, NVMET_ROOT ++ "/subsystems", 0, .{ .TYPE = true }, &st)) == .SUCCESS;
}

/// `/proc/mounts` names a configfs at the mount point (an empty but
/// mounted configfs on a kernel without nvmet is distinguishable from an
/// unmounted one only here).
fn configfsMounted() bool {
    var buf: [1 << 16]u8 = undefined;
    const rc = linux.open("/proc/mounts", .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return false;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var total: usize = 0;
    while (total < buf.len) {
        const n = linux.read(fd, buf[total..].ptr, buf.len - total);
        if (linux.errno(n) != .SUCCESS or n == 0) break;
        total += n;
    }
    var lines = std.mem.splitScalar(u8, buf[0..total], '\n');
    while (lines.next()) |line| {
        var it = std.mem.tokenizeScalar(u8, line, ' ');
        _ = it.next() orelse continue;
        const where = it.next() orelse continue;
        const kind = it.next() orelse continue;
        if (std.mem.eql(u8, where, CONFIGFS_MOUNT) and std.mem.eql(u8, kind, "configfs")) return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// NQN grammar (M:331-353): ≤ 223 bytes, a configfs PATH COMPONENT —
// non-empty, no `/`, no whitespace, no leading `.` (VAL-7g: `..` once
// addressed the configfs PARENT).
// ---------------------------------------------------------------------------

pub const NqnError = error{ Empty, TooLong, HasSlash, HasWhitespace, LeadingDot, NotNqn };

pub fn writeNqnRefusal(w: *std.Io.Writer, r: NqnError, nqn: []const u8) std.Io.Writer.Error!void {
    switch (r) {
        error.Empty => try w.writeAll("an NQN must not be empty"),
        error.TooLong => try w.print("'{s}' is longer than NVMF_NQN_SIZE ({d} bytes)", .{ nqn, NVMF_NQN_SIZE }),
        error.HasSlash => try w.print("'{s}' contains '/': an NQN is a configfs path component", .{nqn}),
        error.HasWhitespace => try w.print("'{s}' contains whitespace", .{nqn}),
        error.LeadingDot => try w.print("'{s}' starts with '.': refused (`..` once addressed the configfs parent)", .{nqn}),
        error.NotNqn => try w.print("'{s}' does not start with 'nqn.' (NVMe Qualified Name grammar)", .{nqn}),
    }
}

/// A host NQN or a subsystem NQN as a configfs component.
pub fn validateNqn(nqn: []const u8) NqnError!void {
    if (nqn.len == 0) return error.Empty;
    if (nqn.len > NVMF_NQN_SIZE) return error.TooLong;
    if (std.mem.indexOfScalar(u8, nqn, '/') != null) return error.HasSlash;
    for (nqn) |c| if (c == ' ' or c == '\t' or c == '\n' or c == '\r') return error.HasWhitespace;
    if (nqn[0] == '.') return error.LeadingDot;
    if (!std.mem.startsWith(u8, nqn, "nqn.")) return error.NotNqn;
}

pub fn isHarnessNqn(nqn: []const u8) bool {
    for (HARNESS_NQN_MARKERS) |m| if (std.mem.indexOf(u8, nqn, m) != null) return true;
    return false;
}

// ---------------------------------------------------------------------------
// Port ids: the pure allocator over a configfs snapshot (N:118-154).
// ---------------------------------------------------------------------------

/// One `ports/<id>` as the walk sees it.
pub const PortEntry = struct {
    id: u16,
    trtype: ?Trtype,
    adrfam: []const u8,
    traddr: []const u8,
    trsvcid: []const u8,
    /// The subsystem NQNs linked under it.
    links: [][]u8,

    pub fn matches(self: PortEntry, l: Listener) bool {
        if (self.trtype != l.trtype) return false;
        if (l.trtype == .loop) return true;
        var ab: [64]u8 = undefined;
        var pb: [8]u8 = undefined;
        const port = std.fmt.bufPrint(&pb, "{d}", .{l.trsvcid}) catch unreachable;
        return std.mem.eql(u8, self.adrfam, l.adrfam.name()) and std.mem.eql(u8, self.traddr, l.traddrText(&ab)) and std.mem.eql(u8, self.trsvcid, port);
    }
};

pub const PortSnapshot = struct {
    entries: []PortEntry,

    pub fn find(self: PortSnapshot, id: u16) ?*const PortEntry {
        for (self.entries) |*e| if (e.id == id) return e;
        return null;
    }

    pub fn deinit(self: *PortSnapshot, gpa: Allocator) void {
        for (self.entries) |e| {
            gpa.free(e.adrfam);
            gpa.free(e.traddr);
            gpa.free(e.trsvcid);
            freeNames(gpa, e.links);
        }
        gpa.free(self.entries);
        self.* = undefined;
    }
};

/// "Ownership is ledger membership": is every nqn linked under the port a
/// ledgered one?
pub const Ownership = struct {
    ctx: *const anyopaque,
    isLedgered: *const fn (ctx: *const anyopaque, nqn: []const u8) bool,

    pub fn ownsPort(self: Ownership, p: *const PortEntry) bool {
        for (p.links) |nqn| if (!self.isLedgered(self.ctx, nqn)) return false;
        return true;
    }
};

/// `base + hash64(key) % 900` (quirk Q5: hash64, the ONE hash in the tree).
pub fn firstCandidate(base: u16, key: []const u8) u16 {
    return base + @as(u16, @intCast(checksum.hash64(key) % PORT_HASH_SLOTS));
}

pub const AllocError = error{PortRangeExhausted};

/// The first id from the candidate, wrapping through `[base, base+999]`,
/// that is FREE or OURS-AND-MATCHING; every id taken by a foreign or
/// incompatible port ⇒ exhausted (the refusal names the knob).
///
/// LOOP: ONE port object per host, whoever made it. The loop initiator
/// addresses no port (`nvme_loop_find_port` takes the FIRST loop port when
/// `traddr` is absent), so a second loop port would serve subsystems no
/// `transport=loop` connect can reach (measured: a foreign-linked loop
/// port skipped by the ownership law left the new share unreachable,
/// `-EIO` at the connect). Links are per subsystem — sharing the object
/// lets no one alter another's; last-out removal still needs every link
/// gone, so a foreign-linked loop port survives our teardown.
pub fn allocatePortId(base: u16, l: Listener, snapshot: PortSnapshot, ownership: Ownership) AllocError!u16 {
    if (l.trtype == .loop) {
        for (snapshot.entries) |e| if (e.trtype == .loop) return e.id;
    }
    var kb: [96]u8 = undefined;
    const start = firstCandidate(base, l.hashKey(&kb));
    var i: u16 = 0;
    while (i < PORT_ID_RANGE) : (i += 1) {
        const id: u16 = base + @as(u16, @intCast((@as(u32, start - base) + i) % PORT_ID_RANGE));
        const entry = snapshot.find(id) orelse return id;
        if (entry.matches(l) and ownership.ownsPort(entry)) return id;
    }
    return error.PortRangeExhausted;
}

// ---------------------------------------------------------------------------
// The walk (§1.7): the ONE probe `list`, the guard, `restore` and
// `adopt` read.
// ---------------------------------------------------------------------------

pub const LiveListener = struct {
    port_id: u16,
    trtype: ?Trtype,
    traddr: []const u8,
    trsvcid: []const u8,
};

pub const LiveShare = struct {
    subnqn: []u8,
    device_path: []u8,
    /// Parsed `device_uuid`; null when the attribute is empty or all-zero.
    device_uuid: ?[16]u8,
    enabled: bool,
    /// Numeric children of `namespaces/`, sorted (the shape law is `[1]`).
    nsids: []u32,
    allow_any_host: bool,
    allow_hosts: [][]u8,
    listeners: []LiveListener,

    /// `device_path non-empty ∨ enabled` (N:440): an empty mkdir'd shell
    /// serves nothing.
    pub fn materialized(self: *const LiveShare) bool {
        return self.device_path.len != 0 or self.enabled;
    }

    pub fn deinit(self: *LiveShare, gpa: Allocator) void {
        gpa.free(self.subnqn);
        gpa.free(self.device_path);
        gpa.free(self.nsids);
        freeNames(gpa, self.allow_hosts);
        for (self.listeners) |l| {
            gpa.free(l.traddr);
            gpa.free(l.trsvcid);
        }
        gpa.free(self.listeners);
        self.* = undefined;
    }
};

pub const Walk = struct {
    shares: []LiveShare,
    ports: PortSnapshot,

    pub fn find(self: *const Walk, subnqn: []const u8) ?*const LiveShare {
        for (self.shares) |*s| if (std.mem.eql(u8, s.subnqn, subnqn)) return s;
        return null;
    }

    pub fn deinit(self: *Walk, gpa: Allocator) void {
        for (self.shares) |*s| s.deinit(gpa);
        gpa.free(self.shares);
        self.ports.deinit(gpa);
        self.* = undefined;
    }
};

fn lessU32(_: void, a: u32, b: u32) bool {
    return a < b;
}

fn lessName(_: void, a: []u8, b: []u8) bool {
    return std.mem.lessThan(u8, a, b);
}

fn readAttrOwned(fs: Fs, gpa: Allocator, path: []const u8) Error![]u8 {
    var buf: [4096]u8 = undefined;
    const text = fs.readAttr(path, &buf) catch |e| switch (e) {
        error.FileNotFound => return gpa.dupe(u8, ""),
        else => return e,
    };
    return gpa.dupe(u8, text);
}

/// The port snapshot: every `ports/<id>` with its attrs and links.
pub fn snapshotPorts(fs: Fs, gpa: Allocator) Error!PortSnapshot {
    const ids = fs.list(gpa, "ports") catch |e| switch (e) {
        error.FileNotFound => return .{ .entries = &.{} },
        else => return e,
    };
    defer freeNames(gpa, ids);
    var entries: std.ArrayList(PortEntry) = .empty;
    errdefer {
        var ps: PortSnapshot = .{ .entries = entries.toOwnedSlice(gpa) catch &.{} };
        ps.deinit(gpa);
    }
    var pb: [256]u8 = undefined;
    for (ids) |name| {
        const id = std.fmt.parseInt(u16, name, 10) catch continue;
        var tb: [64]u8 = undefined;
        const tr = fs.readAttr(std.fmt.bufPrint(&pb, "ports/{s}/addr_trtype", .{name}) catch unreachable, &tb) catch "";
        const adrfam = try readAttrOwned(fs, gpa, std.fmt.bufPrint(&pb, "ports/{s}/addr_adrfam", .{name}) catch unreachable);
        errdefer gpa.free(adrfam);
        const traddr = try readAttrOwned(fs, gpa, std.fmt.bufPrint(&pb, "ports/{s}/addr_traddr", .{name}) catch unreachable);
        errdefer gpa.free(traddr);
        const trsvcid = try readAttrOwned(fs, gpa, std.fmt.bufPrint(&pb, "ports/{s}/addr_trsvcid", .{name}) catch unreachable);
        errdefer gpa.free(trsvcid);
        const links = fs.list(gpa, std.fmt.bufPrint(&pb, "ports/{s}/subsystems", .{name}) catch unreachable) catch try gpa.alloc([]u8, 0);
        errdefer freeNames(gpa, links);
        std.mem.sort([]u8, links, {}, lessName);
        try entries.append(gpa, .{ .id = id, .trtype = Trtype.parse(tr), .adrfam = adrfam, .traddr = traddr, .trsvcid = trsvcid, .links = links });
    }
    return .{ .entries = try entries.toOwnedSlice(gpa) };
}

/// `8-4-4-4-12` → bytes; null when absent, malformed or the nil uuid
/// (nvmet prints the nil uuid for an unset `device_uuid`).
pub fn parseUuidAttr(text: []const u8) ?[16]u8 {
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (t.len == 0) return null;
    var out: [16]u8 = undefined;
    var n: usize = 0;
    var hi: ?u8 = null;
    for (t) |ch| {
        if (ch == '-') continue;
        const v: u8 = switch (ch) {
            '0'...'9' => ch - '0',
            'a'...'f' => ch - 'a' + 10,
            'A'...'F' => ch - 'A' + 10,
            else => return null,
        };
        if (hi) |h| {
            if (n == 16) return null;
            out[n] = (h << 4) | v;
            n += 1;
            hi = null;
        } else hi = v;
    }
    if (hi != null or n != 16) return null;
    if (std.mem.allEqual(u8, &out, 0)) return null;
    return out;
}

pub fn writeUuid(w: *std.Io.Writer, u: [16]u8) std.Io.Writer.Error!void {
    try w.print("{x}-{x}-{x}-{x}-{x}", .{ u[0..4], u[4..6], u[6..8], u[8..10], u[10..16] });
}

pub fn uuidText(buf: *[36]u8, u: [16]u8) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    writeUuid(&w, u) catch unreachable;
    return w.buffered();
}

/// The live walk: every subsystem's namespace facts, allow-list and
/// listeners (from the port snapshot's links).
pub fn walkLive(fs: Fs, gpa: Allocator) Error!Walk {
    var ports = try snapshotPorts(fs, gpa);
    errdefer ports.deinit(gpa);
    const names = fs.list(gpa, "subsystems") catch |e| switch (e) {
        error.FileNotFound => try gpa.alloc([]u8, 0),
        else => return e,
    };
    defer freeNames(gpa, names);
    std.mem.sort([]u8, names, {}, lessName);
    var shares: std.ArrayList(LiveShare) = .empty;
    errdefer {
        for (shares.items) |*s| s.deinit(gpa);
        shares.deinit(gpa);
    }
    var pb: [512]u8 = undefined;
    for (names) |nqn| {
        var s: LiveShare = .{ .subnqn = try gpa.dupe(u8, nqn), .device_path = &.{}, .device_uuid = null, .enabled = false, .nsids = &.{}, .allow_any_host = false, .allow_hosts = &.{}, .listeners = &.{} };
        errdefer s.deinit(gpa);
        var ab: [16]u8 = undefined;
        const any = fs.readAttr(std.fmt.bufPrint(&pb, "subsystems/{s}/attr_allow_any_host", .{nqn}) catch unreachable, &ab) catch "0";
        s.allow_any_host = std.mem.eql(u8, std.mem.trim(u8, any, " \n"), "1");
        const ns_names = fs.list(gpa, std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces", .{nqn}) catch unreachable) catch try gpa.alloc([]u8, 0);
        defer freeNames(gpa, ns_names);
        var nsids: std.ArrayList(u32) = .empty;
        errdefer nsids.deinit(gpa);
        for (ns_names) |n| if (std.fmt.parseInt(u32, n, 10)) |v| try nsids.append(gpa, v) else |_| {};
        std.mem.sort(u32, nsids.items, {}, lessU32);
        s.nsids = try nsids.toOwnedSlice(gpa);
        if (fs.exists(std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}", .{ nqn, NAMESPACE_INDEX }) catch unreachable)) {
            s.device_path = try readAttrOwned(fs, gpa, std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}/device_path", .{ nqn, NAMESPACE_INDEX }) catch unreachable);
            var ub: [64]u8 = undefined;
            s.device_uuid = parseUuidAttr(fs.readAttr(std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}/device_uuid", .{ nqn, NAMESPACE_INDEX }) catch unreachable, &ub) catch "");
            var eb: [16]u8 = undefined;
            const en = fs.readAttr(std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}/enable", .{ nqn, NAMESPACE_INDEX }) catch unreachable, &eb) catch "0";
            s.enabled = std.mem.eql(u8, std.mem.trim(u8, en, " \n"), "1");
        }
        const hosts = fs.list(gpa, std.fmt.bufPrint(&pb, "subsystems/{s}/allowed_hosts", .{nqn}) catch unreachable) catch try gpa.alloc([]u8, 0);
        std.mem.sort([]u8, hosts, {}, lessName);
        s.allow_hosts = hosts;
        var ls: std.ArrayList(LiveListener) = .empty;
        errdefer {
            for (ls.items) |l| {
                gpa.free(l.traddr);
                gpa.free(l.trsvcid);
            }
            ls.deinit(gpa);
        }
        for (ports.entries) |p| {
            for (p.links) |linked| if (std.mem.eql(u8, linked, nqn)) {
                try ls.append(gpa, .{ .port_id = p.id, .trtype = p.trtype, .traddr = try gpa.dupe(u8, p.traddr), .trsvcid = try gpa.dupe(u8, p.trsvcid) });
            };
        }
        s.listeners = try ls.toOwnedSlice(gpa);
        try shares.append(gpa, s);
    }
    return .{ .shares = try shares.toOwnedSlice(gpa), .ports = ports };
}

/// `target status` (N:1181-1219 minus `modules_present`, §0).
pub const Status = struct {
    configfs_mounted: bool,
    nvmet_present: bool,
    subsystems: u32,
    namespaces: u32,
    enabled_namespaces: u32,
    resv_enabled_namespaces: u32,
    ports: u32,
    hosts: u32,

    pub fn writeJson(self: Status, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{{\"configfs_mounted\":{},\"nvmet_present\":{},\"subsystems\":{d},\"namespaces\":{d},\"enabled_namespaces\":{d},\"resv_enabled_namespaces\":{d},\"ports\":{d},\"hosts\":{d}}}", .{ self.configfs_mounted, self.nvmet_present, self.subsystems, self.namespaces, self.enabled_namespaces, self.resv_enabled_namespaces, self.ports, self.hosts });
    }
};

pub fn status(fs: Fs, gpa: Allocator) Error!Status {
    var s: Status = .{ .configfs_mounted = configfsMounted(), .nvmet_present = fs.exists("subsystems"), .subsystems = 0, .namespaces = 0, .enabled_namespaces = 0, .resv_enabled_namespaces = 0, .ports = 0, .hosts = 0 };
    if (!s.nvmet_present) return s;
    const subs = try fs.list(gpa, "subsystems");
    defer freeNames(gpa, subs);
    s.subsystems = @intCast(subs.len);
    var pb: [512]u8 = undefined;
    for (subs) |nqn| {
        const nss = fs.list(gpa, std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces", .{nqn}) catch unreachable) catch continue;
        defer freeNames(gpa, nss);
        for (nss) |n| {
            s.namespaces += 1;
            var b: [16]u8 = undefined;
            if (std.mem.eql(u8, fs.readAttr(std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{s}/enable", .{ nqn, n }) catch unreachable, &b) catch "0", "1")) s.enabled_namespaces += 1;
            if (std.mem.eql(u8, fs.readAttr(std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{s}/resv_enable", .{ nqn, n }) catch unreachable, &b) catch "0", "1")) s.resv_enabled_namespaces += 1;
        }
    }
    const ports = fs.list(gpa, "ports") catch try gpa.alloc([]u8, 0);
    defer freeNames(gpa, ports);
    s.ports = @intCast(ports.len);
    const hosts = fs.list(gpa, "hosts") catch try gpa.alloc([]u8, 0);
    defer freeNames(gpa, hosts);
    s.hosts = @intCast(hosts.len);
    return s;
}

// ---------------------------------------------------------------------------
// The create recipe (§1.2) and the teardown (§1.3).
// ---------------------------------------------------------------------------

/// What `apply` presents: the subsystem, its backing, the recorded
/// identity, the allow-list and the listeners WITH their port ids.
pub const ShareSpec = struct {
    subnqn: []const u8,
    backing_path: []const u8,
    ns_uuid: [16]u8,
    /// Empty = `attr_allow_any_host = 1` (the trusted-fabric posture).
    allow_hosts: []const []const u8,
    listeners: []const Listener,
};

/// `resv_enable` present and accepted, or absent (served WITHOUT PR —
/// the loud note).
pub const Applied = struct { resv_enabled: bool };

pub const ApplyError = Error || error{
    /// A post-enable attribute write (EBUSY/EINVAL from the kernel):
    /// the object is live and its identity is frozen.
    AttrLocked,
    /// The port link refused: the address is not bindable here or the
    /// transport is absent (`CONFIG_NVME_TARGET_TCP`/`_LOOP`).
    PortLinkRefused,
};

fn attrLockedFrom(e: Error) ApplyError {
    return switch (e) {
        error.Busy, error.InvalidArgument => error.AttrLocked,
        else => e,
    };
}

fn fireWriteFailure(path: []const u8, e: anyerror) void {
    var buf: [256]u8 = undefined;
    const detail = std.fmt.bufPrint(&buf, "{s}: {s}", .{ path, @errorName(e) }) catch path;
    stats.nvmet_configfs_write_failures.fire(detail);
}

/// Write an attribute, counting a failure on the tripwire.
fn write(fs: Fs, path: []const u8, value: []const u8) Error!void {
    fs.writeAttr(path, value) catch |e| {
        fireWriteFailure(path, e);
        return e;
    };
}

/// Create (idempotent on an existing dir: a crash-window shell is
/// absorbed, N:433-437).
fn mkdirIdempotent(fs: Fs, path: []const u8) Error!void {
    fs.mkdir(path) catch |e| switch (e) {
        error.PathAlreadyExists => {},
        else => {
            fireWriteFailure(path, e);
            return e;
        },
    };
}

fn symlinkIdempotent(fs: Fs, target: []const u8, link: []const u8) Error!void {
    fs.symlink(target, link) catch |e| switch (e) {
        error.PathAlreadyExists => {},
        else => return e,
    };
}

/// Steps 2–8 of `apply_share` (N:628-724). The caller wrote the PENDING
/// record first (law 6) and allocated every port id; a failure part-way
/// returns with the record claiming whatever exists.
pub fn apply(fs: Fs, spec: ShareSpec) ApplyError!Applied {
    var pb: [512]u8 = undefined;
    const nqn = spec.subnqn;
    // 2. The subsystem and its host posture FIRST.
    try mkdirIdempotent(fs, std.fmt.bufPrint(&pb, "subsystems/{s}", .{nqn}) catch unreachable);
    try write(fs, std.fmt.bufPrint(&pb, "subsystems/{s}/attr_allow_any_host", .{nqn}) catch unreachable, if (spec.allow_hosts.len == 0) "1" else "0");
    for (spec.allow_hosts) |host| {
        try mkdirIdempotent(fs, std.fmt.bufPrint(&pb, "hosts/{s}", .{host}) catch unreachable);
        var tb: [256]u8 = undefined;
        try symlinkIdempotent(fs, std.fmt.bufPrint(&tb, "hosts/{s}", .{host}) catch unreachable, std.fmt.bufPrint(&pb, "subsystems/{s}/allowed_hosts/{s}", .{ nqn, host }) catch unreachable);
    }
    // 3–4. The ONE namespace and its backing.
    try mkdirIdempotent(fs, std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}", .{ nqn, NAMESPACE_INDEX }) catch unreachable);
    write(fs, std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}/device_path", .{ nqn, NAMESPACE_INDEX }) catch unreachable, spec.backing_path) catch |e| return attrLockedFrom(e);
    // 5. The recorded identity.
    var ub: [36]u8 = undefined;
    write(fs, std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}/device_uuid", .{ nqn, NAMESPACE_INDEX }) catch unreachable, uuidText(&ub, spec.ns_uuid)) catch |e| return attrLockedFrom(e);
    // 6. Reservations BEFORE enable (the kernel refuses them after).
    var resv = true;
    const resv_path = std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}/resv_enable", .{ nqn, NAMESPACE_INDEX }) catch unreachable;
    if (fs.exists(resv_path)) {
        write(fs, resv_path, "1") catch |e| return attrLockedFrom(e);
    } else {
        resv = false;
        stats.nvmet_shares_without_pr.inc();
        log.warn("nvmeof share {s}: this kernel's nvmet has no resv_enable — the namespace serves WITHOUT NVMe Persistent Reservations; the single-writer mount guard lands detection-grade (flock + claim) on it", .{nqn});
    }
    // 7. Enable LAST among the namespace writes.
    try write(fs, std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}/enable", .{ nqn, NAMESPACE_INDEX }) catch unreachable, "1");
    // 8. Ports, then the links that enable them.
    for (spec.listeners) |l| {
        try ensurePort(fs, l);
        var tb: [256]u8 = undefined;
        const link = std.fmt.bufPrint(&pb, "ports/{d}/subsystems/{s}", .{ l.port_id, nqn }) catch unreachable;
        fs.symlink(std.fmt.bufPrint(&tb, "subsystems/{s}", .{nqn}) catch unreachable, link) catch |e| switch (e) {
            error.PathAlreadyExists => {},
            else => {
                // An operator's address (EADDRNOTAVAIL) or a transport the
                // kernel lacks: loud, never a tripwire.
                log.warn("nvmeof share {s}: the port link {s} refused ({s}) — the address is not bindable here or the transport is absent (CONFIG_NVME_TARGET_TCP/_LOOP); the namespace is live without this listener", .{ nqn, link, @errorName(e) });
                return error.PortLinkRefused;
            },
        };
    }
    stats.nvmet_shares_applied.inc();
    return .{ .resv_enabled = resv };
}

/// `ports/<id>` with the listener's attributes; an existing object must
/// match exactly (the scan→create race closes here, N:591-597).
pub fn ensurePort(fs: Fs, l: Listener) ApplyError!void {
    var pb: [128]u8 = undefined;
    const dir = std.fmt.bufPrint(&pb, "ports/{d}", .{l.port_id}) catch unreachable;
    if (fs.exists(dir)) {
        var tb: [64]u8 = undefined;
        var ab: [64]u8 = undefined;
        var db: [64]u8 = undefined;
        var sb: [64]u8 = undefined;
        var p2: [128]u8 = undefined;
        const tr = fs.readAttr(std.fmt.bufPrint(&p2, "ports/{d}/addr_trtype", .{l.port_id}) catch unreachable, &tb) catch "";
        if (tr.len == 0) {
            // An empty shell (a crash between mkdir and the attrs): fill it.
            return fillPort(fs, l);
        }
        const fam = fs.readAttr(std.fmt.bufPrint(&p2, "ports/{d}/addr_adrfam", .{l.port_id}) catch unreachable, &ab) catch "";
        const addr = fs.readAttr(std.fmt.bufPrint(&p2, "ports/{d}/addr_traddr", .{l.port_id}) catch unreachable, &db) catch "";
        const svc = fs.readAttr(std.fmt.bufPrint(&p2, "ports/{d}/addr_trsvcid", .{l.port_id}) catch unreachable, &sb) catch "";
        var links: [0][]u8 = .{};
        const entry: PortEntry = .{ .id = l.port_id, .trtype = Trtype.parse(tr), .adrfam = fam, .traddr = addr, .trsvcid = svc, .links = &links };
        if (!entry.matches(l)) {
            log.err("nvmeof: configfs port {d} exists with ({s}, {s}, {s}, {s}) and does not match the listener — a foreign port appeared between the scan and the create; refusing", .{ l.port_id, tr, fam, addr, svc });
            return error.PortLinkRefused;
        }
        return;
    }
    try mkdirIdempotent(fs, dir);
    return fillPort(fs, l);
}

fn fillPort(fs: Fs, l: Listener) ApplyError!void {
    var pb: [128]u8 = undefined;
    try write(fs, std.fmt.bufPrint(&pb, "ports/{d}/addr_trtype", .{l.port_id}) catch unreachable, l.trtype.name());
    if (l.trtype == .loop) return;
    var ab: [64]u8 = undefined;
    var sb: [8]u8 = undefined;
    try write(fs, std.fmt.bufPrint(&pb, "ports/{d}/addr_adrfam", .{l.port_id}) catch unreachable, l.adrfam.name());
    try write(fs, std.fmt.bufPrint(&pb, "ports/{d}/addr_traddr", .{l.port_id}) catch unreachable, l.traddrText(&ab));
    try write(fs, std.fmt.bufPrint(&pb, "ports/{d}/addr_trsvcid", .{l.port_id}) catch unreachable, std.fmt.bufPrint(&sb, "{d}", .{l.trsvcid}) catch unreachable);
}

/// What the teardown removes: the record's facts (ids and hosts are the
/// LEDGER's, never the walk's) and whether each host is still named by
/// another record.
pub const TeardownSpec = struct {
    subnqn: []const u8,
    port_ids: []const u16,
    allow_hosts: []const []const u8,
    /// `hosts/<h>` is removed iff this answers false.
    host_named_elsewhere: *const fn (ctx: *const anyopaque, host: []const u8) bool,
    ctx: *const anyopaque,
};

pub const Torn = struct {
    /// The subsystem was already gone (a verified no-op).
    subsystem_absent: bool,
    ports_removed: u32,
    hosts_removed: u32,
};

fn ignoreAbsent(r: Error!void) Error!void {
    r catch |e| switch (e) {
        error.FileNotFound => {},
        else => return e,
    };
}

/// Steps 2–7 of `teardown` (N:732-855): tolerant of vanished objects.
pub fn teardown(fs: Fs, gpa: Allocator, spec: TeardownSpec) Error!Torn {
    var pb: [512]u8 = undefined;
    const nqn = spec.subnqn;
    var torn: Torn = .{ .subsystem_absent = false, .ports_removed = 0, .hosts_removed = 0 };
    // 2. The port links (recorded ids only).
    for (spec.port_ids) |id| try ignoreAbsent(fs.unlink(std.fmt.bufPrint(&pb, "ports/{d}/subsystems/{s}", .{ id, nqn }) catch unreachable));
    const sub_dir = std.fmt.bufPrint(&pb, "subsystems/{s}", .{nqn}) catch unreachable;
    if (!fs.exists(sub_dir)) {
        torn.subsystem_absent = true;
    } else {
        // 3. The namespace: disable, then rmdir (every numeric child — a
        //    hand-made shape with a second namespace is torn whole).
        var nb: [512]u8 = undefined;
        const ns_names = fs.list(gpa, std.fmt.bufPrint(&nb, "subsystems/{s}/namespaces", .{nqn}) catch unreachable) catch try gpa.alloc([]u8, 0);
        defer freeNames(gpa, ns_names);
        for (ns_names) |n| {
            const en = std.fmt.bufPrint(&nb, "subsystems/{s}/namespaces/{s}/enable", .{ nqn, n }) catch unreachable;
            if (fs.exists(en)) fs.writeAttr(en, "0") catch |e| switch (e) {
                error.FileNotFound => {},
                else => fireWriteFailure(en, e),
            };
            try ignoreAbsent(fs.rmdir(std.fmt.bufPrint(&nb, "subsystems/{s}/namespaces/{s}", .{ nqn, n }) catch unreachable));
        }
        // 4. The allowed-host links.
        const hosts = fs.list(gpa, std.fmt.bufPrint(&nb, "subsystems/{s}/allowed_hosts", .{nqn}) catch unreachable) catch try gpa.alloc([]u8, 0);
        defer freeNames(gpa, hosts);
        for (hosts) |h| try ignoreAbsent(fs.unlink(std.fmt.bufPrint(&nb, "subsystems/{s}/allowed_hosts/{s}", .{ nqn, h }) catch unreachable));
        // 5. The subsystem.
        try ignoreAbsent(fs.rmdir(sub_dir));
    }
    // 6. Last-out port removal.
    for (spec.port_ids) |id| {
        const links = fs.list(gpa, std.fmt.bufPrint(&pb, "ports/{d}/subsystems", .{id}) catch unreachable) catch continue;
        defer freeNames(gpa, links);
        if (links.len != 0) continue;
        fs.rmdir(std.fmt.bufPrint(&pb, "ports/{d}", .{id}) catch unreachable) catch |e| switch (e) {
            error.FileNotFound => {},
            else => return e,
        };
        torn.ports_removed += 1;
    }
    // 7. Hosts no other record names (EBUSY = a foreign reference; logged).
    for (spec.allow_hosts) |h| {
        if (spec.host_named_elsewhere(spec.ctx, h)) continue;
        fs.rmdir(std.fmt.bufPrint(&pb, "hosts/{s}", .{h}) catch unreachable) catch |e| switch (e) {
            error.FileNotFound => {},
            error.Busy, error.DirectoryNotEmpty => log.warn("nvmeof: hosts/{s} is referenced by a subsystem the ledger does not record — left in place", .{h}),
            else => return e,
        };
        torn.hosts_removed += 1;
    }
    stats.nvmet_shares_torn_down.inc();
    return torn;
}

/// Teardown of ONE port object by id (an adopted record's out-of-range
/// id, the crash-window's never-linked shell): link-free last-out.
pub fn removePortIfUnlinked(fs: Fs, gpa: Allocator, id: u16) Error!bool {
    var pb: [64]u8 = undefined;
    const links = fs.list(gpa, std.fmt.bufPrint(&pb, "ports/{d}/subsystems", .{id}) catch unreachable) catch |e| switch (e) {
        error.FileNotFound => return false,
        else => return e,
    };
    defer freeNames(gpa, links);
    if (links.len != 0) return false;
    try ignoreAbsent(fs.rmdir(std.fmt.bufPrint(&pb, "ports/{d}", .{id}) catch unreachable));
    return true;
}

/// Disable → enable of a live namespace: the fidelity tier's "target
/// restart" (the PTPL lapse) without touching the identity.
pub fn cycleNamespace(fs: Fs, subnqn: []const u8) Error!void {
    var pb: [512]u8 = undefined;
    const en = std.fmt.bufPrint(&pb, "subsystems/{s}/namespaces/{d}/enable", .{ subnqn, NAMESPACE_INDEX }) catch unreachable;
    try fs.writeAttr(en, "0");
    try fs.writeAttr(en, "1");
}

// ---------------------------------------------------------------------------
// The fake configfs: an in-process tree with the kernel's semantics.
// ---------------------------------------------------------------------------

pub const FakeFs = struct {
    gpa: Allocator,
    nodes: std.StringHashMapUnmanaged(Node) = .empty,
    /// Every mutation in order (the recipe tests assert the sequence).
    ops: std.ArrayList([]u8) = .empty,
    /// Inject: the kernel lacks `resv_enable` (an older nvmet).
    no_resv_enable: bool = false,
    /// Inject: the port link of this id refuses (an unbindable address).
    refuse_link_port: ?u16 = null,

    const Node = union(enum) {
        dir,
        attr: std.ArrayList(u8),
        link: []u8,
    };

    pub fn init(gpa: Allocator) !FakeFs {
        var self: FakeFs = .{ .gpa = gpa };
        try self.putDir("");
        try self.putDir("hosts");
        try self.putDir("subsystems");
        try self.putDir("ports");
        return self;
    }

    pub fn deinit(self: *FakeFs) void {
        var it = self.nodes.iterator();
        while (it.next()) |kv| {
            switch (kv.value_ptr.*) {
                .dir => {},
                .attr => |*a| a.deinit(self.gpa),
                .link => |l| self.gpa.free(l),
            }
            self.gpa.free(kv.key_ptr.*);
        }
        self.nodes.deinit(self.gpa);
        for (self.ops.items) |o| self.gpa.free(o);
        self.ops.deinit(self.gpa);
    }

    pub fn face(self: *FakeFs) Fs {
        return .{ .ctx = self, .vtable = &vtable };
    }

    fn record(self: *FakeFs, comptime fmt: []const u8, args: anytype) void {
        const line = std.fmt.allocPrint(self.gpa, fmt, args) catch return;
        self.ops.append(self.gpa, line) catch self.gpa.free(line);
    }

    fn putDir(self: *FakeFs, path: []const u8) !void {
        const k = try self.gpa.dupe(u8, path);
        errdefer self.gpa.free(k);
        try self.nodes.put(self.gpa, k, .dir);
    }

    fn putAttr(self: *FakeFs, path: []const u8, value: []const u8) !void {
        const k = try self.gpa.dupe(u8, path);
        errdefer self.gpa.free(k);
        var a: std.ArrayList(u8) = .empty;
        try a.appendSlice(self.gpa, value);
        try self.nodes.put(self.gpa, k, .{ .attr = a });
    }

    fn attrValue(self: *FakeFs, path: []const u8) ?[]const u8 {
        const n = self.nodes.getPtr(path) orelse return null;
        return switch (n.*) {
            .attr => |a| a.items,
            else => null,
        };
    }

    fn parent(path: []const u8) []const u8 {
        return if (std.mem.lastIndexOfScalar(u8, path, '/')) |i| path[0..i] else "";
    }

    fn base(path: []const u8) []const u8 {
        return if (std.mem.lastIndexOfScalar(u8, path, '/')) |i| path[i + 1 ..] else path;
    }

    fn hasChildren(self: *FakeFs, path: []const u8, objects_only: bool) bool {
        var it = self.nodes.iterator();
        while (it.next()) |kv| {
            const k = kv.key_ptr.*;
            if (k.len <= path.len + 1 or !std.mem.startsWith(u8, k, path) or k[path.len] != '/') continue;
            if (std.mem.indexOfScalar(u8, k[path.len + 1 ..], '/') != null) continue;
            if (objects_only and kv.value_ptr.* == .attr) continue;
            return true;
        }
        return false;
    }

    /// Is any symlink's target this path (configfs: EBUSY on rmdir)?
    fn isLinkTarget(self: *FakeFs, path: []const u8) bool {
        var it = self.nodes.iterator();
        while (it.next()) |kv| switch (kv.value_ptr.*) {
            .link => |t| if (std.mem.eql(u8, t, path)) return true,
            else => {},
        };
        return false;
    }

    fn portEnabled(self: *FakeFs, port_dir: []const u8) bool {
        var pb: [128]u8 = undefined;
        return self.hasChildren(std.fmt.bufPrint(&pb, "{s}/subsystems", .{port_dir}) catch unreachable, false);
    }

    fn mkdir(ctx: *anyopaque, path: []const u8) Error!void {
        const self: *FakeFs = @ptrCast(@alignCast(ctx));
        self.record("mkdir {s}", .{path});
        if (self.nodes.contains(path)) return error.PathAlreadyExists;
        const p = parent(path);
        if (!self.nodes.contains(p)) return error.FileNotFound;
        const name = base(path);
        var pb: [512]u8 = undefined;
        if (std.mem.eql(u8, p, "subsystems")) {
            if (name.len > NVMF_NQN_SIZE) return error.InvalidArgument;
            try self.putDir(path);
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/attr_allow_any_host", .{path}) catch unreachable, "0");
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/attr_serial", .{path}) catch unreachable, "");
            try self.putDir(std.fmt.bufPrint(&pb, "{s}/namespaces", .{path}) catch unreachable);
            try self.putDir(std.fmt.bufPrint(&pb, "{s}/allowed_hosts", .{path}) catch unreachable);
            return;
        }
        if (std.mem.endsWith(u8, p, "/namespaces") and std.mem.startsWith(u8, p, "subsystems/")) {
            _ = std.fmt.parseInt(u32, name, 10) catch return error.InvalidArgument;
            try self.putDir(path);
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/device_path", .{path}) catch unreachable, "");
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/device_uuid", .{path}) catch unreachable, "00000000-0000-0000-0000-000000000000");
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/device_nguid", .{path}) catch unreachable, "00000000-0000-0000-0000-000000000000");
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/enable", .{path}) catch unreachable, "0");
            if (!self.no_resv_enable) try self.putAttr(std.fmt.bufPrint(&pb, "{s}/resv_enable", .{path}) catch unreachable, "0");
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/buffered_io", .{path}) catch unreachable, "0");
            return;
        }
        if (std.mem.eql(u8, p, "ports")) {
            _ = std.fmt.parseInt(u16, name, 10) catch return error.InvalidArgument;
            try self.putDir(path);
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/addr_trtype", .{path}) catch unreachable, "");
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/addr_adrfam", .{path}) catch unreachable, "");
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/addr_traddr", .{path}) catch unreachable, "");
            try self.putAttr(std.fmt.bufPrint(&pb, "{s}/addr_trsvcid", .{path}) catch unreachable, "");
            try self.putDir(std.fmt.bufPrint(&pb, "{s}/subsystems", .{path}) catch unreachable);
            try self.putDir(std.fmt.bufPrint(&pb, "{s}/ana_groups", .{path}) catch unreachable);
            try self.putDir(std.fmt.bufPrint(&pb, "{s}/referrals", .{path}) catch unreachable);
            return;
        }
        if (std.mem.eql(u8, p, "hosts")) {
            try self.putDir(path);
            return;
        }
        // configfs refuses mkdir anywhere else (no make_group).
        return error.AccessDenied;
    }

    fn removeNode(self: *FakeFs, path: []const u8) void {
        const kv = self.nodes.fetchRemove(path) orelse return;
        switch (kv.value) {
            .dir => {},
            .attr => |a| {
                var aa = a;
                aa.deinit(self.gpa);
            },
            .link => |l| self.gpa.free(l),
        }
        self.gpa.free(kv.key);
    }

    /// Remove a dir and its ATTRIBUTES and default groups (the kernel
    /// frees them with the item); child objects refuse.
    fn rmdir(ctx: *anyopaque, path: []const u8) Error!void {
        const self: *FakeFs = @ptrCast(@alignCast(ctx));
        self.record("rmdir {s}", .{path});
        const n = self.nodes.get(path) orelse return error.FileNotFound;
        if (n != .dir) return error.NotDir;
        const p = parent(path);
        const is_object = std.mem.eql(u8, p, "subsystems") or std.mem.eql(u8, p, "ports") or std.mem.eql(u8, p, "hosts") or std.mem.endsWith(u8, p, "/namespaces");
        if (!is_object) return error.AccessDenied;
        if (self.isLinkTarget(path)) return error.Busy;
        // Child OBJECTS (a namespace, a link under ports/<id>/subsystems).
        var it = self.nodes.iterator();
        while (it.next()) |kv| {
            const k = kv.key_ptr.*;
            if (!std.mem.startsWith(u8, k, path) or k.len <= path.len or k[path.len] != '/') continue;
            const rest = k[path.len + 1 ..];
            switch (kv.value_ptr.*) {
                .attr => {},
                .link => return error.DirectoryNotEmpty,
                .dir => {
                    // Default groups are one level deep and attribute-free
                    // dirs; an object inside one is a child object.
                    if (std.mem.indexOfScalar(u8, rest, '/') != null) return error.DirectoryNotEmpty;
                },
            }
        }
        // Collect then remove (the map must not mutate under its iterator).
        var doomed: std.ArrayList([]const u8) = .empty;
        defer doomed.deinit(self.gpa);
        var it2 = self.nodes.iterator();
        while (it2.next()) |kv| {
            const k = kv.key_ptr.*;
            if (std.mem.eql(u8, k, path) or (std.mem.startsWith(u8, k, path) and k.len > path.len and k[path.len] == '/')) try doomed.append(self.gpa, k);
        }
        // Keys are owned by the map: copy before removal.
        var copies: std.ArrayList([]u8) = .empty;
        defer {
            for (copies.items) |c| self.gpa.free(c);
            copies.deinit(self.gpa);
        }
        for (doomed.items) |k| try copies.append(self.gpa, try self.gpa.dupe(u8, k));
        for (copies.items) |k| self.removeNode(k);
    }

    fn symlink(ctx: *anyopaque, target: []const u8, link: []const u8) Error!void {
        const self: *FakeFs = @ptrCast(@alignCast(ctx));
        self.record("symlink {s} -> {s}", .{ link, target });
        if (self.nodes.contains(link)) return error.PathAlreadyExists;
        const t = self.nodes.get(target) orelse return error.FileNotFound;
        if (t != .dir) return error.AccessDenied;
        const p = parent(link);
        if (!self.nodes.contains(p)) return error.FileNotFound;
        var pb: [512]u8 = undefined;
        if (std.mem.startsWith(u8, p, "ports/") and std.mem.endsWith(u8, p, "/subsystems")) {
            if (!std.mem.startsWith(u8, target, "subsystems/")) return error.AccessDenied;
            const port_dir = parent(p);
            const id = std.fmt.parseInt(u16, base(port_dir), 10) catch return error.InvalidArgument;
            if (self.refuse_link_port == id) return error.InvalidArgument;
            if (!self.portEnabled(port_dir)) {
                // The first link ENABLES the port: the transport must be set
                // and, for tcp, bindable.
                const tr = self.attrValue(std.fmt.bufPrint(&pb, "{s}/addr_trtype", .{port_dir}) catch unreachable) orelse "";
                if (tr.len == 0) return error.InvalidArgument;
                if (std.mem.eql(u8, tr, "tcp")) {
                    const addr = self.attrValue(std.fmt.bufPrint(&pb, "{s}/addr_traddr", .{port_dir}) catch unreachable) orelse "";
                    const svc = self.attrValue(std.fmt.bufPrint(&pb, "{s}/addr_trsvcid", .{port_dir}) catch unreachable) orelse "";
                    if (addr.len == 0 or svc.len == 0) return error.InvalidArgument;
                }
            }
        } else if (std.mem.startsWith(u8, p, "subsystems/") and std.mem.endsWith(u8, p, "/allowed_hosts")) {
            if (!std.mem.startsWith(u8, target, "hosts/")) return error.AccessDenied;
        } else return error.AccessDenied;
        const k = try self.gpa.dupe(u8, link);
        errdefer self.gpa.free(k);
        const v = try self.gpa.dupe(u8, target);
        errdefer self.gpa.free(v);
        try self.nodes.put(self.gpa, k, .{ .link = v });
    }

    fn unlink(ctx: *anyopaque, path: []const u8) Error!void {
        const self: *FakeFs = @ptrCast(@alignCast(ctx));
        self.record("unlink {s}", .{path});
        const n = self.nodes.get(path) orelse return error.FileNotFound;
        if (n != .link) return error.AccessDenied;
        self.removeNode(path);
    }

    fn writeAttr(ctx: *anyopaque, path: []const u8, value: []const u8) Error!void {
        const self: *FakeFs = @ptrCast(@alignCast(ctx));
        self.record("write {s} = {s}", .{ path, value });
        const n = self.nodes.getPtr(path) orelse return error.FileNotFound;
        if (n.* != .attr) return error.IsDir;
        const name = base(path);
        const dir = parent(path);
        var pb: [512]u8 = undefined;
        const v = std.mem.trimEnd(u8, value, "\n");
        if (std.mem.startsWith(u8, dir, "subsystems/") and std.mem.indexOf(u8, dir, "/namespaces/") != null) {
            const enabled = std.mem.eql(u8, self.attrValue(std.fmt.bufPrint(&pb, "{s}/enable", .{dir}) catch unreachable) orelse "0", "1");
            if (std.mem.eql(u8, name, "device_path") or std.mem.eql(u8, name, "device_uuid") or std.mem.eql(u8, name, "device_nguid")) {
                if (enabled) return error.Busy;
                if (std.mem.eql(u8, name, "device_uuid") and parseUuidAttr(v) == null and !std.mem.eql(u8, v, "00000000-0000-0000-0000-000000000000")) return error.InvalidArgument;
            } else if (std.mem.eql(u8, name, "resv_enable")) {
                if (enabled) return error.InvalidArgument;
            } else if (std.mem.eql(u8, name, "enable")) {
                if (std.mem.eql(u8, v, "1")) {
                    const dp: []const u8 = self.attrValue(std.fmt.bufPrint(&pb, "{s}/device_path", .{dir}) catch unreachable) orelse "";
                    if (dp.len == 0) return error.InvalidArgument;
                } else if (!std.mem.eql(u8, v, "0")) return error.InvalidArgument;
            }
        } else if (std.mem.startsWith(u8, dir, "ports/") and std.mem.startsWith(u8, name, "addr_")) {
            if (self.portEnabled(dir)) return error.AccessDenied;
            if (std.mem.eql(u8, name, "addr_trtype") and Trtype.parse(v) == null) return error.InvalidArgument;
            if (std.mem.eql(u8, name, "addr_adrfam") and !std.mem.eql(u8, v, "ipv4") and !std.mem.eql(u8, v, "ipv6") and !std.mem.eql(u8, v, "loop")) return error.InvalidArgument;
        } else if (std.mem.eql(u8, name, "attr_allow_any_host")) {
            if (!std.mem.eql(u8, v, "0") and !std.mem.eql(u8, v, "1")) return error.InvalidArgument;
        }
        n.attr.clearRetainingCapacity();
        try n.attr.appendSlice(self.gpa, v);
    }

    fn readAttr(ctx: *anyopaque, path: []const u8, buf: []u8) Error![]const u8 {
        const self: *FakeFs = @ptrCast(@alignCast(ctx));
        const v = self.attrValue(path) orelse {
            if (self.nodes.contains(path)) return error.IsDir;
            return error.FileNotFound;
        };
        const n = @min(v.len, buf.len);
        @memcpy(buf[0..n], v[0..n]);
        return buf[0..n];
    }

    fn list(ctx: *anyopaque, gpa: Allocator, path: []const u8) Error![][]u8 {
        const self: *FakeFs = @ptrCast(@alignCast(ctx));
        const n = self.nodes.get(path) orelse return error.FileNotFound;
        if (n != .dir) return error.NotDir;
        var out: std.ArrayList([]u8) = .empty;
        errdefer freeNames(gpa, out.toOwnedSlice(gpa) catch &.{});
        var it = self.nodes.iterator();
        while (it.next()) |kv| {
            const k = kv.key_ptr.*;
            const prefix_ok = if (path.len == 0) true else std.mem.startsWith(u8, k, path) and k.len > path.len and k[path.len] == '/';
            if (!prefix_ok or k.len == 0) continue;
            const rest = if (path.len == 0) k else k[path.len + 1 ..];
            if (rest.len == 0 or std.mem.indexOfScalar(u8, rest, '/') != null) continue;
            try out.append(gpa, try gpa.dupe(u8, rest));
        }
        std.mem.sort([]u8, out.items, {}, lessName);
        return out.toOwnedSlice(gpa);
    }

    fn exists(ctx: *anyopaque, path: []const u8) bool {
        const self: *FakeFs = @ptrCast(@alignCast(ctx));
        return self.nodes.contains(path);
    }

    const vtable: Fs.VTable = .{ .mkdir = mkdir, .rmdir = rmdir, .symlink = symlink, .unlink = unlink, .writeAttr = writeAttr, .readAttr = readAttr, .list = list, .exists = exists };

    /// The op log joined by newlines (a test's expectation string).
    pub fn opLog(self: *FakeFs, gpa: Allocator) ![]u8 {
        var out: std.Io.Writer.Allocating = .init(gpa);
        errdefer out.deinit();
        for (self.ops.items) |o| try out.writer.print("{s}\n", .{o});
        return out.toOwnedSlice();
    }

    pub fn clearOps(self: *FakeFs) void {
        for (self.ops.items) |o| self.gpa.free(o);
        self.ops.clearRetainingCapacity();
    }

    /// Every object path (dirs and links) sorted: the zero-residue
    /// snapshot of the fidelity law.
    pub fn objectCount(self: *FakeFs) usize {
        var n: usize = 0;
        var it = self.nodes.iterator();
        while (it.next()) |kv| if (kv.value_ptr.* != .attr) {
            n += 1;
        };
        // The root + hosts + subsystems + ports.
        return n - 4;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const all_ledgered: Ownership = .{ .ctx = @ptrCast(&RealFs.token), .isLedgered = struct {
    fn f(_: *const anyopaque, _: []const u8) bool {
        return true;
    }
}.f };
const none_ledgered: Ownership = .{ .ctx = @ptrCast(&RealFs.token), .isLedgered = struct {
    fn f(_: *const anyopaque, _: []const u8) bool {
        return false;
    }
}.f };

test "port ids: frozen hash64 first-candidate vectors inside the 900 slots; a relocated base shifts them; a probe wraps through the full range; ownership reuse vs foreign skip; exhaustion names the knob" {
    const l1 = try Listener.tcp("10.0.0.1", 4420);
    const l2 = try Listener.tcp("127.0.0.1", 4420);
    const l3 = try Listener.tcp("127.0.0.1", 4421);
    var kb: [96]u8 = undefined;
    try testing.expectEqualStrings("tcp:10.0.0.1:4420", l1.hashKey(&kb));
    try testing.expectEqualStrings("loop", Listener.loop.hashKey(&kb));
    // Frozen: the port's OWN vectors (quirk Q5 — not the reference's FNV ids).
    const c1 = firstCandidate(PORT_ID_BASE_DEFAULT, "tcp:10.0.0.1:4420");
    const c2 = firstCandidate(PORT_ID_BASE_DEFAULT, "tcp:127.0.0.1:4420");
    const c3 = firstCandidate(PORT_ID_BASE_DEFAULT, "loop");
    try testing.expectEqual(@as(u16, PORT_ID_BASE_DEFAULT + @as(u16, @intCast(checksum.hash64("tcp:10.0.0.1:4420") % 900))), c1);
    try testing.expect(c1 >= PORT_ID_BASE_DEFAULT and c1 < PORT_ID_BASE_DEFAULT + PORT_HASH_SLOTS);
    try testing.expect(c2 >= PORT_ID_BASE_DEFAULT and c2 < PORT_ID_BASE_DEFAULT + PORT_HASH_SLOTS);
    try testing.expect(c3 >= PORT_ID_BASE_DEFAULT and c3 < PORT_ID_BASE_DEFAULT + PORT_HASH_SLOTS);
    try testing.expect(c1 != c2);
    try testing.expectEqual(c1 + 1000, firstCandidate(54000, "tcp:10.0.0.1:4420"));
    // An empty snapshot: the candidate itself.
    try testing.expectEqual(c1, try allocatePortId(PORT_ID_BASE_DEFAULT, l1, .{ .entries = &.{} }, all_ledgered));
    // The candidate taken by a FOREIGN matching port ⇒ skipped; by OURS ⇒ reused.
    var link_name = "nqn.x".*;
    var links = [_][]u8{&link_name};
    var entries = [_]PortEntry{.{ .id = c1, .trtype = .tcp, .adrfam = "ipv4", .traddr = "10.0.0.1", .trsvcid = "4420", .links = &links }};
    const snap: PortSnapshot = .{ .entries = &entries };
    try testing.expectEqual(c1 + 1, try allocatePortId(PORT_ID_BASE_DEFAULT, l1, snap, none_ledgered));
    try testing.expectEqual(c1, try allocatePortId(PORT_ID_BASE_DEFAULT, l1, snap, all_ledgered));
    // Ours but a DIFFERENT endpoint on the candidate ⇒ skipped (one id up).
    var ours_other = [_]PortEntry{.{ .id = firstCandidate(PORT_ID_BASE_DEFAULT, "tcp:127.0.0.1:4421"), .trtype = .tcp, .adrfam = "ipv4", .traddr = "127.0.0.1", .trsvcid = "4420", .links = &links }};
    try testing.expectEqual(ours_other[0].id + 1, try allocatePortId(PORT_ID_BASE_DEFAULT, l3, .{ .entries = &ours_other }, all_ledgered));
    // A full range of foreign tcp ports: exhaustion; the probe visited every id.
    const gpa = testing.allocator;
    const full = try gpa.alloc(PortEntry, PORT_ID_RANGE);
    defer gpa.free(full);
    for (full, 0..) |*e, i| e.* = .{ .id = PORT_ID_BASE_DEFAULT + @as(u16, @intCast(i)), .trtype = .tcp, .adrfam = "ipv4", .traddr = "10.9.9.9", .trsvcid = "1", .links = &links };
    try testing.expectError(error.PortRangeExhausted, allocatePortId(PORT_ID_BASE_DEFAULT, l2, .{ .entries = full }, none_ledgered));
    // Only the top id free: reached by wrapping from a candidate inside the 900 slots.
    const partial = full[0 .. PORT_ID_RANGE - 1];
    try testing.expectEqual(@as(u16, PORT_ID_BASE_DEFAULT + PORT_ID_RANGE - 1), try allocatePortId(PORT_ID_BASE_DEFAULT, l2, .{ .entries = partial }, none_ledgered));
    // LOOP: the one existing loop port is reused whoever linked it (the
    // initiator reaches only the first loop port); none ⇒ the candidate.
    var loop_foreign = [_]PortEntry{.{ .id = 7, .trtype = .loop, .adrfam = "", .traddr = "", .trsvcid = "", .links = &links }};
    try testing.expectEqual(@as(u16, 7), try allocatePortId(PORT_ID_BASE_DEFAULT, Listener.loop, .{ .entries = &loop_foreign }, none_ledgered));
    try testing.expectEqual(c3, try allocatePortId(PORT_ID_BASE_DEFAULT, Listener.loop, .{ .entries = &.{} }, none_ledgered));
    try testing.expectEqual(@as(u16, 64536), PORT_ID_BASE_MAX);
    try testing.expectEqual(@as(u32, 65535), @as(u32, PORT_ID_BASE_MAX) + PORT_ID_RANGE - 1);
}

test "NQN grammar: a path component ≤ 223 bytes starting with nqn.; '..', '/', whitespace and a leading dot refuse; harness markers classify" {
    try validateNqn("nqn.2026-10.io.squeezefs:share-abc");
    try testing.expectError(error.Empty, validateNqn(""));
    try testing.expectError(error.LeadingDot, validateNqn(".."));
    try testing.expectError(error.HasSlash, validateNqn("nqn.a/b"));
    try testing.expectError(error.HasWhitespace, validateNqn("nqn.a b"));
    try testing.expectError(error.NotNqn, validateNqn("hello"));
    const tail: [220]u8 = @splat('a');
    try testing.expectError(error.TooLong, validateNqn("nqn." ++ tail));
    try testing.expect(isHarnessNqn("nqn.2026-10.io.squeezefs:fideli-x"));
    try testing.expect(!isHarnessNqn("nqn.2026-10.io.squeezefs:share-x"));
}

test "listeners: v4 and v6 parse to their adrfam and text; a malformed address refuses; the loop listener has no address; same-endpoint law" {
    var ab: [64]u8 = undefined;
    const v4 = try Listener.tcp("127.0.0.1", 4420);
    try testing.expectEqual(Adrfam.ipv4, v4.adrfam);
    try testing.expectEqualStrings("127.0.0.1", v4.traddrText(&ab));
    const v6 = try Listener.tcp("::1", 4420);
    try testing.expectEqual(Adrfam.ipv6, v6.adrfam);
    try testing.expectEqualStrings("::1", v6.traddrText(&ab));
    try testing.expectError(error.BadAddress, Listener.tcp("nope", 4420));
    try testing.expectEqualStrings("", Listener.loop.traddrText(&ab));
    try testing.expect(v4.sameEndpoint(try Listener.tcp("127.0.0.1", 4420)));
    try testing.expect(!v4.sameEndpoint(try Listener.tcp("127.0.0.1", 4421)));
    try testing.expect(parseUuidAttr("00000000-0000-0000-0000-000000000000") == null);
    try testing.expectEqual(@as(u8, 0x11), parseUuidAttr("11111111-2222-3333-4444-555555555555").?[0]);
    var ub: [36]u8 = undefined;
    try testing.expectEqualStrings("11111111-2222-3333-4444-555555555555", uuidText(&ub, parseUuidAttr("11111111-2222-3333-4444-555555555555").?));
}

const nqn_a = "nqn.2026-10.io.squeezefs:share-aaaa";
const uuid_a: [16]u8 = .{ 0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x33, 0x33, 0x44, 0x44, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55 };

fn noOther(_: *const anyopaque, _: []const u8) bool {
    return false;
}

test "share over the fake tree: the EXACT attribute sequence (allow_any_host first, device_path, device_uuid, resv_enable BEFORE enable, port attrs, the link last); the walk reads it back; teardown leaves zero residue" {
    const gpa = testing.allocator;
    var fake = try FakeFs.init(gpa);
    defer fake.deinit();
    const fs = fake.face();
    var l = try Listener.tcp("127.0.0.1", 4420);
    l.port_id = 53011;
    const applied = try apply(fs, .{ .subnqn = nqn_a, .backing_path = "/build/x.img", .ns_uuid = uuid_a, .allow_hosts = &.{}, .listeners = &.{l} });
    try testing.expect(applied.resv_enabled);
    const ops = try fake.opLog(gpa);
    defer gpa.free(ops);
    try testing.expectEqualStrings(
        "mkdir subsystems/" ++ nqn_a ++ "\n" ++
            "write subsystems/" ++ nqn_a ++ "/attr_allow_any_host = 1\n" ++
            "mkdir subsystems/" ++ nqn_a ++ "/namespaces/1\n" ++
            "write subsystems/" ++ nqn_a ++ "/namespaces/1/device_path = /build/x.img\n" ++
            "write subsystems/" ++ nqn_a ++ "/namespaces/1/device_uuid = 11111111-2222-3333-4444-555555555555\n" ++
            "write subsystems/" ++ nqn_a ++ "/namespaces/1/resv_enable = 1\n" ++
            "write subsystems/" ++ nqn_a ++ "/namespaces/1/enable = 1\n" ++
            "mkdir ports/53011\n" ++
            "write ports/53011/addr_trtype = tcp\n" ++
            "write ports/53011/addr_adrfam = ipv4\n" ++
            "write ports/53011/addr_traddr = 127.0.0.1\n" ++
            "write ports/53011/addr_trsvcid = 4420\n" ++
            "symlink ports/53011/subsystems/" ++ nqn_a ++ " -> subsystems/" ++ nqn_a ++ "\n",
        ops,
    );
    // Post-enable identity writes are LOCKED (EBUSY / EINVAL ⇒ AttrLocked).
    try testing.expectError(error.Busy, fs.writeAttr("subsystems/" ++ nqn_a ++ "/namespaces/1/device_uuid", "11111111-2222-3333-4444-555555555556"));
    try testing.expectError(error.InvalidArgument, fs.writeAttr("subsystems/" ++ nqn_a ++ "/namespaces/1/resv_enable", "1"));
    // The port is ENABLED by its link: addr_* refuse now.
    try testing.expectError(error.AccessDenied, fs.writeAttr("ports/53011/addr_traddr", "10.0.0.1"));
    // The walk.
    var walk = try walkLive(fs, gpa);
    defer walk.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), walk.shares.len);
    const s = walk.find(nqn_a).?;
    try testing.expect(s.enabled and s.materialized() and s.allow_any_host);
    try testing.expectEqualStrings("/build/x.img", s.device_path);
    try testing.expectEqual(uuid_a, s.device_uuid.?);
    try testing.expectEqualSlices(u32, &.{1}, s.nsids);
    try testing.expectEqual(@as(usize, 1), s.listeners.len);
    try testing.expectEqual(@as(u16, 53011), s.listeners[0].port_id);
    try testing.expectEqualStrings("127.0.0.1", s.listeners[0].traddr);
    try testing.expectEqual(@as(usize, 1), walk.ports.entries.len);
    try testing.expect(walk.ports.entries[0].matches(l));
    const st = try status(fs, gpa);
    try testing.expectEqual(@as(u32, 1), st.subsystems);
    try testing.expectEqual(@as(u32, 1), st.resv_enabled_namespaces);
    // Re-applying is idempotent on the dirs but the identity write is
    // locked: AttrLocked, counted on the tripwire (a loud error log).
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.AttrLocked, apply(fs, .{ .subnqn = nqn_a, .backing_path = "/build/x.img", .ns_uuid = uuid_a, .allow_hosts = &.{}, .listeners = &.{l} }));
    try log.testing.expectAllFired();
    try testing.expect(stats.nvmet_configfs_write_failures.load() >= 1);
    // Teardown: mirror order, zero residue.
    fake.clearOps();
    const torn = try teardown(fs, gpa, .{ .subnqn = nqn_a, .port_ids = &.{53011}, .allow_hosts = &.{}, .host_named_elsewhere = noOther, .ctx = @ptrCast(&fake) });
    try testing.expect(!torn.subsystem_absent);
    try testing.expectEqual(@as(u32, 1), torn.ports_removed);
    const ops2 = try fake.opLog(gpa);
    defer gpa.free(ops2);
    try testing.expectEqualStrings(
        "unlink ports/53011/subsystems/" ++ nqn_a ++ "\n" ++
            "write subsystems/" ++ nqn_a ++ "/namespaces/1/enable = 0\n" ++
            "rmdir subsystems/" ++ nqn_a ++ "/namespaces/1\n" ++
            "rmdir subsystems/" ++ nqn_a ++ "\n" ++
            "rmdir ports/53011\n",
        ops2,
    );
    try testing.expectEqual(@as(usize, 0), fake.objectCount());
    // A second teardown: every object vanished is a verified no-op.
    const again = try teardown(fs, gpa, .{ .subnqn = nqn_a, .port_ids = &.{53011}, .allow_hosts = &.{}, .host_named_elsewhere = noOther, .ctx = @ptrCast(&fake) });
    try testing.expect(again.subsystem_absent);
}

test "share: allow-host wiring (hosts/, the allowed_hosts link, attr 0); the loop port carries no address; a shared port is reused and removed last-out; a kernel without resv_enable serves loud without PR" {
    const gpa = testing.allocator;
    var fake = try FakeFs.init(gpa);
    defer fake.deinit();
    const fs = fake.face();
    var l = Listener.loop;
    l.port_id = 53500;
    const host = "nqn.2014-08.org.nvmexpress:uuid:host-1";
    _ = try apply(fs, .{ .subnqn = nqn_a, .backing_path = "/build/a.img", .ns_uuid = uuid_a, .allow_hosts = &.{host}, .listeners = &.{l} });
    var b: [16]u8 = undefined;
    try testing.expectEqualStrings("0", try fs.readAttr("subsystems/" ++ nqn_a ++ "/attr_allow_any_host", &b));
    try testing.expect(fs.exists("hosts/" ++ host));
    try testing.expect(fs.exists("subsystems/" ++ nqn_a ++ "/allowed_hosts/" ++ host));
    try testing.expectEqualStrings("loop", try fs.readAttr("ports/53500/addr_trtype", &b));
    try testing.expectEqualStrings("", try fs.readAttr("ports/53500/addr_traddr", &b));
    // A second share on the SAME loop port: reused (no mkdir), two links.
    const nqn_b = "nqn.2026-10.io.squeezefs:share-bbbb";
    fake.clearOps();
    _ = try apply(fs, .{ .subnqn = nqn_b, .backing_path = "/build/b.img", .ns_uuid = @splat(1), .allow_hosts = &.{}, .listeners = &.{l} });
    const ops = try fake.opLog(gpa);
    defer gpa.free(ops);
    try testing.expect(std.mem.indexOf(u8, ops, "mkdir ports/53500") == null);
    var snap = try snapshotPorts(fs, gpa);
    defer snap.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), snap.entries[0].links.len);
    // Tear A down: the port survives (B's link), the host dir goes (no other record names it).
    const torn_a = try teardown(fs, gpa, .{ .subnqn = nqn_a, .port_ids = &.{53500}, .allow_hosts = &.{host}, .host_named_elsewhere = noOther, .ctx = @ptrCast(&fake) });
    try testing.expectEqual(@as(u32, 0), torn_a.ports_removed);
    try testing.expectEqual(@as(u32, 1), torn_a.hosts_removed);
    try testing.expect(fs.exists("ports/53500"));
    try testing.expect(!fs.exists("hosts/" ++ host));
    const torn_b = try teardown(fs, gpa, .{ .subnqn = nqn_b, .port_ids = &.{53500}, .allow_hosts = &.{}, .host_named_elsewhere = noOther, .ctx = @ptrCast(&fake) });
    try testing.expectEqual(@as(u32, 1), torn_b.ports_removed);
    try testing.expectEqual(@as(usize, 0), fake.objectCount());
    // No resv_enable: the share proceeds, loudly, resv_enabled = false.
    var old = try FakeFs.init(gpa);
    defer old.deinit();
    old.no_resv_enable = true;
    const before = stats.nvmet_shares_without_pr.load();
    const applied = try apply(old.face(), .{ .subnqn = nqn_a, .backing_path = "/build/a.img", .ns_uuid = uuid_a, .allow_hosts = &.{}, .listeners = &.{l} });
    try testing.expect(!applied.resv_enabled);
    try testing.expectEqual(before + 1, stats.nvmet_shares_without_pr.load());
}

test "the crash window: a port link that refuses leaves the namespace LIVE and the second listener's shell unlinked; a foreign port squatting the id refuses at ensurePort; a linked subsystem cannot be removed (EBUSY)" {
    const gpa = testing.allocator;
    var fake = try FakeFs.init(gpa);
    defer fake.deinit();
    const fs = fake.face();
    var l1 = try Listener.tcp("127.0.0.1", 4420);
    l1.port_id = 53100;
    var l2 = try Listener.tcp("192.0.2.55", 4420);
    l2.port_id = 53101;
    fake.refuse_link_port = 53101;
    try testing.expectError(error.PortLinkRefused, apply(fs, .{ .subnqn = nqn_a, .backing_path = "/build/a.img", .ns_uuid = uuid_a, .allow_hosts = &.{}, .listeners = &.{ l1, l2 } }));
    var walk = try walkLive(fs, gpa);
    defer walk.deinit(gpa);
    const s = walk.find(nqn_a).?;
    try testing.expect(s.enabled);
    try testing.expectEqual(@as(usize, 1), s.listeners.len);
    try testing.expect(fs.exists("ports/53101"));
    try testing.expectEqual(@as(usize, 0), (walk.ports.find(53101).?).links.len);
    // The subsystem is a link target: rmdir is EBUSY until the link goes.
    try testing.expectError(error.Busy, fs.rmdir("subsystems/" ++ nqn_a));
    // The recorded-id teardown removes BOTH port objects.
    const torn = try teardown(fs, gpa, .{ .subnqn = nqn_a, .port_ids = &.{ 53100, 53101 }, .allow_hosts = &.{}, .host_named_elsewhere = noOther, .ctx = @ptrCast(&fake) });
    try testing.expectEqual(@as(u32, 2), torn.ports_removed);
    try testing.expectEqual(@as(usize, 0), fake.objectCount());
    // A foreign port on the chosen id with different attrs refuses the share at the port step.
    try fs.mkdir("ports/53100");
    try fs.writeAttr("ports/53100/addr_trtype", "tcp");
    try fs.writeAttr("ports/53100/addr_adrfam", "ipv4");
    try fs.writeAttr("ports/53100/addr_traddr", "10.9.9.9");
    try fs.writeAttr("ports/53100/addr_trsvcid", "4420");
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.PortLinkRefused, ensurePort(fs, l1));
    try log.testing.expectAllFired();
    try testing.expect(try removePortIfUnlinked(fs, gpa, 53100));
    try testing.expect(!(try removePortIfUnlinked(fs, gpa, 53100)));
}

test "the real face: the nvmet tree is walkable on the venue (configfs mounted by preflight) and the status counts read; skipped loud without root" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "the real configfs walk needs root");
    var e: ?linux.E = null;
    const r = preflight(&e);
    if (r != .ready) return testkit.skip(@src(), .no_kernel_feature, "the kernel nvmet target is not ready (configfs preflight)");
    const gpa = testing.allocator;
    var walk = try walkLive(RealFs.face(), gpa);
    defer walk.deinit(gpa);
    const st = try status(RealFs.face(), gpa);
    try testing.expect(st.nvmet_present and st.configfs_mounted);
    try testing.expectEqual(walk.shares.len, @as(usize, st.subsystems));
}

fn attrTextFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [128]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    if (Trtype.parse(text)) |t| try std.testing.expect(Trtype.parse(t.name()) == t);
    _ = parseUuidAttr(text);
}

test "fuzz: the trtype and uuid attribute parsers are total on arbitrary configfs text" {
    try fuzzkit.fuzz({}, attrTextFuzz, .{ .corpus = &.{ fuzzkit.slice("tcp\n"), fuzzkit.slice("c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1\n") } });
}
