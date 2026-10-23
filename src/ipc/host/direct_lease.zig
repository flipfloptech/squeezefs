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
//!
//! WRITE leases (decision 0003 §AY): the same map, granted writable when
//! the session's peer could WRITE every data device node, the binding has
//! write rights and every block passes W1's own state half
//! (`WritePath.probeDirectWriteLease`: sole-owned, undecorated, no
//! custody); the grant takes a slot in the session's ledger
//! (`write_lease_core.Words`, a fresh tag), marks the inode's and every
//! block key's stripe (`lease_epoch`: the mutation chokepoints recall, the
//! read path deposits nothing for the keys) and purges the keys from every
//! tier — the inode is DEVICE-TRUE while the lease stands. The recall
//! (`recallIno`, the host's `RecallHook`): `write_lease_core.recall` per
//! lease, a bounded wait on the ledger's in-flight word for a client mid-
//! write, FORCED past the bound (the session poisoned, `ipc_write_leases_
//! forced`), then the lease released. Linux (the memfd, the device stats).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const lease_epoch = @import("../../data/lease_epoch.zig");
const purge = @import("../../data/purge.zig");
const read_path = @import("../../data/read_path.zig");
const record = @import("../../meta/record.zig");
const write_lease_core = @import("../write_lease_core.zig");
const session_mod = @import("session.zig");
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

    /// The write arm's screen: every node writable by the peer too.
    pub fn grantsWrite(self: *const DirectHost, peer: rendezvous.PeerCred) bool {
        for (self.vols[0..self.n_vols]) |v| if (!rendezvous.deviceWritable(v.st_mode, v.st_uid, v.st_gid, peer.uid, peer.gid)) return false;
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

/// One write lease the host holds for a session.
pub const WriteLease = struct {
    s: *session_mod.Session,
    slot: u32,
    tag: u64,
    ino: u64,
    keys: []lease_epoch.Key,
};

/// The host's write-lease registry and the recall it runs for the
/// mutation chokepoints.
pub const WriteLeases = struct {
    gpa: Allocator,
    io: Io,
    plane: *plane_mod.DataPlane,
    /// Short sections between OS threads and lane fibers (`std.Io.Mutex`
    /// over the host's `Io`, as the dd shards' gates).
    lock: std.Io.Mutex = .init,
    recs: std.ArrayList(*WriteLease) = .empty,
    next_tag: u64 = 1,
    hook: lease_epoch.RecallHook = undefined,

    pub fn init(self: *WriteLeases, gpa: Allocator, io: Io, plane: *plane_mod.DataPlane) void {
        self.* = .{ .gpa = gpa, .io = io, .plane = plane };
        self.hook = .{ .ctx = self, .recall = &recallFn };
        lease_epoch.armRecall(&self.hook);
    }

    pub fn deinit(self: *WriteLeases) void {
        lease_epoch.disarmRecall();
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        for (self.recs.items) |r| self.dropLocked(r);
        self.recs.deinit(self.gpa);
    }

    fn words(r: *WriteLease) *write_lease_core.Words {
        return &r.s.m.statsPage().lease_ledger[r.slot];
    }

    /// The grant (under the host's lock): a free ledger slot of the
    /// session, the stripes marked, the tiers purged. Null = the ledger is
    /// full (`write` refusal).
    pub fn grant(self: *WriteLeases, s: *session_mod.Session, ino: u64, keys: []const lease_epoch.Key) ?struct { slot: u32, tag: u64 } {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        var used: [ipc_layout.WRITE_LEASES_PER_SESSION]bool = @splat(false);
        for (self.recs.items) |r| if (r.s == s) {
            used[r.slot] = true;
        };
        const slot: u32 = blk: {
            for (used, 0..) |u, i| if (!u) break :blk @intCast(i);
            return null;
        };
        const rec = self.gpa.create(WriteLease) catch return null;
        rec.* = .{ .s = s, .slot = slot, .tag = self.next_tag, .ino = ino, .keys = self.gpa.dupe(lease_epoch.Key, keys) catch {
            self.gpa.destroy(rec);
            return null;
        } };
        self.next_tag += 1;
        self.recs.append(self.gpa, rec) catch {
            self.gpa.free(rec.keys);
            self.gpa.destroy(rec);
            return null;
        };
        // Mark BEFORE the purge: a deposit racing the purge reads leased.
        lease_epoch.noteWriteLease(ino, rec.keys);
        for (rec.keys) |k| _ = purge.purgeBlockKey(self.plane.rp.tiers, read_path.BlockKey.init(k.vol_tag, k.offset));
        write_lease_core.grant(words(rec), rec.tag);
        stats.ipc_write_leases_granted.inc();
        stats.ipc_write_leases_live.add(1);
        return .{ .slot = rec.slot, .tag = rec.tag };
    }

    fn dropLocked(self: *WriteLeases, r: *WriteLease) void {
        write_lease_core.retire(words(r));
        lease_epoch.dropWriteLease(r.ino, r.keys);
        stats.ipc_write_leases_live.sub(1);
        self.gpa.free(r.keys);
        self.gpa.destroy(r);
    }

    /// Every lease of `s` goes (teardown: the client is gone, its syscalls
    /// complete with it) or every lease of `s` on `ino` (its unbind).
    pub fn release(self: *WriteLeases, s: *session_mod.Session, ino: ?u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        var i: usize = 0;
        while (i < self.recs.items.len) {
            const r = self.recs.items[i];
            if (r.s == s and (ino == null or r.ino == ino.?)) {
                self.dropLocked(r);
                _ = self.recs.swapRemove(i);
            } else i += 1;
        }
    }

    /// The recall of every write lease on `ino` (module doc).
    pub fn recallIno(self: *WriteLeases, ino: u64) void {
        var mine: [16]*WriteLease = undefined;
        var n: usize = 0;
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            var i: usize = 0;
            while (i < self.recs.items.len) {
                const r = self.recs.items[i];
                if (r.ino == ino and n < mine.len) {
                    mine[n] = r;
                    n += 1;
                    _ = self.recs.swapRemove(i);
                } else i += 1;
            }
        }
        for (mine[0..n]) |r| {
            switch (recallWords(self.io, words(r), write_lease_core.RECALL_BOUND_NS)) {
                .quiescent, .drained => {},
                .forced => {
                    log.err("ipc: write lease of ino {d} (pid {d}) FORCED: a device write in flight {d} ms after the recall — the session is poisoned; the write may sit on a block the layout no longer names", .{ ino, r.s.peer.pid, write_lease_core.RECALL_BOUND_NS / std.time.ns_per_ms });
                    r.s.poison(.write_lease_forced);
                },
            }
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            self.dropLocked(r);
        }
    }

    pub const RecallOutcome = enum { quiescent, drained, forced };

    /// One lease's recall over its ledger words: the store, then — with a
    /// write in flight — a timed POLL through the `Io` face, never a kernel
    /// futex wait: the recaller is a handler fiber more often than not, and
    /// a raw wait would hold its lane's thread (every handler behind it)
    /// for the client's device latency, while the executor's own futex
    /// table is in-process, deaf to the client's wake. Slices grow from
    /// the device's latency class to a millisecond; past `bound_ns` the
    /// lease is forced (the tripwire fires; the caller poisons).
    pub fn recallWords(io: Io, w: *write_lease_core.Words, bound_ns: u64) RecallOutcome {
        stats.ipc_write_lease_recalls.inc();
        if (write_lease_core.recall(w) == .quiescent) return .quiescent;
        const deadline = clock.monotonicNs() + bound_ns;
        var slice_ns: u64 = 20 * std.time.ns_per_us;
        while (!write_lease_core.drained(w)) {
            const now = clock.monotonicNs();
            if (now >= deadline) break;
            io.sleep(.fromNanoseconds(@intCast(@min(slice_ns, deadline - now))), .awake) catch {};
            slice_ns = @min(slice_ns * 2, std.time.ns_per_ms);
        }
        if (write_lease_core.drained(w)) {
            stats.ipc_write_lease_recalls_drained.inc();
            return .drained;
        }
        write_lease_core.force(w);
        stats.ipc_write_leases_forced.fire("a client's direct write in flight past the recall bound");
        return .forced;
    }

    fn recallFn(ctx: *anyopaque, ino: u64) void {
        const self: *WriteLeases = @ptrCast(@alignCast(ctx));
        self.recallIno(ino);
    }
};

