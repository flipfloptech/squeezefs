//! C7 — the data scrub (`ops_fsck.md` §4; charter §8): per referenced
//! mapping, read the stored image — a decorated tenant reads `base + rel`
//! for `ceil_4k(len)` and slices to the exact `len`; an undecorated whole
//! block reads the block on passthrough, else `max_stored_image_len`
//! rounded to the DMA grain (transformed images carry a self-delimiting
//! frame and may exceed the block). Then: encrypted ⇒ AEAD open; compressed
//! ⇒ frame decode incl. the raw escape; plain ⇒ READABILITY ONLY — no stored
//! checksum exists, so read success is the honest verdict, counted apart
//! (`scrub_readability_only`, the honesty gauge). Device-bound sequential:
//! sorted by `(volume, offset)`, `SCRUB_BATCH` blocks per duty-cycle unit
//! through the pacer (the job fabric's throttle seam; a local pacer until it
//! lands). The ladder: online a failure re-resolves the ino's CURRENT layout
//! (the mapping must still be present verbatim) and re-reads under a
//! VALIDATED pin — `pinned` ⇒ re-verify, `pinned_unstable` (an in-flight W1
//! patch: a torn read, not corruption) ⇒ cleared, `refused` (freed under us)
//! ⇒ cleared; offline a failure is a finding directly. A DAMAGED mapping is
//! never re-scrubbed or re-reported.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;

const clock = @import("../../core/clock.zig");
const derive = @import("../../core/derive.zig");
const log = @import("../../core/log.zig");
const layout = @import("../../data/layout.zig");
const transform = @import("../../data/transform.zig");
const fsck = @import("../fsck.zig");
const finding = @import("finding.zig");
const census = @import("census.zig");
const classes = @import("classes.zig");

pub const Error = census.Error;
pub const Reader = classes.BlockReader;

/// What a volume's frame verification yields.
pub const Verdict = enum { aead, frame, plain, failed };

/// The per-volume transform face: `verify(image)` opens / decodes the frame
/// (the Linux binding serializes the plane's one `Transform`; a test binds
/// a `Transform` directly); null = a passthrough volume.
pub const TransformFace = struct {
    ctx: *anyopaque,
    /// The frame verdict over a stored image (the image may be mutated in
    /// place by the AEAD open).
    verify: *const fn (*anyopaque, image: []u8) Verdict,
};

pub const VolumeXform = struct {
    vol_tag: u64,
    cfg: transform.VolumeTransform,
    face: ?TransformFace,
};

/// The throttle seam (the job fabric's KD-3 duty cycle, live-retunable);
/// `pct` 0 / ≥ 100 = unthrottled.
pub const Throttle = struct {
    ctx: ?*anyopaque,
    pct: *const fn (?*anyopaque) u8,

    pub const none: Throttle = .{ .ctx = null, .pct = &zeroPct };

    fn zeroPct(_: ?*anyopaque) u8 {
        return 0;
    }
};

/// `job_throttle_sleep(elapsed, pct) = elapsed × (100 − pct) / pct`; null
/// under 1 ms or at 0 / ≥ 100 (JOBS:L4442-4454): task-active time ≈ `pct`
/// of wall time.
pub fn dutySleepNs(elapsed_ns: u64, pct: u8) ?u64 {
    if (pct == 0 or pct >= 100) return null;
    const sleep = elapsed_ns * (100 - @as(u64, pct)) / pct;
    return if (sleep < std.time.ns_per_ms) null else sleep;
}

/// The test seam: a device offset the reader must fail (the G-VL-5(b)
/// "dm-error or equivalent" arm at the unit tier); `maxInt` = inert.
pub var test_read_fault_off: std.atomic.Value(u64) = .init(std.math.maxInt(u64));

