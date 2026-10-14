//! Tree 0's control records (charter §5.5 "The forest": `slot_state:{s}`,
//! `slot_tails:{s}`, `extent_grant:{a}:{chunk}`, `appender_hint:{identity}`,
//! `node_seq_incarnations`; on VOLUME 0 only `alloc_lease:{vol}`,
//! `dead_member:{id}`, `recovered:{id, vol}`, `dir_rename`,
//! `custody_quarantine:{slot}`; `docs/porting/sym_appenders_manager.md` §1).
//! PORTABLE: canonical, total, sized codecs — the ONE codec every reader
//! and writer of tree 0 uses (`routing/slot_state.zig` re-exports the
//! `slot_state` half).
//!
//! Keys are BINARY (decision 0002 row 230; spec Q-1): `kind 8 ‖ tag u8 ‖
//! id fields BIG-endian`, so one prefix range walk yields index order
//! (`slot_state` of slot 10 sorts after slot 2 — the decimal spelling did
//! not) and no key is longer than 16 B. The charter's `slot_state:{s}`
//! spelling is the LOG's. The kind byte in front keeps every control key
//! below the ASCII-keyed records tree 0 also holds (`set_config`).
//!
//! Values are `extern struct`s of `[N]u8` little-endian fields with a
//! `version u16 ‖ flags u16` head, ONE fixed length per record (spec Q-2:
//! the reference's two `slot_state` lengths, `dead_member`'s three
//! decode-by-length shapes and `slot_tails`' `0xFFFF` sentinel count are
//! all flags or counts here), every reserved word checked zero, every
//! count checked against the byte length before an element is read.
//! Cross-process timestamps are `realtime_ns` (Q-3: a quarantine deadline
//! is compared on another host, so the clock must be realtime; stated on
//! the field).

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const key = @import("../key.zig");
const node_seq = @import("../node_seq.zig");
const checksum = @import("../../core/checksum.zig");

const assert = std.debug.assert;

pub const DecodeError = error{Corrupt};

// ---------------------------------------------------------------------------
// Keys.
// ---------------------------------------------------------------------------

/// The record tag — byte 1 of every control key. Explicit values: the
/// wire never carries a declaration index (0002 row 53's law).
pub const Tag = enum(u8) {
    slot_state = 1,
    slot_tails = 2,
    extent_grant = 3,
    appender_hint = 4,
    custody_quarantine = 5,
    dir_rename = 6,
    alloc_lease = 7,
    dead_member = 8,
    recovered = 9,
    node_seq_incarnations = 10,
    /// A membership row (`WriterClaimV1.kind = 1`) on the HOME volume's
    /// tree 0 (decision 0003 §E): keyed by the member's stable identity.
    member = 11,
    /// A writer's published listener endpoint (charter §5.5 "endpoints
    /// published into each writer's claim-set entry"): keyed by appender
    /// id, written by the manager at its arm and by `PublishEndpoint`;
    /// what a joiner reads off the device to find volume 0's manager
    /// BEFORE it holds any wire.
    endpoint = 12,
    // @anchor:ops_jobs_tags — the job fabric's records (decision 0003 §G;
    // `ops/jobs/records.zig` owns the values): on VOLUME 0 beside
    // `set_config` and `job:enroll`, keyed so one prefix walk lists every
    // job in id order.
    /// `JobRecordV1`: keyed by `job_id u64`.
    job = 13,
    /// `JobShardV1`: keyed by `job_id u64 ‖ shard u32`.
    job_shard = 14,

    /// The id bytes behind the two-byte prefix.
    pub fn idLen(self: Tag) usize {
        return switch (self) {
            .slot_state, .slot_tails, .custody_quarantine => 2,
            .extent_grant => @sizeOf(u32) + @sizeOf(u64),
            .endpoint => 4,
            .appender_hint, .dead_member, .member, .job_shard => 12,
            .recovered => 14,
            .alloc_lease, .job => 8,
            .dir_rename, .node_seq_incarnations => 0,
        };
    }

    pub fn keyLen(self: Tag) usize {
        return KEY_PREFIX_LEN + self.idLen();
    }
};

/// `kind ‖ tag`.
pub const KEY_PREFIX_LEN: usize = 2;
/// The longest control key (`recovered`: node_token ‖ mount_slot ‖ vol).
pub const KEY_MAX_LEN: usize = KEY_PREFIX_LEN + 14;

/// A fixed-capacity key with its length.
pub const ControlKey = struct {
    buf: [KEY_MAX_LEN]u8 = @splat(0),
    len: u8 = 0,

    pub fn slice(self: *const ControlKey) []const u8 {
        return self.buf[0..self.len];
    }

    fn prefix(tag: Tag) ControlKey {
        var k: ControlKey = .{};
        k.buf[0] = key.TREE_CONTROL;
        k.buf[1] = @backingInt(tag);
        k.len = KEY_PREFIX_LEN;
        return k;
    }

    fn putBe(self: *ControlKey, comptime T: type, v: T) void {
        std.mem.writeInt(T, self.buf[self.len..][0..@sizeOf(T)], v, .big);
        self.len += @sizeOf(T);
    }
};

/// The inclusive range every record of `tag` sorts inside: `prefix ‖ 0…`
/// to `prefix ‖ 0xFF…` at the tag's exact key length (the walk's bounds).
pub const KeyRange = struct {
    start: ControlKey,
    end: ControlKey,
};

pub fn rangeOf(tag: Tag) KeyRange {
    var r: KeyRange = .{ .start = ControlKey.prefix(tag), .end = ControlKey.prefix(tag) };
    const n = tag.idLen();
    @memset(r.start.buf[KEY_PREFIX_LEN..][0..n], 0);
    @memset(r.end.buf[KEY_PREFIX_LEN..][0..n], 0xFF);
    r.start.len = @intCast(tag.keyLen());
    r.end.len = @intCast(tag.keyLen());
    return r;
}

/// The identity's STABLE part — what an `appender_hint` and a
/// `dead_member` are keyed by (a Free directory page is anybody's next;
/// a rejoin after a storm must find its predecessor's words).
pub const StableIdentity = struct {
    node_token: u64,
    mount_slot: u32,
};

pub fn slotStateKey(slot: u16) ControlKey {
    var k = ControlKey.prefix(.slot_state);
    k.putBe(u16, slot);
    return k;
}

pub fn slotTailsKey(slot: u16) ControlKey {
    var k = ControlKey.prefix(.slot_tails);
    k.putBe(u16, slot);
    return k;
}

pub fn custodyQuarantineKey(slot: u16) ControlKey {
    var k = ControlKey.prefix(.custody_quarantine);
    k.putBe(u16, slot);
    return k;
}

pub fn appenderHintKey(id: StableIdentity) ControlKey {
    var k = ControlKey.prefix(.appender_hint);
    k.putBe(u64, id.node_token);
    k.putBe(u32, id.mount_slot);
    return k;
}

pub fn deadMemberKey(id: StableIdentity) ControlKey {
    var k = ControlKey.prefix(.dead_member);
    k.putBe(u64, id.node_token);
    k.putBe(u32, id.mount_slot);
    return k;
}

pub fn memberKey(id: StableIdentity) ControlKey {
    var k = ControlKey.prefix(.member);
    k.putBe(u64, id.node_token);
    k.putBe(u32, id.mount_slot);
    return k;
}

pub fn recoveredKey(id: StableIdentity, vol: u16) ControlKey {
    var k = ControlKey.prefix(.recovered);
    k.putBe(u64, id.node_token);
    k.putBe(u32, id.mount_slot);
    k.putBe(u16, vol);
    return k;
}

pub fn allocLeaseKey(vol_tag: u64) ControlKey {
    var k = ControlKey.prefix(.alloc_lease);
    k.putBe(u64, vol_tag);
    return k;
}

pub fn dirRenameKey() ControlKey {
    return ControlKey.prefix(.dir_rename);
}

pub fn nodeSeqIncarnationsKey() ControlKey {
    return ControlKey.prefix(.node_seq_incarnations);
}

/// The tag of a tree-0 key, or null for a key that is not a control key
/// (an ASCII-keyed record, a wrong length, an unknown tag) — total.
pub fn tagOfKey(k: []const u8) ?Tag {
    if (k.len < KEY_PREFIX_LEN or k[0] != key.TREE_CONTROL) return null;
    const tag = std.enums.fromInt(Tag, k[1]) orelse return null;
    if (k.len != tag.keyLen()) return null;
    return tag;
}

fn idBe(comptime T: type, k: []const u8, off: usize) T {
    return std.mem.readInt(T, k[KEY_PREFIX_LEN + off ..][0..@sizeOf(T)], .big);
}

/// The slot of a `slot_state` / `slot_tails` / `custody_quarantine` key.
pub fn slotOfKey(k: []const u8) ?u16 {
    const tag = tagOfKey(k) orelse return null;
    return switch (tag) {
        .slot_state, .slot_tails, .custody_quarantine => idBe(u16, k, 0),
        else => null,
    };
}

pub fn endpointKey(appender_id: u32) ControlKey {
    var k = ControlKey.prefix(.endpoint);
    k.putBe(u32, appender_id);
    return k;
}

pub fn appenderOfKey(k: []const u8) ?u32 {
    const tag = tagOfKey(k) orelse return null;
    return switch (tag) {
        .extent_grant, .endpoint => idBe(u32, k, 0),
        else => null,
    };
}

pub fn identityOfKey(k: []const u8) ?StableIdentity {
    const tag = tagOfKey(k) orelse return null;
    return switch (tag) {
        .appender_hint, .dead_member, .recovered, .member => .{ .node_token = idBe(u64, k, 0), .mount_slot = idBe(u32, k, 8) },
        else => null,
    };
}

pub fn volOfRecoveredKey(k: []const u8) ?u16 {
    const tag = tagOfKey(k) orelse return null;
    return if (tag == .recovered) idBe(u16, k, 12) else null;
}

pub fn volTagOfKey(k: []const u8) ?u64 {
    const tag = tagOfKey(k) orelse return null;
    return if (tag == .alloc_lease) idBe(u64, k, 0) else null;
}

