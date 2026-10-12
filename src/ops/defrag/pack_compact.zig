//! `--pack` compaction (charter §6.1: "occupancy from the census's C8
//! walk, each distinct `off` group copied ONCE at max len into a
//! pre-allocated unpublished destination, every referencer republished,
//! destination refcount raised to the referencer count before any
//! publish"; spec §3.5 `compact_one`): `move_one`'s discipline with three
//! stated deviations — (1) the destination is the OPEN pack (ONE ranged
//! read of the window at `max_len`, re-packed through `Packer.prepare`);
//! (2) the destination carries one reference per REFERENCER before any
//! publish — the handle's own + `refs − 1` raised; a structural refusal
//! of the raise is the tripwire `pack_compaction_raise_refused`; (3)
//! every referencer republishes `old → dst:off':len_i` with its OWN len at
//! the shared slot, ascending ino; each refused referencer releases one
//! destination reference; the handle's own reference is released LAST (a
//! window nobody took is an abandoned slot). The victim's pin release is
//! its LAST reference iff every tenant left — the terminal free through
//! the ordinary ladder. PORTABLE over a `PackSeam` the plane binds
//! (`Packer.prepare/commit`, `plane.readMappingImage`, `republishBinding`).

const std = @import("std");
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const layout = @import("../../data/layout.zig");
const census = @import("../lifecycle/census.zig");
const mover = @import("../lifecycle/mover.zig");
const metrics = @import("metrics.zig");
const plan_mod = @import("plan.zig");

const stats = metrics.stats;
pub const Mapping = layout.Mapping;
pub const Ref = census.Ref;
pub const PackVictim = plan_mod.PackVictim;

pub const Outcome = enum { compacted, deferred, superseded };

/// A window landed in the open pack: the new base/offset the referencers
/// republish onto.
pub const Placed = struct {
    vol_tag: u64,
    block_idx: u64,
    offset: u64,
    incarnation: u64,
    rel_off: u32,
};

pub const SeamError = error{ Io, OutOfMemory, StorageFull };

pub const PackSeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        chunk: *const fn (ctx: *anyopaque) u64,
        isOpenPack: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) bool,
        quiescent: *const fn (ctx: *anyopaque, ino: u64, block_index: u32) bool,
        pin: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) mover.PinVerdict,
        unpin: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) void,
        /// ONE ranged read of the window at `len` bytes.
        readWindow: *const fn (ctx: *anyopaque, m: Mapping, out: []u8) SeamError!usize,
        /// Re-pack `image` into the open pack: the DMA of exactly `slot`
        /// bytes; the handle's own reference is the first. `StorageFull`
        /// ⇒ the arm is stopped (the rest of the victim waits).
        prepare: *const fn (ctx: *anyopaque, image: []const u8) SeamError!Placed,
        /// A window nobody took: the slot's space is returned (every
        /// reference was already released by its refused referencer).
        abandonWindow: *const fn (ctx: *anyopaque, p: Placed) void,
        incrementRefcount: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) bool,
        releaseRef: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) void,
        republish: *const fn (ctx: *anyopaque, ino: u64, block_index: u32, expected: Mapping, new: Mapping) mover.RepublishError!void,
        freeBinding: *const fn (ctx: *anyopaque, old: Mapping) void,
    };
};

