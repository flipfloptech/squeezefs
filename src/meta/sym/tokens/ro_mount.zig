//! `-o ro` mounts and the S5 epoch-poll control plane (charter §5.5
//! "`-o ro` mounts are member-readers + token clients (every reader TTL
//! derives to 0; `reader_staleness_bound_ms` reads 0); with NO live writer
//! a `-o ro` mount is the QUIESCENT reader of the checkpointed projection
//! that fail-stops `ESTALE` at the first poll that sees a writer … The S5
//! epoch-poll of the fixed ledger survives as the CONTROL plane only
//! (predicted-slot 4 KiB read first; a 32-poll gap belt reads the whole
//! ledger)"; `docs/porting/sym_tokens_intents_striping.md` §1.8, §1.11,
//! §1.12):
//!
//! - the reader's TTL law: under a token a cache may hold an entry exactly
//!   as long as freshness is proven — "until the recall", which no TTL
//!   expresses — so every kernel TTL class derives to 0 and an explicit
//!   `-o *_timeout` on a token reader REFUSES loud naming the law;
//! - the member lease the reader holds so its tokens can expire: the
//!   deadline is `T_owner`, the verdicts `live | expired | unknown`;
//! - the quiescent reader: no live writer at the open ⇒ no token client,
//!   the projection served exactly (nothing writes), and the poll watching
//!   for a writer by BOTH faces (a same-host writer's exclusive lock, a
//!   remote writer's FIRST checkpoint = a ledger advance) — either
//!   fail-stops the reader ONCE: every metadata op answers the typed
//!   `ReaderFailStopped` (ESTALE), the root's attributes excepted so
//!   `.stats` reads;
//! - the epoch poll: checkpoint `seq` sits in slot `seq % 32`, so the next
//!   record can only sit in ONE slot — read that 4 KiB, walk successors
//!   while each holds the next seq, stop at an older record or an all-zero
//!   slot; a TORN slot takes the whole-ledger fallback; after 32 STOPPED
//!   polls the belt reads the whole ledger once (a seq the writer CONSUMED
//!   without a record — a crash between a bitmap write and its record —
//!   would otherwise read "not written yet" for ever). Measured: idle poll
//!   4 KiB (was 128 KiB — 32× fewer bytes), 2 new checkpoints 12 KiB, a
//!   torn slot 4 + 128 KiB.
//! PORTABLE: the device is behind `LedgerIo`.

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const log = @import("../../../core/log.zig");
const mount_options = @import("../../../fuse/mount_options.zig");
const ledger = @import("../../ledger.zig");
const jc = @import("../../journal_core.zig");
const table = @import("token_table.zig");

const ns_per_ms: u64 = std.time.ns_per_ms;

// ---------------------------------------------------------------------------
// Constants — value and WHY.
// ---------------------------------------------------------------------------

/// The ledger's 32 round-robin slots (charter §5.1): the gap belt's
/// length — "a truly idle writer costs one 128 KiB read per 32 idle polls".
pub const GAP_BELT_POLLS: u32 = @intCast(ledger.ROOT_LEDGER_SLOTS);
/// One slot = one predicted-slot read.
pub const SLOT_LEN: usize = @intCast(ledger.ROOT_LEDGER_SLOT_LEN);
pub const LEDGER_LEN: usize = @intCast(ledger.ROOT_LEDGER_LEN);
/// `metadata_staleness_bound()` under tokens: 0 — kept for tooling.
pub const READER_STALENESS_BOUND_MS: u64 = 0;

comptime {
    std.debug.assert(GAP_BELT_POLLS == 32);
    std.debug.assert(SLOT_LEN == 4096 and LEDGER_LEN == 128 * 1024);
}

