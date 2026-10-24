//! W1 — the sole-owner extent patch, arm A of the write admission (charter
//! §6.2 item 2; `docs/porting/data_write_path.md` §2.1, §4). ONE aligned
//! DMA at `offset + rel` into the block the file already owns alone: zero
//! allocation, zero free, zero map merge, zero journal entry, zero staging.
//! Eligible iff: a striped file with the UNDECORATED whole-block mapping; no
//! RAM buffer, no staged entry, no open overlay for the key; a passthrough
//! volume; `refcount == 1 ∧ !SHARED` (durably confirmed by one probe of the
//! ino's slot tree on an armed set; a foreign-custody file declines; a
//! non-holder joiner declines); offset and length 4 KiB-aligned, `len ≤
//! PATCH_MAX = block/8`, non-extending, within one block; not stream-
//! adjacent (the per-inode `last_write_end`).
//!
//! The protocol: mark the incarnation unstable → `fence(SeqCst)` → re-check
//! the refcount → ONE DMA → publish → purge the key from all four read tiers
//! → ACK. The clone side pins (refcount CAS) → `fence(SeqCst)` → snapshots
//! the incarnation; unstable/moved ⇒ unpin and retry (3 attempts). Both
//! fences are load-bearing: each side is a store on one word then a load of
//! the OTHER (store buffering); release/acquire lets both read stale on
//! ARM/POWER. The pair lives in `block_alloc.beginPatchSoleOwner` /
//! `pinBlockValidated` as four SeqCst accesses (Zig 0.17 has no standalone
//! fence) and `incarnation_model.PatchCloneModel` proves it with EITHER
//! fence weakened failing — the composed model, since per-protocol models
//! structurally cannot see store buffering across protocols.
//!
//! `patch_edge_rmw_reads = 0` BY CONTRACT: only app-written sectors are
//! ever rewritten (the alignment predicate is the proof — no edge read-
//! modify-write exists in this module; the tripwire is the belt). Measured:
//! rand-4k **354–397 → 61.5–66.7 k IOPS** (13.26×/15.04×/14.57× vs JFS,
//! `design-random-small-writes.md:75,308`); amplification 1.0× W / 0.0× R
//! cargo tier with `patch_writes ≈ ops`. W1 is un-disableable on a
//! production mount: `SQUEEZEFS_PATCH_MAX_BYTES=0` is the A/B lever only.

const std = @import("std");
const Io = std.Io;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const seams = @import("write_seams.zig");
const block_alloc = @import("block_alloc.zig");
const incarnation_model = @import("incarnation_model.zig");

pub const Binding = seams.Binding;
pub const PAGE: u32 = seams.PAGE;

/// `block / 8` = 512 KiB at the shipped block: the natural sub-block
/// denominator; field-identical to the pre-sweep 512 KiB. Above it the
/// device overlay (arm B) is the cheaper legal I/O (a whole-block CoW
/// dest beats patching ≥ ⅛ of a block in place under a crash window).
pub fn patchMaxDerived(block_size: u64) u64 {
    return block_size / 8;
}

pub fn resolvePatchMax(explicit: ?u64, block_size: u64) u64 {
    return explicit orelse patchMaxDerived(block_size);
}

// ---------------------------------------------------------------------------
// Stats (§11.2).
// ---------------------------------------------------------------------------

/// The decision ledger: every striped write resolves to exactly ONE of
/// `patched` or one `patch_ineligible_*` bucket (`Σ ≡ invocations`).
pub const Bucket = enum(u8) {
    adjacent,
    oversize,
    unaligned,
    range_shared,
    overlay,
    device_overlay,
    transform,
    unmapped,
    decorated,
    shared,
    foreign_custody,
    posture,

    pub const count = @typeInfo(Bucket).@"enum".field_names.len;
};

