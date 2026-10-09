//! The slot state machine over a shared `layout.IpcSlot` (charter §7; spec
//! `docs/porting/il_session.md` §3):
//!
//!     FREE ─claim─▶ CLAIMED ─publish─▶ SUBMITTED ─serve─▶ SERVING ─complete─▶ DONE ─release─▶ FREE
//!                     └──────────────── releaseClaimed ──────────────────────────────────────▶ FREE
//!
//! plus the WAITER bit (bit 8) a parked client ORs in, and the generation
//! (bumped at claim) that attributes a DONE to exactly one life of the slot.
//! Every transition names its ordering and the fence it pairs with, on the
//! line. The protocol word is the per-slot futex word (the shared face,
//! `sync/futex.zig`: never `FUTEX_PRIVATE` — the word lives in a MAP_SHARED
//! memfd). `slot_core_model.zig` checks the three cores exhaustively and
//! proves each recorded weakening red.
//!
//! The daemon's guarantee to itself: memory safety and bounded behaviour,
//! never semantic validity — `tryBeginServe` refusing is a protocol
//! violation the caller POISONS (never a panic), and every descriptor word
//! is read exactly once into a private copy (`snapshot`) after the Acquire
//! edge that licenses it.

const std = @import("std");
const layout = @import("layout.zig");
const futex = @import("../sync/futex.zig");

const Atomic = std.atomic.Value;
const IpcSlot = layout.IpcSlot;

pub const FREE: u32 = 0;
pub const CLAIMED: u32 = 1;
pub const SUBMITTED: u32 = 2;
pub const SERVING: u32 = 3;
pub const DONE: u32 = 4;
/// A parked client's registration; cleared by `complete`'s swap and by
/// every store of a plain state.
pub const WAITER: u32 = 1 << 8;

pub fn stateBits(w: u32) u32 {
    return w & 0xff;
}

/// The futex word of a slot (the shared face parks on it).
pub fn word(slot: *IpcSlot) *futex.Word {
    return &slot.core.state;
}

// ---------------------------------------------------------------------------
// Client side.
// ---------------------------------------------------------------------------

/// FREE → CLAIMED: `cmpxchg(FREE, CLAIMED, .acquire)` pairs with `release`'s
/// Release store, so the new claimant's descriptor writes are ordered after
/// the previous consumer's result read. Returns this life's generation: the
/// bump is Relaxed because it is sequenced before `publishSubmitted`'s
/// Release and every DONE observer Acquire-loads a word in that release
/// sequence — whoever sees this life's DONE sees this generation.
pub fn tryClaim(slot: *IpcSlot) ?u64 {
    if (slot.core.state.cmpxchgStrong(FREE, CLAIMED, .acquire, .monotonic) != null) return null;
    return slot.core.generation.fetchAdd(1, .monotonic) + 1;
}

pub const Descriptor = struct {
    op: layout.Op,
    flags: u32 = 0,
    binding: u64,
    offset: u64,
    len: u32,
    arena_off: u64,
};

/// Relaxed stores: the state edge (`publishSubmitted`) orders them.
pub fn publishDescriptor(slot: *IpcSlot, d: Descriptor) void {
    slot.op.store(@backingInt(d.op), .monotonic);
    slot.flags.store(d.flags, .monotonic);
    slot.binding.store(d.binding, .monotonic);
    slot.offset.store(d.offset, .monotonic);
    slot.len.store(d.len, .monotonic);
    slot.arena_off.store(d.arena_off, .monotonic);
}

/// Display-only residence instrument (`layout.ingressDeltaNs`).
pub fn stampIngress(slot: *IpcSlot, now_ns: u64) void {
    slot.ingress_stamp_ns.store(@truncate(now_ns), .monotonic);
}

/// CLAIMED → SUBMITTED: a plain `store(SUBMITTED, .release)` is exact
/// (WAITER cannot be set on an unsubmitted slot); pairs with
/// `tryBeginServe`'s Acquire so the daemon's snapshot sees every descriptor
/// write.
pub fn publishSubmitted(slot: *IpcSlot) void {
    slot.core.state.store(SUBMITTED, .release);
}

/// DONE → FREE after the result (and the payload) were consumed:
/// `store(FREE, .release)` pairs with the next `tryClaim`'s Acquire and
/// clears a late parker's WAITER with the rest of the word.
pub fn release(slot: *IpcSlot) void {
    slot.core.state.store(FREE, .release);
}