/// The lease of `nodeid` (a kernel nodeid: routed to its key ino as the
/// plane's tables are) into `out` (`CTL_MSG_MAX` bytes); `want_write` asks
/// a WRITE lease (granted iff `wl` admits it: every block W1-eligible, a
/// ledger slot free), refused `write` otherwise — the client holds a READ
/// lease by asking again without the flag.
pub fn buildLease(plane: *plane_mod.DataPlane, nodeid: u64, want_write: bool, wl: ?*WriteLeases, s: *session_mod.Session, out: []u8) LeaseVerdict {
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
    var keys: [wire.LEASE_MAX_ENTRIES]lease_epoch.Key = undefined;
    var n_keys: usize = 0;
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
        keys[n_keys] = .{ .vol_tag = m.vol_tag, .offset = m.offset };
        n_keys += 1;
    }
    var writable = false;
    var slot: u32 = 0;
    var tag: u64 = 0;
    if (want_write) {
        const w = wl orelse return .{ .refused = .write };
        // An immutable or append-only inode takes no direct write (the
        // kernel path answers its every write EPERM; a lease would let
        // the client's bytes land with no handler to refuse them). The
        // flag set recalls a standing lease (`kv_fs.ioctl`).
        if (plane.inodeRecordFlags(ino)) |fl| if (fl & record.INODE_FLAGS_ATTR != 0) return .{ .refused = .write };
        for (0..n_blocks) |b| {
            if (entries[b].vol == wire.LeaseEntry.UNMAPPED) continue;
            switch (plane.wp.probeDirectWriteLease(w.io, ino, @intCast(b))) {
                .eligible => {},
                .busy => return .{ .refused = .moved },
                .ineligible => return .{ .refused = .write },
            }
        }
        if (lease_epoch.load(ino) != e1) return .{ .refused = .moved };
        const g = w.grant(s, ino, keys[0..n_keys]) orelse return .{ .refused = .write };
        writable = true;
        slot = g.slot;
        tag = g.tag;
    }
    if (lease_epoch.load(ino) != e1) {
        if (writable) wl.?.release(s, ino);
        return .{ .refused = .moved };
    }
    const l: wire.LeaseOk = .{ .ino = ino, .size = info.size, .epoch = e1, .block_size = bs, .writable = writable, .slot = slot, .tag = tag, .entries = entries[0..n_blocks] };
    const n = l.encode(out) catch {
        if (writable) wl.?.release(s, ino);
        return .{ .refused = .kind };
    };
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
        .write => stats.ipc_leases_refused_write.inc(),
        _ => {},
    }
}