// @anchor:ops_jobs_keys
pub fn jobKey(job_id: u64) ControlKey {
    var k = ControlKey.prefix(.job);
    k.putBe(u64, job_id);
    return k;
}

pub fn jobShardKey(job_id: u64, shard: u32) ControlKey {
    var k = ControlKey.prefix(.job_shard);
    k.putBe(u64, job_id);
    k.putBe(u32, shard);
    return k;
}

/// The job id of a `job` or `job_shard` key.
pub fn jobIdOfKey(k: []const u8) ?u64 {
    const tag = tagOfKey(k) orelse return null;
    return switch (tag) {
        .job, .job_shard => idBe(u64, k, 0),
        else => null,
    };
}

pub fn shardOfKey(k: []const u8) ?u32 {
    const tag = tagOfKey(k) orelse return null;
    return if (tag == .job_shard) idBe(u32, k, 8) else null;
}

// ---------------------------------------------------------------------------
// Field helpers.
// ---------------------------------------------------------------------------

inline fn rd(comptime T: type, b: *const [@sizeOf(T)]u8) T {
    return std.mem.readInt(T, b, .little);
}

inline fn wr(comptime T: type, b: *[@sizeOf(T)]u8, v: T) void {
    std.mem.writeInt(T, b, v, .little);
}

fn exact(comptime T: type, bytes: []const u8) DecodeError!T {
    if (bytes.len != @sizeOf(T)) return error.Corrupt;
    return std.mem.bytesToValue(T, bytes[0..@sizeOf(T)]);
}

// ---------------------------------------------------------------------------
// slot_state:{s}
// ---------------------------------------------------------------------------

/// A slot tree's root pointer as tree 0 records it.
pub const Root = struct { addr: u64, seq: u64 };

pub const SlotVariant = enum(u8) {
    /// The root lives HERE; the slot has no lessee.
    unleased = 1,
    /// `appender_id` leases it; the LIVE root rides the lessee's page and
    /// these words are the grant-time snapshot a newer page root supersedes.
    leased = 2,
    /// The recovery's structural mark (§5.9): the dead lessee's slot between
    /// `Live → Recovering` and its `Unleased`; first touches are refused.
    releasing = 3,
};

/// The decoded record: one shape for every variant (the words a variant
/// does not carry read 0).
pub const SlotState = struct {
    variant: SlotVariant,
    /// The lessee (0 when unleased).
    appender_id: u32 = 0,
    /// The lease generation — the frame screen's word.
    g: u32,
    /// Device offset of the lessee's directory slot A (0 when unleased).
    page_addr: u64 = 0,
    root: Root,
    /// Next local ino — a FLOOR, never lowered (§5.1.8).
    cursor: u64,
    /// The tree's image count (the affinity cap's input).
    slot_tree_extents: u32 = 0,
    /// Manager seq at the last release (`prefer: unleased-then-idle`'s rank).
    last_written: u64 = 0,
    /// The departing ring's stamp frontier (charter "the record-seq space").
    seq_floor: u64 = 0,

    pub fn encode(self: SlotState) [SlotStateWire.LEN]u8 {
        return SlotStateWire.init(self).encode();
    }
};

/// `slot_state` v2 (v1 = rung 5's 28 B unleased-only shape; refused —
/// forward-only, no release carried it).
pub const SlotStateWire = extern struct {
    version_le: [2]u8,
    variant: u8,
    flags: u8,
    appender_id_le: [4]u8,
    g_le: [4]u8,
    page_addr_le: [8]u8,
    root_addr_le: [8]u8,
    root_seq_le: [8]u8,
    cursor_le: [8]u8,
    slot_tree_extents_le: [4]u8,
    reserved_le: [4]u8,
    last_written_le: [8]u8,
    seq_floor_le: [8]u8,

    pub const VERSION: u16 = 2;
    pub const LEN: usize = 68;

    pub fn init(s: SlotState) SlotStateWire {
        var w: SlotStateWire = undefined;
        wr(u16, &w.version_le, VERSION);
        w.variant = @backingInt(s.variant);
        w.flags = 0;
        wr(u32, &w.appender_id_le, s.appender_id);
        wr(u32, &w.g_le, s.g);
        wr(u64, &w.page_addr_le, s.page_addr);
        wr(u64, &w.root_addr_le, s.root.addr);
        wr(u64, &w.root_seq_le, s.root.seq);
        wr(u64, &w.cursor_le, s.cursor);
        wr(u32, &w.slot_tree_extents_le, s.slot_tree_extents);
        wr(u32, &w.reserved_le, 0);
        wr(u64, &w.last_written_le, s.last_written);
        wr(u64, &w.seq_floor_le, s.seq_floor);
        return w;
    }

    pub fn encode(self: SlotStateWire) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    /// Total: exact length, version 2, a known variant, flags and reserved
    /// zero, and the variant's absent words zero (canonical: encode ∘
    /// decode = id).
    pub fn decode(bytes: []const u8) DecodeError!SlotState {
        const w = try exact(SlotStateWire, bytes);
        if (rd(u16, &w.version_le) != VERSION) return error.Corrupt;
        if (w.flags != 0 or rd(u32, &w.reserved_le) != 0) return error.Corrupt;
        const variant = std.enums.fromInt(SlotVariant, w.variant) orelse return error.Corrupt;
        const s: SlotState = .{
            .variant = variant,
            .appender_id = rd(u32, &w.appender_id_le),
            .g = rd(u32, &w.g_le),
            .page_addr = rd(u64, &w.page_addr_le),
            .root = .{ .addr = rd(u64, &w.root_addr_le), .seq = rd(u64, &w.root_seq_le) },
            .cursor = rd(u64, &w.cursor_le),
            .slot_tree_extents = rd(u32, &w.slot_tree_extents_le),
            .last_written = rd(u64, &w.last_written_le),
            .seq_floor = rd(u64, &w.seq_floor_le),
        };
        switch (variant) {
            .unleased => if (s.appender_id != 0 or s.page_addr != 0) return error.Corrupt,
            .leased, .releasing => if (s.last_written != 0) return error.Corrupt,
        }
        return s;
    }
};

// ---------------------------------------------------------------------------
// slot_tails:{s}
// ---------------------------------------------------------------------------

pub const SlotTailsHead = extern struct {
    version_le: [2]u8,
    flags_le: [2]u8,
    g_le: [4]u8,
    count_le: [4]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 12;
    /// Bit 0: the entries are spill runs, not inline tails.
    pub const FLAG_SPILLED: u16 = 1;
};

/// One leaf's frame tail, inline.
pub const TailEntryWire = extern struct {
    leaf_addr_le: [8]u8,
    tail_le: [4]u8,
    reserved_le: [4]u8,

    pub const LEN: usize = 16;
};

/// One spill extent: `count` entries of `TailEntryWire` at `extent_addr`.
pub const SpillRunWire = extern struct {
    extent_addr_le: [8]u8,
    count_le: [4]u8,
    reserved_le: [4]u8,

    pub const LEN: usize = 16;
};

pub const TailEntry = struct { leaf_addr: u64, tail: u32 };
pub const SpillRun = struct { extent_addr: u64, count: u32 };

pub const SlotTails = union(enum) {
    inline_tails: struct { g: u32, tails: []const TailEntry },
    /// `hash64` over the concatenated entry images of every spill extent.
    spilled: struct { g: u32, runs: []const SpillRun, csum: u64 },

    pub fn g(self: SlotTails) u32 {
        return switch (self) {
            .inline_tails => |v| v.g,
            .spilled => |v| v.g,
        };
    }
};

/// Inline cap under a tree-0 value cap: `(cap − head) / entry`, and under
/// u32 (the count word). 5,482 at the 256 KiB node's 65,792 B cap × 12 B
/// in the reference; 4,111 here at 16 B entries.
pub fn slotTailsInlineCap(value_cap: u64) u32 {
    return @intCast(@min((value_cap -| SlotTailsHead.LEN) / TailEntryWire.LEN, std.math.maxInt(u32)));
}

/// Entries per spill extent of `node_size` bytes.
pub fn spillEntriesPerExtent(node_size: u64) u32 {
    return @intCast(node_size / TailEntryWire.LEN);
}

pub fn slotTailsEncodedLen(t: SlotTails) usize {
    return switch (t) {
        .inline_tails => |v| SlotTailsHead.LEN + v.tails.len * TailEntryWire.LEN,
        .spilled => |v| SlotTailsHead.LEN + v.runs.len * SpillRunWire.LEN + 8,
    };
}

/// Encode into `out` (`slotTailsEncodedLen` bytes); refuses a count past
/// u32 or an `out` too short.
pub fn encodeSlotTails(t: SlotTails, out: []u8) error{ Overflow, BufferTooSmall }![]u8 {
    const len = slotTailsEncodedLen(t);
    if (out.len < len) return error.BufferTooSmall;
    var head: SlotTailsHead = undefined;
    wr(u16, &head.version_le, SlotTailsHead.VERSION);
    switch (t) {
        .inline_tails => |v| {
            if (v.tails.len > std.math.maxInt(u32)) return error.Overflow;
            wr(u16, &head.flags_le, 0);
            wr(u32, &head.g_le, v.g);
            wr(u32, &head.count_le, @intCast(v.tails.len));
            @memcpy(out[0..SlotTailsHead.LEN], &std.mem.toBytes(head));
            for (v.tails, 0..) |e, i| {
                var w: TailEntryWire = undefined;
                wr(u64, &w.leaf_addr_le, e.leaf_addr);
                wr(u32, &w.tail_le, e.tail);
                wr(u32, &w.reserved_le, 0);
                @memcpy(out[SlotTailsHead.LEN + i * TailEntryWire.LEN ..][0..TailEntryWire.LEN], &std.mem.toBytes(w));
            }
        },
        .spilled => |v| {
            if (v.runs.len > std.math.maxInt(u32)) return error.Overflow;
            wr(u16, &head.flags_le, SlotTailsHead.FLAG_SPILLED);
            wr(u32, &head.g_le, v.g);
            wr(u32, &head.count_le, @intCast(v.runs.len));
            @memcpy(out[0..SlotTailsHead.LEN], &std.mem.toBytes(head));
            for (v.runs, 0..) |r, i| {
                var w: SpillRunWire = undefined;
                wr(u64, &w.extent_addr_le, r.extent_addr);
                wr(u32, &w.count_le, r.count);
                wr(u32, &w.reserved_le, 0);
                @memcpy(out[SlotTailsHead.LEN + i * SpillRunWire.LEN ..][0..SpillRunWire.LEN], &std.mem.toBytes(w));
            }
            std.mem.writeInt(u64, out[len - 8 ..][0..8], v.csum, .little);
        },
    }
    return out[0..len];
}

