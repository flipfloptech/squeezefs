//! The slot LEASE TABLE (charter §5.5 "Slot acquisition and minting",
//! "Handover", "Death and recovery"; `docs/porting/sym_slot_leases.md` §1):
//! ONE dense per-slot table — the manager's RAM mirror of tree 0 and the
//! commit door's word in one place (spec §9 Q2: the reference kept the
//! lease in four structures and rebuilt a `foreign` bitset per grant;
//! here `foreign` is DERIVED from `(state, holder)` and the holder cache
//! is a view of this table). PORTABLE: atomics and a spin lock, no I/O.
//!
//! Per slot, HOT (one 40 B cell, 2.6 MiB for `W` — read latch-free):
//!   `word`     = `holder u32 ‖ state u8 ‖ dead ‖ rotor` — the word the
//!                door's Dekker pair loads SeqCst against `inflight`;
//!   `g`        = the lease generation (+1 per grant, never at a replay —
//!                the frame screen's word; strictly monotone per slot);
//!   `inflight` = door tokens out (commits past the door, not terminal);
//!   `frontier` = the highest journal position a record of the slot was
//!                applied at in its lessee's ring (the cover loops' quantity);
//!   `ops`      = the holder's op count over two half-window buckets.
//! COLD (under the table's one lock, only for slots that ever held a
//! lease): the offer (`offered_to`, the stand's end), `last_written` (the
//! manager's lease seq at the last release — `prefer: unleased-then-idle`'s
//! rank), the four durable words a lease carries + `seq_floor`, and the
//! per-holder held list (O(held) walks, never a 65 536-entry scan).
//!
//! The state machine (spec §1.3): `unleased → leased` (T1, `g += 1`),
//! `leased → offered` (T2, RAM), `offered → leased` (T3, the lapse),
//! `leased|offered → releasing` (T4), `releasing → leased` (T4', the
//! abort), `releasing → unleased` (T5, after tree 0's `Unleased` landed),
//! `leased{dead} → releasing{dead} → unleased` (T6, recovery), and the
//! migration cutover's `migrating` (rung 5's closed gate, folded in as one
//! more parking state). The two durable HOMES (tree 0 + the lessee's page)
//! are written by the verbs and the page plan in the order spec §1.3
//! names; this table is RAM and `settle` is the pure judge of what a
//! remount finds (rows 5/6, the stale-entry drop, C14's refusal).

const std = @import("std");
const Allocator = std.mem.Allocator;
const jc = @import("../../journal_core.zig");
const inos = @import("../../inos.zig");
const route_table = @import("../../routing/route_table.zig");
const cr = @import("../control_records.zig");
const page = @import("../appender_page.zig");

const assert = std.debug.assert;

/// The routing width: the u16 slot-id wire type, frozen (charter §5.4).
pub const W: usize = route_table.W;
pub const AppenderId = u32;
/// Appender 0 = the fixed ring = the manager (no lessee reads as the
/// manager's structural custody: an unleased tree is the manager's).
pub const MANAGER: AppenderId = 0;

pub const State = enum(u8) {
    unleased = 0,
    leased = 1,
    /// `leased` with ONE named requester standing to accept.
    offered = 2,
    /// A handover's flush-then-transfer (or the recovery's dead mark) is
    /// in flight: the door parks the holder's own commits, refuses others.
    releasing = 3,
    /// Rung 5's cutover gate: `migrate-meta-slot`'s window (parks everyone).
    migrating = 4,
};

/// What one latch-free load of the hot word answers.
pub const View = struct {
    state: State,
    holder: AppenderId,
    dead: bool,
    rotor: bool,
    g: u32,

    pub fn leasedBy(self: View, id: AppenderId) bool {
        return self.state != .unleased and self.state != .migrating and self.holder == id;
    }
};

/// The four durable words a lease carries plus the seq-space floor
/// (charter §5.5 "The record-seq space is monotone across rings").
pub const SlotWords = struct {
    root: cr.Root = .{ .addr = 0, .seq = 0 },
    /// A FLOOR: `max(tree 0, page, in-window max minted, high live key) + 1`.
    cursor: u64 = inos.LOCAL_INO_BASE,
    /// The tree's image count (the affinity ceiling's input).
    extents: u32 = 0,
    seq_floor: u64 = 0,

    /// Floors fold by max; the root is the newer word's.
    pub fn floored(self: SlotWords, other: SlotWords) SlotWords {
        return .{ .root = if (other.root.seq >= self.root.seq) other.root else self.root, .cursor = @max(self.cursor, other.cursor), .extents = other.extents, .seq_floor = @max(self.seq_floor, other.seq_floor) };
    }
};

// ---------------------------------------------------------------------------
// The hot cell.
// ---------------------------------------------------------------------------

const HOLDER_MASK: u64 = 0xFFFF_FFFF;
const STATE_SHIFT: u6 = 32;
const DEAD_BIT: u64 = 1 << 40;
const ROTOR_BIT: u64 = 1 << 41;

fn packWord(state: State, holder: AppenderId, dead: bool, rotor: bool) u64 {
    return @as(u64, holder) | (@as(u64, @backingInt(state)) << STATE_SHIFT) | (if (dead) DEAD_BIT else 0) | (if (rotor) ROTOR_BIT else 0);
}

