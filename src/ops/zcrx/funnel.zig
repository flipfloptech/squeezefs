//! The read path's ONE funnel for a cold device read (charter §6.3;
//! `docs/porting/ops_zcrx.md` §8): behind the data plane's
//! `Volumes.read_range` seam, judged PER CALL — `try the lane FIRST`; the
//! lane serves (ONE gather into the caller's destination, no DMA counted)
//! or the kernel path serves, loud once per device. Portable over a `Lane`
//! face (the real session on Linux, a double in tests); the decorator
//! `volumesOver` wraps any `read_path.Volumes`.
//!
//! THREE mutually exclusive outcomes per call: `zcrx_fills` (the lane
//! served — counted by the session at the whole-read success boundary with
//! `fill_bytes`/`gather_bytes`), `zcrx_kernel_fills` (no lane on the
//! volume, or the range is ineligible — ineligibility is uncounted as a
//! refusal BY DESIGN: the kernel path is simply the serve), and
//! `zcrx_lane_refused` (a lane was present and did not serve), split by
//! reason into the spec's three exclusive gate counters:
//! `zcrx_degraded_bypasses` (the pool term — a starved queue; formerly the
//! UNCOUNTED gate that held two campaigns at ~0 engagement),
//! `zcrx_area_admission_waits` (the window — counted at the decline site),
//! `zcrx_fill_fallbacks` (a per-op error: LOUD, rate-limited, then the
//! kernel path retries into the SAME destination — idempotent, a partial
//! gather is overwritten). A poisoned or structurally starved lane is
//! retired on sight (the session tears down off the read path) and the
//! kernel path serves the device for the rest of the mount.

const std = @import("std");
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const read_path = @import("../../data/read_path.zig");
const stats = @import("stats.zig");

pub const MAX_LANES: usize = 256;

pub const ReadError = error{
    Ineligible,
    WouldBlock,
    Degraded,
    Poisoned,
    OpStatus,
    OpFailed,
};

/// The session's face.
pub const Lane = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        poisoned: *const fn (ctx: *anyopaque) bool,
        structural: *const fn (ctx: *anyopaque) bool,
        degraded: *const fn (ctx: *anyopaque) bool,
        read: *const fn (ctx: *anyopaque, off: u64, dest: []u8, registered: bool) ReadError!void,
        /// Tear the session down (off the read path: the caller spawns or
        /// defers it; here it is the retirement hook).
        retire: *const fn (ctx: *anyopaque) void,
    };
};

pub const Verdict = enum { lane, kernel };

/// Per-device loud-fallback rate limit: one WARN per second per volume.
const FALLBACK_LOG_PERIOD_S: u64 = 1;

