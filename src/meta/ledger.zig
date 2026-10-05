//! The root ledger (charter §5.1 "root ledger", §5.3 "Checkpoint";
//! `docs/porting/kv_journal_ledger.md` §5, §8, §9).
//!
//! 32 round-robin 4 KiB slots (128 KiB, ONE mount read). A record with
//! checkpoint seq `s` lives in slot `s % 32`; mount picks the newest slot
//! whose checksum verifies, so a torn newest slot (a checkpoint racing power
//! loss) falls back to its predecessor — correct by the pending-free rule and
//! its ring twin (`reusable_upto` advances only post-barrier): nothing either
//! record references has been overwritten.
//!
//! THE DENSE RULE: checkpoint seqs are consumed strictly +1, and every
//! checkpoint-class durable step that consumes a seq OUTSIDE a cycle (ring
//! growth, appender leave, region release) RESTATES the last cycle's record
//! at the consumed seq, so a reader polling the predicted next slot never
//! stops on a gap.
//!
//! Slot image (little-endian; always a FULL zero-padded 4096 B write so a
//! shorter record can never leave a longer predecessor's tail parseable):
//!
//!     [0..4)   magic "KVRL"   [4..8) len = payload_len   [8..16) seq
//!     [16..24) xxh3( image[0..16] ‖ 0⁸ ‖ payload )
//!     payload @24: journal_tail_seq u64 ‖ next_ino u64 ‖ alloc_bitmap_generation u64 ‖
//!                  node_seq_watermark u64 ‖ n_roots u16 ‖ n_roots × {tree_id u8 ‖ node_addr u64 ‖ node_seq u64}
//!                  [membership stamp] [append partition: writer_id u16 ‖ writer_count u16]
//!
//! `node_seq_watermark` is not in the charter's field list but IS on the wire
//! (incompat bit NODE_SEQ_WATERMARK): a mount reseeds the node-seq mint at or
//! above it so node incarnation seqs never repeat within a generation — the
//! invariant the bset `node_seq_at_write == node_seq` admission relies on.
//!
//! PORTABLE: encode yields a slot image, decode consumes one; the io task
//! binds `slotOffset` to `write_at`/`read_at`.

const std = @import("std");
const Allocator = std.mem.Allocator;
const checksum = @import("../core/checksum.zig");
const counters = @import("../core/counters.zig");
const jc = @import("journal_core.zig");
const journal = @import("journal.zig");

pub const AppendPartition = journal.AppendPartition;
const CoreGeometry = jc.CoreGeometry;

// ---------------------------------------------------------------------------
// Constants (`docs/porting/kv_journal_ledger.md` §8).
// ---------------------------------------------------------------------------

pub const ROOT_LEDGER_SLOTS: u64 = 32;
pub const ROOT_LEDGER_SLOT_LEN: u64 = 4096;
/// 128 KiB — the mount-read unit.
pub const ROOT_LEDGER_LEN: u64 = ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN;
/// "KVRL".
pub const ROOT_LEDGER_MAGIC: u32 = 0x4B56_524C;
/// `magic ‖ len ‖ seq ‖ xxh3`.
pub const ROOT_LEDGER_HDR_LEN: usize = 24;
/// `4 × u64 + n_roots u16` (the watermark widened it by 8).
pub const PAYLOAD_FIXED_LEN: usize = 8 + 8 + 8 + 8 + 2;
/// `tree_id u8 ‖ node_addr u64 ‖ node_seq u64`.
pub const ROOT_ENC_LEN: usize = 1 + 8 + 8;
/// `set_uuid ‖ set_epoch ‖ member_position ‖ member_count ‖ routing_width ‖ n_runs`.
pub const STAMP_FIXED_LEN: usize = 16 + 8 + 2 + 2 + 4 + 2;
/// `start u16 ‖ stride u16 ‖ count u32`.
pub const RUN_ENC_LEN: usize = 2 + 2 + 4;
/// `slot u16 ‖ next_local_ino u64`.
pub const CURSOR_ENC_LEN: usize = 2 + 8;
/// The stamp's encoding budget: a volume may host tens of thousands of slots
/// as ONE stride run; what the 4096 B slot bounds is encoding COMPLEXITY.
/// 128 runs + 256 cursors keep the worst-case record at 3,765 B with 19 spare
/// roots; MINT_SPREAD (64) fresh mint cursors are ¼ of the cursor cap.
pub const STAMP_MAX_RUNS: usize = 128;
pub const STAMP_MAX_CURSORS: usize = 256;
pub const APPEND_PARTITION_ENC_LEN: usize = 4;
/// Charter §5.3: a checkpoint cycle at least every second; the reader poll
/// cadence derives from it.
pub const CHECKPOINT_MAX_AGE_MS: u64 = 1000;

/// The worst-case stamped record: header + fixed payload + 5 roots + a full
/// stamp + the partition suffix — must fit a slot. (The reference's comment
/// quotes 3,765: it omits the 2 B `n_cursors` and the 4 B suffix; the wire
/// below carries both.)
pub const WORST_CASE_STAMPED_RECORD_LEN: usize = ROOT_LEDGER_HDR_LEN + PAYLOAD_FIXED_LEN + 5 * ROOT_ENC_LEN + (STAMP_FIXED_LEN + STAMP_MAX_RUNS * RUN_ENC_LEN + 4 + 2 + STAMP_MAX_CURSORS * CURSOR_ENC_LEN) + APPEND_PARTITION_ENC_LEN;

comptime {
    std.debug.assert(ROOT_LEDGER_LEN == 131072);
    std.debug.assert(PAYLOAD_FIXED_LEN == 34);
    std.debug.assert(STAMP_FIXED_LEN == 34);
    // 24 + 34 + 85 + (34 + 1024 + 4 + 2 + 2560) + 4 = 3771 ≤ 4096: the budget closes with 19 spare roots.
    std.debug.assert(WORST_CASE_STAMPED_RECORD_LEN == 3771);
    std.debug.assert(WORST_CASE_STAMPED_RECORD_LEN <= ROOT_LEDGER_SLOT_LEN);
    // The ledger extent the journal's placement assumes is this one.
    std.debug.assert(journal.ROOT_LEDGER_LEN == ROOT_LEDGER_LEN);
}

pub const Error = error{
    /// Structural: the slot holds no valid record (never loud here).
    Corrupt,
    ChecksumMismatch,
    OutOfMemory,
};

// ---------------------------------------------------------------------------
// Record types.
// ---------------------------------------------------------------------------

/// One tree root: `(tree_id, node_addr, node_seq)`.
pub const TreeRoot = struct {
    tree_id: u8,
    node_addr: u64,
    node_seq: u64,
};

