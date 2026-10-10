//! `squeezefs bench` (charter §8, §11 the measurement law; `docs/porting/
//! stats_cli.md` §3): the load generator measuring the MOUNTED filesystem
//! through the kernel, exactly like elbencho — ordinary POSIX file I/O on
//! a persistent dataset `<mountpoint>/squeezefs-bench/t{tid}/f{fid}.bin`;
//! never the backend, never the daemon's own I/O layer (the io_uring
//! mandate governs the filesystem's data paths, not this client).
//!
//! The shape vocabulary is ONE: `threads × files × size @ block`,
//! sequential or `--rand` (a shuffled FULL-COVERAGE permutation — every
//! block exactly once), optional O_DIRECT. The bare invocation runs the
//! fixed saturation suite: write seq 1 MiB direct → read seq 1 MiB direct →
//! read rand 4 KiB direct (boxed) → write rand 4 KiB direct (boxed) → stat
//! → del (the del pass leaves the mount clean). Writes are a deterministic
//! non-zero xorshift64* stream seeded per `(tid, fid, block)` so
//! compression cannot fake a row; the write phase ends with `ftruncate +
//! fsync` INSIDE the timed span — honest durable numbers.
//!
//! What the measurement law adds over the reference's rows (§3.4, §7
//! Q13/Q14): every row names `instrument / substrate / venue / profile`;
//! latencies live in a `counters.LatencyHistogram` per worker (26 log2
//! buckets + exact count/sum — never a per-op vector), quantiles reported
//! as the bucket's UPPER bound; a write row carries `device bytes ÷ user
//! bytes`, `wareq-sz` beside the block size, the reclaim ledger and the
//! admission ledger from the `.stats` delta; the must-stay-0 census rides
//! every row; a row under 60 s says `scoping-only`; `--abba` brackets the
//! one lever the client controls (O_DIRECT vs buffered) A-B-B-A; no
//! colours, no thresholds that pretend to judge.
//!
//! `--smoke` (the gate's `zig build bench-smoke`): one iteration of every
//! workload on a 2 × 1 × 4 MiB shape — ≤ 16 MiB of real data, cleaned.
//!
//! The planning, sizing, ordering, fill and row arithmetic are pure and
//! tested on every host; the engine is Linux only.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");
const is_linux = builtin.os.tag == .linux;
const Io = std.Io;
const Allocator = std.mem.Allocator;
const cli = @import("cli.zig");
const args = @import("args.zig");
const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const json_scan = @import("../core/json_scan.zig");

// ---------------------------------------------------------------------------
// Constants (§5.3) with their WHY.
// ---------------------------------------------------------------------------

/// The auto shape: ≤ 16 threads (the saturation suite's measured knee on
/// the 32-core reference box — more threads only add scheduler noise to a
/// client that is not the bottleneck); ≥ 16 GiB in total so a dataset
/// outruns every RAM tier of the client; 2 GiB per thread keeps each
/// worker's own stream past the hot tier's per-stream window; the quarter-
/// of-free cap keeps the dataset from filling the set; below 4 GiB a row
/// measures caches, not the filesystem — refused.
pub const AUTO_THREADS_CAP: u32 = 16;
pub const AUTO_TOTAL_FLOOR: u64 = 16 << 30;
pub const AUTO_PER_THREAD: u64 = 2 << 30;
pub const AUTO_MIN_TOTAL: u64 = 4 << 30;
/// Sizes round down to 1 MiB so `size % block == 0` for BOTH suite blocks
/// (4 KiB divides 1 MiB).
pub const SIZE_ROUND: u64 = 1 << 20;
pub const SUITE_SEQ_BLOCK: u64 = 1 << 20;
pub const SUITE_RAND_BLOCK: u64 = 4096;
/// A random-access pass is time-boxed (full coverage of a 16 GiB dataset
/// at 4 KiB is 4 M ops — a scoping row needs seconds, not the whole
/// permutation); a sequential pass runs to coverage.
pub const RAND_TIME_BOX_S: u32 = 30;
/// The measurement law's headline floor (charter §11): a shorter data row
/// is scoping only and says so.
pub const HEADLINE_SECS: u64 = 60;
/// `O_DIRECT` needs page-aligned blocks and no partial tail block.
pub const DIRECT_ALIGN: u64 = 4096;
/// The seed tag "SQFSBENC": reproducible streams across runs; xorshift so
/// compression cannot fake rows.
pub const SEED_TAG: u64 = 0x5351_4653_4245_4E43;
pub const DATASET_DIR = "squeezefs-bench";
/// The smoke shape: two workers, one 4 MiB file each — every workload
/// runs, ≤ 16 MiB touches the device, under a second on the venue.
pub const SMOKE_THREADS: u32 = 2;
pub const SMOKE_FILE_SIZE: u64 = 4 << 20;

// ---------------------------------------------------------------------------
// Shapes.
// ---------------------------------------------------------------------------

pub const Shape = struct {
    threads: u32,
    files: u32,
    size: u64,
    block: u64,

    pub fn totalBytes(self: Shape) u64 {
        return @as(u64, self.threads) * self.files * self.size;
    }

    pub fn blocksPerFile(self: Shape) u64 {
        return blockCount(self.size, self.block);
    }

    /// A pass covers the file at ITS block (the suite's rand passes run at
    /// 4 KiB over a 1 MiB-block shape): full coverage is `size / pass.block`
    /// blocks per file, never the shape's block count.
    pub fn expectedOps(self: Shape, pass: Pass) u64 {
        return switch (pass.phase) {
            .write, .read => @as(u64, self.threads) * self.files * blockCount(self.size, pass.block),
            .stat, .del => @as(u64, self.threads) * self.files,
        };
    }
};

pub const ShapeError = error{ ZeroThreads, ZeroFiles, ZeroSize, ZeroBlock, DirectBlockUnaligned, DirectSizeNotBlockMultiple, FilesystemTooSmall };

pub fn blockCount(size: u64, block: u64) u64 {
    return (size + block - 1) / block;
}

/// `validate_shape`: every term ≥ 1; `--direct` needs `block % 4096 == 0`
/// and `size % block == 0` (O_DIRECT cannot write a partial EOF tail).
pub fn validateShape(s: Shape, direct: bool) ShapeError!void {
    if (s.threads == 0) return error.ZeroThreads;
    if (s.files == 0) return error.ZeroFiles;
    if (s.size == 0) return error.ZeroSize;
    if (s.block == 0) return error.ZeroBlock;
    if (direct) {
        if (s.block % DIRECT_ALIGN != 0) return error.DirectBlockUnaligned;
        if (s.size % s.block != 0) return error.DirectSizeNotBlockMultiple;
    }
}

pub const Explicit = struct { threads: ?u32 = null, files: ?u32 = null, size: ?u64 = null };

/// The auto-derived total: `max(16 GiB, 2 GiB × threads)` capped at a
/// quarter of the free space (the existing dataset counts as free — the
/// bench reuses it), rounded down to 1 MiB, refused below 4 GiB.
pub fn autoTotal(threads: u32, free_bytes: u64, dataset_bytes: u64) ShapeError!u64 {
    const want = @max(AUTO_TOTAL_FLOOR, AUTO_PER_THREAD * threads);
    const cap = (free_bytes +| dataset_bytes) / 4;
    const total = (@min(want, cap) / SIZE_ROUND) * SIZE_ROUND;
    if (total < AUTO_MIN_TOTAL) return error.FilesystemTooSmall;
    return total;
}

pub fn autoThreads(cpus: u32) u32 {
    return @intCast(derive.clamp(cpus, 1, AUTO_THREADS_CAP));
}