fn unpackState(word: u64) State {
    return @fromBackingInt(@intCast(@as(u8, @truncate(word >> STATE_SHIFT))));
}

fn unpackHolder(word: u64) AppenderId {
    return @truncate(word & HOLDER_MASK);
}

/// The two half-window buckets as ONE CAS word: `epoch u32 ‖ cur u32`.
fn packOps(epoch: u32, cur: u32) u64 {
    return (@as(u64, epoch) << 32) | cur;
}

pub const Cell = struct {
    /// `state ‖ dead ‖ rotor ‖ holder`: SeqCst on the Dekker side.
    word: std.atomic.Value(u64) = .init(0),
    g: std.atomic.Value(u32) = .init(0),
    inflight: std.atomic.Value(u32) = .init(0),
    frontier: std.atomic.Value(u64) = .init(0),
    /// `HolderOps`: the current half-window epoch and its count.
    ops_cur: std.atomic.Value(u64) = .init(0),
    /// The previous half-window's count (valid for epoch `ops_cur.epoch − 1`).
    ops_prev: std.atomic.Value(u32) = .init(0),
    pad: u32 = 0,
};

comptime {
    // 40 B × 65 536 = 2.6 MiB: five words a reader touches in one or two
    // lines; the cold words live off the table (spec §9 Q2's "≤ 32 B hot").
    assert(@sizeOf(Cell) == 40);
}

// ---------------------------------------------------------------------------
// The cold record.
// ---------------------------------------------------------------------------

pub const Cold = struct {
    offered_to: AppenderId = 0,
    /// Monotonic ns; 0 = no standing offer.
    offer_expires_ns: u64 = 0,
    /// The manager's lease seq at the last release (0 = never written).
    last_written: u64 = 0,
    words: SlotWords = .{},
    /// A handover landed here at this monotonic time (the cooldown's base).
    handover_at_ns: u64 = 0,
};

// ---------------------------------------------------------------------------
// Outcomes.
// ---------------------------------------------------------------------------

pub const AcquireOutcome = union(enum) {
    /// The new generation.
    granted: u32,
    /// Another lease stands (or ours, replayed: `holder == requester`).
    refused: struct { holder: AppenderId, g: u32, state: State, dead: bool },
};

pub const OfferOutcome = enum { offered, not_holder, busy };

pub const ReleaseOutcome = enum { released, already, refused };

pub const LapsedOffer = struct { slot: u16, holder: AppenderId, to: AppenderId };

// ---------------------------------------------------------------------------
// The table.
// ---------------------------------------------------------------------------

