//! The direct arm's client half (charter §7 "The direct arm", decision 0003
//! §AX): a TRUSTED client reads the data device itself. The daemon granted
//! the arm at HELLO (`SessionOk.features & FEATURE_DIRECT`: this uid can
//! open every data device node); `DirectHello` brings the lease-epoch page
//! (a sealed memfd, mapped read-only here) and the device paths, which the
//! client opens `O_RDONLY | O_DIRECT` itself — the kernel's permission
//! check is the final word, a refusal there ends the arm for the session
//! (the ring serves it). Per bound fd a LAYOUT LEASE — the file's block map
//! as `{volume, device offset}` under the stripe's epoch — is asked once
//! and cached; a read of one aligned, mapped, in-size block is ONE `pread`
//! on the device from the caller's own thread, then the RECHECK: the
//! stripe's epoch still reads the lease's (loaded AFTER the bytes are in
//! hand — the FUSE direct leg's validation after the DMA, charter §6.3);
//! else the lease is dropped and asked again, and past two attempts the
//! ring serves the op. Nothing here fabricates: a short or failed device
//! read is the ring's op.
//!
//! Raw `std.os.linux` only (the client library's law). The lease table is
//! a fixed array of refcounted rows (a reader pins the lease across its
//! I/O; the replacer drops the table's share; the last share unmaps) so a
//! replacement under a racing reader frees nothing in use. A refused lease
//! parks the binding for a while (`REFUSE_BACKOFF_NS`): a file the arm does
//! not serve costs one ask per backoff, never one per op.

const std = @import("std");
const linux = std.os.linux;
const Atomic = std.atomic.Value;

const wire = @import("wire.zig");
const shm = @import("shm.zig");
const rl = @import("rendezvous_linux.zig");
const layout = @import("layout.zig");
const lease_epoch = @import("../data/lease_epoch.zig");
const rsc = @import("../data/read_size_class.zig");
const clock = @import("../core/clock.zig");

/// Leases cached per session (bindings beyond it ride the ring).
pub const MAX_LEASES: usize = 64;
/// The device grain every direct I/O is aligned to.
pub const GRAIN: u64 = rsc.LBA_GRAIN;
/// A refused lease (kind, custody, moved) is not asked again for this long.
pub const REFUSE_BACKOFF_NS: u64 = 200 * std.time.ns_per_ms;
/// Rechecks before an op gives up on the arm (a publish per attempt).
pub const MAX_ATTEMPTS: u8 = 2;

const page_allocator = std.heap.page_allocator;

/// One cached lease: the header, then its entries.
pub const Lease = struct {
    refs: Atomic(u32),
    ino: u64,
    size: u64,
    epoch: u64,
    block_size: u64,
    n: u32,
    /// `[n]LeaseEntry` right after this struct in the same allocation.
    fn entries(self: *const Lease) []const wire.LeaseEntry {
        const base: [*]const u8 = @ptrCast(self);
        const p: [*]const wire.LeaseEntry = @ptrCast(@alignCast(base + @sizeOf(Lease)));
        return p[0..self.n];
    }

    fn bytesFor(n: usize) usize {
        return @sizeOf(Lease) + n * @sizeOf(wire.LeaseEntry);
    }

    fn create(l: wire.LeaseOk) ?*Lease {
        const mem = page_allocator.alignedAlloc(u8, .fromByteUnits(@alignOf(Lease)), bytesFor(l.entries.len)) catch return null;
        const self: *Lease = @ptrCast(@alignCast(mem.ptr));
        self.* = .{ .refs = .init(1), .ino = l.ino, .size = l.size, .epoch = l.epoch, .block_size = l.block_size, .n = @intCast(l.entries.len) };
        const dst: [*]wire.LeaseEntry = @ptrCast(@alignCast(mem.ptr + @sizeOf(Lease)));
        for (l.entries, 0..) |e, i| dst[i] = e;
        return self;
    }

    /// Pin for a read; false = dying (refs hit 0).
    fn pin(self: *Lease) bool {
        var cur = self.refs.load(.acquire);
        while (cur != 0) {
            cur = self.refs.cmpxchgWeak(cur, cur + 1, .acq_rel, .acquire) orelse return true;
        }
        return false;
    }

    pub fn drop(self: *Lease) void {
        if (self.refs.fetchSub(1, .acq_rel) == 1) {
            const base: [*]align(@alignOf(Lease)) u8 = @ptrCast(@alignCast(self));
            page_allocator.free(base[0..bytesFor(self.n)]);
        }
    }
};

const Row = struct {
    binding: Atomic(u64) = .init(0),
    lease: Atomic(?*Lease) = .init(null),
    /// Monotonic ns until which no lease is asked for this binding.
    refused_until: Atomic(u64) = .init(0),
};