/// `resolve_shape`: explicit flags always override; `size` derives from
/// the total over `threads × files`, rounded down to 1 MiB.
pub fn resolveShape(cpus: u32, free_bytes: u64, dataset_bytes: u64, block: u64, e: Explicit) ShapeError!Shape {
    const threads = e.threads orelse autoThreads(cpus);
    const files = e.files orelse 1;
    const size = e.size orelse blk: {
        const total = try autoTotal(threads, free_bytes, dataset_bytes);
        const per = total / (@as(u64, threads) * files);
        break :blk @max(SIZE_ROUND, (per / SIZE_ROUND) * SIZE_ROUND);
    };
    return .{ .threads = threads, .files = files, .size = size, .block = block };
}

pub fn smokeShape(block: u64) Shape {
    return .{ .threads = SMOKE_THREADS, .files = 1, .size = SMOKE_FILE_SIZE, .block = block };
}

// ---------------------------------------------------------------------------
// Passes.
// ---------------------------------------------------------------------------

pub const Phase = enum { write, read, stat, del };

pub const Pass = struct {
    phase: Phase,
    block: u64,
    rand: bool,
    direct: bool,
    /// Seconds; 0 = full coverage.
    time_box_s: u32,
    /// The bracket arm label (`--abba`), empty otherwise.
    arm: []const u8 = "",

    pub fn moveBytes(self: Pass) bool {
        return self.phase == .write or self.phase == .read;
    }
};

/// The time box: `--time` wins (0 = full coverage); a random pass defaults
/// to 30 s; a sequential pass runs to coverage.
pub fn timeBox(rand: bool, time: ?u32) u32 {
    if (time) |t| return t;
    return if (rand) RAND_TIME_BOX_S else 0;
}

/// The saturation suite in its fixed order.
pub fn suitePasses(time: ?u32) [6]Pass {
    return .{
        .{ .phase = .write, .block = SUITE_SEQ_BLOCK, .rand = false, .direct = true, .time_box_s = timeBox(false, time) },
        .{ .phase = .read, .block = SUITE_SEQ_BLOCK, .rand = false, .direct = true, .time_box_s = timeBox(false, time) },
        .{ .phase = .read, .block = SUITE_RAND_BLOCK, .rand = true, .direct = true, .time_box_s = timeBox(true, time) },
        .{ .phase = .write, .block = SUITE_RAND_BLOCK, .rand = true, .direct = true, .time_box_s = timeBox(true, time) },
        .{ .phase = .stat, .block = SUITE_SEQ_BLOCK, .rand = false, .direct = false, .time_box_s = 0 },
        .{ .phase = .del, .block = SUITE_SEQ_BLOCK, .rand = false, .direct = false, .time_box_s = 0 },
    };
}

/// The explicit phases in the fixed order Write → Read → Stat → Del.
pub fn phasePasses(b: args.Bench, block: u64, out: *[4]Pass) []Pass {
    var n: usize = 0;
    if (b.write) {
        out[n] = .{ .phase = .write, .block = block, .rand = b.rand, .direct = b.direct, .time_box_s = timeBox(b.rand, b.time) };
        n += 1;
    }
    if (b.read) {
        out[n] = .{ .phase = .read, .block = block, .rand = b.rand, .direct = b.direct, .time_box_s = timeBox(b.rand, b.time) };
        n += 1;
    }
    if (b.stat) {
        out[n] = .{ .phase = .stat, .block = block, .rand = false, .direct = false, .time_box_s = 0 };
        n += 1;
    }
    if (b.del) {
        out[n] = .{ .phase = .del, .block = block, .rand = false, .direct = false, .time_box_s = 0 };
        n += 1;
    }
    return out[0..n];
}

// ---------------------------------------------------------------------------
// The streams: block order and the fill.
// ---------------------------------------------------------------------------

/// splitmix64 (Steele, Lea & Flood 2014): the published constants and
/// shifts verbatim — a seed mixer whose outputs are equidistributed, so a
/// per-(tid, fid, block) seed never collides across the dataset.
fn splitmix64(x: u64) u64 {
    var z = x +% 0x9E37_79B9_7F4A_7C15;
    z = (z ^ (z >> 30)) *% 0xBF58_476D_1CE4_E5B9;
    z = (z ^ (z >> 27)) *% 0x94D0_49BB_1331_11EB;
    return z ^ (z >> 31);
}

/// One xorshift64* step (Vigna 2016): the published 12/25/27 shift triple
/// and the multiplier — a 2^64 − 1 period stream from any non-zero state.
fn xorshift64star(x: *u64) u64 {
    x.* ^= x.* >> 12;
    x.* ^= x.* << 25;
    x.* ^= x.* >> 27;
    return x.* *% 0x2545_F491_4F6C_DD1D;
}

/// The stream seed of `(tid, fid, block)` under the tag.
pub fn blockSeed(tid: u32, fid: u32, block: u64) u64 {
    return splitmix64(SEED_TAG ^ splitmix64((@as(u64, tid) << 32) | fid) ^ splitmix64(block +% 1));
}

/// xorshift64* from a non-zero seed: every 8-byte word non-zero.
pub fn fillBlock(buf: []u8, tid: u32, fid: u32, block: u64) void {
    var x = blockSeed(tid, fid, block);
    if (x == 0) x = SEED_TAG;
    var i: usize = 0;
    while (i + 8 <= buf.len) : (i += 8) {
        const word = xorshift64star(&x);
        std.mem.writeInt(u64, buf[i..][0..8], if (word == 0) 1 else word, .little);
    }
    // The tail past the last whole word: a fixed non-zero fill byte.
    while (i < buf.len) : (i += 1) buf[i] = 0xA5;
}

/// `out[k]` = the k-th block to touch: identity, or a Fisher–Yates FULL-
/// COVERAGE permutation (every block exactly once) from `seed`.
pub fn blockOrder(out: []u64, rand: bool, seed: u64) void {
    for (out, 0..) |*o, i| o.* = i;
    if (!rand or out.len < 2) return;
    var x = splitmix64(seed) | 1;
    var i: usize = out.len - 1;
    while (i > 0) : (i -= 1) {
        const j: usize = @intCast(xorshift64star(&x) % (i + 1));
        std.mem.swap(u64, &out[i], &out[j]);
    }
}

// ---------------------------------------------------------------------------
// Rows.
// ---------------------------------------------------------------------------

/// The bucket's upper bound in ns for a quantile over the exact counts —
/// never a midpoint (§7 Q13); the last bucket reports its lower bound
/// (`>16s`).
pub fn quantileNs(s: counters.LatencyHistogram.Snapshot, q_per_mille: u64) u64 {
    if (s.count == 0) return 0;
    const rank = (s.count * q_per_mille + 999) / 1000;
    var seen: u64 = 0;
    for (s.buckets, 0..) |n, i| {
        seen += n;
        if (seen >= rank) return bucketUpperNs(i);
    }
    return bucketUpperNs(counters.hist_buckets - 1);
}

/// Bucket i covers `(2^(i−1), 2^i]` µs (bucket 0 is `≤ 1 µs`).
pub fn bucketUpperNs(i: usize) u64 {
    return (@as(u64, 1) << @intCast(i)) * std.time.ns_per_us;
}

