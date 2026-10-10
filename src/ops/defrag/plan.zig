//! `plan_defrag` (D2 then D1) and `plan_pack_compaction` (spec §3.4, §3.5;
//! charter §6.1 "Compaction = the defrag D1 arm"): PURE planners over the
//! census and the metrics — the movers that execute them are the ONE
//! mover (`lifecycle/mover.zig`, `DestPick.backend_ascending` for D2,
//! `compact_low` for D1) and the pack compactor (`pack_compact.zig`).
//!
//! `plan_defrag`: scope = the named volume (must exist and be placement-
//! eligible — a defrag NAMING a drain's victim fails LOUD at plan time)
//! or every eligible one, sorted. D2: per file with `pairs > 0 ∧ local <
//! pairs` touching the scope: target = the PLURALITY backend among the
//! eligible owners; ONE ascending `floor` per file (each publish bumps it
//! past its destination, so one pass leaves the file same-backend
//! ascending and the next plan finds nothing); a task per refcount-1
//! block not already planned (clones are never de-shared). D1: skip
//! planned bases and bases below the frontier; the rest are `compact_low`
//! tasks sorted `(ino, logical idx)` so ascending gap consumption preserves
//! per-file physical order — compaction must not mint D2 work.
//!
//! `plan_pack_compaction`: per distinct block `pack_windows` (None unless
//! EVERY referencer is a tenant at `block_index 0` with an exact
//! size-carrying mapping — a striped block, a bare whole-block mapping, a
//! damaged marker is never a victim); `live = Σ slot` clamped; victim iff
//! `live ≤ pack_max_slot`; an OPEN pack is never a victim; victims
//! ascending `(live, base)`. A plan executes only if it frees ≥ 1 block:
//! two victims always net one; a LONE victim frees one iff the open pack
//! already has room (`open_room ≥ live`), else nothing moves.

const std = @import("std");
const Allocator = std.mem.Allocator;
const census = @import("../lifecycle/census.zig");
const metrics = @import("metrics.zig");

pub const Ref = census.Ref;
pub const Task = census.Task;
pub const Mapping = census.Mapping;

pub const PlanError = error{ ScopeIneligible, OutOfMemory };

/// A volume row the planner needs.
pub const VolumeRow = struct {
    vol_tag: u64,
    eligible: bool,
    /// The D1 frontier (`used_blocks`).
    frontier: u64,
};

/// One planned move.
pub const Move = struct {
    pub const Kind = enum { d2_locality, d1_compact };
    kind: Kind,
    task: Task,
    /// D2: the target backend; the floor is per FILE (the executor keys it
    /// by `ino`).
    target_vol: u64 = 0,
    ino: u64 = 0,
    logical: u32 = 0,
};

pub const DefragPlan = struct {
    moves: []Move,
    d2_moves: usize,
    d1_moves: usize,

    pub fn deinit(self: *DefragPlan, gpa: Allocator) void {
        for (self.moves) |m| gpa.free(m.task.refs);
        gpa.free(self.moves);
        self.* = undefined;
    }
};

fn eligibleTags(gpa: Allocator, rows: []const VolumeRow) Allocator.Error![]u64 {
    var out: std.ArrayList(u64) = .empty;
    errdefer out.deinit(gpa);
    for (rows) |r| if (r.eligible) try out.append(gpa, r.vol_tag);
    std.mem.sort(u64, out.items, {}, std.sort.asc(u64));
    return out.toOwnedSlice(gpa);
}

fn frontierOf(rows: []const VolumeRow, tag: u64) u64 {
    for (rows) |r| if (r.vol_tag == tag) return r.frontier;
    return 0;
}

/// `(vol_tag, block_idx)`.
pub const BlockKey = struct { vol_tag: u64, block_idx: u64 };
const Key = BlockKey;

