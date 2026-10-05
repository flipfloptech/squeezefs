//! The node lifecycle word (charter §5.3 "Node cache"/"Writeback"/"SMOs",
//! §9 "node lifecycle"; porting spec `docs/porting/kv_node.md` §4): the
//! `clean → dirty → serializing → superseded` state machine as ONE
//! `std.atomic.Value(u64)`. The node cache drives it under the per-node
//! write lock; the atomic half lives here so that every race the lock does
//! NOT serialize (an SMO's `supersede`, the clock sweep's `tryEvict`, the
//! writeback's `endFreeze` outside the lock) is decided by the RMW total
//! order on the word alone. `node_state_core_model.zig` explores every
//! interleaving of those races with `model/explorer.zig`.
//!
//! | state | word | meaning |
//! |---|---|---|
//! | clean | 0 | nothing un-flushed; the ONLY evictable state |
//! | dirty | DIRTY | the open delta holds records not yet frozen |
//! | serializing | FREEZING (± DIRTY) | a frozen image is in flight OUTSIDE the lock; DIRTY re-set = commits landed in the fresh delta meanwhile |
//! | superseded | SUPERSEDED (terminal) | severed from the cache (SMO swap or eviction); readers keep snapshots, writers revalidate-and-retry |
//!
//! Why the atomics alone forbid lost updates: every transition is one RMW,
//! so whichever lands first decides — an applied record is either visible
//! to the SMO (`supersede` reports `was_dirty`) or was never accepted
//! (`markDirty` refuses on SUPERSEDED); eviction is a CAS from the exact
//! clean word, so it never wins against accepted dirt; the freeze clears
//! DIRTY and sets FREEZING in one RMW, so a racing apply lands in the frozen
//! delta or re-sets DIRTY for the fresh one — never in neither.
//!
//! No allocator, no I/O. The two tripwires are the only dependency.

const std = @import("std");
const counters = @import("../core/counters.zig");

pub const Word = u64;

/// Bit 0: the open delta holds records not yet frozen into a bset image. One
/// bit per fact so every transition is ONE RMW on ONE word.
pub const dirty_bit: Word = 1 << 0;
/// Bit 1: a frozen delta image is in flight OUTSIDE the lock (§5.3 "Writeback"
/// freezes under the lock and appends outside it).
pub const freezing_bit: Word = 1 << 1;
/// Bit 2: terminal — the in-RAM object no longer represents the node (SMO
/// swap or eviction). Highest bit so the decode's dominance order is the
/// numeric one.
pub const superseded_bit: Word = 1 << 2;

/// The decoded lifecycle state (`decode` for the dominance order).
pub const LifecycleState = enum(u8) {
    /// No un-flushed records; evictable.
    clean,
    /// Un-flushed records in the open delta; pinned in cache.
    dirty,
    /// A frozen image is being written outside the lock; commits keep
    /// landing in a fresh open delta meanwhile.
    serializing,
    /// Severed from the cache — writers revalidate-and-retry, readers keep
    /// their snapshots.
    superseded,
};

/// `beginFreeze`/`beginForcedFreeze` refused. Protocol misuse is a bug in
/// the serialized writeback/SMO task; the core stays panic-free and types it.
pub const FreezeRefusal = error{
    /// Nothing to freeze: the open delta is empty (ordinary freeze only).
    NotDirty,
    /// A freeze is already in flight — one at a time.
    AlreadyFreezing,
    /// The node is superseded; there is nothing left to write back.
    Superseded,
};

/// What `supersede` displaced — the SMO carries both into its successor
/// build: `was_dirty` ⇒ the open delta needs the bounded second merge;
/// `was_freezing` ⇒ a frozen image is still in flight (asserted for
/// freeze-borne sources).
pub const SupersedeOutcome = struct {
    was_dirty: bool,
    was_freezing: bool,
};

/// `endFreeze` with FREEZING clear: an unpaired end on the serialized task
/// (must-stay-0; the word is still cleared so the node stays flushable).
pub var end_freeze_unarmed: counters.Tripwire = .init("kv_node_end_freeze_unarmed");
/// `abortFreeze` with FREEZING clear: an unpaired abort (must-stay-0; DIRTY
/// is still set so the restored records stay accounted).
pub var abort_freeze_unarmed: counters.Tripwire = .init("kv_node_abort_freeze_unarmed");

