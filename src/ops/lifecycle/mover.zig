//! THE ONE MOVER primitive — `moveOne` (charter §8 "drain = copy-then-
//! republish via ONE mover primitive (a shared block moves ONCE;
//! destination refcount raised before publish; source freed after durable
//! publish + tier purge)"; spec §2.5.3), in NINE steps over a `Seam` the
//! plane binds (`plane.republishBinding`, `pinBlockValidated`, the mint
//! primitives, `readMappingImage`, `freeBindings`) and a RAM world binds
//! for the kill matrix on every host:
//!
//!   1. OPEN-PACK gate: an open pack defers; on the DRAIN's victim it is
//!      SEALED so the next re-plan moves it.
//!   2. QUIESCENT-FIRST: every referencer passes the probe or the task defers.
//!   3. PIN the source (`pinned` ⇒ proceed; `pinned_unstable` ⇒ defer;
//!      `refused` tracked ⇒ superseded (freed concurrently — census
//!      stale); `refused` untracked ⇒ pin-less).
//!   4. COPY the STORED image verbatim (transform-opaque); `src_hash`. A
//!      seam with a REWRITE hook (the key-rotation sweep, `ops/defrag/
//!      rekey.zig`) transforms the copy in RAM before the hash — the
//!      destination lands the rewritten image, the verify-read compares
//!      against it; a hook answering "nothing to rewrite" ends the move
//!      as superseded before any mint.
//!   5. DESTINATION per `DestPick`: `balance` (the hint if eligible else
//!      the lowest-fill survivor), `compact_low` (`allocateBlockBelow`,
//!      no gap ⇒ superseded this pass), `backend_ascending` (`AtOrAbove`
//!      then `floor = idx + 1`) — UNPUBLISHED, registered in flight.
//!   6. WRITE + VERIFY-READ against `src_hash`; any failure ⇒ abandon the
//!      never-published mint + unpin + deferred; a mismatch is an error
//!      log ("destination freed, source untouched"). Then publish the
//!      destination's incarnation word.
//!   7. PRE-PUBLISH REFCOUNT TRANSFER: raise the destination to the FULL
//!      referencer count (`refs − 1` increments; the mint's own +1 is the
//!      first) BEFORE any referencer publishes.
//!   8. PUBLISH per referencer, ascending ino, under the layout lock with
//!      the expected-binding compare: `Superseded` (a foreground write
//!      replaced the mapping / a truncate pruned it) is the contractual
//!      no-op — release one destination ref; the decoration suffix
//!      (`rel_off/len`) is preserved VERBATIM so a whole-block mapping
//!      stays whole-block and a packed tenant stays packed.
//!   9. UNPIN the source: every published referencer already released one
//!      source reference; the pin is the LAST ⇒ the terminal free punches
//!      through the ordinary ladder — never a direct free.
//!
//! Outcome: `published > 0` ⇒ moved; else superseded (every destination
//! reference released — the terminal free punched the destination).

const std = @import("std");
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const checksum = @import("../../core/checksum.zig");
const layout = @import("../../data/layout.zig");
const census = @import("census.zig");
const stats_mod = @import("stats.zig");

const stats = stats_mod.stats;
pub const Mapping = layout.Mapping;
pub const Task = census.Task;
pub const Ref = census.Ref;

pub const Outcome = enum { moved, deferred, superseded };

pub const PinVerdict = enum { pinned, pinned_unstable, refused };

/// How the destination is picked.
pub const DestPick = union(enum) {
    /// The drain / rebalance: the hint if still eligible, else the
    /// lowest-fill placement-eligible survivor excluding the source.
    balance: struct { dest_hint: u64 },
    /// D1 compaction: the lowest same-backend gap strictly below the source.
    compact_low,
    /// D2 locality: ascending on the named backend from `floor.*`.
    backend_ascending: struct { vol_tag: u64, floor: *u64 },
};

/// A minted, UNPUBLISHED destination.
pub const Minted = struct {
    vol_tag: u64,
    idx: u64,
    offset: u64,
    /// The incarnation the published mapping carries.
    incarnation: u64,
};

/// The step a kill seam names (the RAM world's crash points; the plane's
/// binding has none).
pub const KillPoint = enum(u8) {
    after_seal = 1,
    after_pin = 3,
    after_copy = 4,
    after_mint = 5,
    after_write = 6,
    after_raise = 7,
    mid_publish = 8,
    after_publish = 9,
    after_unpin = 10,
};

pub const SeamError = error{ Io, OutOfMemory, Killed };
pub const RepublishError = error{ Superseded, Io, OutOfMemory, Killed };

