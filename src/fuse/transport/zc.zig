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

/// The hold SHAPE as a pure predicate on the request (the transport's
/// half, proved on the worker thread): a non-empty payload, 4 KiB-aligned
/// offset and length (`HOLD_ALIGN`, the LBA grain of the direct
/// `WRITE_FIXED`), within ONE block — the write splitter makes pieces
/// single-block, and a straddling write's pieces all materialize (one lazy
/// extraction on the lane instead of one at delivery: the same copy, one
/// wake later), so it is extracted at delivery. No upper bound of its own:
/// a FUSE WRITE never exceeds `max_write ≤ payload`. The filesystem's gate
/// adds what only it knows (the class by `holdClass`, striped on a
/// passthrough set, the W1 or overlay arm's own screens) from a published
/// per-inode snapshot — never a map lookup on the worker thread (0002
/// ledger Q10).
pub fn holdShape(offset: u64, size: u32, block: u64) bool {
    if (size == 0) return false;
    if (offset % HOLD_ALIGN != 0 or size % HOLD_ALIGN != 0) return false;
    return offset / block == (offset + size - 1) / block;
}

/// Which write arm a held payload is destined for: charter §6.2 items 1–2
/// meet on ONE line, `PATCH_MAX = block/8` — W1's ceiling and the overlay's
/// length floor — so the class costs no constant of its own. At or under
/// it the sole-owner patch (W1), whose filesystem gate declines every
/// W1-ineligible shape (holding those measured 0.45×: rand-4k at fabric
/// RTT paid hold + late extraction per op). Above it the device overlay
/// (item 1: `WRITE_FIXED(device fd ← slot)`, zero daemon copies) — the
/// streaming class, whose extraction at delivery was the ONE daemon copy
/// of every big written byte (io-wq 0.77 ms/MiB, half the daemon's cores
/// on 1M seq write ×4: rung-10-slabcopy, rung-10-perf-baseline-workstation).
pub const HoldClass = enum(u8) { w1, overlay };

pub fn holdClass(size: u32, patch_max: u64) HoldClass {
    return if (@as(u64, size) <= patch_max) .w1 else .overlay;
}

/// The class line as the transport derives it for a PLACEMENT choice
/// (`worker.Group.heldWritePlacement`): `block/8`, the same denominator as
/// the data plane's `sole_owner_patch.patchMaxDerived` (the natural
/// sub-block grain: 512 KiB at the 4 MiB block). The data plane's knob
/// `SQUEEZEFS_PATCH_MAX_BYTES` moves where a write is STORED; where its
/// handler RUNS follows the derived line alone.
pub fn streamingFloor(block: u64) u64 {
    return block / 8; // block/8: W1's ceiling, the overlay's floor (charter §6.2 items 1–2)
}

// ---------------------------------------------------------------------------
// The bridge's NOWAIT pieces (rung-10-seq-parity)
// ---------------------------------------------------------------------------

/// A `READ_FIXED`/`WRITE_FIXED` to a BLOCK DEVICE larger than its queue's
/// `max_sectors` is refused by the block layer's NOWAIT attempt (a
/// `REQ_NOWAIT` bio cannot be split: `bio_split_rw` → -EAGAIN) and
/// re-issued on io-wq — the doomed attempt's CPU, an io-wq worker's wake
/// (measured 192 µs mean, p50 8 µs, on 1M seq write ×4) and 0.9 cores of
/// `iou-wrk`; iomap filesystems set no `REQ_NOWAIT` on their bios and pay
/// none of it. So a bridge is pushed in pieces at or under the fd's
/// ceiling — each one's bio needs no split and lands inline — up to this
/// many pieces: the 1 MiB `max_write` over the smallest common NVMe
/// `max_sectors_kb` (128 KiB; nvme-loop 1020 KiB, nvme-tcp 508 KiB). A
/// ceiling that would need more pieces keeps the one-SQE push (the punt).
pub const BRIDGE_PIECES_MAX: u32 = 8;

/// Pieces for `len` under `ceiling` (0 = no ceiling: a regular file, a
/// pipe, an unprobed fd): 1, or `ceil(len / ceiling)` within the bound.
pub fn bridgePieces(len: u32, ceiling: u32) u32 {
    if (ceiling == 0 or len <= ceiling) return 1;
    const n: u32 = @intCast(derive.ceilDiv(len, ceiling));
    return if (n > BRIDGE_PIECES_MAX) 1 else n;
}

/// The NOWAIT ceiling from the queue's sysfs facts: `max_sectors_kb`
/// (the split bound) and `max_segments × page` (the registered table's
/// one-page bvecs), the smaller; 0 when either is absent or unparsable.
pub fn nowaitCeiling(max_sectors_kb: ?u64, max_segments: ?u64, page: u64) u32 {
    const by_bytes = (max_sectors_kb orelse return 0) * derive.KiB;
    const by_segs = (max_segments orelse return 0) * page;
    const c = @min(by_bytes, by_segs);
    return if (c == 0 or c > std.math.maxInt(u32)) 0 else @intCast(c);
}