/// The `.stats` terms a row reads (by key name) before and after a pass.
pub const StatsTerms = struct {
    fuse_ops: u64 = 0,
    transport_ring_requests: u64 = 0,
    transport_enters: u64 = 0,
    /// Σ device write bytes over every write arm.
    device_write_bytes: u64 = 0,
    device_writes: u64 = 0,
    admission_bytes: u64 = 0,
    terminal_frees: u64 = 0,
    reclaim_queued: u64 = 0,
    reclaim_elided: u64 = 0,
    discard_bytes: u64 = 0,
    held_stores: u64 = 0,
    patch_writes: u64 = 0,
    hot_block_hits: u64 = 0,
    fast_dispatch_serves: u64 = 0,
    zc_write_direct_bytes: u64 = 0,
    zc_write_extract_bytes: u64 = 0,
    invariant_tripwires: u64 = 0,
    direct_device_true: bool = false,
    present: bool = false,

    /// The device-write byte arms (`stats_cli.md` §3.4): every path that
    /// lands user bytes on a data device.
    const device_write_keys = [_][]const u8{ "overlay_store_bytes", "patch_write_bytes", "own_block_dma_bytes", "rewrite_device_write_bytes", "durable_upload_bytes_escalation", "durable_upload_bytes_writeback" };
    const admission_keys = [_][]const u8{ "admit_write_overlay_bytes", "admit_write_inplace_bytes", "admit_write_accumulate_bytes", "admit_write_inline_bytes" };

    pub fn parse(text: []const u8) StatsTerms {
        var t: StatsTerms = .{ .present = true };
        t.fuse_ops = json_scan.number(text, "fuse_ops") orelse 0;
        t.transport_ring_requests = json_scan.number(text, "transport_ring_requests") orelse 0;
        t.transport_enters = json_scan.number(text, "transport_enters") orelse 0;
        for (device_write_keys) |k| t.device_write_bytes += json_scan.number(text, k) orelse 0;
        t.device_writes = json_scan.number(text, "put_obj") orelse 0;
        for (admission_keys) |k| t.admission_bytes += json_scan.number(text, k) orelse 0;
        t.terminal_frees = json_scan.number(text, "terminal_frees") orelse 0;
        t.reclaim_queued = json_scan.number(text, "block_free_reclaim_queued") orelse 0;
        t.reclaim_elided = json_scan.number(text, "block_free_reclaim_elided") orelse 0;
        t.discard_bytes = json_scan.number(text, "block_free_discard_bytes") orelse 0;
        t.held_stores = json_scan.number(text, "held_stores") orelse 0;
        t.patch_writes = json_scan.number(text, "patch_writes") orelse 0;
        t.hot_block_hits = json_scan.number(text, "hot_block_hits") orelse 0;
        t.fast_dispatch_serves = json_scan.number(text, "transport_fast_dispatch_serves") orelse 0;
        t.zc_write_direct_bytes = json_scan.number(text, "transport_zc_write_direct_bytes") orelse 0;
        t.zc_write_extract_bytes = json_scan.number(text, "transport_zc_write_extract_bytes") orelse 0;
        t.invariant_tripwires = json_scan.number(text, "invariant_tripwires") orelse 0;
        t.direct_device_true = (json_scan.number(text, "direct_device_true") orelse 0) != 0;
        return t;
    }

    pub fn delta(after: StatsTerms, before: StatsTerms) StatsTerms {
        var d: StatsTerms = .{ .present = after.present and before.present, .direct_device_true = after.direct_device_true };
        inline for (.{ "fuse_ops", "transport_ring_requests", "transport_enters", "device_write_bytes", "device_writes", "admission_bytes", "terminal_frees", "reclaim_queued", "reclaim_elided", "discard_bytes", "held_stores", "patch_writes", "hot_block_hits", "fast_dispatch_serves", "zc_write_direct_bytes", "zc_write_extract_bytes", "invariant_tripwires" }) |f| {
            @field(d, f) = @field(after, f) -| @field(before, f);
        }
        return d;
    }
};

pub const Row = struct {
    pass: Pass,
    shape: Shape,
    iteration: u32,
    ops: u64,
    bytes: u64,
    elapsed_ns: u64,
    expected_ops: u64,
    boxed_out: bool,
    lat: counters.LatencyHistogram.Snapshot,
    stats: StatsTerms,

    pub fn mibPerS(self: Row) u64 {
        if (self.elapsed_ns == 0) return 0;
        return @intCast((@as(u128, self.bytes) * std.time.ns_per_s) / self.elapsed_ns >> 20);
    }

    pub fn iops(self: Row) u64 {
        if (self.elapsed_ns == 0) return 0;
        return @intCast((@as(u128, self.ops) * std.time.ns_per_s) / self.elapsed_ns);
    }

    /// `device bytes ÷ user bytes` in ppm (integer; no floats on a wire).
    pub fn amplificationPpm(self: Row) u64 {
        if (self.bytes == 0) return 0;
        return @intCast((@as(u128, self.stats.device_write_bytes) * 1_000_000) / self.bytes);
    }

    /// `wareq-sz`: the mean device write request in bytes.
    pub fn wareqSz(self: Row) u64 {
        if (self.stats.device_writes == 0) return 0;
        return self.stats.device_write_bytes / self.stats.device_writes;
    }

    pub fn scopingOnly(self: Row) bool {
        return self.pass.moveBytes() and self.elapsed_ns < HEADLINE_SECS * std.time.ns_per_s;
    }

    pub fn passName(self: Row, buf: []u8) []const u8 {
        return std.fmt.bufPrint(buf, "{s} {d} {s}{s}", .{ @tagName(self.pass.phase), self.pass.block, if (self.pass.rand) "rand" else "seq", if (self.pass.direct) " direct" else "" }) catch buf[0..0];
    }

    /// One machine-parseable line.
    pub fn writeJson(self: Row, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"pass\":\"{s}\",\"arm\":\"{s}\",\"iteration\":{d},\"block\":{d},\"access\":\"{s}\",\"io\":\"{s}\",\"threads\":{d},\"files\":{d},\"size\":{d},\"elapsed_ns\":{d},\"ops\":{d},\"expected_ops\":{d},\"boxed_out\":{},\"bytes\":{d},\"mib_per_s\":{d},\"iops\":{d},\"scoping_only\":{},\"lat_ns\":{{\"min_bucket\":{d},\"mean\":{d},\"p50\":{d},\"p99\":{d},\"p999\":{d},\"max_bucket\":{d}}}", .{
            @tagName(self.pass.phase),             self.pass.arm,                                  self.iteration,             self.pass.block,
            if (self.pass.rand) "rand" else "seq", if (self.pass.direct) "direct" else "buffered", self.shape.threads,         self.shape.files,
            self.shape.size,                       self.elapsed_ns,                                self.ops,                   self.expected_ops,
            self.boxed_out,                        self.bytes,                                     self.mibPerS(),             self.iops(),
            self.scopingOnly(),                    quantileNs(self.lat, 0),                        self.lat.meanNs(),          quantileNs(self.lat, 500),
            quantileNs(self.lat, 990),             quantileNs(self.lat, 999),                      quantileNs(self.lat, 1000),
        });
        const s = self.stats;
        try w.print(",\"stats_present\":{},\"user_bytes\":{d},\"device_write_bytes\":{d},\"device_writes\":{d},\"amplification_ppm\":{d},\"wareq_sz\":{d},\"admission_bytes\":{d},\"reclaim\":{{\"terminal_frees\":{d},\"queued\":{d},\"elided\":{d},\"discard_bytes\":{d}}},\"fuse_ops\":{d},\"transport_ring_requests\":{d},\"transport_enters\":{d},\"engagement\":{{\"held_stores\":{d},\"patch_writes\":{d},\"hot_block_hits\":{d},\"fast_dispatch_serves\":{d},\"zc_write_direct_bytes\":{d},\"zc_write_extract_bytes\":{d}}},\"invariant_tripwires\":{d}}}\n", .{
            s.present,                 self.bytes,               s.device_write_bytes,  s.device_writes,  self.amplificationPpm(), self.wareqSz(),
            s.admission_bytes,         s.terminal_frees,         s.reclaim_queued,      s.reclaim_elided, s.discard_bytes,         s.fuse_ops,
            s.transport_ring_requests, s.transport_enters,       s.held_stores,         s.patch_writes,   s.hot_block_hits,        s.fast_dispatch_serves,
            s.zc_write_direct_bytes,   s.zc_write_extract_bytes, s.invariant_tripwires,
        });
    }

    pub fn writeText(self: Row, w: *Io.Writer) Io.Writer.Error!void {
        var nb: [64]u8 = undefined;
        try w.print("| {s:<22} | {s:<3} | {d:>8} MiB/s | {d:>9} IOPS | {d:>8}/{d:<8} | lat p50 {d} µs p99 {d} µs p999 {d} µs mean {d} µs |{s}\n", .{ self.passName(&nb), self.pass.arm, self.mibPerS(), self.iops(), self.ops, self.expected_ops, quantileNs(self.lat, 500) / std.time.ns_per_us, quantileNs(self.lat, 990) / std.time.ns_per_us, quantileNs(self.lat, 999) / std.time.ns_per_us, self.lat.meanNs() / std.time.ns_per_us, if (self.scopingOnly()) " scoping-only (<60 s)" else "" });
        if (self.boxed_out) try w.print("|   time box expired: {d} of {d} ops ({d}% coverage)\n", .{ self.ops, self.expected_ops, if (self.expected_ops == 0) 0 else self.ops * 100 / self.expected_ops });
        if (self.pass.phase == .write and self.stats.present) {
            try w.print("|   device bytes ÷ user bytes {d}.{d:0>3} (device {d} B / user {d} B); wareq-sz {d} B vs block {d} B; reclaim terminal {d} = queued {d} + elided {d}, discarded {d} B; admission {d} B\n", .{ self.amplificationPpm() / 1_000_000, (self.amplificationPpm() % 1_000_000) / 1000, self.stats.device_write_bytes, self.bytes, self.wareqSz(), self.pass.block, self.stats.terminal_frees, self.stats.reclaim_queued, self.stats.reclaim_elided, self.stats.discard_bytes, self.stats.admission_bytes });
        }
        if (self.stats.present) {
            try w.print("|   fuse_ops {d}, ring requests {d}, enters {d}; engagement held_stores {d} patch_writes {d} hot_block_hits {d} fast_dispatch {d} zc direct {d} B extract {d} B; invariant_tripwires +{d}\n", .{ self.stats.fuse_ops, self.stats.transport_ring_requests, self.stats.transport_enters, self.stats.held_stores, self.stats.patch_writes, self.stats.hot_block_hits, self.stats.fast_dispatch_serves, self.stats.zc_write_direct_bytes, self.stats.zc_write_extract_bytes, self.stats.invariant_tripwires });
        }
    }
};

