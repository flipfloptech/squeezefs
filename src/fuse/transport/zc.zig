//! The buffer-mode lattice's PURE pieces (charter §3 "Capability
//! discipline", §4 "Buffer-mode lattice"; `docs/porting/fuse_kmbuf_zc.md`
//! §3–§7): the kmbuf index laws and the CQE-bid attachment law, the
//! zero-copy slot direction, the paged-opcode mirror with its PROBE latch,
//! the hold predicate and the payload stability class, the bridge deadline
//! ledger with its cancel-once latch, the cancel-CQE ladder and the fused
//! run-queue. Portable value types; the worker composes them over its ring.
//!
//! Nothing here is keyed to a kernel TRACK (decision 0002 ledger: the
//! reference's per-track opcode mirror was an ABI inference). What differs
//! per kernel is resolved by a probe: the kmbuf opcode pair by the ladder in
//! `probes.zig`, the readdir class's pagedness by the first bridge's own
//! verdict (`ReaddirLatch`).

const std = @import("std");
const linux = std.os.linux;
const abi = @import("../abi.zig");
const derive = @import("../../core/derive.zig");

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// `sizeof(fuse_uring_req_header)`: the kernel maps header `i` of a kmbuf
/// queue at `fixed_buf_id × 288` into fixed buffer index 0 (`+ depth` under zc).
pub const HEADER_STRIDE: usize = @sizeOf(abi.fuse_uring_req_header);
/// The bridge deadline: the D1.b op-watchdog threshold's transport twin
/// (a bridge is a device-class op; a slot a DMA never completes is the same
/// wedge the watchdog names). A documented constant, not a knob (0002 Q9).
pub const BRIDGE_DEADLINE_NS: u64 = 30 * std.time.ns_per_s;
/// The deadline scan runs every `deadline / 1024` of worker-pass cadence
/// while a bridge is pending: ~1024 scans per deadline bounds the overshoot
/// to 0.1 % of the bound and costs one clock read per 29 ms, not per pass.
pub const BRIDGE_SCAN_PERIOD_NS: u64 = BRIDGE_DEADLINE_NS / 1024;
/// Hold alignment: the LBA grain of the direct `WRITE_FIXED(device ← slot)`.
pub const HOLD_ALIGN: u64 = 4096;
/// A fused frame is polled at most this many passes (one per pass) before
/// it is handed to a handler lane: the fused poll exists to catch a warm
/// ladder that answers within the worker's own reap window; a frame that
/// needs more is a fill — the lane's work. 4 = the depth floor, so a full
/// run-queue drains within one depth of passes.
pub const FUSED_POLL_MAX: u8 = 4;

comptime {
    std.debug.assert(HEADER_STRIDE == 288);
    std.debug.assert(BRIDGE_SCAN_PERIOD_NS > 0);
}

// ---------------------------------------------------------------------------
// kmbuf index laws (porting spec §3.2)
// ---------------------------------------------------------------------------

/// The kernel requires a power of two below 65536; `max(…, 1)` so a depth
/// of 1 registers one buffer.
pub fn ringEntriesFor(depth: u32) u32 {
    const n: u32 = @intCast(derive.nextPow2(@max(depth, 1)));
    std.debug.assert(n < 65536);
    return n;
}

/// The headers region: `depth × 288` rounded to the page (a page-granular
/// anonymous mapping to register as a fixed buffer).
pub fn headersSpan(depth: u32, page: usize) usize {
    return @intCast(derive.alignUp(@as(u64, depth) * HEADER_STRIDE, page));
}

/// Fixed-table index of the headers buffer: 0 under BufRing; `depth` under
/// zero-copy (indices `0..depth` are the per-ent sparse slots).
pub fn headersIndex(zero_copy: bool, depth: u32) u16 {
    return if (zero_copy) @intCast(depth) else abi.FUSE_URING_FIXED_HEADERS_OFFSET;
}

/// The REGISTER `init.flags` word per mode: zc never rides without BUF_RING,
/// retention never without ZERO_COPY (the kernel refuses both EINVAL).
pub fn initFlags(zero_copy: bool, retention: bool) u16 {
    var f: u16 = abi.FUSE_URING_BUF_RING;
    if (zero_copy) f |= abi.FUSE_URING_ZERO_COPY;
    if (zero_copy and retention) f |= abi.FUSE_URING_PAYLOAD_RETENTION;
    return f;
}