/// `scope_vol` = 0 for every eligible volume. `refs` is the whole layout
/// census (every entry of every live file); `chunk` the block size.
pub fn planDefrag(gpa: Allocator, rows: []const VolumeRow, scope_vol: u64, refs: []const Ref, chunk: u64) PlanError!DefragPlan {
    const eligible = try eligibleTags(gpa, rows);
    defer gpa.free(eligible);
    var scope: []const u64 = eligible;
    if (scope_vol != 0) {
        var found = false;
        for (eligible) |t| found = found or t == scope_vol;
        if (!found) return error.ScopeIneligible;
        scope = &.{scope_vol};
    }
    var walk: census.RamWalk = .{ .refs = refs };
    var plan = census.censusFor(gpa, walk.walk(), scope, chunk) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.ScopeIneligible,
    };
    defer plan.deinit(gpa);
    var planned: std.AutoHashMapUnmanaged(Key, void) = .empty;
    defer planned.deinit(gpa);
    var moves: std.ArrayList(Move) = .empty;
    errdefer {
        for (moves.items) |m| gpa.free(m.task.refs);
        moves.deinit(gpa);
    }
    // ---- D2: per file, logical order.
    var by_ino: std.AutoArrayHashMapUnmanaged(u64, std.ArrayList(metrics.Entry)) = .empty;
    defer {
        for (by_ino.values()) |*l| l.deinit(gpa);
        by_ino.deinit(gpa);
    }
    for (refs) |r| {
        if (!r.mapping.whole_block or r.mapping.damaged) continue;
        const gop = try by_ino.getOrPut(gpa, r.ino);
        if (!gop.found_existing) gop.value_ptr.* = .empty;
        try gop.value_ptr.append(gpa, .{ .block_index = r.block_index, .vol_tag = r.mapping.vol_tag, .block_idx = r.mapping.blockIdx(chunk) });
    }
    var d2: usize = 0;
    for (by_ino.keys(), by_ino.values()) |ino, *list| {
        std.mem.sort(metrics.Entry, list.items, {}, struct {
            fn less(_: void, a: metrics.Entry, b: metrics.Entry) bool {
                return a.block_index < b.block_index;
            }
        }.less);
        const m = metrics.measureD2(list.items);
        if (m.pairs == 0 or m.local == m.pairs) continue;
        var touches = false;
        for (list.items) |e| for (scope) |s| {
            touches = touches or e.vol_tag == s;
        };
        if (!touches) continue;
        const target = metrics.pluralityBackend(list.items, eligible) orelse continue;
        for (list.items) |e| {
            const k: Key = .{ .vol_tag = e.vol_tag, .block_idx = e.block_idx };
            if (planned.contains(k)) continue;
            // The census task for this block: refcount-1 only (clones are
            // never de-shared).
            const t = findTask(plan.tasks, k) orelse continue;
            if (t.refs.len != 1) continue;
            try planned.put(gpa, k, {});
            try moves.append(gpa, .{ .kind = .d2_locality, .task = try dupTask(gpa, t), .target_vol = target, .ino = ino, .logical = e.block_index });
            d2 += 1;
        }
    }
    // ---- D1: referenced blocks at/above the frontier, sorted (ino, logical).
    var d1_list: std.ArrayList(Move) = .empty;
    defer d1_list.deinit(gpa);
    for (plan.tasks) |t| {
        const k: Key = .{ .vol_tag = t.vol_tag, .block_idx = t.block_idx };
        if (planned.contains(k)) continue;
        if (t.block_idx < frontierOf(rows, t.vol_tag)) continue;
        try planned.put(gpa, k, {});
        try d1_list.append(gpa, .{ .kind = .d1_compact, .task = try dupTask(gpa, t), .ino = t.refs[0].ino, .logical = t.refs[0].block_index });
    }
    std.mem.sort(Move, d1_list.items, {}, struct {
        fn less(_: void, a: Move, b: Move) bool {
            if (a.ino != b.ino) return a.ino < b.ino;
            return a.logical < b.logical;
        }
    }.less);
    try moves.appendSlice(gpa, d1_list.items);
    return .{ .moves = try moves.toOwnedSlice(gpa), .d2_moves = d2, .d1_moves = d1_list.items.len };
}

fn findTask(tasks: []const Task, k: Key) ?Task {
    for (tasks) |t| if (t.vol_tag == k.vol_tag and t.block_idx == k.block_idx) return t;
    return null;
}

fn dupTask(gpa: Allocator, t: Task) Allocator.Error!Task {
    var c = t;
    c.refs = try gpa.dupe(Ref, t.refs);
    return c;
}

// ---------------------------------------------------------------------------
// The pack compaction plan.
// ---------------------------------------------------------------------------

