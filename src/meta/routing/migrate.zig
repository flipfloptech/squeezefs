//! Online `migrate-meta-slot` (Linux; charter §5.4 "bulk copy from read
//! snapshots → delta tee in the conveyor pass → per-slot cutover gate →
//! target-first ordered flip"; `docs/porting/dynamic_routing.md` §3, §7
//! Q3–Q10, Q14, Q17–Q19): the engine that re-homes ONE routing slot from
//! member A to member B of a live set.
//!
//! A slot IS a tree here (Q3), so the bulk copy is a TREE COPY: the source
//! slot tree's leaf snapshots are walked latch-free, page by page, and the
//! target tree is packed bottom-up by `builder.StreamWriter` straight into
//! fresh extents of B — every kind travels (inodes, dentries, xattrs, block
//! refs, block maps), ZERO journal traffic per record, the images barriered
//! before the tree joins B's live set. Nothing in a guest slot tree is
//! pinned (the volume's control records live in tree 0, Q4), so the copy
//! is exact and the tee needs no exclusion. Teardown of
//! the source is ONE `slot_state` delete plus the tree's extents into
//! pending-free (O(nodes)). Slot 0 — the root pin, the native keyspace of
//! member 0 — is the one slot that never moves (`ledger.nativeSlotOf`);
//! every other slot moves anywhere, any number of times (Q6's "never back
//! to the origin" refusal does not exist).
//!
//! The delta: the tee armed BEFORE the copy captures the KEYS of every tx
//! acked on A during it; each round re-reads the live value from A and
//! applies Put/Delete through B's conveyor as ordinary transactions. A
//! round the side log overflowed is a fresh snapshot pass (the copy
//! supersedes the lost keys); three consecutive overflows abort loud.
//!
//! The window: close the gate → drain the slot's admitted ops → the final
//! delta → recheck the tee is empty (never flip over a hole) → write 0 (B's
//! checkpoint makes the copy durable) → write 1 (B stamps `E, +s` with the
//! travelled cursor; checkpoint) → write 2 (A stamps `E, −s`; checkpoint)
//! → the RAM swap → reopen. Every retry (a missed deadline, a late commit)
//! reopens the gate and runs more delta rounds; 16 misses abort. The three
//! writes are the crash table: after 0 the old map (B's copy is unclaimed
//! residue a re-run wipes), after 1 a dual claim B@E vs A@E−1 that
//! discovery resolves to B and a writable open converges, after 2 the new
//! map. A re-run that finds the slot already on B is `finishFlip`: the
//! idempotent redo of write 2 on every other claimant.
//!
//! The parked-Δtime refinements of the port (`economy.PendingTimes`) are
//! a RAM read overlay whose drain is an ordinary gated op routed at commit
//! time, so the reference's "times drain" window step has no counterpart
//! here (the phase family carries the seven legs that exist).

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;

const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const key = @import("../key.zig");
const tree = @import("../tree.zig");
const tx = @import("../tx.zig");
const journal = @import("../journal.zig");
const ledger = @import("../ledger.zig");
const builder = @import("../builder.zig");
const node_cache = @import("../node_cache.zig");
const conveyor = @import("../conveyor.zig");
const inos = @import("../inos.zig");
const record = @import("../record.zig");
const backend = @import("../backend.zig");
const meta_set = @import("../meta_set.zig");
const route_table = @import("route_table.zig");
const migration_tee = @import("migration_tee.zig");
const routing_stats = @import("stats.zig");
const rstats = routing_stats.stats;

