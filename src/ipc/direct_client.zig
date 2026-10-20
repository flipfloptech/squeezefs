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
//!
//! WRITES (decision 0003 §AY): under a WRITABLE lease (the daemon's grant
//! with a ledger slot and a tag) an aligned, in-size, single-block write is
//! ONE `pwrite` on the device, bracketed by the recall protocol
//! (`write_lease_core.enterWrite` / `exitWrite` on the ledger's words: the
//! daemon mutates the inode's layout only once no write is in flight);
//! the postlude — the dirty mark, the kernel's cache, the fsync touch —
//! rides a BATCHED `WriteNote` (every `NOTE_BATCH` writes or `NOTE_AGE_NS`,
//! at the lease's drop, and acked at `fsync`). A write that landed under a
//! FORCED recall (the daemon gave up waiting: a wedged client) answers EIO.

const std = @import("std");
const linux = std.os.linux;
const Atomic = std.atomic.Value;

const wire = @import("wire.zig");
const shm = @import("shm.zig");
const rl = @import("rendezvous_linux.zig");
const layout = @import("layout.zig");
const write_lease_core = @import("write_lease_core.zig");
const futex = @import("../sync/futex.zig");
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
/// The postlude note's batch: writes per note, and the age at which a
/// partial batch is sent anyway (checked at the next write; the lease's
/// drop and `fsync` flush whatever is pending). 64 writes amortize the ctl
/// round trip to under a microsecond per write; 10 ms bounds the dirty
/// mark's lag behind an acked write on a trickling writer.
pub const NOTE_BATCH: u32 = 64;
pub const NOTE_AGE_NS: u64 = 10 * std.time.ns_per_ms;

const page_allocator = std.heap.page_allocator;