pub const LeaseTable = struct {
    gpa: Allocator,
    cells: []Cell,
    lock: jc.SpinLock = .{},
    cold: std.AutoHashMapUnmanaged(u16, Cold) = .empty,
    /// Per holder: the slots it leases (any state but unleased/migrating).
    held: std.AutoHashMapUnmanaged(AppenderId, std.ArrayList(u16)) = .empty,
    /// The manager's lease seq: +1 per release — `last_written`'s clock.
    lease_seq: u64 = 0,
    leased_count: std.atomic.Value(u64) = .init(0),

    pub fn init(gpa: Allocator) Allocator.Error!*LeaseTable {
        const self = try gpa.create(LeaseTable);
        errdefer gpa.destroy(self);
        const cells = try gpa.alloc(Cell, W);
        for (cells) |*c| c.* = .{};
        self.* = .{ .gpa = gpa, .cells = cells };
        return self;
    }

    pub fn deinit(self: *LeaseTable) void {
        var it = self.held.valueIterator();
        while (it.next()) |l| l.deinit(self.gpa);
        self.held.deinit(self.gpa);
        self.cold.deinit(self.gpa);
        self.gpa.free(self.cells);
        self.gpa.destroy(self);
    }

    // ---- the latch-free reads ----

    /// One SeqCst load of the hot word (the door's read side) + `g`.
    pub fn view(self: *const LeaseTable, slot: u16) View {
        const w = self.cells[slot].word.load(.seq_cst);
        return .{ .state = unpackState(w), .holder = unpackHolder(w), .dead = w & DEAD_BIT != 0, .rotor = w & ROTOR_BIT != 0, .g = self.cells[slot].g.load(.acquire) };
    }

    pub fn gOf(self: *const LeaseTable, slot: u16) u32 {
        return self.cells[slot].g.load(.acquire);
    }

    pub fn leasedCount(self: *const LeaseTable) u64 {
        return self.leased_count.load(.acquire);
    }

    // ---- the word writers (under the lock; the store is SeqCst so the
    // door's next load sees it in the one total order) ----

    fn storeWord(self: *LeaseTable, slot: u16, state: State, holder: AppenderId, dead: bool, rotor: bool) void {
        self.cells[slot].word.store(packWord(state, holder, dead, rotor), .seq_cst);
    }

    fn coldOf(self: *LeaseTable, slot: u16) Allocator.Error!*Cold {
        const gop = try self.cold.getOrPut(self.gpa, slot);
        if (!gop.found_existing) gop.value_ptr.* = .{};
        return gop.value_ptr;
    }

    fn addHeld(self: *LeaseTable, holder: AppenderId, slot: u16) Allocator.Error!void {
        const gop = try self.held.getOrPut(self.gpa, holder);
        if (!gop.found_existing) gop.value_ptr.* = .empty;
        for (gop.value_ptr.items) |s| if (s == slot) return;
        try gop.value_ptr.append(self.gpa, slot);
        _ = self.leased_count.fetchAdd(1, .acq_rel);
    }

    fn dropHeld(self: *LeaseTable, holder: AppenderId, slot: u16) void {
        const l = self.held.getPtr(holder) orelse return;
        for (l.items, 0..) |s, i| if (s == slot) {
            _ = l.swapRemove(i);
            _ = self.leased_count.fetchSub(1, .acq_rel);
            return;
        };
    }

    // ---- the arm: tree 0's population ----

    /// Load one tree-0 record (the arm, a refresh): the RAM mirror follows
    /// the durable word verbatim. A `releasing` record is the recovery's
    /// structural mark: `dead` (first touches refused).
    pub fn load(self: *LeaseTable, slot: u16, s: cr.SlotState) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        const prev = self.view(slot);
        if (prev.state != .unleased and prev.state != .migrating) self.dropHeld(prev.holder, slot);
        const cold = try self.coldOf(slot);
        cold.words = .{ .root = s.root, .cursor = s.cursor, .extents = s.slot_tree_extents, .seq_floor = s.seq_floor };
        cold.last_written = s.last_written;
        cold.offer_expires_ns = 0;
        self.cells[slot].g.store(s.g, .release);
        switch (s.variant) {
            .unleased => self.storeWord(slot, .unleased, MANAGER, false, false),
            .leased => {
                try self.addHeld(s.appender_id, slot);
                self.storeWord(slot, .leased, s.appender_id, false, false);
            },
            .releasing => {
                try self.addHeld(s.appender_id, slot);
                self.storeWord(slot, .releasing, s.appender_id, true, false);
            },
        }
    }

    // ---- T1: the grant ----

    /// `unleased → leased{holder}` with `g += 1`. A standing lease refuses
    /// naming its holder and `g` (the same holder = the idempotent replay
    /// the caller answers `already`); `releasing{dead}` refuses as dead.
    pub fn acquire(self: *LeaseTable, slot: u16, holder: AppenderId, rotor: bool) Allocator.Error!AcquireOutcome {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        if (v.state != .unleased) return .{ .refused = .{ .holder = v.holder, .g = v.g, .state = v.state, .dead = v.dead } };
        _ = try self.coldOf(slot);
        try self.addHeld(holder, slot);
        const g = v.g +% 1;
        self.cells[slot].g.store(g, .release);
        self.storeWord(slot, .leased, holder, false, rotor);
        return .{ .granted = g };
    }

    /// T1': the durable write failed — back to `unleased` KEEPING the
    /// minted `g` (one ahead of tree 0 is harmless: `g` need only be
    /// monotone) and `last_written = 0`.
    pub fn rollbackUnwritten(self: *LeaseTable, slot: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        if (v.state == .unleased) return;
        self.dropHeld(v.holder, slot);
        if (self.cold.getPtr(slot)) |c| c.last_written = 0;
        self.storeWord(slot, .unleased, MANAGER, false, false);
    }

    /// The grant's words installed (the grant-time snapshot a newer page
    /// root supersedes); the receiver floors its cursor cell from them.
    pub fn setWords(self: *LeaseTable, slot: u16, words: SlotWords) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        (try self.coldOf(slot)).words = words;
    }

    pub fn wordsOf(self: *LeaseTable, slot: u16) SlotWords {
        self.lock.lock();
        defer self.lock.unlock();
        return if (self.cold.get(slot)) |c| c.words else .{};
    }

    /// The live root/cursor/extents the holder publishes (page or tree 0);
    /// floors never regress.
    pub fn noteLiveWords(self: *LeaseTable, slot: u16, root: cr.Root, cursor: u64, extents: u32) void {
        self.lock.lock();
        defer self.lock.unlock();
        const c = self.cold.getPtr(slot) orelse return;
        c.words.root = root;
        c.words.cursor = @max(c.words.cursor, cursor);
        c.words.extents = extents;
    }

    pub fn setRotor(self: *LeaseTable, slot: u16, rotor: bool) void {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        if (v.state == .unleased or v.state == .migrating) return;
        self.storeWord(slot, v.state, v.holder, v.dead, rotor);
    }

    // ---- T2 / T3: the offer and its lapse ----

    pub fn offer(self: *LeaseTable, slot: u16, holder: AppenderId, to: AppenderId, expires_ns: u64) Allocator.Error!OfferOutcome {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        if (v.state == .unleased or v.state == .migrating or v.holder != holder) return .not_holder;
        if (v.state != .leased) return .busy;
        const c = try self.coldOf(slot);
        c.offered_to = to;
        c.offer_expires_ns = expires_ns;
        self.storeWord(slot, .offered, holder, v.dead, v.rotor);
        return .offered;
    }

    /// The ONE requester an offered slot names, while the stand holds.
    pub fn offeredTo(self: *LeaseTable, slot: u16, now_ns: u64) ?AppenderId {
        self.lock.lock();
        defer self.lock.unlock();
        if (self.view(slot).state != .offered) return null;
        const c = self.cold.get(slot) orelse return null;
        return if (now_ns < c.offer_expires_ns) c.offered_to else null;
    }

    /// Every offer whose stand ended goes back to `leased` (T3); the
    /// lapsed ones are reported for the counters and a wire recall's
    /// withdrawal. Bounded by `out`.
    pub fn expireOffers(self: *LeaseTable, now_ns: u64, out: []LapsedOffer) usize {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        var it = self.cold.iterator();
        while (it.next()) |e| {
            const slot = e.key_ptr.*;
            const v = self.view(slot);
            if (v.state != .offered or now_ns < e.value_ptr.offer_expires_ns) continue;
            e.value_ptr.offer_expires_ns = 0;
            self.storeWord(slot, .leased, v.holder, v.dead, v.rotor);
            if (n < out.len) {
                out[n] = .{ .slot = slot, .holder = v.holder, .to = e.value_ptr.offered_to };
                n += 1;
            }
        }
        return n;
    }

    // ---- T4 / T4': the release's half ----

    /// `leased|offered → releasing` under `holder` (the handover's step 1,
    /// the recovery's dead mark): the SeqCst store IS the Dekker closer's
    /// store; the caller's next load of `inflight` is the pair's load.
    /// Answers `g`, or null when the slot is not the holder's to release.
    pub fn beginRelease(self: *LeaseTable, slot: u16, holder: AppenderId, dead: bool) ?u32 {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        if ((v.state != .leased and v.state != .offered) or v.holder != holder) return null;
        if (self.cold.getPtr(slot)) |c| c.offer_expires_ns = 0;
        self.storeWord(slot, .releasing, holder, dead, v.rotor);
        return v.g;
    }

    /// The abort: the slot accepts commits again; the parkers wake (the
    /// door's business, not the table's).
    pub fn abortRelease(self: *LeaseTable, slot: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        if (v.state != .releasing) return;
        // A recovery's abort keeps the dead mark: `Releasing{dead} →
        // Leased{dead}`, a first touch refused in either state.
        self.storeWord(slot, .leased, v.holder, v.dead, v.rotor);
    }

    // ---- T5: the release landed ----

    /// After tree 0's `Unleased` is durable: the RAM drop. Idempotent
    /// against the generation: a stale `g` is refused, the live one lands
    /// once, a replay at the same `g` of an already-unleased slot is
    /// `already`. Records the words and `last_written`.
    pub fn release(self: *LeaseTable, slot: u16, holder: AppenderId, g: u32, words: SlotWords, now_ns: u64) Allocator.Error!ReleaseOutcome {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        if (g != v.g) return .refused;
        if (v.state == .unleased) return .already;
        if (v.holder != holder) return .refused;
        self.dropHeld(holder, slot);
        self.lease_seq += 1;
        const c = try self.coldOf(slot);
        c.words = words;
        c.last_written = self.lease_seq;
        c.offer_expires_ns = 0;
        c.handover_at_ns = now_ns;
        self.storeWord(slot, .unleased, MANAGER, false, false);
        return .released;
    }

    /// The cooldown base: the last handover's time (0 = none).
    pub fn handoverAt(self: *LeaseTable, slot: u16) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        return if (self.cold.get(slot)) |c| c.handover_at_ns else 0;
    }

    pub fn noteHandover(self: *LeaseTable, slot: u16, now_ns: u64) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        (try self.coldOf(slot)).handover_at_ns = now_ns;
    }

    /// The slot LEFT this volume (a cutover flipped it to another member):
    /// the lease forgotten whatever its state; tree 0's record is the
    /// engine's teardown.
    pub fn evict(self: *LeaseTable, slot: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        if (v.state != .unleased and v.state != .migrating) self.dropHeld(v.holder, slot);
        if (self.cold.getPtr(slot)) |c| c.offer_expires_ns = 0;
        self.storeWord(slot, .unleased, MANAGER, false, false);
    }

    // ---- the migration cutover (rung 5's closed gate, one more state) ----

    /// Close `slot` for a flip: every committer parks. Only an unleased or
    /// own-leased slot closes (the engine moves what this volume holds).
    pub fn closeForCutover(self: *LeaseTable, slot: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.view(slot);
        assert(v.state != .migrating);
        // The holder is kept in the word so the reopen restores it.
        self.cells[slot].word.store(packWord(.migrating, v.holder, v.dead, v.rotor) | (@as(u64, @backingInt(v.state)) << 48), .seq_cst);
    }

    pub fn reopenCutover(self: *LeaseTable, slot: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        const w = self.cells[slot].word.load(.seq_cst);
        if (unpackState(w) != .migrating) return;
        const prior: State = @fromBackingInt(@intCast(@as(u8, @truncate(w >> 48))));
        self.storeWord(slot, prior, unpackHolder(w), w & DEAD_BIT != 0, w & ROTOR_BIT != 0);
    }

    pub fn closedSlot(self: *const LeaseTable) ?u16 {
        // Migrations are serial per set, so a walk finds at most one.
        for (self.cells, 0..) |*c, i| if (unpackState(c.word.load(.acquire)) == .migrating) return @intCast(i);
        return null;
    }

    // ---- the door's words (the Dekker pair, spec §1.5) ----

    /// The committer's half: `inflight += 1` then the word, both SeqCst —
    /// C11's one total order over seq_cst operations forbids both this
    /// load and the closer's load of `inflight` reading the pre-race
    /// value (the SeqCst RMW is a drained store under TSO). Answers the
    /// word AFTER the count went in; the caller backs out with `leaveToken`
    /// when the word refuses.
    pub fn enterToken(self: *LeaseTable, slot: u16) View {
        _ = self.cells[slot].inflight.fetchAdd(1, .seq_cst);
        return self.view(slot);
    }

    /// The token's return: true when the slot's count reached zero (the
    /// closer's drain wakes on it). A CAS-free `fetchSub` that never wraps:
    /// a token is returned exactly once.
    pub fn leaveToken(self: *LeaseTable, slot: u16) bool {
        const prev = self.cells[slot].inflight.fetchSub(1, .seq_cst);
        assert(prev >= 1);
        return prev == 1;
    }

    /// The closer's load (SeqCst, after its SeqCst store of `releasing`).
    pub fn inflightOf(self: *const LeaseTable, slot: u16) u32 {
        return self.cells[slot].inflight.load(.seq_cst);
    }

    // ---- the record frontier (spec §3.5) ----

    /// The pass notes the highest position it applied a record of `slot`
    /// at — one store per slot per batch.
    pub fn noteFrontier(self: *LeaseTable, slot: u16, pos: u64) void {
        _ = self.cells[slot].frontier.fetchMax(pos, .acq_rel);
    }

    pub fn frontierOf(self: *const LeaseTable, slot: u16) u64 {
        return self.cells[slot].frontier.load(.acquire);
    }

    /// Every install resets to the receiving ring's head — never `max`ed
    /// with the departing ring's positions (a position in another ring is
    /// unreachable by this ring's `reusable_upto`).
    pub fn resetFrontier(self: *LeaseTable, slot: u16, head: u64) void {
        self.cells[slot].frontier.store(head, .release);
    }

    /// The slots among `candidates` whose frontier is above `tail` (the
    /// grant's clearing set); bounded by `out`.
    pub fn frontiersAbove(self: *const LeaseTable, candidates: []const u16, tail: u64, out: []u16) []u16 {
        var n: usize = 0;
        for (candidates) |s| {
            if (self.frontierOf(s) > tail and n < out.len) {
                out[n] = s;
                n += 1;
            }
        }
        return out[0..n];
    }

    // ---- HolderOps (spec §4.1): two half-window buckets, lock-free ----

    /// One bump per commit on the lessee's hot path: align the epoch by
    /// CAS, then count. A rotation by one step moves `cur → prev`; by two
    /// or more zeroes both.
    pub fn noteHolderOp(self: *LeaseTable, slot: u16, now_ns: u64, t_idle_ns: u64) void {
        const c = &self.cells[slot];
        const epoch: u32 = @truncate(now_ns / @max(t_idle_ns / 2, 1));
        var cur = c.ops_cur.load(.acquire);
        while (true) {
            const e: u32 = @truncate(cur >> 32);
            const n: u32 = @truncate(cur);
            var next: u64 = undefined;
            if (e == epoch) {
                next = packOps(epoch, n +| 1);
            } else if (e +% 1 == epoch) {
                c.ops_prev.store(n, .release);
                next = packOps(epoch, 1);
            } else {
                c.ops_prev.store(0, .release);
                next = packOps(epoch, 1);
            }
            // acq_rel: a winner's rotation is seen whole by the next bumper.
            if (c.ops_cur.cmpxchgWeak(cur, next, .acq_rel, .acquire)) |seen| cur = seen else return;
        }
    }

    /// The holder's ops over the last `T_idle/2 .. T_idle` — `cur + prev`
    /// read against `now`'s epoch (the same window every requester uses).
    pub fn holderOps(self: *const LeaseTable, slot: u16, now_ns: u64, t_idle_ns: u64) u64 {
        const c = &self.cells[slot];
        const epoch: u32 = @truncate(now_ns / @max(t_idle_ns / 2, 1));
        const cur = c.ops_cur.load(.acquire);
        const e: u32 = @truncate(cur >> 32);
        const n: u32 = @truncate(cur);
        if (e == epoch) return @as(u64, n) + c.ops_prev.load(.acquire);
        if (e +% 1 == epoch) return n;
        return 0;
    }

    pub fn resetHolderOps(self: *LeaseTable, slot: u16) void {
        self.cells[slot].ops_cur.store(0, .release);
        self.cells[slot].ops_prev.store(0, .release);
    }

    // ---- per-holder walks ----

    /// The slots `holder` leases (any state but unleased), ascending.
    pub fn heldBy(self: *LeaseTable, holder: AppenderId, out: *std.ArrayList(u16)) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        const l = self.held.get(holder) orelse return;
        try out.appendSlice(self.gpa, l.items);
        std.mem.sort(u16, out.items[out.items.len - l.items.len ..], {}, std.sort.asc(u16));
    }

    pub fn heldCount(self: *LeaseTable, holder: AppenderId) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return if (self.held.get(holder)) |l| l.items.len else 0;
    }

    /// Rotor-marked leases of `holder` (the `2 × M` cap counts these only).
    pub fn rotorHeldBy(self: *LeaseTable, holder: AppenderId) usize {
        self.lock.lock();
        defer self.lock.unlock();
        const l = self.held.get(holder) orelse return 0;
        var n: usize = 0;
        for (l.items) |s| n += @intFromBool(self.view(s).rotor);
        return n;
    }

    pub fn lastWritten(self: *LeaseTable, slot: u16) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        return if (self.cold.get(slot)) |c| c.last_written else 0;
    }

    /// The widest ask: `2 × MINT_SPREAD` (an over-cap rotor's `2M`).
    pub const PICK_MAX: usize = 128;

    /// `prefer: unleased-then-idle` (spec §2.2): from `candidates` (the
    /// hosted set minus the native slot, walked once), the unleased ones
    /// ranked never-leased first (`last_written == 0`), then the least
    /// recently written, ties by slot index; the first `out.len` of them.
    pub fn pickUnleased(self: *LeaseTable, candidates: []const u16, out: []u16) []u16 {
        assert(out.len <= PICK_MAX);
        self.lock.lock();
        defer self.lock.unlock();
        var ranks: [PICK_MAX]u64 = undefined;
        var n: usize = 0;
        for (candidates) |s| {
            if (self.view(s).state != .unleased) continue;
            const lw = if (self.cold.get(s)) |c| c.last_written else 0;
            // Insertion into the bounded best-`out.len` list (want ≤ 128).
            var i = n;
            while (i > 0 and rankLess(lw, s, ranks[i - 1], out[i - 1])) : (i -= 1) {
                if (i < out.len) {
                    ranks[i] = ranks[i - 1];
                    out[i] = out[i - 1];
                }
            }
            if (i < out.len) {
                ranks[i] = lw;
                out[i] = s;
                if (n < out.len) n += 1;
            }
        }
        return out[0..n];
    }

    fn rankLess(lw_a: u64, s_a: u16, lw_b: u64, s_b: u16) bool {
        if (lw_a != lw_b) return lw_a < lw_b;
        return s_a < s_b;
    }
};