/// The attachment law (porting spec §3.4): a delivery CQE flagged
/// `IORING_CQE_F_BUFFER` re-points the ent's attached kmbuf buffer to the
/// bid in its high 16 bits; an unflagged CQE KEEPS the current attachment
/// (the kernel reuses the buffer across consecutive payload-carrying
/// requests and recycles it only when a payload-less one is fetched — a
/// stale attachment can only precede a payload-less delivery, whose reply
/// never writes the payload region). A bid at or past `ring_entries` is a
/// protocol breach the caller shuts down on.
pub const Attachment = struct {
    bid: ?u32 = null,

    pub const Note = union(enum) { kept, repointed: u32, breach: u32 };

    pub fn note(self: *Attachment, cqe_flags: u32, ring_entries: u32) Note {
        if (cqe_flags & abi.IORING_CQE_F_BUFFER == 0) return .kept;
        const bid: u32 = cqe_flags >> abi.IORING_CQE_BUFFER_SHIFT;
        if (bid >= ring_entries) return .{ .breach = bid };
        self.bid = bid;
        return .{ .repointed = bid };
    }

    /// The attached buffer's byte window in the region, or null.
    pub fn window(self: Attachment, region: []u8, buf_size: usize, payload_len: usize) []u8 {
        const bid = self.bid orelse return &.{};
        return region[@as(usize, bid) * buf_size ..][0..payload_len];
    }
};

// ---------------------------------------------------------------------------
// Zero-copy: slot direction and the paged mirror (porting spec §4.1–4.2)
// ---------------------------------------------------------------------------

/// What the kernel installed in a zc slot at delivery: nothing (a copyable
/// request), the app's READ destination pages (`ITER_DEST`: the reply body
/// moves in by `READ_FIXED(fd → slot)`), or the app's WRITE source pages
/// (`ITER_SOURCE`: `WRITE_FIXED(slot → fd)` moves them out; a `READ_FIXED`
/// into it is `-EFAULT` — the direction law).
pub const Dir = enum(u8) { none, dest, source };

/// The readdir class's pagedness (`fuse_readdir_alloc_buf` sets `out_pages`
/// on some kernel trees, not others) resolved by the FIRST bridge on a
/// readdir-class slot: `-EFAULT` ⇒ the slot was never registered (copyable);
/// a full-length completion ⇒ paged. Latched per session, never guessed
/// from a track.
pub const ReaddirLatch = enum(u8) { unresolved, paged, copyable };

/// The opcodes whose reply body rides the app's pages on EVERY kernel tree
/// that zero-copies (`fuse_read_args_fill` / `fuse_readlink_page` set
/// `out_pages`), the readdir class by the latch (optimistic while
/// unresolved: the bridge IS the probe), and the one in-paged opcode (WRITE
/// data pages).
pub fn outPaged(op: abi.Opcode, latch: ReaddirLatch) bool {
    return switch (op) {
        .read, .readlink => true,
        .readdir, .readdirplus => latch != .copyable,
        else => false,
    };
}

pub fn inPaged(op: abi.Opcode) bool {
    return op == .write;
}

pub fn dirOf(op: abi.Opcode, latch: ReaddirLatch) Dir {
    if (inPaged(op)) return .source;
    if (outPaged(op, latch)) return .dest;
    return .none;
}

// ---------------------------------------------------------------------------
// The hold predicate and the stability class (porting spec §4.4, §5.2)
// ---------------------------------------------------------------------------

/// `hold_candidate`: `size > 0 ∧ size < bound ∧ 4 KiB-aligned offset and
/// size`. `bound` is a field of the resolved geometry passed in (0002 ledger
/// Q11: never a process-global): `payload / 2` STRICT — a half-payload write
/// is the streaming class (kernel-split streaming writes arrive
/// payload-sized) and the W1 cap `block/8 ≤ payload/2` on every shipped
/// geometry.
pub fn holdCandidate(offset: u64, size: u32, bound: u32) bool {
    return size > 0 and size < bound and offset % HOLD_ALIGN == 0 and size % HOLD_ALIGN == 0;
}

pub fn holdBound(payload_sz: u32) u32 {
    return payload_sz / 2;
}

