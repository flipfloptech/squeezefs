//! The process allocator `main` hands to everything (charter §12 "explicit
//! allocators everywhere"; the hot-path law "allocate nothing per op" is
//! each site's rule — this is the price every remaining allocation pays).
//!
//! std's `start.zig` gives `main` a `SafeAllocator` in Debug AND in
//! ReleaseSafe (`use_safe_allocator`): page-backed buckets mapped and
//! unmapped as they fill and drain, a checksummed footer with a captured
//! stack trace written per allocation. ReleaseSafe is the product's default
//! build (efdc88b), so the daemon paid that on every allocation: on the dev
//! container's random 4 KiB O_DIRECT read (`docs/evidence/rung-10-dev-
//! perf.md` §2) `SafeAllocator.alloc` was 38 % of the FUSE worker thread's
//! cycles and the bucket `munmap` alone 7 %. Every release mode takes
//! `std.heap.smp_allocator` (per-thread size-class free lists, no
//! per-allocation metadata, the allocator std itself picks for a
//! multi-threaded ReleaseFast binary without libc); Debug keeps std's choice
//! for its leak report at exit. ReleaseSafe keeps every safety check of the
//! language — bounds, overflow, unreachable — which is what the mode is for;
//! a debugging allocator is not one of them.

const std = @import("std");
const Allocator = std.mem.Allocator;

pub fn select(mode: std.builtin.OptimizeMode, std_default: Allocator) Allocator {
    return switch (mode) {
        .debug => std_default,
        .safe, .fast, .small => std.heap.smp_allocator,
    };
}

const testing = std.testing;

test "a release product never runs on std's SafeAllocator: ReleaseSafe (the default build) and the other release modes take smp_allocator; Debug keeps std's default" {
    var safe: std.heap.SafeAllocator = .init(std.heap.page_allocator, .{});
    defer _ = safe.deinit();
    const std_default = safe.allocator();
    inline for (.{ std.builtin.OptimizeMode.safe, .fast, .small }) |mode| {
        const a = select(mode, std_default);
        try testing.expect(a.vtable == std.heap.smp_allocator.vtable);
        try testing.expect(a.vtable != std_default.vtable);
    }
    try testing.expect(select(.debug, std_default).vtable == std_default.vtable);
}

test "smp_allocator round-trips the daemon's allocation shapes from several threads (a free on another thread than the alloc's)" {
    const a = std.heap.smp_allocator;
    const N = 4;
    var bufs: [N][]u8 = undefined;
    for (&bufs, 0..) |*b, i| {
        b.* = try a.alloc(u8, 64 + i * 4096);
        @memset(b.*, @intCast(i));
    }
    var threads: [N]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, struct {
        fn run(b: []u8, v: u8) void {
            for (b) |x| std.debug.assert(x == v);
            std.heap.smp_allocator.free(b);
        }
    }.run, .{ bufs[i], @as(u8, @intCast(i)) });
    for (threads) |t| t.join();
}