// ---------------------------------------------------------------------------
// The remount's judge: a page's entries against tree 0 (spec §1.2 rows 5/6,
// the stale-entry drop, C14).
// ---------------------------------------------------------------------------

pub const Attested = struct { g: u32, words: SlotWords };

pub const Settle = union(enum) {
    /// Row 5: the page says `Releasing` and tree 0 still `Leased{page's
    /// owner, g}` — the next open COMPLETES the release from the page's
    /// words (tree 0 `Unleased`, then the entry drops).
    complete_release: Attested,
    /// Row 6: a `Releasing` entry tree 0 no longer backs (Unleased, or
    /// another lessee) — tree 0 wins, the entry drops.
    drop_entry,
    /// A `Live` entry BELOW tree 0's `g`: stale residue, dropped and counted.
    stale_entry,
    /// A `Live` entry at tree 0's `g` under ANOTHER lessee: C14's conflict,
    /// the mount refuses.
    conflict,
    /// The entry is the live home of the slot: its root/cursor/extents are
    /// adopted (the page is at least as new as the grant-time record).
    adopt: Attested,
};

/// Judge one page entry of `page_owner` against tree 0's record (`null` =
/// tree 0 names no record: a slot that was never granted, so any
/// attestation is residue).
pub fn settleEntry(tree0: ?cr.SlotState, entry: page.SlotEntry, page_owner: AppenderId) Settle {
    const words: SlotWords = .{ .root = .{ .addr = entry.root_addr, .seq = entry.root_seq }, .cursor = entry.cursor, .extents = entry.slot_tree_extents };
    const t = tree0 orelse return if (entry.state == .releasing) .drop_entry else .stale_entry;
    switch (entry.state) {
        .releasing => {
            if (t.variant != .unleased and t.appender_id == page_owner and t.g == entry.g) return .{ .complete_release = .{ .g = entry.g, .words = words } };
            return .drop_entry;
        },
        .live => {
            if (entry.g < t.g) return .stale_entry;
            if (t.variant == .unleased) return .stale_entry;
            if (t.appender_id != page_owner or entry.g != t.g) return .conflict;
            return .{ .adopt = .{ .g = entry.g, .words = words } };
        },
    }
}