comptime {
    if (builtin.os.tag != .linux) @compileError("routing/migrate is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const MetaSet = meta_set.MetaSet;
const KvBackend = backend.KvBackend;
const VolumeIndex = route_table.VolumeIndex;
const Phase = routing_stats.Phase;

/// Consecutive side-log overflows before the migration aborts loud ("the
/// mutation rate outruns the copy"; design §5.5.2's overflow rule).
pub const MAX_CONSECUTIVE_OVERFLOWS: u32 = 3;
/// Consecutive cutover windows that missed the deadline or met a late
/// commit before the engine gives up (abort-and-retry exhausted; never a
/// disabled volume).
pub const MAX_CUTOVER_RETRIES: u32 = 16;
/// The window's deadline: one checkpoint cadence — the window holds three
/// ledger writes (0/1/2), each at most one cadence barrier; the measured
/// target is < 250 ms.
pub const CUTOVER_DEADLINE_NS: u64 = ledger.CHECKPOINT_MAX_AGE_MS * std.time.ns_per_ms;
/// The source walk's page: one leaf's worth of ~90 B inode records keeps
/// a page under 64 KiB of owned copies while the copy streams.
pub const COPY_PAGE: usize = 512;

/// The delta a single target-conveyor batch applies: `deriveBatchTxs(cpus)`
/// transactions of `entryPayloadCap / inode frame` records each — below it
/// the cutover window opens (a round this small lands inside one pass).
pub fn deriveCutoverThreshold(cpus: u32) usize {
    const per_tx: u64 = journal.entryPayloadCap() / journal.recordFrameLen(key.INODE_KEY_LEN, record.INODE_VALUE_LEN);
    return @intCast(@as(u64, conveyor.deriveBatchTxs(cpus)) * per_tx);
}

/// The test seams (`SQUEEZEFS_TEST_MIGRATE_*` class, held in-process): the
/// engine parks at a named phase until released; the flip aborts right
/// after durable write N (the crash matrix: the process state is then what
/// a kill at that instant leaves on the devices).
pub const HoldPhase = enum(u8) { off, after_bulk_copy, gate_closed };

pub const Hold = struct {
    phase: HoldPhase,
    reached: Io.Event = .unset,
    release: Io.Event = .unset,

    pub fn init(phase: HoldPhase) Hold {
        return .{ .phase = phase };
    }

    /// The test side: wait until the engine parked at the phase.
    pub fn waitReached(self: *Hold, io: Io) void {
        self.reached.waitUncancelable(io);
    }

    pub fn releaseEngine(self: *Hold, io: Io) void {
        self.release.set(io);
    }

    fn park(self: *Hold, phase: HoldPhase, io: Io) void {
        if (self.phase != phase) return;
        self.reached.set(io);
        self.release.waitUncancelable(io);
    }
};

pub const Options = struct {
    /// The side log's cap in keys (`migration_tee.deriveDeltaCap`).
    delta_cap: usize,
    cutover_threshold: usize,
    cutover_deadline_ns: u64 = CUTOVER_DEADLINE_NS,
    max_cutover_retries: u32 = MAX_CUTOVER_RETRIES,
    max_consecutive_overflows: u32 = MAX_CONSECUTIVE_OVERFLOWS,
    hold: ?*Hold = null,
    crash_after_write: ?u8 = null,

    pub fn derive(cache_budget_bytes: u64, cpus: u32) Options {
        return .{ .delta_cap = migration_tee.deriveDeltaCap(cache_budget_bytes), .cutover_threshold = deriveCutoverThreshold(cpus) };
    }
};

pub const Report = struct {
    slot: u16,
    from: VolumeIndex,
    to: VolumeIndex,
    records_copied: u64 = 0,
    delta_keys: u64 = 0,
    overflows: u32 = 0,
    cutover_retries: u32 = 0,
    /// The successful window's wall time.
    cutover_ns: u64 = 0,
    /// A re-run that found the slot already flipped (write 2 redone).
    finished_flip: bool = false,

    pub fn write(self: Report, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("slot {d} → member {d} from member {d} ({d} records copied, {d} delta keys, {d} overflows, {d} cutover retries, cutover {d}.{d:0>3} ms{s})", .{ self.slot, self.to, self.from, self.records_copied, self.delta_keys, self.overflows, self.cutover_retries, self.cutover_ns / std.time.ns_per_ms, (self.cutover_ns % std.time.ns_per_ms) / std.time.ns_per_us, if (self.finished_flip) "; a re-run finished the flip" else "" });
    }
};

pub const Error = error{
    /// Member index past the set.
    TargetOutOfRange,
    /// Slot 0: the root pin, member 0's native keyspace.
    SlotZeroPinned,
    /// The source would host nothing (use `remove-meta`).
    SourceWouldBeEmpty,
    /// The target's stamp would exceed its run or cursor budget.
    StampBudget,
    /// Three consecutive side-log overflows.
    OverflowAbort,
    /// Sixteen consecutive missed windows.
    RetriesExhausted,
    /// The `crash_after_write` seam fired.
    CrashSeam,
    Io,
    Corrupt,
    VolumeFailed,
    OutOfMemory,
};

pub fn writeRefusal(w: *Io.Writer, e: Error, slot: u16, target: VolumeIndex, volumes: usize) Io.Writer.Error!void {
    switch (e) {
        error.TargetOutOfRange => try w.print("target volume index {d} out of range ({d} members)", .{ target, volumes }),
        error.SlotZeroPinned => try w.writeAll("slot 0 is the root pin — the native keyspace of member 0 (`/` and every slot-0 record live there un-namespaced); it never migrates. Retire member 0 with `squeezefs volume remove-meta` is likewise refused; every other slot moves"),
        error.SourceWouldBeEmpty => try w.print("migrating slot {d} would leave its host volume hosting nothing — use `squeezefs volume remove-meta` to retire a whole member", .{slot}),
        error.StampBudget => try w.print("the target's membership stamp cannot take slot {d}: it would exceed {d} hosted-slot runs or {d} cursors (the §5.3 encoding budget) — consolidate its slots first", .{ slot, ledger.STAMP_MAX_RUNS, ledger.STAMP_MAX_CURSORS }),
        error.OverflowAbort => try w.print("slot {d} migration aborted: the delta side log overflowed {d} consecutive rounds — the mutation rate outruns the copy; retry at a quieter time (§5.5.2 overflow rule)", .{ slot, MAX_CONSECUTIVE_OVERFLOWS }),
        error.RetriesExhausted => try w.print("slot {d} migration aborted: {d} consecutive cutover windows missed the {d} ms deadline — abort-and-retry exhausted (§5.5.2a; never disabled_volumes)", .{ slot, MAX_CUTOVER_RETRIES, CUTOVER_DEADLINE_NS / std.time.ns_per_ms }),
        error.CrashSeam => try w.writeAll("the crash seam fired after a durable write (test only)"),
        error.Io => try w.writeAll("device I/O failed"),
        error.Corrupt => try w.writeAll("a routing/protocol inconsistency (see the log)"),
        error.VolumeFailed => try w.writeAll("a member latched FAILED"),
        error.OutOfMemory => try w.writeAll("out of memory"),
    }
}

fn mapBackend(e: backend.Error) Error {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.VolumeFailed => error.VolumeFailed,
        error.Io, error.JournalWriteFailed, error.HoleCheckpointFailed, error.ConveyorFull, error.VolumeClosed => error.Io,
        else => error.Corrupt,
    };
}

fn mapCycle(e: anyerror) Error {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.VolumeFailed => error.VolumeFailed,
        error.Io => error.Io,
        else => error.Corrupt,
    };
}