pub const stats = struct {
    pub var patch_writes: counters.ShardedCounter = .{};
    pub var patch_write_bytes: counters.ShardedCounter = .{};
    pub var patch_dma_errors: counters.ShardedCounter = .{};
    pub var patch_ineligible: [Bucket.count]counters.ShardedCounter = @splat(.{});
    /// Must-stay-0 by contract.
    pub var patch_edge_rmw_reads: counters.Tripwire = .init("patch_edge_rmw_reads");

    pub fn ineligibleTotal() u64 {
        var n: u64 = 0;
        for (&patch_ineligible) |*c| n += c.load();
        return n;
    }
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"patch_writes\":{d},\"patch_write_bytes\":{d},\"patch_dma_errors\":{d},\"patch_edge_rmw_reads\":{d}", .{ stats.patch_writes.load(), stats.patch_write_bytes.load(), stats.patch_dma_errors.load(), counters.tripwireCount("patch_edge_rmw_reads") });
    inline for (@typeInfo(Bucket).@"enum".field_names, 0..) |name, i| {
        try w.print(",\"patch_ineligible_{s}\":{d}", .{ name, stats.patch_ineligible[i].load() });
    }
    try w.writeAll("}");
}

fn decline(b: Bucket) bool {
    stats.patch_ineligible[@backingInt(b)].inc();
    return false;
}

// ---------------------------------------------------------------------------
// The request-shape half (before the block lock).
// ---------------------------------------------------------------------------

pub const Shape = struct {
    offset: u64,
    len: u64,
    block_size: u64,
    /// The file's existing size (the EXTENDING check: a grown i_size owes
    /// a meta commit).
    existing_size: u64,
    /// `prev_write_end == offset` (the per-inode word).
    stream_adjacent: bool,
    patch_max: u64,
};

/// FIRST failing predicate wins, exactly one bucket per write. `cap == 0 ∨
/// len == 0` ⇒ false silently (the A/B lever's control arm).
pub fn shapeEligible(s: Shape) bool {
    if (s.patch_max == 0 or s.len == 0) return false;
    if (s.stream_adjacent) return decline(.adjacent);
    const start_block = s.offset / s.block_size;
    const end_block = (s.offset + s.len - 1) / s.block_size;
    if (start_block != end_block or s.len > s.patch_max or s.offset + s.len > s.existing_size) return decline(.oversize);
    if (s.offset % PAGE != 0 or s.len % PAGE != 0) return decline(.unaligned);
    return true;
}

// ---------------------------------------------------------------------------
// The state half (under level 3).
// ---------------------------------------------------------------------------

/// Rung 6's durable half of predicate 4 on an armed set (`sole_owner_
/// verdict`): asked BEFORE the word is retired so a non-resident leaf read
/// never sits inside the unstable window. Absent ⇒ sole. The verdict ORDER
/// is the posture source's (`sym_planes/custody.zig` `soleOwnerVerdict`,
/// bound through `PostureSource.soleOwnerFn`): `non_holder` (a JOINED
/// appender holding no allocation lease for the block's data volume — the
/// patch would retire a lifetime that plane accounts) BEFORE
/// `foreign_custody` (the file's custody is held from its SLOT HOLDER,
/// whose tree this mount cannot read) BEFORE the durable probe (`count ==
/// 1 ∧ !shared` ⇒ `sole`; anything else, or a probe that cannot read —
/// the leak-safe direction — ⇒ `shared`). `tryPatch` declines each as its
/// own counted bucket and runs the RAM pair only on `sole`.
pub const SoleOwnerVerdict = enum { sole, shared, foreign_custody, non_holder };

pub const CustodyProbe = struct {
    ctx: ?*anyopaque,
    /// A RAM buffer, a staged entry or a staged extent record for the key,
    /// or a rewrite epoch binding the block — open OR with its save in
    /// flight: the key the patch would land in is the one the save displaces
    /// (lock-free probes — never the blocking-pool hop).
    ram_custody: *const fn (ctx: ?*anyopaque, ino: u64, block: u32) bool,
    /// An overlay record open on the block (KD-B4-4: same guard, no new fence).
    overlay_open: *const fn (ctx: ?*anyopaque, ino: u64, block: u32) bool,
    /// Clause 7: a shared custody range (inert on every shipped mount).
    range_shared: ?*const fn (ctx: ?*anyopaque, ino: u64, block: u32) bool = null,
    sole_owner: ?*const fn (ctx: ?*anyopaque, ino: u64, binding: Binding) SoleOwnerVerdict = null,
};

