//! The clone protocol (charter §5.5 "`MarkShared` at the source's holder
//! (SHARED bit on every source record, one tx under the 4a guard) →
//! `ShareBlock` at the index home (the allocation holder) → publish the
//! dest refs WITH the bit"; `docs/porting/sym_data_planes.md` §5.3–§5.4;
//! decision 0003 §E "Un-share witness"). PORTABLE: the actors are seams
//! (the source's slot holder, the index home, the cloner's own publish).
//!
//! Which blocks go through it: only entries whose two references would
//! live in two slot trees — the pair is cross-slot, or the source's
//! reference ALREADY carries SHARED (a clone of a clone: the population is
//! cross-slot whatever this pair's slots are). A same-slot clone of an
//! unshared block runs no protocol, no mark, no index, no bit.
//!
//! Steps, per block `b` of source `F` (holder O) into dest `G` (cloner C),
//! index home H: 0. the share-intent WITNESS registered at H; 1.
//! `MarkShared` at O — under F's 4a EXCLUSIVE guard (the same lock its
//! layout commits and its W1 predicate's caller hold) AND the source
//! file's block guard (so the durable bit and the RAM mark land either
//! wholly BEFORE a patch's fenced mark load or wholly AFTER a patch's DMA;
//! 0002 row 243: the port's own plane block stripe, never the FUSE layer's
//! lock table), ONE tx over every source reference re-Put WITH the bit;
//! `Already` stages nothing; `Gone` aborts the clone `ENOENT` ("the cloner
//! never publishes a reference to a block that may be freed"); 2.
//! `ShareBlock` at H — BOTH inos' entries, chunked at 512; 3. C publishes
//! G's layout with its references `takenShared`, only after step 2
//! returned; the witness retires at step 3's ack.
//!
//! Crash windows: C dies after 1 ⇒ a SHARED flag with no clone (harmless:
//! O's free ships to H, H frees; C16 reports); C dies after 2 ⇒ an index
//! entry with no ref ⇒ C16 + GC at the release; O dies mid-1 ⇒ C retries
//! at O's successor (the durable witness is the flag).
//!
//! The model (`shared_ref_core` + the composed loom model): patcher =
//! retire → fence → `peek == 1 ∧ !is_shared`; owner = release → fence →
//! `is_shared` (local terminal free iff terminal ∧ unmarked, else SHIP);
//! cloner = `mark` → fence → `try_acquire` → fence → `snapshot`. The
//! PATCHER's fence weakened fails; the cloner's fences fail only when
//! BOTH are weakened — at least one SeqCst fence between the pin and the
//! snapshot.

const std = @import("std");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const explorer = @import("../../model/explorer.zig");
const block_alloc = @import("../block_alloc.zig");
const pack = @import("../pack.zig");
const shared_index = @import("shared_index.zig");
const custody = @import("custody.zig");

pub const Ref = shared_index.Ref;

// ---------------------------------------------------------------------------
// Stats (§7.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var mark_shared_calls: counters.ShardedCounter = .{};
    pub var mark_shared_already: counters.ShardedCounter = .{};
    pub var mark_shared_gone: counters.ShardedCounter = .{};
    pub var clone_protocol_blocks: counters.ShardedCounter = .{};
    pub var clone_same_slot_plain: counters.ShardedCounter = .{};
    pub var clone_aborted_enoent: counters.ShardedCounter = .{};
    pub var clone_retried_at_successor: counters.ShardedCounter = .{};
    pub var clone_died_after_mark: counters.ShardedCounter = .{};
    pub var clone_died_after_share: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// §5.3 step 0: which blocks go through the protocol.
// ---------------------------------------------------------------------------

/// Cross-slot, or the source already SHARED (a clone of a clone).
pub fn needsProtocol(src_scope: u64, dst_scope: u64, src_shared: bool) bool {
    return src_scope != dst_scope or src_shared;
}

/// The pack scope of an ino on an armed set: `(meta_volume << 32) |
/// forest_slot` (the charter's `(writer, slot, data volume)` — the writer
/// is implicit, one lessee per slot).
pub fn scopeFor(meta_volume: u32, forest_slot: u32) u64 {
    return pack.packScopeKey(meta_volume, forest_slot);
}

// ---------------------------------------------------------------------------
// §5.3 step 1: `MarkShared` at the source's holder.
// ---------------------------------------------------------------------------

