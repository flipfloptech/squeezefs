//! The LAYOUT LEASE EPOCHS (charter §7 "The direct arm", decision 0003 §AX):
//! one monotonic word per inode stripe, bumped by the daemon at every event
//! after which a client's cached layout of that inode may name the wrong
//! bytes — a committed KV change of the inode's record or xattrs (its
//! layout, its size: the conveyor's post-apply, pre-ack hook, every origin)
//! and the admission of a write on the inode (RAM custody the device does
//! not hold yet). A direct client holds `{layout, epoch at grant}` and
//! answers an I/O only while the stripe's word still reads that epoch AFTER
//! the bytes are in hand — the FUSE direct leg's "validation after the DMA"
//! (charter §6.3), linearized at the client's load.
//!
//! The rows live in a page the daemon shares read-only with every direct
//! session (`ipc/host.zig`: a sealed memfd mapped here at arm; the private
//! rows carry the count until then, and their values move over so the word
//! never runs backwards). Stripes, not inodes: a colliding publish costs a
//! spurious re-ask, never a stale serve. The same mix as the shape table
//! (`splitmix64(ino) & mask`), so an inode's two tables share a stripe.
//!
//! Orderings: the bump is a `seq_cst` RMW — it must not pass the RAM apply
//! before it (the plane's lookup then reads the new map before any client
//! sees the new epoch) — and the client's load is `acquire` after its DMA
//! completed (the kernel's completion is the earlier barrier).

const std = @import("std");
const Atomic = std.atomic.Value;

const dlm = @import("../meta/dlm.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");

/// One row per op-guard stripe (the shape table's width).
pub const STRIPES: usize = op_guard.INODE_GUARD_STRIPES;
pub const PAGE_BYTES: usize = STRIPES * @sizeOf(u64);

pub const Row = Atomic(u64);

var private_rows: [STRIPES]Row = @splat(.init(0));
/// The rows in force: the private table until a host shares a page.
var rows: Atomic([*]Row) = .init(&private_rows);

pub fn stripeOf(ino: u64) usize {
    return @intCast(dlm.splitmix64(ino) & (STRIPES - 1));
}

/// The row's current epoch.
pub fn load(ino: u64) u64 {
    return rows.load(.acquire)[stripeOf(ino)].load(.acquire);
}

/// A layout- or custody-changing event on `ino` landed.
pub fn bump(ino: u64) void {
    _ = rows.load(.acquire)[stripeOf(ino)].fetchAdd(1, .seq_cst);
}

/// The host's page takes over: every private count moves (monotonic), then
/// the pointer flips. A bump racing the flip lands on a table no client has
/// read yet.
pub fn share(page: [*]Row) void {
    for (0..STRIPES) |i| {
        const v = private_rows[i].load(.acquire);
        _ = page[i].fetchMax(v, .acq_rel);
    }
    rows.store(page, .release);
}

/// Back to the private rows (the host disarmed; the page is about to go).
pub fn unshare() void {
    const p = rows.swap(&private_rows, .acq_rel);
    if (p == @as([*]Row, &private_rows)) return;
    for (0..STRIPES) |i| _ = private_rows[i].fetchMax(p[i].load(.acquire), .acq_rel);
}

/// A client's view over a shared page: the stripe's epoch.
pub fn loadFrom(page: [*]const Row, ino: u64) u64 {
    return page[stripeOf(ino)].load(.acquire);
}

// ---------------------------------------------------------------------------
// WRITE leases (decision 0003 §AY): the inode stripes that hold one (the
// recall's fast "none" check), the block-key stripes under one (the read
// path's deposit gate: no tier may shadow a block a client writes in place),
// and the recall hook the host installs.
// ---------------------------------------------------------------------------

/// Block-key stripes: 16× the inode stripes, so a 3 GiB file's 768 leased
/// blocks blind a sixth of the deposit gate, not all of it.
pub const KEY_STRIPES: usize = STRIPES * 16;

/// A leased block's key.
pub const Key = struct { vol_tag: u64, offset: u64 };

var write_leased_inos: [STRIPES]Atomic(u32) = @splat(.init(0));
var write_leased_keys: [KEY_STRIPES]Atomic(u32) = @splat(.init(0));

pub const RecallHook = struct {
    ctx: *anyopaque,
    /// Recall every write lease of `ino` and return once no client write
    /// is in flight under any of them (bounded; decision 0003 §AY).
    recall: *const fn (ctx: *anyopaque, ino: u64) void,
};