/// The in-place DMA stamps the fsync touched word (the publish-path stamp
/// never sees it — stamp HERE or the next fsync skips its barrier).
pub const TouchedStamp = struct {
    ctx: ?*anyopaque,
    stamp: *const fn (ctx: ?*anyopaque, ino: u64, ordinal: u32) void,
};

pub const Vehicle = union(enum) {
    slot: seams.HeldSource,
    bytes: []const u8,

    fn len(self: Vehicle) u64 {
        return switch (self) {
            .slot => |s| s.len,
            .bytes => |b| b.len,
        };
    }
};

pub const Request = struct {
    ino: u64,
    block: u32,
    /// Byte offset within the block (page-aligned; the shape half checked).
    rel: u32,
    vehicle: Vehicle,
};

pub const Deps = struct {
    device: seams.DeviceSeam,
    map: seams.MapSeam,
    vols: *const seams.Volumes,
    purge: seams.PurgeSeam,
    custody: CustodyProbe,
    touched: ?TouchedStamp = null,
};

pub const PatchError = error{ InputOutput, VolumeFailed, WriterGuardFenced, OutOfMemory, WorkerShutdown, NoSpaceLeft };

/// A patch whose fence pair passed: the destination the ONE DMA lands at
/// and what the postlude re-stabilizes. Travels with the held (3) guard;
/// the mapping cannot move while it is held, so the CQE side needs no
/// revalidation ladder (the il direct-drive lane holds it across its SQE).
pub const Begun = struct {
    mapping: Binding,
    vol_tag: u64,
    /// The allocator word the fence retired (`publishBlock` re-stabilizes it).
    idx: u64,
    /// `mapping.offset + rel`: the device byte the DMA lands at.
    dest: u64,
    ordinal: u32,
    ino: u64,
    block: u32,
    rel: u32,
};

/// The state half's verdict: begun (the fence pair passed, the word
/// retired) or the one bucket that declined.
pub const Classified = union(enum) {
    begun: Begun,
    declined: Bucket,
};

/// The state half under (3), up to and including the fence pair, UNCOUNTED:
/// the custody probes, the authoritative map lookup, the durable sole-owner
/// verdict, retire → SeqCst → re-check. W1 (`beginPatch`) counts the
/// bucket into its decision ledger; the W2 in-place fold (§J.1 landing 2)
/// reuses the same half for its own arm and counts its own declines. On
/// `.begun` the caller performs its DMAs at `dest` and MUST call
/// `finishInPlace` (or `abandonInPlace` when nothing was submitted).
pub fn beginInPlace(deps: Deps, ino: u64, block: u32, rel: u32) PatchError!Classified {
    const c = deps.custody;
    if (c.range_shared) |rs| if (rs(c.ctx, ino, block)) return .{ .declined = .range_shared };
    if (c.ram_custody(c.ctx, ino, block)) return .{ .declined = .overlay };
    if (c.overlay_open(c.ctx, ino, block)) return .{ .declined = .device_overlay };
    // Predicate 1: the mapping from the AUTHORITATIVE map under the held lock.
    const m = deps.map.lookup(ino, block) orelse return .{ .declined = .unmapped };
    const vol = deps.vols.byTag(m.vol_tag) orelse return .{ .declined = .unmapped };
    if (!vol.passthrough) return .{ .declined = .transform };
    if (!m.whole_block or m.damaged) return .{ .declined = .decorated };
    // Predicate 4, durable half FIRST (before the word is retired).
    if (c.sole_owner) |so| switch (so(c.ctx, ino, m)) {
        .sole => {},
        .shared => return .{ .declined = .shared },
        .foreign_custody => return .{ .declined = .foreign_custody },
        .non_holder => return .{ .declined = .posture },
    };
    const idx = vol.alloc.idxOf(m.offset);
    // The fence pair, patch side: retire → SeqCst → peek == 1 ∧ !SHARED.
    const sole = vol.alloc.beginPatchSoleOwner(idx) catch return error.VolumeFailed;
    if (!sole) {
        // Re-stabilize under the NEW generation (content unchanged); CoW fallback.
        vol.alloc.publishBlock(idx);
        return .{ .declined = .shared };
    }
    return .{ .begun = .{ .mapping = m, .vol_tag = m.vol_tag, .idx = idx, .dest = m.offset + rel, .ordinal = vol.ordinal, .ino = ino, .block = block, .rel = rel } };
}