/// The engine (module doc). One migration at a time per set.
pub fn migrateSlot(set: *MetaSet, slot: u16, target: VolumeIndex, opts: Options) Error!Report {
    const io = set.io;
    set.migration_lock.lockUncancelable(io);
    defer set.migration_lock.unlock(io);

    // ---- preflight ----
    if (target >= set.volumes.len) return error.TargetOutOfRange;
    if (slot == 0) return error.SlotZeroPinned;
    const from = set.volumeIndexOfSlot(slot);
    var report: Report = .{ .slot = slot, .from = from, .to = target };
    if (from == target) {
        try finishFlip(set, slot, target);
        report.finished_flip = true;
        return report;
    }
    const src = set.volumes[from];
    const dst = set.volumes[target];
    if (src.stamp.hostedCount() <= 1) return error.SourceWouldBeEmpty;
    try checkStampBudget(set, dst, slot);

    // ---- arm: wipe residue, the tee, (the gate is armed by closing) ----
    dst.dropTree(slot) catch |e| return mapBackend(e);
    const tee = set.teeFor(slot, opts.delta_cap) catch return error.OutOfMemory;
    src.cv.armTee(&set.tee_hook);
    defer src.cv.disarmTee();
    var gate_closed = false;
    defer if (gate_closed) src.slots.door.reopenCutover(slot);

    var consecutive_overflows: u32 = 0;
    var retries: u32 = 0;
    var keys: std.ArrayList(key.KeyBuf) = .empty;
    defer keys.deinit(set.gpa);

    snapshot: while (true) {
        // ---- phase A: the bulk copy from the source's live snapshots ----
        report.records_copied += try copySlotTree(set.gpa, src, dst, slot);
        if (opts.hold) |h| h.park(.after_bulk_copy, io);

        // ---- delta rounds ----
        while (true) {
            const round = tee.drainRound(&keys) catch return error.OutOfMemory;
            if (round.overflowed) {
                consecutive_overflows += 1;
                report.overflows += 1;
                if (consecutive_overflows >= opts.max_consecutive_overflows) {
                    rstats.migrate_slots_aborted.inc();
                    return error.OverflowAbort;
                }
                dst.dropTree(slot) catch |e| return mapBackend(e);
                continue :snapshot;
            }
            consecutive_overflows = 0;
            try applyDelta(set, src, dst, slot, round.keys, &report);
            if (round.keys.len > opts.cutover_threshold) continue;

            // ---- phase B: the cutover window ----
            var prof = routing_stats.Prof.start();
            src.slots.door.closeForCutover(slot);
            gate_closed = true;
            if (opts.hold) |h| h.park(.gate_closed, io);
            const deadline = clock.monotonicNs() + opts.cutover_deadline_ns;
            const drained = src.slots.door.drain(slot, deadline);
            prof.mark(.drain);
            if (drained == .deadline) {
                prof.finish();
                try retryWindow(set, src, &report, &retries, opts, &gate_closed, "the slot's admitted ops did not drain inside the deadline");
                continue;
            }
            // The final delta through B's conveyor (this task holds no leaf
            // lock); overflow or a late commit ⇒ never flip over a hole.
            const final = tee.drainRound(&keys) catch return error.OutOfMemory;
            if (final.overflowed) {
                prof.finish();
                try retryWindow(set, src, &report, &retries, opts, &gate_closed, "the side log overflowed inside the window");
                continue;
            }
            try applyDelta(set, src, dst, slot, final.keys, &report);
            prof.mark(.final_delta);
            if (tee.pendingLen() != 0 or tee.hasOverflowed() or clock.monotonicNs() > deadline) {
                prof.finish();
                try retryWindow(set, src, &report, &retries, opts, &gate_closed, "late commits landed after the final delta");
                continue;
            }
            // Past this point the window never aborts.
            // Write 0: the copy durable on B before B may claim.
            dst.checkpointNow() catch |e| return mapCycle(e);
            prof.mark(.write0);
            if (opts.crash_after_write == 0) {
                prof.finish();
                return error.CrashSeam;
            }
            // Write 1, TARGET FIRST: B claims `s` at E with the travelled
            // cursor.
            const cursor = src.stamp.cursorSnapshot(slot) orelse inos.LOCAL_INO_BASE;
            const epoch = @max(src.stamp.set_epoch, dst.stamp.set_epoch) + 1;
            dst.stamp.claimSlot(slot) catch return error.OutOfMemory;
            dst.stamp.installFloor(slot, cursor) catch return error.OutOfMemory;
            dst.stamp.setEpoch(epoch);
            dst.checkpointNow() catch |e| return mapCycle(e);
            prof.mark(.write1);
            if (opts.crash_after_write == 1) {
                prof.finish();
                return error.CrashSeam;
            }
            // Write 2: A releases `s` at E.
            src.stamp.releaseSlot(slot) catch return error.OutOfMemory;
            src.stamp.removeCursor(slot);
            src.stamp.setEpoch(epoch);
            src.checkpointNow() catch |e| return mapCycle(e);
            prof.mark(.write2);
            if (opts.crash_after_write == 2) {
                prof.finish();
                return error.CrashSeam;
            }
            // The RAM swap, then the parked ops re-route through it (a
            // parker on the source resumes `SlotMoved`: EAGAIN, the op's
            // retry routes to the target).
            set.publishSlot(slot, target) catch return error.OutOfMemory;
            src.slots.door.reopenCutover(slot);
            gate_closed = false;
            prof.mark(.swap);
            report.cutover_ns = prof.total();
            prof.finish();
            break :snapshot;
        }
    }

    // ---- phase C: post-flip ----
    src.cv.disarmTee();
    src.slots.slotLeft(slot);
    src.dropTree(slot) catch |e| return mapBackend(e);
    rstats.migrate_slots_completed.inc();
    var buf: [256]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    report.write(&w) catch {};
    log.info("metadata set: migrated {s}", .{w.buffered()});
    return report;
}