/// The A-B-B-A bracket over the client's one lever: A = the `--direct`
/// posture as given, B = its opposite; the order `A B B A` cancels a
/// monotone drift (an aging store) to first order.
pub const abba_order = [4][]const u8{ "A", "B", "B", "A" };

pub fn abbaDirect(arm: []const u8, direct_a: bool) bool {
    return if (std.mem.eql(u8, arm, "A")) direct_a else !direct_a;
}

/// The bracket's verdict: mean MiB/s of the A rows vs the B rows (byte-
/// moving passes), and the spread inside each pair (the drift bound).
pub const Bracket = struct {
    a_mean: u64,
    b_mean: u64,
    a_spread: u64,
    b_spread: u64,

    pub fn fold(rows: []const Row) Bracket {
        var a: [2]u64 = .{ 0, 0 };
        var b: [2]u64 = .{ 0, 0 };
        var na: usize = 0;
        var nb: usize = 0;
        for (rows) |r| {
            if (!r.pass.moveBytes()) continue;
            if (std.mem.eql(u8, r.pass.arm, "A")) {
                if (na < 2) a[na] = r.mibPerS();
                na += 1;
            } else if (std.mem.eql(u8, r.pass.arm, "B")) {
                if (nb < 2) b[nb] = r.mibPerS();
                nb += 1;
            }
        }
        return .{ .a_mean = (a[0] + a[1]) / 2, .b_mean = (b[0] + b[1]) / 2, .a_spread = @max(a[0], a[1]) - @min(a[0], a[1]), .b_spread = @max(b[0], b[1]) - @min(b[0], b[1]) };
    }
};

// ---------------------------------------------------------------------------
// The engine (Linux).
// ---------------------------------------------------------------------------