pub const Funnel = struct {
    lanes: [MAX_LANES]std.atomic.Value(?*const Lane) = @splat(.init(null)),
    last_fallback_log_s: [MAX_LANES]std.atomic.Value(u64) = @splat(.init(0)),

    /// Publish a lane for a volume ordinal (release pairs with `dispatch`'s acquire).
    pub fn publish(self: *Funnel, ordinal: u16, lane: *const Lane) void {
        self.lanes[ordinal].store(lane, .release);
    }

    /// Retire a volume's lane; returns it for the caller to tear down.
    pub fn retire(self: *Funnel, ordinal: u16) ?*const Lane {
        return self.lanes[ordinal].swap(null, .acq_rel);
    }

    pub fn present(self: *const Funnel, ordinal: u16) bool {
        return self.lanes[ordinal].load(.acquire) != null;
    }

    /// THE ladder.
    pub fn dispatch(self: *Funnel, ordinal: u16, off: u64, dest: []u8, registered: bool) Verdict {
        const lane = self.lanes[ordinal].load(.acquire) orelse {
            stats.zcrx_kernel_fills.inc();
            return .kernel;
        };
        if (lane.vtable.poisoned(lane.ctx) or lane.vtable.structural(lane.ctx)) {
            if (self.retire(ordinal)) |l| l.vtable.retire(l.ctx);
            stats.zcrx_lane_refused.inc();
            return .kernel;
        }
        if (lane.vtable.degraded(lane.ctx)) {
            stats.zcrx_degraded_bypasses.inc();
            stats.zcrx_lane_refused.inc();
            return .kernel;
        }
        lane.vtable.read(lane.ctx, off, dest, registered) catch |e| {
            switch (e) {
                error.Ineligible => {
                    stats.zcrx_kernel_fills.inc();
                    return .kernel;
                },
                error.WouldBlock => {},
                error.Degraded => stats.zcrx_degraded_bypasses.inc(),
                error.Poisoned => {
                    if (self.retire(ordinal)) |l| l.vtable.retire(l.ctx);
                },
                error.OpStatus, error.OpFailed => {
                    stats.zcrx_fill_fallbacks.inc();
                    const now = clock.realtimeCoarseS();
                    if (self.last_fallback_log_s[ordinal].swap(now, .monotonic) + FALLBACK_LOG_PERIOD_S <= now) {
                        log.warn("zcrx lane: volume {d}: the lane failed a read at {d}+{d} ({s}); the kernel path retries into the same destination", .{ ordinal, off, dest.len, @errorName(e) });
                    }
                },
            }
            stats.zcrx_lane_refused.inc();
            return .kernel;
        };
        return .lane;
    }
};

/// The `Volumes` decorator: the lane first, the inner (kernel-path) seam
/// otherwise. `inner` is the data plane's own volume binding; `ordinal_of`
/// maps a volume tag to its ordinal (null = no such volume ⇒ inner decides).
pub const Decorated = struct {
    inner: read_path.Volumes,
    funnel: *Funnel,
    ordinal_of: *const fn (ctx: *anyopaque, vol_tag: u64) ?u16,

    pub fn volumes(self: *Decorated) read_path.Volumes {
        return .{ .ctx = self, .allocator_of = &allocatorOf, .read_range = &readRange, .device_fd = &deviceFd };
    }

    fn allocatorOf(ctx: *anyopaque, vol_tag: u64) ?*@import("../../data/block_alloc.zig").BlockAllocator {
        const self: *Decorated = @ptrCast(@alignCast(ctx));
        return self.inner.allocator_of(self.inner.ctx, vol_tag);
    }

    fn deviceFd(ctx: *anyopaque, vol_tag: u64) ?std.os.linux.fd_t {
        const self: *Decorated = @ptrCast(@alignCast(ctx));
        return self.inner.device_fd(self.inner.ctx, vol_tag);
    }

    fn readRange(ctx: *anyopaque, vol_tag: u64, device_off: u64, dest: []u8, shape: read_path.DestShape) read_path.DeviceError!read_path.Served {
        const self: *Decorated = @ptrCast(@alignCast(ctx));
        if (self.ordinal_of(self.inner.ctx, vol_tag)) |ord| {
            if (self.funnel.dispatch(ord, device_off, dest, shape == .registered) == .lane) return .lane;
        }
        return self.inner.read_range(self.inner.ctx, vol_tag, device_off, dest, shape);
    }
};

// ---------------------------------------------------------------------------
// Tests: the ladder over a lane double.
// ---------------------------------------------------------------------------

const testing = std.testing;

const Double = struct {
    poisoned: bool = false,
    structural: bool = false,
    degraded: bool = false,
    answer: ReadError!void = {},
    reads: u32 = 0,
    retired: u32 = 0,
    last_registered: bool = false,

    fn lane(self: *Double) Lane {
        return .{ .ctx = self, .vtable = &vt };
    }
    const vt: Lane.VTable = .{ .poisoned = &p, .structural = &s, .degraded = &d, .read = &r, .retire = &t };
    fn me(ctx: *anyopaque) *Double {
        return @ptrCast(@alignCast(ctx));
    }
    fn p(ctx: *anyopaque) bool {
        return me(ctx).poisoned;
    }
    fn s(ctx: *anyopaque) bool {
        return me(ctx).structural;
    }
    fn d(ctx: *anyopaque) bool {
        return me(ctx).degraded;
    }
    fn r(ctx: *anyopaque, _: u64, dest: []u8, registered: bool) ReadError!void {
        const self = me(ctx);
        self.reads += 1;
        self.last_registered = registered;
        try self.answer;
        @memset(dest, 0xEE);
    }
    fn t(ctx: *anyopaque) void {
        me(ctx).retired += 1;
    }
};