/// One arithmetic progression of slot ids `{start + k·stride | 0 ≤ k < count}`.
pub const SlotRun = struct {
    start: u16,
    /// A singleton's stride is canonically 1.
    stride: u16,
    /// ≥ 1.
    count: u32,

    fn last(self: SlotRun) u64 {
        return @as(u64, self.start) + @as(u64, self.count - 1) * @as(u64, self.stride);
    }

    fn contains(self: SlotRun, slot: u16) bool {
        const s: u64 = slot;
        if (s < self.start or s > self.last()) return false;
        if (self.stride == 0) return s == self.start;
        return (s - self.start) % self.stride == 0;
    }
};

/// The hosted-slot set as validated runs, ascending by start.
pub const SlotSet = struct {
    runs: []SlotRun,

    pub const empty: SlotSet = .{ .runs = &.{} };

    /// Validate-before-trust: a zero count, a repeating zero stride, a run
    /// escaping the u16 namespace or any overlap refuses. Takes ownership of
    /// `runs` (sorted in place).
    pub fn fromRuns(runs: []SlotRun) Error!SlotSet {
        var seen: std.StaticBitSet(1 << 16) = .empty;
        for (runs) |r| {
            if (r.count == 0) return error.Corrupt;
            if (r.stride == 0 and r.count > 1) return error.Corrupt;
            if (r.last() > std.math.maxInt(u16)) return error.Corrupt;
            var k: u32 = 0;
            while (k < r.count) : (k += 1) {
                const s: usize = @intCast(@as(u64, r.start) + @as(u64, k) * @as(u64, r.stride));
                if (seen.isSet(s)) return error.Corrupt;
                seen.set(s);
            }
        }
        std.mem.sort(SlotRun, runs, {}, struct {
            fn lt(_: void, a: SlotRun, b: SlotRun) bool {
                return a.start < b.start;
            }
        }.lt);
        return .{ .runs = runs };
    }

    pub fn contains(self: SlotSet, slot: u16) bool {
        for (self.runs) |r| if (r.contains(slot)) return true;
        return false;
    }

    pub fn len(self: SlotSet) usize {
        var n: usize = 0;
        for (self.runs) |r| n += r.count;
        return n;
    }
};

pub const SlotCursor = struct { slot: u16, next_local_ino: u64 };

/// The per-volume set-membership stamp: rides the ledger record because it is
/// the one structure already A/B-rotating, checksummed and rewritable without
/// relocation. `member_position` defines the canonical set order.
pub const MembershipStamp = struct {
    set_uuid: [16]u8,
    set_epoch: u64,
    member_position: u16,
    member_count: u16,
    /// The DERIVED routing width W, stored — never re-derived at mount.
    routing_width: u32,
    slots_hosted: SlotSet,
    /// The slot whose records live in this volume's LEGACY keyspace; null on
    /// fresh members (all their slots are guests). u32 `plus1` on the wire: a
    /// u16 plus1 would overflow at slot 65535.
    native_slot: ?u16,
    /// Per-slot GUEST ino cursors for hosted slots that minted or travelled in.
    slot_cursors: []const SlotCursor,

    pub fn resolvedNativeSlot(self: MembershipStamp) ?u16 {
        return self.native_slot;
    }

    pub fn cursorFor(self: MembershipStamp, slot: u16) ?u64 {
        for (self.slot_cursors) |c| if (c.slot == slot) return c.next_local_ino;
        return null;
    }

    fn encodedLen(self: MembershipStamp) usize {
        return STAMP_FIXED_LEN + self.slots_hosted.runs.len * RUN_ENC_LEN + 4 + 2 + self.slot_cursors.len * CURSOR_ENC_LEN;
    }
};