/// The W1 SHAPE (charter §6.2 item 2) as a pure predicate on the request:
/// 4 KiB-aligned offset and length, `len ≤ block/8` (`PATCH_MAX`), within
/// one block. The filesystem's gate adds what only it knows (striped,
/// non-extending, undecorated whole-block map, passthrough) from a
/// published per-inode snapshot — never a map lookup on the worker thread
/// (0002 ledger Q10). Holding W1-INELIGIBLE shapes measured 0.45× (386k
/// unarmed / 300k fusion-off / 175k fused rand-4k at ~300 µs RTT).
pub fn w1Shape(offset: u64, size: u32, block: u64) bool {
    if (size == 0 or @as(u64, size) > block / 8) return false;
    if (offset % HOLD_ALIGN != 0 or size % HOLD_ALIGN != 0) return false;
    return offset / block == (offset + size - 1) / block;
}

/// The stability law (porting spec §5.2): page-cache folios are sound after
/// an ACK-early (a later modification is a redirty = a new WRITE);
/// O_DIRECT/GUP pages are NOT (the app owns them again at write(2)'s return
/// and the DMA samples them later) — a `.direct_gup` payload refuses
/// `replyRetain`. Stamped at delivery from the write's wire bits:
/// `odirect = (open flags & O_DIRECT) ∧ !(write_flags & FUSE_WRITE_CACHE)`.
pub const PayloadClass = enum(u8) {
    page_cache,
    direct_gup,

    pub fn classify(open_flags: u32, write_flags: u32) PayloadClass {
        const o_direct: u32 = @bitCast(linux.O{ .DIRECT = true });
        if (open_flags & o_direct != 0 and write_flags & abi.FUSE_WRITE_CACHE == 0) return .direct_gup;
        return .page_cache;
    }
};

/// `fuse_candidate(len, ceiling, on) = on ∧ len > 0 ∧ len ≤ ceiling`
/// (ceiling inclusive; zero-length never).
pub fn fuseCandidate(len: u32, ceiling: u32, on: bool) bool {
    return on and len > 0 and len <= ceiling;
}

// ---------------------------------------------------------------------------
// Bridges: the deadline ledger and the cancel-CQE ladder (porting spec §7.3–7.4)
// ---------------------------------------------------------------------------

/// Per-ent bridge bookkeeping: `born` at every pend SET (a re-stamp on a
/// live pend is NOT fresh — no gauge double count), `submitted` at the first
/// flush after the push, `cancelled` the cancel-once latch (reset by
/// `clear`/a fresh `stamp`). `overdue` yields each overdue ent EXACTLY ONCE
/// per pend lifetime.
pub const Deadlines = struct {
    born_ns: []u64,
    submitted: []bool,
    cancelled: []bool,
    outstanding: u32 = 0,

    pub fn init(gpa: std.mem.Allocator, ents: usize) std.mem.Allocator.Error!Deadlines {
        const born = try gpa.alloc(u64, ents);
        errdefer gpa.free(born);
        const submitted = try gpa.alloc(bool, ents);
        errdefer gpa.free(submitted);
        const cancelled = try gpa.alloc(bool, ents);
        @memset(born, 0);
        @memset(submitted, false);
        @memset(cancelled, false);
        return .{ .born_ns = born, .submitted = submitted, .cancelled = cancelled };
    }

    pub fn deinit(self: *Deadlines, gpa: std.mem.Allocator) void {
        gpa.free(self.born_ns);
        gpa.free(self.submitted);
        gpa.free(self.cancelled);
        self.* = undefined;
    }

    /// True iff this stamp opened a NEW pend (the pool gauge moves by one).
    pub fn stamp(self: *Deadlines, ent: usize, now_ns: u64) bool {
        const fresh = self.born_ns[ent] == 0;
        self.born_ns[ent] = @max(now_ns, 1);
        self.submitted[ent] = false;
        self.cancelled[ent] = false;
        if (fresh) self.outstanding += 1;
        return fresh;
    }

    pub fn noteSubmitted(self: *Deadlines, ent: usize) void {
        if (self.born_ns[ent] != 0) self.submitted[ent] = true;
    }

    pub fn clear(self: *Deadlines, ent: usize) void {
        if (self.born_ns[ent] == 0) return;
        self.born_ns[ent] = 0;
        self.submitted[ent] = false;
        self.cancelled[ent] = false;
        self.outstanding -= 1;
    }

    pub fn live(self: *const Deadlines, ent: usize) bool {
        return self.born_ns[ent] != 0;
    }

    /// The next overdue ent not yet cancelled (latching it), or null.
    pub fn nextOverdue(self: *Deadlines, now_ns: u64, timeout_ns: u64) ?usize {
        for (self.born_ns, 0..) |b, i| {
            if (b == 0 or self.cancelled[i]) continue;
            if (now_ns -| b < timeout_ns) continue;
            self.cancelled[i] = true;
            return i;
        }
        return null;
    }

    /// The oldest live pend's age (the `scan_max_age_ms` gauge's input).
    pub fn maxAgeNs(self: *const Deadlines, now_ns: u64) u64 {
        var worst: u64 = 0;
        for (self.born_ns) |b| if (b != 0) {
            worst = @max(worst, now_ns -| b);
        };
        return worst;
    }
};