/// CLAIMED → FREE: the arena-extension hold (a large op claims a contiguous
/// run, submits only the base, the extension slots reserve arena the daemon
/// never observes) is returned without ever being published.
pub fn releaseClaimed(slot: *IpcSlot) void {
    std.debug.assert(stateBits(slot.core.state.load(.monotonic)) == CLAIMED);
    slot.core.state.store(FREE, .release);
}

pub const ParkVerdict = union(enum) {
    /// DONE was already there: never park.
    ready,
    /// Park on the word while it still reads `expected`.
    park: u32,
};

/// Park entry: ONE `fetchOr(WAITER, .acq_rel)` does publish-then-recheck.
/// It lands before `complete`'s swap (the daemon reads the bit and wakes) or
/// after it (the prior reads DONE ⇒ ready); there is no third interleaving.
pub fn parkPrepare(slot: *IpcSlot) ParkVerdict {
    const prior = slot.core.state.fetchOr(WAITER, .acq_rel);
    if (stateBits(prior) == DONE) return .ready;
    return .{ .park = prior | WAITER };
}

/// Poll: state Acquire FIRST (synchronizes with `complete`'s Release half),
/// THEN the generation — once this life's DONE is visible so is its bump, so
/// a recycled slot's DONE is never attributed to an old generation.
pub fn isDoneFor(slot: *const IpcSlot, generation: u64) bool {
    if (stateBits(slot.core.state.load(.acquire)) != DONE) return false;
    return slot.core.generation.load(.monotonic) == generation;
}

/// The result after `isDoneFor` returned true (Relaxed: ordered by the DONE
/// Acquire the caller just performed).
pub fn result(slot: *const IpcSlot) i64 {
    return slot.result.load(.monotonic);
}

// ---------------------------------------------------------------------------
// Daemon side.
// ---------------------------------------------------------------------------

/// SUBMITTED → SERVING, PRESERVING WAITER (a client may be parked already):
/// a CAS loop `cur → SERVING | (cur & WAITER)`, Acquire on success — the
/// edge licensing the ONE descriptor snapshot. `false` when the state is not
/// SUBMITTED: a hostile or duplicate ring entry the caller poisons.
pub fn tryBeginServe(slot: *IpcSlot) bool {
    var cur = slot.core.state.load(.monotonic);
    while (true) {
        if (stateBits(cur) != SUBMITTED) return false;
        if (slot.core.state.cmpxchgWeak(cur, SERVING | (cur & WAITER), .acquire, .monotonic)) |seen| {
            cur = seen;
            continue;
        }
        return true;
    }
}

/// The ONE linearization read (spec §5.3.1 rule 1): every descriptor word
/// once, Relaxed (the `tryBeginServe` Acquire ordered them), into the
/// private copy the serve validates and works from. The 128 B slot is read
/// here and never again for this op.
pub fn snapshot(slot: *const IpcSlot) layout.SlotDescriptor {
    return .{
        .op = slot.op.load(.monotonic),
        .flags = slot.flags.load(.monotonic),
        .binding = slot.binding.load(.monotonic),
        .offset = slot.offset.load(.monotonic),
        .len = slot.len.load(.monotonic),
        .arena_off = slot.arena_off.load(.monotonic),
        .generation = slot.core.generation.load(.monotonic),
    };
}

/// Relaxed: published by `complete`'s Release half.
pub fn setResult(slot: *IpcSlot, r: i64) void {
    slot.result.store(r, .monotonic);
}

/// SERVING → DONE: `swap(DONE, .acq_rel)`. The Release half publishes the
/// result (and the generation, via the release sequence headed at
/// `publishSubmitted`); the Acquire half orders the WAITER decision. The
/// swap CLEARS WAITER — a woken client re-checks via `isDoneFor`, never via
/// the bit. Returns whether a waiter was registered: the caller then
/// `futex.wakeSharedAll(word)` (breadth INT_MAX: a split submitter/reaper
/// pair may both park on one word).
pub fn complete(slot: *IpcSlot) bool {
    const prior = slot.core.state.swap(DONE, .acq_rel);
    return prior & WAITER != 0;
}

/// `setResult` + `complete` + the wake, in the shipped order.
pub fn completeAndWake(slot: *IpcSlot, r: i64) void {
    setResult(slot, r);
    if (complete(slot)) futex.wakeSharedAll(word(slot));
}

// ---------------------------------------------------------------------------
// The lease-refcounted mapping (charter §7 "lease-refcounted unmap"): the
// daemon's teardown drops the registry's reference; every in-flight
// completion and arena window holds one; the LAST release unmaps, so an
// early unmap is unrepresentable (a late completion writes into live
// memory, never a freed mapping).
// ---------------------------------------------------------------------------