pub const LedgerRecord = struct {
    /// Checkpoint sequence; strictly monotonic per volume. `slot = seq % 32`.
    seq: u64,
    tree_roots: []const TreeRoot,
    /// Replay starts here (the tail rule's output).
    journal_tail_seq: u64,
    /// Monotonic ino watermark.
    next_ino: u64,
    alloc_bitmap_generation: u64,
    node_seq_watermark: u64,
    /// Present on every slot-mapped volume; null forever on legacy volumes
    /// (byte-identical to the pre-stamp encoding).
    membership_stamp: ?MembershipStamp,
    /// Null on every un-stamped volume: such a record encodes byte-identically
    /// to the pre-partition image and places itself with `seq % 32`.
    append_partition: ?AppendPartition,

    /// Free what `decodeSlot` allocated.
    pub fn deinit(self: *LedgerRecord, gpa: Allocator) void {
        gpa.free(self.tree_roots);
        if (self.membership_stamp) |st| {
            gpa.free(st.slots_hosted.runs);
            gpa.free(st.slot_cursors);
        }
    }

    pub fn slotIndex(self: LedgerRecord) u64 {
        if (self.append_partition) |part| return ledgerSlotFor(self.seq, part);
        return self.seq % ROOT_LEDGER_SLOTS;
    }

    /// The DENSE rule's restatement: this record at `seq` (a checkpoint-class
    /// step that consumed a seq outside a cycle). Shares the record's slices;
    /// encode it before the original goes away.
    pub fn restatement(self: LedgerRecord, seq: u64) LedgerRecord {
        meta_kv_ledger_restatements.inc();
        var r = self;
        r.seq = seq;
        return r;
    }

    /// Encode into a full zero-padded slot image. The encoding-budget checks
    /// are load-bearing: a stamp past `STAMP_MAX_RUNS` / `STAMP_MAX_CURSORS`
    /// refuses LOUD (the caps exist so the worst case fits); the total-size
    /// check stays as defence in depth for the root count.
    pub fn encodeSlot(self: LedgerRecord) Error![ROOT_LEDGER_SLOT_LEN]u8 {
        var stamp_len: usize = 0;
        if (self.membership_stamp) |st| {
            if (st.slots_hosted.runs.len > STAMP_MAX_RUNS) return error.Corrupt;
            if (st.slot_cursors.len > STAMP_MAX_CURSORS) return error.Corrupt;
            stamp_len = st.encodedLen();
        }
        const part_len: usize = if (self.append_partition != null) APPEND_PARTITION_ENC_LEN else 0;
        const payload_len = PAYLOAD_FIXED_LEN + self.tree_roots.len * ROOT_ENC_LEN + stamp_len + part_len;
        if (ROOT_LEDGER_HDR_LEN + payload_len > ROOT_LEDGER_SLOT_LEN or self.tree_roots.len > std.math.maxInt(u16)) return error.Corrupt;
        var image: [ROOT_LEDGER_SLOT_LEN]u8 = @splat(0);
        std.mem.writeInt(u32, image[0..4], ROOT_LEDGER_MAGIC, .little);
        std.mem.writeInt(u32, image[4..8], @intCast(payload_len), .little);
        std.mem.writeInt(u64, image[8..16], self.seq, .little);
        var pos: usize = ROOT_LEDGER_HDR_LEN;
        std.mem.writeInt(u64, image[pos..][0..8], self.journal_tail_seq, .little);
        pos += 8;
        std.mem.writeInt(u64, image[pos..][0..8], self.next_ino, .little);
        pos += 8;
        std.mem.writeInt(u64, image[pos..][0..8], self.alloc_bitmap_generation, .little);
        pos += 8;
        std.mem.writeInt(u64, image[pos..][0..8], self.node_seq_watermark, .little);
        pos += 8;
        std.mem.writeInt(u16, image[pos..][0..2], @intCast(self.tree_roots.len), .little);
        pos += 2;
        for (self.tree_roots) |root| {
            image[pos] = root.tree_id;
            std.mem.writeInt(u64, image[pos + 1 ..][0..8], root.node_addr, .little);
            std.mem.writeInt(u64, image[pos + 9 ..][0..8], root.node_seq, .little);
            pos += ROOT_ENC_LEN;
        }
        if (self.membership_stamp) |st| {
            image[pos..][0..16].* = st.set_uuid;
            pos += 16;
            std.mem.writeInt(u64, image[pos..][0..8], st.set_epoch, .little);
            pos += 8;
            std.mem.writeInt(u16, image[pos..][0..2], st.member_position, .little);
            pos += 2;
            std.mem.writeInt(u16, image[pos..][0..2], st.member_count, .little);
            pos += 2;
            std.mem.writeInt(u32, image[pos..][0..4], st.routing_width, .little);
            pos += 4;
            std.mem.writeInt(u16, image[pos..][0..2], @intCast(st.slots_hosted.runs.len), .little);
            pos += 2;
            for (st.slots_hosted.runs) |run| {
                std.mem.writeInt(u16, image[pos..][0..2], run.start, .little);
                std.mem.writeInt(u16, image[pos + 2 ..][0..2], run.stride, .little);
                std.mem.writeInt(u32, image[pos + 4 ..][0..4], run.count, .little);
                pos += RUN_ENC_LEN;
            }
            const native_plus1: u32 = if (st.native_slot) |s| @as(u32, s) + 1 else 0;
            std.mem.writeInt(u32, image[pos..][0..4], native_plus1, .little);
            pos += 4;
            std.mem.writeInt(u16, image[pos..][0..2], @intCast(st.slot_cursors.len), .little);
            pos += 2;
            for (st.slot_cursors) |c| {
                std.mem.writeInt(u16, image[pos..][0..2], c.slot, .little);
                std.mem.writeInt(u64, image[pos + 2 ..][0..8], c.next_local_ino, .little);
                pos += CURSOR_ENC_LEN;
            }
        }
        // The partition suffix rides AFTER the stamp: the stamp's own length
        // equation closes exactly, so the decoder tells "no suffix" from
        // "one suffix" (exactly four more bytes) without ambiguity.
        if (self.append_partition) |part| {
            std.mem.writeInt(u16, image[pos..][0..2], part.writer_id, .little);
            std.mem.writeInt(u16, image[pos + 2 ..][0..2], part.writers, .little);
            pos += APPEND_PARTITION_ENC_LEN;
        }
        std.debug.assert(pos == ROOT_LEDGER_HDR_LEN + payload_len);
        std.mem.writeInt(u64, image[16..24], slotChecksum(&image, payload_len), .little);
        return image;
    }

    /// Decode + verify one slot image. TOTAL: every length is checked against
    /// its container before any byte it governs is touched; any failure means
    /// "this slot holds no valid record" — the selection treats it as absent,
    /// never loud. The tail after the roots is one of exactly four shapes
    /// decided by LENGTH: nothing; exactly 4 B = a partition suffix; a stamp;
    /// a stamp then the suffix.
    pub fn decodeSlot(gpa: Allocator, buf: []const u8) Error!LedgerRecord {
        if (buf.len < ROOT_LEDGER_HDR_LEN) return error.Corrupt;
        if (std.mem.readInt(u32, buf[0..4], .little) != ROOT_LEDGER_MAGIC) return error.Corrupt;
        const payload_len: usize = std.mem.readInt(u32, buf[4..8], .little);
        if (payload_len < PAYLOAD_FIXED_LEN or ROOT_LEDGER_HDR_LEN + payload_len > buf.len) return error.Corrupt;
        const seq = std.mem.readInt(u64, buf[8..16], .little);
        const stored = std.mem.readInt(u64, buf[16..24], .little);
        if (stored != slotChecksum(buf, payload_len)) return error.ChecksumMismatch;
        const payload = buf[ROOT_LEDGER_HDR_LEN .. ROOT_LEDGER_HDR_LEN + payload_len];
        const journal_tail_seq = std.mem.readInt(u64, payload[0..8], .little);
        const next_ino = std.mem.readInt(u64, payload[8..16], .little);
        const alloc_bitmap_generation = std.mem.readInt(u64, payload[16..24], .little);
        const node_seq_watermark = std.mem.readInt(u64, payload[24..32], .little);
        const n_roots: usize = std.mem.readInt(u16, payload[32..34], .little);
        const roots_end = PAYLOAD_FIXED_LEN + n_roots * ROOT_ENC_LEN;
        if (roots_end > payload_len) return error.Corrupt;
        const roots = try gpa.alloc(TreeRoot, n_roots);
        errdefer gpa.free(roots);
        var pos: usize = PAYLOAD_FIXED_LEN;
        for (roots) |*r| {
            r.* = .{
                .tree_id = payload[pos],
                .node_addr = std.mem.readInt(u64, payload[pos + 1 ..][0..8], .little),
                .node_seq = std.mem.readInt(u64, payload[pos + 9 ..][0..8], .little),
            };
            pos += ROOT_ENC_LEN;
        }
        var rec: LedgerRecord = .{
            .seq = seq,
            .tree_roots = roots,
            .journal_tail_seq = journal_tail_seq,
            .next_ino = next_ino,
            .alloc_bitmap_generation = alloc_bitmap_generation,
            .node_seq_watermark = node_seq_watermark,
            .membership_stamp = null,
            .append_partition = null,
        };
        if (pos == payload_len) return rec;
        if (payload_len - pos == APPEND_PARTITION_ENC_LEN) {
            rec.append_partition = try decodeAppendPartition(payload[pos..]);
            return rec;
        }
        if (pos + STAMP_FIXED_LEN > payload_len) return error.Corrupt;
        var st: MembershipStamp = undefined;
        st.set_uuid = payload[pos..][0..16].*;
        pos += 16;
        st.set_epoch = std.mem.readInt(u64, payload[pos..][0..8], .little);
        pos += 8;
        st.member_position = std.mem.readInt(u16, payload[pos..][0..2], .little);
        pos += 2;
        st.member_count = std.mem.readInt(u16, payload[pos..][0..2], .little);
        pos += 2;
        st.routing_width = std.mem.readInt(u32, payload[pos..][0..4], .little);
        pos += 4;
        const n_runs: usize = std.mem.readInt(u16, payload[pos..][0..2], .little);
        pos += 2;
        if (n_runs > STAMP_MAX_RUNS) return error.Corrupt;
        if (pos + n_runs * RUN_ENC_LEN > payload_len) return error.Corrupt;
        const runs = try gpa.alloc(SlotRun, n_runs);
        errdefer gpa.free(runs);
        for (runs) |*r| {
            r.* = .{
                .start = std.mem.readInt(u16, payload[pos..][0..2], .little),
                .stride = std.mem.readInt(u16, payload[pos + 2 ..][0..2], .little),
                .count = std.mem.readInt(u32, payload[pos + 4 ..][0..4], .little),
            };
            pos += RUN_ENC_LEN;
        }
        st.slots_hosted = try SlotSet.fromRuns(runs);
        if (pos + 4 + 2 > payload_len) return error.Corrupt;
        const native_plus1 = std.mem.readInt(u32, payload[pos..][0..4], .little);
        pos += 4;
        const n_cursors: usize = std.mem.readInt(u16, payload[pos..][0..2], .little);
        pos += 2;
        if (n_cursors > STAMP_MAX_CURSORS) return error.Corrupt;
        const cursors_end = pos + n_cursors * CURSOR_ENC_LEN;
        if (cursors_end != payload_len and cursors_end + APPEND_PARTITION_ENC_LEN != payload_len) return error.Corrupt;
        const cursors = try gpa.alloc(SlotCursor, n_cursors);
        errdefer gpa.free(cursors);
        for (cursors) |*c| {
            c.* = .{
                .slot = std.mem.readInt(u16, payload[pos..][0..2], .little),
                .next_local_ino = std.mem.readInt(u64, payload[pos + 2 ..][0..8], .little),
            };
            pos += CURSOR_ENC_LEN;
        }
        st.slot_cursors = cursors;
        st.native_slot = if (native_plus1 == 0) null else blk: {
            if (native_plus1 - 1 > std.math.maxInt(u16)) return error.Corrupt;
            break :blk @intCast(native_plus1 - 1);
        };
        if (pos < payload_len) rec.append_partition = try decodeAppendPartition(payload[pos..]);
        rec.membership_stamp = st;
        return rec;
    }
};

