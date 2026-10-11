//! THE READ SEAM — `WriteSideView`, what the read path consults FIRST
//! (charter §6.3 "Precedence: write-side overlays first (the accumulation
//! buffer's coverage-aware snapshot; an open device-overlay record; extent
//! records composed over the base; staged entries) — a dirty local
//! authority outranks durable state"). A small vtable with a RAM default,
//! stable: the read path consumes it duck-typed through `serve` /
//! `hasCustody` / `shadowBinding`.
//!
//! `serve` fills `out` for ONE block window and answers how much of it is
//! authoritative: `served` (every byte — a fresh accumulation buffer's
//! zeros, an overlay record's composed window, a staged entry), `partial`
//! (the covered runs are in `out`; the caller fills the GAPS from `base`
//! — a deferred-seed buffer or an extent overlay over a mapped block),
//! `redirect` (a rewrite epoch's shadow B key: read the block from it,
//! not from the map's A), `none` (no write-side custody: the layout
//! ladder), `settle` (the overlay's bounded retries ran out: the serialized
//! settle under the block + meta locks).

const std = @import("std");
const Io = std.Io;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const staging = @import("staging.zig");
const cov = @import("write_coverage.zig");
const seams = @import("write_seams.zig");
const accumulate = @import("accumulate.zig");
const extent = @import("extent_overlay.zig");
const device_overlay = @import("device_overlay.zig");
const enospc = @import("enospc.zig");
const epoch_mod = @import("write_epoch.zig");

pub const Binding = seams.Binding;

pub const Source = enum(u8) { accumulation, extent, device_overlay, staged };

/// What of a window the view filled — EXACT in both shapes: the
/// accumulation buffer's byte-granular runs (its union holds ≤ `max_runs`
/// by construction) or the page overlay's page set (a touched page is
/// content-valid whole; 1024 pages at the format's largest block). A run
/// table that could drop runs past its capacity would hand the base's
/// bytes to a reader over acked ones.
pub const Covered = union(enum) {
    runs: cov.CoverageUnion,
    pages: extent.PageSet,

    pub fn contains(self: *const Covered, start: u32, end: u32) bool {
        switch (self.*) {
            .runs => |*u| return u.contains(start, end),
            .pages => |*s| {
                var p = start / extent.PAGE;
                const last = (end - 1) / extent.PAGE;
                while (p <= last) : (p += 1) if (!s.isSet(p)) return false;
                return true;
            },
        }
    }

    /// The gaps of `[rel, end)` in ascending order (contiguous untouched
    /// pages coalesce into one gap: one base read each).
    pub fn gaps(self: *const Covered, rel: u32, end: u32) GapIter {
        var it: GapIter = .{ .rel = rel, .end = end, .cursor = rel, .shape = undefined };
        switch (self.*) {
            .runs => |*u| {
                it.shape = .{ .runs = .{} };
                it.shape.runs.n = u.coveredRunsIn(rel, end, &it.shape.runs.buf).len;
            },
            .pages => |*s| it.shape = .{ .pages = s.* },
        }
        return it;
    }

    pub const GapIter = struct {
        rel: u32,
        end: u32,
        cursor: u32,
        shape: union(enum) {
            runs: struct { buf: [cov.CoverageUnion.max_runs]cov.Run = undefined, n: usize = 0, i: usize = 0 },
            pages: extent.PageSet,
        },

        pub fn next(self: *GapIter) ?cov.Run {
            switch (self.shape) {
                .runs => |*r| {
                    while (r.i < r.n) {
                        const run = r.buf[r.i];
                        r.i += 1;
                        if (run.start > self.cursor) {
                            const g: cov.Run = .{ .start = self.cursor, .end = run.start };
                            self.cursor = run.end;
                            return g;
                        }
                        self.cursor = @max(self.cursor, run.end);
                    }
                    if (self.cursor < self.end) {
                        const g: cov.Run = .{ .start = self.cursor, .end = self.end };
                        self.cursor = self.end;
                        return g;
                    }
                    return null;
                },
                .pages => |*s| {
                    while (self.cursor < self.end) {
                        const p = self.cursor / extent.PAGE;
                        if (s.isSet(p)) {
                            self.cursor = @min(self.end, (p + 1) * extent.PAGE);
                            continue;
                        }
                        // An untouched page: extend over the untouched run.
                        var q = p;
                        while ((q + 1) * extent.PAGE < self.end and !s.isSet(q + 1)) : (q += 1) {}
                        const g: cov.Run = .{ .start = self.cursor, .end = @min(self.end, (q + 1) * extent.PAGE) };
                        self.cursor = g.end;
                        return g;
                    }
                    return null;
                },
            }
        }
    };
};