/// SUPERSEDED dominates, then FREEZING (a serializing node with re-accumulated
/// dirt is still serializing), then DIRTY.
pub fn decode(w: Word) LifecycleState {
    if (w & superseded_bit != 0) return .superseded;
    if (w & freezing_bit != 0) return .serializing;
    if (w & dirty_bit != 0) return .dirty;
    return .clean;
}

/// The packed lifecycle word. Every RMW is `acq_rel`, every load `acquire`:
/// the word publishes the delta swap `freeze_locked` performs under the lock
/// to the actors that run outside it.
pub const NodeState = struct {
    word: std.atomic.Value(Word) = .init(0),

    /// A fresh clean node.
    pub const clean: NodeState = .{};

    pub fn state(self: *const NodeState) LifecycleState {
        // acquire: pairs with the release half of every RMW below.
        return decode(self.word.load(.acquire));
    }

    /// Whether the open delta holds un-flushed records.
    pub fn isDirty(self: *const NodeState) bool {
        return self.word.load(.acquire) & dirty_bit != 0;
    }

    /// Whether a frozen delta image is in flight.
    pub fn isFreezing(self: *const NodeState) bool {
        return self.word.load(.acquire) & freezing_bit != 0;
    }

    /// The revalidation predicate writers check under the node lock.
    pub fn isSuperseded(self: *const NodeState) bool {
        return self.word.load(.acquire) & superseded_bit != 0;
    }

    /// A commit applied records to the open delta: set DIRTY — unless the
    /// node was superseded first, in which case NOTHING was accepted and the
    /// caller re-resolves through the cache. One RMW: the superseded check
    /// and the dirty set cannot be split by a racing `supersede`/`tryEvict`
    /// (the split is the weakening `node_state_core_model.zig` proves fails).
    pub fn markDirty(self: *NodeState) error{Superseded}!void {
        var w = self.word.load(.acquire);
        while (true) {
            if (w & superseded_bit != 0) return error.Superseded;
            // acq_rel CAS paired with `supersede`'s acq_rel fetchOr and
            // `tryEvict`'s acq_rel CAS: the RMW order on the word decides the
            // race; release publishes this apply to the next freeze/supersede.
            w = self.word.cmpxchgWeak(w, w | dirty_bit, .acq_rel, .acquire) orelse return;
        }
    }

    /// Begin the freeze-swap: clear DIRTY and set FREEZING in ONE RMW. The
    /// caller (holding the node write lock) swaps the open delta out as the
    /// frozen image, releases the lock, and writes it — never I/O under the
    /// lock. Applies after the swap re-set DIRTY for the fresh delta.
    /// Refuses in order: Superseded, AlreadyFreezing, NotDirty.
    pub fn beginFreeze(self: *NodeState) FreezeRefusal!void {
        var w = self.word.load(.acquire);
        while (true) {
            if (w & superseded_bit != 0) return error.Superseded;
            if (w & freezing_bit != 0) return error.AlreadyFreezing;
            if (w & dirty_bit == 0) return error.NotDirty;
            // acq_rel CAS paired with `markDirty`: acquire sees the applies
            // the frozen image must carry, release publishes the swap.
            w = self.word.cmpxchgWeak(w, (w & ~dirty_bit) | freezing_bit, .acq_rel, .acquire) orelse return;
        }
    }

    /// Enter FREEZING with no open delta to swap (clean, or dirty with an
    /// already-empty overlay): the merge of a clean sibling and the D4
    /// forced-compaction nudge. The SMO's `supersede`/`endFreeze` bookkeeping
    /// then sees exactly what a freeze-borne source carries, with an EMPTY
    /// frozen delta by construction. Never refuses `NotDirty`.
    pub fn beginForcedFreeze(self: *NodeState) FreezeRefusal!void {
        var w = self.word.load(.acquire);
        while (true) {
            if (w & superseded_bit != 0) return error.Superseded;
            if (w & freezing_bit != 0) return error.AlreadyFreezing;
            // acq_rel CAS paired with `markDirty` exactly as `beginFreeze`.
            w = self.word.cmpxchgWeak(w, (w & ~dirty_bit) | freezing_bit, .acq_rel, .acquire) orelse return;
        }
    }

    /// The frozen image reached its destination (append committed, or the
    /// SMO consumed it): clear FREEZING. Returns whether DIRTY re-accumulated
    /// meanwhile — the writeback task's re-enqueue signal. Legal on a
    /// superseded node (the SMO tidies up after the swap).
    pub fn endFreeze(self: *NodeState) bool {
        // acq_rel fetchAnd paired with `markDirty`: acquire sees the applies
        // that re-set DIRTY during the write, release lets the next
        // `beginFreeze` see the image is out of flight.
        const prev = self.word.fetchAnd(~freezing_bit, .acq_rel);
        if (prev & freezing_bit == 0) end_freeze_unarmed.fire("endFreeze without beginFreeze");
        return prev & dirty_bit != 0;
    }

    /// The frozen image could not be written and the caller restored it to
    /// the open delta: clear FREEZING, set DIRTY — the records are accounted
    /// dirty again and a later cycle retries.
    pub fn abortFreeze(self: *NodeState) void {
        var w = self.word.load(.acquire);
        while (true) {
            // acq_rel CAS paired with `beginFreeze`: release republishes the
            // restored records as dirt to the next freeze.
            const next = self.word.cmpxchgWeak(w, (w & ~freezing_bit) | dirty_bit, .acq_rel, .acquire) orelse break;
            w = next;
        }
        if (w & freezing_bit == 0) abort_freeze_unarmed.fire("abortFreeze without beginFreeze");
    }

    /// Terminal transition: the SMO swapped the cache mapping to the
    /// successor(s). Reports what was displaced AT THE RMW INSTANT —
    /// `was_dirty` records the successor build must carry, `was_freezing` a
    /// frozen image still in flight. Errors if already superseded (a double
    /// swap is an SMO serialization bug, surfaced loud but panic-free).
    pub fn supersede(self: *NodeState) error{Superseded}!SupersedeOutcome {
        // acq_rel fetchOr paired with `markDirty`'s acq_rel CAS: acquire sees
        // the overlay records an accepted apply wrote under the lock, so
        // `was_dirty` and the delta the SMO carries agree.
        const prev = self.word.fetchOr(superseded_bit, .acq_rel);
        if (prev & superseded_bit != 0) return error.Superseded;
        return .{
            .was_dirty = prev & dirty_bit != 0,
            .was_freezing = prev & freezing_bit != 0,
        };
    }

    /// Clock eviction's gate: sever the object ONLY from the exact clean
    /// word — one CAS `0 → SUPERSEDED`, so eviction never wins against a node
    /// that just accepted dirt or holds an in-flight freeze, and a writer
    /// that lost fails `markDirty` loud and re-resolves. (A `fetchOr` here is
    /// the weakening the model proves fails.)
    pub fn tryEvict(self: *NodeState) bool {
        // acq_rel CAS paired with `markDirty` and `endFreeze`: acquire sees
        // the last writer's release before the object is freed.
        return self.word.cmpxchgStrong(0, superseded_bit, .acq_rel, .acquire) == null;
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;
const log = @import("../core/log.zig");

test "decode: SUPERSEDED dominates FREEZING dominates DIRTY" {
    try testing.expectEqual(LifecycleState.clean, decode(0));
    try testing.expectEqual(LifecycleState.dirty, decode(dirty_bit));
    try testing.expectEqual(LifecycleState.serializing, decode(freezing_bit));
    try testing.expectEqual(LifecycleState.serializing, decode(freezing_bit | dirty_bit));
    try testing.expectEqual(LifecycleState.superseded, decode(superseded_bit));
    try testing.expectEqual(LifecycleState.superseded, decode(superseded_bit | freezing_bit | dirty_bit));
}

test "node lifecycle word transitions: clean → dirty → serializing → dirty, then superseded is terminal" {
    var st: NodeState = .clean;
    try testing.expectEqual(LifecycleState.clean, st.state());
    try testing.expectError(error.NotDirty, st.beginFreeze());
    try testing.expect(!st.isDirty());

    try st.markDirty();
    try testing.expectEqual(LifecycleState.dirty, st.state());
    try testing.expect(st.isDirty());

    try st.beginFreeze();
    try testing.expectEqual(LifecycleState.serializing, st.state());
    try testing.expect(st.isFreezing());
    try testing.expect(!st.isDirty());
    try testing.expectError(error.AlreadyFreezing, st.beginFreeze());
    try testing.expectError(error.AlreadyFreezing, st.beginForcedFreeze());

    // A commit during the write re-sets DIRTY; the node is still serializing.
    try st.markDirty();
    try testing.expectEqual(LifecycleState.serializing, st.state());
    try testing.expect(st.isDirty());

    // The append landed: the re-enqueue signal fires and the node is dirty.
    try testing.expect(st.endFreeze());
    try testing.expectEqual(LifecycleState.dirty, st.state());

    // A freeze whose image could not be written restores its records.
    try st.beginFreeze();
    st.abortFreeze();
    try testing.expectEqual(LifecycleState.dirty, st.state());

    // A clean write-out: no re-enqueue.
    try st.beginFreeze();
    try testing.expect(!st.endFreeze());
    try testing.expectEqual(LifecycleState.clean, st.state());

    // Supersede reports the pre-state; a second swap is the typed bug; applies
    // are refused forever after; the terminal bit survives an end_freeze.
    try st.markDirty();
    try st.beginFreeze();
    const outcome = try st.supersede();
    try testing.expect(!outcome.was_dirty);
    try testing.expect(outcome.was_freezing);
    try testing.expectError(error.Superseded, st.supersede());
    try testing.expectError(error.Superseded, st.markDirty());
    try testing.expectError(error.Superseded, st.beginFreeze());
    try testing.expectError(error.Superseded, st.beginForcedFreeze());
    try testing.expect(!st.endFreeze());
    try testing.expectEqual(LifecycleState.superseded, st.state());
    try testing.expect(st.isSuperseded());
}

test "tryEvict wins only from the exact clean word" {
    var st: NodeState = .clean;
    try st.markDirty();
    try testing.expect(!st.tryEvict());
    try st.beginFreeze();
    try testing.expect(!st.tryEvict());
    _ = st.endFreeze();
    try testing.expectEqual(LifecycleState.clean, st.state());
    try testing.expect(st.tryEvict());
    try testing.expectEqual(LifecycleState.superseded, st.state());
    try testing.expect(!st.tryEvict());
    try testing.expectError(error.Superseded, st.markDirty());
}

test "forced freeze enters FREEZING from clean with no delta to swap and reports as freeze-borne" {
    var st: NodeState = .clean;
    try st.beginForcedFreeze();
    try testing.expectEqual(LifecycleState.serializing, st.state());
    try testing.expect(!st.isDirty());
    try testing.expectError(error.AlreadyFreezing, st.beginForcedFreeze());
    const outcome = try st.supersede();
    try testing.expect(outcome.was_freezing);
    try testing.expect(!outcome.was_dirty);
    try testing.expect(!st.endFreeze());
    try testing.expect(!st.isFreezing());

    // From dirty it also clears DIRTY (the overlay was already empty).
    var dirty: NodeState = .clean;
    try dirty.markDirty();
    try dirty.beginForcedFreeze();
    try testing.expect(!dirty.isDirty());
    try testing.expect(dirty.isFreezing());
}

test "an unpaired endFreeze/abortFreeze fires its tripwire exactly once and still lands the word" {
    var st: NodeState = .clean;
    const before = counters.invariantTripwires();
    log.testing.expectErrorLogs(2);
    try testing.expect(!st.endFreeze());
    try testing.expectEqual(LifecycleState.clean, st.state());
    st.abortFreeze();
    try testing.expectEqual(LifecycleState.dirty, st.state());
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 2, counters.invariantTripwires());
    try testing.expectEqual(@as(u64, 1), counters.tripwireCount("kv_node_end_freeze_unarmed"));
    try testing.expectEqual(@as(u64, 1), counters.tripwireCount("kv_node_abort_freeze_unarmed"));
}

test {
    _ = @import("node_state_core_model.zig");
}