/// A missed window: reopen the gate (parked ops proceed on the OLD map —
/// the tee catches their records), count, and give up at the budget.
fn retryWindow(set: *MetaSet, src: *KvBackend, report: *Report, retries: *u32, opts: Options, gate_closed: *bool, why: []const u8) Error!void {
    _ = set;
    src.slots.door.reopenCutover(report.slot);
    gate_closed.* = false;
    retries.* += 1;
    report.cutover_retries += 1;
    rstats.migrate_cutover_retries.inc();
    log.info("metadata set: slot {d} cutover window retried ({s}; {d} of {d})", .{ report.slot, why, retries.*, opts.max_cutover_retries });
    if (retries.* >= opts.max_cutover_retries) {
        rstats.migrate_slots_aborted.inc();
        return error.RetriesExhausted;
    }
}

/// The target's stamp after the claim must encode: its runs with `slot`
/// added and its cursors plus one inside the §5.3 caps.
fn checkStampBudget(set: *MetaSet, dst: *KvBackend, slot: u16) Error!void {
    const cur = dst.stamp.hostedSlots(set.gpa) catch return error.OutOfMemory;
    defer set.gpa.free(cur);
    var next: std.ArrayList(u16) = .empty;
    defer next.deinit(set.gpa);
    var inserted = false;
    for (cur) |s| {
        if (!inserted and s > slot) {
            next.append(set.gpa, slot) catch return error.OutOfMemory;
            inserted = true;
        }
        next.append(set.gpa, s) catch return error.OutOfMemory;
    }
    if (!inserted) next.append(set.gpa, slot) catch return error.OutOfMemory;
    const runs = route_table.coalesceRuns(set.gpa, next.items) catch return error.OutOfMemory;
    defer set.gpa.free(runs);
    if (runs.len > ledger.STAMP_MAX_RUNS) return error.StampBudget;
    const cursors = dst.stamp.cursorSnapshots(set.gpa) catch return error.OutOfMemory;
    defer set.gpa.free(cursors);
    if (cursors.len + 1 > ledger.STAMP_MAX_CURSORS) return error.StampBudget;
}