pub const engine = if (is_linux) struct {
    const linux = std.os.linux;

    const Header = struct {
        instrument: cli.Build,
        substrate: []const u8,
        kernel: []const u8,
        host: []const u8,
        posture: ?bool,
    };

    /// `/sys/block/<dev>/device/transport` of a data device (`loop` / `tcp`
    /// / `pcie`); a regular-file volume is `file`; unknown otherwise.
    fn substrateOf(gpa: Allocator, io: Io, uri_text: ?[]const u8, buf: []u8) []const u8 {
        const text = uri_text orelse return "unknown";
        // The first member's device path after `sqdata://`.
        const after = if (std.mem.indexOf(u8, text, "://")) |i| text[i + 3 ..] else text;
        const first = if (std.mem.indexOfScalar(u8, after, ',')) |c| after[0..c] else after;
        var st: linux.Statx = undefined;
        var zbuf: [linux.PATH_MAX:0]u8 = undefined;
        const z = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{first}, 0) catch return "unknown";
        if (linux.errno(linux.statx(linux.AT.FDCWD, z.ptr, 0, .{ .TYPE = true }, &st)) != .SUCCESS) return "unknown";
        if (linux.S.ISREG(st.mode)) return "file";
        const base = std.fs.path.basename(first);
        const path = std.fmt.allocPrint(gpa, "/sys/block/{s}/device/transport", .{base}) catch return "unknown";
        defer gpa.free(path);
        const t = Io.Dir.cwd().readFile(io, path, buf) catch return if (std.mem.startsWith(u8, base, "loop")) "loop" else "unknown";
        return std.mem.trim(u8, t, " \t\r\n");
    }

    fn readStats(gpa: Allocator, io: Io, mountpoint: []const u8) ?[]u8 {
        const path = std.fmt.allocPrint(gpa, "{s}/.stats", .{mountpoint}) catch return null;
        defer gpa.free(path);
        return Io.Dir.cwd().readFileAlloc(io, path, gpa, .limited(16 << 20)) catch null;
    }

    fn readConfig(gpa: Allocator, io: Io, mountpoint: []const u8) ?[]u8 {
        const path = std.fmt.allocPrint(gpa, "{s}/.config", .{mountpoint}) catch return null;
        defer gpa.free(path);
        return Io.Dir.cwd().readFileAlloc(io, path, gpa, .limited(1 << 20)) catch null;
    }

    const Worker = struct {
        tid: u32,
        shape: Shape,
        pass: Pass,
        root: []const u8,
        deadline_ns: u64,
        gpa: Allocator,
        hist: counters.LatencyHistogram = .{},
        ops: u64 = 0,
        bytes: u64 = 0,
        failed: ?linux.E = null,

        fn filePath(self: *Worker, buf: *[linux.PATH_MAX:0]u8, fid: u32) [:0]const u8 {
            return std.fmt.bufPrintSentinel(buf, "{s}/{s}/t{d}/f{d}.bin", .{ self.root, DATASET_DIR, self.tid, fid }, 0) catch unreachable;
        }

        fn boxed(self: *Worker) bool {
            return self.deadline_ns != 0 and clock.monotonicNs() >= self.deadline_ns;
        }

        fn main(self: *Worker) void {
            self.run() catch |e| {
                self.failed = switch (e) {
                    error.OutOfMemory => .NOMEM,
                    error.Io => .IO,
                };
            };
        }

        fn run(self: *Worker) error{ OutOfMemory, Io }!void {
            const nblocks = blockCount(self.shape.size, self.pass.block);
            var order: []u64 = &.{};
            var buf: []align(4096) u8 = &.{};
            if (self.pass.moveBytes()) {
                order = try self.gpa.alloc(u64, @intCast(nblocks));
                buf = try self.gpa.alignedAlloc(u8, .fromByteUnits(4096), @intCast(self.pass.block));
            }
            defer if (order.len != 0) self.gpa.free(order);
            defer if (buf.len != 0) self.gpa.free(buf);
            var fid: u32 = 0;
            while (fid < self.shape.files) : (fid += 1) {
                var pbuf: [linux.PATH_MAX:0]u8 = undefined;
                const path = self.filePath(&pbuf, fid);
                switch (self.pass.phase) {
                    .stat => {
                        const t0 = clock.monotonicNs();
                        var st: linux.Statx = undefined;
                        if (linux.errno(linux.statx(linux.AT.FDCWD, path.ptr, 0, .{ .SIZE = true }, &st)) != .SUCCESS) return error.Io;
                        self.hist.record(clock.monotonicNs() -| t0);
                        self.ops += 1;
                    },
                    .del => {
                        const t0 = clock.monotonicNs();
                        if (linux.errno(linux.unlink(path.ptr)) != .SUCCESS) return error.Io;
                        self.hist.record(clock.monotonicNs() -| t0);
                        self.ops += 1;
                    },
                    .write, .read => {
                        const is_write = self.pass.phase == .write;
                        var flags: linux.O = .{ .ACCMODE = if (is_write) .WRONLY else .RDONLY, .CLOEXEC = true };
                        // No O_TRUNC: iteration N+1 measures steady-state overwrites of the same inodes.
                        if (is_write) flags.CREAT = true;
                        if (self.pass.direct) flags.DIRECT = true;
                        const rc = linux.open(path.ptr, flags, 0o644);
                        if (linux.errno(rc) != .SUCCESS) return error.Io;
                        const fd: linux.fd_t = @intCast(rc);
                        defer _ = linux.close(fd);
                        blockOrder(order, self.pass.rand, blockSeed(self.tid, fid, 0xB10C));
                        for (order) |blk| {
                            const off = blk * self.pass.block;
                            const len: usize = @intCast(@min(self.pass.block, self.shape.size - off));
                            const t0 = clock.monotonicNs();
                            if (is_write) {
                                fillBlock(buf[0..len], self.tid, fid, blk);
                                var done: usize = 0;
                                while (done < len) {
                                    const n = linux.pwrite(fd, buf.ptr + done, len - done, @intCast(off + done));
                                    if (linux.errno(n) != .SUCCESS) return error.Io;
                                    done += @intCast(n);
                                }
                            } else {
                                var done: usize = 0;
                                while (done < len) {
                                    const n = linux.pread(fd, buf.ptr + done, len - done, @intCast(off + done));
                                    if (linux.errno(n) != .SUCCESS or n == 0) return error.Io;
                                    done += @intCast(n);
                                }
                            }
                            self.hist.record(clock.monotonicNs() -| t0);
                            self.ops += 1;
                            self.bytes += len;
                            // Checked AFTER the op: every worker completes at least one.
                            if (self.boxed()) break;
                        }
                        if (is_write) {
                            // Honest durable numbers: the length + the barrier inside the timed span.
                            if (linux.errno(linux.ftruncate(fd, @intCast(self.shape.size))) != .SUCCESS) return error.Io;
                            if (linux.errno(linux.fsync(fd)) != .SUCCESS) return error.Io;
                        }
                    },
                }
                if (self.boxed()) return;
            }
        }
    };

    fn mkdirs(root: []const u8, threads: u32) !void {
        var buf: [linux.PATH_MAX:0]u8 = undefined;
        const base = try std.fmt.bufPrintSentinel(&buf, "{s}/{s}", .{ root, DATASET_DIR }, 0);
        const rc = linux.mkdir(base.ptr, 0o755);
        if (linux.errno(rc) != .SUCCESS and linux.errno(rc) != .EXIST) return error.Io;
        var tid: u32 = 0;
        while (tid < threads) : (tid += 1) {
            const d = try std.fmt.bufPrintSentinel(&buf, "{s}/{s}/t{d}", .{ root, DATASET_DIR, tid }, 0);
            const r = linux.mkdir(d.ptr, 0o755);
            if (linux.errno(r) != .SUCCESS and linux.errno(r) != .EXIST) return error.Io;
        }
    }

    fn rmdirs(root: []const u8, threads: u32) void {
        var buf: [linux.PATH_MAX:0]u8 = undefined;
        var tid: u32 = 0;
        while (tid < threads) : (tid += 1) {
            const d = std.fmt.bufPrintSentinel(&buf, "{s}/{s}/t{d}", .{ root, DATASET_DIR, tid }, 0) catch return;
            _ = linux.rmdir(d.ptr);
        }
        const base = std.fmt.bufPrintSentinel(&buf, "{s}/{s}", .{ root, DATASET_DIR }, 0) catch return;
        _ = linux.rmdir(base.ptr);
    }

    /// `validate_dataset`: every file present at the shape's size, nothing
    /// else — a set without Write validates before timing and creates
    /// nothing.
    fn validateDataset(root: []const u8, shape: Shape) error{ Missing, WrongSize }!void {
        var buf: [linux.PATH_MAX:0]u8 = undefined;
        var tid: u32 = 0;
        while (tid < shape.threads) : (tid += 1) {
            var fid: u32 = 0;
            while (fid < shape.files) : (fid += 1) {
                const path = std.fmt.bufPrintSentinel(&buf, "{s}/{s}/t{d}/f{d}.bin", .{ root, DATASET_DIR, tid, fid }, 0) catch return error.Missing;
                var st: linux.Statx = undefined;
                if (linux.errno(linux.statx(linux.AT.FDCWD, path.ptr, 0, .{ .SIZE = true }, &st)) != .SUCCESS) return error.Missing;
                if (st.size != shape.size) return error.WrongSize;
            }
        }
    }

    fn datasetBytes(root: []const u8, threads: u32, files: u32) u64 {
        var buf: [linux.PATH_MAX:0]u8 = undefined;
        var total: u64 = 0;
        var tid: u32 = 0;
        while (tid < threads) : (tid += 1) {
            var fid: u32 = 0;
            while (fid < files) : (fid += 1) {
                const path = std.fmt.bufPrintSentinel(&buf, "{s}/{s}/t{d}/f{d}.bin", .{ root, DATASET_DIR, tid, fid }, 0) catch return total;
                var st: linux.Statx = undefined;
                if (linux.errno(linux.statx(linux.AT.FDCWD, path.ptr, 0, .{ .SIZE = true }, &st)) == .SUCCESS) total += st.size;
            }
        }
        return total;
    }

    /// The kernel's 64-bit `struct statfs` (`uapi/asm-generic/statfs.h`:
    /// the LP64 layout both x86_64 and aarch64 share; std carries no
    /// binding).
    const StatfsBuf = extern struct {
        f_type: i64,
        f_bsize: i64,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_fsid: [2]i32,
        f_namelen: i64,
        f_frsize: i64,
        f_flags: i64,
        f_spare: [4]i64,
    };

    comptime {
        std.debug.assert(@sizeOf(StatfsBuf) == 120);
    }

    /// `statfs(2)` — the kernel's free-space answer for the auto shape
    /// (the daemon's own `statfs` reply).
    fn freeBytes(root: []const u8) u64 {
        var zbuf: [linux.PATH_MAX:0]u8 = undefined;
        const z = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{root}, 0) catch return 0;
        var st: StatfsBuf = undefined;
        const rc = linux.syscall2(.statfs, @intFromPtr(z.ptr), @intFromPtr(&st));
        if (linux.errno(rc) != .SUCCESS) return 0;
        return st.f_bavail * @as(u64, @intCast(@max(st.f_bsize, 0)));
    }

    fn runPass(p: cli.Process, root: []const u8, shape: Shape, pass: Pass, iteration: u32) !Row {
        const gpa = p.gpa;
        const workers = try gpa.alloc(Worker, shape.threads);
        defer gpa.free(workers);
        const threads = try gpa.alloc(std.Thread, shape.threads);
        defer gpa.free(threads);
        const before_text = readStats(gpa, p.io, root);
        defer if (before_text) |t| gpa.free(t);
        const before: StatsTerms = if (before_text) |t| StatsTerms.parse(t) else .{};
        const t0 = clock.monotonicNs();
        const deadline: u64 = if (pass.time_box_s == 0) 0 else t0 + @as(u64, pass.time_box_s) * std.time.ns_per_s;
        for (workers, 0..) |*w, i| {
            w.* = .{ .tid = @intCast(i), .shape = shape, .pass = pass, .root = root, .deadline_ns = deadline, .gpa = gpa };
            threads[i] = try std.Thread.spawn(.{}, Worker.main, .{w});
        }
        for (threads) |t| t.join();
        const elapsed = clock.monotonicNs() -| t0;
        var row: Row = .{ .pass = pass, .shape = shape, .iteration = iteration, .ops = 0, .bytes = 0, .elapsed_ns = elapsed, .expected_ops = shape.expectedOps(pass), .boxed_out = false, .lat = .{}, .stats = .{} };
        for (workers) |*w| {
            if (w.failed) |e| {
                p.stderr.print("squeezefs bench: worker {d} failed ({t}) in pass {s}\n", .{ w.tid, e, @tagName(pass.phase) }) catch {};
                return error.WorkerFailed;
            }
            row.ops += w.ops;
            row.bytes += w.bytes;
            w.hist.fold(&row.lat);
        }
        row.boxed_out = row.ops < row.expected_ops;
        const after_text = readStats(gpa, p.io, root);
        defer if (after_text) |t| gpa.free(t);
        if (after_text) |t| row.stats = StatsTerms.delta(StatsTerms.parse(t), before);
        return row;
    }

    pub fn run(p: cli.Process, b: args.Bench) u8 {
        const gpa = p.gpa;
        const root = b.mountpoint;
        // Permission: the dataset dir must be creatable (names the allow_other rule).
        mkdirs(root, 1) catch {
            p.stderr.print("squeezefs bench: cannot create {s}/{s} — is {s} a squeezefs mount you may write (an `allow_other` mount is readable by every user but writable by its owner alone)?\n", .{ root, DATASET_DIR, root }) catch {};
            return cli.exit_failed;
        };
        const config_text = readConfig(gpa, p.io, root);
        defer if (config_text) |t| gpa.free(t);
        const stats_text = readStats(gpa, p.io, root);
        defer if (stats_text) |t| gpa.free(t);
        var tbuf: [64]u8 = undefined;
        const substrate = substrateOf(gpa, p.io, if (config_text) |t| json_scan.string(t, "sqdata_uri") else null, &tbuf);
        var uts: linux.utsname = undefined;
        _ = linux.uname(&uts);
        const posture: ?bool = if (stats_text) |t| ((json_scan.number(t, "direct_device_true") orelse 0) != 0) else null;
        // The shape.
        const block = b.block orelse SUITE_SEQ_BLOCK;
        const shape = if (b.smoke) smokeShape(block) else resolveShape(derive.facts().cpus, freeBytes(root), datasetBytes(root, b.threads orelse autoThreads(derive.facts().cpus), b.files orelse 1), block, .{ .threads = b.threads, .files = b.files, .size = b.size }) catch |e| {
            p.stderr.print("squeezefs bench: cannot size the dataset: {s} (free space must leave ≥ 4 GiB for a quarter-of-free dataset; pass -s/-t/-n explicitly)\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        validateShape(shape, b.direct or !b.anyPhase()) catch |e| {
            p.stderr.print("squeezefs bench: shape refused: {s}\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        // The passes.
        var suite = suitePasses(b.time);
        var explicit: [4]Pass = undefined;
        var smoke_passes: [6]Pass = undefined;
        const passes: []Pass = if (b.smoke) blk: {
            smoke_passes = suitePasses(0);
            // The smoke runs on a file-backed venue: buffered I/O, 1 MiB seq + 4 KiB rand, full coverage.
            for (&smoke_passes) |*ps| ps.direct = false;
            break :blk &smoke_passes;
        } else if (b.anyPhase()) phasePasses(b, block, &explicit) else &suite;
        if (!b.json) {
            p.stdout.print("instrument=squeezefs bench {s} {s} profile={s}{s}\nsubstrate={s} venue={s} {s}\nposture: {s}\nshape: threads={d} files/thread={d} size={d} B/file (total {d} MiB) iterations={d}{s}\n", .{
                p.build.train, p.build.commit,                   p.build.profile,                   if (std.mem.eql(u8, p.build.profile, "dist")) "" else "  warning: profile gate is not a dist measurement",
                substrate,     std.mem.sliceTo(&uts.release, 0), std.mem.sliceTo(&uts.nodename, 0),
                if (posture) |dt| (if (dt) "device-true (-o direct_device_true): O_DIRECT rows measure the DEVICE path" else "hybrid (default): O_DIRECT rows may serve from the read tiers once warm") else ".stats unreadable — posture unknown",
                shape.threads, shape.files,                      shape.size,                        shape.totalBytes() >> 20,
                b.iterations,  if (b.smoke) " (smoke)" else "",
            }) catch {};
        }
        if (!b.write and b.anyPhase() and !b.smoke) {
            validateDataset(root, shape) catch |e| {
                p.stderr.print("squeezefs bench: the dataset at {s}/{s} is {s} for this shape — run with -w first (the bench creates nothing it was not asked to)\n", .{ root, DATASET_DIR, if (e == error.Missing) "missing" else "the wrong size" }) catch {};
                return cli.exit_failed;
            };
        }
        mkdirs(root, shape.threads) catch {
            p.stderr.writeAll("squeezefs bench: cannot create the dataset directories\n") catch {};
            return cli.exit_failed;
        };
        var rows: std.ArrayList(Row) = .empty;
        defer rows.deinit(gpa);
        const arms: []const []const u8 = if (b.abba) &abba_order else &[_][]const u8{""};
        var it: u32 = 1;
        while (it <= b.iterations) : (it += 1) {
            for (arms) |arm| {
                for (passes) |ps| {
                    var pass = ps;
                    pass.arm = arm;
                    if (arm.len != 0) pass.direct = abbaDirect(arm, ps.direct);
                    const row = runPass(p, root, shape, pass, it) catch return cli.exit_failed;
                    rows.append(gpa, row) catch return cli.exit_failed;
                    (if (b.json) row.writeJson(p.stdout) else row.writeText(p.stdout)) catch {};
                    p.stdout.flush() catch {};
                }
            }
        }
        if (b.abba and !b.json) {
            const br = Bracket.fold(rows.items);
            p.stdout.print("bracket A-B-B-A: A (direct={}) mean {d} MiB/s spread {d}; B mean {d} MiB/s spread {d}\n", .{ b.direct, br.a_mean, br.a_spread, br.b_mean, br.b_spread }) catch {};
        }
        // The del pass (the suite, the smoke) leaves the mount clean.
        var deleted = false;
        for (passes) |ps| deleted = deleted or ps.phase == .del;
        if (deleted) rmdirs(root, shape.threads);
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }
} else struct {};

pub fn run(p: cli.Process, b: args.Bench) u8 {
    if (!is_linux) {
        p.stderr.writeAll("squeezefs bench: Linux only (charter §1.2)\n") catch {};
        return cli.exit_failed;
    }
    return engine.run(p, b);
}

// ---------------------------------------------------------------------------
// Tests (portable).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "auto shape: threads clamp to [1, 16]; total = max(16 GiB, 2 GiB × threads) capped at free/4 (+ the dataset), rounded to 1 MiB, refused below 4 GiB; size rounds down to 1 MiB; explicit flags override" {
    try testing.expectEqual(@as(u32, 1), autoThreads(0));
    try testing.expectEqual(@as(u32, 8), autoThreads(8));
    try testing.expectEqual(@as(u32, 16), autoThreads(64));
    try testing.expectEqual(@as(u64, 16 << 30), try autoTotal(4, 1 << 40, 0));
    try testing.expectEqual(@as(u64, 32 << 30), try autoTotal(16, 1 << 40, 0));
    // free/4 caps: 40 GiB free → 10 GiB.
    try testing.expectEqual(@as(u64, 10 << 30), try autoTotal(4, 40 << 30, 0));
    // The existing dataset counts as free: 10 GiB free + 30 GiB dataset → 10 GiB.
    try testing.expectEqual(@as(u64, 10 << 30), try autoTotal(4, 10 << 30, 30 << 30));
    try testing.expectError(error.FilesystemTooSmall, autoTotal(4, 8 << 30, 0));
    const s = try resolveShape(4, 1 << 40, 0, SUITE_SEQ_BLOCK, .{});
    try testing.expectEqual(@as(u32, 4), s.threads);
    try testing.expectEqual(@as(u32, 1), s.files);
    try testing.expectEqual(@as(u64, 4 << 30), s.size);
    try testing.expectEqual(@as(u64, 0), s.size % SUITE_RAND_BLOCK);
    const e = try resolveShape(4, 1 << 40, 0, 4096, .{ .threads = 3, .files = 7, .size = 123_456 });
    try testing.expectEqual(@as(u32, 3), e.threads);
    try testing.expectEqual(@as(u32, 7), e.files);
    try testing.expectEqual(@as(u64, 123_456), e.size);
    // Explicit threads with an auto size: 16 GiB / (3 × 7) rounded down to 1 MiB.
    const h = try resolveShape(4, 1 << 40, 0, 4096, .{ .threads = 3, .files = 7 });
    try testing.expectEqual(((16 << 30) / 21 / SIZE_ROUND) * SIZE_ROUND, h.size);
    try testing.expectEqual(SMOKE_THREADS, smokeShape(4096).threads);
    try testing.expect(smokeShape(4096).totalBytes() <= 16 << 20);
}

test "a pass's coverage is the file at the PASS's block: the suite's rand 4 KiB passes over a 1 MiB-block shape expect size/4096 ops per file, not the shape's 1 MiB count (the earlier fold touched 1/256 of the file)" {
    const shape: Shape = .{ .threads = 2, .files = 1, .size = 2 << 30, .block = 1 << 20 };
    const suite = suitePasses(null);
    try testing.expectEqual(@as(u64, 2 * 2048), shape.expectedOps(suite[0]));
    try testing.expectEqual(@as(u64, 2 * 524288), shape.expectedOps(suite[2]));
    try testing.expectEqual(@as(u64, 2 * 524288), shape.expectedOps(suite[3]));
    try testing.expectEqual(@as(u64, 2), shape.expectedOps(suite[4]));
}

test "shape validation: zero terms refuse; --direct needs a 4 KiB-multiple block and a block-multiple size" {
    try validateShape(.{ .threads = 1, .files = 1, .size = 8192, .block = 4096 }, true);
    try testing.expectError(error.ZeroThreads, validateShape(.{ .threads = 0, .files = 1, .size = 1, .block = 1 }, false));
    try testing.expectError(error.ZeroFiles, validateShape(.{ .threads = 1, .files = 0, .size = 1, .block = 1 }, false));
    try testing.expectError(error.ZeroSize, validateShape(.{ .threads = 1, .files = 1, .size = 0, .block = 1 }, false));
    try testing.expectError(error.ZeroBlock, validateShape(.{ .threads = 1, .files = 1, .size = 1, .block = 0 }, false));
    try testing.expectError(error.DirectBlockUnaligned, validateShape(.{ .threads = 1, .files = 1, .size = 8192, .block = 512 }, true));
    try testing.expectError(error.DirectSizeNotBlockMultiple, validateShape(.{ .threads = 1, .files = 1, .size = 5000, .block = 4096 }, true));
    try validateShape(.{ .threads = 1, .files = 1, .size = 5000, .block = 512 }, false);
    try testing.expectEqual(@as(u64, 2), blockCount(5000, 4096));
    try testing.expectEqual(@as(u64, 1), blockCount(4096, 4096));
}

test "passes: the suite is write seq 1m direct → read seq 1m direct → read rand 4k (boxed 30 s) → write rand 4k (boxed) → stat → del; --time overrides the rand boxes only (0 = coverage); explicit phases keep the fixed order and inherit the rand box" {
    const s = suitePasses(null);
    try testing.expectEqual(Phase.write, s[0].phase);
    try testing.expect(s[0].block == SUITE_SEQ_BLOCK and !s[0].rand and s[0].direct and s[0].time_box_s == 0);
    try testing.expectEqual(Phase.read, s[1].phase);
    try testing.expect(s[2].phase == .read and s[2].rand and s[2].block == SUITE_RAND_BLOCK and s[2].time_box_s == RAND_TIME_BOX_S);
    try testing.expect(s[3].phase == .write and s[3].rand and s[3].time_box_s == RAND_TIME_BOX_S);
    try testing.expectEqual(Phase.stat, s[4].phase);
    try testing.expectEqual(Phase.del, s[5].phase);
    const t = suitePasses(7);
    try testing.expectEqual(@as(u32, 7), t[2].time_box_s);
    try testing.expectEqual(@as(u32, 7), t[0].time_box_s);
    try testing.expectEqual(@as(u32, 0), suitePasses(0)[2].time_box_s);
    var buf: [4]Pass = undefined;
    const ps = phasePasses(.{ .mountpoint = "/m", .del = true, .write = true, .stat = true, .rand = true }, 4096, &buf);
    try testing.expectEqual(@as(usize, 3), ps.len);
    try testing.expectEqual(Phase.write, ps[0].phase);
    try testing.expectEqual(Phase.stat, ps[1].phase);
    try testing.expectEqual(Phase.del, ps[2].phase);
    try testing.expectEqual(RAND_TIME_BOX_S, ps[0].time_box_s);
    try testing.expectEqual(@as(u32, 0), ps[1].time_box_s);
    try testing.expectEqual(@as(usize, 0), phasePasses(.{ .mountpoint = "/m" }, 4096, &buf).len);
}

test "block order: sequential is the identity; rand is a full-coverage permutation (every block exactly once), deterministic per seed, different across seeds; the fill is deterministic and non-zero" {
    var order: [64]u64 = undefined;
    blockOrder(&order, false, 1);
    for (order, 0..) |o, i| try testing.expectEqual(@as(u64, i), o);
    var a: [64]u64 = undefined;
    var b: [64]u64 = undefined;
    blockOrder(&a, true, 42);
    blockOrder(&b, true, 42);
    try testing.expectEqualSlices(u64, &a, &b);
    var seen: [64]bool = @splat(false);
    var moved = false;
    for (a, 0..) |o, i| {
        try testing.expect(o < 64 and !seen[o]);
        seen[o] = true;
        moved = moved or o != i;
    }
    try testing.expect(moved);
    var c: [64]u64 = undefined;
    blockOrder(&c, true, 43);
    try testing.expect(!std.mem.eql(u64, &a, &c));
    var one: [1]u64 = undefined;
    blockOrder(&one, true, 9);
    try testing.expectEqual(@as(u64, 0), one[0]);
    var f1: [4096]u8 = undefined;
    var f2: [4096]u8 = undefined;
    fillBlock(&f1, 1, 2, 3);
    fillBlock(&f2, 1, 2, 3);
    try testing.expectEqualSlices(u8, &f1, &f2);
    var i: usize = 0;
    while (i < f1.len) : (i += 8) try testing.expect(std.mem.readInt(u64, f1[i..][0..8], .little) != 0);
    fillBlock(&f2, 1, 2, 4);
    try testing.expect(!std.mem.eql(u8, &f1, &f2));
    var tail: [13]u8 = undefined;
    fillBlock(&tail, 0, 0, 0);
    try testing.expectEqual(@as(u8, 0xA5), tail[12]);
}

test "rows: quantiles are the bucket's UPPER bound over exact counts; MiB/s and IOPS; amplification in ppm and wareq-sz from the stats delta; a byte-moving row under 60 s is scoping-only; the JSON names the measurement-law fields" {
    var h: counters.LatencyHistogram = .{};
    h.record(500); // ≤ 1 µs
    h.record(2_500); // ≤ 2 µs (2 µs after the integer-µs truncation)
    h.record(3_000_000); // ≤ 4 ms
    var snap: counters.LatencyHistogram.Snapshot = .{};
    h.fold(&snap);
    try testing.expectEqual(@as(u64, 1_000), quantileNs(snap, 0));
    try testing.expectEqual(@as(u64, 2_000), quantileNs(snap, 500));
    try testing.expectEqual(@as(u64, 4_096_000), quantileNs(snap, 990)); // bucket "<=4ms" is 2^12 µs
    try testing.expectEqual(@as(u64, 4_096_000), quantileNs(snap, 1000));
    try testing.expectEqual(@as(u64, 0), quantileNs(.{}, 500));
    const row: Row = .{ .pass = .{ .phase = .write, .block = 4096, .rand = true, .direct = true, .time_box_s = 30 }, .shape = .{ .threads = 2, .files = 1, .size = 1 << 20, .block = 4096 }, .iteration = 1, .ops = 512, .bytes = 2 << 20, .elapsed_ns = std.time.ns_per_s, .expected_ops = 512, .boxed_out = false, .lat = snap, .stats = .{ .present = true, .device_write_bytes = 4 << 20, .device_writes = 2, .terminal_frees = 3, .reclaim_queued = 1, .reclaim_elided = 2 } };
    try testing.expectEqual(@as(u64, 2), row.mibPerS());
    try testing.expectEqual(@as(u64, 512), row.iops());
    try testing.expectEqual(@as(u64, 2_000_000), row.amplificationPpm());
    try testing.expectEqual(@as(u64, 2 << 20), row.wareqSz());
    try testing.expect(row.scopingOnly());
    var out: Io.Writer.Allocating = .init(testing.allocator);
    defer out.deinit();
    try row.writeJson(&out.writer);
    for ([_][]const u8{ "\"pass\":\"write\"", "\"access\":\"rand\"", "\"io\":\"direct\"", "\"mib_per_s\":2", "\"iops\":512", "\"scoping_only\":true", "\"p99\":4096000", "\"amplification_ppm\":2000000", "\"wareq_sz\":2097152", "\"reclaim\":{\"terminal_frees\":3,\"queued\":1,\"elided\":2,\"discard_bytes\":0}", "\"invariant_tripwires\":0" }) |needle| {
        if (std.mem.indexOf(u8, out.written(), needle) == null) {
            std.debug.print("\n--- missing {s} in {s} ---\n", .{ needle, out.written() });
            return error.TestExpectedNeedle;
        }
    }
    out.clearRetainingCapacity();
    try row.writeText(&out.writer);
    try testing.expect(std.mem.indexOf(u8, out.written(), "device bytes ÷ user bytes 2.000") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "scoping-only (<60 s)") != null);
    var long = row;
    long.elapsed_ns = 61 * std.time.ns_per_s;
    try testing.expect(!long.scopingOnly());
    const stat_row: Row = .{ .pass = .{ .phase = .stat, .block = 4096, .rand = false, .direct = false, .time_box_s = 0 }, .shape = row.shape, .iteration = 1, .ops = 2, .bytes = 0, .elapsed_ns = 1000, .expected_ops = 2, .boxed_out = false, .lat = .{}, .stats = .{} };
    try testing.expect(!stat_row.scopingOnly());
}