/// W1's state half: `beginInPlace` with the decline counted into the
/// decision ledger. Null ⇒ one bucket counted, the caller falls through (a
/// failed re-check already re-stabilized the word). The caller then
/// performs ONE DMA at `dest` and MUST call `finishPatch` on BOTH exits.
pub fn beginPatch(deps: Deps, ino: u64, block: u32, rel: u32) PatchError!?Begun {
    return switch (try beginInPlace(deps, ino, block, rel)) {
        .begun => |b| b,
        .declined => |bucket| {
            _ = decline(bucket);
            return null;
        },
    };
}

/// A begun in-place write nothing was submitted for: re-stabilize the word
/// under a new generation (content unchanged, so no purge).
pub fn abandonInPlace(deps: Deps, b: Begun) void {
    if (deps.vols.byTag(b.vol_tag)) |vol| vol.alloc.publishBlock(b.idx);
}

/// The postlude on BOTH exits of an in-place DMA, for any arm (`arm`
/// names it in the loud line): publish (re-stabilize under the NEW
/// generation), purge the key from the read tiers — the window `[rel,
/// rel + len)` alone from the read cache's grains, the block from the
/// whole-block tiers —, then — exact ⇒ the fsync touched stamp; failed ⇒
/// loud, the error for exactly this write (the block's old bytes intact
/// outside the window).
pub fn finishInPlace(deps: Deps, b: Begun, len: u64, dma: seams.DmaError!void, arm: []const u8) PatchError!void {
    const vol = deps.vols.byTag(b.vol_tag) orelse return error.VolumeFailed;
    vol.alloc.publishBlock(b.idx);
    deps.purge.purgeRange(b.mapping, b.rel, len);
    dma catch |e| {
        if (deps.vols.admitsLine(.inplace_dma)) log.err("{s} ino {d} block {d} rel {d}: DMA failed ({s}) — EIO for exactly this write; the block is re-stabilized with its old bytes intact outside the window", .{ arm, b.ino, b.block, b.rel, @errorName(e) });
        return switch (e) {
            error.InputOutput => error.InputOutput,
            error.NoSpaceLeft => error.NoSpaceLeft,
            error.WorkerShutdown => error.WorkerShutdown,
            error.VolumeFailed => error.VolumeFailed,
            error.WriterGuardFenced => error.WriterGuardFenced,
            error.OutOfMemory => error.OutOfMemory,
        };
    };
    if (deps.touched) |t| t.stamp(t.ctx, b.ino, b.ordinal);
}

/// W1's postlude: `finishInPlace`, then the ledger (`patch_writes`,
/// `patch_write_bytes`; a failure counts `patch_dma_errors`).
pub fn finishPatch(deps: Deps, b: Begun, len: u64, dma: seams.DmaError!void) PatchError!void {
    finishInPlace(deps, b, len, dma, "W1 patch") catch |e| {
        stats.patch_dma_errors.inc();
        return e;
    };
    stats.patch_writes.inc();
    stats.patch_write_bytes.add(len);
}