/// The plane's face. Every call may return `Killed` (the RAM world's
/// crash seam); the plane never does.
pub const Seam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        chunk: *const fn (ctx: *anyopaque) u64,
        isOpenPack: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) bool,
        sealPack: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) void,
        quiescent: *const fn (ctx: *anyopaque, ino: u64, block_index: u32) bool,
        pin: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) PinVerdict,
        /// The RAM refcount cell (null = untracked).
        refcount: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) ?u32,
        unpin: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) void,
        /// The STORED image of the whole block into `out`; the bytes read.
        readImage: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64, out: []u8) SeamError!usize,
        /// Transform the copied image IN PLACE before it is hashed and
        /// written (`image[0..len]` is what `readImage` returned; the
        /// buffer is a whole chunk): the new length, or null = the block
        /// needs no rewrite (the move ends superseded, nothing minted).
        /// Null hook = the verbatim copy of every other mover.
        rewrite: ?*const fn (ctx: *anyopaque, task: Task, image: []u8, len: usize) SeamError!?usize = null,
        allocate: *const fn (ctx: *anyopaque, pick: DestPick, src_vol: u64, src_idx: u64) SeamError!?Minted,
        abandonUnpublished: *const fn (ctx: *anyopaque, m: Minted) void,
        write: *const fn (ctx: *anyopaque, m: Minted, image: []const u8) SeamError!void,
        readBack: *const fn (ctx: *anyopaque, m: Minted, out: []u8) SeamError!usize,
        publishBlock: *const fn (ctx: *anyopaque, m: Minted) void,
        incrementRefcount: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) bool,
        /// Release one RAM reference (a terminal release frees through the ladder).
        releaseRef: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64) void,
        /// The republish under the layout lock: `lookup(block_index) ==
        /// expected` (sameRef + the window) else `Superseded`; stages the
        /// swapped entry + `+dst −src` refs; commits.
        republish: *const fn (ctx: *anyopaque, ino: u64, block_index: u32, expected: Mapping, new: Mapping) RepublishError!void,
        /// The displaced source binding's release through the ladder,
        /// AFTER the tx landed.
        freeBinding: *const fn (ctx: *anyopaque, old: Mapping) void,
        kill: *const fn (ctx: *anyopaque, at: KillPoint) SeamError!void,
    };
};

pub const MoveError = error{ Io, OutOfMemory, Killed };

/// `drain_victim`: the volume being drained (its open pack is sealed
/// here), else 0.
pub fn moveOne(seam: Seam, gpa: Allocator, task: Task, pick: DestPick, drain_victim: u64) MoveError!Outcome {
    const vt = seam.vtable;
    const ctx = seam.ctx;
    const src_vol = task.vol_tag;
    const src_idx = task.block_idx;

    // 1. The open-pack gate.
    if (vt.isOpenPack(ctx, src_vol, src_idx)) {
        if (src_vol == drain_victim) vt.sealPack(ctx, src_vol, src_idx);
        try vt.kill(ctx, .after_seal);
        stats.pack_mover_open_defers.inc();
        return .deferred;
    }
    // 2. Quiescent-first.
    for (task.refs) |r| if (!vt.quiescent(ctx, r.ino, r.block_index)) {
        stats.evacuate_deferred_staged_blocks.inc();
        return .deferred;
    };
    // 3. The pin.
    var pinned = false;
    switch (vt.pin(ctx, src_vol, src_idx)) {
        .pinned => pinned = true,
        .pinned_unstable => {
            vt.unpin(ctx, src_vol, src_idx);
            return .deferred;
        },
        .refused => if (vt.refcount(ctx, src_vol, src_idx) != null) return .superseded,
    }
    try vt.kill(ctx, .after_pin);
    return moveAfterPin(seam, gpa, task, pick, &pinned) catch |e| {
        // A crash leaves no RAM to unpin (the remount re-seeds); a device
        // error does.
        if (e != error.Killed and pinned) vt.unpin(ctx, src_vol, src_idx);
        return e;
    };
}

