//! The node cache's revalidation epoch and reader gate (charter §5.3 "Node
//! cache", §5.5 symmetric metadata; survey `docs/porting/
//! kv_tree_cache_commit_survey.md` §3.2, §7.1): the lock-free words that
//! make one node cache safe to share with a second process — a coherent
//! READER that polls the root ledger and drops every cached node the new
//! roots do not cover (`RevalidationEpoch`) and mutates nothing
//! (`ReaderGate`). Which appender may mutate a slot tree is the lease
//! gate's verdict (`node_cache.LeaseGate`); the pre-forest per-volume
//! appender partition is gone (decision 0003 Q3). `NodeEnv` is the
//! per-cache bundle every node shares; `StructuralHolds` is the hold-Σ
//! ledger the flush-ceiling audit reads. `epoch_core_model.zig` explores
//! the races with `model/explorer.zig`.
//!
//! ## The two-word publication law
//!
//! A revalidation publishes TWO facts — the new checkpoint epoch and the
//! new durable journal tail — and a loader must read them as a coherent
//! pair, because the tail is the torn-tail classifier's input (§5.3 "Node
//! cache": a later valid bset with `journal_seq_horizon ≤ durable tail`
//! fails the node loud; above it, drop silently). A node stamped "current as
//! of epoch E" whose tail came from an OLDER epoch may have silently dropped
//! a tail bset E's checkpoint covers — the silent-divergence class. The
//! reverse pairing (older epoch, newer tail) is merely strict: the
//! classifier is louder than necessary and the node is dropped at the next
//! poll anyway. So the orders are pinned in opposite directions:
//!
//! | side | order | ordering |
//! |---|---|---|
//! | publisher `publish` | tail, THEN epoch | release |
//! | loader `loadSnapshot` | epoch, THEN tail | acquire |
//!
//! Any epoch a loader observes was published before the tail it then
//! reads, so `snapshot.tail ≥ tail(snapshot.epoch)`. The model
//! `loader_never_sees_an_epoch_newer_than_its_tail` fails on either
//! reversal.
//!
//! ## The hit path
//!
//! `probe` is what every cache hit pays: one monotonic load compared with
//! the node's immutable epoch stamp. Monotonic suffices because a mismatch
//! is a MISS — the load path re-reads the pair with acquire. An un-armed
//! cache (every write mount) holds `unarmed_epoch`, stamps every node with
//! it, and never takes the branch.

const std = @import("std");

/// The epoch of a cache nobody armed — the shipped single-writer posture:
/// every node is stamped with it, so the hit-path compare always agrees and
/// no revalidation can drop a mapping the local mount owns.
pub const unarmed_epoch: u64 = 0;

/// A coherent read of the pair: the epoch a node loaded now may claim and
/// the durable tail its torn-tail classification must use.
pub const LoadSnapshot = struct {
    epoch: u64,
    tail: u64,
};

/// `publish` moved the epoch: `(previous, new)`.
pub const Published = struct {
    previous: u64,
    new: u64,
};