/// Total: the head exact, flags ⊆ {spilled}, the count checked against the
/// length BEFORE any element is read, every reserved word zero; the
/// entries are borrowed views into `bytes` (the caller decodes elements
/// with `tailEntryAt` / `spillRunAt`).
pub const DecodedSlotTails = struct {
    g: u32,
    spilled: bool,
    count: u32,
    entries: []const u8,
    csum: u64,

    pub fn tailEntryAt(self: DecodedSlotTails, i: usize) TailEntry {
        assert(!self.spilled and i < self.count);
        const w = std.mem.bytesToValue(TailEntryWire, self.entries[i * TailEntryWire.LEN ..][0..TailEntryWire.LEN]);
        return .{ .leaf_addr = rd(u64, &w.leaf_addr_le), .tail = rd(u32, &w.tail_le) };
    }

    pub fn spillRunAt(self: DecodedSlotTails, i: usize) SpillRun {
        assert(self.spilled and i < self.count);
        const w = std.mem.bytesToValue(SpillRunWire, self.entries[i * SpillRunWire.LEN ..][0..SpillRunWire.LEN]);
        return .{ .extent_addr = rd(u64, &w.extent_addr_le), .count = rd(u32, &w.count_le) };
    }
};

pub fn decodeSlotTails(bytes: []const u8) DecodeError!DecodedSlotTails {
    if (bytes.len < SlotTailsHead.LEN) return error.Corrupt;
    const head = std.mem.bytesToValue(SlotTailsHead, bytes[0..SlotTailsHead.LEN]);
    if (rd(u16, &head.version_le) != SlotTailsHead.VERSION) return error.Corrupt;
    const flags = rd(u16, &head.flags_le);
    if (flags & ~SlotTailsHead.FLAG_SPILLED != 0) return error.Corrupt;
    const spilled = flags & SlotTailsHead.FLAG_SPILLED != 0;
    const count = rd(u32, &head.count_le);
    const trailer: usize = if (spilled) 8 else 0;
    // Bound BEFORE multiplying: a count near u32::MAX must not wrap.
    const avail = bytes.len - SlotTailsHead.LEN;
    if (avail < trailer) return error.Corrupt;
    if ((avail - trailer) / TailEntryWire.LEN != count or (avail - trailer) % TailEntryWire.LEN != 0) return error.Corrupt;
    const entries = bytes[SlotTailsHead.LEN .. bytes.len - trailer];
    var i: usize = 0;
    while (i < count) : (i += 1) {
        // Both entry shapes carry `reserved u32` at [12..16).
        if (std.mem.readInt(u32, entries[i * 16 + 12 ..][0..4], .little) != 0) return error.Corrupt;
        if (spilled and std.mem.readInt(u32, entries[i * 16 + 8 ..][0..4], .little) == 0) return error.Corrupt;
    }
    return .{
        .g = rd(u32, &head.g_le),
        .spilled = spilled,
        .count = count,
        .entries = entries,
        .csum = if (spilled) std.mem.readInt(u64, bytes[bytes.len - 8 ..][0..8], .little) else 0,
    };
}

/// The spill checksum law: hash64 over the concatenated entry images.
pub fn spillChecksum(images: []const []const u8) u64 {
    var h: checksum.Hasher = .init();
    for (images) |img| h.update(img);
    return h.finalLow64();
}

// ---------------------------------------------------------------------------
// extent_grant:{a}:{id} — the grant record as a family of chunks
// ---------------------------------------------------------------------------

/// The key of chunk `id` of appender `appender_id`'s grant record. One
/// appender's chunks share the prefix `kind ‖ tag ‖ a`, so ONE range walk
/// (`extentGrantFamily`) yields them all in id order, the move marker
/// (`GRANT_MARKER_ID`) last.
pub fn extentGrantKey(appender_id: u32, id: u64) ControlKey {
    var k = ControlKey.prefix(.extent_grant);
    k.putBe(u32, appender_id);
    k.putBe(u64, id);
    return k;
}

/// Every chunk key of one appender's record (marker included).
pub fn extentGrantFamily(appender_id: u32) KeyRange {
    return .{ .start = extentGrantKey(appender_id, 0), .end = extentGrantKey(appender_id, GRANT_MARKER_ID) };
}

/// The chunk id of an `extent_grant` key.
pub fn chunkOfKey(k: []const u8) ?u64 {
    const tag = tagOfKey(k) orelse return null;
    return if (tag == .extent_grant) idBe(u64, k, 4) else null;
}

/// The id of a family's MOVE MARKER: present only while a record change
/// too large for one control entry is staged (`grant_record.zig`) — every
/// chunk id inside its `[staged_lo, staged_hi]` is ineffective until the
/// entry that deletes the marker lands. The largest id: it sorts last.
pub const GRANT_MARKER_ID: u64 = std.math.maxInt(u64);

pub const GrantChunkKind = enum(u8) { runs = 1, marker = 2 };

/// A chunk's head: the key's appender and id repeated (the checksum binds
/// the value to its key — a chunk read under another key is `Corrupt`),
/// the stamp over the whole value (`checksum.stamp` at `CSUM_OFF`).
pub const ExtentGrantChunkHead = extern struct {
    version_le: [2]u8,
    kind: u8,
    reserved0: u8,
    n_runs_le: [4]u8,
    appender_le: [4]u8,
    reserved1_le: [4]u8,
    id_le: [8]u8,
    /// A marker's staged range (inclusive); 0 on a runs chunk.
    staged_lo_le: [8]u8,
    staged_hi_le: [8]u8,
    csum_le: [8]u8,

    pub const VERSION: u16 = 2;
    pub const LEN: usize = 48;
    pub const CSUM_OFF: usize = 40;
};

/// One run of granted extents (indices): `[start, start + len)`. `len` is
/// u32 — an extent COUNT never exceeds u32 on a volume the format admits
/// (spec Q-12); `reserved` checked zero.
pub const GrantRunWire = extern struct {
    start_le: [8]u8,
    len_le: [4]u8,
    reserved_le: [4]u8,

    pub const LEN: usize = 16;
};

pub const GrantRun = struct {
    start: u64,
    len: u32,

    pub fn end(self: GrantRun) u64 {
        return self.start + self.len;
    }

    pub fn contains(self: GrantRun, extent: u64) bool {
        return extent >= self.start and extent < self.end();
    }
};

/// A chunk's byte bound: one 4 KiB page — the journal's write grain. A
/// carve, a return or a custody move rewrites only the chunks whose
/// extents it changes, so a one-run change costs one page of entry bytes
/// (a whole-record rewrite cost up to the tree-0 value cap, 64 KiB, per
/// change), and a record of any size is a set of tree-0 values below the
/// smallest node's value cap (16,640 B at 64 KiB — `grant_record.zig`
/// asserts it).
pub const GRANT_CHUNK_BYTES_MAX: usize = 4096;

/// The runs one chunk carries: 253.
pub const GRANT_CHUNK_RUNS: u32 = (GRANT_CHUNK_BYTES_MAX - ExtentGrantChunkHead.LEN) / GrantRunWire.LEN;

pub fn grantChunkLen(n_runs: usize) usize {
    return ExtentGrantChunkHead.LEN + n_runs * GrantRunWire.LEN;
}

/// Runs are NORMALIZED — ascending, non-empty, non-adjacent, no overflow —
/// and the decoder refuses any other spelling (one canonical image per
/// set of extents inside a chunk).
pub fn runsNormalized(runs: []const GrantRun) bool {
    var prev_end: ?u64 = null;
    for (runs) |r| {
        if (r.len == 0) return false;
        const e = std.math.add(u64, r.start, r.len) catch return false;
        if (prev_end) |pe| if (r.start <= pe) return false;
        prev_end = e;
    }
    return true;
}

fn chunkHead(kind: GrantChunkKind, appender_id: u32, id: u64, n_runs: u32, lo: u64, hi: u64) ExtentGrantChunkHead {
    var h: ExtentGrantChunkHead = undefined;
    wr(u16, &h.version_le, ExtentGrantChunkHead.VERSION);
    h.kind = @backingInt(kind);
    h.reserved0 = 0;
    wr(u32, &h.n_runs_le, n_runs);
    wr(u32, &h.appender_le, appender_id);
    wr(u32, &h.reserved1_le, 0);
    wr(u64, &h.id_le, id);
    wr(u64, &h.staged_lo_le, lo);
    wr(u64, &h.staged_hi_le, hi);
    wr(u64, &h.csum_le, 0);
    return h;
}

/// Chunk `id` of `appender_id`'s record: a non-empty normalized run list of
/// at most `GRANT_CHUNK_RUNS`, stamped. `id` is never the marker's.
pub fn encodeGrantChunk(appender_id: u32, id: u64, runs: []const GrantRun, out: []u8) error{ NotNormalized, BufferTooSmall }![]u8 {
    if (runs.len == 0 or runs.len > GRANT_CHUNK_RUNS or !runsNormalized(runs) or id == GRANT_MARKER_ID) return error.NotNormalized;
    const len = grantChunkLen(runs.len);
    if (out.len < len) return error.BufferTooSmall;
    @memcpy(out[0..ExtentGrantChunkHead.LEN], &std.mem.toBytes(chunkHead(.runs, appender_id, id, @intCast(runs.len), 0, 0)));
    for (runs, 0..) |r, i| {
        var w: GrantRunWire = undefined;
        wr(u64, &w.start_le, r.start);
        wr(u32, &w.len_le, r.len);
        wr(u32, &w.reserved_le, 0);
        @memcpy(out[ExtentGrantChunkHead.LEN + i * GrantRunWire.LEN ..][0..GrantRunWire.LEN], &std.mem.toBytes(w));
    }
    checksum.stamp(out[0..len], ExtentGrantChunkHead.CSUM_OFF);
    return out[0..len];
}

