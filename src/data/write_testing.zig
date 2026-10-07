//! Test fixtures for the write path (test builds only): a RAM `FakeDevice`
//! with CQE-ordering control (a hook between the bytes landing and the
//! completion) and torn-DMA injection, a RAM block-map authority, and a
//! fixture bundling volumes, allocators and seams. Every write-path core is
//! exercised over these on every host; the Linux e2e binds `nvme_dev`.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const seams = @import("write_seams.zig");
const block_alloc = @import("block_alloc.zig");

comptime {
    assert(builtin.is_test);
}

pub const Binding = seams.Binding;
const PAGE = seams.PAGE;

const GrainKey = struct { vol: u64, off: u64 };

/// What one write did (the device-byte ledger of a test).
pub const WriteRecord = struct { vol: u64, off: u64, len: u64, fixed: bool };

pub const Hook = struct {
    ctx: *anyopaque,
    /// Runs after the bytes landed, before the completion is answered —
    /// the window between a DMA and its CQE.
    call: *const fn (ctx: *anyopaque, dev: *FakeDevice, vol: u64, off: u64, len: u64) void,
};

pub const FakeDevice = struct {
    gpa: Allocator,
    grains: std.AutoHashMapUnmanaged(GrainKey, [PAGE]u8) = .empty,
    lock: std.Io.Mutex = .init,
    /// The "registered slots" a `write_fixed` reads from.
    slots: std.AutoHashMapUnmanaged(u16, []const u8) = .empty,
    writes: std.ArrayList(WriteRecord) = .empty,
    reads: u64 = 0,
    read_bytes: u64 = 0,
    flushes: std.ArrayList(u64) = .empty,
    /// Injection: fail the next write (after `torn_grains` grains landed).
    fail_next: ?seams.DmaError = null,
    torn_grains: usize = 0,
    fail_next_read: ?seams.ReadError = null,
    fail_next_flush: ?seams.DmaError = null,
    hook: ?Hook = null,
    /// Deny `write_fixed` (a non-zc session).
    no_write_fixed: bool = false,
    /// Every call sequence the test asserts order on.
    trace: std.ArrayList(Event) = .empty,

    pub const Event = enum { write, write_fixed, read, flush };

    pub fn init(gpa: Allocator) FakeDevice {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *FakeDevice) void {
        self.grains.deinit(self.gpa);
        self.slots.deinit(self.gpa);
        self.writes.deinit(self.gpa);
        self.flushes.deinit(self.gpa);
        self.trace.deinit(self.gpa);
        self.* = undefined;
    }

    pub fn seam(self: *FakeDevice) seams.DeviceSeam {
        return .{ .ctx = self, .vtable = if (self.no_write_fixed) &vtable_no_fixed else &vtable };
    }

    const vtable: seams.DeviceSeam.VTable = .{ .write = &write, .write_fixed = &writeFixed, .read = &read, .flush = &flush };
    const vtable_no_fixed: seams.DeviceSeam.VTable = .{ .write = &write, .write_fixed = null, .read = &read, .flush = &flush };

    pub fn registerSlot(self: *FakeDevice, buf_index: u16, bytes: []const u8) !void {
        try self.slots.put(self.gpa, buf_index, bytes);
    }

    fn land(self: *FakeDevice, vol: u64, off: u64, data: []const u8, max_grains: ?usize) !void {
        var pos: usize = 0;
        var o = off;
        var n: usize = 0;
        while (pos < data.len) : (o += PAGE) {
            if (max_grains) |m| if (n >= m) return error.InputOutput;
            var g: [PAGE]u8 = @splat(0);
            const take = @min(PAGE, data.len - pos);
            @memcpy(g[0..take], data[pos..][0..take]);
            try self.grains.put(self.gpa, .{ .vol = vol, .off = o }, g);
            pos += take;
            n += 1;
        }
    }

    fn writeInner(self: *FakeDevice, vol: u64, off: u64, data: []const u8, fixed: bool) seams.DmaError!void {
        self.lock.lockUncancelable(std.testing.io);
        const injected = self.fail_next;
        const torn = self.torn_grains;
        self.fail_next = null;
        self.torn_grains = 0;
        self.trace.append(self.gpa, if (fixed) .write_fixed else .write) catch return error.OutOfMemory;
        if (injected) |e| {
            // A torn DMA: the first `torn` grains land, then the error.
            self.land(vol, off, data, torn) catch {};
            self.lock.unlock(std.testing.io);
            return e;
        }
        self.land(vol, off, data, null) catch return error.OutOfMemory;
        self.writes.append(self.gpa, .{ .vol = vol, .off = off, .len = data.len, .fixed = fixed }) catch return error.OutOfMemory;
        const hook = self.hook;
        self.lock.unlock(std.testing.io);
        if (hook) |h| h.call(h.ctx, self, vol, off, data.len);
    }

    fn write(ctx: *anyopaque, vol: u64, off: u64, data: []const u8) seams.DmaError!void {
        const self: *FakeDevice = @ptrCast(@alignCast(ctx));
        return self.writeInner(vol, off, data, false);
    }

    fn writeFixed(ctx: *anyopaque, vol: u64, off: u64, src: seams.HeldSource) seams.DmaError!void {
        const self: *FakeDevice = @ptrCast(@alignCast(ctx));
        const bytes = self.slots.get(src.buf_index) orelse return error.InputOutput;
        return self.writeInner(vol, off, bytes[0..src.len], true);
    }

    fn read(ctx: *anyopaque, vol: u64, off: u64, out: []u8) seams.ReadError!void {
        const self: *FakeDevice = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(std.testing.io);
        defer self.lock.unlock(std.testing.io);
        self.trace.append(self.gpa, .read) catch return error.OutOfMemory;
        if (self.fail_next_read) |e| {
            self.fail_next_read = null;
            return e;
        }
        self.reads += 1;
        self.read_bytes += out.len;
        var pos: usize = 0;
        var o = off;
        while (pos < out.len) : (o += PAGE) {
            const take = @min(PAGE, out.len - pos);
            if (self.grains.get(.{ .vol = vol, .off = o })) |g| {
                @memcpy(out[pos..][0..take], g[0..take]);
            } else {
                @memset(out[pos..][0..take], 0); // an unwritten block reads zeros (discarded)
            }
            pos += take;
        }
    }

    fn flush(ctx: *anyopaque, vol: u64) seams.DmaError!void {
        const self: *FakeDevice = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(std.testing.io);
        defer self.lock.unlock(std.testing.io);
        self.trace.append(self.gpa, .flush) catch return error.OutOfMemory;
        if (self.fail_next_flush) |e| {
            self.fail_next_flush = null;
            return e;
        }
        self.flushes.append(self.gpa, vol) catch return error.OutOfMemory;
    }

    /// The device bytes at `[off, off + out.len)` for assertions.
    pub fn peek(self: *FakeDevice, vol: u64, off: u64, out: []u8) void {
        read(self, vol, off, out) catch unreachable;
        self.reads -= 1;
        self.read_bytes -= out.len;
    }

    pub fn writeBytesTotal(self: *const FakeDevice) u64 {
        var n: u64 = 0;
        for (self.writes.items) |w| n += w.len;
        return n;
    }

    pub fn resetLedger(self: *FakeDevice) void {
        self.writes.clearRetainingCapacity();
        self.reads = 0;
        self.read_bytes = 0;
        self.flushes.clearRetainingCapacity();
        self.trace.clearRetainingCapacity();
    }
};

