//! The direct arm's host half (charter §7 "The direct arm", decision 0003
//! §AX): the lease-epoch page every direct session maps read-only, the
//! device screen a session passes at HELLO, the `DirectOk` the client
//! builds its device table from, and the LAYOUT LEASE builder — a resident
//! striped passthrough file's whole block map as `{volume ordinal, device
//! offset}` per block, under the stripe's epoch read BEFORE the map (a
//! publish landing between the two reads answers `moved`; one landing after
//! fails the client's first recheck): the client's I/O is right iff the
//! stripe still reads that epoch after its bytes are in hand. Custody
//! refuses the lease: a parked write holds bytes the device does not.
//! Linux (the memfd, the device stats).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;

const log = @import("../../core/log.zig");
const lease_epoch = @import("../../data/lease_epoch.zig");
const layout_mod = @import("../../data/layout.zig");
const plane_mod = @import("../../data/plane.zig");
const route_table = @import("../../meta/routing/route_table.zig");
const wire = @import("../wire.zig");
const ipc_layout = @import("../layout.zig");
const rendezvous = @import("../rendezvous.zig");
const stats = @import("stats.zig").stats;

comptime {
    if (builtin.os.tag != .linux) @compileError("ipc/host/direct_lease is Linux only; gate the import on builtin.os.tag");
}

/// One data volume as the screen and the client see it.
pub const Vol = struct {
    path: [wire.DEVICE_PATH_LEN]u8,
    st_mode: u32,
    st_uid: u32,
    st_gid: u32,
};