/// What the cancel's OWN CQE means (the four arms). `-ENOENT` with a live
/// pend is THE proven lost-completion shape: the CQ is FIFO, the op's
/// completion was POSTED before this CQE, and a live pend says it never
/// reached us — synthesizing is safe exactly because no kernel op still
/// references the slot. `-EALREADY` (or anything else) means the op still
/// runs kernel-side: a synthesis would let a late DMA alias a recycled
/// slot — re-stamp and stay watchdog-named.
pub const CancelAction = enum { nothing, await_original, synthesize_lost, restamp };

pub fn cancelAction(res: i32, pend_live: bool) CancelAction {
    if (!pend_live) return .nothing;
    if (res == 0) return .await_original;
    if (res == -@as(i32, @intCast(@backingInt(linux.E.NOENT)))) return .synthesize_lost;
    return .restamp;
}

// ---------------------------------------------------------------------------
// The fused run-queue (charter §4 "poll ONE fused handler")
// ---------------------------------------------------------------------------

/// A bounded FIFO of frame indices, capacity = the group's ents (a fused
/// frame exists only while its ent owes a reply, so the bound is
/// structural). Worker-owned; refusal at capacity is the demotion signal.
pub const FusedQueue = struct {
    items: []u32,
    head: u32 = 0,
    len: u32 = 0,

    pub fn init(gpa: std.mem.Allocator, capacity: usize) std.mem.Allocator.Error!FusedQueue {
        return .{ .items = try gpa.alloc(u32, @max(capacity, 1)) };
    }

    pub fn deinit(self: *FusedQueue, gpa: std.mem.Allocator) void {
        gpa.free(self.items);
        self.* = undefined;
    }

    pub fn push(self: *FusedQueue, frame: u32) bool {
        if (self.len == self.items.len) return false;
        self.items[(self.head + self.len) % @as(u32, @intCast(self.items.len))] = frame;
        self.len += 1;
        return true;
    }

    pub fn pop(self: *FusedQueue) ?u32 {
        if (self.len == 0) return null;
        const v = self.items[self.head];
        self.head = (self.head + 1) % @as(u32, @intCast(self.items.len));
        self.len -= 1;
        return v;
    }

    pub fn isEmpty(self: *const FusedQueue) bool {
        return self.len == 0;
    }
};

// ---------------------------------------------------------------------------
// Tests: the laws as the spec states them (porting spec §12, portable half).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "kmbuf indexing: header stride 288, ring_entries = next_pow2(depth) ≥ 1, the headers span is page-granular, the headers index is 0 under BufRing and depth under zc, zc never rides without BUF_RING and retention never without zc" {
    try testing.expectEqual(@as(usize, 288), HEADER_STRIDE);
    try testing.expectEqual(@as(u32, 1), ringEntriesFor(0));
    try testing.expectEqual(@as(u32, 1), ringEntriesFor(1));
    try testing.expectEqual(@as(u32, 4), ringEntriesFor(3));
    try testing.expectEqual(@as(u32, 32), ringEntriesFor(32));
    try testing.expectEqual(@as(u32, 64), ringEntriesFor(33));
    try testing.expectEqual(@as(usize, 4096), headersSpan(4, 4096));
    try testing.expectEqual(@as(usize, 12288), headersSpan(32, 4096));
    try testing.expectEqual(@as(u16, 0), headersIndex(false, 32));
    try testing.expectEqual(@as(u16, 32), headersIndex(true, 32));
    try testing.expectEqual(abi.FUSE_URING_BUF_RING, initFlags(false, false));
    try testing.expectEqual(abi.FUSE_URING_BUF_RING, initFlags(false, true));
    try testing.expectEqual(abi.FUSE_URING_BUF_RING | abi.FUSE_URING_ZERO_COPY, initFlags(true, false));
    try testing.expectEqual(abi.FUSE_URING_BUF_RING | abi.FUSE_URING_ZERO_COPY | abi.FUSE_URING_PAYLOAD_RETENTION, initFlags(true, true));
}