/// xxh3 over the slot header with the checksum field zeroed, then the payload.
fn slotChecksum(image: []const u8, payload_len: usize) u64 {
    var h = std.hash.XxHash3.init(0);
    h.update(image[0..16]);
    const zeros: [8]u8 = @splat(0);
    h.update(&zeros);
    h.update(image[ROOT_LEDGER_HDR_LEN .. ROOT_LEDGER_HDR_LEN + payload_len]);
    return h.final();
}

/// The 4 B suffix through `AppendPartition.new`: an illegal count or id is a
/// writer-bug screen (the checksum already verified).
fn decodeAppendPartition(tail: []const u8) Error!AppendPartition {
    std.debug.assert(tail.len >= APPEND_PARTITION_ENC_LEN);
    const writer_id = std.mem.readInt(u16, tail[0..2], .little);
    const writers = std.mem.readInt(u16, tail[2..4], .little);
    return AppendPartition.new(writers, writer_id) catch error.Corrupt;
}

// ---------------------------------------------------------------------------
// Placement and selection.
// ---------------------------------------------------------------------------

/// Slots each appender owns under a `writers`-way partition. The floor of 2
/// is normative: newest-valid-wins with a torn newest slot must fall back to
/// a predecessor OF THE SAME APPENDER — `MAX_APPENDERS` derives from this.
pub fn ledgerSlotsPerWriter(writers: u16) u64 {
    return ROOT_LEDGER_SLOTS / writers;
}

/// Solo: `seq % 32` verbatim; partitioned: the appender's contiguous range,
/// round-robin inside it.
pub fn ledgerSlotFor(seq: u64, part: AppendPartition) u64 {
    if (part.isSolo()) return seq % ROOT_LEDGER_SLOTS;
    const per = ledgerSlotsPerWriter(part.writers);
    return @as(u64, part.writer_id) * per + seq % per;
}

/// Device offset of `slot` in a ledger based at `base`.
pub fn slotOffset(base: u64, slot: u64) u64 {
    std.debug.assert(slot < ROOT_LEDGER_SLOTS);
    return base + slot * ROOT_LEDGER_SLOT_LEN;
}

/// Newest-valid-wins over the ledger image: decode every slot that fits (a
/// short extent reads as absent past its end), keep the highest `seq` that
/// verifies; null when none does (fresh volume — the loud policy on a
/// formatted volume is mount wiring: format ALWAYS writes a bootstrap record).
pub fn newestValid(gpa: Allocator, image: []const u8) Allocator.Error!?LedgerRecord {
    var newest: ?LedgerRecord = null;
    var slot: usize = 0;
    while (slot < ROOT_LEDGER_SLOTS) : (slot += 1) {
        const start = slot * @as(usize, ROOT_LEDGER_SLOT_LEN);
        if (start >= image.len) break;
        const end = @min(start + @as(usize, ROOT_LEDGER_SLOT_LEN), image.len);
        var rec = LedgerRecord.decodeSlot(gpa, image[start..end]) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Corrupt, error.ChecksumMismatch => continue,
        };
        if (newest) |*n| {
            if (rec.seq > n.seq) {
                n.deinit(gpa);
                newest = rec;
            } else rec.deinit(gpa);
        } else newest = rec;
    }
    return newest;
}

/// Format's bootstrap record: fresh ring (tail 0), the ino watermark,
/// generation 1 bitmap, the pre-partition form regardless of stamped bits
/// (partition ADOPTION is a first-writable-mount minting act).
pub fn bootstrapRecord(tree_roots: []const TreeRoot, next_ino: u64, node_seq_watermark: u64, stamp: ?MembershipStamp) LedgerRecord {
    return .{
        .seq = 1,
        .tree_roots = tree_roots,
        .journal_tail_seq = 0,
        .next_ino = next_ino,
        .alloc_bitmap_generation = 1,
        .node_seq_watermark = node_seq_watermark,
        .membership_stamp = stamp,
        .append_partition = null,
    };
}