/// One victim pack block with its windows (distinct `off` at max len) and
/// every referencer.
pub const PackVictim = struct {
    vol_tag: u64,
    block_idx: u64,
    live_bytes: u64,
    windows: []metrics.PackWindow,
    refs: []Ref,
};

pub const PackPlan = struct {
    victims: []PackVictim,
    /// Blocks the plan frees (≥ 1 to execute).
    frees: u64,
    skipped_lone_victim: bool,

    pub fn deinit(self: *PackPlan, gpa: Allocator) void {
        for (self.victims) |v| {
            gpa.free(v.windows);
            gpa.free(v.refs);
        }
        gpa.free(self.victims);
        self.* = undefined;
    }

    pub fn executes(self: PackPlan) bool {
        return self.frees >= 1;
    }
};

/// The windows of a pack block, or null when any referencer is not a
/// tenant at `block_index 0` with an exact size-carrying mapping.
pub fn packWindows(gpa: Allocator, refs: []const Ref) Allocator.Error!?[]metrics.PackWindow {
    var out: std.ArrayList(metrics.PackWindow) = .empty;
    errdefer out.deinit(gpa);
    for (refs) |r| {
        if (r.block_index != 0 or r.mapping.whole_block or r.mapping.damaged or r.mapping.len == 0) {
            out.deinit(gpa);
            return null;
        }
        var merged = false;
        for (out.items) |*w| if (w.off == r.mapping.rel_off) {
            if (r.mapping.len > w.len) w.len = r.mapping.len;
            merged = true;
        };
        if (!merged) try out.append(gpa, .{ .off = r.mapping.rel_off, .len = r.mapping.len });
    }
    return try out.toOwnedSlice(gpa);
}

pub const PackInputs = struct {
    refs: []const Ref,
    scope: []const u64,
    chunk: u64,
    max_slot: u64,
    /// `(vol_tag, block_idx)` of every OPEN pack.
    open: []const BlockKey,
    /// Bytes the open pack can still take (the lone-victim rule).
    open_room_bytes: u64,
};

pub fn planPackCompaction(gpa: Allocator, in: PackInputs) PlanError!PackPlan {
    var walk: census.RamWalk = .{ .refs = in.refs };
    var plan = census.censusFor(gpa, walk.walk(), in.scope, in.chunk) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.ScopeIneligible,
    };
    defer plan.deinit(gpa);
    var victims: std.ArrayList(PackVictim) = .empty;
    errdefer {
        for (victims.items) |v| {
            gpa.free(v.windows);
            gpa.free(v.refs);
        }
        victims.deinit(gpa);
    }
    for (plan.tasks) |t| {
        var is_open = false;
        for (in.open) |o| is_open = is_open or (o.vol_tag == t.vol_tag and o.block_idx == t.block_idx);
        if (is_open) {
            metrics.stats.pack_compaction_deferred.inc();
            continue;
        }
        const windows = (try packWindows(gpa, t.refs)) orelse continue;
        const live = metrics.packLiveBytes(windows, in.chunk);
        if (!(live <= in.max_slot)) {
            gpa.free(windows);
            continue;
        }
        try victims.append(gpa, .{ .vol_tag = t.vol_tag, .block_idx = t.block_idx, .live_bytes = live, .windows = windows, .refs = try gpa.dupe(Ref, t.refs) });
    }
    std.mem.sort(PackVictim, victims.items, {}, struct {
        fn less(_: void, a: PackVictim, b: PackVictim) bool {
            if (a.live_bytes != b.live_bytes) return a.live_bytes < b.live_bytes;
            if (a.vol_tag != b.vol_tag) return a.vol_tag < b.vol_tag;
            return a.block_idx < b.block_idx;
        }
    }.less);
    // Two victims always net one block (each ≤ half a chunk); a lone one
    // frees iff the open pack already has room.
    var frees: u64 = 0;
    var skipped = false;
    if (victims.items.len >= 2) {
        var live: u64 = 0;
        for (victims.items) |v| live += v.live_bytes;
        const needed = (live + in.chunk - 1) / in.chunk;
        frees = victims.items.len - needed;
        if (frees == 0) frees = 1;
    } else if (victims.items.len == 1) {
        if (in.open_room_bytes >= victims.items[0].live_bytes) frees = 1 else {
            skipped = true;
            metrics.stats.pack_compaction_skipped_lone_victim.inc();
        }
    }
    return .{ .victims = try victims.toOwnedSlice(gpa), .frees = frees, .skipped_lone_victim = skipped };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const CHUNK: u64 = 4 << 20;