const Counts = struct {
    fills: u64,
    kernel: u64,
    refused: u64,
    degraded: u64,
    fallbacks: u64,

    fn now() Counts {
        return .{ .fills = stats.zcrx_fills.load(), .kernel = stats.zcrx_kernel_fills.load(), .refused = stats.zcrx_lane_refused.load(), .degraded = stats.zcrx_degraded_bypasses.load(), .fallbacks = stats.zcrx_fill_fallbacks.load() };
    }
};

test "the dispatch ladder: no lane ⇒ kernel (counted); a lane serves ⇒ lane (the session counts fills); ineligible ⇒ kernel fill; degraded/decline/poison/op-error ⇒ refused by reason; the three outcomes are mutually exclusive and exhaustive" {
    var f: Funnel = .{};
    var dbl: Double = .{};
    const l = dbl.lane();
    var dest: [4096]u8 = @splat(0);
    var c0 = Counts.now();
    // No lane.
    try testing.expectEqual(Verdict.kernel, f.dispatch(3, 0, &dest, false));
    var c1 = Counts.now();
    try testing.expectEqual(c0.kernel + 1, c1.kernel);
    try testing.expectEqual(c0.refused, c1.refused);
    // A lane serving.
    f.publish(3, &l);
    try testing.expect(f.present(3));
    c0 = Counts.now();
    try testing.expectEqual(Verdict.lane, f.dispatch(3, 0, &dest, true));
    try testing.expectEqual(@as(u8, 0xEE), dest[0]);
    try testing.expect(dbl.last_registered);
    c1 = Counts.now();
    try testing.expectEqual(c0.kernel, c1.kernel);
    try testing.expectEqual(c0.refused, c1.refused);
    // Ineligible: the kernel path is the serve, uncounted as a refusal.
    dbl.answer = error.Ineligible;
    c0 = Counts.now();
    try testing.expectEqual(Verdict.kernel, f.dispatch(3, 0, &dest, false));
    c1 = Counts.now();
    try testing.expectEqual(c0.kernel + 1, c1.kernel);
    try testing.expectEqual(c0.refused, c1.refused);
    // Degraded gate before the read; the degraded error after it.
    dbl.answer = {};
    dbl.degraded = true;
    c0 = Counts.now();
    const reads0 = dbl.reads;
    try testing.expectEqual(Verdict.kernel, f.dispatch(3, 0, &dest, false));
    try testing.expectEqual(reads0, dbl.reads);
    c1 = Counts.now();
    try testing.expectEqual(c0.refused + 1, c1.refused);
    try testing.expectEqual(c0.degraded + 1, c1.degraded);
    dbl.degraded = false;
    dbl.answer = error.Degraded;
    c0 = Counts.now();
    try testing.expectEqual(Verdict.kernel, f.dispatch(3, 0, &dest, false));
    c1 = Counts.now();
    try testing.expectEqual(c0.degraded + 1, c1.degraded);
    try testing.expectEqual(c0.refused + 1, c1.refused);
    // An admission decline: refused, not a fallback, not a bypass.
    dbl.answer = error.WouldBlock;
    c0 = Counts.now();
    try testing.expectEqual(Verdict.kernel, f.dispatch(3, 0, &dest, false));
    c1 = Counts.now();
    try testing.expectEqual(c0.refused + 1, c1.refused);
    try testing.expectEqual(c0.fallbacks, c1.fallbacks);
    try testing.expectEqual(c0.degraded, c1.degraded);
    // A per-op error: a LOUD fallback; the lane stays published.
    dbl.answer = error.OpStatus;
    c0 = Counts.now();
    try testing.expectEqual(Verdict.kernel, f.dispatch(3, 4096, &dest, false));
    c1 = Counts.now();
    try testing.expectEqual(c0.fallbacks + 1, c1.fallbacks);
    try testing.expectEqual(c0.refused + 1, c1.refused);
    try testing.expect(f.present(3));
    // A poisoned lane is retired on sight; the next call is a kernel fill.
    dbl.answer = {};
    dbl.poisoned = true;
    c0 = Counts.now();
    try testing.expectEqual(Verdict.kernel, f.dispatch(3, 0, &dest, false));
    try testing.expectEqual(@as(u32, 1), dbl.retired);
    try testing.expect(!f.present(3));
    c1 = Counts.now();
    try testing.expectEqual(c0.refused + 1, c1.refused);
    c0 = Counts.now();
    try testing.expectEqual(Verdict.kernel, f.dispatch(3, 0, &dest, false));
    c1 = Counts.now();
    try testing.expectEqual(c0.kernel + 1, c1.kernel);
    // Structural starvation retires too.
    var dbl2: Double = .{ .structural = true };
    const l2 = dbl2.lane();
    f.publish(5, &l2);
    try testing.expectEqual(Verdict.kernel, f.dispatch(5, 0, &dest, false));
    try testing.expectEqual(@as(u32, 1), dbl2.retired);
    try testing.expect(!f.present(5));
}