// ---------------------------------------------------------------------------
// The checkpoint tail and the post-barrier reclamation clock (§5.3).
// ---------------------------------------------------------------------------

/// `tail = min(H, inputs…)` where `H` is the head at cycle start and every
/// input is an ENTRY START: the oldest in-flight reservation, the dying
/// floors, the unpublished root floors, the dying leaf floors, every node's
/// dirty floor. Callers pass entry starts (record seqs are `entry_start + i`;
/// a raw per-record floor could land mid-entry and replay's chain walk —
/// which parses AT the tail — would drop the entry's acked records plus
/// collateral entries to the resync point). A lower tail only lengthens the
/// replay window, absorbed idempotently by the per-key LWW replay gate.
pub fn checkpointTail(h: u64, inputs: []const u64) u64 {
    var tail = h;
    for (inputs) |f| tail = @min(tail, f);
    return tail;
}

/// Charter §5.3: a cycle is due when the journal distance exceeds half the
/// ring (`head − reusable_upto > L/2`).
pub fn ringUnderPressure(head: u64, reusable_upto: u64, logical_len: u64) bool {
    return head - reusable_upto > logical_len / 2;
}

/// The post-barrier reclamation queue: a checkpoint pushes `(tail, epoch)`
/// where `epoch` = barriers already STARTED when the record was written —
/// none of which can cover it. After a barrier completes, every entry with
/// `epoch < covered` (covered = the completed barrier's start ticket) is
/// drained in order and its tail applied to ALL THREE watermarks on one
/// clock — `reusable_upto`, the allocator's durable tail, the cache's
/// elision tail — strictly post-barrier, never on the in-RAM tail. Push
/// epochs are non-decreasing, so the covered set is always a prefix.
pub const ReclaimQueue = struct {
    gpa: Allocator,
    lock: jc.SpinLock = .{},
    pending: std.ArrayList(Entry) = .empty,

    pub const Entry = struct { tail: u64, epoch: u64 };

    pub fn init(gpa: Allocator) ReclaimQueue {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *ReclaimQueue) void {
        self.pending.deinit(self.gpa);
    }

    pub fn push(self: *ReclaimQueue, tail: u64, epoch: u64) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        std.debug.assert(self.pending.items.len == 0 or self.pending.items[self.pending.items.len - 1].epoch <= epoch);
        try self.pending.append(self.gpa, .{ .tail = tail, .epoch = epoch });
    }

    /// Pop the oldest entry iff a barrier that started at `covered` or later
    /// flushed it (`epoch < covered`). Loop until null to drain the prefix.
    pub fn popCovered(self: *ReclaimQueue, covered: u64) ?Entry {
        self.lock.lock();
        defer self.lock.unlock();
        if (self.pending.items.len == 0 or self.pending.items[0].epoch >= covered) return null;
        return self.pending.orderedRemove(0);
    }

    pub fn len(self: *ReclaimQueue) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.pending.items.len;
    }
};

/// Drain every covered tail into the ring's watermark (the ring half of the
/// one clock; the allocator and cache halves land with their modules).
/// Answers the last tail applied.
pub fn advanceRingAfterBarrier(queue: *ReclaimQueue, covered: u64, core: *jc.JournalCore) ?u64 {
    var last: ?u64 = null;
    while (queue.popCovered(covered)) |e| {
        core.advanceReusableUpto(e.tail);
        last = e.tail;
    }
    return last;
}

/// DENSE-rule restatements written (`META_KV_LEDGER_RESTATEMENTS`).
pub var meta_kv_ledger_restatements: counters.ShardedCounter = .{};

// ---------------------------------------------------------------------------
// Tests (`docs/porting/kv_journal_ledger.md` §7, as image tests).
// ---------------------------------------------------------------------------

const testing = std.testing;

const roots3 = [_]TreeRoot{
    .{ .tree_id = 1, .node_addr = 0x10000, .node_seq = 11 },
    .{ .tree_id = 2, .node_addr = 0x20000, .node_seq = 22 },
    .{ .tree_id = 7, .node_addr = 0x70000, .node_seq = 77 },
};

fn plainRecord(seq: u64) LedgerRecord {
    return .{
        .seq = seq,
        .tree_roots = &roots3,
        .journal_tail_seq = 4000 * seq,
        .next_ino = 1000 + seq,
        .alloc_bitmap_generation = seq,
        .node_seq_watermark = 500 + seq,
        .membership_stamp = null,
        .append_partition = null,
    };
}

fn expectRecordEq(want: LedgerRecord, got: LedgerRecord) !void {
    try testing.expectEqual(want.seq, got.seq);
    try testing.expectEqualSlices(TreeRoot, want.tree_roots, got.tree_roots);
    try testing.expectEqual(want.journal_tail_seq, got.journal_tail_seq);
    try testing.expectEqual(want.next_ino, got.next_ino);
    try testing.expectEqual(want.alloc_bitmap_generation, got.alloc_bitmap_generation);
    try testing.expectEqual(want.node_seq_watermark, got.node_seq_watermark);
    try testing.expectEqual(want.append_partition, got.append_partition);
    try testing.expectEqual(want.membership_stamp == null, got.membership_stamp == null);
    if (want.membership_stamp) |w| {
        const g = got.membership_stamp.?;
        try testing.expectEqualSlices(u8, &w.set_uuid, &g.set_uuid);
        try testing.expectEqual(w.set_epoch, g.set_epoch);
        try testing.expectEqual(w.member_position, g.member_position);
        try testing.expectEqual(w.member_count, g.member_count);
        try testing.expectEqual(w.routing_width, g.routing_width);
        try testing.expectEqualSlices(SlotRun, w.slots_hosted.runs, g.slots_hosted.runs);
        try testing.expectEqual(w.native_slot, g.native_slot);
        try testing.expectEqualSlices(SlotCursor, w.slot_cursors, g.slot_cursors);
    }
}