pub const Verifier = struct {
    gpa: Allocator,
    reader: Reader,
    chunk: u64,
    volumes: []const VolumeXform,
    /// DMA-aligned: the device reader lands O_DIRECT windows here.
    image: []align(transform.DMA_GRAIN) u8,
    scratch: []u8,

    pub fn init(gpa: Allocator, reader: Reader, chunk: u64, volumes: []const VolumeXform) Allocator.Error!Verifier {
        var block: u32 = 0;
        for (volumes) |v| block = @max(block, v.cfg.block_size);
        // The largest image any volume stores, rounded to the grain.
        var image_len: u64 = chunk;
        for (volumes) |v| image_len = @max(image_len, derive.alignUp(transform.maxStoredImageLen(v.cfg), transform.DMA_GRAIN));
        const image = try gpa.alignedAlloc(u8, .fromByteUnits(transform.DMA_GRAIN), @intCast(image_len));
        errdefer gpa.free(image);
        const scratch = try gpa.alloc(u8, @intCast(if (block == 0) 0 else transform.scratchLen(block)));
        return .{ .gpa = gpa, .reader = reader, .chunk = chunk, .volumes = volumes, .image = image, .scratch = scratch };
    }

    pub fn deinit(self: *Verifier) void {
        self.gpa.free(self.image);
        self.gpa.free(self.scratch);
        self.* = undefined;
    }

    fn volume(self: *const Verifier, vol_tag: u64) ?*const VolumeXform {
        for (self.volumes) |*v| if (v.vol_tag == vol_tag) return v;
        return null;
    }

    pub const Outcome = struct { verdict: Verdict, arm: finding.ScrubArm, bytes: u64 };

    /// `verify_stored_block`: the read + the frame verdict.
    pub fn verifyStored(self: *Verifier, m: layout.Mapping) Error!Outcome {
        const v = self.volume(m.vol_tag) orelse return .{ .verdict = .failed, .arm = .read, .bytes = 0 };
        const passthrough = v.cfg.isPassthrough();
        var off: u64 = undefined;
        var len: u64 = undefined;
        var exact: u64 = undefined;
        if (m.whole_block) {
            off = m.offset;
            len = if (passthrough) v.cfg.block_size else derive.alignUp(transform.maxStoredImageLen(v.cfg), transform.DMA_GRAIN);
            exact = len;
        } else {
            off = m.offset + m.rel_off;
            len = layout.ceilGrain(m.len);
            exact = m.len;
        }
        if (len > self.image.len) return .{ .verdict = .failed, .arm = .short_read, .bytes = 0 };
        const dest = self.image[0..@intCast(len)];
        if (test_read_fault_off.load(.acquire) == off) return .{ .verdict = .failed, .arm = .read, .bytes = 0 };
        self.reader.read(self.reader.ctx, m.vol_tag, off, dest) catch return .{ .verdict = .failed, .arm = .read, .bytes = 0 };
        if (passthrough) return .{ .verdict = .plain, .arm = .read, .bytes = len };
        const face = v.face orelse return .{ .verdict = .failed, .arm = .frame, .bytes = len };
        const verdict = face.verify(face.ctx, dest[0..@intCast(exact)]);
        return .{ .verdict = verdict, .arm = if (v.cfg.encryption != .none) .aead else .frame, .bytes = len };
    }

    /// The C2-lost content arm: good content ⇒ true.
    pub fn verifyMapping(self: *Verifier, m: layout.Mapping) Error!bool {
        const o = try self.verifyStored(m);
        return o.verdict != .failed;
    }
};

/// The one `Transform` behind a face (the portable binding; the Linux
/// runner wraps the plane's locked transform the same way).
/// One codec behind a face (the portable binding; the Linux runner binds
/// the plane's codec pool the same way).
pub const TransformBinding = struct {
    codec: *transform.Codec,
    scratch: []u8,

    pub fn face(self: *TransformBinding) TransformFace {
        return .{ .ctx = self, .verify = &verifyFn };
    }

    fn verifyFn(ctx: *anyopaque, image: []u8) Verdict {
        const self: *TransformBinding = @ptrCast(@alignCast(ctx));
        _ = self.codec.processRead(image, self.scratch) catch return .failed;
        return if (self.codec.cfg().encryption != .none) .aead else .frame;
    }
};

const Row = struct { m: census.MappingRef, off: u64 };

fn rowLess(_: void, a: Row, b: Row) bool {
    if (a.m.mapping.vol_tag != b.m.mapping.vol_tag) return a.m.mapping.vol_tag < b.m.mapping.vol_tag;
    return a.off < b.off;
}