/// Steps 4–9, with `pinned` cleared as soon as the source is unpinned.
fn moveAfterPin(seam: Seam, gpa: Allocator, task: Task, pick: DestPick, pinned_ptr: *bool) MoveError!Outcome {
    const vt = seam.vtable;
    const ctx = seam.ctx;
    const chunk = vt.chunk(ctx);
    const src_vol = task.vol_tag;
    const src_idx = task.block_idx;
    var pinned = pinned_ptr.*;
    defer pinned_ptr.* = pinned;
    // 4. The copy (the stored image, transform-opaque).
    const image = try gpa.alloc(u8, chunk);
    defer gpa.free(image);
    var n = try vt.readImage(ctx, src_vol, src_idx, image);
    if (vt.rewrite) |rw| {
        n = (try rw(ctx, task, image, n)) orelse {
            if (pinned) vt.unpin(ctx, src_vol, src_idx);
            pinned = false;
            return .superseded;
        };
    }
    const src_hash = checksum.hash64(image[0..n]);
    stats.evacuate_inflight_bytes.add(n);
    defer stats.evacuate_inflight_bytes.sub(n);
    try vt.kill(ctx, .after_copy);
    // 5. The destination, UNPUBLISHED.
    const dst = (try vt.allocate(ctx, pick, src_vol, src_idx)) orelse {
        if (pinned) vt.unpin(ctx, src_vol, src_idx);
        pinned = false;
        return .superseded;
    };
    try vt.kill(ctx, .after_mint);
    // 6. Write + verify-read, then publish the incarnation word.
    const landed = blk: {
        vt.write(ctx, dst, image[0..n]) catch break :blk false;
        const back = try gpa.alloc(u8, chunk);
        defer gpa.free(back);
        const m = vt.readBack(ctx, dst, back) catch break :blk false;
        if (m != n or checksum.hash64(back[0..m]) != src_hash) {
            log.err("mover: vol {x} block {d} → vol {x} block {d}: the verify-read mismatched the source image — destination freed, source untouched", .{ src_vol, src_idx, dst.vol_tag, dst.idx });
            break :blk false;
        }
        break :blk true;
    };
    if (!landed) {
        vt.abandonUnpublished(ctx, dst);
        if (pinned) vt.unpin(ctx, src_vol, src_idx);
        pinned = false;
        return .deferred;
    }
    try vt.kill(ctx, .after_write);
    vt.publishBlock(ctx, dst);
    // 7. The pre-publish refcount transfer: the mint's own +1 is the first.
    var raised: u32 = 1;
    while (raised < task.refs.len) : (raised += 1) {
        if (!vt.incrementRefcount(ctx, dst.vol_tag, dst.idx)) {
            // A refused raise frees the destination and defers.
            while (raised > 0) : (raised -= 1) vt.releaseRef(ctx, dst.vol_tag, dst.idx);
            if (pinned) vt.unpin(ctx, src_vol, src_idx);
            pinned = false;
            return .deferred;
        }
    }
    try vt.kill(ctx, .after_raise);
    // 8. Publish per referencer, ascending ino, under the layout lock.
    var published: u32 = 0;
    var remaining_dst_refs: u32 = raised;
    for (task.refs, 0..) |r, i| {
        var new = r.mapping;
        new.vol_tag = dst.vol_tag;
        new.offset = dst.offset;
        new.incarnation = dst.incarnation;
        if (i == 1) try vt.kill(ctx, .mid_publish);
        if (!vt.quiescent(ctx, r.ino, r.block_index)) {
            vt.releaseRef(ctx, dst.vol_tag, dst.idx);
            remaining_dst_refs -= 1;
            continue;
        }
        vt.republish(ctx, r.ino, r.block_index, r.mapping, new) catch |e| switch (e) {
            error.Superseded => {
                stats.evacuate_stale_token_noops.inc();
                vt.releaseRef(ctx, dst.vol_tag, dst.idx);
                remaining_dst_refs -= 1;
                continue;
            },
            error.Killed => return error.Killed,
            else => {
                vt.releaseRef(ctx, dst.vol_tag, dst.idx);
                remaining_dst_refs -= 1;
                continue;
            },
        };
        // The displaced source reference frees AFTER the tx landed.
        vt.freeBinding(ctx, r.mapping);
        published += 1;
    }
    try vt.kill(ctx, .after_publish);
    // 9. Unpin: the pin is the last source reference iff every referencer
    // published; the terminal free punches through the ladder.
    if (pinned) vt.unpin(ctx, src_vol, src_idx);
    pinned = false;
    try vt.kill(ctx, .after_unpin);
    if (published > 0) {
        stats.evacuate_blocks_moved.inc();
        stats.evacuate_bytes_moved.add(n);
        if (task.isShared()) stats.evacuate_shared_blocks_moved.inc();
        return .moved;
    }
    std.debug.assert(remaining_dst_refs == 0);
    return .superseded;
}

// ---------------------------------------------------------------------------
// The RAM world: blocks, bindings, pins, a crash seam, a remount.
// ---------------------------------------------------------------------------