pub const Partial = struct {
    /// What of `out` the view filled (block-relative, clipped to the window).
    covered: Covered,
    /// Where the gaps come from (null ⇒ zeros: a hole).
    base: ?Binding,
};

pub const Outcome = union(enum) {
    served: Source,
    partial: Partial,
    redirect: Binding,
    none,
    settle,
    /// A try-only request met custody it would have had to WAIT for (a
    /// held block lock, an overlay record mid-store): the probe demotes
    /// to a handler lane, which asks again blocking.
    busy,
};

pub const Request = struct {
    ino: u64,
    block: u32,
    /// Byte offset within the block.
    rel: u32,
    out: []u8,
    /// The FUSE queue worker's warm-ladder probe (charter §4 "the reap
    /// thread never blocks on any arm"): never park on the block lock,
    /// never await an overlay's pages — answer `busy` instead. A lane's
    /// read passes false and waits like every other (3) taker. The lock
    /// the probe would park on is held by a WRITE that may be awaiting a
    /// CQE on THIS worker's ring (the held-slot direct store, the settle):
    /// the park is a deadlock, not a wait (LTP rwtest03, rung 9).
    try_only: bool = false,
};

pub const stats = struct {
    pub var write_side_serves: counters.ShardedCounter = .{};
    pub var write_side_partials: counters.ShardedCounter = .{};
    pub var write_side_redirects: counters.ShardedCounter = .{};
    pub var write_side_settles: counters.ShardedCounter = .{};
    /// Try-only probes that met custody they would have waited for.
    pub var write_side_busy: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"write_side_serves\":{d},\"write_side_partials\":{d},\"write_side_redirects\":{d},\"write_side_settles\":{d},\"write_side_busy\":{d}}}", .{ stats.write_side_serves.load(), stats.write_side_partials.load(), stats.write_side_redirects.load(), stats.write_side_settles.load(), stats.write_side_busy.load() });
}

pub const WriteSideView = struct {
    ctx: ?*anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        serve: *const fn (ctx: ?*anyopaque, io: Io, req: Request) Outcome,
        /// Any write-side custody of the block (the W1/overlay predicates'
        /// probe and the read path's precedence check).
        has_custody: *const fn (ctx: ?*anyopaque, ino: u64, block: u32) bool,
        /// The rewrite epoch's shadow binding (the refetch-compose source).
        shadow_binding: *const fn (ctx: ?*anyopaque, ino: u64, block: u32) ?Binding,
    };

    pub fn serve(self: WriteSideView, io: Io, req: Request) Outcome {
        return self.vtable.serve(self.ctx, io, req);
    }

    pub fn hasCustody(self: WriteSideView, ino: u64, block: u32) bool {
        return self.vtable.has_custody(self.ctx, ino, block);
    }

    pub fn shadowBinding(self: WriteSideView, ino: u64, block: u32) ?Binding {
        return self.vtable.shadow_binding(self.ctx, ino, block);
    }

    /// Before the write plane binds: nothing is dirty.
    pub const none: WriteSideView = .{ .ctx = null, .vtable = &none_vtable };

    const none_vtable: VTable = .{ .serve = &noneServe, .has_custody = &noneCustody, .shadow_binding = &noneShadow };

    fn noneServe(_: ?*anyopaque, _: Io, _: Request) Outcome {
        return .none;
    }
    fn noneCustody(_: ?*anyopaque, _: u64, _: u32) bool {
        return false;
    }
    fn noneShadow(_: ?*anyopaque, _: u64, _: u32) ?Binding {
        return null;
    }
};