/// Compact ONE victim: every distinct `off` window copied once at its max
/// len; its referencers republished with their own lens.
pub fn compactOne(seam: PackSeam, gpa: Allocator, v: PackVictim) error{ Io, OutOfMemory }!Outcome {
    const vt = seam.vtable;
    const ctx = seam.ctx;
    const chunk = vt.chunk(ctx);
    if (vt.isOpenPack(ctx, v.vol_tag, v.block_idx)) {
        stats.pack_compaction_deferred.inc();
        return .deferred;
    }
    for (v.refs) |r| if (!vt.quiescent(ctx, r.ino, r.block_index)) {
        stats.pack_compaction_deferred.inc();
        return .deferred;
    };
    var pinned = false;
    switch (vt.pin(ctx, v.vol_tag, v.block_idx)) {
        .pinned => pinned = true,
        .pinned_unstable => {
            vt.unpin(ctx, v.vol_tag, v.block_idx);
            return .deferred;
        },
        .refused => return .superseded,
    }
    defer if (pinned) vt.unpin(ctx, v.vol_tag, v.block_idx);
    const buf = try gpa.alloc(u8, chunk);
    defer gpa.free(buf);
    var moved_tenants: u64 = 0;
    var any_landed = false;
    for (v.windows) |w| {
        // The longest referencer of this window, in census order.
        var longest: ?Ref = null;
        var n_refs: u32 = 0;
        for (v.refs) |r| if (r.mapping.rel_off == w.off) {
            n_refs += 1;
            if (longest == null or r.mapping.len > longest.?.mapping.len) longest = r;
        };
        const lr = longest orelse continue;
        const n = vt.readWindow(ctx, lr.mapping, buf) catch return error.Io;
        const placed = vt.prepare(ctx, buf[0..n]) catch |e| switch (e) {
            error.StorageFull => return if (any_landed) .compacted else .deferred,
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
        };
        stats.pack_compaction_windows_copied.inc();
        stats.pack_compaction_bytes_copied.add(n);
        // One reference per referencer BEFORE any publish: the handle's
        // own + refs − 1 raised.
        var raised: u32 = 1;
        while (raised < n_refs) : (raised += 1) {
            if (!vt.incrementRefcount(ctx, placed.vol_tag, placed.block_idx)) {
                stats.pack_compaction_raise_refused.fire("the open pack refused the pre-publish raise");
                while (raised > 0) : (raised -= 1) vt.releaseRef(ctx, placed.vol_tag, placed.block_idx);
                vt.abandonWindow(ctx, placed);
                return .deferred;
            }
        }
        // Publish per referencer, ascending ino (census order), each with
        // its OWN len at the shared slot.
        var published: u32 = 0;
        for (v.refs) |r| {
            if (r.mapping.rel_off != w.off) continue;
            const new = Mapping.tenant(placed.vol_tag, placed.offset, placed.incarnation, placed.rel_off, r.mapping.len);
            if (!vt.quiescent(ctx, r.ino, r.block_index)) {
                vt.releaseRef(ctx, placed.vol_tag, placed.block_idx);
                continue;
            }
            vt.republish(ctx, r.ino, r.block_index, r.mapping, new) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => {
                    vt.releaseRef(ctx, placed.vol_tag, placed.block_idx);
                    continue;
                },
            };
            vt.freeBinding(ctx, r.mapping);
            published += 1;
            moved_tenants += 1;
        }
        // The handle is dropped LAST: a window every referencer refused is
        // an abandoned slot (its references were released one per refusal,
        // the handle's own among them); a taken window's references ARE
        // the referencers' (RAM = population once the records land).
        if (published == 0) vt.abandonWindow(ctx, placed) else any_landed = true;
    }
    if (!any_landed) return .superseded;
    stats.pack_compaction_tenants_moved.add(moved_tenants);
    return .compacted;
}

/// Run a whole plan; returns the victims compacted.
pub fn compactPlan(seam: PackSeam, gpa: Allocator, p: plan_mod.PackPlan, chunk: u64) error{ Io, OutOfMemory }!usize {
    if (!p.executes()) return 0;
    var n: usize = 0;
    for (p.victims) |v| {
        switch (try compactOne(seam, gpa, v)) {
            .compacted => {
                n += 1;
                stats.pack_compactions.inc();
                stats.pack_compaction_bytes_reclaimed.add(chunk - v.live_bytes);
            },
            .deferred, .superseded => {},
        }
    }
    return n;
}

// ---------------------------------------------------------------------------
// The RAM pack world (over the mover's RAM world) + tests.
// ---------------------------------------------------------------------------