/// Arm A under (3): `true` ⇒ patched and acked; `false` ⇒ one bucket, fall
/// through; an error fails THIS write (EIO), nothing acked/parked/lost, the
/// word re-stabilized. `beginPatch` → ONE DMA, two vehicles one contract →
/// `finishPatch`.
pub fn tryPatch(deps: Deps, io: Io, req: Request) PatchError!bool {
    _ = io;
    const b = (try beginPatch(deps, req.ino, req.block, req.rel)) orelse return false;
    const result: seams.DmaError!void = switch (req.vehicle) {
        .slot => |src| if (deps.device.writeFixed(b.vol_tag, b.dest, src)) |_| {} else |e| e,
        .bytes => |bytes| if (deps.device.write(b.vol_tag, b.dest, bytes)) |_| {} else |e| e,
    };
    try finishPatch(deps, b, req.vehicle.len(), result);
    return true;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const journal = @import("../meta/journal.zig");
const wt = @import("write_testing.zig");

test "the shape half: FIRST failing predicate wins, one bucket per write; cap 0 or an empty payload is silent" {
    const base: Shape = .{ .offset = 4096, .len = 4096, .block_size = wt.BS, .existing_size = 4 * wt.BS, .stream_adjacent = false, .patch_max = wt.BS / 8 };
    const before = stats.ineligibleTotal();
    const adj0 = stats.patch_ineligible[@backingInt(Bucket.adjacent)].load();
    const over0 = stats.patch_ineligible[@backingInt(Bucket.oversize)].load();
    const unal0 = stats.patch_ineligible[@backingInt(Bucket.unaligned)].load();
    try testing.expect(shapeEligible(base));
    try testing.expectEqual(before, stats.ineligibleTotal());
    var s = base;
    s.patch_max = 0;
    try testing.expect(!shapeEligible(s));
    try testing.expectEqual(before, stats.ineligibleTotal()); // silent
    s = base;
    s.stream_adjacent = true;
    s.len = 3; // would also be unaligned — adjacency wins
    try testing.expect(!shapeEligible(s));
    try testing.expectEqual(adj0 + 1, stats.patch_ineligible[@backingInt(Bucket.adjacent)].load());
    s = base;
    s.len = wt.BS / 8 + PAGE; // over the cap
    try testing.expect(!shapeEligible(s));
    s = base;
    s.offset = wt.BS - PAGE;
    s.len = 2 * PAGE; // spans two blocks
    try testing.expect(!shapeEligible(s));
    s = base;
    s.existing_size = 4096 + 2048; // extends
    try testing.expect(!shapeEligible(s));
    try testing.expectEqual(over0 + 3, stats.patch_ineligible[@backingInt(Bucket.oversize)].load());
    s = base;
    s.offset = 4096 + 512;
    try testing.expect(!shapeEligible(s));
    s = base;
    s.len = 4096 + 512;
    try testing.expect(!shapeEligible(s));
    try testing.expectEqual(unal0 + 2, stats.patch_ineligible[@backingInt(Bucket.unaligned)].load());
    try testing.expectEqual(before + 6, stats.ineligibleTotal());
    try testing.expectEqual(@as(u64, 512 * 1024), patchMaxDerived(4 << 20));
    try testing.expectEqual(@as(u64, 7), resolvePatchMax(7, 4 << 20));
}

const Probe = struct {
    ram: bool = false,
    overlay: bool = false,
    verdict: SoleOwnerVerdict = .sole,
    stamps: u32 = 0,

    fn ramCustody(ctx: ?*anyopaque, _: u64, _: u32) bool {
        const p: *Probe = @ptrCast(@alignCast(ctx.?));
        return p.ram;
    }
    fn overlayOpen(ctx: ?*anyopaque, _: u64, _: u32) bool {
        const p: *Probe = @ptrCast(@alignCast(ctx.?));
        return p.overlay;
    }
    fn soleOwner(ctx: ?*anyopaque, _: u64, _: Binding) SoleOwnerVerdict {
        const p: *Probe = @ptrCast(@alignCast(ctx.?));
        return p.verdict;
    }
    fn stamp(ctx: ?*anyopaque, _: u64, _: u32) void {
        const p: *Probe = @ptrCast(@alignCast(ctx.?));
        p.stamps += 1;
    }
    fn custody(self: *Probe) CustodyProbe {
        return .{ .ctx = self, .ram_custody = &ramCustody, .overlay_open = &overlayOpen, .sole_owner = &soleOwner };
    }
    fn touched(self: *Probe) TouchedStamp {
        return .{ .ctx = self, .stamp = &stamp };
    }
};

fn mkDeps(f: *wt.Fixture, p: *Probe) Deps {
    return .{ .device = f.dev.seam(), .map = f.map.seam(), .vols = &f.vols, .purge = .none, .custody = p.custody(), .touched = p.touched() };
}

test "patch_byte_exactness_on_fresh_striped_file: ONE aligned DMA at offset + rel, only the window changes, zero allocation/free/merge, the word stays stable, the touched stamp fires, patch_edge_rmw_reads = 0" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 8);
    defer f.deinit();
    var p: Probe = .{};
    const b = try f.publishBlock(1, 0, 0xAA);
    var payload: [2 * PAGE]u8 align(4096) = @splat(0x55);
    const used = f.alloc_a.usedBlocks();
    const pw0 = stats.patch_writes.load();
    const pb0 = stats.patch_write_bytes.load();
    const rmw0 = counters.tripwireCount("patch_edge_rmw_reads");
    const idx = f.alloc_a.idxOf(b.offset);
    const word_before = f.alloc_a.incarnationWord(idx);
    try testing.expect(try tryPatch(mkDeps(f, &p), io, .{ .ino = 1, .block = 0, .rel = 3 * PAGE, .vehicle = .{ .bytes = &payload } }));
    try testing.expectEqual(@as(usize, 1), f.dev.writes.items.len);
    try testing.expectEqual(b.offset + 3 * PAGE, f.dev.writes.items[0].off);
    try testing.expectEqual(@as(u64, 2 * PAGE), f.dev.writes.items[0].len);
    try testing.expectEqual(@as(u64, 0), f.dev.reads); // no edge RMW read
    try testing.expectEqual(used, f.alloc_a.usedBlocks());
    try testing.expectEqual(@as(usize, 0), f.map.merges.items.len);
    try testing.expectEqual(@as(usize, 0), f.frees.freed.items.len);
    var got: [wt.BS]u8 = undefined;
    f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. 3 * PAGE], 0xAA));
    try testing.expect(std.mem.allEqual(u8, got[3 * PAGE .. 5 * PAGE], 0x55));
    try testing.expect(std.mem.allEqual(u8, got[5 * PAGE ..], 0xAA));
    // The word: a new generation, stable again.
    const word_after = f.alloc_a.incarnationWord(idx);
    try testing.expect(block_alloc.Incarnation.isStable(word_after));
    try testing.expectEqual(block_alloc.Incarnation.generation(word_before) + 1, block_alloc.Incarnation.generation(word_after));
    try testing.expectEqual(@as(u32, 1), p.stamps);
    try testing.expectEqual(pw0 + 1, stats.patch_writes.load());
    try testing.expectEqual(pb0 + 2 * PAGE, stats.patch_write_bytes.load());
    try testing.expectEqual(rmw0, counters.tripwireCount("patch_edge_rmw_reads"));
    // The slot vehicle: WRITE_FIXED straight from the held slot.
    var held: [PAGE]u8 align(4096) = @splat(0x77);
    try f.dev.registerSlot(3, &held);
    try testing.expect(try tryPatch(mkDeps(f, &p), io, .{ .ino = 1, .block = 0, .rel = 0, .vehicle = .{ .slot = .{ .qid = 0, .buf_index = 3, .len = PAGE } } }));
    try testing.expect(f.dev.writes.items[1].fixed);
    f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0..PAGE], 0x77));
}