/// The family's move marker staging the chunk ids `[lo, hi]`.
pub fn encodeGrantMarker(appender_id: u32, lo: u64, hi: u64, out: *[ExtentGrantChunkHead.LEN]u8) error{NotNormalized}![]u8 {
    if (lo > hi or hi >= GRANT_MARKER_ID) return error.NotNormalized;
    out.* = std.mem.toBytes(chunkHead(.marker, appender_id, GRANT_MARKER_ID, 0, lo, hi));
    checksum.stamp(out, ExtentGrantChunkHead.CSUM_OFF);
    return out;
}

/// A decoded chunk: a borrowed run list or a marker's staged range.
pub const GrantChunk = union(GrantChunkKind) {
    runs: GrantChunkRuns,
    marker: struct { lo: u64, hi: u64 },
};

pub const GrantChunkRuns = struct {
    n_runs: u32,
    bytes: []const u8,

    pub fn runAt(self: GrantChunkRuns, i: usize) GrantRun {
        assert(i < self.n_runs);
        const w = std.mem.bytesToValue(GrantRunWire, self.bytes[i * GrantRunWire.LEN ..][0..GrantRunWire.LEN]);
        return .{ .start = rd(u64, &w.start_le), .len = rd(u32, &w.len_le) };
    }

    /// Every run into `out` (caller sizes it to `n_runs`).
    pub fn collect(self: GrantChunkRuns, out: []GrantRun) []GrantRun {
        for (out[0..self.n_runs], 0..) |*r, i| r.* = self.runAt(i);
        return out[0..self.n_runs];
    }
};

/// The chunk stored under `extent_grant:{appender_id}:{id}`: the stamp
/// verified, the head's appender and id equal to the key's, every
/// reserved word zero, the run list non-empty, normalized and at most
/// `GRANT_CHUNK_RUNS` long; a marker
/// lives only at `GRANT_MARKER_ID` and stages a non-empty range below it.
pub fn decodeGrantChunk(appender_id: u32, id: u64, bytes: []const u8) DecodeError!GrantChunk {
    if (bytes.len < ExtentGrantChunkHead.LEN) return error.Corrupt;
    if (!checksum.verify(bytes, ExtentGrantChunkHead.CSUM_OFF)) return error.Corrupt;
    const h = std.mem.bytesToValue(ExtentGrantChunkHead, bytes[0..ExtentGrantChunkHead.LEN]);
    if (rd(u16, &h.version_le) != ExtentGrantChunkHead.VERSION or h.reserved0 != 0 or rd(u32, &h.reserved1_le) != 0) return error.Corrupt;
    if (rd(u32, &h.appender_le) != appender_id or rd(u64, &h.id_le) != id) return error.Corrupt;
    const n = rd(u32, &h.n_runs_le);
    const lo = rd(u64, &h.staged_lo_le);
    const hi = rd(u64, &h.staged_hi_le);
    const kind = std.enums.fromInt(GrantChunkKind, h.kind) orelse return error.Corrupt;
    switch (kind) {
        .marker => {
            if (id != GRANT_MARKER_ID or n != 0 or bytes.len != ExtentGrantChunkHead.LEN) return error.Corrupt;
            if (lo > hi or hi >= GRANT_MARKER_ID) return error.Corrupt;
            return .{ .marker = .{ .lo = lo, .hi = hi } };
        },
        .runs => {
            if (id == GRANT_MARKER_ID or lo != 0 or hi != 0 or n == 0 or n > GRANT_CHUNK_RUNS) return error.Corrupt;
            // Bound BEFORE multiplying: a count near u32::MAX must not wrap.
            const avail = bytes.len - ExtentGrantChunkHead.LEN;
            if (avail / GrantRunWire.LEN != n or avail % GrantRunWire.LEN != 0) return error.Corrupt;
            const d: GrantChunkRuns = .{ .n_runs = n, .bytes = bytes[ExtentGrantChunkHead.LEN..] };
            var prev_end: ?u64 = null;
            for (0..n) |i| {
                const w = std.mem.bytesToValue(GrantRunWire, d.bytes[i * GrantRunWire.LEN ..][0..GrantRunWire.LEN]);
                if (rd(u32, &w.reserved_le) != 0) return error.Corrupt;
                const r = d.runAt(i);
                if (r.len == 0) return error.Corrupt;
                const e = std.math.add(u64, r.start, r.len) catch return error.Corrupt;
                if (prev_end) |pe| if (r.start <= pe) return error.Corrupt;
                prev_end = e;
            }
            return .{ .runs = d };
        },
    }
}

// ---------------------------------------------------------------------------
// appender_hint:{identity}
// ---------------------------------------------------------------------------

/// A GrowRing segment carved that no page names yet — bound to the
/// INCARNATION that asked (`appender_id` + page `term`).
pub const PendingSegment = struct {
    appender_id: u32,
    term: u64,
    /// Device bytes.
    start: u64,
    len: u64,
};

pub const AppenderHint = struct {
    /// The ring size the identity's GrowRings reached (0 = no hint).
    ring_bytes: u64 = 0,
    /// The largest derived grant it asked (0 = no hint).
    grant_extents: u64 = 0,
    pending: ?PendingSegment = null,

    pub fn encode(self: AppenderHint) [AppenderHintWire.LEN]u8 {
        return AppenderHintWire.init(self).encode();
    }
};

pub const AppenderHintWire = extern struct {
    version_le: [2]u8,
    flags_le: [2]u8,
    ring_bytes_le: [8]u8,
    grant_extents_le: [8]u8,
    pending_appender_id_le: [4]u8,
    reserved_le: [4]u8,
    pending_term_le: [8]u8,
    pending_start_le: [8]u8,
    pending_len_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 52;
    pub const FLAG_PENDING: u16 = 1;

    pub fn init(h: AppenderHint) AppenderHintWire {
        var w: AppenderHintWire = std.mem.zeroes(AppenderHintWire);
        wr(u16, &w.version_le, VERSION);
        wr(u16, &w.flags_le, if (h.pending != null) FLAG_PENDING else 0);
        wr(u64, &w.ring_bytes_le, h.ring_bytes);
        wr(u64, &w.grant_extents_le, h.grant_extents);
        if (h.pending) |p| {
            wr(u32, &w.pending_appender_id_le, p.appender_id);
            wr(u64, &w.pending_term_le, p.term);
            wr(u64, &w.pending_start_le, p.start);
            wr(u64, &w.pending_len_le, p.len);
        }
        return w;
    }

    pub fn encode(self: AppenderHintWire) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    /// Total: every pending word 0 and checked 0 when the flag is clear;
    /// a pending segment of zero length or past u64 is refused.
    pub fn decode(bytes: []const u8) DecodeError!AppenderHint {
        const w = try exact(AppenderHintWire, bytes);
        if (rd(u16, &w.version_le) != VERSION) return error.Corrupt;
        const flags = rd(u16, &w.flags_le);
        if (flags & ~FLAG_PENDING != 0) return error.Corrupt;
        if (rd(u32, &w.reserved_le) != 0) return error.Corrupt;
        var h: AppenderHint = .{ .ring_bytes = rd(u64, &w.ring_bytes_le), .grant_extents = rd(u64, &w.grant_extents_le) };
        const p: PendingSegment = .{ .appender_id = rd(u32, &w.pending_appender_id_le), .term = rd(u64, &w.pending_term_le), .start = rd(u64, &w.pending_start_le), .len = rd(u64, &w.pending_len_le) };
        if (flags & FLAG_PENDING != 0) {
            if (p.len == 0) return error.Corrupt;
            _ = std.math.add(u64, p.start, p.len) catch return error.Corrupt;
            h.pending = p;
        } else if (p.appender_id != 0 or p.term != 0 or p.start != 0 or p.len != 0) return error.Corrupt;
        return h;
    }
};

// ---------------------------------------------------------------------------
// custody_quarantine:{slot}, dir_rename — volume 0 only
// ---------------------------------------------------------------------------

pub const CustodyQuarantineWire = extern struct {
    version_le: [2]u8,
    reserved_le: [2]u8,
    /// CLOCK_REALTIME ns: a deadline compared on another host.
    until_realtime_ns_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 12;

    pub fn init(until_realtime_ns: u64) CustodyQuarantineWire {
        var w: CustodyQuarantineWire = undefined;
        wr(u16, &w.version_le, VERSION);
        wr(u16, &w.reserved_le, 0);
        wr(u64, &w.until_realtime_ns_le, until_realtime_ns);
        return w;
    }

    pub fn encode(self: CustodyQuarantineWire) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    pub fn decode(bytes: []const u8) DecodeError!u64 {
        const w = try exact(CustodyQuarantineWire, bytes);
        if (rd(u16, &w.version_le) != VERSION or rd(u16, &w.reserved_le) != 0) return error.Corrupt;
        return rd(u64, &w.until_realtime_ns_le);
    }
};

pub const DirRename = struct {
    holder: u32,
    /// PROVENANCE (the serving manager's era), never a check.
    term: u64,
    since_realtime_ns: u64,
};

pub const DirRenameWire = extern struct {
    version_le: [2]u8,
    reserved_le: [2]u8,
    holder_le: [4]u8,
    term_le: [8]u8,
    since_realtime_ns_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 24;

    pub fn init(d: DirRename) DirRenameWire {
        var w: DirRenameWire = undefined;
        wr(u16, &w.version_le, VERSION);
        wr(u16, &w.reserved_le, 0);
        wr(u32, &w.holder_le, d.holder);
        wr(u64, &w.term_le, d.term);
        wr(u64, &w.since_realtime_ns_le, d.since_realtime_ns);
        return w;
    }

    pub fn encode(self: DirRenameWire) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    pub fn decode(bytes: []const u8) DecodeError!DirRename {
        const w = try exact(DirRenameWire, bytes);
        if (rd(u16, &w.version_le) != VERSION or rd(u16, &w.reserved_le) != 0) return error.Corrupt;
        return .{ .holder = rd(u32, &w.holder_le), .term = rd(u64, &w.term_le), .since_realtime_ns = rd(u64, &w.since_realtime_ns_le) };
    }
};

