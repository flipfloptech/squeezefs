//! The cross-owner INTENT record (charter §5.5 "Cross-owner namespace
//! ops": "one intent in the initiator's ring, foreign steps shipped, the
//! roll-forward at mount and on the cadence"; `docs/porting/
//! sym_tokens_intents_striping.md` §2.1, §2.8): the v4 image — a 32 B
//! `IntentHeaderWire` + `step_count` × `XvStepWire` (48 B + the padded
//! name) checksummed hash64 over the field-zeroed image; the KEY law
//! (the id IS the key: 56 bits in the xattr hash field, 8 in the
//! collision byte — injective, so a write needs no chain probe and no new
//! lock); the HOME law (local 0 of a slot's namespace, which no inode ever
//! owns: cursors mint from 2); the `tx_id` minter (`writer_term << 40 |
//! seq`, the seq a per-mount monotone counter recovered as `max(open) + 1`
//! — decision 0002 Q7: never the wall clock); the REGISTER of open intents
//! (`minted ≡ retired + open`, the must-stay-0 `stuck` gauge); the
//! ABORTING flag (0002 Q6: a compensation flips it first so a roll-forward
//! that reads it runs the INVERSE plan — the crash window between an
//! applied step and the last inverse is closed on the record).
//!
//! PORTABLE. Every registry is a field of the set's plane, never a process
//! static (0002 row 259's law).