/// C14's other face: ONE slot `Live` on TWO pages refuses the mount. The
/// pages' entries are passed as `(owner, entries)` rows; the first slot
/// seen twice is answered.
pub fn liveOnTwoPages(gpa: Allocator, pages: []const struct { owner: AppenderId, entries: []const page.SlotEntry }) Allocator.Error!?u16 {
    var seen = std.AutoHashMapUnmanaged(u16, AppenderId).empty;
    defer seen.deinit(gpa);
    for (pages) |p| for (p.entries) |e| {
        if (e.state != .live) continue;
        const gop = try seen.getOrPut(gpa, e.slot);
        if (gop.found_existing and gop.value_ptr.* != p.owner) return e.slot;
        gop.value_ptr.* = p.owner;
    };
    return null;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the state machine: grant mints g+1 and refuses a standing lease naming it; offer/lapse are RAM; begin/abort release; a stale g is refused, the live release lands once and a replay is already; the held walk and the leased gauge follow" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    try testing.expectEqual(State.unleased, t.view(7).state);
    const g1 = try t.acquire(7, 1, true);
    try testing.expectEqual(@as(u32, 1), g1.granted);
    try testing.expect(t.view(7).leasedBy(1) and t.view(7).rotor);
    const again = try t.acquire(7, 2, false);
    try testing.expectEqual(@as(u32, 1), again.refused.holder);
    try testing.expectEqual(@as(u32, 1), again.refused.g);
    try testing.expectEqual(@as(u64, 1), t.leasedCount());
    try testing.expectEqual(@as(usize, 1), t.rotorHeldBy(1));
    // Offer to 2, standing until 1000; a second offer is busy; the lapse.
    try testing.expectEqual(OfferOutcome.offered, try t.offer(7, 1, 2, 1000));
    try testing.expectEqual(OfferOutcome.busy, try t.offer(7, 1, 3, 1000));
    try testing.expectEqual(OfferOutcome.not_holder, try t.offer(7, 9, 3, 1000));
    try testing.expectEqual(@as(?u32, 2), t.offeredTo(7, 999));
    try testing.expectEqual(@as(?u32, null), t.offeredTo(7, 1000));
    var lapsed: [4]LapsedOffer = undefined;
    try testing.expectEqual(@as(usize, 0), t.expireOffers(999, &lapsed));
    try testing.expectEqual(@as(usize, 1), t.expireOffers(1000, &lapsed));
    try testing.expectEqual(@as(u16, 7), lapsed[0].slot);
    try testing.expectEqual(State.leased, t.view(7).state);
    // The release half: not the holder's ⇒ null; the holder's ⇒ g; abort.
    try testing.expectEqual(@as(?u32, null), t.beginRelease(7, 2, false));
    try testing.expectEqual(@as(?u32, 1), t.beginRelease(7, 1, false));
    try testing.expectEqual(State.releasing, t.view(7).state);
    t.abortRelease(7);
    try testing.expectEqual(State.leased, t.view(7).state);
    _ = t.beginRelease(7, 1, false).?;
    const words: SlotWords = .{ .root = .{ .addr = 4096, .seq = 9 }, .cursor = 77, .extents = 3, .seq_floor = 500 };
    try testing.expectEqual(ReleaseOutcome.refused, try t.release(7, 1, 0, words, 5));
    try testing.expectEqual(ReleaseOutcome.refused, try t.release(7, 2, 1, words, 5));
    try testing.expectEqual(ReleaseOutcome.released, try t.release(7, 1, 1, words, 5));
    try testing.expectEqual(ReleaseOutcome.already, try t.release(7, 1, 1, words, 5));
    try testing.expectEqual(State.unleased, t.view(7).state);
    try testing.expectEqual(@as(u64, 0), t.leasedCount());
    try testing.expectEqual(@as(u64, 1), t.lastWritten(7));
    try testing.expectEqual(words, t.wordsOf(7));
    // The next grant carries g = 2 (monotone across the release).
    try testing.expectEqual(@as(u32, 2), (try t.acquire(7, 2, false)).granted);
    var held: std.ArrayList(u16) = .empty;
    defer held.deinit(gpa);
    try t.heldBy(2, &held);
    try testing.expectEqualSlices(u16, &.{7}, held.items);
    // The rollback keeps g, clears the rank.
    t.rollbackUnwritten(7);
    try testing.expectEqual(State.unleased, t.view(7).state);
    try testing.expectEqual(@as(u32, 2), t.gOf(7));
    try testing.expectEqual(@as(u64, 0), t.lastWritten(7));
}