// ---------------------------------------------------------------------------
// alloc_lease:{vol}, dead_member:{id}, recovered:{id, vol} — volume 0 only
// ---------------------------------------------------------------------------

pub const AllocLeaseHead = extern struct {
    version_le: [2]u8,
    reserved_le: [2]u8,
    node_token_le: [8]u8,
    mount_slot_le: [4]u8,
    holder_appender_id_le: [4]u8,
    writer_id_le: [8]u8,
    home_vol_le: [2]u8,
    reserved2_le: [2]u8,
    n_bitmap_le: [4]u8,
    control_ino_le: [8]u8,
    blocks_le: [8]u8,
    term_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 60;
};

/// One extent of the data allocation bitmap on the holder's home volume.
pub const BitmapRefWire = extern struct {
    vol_le: [2]u8,
    reserved: [6]u8,
    start_le: [8]u8,
    len_le: [8]u8,

    pub const LEN: usize = 24;
};

pub const BitmapRef = struct { vol: u16, start: u64, len: u64 };

pub const AllocLease = struct {
    node_token: u64,
    mount_slot: u32,
    holder_appender_id: u32,
    writer_id: u64,
    home_vol: u16,
    control_ino: u64,
    blocks: u64,
    term: u64,
    bitmap: []const BitmapRef,
};

pub fn allocLeaseEncodedLen(n_bitmap: usize) usize {
    return AllocLeaseHead.LEN + n_bitmap * BitmapRefWire.LEN;
}

pub fn encodeAllocLease(l: AllocLease, out: []u8) error{ Overflow, BufferTooSmall, ZeroLength }![]u8 {
    if (l.bitmap.len > std.math.maxInt(u32)) return error.Overflow;
    const len = allocLeaseEncodedLen(l.bitmap.len);
    if (out.len < len) return error.BufferTooSmall;
    var h: AllocLeaseHead = undefined;
    wr(u16, &h.version_le, AllocLeaseHead.VERSION);
    wr(u16, &h.reserved_le, 0);
    wr(u64, &h.node_token_le, l.node_token);
    wr(u32, &h.mount_slot_le, l.mount_slot);
    wr(u32, &h.holder_appender_id_le, l.holder_appender_id);
    wr(u64, &h.writer_id_le, l.writer_id);
    wr(u16, &h.home_vol_le, l.home_vol);
    wr(u16, &h.reserved2_le, 0);
    wr(u32, &h.n_bitmap_le, @intCast(l.bitmap.len));
    wr(u64, &h.control_ino_le, l.control_ino);
    wr(u64, &h.blocks_le, l.blocks);
    wr(u64, &h.term_le, l.term);
    @memcpy(out[0..AllocLeaseHead.LEN], &std.mem.toBytes(h));
    for (l.bitmap, 0..) |b, i| {
        if (b.len == 0) return error.ZeroLength;
        var w: BitmapRefWire = std.mem.zeroes(BitmapRefWire);
        wr(u16, &w.vol_le, b.vol);
        wr(u64, &w.start_le, b.start);
        wr(u64, &w.len_le, b.len);
        @memcpy(out[AllocLeaseHead.LEN + i * BitmapRefWire.LEN ..][0..BitmapRefWire.LEN], &std.mem.toBytes(w));
    }
    return out[0..len];
}

pub const DecodedAllocLease = struct {
    lease: AllocLease,
    refs: []const u8,

    pub fn bitmapAt(self: DecodedAllocLease, i: usize) BitmapRef {
        const w = std.mem.bytesToValue(BitmapRefWire, self.refs[i * BitmapRefWire.LEN ..][0..BitmapRefWire.LEN]);
        return .{ .vol = rd(u16, &w.vol_le), .start = rd(u64, &w.start_le), .len = rd(u64, &w.len_le) };
    }

    pub fn count(self: DecodedAllocLease) usize {
        return self.refs.len / BitmapRefWire.LEN;
    }
};

pub fn decodeAllocLease(bytes: []const u8) DecodeError!DecodedAllocLease {
    if (bytes.len < AllocLeaseHead.LEN) return error.Corrupt;
    const h = std.mem.bytesToValue(AllocLeaseHead, bytes[0..AllocLeaseHead.LEN]);
    if (rd(u16, &h.version_le) != AllocLeaseHead.VERSION) return error.Corrupt;
    if (rd(u16, &h.reserved_le) != 0 or rd(u16, &h.reserved2_le) != 0) return error.Corrupt;
    const n = rd(u32, &h.n_bitmap_le);
    const avail = bytes.len - AllocLeaseHead.LEN;
    if (avail / BitmapRefWire.LEN != n or avail % BitmapRefWire.LEN != 0) return error.Corrupt;
    const refs = bytes[AllocLeaseHead.LEN..];
    var i: usize = 0;
    while (i < n) : (i += 1) {
        const w = std.mem.bytesToValue(BitmapRefWire, refs[i * BitmapRefWire.LEN ..][0..BitmapRefWire.LEN]);
        if (!std.mem.allEqual(u8, &w.reserved, 0)) return error.Corrupt;
        const len = rd(u64, &w.len_le);
        if (len == 0) return error.Corrupt;
        _ = std.math.add(u64, rd(u64, &w.start_le), len) catch return error.Corrupt;
    }
    return .{
        .lease = .{
            .node_token = rd(u64, &h.node_token_le),
            .mount_slot = rd(u32, &h.mount_slot_le),
            .holder_appender_id = rd(u32, &h.holder_appender_id_le),
            .writer_id = rd(u64, &h.writer_id_le),
            .home_vol = rd(u16, &h.home_vol_le),
            .control_ino = rd(u64, &h.control_ino_le),
            .blocks = rd(u64, &h.blocks_le),
            .term = rd(u64, &h.term_le),
            .bitmap = &.{},
        },
        .refs = refs,
    };
}

pub const DeadMember = struct {
    epoch: u64,
    ts_realtime_ns: u64,
    pr_key: u64,
    /// An EARLY death record (the home shard evicted before `T_owner`):
    /// the recovered slots are quarantined.
    early: bool,
};

pub const DeadMemberWire = extern struct {
    version_le: [2]u8,
    flags_le: [2]u8,
    reserved_le: [4]u8,
    epoch_le: [8]u8,
    ts_realtime_ns_le: [8]u8,
    pr_key_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 32;
    pub const FLAG_EARLY: u16 = 1;

    pub fn init(d: DeadMember) DeadMemberWire {
        var w: DeadMemberWire = undefined;
        wr(u16, &w.version_le, VERSION);
        wr(u16, &w.flags_le, if (d.early) FLAG_EARLY else 0);
        wr(u32, &w.reserved_le, 0);
        wr(u64, &w.epoch_le, d.epoch);
        wr(u64, &w.ts_realtime_ns_le, d.ts_realtime_ns);
        wr(u64, &w.pr_key_le, d.pr_key);
        return w;
    }

    pub fn encode(self: DeadMemberWire) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    pub fn decode(bytes: []const u8) DecodeError!DeadMember {
        const w = try exact(DeadMemberWire, bytes);
        if (rd(u16, &w.version_le) != VERSION or rd(u32, &w.reserved_le) != 0) return error.Corrupt;
        const flags = rd(u16, &w.flags_le);
        if (flags & ~FLAG_EARLY != 0) return error.Corrupt;
        return .{ .epoch = rd(u64, &w.epoch_le), .ts_realtime_ns = rd(u64, &w.ts_realtime_ns_le), .pr_key = rd(u64, &w.pr_key_le), .early = flags & FLAG_EARLY != 0 };
    }
};

pub const Recovered = struct { by_term: u64, ts_realtime_ns: u64 };

pub const RecoveredWire = extern struct {
    version_le: [2]u8,
    reserved_le: [2]u8,
    reserved2_le: [4]u8,
    by_term_le: [8]u8,
    ts_realtime_ns_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 24;

    pub fn init(r: Recovered) RecoveredWire {
        var w: RecoveredWire = undefined;
        wr(u16, &w.version_le, VERSION);
        wr(u16, &w.reserved_le, 0);
        wr(u32, &w.reserved2_le, 0);
        wr(u64, &w.by_term_le, r.by_term);
        wr(u64, &w.ts_realtime_ns_le, r.ts_realtime_ns);
        return w;
    }

    pub fn encode(self: RecoveredWire) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    pub fn decode(bytes: []const u8) DecodeError!Recovered {
        const w = try exact(RecoveredWire, bytes);
        if (rd(u16, &w.version_le) != VERSION or rd(u16, &w.reserved_le) != 0 or rd(u32, &w.reserved2_le) != 0) return error.Corrupt;
        return .{ .by_term = rd(u64, &w.by_term_le), .ts_realtime_ns = rd(u64, &w.ts_realtime_ns_le) };
    }
};

// ---------------------------------------------------------------------------
// endpoint: a writer's published listener (family, port, address) and its
// registrant key — the durable half of the manager's endpoint table.
// ---------------------------------------------------------------------------

pub const Endpoint = struct {
    /// `wire/endpoint.zig`'s families: 4 / 6 (never none — a writer with
    /// no listener publishes nothing).
    family: u8,
    port: u16,
    addr: [16]u8,
    pr_key: u64,
};