pub const MarkVerdict = enum {
    /// Every source record re-Put WITH the bit, the RAM mark set.
    marked,
    /// The durable witness already stood: nothing staged.
    already,
    /// No record: the clone aborts `ENOENT`.
    gone,
    /// The holder's ring refused the tx (retry).
    deferred,
    /// The holder is unreachable (its successor serves the retry).
    holder_unreachable,
};

/// The source holder's durable half: the source ino's references to the
/// block (both halves decoded), re-Put WITH the bit in ONE tx.
pub const SourceRefs = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// `{count, shared}` of the SOURCE ino's references to the block.
        probe: *const fn (*anyopaque, owner_ino: u64, vol_tag: u64, block_idx: u64) ?custody.RefProbe,
        /// Re-Put every source reference with SHARED (one tx); `false` ⇒
        /// the ring refused.
        rePutShared: *const fn (*anyopaque, owner_ino: u64, vol_tag: u64, block_idx: u64) bool,
    };
};

/// The two guards the served mark takes: the ino's 4a exclusive guard and
/// the plane's block stripe (level 3) — the same rung-3 lock order.
pub const Guards = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        lockIno: *const fn (*anyopaque, ino: u64) void,
        unlockIno: *const fn (*anyopaque, ino: u64) void,
        lockBlock: *const fn (*anyopaque, ino: u64, block_index: u32) void,
        unlockBlock: *const fn (*anyopaque, ino: u64, block_index: u32) void,
    };
};

/// The holder's executor of `MarkShared{vol_tag, block_idx, owner_ino,
/// block_index}`: guards → probe → re-Put → the RAM mark on the serving
/// mount's allocator.
pub fn markShared(refs: SourceRefs, guards: Guards, alloc: ?*block_alloc.BlockAllocator, vol_tag: u64, block_idx: u64, owner_ino: u64, block_index: u32) MarkVerdict {
    stats.mark_shared_calls.inc();
    guards.vtable.lockIno(guards.ctx, owner_ino);
    defer guards.vtable.unlockIno(guards.ctx, owner_ino);
    guards.vtable.lockBlock(guards.ctx, owner_ino, block_index);
    defer guards.vtable.unlockBlock(guards.ctx, owner_ino, block_index);
    const p = refs.vtable.probe(refs.ctx, owner_ino, vol_tag, block_idx) orelse return .holder_unreachable;
    if (p.count == 0) {
        stats.mark_shared_gone.inc();
        return .gone;
    }
    if (p.shared) {
        stats.mark_shared_already.inc();
        if (alloc) |a| a.markShared(block_idx);
        return .already;
    }
    if (!refs.vtable.rePutShared(refs.ctx, owner_ino, vol_tag, block_idx)) return .deferred;
    if (alloc) |a| a.markShared(block_idx);
    return .marked;
}

// ---------------------------------------------------------------------------
// §5.3 The cloner's driver.
// ---------------------------------------------------------------------------

/// The cloner's three remote faces.
pub const Actors = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// Step 1 at the source's holder.
        markShared: *const fn (*anyopaque, vol_tag: u64, block_idx: u64, src: Ref) MarkVerdict,
        /// Step 2 at the index home.
        shareBlock: *const fn (*anyopaque, vol_tag: u64, block_idx: u64, refs: []const Ref) shared_index.ShareVerdict,
        /// Step 3: the dest's reference published WITH the bit (the layout
        /// commit's `takenShared`); `false` ⇒ the publish refused.
        publishDestRef: *const fn (*anyopaque, vol_tag: u64, block_idx: u64, dst: Ref) bool,
        /// The witness (registered before step 1, retired at step 3's ack).
        registerIntent: *const fn (*anyopaque, vol_tag: u64, block_idx: u64, owner_ino: u64) ?u64,
        retireIntent: *const fn (*anyopaque, seq: u64) void,
    };
};

/// One block of the clone: the source's reference, the dest's.
pub const Entry = struct {
    vol_tag: u64,
    block_idx: u64,
    src: Ref,
    dst: Ref,
    src_scope: u64,
    dst_scope: u64,
    src_shared: bool,
};

pub const Step = enum { intent, mark, share, publish };

pub const CloneError = error{ Enoent, Deferred, HolderUnreachable, IndexRejected, PublishRefused, Died };