pub const stats = struct {
    /// `reader_staleness_bound_ms` = 0 under tokens (kept for tooling).
    pub var reader_staleness_bound_ms: counters.Gauge = .{};
    /// Posture words: a quiescent reader armed; one that fail-stopped.
    pub var reader_quiescent: counters.Gauge = .{};
    pub var reader_quiescent_writer_appeared: counters.Gauge = .{};
    /// Ops refused `ReaderFailStopped` after the fail-stop (ESTALE).
    pub var reader_fail_stopped_refusals: counters.ShardedCounter = .{};
    /// An explicit kernel TTL on a token reader refused at the mount.
    pub var reader_explicit_ttl_refusals: counters.ShardedCounter = .{};
    /// The control-plane poll: bytes read, predicted-slot hits (the next
    /// record sat where the arithmetic said), whole-ledger reads (a torn
    /// slot), gap scans (the belt).
    pub var meta_kv_revalidate_ledger_read_bytes: counters.ShardedCounter = .{};
    pub var meta_kv_revalidate_predicted_hits: counters.ShardedCounter = .{};
    pub var meta_kv_revalidate_polls: counters.ShardedCounter = .{};
    pub var meta_kv_revalidate_ledger_full_reads: counters.ShardedCounter = .{};
    pub var meta_kv_revalidate_gap_scans: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// The reader's TTL law.
// ---------------------------------------------------------------------------

pub const TtlRefusal = struct {
    option: []const u8,

    pub fn format(self: TtlRefusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("-o {s} is refused on a token reader: every kernel TTL derives to 0 under tokens (a cache may hold an entry exactly as long as freshness is proven — until the recall — which no TTL expresses); drop the option", .{self.option});
    }
};

/// The four classes at 0 — or the refusal naming the explicit option.
pub fn tokenReaderTtls(opts: *const mount_options.MountOptions) error{ExplicitTtlRefused}!mount_options.Ttls {
    if (opts.explicitTtl()) |class| {
        stats.reader_explicit_ttl_refusals.inc();
        const r: TtlRefusal = .{ .option = mount_options.ttlOptionName(class) };
        log.err("{f}", .{r});
        return error.ExplicitTtlRefused;
    }
    stats.reader_staleness_bound_ms.set(READER_STALENESS_BOUND_MS);
    return .{ .attr_ns = 0, .entry_ns = 0, .dir_entry_ns = 0, .negative_ns = 0 };
}

// ---------------------------------------------------------------------------
// The member lease (the reader's own) and the verdict face.
// ---------------------------------------------------------------------------

/// The lease the membership owner granted this reader: `deadline =
/// granted_at + T_owner`; renewed by every `Renew`'s grant.
pub const MemberLease = struct {
    deadline_ns: std.atomic.Value(u64) = .init(0),

    pub fn renew(self: *MemberLease, granted_at_ns: u64) void {
        self.deadline_ns.store(granted_at_ns + table.T_OWNER_NS, .release);
    }

    pub fn live(self: *const MemberLease, now_ns: u64) bool {
        return now_ns < self.deadline_ns.load(.acquire);
    }

    pub fn remainingNs(self: *const MemberLease, now_ns: u64) u64 {
        return self.deadline_ns.load(.acquire) -| now_ns;
    }

    /// The reader-side `SelfLease` face.
    pub fn selfLease(self: *MemberLease) @import("token_cache.zig").SelfLease {
        return .{ .ctx = self, .live = &selfLive };
    }

    fn selfLive(ctx: ?*anyopaque, now_ns: u64) bool {
        const self: *MemberLease = @ptrCast(@alignCast(ctx.?));
        return self.live(now_ns);
    }
};

/// The owner's table of its members' leases: the holder-side
/// `LeaseOracle` face (a member not listed has LEFT or been EVICTED).
pub const OwnerLeases = struct {
    gpa: Allocator,
    lock: jc.SpinLock = .{},
    deadlines: std.AutoHashMapUnmanaged(table.ClientId, u64) = .empty,

    pub fn init(gpa: Allocator) OwnerLeases {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *OwnerLeases) void {
        self.deadlines.deinit(self.gpa);
    }

    pub fn grant(self: *OwnerLeases, client: table.ClientId, granted_at_ns: u64) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        try self.deadlines.put(self.gpa, client, granted_at_ns + table.T_OWNER_NS);
    }

    /// A leave or an eviction.
    pub fn remove(self: *OwnerLeases, client: table.ClientId) void {
        self.lock.lock();
        defer self.lock.unlock();
        _ = self.deadlines.remove(client);
    }

    pub fn oracle(self: *OwnerLeases) table.LeaseOracle {
        return .{ .ctx = self, .verdict = &verdictFn, .remaining = &remainingFn };
    }

    fn verdictFn(ctx: ?*anyopaque, client: table.ClientId, now_ns: u64) table.LeaseVerdict {
        const self: *OwnerLeases = @ptrCast(@alignCast(ctx.?));
        self.lock.lock();
        defer self.lock.unlock();
        const d = self.deadlines.get(client) orelse return .expired;
        return if (now_ns < d) .live else .expired;
    }

    fn remainingFn(ctx: ?*anyopaque, client: table.ClientId, now_ns: u64) ?u64 {
        const self: *OwnerLeases = @ptrCast(@alignCast(ctx.?));
        self.lock.lock();
        defer self.lock.unlock();
        const d = self.deadlines.get(client) orelse return 0;
        return d -| now_ns;
    }
};