/// One cached lease: the header, then its entries.
pub const Lease = struct {
    refs: Atomic(u32),
    ino: u64,
    size: u64,
    epoch: u64,
    block_size: u64,
    n: u32,
    /// The write arm's grant (`slot`/`tag` of the session ledger), and
    /// whether a WRITE lease was asked for (refused ⇒ the file's writes are
    /// the ring's until the lease turns over; never re-asked per write).
    writable: bool,
    write_asked: bool,
    slot: u32,
    tag: u64,
    /// The pending postlude note.
    note_count: Atomic(u32) = .init(0),
    note_max_end: Atomic(u64) = .init(0),
    note_vols: Atomic(u32) = .init(0),
    note_first_ns: Atomic(u64) = .init(0),
    /// Volumes written since the last `fsync` (their `fdatasync` is owed).
    dirty_vols: Atomic(u32) = .init(0),
    /// `[n]LeaseEntry` right after this struct in the same allocation.
    fn entries(self: *const Lease) []const wire.LeaseEntry {
        const base: [*]const u8 = @ptrCast(self);
        const p: [*]const wire.LeaseEntry = @ptrCast(@alignCast(base + @sizeOf(Lease)));
        return p[0..self.n];
    }

    fn bytesFor(n: usize) usize {
        return @sizeOf(Lease) + n * @sizeOf(wire.LeaseEntry);
    }

    fn create(l: wire.LeaseOk, write_asked: bool) ?*Lease {
        const mem = page_allocator.alignedAlloc(u8, .fromByteUnits(@alignOf(Lease)), bytesFor(l.entries.len)) catch return null;
        const self: *Lease = @ptrCast(@alignCast(mem.ptr));
        self.* = .{ .refs = .init(1), .ino = l.ino, .size = l.size, .epoch = l.epoch, .block_size = l.block_size, .n = @intCast(l.entries.len), .writable = l.writable, .write_asked = write_asked, .slot = l.slot, .tag = l.tag };
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
    writes: *Atomic(u64),
    write_bytes: *Atomic(u64),
    write_notes: *Atomic(u64),
    writes_recalled: *Atomic(u64),
    /// The session's ledger (the stats page's protocol region).
    ledger: *[layout.WRITE_LEASES_PER_SESSION]write_lease_core.Words,
};

pub const Direct = struct {
    page: []align(layout.PAGE_BYTES) const u8,
    rows: [*]const lease_epoch.Row,
    dev_fds: [wire.DIRECT_MAX_VOLS]linux.fd_t,
    n_vols: u32,
    block_size: u64,
    /// The write arm granted: the devices are open `O_RDWR`.
    writable: bool,
    table: [MAX_LEASES]Row = @splat(.{}),

    pub const EstablishError = error{ Refused, Protocol, Unsealed, Map, DeviceOpen };

    /// `DirectHello` over the ctl socket (the caller holds the ctl lock):
    /// the page mapped, every device opened (`O_RDWR` when the write arm
    /// was granted). Any failure leaves nothing open or mapped.
    pub fn establish(ctl: linux.fd_t, write: bool) EstablishError!Direct {
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
            .writable = write,
        };
        errdefer self.closeAll();
        for (0..ok.n_vols) |i| {
            const path = ok.path(i);
            if (path.len == 0) return error.Protocol;
            var z: [wire.DEVICE_PATH_LEN]u8 = @splat(0);
            @memcpy(z[0..path.len], path);
            const rc = linux.open(@ptrCast(&z), .{ .ACCMODE = if (write) .RDWR else .RDONLY, .DIRECT = true, .CLOEXEC = true }, 0);
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

    /// The binding closed: its row goes. The lease comes back with the
    /// table's share (the caller sends its pending note when it can take
    /// the ctl lock — a `close` is async-signal-safe and may not — then
    /// drops it; the daemon's unbind drops the lease either way, the dirty
    /// mark of an unsent partial batch being the loss).
    pub fn forget(self: *Direct, binding: u64) ?*Lease {
        const row = self.rowOf(binding) orelse return null;
        const l = row.lease.swap(null, .acq_rel);
        row.refused_until.store(0, .release);
        row.binding.store(0, .release);
        return l;
    }

    /// The pinned lease of `binding` for `fsync` (null = none: nothing of
    /// this arm's is owed).
    pub fn leaseFor(self: *Direct, binding: u64) ?*Lease {
        return self.pinned(binding);
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
    /// a protocol failure → backoff too). A WRITE ask the daemon refuses
    /// `write` is asked again read-only (the file's writes ride the ring,
    /// its reads the arm).
    pub fn ask(self: *Direct, ctl: linux.fd_t, binding: u64, stats: Stats, want_write: bool) ?*Lease {
        _ = stats.asks.fetchAdd(1, .monotonic);
        const buf = page_allocator.alloc(u8, wire.CTL_MSG_MAX) catch {
            self.backoff(binding);
            return null;
        };
        defer page_allocator.free(buf);
        var want = want_write and self.writable;
        const frame: []const u8 = while (true) {
            const req: wire.LeaseReq = .{ .binding_id = binding, .flags = if (want) wire.LEASE_WRITE else 0 };
            rl.sendCtl(ctl, wire.frameBytes(&req), null) catch {
                self.backoff(binding);
                return null;
            };
            const r = rl.recvCtl(ctl, buf) catch {
                self.backoff(binding);
                return null;
            };
            if (r.fd) |f| _ = linux.close(f);
            const got = buf[0..r.len];
            if (want) if (wire.decodeFixed(wire.LeaseRefused, got)) |ref| {
                if ((ref.refuseClass() catch .kind) == .write) {
                    want = false;
                    continue;
                }
            } else |_| {};
            break got;
        };
        const l = wire.LeaseOk.decode(frame) catch {
            self.backoff(binding);
            return null;
        };
        if (l.block_size != self.block_size) {
            self.backoff(binding);
            return null;
        }
        const lease = Lease.create(l, want_write and self.writable) orelse {
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
                break :blk asker.ask(self, binding, stats, false) orelse return .ring;
            };
            defer l.drop();
            // The size and the map are the lease's: a moved epoch makes
            // every answer from them stale — EOF included (a grow through
            // the ring or the kernel since the grant).
            if (lease_epoch.loadFrom(self.rows, l.ino) != l.epoch) {
                _ = stats.rechecks_failed.fetchAdd(1, .monotonic);
                self.invalidate(binding, l);
                continue;
            }
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

    pub const WriteVerdict = union(enum) { served: usize, ring, eio };

    /// The direct write (module doc): `.served`, `.ring` (not this arm's
    /// shape, or the lease was recalled: the ring lands it), or `.eio` (a
    /// write landed under a FORCED recall).
    pub fn pwrite(self: *Direct, binding: u64, src: []const u8, offset: u64, stats: Stats, asker: anytype) WriteVerdict {
        if (!self.writable or src.len == 0) return .ring;
        if (offset % GRAIN != 0 or src.len % GRAIN != 0 or @intFromPtr(src.ptr) % GRAIN != 0) return .ring;
        var attempt: u8 = 0;
        while (attempt < MAX_ATTEMPTS) : (attempt += 1) {
            const l = self.pinned(binding) orelse blk: {
                if (self.backedOff(binding)) return .ring;
                break :blk asker.ask(self, binding, stats, true) orelse return .ring;
            };
            defer l.drop();
            if (!l.writable) {
                // A read lease: turned over ONCE into a write ask; a lease
                // whose write ask was refused keeps the ring for writes.
                if (l.write_asked) return .ring;
                self.invalidate(binding, l);
                continue;
            }
            if (lease_epoch.loadFrom(self.rows, l.ino) != l.epoch) {
                _ = stats.rechecks_failed.fetchAdd(1, .monotonic);
                self.invalidate(binding, l);
                continue;
            }
            // Size-neutral: within size, one block, mapped.
            if (offset + src.len > l.size) return .ring;
            const bs = l.block_size;
            const b = offset / bs;
            if ((offset + src.len - 1) / bs != b or b >= l.n) return .ring;
            const e = l.entries()[b];
            if (e.vol == wire.LeaseEntry.UNMAPPED or e.vol >= self.n_vols) return .ring;
            const fd = self.dev_fds[e.vol];
            if (fd < 0) return .ring;
            const words = &stats.ledger[l.slot];
            if (!write_lease_core.enterWrite(words, l.tag)) {
                // Recalled (or the slot re-granted): the lease is dead.
                _ = stats.writes_recalled.fetchAdd(1, .monotonic);
                self.invalidate(binding, l);
                continue;
            }
            const rc = linux.pwrite(fd, src.ptr, src.len, @intCast(e.dev_off + (offset - b * bs)));
            if (write_lease_core.exitWrite(words)) futex.wakeSharedAll(&words.inflight);
            if (linux.errno(rc) != .SUCCESS or rc != src.len) return .ring;
            if (write_lease_core.landedUnderForce(words)) {
                self.invalidate(binding, l);
                return .eio;
            }
            _ = stats.writes.fetchAdd(1, .monotonic);
            _ = stats.write_bytes.fetchAdd(src.len, .monotonic);
            // The postlude note accumulates; the batch or its age sends it.
            const now = clock.monotonicNs();
            const prev_count = l.note_count.fetchAdd(1, .acq_rel);
            if (prev_count == 0) l.note_first_ns.store(now, .release);
            _ = l.note_max_end.fetchMax(offset + src.len, .acq_rel);
            _ = l.note_vols.fetchOr(@as(u32, 1) << @intCast(e.vol), .acq_rel);
            _ = l.dirty_vols.fetchOr(@as(u32, 1) << @intCast(e.vol), .acq_rel);
            if (prev_count + 1 >= NOTE_BATCH or now -| l.note_first_ns.load(.acquire) >= NOTE_AGE_NS) asker.note(l, binding, stats, false);
            return .{ .served = src.len };
        }
        return .ring;
    }

    /// The pending note's frame, taken (null = nothing pending).
    pub fn takeNote(l: *Lease, binding: u64, ack: bool) ?wire.WriteNote {
        const count = l.note_count.swap(0, .acq_rel);
        if (count == 0) return null;
        return .{ .binding_id = binding, .max_end = l.note_max_end.swap(0, .acq_rel), .count = count, .vols_mask = l.note_vols.swap(0, .acq_rel), .flags = if (ack) wire.NOTE_ACK else 0 };
    }

    /// `fsync`'s device half: `fdatasync` every device written since the
    /// last one (the note's ack is the caller's, under its ctl lock).
    pub fn fdatasyncDirty(self: *Direct, l: *Lease) bool {
        var mask = l.dirty_vols.swap(0, .acq_rel);
        var ok = true;
        while (mask != 0) : (mask &= mask - 1) {
            const v: u5 = @intCast(@ctz(mask));
            if (self.dev_fds[v] >= 0 and linux.errno(linux.fdatasync(self.dev_fds[v])) != .SUCCESS) ok = false;
        }
        return ok;
    }
};