pub const RamWorld = struct {
    pub const BlockState = enum { free, unpublished, allocated };
    pub const Block = struct {
        state: BlockState = .free,
        refcount: u32 = 0,
        incarnation: u64 = 0,
        data: [DATA]u8 = @splat(0),
        len: usize = 0,
        /// RAM: the mover's pin (lost at a crash).
        pinned: bool = false,
        /// The recovered-without-refs class: no RAM cell (pin-less moves).
        untracked: bool = false,
    };
    /// A block's RAM image: wide enough for a REAL transform image (the
    /// rotation sweep's tests re-encrypt framed images in this world: a
    /// one-frame image is one 4 KiB grain; a two-tenant pack two) — the
    /// bindings' tests use the first few bytes.
    pub const DATA: usize = 16 << 10;
    pub const BindKey = struct { ino: u64, block_index: u32 };

    gpa: Allocator,
    chunk: u64,
    /// Volumes × blocks.
    vols: std.AutoArrayHashMapUnmanaged(u64, []Block) = .empty,
    /// The durable layout bindings.
    bindings: std.AutoArrayHashMapUnmanaged(BindKey, Mapping) = .empty,
    open_packs: std.AutoHashMapUnmanaged(u128, void) = .empty,
    busy: std.AutoHashMapUnmanaged(BindKey, void) = .empty,
    eligible: std.AutoHashMapUnmanaged(u64, void) = .empty,
    kill_at: ?KillPoint = null,
    killed: bool = false,
    next_incarnation: u64 = 100,
    frees: u64 = 0,
    republishes: u64 = 0,
    /// Test seam: the next `write`'s bytes land corrupted.
    corrupt_next_write: bool = false,
    /// Test seam: the next `republish` of `ino` sees a foreign binding.
    supersede_ino: ?u64 = null,

    pub fn init(gpa: Allocator, chunk: u64) RamWorld {
        return .{ .gpa = gpa, .chunk = chunk };
    }

    pub fn deinit(self: *RamWorld) void {
        for (self.vols.values()) |b| self.gpa.free(b);
        self.vols.deinit(self.gpa);
        self.bindings.deinit(self.gpa);
        self.open_packs.deinit(self.gpa);
        self.busy.deinit(self.gpa);
        self.eligible.deinit(self.gpa);
    }

    pub fn addVolume(self: *RamWorld, vol_tag: u64, blocks: usize) !void {
        const b = try self.gpa.alloc(Block, blocks);
        @memset(b, .{});
        try self.vols.put(self.gpa, vol_tag, b);
        try self.eligible.put(self.gpa, vol_tag, {});
    }

    pub fn seam(self: *RamWorld) Seam {
        return .{ .ctx = self, .vtable = &vt };
    }

    pub fn block(self: *RamWorld, vol_tag: u64, idx: u64) *Block {
        return &self.vols.get(vol_tag).?[@intCast(idx)];
    }

    /// Plant a block holding `data` (≤ the chunk — a whole block's image is
    /// at most its chunk) and bind it to each `(ino, block_index)`.
    pub fn plant(self: *RamWorld, vol_tag: u64, idx: u64, data: []const u8, owners: []const BindKey) !Mapping {
        std.debug.assert(data.len <= self.chunk and data.len <= DATA);
        const b = self.block(vol_tag, idx);
        b.state = .allocated;
        b.incarnation = self.next_incarnation;
        self.next_incarnation += 1;
        @memcpy(b.data[0..data.len], data);
        b.len = data.len;
        b.refcount = @intCast(owners.len);
        const m = Mapping.wholeBlock(vol_tag, idx * self.chunk, b.incarnation);
        for (owners) |o| try self.bindings.put(self.gpa, o, m);
        return m;
    }

    /// A packed tenant window of an existing block.
    pub fn plantTenant(self: *RamWorld, vol_tag: u64, idx: u64, rel_off: u32, len: u32, owner: BindKey) !Mapping {
        const b = self.block(vol_tag, idx);
        b.refcount += 1;
        const m = Mapping.tenant(vol_tag, idx * self.chunk, b.incarnation, rel_off, len);
        try self.bindings.put(self.gpa, owner, m);
        return m;
    }

    pub fn task(self: *RamWorld, gpa: Allocator, vol_tag: u64, idx: u64) !Task {
        var refs: std.ArrayList(Ref) = .empty;
        var it = self.bindings.iterator();
        while (it.next()) |kv| if (kv.value_ptr.vol_tag == vol_tag and kv.value_ptr.blockIdx(self.chunk) == idx) try refs.append(gpa, .{ .ino = kv.key_ptr.ino, .block_index = kv.key_ptr.block_index, .mapping = kv.value_ptr.* });
        std.mem.sort(Ref, refs.items, {}, struct {
            fn less(_: void, a: Ref, b: Ref) bool {
                if (a.ino != b.ino) return a.ino < b.ino;
                return a.block_index < b.block_index;
            }
        }.less);
        const b = self.block(vol_tag, idx);
        return .{ .vol_tag = vol_tag, .block_idx = idx, .offset = idx * self.chunk, .incarnation = b.incarnation, .refs = try refs.toOwnedSlice(gpa) };
    }

    const vt: Seam.VTable = .{ .chunk = &chunkFn, .isOpenPack = &isOpenPack, .sealPack = &sealPack, .quiescent = &quiescent, .pin = &pin, .refcount = &refcount, .unpin = &unpin, .readImage = &readImage, .allocate = &allocate, .abandonUnpublished = &abandon, .write = &write, .readBack = &readBack, .publishBlock = &publishBlock, .incrementRefcount = &incRef, .releaseRef = &releaseRef, .republish = &republish, .freeBinding = &freeBinding, .kill = &kill };

    fn self_(ctx: *anyopaque) *RamWorld {
        return @ptrCast(@alignCast(ctx));
    }
    fn chunkFn(ctx: *anyopaque) u64 {
        return self_(ctx).chunk;
    }
    fn packKey(vol_tag: u64, idx: u64) u128 {
        return (@as(u128, vol_tag) << 64) | idx;
    }
    fn isOpenPack(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
        return self_(ctx).open_packs.contains(packKey(vol_tag, idx));
    }
    fn sealPack(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        _ = self_(ctx).open_packs.remove(packKey(vol_tag, idx));
    }
    fn quiescent(ctx: *anyopaque, ino: u64, block_index: u32) bool {
        return !self_(ctx).busy.contains(.{ .ino = ino, .block_index = block_index });
    }
    fn pin(ctx: *anyopaque, vol_tag: u64, idx: u64) PinVerdict {
        const b = self_(ctx).block(vol_tag, idx);
        if (b.state != .allocated or b.refcount == 0) return .refused;
        b.refcount += 1;
        b.pinned = true;
        return .pinned;
    }
    fn refcount(ctx: *anyopaque, vol_tag: u64, idx: u64) ?u32 {
        const b = self_(ctx).block(vol_tag, idx);
        return if (b.untracked) null else b.refcount;
    }
    fn unpin(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        const s = self_(ctx);
        const b = s.block(vol_tag, idx);
        b.pinned = false;
        s.release(vol_tag, idx);
    }
    pub fn release(self: *RamWorld, vol_tag: u64, idx: u64) void {
        const b = self.block(vol_tag, idx);
        std.debug.assert(b.refcount > 0);
        b.refcount -= 1;
        if (b.refcount == 0) {
            // The terminal free through the ladder: the block returns.
            b.state = .free;
            b.len = 0;
            @memset(&b.data, 0);
            self.frees += 1;
        }
    }
    fn readImage(ctx: *anyopaque, vol_tag: u64, idx: u64, out: []u8) SeamError!usize {
        const b = self_(ctx).block(vol_tag, idx);
        @memcpy(out[0..b.len], b.data[0..b.len]);
        return b.len;
    }
    fn allocate(ctx: *anyopaque, pick: DestPick, src_vol: u64, src_idx: u64) SeamError!?Minted {
        const s = self_(ctx);
        switch (pick) {
            .balance => |p| {
                // The hint if eligible, else the lowest-fill survivor.
                var choice: ?u64 = null;
                if (p.dest_hint != 0 and p.dest_hint != src_vol and s.eligible.contains(p.dest_hint)) choice = p.dest_hint;
                if (choice == null) {
                    var best_fill: u64 = std.math.maxInt(u64);
                    for (s.vols.keys(), s.vols.values()) |tag, blocks| {
                        if (tag == src_vol or !s.eligible.contains(tag)) continue;
                        var used: u64 = 0;
                        for (blocks) |b| if (b.state != .free) {
                            used += 1;
                        };
                        const fill = used * 1_000_000 / blocks.len;
                        if (fill < best_fill) {
                            best_fill = fill;
                            choice = tag;
                        }
                    }
                }
                const tag = choice orelse return error.Io;
                return s.mintOn(tag, 0, null);
            },
            .compact_low => return s.mintOn(src_vol, 0, src_idx),
            .backend_ascending => |p| {
                const m = (try s.mintOn(p.vol_tag, p.floor.*, null)) orelse return null;
                p.floor.* = m.idx + 1;
                return m;
            },
        }
    }
    pub fn mintOn(self: *RamWorld, vol_tag: u64, from: u64, below: ?u64) SeamError!?Minted {
        const blocks = self.vols.get(vol_tag) orelse return error.Io;
        var i: u64 = from;
        const end: u64 = if (below) |b| @min(b, blocks.len) else blocks.len;
        while (i < end) : (i += 1) {
            if (blocks[@intCast(i)].state == .free) {
                blocks[@intCast(i)] = .{ .state = .unpublished, .refcount = 1, .incarnation = self.next_incarnation };
                self.next_incarnation += 1;
                return .{ .vol_tag = vol_tag, .idx = i, .offset = i * self.chunk, .incarnation = blocks[@intCast(i)].incarnation };
            }
        }
        return null;
    }
    fn abandon(ctx: *anyopaque, m: Minted) void {
        const b = self_(ctx).block(m.vol_tag, m.idx);
        std.debug.assert(b.state == .unpublished);
        b.* = .{};
    }
    fn write(ctx: *anyopaque, m: Minted, image: []const u8) SeamError!void {
        const s = self_(ctx);
        const b = s.block(m.vol_tag, m.idx);
        @memcpy(b.data[0..image.len], image);
        b.len = image.len;
        if (s.corrupt_next_write) {
            s.corrupt_next_write = false;
            b.data[0] ^= 0xFF;
        }
    }
    fn readBack(ctx: *anyopaque, m: Minted, out: []u8) SeamError!usize {
        const b = self_(ctx).block(m.vol_tag, m.idx);
        @memcpy(out[0..b.len], b.data[0..b.len]);
        return b.len;
    }
    fn publishBlock(ctx: *anyopaque, m: Minted) void {
        self_(ctx).block(m.vol_tag, m.idx).state = .allocated;
    }
    fn incRef(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
        const b = self_(ctx).block(vol_tag, idx);
        if (b.state == .free) return false;
        b.refcount += 1;
        return true;
    }
    fn releaseRef(ctx: *anyopaque, vol_tag: u64, idx: u64) void {
        self_(ctx).release(vol_tag, idx);
    }
    fn republish(ctx: *anyopaque, ino: u64, block_index: u32, expected: Mapping, new: Mapping) RepublishError!void {
        const s = self_(ctx);
        const k: BindKey = .{ .ino = ino, .block_index = block_index };
        const cur = s.bindings.get(k) orelse return error.Superseded;
        if (s.supersede_ino) |si| if (si == ino) {
            s.supersede_ino = null;
            return error.Superseded;
        };
        if (!cur.sameRef(expected, s.chunk) or cur.rel_off != expected.rel_off or cur.len != expected.len) return error.Superseded;
        // ONE tx: the swapped entry; the durable refs are the bindings'
        // population (the RAM cell moved at steps 7/9).
        try s.bindings.put(s.gpa, k, new);
        s.republishes += 1;
        return;
    }
    fn freeBinding(ctx: *anyopaque, old: Mapping) void {
        const s = self_(ctx);
        s.release(old.vol_tag, old.blockIdx(s.chunk));
    }
    fn kill(ctx: *anyopaque, at: KillPoint) SeamError!void {
        const s = self_(ctx);
        if (s.kill_at) |k| if (k == at) {
            s.killed = true;
            return error.Killed;
        };
    }

    /// "Remount": RAM state is gone — pins, in-flight mints, RAM cells.
    /// The allocator's refcounts are re-seeded from the durable bindings'
    /// population; an unreferenced block (a leaked unpublished mint, a
    /// source every referencer left) is reconciled FREE.
    pub fn remount(self: *RamWorld) void {
        self.kill_at = null;
        self.killed = false;
        for (self.vols.keys(), self.vols.values()) |tag, blocks| {
            for (blocks, 0..) |*b, i| {
                var pop: u32 = 0;
                var it = self.bindings.iterator();
                while (it.next()) |kv| if (kv.value_ptr.vol_tag == tag and kv.value_ptr.blockIdx(self.chunk) == i) {
                    pop += 1;
                };
                b.pinned = false;
                if (pop == 0) {
                    if (b.state != .free) self.frees += 1;
                    b.* = .{};
                } else {
                    b.state = .allocated;
                    b.refcount = pop;
                }
            }
        }
    }

    /// The bytes a binding resolves to.
    pub fn resolve(self: *RamWorld, k: BindKey) ?[]const u8 {
        const m = self.bindings.get(k) orelse return null;
        const b = self.block(m.vol_tag, m.blockIdx(self.chunk));
        if (b.state != .allocated) return null;
        if (m.whole_block) return b.data[0..b.len];
        return b.data[m.rel_off .. m.rel_off + m.len];
    }

    pub fn usedBlocks(self: *RamWorld, vol_tag: u64) u64 {
        var n: u64 = 0;
        for (self.vols.get(vol_tag).?) |b| if (b.state != .free) {
            n += 1;
        };
        return n;
    }

    pub fn refsOn(self: *RamWorld, vol_tag: u64) u64 {
        var n: u64 = 0;
        var it = self.bindings.iterator();
        while (it.next()) |kv| if (kv.value_ptr.vol_tag == vol_tag) {
            n += 1;
        };
        return n;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const CHUNK: u64 = 4096;

const Fixture = struct {
    w: RamWorld,
    /// Logical contents per binding, for the byte-exact checks.
    expect: std.AutoArrayHashMapUnmanaged(RamWorld.BindKey, [RamWorld.DATA]u8) = .empty,

    fn init(gpa: Allocator) !Fixture {
        var f: Fixture = .{ .w = RamWorld.init(gpa, CHUNK) };
        try f.w.addVolume(0xA, 8);
        try f.w.addVolume(0xB, 8);
        return f;
    }

    fn deinit(self: *Fixture, gpa: Allocator) void {
        self.w.deinit();
        self.expect.deinit(gpa);
    }

    fn plant(self: *Fixture, gpa: Allocator, vol: u64, idx: u64, fill: u8, owners: []const RamWorld.BindKey) !void {
        var data: [RamWorld.DATA]u8 = @splat(fill);
        data[1] = @intCast(idx);
        _ = try self.w.plant(vol, idx, data[0..@min(RamWorld.DATA, CHUNK)], owners);
        for (owners) |o| try self.expect.put(gpa, o, data);
    }

    /// Every binding resolves to its bytes; the allocator agrees with the
    /// bindings' population; nothing is pinned.
    fn check(self: *Fixture) !void {
        var it = self.expect.iterator();
        while (it.next()) |kv| {
            const got = self.w.resolve(kv.key_ptr.*) orelse return error.LostBinding;
            const m = self.w.bindings.get(kv.key_ptr.*).?;
            const want = if (m.whole_block) kv.value_ptr[0..self.w.block(m.vol_tag, m.blockIdx(CHUNK)).len] else kv.value_ptr[m.rel_off .. m.rel_off + m.len];
            try testing.expectEqualSlices(u8, want, got);
        }
        try testing.expectEqual(self.expect.count(), self.w.bindings.count());
        for (self.w.vols.keys(), self.w.vols.values()) |tag, blocks| {
            for (blocks, 0..) |b, i| {
                var pop: u32 = 0;
                var bit = self.w.bindings.iterator();
                while (bit.next()) |kv| if (kv.value_ptr.vol_tag == tag and kv.value_ptr.blockIdx(CHUNK) == i) {
                    pop += 1;
                };
                if (b.state == .allocated) try testing.expectEqual(pop, b.refcount) else try testing.expectEqual(@as(u32, 0), pop);
            }
        }
    }
};

test "moveOne moves a sole-owned block onto the lowest-fill survivor: the source is freed only after the publish; the destination's refcount equals the referencer count; bytes identical" {
    const gpa = testing.allocator;
    var f = try Fixture.init(gpa);
    defer f.deinit(gpa);
    try f.plant(gpa, 0xA, 3, 0x11, &.{.{ .ino = 10, .block_index = 0 }});
    const t = try f.w.task(gpa, 0xA, 3);
    defer gpa.free(t.refs);
    const moved0 = stats.evacuate_blocks_moved.load();
    try testing.expectEqual(Outcome.moved, try moveOne(f.w.seam(), gpa, t, .{ .balance = .{ .dest_hint = 0 } }, 0xA));
    try testing.expectEqual(moved0 + 1, stats.evacuate_blocks_moved.load());
    try testing.expectEqual(@as(u64, 0), f.w.usedBlocks(0xA));
    try testing.expectEqual(@as(u64, 1), f.w.usedBlocks(0xB));
    try testing.expectEqual(@as(u64, 1), f.w.frees);
    try testing.expectEqual(@as(u64, 0), stats.evacuate_inflight_bytes.load());
    try f.check();
}

test "a clone-shared block moves ONCE with every referencer republished; the destination refcount is raised to the referencer count BEFORE any publish; a packed tenant keeps its decoration verbatim" {
    const gpa = testing.allocator;
    var f = try Fixture.init(gpa);
    defer f.deinit(gpa);
    try f.plant(gpa, 0xA, 1, 0x22, &.{ .{ .ino = 5, .block_index = 0 }, .{ .ino = 9, .block_index = 2 }, .{ .ino = 7, .block_index = 1 } });
    // A tenant window on the same block (a packed co-tenant of the share).
    const tm = try f.w.plantTenant(0xA, 1, 8, 16, .{ .ino = 11, .block_index = 0 });
    try f.expect.put(gpa, .{ .ino = 11, .block_index = 0 }, f.expect.get(.{ .ino = 5, .block_index = 0 }).?);
    _ = tm;
    const t = try f.w.task(gpa, 0xA, 1);
    defer gpa.free(t.refs);
    try testing.expectEqual(@as(usize, 4), t.refs.len);
    try testing.expectEqual(@as(u64, 5), t.refs[0].ino);
    const shared0 = stats.evacuate_shared_blocks_moved.load();
    try testing.expectEqual(Outcome.moved, try moveOne(f.w.seam(), gpa, t, .{ .balance = .{ .dest_hint = 0xB } }, 0xA));
    try testing.expectEqual(shared0 + 1, stats.evacuate_shared_blocks_moved.load());
    try testing.expectEqual(@as(u64, 4), f.w.republishes);
    try testing.expectEqual(@as(u64, 0), f.w.usedBlocks(0xA));
    const nb = f.w.bindings.get(.{ .ino = 11, .block_index = 0 }).?;
    try testing.expect(!nb.whole_block and nb.rel_off == 8 and nb.len == 16 and nb.vol_tag == 0xB);
    try testing.expect(f.w.bindings.get(.{ .ino = 5, .block_index = 0 }).?.whole_block);
    try f.check();
}

test "defers: an open pack on the drain's victim is SEALED and deferred; a live referencer defers; a verify-read mismatch frees the destination and leaves the source untouched; a stale census (block freed) is superseded; compact_low with no gap is superseded" {
    const gpa = testing.allocator;
    var f = try Fixture.init(gpa);
    defer f.deinit(gpa);
    try f.plant(gpa, 0xA, 2, 0x33, &.{.{ .ino = 1, .block_index = 0 }});
    const t = try f.w.task(gpa, 0xA, 2);
    defer gpa.free(t.refs);
    try f.w.open_packs.put(gpa, (@as(u128, 0xA) << 64) | 2, {});
    try testing.expectEqual(Outcome.deferred, try moveOne(f.w.seam(), gpa, t, .compact_low, 0xA));
    try testing.expect(!f.w.open_packs.contains((@as(u128, 0xA) << 64) | 2));
    try f.w.busy.put(gpa, .{ .ino = 1, .block_index = 0 }, {});
    try testing.expectEqual(Outcome.deferred, try moveOne(f.w.seam(), gpa, t, .compact_low, 0xA));
    _ = f.w.busy.remove(.{ .ino = 1, .block_index = 0 });
    f.w.corrupt_next_write = true;
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(Outcome.deferred, try moveOne(f.w.seam(), gpa, t, .{ .balance = .{ .dest_hint = 0xB } }, 0xA));
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 0), f.w.usedBlocks(0xB));
    try testing.expectEqual(@as(u32, 1), f.w.block(0xA, 2).refcount);
    try f.check();
    // compact_low: no free gap below idx 2? Fill 0 and 1 first.
    try f.plant(gpa, 0xA, 0, 0x44, &.{.{ .ino = 2, .block_index = 0 }});
    try f.plant(gpa, 0xA, 1, 0x55, &.{.{ .ino = 3, .block_index = 0 }});
    try testing.expectEqual(Outcome.superseded, try moveOne(f.w.seam(), gpa, t, .compact_low, 0));
    try f.check();
    // compact_low with a gap: moves below.
    const t0 = try f.w.task(gpa, 0xA, 0);
    defer gpa.free(t0.refs);
    _ = f.w.bindings.swapRemove(.{ .ino = 2, .block_index = 0 });
    f.w.block(0xA, 0).* = .{};
    _ = f.expect.swapRemove(.{ .ino = 2, .block_index = 0 });
    try testing.expectEqual(Outcome.moved, try moveOne(f.w.seam(), gpa, t, .compact_low, 0));
    try testing.expectEqual(@as(u64, 0), f.w.bindings.get(.{ .ino = 1, .block_index = 0 }).?.blockIdx(CHUNK));
    try f.check();
    // A stale census: the block is freed under the task.
    try testing.expectEqual(Outcome.superseded, try moveOne(f.w.seam(), gpa, t, .compact_low, 0));
}