pub const EndpointRecordWire = extern struct {
    version_le: [2]u8,
    family: u8,
    reserved: u8,
    port_le: [2]u8,
    reserved2_le: [2]u8,
    addr: [16]u8,
    pr_key_le: [8]u8,

    pub const VERSION: u16 = 1;
    pub const LEN: usize = 32;
    pub const FAMILY_IP4: u8 = 4;
    pub const FAMILY_IP6: u8 = 6;

    pub fn init(e: Endpoint) EndpointRecordWire {
        var w: EndpointRecordWire = undefined;
        wr(u16, &w.version_le, VERSION);
        w.family = e.family;
        w.reserved = 0;
        wr(u16, &w.port_le, e.port);
        wr(u16, &w.reserved2_le, 0);
        w.addr = e.addr;
        wr(u64, &w.pr_key_le, e.pr_key);
        return w;
    }

    pub fn encode(self: EndpointRecordWire) [LEN]u8 {
        return std.mem.toBytes(self);
    }

    pub fn decode(bytes: []const u8) DecodeError!Endpoint {
        const w = try exact(EndpointRecordWire, bytes);
        if (rd(u16, &w.version_le) != VERSION or w.reserved != 0 or rd(u16, &w.reserved2_le) != 0) return error.Corrupt;
        switch (w.family) {
            FAMILY_IP4 => if (!std.mem.allEqual(u8, w.addr[4..], 0)) return error.Corrupt,
            FAMILY_IP6 => {},
            else => return error.Corrupt,
        }
        if (rd(u16, &w.port_le) == 0) return error.Corrupt;
        return .{ .family = w.family, .port = rd(u16, &w.port_le), .addr = w.addr, .pr_key = rd(u64, &w.pr_key_le) };
    }
};

// ---------------------------------------------------------------------------
// node_seq_incarnations: the value codec is `node_seq.zig`'s (9 B, total);
// the key is this module's binary one.
// ---------------------------------------------------------------------------

pub const encodeNodeSeqIncarnations = node_seq.encodeIncarnations;
pub const decodeNodeSeqIncarnations = node_seq.decodeIncarnations;

comptime {
    assert(@sizeOf(SlotStateWire) == SlotStateWire.LEN);
    assert(@sizeOf(SlotTailsHead) == SlotTailsHead.LEN);
    assert(@sizeOf(TailEntryWire) == TailEntryWire.LEN and @sizeOf(SpillRunWire) == SpillRunWire.LEN);
    assert(@sizeOf(ExtentGrantChunkHead) == ExtentGrantChunkHead.LEN and @sizeOf(GrantRunWire) == GrantRunWire.LEN);
    assert(@offsetOf(ExtentGrantChunkHead, "csum_le") == ExtentGrantChunkHead.CSUM_OFF);
    assert(@sizeOf(AppenderHintWire) == AppenderHintWire.LEN);
    assert(@sizeOf(CustodyQuarantineWire) == CustodyQuarantineWire.LEN);
    assert(@sizeOf(DirRenameWire) == DirRenameWire.LEN);
    assert(@sizeOf(AllocLeaseHead) == AllocLeaseHead.LEN and @sizeOf(BitmapRefWire) == BitmapRefWire.LEN);
    assert(@sizeOf(DeadMemberWire) == DeadMemberWire.LEN);
    assert(@sizeOf(RecoveredWire) == RecoveredWire.LEN);
    assert(@sizeOf(EndpointRecordWire) == EndpointRecordWire.LEN);
    // Every control key sorts below the ASCII-keyed records of tree 0.
    assert(key.TREE_CONTROL < 'a');
    _ = checksum;
}

// ---------------------------------------------------------------------------
// Tests (spec §7: codecs + fuzz).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "control keys: kind ‖ tag ‖ BE id, inside the tag's range, index order; the inverse maps answer; foreign keys are null" {
    const a = slotStateKey(2);
    const b = slotStateKey(10);
    try testing.expectEqual(@as(usize, 4), a.len);
    try testing.expectEqualSlices(u8, &.{ 8, 1, 0, 2 }, a.slice());
    // Index order (the decimal spelling sorted 10 before 2).
    try testing.expect(key.lessThan(a.slice(), b.slice()));
    const r = rangeOf(.slot_state);
    try testing.expect(!key.lessThan(a.slice(), r.start.slice()) and !key.lessThan(r.end.slice(), b.slice()));
    try testing.expectEqual(@as(?u16, 10), slotOfKey(b.slice()));
    try testing.expectEqual(@as(?Tag, .slot_state), tagOfKey(b.slice()));
    try testing.expectEqual(@as(?Tag, null), tagOfKey("slot_state:7"));
    try testing.expectEqual(@as(?Tag, null), tagOfKey(&.{ 8, 1, 0 }));
    try testing.expectEqual(@as(?Tag, null), tagOfKey(&.{ 8, 99, 0, 0 }));
    try testing.expectEqual(@as(?Tag, null), tagOfKey(&.{ 1, 1, 0, 0 }));
    try testing.expectEqual(@as(?u16, null), slotOfKey(extentGrantKey(3, 0).slice()));
    try testing.expectEqual(@as(?u32, 0x01020304), appenderOfKey(extentGrantKey(0x01020304, 9).slice()));
    try testing.expectEqual(@as(?u64, 0x0506070809), chunkOfKey(extentGrantKey(1, 0x0506070809).slice()));
    try testing.expectEqual(@as(usize, 14), extentGrantKey(1, 0).len);
    // One appender's family: its chunks in id order, the marker last, no
    // other appender's chunk inside.
    const fam = extentGrantFamily(7);
    try testing.expect(key.lessThan(fam.start.slice(), extentGrantKey(7, 1).slice()) and key.lessThan(extentGrantKey(7, 1).slice(), fam.end.slice()));
    try testing.expectEqualSlices(u8, extentGrantKey(7, GRANT_MARKER_ID).slice(), fam.end.slice());
    try testing.expect(key.lessThan(extentGrantKey(6, GRANT_MARKER_ID).slice(), fam.start.slice()) and key.lessThan(fam.end.slice(), extentGrantKey(8, 0).slice()));
    const id: StableIdentity = .{ .node_token = 0xAABB, .mount_slot = 0x5001 };
    try testing.expectEqual(@as(usize, 14), appenderHintKey(id).len);
    try testing.expectEqual(id, identityOfKey(appenderHintKey(id).slice()).?);
    try testing.expectEqual(id, identityOfKey(deadMemberKey(id).slice()).?);
    try testing.expectEqual(@as(?u16, 7), volOfRecoveredKey(recoveredKey(id, 7).slice()));
    try testing.expectEqual(@as(usize, 16), recoveredKey(id, 7).len);
    try testing.expectEqual(@as(?u64, 0xDEAD), volTagOfKey(allocLeaseKey(0xDEAD).slice()));
    try testing.expectEqual(@as(usize, 2), dirRenameKey().len);
    try testing.expectEqual(@as(?Tag, .node_seq_incarnations), tagOfKey(nodeSeqIncarnationsKey().slice()));
    // Every control key sorts below "set_config".
    try testing.expect(key.lessThan(recoveredKey(.{ .node_token = std.math.maxInt(u64), .mount_slot = std.math.maxInt(u32) }, 0xFFFF).slice(), "set_config"));
    // Distinct tags never collide at equal id bytes.
    try testing.expect(!std.mem.eql(u8, slotStateKey(5).slice(), slotTailsKey(5).slice()));
}

test "slot_state v2: one 68 B shape for Unleased / Leased / Releasing; round-trips; v1, a stray word, flags, a bad variant and a wrong length refuse" {
    const un: SlotState = .{ .variant = .unleased, .g = 3, .root = .{ .addr = 0x40000, .seq = 77 }, .cursor = 9000, .slot_tree_extents = 4, .last_written = 12, .seq_floor = 500 };
    const enc = un.encode();
    try testing.expectEqual(@as(usize, 68), enc.len);
    try testing.expectEqual(un, try SlotStateWire.decode(&enc));
    const le: SlotState = .{ .variant = .leased, .appender_id = 7, .g = 4, .page_addr = 0x1000, .root = .{ .addr = 0x40000, .seq = 77 }, .cursor = 9000, .slot_tree_extents = 4, .seq_floor = 600 };
    try testing.expectEqual(le, try SlotStateWire.decode(&le.encode()));
    const rel: SlotState = .{ .variant = .releasing, .appender_id = 7, .g = 4, .page_addr = 0x1000, .root = .{ .addr = 1, .seq = 2 }, .cursor = 3 };
    try testing.expectEqual(rel, try SlotStateWire.decode(&rel.encode()));
    try testing.expectError(error.Corrupt, SlotStateWire.decode(enc[0..67]));
    var v1 = enc;
    v1[0] = 1;
    try testing.expectError(error.Corrupt, SlotStateWire.decode(&v1));
    var bad_variant = enc;
    bad_variant[2] = 4;
    try testing.expectError(error.Corrupt, SlotStateWire.decode(&bad_variant));
    var flags = enc;
    flags[3] = 1;
    try testing.expectError(error.Corrupt, SlotStateWire.decode(&flags));
    // An unleased record naming a lessee is not canonical.
    var stray = enc;
    stray[4] = 1;
    try testing.expectError(error.Corrupt, SlotStateWire.decode(&stray));
    // A leased record with `last_written` set is not canonical either.
    var le_lw = le.encode();
    le_lw[52] = 1;
    try testing.expectError(error.Corrupt, SlotStateWire.decode(&le_lw));
    var reserved = enc;
    reserved[48] = 1;
    try testing.expectError(error.Corrupt, SlotStateWire.decode(&reserved));
}