pub const SessionMap = struct {
    refs: Atomic(u32),
    mapping: layout.Mapping,
    /// What the last release runs (`shm.unmap` in the product; a counter in
    /// tests).
    unmap: *const fn (ctx: *anyopaque, m: layout.Mapping) void,
    ctx: *anyopaque,

    pub fn init(mapping: layout.Mapping, ctx: *anyopaque, unmap: *const fn (ctx: *anyopaque, m: layout.Mapping) void) SessionMap {
        return .{ .refs = .init(1), .mapping = mapping, .unmap = unmap, .ctx = ctx };
    }

    /// A new accessor (Relaxed: the caller already holds a reference that
    /// keeps the mapping alive; nothing is published by the count).
    pub fn retain(self: *SessionMap) void {
        const prior = self.refs.fetchAdd(1, .monotonic);
        std.debug.assert(prior != 0);
    }

    /// The accessor is done. `.acq_rel` on the decrement: the Release half
    /// orders this accessor's last arena/slot access before the count hits
    /// zero; the Acquire half (on the last release) orders the unmap after
    /// every OTHER accessor's released accesses — the pair every refcount
    /// drop names. Returns true when this release unmapped.
    pub fn releaseRef(self: *SessionMap) bool {
        if (self.refs.fetchSub(1, .acq_rel) != 1) return false;
        self.unmap(self.ctx, self.mapping);
        return true;
    }

    pub fn count(self: *const SessionMap) u32 {
        return self.refs.load(.monotonic);
    }
};

// ---------------------------------------------------------------------------
// Tests (spec §8: the slot_core rows).
// ---------------------------------------------------------------------------

const testing = std.testing;

fn freshSlot() IpcSlot {
    return .{};
}

test "full cycle walk and generation stamps: FREE → CLAIMED → SUBMITTED → SERVING → DONE → FREE; the generation is monotonic per life" {
    var s = freshSlot();
    const g1 = tryClaim(&s).?;
    try testing.expectEqual(@as(u64, 1), g1);
    try testing.expectEqual(@as(?u64, null), tryClaim(&s));
    publishDescriptor(&s, .{ .op = .read, .binding = 7, .offset = 4096, .len = 512, .arena_off = 0 });
    stampIngress(&s, 0x1_0000_1234);
    publishSubmitted(&s);
    try testing.expectEqual(SUBMITTED, stateBits(s.core.state.load(.monotonic)));
    try testing.expect(tryBeginServe(&s));
    const d = snapshot(&s);
    try testing.expectEqual(@as(u32, 1), d.op);
    try testing.expectEqual(@as(u64, 7), d.binding);
    try testing.expectEqual(@as(u64, 4096), d.offset);
    try testing.expectEqual(@as(u32, 512), d.len);
    try testing.expectEqual(g1, d.generation);
    try testing.expectEqual(@as(u32, 0x1234), s.ingress_stamp_ns.load(.monotonic));
    try testing.expect(!isDoneFor(&s, g1));
    setResult(&s, 512);
    try testing.expect(!complete(&s));
    try testing.expect(isDoneFor(&s, g1));
    try testing.expectEqual(@as(i64, 512), result(&s));
    release(&s);
    try testing.expectEqual(FREE, s.core.state.load(.monotonic));
    const g2 = tryClaim(&s).?;
    try testing.expectEqual(@as(u64, 2), g2);
    try testing.expect(!isDoneFor(&s, g1));
    try testing.expect(!isDoneFor(&s, g2));
}

test "releaseClaimed returns an unsubmitted hold to FREE; its generation never consumes a later life's DONE" {
    var s = freshSlot();
    const hold = tryClaim(&s).?;
    releaseClaimed(&s);
    try testing.expectEqual(FREE, s.core.state.load(.monotonic));
    const g = tryClaim(&s).?;
    try testing.expect(g > hold);
    publishSubmitted(&s);
    try testing.expect(tryBeginServe(&s));
    completeAndWake(&s, 0);
    try testing.expect(!isDoneFor(&s, hold));
    try testing.expect(isDoneFor(&s, g));
}

