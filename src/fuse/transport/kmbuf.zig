//! Per-worker kmbuf / zero-copy RESOURCES (charter §3 "kmbuf buffer rings",
//! "FUSE zero-copy"; `docs/porting/fuse_kmbuf_zc.md` §3.2, §4.1): the ent
//! headers region (one page-granular anonymous mapping, registered as a
//! fixed buffer under kmbuf), the kernel-managed buffer ring at bgid 0 with
//! its mmapped buffer REGION, and under zero-copy the sparse fixed-buffer
//! table whose slots the kernel fills with the app's pages at delivery.
//! Linux only; the worker composes these over its own ring.
//!
//! The kernel's `io_uring_buf_ring` (head/tail/`bufs[]`) is kernel memory
//! the daemon never maps — only the buffer REGION is; the daemon never
//! touches a ring tail. Buffer `bid` lives at `region + bid × buf_size`;
//! which bid an ent holds is the CQE's word (`zc.Attachment`).

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const page_size_min = std.heap.page_size_min;

const uring128 = @import("../../io/uring128.zig");
const abi = @import("../abi.zig");
const zc = @import("zc.zig");
const counters = @import("../../core/counters.zig");
const errors = @import("../../core/errors.zig");
const log = @import("../../core/log.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("fuse/transport/kmbuf is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The ENOMEM retry on the kmbuf registration: the 2026-08-10 gate capture
/// saw page-cache churn refuse 32 × 1 MiB ONCE with ample `available`;
/// bounded so a truly exhausted box refuses within ~1 s (50 + 100 + 200 +
/// 400 + 400 ms); the SAME registration each attempt — never a downgrade;
/// every other errno refuses at once.
pub const REGISTER_RETRIES: u8 = 5;
pub const REGISTER_RETRY_BASE_NS: u64 = 50 * std.time.ns_per_ms;
pub const REGISTER_RETRY_MAX_NS: u64 = 400 * std.time.ns_per_ms;

/// `transport_kmbuf_pinned_bytes`: kernel page memory the kmbuf rings
/// hold (`ring_entries × buf_size` per queue) — the payload arena of a
/// kmbuf session, a non-sheddable budget component.
pub var pinned_bytes: counters.Gauge = .{};
/// `transport_kmbuf_register_retries`: ENOMEM retries taken.
pub var register_retries: counters.ShardedCounter = .{};

/// `KmbufRingEnomem` is the typed refusal of a ring registration the
/// kernel answers ENOMEM after every retry. NOT a high-order shortage:
/// the region allocator (kmbuf patch 0003, `io_region_allocate_pages_
/// multi_buf`) tries one compound block per buffer and falls back to
/// order-0 pages — but takes those with `alloc_pages_bulk_node`, which
/// draws on the free lists and does not reclaim, all or nothing per
/// region; without `CAP_IPC_LOCK` the region is also charged to
/// `RLIMIT_MEMLOCK` (`__io_account_mem`). The probe decided the mode, so
/// the law is fail loud, never degrade (charter §3); the refusal names
/// the region's pages and the levers that shrink it (`regionPages`).
pub const Error = error{ HeadersMapFailed, BufferTableFailed, KmbufRegisterFailed, KmbufRingEnomem, RegionMapFailed };

/// The pages one queue's kmbuf region takes at once: `ring_entries ×
/// buf_size / page` (36 × 1 MiB on 4 KiB pages = 9,216).
pub fn regionPages(ring_entries: u32, buf_size: u32, page: usize) u64 {
    return @as(u64, ring_entries) * buf_size / page;
}

/// The 288 B ent headers, back to back, page-granular (what the kernel
/// addresses at `fixed_buf_id × 288` under kmbuf; what the two REGISTER
/// iovecs point at under UserEnts).
pub const Headers = struct {
    span: []align(page_size_min) u8,
    depth: u32,

    pub fn init(depth: u32, page: usize) Error!Headers {
        const len = zc.headersSpan(depth, page);
        const rc = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
        if (linux.errno(rc) != .SUCCESS) {
            log.err("transport: mmap of the {d}-byte headers region failed ({t})", .{ len, linux.errno(rc) });
            return error.HeadersMapFailed;
        }
        const ptr: [*]align(page_size_min) u8 = @ptrFromInt(rc);
        return .{ .span = ptr[0..len], .depth = depth };
    }

    pub fn deinit(self: *Headers) void {
        _ = linux.munmap(self.span.ptr, self.span.len);
        self.* = undefined;
    }

    pub fn at(self: *Headers, ent: u32) *abi.fuse_uring_req_header {
        std.debug.assert(ent < self.depth);
        return @ptrCast(@alignCast(self.span.ptr + @as(usize, ent) * zc.HEADER_STRIDE));
    }

    pub fn iovec(self: *Headers) std.posix.iovec {
        return .{ .base = self.span.ptr, .len = @as(usize, self.depth) * zc.HEADER_STRIDE };
    }
};

/// The kernel-managed buffer ring and, under zero-copy, the sparse table.
pub const Kmbuf = struct {
    opcodes: abi.KmbufRegisterOpcodes,
    zero_copy: bool,
    depth: u32,
    /// Page-aligned payload stride: the kernel refuses a `buf_size` that is
    /// not page-aligned (`PAGE_ALIGNED`, patch 0003).
    buf_size: u32,
    ring_entries: u32,
    region: []align(page_size_min) u8,
    headers_index: u16,

    /// Setup order (porting spec §14.2): the fixed-buffer table (sparse
    /// `depth` slots under zc, the headers at `headersIndex`) → the kmbuf
    /// ring (ENOMEM retry through `io`; `test_enomem` is the
    /// `SQUEEZEFS_TEST_KMBUF_REGISTER_ENOMEM` seam: every attempt answers
    /// ENOMEM without reaching the kernel) → the region mmap.
    pub fn setup(ring: *uring128.Ring128, io: Io, headers: *Headers, opcodes: abi.KmbufRegisterOpcodes, zero_copy: bool, payload_sz: u32, page: usize, test_enomem: bool) Error!Kmbuf {
        const depth = headers.depth;
        const buf_size: u32 = @intCast(std.mem.alignForward(usize, payload_sz, page));
        const headers_index = zc.headersIndex(zero_copy, depth);
        ring.registerBuffersSparse(@as(u32, headers_index) + 1) catch |e| {
            log.err("transport: the fixed-buffer table ({d} slots) refused ({s})", .{ @as(u32, headers_index) + 1, @errorName(e) });
            return error.BufferTableFailed;
        };
        errdefer ring.unregisterBuffers() catch {};
        ring.registerBuffersUpdate(headers_index, &.{headers.iovec()}) catch |e| {
            log.err("transport: registering the headers buffer at index {d} refused ({s}) — RLIMIT_MEMLOCK without CAP_IPC_LOCK is the usual cause", .{ headers_index, @errorName(e) });
            return error.BufferTableFailed;
        };
        const ring_entries = zc.ringEntriesFor(depth);
        const reg: uring128.BufReg = .{ .ring_addr = buf_size, .ring_entries = ring_entries, .bgid = abi.FUSE_URING_RINGBUF_GROUP, .flags = 0, .resv = @splat(0) };
        var attempt: u8 = 0;
        var wait_ns = REGISTER_RETRY_BASE_NS;
        while (true) {
            const registered: errors.Error!void = if (test_enomem) error.OutOfMemory else ring.registerBufRing(opcodes.register, reg);
            registered catch |e| {
                if (e == error.OutOfMemory and attempt < REGISTER_RETRIES) {
                    attempt += 1;
                    register_retries.inc();
                    log.warn("transport: kmbuf ring (opcode {d}, bgid 0, buf_size {d}, entries {d}) refused ENOMEM — retry {d}/{d} in {d} ms (page-cache churn; the limit binds the process's cumulative pins)", .{ opcodes.register, buf_size, ring_entries, attempt, REGISTER_RETRIES, wait_ns / std.time.ns_per_ms });
                    io.sleep(.fromNanoseconds(@intCast(wait_ns)), .awake) catch {};
                    wait_ns = @min(wait_ns * 2, REGISTER_RETRY_MAX_NS);
                    continue;
                }
                if (e == error.OutOfMemory) {
                    log.err("transport: kmbuf ring registration ENOMEM ({d} pages at once: {d} buffers × {d} B at bgid 0, opcode {d}) after {d} retries — the kernel takes order-0 pages when no {d}-byte block is free, from the free lists only (its bulk allocation does not reclaim), and charges the region to RLIMIT_MEMLOCK without CAP_IPC_LOCK; the probe-decided buffer mode fails loud, never degrades (charter §3)", .{ regionPages(ring_entries, buf_size, page), ring_entries, buf_size, opcodes.register, attempt, buf_size });
                    return error.KmbufRingEnomem;
                }
                log.err("transport: kmbuf ring registration refused ({s}): opcode {d}, bgid 0, buf_size {d}, entries {d}", .{ @errorName(e), opcodes.register, buf_size, ring_entries });
                return error.KmbufRegisterFailed;
            };
            break;
        }
        const span: usize = @as(usize, ring_entries) * buf_size;
        const region = ring.mmapRegion(abi.IORING_OFF_KMBUF_RING | (@as(u64, abi.FUSE_URING_RINGBUF_GROUP) << abi.IORING_OFF_KMBUF_SHIFT), span) catch |e| {
            log.err("transport: mmap of the {d}-byte kmbuf region failed ({s})", .{ span, @errorName(e) });
            return error.RegionMapFailed;
        };
        pinned_bytes.add(span);
        return .{ .opcodes = opcodes, .zero_copy = zero_copy, .depth = depth, .buf_size = buf_size, .ring_entries = ring_entries, .region = region, .headers_index = headers_index };
    }

    /// Teardown: unmap the region, drop the fixed-buffer table and ask the
    /// kernel to release the kmbuf ring — only once no handler can still
    /// read a request's payload in the region (`Group.deinit`, after the
    /// session awaited every handler). The FUSE connection pins the ring
    /// for its life and a single-issuer ring refuses another thread's
    /// register, so either unregister may be refused — expected and quiet:
    /// the pin releases with the ring fd regardless.
    pub fn teardown(self: *Kmbuf, ring: *uring128.Ring128) void {
        uring128.Ring128.munmapRegion(self.region);
        pinned_bytes.sub(@as(u64, self.ring_entries) * self.buf_size);
        ring.unregisterBuffers() catch {};
        const reg: uring128.BufReg = .{ .ring_addr = 0, .ring_entries = 0, .bgid = abi.FUSE_URING_RINGBUF_GROUP, .flags = 0, .resv = @splat(0) };
        ring.registerBufRing(self.opcodes.unregister, reg) catch |e| log.debug("transport: kmbuf ring unregister (opcode {d}) answered {s} — the FUSE pin releases with the ring fd", .{ self.opcodes.unregister, @errorName(e) });
        self.* = undefined;
    }

    /// Buffer `bid`'s window (`payload_len` bytes), empty past the ring.
    pub fn buffer(self: *Kmbuf, bid: u32, payload_len: usize) []u8 {
        if (bid >= self.ring_entries) return &.{};
        return self.region[@as(usize, bid) * self.buf_size ..][0..payload_len];
    }

    pub fn pinnedBytes(self: *const Kmbuf) u64 {
        return @as(u64, self.ring_entries) * self.buf_size;
    }
};

// ---------------------------------------------------------------------------
// Tests (venue: the sqz kernel; a stock kernel skips the kmbuf half).
// ---------------------------------------------------------------------------

const testing = std.testing;
const probes = @import("probes.zig");

test "headers: page-granular span, ent i at base + i × 288, the iovec covers depth × 288" {
    var h = try Headers.init(5, 4096);
    defer h.deinit();
    try testing.expectEqual(@as(usize, 4096), h.span.len);
    try testing.expectEqual(@intFromPtr(h.span.ptr) + 4 * 288, @intFromPtr(h.at(4)));
    try testing.expectEqual(@as(usize, 5 * 288), h.iovec().len);
    h.at(3).ring_ent_in_out.payload_sz = 77;
    try testing.expectEqual(@as(u32, 77), std.mem.readInt(u32, h.span[3 * 288 + 256 + 16 ..][0..4], .little));
}

test "kmbuf setup on this kernel: the headers register at index 0 (BufRing) or depth (zc), the ring registers at bgid 0 with a page-aligned buf_size and next_pow2(depth) entries, the region maps and bid 0 is real memory; teardown releases" {
    const page: u32 = @intCast(std.heap.pageSize());
    const verdict = probes.driver.probeKmbuf(page);
    const pair = verdict.opcodes() orelse return testkit.skip(@src(), .no_kernel_feature, "no kmbuf register opcode on this kernel");
    for ([_]bool{ false, true }) |zero_copy| {
        var ring = try uring128.Ring128.init(16, 0);
        defer ring.deinit();
        var h = try Headers.init(6, page);
        defer h.deinit();
        const before = pinned_bytes.load();
        var k = try Kmbuf.setup(&ring, testing.io, &h, pair, zero_copy, 1 << 20, page, false);
        try testing.expectEqual(@as(u32, 8), k.ring_entries);
        try testing.expectEqual(@as(u32, 1 << 20), k.buf_size);
        try testing.expectEqual(@as(u16, if (zero_copy) 6 else 0), k.headers_index);
        try testing.expectEqual(@as(usize, 8 << 20), k.region.len);
        try testing.expectEqual(before + (8 << 20), pinned_bytes.load());
        k.buffer(0, 16)[0] = 0xA5;
        try testing.expectEqual(@as(u8, 0xA5), k.region[0]);
        try testing.expectEqual(@as(usize, 0), k.buffer(8, 16).len);
        try testing.expectEqual(@intFromPtr(k.region.ptr) + (3 << 20), @intFromPtr(k.buffer(3, 4).ptr));
        // A second registration at bgid 0 is the live-ring EEXIST.
        const reg: uring128.BufReg = .{ .ring_addr = k.buf_size, .ring_entries = 8, .bgid = 0, .flags = 0, .resv = @splat(0) };
        try testing.expectError(error.PathAlreadyExists, ring.registerBufRing(pair.register, reg));
        k.teardown(&ring);
        try testing.expectEqual(before, pinned_bytes.load());
    }
}

test "regionPages: one queue's region at once — 36 × 1 MiB on 4 KiB pages is 9,216 pages, on 64 KiB pages 576" {
    try testing.expectEqual(@as(u64, 9216), regionPages(36, 1 << 20, 4096));
    try testing.expectEqual(@as(u64, 576), regionPages(36, 1 << 20, 65536));
    try testing.expectEqual(@as(u64, 32), regionPages(32, 4096, 4096));
}

test "the ENOMEM seam: every attempt answers ENOMEM — the bounded ladder retries REGISTER_RETRIES times, the refusal is the typed KmbufRingEnomem naming the remedy, nothing is pinned, the table is released" {
    const page: u32 = @intCast(std.heap.pageSize());
    const verdict = probes.driver.probeKmbuf(page);
    const pair = verdict.opcodes() orelse return testkit.skip(@src(), .no_kernel_feature, "no kmbuf register opcode on this kernel");
    var ring = try uring128.Ring128.init(16, 0);
    defer ring.deinit();
    var h = try Headers.init(6, page);
    defer h.deinit();
    const before = pinned_bytes.load();
    const retries_before = register_retries.load();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.KmbufRingEnomem, Kmbuf.setup(&ring, testing.io, &h, pair, true, 1 << 20, page, true));
    try log.testing.expectAllFired();
    try testing.expectEqual(retries_before + REGISTER_RETRIES, register_retries.load());
    try testing.expectEqual(before, pinned_bytes.load());
    // The fixed-buffer table was released: a fresh sparse table registers.
    try ring.registerBuffersSparse(1);
}