test "the Volumes decorator: a lane fill answers `.lane` without touching the inner seam; everything else is the inner seam's `.dma`" {
    const Inner = struct {
        calls: u32 = 0,
        fn read(ctx: *anyopaque, _: u64, _: u64, dest: []u8, _: read_path.DestShape) read_path.DeviceError!read_path.Served {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.calls += 1;
            @memset(dest, 0x11);
            return .dma;
        }
        fn alloc(_: *anyopaque, _: u64) ?*@import("../../data/block_alloc.zig").BlockAllocator {
            return null;
        }
        fn fd(_: *anyopaque, _: u64) ?std.os.linux.fd_t {
            return null;
        }
        fn ordinal(_: *anyopaque, vol_tag: u64) ?u16 {
            return if (vol_tag == 0xA) 2 else null;
        }
    };
    var inner: Inner = .{};
    var f: Funnel = .{};
    var dbl: Double = .{};
    const l = dbl.lane();
    f.publish(2, &l);
    var dec: Decorated = .{ .inner = .{ .ctx = &inner, .allocator_of = &Inner.alloc, .read_range = &Inner.read, .device_fd = &Inner.fd }, .funnel = &f, .ordinal_of = &Inner.ordinal };
    const vols = dec.volumes();
    var dest: [512]u8 = @splat(0);
    try testing.expectEqual(read_path.Served.lane, try vols.read_range(vols.ctx, 0xA, 0, &dest, .pooled));
    try testing.expectEqual(@as(u8, 0xEE), dest[0]);
    try testing.expectEqual(@as(u32, 0), inner.calls);
    // An unknown tag: the inner seam.
    try testing.expectEqual(read_path.Served.dma, try vols.read_range(vols.ctx, 0xB, 0, &dest, .registered));
    try testing.expectEqual(@as(u8, 0x11), dest[0]);
    try testing.expectEqual(@as(u32, 1), inner.calls);
    // A lane decline: the inner seam serves the same destination.
    dbl.answer = error.WouldBlock;
    try testing.expectEqual(read_path.Served.dma, try vols.read_range(vols.ctx, 0xA, 0, &dest, .pooled));
    try testing.expectEqual(@as(u32, 2), inner.calls);
    try testing.expect(vols.allocator_of(vols.ctx, 0xA) == null);
}