test "slot_tails: inline and spilled round-trip behind the flag bit; the count is bounded by the length; reserved words and a zero-count spill run refuse; caps derive from the value cap" {
    var buf: [256]u8 = undefined;
    const tails = [_]TailEntry{ .{ .leaf_addr = 0x10000, .tail = 3 }, .{ .leaf_addr = 0x20000, .tail = 0 } };
    const enc = try encodeSlotTails(.{ .inline_tails = .{ .g = 9, .tails = &tails } }, &buf);
    try testing.expectEqual(@as(usize, 12 + 32), enc.len);
    const d = try decodeSlotTails(enc);
    try testing.expectEqual(@as(u32, 9), d.g);
    try testing.expect(!d.spilled);
    try testing.expectEqual(@as(u32, 2), d.count);
    try testing.expectEqual(tails[1], d.tailEntryAt(1));
    const runs = [_]SpillRun{.{ .extent_addr = 0x300000, .count = 4096 }};
    const img = [_]u8{ 1, 2, 3 };
    const sp = try encodeSlotTails(.{ .spilled = .{ .g = 10, .runs = &runs, .csum = spillChecksum(&.{&img}) } }, &buf);
    try testing.expectEqual(@as(usize, 12 + 16 + 8), sp.len);
    const ds = try decodeSlotTails(sp);
    try testing.expect(ds.spilled);
    try testing.expectEqual(runs[0], ds.spillRunAt(0));
    try testing.expectEqual(checksum.hash64(&img), ds.csum);
    // A count that does not match the length refuses before any element is read.
    var short = buf;
    @memcpy(short[0..enc.len], enc);
    try testing.expectError(error.Corrupt, decodeSlotTails(short[0 .. enc.len - 1]));
    var wild = buf;
    @memcpy(wild[0..enc.len], enc);
    std.mem.writeInt(u32, wild[8..12], std.math.maxInt(u32), .little);
    try testing.expectError(error.Corrupt, decodeSlotTails(wild[0..enc.len]));
    var res = buf;
    @memcpy(res[0..enc.len], enc);
    res[12 + 12] = 1;
    try testing.expectError(error.Corrupt, decodeSlotTails(res[0..enc.len]));
    var flags2 = buf;
    @memcpy(flags2[0..enc.len], enc);
    flags2[2] = 2;
    try testing.expectError(error.Corrupt, decodeSlotTails(flags2[0..enc.len]));
    var zero_run = buf;
    @memcpy(zero_run[0..sp.len], sp);
    std.mem.writeInt(u32, zero_run[12 + 8 ..][0..4], 0, .little);
    try testing.expectError(error.Corrupt, decodeSlotTails(zero_run[0..sp.len]));
    try testing.expectError(error.BufferTooSmall, encodeSlotTails(.{ .inline_tails = .{ .g = 1, .tails = &tails } }, buf[0..20]));
    try testing.expectEqual(@as(u32, 4111), slotTailsInlineCap(65_792));
    try testing.expectEqual(@as(u32, 16384), spillEntriesPerExtent(262_144));
}

test "extent_grant chunks: normalized runs round-trip stamped and bound to their key; ascending/non-empty/non-adjacent/overflow each refuse in both directions; a marker stages a range at the marker id only; the chunk run cap ties" {
    var buf: [512]u8 = undefined;
    const runs = [_]GrantRun{ .{ .start = 10, .len = 2 }, .{ .start = 13, .len = 1 }, .{ .start = 100, .len = 50 } };
    const enc = try encodeGrantChunk(4, 9, &runs, &buf);
    try testing.expectEqual(@as(usize, 48 + 48), enc.len);
    const d = (try decodeGrantChunk(4, 9, enc)).runs;
    try testing.expectEqual(@as(u32, 3), d.n_runs);
    try testing.expectEqual(runs[2], d.runAt(2));
    var out: [3]GrantRun = undefined;
    try testing.expectEqualSlices(GrantRun, &runs, d.collect(&out));
    // Bound to its key: another appender's or another id's key refuses.
    try testing.expectError(error.Corrupt, decodeGrantChunk(5, 9, enc));
    try testing.expectError(error.Corrupt, decodeGrantChunk(4, 10, enc));
    try testing.expectError(error.NotNormalized, encodeGrantChunk(4, 9, &.{}, &buf));
    try testing.expectError(error.NotNormalized, encodeGrantChunk(4, 9, &.{ .{ .start = 10, .len = 2 }, .{ .start = 12, .len = 1 } }, &buf)); // adjacent
    try testing.expectError(error.NotNormalized, encodeGrantChunk(4, 9, &.{.{ .start = 10, .len = 0 }}, &buf));
    try testing.expectError(error.NotNormalized, encodeGrantChunk(4, 9, &.{ .{ .start = 20, .len = 1 }, .{ .start = 10, .len = 1 } }, &buf));
    try testing.expectError(error.NotNormalized, encodeGrantChunk(4, 9, &.{.{ .start = std.math.maxInt(u64), .len = 1 }}, &buf));
    try testing.expectError(error.NotNormalized, encodeGrantChunk(4, GRANT_MARKER_ID, &runs, &buf));
    try testing.expectError(error.BufferTooSmall, encodeGrantChunk(4, 9, &runs, buf[0..60]));
    // The decoder refuses the same spellings byte-wise — even re-stamped.
    const Mut = struct {
        fn refused(src: []const u8, off: usize, v: u64, width: u8) !void {
            var m: [512]u8 = undefined;
            @memcpy(m[0..src.len], src);
            switch (width) {
                1 => m[off] = @truncate(v),
                4 => std.mem.writeInt(u32, m[off..][0..4], @truncate(v), .little),
                else => std.mem.writeInt(u64, m[off..][0..8], v, .little),
            }
            try testing.expectError(error.Corrupt, decodeGrantChunk(4, 9, m[0..src.len]));
            checksum.stamp(m[0..src.len], ExtentGrantChunkHead.CSUM_OFF);
            try testing.expectError(error.Corrupt, decodeGrantChunk(4, 9, m[0..src.len]));
        }
    };
    try Mut.refused(enc, 48 + 16, 12, 8); // the second run adjacent to the first
    try Mut.refused(enc, 48 + 8, 0, 4); // a zero-length run
    try Mut.refused(enc, 48 + 12, 1, 4); // a run's reserved word
    try Mut.refused(enc, 4, 2, 4); // the count
    try Mut.refused(enc, 3, 1, 1); // reserved0
    try Mut.refused(enc, 12, 1, 4); // reserved1
    try Mut.refused(enc, 2, 3, 1); // an unknown kind
    try Mut.refused(enc, 24, 1, 8); // a staged range on a runs chunk
    // A flipped byte anywhere fails the stamp.
    var flip: [512]u8 = undefined;
    @memcpy(flip[0..enc.len], enc);
    flip[60] ^= 1;
    try testing.expectError(error.Corrupt, decodeGrantChunk(4, 9, flip[0..enc.len]));
    try testing.expectError(error.Corrupt, decodeGrantChunk(4, 9, enc[0..47]));
    // The marker.
    var mbuf: [ExtentGrantChunkHead.LEN]u8 = undefined;
    const m = try encodeGrantMarker(4, 20, 30, &mbuf);
    const md = (try decodeGrantChunk(4, GRANT_MARKER_ID, m)).marker;
    try testing.expectEqual(@as(u64, 20), md.lo);
    try testing.expectEqual(@as(u64, 30), md.hi);
    try testing.expectError(error.Corrupt, decodeGrantChunk(4, 3, m));
    try testing.expectError(error.NotNormalized, encodeGrantMarker(4, 31, 30, &mbuf));
    try testing.expectError(error.NotNormalized, encodeGrantMarker(4, 1, GRANT_MARKER_ID, &mbuf));
    try testing.expectEqual(@as(u32, 253), GRANT_CHUNK_RUNS);
    try testing.expectEqual(GRANT_CHUNK_BYTES_MAX, grantChunkLen(GRANT_CHUNK_RUNS));
    var many: [GRANT_CHUNK_RUNS + 1]GrantRun = undefined;
    for (&many, 0..) |*r, i| r.* = .{ .start = 2 * i, .len = 1 };
    var big: [GRANT_CHUNK_BYTES_MAX + 16]u8 = undefined;
    try testing.expectError(error.NotNormalized, encodeGrantChunk(4, 9, &many, &big));
    _ = try encodeGrantChunk(4, 9, many[0..GRANT_CHUNK_RUNS], &big);
}

test "appender_hint: a pending segment rides the flag; the clear form checks every pending word zero; a zero-length or overflowing segment refuses" {
    const plain: AppenderHint = .{ .ring_bytes = 1 << 20, .grant_extents = 40 };
    try testing.expectEqual(plain, try AppenderHintWire.decode(&plain.encode()));
    try testing.expectEqual(@as(usize, 52), plain.encode().len);
    const pending: AppenderHint = .{ .ring_bytes = 2 << 20, .pending = .{ .appender_id = 3, .term = 5, .start = 0x400000, .len = 0x40000 } };
    try testing.expectEqual(pending, try AppenderHintWire.decode(&pending.encode()));
    var stray = plain.encode();
    stray[36] = 1; // pending_term with the flag clear
    try testing.expectError(error.Corrupt, AppenderHintWire.decode(&stray));
    var zero_len = pending.encode();
    @memset(zero_len[44..52], 0);
    try testing.expectError(error.Corrupt, AppenderHintWire.decode(&zero_len));
    var overflow = pending.encode();
    @memset(overflow[36..44], 0xFF);
    try testing.expectError(error.Corrupt, AppenderHintWire.decode(&overflow));
    var flags = plain.encode();
    flags[2] = 2;
    try testing.expectError(error.Corrupt, AppenderHintWire.decode(&flags));
    try testing.expectError(error.Corrupt, AppenderHintWire.decode(plain.encode()[0..51]));
}

