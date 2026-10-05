//! `Arc(T)` and `ArcSwap(T)`: the arc-swap-style immutable-snapshot
//! publication the charter names for every latch-free hot table (§1.3
//! "arc-swap-style immutable snapshots"; §5.3 "Node cache": `CachedNode` =
//! arc-swapped immutable snapshot, readers latch-free). The reference uses
//! the `arc_swap` crate; this is the port's own, written as one 64-bit
//! word and model-checked (`arc_swap_model.zig`).
//!
//! ## The split-count protocol
//!
//! A reader of a plain `atomic pointer + refcount` pair has a hazard
//! window: between loading the pointer and bumping the object's count the
//! storer can swap the pointer out and drop the last reference — a
//! use-after-free the explorer finds in a handful of states (the
//! `naive` weakening). The fix is the split count: the slot WORD packs the
//! pointer with a per-slot CLAIM count, and every reference a reader takes
//! is pre-paid.
//!
//! - `store` deposits a POOL of `pool` references into the new object's
//!   strong count before publishing it, and swaps the word to `(new, 0)`.
//! - `load` is ONE `fetch_add` on the word: it reads the pointer and claims
//!   one pooled reference in the same atomic step, so the reader owns a real
//!   strong reference the instant it knows the pointer — there is no window
//!   in which the object can die under it.
//! - The swap that displaces a pointer returns `(old, claims)` ATOMICALLY,
//!   so the storer knows exactly how many pooled references readers took
//!   and returns the rest: `old.strong -= pool - claims`. Whoever drops the
//!   last strong reference frees the object — the free is DEFERRED to the
//!   last reader when readers are still inside, never performed by the
//!   storer under them.
//! - A reader observing the claim count at `flush_at` re-biases the pool
//!   (`strong += claims; CAS word (ptr, claims) → (ptr, 0)`), so a hot
//!   snapshot read millions of times between stores never exhausts the
//!   count field.
//!
//! Weakenings `arc_swap_model.zig` proves fail: the naive two-step load;
//! a swap split into "read pointer, then read-and-reset the count" (a
//! claim between the two is reconciled away); a flush done as a plain
//! store instead of a CAS (a concurrent claim is erased).
//!
//! ## The word
//!
//! `[63..48) claims | [48..0) pointer`. 48 pointer bits is the user-space
//! virtual-address ceiling of every target this product runs on
//! (x86-64 4-level paging and aarch64's default 48-bit VA both stop at
//! 2^47; the 52/57-bit extensions only hand out higher addresses to a
//! process that asks with an mmap hint, which this one never does);
//! `pack` asserts it. 16 claim bits with a flush at 2^15: the field can
//! absorb 32,768 claims that race one flush CAS — more concurrent loads
//! than the daemon has threads by two orders of magnitude; the
//! `arc_swap_claims_overflow` tripwire counts the impossible wrap.