// ---------------------------------------------------------------------------
// The quiescent reader.
// ---------------------------------------------------------------------------

/// How a `-o ro` open arms itself.
pub const ReaderArm = enum {
    /// A live writer exists (an exclusive writer lock on this host, or a
    /// heartbeat-fresh `writer_claim` on any volume): the token client.
    tokens,
    /// No live writer: the projection its open replayed, served exactly.
    quiescent,
};

pub fn armFor(live_writer_present: bool) ReaderArm {
    return if (live_writer_present) .tokens else .quiescent;
}

/// What a quiescent poll saw.
pub const WriterFace = enum {
    none,
    /// A same-host writer took the exclusive lock.
    same_host_lock,
    /// A remote writer's first checkpoint: the ledger advanced past the
    /// adopted seq.
    ledger_advance,
};

pub const QuiescentReader = struct {
    adopted_seq: u64,
    fail_stopped: std.atomic.Value(bool) = .init(false),

    pub fn init(adopted_seq: u64) QuiescentReader {
        stats.reader_quiescent.set(1);
        return .{ .adopted_seq = adopted_seq };
    }

    /// The poll's verdict: a writer by either face fail-stops the reader
    /// ONCE — adopting the advance would serve a live writer's state from
    /// a projection (R-SYM-4's second method). The swap is REFUSED.
    pub fn onPoll(self: *QuiescentReader, face: WriterFace, mountpoint: []const u8) void {
        if (face == .none) return;
        if (self.fail_stopped.swap(true, .acq_rel)) return;
        stats.reader_quiescent_writer_appeared.set(1);
        log.err("-o ro {s}: a writer appeared ({t}) under the quiescent reader — every metadata op now answers ESTALE; remount to join as a token reader", .{ mountpoint, face });
    }

    /// The op gate: the typed fail-stop (ESTALE), the root's attributes
    /// excepted (`.stats` stays reachable).
    pub fn admit(self: *const QuiescentReader, is_root_attr: bool) error{ReaderFailStopped}!void {
        if (!self.fail_stopped.load(.acquire)) return;
        if (is_root_attr) return;
        stats.reader_fail_stopped_refusals.inc();
        return error.ReaderFailStopped;
    }

    pub fn isFailStopped(self: *const QuiescentReader) bool {
        return self.fail_stopped.load(.acquire);
    }
};

// ---------------------------------------------------------------------------
// The epoch poll.
// ---------------------------------------------------------------------------

pub const IoError = error{ Io, OutOfMemory };

/// The ledger device: one slot, or the whole extent.
pub const LedgerIo = struct {
    ctx: *anyopaque,
    readSlot: *const fn (ctx: *anyopaque, slot: u64, out: *[SLOT_LEN]u8) IoError!void,
    readAll: *const fn (ctx: *anyopaque, out: []u8) IoError!void,
};

pub const PollOutcome = struct {
    /// The newest record adopted (null = nothing newer than `adopted`).
    newest: ?ledger.LedgerRecord,
    bytes_read: u64,
    /// The whole ledger was read (a torn slot or the belt).
    full_read: bool,
    /// The predicted slot held the next record.
    predicted_hit: bool,
};