/// The crash seam: the cloner dies after the named step of the named
/// block (RAM-only; the contracts' crash windows).
pub const DieAfter = struct { block_idx: u64, step: Step };

pub const Outcome = struct { protocol_blocks: u32, plain_blocks: u32 };

/// The driver over the clone's entries: a same-slot unshared block
/// publishes plain; every other walks intent → mark → share → publish.
pub fn cloneShareBlocks(actors: Actors, entries: []const Entry, die_after: ?DieAfter) CloneError!Outcome {
    var out: Outcome = .{ .protocol_blocks = 0, .plain_blocks = 0 };
    for (entries) |e| {
        if (!needsProtocol(e.src_scope, e.dst_scope, e.src_shared)) {
            if (!actors.vtable.publishDestRef(actors.ctx, e.vol_tag, e.block_idx, e.dst)) return error.PublishRefused;
            stats.clone_same_slot_plain.inc();
            out.plain_blocks += 1;
            continue;
        }
        // Step 0: the witness.
        const seq = actors.vtable.registerIntent(actors.ctx, e.vol_tag, e.block_idx, e.src.owner_ino) orelse return error.Deferred;
        var retired = false;
        defer if (!retired) actors.vtable.retireIntent(actors.ctx, seq);
        if (dies(die_after, e.block_idx, .intent)) return died(e.block_idx, .intent);
        // Step 1: MarkShared at the source's holder.
        switch (actors.vtable.markShared(actors.ctx, e.vol_tag, e.block_idx, e.src)) {
            .marked, .already => {},
            .gone => {
                stats.clone_aborted_enoent.inc();
                return error.Enoent;
            },
            .deferred => return error.Deferred,
            .holder_unreachable => {
                // O died mid-1: the durable witness is the flag; the retry
                // lands at O's successor (the caller re-resolves and re-drives).
                stats.clone_retried_at_successor.inc();
                return error.HolderUnreachable;
            },
        }
        if (dies(die_after, e.block_idx, .mark)) {
            stats.clone_died_after_mark.inc();
            return died(e.block_idx, .mark);
        }
        // Step 2: ShareBlock at the index home, both inos.
        switch (actors.vtable.shareBlock(actors.ctx, e.vol_tag, e.block_idx, &.{ e.src, e.dst })) {
            .written => {},
            .rejected => return error.IndexRejected,
            .deferred, .io_error => return error.Deferred,
        }
        if (dies(die_after, e.block_idx, .share)) {
            stats.clone_died_after_share.inc();
            return died(e.block_idx, .share);
        }
        // Step 3: the dest reference WITH the bit; the witness retires at the ack.
        if (!actors.vtable.publishDestRef(actors.ctx, e.vol_tag, e.block_idx, e.dst)) return error.PublishRefused;
        actors.vtable.retireIntent(actors.ctx, seq);
        retired = true;
        stats.clone_protocol_blocks.inc();
        out.protocol_blocks += 1;
        if (dies(die_after, e.block_idx, .publish)) return died(e.block_idx, .publish);
    }
    return out;
}

fn dies(die_after: ?DieAfter, block_idx: u64, step: Step) bool {
    const d = die_after orelse return false;
    return d.block_idx == block_idx and d.step == step;
}

fn died(block_idx: u64, step: Step) CloneError {
    log.warn("clone protocol: the cloner DIED after step {s} of block {d} (the crash seam)", .{ @tagName(step), block_idx });
    return error.Died;
}

// ---------------------------------------------------------------------------
// §5.4 The model: shared_ref_core composed with the W1 pair.
// ---------------------------------------------------------------------------

pub const Weakening = enum { none, patcher_fence, cloner_fence_1, cloner_fence_2, cloner_both };