fn wb(vol: u64, idx: u64) Mapping {
    return Mapping.wholeBlock(vol, idx * CHUNK, 1);
}

test "plan_defrag: D2 then D1 — a non-local file plans its refcount-1 blocks toward the plurality backend (clones skipped); D1 compacts referenced blocks at/above the frontier sorted (ino, logical); planned bases are never double-planned; a scope naming an ineligible volume fails loud" {
    const gpa = testing.allocator;
    const rows = [_]VolumeRow{ .{ .vol_tag = 1, .eligible = true, .frontier = 2 }, .{ .vol_tag = 2, .eligible = true, .frontier = 1 }, .{ .vol_tag = 3, .eligible = false, .frontier = 0 } };
    const refs = [_]Ref{
        // ino 10: blocks on vol 1 (idx 5, 6) and vol 2 (idx 0): plurality 1; non-local pair (1→2).
        .{ .ino = 10, .block_index = 0, .mapping = wb(1, 5) },
        .{ .ino = 10, .block_index = 1, .mapping = wb(1, 6) },
        .{ .ino = 10, .block_index = 2, .mapping = wb(2, 0) },
        // ino 11 shares vol 2 idx 0 (a clone): never de-shared.
        .{ .ino = 11, .block_index = 0, .mapping = wb(2, 0) },
        // ino 12: a local file above the frontier on vol 1 (idx 7): D1 only.
        .{ .ino = 12, .block_index = 0, .mapping = wb(1, 7) },
        // ino 13: below the frontier on vol 1: nothing.
        .{ .ino = 13, .block_index = 0, .mapping = wb(1, 0) },
        // ino 14: on the ineligible volume.
        .{ .ino = 14, .block_index = 0, .mapping = wb(3, 9) },
    };
    var p = try planDefrag(gpa, &rows, 0, &refs, CHUNK);
    defer p.deinit(gpa);
    // D2 for ino 10: idx 5 and 6 (refcount 1) toward vol 1; vol 2 idx 0 is shared ⇒ skipped.
    try testing.expectEqual(@as(usize, 2), p.d2_moves);
    try testing.expectEqual(Move.Kind.d2_locality, p.moves[0].kind);
    try testing.expectEqual(@as(u64, 1), p.moves[0].target_vol);
    try testing.expectEqual(@as(u64, 10), p.moves[0].ino);
    try testing.expectEqual(@as(u64, 5), p.moves[0].task.block_idx);
    try testing.expectEqual(@as(u64, 6), p.moves[1].task.block_idx);
    // D1: vol 1 idx 7 (ino 12) at/above frontier 2; vol 1 idx 5/6 already
    // planned; vol 2 idx 0 is below frontier... frontier 1 ⇒ idx 0 < 1: skipped.
    try testing.expectEqual(@as(usize, 1), p.d1_moves);
    try testing.expectEqual(Move.Kind.d1_compact, p.moves[2].kind);
    try testing.expectEqual(@as(u64, 7), p.moves[2].task.block_idx);
    try testing.expectEqual(@as(usize, 3), p.moves.len);
    try testing.expectError(error.ScopeIneligible, planDefrag(gpa, &rows, 3, &refs, CHUNK));
    try testing.expectError(error.ScopeIneligible, planDefrag(gpa, &rows, 99, &refs, CHUNK));
    // Scoped to vol 2: ino 10 touches it; the only refcount-1 blocks of ino 10 are on vol 1 — the
    // D2 census is over the scope so they are not tasks: nothing planned, no D1 (idx 0 < frontier).
    var p2 = try planDefrag(gpa, &rows, 2, &refs, CHUNK);
    defer p2.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), p2.moves.len);
}

test "plan_defrag is idempotent on a converged layout: a same-backend ascending file below the frontier plans nothing" {
    const gpa = testing.allocator;
    const rows = [_]VolumeRow{.{ .vol_tag = 1, .eligible = true, .frontier = 3 }};
    const refs = [_]Ref{
        .{ .ino = 1, .block_index = 0, .mapping = wb(1, 0) },
        .{ .ino = 1, .block_index = 1, .mapping = wb(1, 1) },
        .{ .ino = 1, .block_index = 2, .mapping = wb(1, 2) },
    };
    var p = try planDefrag(gpa, &rows, 0, &refs, CHUNK);
    defer p.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), p.moves.len);
}