/// The tree copy (module doc): the source tree's records in key order
/// through `StreamWriter` into fresh extents of B, the images barriered,
/// the tree opened at its root and adopted into B's live set. Returns the
/// records copied. The offline verbs' per-slot copy too.
pub fn copySlotTree(gpa: Allocator, src: *KvBackend, dst: *KvBackend, slot: u16) Error!u64 {
    const identity: tree.TreeIdentity = .{ .slot = key.guestForestSlot(slot) };
    const Sink = struct {
        fn write(ctx: *anyopaque, addr: u64, image: []const u8) anyerror!void {
            const be: *KvBackend = @ptrCast(@alignCast(ctx));
            try be.cache.loader.writeImage(be.cache.loader.ctx, addr, image);
            // An extent an earlier SMO retired and the allocator handed back:
            // else the open below loads `RetiredExtent`.
            be.cache.unretire(addr);
        }
        fn mint(ctx: *anyopaque) anyerror!u64 {
            const be: *KvBackend = @ptrCast(@alignCast(ctx));
            return be.seq.mint();
        }
    };
    var sw = builder.StreamWriter.init(gpa, dst.layout, dst.sb.heap.start, &dst.alloc, key.KIND_INTERIOR, .{ .ctx = dst, .write = Sink.write }, .{ .ctx = dst, .mint = Sink.mint });
    defer sw.deinit();
    if (src.slotTree(slot)) |st| {
        var cursor = key.KeyBuf.fromSlice("");
        var page: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &page);
        while (true) {
            tree.deinitKeyValues(gpa, &page);
            page = .empty;
            st.range(gpa, cursor.slice(), &tree.KEY_SPACE_MAX, COPY_PAGE, &page) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Corrupt,
            };
            if (page.items.len == 0) break;
            for (page.items) |kv| sw.append(kv.key, kv.value) catch |e| return mapStream(e);
            if (page.items.len < COPY_PAGE) break;
            cursor = key.keySuccessor(page.items[page.items.len - 1].key);
        }
    }
    const root = sw.finish() catch |e| return mapStream(e);
    rstats.migrate_records_copied.add(sw.records_written);
    // The images durable before anything (the delta, write 0's record)
    // can name them.
    dst.dev.barrier() catch return error.Io;
    const t = tree.KvTree.open(gpa, dst.cache, identity, .{ .addr = root.addr, .seq = root.seq }, &dst.seq) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        else => error.Corrupt,
    };
    errdefer t.destroy();
    dst.adoptTree(t) catch return error.OutOfMemory;
    return sw.records_written;
}

pub fn mapBackendError(e: backend.Error) Error {
    return mapBackend(e);
}

pub fn mapCycleError(e: anyerror) Error {
    return mapCycle(e);
}

fn mapStream(e: builder.StreamError) Error {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.NoSpace => error.Io,
        error.SinkFailed, error.SeqMintFailed => error.Io,
        else => error.Corrupt,
    };
}

/// One delta round: every teed key re-read from A's tree (Put the live
/// value, Delete an absent one) into B's tree through B's conveyor, in
/// transactions packed under the whole-entry cap.
fn applyDelta(set: *MetaSet, src: *KvBackend, dst: *KvBackend, slot: u16, keys: []const key.KeyBuf, report: *Report) Error!void {
    if (keys.len == 0) return;
    const gpa = set.gpa;
    const src_tree = src.slotTree(slot);
    const cap = journal.entryPayloadCap();
    var i: usize = 0;
    while (i < keys.len) {
        const t = dst.begin(@src()) catch return error.OutOfMemory;
        // The engine's copy: the slot is closed at the source, the target's
        // tree unpublished — no door tokens (the exclusion is the engine's).
        t.door_bypass = true;
        var bytes: u64 = 0;
        var puts: u64 = 0;
        var staged: usize = 0;
        while (i < keys.len) : (i += 1) {
            const k = keys[i].slice();
            const kind = key.slotTreeKeyKind(k) catch {
                dst.abandon(t);
                return error.Corrupt;
            };
            const value: ?[]u8 = if (src_tree) |st| (st.lookup(gpa, k) catch |e| {
                dst.abandon(t);
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                    else => error.Corrupt,
                };
            }) else null;
            defer if (value) |v| gpa.free(v);
            const frame = journal.recordFrameLen(k.len, if (value) |v| v.len else 0);
            if (staged > 0 and bytes + frame > cap) break;
            if (value) |v| {
                t.stagePut(kind, k, v) catch {
                    dst.abandon(t);
                    return error.OutOfMemory;
                };
                puts += 1;
            } else t.stageDelete(kind, k) catch {
                dst.abandon(t);
                return error.OutOfMemory;
            };
            bytes += frame;
            staged += 1;
        }
        dst.commit(t) catch |e| return mapBackend(e);
        report.delta_keys += staged;
        rstats.migrate_delta_keys.add(staged);
        rstats.migrate_records_copied.add(puts);
    }
}