test "exclusions under the lock, each its own bucket: RAM/staged custody, an open device overlay, a transformed volume, a hole, a decorated mapping, a shared refcount after a clone (the fence pair backs off and re-stabilizes), the durable verdicts" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 8);
    defer f.deinit();
    var p: Probe = .{};
    const b = try f.publishBlock(2, 0, 0xAA);
    var payload: [PAGE]u8 align(4096) = @splat(1);
    const req: Request = .{ .ino = 2, .block = 0, .rel = 0, .vehicle = .{ .bytes = &payload } };
    const patched0 = stats.patch_writes.load();
    const total0 = stats.ineligibleTotal();
    var b0: [Bucket.count]u64 = undefined;
    for (&b0, 0..) |*c, i| c.* = stats.patch_ineligible[i].load();
    p.ram = true;
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, req));
    try testing.expectEqual(b0[@backingInt(Bucket.overlay)] + 1, stats.patch_ineligible[@backingInt(Bucket.overlay)].load());
    p.ram = false;
    p.overlay = true;
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, req));
    try testing.expectEqual(b0[@backingInt(Bucket.device_overlay)] + 1, stats.patch_ineligible[@backingInt(Bucket.device_overlay)].load());
    p.overlay = false;
    // Hole.
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, .{ .ino = 2, .block = 5, .rel = 0, .vehicle = .{ .bytes = &payload } }));
    try testing.expectEqual(b0[@backingInt(Bucket.unmapped)] + 1, stats.patch_ineligible[@backingInt(Bucket.unmapped)].load());
    // Transformed volume (B).
    try f.map.bind(2, 1, Binding.wholeBlock(wt.VOL_B, 0, 1));
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, .{ .ino = 2, .block = 1, .rel = 0, .vehicle = .{ .bytes = &payload } }));
    try testing.expectEqual(b0[@backingInt(Bucket.transform)] + 1, stats.patch_ineligible[@backingInt(Bucket.transform)].load());
    // Decorated (a tenant window) never scribbles outside its window: declines.
    try f.map.bind(2, 2, Binding.tenant(wt.VOL_A, b.offset, b.incarnation, 0, 100));
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, .{ .ino = 2, .block = 2, .rel = 0, .vehicle = .{ .bytes = &payload } }));
    try testing.expectEqual(b0[@backingInt(Bucket.decorated)] + 1, stats.patch_ineligible[@backingInt(Bucket.decorated)].load());
    // Shared after a clone: the pin raises the refcount; the pair backs off, re-stabilizes, CoW fallback.
    const idx = f.alloc_a.idxOf(b.offset);
    try testing.expectEqual(block_alloc.PinVerdict.pinned, f.alloc_a.pinBlockValidated(idx));
    const gen_before = block_alloc.Incarnation.generation(f.alloc_a.incarnationWord(idx));
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, req));
    try testing.expectEqual(b0[@backingInt(Bucket.shared)] + 1, stats.patch_ineligible[@backingInt(Bucket.shared)].load());
    try testing.expect(block_alloc.Incarnation.isStable(f.alloc_a.incarnationWord(idx)));
    try testing.expectEqual(gen_before + 1, block_alloc.Incarnation.generation(f.alloc_a.incarnationWord(idx)));
    try testing.expectEqual(@as(usize, 0), f.dev.writes.items.len);
    f.alloc_a.unpin(idx);
    // The SHARED mark (a clone's durable bit) declines likewise.
    f.alloc_a.markShared(idx);
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, req));
    try testing.expectEqual(b0[@backingInt(Bucket.shared)] + 2, stats.patch_ineligible[@backingInt(Bucket.shared)].load());
    f.alloc_a.clearShared(idx);
    // The durable verdicts.
    p.verdict = .foreign_custody;
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, req));
    p.verdict = .non_holder;
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, req));
    p.verdict = .shared;
    try testing.expect(!try tryPatch(mkDeps(f, &p), io, req));
    try testing.expectEqual(b0[@backingInt(Bucket.foreign_custody)] + 1, stats.patch_ineligible[@backingInt(Bucket.foreign_custody)].load());
    try testing.expectEqual(b0[@backingInt(Bucket.posture)] + 1, stats.patch_ineligible[@backingInt(Bucket.posture)].load());
    try testing.expectEqual(b0[@backingInt(Bucket.shared)] + 3, stats.patch_ineligible[@backingInt(Bucket.shared)].load());
    p.verdict = .sole;
    // The ledger identity: every invocation resolved to exactly one of {patched, one bucket}.
    try testing.expectEqual(@as(u64, 10), (stats.patch_writes.load() - patched0) + (stats.ineligibleTotal() - total0));
    try testing.expect(try tryPatch(mkDeps(f, &p), io, req));
    try testing.expectEqual(@as(u64, 11), (stats.patch_writes.load() - patched0) + (stats.ineligibleTotal() - total0));
}