test "the volume-0 records: custody_quarantine, dir_rename, alloc_lease (+ bitmap refs), dead_member (early flag), recovered — one fixed shape each, round-trip, reserved checked" {
    try testing.expectEqual(@as(u64, 123_456), try CustodyQuarantineWire.decode(&CustodyQuarantineWire.init(123_456).encode()));
    var cq = CustodyQuarantineWire.init(1).encode();
    cq[2] = 1;
    try testing.expectError(error.Corrupt, CustodyQuarantineWire.decode(&cq));
    const dr: DirRename = .{ .holder = 4, .term = 9, .since_realtime_ns = 1_700_000_000_000_000_000 };
    try testing.expectEqual(dr, try DirRenameWire.decode(&DirRenameWire.init(dr).encode()));
    try testing.expectEqual(@as(usize, 24), DirRenameWire.LEN);
    var buf: [256]u8 = undefined;
    const refs = [_]BitmapRef{ .{ .vol = 0, .start = 0x800000, .len = 0x40000 }, .{ .vol = 0, .start = 0x900000, .len = 0x40000 } };
    const lease: AllocLease = .{ .node_token = 1, .mount_slot = 2, .holder_appender_id = 3, .writer_id = 4, .home_vol = 0, .control_ino = 6, .blocks = 7, .term = 8, .bitmap = &refs };
    const enc = try encodeAllocLease(lease, &buf);
    try testing.expectEqual(@as(usize, 60 + 48), enc.len);
    const d = try decodeAllocLease(enc);
    try testing.expectEqual(@as(u64, 4), d.lease.writer_id);
    try testing.expectEqual(@as(usize, 2), d.count());
    try testing.expectEqual(refs[1], d.bitmapAt(1));
    var zl = buf;
    @memcpy(zl[0..enc.len], enc);
    @memset(zl[60 + 16 ..][0..8], 0);
    try testing.expectError(error.Corrupt, decodeAllocLease(zl[0..enc.len]));
    var rsv = buf;
    @memcpy(rsv[0..enc.len], enc);
    rsv[60 + 3] = 1;
    try testing.expectError(error.Corrupt, decodeAllocLease(rsv[0..enc.len]));
    try testing.expectError(error.ZeroLength, encodeAllocLease(.{ .node_token = 1, .mount_slot = 2, .holder_appender_id = 3, .writer_id = 4, .home_vol = 0, .control_ino = 6, .blocks = 7, .term = 8, .bitmap = &.{.{ .vol = 0, .start = 1, .len = 0 }} }, &buf));
    const dm: DeadMember = .{ .epoch = 3, .ts_realtime_ns = 99, .pr_key = 0xABCD, .early = true };
    try testing.expectEqual(dm, try DeadMemberWire.decode(&DeadMemberWire.init(dm).encode()));
    const dm2: DeadMember = .{ .epoch = 3, .ts_realtime_ns = 99, .pr_key = 0, .early = false };
    try testing.expectEqual(dm2, try DeadMemberWire.decode(&DeadMemberWire.init(dm2).encode()));
    var dmf = DeadMemberWire.init(dm).encode();
    dmf[2] = 3;
    try testing.expectError(error.Corrupt, DeadMemberWire.decode(&dmf));
    const rc: Recovered = .{ .by_term = 2, .ts_realtime_ns = 5 };
    try testing.expectEqual(rc, try RecoveredWire.decode(&RecoveredWire.init(rc).encode()));
    var rcr = RecoveredWire.init(rc).encode();
    rcr[5] = 1;
    try testing.expectError(error.Corrupt, RecoveredWire.decode(&rcr));
    try testing.expectEqual(@as(u64, 5), try decodeNodeSeqIncarnations(&encodeNodeSeqIncarnations(5)));
}

test "endpoint: a v4 and a v6 listener round-trip at 32 B keyed by appender id; none, a stray v4 tail, port 0 and a reserved word refuse" {
    var v4: Endpoint = .{ .family = EndpointRecordWire.FAMILY_IP4, .port = 39123, .addr = @splat(0), .pr_key = 0xC0FFEE };
    @memcpy(v4.addr[0..4], &[_]u8{ 10, 0, 0, 7 });
    const enc = EndpointRecordWire.init(v4).encode();
    try testing.expectEqual(EndpointRecordWire.LEN, enc.len);
    try testing.expectEqual(v4, try EndpointRecordWire.decode(&enc));
    var v6: Endpoint = .{ .family = EndpointRecordWire.FAMILY_IP6, .port = 1, .addr = @splat(0), .pr_key = 0 };
    v6.addr[0] = 0xfe;
    v6.addr[1] = 0x80;
    v6.addr[15] = 1;
    try testing.expectEqual(v6, try EndpointRecordWire.decode(&EndpointRecordWire.init(v6).encode()));
    var bad = enc;
    bad[2] = 0;
    try testing.expectError(error.Corrupt, EndpointRecordWire.decode(&bad));
    bad = enc;
    bad[12] = 1;
    try testing.expectError(error.Corrupt, EndpointRecordWire.decode(&bad));
    bad = enc;
    bad[4] = 0;
    bad[5] = 0;
    try testing.expectError(error.Corrupt, EndpointRecordWire.decode(&bad));
    bad = enc;
    bad[6] = 1;
    try testing.expectError(error.Corrupt, EndpointRecordWire.decode(&bad));
    try testing.expectError(error.Corrupt, EndpointRecordWire.decode(enc[0..31]));
    try testing.expectEqual(@as(usize, 6), endpointKey(1).len);
    try testing.expectEqual(@as(?u32, 0x0A0B0C0D), appenderOfKey(endpointKey(0x0A0B0C0D).slice()));
    try testing.expectEqual(@as(?Tag, .endpoint), tagOfKey(endpointKey(1).slice()));
    try testing.expect(!std.mem.eql(u8, endpointKey(1).slice()[0..2], extentGrantKey(1, 0).slice()[0..2]));
}

fn checkDecodersTotal(bytes: []const u8) !void {
    _ = tagOfKey(bytes);
    _ = slotOfKey(bytes);
    _ = identityOfKey(bytes);
    if (SlotStateWire.decode(bytes)) |s| {
        try testing.expectEqualSlices(u8, bytes, &s.encode());
    } else |_| {}
    if (decodeSlotTails(bytes)) |d| {
        try testing.expectEqual(bytes.len, SlotTailsHead.LEN + @as(usize, d.count) * 16 + @as(usize, if (d.spilled) 8 else 0));
    } else |_| {}
    const grant_appender: u32 = if (bytes.len >= 12) std.mem.readInt(u32, bytes[8..12], .little) else 0;
    const grant_id: u64 = if (bytes.len >= 24) std.mem.readInt(u64, bytes[16..24], .little) else 0;
    if (decodeGrantChunk(grant_appender, grant_id, bytes)) |c| switch (c) {
        .runs => |d| {
            var out: [32]GrantRun = undefined;
            if (d.n_runs <= out.len) {
                var buf: [48 + 32 * 16]u8 = undefined;
                const re = try encodeGrantChunk(grant_appender, grant_id, d.collect(&out), &buf);
                try testing.expectEqualSlices(u8, bytes, re);
            }
        },
        .marker => |mk| {
            var buf: [ExtentGrantChunkHead.LEN]u8 = undefined;
            try testing.expectEqualSlices(u8, bytes, try encodeGrantMarker(grant_appender, mk.lo, mk.hi, &buf));
        },
    } else |_| {}
    if (AppenderHintWire.decode(bytes)) |h| {
        try testing.expectEqualSlices(u8, bytes, &h.encode());
    } else |_| {}
    _ = CustodyQuarantineWire.decode(bytes) catch {};
    if (DirRenameWire.decode(bytes)) |d| try testing.expectEqualSlices(u8, bytes, &DirRenameWire.init(d).encode()) else |_| {}
    _ = decodeAllocLease(bytes) catch {};
    if (DeadMemberWire.decode(bytes)) |d| try testing.expectEqualSlices(u8, bytes, &DeadMemberWire.init(d).encode()) else |_| {}
    if (RecoveredWire.decode(bytes)) |r| try testing.expectEqualSlices(u8, bytes, &RecoveredWire.init(r).encode()) else |_| {}
    if (EndpointRecordWire.decode(bytes)) |e| try testing.expectEqualSlices(u8, bytes, &EndpointRecordWire.init(e).encode()) else |_| {}
    _ = decodeNodeSeqIncarnations(bytes) catch {};
}

fn decodersTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    try checkDecodersTotal(buf[0..len]);
}

fn fuzzSlice(comptime bytes: []const u8) []const u8 {
    const framed = comptime blk: {
        var len: [4]u8 = undefined;
        std.mem.writeInt(u32, &len, bytes.len, .little);
        break :blk len ++ bytes[0..bytes.len].*;
    };
    return &framed;
}

test "fuzz: every tree-0 decoder is total on arbitrary bytes and canonical where it accepts" {
    try fuzzkit.fuzz({}, decodersTotalFuzz, .{ .corpus = &.{
        fuzzSlice(&(SlotState{ .variant = .unleased, .g = 1, .root = .{ .addr = 2, .seq = 3 }, .cursor = 4 }).encode()),
        fuzzSlice(&(AppenderHint{ .ring_bytes = 1 }).encode()),
        fuzzSlice(&.{ 1, 0, 0, 0, 1, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0 }),
        fuzzSlice(&.{ 8, 1, 0, 7 }),
    } });
}

test "property: total under a PRNG, including mutations of valid encodings" {
    var prng = std.Random.DefaultPrng.init(0x5A17);
    const r = prng.random();
    var buf: [256]u8 = undefined;
    for (0..20_000) |_| {
        const len = r.intRangeAtMost(usize, 0, buf.len);
        r.bytes(buf[0..len]);
        try checkDecodersTotal(buf[0..len]);
    }
    var gbuf: [96]u8 = undefined;
    const grant = try encodeGrantChunk(3, 1, &.{ .{ .start = 1, .len = 2 }, .{ .start = 5, .len = 1 } }, &gbuf);
    var tbuf: [64]u8 = undefined;
    const tails = try encodeSlotTails(.{ .inline_tails = .{ .g = 1, .tails = &.{.{ .leaf_addr = 9, .tail = 1 }} } }, &tbuf);
    const valid = [_][]const u8{
        &(SlotState{ .variant = .leased, .appender_id = 1, .g = 2, .page_addr = 3, .root = .{ .addr = 4, .seq = 5 }, .cursor = 6 }).encode(),
        &(AppenderHint{ .ring_bytes = 7, .pending = .{ .appender_id = 1, .term = 1, .start = 1, .len = 1 } }).encode(),
        grant,
        tails,
        &DeadMemberWire.init(.{ .epoch = 1, .ts_realtime_ns = 2, .pr_key = 3, .early = false }).encode(),
    };
    for (0..5000) |_| {
        const src = valid[r.intRangeLessThan(usize, 0, valid.len)];
        @memcpy(buf[0..src.len], src);
        const flips = r.intRangeAtMost(usize, 1, 3);
        for (0..flips) |_| buf[r.intRangeLessThan(usize, 0, src.len)] ^= @as(u8, 1) << r.int(u3);
        try checkDecodersTotal(buf[0..src.len]);
        try checkDecodersTotal(buf[0..r.intRangeAtMost(usize, 0, src.len)]);
    }
}