test "tryBeginServe refuses every non-SUBMITTED state (the poison verdict), never panics" {
    var s = freshSlot();
    for ([_]u32{ FREE, CLAIMED, SERVING, DONE, FREE | WAITER, CLAIMED | WAITER, SERVING | WAITER, DONE | WAITER }) |w| {
        s.core.state.store(w, .monotonic);
        try testing.expect(!tryBeginServe(&s));
        try testing.expectEqual(w, s.core.state.load(.monotonic));
    }
    s.core.state.store(SUBMITTED, .monotonic);
    try testing.expect(tryBeginServe(&s));
    try testing.expectEqual(SERVING, s.core.state.load(.monotonic));
}

test "the WAITER bit is preserved across tryBeginServe and reported (then cleared) by complete" {
    var s = freshSlot();
    _ = tryClaim(&s);
    publishSubmitted(&s);
    switch (parkPrepare(&s)) {
        .park => |expected| try testing.expectEqual(SUBMITTED | WAITER, expected),
        .ready => return error.TestUnexpectedReady,
    }
    try testing.expect(tryBeginServe(&s));
    try testing.expectEqual(SERVING | WAITER, s.core.state.load(.monotonic));
    try testing.expect(complete(&s));
    try testing.expectEqual(DONE, s.core.state.load(.monotonic));
}

test "parkPrepare after DONE is ready, not stranded; the one RMW is publish-then-recheck" {
    var s = freshSlot();
    const g = tryClaim(&s).?;
    publishSubmitted(&s);
    try testing.expect(tryBeginServe(&s));
    completeAndWake(&s, 9);
    try testing.expect(parkPrepare(&s) == .ready);
    try testing.expect(isDoneFor(&s, g));
}

test "a stale generation never sees a recycled slot's DONE" {
    var s = freshSlot();
    const g1 = tryClaim(&s).?;
    publishSubmitted(&s);
    try testing.expect(tryBeginServe(&s));
    completeAndWake(&s, 1);
    try testing.expect(isDoneFor(&s, g1));
    release(&s);
    const g2 = tryClaim(&s).?;
    publishSubmitted(&s);
    try testing.expect(tryBeginServe(&s));
    completeAndWake(&s, 2);
    try testing.expect(!isDoneFor(&s, g1));
    try testing.expect(isDoneFor(&s, g2));
}

test "cycle stress: 10 k lives on one slot, a server thread racing the consumer — exactly one DONE consumed per generation, every result its own" {
    var s = freshSlot();
    const lives: u32 = 10_000;
    const Server = struct {
        fn run(slot: *IpcSlot, n: u32) void {
            var served: u32 = 0;
            while (served < n) {
                if (!tryBeginServe(slot)) {
                    std.atomic.spinLoopHint();
                    continue;
                }
                const d = snapshot(slot);
                completeAndWake(slot, @intCast(d.offset));
                served += 1;
            }
        }
    };
    const t = try std.Thread.spawn(.{}, Server.run, .{ &s, lives });
    var i: u32 = 0;
    while (i < lives) : (i += 1) {
        const g = tryClaim(&s).?;
        publishDescriptor(&s, .{ .op = .read, .binding = 1, .offset = i, .len = 0, .arena_off = 0 });
        publishSubmitted(&s);
        while (!isDoneFor(&s, g)) std.atomic.spinLoopHint();
        try testing.expectEqual(@as(i64, i), result(&s));
        release(&s);
    }
    t.join();
    try testing.expectEqual(@as(u64, lives), s.core.generation.load(.monotonic));
}

const UnmapTally = struct {
    count: u32 = 0,
    fn unmap(ctx: *anyopaque, _: layout.Mapping) void {
        const self: *UnmapTally = @ptrCast(@alignCast(ctx));
        self.count += 1;
    }
};

test "SessionMap: the last release unmaps exactly once; accessors retained before the registry's drop keep the mapping alive" {
    var tally: UnmapTally = .{};
    var mem: [layout.PAGE_BYTES]u8 align(layout.PAGE_BYTES) = undefined;
    const l = try layout.SessionLayout.compute(.{ .ring_entries = 2, .slots = 1, .arena_bytes = 4096, .max_op_bytes = 4096 });
    var m = SessionMap.init(layout.Mapping.init(&mem, l), &tally, &UnmapTally.unmap);
    m.retain();
    m.retain();
    try testing.expectEqual(@as(u32, 3), m.count());
    // The registry drops first (teardown); two in-flight accessors remain.
    try testing.expect(!m.releaseRef());
    try testing.expectEqual(@as(u32, 0), tally.count);
    try testing.expect(!m.releaseRef());
    try testing.expectEqual(@as(u32, 0), tally.count);
    try testing.expect(m.releaseRef());
    try testing.expectEqual(@as(u32, 1), tally.count);
}