test "stale mapping supersession is a contractual no-op: a write between copy and publish wins — the destination reference is released, the source stays, the ledger counts the noop" {
    const gpa = testing.allocator;
    var f = try Fixture.init(gpa);
    defer f.deinit(gpa);
    try f.plant(gpa, 0xA, 4, 0x66, &.{ .{ .ino = 20, .block_index = 0 }, .{ .ino = 21, .block_index = 0 } });
    const t = try f.w.task(gpa, 0xA, 4);
    defer gpa.free(t.refs);
    f.w.supersede_ino = 20;
    const noops0 = stats.evacuate_stale_token_noops.load();
    try testing.expectEqual(Outcome.moved, try moveOne(f.w.seam(), gpa, t, .{ .balance = .{ .dest_hint = 0xB } }, 0xA));
    try testing.expectEqual(noops0 + 1, stats.evacuate_stale_token_noops.load());
    // ino 21 moved, ino 20 stayed; both blocks live with refcount 1 each.
    try testing.expectEqual(@as(u64, 0xA), f.w.bindings.get(.{ .ino = 20, .block_index = 0 }).?.vol_tag);
    try testing.expectEqual(@as(u64, 0xB), f.w.bindings.get(.{ .ino = 21, .block_index = 0 }).?.vol_tag);
    try testing.expectEqual(@as(u32, 1), f.w.block(0xA, 4).refcount);
    try f.check();
    // Both superseded ⇒ the destination's terminal free punched it.
    const t2 = try f.w.task(gpa, 0xA, 4);
    defer gpa.free(t2.refs);
    f.w.supersede_ino = 20;
    try testing.expectEqual(Outcome.superseded, try moveOne(f.w.seam(), gpa, t2, .{ .balance = .{ .dest_hint = 0xB } }, 0xA));
    try testing.expectEqual(@as(u64, 1), f.w.usedBlocks(0xB));
    try f.check();
}