/// The revalidation epoch + durable journal tail of one node cache.
pub const RevalidationEpoch = struct {
    epoch_word: std.atomic.Value(u64),
    tail_word: std.atomic.Value(u64),

    /// Un-armed, seeded with the mount's durable tail.
    pub fn init(durable_tail: u64) RevalidationEpoch {
        return .{ .epoch_word = .init(unarmed_epoch), .tail_word = .init(durable_tail) };
    }

    /// The hit-path probe: one monotonic load (a mismatch is a miss; the
    /// miss path re-reads with acquire in `loadSnapshot`).
    pub inline fn probe(self: *const RevalidationEpoch) u64 {
        return self.epoch_word.load(.monotonic);
    }

    /// Whether a reader armed revalidation on this cache.
    pub inline fn isArmed(self: *const RevalidationEpoch) bool {
        return self.probe() != unarmed_epoch;
    }

    /// The durable journal tail (the torn-tail classifier's input).
    pub fn tail(self: *const RevalidationEpoch) u64 {
        // acquire: pairs with `advanceTail`/`publish`'s acq_rel fetchMax.
        return self.tail_word.load(.acquire);
    }

    /// Advance the tail alone — the write mount's `set_durable_tail` and the
    /// reader's seed at arm time. Monotone.
    pub fn advanceTail(self: *RevalidationEpoch, t: u64) void {
        // acq_rel fetchMax: pairs with `tail`'s and `loadSnapshot`'s acquire
        // loads; monotone so a late advance never moves the tail back.
        _ = self.tail_word.fetchMax(t, .acq_rel);
    }

    /// Arm at `epoch` (the mounted ledger record's seq). `false` if already
    /// armed or `epoch == unarmed_epoch` — arming is a once-per-mount
    /// declaration, not a mutable mode.
    pub fn arm(self: *RevalidationEpoch, epoch: u64) bool {
        if (epoch == unarmed_epoch) return false;
        // release CAS: pairs with `loadSnapshot`'s acquire epoch load.
        return self.epoch_word.cmpxchgStrong(unarmed_epoch, epoch, .release, .monotonic) == null;
    }

    /// Publish a newer checkpoint: TAIL FIRST, THEN THE EPOCH — the two-word
    /// law. Reversing the two stores (epoch, then tail) fails the model
    /// `loader_never_sees_an_epoch_newer_than_its_tail`: a loader reads the
    /// new epoch and the old tail and classifies against a tail the epoch
    /// does not vouch for. Returns the move, or null when the record is not
    /// newer (an inert poll; a racing poller's sweep covers ours) or the
    /// cache is un-armed (a write mount is never dropped into revalidation
    /// by a stray call).
    pub fn publish(self: *RevalidationEpoch, t: u64, epoch: u64) ?Published {
        var cur = self.epoch_word.load(.monotonic);
        if (cur == unarmed_epoch or epoch <= cur) return null;
        // The tail must be visible BEFORE the epoch that vouches for it:
        // acq_rel fetchMax paired with the loader's acquire tail load.
        _ = self.tail_word.fetchMax(t, .acq_rel);
        while (true) {
            // release CAS paired with the loader's acquire epoch load: an
            // observed epoch implies the tail store above is visible.
            const observed = self.epoch_word.cmpxchgWeak(cur, epoch, .release, .monotonic) orelse
                return .{ .previous = cur, .new = epoch };
            // A concurrent poller published this epoch or newer: its sweep covers ours.
            if (observed >= epoch) return null;
            cur = observed;
        }
    }

    /// The load-path snapshot: EPOCH FIRST, THEN THE TAIL, both acquire, so
    /// the pair is never inverted (reading the tail first fails the same
    /// model from the loader's side).
    pub fn loadSnapshot(self: *const RevalidationEpoch) LoadSnapshot {
        const epoch = self.epoch_word.load(.acquire);
        const t = self.tail_word.load(.acquire);
        return .{ .epoch = epoch, .tail = t };
    }
};

/// The ONE RAM-mutation gate `apply_locked` pays a single monotonic load
/// for: a mount that declared itself a coherent READER mutates no node (its
/// cache is a projection of somebody else's tree).
pub const ReaderGate = struct {
    reader: std.atomic.Value(bool) = .init(false),

    /// Not a reader: the write mount's posture.
    pub const writer: ReaderGate = .{};

    pub inline fn isReader(self: *const ReaderGate) bool {
        return self.reader.load(.monotonic);
    }

    /// Declare this mount a coherent reader (idempotent; once per mount,
    /// before any mutation it gates).
    pub fn setReader(self: *ReaderGate) void {
        self.reader.store(true, .release);
    }
};

/// The class of a structural hold of the SMO mutex — the two actors whose
/// hold time the flush-ceiling audit excludes from a leaf's age.
pub const HoldClass = enum(u1) {
    /// A dead appender's recovery (its excusable overlap is capped at the
    /// published recovery bound).
    recovery = 0,
    /// The manager's or a joiner's SERVICE of the fleet under the mutex
    /// (slot grants/releases, adoptions, projection refreshes).
    service = 1,
};

/// Per class: the Σ of completed hold time, the active hold's start (0 =
/// none) and its nesting depth (a class's guards may nest; the outermost
/// brackets the hold). Both classes hold the ONE SMO mutex, so the Σs are
/// exact and never double count. A leaf stamps the Σ at its clean → dirty
/// transition; the audit's `Σ(now) − Σ(stamp)` is exactly the hold time
/// overlapping the leaf's dirty window.
pub const StructuralHolds = struct {
    held_ns: [2]std.atomic.Value(u64) = @splat(.init(0)),
    active_since_ns: [2]std.atomic.Value(u64) = @splat(.init(0)),
    depth: [2]std.atomic.Value(u32) = @splat(.init(0)),

    pub const none: StructuralHolds = .{};

    /// A hold of `class` began at `now_ns` (CLOCK_MONOTONIC); a nested guard
    /// of an active hold only deepens it.
    pub fn begin(self: *StructuralHolds, class: HoldClass, now_ns: u64) void {
        const i = @backingInt(class);
        // acq_rel fetchAdd paired with `end`'s acq_rel fetchSub: the
        // outermost guard is the one that observed depth 0.
        if (self.depth[i].fetchAdd(1, .acq_rel) == 0) {
            // `max(now, 1)`: 0 means "no active hold" to `heldNs`.
            // release: pairs with `heldNs`'s acquire load.
            self.active_since_ns[i].store(@max(now_ns, 1), .release);
        }
    }

    /// A guard of `class` dropped at `now_ns`: the outermost one's duration
    /// joins the Σ.
    pub fn end(self: *StructuralHolds, class: HoldClass, now_ns: u64) void {
        const i = @backingInt(class);
        if (self.depth[i].fetchSub(1, .acq_rel) == 1) {
            // acq_rel swap: closes the active window before the Σ grows, so
            // `heldNs` never counts the same span twice.
            const since = self.active_since_ns[i].swap(0, .acq_rel);
            if (since != 0) _ = self.held_ns[i].fetchAdd(now_ns -| since, .acq_rel);
        }
    }

    /// The Σ of `class` hold time up to `now_ns`, the active hold's elapsed
    /// part included — the stamp a leaf takes at its dirty transition and
    /// the value the audit compares it against.
    pub fn heldNs(self: *const StructuralHolds, class: HoldClass, now_ns: u64) u64 {
        const i = @backingInt(class);
        const completed = self.held_ns[i].load(.acquire);
        const since = self.active_since_ns[i].load(.acquire);
        return if (since != 0) completed +| (now_ns -| since) else completed;
    }

    /// Both classes' Σ at `now_ns` (`[recovery, service]`).
    pub fn snapshot(self: *const StructuralHolds, now_ns: u64) [2]u64 {
        return .{ self.heldNs(.recovery, now_ns), self.heldNs(.service, now_ns) };
    }
};

