//! The read view — a per-ino cached projection of what one data-plane op
//! needs of an inode's metadata (the inode record's size; the layout
//! record's kind, file id, durable copy, kvmap head and its whole inline
//! block map or inline payload), read with acquire loads and no RMW, so a
//! striped file's 4 KiB random read resolves its size and its block's
//! binding without a KV lookup (`docs/evidence/rung-10-read-view.md`: the
//! three lookups were 2.4 of a read's 6.4 µs). PORTABLE.
//!
//! Exactness rests on two generations the filler captures BEFORE its
//! lookups and every hit compares AFTER its loads: the ino's mutation
//! witness (`meta/ino_witness.zig`, bumped by the node cache after every
//! record of the ino lands or leaves) and the mount's slot-move epoch (a
//! slot this mount read locally may become another appender's). A hit
//! whose either generation moved is a miss; the slow path refills. Within
//! a slot a seqlock orders the fill against its readers: the fill takes
//! the seq odd, stores, takes it even; a reader loads the seq, the words,
//! the seq again — every word an ACQUIRE load, which is what orders the
//! second seq load after them (an acquire load orders what follows it;
//! Boehm's seqlock paper) — and a changed or odd seq is a torn read, a
//! miss. The data words are only ever `Atomic(u64)`: no plain load races
//! a fill's store. `read_view_model.zig` explores the three parties.
//!
//! Replacement is 4-way set-associative FIFO by fill time with a refill
//! floor (`REFILL_MIN_NS`): a set thrashed by five hot files is left to
//! the KV path at most once per floor, never a 64 KiB copy per read. A
//! fill is ONE per generation: a way already holding the ino as of the
//! filler's generations is left alone, so the ops that missed on a slot
//! mid-fill never queue fills of their own behind it.
//! Memory is fixed at init — slots × (two header lines + the record cap)
//! — so a reader's borrowed words never dangle, and the pool is a fixed
//! budget component (`read_view_bytes`).

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const budget = @import("../mem/budget.zig");
const lazy = @import("../mem/lazy.zig");
const jc = @import("../meta/journal_core.zig");
const dlm = @import("../meta/dlm.zig");
const ino_witness = @import("../meta/ino_witness.zig");
const layout = @import("layout.zig");

pub const stats = struct {
    /// `read_view_hits`: ops resolved off the view (no KV lookup).
    pub var read_view_hits: counters.ShardedCounter = .{};
    /// `read_view_misses`: no slot held the ino (the slow path fills).
    pub var read_view_misses: counters.ShardedCounter = .{};
    /// `read_view_stale`: a slot held the ino under an older witness
    /// generation or slot-move epoch (a mutation since the fill).
    pub var read_view_stale: counters.ShardedCounter = .{};
    /// `read_view_torn`: a slot was mid-fill while read (seq odd/moved).
    pub var read_view_torn: counters.ShardedCounter = .{};
    /// `read_view_fills`: slots (re)filled off the KV path.
    pub var read_view_fills: counters.ShardedCounter = .{};
    /// `read_view_fill_refused`: a fill declined — the set's ways all
    /// filled within the refill floor, a record the head decode refused,
    /// or one wider than the slot.
    pub var read_view_fill_refused: counters.ShardedCounter = .{};
    /// `read_view_fill_redundant`: a fill skipped because the way already
    /// holds the ino as of the filler's generations (a concurrent filler,
    /// or a torn read that was never stale).
    pub var read_view_fill_redundant: counters.ShardedCounter = .{};
    /// `read_view_bytes`: the fixed pool (slots × slot bytes).
    pub var read_view_bytes: counters.Gauge = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"read_view_hits\":{d},\"read_view_misses\":{d},\"read_view_stale\":{d},\"read_view_torn\":{d},\"read_view_fills\":{d},\"read_view_fill_refused\":{d},\"read_view_fill_redundant\":{d},\"read_view_bytes\":{d}}}", .{
        stats.read_view_hits.load(),
        stats.read_view_misses.load(),
        stats.read_view_stale.load(),
        stats.read_view_torn.load(),
        stats.read_view_fills.load(),
        stats.read_view_fill_refused.load(),
        stats.read_view_fill_redundant.load(),
        stats.read_view_bytes.load(),
    });
}

/// 64 sets × 4 ways = 256 slots: a set thrashes only with five hot files
/// hashed into one of 64 sets (P ≈ 10⁻⁴ at 32 hot files), and at the
/// 64 KiB record cap the pool is 16.4 MiB — the price of a borrowed word
/// that never dangles.
pub const SETS: usize = 64;
pub const WAYS: usize = 4;
pub const SLOTS: usize = SETS * WAYS;
/// A way is replaced for ANOTHER ino only once its fill is this old: a
/// set the hot files overflow costs at most one 64 KiB fill per floor,
/// not one per read (the KV path serves it meanwhile). One millisecond:
/// the device leg's order, a thousandth of a read's worth of fills.
pub const REFILL_MIN_NS: u64 = std.time.ns_per_ms;