test "the stats delta reads the terms by key name across the families; absent keys read 0; the delta saturates" {
    const before = StatsTerms.parse("{\"daemon\":{\"fuse_ops\":10,\"invariant_tripwires\":0},\"data\":{\"overlay_store_bytes\":100,\"patch_write_bytes\":50,\"put_obj\":3,\"admit_write_overlay_bytes\":100,\"terminal_frees\":1,\"block_free_reclaim_queued\":1,\"direct_device_true\":1}}");
    const after = StatsTerms.parse("{\"daemon\":{\"fuse_ops\":25,\"invariant_tripwires\":0},\"data\":{\"overlay_store_bytes\":400,\"patch_write_bytes\":50,\"put_obj\":7,\"admit_write_overlay_bytes\":300,\"terminal_frees\":1,\"block_free_reclaim_queued\":1,\"direct_device_true\":1}}");
    try testing.expectEqual(@as(u64, 150), before.device_write_bytes);
    const d = StatsTerms.delta(after, before);
    try testing.expect(d.present and d.direct_device_true);
    try testing.expectEqual(@as(u64, 15), d.fuse_ops);
    try testing.expectEqual(@as(u64, 300), d.device_write_bytes);
    try testing.expectEqual(@as(u64, 4), d.device_writes);
    try testing.expectEqual(@as(u64, 200), d.admission_bytes);
    try testing.expectEqual(@as(u64, 0), d.terminal_frees);
    try testing.expectEqual(@as(u64, 0), StatsTerms.delta(before, after).fuse_ops);
    try testing.expect(!StatsTerms.delta(after, .{}).present);
}