// ---------------------------------------------------------------------------
// The RAM block-map authority.
// ---------------------------------------------------------------------------

pub const RamFile = struct {
    kind: seams.FileKind = .striped,
    size: u64 = 0,
    map: std.AutoHashMapUnmanaged(u32, Binding) = .empty,
};

pub const MergeLog = struct { ino: u64, blocks: u8, min_size: u64, fencing: u64, displaced: u8 };

pub const RamMap = struct {
    gpa: Allocator,
    chunk: u64 = BS,
    files: std.AutoHashMapUnmanaged(u64, RamFile) = .empty,
    lock: std.Io.Mutex = .init,
    fencing: u64 = 1,
    fail_next_merge: ?seams.MergeError = null,
    merges: std.ArrayList(MergeLog) = .empty,
    refs_added: u64 = 0,
    refs_released: u64 = 0,
    /// Order of merges against the device trace (an fsync's DUR-1 pin).
    on_merge: ?*const fn (*RamMap) void = null,

    pub fn init(gpa: Allocator) RamMap {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *RamMap) void {
        var it = self.files.valueIterator();
        while (it.next()) |f| f.map.deinit(self.gpa);
        self.files.deinit(self.gpa);
        self.merges.deinit(self.gpa);
        self.* = undefined;
    }

    pub fn seam(self: *RamMap) seams.MapSeam {
        return .{ .ctx = self, .vtable = &vtable };
    }

    const vtable: seams.MapSeam.VTable = .{ .lookup = &lookup, .view = &view, .merge = &merge, .current_fencing = &currentFencing };

    pub fn create(self: *RamMap, ino: u64, kind: seams.FileKind, size: u64) !void {
        try self.files.put(self.gpa, ino, .{ .kind = kind, .size = size });
    }

    pub fn bind(self: *RamMap, ino: u64, block: u32, b: Binding) !void {
        const f = self.files.getPtr(ino).?;
        try f.map.put(self.gpa, block, b);
    }

    pub fn remove(self: *RamMap, ino: u64) void {
        if (self.files.fetchRemove(ino)) |kv| {
            var m = kv.value.map;
            m.deinit(self.gpa);
        }
    }

    pub fn sizeOf(self: *RamMap, ino: u64) u64 {
        return if (self.files.get(ino)) |f| f.size else 0;
    }

    fn lookup(ctx: *anyopaque, ino: u64, block: u32) ?Binding {
        const self: *RamMap = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(std.testing.io);
        defer self.lock.unlock(std.testing.io);
        const f = self.files.get(ino) orelse return null;
        return f.map.get(block);
    }

    fn view(ctx: *anyopaque, ino: u64) ?seams.FileView {
        const self: *RamMap = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(std.testing.io);
        defer self.lock.unlock(std.testing.io);
        const f = self.files.get(ino) orelse return null;
        return .{ .kind = f.kind, .size = f.size, .map_resident = true };
    }

    fn merge(ctx: *anyopaque, req: seams.MergeRequest) seams.MergeError!seams.MergeOutcome {
        const self: *RamMap = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(std.testing.io);
        defer self.lock.unlock(std.testing.io);
        if (self.fail_next_merge) |e| {
            self.fail_next_merge = null;
            return e;
        }
        if (req.fencing != self.fencing) return error.FencingTokenExpired;
        const f = self.files.getPtr(req.ino) orelse return error.NotFound;
        // Every new binding needs a proof (the publish law).
        for (req.inserts) |ins| {
            var covered = false;
            for (req.proofs) |p| if (p.vol_tag == ins.binding.vol_tag and p.offset <= ins.binding.offset and ins.binding.offset < p.offset + p.len) {
                covered = true;
            };
            if (!covered) return error.Transient;
        }
        var out: seams.MergeOutcome = .{};
        for (req.inserts) |ins| {
            if (f.map.get(ins.block)) |old| {
                if (!old.sameRef(ins.binding, self.chunk)) {
                    out.displaced[out.n_displaced] = old;
                    out.n_displaced += 1;
                    self.refs_released += 1;
                    self.refs_added += 1;
                }
            } else {
                self.refs_added += 1;
            }
            f.map.put(self.gpa, ins.block, ins.binding) catch return error.OutOfMemory;
        }
        if (req.min_size > f.size) f.size = req.min_size;
        f.kind = .striped;
        self.merges.append(self.gpa, .{ .ino = req.ino, .blocks = @intCast(req.inserts.len), .min_size = req.min_size, .fencing = req.fencing, .displaced = out.n_displaced }) catch return error.OutOfMemory;
        if (self.on_merge) |h| h(self);
        return out;
    }

    fn currentFencing(ctx: *anyopaque, _: u64) u64 {
        const self: *RamMap = @ptrCast(@alignCast(ctx));
        return self.fencing;
    }
};