test "slot byte layout: magic KVRL, len, seq, checksum = xxh3(hdr[0..16] ‖ 0⁸ ‖ payload); payload field offsets; zero-padded to 4096" {
    const rec = plainRecord(5);
    const img = try rec.encodeSlot();
    try testing.expectEqualSlices(u8, &.{ 0x4C, 0x52, 0x56, 0x4B }, img[0..4]);
    const payload_len = PAYLOAD_FIXED_LEN + 3 * ROOT_ENC_LEN;
    try testing.expectEqual(@as(u32, payload_len), std.mem.readInt(u32, img[4..8], .little));
    try testing.expectEqual(@as(u64, 5), std.mem.readInt(u64, img[8..16], .little));
    var concat: [16 + 8 + payload_len]u8 = undefined;
    @memcpy(concat[0..16], img[0..16]);
    @memset(concat[16..24], 0);
    @memcpy(concat[24..], img[24 .. 24 + payload_len]);
    try testing.expectEqual(checksum.xxh3(&concat), std.mem.readInt(u64, img[16..24], .little));
    try testing.expectEqual(@as(u64, 20000), std.mem.readInt(u64, img[24..32], .little));
    try testing.expectEqual(@as(u64, 1005), std.mem.readInt(u64, img[32..40], .little));
    try testing.expectEqual(@as(u64, 5), std.mem.readInt(u64, img[40..48], .little));
    try testing.expectEqual(@as(u64, 505), std.mem.readInt(u64, img[48..56], .little));
    try testing.expectEqual(@as(u16, 3), std.mem.readInt(u16, img[56..58], .little));
    try testing.expectEqual(@as(u8, 1), img[58]);
    try testing.expectEqual(@as(u64, 0x10000), std.mem.readInt(u64, img[59..67], .little));
    try testing.expectEqual(@as(u64, 11), std.mem.readInt(u64, img[67..75], .little));
    for (img[24 + payload_len ..]) |b| try testing.expectEqual(@as(u8, 0), b);
}

test "slot encode/decode round trip; a flipped byte, [..50], [..10] and zeros are errors" {
    const rec = plainRecord(9);
    const img = try rec.encodeSlot();
    var got = try LedgerRecord.decodeSlot(testing.allocator, &img);
    defer got.deinit(testing.allocator);
    try expectRecordEq(rec, got);
    var bad = img;
    bad[30] ^= 0x01;
    try testing.expectError(error.ChecksumMismatch, LedgerRecord.decodeSlot(testing.allocator, &bad));
    bad = img;
    bad[17] ^= 0x01;
    try testing.expectError(error.ChecksumMismatch, LedgerRecord.decodeSlot(testing.allocator, &bad));
    try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, img[0..50]));
    try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, img[0..10]));
    const zeros: [4096]u8 = @splat(0);
    try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &zeros));
    // A lying length is refused before any byte it governs is read.
    bad = img;
    std.mem.writeInt(u32, bad[4..8], 5000, .little);
    try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &bad));
    std.mem.writeInt(u32, bad[4..8], 33, .little);
    try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &bad));
    // n_roots inconsistent with the payload length (restamped so only the
    // length rule can reject it).
    bad = img;
    std.mem.writeInt(u16, bad[56..58], 4, .little);
    const plen: usize = std.mem.readInt(u32, bad[4..8], .little);
    std.mem.writeInt(u64, bad[16..24], slotChecksum(&bad, plen), .little);
    try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &bad));
}

test "the four tail shapes by length: nothing / partition suffix / stamp / stamp + suffix" {
    var runs = [_]SlotRun{ .{ .start = 100, .stride = 7, .count = 3 }, .{ .start = 5, .stride = 1, .count = 1 } };
    const cursors = [_]SlotCursor{ .{ .slot = 5, .next_local_ino = 42 }, .{ .slot = 114, .next_local_ino = 7 } };
    const stamp: MembershipStamp = .{
        .set_uuid = .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 },
        .set_epoch = 3,
        .member_position = 1,
        .member_count = 4,
        .routing_width = 65536,
        .slots_hosted = try SlotSet.fromRuns(&runs),
        .native_slot = 5,
        .slot_cursors = &cursors,
    };
    // fromRuns sorted the runs ascending.
    try testing.expectEqual(@as(u16, 5), runs[0].start);
    try testing.expect(stamp.slots_hosted.contains(114));
    try testing.expect(!stamp.slots_hosted.contains(101));
    try testing.expectEqual(@as(usize, 4), stamp.slots_hosted.len());
    try testing.expectEqual(@as(?u64, 42), stamp.cursorFor(5));
    try testing.expectEqual(@as(?u64, null), stamp.cursorFor(6));
    const part = try AppendPartition.new(4, 2);
    var base = plainRecord(12);
    // Shape 2: suffix only.
    base.append_partition = part;
    {
        const img = try base.encodeSlot();
        try testing.expectEqual(@as(u32, PAYLOAD_FIXED_LEN + 3 * ROOT_ENC_LEN + 4), std.mem.readInt(u32, img[4..8], .little));
        var got = try LedgerRecord.decodeSlot(testing.allocator, &img);
        defer got.deinit(testing.allocator);
        try expectRecordEq(base, got);
        try testing.expectEqual(@as(u64, 2 * 8 + 12 % 8), got.slotIndex());
    }
    // Shape 3: stamp only.
    base.append_partition = null;
    base.membership_stamp = stamp;
    {
        const img = try base.encodeSlot();
        const expect_len = PAYLOAD_FIXED_LEN + 3 * ROOT_ENC_LEN + STAMP_FIXED_LEN + 2 * RUN_ENC_LEN + 4 + 2 + 2 * CURSOR_ENC_LEN;
        try testing.expectEqual(@as(u32, expect_len), std.mem.readInt(u32, img[4..8], .little));
        var got = try LedgerRecord.decodeSlot(testing.allocator, &img);
        defer got.deinit(testing.allocator);
        try expectRecordEq(base, got);
        try testing.expectEqual(@as(u64, 12), got.slotIndex());
    }
    // Shape 4: stamp then suffix.
    base.append_partition = part;
    {
        const img = try base.encodeSlot();
        var got = try LedgerRecord.decodeSlot(testing.allocator, &img);
        defer got.deinit(testing.allocator);
        try expectRecordEq(base, got);
    }
    // No native slot encodes plus1 = 0 and round-trips as null.
    var no_native = stamp;
    no_native.native_slot = null;
    base.membership_stamp = no_native;
    base.append_partition = null;
    {
        const img = try base.encodeSlot();
        var got = try LedgerRecord.decodeSlot(testing.allocator, &img);
        defer got.deinit(testing.allocator);
        try testing.expectEqual(@as(?u16, null), got.membership_stamp.?.resolvedNativeSlot());
    }
}

