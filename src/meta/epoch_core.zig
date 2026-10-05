//! The node cache's revalidation epoch and append gate (charter §5.3 "Node
//! cache", §5.5 symmetric metadata; survey `docs/porting/
//! kv_tree_cache_commit_survey.md` §3.2, §7.1): the lock-free words that
//! make one node cache safe to share with a second process — a coherent
//! READER that polls the root ledger and drops every cached node the new
//! roots do not cover (`RevalidationEpoch`), and the partitioning verdict
//! "two writers never cache the same node" (`AppendGate`). `NodeEnv` is the
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

/// Appender id of the volume's root authority — the one appender owning
/// structural state (roots, interior nodes, SMOs). Must equal the journal
/// module's root-authority writer id; the tie is that module's test, because
/// this one stays dependency-free.
pub const root_authority: u16 = 0;

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

/// The decoded `AppendGate` word — byte-exact with the packed layout: bits
/// 0..16 appender id, bits 16..31 appender count, bit 31 reader.
pub const GateState = packed struct(u32) {
    /// This mount's appender id.
    writer_id: u16,
    /// Appenders the volume's structures are partitioned for (0/1 = solo —
    /// today's every mount). 15 bits: bit 31 is the reader flag.
    writers: u15,
    /// This mount declared itself a coherent READER: it mutates no node.
    reader: bool,

    /// Whether this appender owns the volume's structural state — interior
    /// nodes, tree roots, SMOs.
    pub inline fn isAuthority(self: GateState) bool {
        return self.writer_id == root_authority;
    }

    /// A volume with exactly one appender: the shipped posture, where every
    /// partitioning check is structurally inert.
    pub inline fn isSolo(self: GateState) bool {
        return self.writers <= 1;
    }
};

const reader_bit: u32 = @bitCast(GateState{ .writer_id = 0, .writers = 0, .reader = true });
/// The largest appender count the 15-bit field holds.
pub const max_writers: u16 = std.math.maxInt(u15);

comptime {
    // The packed layout IS the wire layout the reference ships (reader bit
    // 31, writers 16..31, writer_id 0..16).
    std.debug.assert(reader_bit == 1 << 31);
    std.debug.assert(@as(u32, @bitCast(GateState{ .writer_id = 0, .writers = 1, .reader = false })) == 1 << 16);
    std.debug.assert(@as(u32, @bitCast(GateState{ .writer_id = 1, .writers = 0, .reader = false })) == 1);
}

/// One packed word carrying both mutation gates, so the ONE RAM-mutation
/// choke point (`apply_locked`) pays a single monotonic load instead of two:
/// reader ⇒ refuse every mutation; `level > 0 ∧ ¬authority` ⇒ refuse.
pub const AppendGate = struct {
    /// Word 0 = solo authority, not a reader: the shipped posture.
    word: std.atomic.Value(u32) = .init(0),

    pub const solo: AppendGate = .{};

    /// One monotonic load — the whole cost of both gates (a declaration
    /// happens once per mount, before any mutation it gates).
    pub inline fn load(self: *const AppendGate) GateState {
        return @bitCast(self.word.load(.monotonic));
    }

    /// Declare this mount a coherent reader (idempotent).
    pub fn setReader(self: *AppendGate) void {
        // acq_rel fetchOr paired with `setAppender`'s acq_rel CAS: the two
        // declarations compose on the one word, never clobber (model
        // `gate_declarations_never_clobber_each_other`).
        _ = self.word.fetchOr(reader_bit, .acq_rel);
    }

    /// Declare this mount appender `writer_id` of `writers`. Refuses an
    /// out-of-range partition (the count must fit its 15 bits, the id must
    /// be inside its own count, zero appenders is nonsense) — structural
    /// nonsense never reaches the partitioning checks.
    pub fn setAppender(self: *AppendGate, writers: u16, writer_id: u16) error{InvalidPartition}!void {
        if (writers == 0 or writers > max_writers or writer_id >= writers) return error.InvalidPartition;
        const bits: u32 = @bitCast(GateState{ .writer_id = writer_id, .writers = @intCast(writers), .reader = false });
        var cur = self.word.load(.monotonic);
        while (true) {
            // acq_rel CAS preserving the reader bit — a plain store here is
            // the weakening the model proves loses a racing `setReader`.
            cur = self.word.cmpxchgWeak(cur, (cur & reader_bit) | bits, .acq_rel, .monotonic) orelse return;
        }
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
/// pair, the mutation gates and the hold ledger. One per cache, handed to
/// every node at construction — the node needs it to answer "may I be
/// mutated?" at the choke point, the cache to answer "is this node still
/// current?" on every hit.
pub const NodeEnv = struct {
    epoch: RevalidationEpoch,
    gate: AppendGate,
    holds: StructuralHolds,

    pub fn init(durable_tail: u64) NodeEnv {
        return .{ .epoch = .init(durable_tail), .gate = .solo, .holds = .none };
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

test "gate: word 0 is the solo authority; declarations compose; refusals are typed" {
    var gate: AppendGate = .solo;
    const g0 = gate.load();
    try testing.expect(!g0.reader and g0.isSolo() and g0.isAuthority());
    try testing.expectError(error.InvalidPartition, gate.setAppender(0, 0));
    try testing.expectError(error.InvalidPartition, gate.setAppender(4, 4));
    try testing.expectError(error.InvalidPartition, gate.setAppender(max_writers + 1, 0));
    try gate.setAppender(4, 3);
    gate.setReader();
    gate.setReader();
    const g = gate.load();
    try testing.expect(g.reader);
    try testing.expectEqual(@as(u16, 4), @as(u16, g.writers));
    try testing.expectEqual(@as(u16, 3), g.writer_id);
    try testing.expect(!g.isAuthority() and !g.isSolo());
    try testing.expectEqual(@as(u32, (1 << 31) | (4 << 16) | 3), gate.word.load(.monotonic));
    // A later appender declaration keeps the reader bit.
    try gate.setAppender(max_writers, 0);
    try testing.expect(gate.load().reader and gate.load().isAuthority());
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

test "node env: un-armed, solo, no holds" {
    const env: NodeEnv = .init(7);
    try testing.expect(!env.epoch.isArmed());
    try testing.expectEqual(@as(u64, 7), env.epoch.tail());
    try testing.expect(env.gate.load().isSolo());
    try testing.expectEqual([2]u64{ 0, 0 }, env.holds.snapshot(1));
}

test {
    _ = @import("epoch_core_model.zig");
}
