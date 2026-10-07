//! The warm ladder — the TRY-ONLY probe the transport calls on the queue
//! thread (charter §4 READ dispatch law arm (a): "serve inline if the
//! filesystem's try-only warm ladder answers (per-inode try-lock → active
//! buffer → staging ring → hot tier → read-lane hold → NVMe read cache)";
//! `docs/porting/data_read_path.md` §10). PORTABLE over `read_path.ReadPath`.
//!
//! What the term kills: the hop reap thread → inbound queue → session
//! dispatch task → lane spawn → first poll measured `queue_wait` 69 µs +
//! `dispatch_lag` 89 µs = 158 µs of a 439 µs 4 KiB random read (36 %), with
//! the device at 40 µs; unloaded 6.7 µs hop vs 53 ns inline serve.
//!
//! LAW: nothing in rungs 0–5 awaits, spins, allocates per op or touches a
//! device. A contended per-inode guard is a DEMOTE, never a wait; every
//! miss is a Demote; the handler lane then owns the request (and every
//! count — the ladder is silent on Demote). `Demote` conditions exactly
//! per §10: virtual/zero size, device-true O_DIRECT (a tier serve would
//! turn a device-true row warm — measurement fraud), retained extents, a
//! request wider than the window (FUSE-4e: never a short read below EOF),
//! a contended guard, no size authority, a multi-block request, an open
//! device overlay, W2 runs, an active buffer that does not contain the
//! window, a hole/NeedsAuthority key, every tier missed.
//!
//! Binding currency on rung 4 is structural: the caller holds the inode's
//! shared guard and the key came from the CURRENT map; this daemon's
//! writers update that map synchronously under the same guard.
//!
//! The zc legs: `directLeg` computes the PURE eligibility of the direct
//! device leg (cold ∧ passthrough ∧ 4 KiB-aligned ∧ single block ∧ not
//! device-true ∧ resolvable) and `fdSource` is the fill-pool bounce (the
//! handler's `replyFdBody(memfd, off)` from the MAP_SHARED slab).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const layout = @import("layout.zig");
const rsc = @import("read_size_class.zig");
const read_path = @import("read_path.zig");
const hot_tier = @import("hot_tier.zig");
const fsv = @import("../fuse/transport/filesystem_vtable.zig");
const codec = @import("../fuse/codec.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");

pub const BlockKey = rsc.BlockKey;

pub const stats = struct {
    /// `fast_dispatch_serves`: READs the ladder answered inline.
    pub var fast_dispatch_serves: counters.ShardedCounter = .{};
    /// `fast_dispatch_demotes`: READs the ladder declined (by reason below).
    pub var fast_dispatch_demotes: counters.ShardedCounter = .{};
    pub var fast_dispatch_demote_pre_screen: counters.ShardedCounter = .{};
    pub var fast_dispatch_demote_contended: counters.ShardedCounter = .{};
    pub var fast_dispatch_demote_write_side: counters.ShardedCounter = .{};
    pub var fast_dispatch_demote_miss: counters.ShardedCounter = .{};
    pub var fast_dispatch_eof: counters.ShardedCounter = .{};
    pub var ipc_hold_probe_serves: counters.ShardedCounter = .{};
    pub var ipc_hold_probe_misses: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_inline: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_staged: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_hot: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_hold: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_cache: counters.ShardedCounter = .{};
    /// Direct legs the predicate admitted (the transport bridges them).
    pub var direct_leg_offers: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"fast_dispatch_serves\":{d},\"fast_dispatch_demotes\":{d},\"fast_dispatch_demote_pre_screen\":{d},\"fast_dispatch_demote_contended\":{d},\"fast_dispatch_demote_write_side\":{d},\"fast_dispatch_demote_miss\":{d},\"fast_dispatch_eof\":{d},\"ipc_hold_probe_serves\":{d},\"ipc_hold_probe_misses\":{d},\"fast_dispatch_serve_inline\":{d},\"fast_dispatch_serve_staged\":{d},\"fast_dispatch_serve_hot\":{d},\"fast_dispatch_serve_hold\":{d},\"fast_dispatch_serve_cache\":{d},\"direct_leg_offers\":{d}}}", .{
        stats.fast_dispatch_serves.load(),
        stats.fast_dispatch_demotes.load(),
        stats.fast_dispatch_demote_pre_screen.load(),
        stats.fast_dispatch_demote_contended.load(),
        stats.fast_dispatch_demote_write_side.load(),
        stats.fast_dispatch_demote_miss.load(),
        stats.fast_dispatch_eof.load(),
        stats.ipc_hold_probe_serves.load(),
        stats.ipc_hold_probe_misses.load(),
        stats.fast_dispatch_serve_inline.load(),
        stats.fast_dispatch_serve_staged.load(),
        stats.fast_dispatch_serve_hot.load(),
        stats.fast_dispatch_serve_hold.load(),
        stats.fast_dispatch_serve_cache.load(),
        stats.direct_leg_offers.load(),
    });
}