fn tenant(vol: u64, idx: u64, off: u32, len: u32) Mapping {
    return Mapping.tenant(vol, idx * CHUNK, 1, off, len);
}

test "plan_pack_compaction: victims below half re-pack (two always net one), a block above half is untouched, a striped/bare block is never a victim, an open pack is deferred, a lone victim needs open-pack room, windows count a clone share and a nested prefix once; victims ascend (live, base)" {
    const gpa = testing.allocator;
    const refs = [_]Ref{
        // block 0: three tenants, live 12 KiB (victim).
        .{ .ino = 1, .block_index = 0, .mapping = tenant(1, 0, 0, 4000) },
        .{ .ino = 2, .block_index = 0, .mapping = tenant(1, 0, 4096, 4000) },
        .{ .ino = 3, .block_index = 0, .mapping = tenant(1, 0, 8192, 100) },
        // block 1: a clone share + a nested prefix share: live 8 KiB (victim, smaller: sorts first).
        .{ .ino = 4, .block_index = 0, .mapping = tenant(1, 1, 0, 5000) },
        .{ .ino = 5, .block_index = 0, .mapping = tenant(1, 1, 0, 5000) },
        .{ .ino = 6, .block_index = 0, .mapping = tenant(1, 1, 0, 100) },
        // block 2: above half: untouched.
        .{ .ino = 7, .block_index = 0, .mapping = tenant(1, 2, 0, @intCast(CHUNK / 2 + 1)) },
        // block 3: a striped whole-block mapping: never a victim.
        .{ .ino = 8, .block_index = 0, .mapping = wb(1, 3) },
        // block 4: a tenant at block_index 1 (not a pack tenant shape): never.
        .{ .ino = 9, .block_index = 1, .mapping = tenant(1, 4, 0, 100) },
        // block 5: the open pack: deferred.
        .{ .ino = 10, .block_index = 0, .mapping = tenant(1, 5, 0, 100) },
    };
    const deferred0 = metrics.stats.pack_compaction_deferred.load();
    var p = try planPackCompaction(gpa, .{ .refs = &refs, .scope = &.{1}, .chunk = CHUNK, .max_slot = CHUNK / 2, .open = &.{.{ .vol_tag = 1, .block_idx = 5 }}, .open_room_bytes = 0 });
    defer p.deinit(gpa);
    try testing.expectEqual(deferred0 + 1, metrics.stats.pack_compaction_deferred.load());
    try testing.expectEqual(@as(usize, 2), p.victims.len);
    try testing.expectEqual(@as(u64, 1), p.victims[0].block_idx);
    try testing.expectEqual(@as(u64, 8192), p.victims[0].live_bytes);
    try testing.expectEqual(@as(usize, 1), p.victims[0].windows.len);
    try testing.expectEqual(@as(u32, 5000), p.victims[0].windows[0].len);
    try testing.expectEqual(@as(usize, 3), p.victims[0].refs.len);
    try testing.expectEqual(@as(u64, 0), p.victims[1].block_idx);
    try testing.expectEqual(@as(u64, 12288), p.victims[1].live_bytes);
    try testing.expectEqual(@as(usize, 3), p.victims[1].windows.len);
    try testing.expect(p.executes());
    try testing.expectEqual(@as(u64, 1), p.frees);
    // A lone victim without open-pack room moves nothing; with room, one block frees.
    var lone = try planPackCompaction(gpa, .{ .refs = refs[0..3], .scope = &.{1}, .chunk = CHUNK, .max_slot = CHUNK / 2, .open = &.{}, .open_room_bytes = 0 });
    defer lone.deinit(gpa);
    try testing.expect(!lone.executes() and lone.skipped_lone_victim);
    var lone2 = try planPackCompaction(gpa, .{ .refs = refs[0..3], .scope = &.{1}, .chunk = CHUNK, .max_slot = CHUNK / 2, .open = &.{}, .open_room_bytes = 16384 });
    defer lone2.deinit(gpa);
    try testing.expect(lone2.executes());
    try testing.expectEqual(@as(?[]metrics.PackWindow, null), try packWindows(gpa, refs[7..8]));
}