test "the A-B-B-A bracket: the arm order, the direct lever per arm, the fold over byte-moving rows" {
    try testing.expectEqualStrings("A", abba_order[0]);
    try testing.expectEqualStrings("B", abba_order[1]);
    try testing.expectEqualStrings("B", abba_order[2]);
    try testing.expectEqualStrings("A", abba_order[3]);
    try testing.expect(abbaDirect("A", true) and !abbaDirect("B", true) and !abbaDirect("A", false) and abbaDirect("B", false));
    const mk = struct {
        fn row(arm: []const u8, mib: u64, phase: Phase) Row {
            return .{ .pass = .{ .phase = phase, .block = 4096, .rand = false, .direct = true, .time_box_s = 0, .arm = arm }, .shape = .{ .threads = 1, .files = 1, .size = 1, .block = 4096 }, .iteration = 1, .ops = 1, .bytes = mib << 20, .elapsed_ns = std.time.ns_per_s, .expected_ops = 1, .boxed_out = false, .lat = .{}, .stats = .{} };
        }
    };
    const rows = [_]Row{ mk.row("A", 100, .read), mk.row("B", 80, .read), mk.row("B", 84, .read), mk.row("A", 104, .read), mk.row("A", 9999, .stat) };
    const br = Bracket.fold(&rows);
    try testing.expectEqual(@as(u64, 102), br.a_mean);
    try testing.expectEqual(@as(u64, 82), br.b_mean);
    try testing.expectEqual(@as(u64, 4), br.a_spread);
    try testing.expectEqual(@as(u64, 4), br.b_spread);
}

fn statsTermsFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [1024]u8 = undefined;
    const len = smith.slice(&buf);
    const t = StatsTerms.parse(buf[0..len]);
    try std.testing.expect(t.present);
}

test "fuzz: the .stats term reader is total on arbitrary JSON-shaped text" {
    try fuzzkit.fuzz({}, statsTermsFuzz, .{ .corpus = &.{fuzzkit.slice("{\"fuse_ops\":12,\"put_obj\":-1,\"direct_device_true\":1e9}")} });
}