/// The scrub over the pass's census (a FRESH one post-settle is the
/// caller's choice of `cen`): every mapping sorted by `(volume, offset)`,
/// `SCRUB_BATCH` per pacer unit; failures through the ladder.
pub fn run(pass: *classes.Pass, cen: *const census.Census, v: *Verifier, throttle: Throttle) Error!void {
    const gpa = pass.gpa;
    const host = pass.host;
    var rows: std.ArrayList(Row) = .empty;
    defer rows.deinit(gpa);
    for (cen.mappings.items) |m| {
        if (m.mapping.damaged) continue;
        // A reservation names no bytes (charter §6.6): counted, never read.
        if (m.mapping.unwritten) {
            pass.counters.scrub_unwritten_skipped += 1;
            continue;
        }
        try rows.append(gpa, .{ .m = m, .off = m.mapping.offset + m.mapping.rel_off });
    }
    std.mem.sort(Row, rows.items, {}, rowLess);
    var unit_start = clock.monotonicNs();
    var in_unit: usize = 0;
    for (rows.items) |r| {
        if (host.cancelled()) return;
        const o = try v.verifyStored(r.m.mapping);
        pass.counters.scrub_blocks_scanned += 1;
        pass.counters.scrub_bytes_scanned += o.bytes;
        switch (o.verdict) {
            .aead => pass.counters.scrub_aead_verified += 1,
            .frame => pass.counters.scrub_frame_verified += 1,
            .plain => pass.counters.scrub_readability_only += 1,
            .failed => {
                if (try reverify(pass, v, r.m, o.arm)) {
                    pass.counters.scrub_failures += 1;
                    try pass.findings.append(gpa, .{ .c7_scrub = .{ .ino = r.m.ino, .block_index = r.m.block_index, .vol_tag = r.m.mapping.vol_tag, .block_idx = r.m.mapping.blockIdx(host.geo.chunk), .arm = o.arm } });
                } else pass.counters.suspects_cleared += 1;
            },
        }
        in_unit += 1;
        if (in_unit == fsck.SCRUB_BATCH) {
            in_unit = 0;
            const now = clock.monotonicNs();
            if (dutySleepNs(now -| unit_start, throttle.pct(throttle.ctx))) |ns| host.io.sleep(.fromNanoseconds(@intCast(ns)), .awake) catch {};
            unit_start = clock.monotonicNs();
        }
    }
}