/// Header words of a slot (two cache lines).
const W_SEQ: usize = 0;
const W_INO: usize = 1;
const W_GEN: usize = 2;
const W_MOVES: usize = 3;
const W_SIZE: usize = 4;
const W_HEAD: usize = 5;
const W_KVGEN: usize = 6;
const W_N: usize = 7;
const W_FID: usize = 8; // 2 words
const W_DC: usize = 10; // 5 words (MappingWire)
/// The inode record's `flags` word (`record.INODE_FLAG_*`): the write
/// law's IMMUTABLE/APPEND bits, current under the witness generation like
/// the size — the shared-mode admission and the staged-point fill read
/// them here, so a restricted inode never reaches an arm that reads no
/// record (`plane.sharedProbe`, `plane.stagedPoint`).
const W_FLAGS: usize = 15;
const HEADER_WORDS: usize = 16;
pub const HEADER_BYTES: usize = HEADER_WORDS * WORD;

/// `W_HEAD`: bits 0–7 the layout kind + 1 (0 = no layout record: an inline
/// file never written), bit 8 a durable copy, bit 9 a kvmap head, bit 10 a
/// sweep cursor, bit 11 a map generation; bits 32–63 the sweep cursor.
const HEAD_KIND_MASK: u64 = 0xFF;
const HEAD_DURABLE_COPY: u64 = 1 << 8;
const HEAD_KVMAP: u64 = 1 << 9;
const HEAD_SWEEP: u64 = 1 << 10;
const HEAD_MAP_GEN: u64 = 1 << 11;
const HEAD_SWEEP_SHIFT: u6 = 32;

/// The slot's word: every datum is stored and loaded as one `u64` (the
/// acquire loads that order the seq recheck), bytes packed little-endian.
const WORD: usize = @sizeOf(u64);

/// One inline-map entry as the slot stores it: `MapEntryWire`'s 48 bytes
/// as six little-endian words (block in word 0's low half, the reserved
/// bytes in its high half).
const ENTRY_WORDS: usize = layout.MAP_ENTRY_WIRE_LEN / WORD;
const ENTRY_RESERVED_SHIFT: u6 = @bitSizeOf(u8) * @offsetOf(layout.MapEntryWire, "reserved_le");
const MAPPING_WORDS: usize = layout.MAPPING_WIRE_LEN / WORD;

comptime {
    std.debug.assert(ENTRY_WORDS * WORD == layout.MAP_ENTRY_WIRE_LEN);
    std.debug.assert(MAPPING_WORDS * WORD == layout.MAPPING_WIRE_LEN);
    std.debug.assert(W_DC + MAPPING_WORDS <= HEADER_WORDS);
    std.debug.assert(@offsetOf(layout.MapEntryWire, "mapping") == WORD);
    std.debug.assert(ENTRY_RESERVED_SHIFT == 32);
}

/// The body words a slot needs for the widest record of `record_cap`
/// bytes: an inline map's entries or an inline payload, both ≤ the cap.
pub fn bodyWords(record_cap: usize) usize {
    return (record_cap + WORD - 1) / WORD;
}

pub fn slotBytes(record_cap: usize) usize {
    return HEADER_BYTES + bodyWords(record_cap) * WORD;
}

/// The inode record's facts a fill installs beside the layout.
pub const InodeFacts = struct {
    size: u64,
    /// `record.InodeValue.flags`.
    flags: u32 = 0,
};

/// What a hit resolved.
pub const Found = struct {
    /// The inode record's size (the caller folds the parked claim).
    size: u64,
    /// The inode record's flags (`record.INODE_FLAG_*`).
    flags: u32,
    /// The layout as `layout.resolvePoint` reads it; null = no record.
    point: ?layout.Point,
};

const Slot = struct {
    w: [HEADER_WORDS]Atomic(u64) align(std.atomic.cache_line),
    body: []Atomic(u64),
    /// Writer-side (under the set lock): the fill's instant.
    filled_ns: u64,
};

const Set = struct {
    lock: jc.SpinLock = .{},
    ways: [WAYS]Slot,
};

