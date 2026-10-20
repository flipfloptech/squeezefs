//! THE WRITE-LEASE RECALL — the lock-free core of the direct write arm
//! (charter §7 "The direct arm", decision 0003 §AX.5 → §AY): a client
//! writes the data device in place under a lease the daemon can RECALL,
//! and the daemon mutates the inode's layout only once no client write is
//! in flight. Two words per lease in client-writable shared memory (the
//! session's ledger): `inflight` (the client's count of device writes
//! between their issue and their completion) and `recall` (0 live, 1
//! recalled, 2 recalled and FORCED past the bound — the client's writes
//! after it may have landed on a block the layout no longer names: EIO).
//!
//! The protocol is Dekker's: the writer stores (`inflight += 1`) then
//! loads (`recall`); the recaller stores (`recall := 1`) then loads
//! (`inflight`). Both are SeqCst RMW/stores followed by SeqCst loads — the
//! store must be VISIBLE before the load (store buffering is the hazard:
//! each side sees the other's old value and both proceed — a client write
//! in flight under a layout the daemon is already changing). A recaller
//! that reads `inflight == 0` after its store proceeds AT ONCE (no round
//! trip: a client that increments afterwards reads `recall == 1` and backs
//! out); one that reads `inflight > 0` waits for the count to reach 0 (the
//! client wakes it on its last decrement when `recall` is set), bounded —
//! a client wedged past `RECALL_BOUND_NS` is forced (`recall := 2`,
//! counted, the session poisoned): its syscall, if any, completes within
//! the device's latency, long before the bound. `WriteLeaseModel` proves
//! the invariant (never a mutation with a write in flight) and that
//! dropping EITHER side's fence breaks it.
//!
//! PORTABLE: the words are plain atomics; the wait/wake faces are the
//! caller's (`sync/futex.zig` on Linux, the model's bucket in the model).

const std = @import("std");
const Atomic = std.atomic.Value;

pub const LIVE: u32 = 0;
pub const RECALLED: u32 = 1;
pub const FORCED: u32 = 2;

/// How long a recaller waits for the in-flight count to drain before it
/// forces the lease: a direct write is one device I/O (≤ 100 µs on NVMe,
/// microseconds here), the client's turnaround µs more — 1 s is three
/// orders above both and short enough that a truncate behind a dead
/// client is a hiccup, not a hang.
pub const RECALL_BOUND_NS: u64 = std.time.ns_per_s;

/// One lease's words as laid out in the session's ledger. `tag` is the
/// grant's generation (0 = the slot is free): a client whose cached lease
/// names a slot the daemon has since re-granted reads a tag not its own
/// at entry and backs out — its old layout never writes under the new
/// lease's words.
pub const Words = extern struct {
    tag: Atomic(u64) = .init(0),
    inflight: Atomic(u32) = .init(0),
    recall: Atomic(u32) = .init(LIVE),
};

comptime {
    std.debug.assert(@sizeOf(Words) == 16);
}

/// The writer's entry: count the write in, then read the recall (and the
/// tag). True = proceed with the device write (the caller MUST call
/// `exitWrite` after it); false = the lease is recalled or the slot
/// re-granted (the count is already undone) — the op goes to the ring and
/// the lease is dropped.
pub fn enterWrite(w: *Words, tag: u64) bool {
    _ = w.inflight.fetchAdd(1, .seq_cst);
    if (w.recall.load(.seq_cst) != LIVE or w.tag.load(.seq_cst) != tag) {
        _ = w.inflight.fetchSub(1, .seq_cst);
        return false;
    }
    return true;
}

/// The daemon's grant: the slot's words for a new lease (`tag` unique per
/// grant). The tag lands before the recall clears (release): a client
/// entering under the OLD tag reads either the old recall or the new tag.
pub fn grant(w: *Words, tag: u64) void {
    w.tag.store(tag, .seq_cst);
    w.inflight.store(0, .seq_cst);
    w.recall.store(LIVE, .seq_cst);
}

/// The daemon's release of a slot nobody writes under any more.
pub fn retire(w: *Words) void {
    w.recall.store(RECALLED, .seq_cst);
    w.tag.store(0, .seq_cst);
}

/// The writer's exit after its device write: true = a recaller may be
/// waiting on the count (the caller wakes the word).
pub fn exitWrite(w: *Words) bool {
    _ = w.inflight.fetchSub(1, .seq_cst);
    return w.recall.load(.seq_cst) != LIVE;
}

/// The write's verdict once landed: `recall == FORCED` means the daemon
/// gave up waiting and mutated — the bytes may sit on a block the layout
/// no longer names.
pub fn landedUnderForce(w: *const Words) bool {
    return w.recall.load(.seq_cst) == FORCED;
}

pub const RecallVerdict = enum {
    /// No write in flight at the store: proceed now.
    quiescent,
    /// Writes in flight: the caller waits on `inflight` to reach 0.
    draining,
};

/// The recaller's store: `recall := 1`, then the in-flight read.
pub fn recall(w: *Words) RecallVerdict {
    w.recall.store(RECALLED, .seq_cst);
    return if (w.inflight.load(.seq_cst) == 0) .quiescent else .draining;
}

/// The recaller after its wait: true = drained; false = still in flight.
pub fn drained(w: *const Words) bool {
    return w.inflight.load(.seq_cst) == 0;
}

/// The recaller forcing a lease a wedged client holds.
pub fn force(w: *Words) void {
    w.recall.store(FORCED, .seq_cst);
}

const testing = std.testing;

test "the recall core: a live lease admits a write and exits silently; a recalled lease refuses entry with the count undone; a write in flight at the recall drains it and the exit says to wake; a landed write under a forced lease is told" {
    var w: Words = .{};
    grant(&w, 7);
    try testing.expect(!enterWrite(&w, 6));
    try testing.expect(enterWrite(&w, 7));
    try testing.expectEqual(RecallVerdict.draining, recall(&w));
    try testing.expect(!drained(&w));
    try testing.expect(exitWrite(&w));
    try testing.expect(drained(&w));
    try testing.expect(!enterWrite(&w, 7));
    try testing.expectEqual(@as(u32, 0), w.inflight.load(.seq_cst));
    grant(&w, 8);
    try testing.expect(!enterWrite(&w, 7));
    try testing.expect(enterWrite(&w, 8));
    try testing.expect(!exitWrite(&w));
    var v: Words = .{};
    grant(&v, 1);
    try testing.expectEqual(RecallVerdict.quiescent, recall(&v));
    var f: Words = .{};
    grant(&f, 1);
    try testing.expect(enterWrite(&f, 1));
    force(&f);
    try testing.expect(landedUnderForce(&f));
    try testing.expect(exitWrite(&f));
    retire(&f);
    try testing.expect(!enterWrite(&f, 1));
}