test "the Dekker words: a token enters and leaves exactly once; the closer's store is visible to the next entrant; the frontier is a fetch_max reset at install; the cutover close keeps the prior state for its reopen" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    _ = try t.acquire(3, 1, false);
    const v = t.enterToken(3);
    try testing.expect(v.leasedBy(1));
    try testing.expectEqual(@as(u32, 1), t.inflightOf(3));
    _ = t.beginRelease(3, 1, false).?;
    try testing.expectEqual(State.releasing, t.enterToken(3).state);
    try testing.expect(!t.leaveToken(3));
    try testing.expect(t.leaveToken(3));
    try testing.expectEqual(@as(u32, 0), t.inflightOf(3));
    t.noteFrontier(3, 500);
    t.noteFrontier(3, 400);
    try testing.expectEqual(@as(u64, 500), t.frontierOf(3));
    var out: [4]u16 = undefined;
    try testing.expectEqualSlices(u16, &.{3}, t.frontiersAbove(&.{ 3, 4 }, 450, &out));
    t.resetFrontier(3, 10);
    try testing.expectEqual(@as(u64, 10), t.frontierOf(3));
    t.abortRelease(3);
    t.closeForCutover(3);
    try testing.expectEqual(State.migrating, t.view(3).state);
    try testing.expectEqual(@as(?u16, 3), t.closedSlot());
    t.reopenCutover(3);
    try testing.expect(t.view(3).leasedBy(1));
    try testing.expectEqual(@as(?u16, null), t.closedSlot());
}