pub const Table = struct {
    gpa: Allocator,
    sets: []Set,
    /// Every slot's body, one allocation (`body_words` per slot).
    bodies: []Atomic(u64),
    body_words: usize,
    geo: layout.Geometry,
    /// The mount's slot-move epoch (`sym/slots/moves.Moves.epoch`); null
    /// = a flat mount, nothing moves.
    moves: ?*const Atomic(u64) = null,

    pub fn init(gpa: Allocator, record_cap: usize, geo: layout.Geometry) Allocator.Error!Table {
        const words = bodyWords(record_cap);
        const sets = try gpa.alloc(Set, SETS);
        errdefer gpa.free(sets);
        // Lazily materialized (`mem/lazy.zig`): a body is resident once a
        // fill wrote it — the table was 16 MiB of an idle daemon, written
        // at birth by the allocator's `undefined` fill.
        const bodies = try lazy.alloc(Atomic(u64), words * SLOTS);
        errdefer lazy.free(Atomic(u64), bodies);
        for (sets, 0..) |*set, si| {
            set.* = .{ .ways = undefined };
            for (&set.ways, 0..) |*s, wi| {
                s.* = .{ .w = undefined, .body = bodies[(si * WAYS + wi) * words ..][0..words], .filled_ns = 0 };
                for (&s.w) |*word| word.* = .init(0);
            }
        }
        stats.read_view_bytes.set(SLOTS * slotBytes(record_cap));
        return .{ .gpa = gpa, .sets = sets, .bodies = bodies, .body_words = words, .geo = geo };
    }

    pub fn deinit(self: *Table) void {
        lazy.free(Atomic(u64), self.bodies);
        self.gpa.free(self.sets);
        stats.read_view_bytes.set(0);
        self.* = undefined;
    }

    /// The fixed pool as the budget sees it (charter §6.2 R5): bytes as
    /// floor and gauge, weight 0, no shed — a reader's borrowed words
    /// cannot be returned under it.
    pub fn component(_: *Table) budget.Component {
        return budget.Component.init("read_view", stats.read_view_bytes.load(), 0, .{ .gauge = &stats.read_view_bytes }, null);
    }

    pub fn bindMoves(self: *Table, epoch: ?*const Atomic(u64)) void {
        self.moves = epoch;
    }

    /// The epoch a fill records and a hit compares.
    pub fn movesNow(self: *const Table) u64 {
        const m = self.moves orelse return 0;
        return m.load(.acquire);
    }

    fn setOf(self: *Table, ino: u64) *Set {
        return &self.sets[@intCast(dlm.splitmix64(ino) & (SETS - 1))];
    }

    // ---- the hit path ----

    /// `ino`'s facts and, for a striped inline map, `block`'s binding —
    /// current as of the return, or null (a miss, a stale slot, a torn
    /// read, an entry the window law refuses: the KV path answers).
    pub fn lookup(self: *Table, ino: u64, block: ?u32) ?Found {
        const set = self.setOf(ino);
        for (&set.ways) |*s| {
            const s0 = s.w[W_SEQ].load(.acquire);
            if (s.w[W_INO].load(.acquire) != ino) continue;
            // Mid-fill (seq odd): torn, not a miss — the fill in flight
            // carries this ino current, so the caller's own fill is skipped.
            if (s0 & 1 != 0) {
                stats.read_view_torn.inc();
                return null;
            }
            const gen = s.w[W_GEN].load(.acquire);
            const moves = s.w[W_MOVES].load(.acquire);
            const size = s.w[W_SIZE].load(.acquire);
            const head = s.w[W_HEAD].load(.acquire);
            const kvgen = s.w[W_KVGEN].load(.acquire);
            const n = s.w[W_N].load(.acquire);
            const flags = s.w[W_FLAGS].load(.acquire);
            var fid: [2]u64 = undefined;
            for (&fid, 0..) |*f, i| f.* = s.w[W_FID + i].load(.acquire);
            var dc: [MAPPING_WORDS]u64 = undefined;
            for (&dc, 0..) |*d, i| d.* = s.w[W_DC + i].load(.acquire);
            const kind_tag: u8 = @truncate(head & HEAD_KIND_MASK);
            var entry: ?[ENTRY_WORDS]u64 = null;
            if (block) |b| if (kind_tag == @as(u8, @backingInt(layout.Kind.striped)) + 1 and head & HEAD_KVMAP == 0) {
                entry = self.findEntry(s, @min(n, self.body_words / ENTRY_WORDS), b);
            };
            if (s.w[W_SEQ].load(.acquire) != s0) {
                stats.read_view_torn.inc();
                return null;
            }
            if (ino_witness.genOf(ino) != gen or self.movesNow() != moves) {
                stats.read_view_stale.inc();
                return null;
            }
            const found = decodeFound(self.geo, size, @truncate(flags), head, kvgen, fid, dc, entry) orelse return null;
            stats.read_view_hits.inc();
            return found;
        }
        stats.read_view_misses.inc();
        return null;
    }

    /// The inline payload of `ino` from `offset` into `out`: bytes copied,
    /// or null when the view cannot answer (a miss, stale, torn, not an
    /// inline file). Validated like `lookup`.
    pub fn inlineData(self: *Table, ino: u64, offset: u64, out: []u8) ?usize {
        const set = self.setOf(ino);
        for (&set.ways) |*s| {
            const s0 = s.w[W_SEQ].load(.acquire);
            if (s.w[W_INO].load(.acquire) != ino) continue;
            if (s0 & 1 != 0) {
                stats.read_view_torn.inc();
                return null;
            }
            const gen = s.w[W_GEN].load(.acquire);
            const moves = s.w[W_MOVES].load(.acquire);
            const head = s.w[W_HEAD].load(.acquire);
            const len = @min(s.w[W_N].load(.acquire), self.body_words * WORD);
            const kind_tag: u8 = @truncate(head & HEAD_KIND_MASK);
            if (kind_tag != @as(u8, @backingInt(layout.Kind.inline_)) + 1) return null;
            var n: usize = 0;
            if (offset < len) {
                n = @intCast(@min(out.len, len - offset));
                var i: usize = 0;
                while (i < n) {
                    const at: usize = @intCast(offset + i);
                    const word = s.body[at / WORD].load(.acquire);
                    const bytes = std.mem.toBytes(std.mem.nativeToLittle(u64, word));
                    const from = at % WORD;
                    const take = @min(WORD - from, n - i);
                    @memcpy(out[i..][0..take], bytes[from..][0..take]);
                    i += take;
                }
            }
            if (s.w[W_SEQ].load(.acquire) != s0) {
                stats.read_view_torn.inc();
                return null;
            }
            if (ino_witness.genOf(ino) != gen or self.movesNow() != moves) {
                stats.read_view_stale.inc();
                return null;
            }
            stats.read_view_hits.inc();
            return n;
        }
        stats.read_view_misses.inc();
        return null;
    }

    /// Binary search of the inline map's `n` entries for `block`: the six
    /// words of the entry, or null (a hole). Every load acquire; a fill in
    /// flight may make the words garbage, which the seq recheck discards.
    fn findEntry(_: *Table, s: *const Slot, n: u64, block: u32) ?[ENTRY_WORDS]u64 {
        var lo: usize = 0;
        var hi: usize = @intCast(n);
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            const b: u32 = @truncate(s.body[mid * ENTRY_WORDS].load(.acquire));
            if (b < block) lo = mid + 1 else hi = mid;
        }
        if (lo >= n) return null;
        var e: [ENTRY_WORDS]u64 = undefined;
        for (&e, 0..) |*w, i| w.* = s.body[lo * ENTRY_WORDS + i].load(.acquire);
        if (@as(u32, @truncate(e[0])) != block) return null;
        return e;
    }

    fn decodeFound(geo: layout.Geometry, size: u64, flags: u32, head: u64, kvgen: u64, fid: [2]u64, dc: [MAPPING_WORDS]u64, entry: ?[ENTRY_WORDS]u64) ?Found {
        const kind_tag: u8 = @truncate(head & HEAD_KIND_MASK);
        if (kind_tag == 0) return .{ .size = size, .flags = flags, .point = null };
        const kind = layout.Kind.fromByte(kind_tag - 1) catch return null;
        var file_id: layout.FileId = undefined;
        std.mem.writeInt(u64, file_id[0..8], fid[0], .little);
        std.mem.writeInt(u64, file_id[8..16], fid[1], .little);
        const durable_copy: ?layout.Mapping = if (head & HEAD_DURABLE_COPY != 0) (mappingOfWords(dc, geo.chunk) catch return null) else null;
        const kvmap: ?layout.Layout.KvmapHead = if (head & HEAD_KVMAP != 0) .{
            .sweep = if (head & HEAD_SWEEP != 0) @as(u32, @truncate(head >> HEAD_SWEEP_SHIFT)) else null,
            .gen = if (head & HEAD_MAP_GEN != 0) kvgen else null,
        } else null;
        var binding: ?layout.Mapping = null;
        if (entry) |e| {
            if (e[0] >> ENTRY_RESERVED_SHIFT != 0) return null;
            var m: [MAPPING_WORDS]u64 = undefined;
            @memcpy(&m, e[1..]);
            binding = mappingOfWords(m, geo.chunk) catch return null;
        }
        return .{ .size = size, .flags = flags, .point = .{ .kind = kind, .size = size, .file_id = file_id, .durable_copy = durable_copy, .kvmap = kvmap, .binding = binding } };
    }

    fn mappingOfWords(words: [MAPPING_WORDS]u64, chunk: u64) layout.LayoutError!layout.Mapping {
        var bytes: [layout.MAPPING_WIRE_LEN]u8 = undefined;
        for (words, 0..) |w, i| std.mem.writeInt(u64, bytes[i * 8 ..][0..8], w, .little);
        return layout.Mapping.decode(std.mem.bytesToValue(layout.MappingWire, &bytes), chunk);
    }

    // ---- the fill path (the KV path's exit) ----

    /// Install `ino`'s facts read under witness generation `gen` and
    /// slot-move epoch `moves` (both captured BEFORE the lookups that
    /// produced `facts` and `record`). `record` is the layout record's
    /// bytes (null = none). A record the head decode refuses is not
    /// cached (the KV path reports it per op).
    pub fn fill(self: *Table, ino: u64, facts: InodeFacts, record: ?[]const u8, gen: u64, moves: u64) void {
        var head: u64 = 0;
        var kvgen: u64 = 0;
        var n: u64 = 0;
        var fid: [2]u64 = .{ 0, 0 };
        var dc: [MAPPING_WORDS]u64 = @splat(0);
        var body_src: []const u8 = &.{};
        if (record) |bytes| {
            const l = layout.decodeHead(bytes, self.geo) catch {
                stats.read_view_fill_refused.inc();
                return;
            };
            head = @as(u64, @backingInt(l.kind)) + 1;
            fid[0] = std.mem.readInt(u64, l.file_id[0..8], .little);
            fid[1] = std.mem.readInt(u64, l.file_id[8..16], .little);
            if (l.durableCopy()) |m| {
                head |= HEAD_DURABLE_COPY;
                const wire = std.mem.toBytes(m.encode());
                for (&dc, 0..) |*d, i| d.* = std.mem.readInt(u64, wire[i * 8 ..][0..8], .little);
            }
            if (l.kvmapHead()) |k| {
                head |= HEAD_KVMAP;
                if (k.sweep) |s| head |= HEAD_SWEEP | (@as(u64, s) << HEAD_SWEEP_SHIFT);
                if (k.gen) |g| {
                    head |= HEAD_MAP_GEN;
                    kvgen = g;
                }
            }
            switch (l.kind) {
                .inline_ => {
                    n = l.data.len;
                    body_src = l.data;
                },
                .striped => if (l.kvmapHead() == null) {
                    n = l.entries.len;
                    body_src = std.mem.sliceAsBytes(l.entries);
                },
                .staged, .packed_ => {},
            }
        }
        if (body_src.len > self.body_words * WORD) {
            stats.read_view_fill_refused.inc();
            return;
        }
        const set = self.setOf(ino);
        set.lock.lock();
        defer set.lock.unlock();
        const s = self.victim(set, ino) orelse {
            stats.read_view_fill_refused.inc();
            return;
        };
        // One fill per generation: a way already holding `ino` as of this
        // filler's generations (or newer — a concurrent filler got there
        // first) is left alone. Without this, every op that read the slot
        // mid-fill (a miss) queued a fill of its own, and 32 writers on one
        // file kept the set lock busy with 36 KiB copies for good
        // (rw4k-q32j1: 707 k fills for 3 M writes, p99 54 → 2,100 µs).
        if (s.w[W_INO].load(.monotonic) == ino and s.w[W_GEN].load(.monotonic) >= gen and s.w[W_MOVES].load(.monotonic) >= moves) {
            stats.read_view_fill_redundant.inc();
            return;
        }
        // The seq odd (acq_rel: no store below is hoisted above it), the
        // words, the seq even (release: every store is before it).
        _ = s.w[W_SEQ].fetchAdd(1, .acq_rel);
        s.w[W_INO].store(ino, .monotonic);
        s.w[W_GEN].store(gen, .monotonic);
        s.w[W_MOVES].store(moves, .monotonic);
        s.w[W_SIZE].store(facts.size, .monotonic);
        s.w[W_FLAGS].store(facts.flags, .monotonic);
        s.w[W_HEAD].store(head, .monotonic);
        s.w[W_KVGEN].store(kvgen, .monotonic);
        s.w[W_N].store(n, .monotonic);
        for (fid, 0..) |f, i| s.w[W_FID + i].store(f, .monotonic);
        for (dc, 0..) |d, i| s.w[W_DC + i].store(d, .monotonic);
        var i: usize = 0;
        var wi: usize = 0;
        while (i < body_src.len) : (wi += 1) {
            var word: [WORD]u8 = @splat(0);
            const take = @min(WORD, body_src.len - i);
            @memcpy(word[0..take], body_src[i..][0..take]);
            s.body[wi].store(std.mem.readInt(u64, &word, .little), .monotonic);
            i += take;
        }
        _ = s.w[W_SEQ].fetchAdd(1, .release);
        s.filled_ns = clock.monotonicNs();
        stats.read_view_fills.inc();
    }

    /// Under the set lock: the way holding `ino`, else an empty way, else
    /// the oldest fill past the refill floor; null = every way is a hot
    /// file filled within the floor.
    fn victim(_: *Table, set: *Set, ino: u64) ?*Slot {
        var empty: ?*Slot = null;
        var oldest: ?*Slot = null;
        for (&set.ways) |*s| {
            const held = s.w[W_INO].load(.monotonic);
            if (held == ino) return s;
            if (held == 0) {
                if (empty == null) empty = s;
                continue;
            }
            if (oldest == null or s.filled_ns < oldest.?.filled_ns) oldest = s;
        }
        if (empty) |e| return e;
        const o = oldest.?;
        if (clock.monotonicNs() -| o.filled_ns < REFILL_MIN_NS) return null;
        return o;
    }

    /// `ino`'s slot emptied (the inode destroyed): the way is free at once.
    pub fn forget(self: *Table, ino: u64) void {
        const set = self.setOf(ino);
        set.lock.lock();
        defer set.lock.unlock();
        for (&set.ways) |*s| if (s.w[W_INO].load(.monotonic) == ino) {
            _ = s.w[W_SEQ].fetchAdd(1, .acq_rel);
            s.w[W_INO].store(0, .monotonic);
            _ = s.w[W_SEQ].fetchAdd(1, .release);
            s.filled_ns = 0;
        };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const test_geo: layout.Geometry = .{ .chunk = layout.DEFAULT_CHUNK, .inline_ceiling = 60 * 1024 };

fn stripedRecord(gpa: Allocator, size: u64, blocks: u32, vol_tag: u64) ![]u8 {
    const entries = try gpa.alloc(layout.MapEntryWire, blocks);
    defer gpa.free(entries);
    for (entries, 0..) |*e, b| e.* = (layout.MapEntry{ .block = @intCast(b), .mapping = layout.Mapping.wholeBlock(vol_tag, (b + 1) * layout.DEFAULT_CHUNK, 7) }).encode();
    return layout.Layout.stripedInline(size, entries).encodeAlloc(gpa);
}

test "derivations: 64 sets × 4 ways; a slot is two header lines + the record cap (16.4 MiB at the 64 KiB cap, 4.1 MiB at the 64 KiB node's 16 KiB)" {
    try testing.expectEqual(@as(usize, 256), SLOTS);
    try testing.expectEqual(@as(usize, 128), HEADER_BYTES);
    try testing.expectEqual(@as(usize, 128 + 65_536), slotBytes(65_536));
    try testing.expectEqual(@as(usize, 256 * (128 + 65_536)), SLOTS * slotBytes(65_536));
    try testing.expectEqual(@as(usize, 256 * (128 + 16_384)), SLOTS * slotBytes(16_384));
    // The widest inline map the cap admits fits the body in words.
    const entries_max = (65_536 - layout.LAYOUT_HEADER_LEN - 4) / layout.MAP_ENTRY_WIRE_LEN;
    try testing.expect(entries_max * ENTRY_WORDS <= bodyWords(65_536));
}

test "a filled ino hits with its size, kind and every block's binding (a hole past the map); another ino misses; the witness moving stales it until a refill under the new generation; the slot-move epoch likewise; forget empties the way" {
    var t = try Table.init(testing.allocator, 65_536, test_geo);
    defer t.deinit();
    try testing.expectEqual(@as(u64, 256 * slotBytes(65_536)), stats.read_view_bytes.load());
    const ino: u64 = 4242;
    const rec = try stripedRecord(testing.allocator, 3 * layout.DEFAULT_CHUNK, 768, 11);
    defer testing.allocator.free(rec);
    const hits0 = stats.read_view_hits.load();
    const misses0 = stats.read_view_misses.load();
    const stale0 = stats.read_view_stale.load();
    try testing.expect(t.lookup(ino, 0) == null);
    try testing.expectEqual(misses0 + 1, stats.read_view_misses.load());
    const g = ino_witness.genOf(ino);
    t.fill(ino, .{ .size = 3 * layout.DEFAULT_CHUNK }, rec, g, t.movesNow());
    // Every block resolves off the view; a hole past the map is null.
    var b: u32 = 0;
    while (b < 768) : (b += 7) {
        const f = t.lookup(ino, b) orelse return error.TestExpectedEqual;
        try testing.expectEqual(@as(u64, 3 * layout.DEFAULT_CHUNK), f.size);
        try testing.expectEqual(layout.Kind.striped, f.point.?.kind);
        try testing.expectEqual(@as(u64, (b + 1) * layout.DEFAULT_CHUNK), f.point.?.binding.?.offset);
        try testing.expectEqual(@as(u64, 11), f.point.?.binding.?.vol_tag);
    }
    const hole = t.lookup(ino, 768) orelse return error.TestExpectedEqual;
    try testing.expect(hole.point.?.binding == null);
    const head_only = t.lookup(ino, null) orelse return error.TestExpectedEqual;
    try testing.expect(head_only.point.?.binding == null and head_only.point.?.kvmap == null);
    try testing.expect(t.lookup(ino + 1, 0) == null);
    const hits1 = stats.read_view_hits.load();
    try testing.expect(hits1 > hits0);
    // A record of the ino landed: the view is stale (one load tells).
    ino_witness.touch(ino);
    try testing.expect(t.lookup(ino, 3) == null);
    try testing.expectEqual(stale0 + 1, stats.read_view_stale.load());
    try testing.expectEqual(hits1, stats.read_view_hits.load());
    // The refill under the new generation hits again, in the SAME way.
    const rec2 = try stripedRecord(testing.allocator, 4 * layout.DEFAULT_CHUNK, 768, 12);
    defer testing.allocator.free(rec2);
    // A fill under the generation the way already holds is redundant
    // (the ops that missed mid-fill never queue fills of their own).
    const redundant0 = stats.read_view_fill_redundant.load();
    const fills0 = stats.read_view_fills.load();
    t.fill(ino, .{ .size = 4 * layout.DEFAULT_CHUNK }, rec2, g, t.movesNow());
    try testing.expectEqual(redundant0 + 1, stats.read_view_fill_redundant.load());
    try testing.expectEqual(fills0, stats.read_view_fills.load());
    t.fill(ino, .{ .size = 4 * layout.DEFAULT_CHUNK }, rec2, ino_witness.genOf(ino), t.movesNow());
    try testing.expectEqual(fills0 + 1, stats.read_view_fills.load());
    const f2 = t.lookup(ino, 3) orelse return error.TestExpectedEqual;
    try testing.expectEqual(@as(u64, 12), f2.point.?.binding.?.vol_tag);
    try testing.expectEqual(@as(u64, 4 * layout.DEFAULT_CHUNK), f2.size);
    // The slot-move epoch: a lease transition stales every view.
    var epoch: Atomic(u64) = .init(0);
    t.bindMoves(&epoch);
    try testing.expect(t.lookup(ino, 3) != null);
    _ = epoch.fetchAdd(1, .release);
    try testing.expect(t.lookup(ino, 3) == null);
    t.fill(ino, .{ .size = 4 * layout.DEFAULT_CHUNK }, rec2, ino_witness.genOf(ino), t.movesNow());
    try testing.expect(t.lookup(ino, 3) != null);
    // No record: an inline file of zeros (size from the inode record).
    t.fill(ino + 5, .{ .size = 17 }, null, ino_witness.genOf(ino + 5), t.movesNow());
    const none = t.lookup(ino + 5, 0) orelse return error.TestExpectedEqual;
    try testing.expectEqual(@as(u64, 17), none.size);
    try testing.expect(none.point == null);
    try testing.expectEqual(@as(u32, 0), none.flags);
    // The inode record's flags ride the row, with or without a layout, and
    // a flag change (a record of the ino landed) stales it like the size.
    t.fill(ino + 6, .{ .size = 9, .flags = 0b110 }, null, ino_witness.genOf(ino + 6), t.movesNow());
    try testing.expectEqual(@as(u32, 0b110), (t.lookup(ino + 6, null) orelse return error.TestExpectedEqual).flags);
    t.fill(ino + 7, .{ .size = 4 * layout.DEFAULT_CHUNK, .flags = 0b10 }, rec2, ino_witness.genOf(ino + 7), t.movesNow());
    try testing.expectEqual(@as(u32, 0b10), (t.lookup(ino + 7, 3) orelse return error.TestExpectedEqual).flags);
    ino_witness.touch(ino + 7);
    try testing.expect(t.lookup(ino + 7, 3) == null);
    t.forget(ino);
    try testing.expect(t.lookup(ino, 3) == null);
    try testing.expect(t.lookup(ino + 5, 0) != null);
}

test "the kinds: a staged record carries its file id and durable copy, a packed one its tenant, a kvmap head its sweep cursor and generation (no binding: tree 7's); an inline record serves its payload validated; a record the head decode refuses is not cached" {
    var t = try Table.init(testing.allocator, 65_536, test_geo);
    defer t.deinit();
    const fid: layout.FileId = "0123456789abcdef".*;
    const copy = layout.Mapping.wholeBlock(3, 5 * layout.DEFAULT_CHUNK, 9);
    const staged = try layout.Layout.staged(1000, fid, copy).encodeAlloc(testing.allocator);
    defer testing.allocator.free(staged);
    t.fill(10, .{ .size = 1000 }, staged, ino_witness.genOf(10), 0);
    const s = (t.lookup(10, 0) orelse return error.TestExpectedEqual).point.?;
    try testing.expectEqual(layout.Kind.staged, s.kind);
    try testing.expectEqualSlices(u8, &fid, &s.file_id);
    try testing.expectEqual(copy.offset, s.durable_copy.?.offset);
    try testing.expect(s.binding == null);
    const tenant = layout.Mapping.tenant(3, 5 * layout.DEFAULT_CHUNK, 9, 8192, 1000);
    const packed_rec = try layout.Layout.packedTenant(1000, fid, tenant).encodeAlloc(testing.allocator);
    defer testing.allocator.free(packed_rec);
    t.fill(11, .{ .size = 1000 }, packed_rec, ino_witness.genOf(11), 0);
    const p = (t.lookup(11, 0) orelse return error.TestExpectedEqual).point.?;
    try testing.expectEqual(layout.Kind.packed_, p.kind);
    try testing.expectEqual(@as(u32, 8192), p.durable_copy.?.rel_off);
    try testing.expectEqual(@as(u32, 1000), p.durable_copy.?.len);
    const kv = try layout.Layout.stripedKvmap(1 << 40, 77, 5).encodeAlloc(testing.allocator);
    defer testing.allocator.free(kv);
    t.fill(12, .{ .size = 1 << 40 }, kv, ino_witness.genOf(12), 0);
    const k = (t.lookup(12, 3) orelse return error.TestExpectedEqual).point.?;
    try testing.expectEqual(layout.Kind.striped, k.kind);
    try testing.expectEqual(@as(?u32, 77), k.kvmap.?.sweep);
    try testing.expectEqual(@as(?u64, 5), k.kvmap.?.gen);
    try testing.expect(k.binding == null);
    const kv2 = try layout.Layout.stripedKvmap(1 << 40, null, null).encodeAlloc(testing.allocator);
    defer testing.allocator.free(kv2);
    t.fill(13, .{ .size = 1 << 40 }, kv2, ino_witness.genOf(13), 0);
    const k2 = (t.lookup(13, null) orelse return error.TestExpectedEqual).point.?;
    try testing.expect(k2.kvmap.?.sweep == null and k2.kvmap.?.gen == null);
    // Inline: the payload at any offset, clamped at the length.
    var payload: [1000]u8 = undefined;
    for (&payload, 0..) |*c, i| c.* = @truncate(i *% 31);
    const inl = try layout.Layout.inlineData(1000, &payload).encodeAlloc(testing.allocator);
    defer testing.allocator.free(inl);
    t.fill(14, .{ .size = 1000 }, inl, ino_witness.genOf(14), 0);
    try testing.expectEqual(layout.Kind.inline_, (t.lookup(14, null) orelse return error.TestExpectedEqual).point.?.kind);
    var out: [4096]u8 = undefined;
    try testing.expectEqual(@as(?usize, 1000), t.inlineData(14, 0, &out));
    try testing.expectEqualSlices(u8, &payload, out[0..1000]);
    try testing.expectEqual(@as(?usize, 7), t.inlineData(14, 993, &out));
    try testing.expectEqualSlices(u8, payload[993..], out[0..7]);
    try testing.expectEqual(@as(?usize, 0), t.inlineData(14, 1000, &out));
    try testing.expectEqual(@as(?usize, 100), t.inlineData(14, 3, out[0..100]));
    try testing.expectEqualSlices(u8, payload[3..103], out[0..100]);
    // Not inline: the view declines the payload ask; a miss declines too.
    try testing.expect(t.inlineData(10, 0, &out) == null);
    try testing.expect(t.inlineData(99, 0, &out) == null);
    // A corrupt record: refused, counted, the ino still misses.
    const refused0 = stats.read_view_fill_refused.load();
    t.fill(15, .{ .size = 5 }, &[_]u8{ 9, 9, 9 }, ino_witness.genOf(15), 0);
    try testing.expectEqual(refused0 + 1, stats.read_view_fill_refused.load());
    try testing.expect(t.lookup(15, 0) == null);
}

test "replacement: the four ways of a set hold four inos; a fifth within the refill floor is refused (counted) and the four stay; past the floor it replaces the oldest fill; a refill of a held ino never waits for the floor" {
    var t = try Table.init(testing.allocator, 16_384, test_geo);
    defer t.deinit();
    // Five inos of ONE set (probe the hash).
    var inos: [5]u64 = undefined;
    var found: usize = 0;
    var cand: u64 = 1;
    const target = dlm.splitmix64(1) & (SETS - 1);
    while (found < 5) : (cand += 1) {
        if (dlm.splitmix64(cand) & (SETS - 1) == target) {
            inos[found] = cand;
            found += 1;
        }
    }
    const rec = try stripedRecord(testing.allocator, 4 * layout.DEFAULT_CHUNK, 4, 1);
    defer testing.allocator.free(rec);
    for (inos[0..4]) |ino| t.fill(ino, .{ .size = 4 * layout.DEFAULT_CHUNK }, rec, ino_witness.genOf(ino), 0);
    for (inos[0..4]) |ino| try testing.expect(t.lookup(ino, 1) != null);
    const refused0 = stats.read_view_fill_refused.load();
    t.fill(inos[4], .{ .size = 4 * layout.DEFAULT_CHUNK }, rec, ino_witness.genOf(inos[4]), 0);
    try testing.expectEqual(refused0 + 1, stats.read_view_fill_refused.load());
    try testing.expect(t.lookup(inos[4], 1) == null);
    for (inos[0..4]) |ino| try testing.expect(t.lookup(ino, 1) != null);
    // A held ino refills at once (its own way) under a newer generation,
    // whatever the floor.
    const fills0 = stats.read_view_fills.load();
    ino_witness.touch(inos[2]);
    t.fill(inos[2], .{ .size = 5 * layout.DEFAULT_CHUNK }, rec, ino_witness.genOf(inos[2]), 0);
    try testing.expectEqual(fills0 + 1, stats.read_view_fills.load());
    try testing.expectEqual(@as(u64, 5 * layout.DEFAULT_CHUNK), (t.lookup(inos[2], 1) orelse return error.TestExpectedEqual).size);
    // Age the oldest way past the floor: the fifth takes it, the others stay.
    const set = t.setOf(inos[0]);
    var oldest: *Slot = &set.ways[0];
    for (&set.ways) |*s| if (s.filled_ns < oldest.filled_ns) {
        oldest = s;
    };
    const evicted = oldest.w[W_INO].load(.monotonic);
    oldest.filled_ns -= REFILL_MIN_NS;
    t.fill(inos[4], .{ .size = 4 * layout.DEFAULT_CHUNK }, rec, ino_witness.genOf(inos[4]), 0);
    try testing.expect(t.lookup(inos[4], 1) != null);
    try testing.expect(t.lookup(evicted, 1) == null);
    var kept: usize = 0;
    for (inos[0..4]) |ino| if (t.lookup(ino, 1) != null) {
        kept += 1;
    };
    try testing.expectEqual(@as(usize, 3), kept);
}

test "the seqlock under a racing filler: readers never see a binding of one record under the size of another — every hit is one fill's words, every torn or stale read a miss" {
    var t = try Table.init(testing.allocator, 65_536, test_geo);
    defer t.deinit();
    const ino: u64 = 777;
    const a = try stripedRecord(testing.allocator, 1, 768, 1);
    defer testing.allocator.free(a);
    const b = try stripedRecord(testing.allocator, 2, 768, 2);
    defer testing.allocator.free(b);
    t.fill(ino, .{ .size = 1 }, a, ino_witness.genOf(ino), 0);
    const Race = struct {
        var stop: Atomic(bool) = .init(false);
        var fills: u64 = 0;
        fn filler(tbl: *Table, ra: []const u8, rb: []const u8) void {
            var i: u64 = 0;
            while (!stop.load(.acquire)) : (i += 1) {
                const odd = i & 1 != 0;
                // A record changed: the witness moves, then the fill (one
                // per generation); a yield leaves the readers their window.
                ino_witness.touch(ino);
                tbl.fill(ino, .{ .size = if (odd) 2 else 1 }, if (odd) rb else ra, ino_witness.genOf(ino), 0);
                fills += 1;
                std.Thread.yield() catch {};
            }
        }
        fn reader(tbl: *Table, served: *u64, mismatched: *u64) void {
            var i: u32 = 0;
            while (!stop.load(.acquire)) : (i +%= 1) {
                const f = tbl.lookup(ino, i % 768) orelse continue;
                served.* += 1;
                if (f.point.?.binding.?.vol_tag != f.size) mismatched.* += 1;
            }
        }
    };
    var served: [4]u64 = @splat(0);
    var mismatched: [4]u64 = @splat(0);
    const w = try std.Thread.spawn(.{}, Race.filler, .{ &t, a, b });
    var readers: [4]std.Thread = undefined;
    for (&readers, 0..) |*r, i| r.* = try std.Thread.spawn(.{}, Race.reader, .{ &t, &served[i], &mismatched[i] });
    const t0 = clock.monotonicNs();
    while (clock.monotonicNs() - t0 < 200 * std.time.ns_per_ms) std.Thread.yield() catch {};
    Race.stop.store(true, .release);
    w.join();
    for (readers) |r| r.join();
    var total: u64 = 0;
    for (served, mismatched) |s, m| {
        total += s;
        try testing.expectEqual(@as(u64, 0), m);
    }
    try testing.expect(total > 0);
    try testing.expect(Race.fills > 0);
}