/// The per-cache environment every cached node shares: the revalidation
/// pair, the reader gate and the hold ledger. One per cache, handed to
/// every node at construction — the node needs it to answer "may I be
/// mutated?" at the choke point, the cache to answer "is this node still
/// current?" on every hit.
pub const NodeEnv = struct {
    epoch: RevalidationEpoch,
    gate: ReaderGate,
    holds: StructuralHolds,

    pub fn init(durable_tail: u64) NodeEnv {
        return .{ .epoch = .init(durable_tail), .gate = .writer, .holds = .none };
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "epoch: arming is once per mount; publish is inert un-armed and on a non-newer record" {
    var ep: RevalidationEpoch = .init(10);
    try testing.expect(!ep.isArmed());
    try testing.expectEqual(@as(u64, 10), ep.tail());
    try testing.expectEqual(@as(?Published, null), ep.publish(20, 2));
    try testing.expectEqual(@as(u64, 10), ep.tail());
    try testing.expect(!ep.arm(unarmed_epoch));
    try testing.expect(ep.arm(1));
    try testing.expect(!ep.arm(2));
    try testing.expectEqual(@as(u64, 1), ep.probe());

    try testing.expectEqual(Published{ .previous = 1, .new = 2 }, ep.publish(20, 2).?);
    try testing.expectEqual(LoadSnapshot{ .epoch = 2, .tail = 20 }, ep.loadSnapshot());
    try testing.expectEqual(@as(?Published, null), ep.publish(25, 2));
    try testing.expectEqual(@as(?Published, null), ep.publish(25, 1));
    // An inert publish never touches the tail; the tail is monotone.
    try testing.expectEqual(@as(u64, 20), ep.tail());
    ep.advanceTail(15);
    try testing.expectEqual(@as(u64, 20), ep.tail());
    ep.advanceTail(30);
    try testing.expectEqual(@as(u64, 30), ep.tail());
}

test "gate: a write mount is no reader; the reader declaration is idempotent and sticky" {
    var gate: ReaderGate = .writer;
    try testing.expect(!gate.isReader());
    gate.setReader();
    gate.setReader();
    try testing.expect(gate.isReader());
}

test "structural holds: the outermost guard brackets a nested hold; Σ is exact and includes the active part" {
    var holds: StructuralHolds = .none;
    try testing.expectEqual([2]u64{ 0, 0 }, holds.snapshot(100));
    holds.begin(.service, 100);
    holds.begin(.service, 120); // nested: does not restart the window
    try testing.expectEqual(@as(u64, 50), holds.heldNs(.service, 150));
    holds.end(.service, 160); // inner guard: still held
    try testing.expectEqual(@as(u64, 70), holds.heldNs(.service, 170));
    holds.end(.service, 200); // outermost: Σ += 100
    try testing.expectEqual(@as(u64, 100), holds.heldNs(.service, 999));
    try testing.expectEqual(@as(u64, 0), holds.heldNs(.recovery, 999));
    // A hold at monotonic 0 is still an active hold (the floor of 1).
    holds.begin(.recovery, 0);
    try testing.expectEqual(@as(u64, 9), holds.heldNs(.recovery, 10));
    holds.end(.recovery, 10);
    try testing.expectEqual([2]u64{ 9, 100 }, holds.snapshot(10));
}

test "node env: un-armed, a writer, no holds" {
    const env: NodeEnv = .init(7);
    try testing.expect(!env.epoch.isArmed());
    try testing.expectEqual(@as(u64, 7), env.epoch.tail());
    try testing.expect(!env.gate.isReader());
    try testing.expectEqual([2]u64{ 0, 0 }, env.holds.snapshot(1));
}

test {
    _ = @import("epoch_core_model.zig");
}