test "holder ops over two half-window buckets: counts inside the window, cur → prev at one rotation, zeroed at two; load from tree 0 mirrors every variant; dead marks refuse" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    const half: u64 = 500;
    const t_idle = 2 * half;
    t.noteHolderOp(5, 100, t_idle);
    t.noteHolderOp(5, 200, t_idle);
    try testing.expectEqual(@as(u64, 2), t.holderOps(5, 300, t_idle));
    t.noteHolderOp(5, 600, t_idle);
    try testing.expectEqual(@as(u64, 3), t.holderOps(5, 700, t_idle));
    // Read one epoch later without a note: the old cur stands alone.
    try testing.expectEqual(@as(u64, 1), t.holderOps(5, 1200, t_idle));
    try testing.expectEqual(@as(u64, 0), t.holderOps(5, 1700, t_idle));
    t.noteHolderOp(5, 1700, t_idle);
    try testing.expectEqual(@as(u64, 1), t.holderOps(5, 1800, t_idle));
    t.resetHolderOps(5);
    try testing.expectEqual(@as(u64, 0), t.holderOps(5, 1800, t_idle));
    // Tree 0's three variants.
    try t.load(9, .{ .variant = .leased, .appender_id = 4, .g = 6, .root = .{ .addr = 1, .seq = 2 }, .cursor = 30, .slot_tree_extents = 2, .seq_floor = 11 });
    try testing.expect(t.view(9).leasedBy(4));
    try testing.expectEqual(@as(u32, 6), t.gOf(9));
    try testing.expectEqual(@as(u64, 30), t.wordsOf(9).cursor);
    try t.load(9, .{ .variant = .releasing, .appender_id = 4, .g = 6, .root = .{ .addr = 1, .seq = 2 }, .cursor = 30 });
    try testing.expect(t.view(9).dead and t.view(9).state == .releasing);
    try testing.expectEqual(State.releasing, (try t.acquire(9, 1, false)).refused.state);
    try t.load(9, .{ .variant = .unleased, .g = 6, .root = .{ .addr = 1, .seq = 2 }, .cursor = 31, .last_written = 44 });
    try testing.expectEqual(State.unleased, t.view(9).state);
    try testing.expectEqual(@as(u64, 44), t.lastWritten(9));
    try testing.expectEqual(@as(u64, 0), t.leasedCount());
}