const std = @import("std");
const fuzzkit = @import("../../../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const checksum = @import("../../../core/checksum.zig");
const clock = @import("../../../core/clock.zig");
const sync = @import("../../../sync/sync.zig");
const verbs = @import("../../../wire/verbs.zig");
const codec = @import("../../../wire/codec.zig");
const key = @import("../../key.zig");
const record = @import("../../record.zig");
const acquire = @import("../slots/acquire.zig");
const token_table = @import("../tokens/token_table.zig");

const assert = std.debug.assert;

pub const StepWire = verbs.XvStepWire;
pub const StepKind = verbs.StepKind;
pub const ParentUpdate = verbs.ParentUpdate;

/// `XV_MAX_STEPS = 2 × MINT_SPREAD + 8 = 136`: the widest plan is the
/// one-intent `rmdir` of a K = 64 striped directory — the parent dentry +
/// `K + 2` marker removes + `K + 1` count steps — with the pre-striping
/// headroom; the travelling-guard plan shares the bound (`verbs.
/// XV_GUARDS_MAX`, `dlm.wide_requests`).
pub const XV_MAX_STEPS: usize = 2 * acquire.MINT_SPREAD + 8;

/// The widest image a plan encodes to: the header, every step but one
/// carrying at most a MARKER name (`\0sqz.stripe:NN`, 14 B, padded to
/// 16 — the striped `rmdir`'s `K + 2` removes; the count steps carry
/// none) and ONE user name at `NAME_MAX` (the parent's dentry step, padded
/// to 8) — ≈ 8.9 KiB, inside the 16 KiB record cap at 64 KiB nodes.
pub const IMAGE_MAX: usize = IntentHeaderWire.LEN + (XV_MAX_STEPS - 1) * (@sizeOf(StepWire) + codec.padTo(14, 8)) + @sizeOf(StepWire) + codec.padTo(record.NAME_MAX, 8);

comptime {
    assert(XV_MAX_STEPS == 136 and XV_MAX_STEPS == verbs.XV_GUARDS_MAX);
    assert(IMAGE_MAX < 16 << 10);
}

/// The op the intent carries (the header's `op`; explicit values — the
/// record never carries a declaration index).
pub const Op = enum(u8) {
    link = 1,
    unlink = 2,
    rmdir = 3,
    rename = 4,
    exchange = 5,
    create = 6,
    stripe_dir = 7,
};

/// Header `flags` bit 0: the plan is being COMPENSATED — a roll-forward
/// runs the inverse plan and retires (0002 Q6).
pub const FLAG_ABORTING: u8 = 1 << 0;
const FLAGS_KNOWN: u8 = FLAG_ABORTING;

pub const MAGIC: [8]u8 = "SQZXTENT".*;
pub const VERSION: u16 = 1;

/// 32 B, little-endian.
pub const IntentHeaderWire = extern struct {
    magic: [8]u8,
    version_le: [2]u8,
    op: u8,
    flags: u8,
    tx_id_le: [8]u8,
    step_count_le: [2]u8,
    /// The leading steps the initiator ATTESTED applied when it left the
    /// intent open (0 = none attested; ≤ `step_count`): the roll-forward's
    /// compensation inverts them however a later op moved their objects.
    applied_le: [2]u8,
    csum_le: [8]u8,

    pub const LEN: usize = 32;
    pub const CSUM_OFF: usize = 24;
};

comptime {
    assert(@sizeOf(IntentHeaderWire) == IntentHeaderWire.LEN);
    assert(@offsetOf(IntentHeaderWire, "csum_le") == IntentHeaderWire.CSUM_OFF);
    assert(@sizeOf(StepWire) == 48);
}

/// One step as the record carries it: the wire word plus its name bytes
/// (borrowed from the image on decode; the plan builder's on encode).
pub const StepRec = struct {
    w: StepWire,
    name: []const u8,

    /// The step's bytes in the image: 48 + the name padded to 8.
    pub fn encodedLen(self: StepRec) usize {
        return @sizeOf(StepWire) + codec.padTo(self.name.len, 8);
    }
};

/// The decoded record.
pub const Intent = struct {
    op: Op,
    flags: u8,
    tx_id: u64,
    /// `applied_le` (see the header).
    applied: u16 = 0,
    steps: []const StepRec,

    pub fn aborting(self: Intent) bool {
        return self.flags & FLAG_ABORTING != 0;
    }
};

pub const EncodeError = error{ TooManySteps, ValueTooLarge };
pub const DecodeError = error{Corrupt};

/// The encoded size of `steps` under one header.
pub fn encodedLen(steps: []const StepRec) usize {
    var n: usize = IntentHeaderWire.LEN;
    for (steps) |s| n += s.encodedLen();
    return n;
}

/// Encode into `out` (≥ `encodedLen(steps)`); refuses 0 or more than
/// `XV_MAX_STEPS` steps, and an image past `cap` (the volume's record
/// cap `min(64 KiB, node_size/4)` — charter §5.1; the widest plan encodes
/// to ≈ 8.6 KiB, inside the 16 KiB cap at 64 KiB nodes).
pub fn encode(op: Op, flags: u8, tx_id: u64, applied: u16, steps: []const StepRec, cap: usize, out: []u8) EncodeError![]u8 {
    if (steps.len == 0 or steps.len > XV_MAX_STEPS) return error.TooManySteps;
    assert(applied <= steps.len);
    const n = encodedLen(steps);
    if (n > cap or n > out.len) return error.ValueTooLarge;
    var h: IntentHeaderWire = undefined;
    h.magic = MAGIC;
    std.mem.writeInt(u16, &h.version_le, VERSION, .little);
    h.op = @backingInt(op);
    h.flags = flags;
    std.mem.writeInt(u64, &h.tx_id_le, tx_id, .little);
    std.mem.writeInt(u16, &h.step_count_le, @intCast(steps.len), .little);
    std.mem.writeInt(u16, &h.applied_le, applied, .little);
    @memset(&h.csum_le, 0);
    out[0..IntentHeaderWire.LEN].* = std.mem.toBytes(h);
    var off: usize = IntentHeaderWire.LEN;
    for (steps) |s| {
        assert(s.w.check() and s.w.name_len == s.name.len);
        out[off..][0..@sizeOf(StepWire)].* = std.mem.toBytes(s.w);
        off += @sizeOf(StepWire);
        @memcpy(out[off..][0..s.name.len], s.name);
        const padded = codec.padTo(s.name.len, 8);
        @memset(out[off + s.name.len .. off + padded], 0);
        off += padded;
    }
    assert(off == n);
    checksum.stamp(out[0..n], IntentHeaderWire.CSUM_OFF);
    return out[0..n];
}

/// Total decode: magic, version, flags, checksum, 1..=136 steps, every
/// step's `check()`, zero padding, NO trailing bytes. The steps are
/// written into `steps_out` (≥ `XV_MAX_STEPS`); names borrow `bytes`.
pub fn decode(bytes: []const u8, steps_out: []StepRec) DecodeError!Intent {
    if (bytes.len < IntentHeaderWire.LEN) return error.Corrupt;
    const h = std.mem.bytesToValue(IntentHeaderWire, bytes[0..IntentHeaderWire.LEN]);
    if (!std.mem.eql(u8, &h.magic, &MAGIC)) return error.Corrupt;
    if (std.mem.readInt(u16, &h.version_le, .little) != VERSION) return error.Corrupt;
    if (h.flags & ~FLAGS_KNOWN != 0) return error.Corrupt;
    const op = std.enums.fromInt(Op, h.op) orelse return error.Corrupt;
    const n = std.mem.readInt(u16, &h.step_count_le, .little);
    if (n == 0 or n > XV_MAX_STEPS or n > steps_out.len) return error.Corrupt;
    const applied = std.mem.readInt(u16, &h.applied_le, .little);
    if (applied > n) return error.Corrupt;
    if (!checksum.verify(bytes, IntentHeaderWire.CSUM_OFF)) return error.Corrupt;
    var off: usize = IntentHeaderWire.LEN;
    var i: usize = 0;
    while (i < n) : (i += 1) {
        if (bytes.len - off < @sizeOf(StepWire)) return error.Corrupt;
        const w = std.mem.bytesToValue(StepWire, bytes[off..][0..@sizeOf(StepWire)]);
        if (!w.check()) return error.Corrupt;
        if (!stepWordsAdmissible(w)) return error.Corrupt;
        off += @sizeOf(StepWire);
        const padded = codec.padTo(w.name_len, 8);
        if (bytes.len - off < padded) return error.Corrupt;
        const name = bytes[off..][0..w.name_len];
        if (!std.mem.allEqual(u8, bytes[off + w.name_len .. off + padded], 0)) return error.Corrupt;
        steps_out[i] = .{ .w = w, .name = name };
        off += padded;
    }
    if (off != bytes.len) return error.Corrupt;
    return .{ .op = op, .flags = h.flags, .tx_id = std.mem.readInt(u64, &h.tx_id_le, .little), .applied = applied, .steps = steps_out[0..n] };
}

/// Every unused word of a step is zero (spec §2.1's per-kind field
/// table): a dentry step carries no counts, a count step no name, …
pub fn stepWordsAdmissible(w: StepWire) bool {
    const kind: StepKind = @fromBackingInt(w.kind);
    return switch (kind) {
        .remove_dentry => w.c == 0 and w.w0 == 0 and w.w1 == 0 and w.w2 == 0 and w.w3 == 0 and w.flags == 0,
        .insert_dentry => w.c == 0 and w.w0 == 0 and w.w1 == 0 and w.w2 == 0 and w.w3 == 0 and w.flags == 0,
        .set_nlink => w.b == 0 and w.w2 == 0 and w.w3 == 0 and w.ft_bits == 0 and w.parent_update == 0 and (w.flags & verbs.STEP_FLAG_HAS_CTIME != 0 or w.c == 0),
        .touch_ctime => w.b == 0 and w.w0 == 0 and w.w1 == 0 and w.w2 == 0 and w.w3 == 0 and w.ft_bits == 0 and w.parent_update == 0 and w.flags == 0,
        .mint_inode => w.b == 0 and w.c == 0 and w.ft_bits == 0 and w.parent_update == 0 and w.flags == 0,
        .create_inode => w.ft_bits == 0 and w.parent_update == 0 and w.flags == 0,
    };
}

// ---------------------------------------------------------------------------
// The key law and the home law.
// ---------------------------------------------------------------------------

/// The intent HOME of routing slot `slot`: local 0 of its namespace — the
/// native slot's is key ino 0, a guest slot's `guestLocalIno(slot, 0)`;
/// "guest cursors mint from 2, so no inode ever owns it".
pub fn homeIno(slot: u16) u64 {
    return if (slot == 0) 0 else key.guestLocalIno(slot, 0);
}

/// The slot an intent home belongs to (`homeIno`'s inverse).
pub fn slotOfHome(home: u64) u16 {
    if (key.splitGuestLocal(home)) |g| return g.slot;
    return 0;
}

/// The record's key: the xattr key `(home, tx_id & HASH56_MAX, tx_id >>
/// 56)` — the full 64 bits, injectively.
pub fn recordKey(home: u64, tx_id: u64) [key.XATTR_KEY_LEN]u8 {
    return key.xattrKey(home, tx_id & key.HASH56_MAX, @intCast(tx_id >> 56));
}

/// The `tx_id` an intent key names.
pub fn txIdOfKey(k: []const u8) ?u64 {
    if (k.len != key.XATTR_KEY_LEN) return null;
    const xk = std.mem.bytesToValue(key.XattrKey, k[0..key.XATTR_KEY_LEN]);
    return (@as(u64, xk.collSeq()) << 56) | xk.hash56();
}

/// The inclusive range of every intent under `home`.
pub const HomeRange = struct { start: [key.XATTR_KEY_LEN]u8, end: [key.XATTR_KEY_LEN]u8 };

pub fn homeRange(home: u64) HomeRange {
    return .{ .start = key.xattrKey(home, 0, 0), .end = key.xattrKey(home, key.HASH56_MAX, std.math.maxInt(u8)) };
}

/// The record's xattr NAME `xtx:{tx_id:016x}` (offline-probe readable).
pub const NAME_LEN: usize = 4 + 16;

pub fn recordName(tx_id: u64, out: *[NAME_LEN]u8) []const u8 {
    return std.fmt.bufPrint(out, "xtx:{x:0>16}", .{tx_id}) catch unreachable;
}

/// The stored VALUE: the `XattrValue{name, image}` envelope.
pub fn storedValueLen(image_len: usize) usize {
    return 1 + NAME_LEN + image_len;
}

pub fn storedValue(tx_id: u64, image: []const u8, out: []u8) []const u8 {
    var name_buf: [NAME_LEN]u8 = undefined;
    const name = recordName(tx_id, &name_buf);
    return (record.XattrValue{ .name = name, .value = image }).encodeInto(out) catch unreachable;
}

/// Decode a stored value: the name must match the key's `tx_id` (an
/// id-mismatched record refuses the mount — spec §2.8).
pub fn decodeStored(k: []const u8, value: []const u8, steps_out: []StepRec) DecodeError!Intent {
    const xv = record.XattrValue.decode(value) catch return error.Corrupt;
    const tx_id = txIdOfKey(k) orelse return error.Corrupt;
    var name_buf: [NAME_LEN]u8 = undefined;
    if (!std.mem.eql(u8, xv.name, recordName(tx_id, &name_buf))) return error.Corrupt;
    const it = try decode(xv.value, steps_out);
    if (it.tx_id != tx_id) return error.Corrupt;
    return it;
}

// ---------------------------------------------------------------------------
// The tx_id minter.
// ---------------------------------------------------------------------------

/// `tx_id = writer_term << 40 | seq`: the term (the region's page term —
/// unique per incarnation) makes ids unique across crashes; `seq` is a
/// per-mount monotone CAS word recovered as `max(open intents' seq) + 1`
/// at open — never a wall clock (0002 Q7).
pub const SEQ_BITS: u6 = 40;
pub const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;

pub const Minter = struct {
    term: u64,
    seq: std.atomic.Value(u64),

    pub fn init(term: u64) Minter {
        return .{ .term = term, .seq = .init(1) };
    }

    /// Recover past every open intent of this term the scan found.
    pub fn recover(self: *Minter, open_tx_id: u64) void {
        if (open_tx_id >> SEQ_BITS != self.term) return;
        _ = self.seq.fetchMax((open_tx_id & SEQ_MASK) + 1, .monotonic);
    }

    pub fn next(self: *Minter) u64 {
        const s = self.seq.fetchAdd(1, .monotonic);
        assert(s <= SEQ_MASK);
        return (self.term << SEQ_BITS) | s;
    }

    pub fn termOf(tx_id: u64) u64 {
        return tx_id >> SEQ_BITS;
    }
};

// ---------------------------------------------------------------------------
// The register.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Intents whose record became durable (or adopted at a roll-forward).
    pub var xv_cross_owner_intents_minted: counters.ShardedCounter = .{};
    /// Counted entries removed from the register (retired, or found gone).
    pub var xv_cross_owner_intents_retired: counters.ShardedCounter = .{};
    /// The register's population (`minted ≡ retired + open`).
    pub var xv_cross_owner_intents_open: counters.Gauge = .{};
    /// Abandoned entries older than `STUCK_GRACE_NS` — must stay 0: a
    /// slot's dead holder is evicted and its slot recovered inside it.
    pub var xv_cross_owner_intents_stuck: counters.Gauge = .{};
    pub var xv_cross_owner_intent_stuck: counters.Tripwire = .init("xv_cross_owner_intent_stuck");
    /// Inserts the full register refused (`Deferred` to the op — the
    /// cadence's backlog is the backpressure; the adoption resumes at the
    /// next pass). Never a hash-map overrun: a failing device once filled
    /// it with ghosts of intents that never became durable.
    pub var xv_register_full_refusals: counters.ShardedCounter = .{};
};