const std = @import("std");
const counters = @import("../core/counters.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;

/// Pointer bits in the slot word (see the module doc).
pub const ptr_bits: u6 = 48;
pub const ptr_mask: u64 = (@as(u64, 1) << ptr_bits) - 1;
/// One claim, in word units.
pub const one_claim: u64 = @as(u64, 1) << ptr_bits;
/// The claim pool a store deposits in the published object's strong count:
/// the whole range of the 16-bit claim field.
pub const pool: u64 = @as(u64, 1) << (64 - @as(u7, ptr_bits));
/// A load that observes this many outstanding claims re-biases the pool:
/// half the field, so a flush that loses its CAS to concurrent claims still
/// leaves 2^15 claims of headroom before the field could wrap.
pub const flush_at: u64 = pool / 2;

/// The claim field wrapped: a load observed `pool - 1` outstanding claims,
/// so its own claim carried out of the field and the pool accounting is
/// broken for this object. Must stay 0 (see the module doc for why it
/// cannot be reached on real hardware).
pub var claims_overflow: counters.Tripwire = .init("arc_swap_claims_overflow");

fn pack(addr: usize, claims: u64) u64 {
    const a: u64 = @intCast(addr);
    // The user-space VA ceiling of every target (module doc): a pointer
    // with a high bit set cannot be packed and would corrupt the count.
    assert(a & ~ptr_mask == 0);
    assert(claims < pool);
    return a | (claims << ptr_bits);
}

const Unpacked = struct { addr: usize, claims: u64 };

fn unpack(w: u64) Unpacked {
    return .{ .addr = @intCast(w & ptr_mask), .claims = w >> ptr_bits };
}

/// A heap-allocated, atomically refcounted `T`. `release` runs `T.deinit`
/// (when `T` declares one) and frees the block with the allocator it was
/// created with, so a deferred free needs no allocator in hand.
pub fn Arc(comptime T: type) type {
    return struct {
        const Self = @This();

        strong: std.atomic.Value(u64),
        gpa: Allocator,
        value: T,

        /// One reference, owned by the caller.
        pub fn create(gpa: Allocator, value: T) Allocator.Error!*Self {
            const self = try gpa.create(Self);
            self.* = .{ .strong = .init(1), .gpa = gpa, .value = value };
            return self;
        }

        /// One more reference. Monotonic: the holder already has a
        /// reference, so the object's contents are already visible to it.
        pub fn retain(self: *Self) *Self {
            _ = self.strong.fetchAdd(1, .monotonic);
            return self;
        }

        /// Drop one reference; the last one frees. acq_rel: release orders
        /// this holder's accesses before the drop, acquire on the last drop
        /// pairs with every other holder's release so the destructor sees
        /// them all.
        pub fn release(self: *Self) void {
            if (self.strong.fetchSub(1, .acq_rel) == 1) self.destroy();
        }

        /// Drop `n` references at once (the slot's pool reconciliation).
        fn releaseMany(self: *Self, n: u64) void {
            if (n == 0) return;
            if (self.strong.fetchSub(n, .acq_rel) == n) self.destroy();
        }

        fn destroy(self: *Self) void {
            if (@hasDecl(T, "deinit")) self.value.deinit(self.gpa);
            self.gpa.destroy(self);
        }

        /// The live count (tests and the eviction gate).
        pub fn strongCount(self: *const Self) u64 {
            return self.strong.load(.monotonic);
        }
    };
}

/// A slot holding exactly one `*Arc(T)` at a time, readable latch-free.
pub fn ArcSwap(comptime T: type) type {
    return struct {
        const Self = @This();
        pub const Ptr = *Arc(T);

        word: std.atomic.Value(u64),

        /// Take the caller's reference to `initial` as the slot's.
        pub fn init(initial: Ptr) Self {
            return .{ .word = .init(install(initial)) };
        }

        /// Deposit the pool and build the word. The caller's one reference
        /// becomes part of the pool.
        fn install(p: Ptr) u64 {
            _ = p.strong.fetchAdd(pool - 1, .monotonic);
            return pack(@intFromPtr(p), 0);
        }

        /// Return the unclaimed part of a displaced object's pool. The
        /// object dies here only when no reader holds it.
        fn reconcile(w: u64) void {
            const u = unpack(w);
            const p: Ptr = @ptrFromInt(u.addr);
            p.releaseMany(pool - u.claims);
        }

        /// Latch-free read: ONE acquire fetch_add claims a pooled reference
        /// and reads the pointer in the same step. acquire pairs with
        /// `store`'s acq_rel swap, so the object's contents are visible.
        /// The caller owns the returned reference (`release` it).
        pub fn load(self: *Self) Ptr {
            const w = self.word.fetchAdd(one_claim, .acquire);
            const u = unpack(w);
            const p: Ptr = @ptrFromInt(u.addr);
            if (u.claims == pool - 1) {
                claims_overflow.fire("arc_swap: the 16-bit claim field wrapped under one load");
                return p;
            }
            if (u.claims >= flush_at) self.flush(p, u.claims + 1);
            return p;
        }

        /// Re-bias: move `claims` claims from the field into the object's
        /// strong count, then reset the field. The CAS (not a store) is
        /// what keeps a claim landing in between from being erased — the
        /// weakening the model proves fails. On a lost CAS the deposit is
        /// withdrawn; the withdrawal cannot reach zero because the caller
        /// still holds the one claim it just took.
        fn flush(self: *Self, p: Ptr, taken: u64) void {
            _ = p.strong.fetchAdd(taken, .monotonic);
            const expected = pack(@intFromPtr(p), taken);
            // acq_rel CAS paired with `load`'s acquire fetch_add and
            // `store`'s acq_rel swap on the same word.
            if (self.word.cmpxchgStrong(expected, pack(@intFromPtr(p), 0), .acq_rel, .monotonic) != null) {
                _ = p.strong.fetchSub(taken, .acq_rel);
            }
        }

        /// Publish `new` (taking the caller's reference) and displace the
        /// current object. acq_rel swap: release publishes `new`'s contents
        /// to loaders, acquire sees every claim readers took on the old
        /// word so the reconciliation returns exactly the unclaimed rest.
        pub fn store(self: *Self, new: Ptr) void {
            const old = self.word.swap(install(new), .acq_rel);
            reconcile(old);
        }

        /// The slot's own reference leaves; in-flight readers keep theirs.
        /// The slot must not be used afterwards.
        pub fn deinit(self: *Self) void {
            const old = self.word.swap(0, .acq_rel);
            reconcile(old);
        }

        /// Outstanding claims on the current word (tests).
        pub fn claims(self: *const Self) u64 {
            return unpack(self.word.load(.monotonic)).claims;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

const Payload = struct {
    id: u64,
    destroyed: *std.atomic.Value(u64),

    pub fn deinit(self: *Payload, _: Allocator) void {
        _ = self.destroyed.fetchAdd(1, .monotonic);
    }
};

const PayloadArc = Arc(Payload);
const PayloadSwap = ArcSwap(Payload);

test "Arc: create/retain/release conserve; the last release runs deinit exactly once" {
    var destroyed: std.atomic.Value(u64) = .init(0);
    const a = try PayloadArc.create(testing.allocator, .{ .id = 1, .destroyed = &destroyed });
    try testing.expectEqual(@as(u64, 1), a.strongCount());
    _ = a.retain();
    try testing.expectEqual(@as(u64, 2), a.strongCount());
    a.release();
    try testing.expectEqual(@as(u64, 0), destroyed.load(.monotonic));
    a.release();
    try testing.expectEqual(@as(u64, 1), destroyed.load(.monotonic));
}

test "ArcSwap: a load owns a reference the slot cannot take away; store defers the free to the last reader" {
    var destroyed: std.atomic.Value(u64) = .init(0);
    const a = try PayloadArc.create(testing.allocator, .{ .id = 1, .destroyed = &destroyed });
    var slot: PayloadSwap = .init(a);
    try testing.expectEqual(pool, a.strongCount());
    const r1 = slot.load();
    try testing.expectEqual(a, r1);
    try testing.expectEqual(@as(u64, 1), slot.claims());
    const r2 = slot.load();
    try testing.expectEqual(@as(u64, 2), slot.claims());
    // Displace A: readers r1/r2 keep it alive; the pool's unclaimed rest is returned.
    const b = try PayloadArc.create(testing.allocator, .{ .id = 2, .destroyed = &destroyed });
    slot.store(b);
    try testing.expectEqual(@as(u64, 0), slot.claims());
    try testing.expectEqual(@as(u64, 2), a.strongCount());
    try testing.expectEqual(@as(u64, 0), destroyed.load(.monotonic));
    r1.release();
    try testing.expectEqual(@as(u64, 0), destroyed.load(.monotonic));
    r2.release();
    try testing.expectEqual(@as(u64, 1), destroyed.load(.monotonic));
    // A fresh load sees B.
    const r3 = slot.load();
    try testing.expectEqual(@as(u64, 2), r3.value.id);
    slot.deinit();
    try testing.expectEqual(@as(u64, 1), destroyed.load(.monotonic));
    r3.release();
    try testing.expectEqual(@as(u64, 2), destroyed.load(.monotonic));
}

test "ArcSwap: the flush re-biases at flush_at and conserves references" {
    var destroyed: std.atomic.Value(u64) = .init(0);
    const a = try PayloadArc.create(testing.allocator, .{ .id = 1, .destroyed = &destroyed });
    var slot: PayloadSwap = .init(a);
    // flush_at + 1 loads: the last one observes `flush_at` claims and
    // resets the field; every reference is still accounted.
    var i: u64 = 0;
    while (i <= flush_at) : (i += 1) {
        const r = slot.load();
        try testing.expectEqual(a, r);
        r.release();
    }
    try testing.expectEqual(@as(u64, 0), slot.claims());
    try testing.expectEqual(pool, a.strongCount());
    slot.deinit();
    try testing.expectEqual(@as(u64, 1), destroyed.load(.monotonic));
}

test "ArcSwap storm: readers load/release while a storer publishes; every object dies exactly once" {
    const Shared = struct {
        slot: PayloadSwap,
        destroyed: std.atomic.Value(u64) = .init(0),
        created: std.atomic.Value(u64) = .init(1),
        stop: std.atomic.Value(bool) = .init(false),
        reads: std.atomic.Value(u64) = .init(0),

        fn reader(s: *@This()) void {
            while (!s.stop.load(.acquire)) {
                const r = s.slot.load();
                // The object is intact for as long as the reference is held.
                std.debug.assert(r.value.id != 0);
                _ = s.reads.fetchAdd(1, .monotonic);
                r.release();
            }
        }
        fn storer(s: *@This()) !void {
            var n: u64 = 2;
            while (n < 2000) : (n += 1) {
                const obj = try PayloadArc.create(testing.allocator, .{ .id = n, .destroyed = &s.destroyed });
                _ = s.created.fetchAdd(1, .monotonic);
                s.slot.store(obj);
            }
            s.stop.store(true, .release);
        }
    };
    var s: Shared = .{ .slot = undefined };
    const first = try PayloadArc.create(testing.allocator, .{ .id = 1, .destroyed = &s.destroyed });
    s.slot = .init(first);
    var rs: [6]std.Thread = undefined;
    for (&rs) |*t| t.* = try std.Thread.spawn(.{}, Shared.reader, .{&s});
    const st = try std.Thread.spawn(.{}, Shared.storer, .{&s});
    st.join();
    for (rs) |t| t.join();
    s.slot.deinit();
    try testing.expectEqual(s.created.load(.monotonic), s.destroyed.load(.monotonic));
    try testing.expect(s.reads.load(.monotonic) > 0);
}

test {
    _ = @import("arc_swap_model.zig");
}