pub const EpochPoll = struct {
    gpa: Allocator,
    io: LedgerIo,
    adopted_seq: u64,
    /// Polls since the last advance; at `GAP_BELT_POLLS` the belt reads
    /// the whole ledger once and resets.
    stopped_polls: u32 = 0,

    pub fn init(gpa: Allocator, io: LedgerIo, adopted_seq: u64) EpochPoll {
        return .{ .gpa = gpa, .io = io, .adopted_seq = adopted_seq };
    }

    const SlotRead = union(enum) { record: ledger.LedgerRecord, zero, torn };

    fn readSlot(self: *EpochPoll, slot: u64) IoError!SlotRead {
        var buf: [SLOT_LEN]u8 = undefined;
        try self.io.readSlot(self.io.ctx, slot, &buf);
        stats.meta_kv_revalidate_ledger_read_bytes.add(SLOT_LEN);
        if (ledger.LedgerRecord.decodeSlot(self.gpa, &buf)) |rec| return .{ .record = rec } else |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => {},
        }
        if (std.mem.allEqual(u8, &buf, 0)) return .zero;
        return .torn;
    }

    fn readWhole(self: *EpochPoll) IoError!?ledger.LedgerRecord {
        const image = try self.gpa.alloc(u8, LEDGER_LEN);
        defer self.gpa.free(image);
        try self.io.readAll(self.io.ctx, image);
        stats.meta_kv_revalidate_ledger_read_bytes.add(LEDGER_LEN);
        stats.meta_kv_revalidate_ledger_full_reads.inc();
        return ledger.newestValid(self.gpa, image);
    }

    /// ONE poll: the predicted slot first, successors while each holds the
    /// next seq, the fallback on a torn slot, the belt every 32 stopped
    /// polls. The caller owns `newest` (`deinit` it).
    pub fn poll(self: *EpochPoll) IoError!PollOutcome {
        stats.meta_kv_revalidate_polls.inc();
        const bytes_before = stats.meta_kv_revalidate_ledger_read_bytes.load();
        var out: PollOutcome = .{ .newest = null, .bytes_read = 0, .full_read = false, .predicted_hit = false };
        var newest: ?ledger.LedgerRecord = null;
        var expect = self.adopted_seq + 1;
        var first = true;
        walk: while (true) {
            switch (try self.readSlot(expect % ledger.ROOT_LEDGER_SLOTS)) {
                .record => |rec| {
                    // A writer that raced ahead by more than the ring lands a
                    // higher seq of the same residue — adopted the same way;
                    // an OLDER record is the stop.
                    if (rec.seq < expect) {
                        var r = rec;
                        r.deinit(self.gpa);
                        break :walk;
                    }
                    if (first) out.predicted_hit = true;
                    if (newest) |*n| n.deinit(self.gpa);
                    newest = rec;
                    expect = rec.seq + 1;
                },
                .zero => break :walk,
                .torn => {
                    if (newest) |*n| n.deinit(self.gpa);
                    newest = try self.readWhole();
                    out.full_read = true;
                    if (newest) |n| if (n.seq <= self.adopted_seq) {
                        newest.?.deinit(self.gpa);
                        newest = null;
                    };
                    break :walk;
                },
            }
            first = false;
        }
        if (newest == null and !out.full_read) {
            self.stopped_polls += 1;
            if (self.stopped_polls >= GAP_BELT_POLLS) {
                self.stopped_polls = 0;
                stats.meta_kv_revalidate_gap_scans.inc();
                newest = try self.readWhole();
                out.full_read = true;
                if (newest) |n| if (n.seq <= self.adopted_seq) {
                    newest.?.deinit(self.gpa);
                    newest = null;
                };
            }
        }
        if (newest) |n| {
            self.adopted_seq = n.seq;
            self.stopped_polls = 0;
        }
        if (out.predicted_hit) stats.meta_kv_revalidate_predicted_hits.inc();
        out.newest = newest;
        out.bytes_read = stats.meta_kv_revalidate_ledger_read_bytes.load() - bytes_before;
        return out;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// A RAM ledger the tests write checkpoint records into.
const RamLedger = struct {
    image: [LEDGER_LEN]u8 = @splat(0),
    reads: u32 = 0,

    fn write(self: *RamLedger, rec: ledger.LedgerRecord) !void {
        const img = try rec.encodeSlot();
        @memcpy(self.image[@intCast(rec.slotIndex() * ledger.ROOT_LEDGER_SLOT_LEN)..][0..SLOT_LEN], &img);
    }

    fn readSlot(ctx: *anyopaque, slot: u64, out: *[SLOT_LEN]u8) IoError!void {
        const self: *RamLedger = @ptrCast(@alignCast(ctx));
        self.reads += 1;
        @memcpy(out, self.image[@intCast(slot * ledger.ROOT_LEDGER_SLOT_LEN)..][0..SLOT_LEN]);
    }

    fn readAll(ctx: *anyopaque, out: []u8) IoError!void {
        const self: *RamLedger = @ptrCast(@alignCast(ctx));
        self.reads += 1;
        @memcpy(out[0..LEDGER_LEN], &self.image);
    }

    fn io(self: *RamLedger) LedgerIo {
        return .{ .ctx = self, .readSlot = &readSlot, .readAll = &readAll };
    }
};

fn recordAt(seq: u64) ledger.LedgerRecord {
    return .{ .seq = seq, .tree_roots = &.{}, .journal_tail_seq = seq * 10, .next_ino = 100 + seq, .alloc_bitmap_generation = 1, .node_seq_watermark = 1, .membership_stamp = ledger.MembershipStamp.oneMember(@splat(0x11)) };
}

test "the poll reads the predicted slot first: an idle writer costs 4 KiB; two new checkpoints 12 KiB; the hit rate over 32 advancing polls is 32/32" {
    const gpa = testing.allocator;
    var dev: RamLedger = .{};
    try dev.write(recordAt(1));
    var p = EpochPoll.init(gpa, dev.io(), 1);
    // Idle: the next slot is zero — 4 KiB, no record.
    var o = try p.poll();
    try testing.expectEqual(@as(u64, SLOT_LEN), o.bytes_read);
    try testing.expect(o.newest == null and !o.full_read and !o.predicted_hit);
    // Two checkpoints landed: 2 and 3 — the predicted slot, its successor,
    // then the zero stop = 12 KiB.
    try dev.write(recordAt(2));
    try dev.write(recordAt(3));
    o = try p.poll();
    try testing.expectEqual(@as(u64, 3 * SLOT_LEN), o.bytes_read);
    try testing.expectEqual(@as(u64, 3), o.newest.?.seq);
    try testing.expect(o.predicted_hit and !o.full_read);
    o.newest.?.deinit(gpa);
    try testing.expectEqual(@as(u64, 3), p.adopted_seq);
    // 32 advancing polls, one checkpoint each: every one a predicted hit.
    var hits: u32 = 0;
    var seq: u64 = 4;
    while (seq < 36) : (seq += 1) {
        try dev.write(recordAt(seq));
        var r = try p.poll();
        if (r.predicted_hit) hits += 1;
        try testing.expectEqual(seq, r.newest.?.seq);
        r.newest.?.deinit(gpa);
    }
    try testing.expectEqual(@as(u32, 32), hits);
    try testing.expect(hits >= 31);
}

test "a torn slot takes the whole-ledger fallback (4 KiB + 128 KiB) and adopts the newest valid record; a record older than the prediction is the stop" {
    const gpa = testing.allocator;
    var dev: RamLedger = .{};
    try dev.write(recordAt(1));
    try dev.write(recordAt(2));
    var p = EpochPoll.init(gpa, dev.io(), 1);
    // Tear slot 3 (seq 3's slot) after writing a valid 3: the checksum fails.
    try dev.write(recordAt(3));
    dev.image[3 * SLOT_LEN + 40] ^= 0xFF;
    const full_before = stats.meta_kv_revalidate_ledger_full_reads.load();
    var o = try p.poll();
    // Slot 2 = the prediction (hit), slot 3 torn → the fallback.
    try testing.expectEqual(@as(u64, 2 * SLOT_LEN + LEDGER_LEN), o.bytes_read);
    try testing.expect(o.full_read);
    try testing.expectEqual(@as(u64, 2), o.newest.?.seq);
    o.newest.?.deinit(gpa);
    try testing.expectEqual(full_before + 1, stats.meta_kv_revalidate_ledger_full_reads.load());
    // An OLDER record in the predicted slot (a ring wrap not yet reached):
    // the stop, nothing adopted.
    var dev2: RamLedger = .{};
    try dev2.write(recordAt(3));
    var q = EpochPoll.init(gpa, dev2.io(), 34);
    const o2 = try q.poll();
    try testing.expect(o2.newest == null);
    try testing.expectEqual(@as(u64, SLOT_LEN), o2.bytes_read);
}

test "the gap belt: a seq the writer consumed without a record stops the prediction for ever; after 32 stopped polls the belt reads the whole ledger once and adopts past the gap" {
    const gpa = testing.allocator;
    var dev: RamLedger = .{};
    try dev.write(recordAt(1));
    // Seq 2 consumed with no record; seq 3 written.
    try dev.write(recordAt(3));
    var p = EpochPoll.init(gpa, dev.io(), 1);
    const scans_before = stats.meta_kv_revalidate_gap_scans.load();
    var i: u32 = 0;
    while (i < GAP_BELT_POLLS - 1) : (i += 1) {
        const o = try p.poll();
        try testing.expect(o.newest == null and !o.full_read);
    }
    var o = try p.poll();
    try testing.expect(o.full_read);
    try testing.expectEqual(@as(u64, 3), o.newest.?.seq);
    o.newest.?.deinit(gpa);
    try testing.expectEqual(scans_before + 1, stats.meta_kv_revalidate_gap_scans.load());
    try testing.expectEqual(@as(u32, 0), p.stopped_polls);
}

test "the quiescent reader: a writer by either face fail-stops ONCE; every op answers ReaderFailStopped except the root's attributes" {
    var q = QuiescentReader.init(7);
    try q.admit(false);
    q.onPoll(.none, "/m");
    try testing.expect(!q.isFailStopped());
    log.testing.expectErrorLogs(1);
    q.onPoll(.ledger_advance, "/m");
    q.onPoll(.same_host_lock, "/m");
    try log.testing.expectAllFired();
    try testing.expect(q.isFailStopped());
    try testing.expectError(error.ReaderFailStopped, q.admit(false));
    try q.admit(true);
    try testing.expectEqual(@as(u64, 1), stats.reader_quiescent_writer_appeared.load());
    try testing.expectEqual(ReaderArm.quiescent, armFor(false));
    try testing.expectEqual(ReaderArm.tokens, armFor(true));
}

test "the member lease: T_owner from the grant; the owner's table answers live/expired and a member it does not list is EXPIRED" {
    var ml: MemberLease = .{};
    try testing.expect(!ml.live(0));
    ml.renew(1000);
    try testing.expect(ml.live(1000 + table.T_OWNER_NS - 1));
    try testing.expect(!ml.live(1000 + table.T_OWNER_NS));
    try testing.expectEqual(table.T_OWNER_NS, ml.remainingNs(1000));
    var ol = OwnerLeases.init(testing.allocator);
    defer ol.deinit();
    const o = ol.oracle();
    try testing.expectEqual(table.LeaseVerdict.expired, o.verdict(o.ctx, 5, 0));
    try ol.grant(5, 10);
    try testing.expectEqual(table.LeaseVerdict.live, o.verdict(o.ctx, 5, 10));
    try testing.expectEqual(@as(?u64, table.T_OWNER_NS - 5), o.remaining(o.ctx, 5, 15));
    try testing.expectEqual(table.LeaseVerdict.expired, o.verdict(o.ctx, 5, 10 + table.T_OWNER_NS));
    ol.remove(5);
    try testing.expectEqual(table.LeaseVerdict.expired, o.verdict(o.ctx, 5, 10));
}

test "every reader TTL derives to 0 under tokens; an explicit -o *_timeout on a token reader refuses loud naming the law" {
    var o: mount_options.MountOptions = .{ .read_only = true };
    const ttls = try tokenReaderTtls(&o);
    try testing.expectEqual(@as(u64, 0), ttls.attr_ns + ttls.entry_ns + ttls.dir_entry_ns + ttls.negative_ns);
    try testing.expect(!ttls.negativeEnabled());
    try testing.expectEqual(@as(u64, 0), stats.reader_staleness_bound_ms.load());
    try testing.expectEqual(mount_options.Outcome.ok, mount_options.parseOptionString(&o, "attr_timeout=1"));
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.ExplicitTtlRefused, tokenReaderTtls(&o));
    try log.testing.expectAllFired();
}