test "the mover's nine steps with a kill at each: after the remount never a lost or duplicated binding, every byte intact, the allocator reconciled to the bindings' population; the re-plan converges the drain" {
    const gpa = testing.allocator;
    inline for (@typeInfo(KillPoint).@"enum".field_names) |name| {
        const at = @field(KillPoint, name);
        var f = try Fixture.init(gpa);
        defer f.deinit(gpa);
        try f.plant(gpa, 0xA, 5, 0x77, &.{ .{ .ino = 30, .block_index = 0 }, .{ .ino = 31, .block_index = 3 }, .{ .ino = 32, .block_index = 1 } });
        try f.plant(gpa, 0xA, 6, 0x88, &.{.{ .ino = 33, .block_index = 0 }});
        if (at == .after_seal) try f.w.open_packs.put(gpa, (@as(u128, 0xA) << 64) | 5, {});
        const t = try f.w.task(gpa, 0xA, 5);
        defer gpa.free(t.refs);
        f.w.kill_at = at;
        const r = moveOne(f.w.seam(), gpa, t, .{ .balance = .{ .dest_hint = 0xB } }, 0xA);
        try testing.expectError(error.Killed, r);
        try testing.expect(f.w.killed);
        f.w.remount();
        try f.check();
        // Every binding of the task names ONE block, all the same (a
        // partial publish moved a prefix of the referencers — each to the
        // destination; the rest still on the source — never two copies
        // for one binding, never a binding to a freed block).
        for (t.refs) |ref| try testing.expect(f.w.resolve(.{ .ino = ref.ino, .block_index = ref.block_index }) != null);
        // The drain re-plans from the current bindings and converges.
        var passes: usize = 0;
        while (f.w.refsOn(0xA) > 0 and passes < 8) : (passes += 1) {
            for ([_]u64{ 5, 6 }) |idx| {
                const tk = try f.w.task(gpa, 0xA, idx);
                defer gpa.free(tk.refs);
                if (tk.refs.len == 0) continue;
                _ = try moveOne(f.w.seam(), gpa, tk, .{ .balance = .{ .dest_hint = 0xB } }, 0xA);
            }
        }
        try testing.expectEqual(@as(u64, 0), f.w.refsOn(0xA));
        try testing.expectEqual(@as(u64, 0), f.w.usedBlocks(0xA));
        try f.check();
    }
}

test "backend_ascending advances the floor past every destination (the D2 convergence invariant)" {
    const gpa = testing.allocator;
    var f = try Fixture.init(gpa);
    defer f.deinit(gpa);
    try f.plant(gpa, 0xA, 7, 0x99, &.{.{ .ino = 40, .block_index = 0 }});
    try f.plant(gpa, 0xA, 6, 0xAA, &.{.{ .ino = 40, .block_index = 1 }});
    var floor: u64 = 0;
    for ([_]u64{ 7, 6 }) |idx| {
        const t = try f.w.task(gpa, 0xA, idx);
        defer gpa.free(t.refs);
        try testing.expectEqual(Outcome.moved, try moveOne(f.w.seam(), gpa, t, .{ .backend_ascending = .{ .vol_tag = 0xB, .floor = &floor } }, 0));
    }
    try testing.expectEqual(@as(u64, 2), floor);
    try testing.expectEqual(@as(u64, 0), f.w.bindings.get(.{ .ino = 40, .block_index = 0 }).?.blockIdx(CHUNK));
    try testing.expectEqual(@as(u64, 1), f.w.bindings.get(.{ .ino = 40, .block_index = 1 }).?.blockIdx(CHUNK));
    try f.check();
}