/// The RAM default: the composition over the write plane's own tables in
/// the charter's precedence.
pub const Composite = struct {
    table: *accumulate.Table,
    overlay: *device_overlay.Overlay,
    epochs: *epoch_mod.Epochs,
    locks: *seams.BlockLocks,
    staged: ?Staged = null,
    /// The device: a READ of a block whose deferred page overlay still has
    /// unseeded pages seeds them here first (the base's page bytes — a
    /// passthrough mapping's own), so the page coverage it answers is
    /// exact. Null (a fixture without a device) serves the masked pages'
    /// covered bytes alone.
    device: ?seams.DeviceSeam = null,

    /// The seed's base reader: `binding.offset + rel` on the device.
    const SeedBase = struct {
        device: seams.DeviceSeam,
        binding: Binding,

        fn read(ctx: ?*anyopaque, rel: u32, dst: []u8) seams.ReadError!void {
            const self: *const SeedBase = @ptrCast(@alignCast(ctx.?));
            return self.device.read(self.binding.vol_tag, self.binding.offset + rel, dst);
        }
    };

    pub const Staged = struct {
        pool: *staging.Pool,
        seg: *staging.Segment,
        /// The staged file's ring key for `ino` (null = not staged).
        key_of: *const fn (ctx: ?*anyopaque, ino: u64) ?staging.StagingKey,
        ctx: ?*anyopaque = null,
    };

    pub fn view(self: *Composite) WriteSideView {
        return .{ .ctx = self, .vtable = &vtable };
    }

    const vtable: WriteSideView.VTable = .{ .serve = &serve, .has_custody = &hasCustody, .shadow_binding = &shadowBinding };

    fn serve(ctx: ?*anyopaque, io: Io, req: Request) Outcome {
        const self: *Composite = @ptrCast(@alignCast(ctx.?));
        const end: u32 = req.rel + @as(u32, @intCast(req.out.len));
        // 1. The accumulation buffer's coverage-aware snapshot (the short
        //    shared section under the block lock). A partial's gaps come
        //    from the write-side authority's base: the epoch's newest
        //    shadow (open or saving) outranks the binding the checkout saw.
        if (self.table.has(req.ino, req.block)) {
            const g = if (req.try_only)
                (self.locks.tryAcquire(io, req.ino, req.block) orelse {
                    stats.write_side_busy.inc();
                    return .busy;
                })
            else
                self.locks.acquire(io, req.ino, req.block, .read);
            defer g.release(io);
            if (self.table.get(req.ino, req.block)) |e| {
                const base = self.epochs.shadowOf(req.ino, req.block) orelse e.old_binding;
                switch (e.repr) {
                    .full => {
                        const snap = self.table.snapshot(e);
                        defer snap.release(&self.table.pool);
                        // A content-valid buffer is the block (its gaps hold
                        // the zeros or the base its exit established): the
                        // window verbatim. A fresh one's gaps are zeros by
                        // definition; a deferred one's are the base's.
                        if (e.content_valid) {
                            @memcpy(req.out, snap.bytes()[req.rel..end]);
                            stats.write_side_serves.inc();
                            return .{ .served = .accumulation };
                        }
                        snap.coveredRead(req.rel, req.out);
                        if (e.seed == .fresh) {
                            stats.write_side_serves.inc();
                            return .{ .served = .accumulation };
                        }
                        stats.write_side_partials.inc();
                        return .{ .partial = .{ .covered = .{ .runs = clip(&snap.coverage, req.rel, end) }, .base = base } };
                    },
                    .extent => |*x| {
                        if (!x.base_deferred) {
                            x.coveredRead(req.rel, req.out);
                            stats.write_side_serves.inc();
                            return .{ .served = .extent };
                        }
                        // A deferred overlay's masked pages take their base
                        // now (a device act — never on a try-only probe), so
                        // every touched page is whole and the page set exact.
                        if (x.hasUnseeded()) {
                            if (req.try_only) {
                                stats.write_side_busy.inc();
                                return .busy;
                            }
                            // A base that cannot be read leaves the pages
                            // masked: their uncovered bytes serve as zeros
                            // (the gap fill's own law on a failed read) and
                            // the next read retries the seed.
                            if (self.device) |dev| if (base) |b| {
                                const sb: SeedBase = .{ .device = dev, .binding = b };
                                x.seedPagewise(self.table.gpa, .{ .ctx = @constCast(&sb), .read = &SeedBase.read }) catch {};
                            };
                        }
                        x.coveredRead(req.rel, req.out);
                        stats.write_side_partials.inc();
                        return .{ .partial = .{ .covered = .{ .pages = x.pageSet() }, .base = base } };
                    },
                }
            }
        }
        // 2. An open device-overlay record: snapshot (g, C, dest) + revalidate
        //    (awaits in-flight pages and reads the device: a lane's act).
        if (req.try_only and self.overlay.hasCustody(req.ino, req.block)) {
            stats.write_side_busy.inc();
            return .busy;
        }
        switch (self.overlay.serve(io, req.ino, req.block, req.rel, req.out)) {
            .served => {
                stats.write_side_serves.inc();
                return .{ .served = .device_overlay };
            },
            .settle => {
                stats.write_side_settles.inc();
                return .settle;
            },
            .none => {},
        }
        // 3. The rewrite epoch's shadow (the newest B key).
        if (self.epochs.shadowOf(req.ino, req.block)) |b| {
            stats.write_side_redirects.inc();
            return .{ .redirect = b };
        }
        // 4. A staged entry (this client's ring).
        if (self.staged) |st| if (st.key_of(st.ctx, req.ino)) |key| {
            if (st.seg.get(key)) |lease| {
                defer lease.release();
                const bytes = lease.bytes();
                const start: usize = @as(usize, req.block) * self.table.block_size + req.rel;
                @memset(req.out, 0);
                if (start < bytes.len) {
                    const n = @min(req.out.len, bytes.len - start);
                    @memcpy(req.out[0..n], bytes[start .. start + n]);
                }
                stats.write_side_serves.inc();
                return .{ .served = .staged };
            }
        };
        return .none;
    }

    fn clip(u: *const cov.CoverageUnion, rel: u32, end: u32) cov.CoverageUnion {
        var out: cov.CoverageUnion = .empty;
        var runs: [cov.CoverageUnion.max_runs]cov.Run = undefined;
        for (u.coveredRunsIn(rel, end, &runs)) |r| _ = out.record(r.start, r.end, end) catch {};
        return out;
    }

    fn hasCustody(ctx: ?*anyopaque, ino: u64, block: u32) bool {
        const self: *Composite = @ptrCast(@alignCast(ctx.?));
        return self.table.has(ino, block) or self.overlay.hasCustody(ino, block) or self.epochs.bindsBlock(ino, block);
    }

    fn shadowBinding(ctx: ?*anyopaque, ino: u64, block: u32) ?Binding {
        const self: *Composite = @ptrCast(@alignCast(ctx.?));
        return self.epochs.shadowOf(ino, block);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");
const layout = @import("layout.zig");
const block_alloc = @import("block_alloc.zig");
const pipeline = @import("write_pipeline.zig");

const ViewWorld = struct {
    f: *wt.Fixture,
    table: accumulate.Table,
    epochs: epoch_mod.Epochs,
    stripes: layout.MetaStripes = .{},
    locks: seams.BlockLocks,
    gov: pipeline.Governor,
    ov: device_overlay.Overlay,
    comp: Composite,

    fn init(gpa: std.mem.Allocator) !*ViewWorld {
        const w = try gpa.create(ViewWorld);
        errdefer gpa.destroy(w);
        w.* = .{ .f = try wt.Fixture.init(gpa, 16), .table = try accumulate.Table.init(gpa, @intCast(wt.BS), 4), .epochs = epoch_mod.Epochs.init(gpa, wt.BS, true), .locks = try seams.BlockLocks.init(gpa, 64), .gov = try pipeline.Governor.init(gpa, wt.BS, 2, .adaptive, null, pipeline.capBytes(1 << 30)), .ov = undefined, .comp = undefined };
        w.ov = try device_overlay.Overlay.init(gpa, .{ .block_size = wt.BS, .patch_max = wt.BS / 8 }, .{ .device = w.f.dev.seam(), .map = w.f.map.seam(), .free = w.f.frees.seam(), .purge = .none, .vols = &w.f.vols, .picker = .{ .ctx = w.f.alloc_a, .pick = &pickA }, .epochs = &w.epochs, .stripes = &w.stripes, .governor = &w.gov }, 8);
        w.comp = .{ .table = &w.table, .overlay = &w.ov, .epochs = &w.epochs, .locks = &w.locks, .device = w.f.dev.seam() };
        return w;
    }

    fn deinit(self: *ViewWorld, gpa: std.mem.Allocator) void {
        self.ov.deinit(testing.io);
        self.gov.deinit();
        self.locks.deinit(gpa);
        self.epochs.deinit();
        self.table.deinit();
        self.f.deinit();
        gpa.destroy(self);
    }

    fn pickA(ctx: ?*anyopaque, _: enospc.Place) ?*block_alloc.BlockAllocator {
        return @ptrCast(@alignCast(ctx.?));
    }
};

test "the seam: none answers nothing; a fresh accumulation buffer serves its covered runs with zero gaps (authoritative); a deferred-seed buffer answers partial with the old binding as the base; an extent overlay over a mapped block likewise" {
    const io = testing.io;
    const w = try ViewWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    const v = w.comp.view();
    var out: [wt.BS]u8 = undefined;
    try testing.expectEqual(Outcome.none, WriteSideView.none.serve(io, .{ .ino = 1, .block = 0, .rel = 0, .out = &out }));
    try testing.expectEqual(Outcome.none, v.serve(io, .{ .ino = 1, .block = 0, .rel = 0, .out = &out }));
    try testing.expect(!v.hasCustody(1, 0));
    // Fresh buffer.
    const e = w.table.checkout(1, 0, false, null).?;
    var q: [wt.BS / 4]u8 = @splat(0x44);
    _ = try w.table.recordWrite(e, wt.BS / 2, &q);
    try testing.expect(v.hasCustody(1, 0));
    try testing.expectEqual(Outcome{ .served = .accumulation }, v.serve(io, .{ .ino = 1, .block = 0, .rel = 0, .out = &out }));
    try testing.expect(std.mem.allEqual(u8, out[0 .. wt.BS / 2], 0));
    try testing.expect(std.mem.allEqual(u8, out[wt.BS / 2 .. 3 * wt.BS / 4], 0x44));
    // Deferred seed: partial, base = the old binding, covered = the clipped runs.
    const a = try w.f.publishBlock(2, 0, 0xAA);
    const d = w.table.checkout(2, 0, true, a).?;
    _ = try w.table.recordWrite(d, wt.BS / 2, &q);
    var half: [wt.BS / 2]u8 = undefined;
    const o = v.serve(io, .{ .ino = 2, .block = 0, .rel = wt.BS / 4, .out = &half });
    try testing.expect(o == .partial);
    try testing.expect(o.partial.base.?.eql(a));
    try testing.expect(o.partial.covered.contains(wt.BS / 2, 3 * wt.BS / 4));
    try testing.expect(!o.partial.covered.contains(wt.BS / 4, wt.BS / 2));
    try testing.expect(std.mem.allEqual(u8, half[wt.BS / 4 ..], 0x44));
    w.table.retire(d);
    // Extent overlay over a mapped block: partial as well, page-exact —
    // the touched page is whole, every other page a gap for the base.
    const x = w.table.checkoutExtent(2, 0, true, a).?;
    try x.repr.extent.merge(testing.allocator, 4096 + 100, q[0..1000]);
    const o2 = v.serve(io, .{ .ino = 2, .block = 0, .rel = 0, .out = &out });
    try testing.expect(o2 == .partial and o2.partial.covered.contains(4096, 8192));
    try testing.expect(!o2.partial.covered.contains(0, 4096));
    try testing.expect(std.mem.allEqual(u8, out[4096 + 100 .. 4096 + 1100], 0x44));
    var gaps = o2.partial.covered.gaps(0, @intCast(wt.BS));
    try testing.expectEqual(cov.Run{ .start = 0, .end = 4096 }, gaps.next().?);
    try testing.expectEqual(cov.Run{ .start = 8192, .end = @intCast(wt.BS) }, gaps.next().?);
    try testing.expect(gaps.next() == null);
    w.table.retire(x);
    w.table.retire(e);
}

test "Covered.gaps is exact in both shapes: byte runs (a deferred buffer's clipped union) and the page set (a scattered overlay past any run table's capacity: every untouched page is a gap, contiguous ones coalesce, the window clips)" {
    // Runs: covered [100, 200) and [300, 400) of a [50, 450) window.
    var u: cov.CoverageUnion = .empty;
    _ = try u.record(100, 200, 4096);
    _ = try u.record(300, 400, 4096);
    const c: Covered = .{ .runs = u };
    var it = c.gaps(50, 450);
    try testing.expectEqual(cov.Run{ .start = 50, .end = 100 }, it.next().?);
    try testing.expectEqual(cov.Run{ .start = 200, .end = 300 }, it.next().?);
    try testing.expectEqual(cov.Run{ .start = 400, .end = 450 }, it.next().?);
    try testing.expect(it.next() == null);
    var it2 = c.gaps(100, 200);
    try testing.expect(it2.next() == null);
    // Pages: every other page of a 1024-page block touched — 512 runs, far
    // past the 65 the union could hold; each untouched page is one gap.
    var s = extent.PageSet.empty;
    var p: usize = 0;
    while (p < extent.MAX_PAGES) : (p += 2) s.set(p);
    const pc: Covered = .{ .pages = s };
    const P = extent.PAGE;
    var n: usize = 0;
    var it3 = pc.gaps(0, extent.MAX_PAGES * P);
    while (it3.next()) |g| : (n += 1) {
        try testing.expectEqual(g.start + P, g.end);
        try testing.expectEqual(@as(u32, 1), (g.start / P) % 2);
    }
    try testing.expectEqual(extent.MAX_PAGES / 2, n);
    try testing.expect(pc.contains(0, P) and !pc.contains(0, 2 * P));
    // A window inside one untouched page clips; contiguous untouched pages coalesce.
    var s2 = extent.PageSet.empty;
    s2.set(5);
    const pc2: Covered = .{ .pages = s2 };
    var it4 = pc2.gaps(3 * P + 10, 9 * P - 10);
    try testing.expectEqual(cov.Run{ .start = 3 * P + 10, .end = 5 * P }, it4.next().?);
    try testing.expectEqual(cov.Run{ .start = 6 * P, .end = 9 * P - 10 }, it4.next().?);
    try testing.expect(it4.next() == null);
}

test "precedence: an open overlay record serves (composed over the old binding) ahead of the epoch shadow; a shadowed block redirects to the B key; a block with no custody falls through" {
    const io = testing.io;
    const w = try ViewWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    const v = w.comp.view();
    const a = try w.f.publishBlock(3, 0, 0xAA);
    var p: [wt.BS / 2]u8 align(4096) = @splat(0x55);
    _ = try w.ov.store(io, .{ .ino = 3, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = .{ .kind = .striped, .size = wt.BS, .map_resident = true }, .overwrite = true });
    var out: [wt.BS]u8 = undefined;
    try testing.expectEqual(Outcome{ .served = .device_overlay }, v.serve(io, .{ .ino = 3, .block = 0, .rel = 0, .out = &out }));
    try testing.expect(std.mem.allEqual(u8, out[0 .. wt.BS / 2], 0x55));
    try testing.expect(std.mem.allEqual(u8, out[wt.BS / 2 ..], 0xAA));
    try testing.expect(v.hasCustody(3, 0));
    // The drain feeds the epoch: now the view redirects to the shadow B key.
    const cs: epoch_mod.Epochs.CloseSeams = .{ .map = w.f.map.seam(), .free = w.f.frees.seam(), .purge = .none };
    w.ov.drainBlock(io, &w.locks, 3, 0, false, cs);
    const o = v.serve(io, .{ .ino = 3, .block = 0, .rel = 0, .out = &out });
    try testing.expect(o == .redirect);
    try testing.expect(!o.redirect.eql(a));
    try testing.expect(v.shadowBinding(3, 0).?.eql(o.redirect));
    try testing.expect(v.hasCustody(3, 0));
    _ = w.epochs.close(io, 3, cs);
    try testing.expectEqual(Outcome.none, v.serve(io, .{ .ino = 3, .block = 0, .rel = 0, .out = &out }));
    try testing.expect(!v.hasCustody(3, 0));
    try testing.expect(stats.write_side_redirects.load() >= 1);
}

test "try-only (the uring worker's warm-ladder probe): a held block lock answers `busy` at once — never a park on the reap thread; an open overlay record answers `busy` (its serve awaits pages and reads the device); a free lock serves as before (repro-port: LTP rwtest03 wedged a FUSE queue worker in BlockLocks.acquire(read) under a write that held the lock while awaiting that worker's ring)" {
    const io = testing.io;
    const w = try ViewWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    const v = w.comp.view();
    var out: [wt.BS]u8 = undefined;
    const e = w.table.checkout(1, 0, false, null).?;
    var q: [wt.BS / 4]u8 = @splat(0x44);
    _ = try w.table.recordWrite(e, wt.BS / 2, &q);
    // The write side holds (3) for (1, 0): a blocking serve would park here.
    const held = w.locks.acquire(io, 1, 0, .write_checkout);
    const busy0 = stats.write_side_busy.load();
    const t0 = clock.monotonicNs();
    try testing.expectEqual(Outcome.busy, v.serve(io, .{ .ino = 1, .block = 0, .rel = 0, .out = &out, .try_only = true }));
    try testing.expect(clock.monotonicNs() - t0 < 100 * std.time.ns_per_ms);
    try testing.expectEqual(busy0 + 1, stats.write_side_busy.load());
    held.release(io);
    // Free again: the try-only probe serves exactly what the blocking serve does.
    try testing.expectEqual(Outcome{ .served = .accumulation }, v.serve(io, .{ .ino = 1, .block = 0, .rel = 0, .out = &out, .try_only = true }));
    try testing.expect(std.mem.allEqual(u8, out[wt.BS / 2 .. 3 * wt.BS / 4], 0x44));
    w.table.retire(e);
    // An open overlay record: busy for the probe, served for the lane.
    _ = try w.f.publishBlock(3, 0, 0xAA);
    var p: [wt.BS / 2]u8 align(4096) = @splat(0x55);
    _ = try w.ov.store(io, .{ .ino = 3, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = .{ .kind = .striped, .size = wt.BS, .map_resident = true }, .overwrite = true });
    try testing.expectEqual(Outcome.busy, v.serve(io, .{ .ino = 3, .block = 0, .rel = 0, .out = &out, .try_only = true }));
    try testing.expectEqual(Outcome{ .served = .device_overlay }, v.serve(io, .{ .ino = 3, .block = 0, .rel = 0, .out = &out }));
    const cs: epoch_mod.Epochs.CloseSeams = .{ .map = w.f.map.seam(), .free = w.f.frees.seam(), .purge = .none };
    w.ov.drainBlock(io, &w.locks, 3, 0, false, cs);
    _ = w.epochs.close(io, 3, cs);
}