test "patch_dma_error_returns_eio_and_restabilizes: the word is published back under the new generation, the error is this write's alone, bytes outside the window are intact" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 8);
    defer f.deinit();
    var p: Probe = .{};
    const b = try f.publishBlock(3, 0, 0xAA);
    var payload: [PAGE]u8 align(4096) = @splat(1);
    f.dev.fail_next = error.InputOutput;
    const errs0 = stats.patch_dma_errors.load();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.InputOutput, tryPatch(mkDeps(f, &p), io, .{ .ino = 3, .block = 0, .rel = PAGE, .vehicle = .{ .bytes = &payload } }));
    try log.testing.expectAllFired();
    try testing.expectEqual(errs0 + 1, stats.patch_dma_errors.load());
    const idx = f.alloc_a.idxOf(b.offset);
    try testing.expect(block_alloc.Incarnation.isStable(f.alloc_a.incarnationWord(idx)));
    var got: [wt.BS]u8 = undefined;
    f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0..PAGE], 0xAA));
    try testing.expect(std.mem.allEqual(u8, got[2 * PAGE ..], 0xAA));
    try testing.expectEqual(@as(u32, 0), p.stamps); // a failed patch stamps nothing
}

test "a FAILED volume's in-place DMA failures: N failed patches each answer EIO and are counted (patch_dma_errors), and the set's law admits ONE line for all N" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 8);
    defer f.deinit();
    var latch: journal.FailureLatch = .{};
    f.vols.lines = latch.lineSeam();
    defer f.vols.lines = null;
    try testing.expect(latch.failStop());
    var p: Probe = .{};
    _ = try f.publishBlock(3, 0, 0xAA);
    var payload: [PAGE]u8 align(4096) = @splat(1);
    const errs0 = stats.patch_dma_errors.load();
    const quieted0 = journal.meta_failed_volume_lines_quieted.load();
    const n_ops = 24;
    log.testing.expectErrorLogs(1);
    for (0..n_ops) |_| {
        f.dev.fail_next = error.InputOutput;
        try testing.expectError(error.InputOutput, tryPatch(mkDeps(f, &p), io, .{ .ino = 3, .block = 0, .rel = PAGE, .vehicle = .{ .bytes = &payload } }));
    }
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(usize, 1), log.testing.capturedErrorLogs());
    try testing.expectEqual(errs0 + n_ops, stats.patch_dma_errors.load());
    try testing.expectEqual(quieted0 + n_ops - 1, journal.meta_failed_volume_lines_quieted.load());
}