/// The re-run after a crash past write 1 or 2 (`from == target` at
/// preflight): every OTHER member still claiming the slot gets the
/// idempotent redo of write 2 at the target's epoch and its copy dropped
/// (Q14: only the claimants, never a scan of every member).
fn finishFlip(set: *MetaSet, slot: u16, target: VolumeIndex) Error!void {
    const dst = set.volumes[target];
    for (set.volumes, 0..) |be, v| {
        if (v == target) continue;
        if (!be.stamp.hosts(slot) and be.slotTree(slot) == null) continue;
        be.stamp.releaseSlot(slot) catch return error.OutOfMemory;
        be.stamp.removeCursor(slot);
        be.stamp.setEpoch(@max(be.stamp.set_epoch, dst.stamp.set_epoch));
        be.dropTree(slot) catch |e| return mapBackend(e);
        be.checkpointNow() catch |e| return mapCycle(e);
        log.info("metadata set: slot {d} flip finished on member {d} (its stale claim released at epoch {d}, its copy dropped)", .{ slot, v, be.stamp.set_epoch });
    }
}

// ---------------------------------------------------------------------------
// Tests (Linux; the `meta_set.TestSet` harness).
// ---------------------------------------------------------------------------

const testing = std.testing;
const TestSet = meta_set.TestSet;

test "derivations: the cutover threshold is one conveyor batch of inode-frame records; the constants carry their reasons" {
    const per_tx = journal.entryPayloadCap() / journal.recordFrameLen(key.INODE_KEY_LEN, record.INODE_VALUE_LEN);
    try testing.expectEqual(@as(usize, @intCast(@as(u64, conveyor.deriveBatchTxs(8)) * per_tx)), deriveCutoverThreshold(8));
    try testing.expect(deriveCutoverThreshold(64) > deriveCutoverThreshold(8));
    try testing.expectEqual(@as(u64, 1000 * std.time.ns_per_ms), CUTOVER_DEADLINE_NS);
    try testing.expectEqual(@as(u32, 3), MAX_CONSECUTIVE_OVERFLOWS);
    try testing.expectEqual(@as(u32, 16), MAX_CUTOVER_RETRIES);
    const o = Options.derive(512 << 20, 8);
    try testing.expectEqual(migration_tee.deriveDeltaCap(512 << 20), o.delta_cap);
}

/// The most loaded slot of member `v` among `globals` that is not slot 0
/// — the test's "a loaded slot of member v".
fn loadedSlotOf(set: *MetaSet, globals: []const u64, v: VolumeIndex) u16 {
    var counts = std.AutoHashMap(u16, u16).init(testing.allocator);
    defer counts.deinit();
    var best: u16 = 0;
    var best_n: u16 = 0;
    for (globals) |g| {
        const s = route_table.slotOf(g);
        if (s == 0 or set.volumeIndexOfSlot(s) != v) continue;
        const e = counts.getOrPutValue(s, 0) catch unreachable;
        e.value_ptr.* += 1;
        if (e.value_ptr.* > best_n) {
            best_n = e.value_ptr.*;
            best = s;
        }
    }
    std.debug.assert(best_n > 0);
    return best;
}