test "decode refusals: illegal partition, n_runs over the cap, run geometry, truncated stamp tail, cursors not closing, native slot past u16" {
    var base = plainRecord(1);
    // Illegal partition (writers 3) in a suffix-only tail.
    {
        base.append_partition = try AppendPartition.new(4, 1);
        var img = try base.encodeSlot();
        const plen: usize = std.mem.readInt(u32, img[4..8], .little);
        std.mem.writeInt(u16, img[24 + plen - 2 ..][0..2], 3, .little);
        std.mem.writeInt(u64, img[16..24], slotChecksum(&img, plen), .little);
        try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &img));
        base.append_partition = null;
    }
    var runs = [_]SlotRun{.{ .start = 1, .stride = 1, .count = 2 }};
    const stamp: MembershipStamp = .{
        .set_uuid = @splat(9),
        .set_epoch = 1,
        .member_position = 0,
        .member_count = 1,
        .routing_width = 65536,
        .slots_hosted = try SlotSet.fromRuns(&runs),
        .native_slot = null,
        .slot_cursors = &.{},
    };
    base.membership_stamp = stamp;
    const stamp_at = ROOT_LEDGER_HDR_LEN + PAYLOAD_FIXED_LEN + 3 * ROOT_ENC_LEN;
    const n_runs_at = stamp_at + 16 + 8 + 2 + 2 + 4;
    // n_runs over the cap.
    {
        var img = try base.encodeSlot();
        const plen: usize = std.mem.readInt(u32, img[4..8], .little);
        std.mem.writeInt(u16, img[n_runs_at..][0..2], STAMP_MAX_RUNS + 1, .little);
        std.mem.writeInt(u64, img[16..24], slotChecksum(&img, plen), .little);
        try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &img));
        // n_runs under the cap but not fitting the payload.
        std.mem.writeInt(u16, img[n_runs_at..][0..2], 100, .little);
        std.mem.writeInt(u64, img[16..24], slotChecksum(&img, plen), .little);
        try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &img));
    }
    // Run geometry: count 0; overlapping runs; stride 0 repeating; namespace escape.
    {
        var img = try base.encodeSlot();
        const plen: usize = std.mem.readInt(u32, img[4..8], .little);
        std.mem.writeInt(u32, img[n_runs_at + 2 + 4 ..][0..4], 0, .little);
        std.mem.writeInt(u64, img[16..24], slotChecksum(&img, plen), .little);
        try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &img));
        var r_overlap = [_]SlotRun{ .{ .start = 1, .stride = 2, .count = 3 }, .{ .start = 3, .stride = 1, .count = 1 } };
        try testing.expectError(error.Corrupt, SlotSet.fromRuns(&r_overlap));
        var r_stride0 = [_]SlotRun{.{ .start = 1, .stride = 0, .count = 2 }};
        try testing.expectError(error.Corrupt, SlotSet.fromRuns(&r_stride0));
        var r_escape = [_]SlotRun{.{ .start = 65535, .stride = 1, .count = 2 }};
        try testing.expectError(error.Corrupt, SlotSet.fromRuns(&r_escape));
        var r_single0 = [_]SlotRun{.{ .start = 9, .stride = 0, .count = 1 }};
        const ok = try SlotSet.fromRuns(&r_single0);
        try testing.expect(ok.contains(9));
    }
    // A stamp tail shorter than the fixed prefix (neither empty nor 4 B).
    {
        var img = try base.encodeSlot();
        const plen: usize = PAYLOAD_FIXED_LEN + 3 * ROOT_ENC_LEN + 10;
        std.mem.writeInt(u32, img[4..8], @intCast(plen), .little);
        std.mem.writeInt(u64, img[16..24], slotChecksum(&img, plen), .little);
        try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &img));
    }
    // Cursors that do not close to the payload end (nor leave exactly 4 B).
    {
        var img = try base.encodeSlot();
        const plen: usize = std.mem.readInt(u32, img[4..8], .little);
        std.mem.writeInt(u32, img[4..8], @intCast(plen + 3), .little);
        std.mem.writeInt(u64, img[16..24], slotChecksum(&img, plen + 3), .little);
        try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &img));
        // n_cursors over the cap.
        std.mem.writeInt(u32, img[4..8], @intCast(plen), .little);
        const n_cursors_at = n_runs_at + 2 + RUN_ENC_LEN + 4;
        std.mem.writeInt(u16, img[n_cursors_at..][0..2], STAMP_MAX_CURSORS + 1, .little);
        std.mem.writeInt(u64, img[16..24], slotChecksum(&img, plen), .little);
        try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &img));
    }
    // native_plus1 − 1 past the u16 namespace.
    {
        var img = try base.encodeSlot();
        const plen: usize = std.mem.readInt(u32, img[4..8], .little);
        const native_at = n_runs_at + 2 + RUN_ENC_LEN;
        std.mem.writeInt(u32, img[native_at..][0..4], 65537, .little);
        std.mem.writeInt(u64, img[16..24], slotChecksum(&img, plen), .little);
        try testing.expectError(error.Corrupt, LedgerRecord.decodeSlot(testing.allocator, &img));
    }
    // Encode refusals: runs past the cap; a record that cannot fit a slot.
    {
        var too_many: [STAMP_MAX_RUNS + 1]SlotRun = undefined;
        for (&too_many, 0..) |*r, i| r.* = .{ .start = @intCast(i * 2), .stride = 1, .count = 1 };
        var st = stamp;
        st.slots_hosted = try SlotSet.fromRuns(&too_many);
        base.membership_stamp = st;
        try testing.expectError(error.Corrupt, base.encodeSlot());
        base.membership_stamp = null;
        var many_roots: [240]TreeRoot = undefined;
        for (&many_roots, 0..) |*r, i| r.* = .{ .tree_id = @intCast(i % 9 + 1), .node_addr = i, .node_seq = i };
        base.tree_roots = &many_roots;
        try testing.expectError(error.Corrupt, base.encodeSlot());
        base.tree_roots = many_roots[0..237];
        _ = try base.encodeSlot(); // 24 + 34 + 237 × 17 = 4087 fits
    }
}

test "placement: solo seq % 32; partitioned writer range; slots per writer; offsets" {
    try testing.expectEqual(@as(u64, 8), plainRecord(40).slotIndex());
    try testing.expectEqual(@as(u64, 8), plainRecord(8).slotIndex());
    try testing.expectEqual(@as(u64, 31), plainRecord(31).slotIndex());
    try testing.expectEqual(@as(u64, 7), ledgerSlotFor(7, AppendPartition.solo));
    try testing.expectEqual(@as(u64, 2), ledgerSlotsPerWriter(16));
    try testing.expectEqual(@as(u64, 8), ledgerSlotsPerWriter(4));
    const p = try AppendPartition.new(16, 15);
    try testing.expectEqual(@as(u64, 30), ledgerSlotFor(100, p));
    try testing.expectEqual(@as(u64, 31), ledgerSlotFor(101, p));
    try testing.expectEqual(@as(u64, 4096 + 8 * 4096), slotOffset(4096, 8));
}