test "pick prefers never-written, then the least recently written, ties by index; leased candidates are skipped; the ask is bounded by want" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    const words: SlotWords = .{};
    // 10 and 12 were written (12 earlier), 11 leased, 13/14 never.
    _ = try t.acquire(12, 1, false);
    _ = try t.release(12, 1, 1, words, 0);
    _ = try t.acquire(10, 1, false);
    _ = try t.release(10, 1, 1, words, 0);
    _ = try t.acquire(11, 1, false);
    var out: [8]u16 = undefined;
    try testing.expectEqualSlices(u16, &.{ 13, 14, 12, 10 }, t.pickUnleased(&.{ 10, 11, 12, 13, 14 }, &out));
    try testing.expectEqualSlices(u16, &.{ 13, 14 }, t.pickUnleased(&.{ 10, 11, 12, 13, 14 }, out[0..2]));
    try testing.expectEqualSlices(u16, &.{12}, t.pickUnleased(&.{ 10, 11, 12 }, out[0..1]));
}

test "settle: row 5 completes the release from the page's words; row 6 drops; a Live entry below g is stale; at g under another lessee a conflict; at g under the owner adopted; one slot Live on two pages is named" {
    const gpa = testing.allocator;
    const live: page.SlotEntry = .{ .slot = 5, .state = .live, .g = 3, .slot_tree_extents = 2, .root_addr = 4096, .root_seq = 9, .cursor = 40 };
    const rel: page.SlotEntry = .{ .slot = 5, .state = .releasing, .g = 3, .slot_tree_extents = 2, .root_addr = 4096, .root_seq = 9, .cursor = 40 };
    const leased_me: cr.SlotState = .{ .variant = .leased, .appender_id = 1, .g = 3, .root = .{ .addr = 1, .seq = 1 }, .cursor = 2 };
    const leased_other: cr.SlotState = .{ .variant = .leased, .appender_id = 2, .g = 3, .root = .{ .addr = 1, .seq = 1 }, .cursor = 2 };
    const unleased: cr.SlotState = .{ .variant = .unleased, .g = 3, .root = .{ .addr = 1, .seq = 1 }, .cursor = 2 };
    const r5 = settleEntry(leased_me, rel, 1);
    try testing.expectEqual(@as(u32, 3), r5.complete_release.g);
    try testing.expectEqual(@as(u64, 40), r5.complete_release.words.cursor);
    try testing.expectEqual(Settle.drop_entry, settleEntry(unleased, rel, 1));
    try testing.expectEqual(Settle.drop_entry, settleEntry(leased_other, rel, 1));
    try testing.expectEqual(Settle.drop_entry, settleEntry(null, rel, 1));
    var newer = leased_me;
    newer.g = 4;
    try testing.expectEqual(Settle.stale_entry, settleEntry(newer, live, 1));
    try testing.expectEqual(Settle.stale_entry, settleEntry(unleased, live, 1));
    try testing.expectEqual(Settle.stale_entry, settleEntry(null, live, 1));
    try testing.expectEqual(Settle.conflict, settleEntry(leased_other, live, 1));
    try testing.expectEqual(@as(u64, 9), settleEntry(leased_me, live, 1).adopt.words.root.seq);
    var other_live = live;
    other_live.slot = 6;
    try testing.expectEqual(@as(?u16, null), try liveOnTwoPages(gpa, &.{ .{ .owner = 1, .entries = &.{live} }, .{ .owner = 2, .entries = &.{ other_live, rel } } }));
    try testing.expectEqual(@as(?u16, 5), try liveOnTwoPages(gpa, &.{ .{ .owner = 1, .entries = &.{live} }, .{ .owner = 2, .entries = &.{live} } }));
}