// ---------------------------------------------------------------------------
// The frees ledger.
// ---------------------------------------------------------------------------

pub const RamFrees = struct {
    gpa: Allocator,
    vols: *const seams.Volumes,
    freed: std.ArrayList(Binding) = .empty,

    pub fn init(gpa: Allocator, vols: *const seams.Volumes) RamFrees {
        return .{ .gpa = gpa, .vols = vols };
    }

    pub fn deinit(self: *RamFrees) void {
        self.freed.deinit(self.gpa);
    }

    pub fn seam(self: *RamFrees) seams.FreeSeam {
        return .{ .ctx = self, .free_fn = &free };
    }

    fn free(ctx: *anyopaque, _: Io, b: Binding) seams.FreeVerdict {
        const self: *RamFrees = @ptrCast(@alignCast(ctx));
        self.freed.append(self.gpa, b) catch return .refused;
        const v = self.vols.byTag(b.vol_tag) orelse return .refused;
        const verdict = v.alloc.freeBlock(v.alloc.idxOf(b.offset)) catch return .refused;
        return switch (verdict) {
            .terminal => .terminal,
            .nonterminal => .nonterminal,
            .refused => .refused,
        };
    }
};

// ---------------------------------------------------------------------------
// The fixture: one passthrough volume, one transformed, both 64 KiB blocks.
// ---------------------------------------------------------------------------