pub const RamPackWorld = struct {
    w: *mover.RamWorld,
    /// The open pack: block + next slot; opened on the first prepare.
    open: ?struct { vol_tag: u64, idx: u64, next: u32 } = null,
    open_vol: u64,
    chunk: u64,
    /// Test seam: `prepare` answers StorageFull.
    full: bool = false,
    frees_before: u64 = 0,

    pub fn seam(self: *RamPackWorld) PackSeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: PackSeam.VTable = .{ .chunk = &chunkFn, .isOpenPack = &isOpen, .quiescent = &quiescent, .pin = &pin, .unpin = &unpin, .readWindow = &readWindow, .prepare = &prepare, .abandonWindow = &abandonWindow, .incrementRefcount = &incRef, .releaseRef = &releaseRef, .republish = &republish, .freeBinding = &freeBinding };

    fn self_(ctx: *anyopaque) *RamPackWorld {
        return @ptrCast(@alignCast(ctx));
    }
    fn chunkFn(ctx: *anyopaque) u64 {
        return self_(ctx).chunk;
    }
    fn isOpen(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
        const s = self_(ctx);
        const o = s.open orelse return false;
        return o.vol_tag == vol_tag and o.idx == idx;
    }
    fn quiescent(ctx: *anyopaque, ino: u64, block_index: u32) bool {
        return !self_(ctx).w.busy.contains(.{ .ino = ino, .block_index = block_index });
    }
    fn pin(ctx: *anyopaque, vol_tag: u64, idx: u64) mover.PinVerdict {
        return mover.RamWorld.seam(self_(ctx).w).vtable.pin(self_(ctx).w, vol_tag, idx);
    }
    fn unpin(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        mover.RamWorld.seam(self_(ctx).w).vtable.unpin(self_(ctx).w, vol_tag, idx);
    }
    fn readWindow(ctx: *anyopaque, m: Mapping, out: []u8) SeamError!usize {
        const s = self_(ctx);
        const b = s.w.block(m.vol_tag, m.blockIdx(s.chunk));
        const n: usize = m.len;
        @memcpy(out[0..n], b.data[m.rel_off .. m.rel_off + n]);
        return n;
    }
    fn prepare(ctx: *anyopaque, image: []const u8) SeamError!Placed {
        const s = self_(ctx);
        if (s.full) return error.StorageFull;
        const slot: u32 = @intCast(image.len);
        if (s.open == null or @as(u64, s.open.?.next) + slot > mover.RamWorld.DATA) {
            // Seal + refill: a fresh block on the open volume (the packer's
            // own +1 is minted with it).
            const m = (mover.RamWorld.mintOn(s.w, s.open_vol, 0, null) catch return error.Io) orelse return error.StorageFull;
            s.w.block(m.vol_tag, m.idx).state = .allocated;
            s.open = .{ .vol_tag = m.vol_tag, .idx = m.idx, .next = 0 };
        }
        const o = &s.open.?;
        const b = s.w.block(o.vol_tag, o.idx);
        @memcpy(b.data[o.next .. o.next + image.len], image);
        if (b.len < o.next + image.len) b.len = o.next + image.len;
        // The tenant's reference (the handle's own).
        b.refcount += 1;
        const placed: Placed = .{ .vol_tag = o.vol_tag, .block_idx = o.idx, .offset = o.idx * s.chunk, .incarnation = b.incarnation, .rel_off = o.next };
        o.next += slot;
        return placed;
    }
    fn abandonWindow(_: *anyopaque, _: Placed) void {}
    fn incRef(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
        return mover.RamWorld.seam(self_(ctx).w).vtable.incrementRefcount(self_(ctx).w, vol_tag, idx);
    }
    fn releaseRef(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        self_(ctx).w.release(vol_tag, idx);
    }
    fn republish(ctx: *anyopaque, ino: u64, block_index: u32, expected: Mapping, new: Mapping) mover.RepublishError!void {
        return mover.RamWorld.seam(self_(ctx).w).vtable.republish(self_(ctx).w, ino, block_index, expected, new);
    }
    fn freeBinding(ctx: *anyopaque, old: Mapping) void {
        mover.RamWorld.seam(self_(ctx).w).vtable.freeBinding(self_(ctx).w, old);
    }

    /// The open pack's reference is the packer's own: at the end of a pass
    /// the packer seals it, which drops that +1 (the block stays while a
    /// tenant references it).
    pub fn sealOpen(self: *RamPackWorld) void {
        if (self.open) |o| {
            self.w.release(o.vol_tag, o.idx);
            self.open = null;
        }
    }
};

const testing = std.testing;