/// Three threads over three words — `refcount` (the owner holds 1),
/// `shared` (the mark), `stable` (the incarnation). The RMWs on `refcount`
/// (the owner's release, the cloner's acquire) are atomic and totally
/// ordered among themselves; what a weak ordering admits is (a) a STORE
/// still in its thread's buffer when a later load runs — the patcher's
/// retire, the cloner's mark — and (b) a relaxed RMW's result not yet
/// visible to another thread's plain LOAD — the cloner's increment as the
/// patcher's `peek` sees it. A fence step drains both; the weakening
/// deletes the drain.
pub fn SharedRefModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const P = 0;
        const O = 1;
        const C = 2;

        refcount: u8 = 1,
        shared: u8 = 0,
        stable: u8 = 1,
        /// The patcher's retire store in its buffer.
        p_retire_pending: bool = false,
        /// The cloner's mark store in its buffer.
        c_mark_pending: bool = false,
        /// The cloner's increment landed in the RMW chain but not yet
        /// visible to the patcher's plain load.
        c_acquire_unseen: bool = false,
        // Outcomes.
        patched: bool = false,
        pinned: bool = false,
        clone_valid: bool = false,
        owner_terminal: bool = false,
        owner_local_free: bool = false,
        owner_shipped: bool = false,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                P => self.pc[P] == 4,
                O => self.pc[O] == 3,
                C => self.pc[C] == 6,
                else => unreachable,
            };
        }

        fn drainP(self: *Self) void {
            if (self.p_retire_pending) {
                self.stable = 0;
                self.p_retire_pending = false;
            }
        }

        fn drainC(self: *Self) void {
            if (self.c_mark_pending) {
                self.shared = 1;
                self.c_mark_pending = false;
            }
            self.c_acquire_unseen = false;
        }

        /// The refcount as the patcher's plain load sees it.
        fn refcountSeenByP(self: *const Self) u8 {
            return self.refcount - @as(u8, if (self.c_acquire_unseen) 1 else 0);
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                P => switch (self.pc[P]) {
                    0 => {
                        self.p_retire_pending = true; // retire (buffered)
                        self.pc[P] = 1;
                    },
                    1 => {
                        if (weakening != .patcher_fence) self.drainP();
                        self.pc[P] = 2;
                    },
                    2 => {
                        // peek == 1 ∧ !is_shared ⇒ patch (the RAM pair).
                        if (self.refcountSeenByP() == 1 and self.shared == 0) self.patched = true;
                        self.pc[P] = 3;
                    },
                    3 => {
                        self.drainP();
                        self.pc[P] = 4;
                    },
                    else => unreachable,
                },
                O => switch (self.pc[O]) {
                    0 => {
                        // release: an RMW on the chain — terminal iff 1 → 0.
                        if (self.refcount == 1) self.owner_terminal = true;
                        self.refcount -= 1;
                        self.pc[O] = 1;
                    },
                    1 => {
                        // fence → is_shared: local terminal free iff
                        // terminal ∧ unmarked, else SHIP.
                        if (self.owner_terminal) {
                            if (self.shared == 0) self.owner_local_free = true else self.owner_shipped = true;
                        }
                        self.pc[O] = 2;
                    },
                    2 => self.pc[O] = 3,
                    else => unreachable,
                },
                C => switch (self.pc[C]) {
                    0 => {
                        self.c_mark_pending = true; // mark (buffered)
                        self.pc[C] = 1;
                    },
                    1 => {
                        if (weakening != .cloner_fence_1 and weakening != .cloner_both) self.drainC();
                        self.pc[C] = 2;
                    },
                    2 => {
                        // try_acquire: a CAS from NONZERO on the RMW chain.
                        if (self.refcount > 0) {
                            self.refcount += 1;
                            self.pinned = true;
                            self.c_acquire_unseen = true;
                        }
                        self.pc[C] = 3;
                    },
                    3 => {
                        if (weakening != .cloner_fence_2 and weakening != .cloner_both) self.drainC();
                        self.pc[C] = 4;
                    },
                    4 => {
                        // snapshot: valid iff pinned ∧ stable.
                        if (self.pinned and self.stable == 1) self.clone_valid = true;
                        self.pc[C] = 5;
                    },
                    5 => {
                        self.drainC();
                        self.pc[C] = 6;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            // No store-buffering outcome: patched ∧ pinned ∧ a pre-patch snapshot.
            if (self.patched and self.pinned and self.clone_valid) return error.PatchUnderAValidatedPin;
            // A local terminal free never coexists with a landed pin.
            if (self.owner_local_free and self.pinned) return error.FreedUnderAPin;
        }

        /// Count conservation: the owner's 1 minus its one release plus
        /// the pin; a terminal release either freed locally or shipped —
        /// never both, never neither.
        pub fn final(self: *const Self) !void {
            const pin_part: u8 = if (self.pinned) 1 else 0;
            if (self.refcount != pin_part) return error.CountNotConserved;
            if (self.owner_local_free and self.owner_shipped) return error.FreedAndShipped;
            if (self.owner_terminal and !(self.owner_local_free or self.owner_shipped)) return error.TerminalWithoutVerdict;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "a_same_slot_clone_of_an_unshared_block_runs_no_protocol_and_a_shared_source_does; the pack scope is (meta_volume << 32) | slot" {
    try testing.expect(!needsProtocol(scopeFor(0, 5), scopeFor(0, 5), false));
    try testing.expect(needsProtocol(scopeFor(0, 5), scopeFor(0, 6), false));
    try testing.expect(needsProtocol(scopeFor(0, 5), scopeFor(1, 5), false)); // two meta volumes' natives never share a scope
    try testing.expect(needsProtocol(scopeFor(0, 5), scopeFor(0, 5), true)); // a clone of a clone
}

/// The three actors in RAM: a source holder over an allocator + a routed
/// refs table, the index home, the cloner's publish.
const Fixture = struct {
    alloc: *block_alloc.BlockAllocator,
    routed: shared_index.RamRoutedRefs = .{},
    rs: shared_index.RamIndexStore,
    home: shared_index.IndexHome,
    marks: u32 = 0,
    publishes: u32 = 0,
    holder_dead: bool = false,
    refuse_publish: bool = false,
    ino_locks: u32 = 0,
    block_locks: u32 = 0,

    fn init(gpa: Allocator) !*Fixture {
        const f = try gpa.create(Fixture);
        f.* = .{ .alloc = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = 5, .vol = 0, .block_size = 4096, .capacity_bytes = 64 * 4096 }), .rs = .{ .gpa = gpa }, .home = undefined };
        f.home = .{ .gpa = gpa, .store = f.rs.store(), .routed = f.routed.routed() };
        return f;
    }

    fn deinit(self: *Fixture, gpa: Allocator) void {
        self.rs.deinit();
        self.alloc.deinit();
        gpa.destroy(self);
    }

    // SourceRefs over the routed table.
    fn probe(ctx: *anyopaque, owner_ino: u64, _: u64, block_idx: u64) ?custody.RefProbe {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        if (f.holder_dead) return null;
        var count: u32 = 0;
        var shared = false;
        for (f.routed.rows) |r| if (r.used and r.owner_ino == owner_ino and r.block_idx == block_idx) {
            count += 1;
            shared = shared or r.shared;
        };
        return .{ .count = count, .shared = shared };
    }
    fn rePutShared(ctx: *anyopaque, owner_ino: u64, _: u64, block_idx: u64) bool {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        for (&f.routed.rows) |*r| if (r.used and r.owner_ino == owner_ino and r.block_idx == block_idx) {
            r.shared = true;
        };
        f.marks += 1;
        return true;
    }
    const refs_vtable: SourceRefs.VTable = .{ .probe = &probe, .rePutShared = &rePutShared };

    fn lockIno(ctx: *anyopaque, _: u64) void {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        f.ino_locks += 1;
    }
    fn unlockIno(_: *anyopaque, _: u64) void {}
    fn lockBlock(ctx: *anyopaque, _: u64, _: u32) void {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        f.block_locks += 1;
    }
    fn unlockBlock(_: *anyopaque, _: u64, _: u32) void {}
    const guards_vtable: Guards.VTable = .{ .lockIno = &lockIno, .unlockIno = &unlockIno, .lockBlock = &lockBlock, .unlockBlock = &unlockBlock };

    // Actors.
    fn actorMark(ctx: *anyopaque, vol_tag: u64, block_idx: u64, src: Ref) MarkVerdict {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        return markShared(.{ .ctx = f, .vtable = &refs_vtable }, .{ .ctx = f, .vtable = &guards_vtable }, f.alloc, vol_tag, block_idx, src.owner_ino, src.block_index);
    }
    fn actorShare(ctx: *anyopaque, vol_tag: u64, block_idx: u64, refs: []const Ref) shared_index.ShareVerdict {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        return f.home.shareBlock(vol_tag, block_idx, refs, false);
    }
    fn actorPublish(ctx: *anyopaque, _: u64, block_idx: u64, dst: Ref) bool {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        if (f.refuse_publish) return false;
        f.routed.set(dst.owner_ino, block_idx, dst.block_index, true);
        _ = f.alloc.incrementRefcount(block_idx);
        f.publishes += 1;
        return true;
    }
    fn actorRegister(ctx: *anyopaque, vol_tag: u64, block_idx: u64, owner_ino: u64) ?u64 {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        return f.home.registerIntent(vol_tag, block_idx, owner_ino);
    }
    fn actorRetire(ctx: *anyopaque, seq: u64) void {
        const f: *Fixture = @ptrCast(@alignCast(ctx));
        _ = f.home.retireIntent(seq);
    }
    const actors_vtable: Actors.VTable = .{ .markShared = &actorMark, .shareBlock = &actorShare, .publishDestRef = &actorPublish, .registerIntent = &actorRegister, .retireIntent = &actorRetire };

    fn actors(self: *Fixture) Actors {
        return .{ .ctx = self, .vtable = &actors_vtable };
    }

    fn entry(block_idx: u64, cross: bool, src_shared: bool) Entry {
        return .{ .vol_tag = 5, .block_idx = block_idx, .src = .{ .owner_ino = 100, .block_index = @intCast(block_idx) }, .dst = .{ .owner_ino = 200, .block_index = @intCast(block_idx) }, .src_scope = scopeFor(0, 1), .dst_scope = if (cross) scopeFor(0, 2) else scopeFor(0, 1), .src_shared = src_shared };
    }
};

test "a_clone_on_an_armed_forest_marks_the_source_shared_and_indexes_both_inos: the three steps under both guards, the dest published WITH the bit, the witness registered before step 1 and retired at step 3; a same-slot block is plain" {
    const gpa = testing.allocator;
    const f = try Fixture.init(gpa);
    defer f.deinit(gpa);
    var i: u64 = 0;
    while (i < 4) : (i += 1) {
        _ = try f.alloc.allocateBlock();
        f.routed.set(100, i, @intCast(i), false);
    }
    const out = try cloneShareBlocks(f.actors(), &.{ Fixture.entry(0, true, false), Fixture.entry(1, false, false), Fixture.entry(2, true, false) }, null);
    try testing.expectEqual(@as(u32, 2), out.protocol_blocks);
    try testing.expectEqual(@as(u32, 1), out.plain_blocks);
    try testing.expectEqual(@as(u32, 2), f.marks);
    try testing.expectEqual(@as(u32, 2), f.ino_locks);
    try testing.expectEqual(@as(u32, 2), f.block_locks);
    try testing.expectEqual(@as(u32, 3), f.publishes);
    try testing.expect(f.alloc.isShared(0) and !f.alloc.isShared(1) and f.alloc.isShared(2));
    try testing.expectEqual(@as(u32, 2), try f.home.population(5, 0));
    try testing.expectEqual(@as(u32, 0), try f.home.population(5, 1));
    try testing.expectEqual(@as(u32, 0), f.home.inFlight(5, 0)); // retired at step 3
    try testing.expectEqual(@as(?u32, 2), f.alloc.refcount(0));
    // a_clone_of_a_clone_inherits_the_shared_bit: the source is already
    // SHARED ⇒ the protocol runs even same-slot; MarkShared is `already`.
    const already0 = stats.mark_shared_already.load();
    const out2 = try cloneShareBlocks(f.actors(), &.{Fixture.entry(0, false, true)}, null);
    try testing.expectEqual(@as(u32, 1), out2.protocol_blocks);
    try testing.expectEqual(already0 + 1, stats.mark_shared_already.load());
}

test "mark_shared_is_idempotent_against_the_durable_witness_and_gone_aborts_the_clone: a missing source record is ENOENT and nothing is indexed; the witness is retired on the abort" {
    const gpa = testing.allocator;
    const f = try Fixture.init(gpa);
    defer f.deinit(gpa);
    _ = try f.alloc.allocateBlock();
    const enoent0 = stats.clone_aborted_enoent.load();
    try testing.expectError(error.Enoent, cloneShareBlocks(f.actors(), &.{Fixture.entry(0, true, false)}, null));
    try testing.expectEqual(enoent0 + 1, stats.clone_aborted_enoent.load());
    try testing.expectEqual(@as(u32, 0), try f.home.population(5, 0));
    try testing.expectEqual(@as(u32, 0), f.home.inFlight(5, 0));
    try testing.expect(!f.alloc.isShared(0));
    // O dies mid-1: the cloner retries at the successor (HolderUnreachable).
    f.routed.set(100, 0, 0, false);
    f.holder_dead = true;
    const retried0 = stats.clone_retried_at_successor.load();
    try testing.expectError(error.HolderUnreachable, cloneShareBlocks(f.actors(), &.{Fixture.entry(0, true, false)}, null));
    try testing.expectEqual(retried0 + 1, stats.clone_retried_at_successor.load());
    f.holder_dead = false;
    // A refused publish leaves the index entries (C16 reports, the release GCs) and retires the witness.
    f.refuse_publish = true;
    try testing.expectError(error.PublishRefused, cloneShareBlocks(f.actors(), &.{Fixture.entry(0, true, false)}, null));
    try testing.expectEqual(@as(u32, 0), f.home.inFlight(5, 0));
}

test "the two cloner-death windows: after step 1 a SHARED flag with no clone (O's free ships to H, H frees); after step 2 an index entry with no ref (C16 + GC at the release); the witness is retired in both" {
    const gpa = testing.allocator;
    const f = try Fixture.init(gpa);
    defer f.deinit(gpa);
    _ = try f.alloc.allocateBlock();
    _ = try f.alloc.allocateBlock();
    f.routed.set(100, 0, 0, false);
    f.routed.set(100, 1, 1, false);
    // a_cloner_dying_after_mark_shared_leaves_a_harmless_flag_c16_reports
    const after_mark0 = stats.clone_died_after_mark.load();
    try testing.expectError(error.Died, cloneShareBlocks(f.actors(), &.{Fixture.entry(0, true, false)}, .{ .block_idx = 0, .step = .mark }));
    try testing.expectEqual(after_mark0 + 1, stats.clone_died_after_mark.load());
    try testing.expect(f.alloc.isShared(0));
    try testing.expectEqual(@as(u32, 0), try f.home.population(5, 0));
    try testing.expectEqual(@as(u32, 0), f.home.inFlight(5, 0));
    // C16: the flag without an entry.
    const flagged = @import("../../meta/key.zig").blockRefKey(5, 0, 100, 0);
    const d = shared_index.sharedIndexDrift(&.{&flagged}, &.{});
    try testing.expectEqual(@as(u64, 1), d.flag_without_entry);
    // O's terminal free of the flagged block: the gate asks H, which never
    // named it ⇒ NotShared ⇒ the ordinary local verdict frees it.
    const owner: Ref = .{ .owner_ino = 100, .block_index = 0 };
    f.alloc.shared_gate = f.home.sharedFreeGate(&owner);
    try testing.expectEqual(block_alloc.BeginFree.terminal, try f.alloc.freeBlock(0));
    // a_cloner_dying_after_share_block_is_reported_by_c16_and_gcd_at_the_release
    const after_share0 = stats.clone_died_after_share.load();
    try testing.expectError(error.Died, cloneShareBlocks(f.actors(), &.{Fixture.entry(1, true, false)}, .{ .block_idx = 1, .step = .share }));
    try testing.expectEqual(after_share0 + 1, stats.clone_died_after_share.load());
    try testing.expectEqual(@as(u32, 2), try f.home.population(5, 1)); // both entries, the dest never published
    try testing.expectEqual(@as(u32, 0), f.home.inFlight(5, 1));
    // The source's release: Held{1}? No — the dest's entry is GC'd (its ino
    // never referenced the block), so the release FREES.
    f.routed.remove(100, 1);
    const gc0 = shared_index.stats.release_shared_gc.load();
    try testing.expectEqual(shared_index.ReleaseVerdict.freed, f.home.releaseShared(5, 1, .{ .owner_ino = 100, .block_index = 1 }));
    try testing.expectEqual(gc0 + 1, shared_index.stats.release_shared_gc.load());
}

test "shared_ref_core_composed_never_patches_or_frees_a_marked_block: the composed model holds; the PATCHER's fence weakened fails; the cloner's fences fail only when BOTH are weakened (at least one SeqCst fence between the pin and the snapshot)" {
    try explorer.expectHolds(SharedRefModel(.none), .{});
    try explorer.expectFails(SharedRefModel(.patcher_fence), .{}, error.InvariantViolated);
    try explorer.expectHolds(SharedRefModel(.cloner_fence_1), .{});
    try explorer.expectHolds(SharedRefModel(.cloner_fence_2), .{});
    try explorer.expectFails(SharedRefModel(.cloner_both), .{}, error.InvariantViolated);
}