test "clone_cfr_vs_patch_storm (composed with block_alloc's pair): a clone pinning under a patch storm never observes an unstable snapshot as valid; the refcount is conserved; the model with either fence weakened fails" {
    const io = testing.io;
    const f = try wt.Fixture.init(testing.allocator, 8);
    defer f.deinit();
    var p: Probe = .{};
    const b = try f.publishBlock(4, 0, 0xAA);
    const idx = f.alloc_a.idxOf(b.offset);
    const Storm = struct {
        fn patcher(io_: Io, fx: *wt.Fixture, pr: *Probe) void {
            var payload: [PAGE]u8 align(4096) = @splat(0x11);
            var i: u32 = 0;
            while (i < 200) : (i += 1) {
                _ = tryPatch(mkDeps(fx, pr), io_, .{ .ino = 4, .block = 0, .rel = 0, .vehicle = .{ .bytes = &payload } }) catch unreachable;
            }
        }
        fn cloner(alloc: *block_alloc.BlockAllocator, i: u64, bad: *std.atomic.Value(u32)) void {
            var n: u32 = 0;
            while (n < 200) : (n += 1) {
                switch (alloc.pinBlockRetrying(i)) {
                    .pinned => {
                        // A validated pin holds the count at ≥ 2 for its whole hold (conservation).
                        if ((alloc.refcount(i) orelse 0) < 2) _ = bad.fetchAdd(1, .monotonic);
                        alloc.unpin(i);
                    },
                    .pinned_unstable => {}, // three attempts met a mid-flight patch (already unpinned inside): the CoW fallback
                    .refused => unreachable,
                }
            }
        }
    };
    var bad: std.atomic.Value(u32) = .init(0);
    const t1 = try std.Thread.spawn(.{}, Storm.patcher, .{ io, f, &p });
    const t2 = try std.Thread.spawn(.{}, Storm.cloner, .{ f.alloc_a, idx, &bad });
    t1.join();
    t2.join();
    try testing.expectEqual(@as(u32, 0), bad.load(.monotonic));
    try testing.expectEqual(@as(?u32, 1), f.alloc_a.refcount(idx));
    try testing.expect(block_alloc.Incarnation.isStable(f.alloc_a.incarnationWord(idx)));
    // The proof: the composed pair model holds and EITHER fence weakened admits the stale clone.
    const explorer = @import("../model/explorer.zig");
    try explorer.expectHolds(incarnation_model.PatchCloneModel(.none), .{});
    try explorer.expectFails(incarnation_model.PatchCloneModel(.patch_fence), .{}, error.InvariantViolated);
    try explorer.expectFails(incarnation_model.PatchCloneModel(.clone_fence), .{}, error.InvariantViolated);
}