test "victims below half re-pack into one fresh pack and free; every tenant byte-exact; a clone share is copied once onto one window; a nested prefix share once at the longer len" {
    // The RAM block holds 64 data bytes under a 4 KiB chunk: the grain law
    // (`ceilGrain`) is pinned by `metrics.packLiveBytes`; the fake's
    // `prepare` places windows at `image.len` granularity so the
    // executor's discipline (copy once, raise, publish, release last) is
    // what this test pins.
    const gpa = testing.allocator;
    var w = mover.RamWorld.init(gpa, 4096);
    defer w.deinit();
    try w.addVolume(0xA, 8);
    var pw: RamPackWorld = .{ .w = &w, .open_vol = 0xA, .chunk = 4096 };
    // Three victim packs, each with a few small tenants (lens ≤ 20 B so a
    // 64 B data window holds them at offsets 0/20/40 — the RAM block's
    // data cap is 64 B; the "4 KiB grain" is modelled as 20 B slots here
    // by using rel_off multiples the fake does not re-align).
    var data: [mover.RamWorld.DATA]u8 = undefined;
    for (&data, 0..) |*d, i| d.* = @truncate(i);
    // block 0: tenants at 0 (len 10, shared by ino 1 and 2 — the clone share) and 20 (len 10, ino 3).
    _ = try w.plant(0xA, 0, data[0..4096], &.{});
    _ = try w.plantTenant(0xA, 0, 0, 10, .{ .ino = 1, .block_index = 0 });
    _ = try w.plantTenant(0xA, 0, 0, 10, .{ .ino = 2, .block_index = 0 });
    _ = try w.plantTenant(0xA, 0, 20, 10, .{ .ino = 3, .block_index = 0 });
    // block 1: a nested prefix share at 0 (len 5 for ino 4, len 12 for ino 5).
    _ = try w.plant(0xA, 1, data[0..4096], &.{});
    _ = try w.plantTenant(0xA, 1, 0, 5, .{ .ino = 4, .block_index = 0 });
    _ = try w.plantTenant(0xA, 1, 0, 12, .{ .ino = 5, .block_index = 0 });
    const victims = [_]PackVictim{
        .{ .vol_tag = 0xA, .block_idx = 0, .live_bytes = 20, .windows = @constCast(&[_]metrics.PackWindow{ .{ .off = 0, .len = 10 }, .{ .off = 20, .len = 10 } }), .refs = @constCast(&[_]Ref{
            .{ .ino = 1, .block_index = 0, .mapping = w.bindings.get(.{ .ino = 1, .block_index = 0 }).? },
            .{ .ino = 2, .block_index = 0, .mapping = w.bindings.get(.{ .ino = 2, .block_index = 0 }).? },
            .{ .ino = 3, .block_index = 0, .mapping = w.bindings.get(.{ .ino = 3, .block_index = 0 }).? },
        }) },
        .{ .vol_tag = 0xA, .block_idx = 1, .live_bytes = 12, .windows = @constCast(&[_]metrics.PackWindow{.{ .off = 0, .len = 12 }}), .refs = @constCast(&[_]Ref{
            .{ .ino = 4, .block_index = 0, .mapping = w.bindings.get(.{ .ino = 4, .block_index = 0 }).? },
            .{ .ino = 5, .block_index = 0, .mapping = w.bindings.get(.{ .ino = 5, .block_index = 0 }).? },
        }) },
    };
    const copied0 = stats.pack_compaction_windows_copied.load();
    const used0 = w.usedBlocks(0xA);
    try testing.expectEqual(@as(u64, 2), used0);
    for (victims) |v| try testing.expectEqual(Outcome.compacted, try compactOne(pw.seam(), gpa, v));
    pw.sealOpen();
    // 3 windows copied once each (the clone share once, the nested prefix once at len 12).
    try testing.expectEqual(copied0 + 3, stats.pack_compaction_windows_copied.load());
    // Both victims freed, one fresh pack holds everything: 1 block used.
    try testing.expectEqual(@as(u64, 1), w.usedBlocks(0xA));
    try testing.expectEqual(@as(u32, 5), w.block(0xA, 2).refcount); // 5 tenants on the one fresh pack
    // Byte-exact per tenant.
    try testing.expectEqualSlices(u8, data[0..10], w.resolve(.{ .ino = 1, .block_index = 0 }).?);
    try testing.expectEqualSlices(u8, data[0..10], w.resolve(.{ .ino = 2, .block_index = 0 }).?);
    try testing.expectEqualSlices(u8, data[20..30], w.resolve(.{ .ino = 3, .block_index = 0 }).?);
    try testing.expectEqualSlices(u8, data[0..5], w.resolve(.{ .ino = 4, .block_index = 0 }).?);
    try testing.expectEqualSlices(u8, data[0..12], w.resolve(.{ .ino = 5, .block_index = 0 }).?);
    // The clone share and the nested prefix each sit on ONE window.
    const m1 = w.bindings.get(.{ .ino = 1, .block_index = 0 }).?;
    const m2 = w.bindings.get(.{ .ino = 2, .block_index = 0 }).?;
    try testing.expect(m1.sameRef(m2, 4096) and m1.rel_off == m2.rel_off);
    const m4 = w.bindings.get(.{ .ino = 4, .block_index = 0 }).?;
    const m5 = w.bindings.get(.{ .ino = 5, .block_index = 0 }).?;
    try testing.expect(m4.rel_off == m5.rel_off and m4.len == 5 and m5.len == 12);
}