test "migrate a loaded slot WHILE a writer storm hits it: every acked create present after the flip and after a remount, the tee's deltas applied, parked ops resumed (routing_gate_parks moved), the slot's records gone from the source, the window exact-sum" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "migrate-storm", 2);
    defer s.destroy();
    var acked: std.ArrayList(u64) = .empty;
    defer acked.deinit(gpa);
    var slot: u16 = 0;
    {
        const set = try s.open();
        var nb: [24]u8 = undefined;
        for (0..300) |i| try acked.append(gpa, try meta_set.createFile(set, inos.ROOT_INO, try std.fmt.bufPrint(&nb, "pre{d}", .{i})));
        slot = loadedSlotOf(set, acked.items, 0);
        try testing.expectEqual(@as(VolumeIndex, 0), set.volumeIndexOfSlot(slot));
        // The storm: a second thread creates under the root and ALSO
        // mutates records of the migrating slot (xattr puts on its inodes)
        // while the engine runs; it stops after the flip.
        const Storm = struct {
            set: *MetaSet,
            slot: u16,
            victims: []const u64,
            stop: std.atomic.Value(bool) = .init(false),
            acked: std.ArrayList(u64) = .empty,
            xattr_puts: u64 = 0,
            gpa: Allocator,

            fn run(self: *@This()) void {
                var i: usize = 0;
                var name_buf: [24]u8 = undefined;
                while (!self.stop.load(.acquire)) : (i += 1) {
                    const name = std.fmt.bufPrint(&name_buf, "storm{d}", .{i}) catch unreachable;
                    const g = meta_set.createFile(self.set, inos.ROOT_INO, name) catch continue;
                    self.acked.append(self.gpa, g) catch @panic("storm: out of memory (an acked create would go unchecked)");
                    // A mutation on the migrating slot through the door.
                    const victim = self.victims[i % self.victims.len];
                    const eff = self.set.effectiveIno(victim);
                    const t = self.set.begin(@src()) catch continue;
                    var vb: [8]u8 = undefined;
                    std.mem.writeInt(u64, &vb, i, .little);
                    var xb: [64]u8 = undefined;
                    const xv = (record.XattrValue{ .name = "user.storm", .value = &vb }).encodeInto(&xb) catch unreachable;
                    t.stagePut(key.TREE_XATTRS, &key.xattrKey(eff, key.xattrNameHash56("user.storm", self.set.nameKey()), 0), xv) catch {
                        self.set.abandon(t);
                        continue;
                    };
                    if (self.set.commit(t)) |_| self.xattr_puts += 1 else |_| {}
                }
            }
        };
        var victims: std.ArrayList(u64) = .empty;
        defer victims.deinit(gpa);
        for (acked.items) |g| if (route_table.slotOf(g) == slot) try victims.append(gpa, g);
        try testing.expect(victims.items.len >= 2);
        var storm: Storm = .{ .set = set, .slot = slot, .victims = victims.items, .gpa = gpa };
        defer storm.acked.deinit(gpa);
        const th = try std.Thread.spawn(.{}, Storm.run, .{&storm});
        var storm_live = true;
        defer if (storm_live) {
            storm.stop.store(true, .release);
            th.join();
        };
        // The engine, with a hold after the bulk copy so the storm is
        // guaranteed to land commits the tee must carry.
        var hold = Hold.init(.after_bulk_copy);
        var opts = Options.derive(64 << 20, 4);
        opts.hold = &hold;
        const Engine = struct {
            fn run(st: *MetaSet, sl: u16, o: Options, out: *(Error!Report)) void {
                out.* = migrateSlot(st, sl, 1, o);
            }
        };
        var out: Error!Report = error.Io;
        const eth = try std.Thread.spawn(.{}, Engine.run, .{ set, slot, opts, &out });
        hold.waitReached(set.io);
        // Let the storm run a while against the migrating slot, then
        // release the engine.
        const puts_at_hold = storm.xattr_puts;
        while (storm.xattr_puts < puts_at_hold + 20) std.Thread.yield() catch {};
        const parks0 = rstats.routing_gate_parks.load();
        hold.releaseEngine(set.io);
        eth.join();
        storm.stop.store(true, .release);
        th.join();
        storm_live = false;
        const report = try out;
        try testing.expectEqual(slot, report.slot);
        try testing.expectEqual(@as(VolumeIndex, 1), set.volumeIndexOfSlot(slot));
        try testing.expect(report.records_copied > 0);
        try testing.expect(report.delta_keys > 0);
        try testing.expect(report.cutover_ns > 0);
        try testing.expect(rstats.routing_gate_parks.load() >= parks0);
        // Every acked record (pre and storm) present; the victims' final
        // xattr value is the storm's last.
        for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
        for (storm.acked.items) |g| try testing.expect(try meta_set.present(set, g));
        for (victims.items) |g| {
            const eff = set.effectiveIno(g);
            const xv = (try set.lookup(gpa, key.TREE_XATTRS, &key.xattrKey(eff, key.xattrNameHash56("user.storm", set.nameKey()), 0))).?;
            defer gpa.free(xv);
        }
        // The source no longer holds the slot's tree; the target does.
        try testing.expect(set.volumes[0].slotTree(slot) == null);
        try testing.expect(set.volumes[1].slotTree(slot) != null);
        try testing.expect(!set.volumes[0].stamp.hosts(slot));
        try testing.expect(set.volumes[1].stamp.hosts(slot));
        try testing.expect(set.volumes[1].stamp.set_epoch > 1);
        // The window family is exact-sum with equal counts.
        const total = rstats.migrate_cutover_phase_ns[@backingInt(Phase.total)].snapshot();
        for (rstats.migrate_cutover_phase_ns[0..Phase.legs]) |*h| try testing.expectEqual(total.count, h.snapshot().count);
        try testing.expect(rstats.migrate_slots_completed.load() >= 1);
        // New mints into the migrated slot land on the target with a
        // cursor past every travelled ino.
        const fresh = try meta_set.createFile(set, inos.ROOT_INO, "post-flip");
        for (acked.items) |g| try testing.expect(g != fresh);
        try acked.appendSlice(gpa, storm.acked.items);
        try acked.append(gpa, fresh);
        set.close();
    }
    {
        const set = try s.openReversed();
        defer set.close();
        try testing.expectEqual(@as(VolumeIndex, 1), set.volumeIndexOfSlot(slot));
        for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
        // Idempotent: a re-run of the same migration finishes the flip
        // (nothing to move) and reports so.
        const again = try migrateSlot(set, slot, 1, Options.derive(64 << 20, 4));
        try testing.expect(again.finished_flip);
    }
}