/// Why the ladder declined (the handler owns the request from here).
pub const Demote = enum(u8) {
    pre_screen,
    contended,
    write_side,
    miss,
};

/// Which rung served (the ledger's warm split).
pub const Arm = enum(u8) { inline_, staged, hot, hold, cache };

pub const Verdict = union(enum) {
    served: struct { n: usize, arm: Arm },
    eof,
    demote: Demote,
};

/// The pure eligibility of the direct device leg: cold is the CALLER's
/// premise (the probe declined first).
pub fn directLegEligible(passthrough: bool, device_true: bool, offset: u64, size: u32, block_size: u64) bool {
    if (!passthrough or device_true or size == 0) return false;
    if (!derive.isAligned(offset, rsc.LBA_GRAIN) or !derive.isAligned(size, rsc.LBA_GRAIN)) return false;
    return offset / block_size == (offset + size - 1) / block_size;
}

pub const WarmLadder = struct {
    rp: *read_path.ReadPath,
    guards: *op_guard.Table,
    io: Io,
    /// The direct device leg is OFFERED only once the transport can run the
    /// post-DMA incarnation + binding recheck at its bridge completion (the
    /// reference validated after its synchronous fetch; `filesystem_vtable`
    /// has no completion hook yet). Until then the leg declines — never
    /// unvalidated device bytes in an app's pages.
    direct_leg_armed: bool = false,

    /// The probe: serve `[offset, offset + size)` of `ino` INTO `window`.
    pub fn probe(self: *WarmLadder, ino: u64, offset: u64, size: u32, odirect: bool, window: []u8) Verdict {
        const rp = self.rp;
        // 0. Pre-screen.
        if (size == 0) return self.demote(.pre_screen);
        if (odirect and rp.cfg.direct_device_true) return self.demote(.pre_screen);
        if (rp.write_side.has_retained_extents(rp.write_side.ctx, ino)) return self.demote(.pre_screen);
        if (size > window.len) return self.demote(.pre_screen);
        // 1. The per-inode TRY-lock: a contended stripe is a Demote.
        const guard = self.guards.tryAcquire(self.io, ino, .shared) orelse return self.demote(.contended);
        defer guard.release(self.io);
        // 2. Size authority.
        const info = rp.meta.file(rp.meta.ctx, ino) orelse return self.demote(.miss);
        if (offset >= info.size) {
            stats.fast_dispatch_eof.inc();
            return .eof;
        }
        const want: usize = @intCast(@min(size, info.size - offset));
        const out = window[0..want];
        const v = self.serve(ino, info, offset, out, odirect) orelse return self.demote(.miss);
        stats.fast_dispatch_serves.inc();
        read_path.stats.read_copy_dest_bytes.add(v.n);
        read_path.stats.read_user_bytes.add(v.n);
        switch (v.arm) {
            .inline_ => stats.fast_dispatch_serve_inline.inc(),
            .staged => stats.fast_dispatch_serve_staged.inc(),
            .hot => {
                stats.fast_dispatch_serve_hot.inc();
                read_path.stats.read_copy_warm_serve_bytes.add(v.n);
                read_path.stats.read_copy_hot_serve_bytes.add(v.n);
            },
            .hold => {
                stats.fast_dispatch_serve_hold.inc();
                read_path.stats.read_copy_warm_serve_bytes.add(v.n);
                read_path.stats.read_copy_hold_serve_bytes.add(v.n);
            },
            .cache => {
                stats.fast_dispatch_serve_cache.inc();
                read_path.stats.read_copy_warm_serve_bytes.add(v.n);
                read_path.stats.read_copy_cache_serve_bytes.add(v.n);
            },
        }
        if (odirect) read_path.stats.read_odirect_requests.inc();
        // 5. The ring feed: silent consumption advances the consume edge.
        rp.ringReadLaneTouch(ino, offset, v.n, info.size);
        return .{ .served = .{ .n = v.n, .arm = v.arm } };
    }

    fn demote(_: *WarmLadder, why: Demote) Verdict {
        stats.fast_dispatch_demotes.inc();
        switch (why) {
            .pre_screen => stats.fast_dispatch_demote_pre_screen.inc(),
            .contended => stats.fast_dispatch_demote_contended.inc(),
            .write_side => stats.fast_dispatch_demote_write_side.inc(),
            .miss => stats.fast_dispatch_demote_miss.inc(),
        }
        return .{ .demote = why };
    }

    const Served = struct { n: usize, arm: Arm };

    /// Rungs 3–4 by layout; null = Miss.
    fn serve(self: *WarmLadder, ino: u64, info: read_path.FileInfo, offset: u64, out: []u8, odirect: bool) ?Served {
        const rp = self.rp;
        const bs = rp.cfg.block_size;
        switch (info.kind) {
            .inline_ => {
                const n = rp.meta.inline_data(rp.meta.ctx, ino, offset, out);
                @memset(out[n..], 0);
                return .{ .n = out.len, .arm = .inline_ };
            },
            .staged => {
                switch (rp.write_side.staged_payload(rp.write_side.ctx, ino, info.file_id, offset, out.len, out)) {
                    .served => |n| {
                        read_path.stats.cache_hits.inc();
                        return .{ .n = n, .arm = .staged };
                    },
                    else => return null,
                }
            },
            // A packed tenant is a device window: the lane's.
            .packed_ => return null,
            .striped => {
                const b = offset / bs;
                const end = offset + out.len;
                if ((end - 1) / bs != b) return null;
                const rel_s = offset - b * bs;
                const rel_e = end - b * bs;
                const ws = rp.write_side;
                if (ws.open_overlay(ws.ctx, ino, b)) return null;
                if (ws.extent_runs(ws.ctx, ino, b, rel_s, rel_e, &.{}) != .none) return null;
                // The staging ring's active block (the write side's custody).
                if (ws.staged_block(ws.ctx, ino, b, rel_s, rel_e, out)) |n| {
                    @memset(out[n..], 0);
                    return .{ .n = out.len, .arm = .staged };
                }
                if (rp.cfg.direct_device_true and odirect) return null;
                const key = rp.meta.binding(rp.meta.ctx, ino, b) orelse return null;
                const k = BlockKey.fromMapping(key);
                if (rp.hot.getServing(k, out.len)) |hit| {
                    defer rp.hot.unpin(hit);
                    @memcpy(out, rp.pool.bytes(hit.slot)[@intCast(rel_s)..@intCast(rel_e)]);
                    hot_tier.stats.hot_block_hits.inc();
                    read_path.stats.cache_hits.inc();
                    if (rp.cfg.read_lane) rp.hold.credit(k, out.len);
                    if (odirect) read_path.stats.read_odirect_tier_serves.inc();
                    return .{ .n = out.len, .arm = .hot };
                }
                if (rp.cfg.read_lane) {
                    if (rp.hold.serve(k, 0)) |sv| {
                        defer rp.hold.unpin(sv);
                        @memcpy(out, rp.pool.bytes(sv.slot)[@intCast(rel_s)..@intCast(rel_e)]);
                        read_path.stats.cache_hits.inc();
                        read_path.stats.read_lane_serves.inc();
                        read_path.stats.read_lane_serve_bytes.add(out.len);
                        stats.ipc_hold_probe_serves.inc();
                        rp.hold.credit(k, out.len);
                        if (odirect) read_path.stats.read_odirect_tier_serves.inc();
                        return .{ .n = out.len, .arm = .hold };
                    }
                    stats.ipc_hold_probe_misses.inc();
                }
                if (rp.nvme.getRange(k, rel_s, rel_e, out)) |n| {
                    @memset(out[n..], 0);
                    read_path.stats.cache_hits.inc();
                    if (rp.cfg.read_lane) rp.hold.credit(k, out.len);
                    if (odirect) read_path.stats.read_odirect_tier_serves.inc();
                    return .{ .n = out.len, .arm = .cache };
                }
                return null;
            },
        }
    }

    /// The direct device leg after the probe declined: `{fd, off, len}`
    /// when the WHOLE reply can DMA from the device into the app's pages
    /// (cold = not in any tier, no flight open), else null.
    pub fn directLeg(self: *WarmLadder, ino: u64, offset: u64, size: u32, odirect: bool) ?fsv.DirectLeg {
        if (!self.direct_leg_armed) return null;
        const rp = self.rp;
        const device_true = rp.cfg.direct_device_true and odirect;
        if (!directLegEligible(rp.cfg.passthrough, device_true, offset, size, rp.cfg.block_size)) return null;
        const info = rp.meta.file(rp.meta.ctx, ino) orelse return null;
        if (offset + size > info.size or info.kind != .striped) return null;
        const b = offset / rp.cfg.block_size;
        const m = rp.meta.binding(rp.meta.ctx, ino, b) orelse return null;
        const k = BlockKey.fromMapping(m);
        if (rp.hot.contains(k) or rp.hold.contains(k) or rp.nvme.has(k) or rp.flights.inFlight(self.io, k)) return null;
        const fd = rp.vols.device_fd(rp.vols.ctx, k.vol_tag) orelse return null;
        stats.direct_leg_offers.inc();
        return .{ .fd = fd, .off = k.offset + (offset - b * rp.cfg.block_size), .len = size };
    }

    /// The fill-pool bounce for the handler: `(memfd, off)` of a served
    /// view when it lies on the MAP_SHARED slab (`replyFdBody`), null ⇒ the
    /// copy arm.
    pub fn fdSource(self: *WarmLadder, view: []const u8) ?@import("read_fill_pool.zig").FdOffset {
        return self.rp.fdSource(view);
    }

    // ---- the `Filesystem.VTable` arms (the lifecycle layer wires them) ----

    /// `probe_read(ctx, req, window)`.
    pub fn probeRead(self: *WarmLadder, req: fsv.Request, window: []u8) ?codec.Reply {
        const in = switch (req.op) {
            .read => |r| r,
            else => return null,
        };
        const o_direct: u32 = @bitCast(linux.O{ .DIRECT = true });
        return switch (self.probe(req.nodeid(), in.offset, in.size, in.flags & o_direct != 0, window)) {
            .served => |s| codec.replyData(req.unique(), window[0..s.n]),
            .eof => codec.replyData(req.unique(), &.{}),
            .demote => null,
        };
    }

    /// `direct_leg(ctx, req)`.
    pub fn directLegFor(self: *WarmLadder, req: fsv.Request) ?fsv.DirectLeg {
        const in = switch (req.op) {
            .read => |r| r,
            else => return null,
        };
        const o_direct: u32 = @bitCast(linux.O{ .DIRECT = true });
        return self.directLeg(req.nodeid(), in.offset, in.size, in.flags & o_direct != 0);
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "direct-leg eligibility: passthrough ∧ !device_true ∧ 4 KiB-aligned ∧ single block" {
    const bs: u64 = 4 << 20;
    try testing.expect(directLegEligible(true, false, 0, 4096, bs));
    try testing.expect(directLegEligible(true, false, bs - 4096, 4096, bs));
    try testing.expect(!directLegEligible(true, false, bs - 4096, 8192, bs));
    try testing.expect(!directLegEligible(true, false, 100, 4096, bs));
    try testing.expect(!directLegEligible(true, false, 0, 4000, bs));
    try testing.expect(!directLegEligible(false, false, 0, 4096, bs));
    try testing.expect(!directLegEligible(true, true, 0, 4096, bs));
    try testing.expect(!directLegEligible(true, false, 0, 0, bs));
}

test "the warm ladder never blocks: a cold block demotes (miss), a held guard demotes (contended) without waiting, a tier-resident block serves byte-exact and EOF-clamped into the window, oversize and device-true O_DIRECT pre-screen; serves + demotes ≡ probes" {
    const F = read_path.TestFixture;
    const f = try F.init(testing.allocator, .{});
    defer f.deinit();
    const tail: u64 = 300 * derive.KiB;
    f.size = F.bs + tail;
    f.bind(0, 2);
    f.bind(1, 3);
    var guards = try op_guard.Table.init(testing.allocator, 64);
    defer guards.deinit();
    var ladder: WarmLadder = .{ .rp = f.rp, .guards = &guards, .io = testing.io };
    const window = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), 1 << 20);
    defer testing.allocator.free(window);
    const serves0 = stats.fast_dispatch_serves.load();
    const demotes0 = stats.fast_dispatch_demotes.load();
    var probes: u64 = 0;
    // Cold: a miss, nothing touched the device.
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 0, 4096, false, window));
    probes += 1;
    try testing.expectEqual(@as(u64, 0), f.device_reads.load(.monotonic));
    // The lane fills it; the probe then serves from the hot tier.
    _ = try f.rp.read(testing.io, 7, 0, 1 << 20, .{ .buf = window }, .{});
    @memset(window, 0);
    const v = ladder.probe(7, 8192, 4096, false, window);
    probes += 1;
    try testing.expectEqual(Verdict{ .served = .{ .n = 4096, .arm = .hot } }, v);
    try F.expected(2 * F.bs + 8192, window[0..4096]);
    // EOF clamp: block 1 holds a 300 KiB tail (a whole-block fill: above
    // the ranged threshold); a 4 KiB request 1000 bytes before EOF serves 1000.
    _ = try f.rp.read(testing.io, 7, F.bs, tail, .{ .buf = window }, .{});
    const v2 = ladder.probe(7, F.bs + tail - 1000, 4096, false, window);
    probes += 1;
    try testing.expectEqual(Verdict{ .served = .{ .n = 1000, .arm = .hot } }, v2);
    try F.expected(3 * F.bs + tail - 1000, window[0..1000]);
    try testing.expectEqual(Verdict.eof, ladder.probe(7, F.bs + tail, 4096, false, window));
    probes += 1;
    // Pre-screens.
    try testing.expectEqual(Verdict{ .demote = .pre_screen }, ladder.probe(7, 0, 0, false, window));
    try testing.expectEqual(Verdict{ .demote = .pre_screen }, ladder.probe(7, 0, 4096, false, window[0..100]));
    probes += 2;
    // A held exclusive guard: Demote at once (no wait).
    const held = guards.acquire(testing.io, 7, .exclusive);
    const t0 = clock.monotonicNs();
    try testing.expectEqual(Verdict{ .demote = .contended }, ladder.probe(7, 0, 4096, false, window));
    probes += 1;
    try testing.expect(clock.monotonicNs() - t0 < 100 * std.time.ns_per_ms);
    held.release(testing.io);
    // The hold arm after the hot tier dropped the block: block 0's whole-
    // block read credited its full coverage (retired from the hold); block
    // 1's 300 KiB read left the hold entry 700 KiB short of retirement.
    const k0 = BlockKey.init(F.vol_tag, 2 * F.bs);
    try testing.expect(!f.rp.hold.contains(k0));
    const k1 = BlockKey.init(F.vol_tag, 3 * F.bs);
    try testing.expect(f.rp.hot.purge(k1));
    try testing.expect(f.rp.hold.contains(k1));
    try testing.expectEqual(Verdict{ .served = .{ .n = 4096, .arm = .hold } }, ladder.probe(7, F.bs + 4096, 4096, false, window));
    probes += 1;
    try F.expected(3 * F.bs + 4096, window[0..4096]);
    // The partition law.
    try testing.expectEqual(probes - 1, (stats.fast_dispatch_serves.load() - serves0) + (stats.fast_dispatch_demotes.load() - demotes0));
    // Device-true O_DIRECT pre-screens; the direct leg declines unarmed and
    // offers on an aligned cold single-block request once armed.
    const g = try F.init(testing.allocator, .{ .direct_device_true = true });
    defer g.deinit();
    g.size = F.bs;
    g.bind(0, 4);
    var ladder2: WarmLadder = .{ .rp = g.rp, .guards = &guards, .io = testing.io };
    try testing.expectEqual(Verdict{ .demote = .pre_screen }, ladder2.probe(7, 0, 4096, true, window));
    try testing.expect(ladder2.directLeg(7, 0, 4096, false) == null);
    ladder2.direct_leg_armed = true;
    // The fixture resolves no device fd: still null; the predicate itself passed.
    try testing.expect(ladder2.directLeg(7, 0, 4096, false) == null);
    try testing.expect(ladder.fdSource(window) == null);
}