test "the attachment law: a flagged CQE re-points, an unflagged one keeps, an out-of-range bid is a breach; the window addresses region + bid × buf_size" {
    var a: Attachment = .{};
    var region: [4 * 64]u8 = undefined;
    for (&region, 0..) |*b, i| b.* = @intCast(i / 64);
    try testing.expectEqual(Attachment.Note.kept, a.note(0, 4));
    try testing.expectEqual(@as(usize, 0), a.window(&region, 64, 16).len);
    try testing.expectEqual(Attachment.Note{ .repointed = 2 }, a.note(abi.IORING_CQE_F_BUFFER | (2 << 16), 4));
    try testing.expectEqual(@as(u8, 2), a.window(&region, 64, 16)[0]);
    try testing.expectEqual(Attachment.Note.kept, a.note(0, 4));
    try testing.expectEqual(@as(?u32, 2), a.bid);
    try testing.expectEqual(Attachment.Note{ .breach = 4 }, a.note(abi.IORING_CQE_F_BUFFER | (4 << 16), 4));
    try testing.expectEqual(@as(?u32, 2), a.bid);
    try testing.expectEqual(Attachment.Note{ .repointed = 0 }, a.note(abi.IORING_CQE_F_BUFFER, 4));
    try testing.expectEqual(@as(u8, 0), a.window(&region, 64, 16)[0]);
}

test "the paged mirror is not track-keyed: READ/READLINK out-paged always, WRITE in-paged, the readdir class optimistic until the latch says copyable, everything else none" {
    try testing.expectEqual(Dir.dest, dirOf(.read, .unresolved));
    try testing.expectEqual(Dir.dest, dirOf(.readlink, .copyable));
    try testing.expectEqual(Dir.source, dirOf(.write, .unresolved));
    try testing.expectEqual(Dir.dest, dirOf(.readdir, .unresolved));
    try testing.expectEqual(Dir.dest, dirOf(.readdirplus, .paged));
    try testing.expectEqual(Dir.none, dirOf(.readdir, .copyable));
    try testing.expectEqual(Dir.none, dirOf(.readdirplus, .copyable));
    try testing.expectEqual(Dir.none, dirOf(.getattr, .unresolved));
    try testing.expectEqual(Dir.none, dirOf(.lookup, .paged));
    try testing.expect(!inPaged(.read) and inPaged(.write));
}

test "hold_candidate: strict half-payload bound, 4 KiB alignment, zero length never; the W1 shape caps at block/8 within one block" {
    const bound = holdBound(1 << 20);
    try testing.expectEqual(@as(u32, 512 << 10), bound);
    try testing.expect(holdCandidate(0, 4096, bound));
    try testing.expect(holdCandidate(4096 * 7, 4096 * 3, bound));
    try testing.expect(!holdCandidate(0, 0, bound));
    try testing.expect(!holdCandidate(0, 512 << 10, bound));
    try testing.expect(holdCandidate(0, (512 << 10) - 4096, bound));
    try testing.expect(!holdCandidate(1, 4096, bound));
    try testing.expect(!holdCandidate(0, 4095, bound));
    const block: u64 = 4 << 20;
    try testing.expect(w1Shape(0, 4096, block));
    try testing.expect(w1Shape(block - 4096, 4096, block));
    try testing.expect(w1Shape(0, 512 << 10, block));
    try testing.expect(!w1Shape(0, (512 << 10) + 4096, block));
    try testing.expect(!w1Shape(block - 4096, 8192, block));
    try testing.expect(!w1Shape(512, 4096, block));
    try testing.expect(!w1Shape(0, 0, block));
}

test "the stability class: O_DIRECT without FUSE_WRITE_CACHE is direct_gup; a writeback-cache write is page_cache even with O_DIRECT; a plain write is page_cache" {
    const o_direct: u32 = @bitCast(linux.O{ .DIRECT = true });
    try testing.expectEqual(PayloadClass.direct_gup, PayloadClass.classify(o_direct, 0));
    try testing.expectEqual(PayloadClass.page_cache, PayloadClass.classify(o_direct, abi.FUSE_WRITE_CACHE));
    try testing.expectEqual(PayloadClass.page_cache, PayloadClass.classify(0, 0));
    try testing.expectEqual(PayloadClass.page_cache, PayloadClass.classify(@bitCast(linux.O{ .ACCMODE = .WRONLY }), 0));
}