var recall_hook: Atomic(?*const RecallHook) = .init(null);

pub fn armRecall(hook: *const RecallHook) void {
    recall_hook.store(hook, .release);
}

pub fn disarmRecall() void {
    recall_hook.store(null, .release);
}

pub fn keyStripeOf(vol_tag: u64, offset: u64) usize {
    return @intCast(dlm.splitmix64(vol_tag ^ dlm.splitmix64(offset)) & (KEY_STRIPES - 1));
}

/// A write lease on `ino` over `keys` begins: the stripes count it.
pub fn noteWriteLease(ino: u64, keys: []const Key) void {
    _ = write_leased_inos[stripeOf(ino)].fetchAdd(1, .acq_rel);
    for (keys) |k| _ = write_leased_keys[keyStripeOf(k.vol_tag, k.offset)].fetchAdd(1, .acq_rel);
}

pub fn dropWriteLease(ino: u64, keys: []const Key) void {
    _ = write_leased_inos[stripeOf(ino)].fetchSub(1, .acq_rel);
    for (keys) |k| _ = write_leased_keys[keyStripeOf(k.vol_tag, k.offset)].fetchSub(1, .acq_rel);
}

/// Some write lease stands on `ino`'s stripe (conservative: a collision
/// reads leased).
pub fn inoWriteLeased(ino: u64) bool {
    return write_leased_inos[stripeOf(ino)].load(.acquire) != 0;
}

/// The deposit gate: a block on a leased key stripe stays out of every tier.
pub fn keyWriteLeased(vol_tag: u64, offset: u64) bool {
    return write_leased_keys[keyStripeOf(vol_tag, offset)].load(.acquire) != 0;
}

/// The mutation chokepoints' face: nothing when no write lease stands on
/// the stripe (one load); else the host's recall, which returns once every
/// client write under the inode's leases has drained.
pub fn recallWrites(ino: u64) void {
    if (!inoWriteLeased(ino)) return;
    const h = recall_hook.load(.acquire) orelse return;
    h.recall(h.ctx, ino);
}

const testing = std.testing;

test "write leases: the inode stripe and the key stripes count a lease in and out; the recall face is silent without a lease and calls the hook with one" {
    const ino: u64 = 0x9a9a;
    const keys = [_]Key{ .{ .vol_tag = 1, .offset = 4 << 20 }, .{ .vol_tag = 1, .offset = 8 << 20 } };
    const Probe = struct {
        var calls: u32 = 0;
        var last: u64 = 0;
        fn recall(_: *anyopaque, i: u64) void {
            calls += 1;
            last = i;
        }
    };
    var sink: u8 = 0;
    const hook: RecallHook = .{ .ctx = &sink, .recall = &Probe.recall };
    armRecall(&hook);
    defer disarmRecall();
    try testing.expect(!inoWriteLeased(ino));
    recallWrites(ino);
    try testing.expectEqual(@as(u32, 0), Probe.calls);
    noteWriteLease(ino, &keys);
    try testing.expect(inoWriteLeased(ino));
    try testing.expect(keyWriteLeased(1, 4 << 20));
    try testing.expect(keyWriteLeased(1, 8 << 20));
    recallWrites(ino);
    try testing.expectEqual(@as(u32, 1), Probe.calls);
    try testing.expectEqual(ino, Probe.last);
    dropWriteLease(ino, &keys);
    try testing.expect(!inoWriteLeased(ino));
    try testing.expect(!keyWriteLeased(1, 4 << 20));
    recallWrites(ino);
    try testing.expectEqual(@as(u32, 1), Probe.calls);
}

test "the lease epoch: a bump moves exactly its stripe; a shared page inherits the private counts and takes every later bump; unshare folds the page back" {
    const ino: u64 = 0x5151;
    const other: u64 = blk: {
        var i: u64 = 1;
        while (stripeOf(i) == stripeOf(ino)) i += 1;
        break :blk i;
    };
    const e0 = load(ino);
    const o0 = load(other);
    bump(ino);
    try testing.expectEqual(e0 + 1, load(ino));
    try testing.expectEqual(o0, load(other));
    var page: [STRIPES]Row = @splat(.init(0));
    share(&page);
    defer unshare();
    try testing.expectEqual(e0 + 1, loadFrom(&page, ino));
    bump(ino);
    try testing.expectEqual(e0 + 2, loadFrom(&page, ino));
    try testing.expectEqual(e0 + 2, load(ino));
    try testing.expectEqual(stripeOf(ino), stripeOf(ino));
}