pub const Stats = struct {
    ops: *Atomic(u64),
    bytes: *Atomic(u64),
    rechecks_failed: *Atomic(u64),
    asks: *Atomic(u64),
};

pub const Direct = struct {
    page: []align(layout.PAGE_BYTES) const u8,
    rows: [*]const lease_epoch.Row,
    dev_fds: [wire.DIRECT_MAX_VOLS]linux.fd_t,
    n_vols: u32,
    block_size: u64,
    table: [MAX_LEASES]Row = @splat(.{}),

    pub const EstablishError = error{ Refused, Protocol, Unsealed, Map, DeviceOpen };

    /// `DirectHello` over the ctl socket (the caller holds the ctl lock):
    /// the page mapped, every device opened. Any failure leaves nothing
    /// open or mapped.
    pub fn establish(ctl: linux.fd_t) EstablishError!Direct {
        const req: wire.DirectHello = .{};
        rl.sendCtl(ctl, wire.frameBytes(&req), null) catch return error.Protocol;
        var buf: [@sizeOf(wire.DirectOk)]u8 = undefined;
        const r = rl.recvCtl(ctl, &buf) catch return error.Protocol;
        const frame = buf[0..r.len];
        switch (wire.tagOf(frame) catch return error.Protocol) {
            .lease_refused => {
                if (r.fd) |f| _ = linux.close(f);
                return error.Refused;
            },
            .direct_ok => {},
            else => {
                if (r.fd) |f| _ = linux.close(f);
                return error.Protocol;
            },
        }
        const ok = wire.decodeFixed(wire.DirectOk, frame) catch {
            if (r.fd) |f| _ = linux.close(f);
            return error.Protocol;
        };
        const fd = r.fd orelse return error.Protocol;
        defer _ = linux.close(fd);
        shm.checkSeals(fd) catch return error.Unsealed;
        const size = shm.fileSize(fd) catch return error.Protocol;
        if (size != ok.page_bytes or size != lease_epoch.PAGE_BYTES) return error.Protocol;
        const p = linux.mmap(null, lease_epoch.PAGE_BYTES, .{ .READ = true }, .{ .TYPE = .SHARED }, fd, 0);
        if (linux.errno(p) != .SUCCESS) return error.Map;
        var self: Direct = .{
            .page = @as([*]align(layout.PAGE_BYTES) const u8, @ptrFromInt(p))[0..lease_epoch.PAGE_BYTES],
            .rows = @ptrFromInt(p),
            .dev_fds = @splat(-1),
            .n_vols = ok.n_vols,
            .block_size = ok.block_size,
        };
        errdefer self.closeAll();
        for (0..ok.n_vols) |i| {
            const path = ok.path(i);
            if (path.len == 0) return error.Protocol;
            var z: [wire.DEVICE_PATH_LEN]u8 = @splat(0);
            @memcpy(z[0..path.len], path);
            const rc = linux.open(@ptrCast(&z), .{ .ACCMODE = .RDONLY, .DIRECT = true, .CLOEXEC = true }, 0);
            if (linux.errno(rc) != .SUCCESS) return error.DeviceOpen;
            self.dev_fds[i] = @intCast(rc);
        }
        return self;
    }

    fn closeAll(self: *Direct) void {
        for (&self.dev_fds) |*f| if (f.* >= 0) {
            _ = linux.close(f.*);
            f.* = -1;
        };
        _ = linux.munmap(self.page.ptr, self.page.len);
    }

    pub fn teardown(self: *Direct) void {
        for (&self.table) |*row| if (row.lease.swap(null, .acq_rel)) |l| l.drop();
        self.closeAll();
    }

    fn rowOf(self: *Direct, binding: u64) ?*Row {
        for (&self.table) |*row| if (row.binding.load(.acquire) == binding) return row;
        return null;
    }

    /// The pinned lease of `binding`, or null (none cached / dying).
    fn pinned(self: *Direct, binding: u64) ?*Lease {
        const row = self.rowOf(binding) orelse return null;
        const l = row.lease.load(.acquire) orelse return null;
        return if (l.pin()) l else null;
    }

    /// Install `l` for `binding` (the table's share is `l`'s initial ref);
    /// the previous lease's share drops. False = no row free (the lease
    /// is dropped; the ring serves this binding).
    fn install(self: *Direct, binding: u64, l: *Lease) bool {
        const row = self.rowOf(binding) orelse blk: {
            for (&self.table) |*row| {
                if (row.binding.cmpxchgStrong(0, binding, .acq_rel, .acquire) == null) break :blk row;
            }
            l.drop();
            return false;
        };
        if (row.lease.swap(l, .acq_rel)) |old| old.drop();
        return true;
    }

    /// `lease` was found stale: drop the table's share iff it still holds it.
    fn invalidate(self: *Direct, binding: u64, lease: *Lease) void {
        const row = self.rowOf(binding) orelse return;
        if (row.lease.cmpxchgStrong(lease, null, .acq_rel, .acquire) == null) lease.drop();
    }

    /// The binding closed: its row goes.
    pub fn forget(self: *Direct, binding: u64) void {
        const row = self.rowOf(binding) orelse return;
        if (row.lease.swap(null, .acq_rel)) |l| l.drop();
        row.refused_until.store(0, .release);
        row.binding.store(0, .release);
    }

    fn backoff(self: *Direct, binding: u64) void {
        const until = clock.monotonicNs() + REFUSE_BACKOFF_NS;
        if (self.rowOf(binding)) |row| {
            row.refused_until.store(until, .release);
            return;
        }
        for (&self.table) |*row| if (row.binding.cmpxchgStrong(0, binding, .acq_rel, .acquire) == null) {
            row.refused_until.store(until, .release);
            return;
        };
    }

    fn backedOff(self: *Direct, binding: u64) bool {
        const row = self.rowOf(binding) orelse return false;
        return clock.monotonicNs() < row.refused_until.load(.acquire);
    }

    /// The ask over the ctl socket (the caller holds the ctl lock): the
    /// decoded lease installed and pinned, or null (refused → backoff;
    /// a protocol failure → backoff too).
    pub fn ask(self: *Direct, ctl: linux.fd_t, binding: u64, stats: Stats) ?*Lease {
        _ = stats.asks.fetchAdd(1, .monotonic);
        const req: wire.LeaseReq = .{ .binding_id = binding };
        rl.sendCtl(ctl, wire.frameBytes(&req), null) catch {
            self.backoff(binding);
            return null;
        };
        const buf = page_allocator.alloc(u8, wire.CTL_MSG_MAX) catch {
            self.backoff(binding);
            return null;
        };
        defer page_allocator.free(buf);
        const r = rl.recvCtl(ctl, buf) catch {
            self.backoff(binding);
            return null;
        };
        if (r.fd) |f| _ = linux.close(f);
        const frame = buf[0..r.len];
        const l = wire.LeaseOk.decode(frame) catch {
            self.backoff(binding);
            return null;
        };
        if (l.block_size != self.block_size) {
            self.backoff(binding);
            return null;
        }
        const lease = Lease.create(l) orelse {
            self.backoff(binding);
            return null;
        };
        if (!lease.pin()) unreachable;
        if (!self.install(binding, lease)) {
            // The table is full: the pin is the only share; the ring serves.
            lease.drop();
            return null;
        }
        return lease;
    }

    pub const Verdict = union(enum) { served: usize, ring };

    /// The direct read: `.served` with the bytes in `dst`, or `.ring`.
    /// `ask` runs under the caller's ctl lock through `asker`.
    pub fn pread(self: *Direct, binding: u64, dst: []u8, offset: u64, stats: Stats, asker: anytype) Verdict {
        if (dst.len == 0) return .ring;
        if (offset % GRAIN != 0 or @intFromPtr(dst.ptr) % GRAIN != 0) return .ring;
        var attempt: u8 = 0;
        while (attempt < MAX_ATTEMPTS) : (attempt += 1) {
            const l = self.pinned(binding) orelse blk: {
                if (self.backedOff(binding)) return .ring;
                break :blk asker.ask(self, binding, stats) orelse return .ring;
            };
            defer l.drop();
            if (offset >= l.size) return .{ .served = 0 };
            const want: usize = @intCast(@min(dst.len, l.size - offset));
            // Whole grains only: a tail short of the grain is the ring's.
            if (want % GRAIN != 0) return .ring;
            const bs = l.block_size;
            const b = offset / bs;
            if ((offset + want - 1) / bs != b or b >= l.n) return .ring;
            const e = l.entries()[b];
            if (e.vol == wire.LeaseEntry.UNMAPPED or e.vol >= self.n_vols) return .ring;
            const fd = self.dev_fds[e.vol];
            if (fd < 0) return .ring;
            const rc = linux.pread(fd, dst.ptr, want, @intCast(e.dev_off + (offset - b * bs)));
            if (linux.errno(rc) != .SUCCESS or rc != want) return .ring;
            // The recheck, bytes in hand.
            if (lease_epoch.loadFrom(self.rows, l.ino) == l.epoch) {
                _ = stats.ops.fetchAdd(1, .monotonic);
                _ = stats.bytes.fetchAdd(want, .monotonic);
                return .{ .served = want };
            }
            _ = stats.rechecks_failed.fetchAdd(1, .monotonic);
            self.invalidate(binding, l);
        }
        return .ring;
    }
};