test "preflight refusals: slot 0 is pinned; a target past the set; and the crash table — a flip torn after write 0 leaves the old map (B's residue wiped by the re-run), after write 1 a dual claim discovery resolves to B and a writable open converges, after write 2 the new map; every acked record present in every window; a slot moves back to its origin" {
    const gpa = testing.allocator;
    const s = try TestSet.create(gpa, "migrate-crash", 2);
    defer s.destroy();
    var acked: std.ArrayList(u64) = .empty;
    defer acked.deinit(gpa);
    var slots: [3]u16 = undefined;
    {
        const set = try s.open();
        defer set.close();
        var nb: [24]u8 = undefined;
        for (0..400) |i| try acked.append(gpa, try meta_set.createFile(set, inos.ROOT_INO, try std.fmt.bufPrint(&nb, "c{d}", .{i})));
        try testing.expectError(error.SlotZeroPinned, migrateSlot(set, 0, 1, Options.derive(64 << 20, 4)));
        try testing.expectError(error.TargetOutOfRange, migrateSlot(set, 2, 7, Options.derive(64 << 20, 4)));
        // Three distinct loaded slots of member 0, one per crash window.
        var n: usize = 0;
        for (acked.items) |g| {
            const sl = route_table.slotOf(g);
            if (sl == 0 or set.volumeIndexOfSlot(sl) != 0) continue;
            var dup = false;
            for (slots[0..n]) |x| dup = dup or x == sl;
            if (dup) continue;
            slots[n] = sl;
            n += 1;
            if (n == 3) break;
        }
        try testing.expectEqual(@as(usize, 3), n);
    }
    // One torn flip per mount session (a torn flip ends the writer: the
    // next writable open is where the rule resolves it), then the rule.
    for (slots, 0..) |sl, w| {
        {
            const set = try s.open();
            defer set.close();
            var o = Options.derive(64 << 20, 4);
            o.crash_after_write = @intCast(w);
            try testing.expectError(error.CrashSeam, migrateSlot(set, sl, 1, o));
            // In-process the RAM map is untouched by a torn flip; the gate
            // reopened; every record still served.
            try testing.expectEqual(@as(VolumeIndex, 0), set.volumeIndexOfSlot(sl));
            try testing.expectEqual(@as(?u16, null), set.volumes[0].slots.door.closedSlot());
            for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
        }
        const d0 = rstats.routing_dual_claims_converged.load();
        const set = try s.openReversed();
        defer set.close();
        const expect_host: VolumeIndex = if (w == 0) 0 else 1;
        try testing.expectEqual(expect_host, set.volumeIndexOfSlot(sl));
        try testing.expectEqual(d0 + @as(u64, if (w == 1) 1 else 0), rstats.routing_dual_claims_converged.load());
        if (w == 1) {
            try testing.expect(!set.volumes[0].stamp.hosts(sl));
            try testing.expect(set.volumes[0].slotTree(sl) == null);
        }
        for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
        // The re-run: window 0's slot migrates for real (B's residue wiped
        // first); windows 1 and 2's finish the flip (nothing to move).
        const r = try migrateSlot(set, sl, 1, Options.derive(64 << 20, 4));
        try testing.expectEqual(w != 0, r.finished_flip);
        try testing.expectEqual(@as(VolumeIndex, 1), set.volumeIndexOfSlot(sl));
        for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
    }
    // And back again: the origin takes a slot as a guest tree (no "never
    // back to the origin" refusal); a fresh mint there lands past every
    // travelled ino.
    {
        const set = try s.open();
        defer set.close();
        const back = try migrateSlot(set, slots[0], 0, Options.derive(64 << 20, 4));
        try testing.expect(!back.finished_flip);
        try testing.expectEqual(@as(VolumeIndex, 0), set.volumeIndexOfSlot(slots[0]));
        for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
        const fresh = try meta_set.createFile(set, inos.ROOT_INO, "after-the-round-trip");
        for (acked.items) |g| try testing.expect(g != fresh);
        try testing.expect(try meta_set.present(set, fresh));
    }
}