pub const DirectHost = struct {
    memfd: linux.fd_t,
    page: []align(ipc_layout.PAGE_BYTES) u8,
    vols: [wire.DIRECT_MAX_VOLS]Vol,
    n_vols: u32,
    block_size: u64,

    pub const InitError = error{ TooManyVolumes, PathTooLong, DeviceStat, Memfd, Map };

    /// Arm: the sealed page, the volumes' paths and node facts.
    pub fn init(plane: *plane_mod.DataPlane, block_size: u64) InitError!DirectHost {
        const n = plane.vols.n();
        if (n > wire.DIRECT_MAX_VOLS) return error.TooManyVolumes;
        var self: DirectHost = .{ .memfd = -1, .page = undefined, .vols = undefined, .n_vols = n, .block_size = block_size };
        for (0..n) |i| {
            const dev = plane.devs[i] orelse return error.DeviceStat;
            if (dev.path.len >= wire.DEVICE_PATH_LEN) return error.PathTooLong;
            var st: linux.Statx = undefined;
            if (linux.errno(linux.statx(linux.AT.FDCWD, dev.path, 0, .{ .TYPE = true, .MODE = true, .UID = true, .GID = true }, &st)) != .SUCCESS) return error.DeviceStat;
            self.vols[i] = .{ .path = @splat(0), .st_mode = st.mode, .st_uid = st.uid, .st_gid = st.gid };
            @memcpy(self.vols[i].path[0..dev.path.len], dev.path);
        }
        const fd_raw = linux.memfd_create("sqz-il-lease", linux.MFD.CLOEXEC | linux.MFD.ALLOW_SEALING);
        if (linux.errno(fd_raw) != .SUCCESS) return error.Memfd;
        const fd: linux.fd_t = @intCast(fd_raw);
        errdefer _ = linux.close(fd);
        if (linux.errno(linux.ftruncate(fd, @intCast(lease_epoch.PAGE_BYTES))) != .SUCCESS) return error.Memfd;
        if (linux.errno(linux.fcntl(fd, linux.F.ADD_SEALS, linux.F.SEAL_SHRINK | linux.F.SEAL_GROW | linux.F.SEAL_SEAL)) != .SUCCESS) return error.Memfd;
        const p = linux.mmap(null, lease_epoch.PAGE_BYTES, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
        if (linux.errno(p) != .SUCCESS) return error.Map;
        self.memfd = fd;
        self.page = @as([*]align(ipc_layout.PAGE_BYTES) u8, @ptrFromInt(p))[0..lease_epoch.PAGE_BYTES];
        lease_epoch.share(@ptrCast(@alignCast(self.page.ptr)));
        return self;
    }

    pub fn deinit(self: *DirectHost) void {
        lease_epoch.unshare();
        _ = linux.munmap(self.page.ptr, self.page.len);
        _ = linux.close(self.memfd);
        self.* = undefined;
    }

    /// The HELLO screen: the peer could open every data device node.
    pub fn grants(self: *const DirectHost, peer: rendezvous.PeerCred) bool {
        for (self.vols[0..self.n_vols]) |v| if (!rendezvous.deviceReadable(v.st_mode, v.st_uid, v.st_gid, peer.uid, peer.gid)) return false;
        return true;
    }

    pub fn directOk(self: *const DirectHost) wire.DirectOk {
        var ok: wire.DirectOk = .{ .n_vols = self.n_vols, .block_size = self.block_size, .page_bytes = lease_epoch.PAGE_BYTES };
        for (self.vols[0..self.n_vols], 0..) |v, i| ok.paths[i] = v.path;
        return ok;
    }
};

pub const LeaseVerdict = union(enum) {
    /// The frame's byte length in `out`.
    ok: usize,
    refused: wire.LeaseRefuseClass,
};

/// The lease of `nodeid` (a kernel nodeid: routed to its key ino as the
/// plane's tables are) into `out` (`CTL_MSG_MAX` bytes).
pub fn buildLease(plane: *plane_mod.DataPlane, nodeid: u64, out: []u8) LeaseVerdict {
    if (!route_table.isRoutable(nodeid)) return .{ .refused = .kind };
    const ino = route_table.effectiveIno(nodeid);
    const rp = plane.rp;
    if (!rp.cfg.passthrough) return .{ .refused = .kind };
    // The epoch FIRST, then the map (module doc).
    const e1 = lease_epoch.load(ino);
    const info = rp.meta.file(rp.meta.ctx, ino) orelse return .{ .refused = .kind };
    if (info.kind != .striped) return .{ .refused = .kind };
    const ws = rp.write_side;
    if (ws.has_retained_extents(ws.ctx, ino)) return .{ .refused = .custody };
    const bs = plane.block_size;
    const n_blocks: usize = @intCast(@min((info.size + bs - 1) / bs, wire.LEASE_MAX_ENTRIES));
    var entries: [wire.LEASE_MAX_ENTRIES]wire.LeaseEntry = undefined;
    for (0..n_blocks) |b| {
        if (ws.has_custody(ws.ctx, ino, b)) return .{ .refused = .custody };
        const m = rp.meta.binding(rp.meta.ctx, ino, b) orelse {
            entries[b] = .{ .vol = wire.LeaseEntry.UNMAPPED, .dev_off = 0 };
            continue;
        };
        if (!m.whole_block or m.damaged) return .{ .refused = .kind };
        const vol = plane.vols.byTag(m.vol_tag) orelse return .{ .refused = .kind };
        if (vol.ordinal >= wire.DIRECT_MAX_VOLS or !vol.passthrough) return .{ .refused = .kind };
        entries[b] = .{ .vol = @intCast(vol.ordinal), .dev_off = m.offset };
    }
    if (lease_epoch.load(ino) != e1) return .{ .refused = .moved };
    const l: wire.LeaseOk = .{ .ino = ino, .size = info.size, .epoch = e1, .block_size = bs, .entries = entries[0..n_blocks] };
    const n = l.encode(out) catch return .{ .refused = .kind };
    stats.ipc_leases_granted.inc();
    return .{ .ok = n };
}

pub fn countRefusal(class: wire.LeaseRefuseClass) void {
    switch (class) {
        .disabled => stats.ipc_leases_refused_disabled.inc(),
        .kind => stats.ipc_leases_refused_kind.inc(),
        .custody => stats.ipc_leases_refused_custody.inc(),
        .binding => stats.ipc_leases_refused_binding.inc(),
        .moved => stats.ipc_leases_refused_moved.inc(),
        _ => {},
    }
}