test "fuse_candidate: on ∧ 0 < len ≤ ceiling (inclusive)" {
    try testing.expect(fuseCandidate(4096, 131072, true));
    try testing.expect(fuseCandidate(131072, 131072, true));
    try testing.expect(!fuseCandidate(131073, 131072, true));
    try testing.expect(!fuseCandidate(0, 131072, true));
    try testing.expect(!fuseCandidate(4096, 131072, false));
}

test "the deadline ledger: stamp is fresh once, re-stamp is not, clear returns the share, overdue yields each ent exactly once per pend lifetime, a fresh stamp re-arms the cancel latch" {
    var d = try Deadlines.init(testing.allocator, 4);
    defer d.deinit(testing.allocator);
    try testing.expect(d.stamp(1, 1000));
    try testing.expect(!d.stamp(1, 2000));
    try testing.expectEqual(@as(u32, 1), d.outstanding);
    try testing.expect(d.live(1) and !d.live(0));
    d.noteSubmitted(1);
    try testing.expect(d.submitted[1]);
    try testing.expectEqual(@as(?usize, null), d.nextOverdue(2000 + BRIDGE_DEADLINE_NS - 1, BRIDGE_DEADLINE_NS));
    try testing.expectEqual(@as(?usize, 1), d.nextOverdue(2000 + BRIDGE_DEADLINE_NS, BRIDGE_DEADLINE_NS));
    try testing.expectEqual(@as(?usize, null), d.nextOverdue(2000 + 2 * BRIDGE_DEADLINE_NS, BRIDGE_DEADLINE_NS));
    try testing.expectEqual(2 * BRIDGE_DEADLINE_NS, d.maxAgeNs(2000 + 2 * BRIDGE_DEADLINE_NS));
    // A restamp re-arms the latch: the ent is yielded once more.
    _ = d.stamp(1, 5000);
    try testing.expectEqual(@as(?usize, 1), d.nextOverdue(5000 + BRIDGE_DEADLINE_NS, BRIDGE_DEADLINE_NS));
    d.clear(1);
    try testing.expectEqual(@as(u32, 0), d.outstanding);
    d.clear(1);
    try testing.expectEqual(@as(u32, 0), d.outstanding);
    try testing.expectEqual(@as(?usize, null), d.nextOverdue(std.math.maxInt(u64), BRIDGE_DEADLINE_NS));
    try testing.expect(d.stamp(2, 7));
    try testing.expectEqual(@as(u32, 1), d.outstanding);
    try testing.expectEqual(BRIDGE_DEADLINE_NS, 30 * std.time.ns_per_s);
    try testing.expectEqual(BRIDGE_DEADLINE_NS / 1024, BRIDGE_SCAN_PERIOD_NS);
}

test "the cancel-CQE ladder: pend gone ⇒ nothing; 0 ⇒ await the original; -ENOENT with a live pend ⇒ synthesize the lost completion; -EALREADY or anything else ⇒ restamp" {
    const enoent = -@as(i32, @intCast(@backingInt(linux.E.NOENT)));
    const ealready = -@as(i32, @intCast(@backingInt(linux.E.ALREADY)));
    try testing.expectEqual(CancelAction.nothing, cancelAction(0, false));
    try testing.expectEqual(CancelAction.nothing, cancelAction(enoent, false));
    try testing.expectEqual(CancelAction.await_original, cancelAction(0, true));
    try testing.expectEqual(CancelAction.synthesize_lost, cancelAction(enoent, true));
    try testing.expectEqual(CancelAction.restamp, cancelAction(ealready, true));
    try testing.expectEqual(CancelAction.restamp, cancelAction(-5, true));
}

test "the fused run-queue is a bounded FIFO: refusal at capacity is the demotion signal, pops come in order, wrap-around holds" {
    var q = try FusedQueue.init(testing.allocator, 3);
    defer q.deinit(testing.allocator);
    try testing.expect(q.isEmpty());
    try testing.expect(q.push(10) and q.push(11) and q.push(12));
    try testing.expect(!q.push(13));
    try testing.expectEqual(@as(?u32, 10), q.pop());
    try testing.expect(q.push(13));
    try testing.expectEqual(@as(?u32, 11), q.pop());
    try testing.expectEqual(@as(?u32, 12), q.pop());
    try testing.expectEqual(@as(?u32, 13), q.pop());
    try testing.expectEqual(@as(?u32, null), q.pop());
    try testing.expectEqual(@as(u8, 4), FUSED_POLL_MAX);
}