pub const BS: u64 = 16 * PAGE;
pub const VOL_A: u64 = 0xA11;
pub const VOL_B: u64 = 0xB22;

pub const Fixture = struct {
    gpa: Allocator,
    dev: FakeDevice,
    map: RamMap,
    vols: seams.Volumes = .{},
    frees: RamFrees,
    alloc_a: *block_alloc.BlockAllocator,
    alloc_b: *block_alloc.BlockAllocator,

    pub fn init(gpa: Allocator, blocks_per_vol: u64) !*Fixture {
        const f = try gpa.create(Fixture);
        errdefer gpa.destroy(f);
        f.* = .{ .gpa = gpa, .dev = FakeDevice.init(gpa), .map = RamMap.init(gpa), .frees = undefined, .alloc_a = undefined, .alloc_b = undefined };
        f.alloc_a = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = VOL_A, .vol = 0, .block_size = BS, .capacity_bytes = blocks_per_vol * BS });
        f.alloc_b = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = VOL_B, .vol = 1, .block_size = BS, .capacity_bytes = blocks_per_vol * BS });
        f.vols.add(.{ .vol_tag = VOL_A, .ordinal = 0, .alloc = f.alloc_a, .passthrough = true, .write_cache_volatile = true });
        f.vols.add(.{ .vol_tag = VOL_B, .ordinal = 1, .alloc = f.alloc_b, .passthrough = false, .write_cache_volatile = false });
        f.frees = RamFrees.init(gpa, &f.vols);
        return f;
    }

    pub fn deinit(self: *Fixture) void {
        self.dev.deinit();
        self.map.deinit();
        self.frees.deinit();
        self.alloc_a.deinit();
        self.alloc_b.deinit();
        self.gpa.destroy(self);
    }

    /// A published, mapped block of `ino` at `block` on volume A with the
    /// pattern `fill` on the device: the overwrite shapes' base.
    pub fn publishBlock(self: *Fixture, ino: u64, block: u32, fill: u8) !Binding {
        const m = try self.alloc_a.allocateBlock();
        var buf: [BS]u8 = @splat(fill);
        try self.dev.seam().vtable.write(&self.dev, VOL_A, m.offset, &buf);
        self.alloc_a.publishBlock(m.idx);
        const b = Binding.wholeBlock(VOL_A, m.offset, m.stamp);
        if (self.map.files.get(ino) == null) try self.map.create(ino, .striped, 0);
        try self.map.bind(ino, block, b);
        const f = self.map.files.getPtr(ino).?;
        f.size = @max(f.size, (@as(u64, block) + 1) * BS);
        self.dev.resetLedger();
        return b;
    }
};