test "a refused publish releases exactly one destination reference; a tenant re-staged mid-plan (not quiescent) defers the whole victim; StorageFull mid-victim keeps what landed" {
    const gpa = testing.allocator;
    var w = mover.RamWorld.init(gpa, 4096);
    defer w.deinit();
    try w.addVolume(0xA, 8);
    var pw: RamPackWorld = .{ .w = &w, .open_vol = 0xA, .chunk = 4096 };
    var data: [mover.RamWorld.DATA]u8 = @splat(0x5A);
    _ = try w.plant(0xA, 0, data[0..4096], &.{});
    _ = try w.plantTenant(0xA, 0, 0, 8, .{ .ino = 1, .block_index = 0 });
    _ = try w.plantTenant(0xA, 0, 16, 8, .{ .ino = 2, .block_index = 0 });
    const v: PackVictim = .{ .vol_tag = 0xA, .block_idx = 0, .live_bytes = 16, .windows = @constCast(&[_]metrics.PackWindow{ .{ .off = 0, .len = 8 }, .{ .off = 16, .len = 8 } }), .refs = @constCast(&[_]Ref{
        .{ .ino = 1, .block_index = 0, .mapping = w.bindings.get(.{ .ino = 1, .block_index = 0 }).? },
        .{ .ino = 2, .block_index = 0, .mapping = w.bindings.get(.{ .ino = 2, .block_index = 0 }).? },
    }) };
    try w.busy.put(gpa, .{ .ino = 2, .block_index = 0 }, {});
    try testing.expectEqual(Outcome.deferred, try compactOne(pw.seam(), gpa, v));
    _ = w.busy.remove(.{ .ino = 2, .block_index = 0 });
    // ino 2's publish is superseded: its window is abandoned, the victim
    // keeps ino 2 (refcount 1), ino 1 moved.
    w.supersede_ino = 2;
    try testing.expectEqual(Outcome.compacted, try compactOne(pw.seam(), gpa, v));
    pw.sealOpen();
    try testing.expectEqual(@as(u64, 0xA), w.bindings.get(.{ .ino = 2, .block_index = 0 }).?.vol_tag);
    try testing.expectEqual(@as(u64, 0), w.bindings.get(.{ .ino = 2, .block_index = 0 }).?.blockIdx(4096));
    try testing.expectEqual(@as(u32, 1), w.block(0xA, 0).refcount);
    const dst = w.bindings.get(.{ .ino = 1, .block_index = 0 }).?;
    try testing.expectEqual(@as(u32, 1), w.block(0xA, dst.blockIdx(4096)).refcount);
    // StorageFull on a fresh victim: nothing landed ⇒ deferred, the victim untouched.
    pw.full = true;
    const v2: PackVictim = .{ .vol_tag = 0xA, .block_idx = 0, .live_bytes = 8, .windows = @constCast(&[_]metrics.PackWindow{.{ .off = 16, .len = 8 }}), .refs = @constCast(&[_]Ref{.{ .ino = 2, .block_index = 0, .mapping = w.bindings.get(.{ .ino = 2, .block_index = 0 }).? }}) };
    try testing.expectEqual(Outcome.deferred, try compactOne(pw.seam(), gpa, v2));
    try testing.expectEqual(@as(u32, 1), w.block(0xA, 0).refcount);
}