/// `stuck_grace = T_owner` (45 s): a slot's dead holder is evicted and its
/// slot recovered inside the membership TTL; an intent abandoned past it
/// names a slot no live holder serves.
pub const STUCK_GRACE_NS: u64 = token_table.T_OWNER_NS;

pub const IntentState = enum { in_flight, abandoned };

pub const Entry = struct {
    since_ns: u64,
    state: IntentState,
    /// `minted` was counted (the record is durable).
    counted: bool,
    /// The home the record lives under (the cadence's reconciliation).
    home: u64,
};

/// Open intents of this mount: `tx_id → Entry`, sized once at the plane's
/// arm (`XV_MAX_STEPS`-class plans in flight per lane; the live population
/// is bounded by the op concurrency).
pub const Register = struct {
    io: Io,
    lock: sync.Mutex = .init,
    map: std.AutoHashMapUnmanaged(u64, Entry) = .empty,
    /// The entries `init` reserved room for: an insert past it refuses.
    capacity: u32,

    pub const InsertError = error{RegisterFull};

    pub fn init(gpa: Allocator, io: Io, capacity: u32) Allocator.Error!Register {
        var r: Register = .{ .io = io, .capacity = capacity };
        try r.map.ensureTotalCapacity(gpa, capacity);
        return r;
    }

    /// Under the lock: room for one more new entry.
    fn admitLocked(self: *Register) InsertError!void {
        if (self.map.count() < self.capacity) return;
        stats.xv_register_full_refusals.inc();
        return error.RegisterFull;
    }

    pub fn deinit(self: *Register, gpa: Allocator) void {
        self.map.deinit(gpa);
    }

    /// Registered IN FLIGHT before the record exists: a cadence scan that
    /// sees the record always finds its owner.
    pub fn noteInFlight(self: *Register, tx_id: u64, home: u64, now_ns: u64) InsertError!void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        try self.admitLocked();
        self.map.putAssumeCapacity(tx_id, .{ .since_ns = now_ns, .state = .in_flight, .counted = false, .home = home });
    }

    /// The record is durable: `minted` counts once.
    pub fn noteDurable(self: *Register, tx_id: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        const e = self.map.getPtr(tx_id) orelse return;
        if (e.counted) return;
        e.counted = true;
        stats.xv_cross_owner_intents_minted.inc();
        stats.xv_cross_owner_intents_open.add(1);
    }

    /// Adopted by a roll-forward scan: counted at adoption, abandoned.
    pub fn adopt(self: *Register, tx_id: u64, home: u64, now_ns: u64) InsertError!void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.map.getPtr(tx_id)) |e| {
            if (!e.counted) {
                e.counted = true;
                stats.xv_cross_owner_intents_minted.inc();
                stats.xv_cross_owner_intents_open.add(1);
            }
            return;
        }
        try self.admitLocked();
        self.map.putAssumeCapacity(tx_id, .{ .since_ns = now_ns, .state = .abandoned, .counted = true, .home = home });
        stats.xv_cross_owner_intents_minted.inc();
        stats.xv_cross_owner_intents_open.add(1);
    }

    /// The live op gave up (a ship failed): the cadence's to complete.
    pub fn noteAbandoned(self: *Register, tx_id: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.map.getPtr(tx_id)) |e| e.state = .abandoned;
    }

    /// The live roll-forward took it back (adoption by register STATE).
    pub fn noteTaken(self: *Register, tx_id: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.map.getPtr(tx_id)) |e| e.state = .in_flight;
    }

    /// Retired (or found gone): `retired` counts every counted removal.
    pub fn forget(self: *Register, tx_id: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        const e = self.map.fetchRemove(tx_id) orelse return;
        if (e.value.counted) {
            stats.xv_cross_owner_intents_retired.inc();
            stats.xv_cross_owner_intents_open.sub(1);
        }
    }

    pub fn stateOf(self: *Register, tx_id: u64) ?IntentState {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        const e = self.map.get(tx_id) orelse return null;
        return e.state;
    }

    pub fn anyAbandoned(self: *Register) bool {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        var it = self.map.valueIterator();
        while (it.next()) |e| if (e.state == .abandoned) return true;
        return false;
    }

    /// The abandoned ids (into `out`, ascending), for a cadence pass.
    pub fn abandoned(self: *Register, out: []u64) []u64 {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        var n: usize = 0;
        var it = self.map.iterator();
        while (it.next()) |e| {
            if (e.value_ptr.state != .abandoned or n == out.len) continue;
            out[n] = e.key_ptr.*;
            n += 1;
        }
        std.mem.sort(u64, out[0..n], {}, std.sort.asc(u64));
        return out[0..n];
    }

    /// Reconcile against a durable scan: an abandoned entry no scan lists
    /// under its home was retired by another appender or moved with its
    /// slot — forgotten, never a ghost on `open`/`stuck`.
    pub fn forgetAbandonedAbsent(self: *Register, home: u64, present_sorted: []const u64) void {
        var gone: [64]u64 = undefined;
        var n: usize = 0;
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            var it = self.map.iterator();
            while (it.next()) |e| {
                if (e.value_ptr.state != .abandoned or e.value_ptr.home != home or n == gone.len) continue;
                if (std.sort.binarySearch(u64, present_sorted, e.key_ptr.*, orderU64) == null) {
                    gone[n] = e.key_ptr.*;
                    n += 1;
                }
            }
        }
        for (gone[0..n]) |id| self.forget(id);
    }

    fn orderU64(ctx: u64, item: u64) std.math.Order {
        return std.math.order(ctx, item);
    }

    /// Publish `open` and `stuck`; a stuck intent fires the tripwire once
    /// per transition into the stuck state.
    pub fn publish(self: *Register, now_ns: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        var stuck: u64 = 0;
        var it = self.map.valueIterator();
        while (it.next()) |e| {
            if (e.state == .abandoned and now_ns -| e.since_ns > STUCK_GRACE_NS) stuck += 1;
        }
        const before = stats.xv_cross_owner_intents_stuck.load();
        stats.xv_cross_owner_intents_stuck.set(stuck);
        if (stuck > before) stats.xv_cross_owner_intent_stuck.fire("an abandoned cross-owner intent is older than the membership TTL");
    }

    pub fn count(self: *Register) usize {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        return self.map.count();
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn sampleSteps() [3]StepRec {
    return .{
        .{ .w = .{ .kind = 6, .parent_update = 0, .name_len = 0, .flags = 0, .ft_bits = 0, .a = 1000, .b = 0, .c = 77, .w0 = 0o100644, .w1 = 1, .w2 = 2, .w3 = 0 }, .name = "" },
        .{ .w = .{ .kind = 2, .parent_update = 3, .name_len = 5, .flags = 0, .ft_bits = 0o100000, .a = 9, .b = 1000, .c = 0, .w0 = 0, .w1 = 0, .w2 = 0, .w3 = 0 }, .name = "hello" },
        .{ .w = .{ .kind = 3, .parent_update = 0, .name_len = 0, .flags = verbs.STEP_FLAG_HAS_CTIME, .ft_bits = 0, .a = 1000, .b = 0, .c = 88, .w0 = 1, .w1 = 2, .w2 = 0, .w3 = 0 }, .name = "" },
    };
}

test "the intent image round-trips: 32 B header + 48 B steps + padded names, hash64 over the field-zeroed image; the attested applied prefix survives; every refusal class — magic, version, flags, an applied prefix past the steps, checksum, 0 or 137 steps, a bad step word, a trailing byte, a non-zero pad, a record past the cap" {
    var buf: [4096]u8 = undefined;
    const steps = sampleSteps();
    const img = try encode(.create, 0, (3 << 40) | 42, 0, &steps, 16 << 10, &buf);
    try testing.expectEqual(@as(usize, 32 + 48 + 56 + 48), img.len);
    var out: [XV_MAX_STEPS]StepRec = undefined;
    const it = try decode(img, &out);
    try testing.expectEqual(Op.create, it.op);
    try testing.expectEqual(@as(u64, (3 << 40) | 42), it.tx_id);
    try testing.expectEqual(@as(usize, 3), it.steps.len);
    try testing.expectEqualStrings("hello", it.steps[1].name);
    try testing.expect(!it.aborting());
    try testing.expectEqual(@as(u16, 0), it.applied);
    // The ABORTING flag survives.
    const img2 = try encode(.create, FLAG_ABORTING, 7, 0, &steps, 16 << 10, &buf);
    try testing.expect((try decode(img2, &out)).aborting());
    // The attested prefix survives, up to every step.
    var buf3: [4096]u8 = undefined;
    try testing.expectEqual(@as(u16, 3), (try decode(try encode(.create, 0, 7, 3, &steps, 16 << 10, &buf3), &out)).applied);
    // Refusals.
    var bad: [4096]u8 = undefined;
    const mk = struct {
        fn flip(src: []const u8, dst: []u8, off: usize, v: u8) []u8 {
            @memcpy(dst[0..src.len], src);
            dst[off] = v;
            return dst[0..src.len];
        }
    };
    try testing.expectError(error.Corrupt, decode(mk.flip(img, &bad, 0, 'x'), &out)); // magic
    try testing.expectError(error.Corrupt, decode(mk.flip(img, &bad, 8, 2), &out)); // version (checksum would also fail)
    try testing.expectError(error.Corrupt, decode(mk.flip(img, &bad, 11, 2), &out)); // unknown flag
    // An applied prefix past the step count (re-stamped so only the
    // bound refuses).
    @memcpy(bad[0..img.len], img);
    bad[22] = 4;
    checksum.stamp(bad[0..img.len], IntentHeaderWire.CSUM_OFF);
    try testing.expectError(error.Corrupt, decode(bad[0..img.len], &out));
    try testing.expectError(error.Corrupt, decode(mk.flip(img, &bad, 40, 0xFF), &out)); // a step byte: checksum
    try testing.expectError(error.Corrupt, decode(img[0 .. img.len - 1], &out));
    @memcpy(bad[0..img.len], img);
    bad[img.len] = 0;
    try testing.expectError(error.Corrupt, decode(bad[0 .. img.len + 1], &out));
    // A non-zero pad byte (re-stamped so only the pad law refuses).
    @memcpy(bad[0..img.len], img);
    bad[32 + 48 + 48 + 5] = 1;
    checksum.stamp(bad[0..img.len], IntentHeaderWire.CSUM_OFF);
    try testing.expectError(error.Corrupt, decode(bad[0..img.len], &out));
    // A step with a count word on a dentry kind (step 1's `w0`).
    @memcpy(bad[0..img.len], img);
    bad[32 + 48 + 32] = 1;
    checksum.stamp(bad[0..img.len], IntentHeaderWire.CSUM_OFF);
    try testing.expectError(error.Corrupt, decode(bad[0..img.len], &out));
    // The step bounds and the cap.
    try testing.expectError(error.TooManySteps, encode(.create, 0, 1, 0, &.{}, 16 << 10, &buf));
    var many: [XV_MAX_STEPS + 1]StepRec = undefined;
    for (&many) |*s| s.* = steps[0];
    try testing.expectError(error.TooManySteps, encode(.create, 0, 1, 0, &many, 1 << 20, &buf));
    var big: [XV_MAX_STEPS * 64 + 64]u8 = undefined;
    const wide = try encode(.rmdir, 0, 1, 0, many[0..XV_MAX_STEPS], 1 << 20, &big);
    try testing.expectEqual(@as(usize, 32 + 48 * XV_MAX_STEPS), wide.len);
    try testing.expectError(error.ValueTooLarge, encode(.rmdir, 0, 1, 0, many[0..XV_MAX_STEPS], 4096, &big));
    try testing.expectEqual(XV_MAX_STEPS, (try decode(wide, &out)).steps.len);
}

fn decodeTotalFuzz(_: void, smith: *std.testing.Smith) anyerror!void {
    var in: [1024]u8 = undefined;
    const len = smith.slice(&in);
    const input = in[0..len];
    var out: [XV_MAX_STEPS]StepRec = undefined;
    // The stored form under an arbitrary key: total, and the key's tx_id law holds.
    var stored_steps: [XV_MAX_STEPS]StepRec = undefined;
    const split = @min(len, key.XATTR_KEY_LEN);
    _ = decodeStored(input[0..split], input[split..], &stored_steps) catch {};
    const it = decode(input, &out) catch return;
    var buf: [XV_MAX_STEPS * 56 + 64]u8 = undefined;
    const re = try encode(it.op, it.flags, it.tx_id, it.applied, it.steps, 1 << 20, &buf);
    try testing.expectEqualSlices(u8, input, re);
}

test "fuzz: the intent decoder and its stored form are total and encode∘decode is the identity" {
    var buf: [1024]u8 = undefined;
    const steps = sampleSteps();
    const img = try encode(.create, 0, 42, 0, &steps, 16 << 10, &buf);
    var framed: [4 + 1024]u8 = undefined;
    std.mem.writeInt(u32, framed[0..4], @intCast(img.len), .little);
    @memcpy(framed[4..][0..img.len], img);
    try fuzzkit.fuzz({}, decodeTotalFuzz, .{ .corpus = &.{framed[0 .. 4 + img.len]} });
    // The PRNG arm: mutations of a valid image never panic and a surviving
    // decode re-encodes byte-identically.
    var prng = std.Random.DefaultPrng.init(7);
    const rnd = prng.random();
    var i: usize = 0;
    while (i < 2000) : (i += 1) {
        var m: [1024]u8 = undefined;
        @memcpy(m[0..img.len], img);
        const flips = 1 + rnd.uintLessThan(usize, 4);
        for (0..flips) |_| m[rnd.uintLessThan(usize, img.len)] = rnd.int(u8);
        const cut = if (rnd.boolean()) img.len else rnd.uintLessThan(usize, img.len + 1);
        var out: [XV_MAX_STEPS]StepRec = undefined;
        const it = decode(m[0..cut], &out) catch continue;
        var re_buf: [1024]u8 = undefined;
        const re = try encode(it.op, it.flags, it.tx_id, it.applied, it.steps, 1 << 20, &re_buf);
        try testing.expectEqualSlices(u8, m[0..cut], re);
    }
}

test "the key law and the home law: the id is the key injectively; a home is local 0 of a slot's namespace; the stored value names its id; the minter's term/seq shape and its recovery" {
    try testing.expectEqual(@as(u64, 0), homeIno(0));
    try testing.expectEqual(key.guestLocalIno(17, 0), homeIno(17));
    try testing.expectEqual(@as(u16, 17), slotOfHome(homeIno(17)));
    try testing.expectEqual(@as(u16, 0), slotOfHome(homeIno(0)));
    const id: u64 = 0xAB_0000_0012_3456_78;
    const k = recordKey(homeIno(3), id);
    try testing.expectEqual(id, txIdOfKey(&k).?);
    try testing.expect(txIdOfKey(k[0..5]) == null);
    const r = homeRange(homeIno(3));
    try testing.expect(!key.lessThan(&k, &r.start) and !key.lessThan(&r.end, &k));
    var buf: [1024]u8 = undefined;
    const steps = sampleSteps();
    const img = try encode(.unlink, 0, id, 0, &steps, 16 << 10, &buf);
    var vbuf: [1024]u8 = undefined;
    const v = storedValue(id, img, &vbuf);
    try testing.expectEqual(storedValueLen(img.len), v.len);
    var out: [XV_MAX_STEPS]StepRec = undefined;
    const it = try decodeStored(&k, v, &out);
    try testing.expectEqual(Op.unlink, it.op);
    // A value under another id's key refuses (name/key mismatch).
    const other = recordKey(homeIno(3), id + 1);
    try testing.expectError(error.Corrupt, decodeStored(&other, v, &out));
    var m = Minter.init(5);
    try testing.expectEqual(@as(u64, (5 << 40) | 1), m.next());
    m.recover((5 << 40) | 900);
    m.recover((4 << 40) | 5000); // another term: ignored
    try testing.expectEqual(@as(u64, (5 << 40) | 901), m.next());
    try testing.expectEqual(@as(u64, 5), Minter.termOf(m.next()));
}

test "the register: minted ≡ retired + open; in-flight → durable → retired; abandoned entries are listed ascending, reconciled against a scan, and age into `stuck` (the tripwire fires once per transition)" {
    const gpa = testing.allocator;
    var r = try Register.init(gpa, testing.io, 64);
    defer r.deinit(gpa);
    const minted0 = stats.xv_cross_owner_intents_minted.load();
    const retired0 = stats.xv_cross_owner_intents_retired.load();
    const open0 = stats.xv_cross_owner_intents_open.load();
    try r.noteInFlight(10, 0, 1000);
    try testing.expectEqual(IntentState.in_flight, r.stateOf(10).?);
    try testing.expectEqual(minted0, stats.xv_cross_owner_intents_minted.load());
    r.noteDurable(10);
    r.noteDurable(10);
    try testing.expectEqual(minted0 + 1, stats.xv_cross_owner_intents_minted.load());
    try testing.expectEqual(open0 + 1, stats.xv_cross_owner_intents_open.load());
    try r.noteInFlight(12, 0, 1000);
    r.noteDurable(12);
    r.noteAbandoned(12);
    try r.noteInFlight(11, 0, 1000);
    r.noteDurable(11);
    r.noteAbandoned(11);
    try testing.expect(r.anyAbandoned());
    var ids: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 11, 12 }, r.abandoned(&ids));
    // 12 was retired by somebody else: a scan listing only 11 forgets it.
    r.forgetAbandonedAbsent(0, &.{11});
    try testing.expectEqual(@as(?IntentState, null), r.stateOf(12));
    try testing.expectEqual(retired0 + 1, stats.xv_cross_owner_intents_retired.load());
    // An uncounted in-flight entry forgotten counts nothing.
    try r.noteInFlight(13, 0, 1000);
    r.forget(13);
    try testing.expectEqual(retired0 + 1, stats.xv_cross_owner_intents_retired.load());
    // Taken back by the live op.
    r.noteTaken(11);
    try testing.expect(!r.anyAbandoned());
    r.noteAbandoned(11);
    const stuck_before = stats.xv_cross_owner_intent_stuck.load();
    r.publish(1000 + STUCK_GRACE_NS);
    try testing.expectEqual(@as(u64, 0), stats.xv_cross_owner_intents_stuck.load());
    stats.xv_cross_owner_intent_stuck.resetLogBudgetForTest();
    @import("../../../core/log.zig").testing.expectErrorLogs(1);
    r.publish(1001 + STUCK_GRACE_NS);
    try @import("../../../core/log.zig").testing.expectAllFired();
    try testing.expectEqual(@as(u64, 1), stats.xv_cross_owner_intents_stuck.load());
    try testing.expectEqual(stuck_before + 1, stats.xv_cross_owner_intent_stuck.load());
    r.forget(11);
    r.forget(10);
    r.publish(2000 + STUCK_GRACE_NS);
    try testing.expectEqual(@as(u64, 0), stats.xv_cross_owner_intents_stuck.load());
    // Adoption counts once and lands abandoned.
    try r.adopt(20, 0, 5);
    try r.adopt(20, 0, 5);
    try testing.expectEqual(IntentState.abandoned, r.stateOf(20).?);
    r.forget(20);
    const minted = stats.xv_cross_owner_intents_minted.load() - minted0;
    const retired = stats.xv_cross_owner_intents_retired.load() - retired0;
    try testing.expectEqual(minted, retired + (stats.xv_cross_owner_intents_open.load() - open0));
    try testing.expectEqual(@as(usize, 0), r.count());
}