test "newest-valid-wins over the 128 KiB image; a torn newest slot falls back to its predecessor; seq 40 overwrites slot 8; empty ⇒ null; short extent ⇒ rest absent" {
    const gpa = testing.allocator;
    const image = try gpa.alloc(u8, @intCast(ROOT_LEDGER_LEN));
    defer gpa.free(image);
    @memset(image, 0);
    try testing.expect((try newestValid(gpa, image)) == null);
    for (1..6) |s| {
        const img = try plainRecord(s).encodeSlot();
        const at: usize = @intCast(slotOffset(0, plainRecord(s).slotIndex()));
        @memcpy(image[at .. at + 4096], &img);
    }
    {
        var got = (try newestValid(gpa, image)).?;
        defer got.deinit(gpa);
        try testing.expectEqual(@as(u64, 5), got.seq);
    }
    // Tear slot 5 (seq 5): only its first 20 bytes landed, the rest is the
    // prior garbage ⇒ seq 4 is selected; smash slot 4 ⇒ seq 3.
    @memset(image[5 * 4096 + 20 .. 6 * 4096], 0x5A);
    {
        var got = (try newestValid(gpa, image)).?;
        defer got.deinit(gpa);
        try testing.expectEqual(@as(u64, 4), got.seq);
    }
    @memset(image[4 * 4096 + 40 .. 4 * 4096 + 48], 0x5A);
    {
        var got = (try newestValid(gpa, image)).?;
        defer got.deinit(gpa);
        try testing.expectEqual(@as(u64, 3), got.seq);
        try testing.expectEqual(@as(u64, 12000), got.journal_tail_seq);
    }
    // Round robin: seq 8 then seq 40 both land in slot 8; selection is by seq.
    const r8 = try plainRecord(8).encodeSlot();
    @memcpy(image[8 * 4096 .. 9 * 4096], &r8);
    {
        var got = (try newestValid(gpa, image)).?;
        defer got.deinit(gpa);
        try testing.expectEqual(@as(u64, 8), got.seq);
    }
    const r40 = try plainRecord(40).encodeSlot();
    @memcpy(image[8 * 4096 .. 9 * 4096], &r40);
    {
        var got = (try newestValid(gpa, image)).?;
        defer got.deinit(gpa);
        try testing.expectEqual(@as(u64, 40), got.seq);
    }
    // A short extent: only the first two slots are present ⇒ seq 1 (slot 1).
    {
        var got = (try newestValid(gpa, image[0 .. 2 * 4096])).?;
        defer got.deinit(gpa);
        try testing.expectEqual(@as(u64, 1), got.seq);
    }
    // A partial trailing slot decodes if its record fits the bytes present.
    {
        var got = (try newestValid(gpa, image[0 .. 4096 + 200])).?;
        defer got.deinit(gpa);
        try testing.expectEqual(@as(u64, 1), got.seq);
    }
}

test "bootstrap record: seq 1, tail 0, generation 1, no partition; encodes and decodes" {
    const rec = bootstrapRecord(&roots3, 2, 17, null);
    try testing.expectEqual(@as(u64, 1), rec.seq);
    try testing.expectEqual(@as(u64, 0), rec.journal_tail_seq);
    try testing.expectEqual(@as(u64, 1), rec.alloc_bitmap_generation);
    try testing.expectEqual(@as(?AppendPartition, null), rec.append_partition);
    try testing.expectEqual(@as(u64, 1), rec.slotIndex());
    const img = try rec.encodeSlot();
    var got = try LedgerRecord.decodeSlot(testing.allocator, &img);
    defer got.deinit(testing.allocator);
    try expectRecordEq(rec, got);
}

test "the DENSE rule: a restatement is the same record at the consumed seq, counted" {
    const before = meta_kv_ledger_restatements.load();
    const rec = plainRecord(20);
    const restated = rec.restatement(21);
    try testing.expectEqual(@as(u64, 21), restated.seq);
    try testing.expectEqual(@as(u64, 21), restated.slotIndex());
    try testing.expectEqual(rec.journal_tail_seq, restated.journal_tail_seq);
    try testing.expectEqual(rec.node_seq_watermark, restated.node_seq_watermark);
    try testing.expectEqualSlices(TreeRoot, rec.tree_roots, restated.tree_roots);
    try testing.expectEqual(before + 1, meta_kv_ledger_restatements.load());
}

test "tail rule: min over H and the entry-start inputs; ring pressure at L/2" {
    try testing.expectEqual(@as(u64, 100), checkpointTail(100, &.{}));
    try testing.expectEqual(@as(u64, 40), checkpointTail(100, &.{ std.math.maxInt(u64), 40, 90 }));
    try testing.expectEqual(@as(u64, 100), checkpointTail(100, &.{ std.math.maxInt(u64), 150 }));
    try testing.expect(!ringUnderPressure(100, 0, 200));
    try testing.expect(ringUnderPressure(101, 0, 200));
    try testing.expect(!ringUnderPressure(300, 200, 200));
}

test "pending_reclaim: a tail opens reusable_upto only once a barrier that started after its push completes" {
    const geo: CoreGeometry = .{ .page_data_len = 100, .pages = 4, .reserve_bytes = 0, .grain = 1 };
    var core = jc.JournalCore.init(geo, 300, 0);
    var q = ReclaimQueue.init(testing.allocator);
    defer q.deinit();
    // Checkpoint 1 wrote tail 100 while barrier #3 had started; checkpoint 2
    // wrote tail 250 while #5 had started.
    try q.push(100, 3);
    try q.push(250, 5);
    // Barrier #3 completes: it had started BEFORE the push ⇒ covers nothing.
    try testing.expectEqual(@as(?u64, null), advanceRingAfterBarrier(&q, 3, &core));
    try testing.expectEqual(@as(u64, 0), core.reusableUpto());
    // Barrier #4 completes: covers the first push only.
    try testing.expectEqual(@as(?u64, 100), advanceRingAfterBarrier(&q, 4, &core));
    try testing.expectEqual(@as(u64, 100), core.reusableUpto());
    try testing.expectEqual(@as(usize, 1), q.len());
    // Barrier #6: covers the second.
    try testing.expectEqual(@as(?u64, 250), advanceRingAfterBarrier(&q, 6, &core));
    try testing.expectEqual(@as(u64, 250), core.reusableUpto());
    try testing.expectEqual(@as(usize, 0), q.len());
    // Admission bounds against the watermark, never the in-RAM tail.
    try testing.expect(core.tryAdmit(350, .user) != null);
    try testing.expectEqual(@as(?jc.Admission, null), core.tryAdmit(351, .user));
}