/// `reverify_scrub_failure`: true = the failure stands.
fn reverify(pass: *classes.Pass, v: *Verifier, m: census.MappingRef, arm: finding.ScrubArm) Error!bool {
    _ = arm;
    const host = pass.host;
    if (!host.online()) return true;
    pass.counters.suspects += 1;
    // The mapping must still be present verbatim in the CURRENT layout.
    var binds: std.ArrayList(census.MappingRef) = .empty;
    defer binds.deinit(pass.gpa);
    if (!(try host.currentBindings(pass.gpa, m.ino, &binds))) return false;
    var current: ?layout.Mapping = null;
    for (binds.items) |b| if (b.block_index == m.block_index and b.mapping.eql(m.mapping)) {
        current = b.mapping;
    };
    const cur = current orelse return false;
    const vol = host.volume(cur.vol_tag) orelse return true;
    const idx = cur.blockIdx(host.geo.chunk);
    if (idx >= vol.alloc.capacityBlocks()) return true;
    switch (vol.alloc.pinBlockValidated(idx)) {
        .pinned => {
            defer vol.alloc.unpin(idx);
            const again = try v.verifyStored(cur);
            return again.verdict == .failed;
        },
        .pinned_unstable => {
            vol.alloc.unpin(idx);
            pass.counters.scrub_pin_unstable_cleared += 1;
            return false;
        },
        .refused => return false,
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the throttle law is exact at every duty target: sleep ∈ {3t, t, t/3} at 25/50/75; null at 0, ≥ 100 and under 1 ms" {
    const t: u64 = 40 * std.time.ns_per_ms;
    try testing.expectEqual(@as(?u64, 3 * t), dutySleepNs(t, 25));
    try testing.expectEqual(@as(?u64, t), dutySleepNs(t, 50));
    try testing.expectEqual(@as(?u64, t / 3), dutySleepNs(t, 75));
    try testing.expect(dutySleepNs(t, 0) == null);
    try testing.expect(dutySleepNs(t, 100) == null);
    try testing.expect(dutySleepNs(t, 150) == null);
    try testing.expect(dutySleepNs(500_000, 50) == null);
}

test "verifyStored: a passthrough block is readability-only, a transformed tenant reads its exact window and the frame verdict names the arm; the fault seam fails the read" {
    const gpa = testing.allocator;
    const chunk: u64 = 64 * 1024;
    const Dev = struct {
        bytes: []u8,
        fn read(ctx: *anyopaque, _: u64, off: u64, dest: []u8) Error!void {
            const d: *@This() = @ptrCast(@alignCast(ctx));
            if (off + dest.len > d.bytes.len) return error.Io;
            @memcpy(dest, d.bytes[@intCast(off)..][0..dest.len]);
        }
    };
    const img = try gpa.alloc(u8, 4 * chunk);
    defer gpa.free(img);
    @memset(img, 0xA5);
    var dev: Dev = .{ .bytes = img };
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const cfg: transform.VolumeTransform = .{ .compression = .lz4, .block_size = @intCast(chunk) };
    const xf = try transform.Transform.init(&env, cfg, null);
    var t = try transform.Codec.init(gpa, &xf);
    defer t.deinit(gpa);
    const scratch = try gpa.alloc(u8, @intCast(transform.scratchLen(@intCast(chunk))));
    defer gpa.free(scratch);
    // A compressed tenant frame at block 1, rel 4096.
    var plain_buf: [1200]u8 = undefined;
    for (0..100) |i| @memcpy(plain_buf[i * 12 ..][0..12], "hello scrub ");
    const plain: []const u8 = &plain_buf;
    const wscratch = try gpa.alloc(u8, @intCast(transform.scratchLen(@intCast(chunk))));
    defer gpa.free(wscratch);
    const frame = try t.processWrite(plain, wscratch);
    @memcpy(img[@intCast(chunk + 4096)..][0..frame.len], frame);
    var tb: TransformBinding = .{ .codec = &t, .scratch = scratch };
    const vols = [_]VolumeXform{
        .{ .vol_tag = 0x10, .cfg = .{ .block_size = @intCast(chunk) }, .face = null },
        .{ .vol_tag = 0x20, .cfg = cfg, .face = tb.face() },
    };
    var v = try Verifier.init(gpa, .{ .ctx = &dev, .read = &Dev.read }, chunk, &vols);
    defer v.deinit();
    const plain_block = try v.verifyStored(layout.Mapping.wholeBlock(0x10, 0, 0));
    try testing.expectEqual(Verdict.plain, plain_block.verdict);
    try testing.expectEqual(chunk, plain_block.bytes);
    const tenant = try v.verifyStored(layout.Mapping.tenant(0x20, chunk, 0, 4096, @intCast(frame.len)));
    try testing.expectEqual(Verdict.frame, tenant.verdict);
    try testing.expectEqual(finding.ScrubArm.frame, tenant.arm);
    // Flip the frame's magic: the frame decode refuses (a flipped literal
    // inside an lz4 body decodes to WRONG bytes without an error — the
    // frame arm verifies the self-delimiting structure, never a checksum
    // the format does not carry; that is the honesty gauge's point). The
    // refusal is the transform's loud tripwire by contract.
    img[@intCast(chunk + 4096 + 1)] ^= 0xFF;
    transform.stats.transform_frame_refusals_magic.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    const broken = try v.verifyStored(layout.Mapping.tenant(0x20, chunk, 0, 4096, @intCast(frame.len)));
    try log.testing.expectAllFired();
    try testing.expectEqual(Verdict.failed, broken.verdict);
    // The fault seam.
    test_read_fault_off.store(0, .release);
    defer test_read_fault_off.store(std.math.maxInt(u64), .release);
    const faulted = try v.verifyStored(layout.Mapping.wholeBlock(0x10, 0, 0));
    try testing.expectEqual(Verdict.failed, faulted.verdict);
    try testing.expectEqual(finding.ScrubArm.read, faulted.arm);
}