// ---------------------------------------------------------------------------
// Tests: the recall over the ledger words (the host's part of the Dekker;
// the client's part is `write_lease_core` and the model).
// ---------------------------------------------------------------------------

const testing = std.testing;

const Exiter = struct {
    w: *write_lease_core.Words,
    delay_ns: u64,
    fn run(self: Exiter) void {
        const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(self.delay_ns) };
        _ = linux.nanosleep(&ts, null);
        _ = write_lease_core.exitWrite(self.w);
    }
};

test "the recall over the ledger: quiescent at once with nothing in flight; a write in flight drains when the client exits (the recaller polled, never woke a lane's thread); a client that never exits is FORCED at the bound — the tripwire fires and the words read forced" {
    var w: write_lease_core.Words = .{};
    write_lease_core.grant(&w, 1);
    const recalls0 = stats.ipc_write_lease_recalls.load();
    try testing.expectEqual(WriteLeases.RecallOutcome.quiescent, WriteLeases.recallWords(testing.io, &w, 100 * std.time.ns_per_ms));
    try testing.expectEqual(recalls0 + 1, stats.ipc_write_lease_recalls.load());
    try testing.expect(!write_lease_core.enterWrite(&w, 1));
    // In flight, exits 30 ms in.
    var d: write_lease_core.Words = .{};
    write_lease_core.grant(&d, 2);
    try testing.expect(write_lease_core.enterWrite(&d, 2));
    const drained0 = stats.ipc_write_lease_recalls_drained.load();
    const t = try std.Thread.spawn(.{}, Exiter.run, .{Exiter{ .w = &d, .delay_ns = 30 * std.time.ns_per_ms }});
    const t0 = clock.monotonicNs();
    try testing.expectEqual(WriteLeases.RecallOutcome.drained, WriteLeases.recallWords(testing.io, &d, std.time.ns_per_s));
    t.join();
    try testing.expect(clock.monotonicNs() - t0 < 500 * std.time.ns_per_ms);
    try testing.expectEqual(drained0 + 1, stats.ipc_write_lease_recalls_drained.load());
    try testing.expect(!write_lease_core.landedUnderForce(&d));
    // Wedged: never exits; forced at a short bound.
    var f: write_lease_core.Words = .{};
    write_lease_core.grant(&f, 3);
    try testing.expect(write_lease_core.enterWrite(&f, 3));
    const forced0 = stats.ipc_write_leases_forced.load();
    // The tripwire's one loud line per site per second is the contract.
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(WriteLeases.RecallOutcome.forced, WriteLeases.recallWords(testing.io, &f, 50 * std.time.ns_per_ms));
    try log.testing.expectAllFired();
    try testing.expectEqual(forced0 + 1, stats.ipc_write_leases_forced.load());
    try testing.expect(write_lease_core.landedUnderForce(&f));
    // The wedged client's late exit: told it was forced.
    try testing.expect(write_lease_core.exitWrite(&f));
}