/// A sysfs queue attribute's text: a decimal with a trailing newline.
pub fn parseSysfsUnsigned(text: []const u8) ?u64 {
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (t.len == 0) return null;
    return std.fmt.parseInt(u64, t, 10) catch null;
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

test "the hold shape: 4 KiB alignment, within one block, zero length never, no bound of its own (a whole-payload 1 MiB write holds, a 4 MiB block-sized write holds); the class splits on PATCH_MAX = block/8 — at or under it W1, above it the overlay" {
    const block: u64 = 4 << 20;
    try testing.expect(holdShape(0, 4096, block));
    try testing.expect(holdShape(block - 4096, 4096, block));
    try testing.expect(holdShape(4096 * 7, 4096 * 3, block));
    try testing.expect(holdShape(0, 512 << 10, block));
    try testing.expect(holdShape(0, (512 << 10) + 4096, block));
    try testing.expect(holdShape(3 << 20, 1 << 20, block)); // the streaming shape: the last 1 MiB of a block
    try testing.expect(holdShape(block, @intCast(block), block)); // a block-sized write at a block edge
    try testing.expect(!holdShape(block - 4096, 8192, block)); // straddles the block edge
    try testing.expect(!holdShape((3 << 20) + 4096, 1 << 20, block)); // a 1 MiB write that straddles
    try testing.expect(!holdShape(512, 4096, block));
    try testing.expect(!holdShape(1, 4096, block));
    try testing.expect(!holdShape(0, 4095, block));
    try testing.expect(!holdShape(0, 0, block));
    const patch_max = block / 8;
    try testing.expectEqual(HoldClass.w1, holdClass(4096, patch_max));
    try testing.expectEqual(HoldClass.w1, holdClass(512 << 10, patch_max));
    try testing.expectEqual(HoldClass.overlay, holdClass((512 << 10) + 4096, patch_max));
    try testing.expectEqual(HoldClass.overlay, holdClass(1 << 20, patch_max));
    // The patch-OFF control arm (SQUEEZEFS_PATCH_MAX_BYTES=0): every length is the overlay's.
    try testing.expectEqual(HoldClass.overlay, holdClass(4096, 0));
    // The placement line restates the data plane's derived patch max: block/8 (512 KiB at 4 MiB).
    try testing.expectEqual(patch_max, streamingFloor(block));
    try testing.expectEqual(@as(u64, 512 << 10), streamingFloor(block));
    try testing.expectEqual(@import("../../data/sole_owner_patch.zig").patchMaxDerived(block), streamingFloor(block));
}

test "the bridge's NOWAIT pieces: no ceiling or a fitting DMA is one piece; nvme-loop's 1020 KiB cuts 1 MiB in two, nvme-tcp's 508 KiB in three, a 128 KiB NVMe in eight (the bound); a ceiling needing more keeps the one-SQE push; the ceiling is the smaller of max_sectors_kb and max_segments × page, 0 when either is absent" {
    const mib: u32 = 1 << 20;
    try testing.expectEqual(@as(u32, 1), bridgePieces(mib, 0));
    try testing.expectEqual(@as(u32, 1), bridgePieces(4096, 1020 << 10));
    try testing.expectEqual(@as(u32, 1), bridgePieces(1020 << 10, 1020 << 10));
    try testing.expectEqual(@as(u32, 2), bridgePieces(mib, 1020 << 10));
    try testing.expectEqual(@as(u32, 3), bridgePieces(mib, 508 << 10));
    try testing.expectEqual(@as(u32, 8), bridgePieces(mib, 128 << 10));
    try testing.expectEqual(@as(u32, 1), bridgePieces(mib, 64 << 10));
    try testing.expectEqual(@as(u32, 8), BRIDGE_PIECES_MAX);
    // nvme-loop: 1020 KiB over 256 segments of a page ⇒ 1020 KiB; a 128 KiB / 33-segment NVMe ⇒ 128 KiB.
    try testing.expectEqual(@as(u32, 1020 << 10), nowaitCeiling(1020, 256, 4096));
    try testing.expectEqual(@as(u32, 128 << 10), nowaitCeiling(128, 33, 4096));
    try testing.expectEqual(@as(u32, 16 << 10), nowaitCeiling(1020, 4, 4096));
    try testing.expectEqual(@as(u32, 0), nowaitCeiling(null, 256, 4096));
    try testing.expectEqual(@as(u32, 0), nowaitCeiling(1020, null, 4096));
    try testing.expectEqual(@as(?u64, 1020), parseSysfsUnsigned("1020\n"));
    try testing.expectEqual(@as(?u64, null), parseSysfsUnsigned("\n"));
    try testing.expectEqual(@as(?u64, null), parseSysfsUnsigned("x"));
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
