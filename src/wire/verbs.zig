//! Every verb of the cluster wire and every verb's request/reply payload
//! (charter §5.5 — the manager family, the slot verbs, the token block,
//! the cross-owner steps, the clone protocol, the allocation lease and
//! block grants, the death ledger, the membership plane, the record ship;
//! `docs/porting/sym_wire_membership_recovery.md` §1.6 and the sibling
//! specs' wire sections). Later waves write HANDLERS; the codecs live here
//! so the wire is ONE vocabulary.
//!
//! Numbers are BLOCK-coded so parallel stages never collide (the
//! reference's `0x0100` membership, `0x0300` publish, `0x0400` tokens,
//! `0x0500` manager with the documented low-byte codes); one `verb: u16`
//! per call — the reference's inner `ManagerCall` enum whose declaration
//! index was the identity is gone (decision 0002 row 222). Every manager
//! request names its `volume` ordinal first (one listener serves every
//! volume a node manages; a frame naming an ordinal the set lacks is
//! `STATUS_NOT_MANAGER`).
//!
//! Every payload is `codec.zig`'s shape: a padding-free `extern struct`
//! head + length-framed tails, every count checked against the bytes
//! present before an item is viewed, every decoder total and fuzzed. The
//! structural caps named here are CODEC caps (what a well-formed frame may
//! carry); the SCREEN against durable/derived state is `screen.zig`.
//!
//! Portable.

const std = @import("std");
const codec = @import("codec.zig");
const endpoint = @import("endpoint.zig");
const record = @import("../meta/record.zig");

pub const Error = codec.Error;
pub const Builder = codec.Builder;
pub const Cursor = codec.Cursor;
pub const EndpointWire = endpoint.EndpointWire;
pub const InodeValueWire = record.InodeValueWire;

// ---------------------------------------------------------------------------
// Vocabulary schemas (two-level versioning, §1.9): each block versions
// itself; the frame carries the block's number.
// ---------------------------------------------------------------------------

/// The wire's own vocabulary (handshake, ping, membership): 6 = the
/// reference's sixth bump, kept so a same-numbered foreign peer is told
/// "schema 6 ≠ 6-of-another-codec" by the framing version instead.
pub const CLUSTER_WIRE_SCHEMA: u32 = 6;
pub const MANAGER_SCHEMA: u32 = 1;
/// The cross-owner / record-ship block (the reference's meta-ship 3).
pub const META_SHIP_SCHEMA: u32 = 3;
pub const TOKEN_SCHEMA: u32 = 1;

pub const Block = enum(u8) {
    wire,
    membership,
    xv,
    token,
    manager,

    pub fn schema(self: Block) u32 {
        return switch (self) {
            .wire, .membership => CLUSTER_WIRE_SCHEMA,
            .xv => META_SHIP_SCHEMA,
            .token => TOKEN_SCHEMA,
            .manager => MANAGER_SCHEMA,
        };
    }
};

// ---------------------------------------------------------------------------
// The verb numbers.
// ---------------------------------------------------------------------------

pub const Verb = enum(u16) {
    ping = 0x0000,
    // ---- membership 0x0100 ----
    join = 0x0100,
    renew = 0x0101,
    leave = 0x0102,
    census = 0x0103,
    // ---- cross-owner steps + record ship 0x0300 ----
    xv_step = 0x0300,
    lookup_exact = 0x0301,
    xv_guards = 0x0302,
    xv_release = 0x0303,
    record_ship = 0x0304,
    free_blocks = 0x0305,
    // ---- tokens 0x0400 ----
    token_grant = 0x0400,
    token_recall = 0x0401,
    token_recall_ack = 0x0402,
    token_release = 0x0403,
    custody_grant = 0x0404,
    // ---- manager 0x0500 (low byte = the reference's documented code) ----
    join_appender = 0x0500,
    extent_grant = 0x0501,
    return_extents = 0x0502,
    acquire_slots = 0x0503,
    acquire_slot = 0x0504,
    offer_slot = 0x0505,
    release_slot = 0x0506,
    resolve_slot = 0x0507,
    dir_rename_lock = 0x0560,
    dir_rename_unlock = 0x0561,
    mark_shared = 0x0570,
    share_block = 0x0571,
    release_shared = 0x0572,
    block_grant = 0x0580,
    return_blocks = 0x0581,
    alloc_lease_acquire = 0x0582,
    alloc_lease_bitmap = 0x0583,
    alloc_lease_release = 0x0584,
    record_recovered = 0x0585,
    record_death = 0x0586,
    leave_appender = 0x05B0,
    publish_endpoint = 0x05B1,
    resolve_endpoint = 0x05B2,
    publish_roots = 0x05B3,
    grow_ring = 0x05B4,
    // ---- the handshake 0xFFF0 (MAC-less, the handshake class cap) ----
    challenge = 0xFFF0,
    prove = 0xFFF1,
    admitted = 0xFFF2,
    refused = 0xFFF3,

    pub const count = @typeInfo(Verb).@"enum".field_names.len;

    pub fn fromWire(w: u16) ?Verb {
        return std.enums.fromInt(Verb, w);
    }

    pub fn block(self: Verb) Block {
        const w = @backingInt(self);
        return switch (w >> 8) {
            0x00, 0xFF => .wire,
            0x01 => .membership,
            0x03 => .xv,
            0x04 => .token,
            0x05 => .manager,
            else => unreachable,
        };
    }

    pub fn schema(self: Verb) u32 {
        return self.block().schema();
    }

    pub fn isHandshake(self: Verb) bool {
        return @backingInt(self) >= 0xFFF0;
    }

    /// The ordinal in declaration order (the handler table's index).
    pub fn index(self: Verb) usize {
        inline for (@typeInfo(Verb).@"enum".field_values, 0..) |value, i| if (value == @backingInt(self)) return i;
        unreachable;
    }
};

// ---------------------------------------------------------------------------
// Codec caps (what a well-formed frame may carry).
// ---------------------------------------------------------------------------

/// The appender page's ring segment table (charter §5.5 "ring segment
/// table ≤ 8").
pub const RING_SEGMENTS_MAX: usize = 8;
/// `ShareBlock` is chunked at 512 refs per call (the reference's
/// `clone_share_blocks`; 512 × 16 B = 8 KiB per frame).
pub const SHARE_BLOCK_CHUNK: usize = 512;
/// A census page: ~120 B/row stays far inside the control cap.
pub const CENSUS_PAGE_MAX: usize = 1024;
/// The intent's step bound and the travelling-guard plan bound: `2 ×
/// MINT_SPREAD + 8 = 136` (the one-intent `rmdir` of a K = 64 directory).
pub const XV_GUARDS_MAX: usize = 136;
/// A mount point path (the OQ-5 collision refusal names it): PATH_MAX.
pub const MOUNT_PATH_MAX: usize = 4096;
/// The storage-trust peer id cap (an attacker-chosen field, bounded).
pub const PEER_ID_MAX: usize = 256;
/// A refusal reason (a log rendering).
pub const REASON_MAX: usize = 1024;
/// The largest record value a shipped record-level mutation carries
/// (charter §5.1's `min(64 KiB, node_size/4)` cap at its widest).
pub const RECORD_VALUE_MAX: usize = 64 << 10;
/// Any u16-counted array of 8..16 B items fits the 1 MiB control cap;
/// u32 counts are bounded by the bytes present (`takeArray`) and these.
pub const ROOTS_MAX: usize = 65536;
pub const TAILS_MAX: usize = 65536;
pub const OBJECTS_MAX: usize = 65536;

// ---------------------------------------------------------------------------
// Shared wire words.
// ---------------------------------------------------------------------------

/// The appender identity (24 B): `(node_token, mount_slot)` is the death
/// key; `writer_id` is the 64-bit incarnation hash and never binds.
pub const WireIdentity = extern struct {
    node_token: u64,
    writer_id: u64,
    mount_slot: u32,
    reserved: u32,

    pub const zero: WireIdentity = .{ .node_token = 0, .writer_id = 0, .mount_slot = 0, .reserved = 0 };

    pub fn isZero(self: WireIdentity) bool {
        return self.node_token == 0 and self.writer_id == 0 and self.mount_slot == 0;
    }

    /// The death key's identity: the pair, never the incarnation.
    pub fn sameMember(a: WireIdentity, b: WireIdentity) bool {
        return a.node_token == b.node_token and a.mount_slot == b.mount_slot;
    }

    /// The 12 B `peer_id` the storage-trust proof carries: `node_token BE
    /// ‖ mount_slot BE` (the text `node_{16hex}.m{8hex}` is a rendering).
    pub fn peerId(self: WireIdentity) [12]u8 {
        var out: [12]u8 = undefined;
        std.mem.writeInt(u64, out[0..8], self.node_token, .big);
        std.mem.writeInt(u32, out[8..12], self.mount_slot, .big);
        return out;
    }

    pub fn fromPeerId(id: []const u8) ?WireIdentity {
        if (id.len != 12) return null;
        return .{ .node_token = std.mem.readInt(u64, id[0..8], .big), .writer_id = 0, .mount_slot = std.mem.readInt(u32, id[8..12], .big), .reserved = 0 };
    }

    pub fn format(self: WireIdentity, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("node_{x:0>16}.m{x:0>8}", .{ self.node_token, self.mount_slot });
    }
};

/// A byte run (ring segments, bitmap regions).
pub const RunWire = extern struct { start: u64, len: u64 };
/// An extent / block run: `start` in extents (or blocks), `len` u32.
pub const GrantRunWire = extern struct { start: u64, len: u32, reserved: u32 };
pub const RootWire = extern struct { addr: u64, seq: u64 };

/// The slot's lease words a release carries (`ReleaseSlot`): root, cursor,
/// the seq floor, the tree's extent count.
pub const SlotWordsWire = extern struct {
    root: RootWire,
    cursor: u64,
    seq_floor: u64,
    slot_tree_extents: u32,
    reserved: u32,
};

/// `PublishRoots` row: a publication moves no floor.
pub const SlotRootWire = extern struct {
    root: RootWire,
    cursor: u64,
    slot_tree_extents: u32,
    g: u32,
    slot: u16,
    reserved: [6]u8,
};

pub const SlotGrantWire = extern struct {
    words: SlotWordsWire,
    g: u32,
    slot: u16,
    reserved: u16,
};

pub const LeafTailWire = extern struct { leaf_addr: u64, tail: u32, reserved: u32 };
pub const BlockRefWire = extern struct { owner_ino: u64, block_index: u32, reserved: u32 };
pub const BitmapRefWire = extern struct { start: u64, len: u64, vol: u16, reserved: [6]u8 };
pub const SlotLeaseAckWire = extern struct { g: u32, slot: u16, reserved: u16 };
pub const OfferedSlotWire = extern struct { slot: u16, reserved: u16, holder: u32 };

/// `{already}` — the reply of every idempotent verb that has nothing else
/// to say (Left, Published, Released, Recorded, Offered, Unlocked, …).
pub const Ack = extern struct {
    already: u8,
    reserved: [7]u8,

    pub const fresh: Ack = .{ .already = 0, .reserved = @splat(0) };
    pub const replay: Ack = .{ .already = 1, .reserved = @splat(0) };

    pub fn check(self: Ack) bool {
        return self.already <= 1;
    }
};

pub const Empty = extern struct {};

comptime {
    std.debug.assert(codec.wireSize(WireIdentity) == 24);
    std.debug.assert(codec.wireSize(RunWire) == 16);
    std.debug.assert(codec.wireSize(GrantRunWire) == 16);
    std.debug.assert(codec.wireSize(SlotWordsWire) == 40);
    std.debug.assert(codec.wireSize(SlotRootWire) == 40);
    std.debug.assert(codec.wireSize(SlotGrantWire) == 48);
    std.debug.assert(codec.wireSize(BitmapRefWire) == 24);
    std.debug.assert(codec.wireSize(Ack) == 8);
    std.debug.assert(codec.wireSize(Empty) == 0);
}

/// A head + one counted array tail: the shape most verbs take.
fn HeadWithArray(comptime HeadT: type, comptime Item: type, comptime count_field: []const u8, comptime cap: usize) type {
    return struct {
        const Self = @This();
        pub const Head = HeadT;
        pub const ItemT = Item;
        head: Head,
        items: []align(1) const Item,

        pub fn encode(self: Self, b: *Builder) codec.BuildError!void {
            std.debug.assert(@field(self.head, count_field) == self.items.len);
            try b.put(Head, self.head);
            try b.putView(Item, self.items);
        }

        pub fn decode(c: *Cursor) Error!Self {
            const head = try c.take(Head);
            const n = try codec.countAtMost(@field(head, count_field), cap);
            return .{ .head = head, .items = try c.takeArray(Item, n) };
        }
    };
}

/// A head + one length-framed byte tail padded to 8.
fn HeadWithBytes(comptime HeadT: type, comptime len_field: []const u8, comptime cap: usize) type {
    return struct {
        const Self = @This();
        pub const Head = HeadT;
        head: Head,
        bytes: []const u8,

        pub fn encode(self: Self, b: *Builder) codec.BuildError!void {
            std.debug.assert(@field(self.head, len_field) == self.bytes.len);
            try b.put(Head, self.head);
            try b.putBytes(self.bytes);
            try b.putPadding(self.bytes.len, 8);
        }

        pub fn decode(c: *Cursor) Error!Self {
            const head = try c.take(Head);
            const n = try codec.countAtMost(@field(head, len_field), cap);
            const bytes = try c.takeBytes(n);
            try c.takePadding(n, 8);
            return .{ .head = head, .bytes = bytes };
        }
    };
}

// ---------------------------------------------------------------------------
// Manager — region.
// ---------------------------------------------------------------------------

pub const JoinAppenderReq = extern struct {
    volume: u16,
    reserved: u16,
    reserved2: u32,
    identity: WireIdentity,
    /// 0 = derived; else clamped to [floor, ceiling] by the manager.
    ring_want_bytes: u64,
};

pub const JoinedHead = extern struct {
    appender_id: u32,
    already: u8,
    n_segments: u8,
    n_grant: u8,
    reserved: u8,
    page_addr: u64,
    /// Screened by the JOINER (`screen.joinedNodeSeqBase`).
    node_seq_base: u64,
    ring_bytes: u64,

    pub fn check(self: JoinedHead) bool {
        return self.already <= 1 and self.n_segments <= RING_SEGMENTS_MAX;
    }
};

pub const Joined = struct {
    head: JoinedHead,
    segments: []align(1) const RunWire,
    grant: []align(1) const GrantRunWire,

    pub fn encode(self: Joined, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.n_segments == self.segments.len and self.head.n_grant == self.grant.len);
        try b.put(JoinedHead, self.head);
        try b.putView(RunWire, self.segments);
        try b.putView(GrantRunWire, self.grant);
    }

    pub fn decode(c: *Cursor) Error!Joined {
        const head = try c.take(JoinedHead);
        const segments = try c.takeArray(RunWire, head.n_segments);
        const grant = try c.takeArray(GrantRunWire, head.n_grant);
        return .{ .head = head, .segments = segments, .grant = grant };
    }
};

pub const ExtentGrantReq = extern struct {
    volume: u16,
    reserved: u16,
    appender_id: u32,
    /// 0 = the derived size; explicit ≤ the manager's cap (clamped).
    want: u32,
    reserved2: u32,
};

pub const GrantedHead = extern struct { n: u16, reserved: [6]u8 };
pub const Granted = HeadWithArray(GrantedHead, GrantRunWire, "n", std.math.maxInt(u16));

pub const ReturnExtentsHead = extern struct { volume: u16, n_runs: u16, appender_id: u32, reserved: u64 };
/// Runs travel as RUNS and are intersected with the record run by run.
pub const ReturnExtentsReq = HeadWithArray(ReturnExtentsHead, GrantRunWire, "n_runs", std.math.maxInt(u16));
pub const Returned = extern struct { cleared: u64, already: u64 };

pub const LeaveAppenderHead = extern struct {
    volume: u16,
    n_unclaimed: u16,
    appender_id: u32,
    identity: WireIdentity,
    reserved: u64,
};
pub const LeaveAppenderReq = HeadWithArray(LeaveAppenderHead, GrantRunWire, "n_unclaimed", std.math.maxInt(u16));

pub const PublishEndpointReq = extern struct {
    volume: u16,
    reserved: u16,
    appender_id: u32,
    identity: WireIdentity,
    pr_key: u64,
    endpoint: EndpointWire,
    reserved2: [4]u8,
};

pub const ResolveEndpointReq = extern struct { volume: u16, reserved: u16, appender_id: u32 };

pub const EndpointReply = extern struct {
    present: u8,
    reserved: [3]u8,
    endpoint: EndpointWire,

    pub fn check(self: EndpointReply) bool {
        return self.present <= 1 and (self.present == 1 or self.endpoint.isNone());
    }
};

pub const PublishRootsHead = extern struct { volume: u16, reserved: u16, appender_id: u32, n: u32, reserved2: u32 };
pub const PublishRootsReq = HeadWithArray(PublishRootsHead, SlotRootWire, "n", ROOTS_MAX);
pub const RootsPublished = extern struct { published: u32, already: u32 };

pub const GrowRingReq = extern struct { volume: u16, reserved: u16, appender_id: u32, want_bytes: u64 };

pub const RingGrown = extern struct {
    present: u8,
    reserved: [7]u8,
    segment: RunWire,

    pub fn check(self: RingGrown) bool {
        return self.present <= 1 and (self.present == 1 or (self.segment.start == 0 and self.segment.len == 0));
    }
};

// ---------------------------------------------------------------------------
// Manager — slots.
// ---------------------------------------------------------------------------

pub const AcquireSlotsReq = extern struct { volume: u16, want: u16, appender_id: u32 };
pub const AcquireSlotReq = extern struct { volume: u16, slot: u16, appender_id: u32 };

pub const AcquireKind = enum(u8) { granted = 0, refused = 1 };

pub const AcquireHead = extern struct {
    kind: u8,
    reserved: u8,
    n: u16,
    already: u16,
    /// `refused`: the slot another appender leases, its holder and `g`.
    slot: u16,
    holder: u32,
    g: u32,

    pub fn check(self: AcquireHead) bool {
        return self.kind <= 1 and (self.kind == 0 or self.n == 0);
    }
};
pub const AcquireReply = HeadWithArray(AcquireHead, SlotGrantWire, "n", std.math.maxInt(u16));

pub const OfferSlotReq = extern struct { volume: u16, slot: u16, appender_id: u32, to: u32, reserved: u32 };

pub const ReleaseSlotHead = extern struct {
    volume: u16,
    slot: u16,
    appender_id: u32,
    g: u32,
    n_tails: u32,
    words: SlotWordsWire,
};
pub const ReleaseSlotReq = HeadWithArray(ReleaseSlotHead, LeafTailWire, "n_tails", TAILS_MAX);

pub const ResolveSlotReq = extern struct { volume: u16, slot: u16, reserved: u32 };

pub const SlotHolderKind = enum(u8) { holder = 0, unleased = 1 };

pub const SlotHolderReply = extern struct {
    kind: u8,
    reserved: [3]u8,
    appender_id: u32,
    g: u32,
    reserved2: u32,

    pub fn check(self: SlotHolderReply) bool {
        return self.kind <= 1;
    }
};

// ---------------------------------------------------------------------------
// Manager — the set-wide dir_rename lease (volume 0 only).
// ---------------------------------------------------------------------------

pub const DirRenameReq = extern struct { volume: u16, reserved: u16, appender_id: u32 };

pub const DirRenameKind = enum(u8) { locked = 0, busy = 1 };

pub const DirRenameReply = extern struct {
    kind: u8,
    already: u8,
    reserved: u16,
    holder: u32,

    pub fn check(self: DirRenameReply) bool {
        return self.kind <= 1 and self.already <= 1;
    }
};

// ---------------------------------------------------------------------------
// Manager — the clone protocol.
// ---------------------------------------------------------------------------

pub const MarkSharedReq = extern struct {
    volume: u16,
    reserved: u16,
    block_index: u32,
    vol_tag: u64,
    block_idx: u64,
    owner_ino: u64,
};

pub const MarkKind = enum(u8) { marked = 0, gone = 1 };

pub const MarkReply = extern struct {
    kind: u8,
    already: u8,
    reserved: [6]u8,

    pub fn check(self: MarkReply) bool {
        return self.kind <= 1 and self.already <= 1;
    }
};

pub const ShareBlockHead = extern struct {
    volume: u16,
    n: u16,
    reserved: u32,
    vol_tag: u64,
    block_idx: u64,

    pub fn check(self: ShareBlockHead) bool {
        return self.n <= SHARE_BLOCK_CHUNK;
    }
};
pub const ShareBlockReq = HeadWithArray(ShareBlockHead, BlockRefWire, "n", SHARE_BLOCK_CHUNK);
pub const Shared = extern struct { inserted: u32, already: u32 };

pub const ReleaseSharedReq = extern struct {
    volume: u16,
    present: u8,
    reserved: u8,
    reserved2: u32,
    vol_tag: u64,
    block_idx: u64,
    owner: BlockRefWire,

    pub fn check(self: ReleaseSharedReq) bool {
        return self.present <= 1;
    }
};

pub const SharedReleasedKind = enum(u8) { not_shared = 0, held = 1, freed = 2 };

pub const SharedReleased = extern struct {
    kind: u8,
    reserved: [3]u8,
    remaining: u32,

    pub fn check(self: SharedReleased) bool {
        return self.kind <= 2;
    }
};

// ---------------------------------------------------------------------------
// Manager — allocation lease, block grants, the death ledger.
// ---------------------------------------------------------------------------

pub const BlockGrantReq = extern struct {
    volume: u16,
    reserved: u16,
    /// 0 = the holder's derivation `G`.
    want: u32,
    writer: WireIdentity,
    vol_tag: u64,
    held_unconsumed: u64,
};

pub const BlocksGrantedKind = enum(u8) { granted = 0, full = 1 };

pub const BlocksGrantedHead = extern struct {
    kind: u8,
    already: u8,
    n: u16,
    reserved: u32,

    pub fn check(self: BlocksGrantedHead) bool {
        return self.kind <= 1 and self.already <= 1 and (self.kind == 0 or self.n == 0);
    }
};
pub const BlocksGranted = HeadWithArray(BlocksGrantedHead, GrantRunWire, "n", std.math.maxInt(u16));

pub const ReturnBlocksReq = extern struct {
    volume: u16,
    reserved: u16,
    len: u32,
    writer: WireIdentity,
    vol_tag: u64,
    start: u64,
};

pub const BlocksReturned = extern struct {
    present: u8,
    reserved: [7]u8,
    cleared: u64,

    pub fn check(self: BlocksReturned) bool {
        return self.present <= 1;
    }
};

pub const AllocLeaseAcquireReq = extern struct {
    volume: u16,
    home_vol: u16,
    appender_id: u32,
    identity: WireIdentity,
    vol_tag: u64,
    blocks: u64,
};

pub const AllocLeaseGrantedHead = extern struct {
    already: u8,
    reserved: u8,
    n_bitmap: u16,
    reserved2: u32,
    term: u64,
    predecessor_blocks: u64,

    pub fn check(self: AllocLeaseGrantedHead) bool {
        return self.already <= 1;
    }
};
pub const AllocLeaseGranted = HeadWithArray(AllocLeaseGrantedHead, BitmapRefWire, "n_bitmap", std.math.maxInt(u16));

pub const AllocLeaseBitmapHead = extern struct {
    volume: u16,
    n: u16,
    reserved: u32,
    identity: WireIdentity,
    vol_tag: u64,
    term: u64,
};
pub const AllocLeaseBitmapReq = HeadWithArray(AllocLeaseBitmapHead, BitmapRefWire, "n", std.math.maxInt(u16));

pub const AllocLeaseReleaseReq = extern struct {
    volume: u16,
    reserved: u16,
    reserved2: u32,
    identity: WireIdentity,
    vol_tag: u64,
    term: u64,
};

pub const RecordDeathReq = extern struct {
    volume: u16,
    reserved: u16,
    reserved2: u32,
    member: WireIdentity,
    epoch: u64,
    /// The dead member's registrant key (0 = none).
    pr_key: u64,
};

pub const RecordRecoveredReq = extern struct {
    volume: u16,
    vol: u16,
    reserved: u32,
    member: WireIdentity,
};

// ---------------------------------------------------------------------------
// Membership (the home shard's owner).
// ---------------------------------------------------------------------------

pub const Role = enum(u8) { writer = 0, reader = 1 };

pub const JOIN_FLAG_HAS_PRIOR_EPOCH: u8 = 1 << 0;
pub const JOIN_FLAG_HAS_ENDPOINT: u8 = 1 << 1;

pub const JoinHead = extern struct {
    role: u8,
    flags: u8,
    mount_len: u16,
    pid: u32,
    /// A writer's `node_token BE ‖ mount_slot BE ‖ 0⁴`; a reader's 16
    /// random bytes (readers hold no region).
    id: [16]u8,
    boot: [16]u8,
    endpoint: EndpointWire,
    reserved: [4]u8,
    prior_epoch: u64,
    pr_key: u64,

    pub fn check(self: JoinHead) bool {
        if (self.role > 1 or self.flags & ~(JOIN_FLAG_HAS_PRIOR_EPOCH | JOIN_FLAG_HAS_ENDPOINT) != 0) return false;
        if (self.flags & JOIN_FLAG_HAS_ENDPOINT == 0 and !self.endpoint.isNone()) return false;
        if (self.flags & JOIN_FLAG_HAS_PRIOR_EPOCH == 0 and self.prior_epoch != 0) return false;
        return true;
    }
};
pub const JoinReq = HeadWithBytes(JoinHead, "mount_len", MOUNT_PATH_MAX);

pub const GrantHead = extern struct {
    epoch: u64,
    term: u64,
    granted_at_owner_ms: u64,
    /// The reclaim's acknowledged prior epoch (0 = none).
    ack_prior_epoch: u64,
    t_owner_ms: u32,
    skew_max_ms: u32,
    d_purge_ms: u32,
    renew_ms: u32,
    checkpoint_ceiling_ms: u32,
    n_ack: u16,
    n_release: u16,
    n_offered: u16,
    reserved: [6]u8,
};

/// The membership grant: the lease clocks, the slot-lease carriage.
pub const Grant = struct {
    head: GrantHead,
    slot_leases_ack: []align(1) const SlotLeaseAckWire,
    slot_release_notices: []align(1) const u16,
    offered_slots: []align(1) const OfferedSlotWire,

    pub fn encode(self: Grant, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.n_ack == self.slot_leases_ack.len and self.head.n_release == self.slot_release_notices.len and self.head.n_offered == self.offered_slots.len);
        try b.put(GrantHead, self.head);
        try b.putView(SlotLeaseAckWire, self.slot_leases_ack);
        try b.putView(u16, self.slot_release_notices);
        try b.putPadding(self.slot_release_notices.len * 2, 8);
        try b.putView(OfferedSlotWire, self.offered_slots);
    }

    pub fn decode(c: *Cursor) Error!Grant {
        const head = try c.take(GrantHead);
        const ack = try c.takeArray(SlotLeaseAckWire, head.n_ack);
        const rel = try c.takeArray(u16, head.n_release);
        try c.takePadding(@as(usize, head.n_release) * 2, 8);
        const off = try c.takeArray(OfferedSlotWire, head.n_offered);
        return .{ .head = head, .slot_leases_ack = ack, .slot_release_notices = rel, .offered_slots = off };
    }
};

pub const WindowDeclHead = extern struct { vol_tag: u64, n_ranges: u16, reserved: [6]u8 };

/// One data volume's declared grant windows (the renewal's carriage).
pub const WindowDecl = struct { vol_tag: u64, ranges: []align(1) const GrantRunWire };

/// Nested counted sequences are validated whole at decode and re-walked
/// by an iterator (no allocation; the frame bytes are the storage).
pub const WindowIterator = struct {
    c: Cursor,
    left: usize,

    pub fn next(self: *WindowIterator) ?WindowDecl {
        if (self.left == 0) return null;
        self.left -= 1;
        // Validated at decode: these cannot fail.
        const head = self.c.take(WindowDeclHead) catch unreachable;
        const ranges = self.c.takeArray(GrantRunWire, head.n_ranges) catch unreachable;
        return .{ .vol_tag = head.vol_tag, .ranges = ranges };
    }
};

pub const RenewHead = extern struct {
    id: [16]u8,
    epoch: u64,
    acked_free_epoch: u64,
    n_windows: u16,
    reserved: [6]u8,
};

pub const RenewReq = struct {
    head: RenewHead,
    windows_bytes: []const u8,

    pub fn encodeWindows(b: *Builder, decls: []const WindowDecl) codec.BuildError!void {
        for (decls) |d| {
            try b.put(WindowDeclHead, .{ .vol_tag = d.vol_tag, .n_ranges = @intCast(d.ranges.len), .reserved = @splat(0) });
            try b.putView(GrantRunWire, d.ranges);
        }
    }

    pub fn encode(self: RenewReq, b: *Builder) codec.BuildError!void {
        try b.put(RenewHead, self.head);
        try b.putBytes(self.windows_bytes);
    }

    pub fn decode(c: *Cursor) Error!RenewReq {
        const head = try c.take(RenewHead);
        const start = c.pos;
        var i: usize = 0;
        while (i < head.n_windows) : (i += 1) {
            const wh = try c.take(WindowDeclHead);
            _ = try c.takeArray(GrantRunWire, wh.n_ranges);
        }
        return .{ .head = head, .windows_bytes = c.bytes[start..c.pos] };
    }

    pub fn windows(self: RenewReq) WindowIterator {
        return .{ .c = .init(self.windows_bytes), .left = self.head.n_windows };
    }
};

pub const LeaveReq = extern struct { id: [16]u8 };

pub const CensusReq = extern struct { cursor: u64, limit: u32, reserved: u32 };

pub const MemberState = enum(u8) { live = 0, grace = 1, departed = 2 };

pub const MemberSnapshotHead = extern struct {
    id: [16]u8,
    boot: [16]u8,
    endpoint: EndpointWire,
    role: u8,
    state: u8,
    mount_len: u16,
    pid: u32,
    reserved: u32,
    epoch: u64,
    age_ms: u64,
    expires_in_ms: u64,
    acked_free_epoch: u64,

    pub fn check(self: MemberSnapshotHead) bool {
        return self.role <= 1 and self.state <= 2 and self.mount_len <= MOUNT_PATH_MAX;
    }
};

pub const MemberSnapshot = struct { head: MemberSnapshotHead, mount: []const u8 };

pub const MemberIterator = struct {
    c: Cursor,
    left: usize,

    pub fn next(self: *MemberIterator) ?MemberSnapshot {
        if (self.left == 0) return null;
        self.left -= 1;
        const head = self.c.take(MemberSnapshotHead) catch unreachable;
        const mount = self.c.takeBytes(head.mount_len) catch unreachable;
        self.c.takePadding(head.mount_len, 8) catch unreachable;
        return .{ .head = head, .mount = mount };
    }
};

pub const CensusPageHead = extern struct {
    n: u16,
    has_next: u8,
    reserved: [5]u8,
    next: u64,

    pub fn check(self: CensusPageHead) bool {
        return self.n <= CENSUS_PAGE_MAX and self.has_next <= 1 and (self.has_next == 1 or self.next == 0);
    }
};

pub const CensusPage = struct {
    head: CensusPageHead,
    rows_bytes: []const u8,

    pub fn encodeRow(b: *Builder, row: MemberSnapshot) codec.BuildError!void {
        std.debug.assert(row.head.mount_len == row.mount.len);
        try b.put(MemberSnapshotHead, row.head);
        try b.putBytes(row.mount);
        try b.putPadding(row.mount.len, 8);
    }

    pub fn encode(self: CensusPage, b: *Builder) codec.BuildError!void {
        try b.put(CensusPageHead, self.head);
        try b.putBytes(self.rows_bytes);
    }

    pub fn decode(c: *Cursor) Error!CensusPage {
        const head = try c.take(CensusPageHead);
        const start = c.pos;
        var i: usize = 0;
        while (i < head.n) : (i += 1) {
            const rh = try c.take(MemberSnapshotHead);
            _ = try c.takeBytes(rh.mount_len);
            try c.takePadding(rh.mount_len, 8);
        }
        return .{ .head = head, .rows_bytes = c.bytes[start..c.pos] };
    }

    pub fn rows(self: CensusPage) MemberIterator {
        return .{ .c = .init(self.rows_bytes), .left = self.head.n };
    }
};

// ---------------------------------------------------------------------------
// Tokens (the holder's GPFS-strict read tokens; `client_id` = the member's
// 64-bit identity word).
// ---------------------------------------------------------------------------

pub const WANT_DENTRIES: u8 = 1 << 0;
pub const WANT_RECORDS: u8 = 1 << 1;

pub const TokenGrantHead = extern struct {
    volume: u16,
    wants: u8,
    /// 0 = Read, the only wire mode (the lessee needs no token on its own tree).
    mode: u8,
    xattr_after_len: u32,
    client_id: u64,
    object: u64,
    after: u64,

    pub fn check(self: TokenGrantHead) bool {
        return self.wants & ~(WANT_DENTRIES | WANT_RECORDS) == 0 and self.mode == 0 and self.xattr_after_len <= record.NAME_MAX;
    }
};
pub const TokenGrantReq = HeadWithBytes(TokenGrantHead, "xattr_after_len", record.NAME_MAX);

pub const GrantPageKind = enum(u8) { granted = 0, not_holder = 1, gone = 2, holder_dead = 3 };
pub const PAGE_XATTRS_COMPLETE: u8 = 1 << 0;
pub const PAGE_HAS_DIR: u8 = 1 << 1;
pub const PAGE_DIR_COMPLETE: u8 = 1 << 2;
pub const PAGE_ALREADY: u8 = 1 << 3;

pub const GrantPageHead = extern struct {
    kind: u8,
    flags: u8,
    reserved: u16,
    n_xattrs: u32,
    n_dir: u32,
    /// `not_holder` / `holder_dead`: the lessee the redirect names.
    holder: u32,
    attrs: InodeValueWire,
    reserved2: [4]u8,

    pub fn check(self: GrantPageHead) bool {
        return self.kind <= 3 and self.flags & ~(PAGE_XATTRS_COMPLETE | PAGE_HAS_DIR | PAGE_DIR_COMPLETE | PAGE_ALREADY) == 0;
    }
};

pub const XattrHead = extern struct {
    name_len: u16,
    reserved: u16,
    value_len: u32,

    pub fn check(self: XattrHead) bool {
        return self.name_len <= record.NAME_MAX and self.value_len <= RECORD_VALUE_MAX;
    }
};
pub const XattrEntry = struct { name: []const u8, value: []const u8 };

pub const DirRecordHead = extern struct {
    cookie: u64,
    child_ino: u64,
    file_type: u8,
    name_len: u8,
    reserved: [6]u8,
};
pub const DirEntry = struct { head: DirRecordHead, name: []const u8 };

pub const XattrIterator = struct {
    c: Cursor,
    left: usize,
    pub fn next(self: *XattrIterator) ?XattrEntry {
        if (self.left == 0) return null;
        self.left -= 1;
        const h = self.c.take(XattrHead) catch unreachable;
        const name = self.c.takeBytes(h.name_len) catch unreachable;
        const value = self.c.takeBytes(h.value_len) catch unreachable;
        self.c.takePadding(@as(usize, h.name_len) + h.value_len, 8) catch unreachable;
        return .{ .name = name, .value = value };
    }
};

pub const DirIterator = struct {
    c: Cursor,
    left: usize,
    pub fn next(self: *DirIterator) ?DirEntry {
        if (self.left == 0) return null;
        self.left -= 1;
        const h = self.c.take(DirRecordHead) catch unreachable;
        const name = self.c.takeBytes(h.name_len) catch unreachable;
        self.c.takePadding(h.name_len, 8) catch unreachable;
        return .{ .head = h, .name = name };
    }
};

/// One token grant page: the folded inode, the carried xattrs, the
/// dentry page — under ONE byte budget (the control cap).
pub const GrantPage = struct {
    head: GrantPageHead,
    xattrs_bytes: []const u8,
    dir_bytes: []const u8,

    pub fn encodeXattr(b: *Builder, e: XattrEntry) codec.BuildError!void {
        try b.put(XattrHead, .{ .name_len = @intCast(e.name.len), .reserved = 0, .value_len = @intCast(e.value.len) });
        try b.putBytes(e.name);
        try b.putBytes(e.value);
        try b.putPadding(e.name.len + e.value.len, 8);
    }

    pub fn encodeDir(b: *Builder, e: DirEntry) codec.BuildError!void {
        std.debug.assert(e.head.name_len == e.name.len);
        try b.put(DirRecordHead, e.head);
        try b.putBytes(e.name);
        try b.putPadding(e.name.len, 8);
    }

    pub fn encode(self: GrantPage, b: *Builder) codec.BuildError!void {
        try b.put(GrantPageHead, self.head);
        try b.putBytes(self.xattrs_bytes);
        try b.putBytes(self.dir_bytes);
    }

    pub fn decode(c: *Cursor) Error!GrantPage {
        const head = try c.take(GrantPageHead);
        const xs = c.pos;
        var i: usize = 0;
        while (i < head.n_xattrs) : (i += 1) {
            const h = try c.take(XattrHead);
            _ = try c.takeBytes(@as(usize, h.name_len) + h.value_len);
            try c.takePadding(@as(usize, h.name_len) + h.value_len, 8);
        }
        const ds = c.pos;
        i = 0;
        while (i < head.n_dir) : (i += 1) {
            const h = try c.take(DirRecordHead);
            _ = try c.takeBytes(h.name_len);
            try c.takePadding(h.name_len, 8);
        }
        return .{ .head = head, .xattrs_bytes = c.bytes[xs..ds], .dir_bytes = c.bytes[ds..c.pos] };
    }

    pub fn xattrs(self: GrantPage) XattrIterator {
        return .{ .c = .init(self.xattrs_bytes), .left = self.head.n_xattrs };
    }
    pub fn dir(self: GrantPage) DirIterator {
        return .{ .c = .init(self.dir_bytes), .left = self.head.n_dir };
    }
};

pub const TokenRecallReq = extern struct { volume: u16, reserved: u16, wait_ms: u32, client_id: u64 };

pub const RecallFrameHead = extern struct { frame_id: u64, n: u32, reserved: u32 };
/// `frame_id 0 ∧ n 0` = "the park elapsed, nothing to recall".
pub const RecallFrame = HeadWithArray(RecallFrameHead, u64, "n", OBJECTS_MAX);

pub const TokenRecallAckReq = extern struct { volume: u16, reserved: u16, reserved2: u32, client_id: u64, frame_id: u64 };

pub const TokenReleaseHead = extern struct { volume: u16, n: u16, reserved: u32, client_id: u64 };
pub const TokenReleaseReq = HeadWithArray(TokenReleaseHead, u64, "n", OBJECTS_MAX);
pub const Released = extern struct { count: u32, reserved: u32 };

pub const CustodyGrantReq = extern struct {
    volume: u16,
    concurrent_write: u8,
    reserved: u8,
    wait_ms: u32,
    client_id: u64,
    object: u64,
    span_start: u64,
    span_len: u64,
    lease_epoch: u64,
    reserved2: u64,

    pub fn check(self: CustodyGrantReq) bool {
        return self.concurrent_write <= 1;
    }
};

pub const CustodyKind = enum(u8) { granted = 0, refused = 1 };

pub const CustodyReplyHead = extern struct {
    kind: u8,
    reserved: [3]u8,
    /// The custody status word on `refused` (the data plane's).
    status: u32,
    grant_epoch: u64,
    page_len: u32,
    reserved2: u32,

    pub fn check(self: CustodyReplyHead) bool {
        return self.kind <= 1;
    }
};

/// The custody grant's reply CARRIES a token page (§1.3 step G5).
pub const CustodyReply = struct {
    head: CustodyReplyHead,
    page_bytes: []const u8,

    pub fn encode(self: CustodyReply, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.page_len == self.page_bytes.len);
        try b.put(CustodyReplyHead, self.head);
        try b.putBytes(self.page_bytes);
        try b.putPadding(self.page_bytes.len, 8);
    }

    pub fn decode(c: *Cursor) Error!CustodyReply {
        const head = try c.take(CustodyReplyHead);
        const bytes = try c.takeBytes(head.page_len);
        try c.takePadding(head.page_len, 8);
        if (bytes.len != 0) _ = try codec.decodePayload(GrantPage, bytes);
        return .{ .head = head, .page_bytes = bytes };
    }

    pub fn page(self: CustodyReply) ?GrantPage {
        if (self.page_bytes.len == 0) return null;
        return codec.decodePayload(GrantPage, self.page_bytes) catch unreachable;
    }
};

// ---------------------------------------------------------------------------
// Cross-owner steps, travelling guards, the record ship, shipped frees.
// ---------------------------------------------------------------------------

pub const StepKind = enum(u8) { remove_dentry = 1, insert_dentry = 2, set_nlink = 3, touch_ctime = 4, mint_inode = 5, create_inode = 6 };
pub const ParentUpdate = enum(u8) { none = 0, shared_times = 1, exclusive_times = 2, exclusive_times_bump = 3 };
pub const STEP_FLAG_HAS_CTIME: u8 = 1 << 0;

/// The intent step (48 B; `sym_tokens_intents_striping.md` §2.1): the
/// same image the initiator's intent record carries, so a shipped step
/// and a replayed one decode through ONE codec.
pub const XvStepWire = extern struct {
    kind: u8,
    parent_update: u8,
    name_len: u8,
    flags: u8,
    ft_bits: u32,
    a: u64,
    b: u64,
    c: u64,
    w0: u32,
    w1: u32,
    w2: u32,
    w3: u32,

    pub fn check(self: XvStepWire) bool {
        if (self.kind < 1 or self.kind > 6 or self.parent_update > 3 or self.flags & ~STEP_FLAG_HAS_CTIME != 0) return false;
        // Dentry kinds carry a name; the others must not.
        const dentry = self.kind == 1 or self.kind == 2;
        if (!dentry and self.name_len != 0) return false;
        if (dentry and self.name_len == 0) return false;
        return true;
    }
};

pub const XvStepHead = extern struct {
    volume: u16,
    step_idx: u16,
    reserved: u32,
    tx_id: u64,
    /// 0 = no guard scope travelled.
    scope: u64,
    step: XvStepWire,
};

pub const XvStepReq = struct {
    head: XvStepHead,
    name: []const u8,

    pub fn encode(self: XvStepReq, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.step.name_len == self.name.len);
        try b.put(XvStepHead, self.head);
        try b.putBytes(self.name);
        try b.putPadding(self.name.len, 8);
    }

    pub fn decode(c: *Cursor) Error!XvStepReq {
        const head = try c.take(XvStepHead);
        const name = try c.takeBytes(head.step.name_len);
        try c.takePadding(head.step.name_len, 8);
        return .{ .head = head, .name = name };
    }
};

pub const StepStatus = enum(u8) { applied = 0, already_applied = 1, foreign_skipped = 2 };

pub const StepOutcome = extern struct {
    status: u8,
    has_inode: u8,
    reserved: [6]u8,
    inode: InodeValueWire,
    reserved2: [4]u8,

    pub fn check(self: StepOutcome) bool {
        return self.status <= 2 and self.has_inode <= 1;
    }
};

pub const LookupExactHead = extern struct {
    volume: u16,
    name_len: u8,
    reserved: u8,
    reserved2: u32,
    parent: u64,

    pub fn check(self: LookupExactHead) bool {
        return self.name_len != 0;
    }
};
pub const LookupExactReq = HeadWithBytes(LookupExactHead, "name_len", record.NAME_MAX);

pub const DentryExact = extern struct {
    present: u8,
    file_type: u8,
    reserved: [6]u8,
    ino: u64,

    pub fn check(self: DentryExact) bool {
        return self.present <= 1 and (self.present == 1 or (self.ino == 0 and self.file_type == 0));
    }
};

pub const GuardInodeWire = extern struct {
    ino: u64,
    exclusive: u8,
    reserved: [7]u8,

    pub fn check(self: GuardInodeWire) bool {
        return self.exclusive <= 1;
    }
};

pub const GuardDentryHead = extern struct {
    parent: u64,
    exclusive: u8,
    name_len: u8,
    reserved: [6]u8,

    pub fn check(self: GuardDentryHead) bool {
        return self.exclusive <= 1 and self.name_len != 0;
    }
};
pub const GuardDentry = struct { head: GuardDentryHead, name: []const u8 };

pub const GuardDentryIterator = struct {
    c: Cursor,
    left: usize,
    pub fn next(self: *GuardDentryIterator) ?GuardDentry {
        if (self.left == 0) return null;
        self.left -= 1;
        const h = self.c.take(GuardDentryHead) catch unreachable;
        const name = self.c.takeBytes(h.name_len) catch unreachable;
        self.c.takePadding(h.name_len, 8) catch unreachable;
        return .{ .head = h, .name = name };
    }
};

pub const XvGuardsHead = extern struct {
    volume: u16,
    n_inodes: u16,
    n_dentries: u16,
    reserved: u16,
    scope: u64,

    pub fn check(self: XvGuardsHead) bool {
        return @as(usize, self.n_inodes) + self.n_dentries <= XV_GUARDS_MAX and self.scope != 0;
    }
};

/// One travelling-guard set for one holder's table.
pub const XvGuardsReq = struct {
    head: XvGuardsHead,
    inodes: []align(1) const GuardInodeWire,
    dentries_bytes: []const u8,

    pub fn encodeDentry(b: *Builder, d: GuardDentry) codec.BuildError!void {
        std.debug.assert(d.head.name_len == d.name.len);
        try b.put(GuardDentryHead, d.head);
        try b.putBytes(d.name);
        try b.putPadding(d.name.len, 8);
    }

    pub fn encode(self: XvGuardsReq, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.n_inodes == self.inodes.len);
        try b.put(XvGuardsHead, self.head);
        try b.putView(GuardInodeWire, self.inodes);
        try b.putBytes(self.dentries_bytes);
    }

    pub fn decode(c: *Cursor) Error!XvGuardsReq {
        const head = try c.take(XvGuardsHead);
        const inodes = try c.takeArray(GuardInodeWire, head.n_inodes);
        const start = c.pos;
        var i: usize = 0;
        while (i < head.n_dentries) : (i += 1) {
            const h = try c.take(GuardDentryHead);
            _ = try c.takeBytes(h.name_len);
            try c.takePadding(h.name_len, 8);
        }
        return .{ .head = head, .inodes = inodes, .dentries_bytes = c.bytes[start..c.pos] };
    }

    pub fn dentries(self: XvGuardsReq) GuardDentryIterator {
        return .{ .c = .init(self.dentries_bytes), .left = self.head.n_dentries };
    }
};

pub const XvReleaseReq = extern struct {
    volume: u16,
    reserved: u16,
    reserved2: u32,
    scope: u64,
    ino: u64,

    pub fn check(self: XvReleaseReq) bool {
        return self.scope != 0;
    }
};

pub const RecordShipKind = enum(u8) { setattr = 1, setxattr = 2, removexattr = 3, publish_layout = 4 };

pub const SETATTR_MODE: u32 = 1 << 0;
pub const SETATTR_UID: u32 = 1 << 1;
pub const SETATTR_GID: u32 = 1 << 2;
pub const SETATTR_SIZE: u32 = 1 << 3;
pub const SETATTR_ATIME: u32 = 1 << 4;
pub const SETATTR_MTIME: u32 = 1 << 5;
pub const SETATTR_CTIME: u32 = 1 << 6;
pub const SETATTR_KNOWN: u32 = 0x7F;

pub const SetattrWire = extern struct {
    valid: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    atime: i64,
    mtime: i64,
    ctime: i64,

    pub const none: SetattrWire = .{ .valid = 0, .mode = 0, .uid = 0, .gid = 0, .size = 0, .atime = 0, .mtime = 0, .ctime = 0 };

    pub fn check(self: SetattrWire) bool {
        return self.valid & ~SETATTR_KNOWN == 0;
    }
};

pub const RecordShipHead = extern struct {
    volume: u16,
    kind: u8,
    reserved: u8,
    name_len: u16,
    reserved2: u16,
    value_len: u32,
    reserved3: u32,
    ino: u64,
    /// The per-holder custody lease epoch the mutation presents.
    lease_epoch: u64,
    setattr: SetattrWire,

    pub fn check(self: RecordShipHead) bool {
        if (self.kind < 1 or self.kind > 4) return false;
        if (self.name_len > record.NAME_MAX or self.value_len > RECORD_VALUE_MAX) return false;
        return switch (@as(RecordShipKind, @fromBackingInt(@intCast(self.kind)))) {
            .setattr => self.name_len == 0 and self.value_len == 0,
            .setxattr, .publish_layout => self.name_len != 0 and self.setattr.valid == 0,
            .removexattr => self.name_len != 0 and self.value_len == 0 and self.setattr.valid == 0,
        };
    }
};

/// ONE shipped verb per record-level mutation of a foreign-slot file.
pub const RecordShipReq = struct {
    head: RecordShipHead,
    name: []const u8,
    value: []const u8,

    pub fn encode(self: RecordShipReq, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.name_len == self.name.len and self.head.value_len == self.value.len);
        try b.put(RecordShipHead, self.head);
        try b.putBytes(self.name);
        try b.putBytes(self.value);
        try b.putPadding(self.name.len + self.value.len, 8);
    }

    pub fn decode(c: *Cursor) Error!RecordShipReq {
        const head = try c.take(RecordShipHead);
        const name = try c.takeBytes(head.name_len);
        const value = try c.takeBytes(head.value_len);
        try c.takePadding(@as(usize, head.name_len) + head.value_len, 8);
        return .{ .head = head, .name = name, .value = value };
    }
};

pub const RecordShipReplyKind = enum(u8) { served = 0, slot_moved = 1 };

pub const RecordShipReply = extern struct {
    kind: u8,
    has_inode: u8,
    reserved: u16,
    holder: u32,
    inode: InodeValueWire,
    reserved2: [4]u8,

    pub fn check(self: RecordShipReply) bool {
        return self.kind <= 1 and self.has_inode <= 1;
    }
};

pub const FreeBlocksHead = extern struct { volume: u16, n: u16, reserved: u32, vol_tag: u64, lease_epoch: u64 };
pub const FreeBlocksReq = HeadWithArray(FreeBlocksHead, u64, "n", std.math.maxInt(u16));

pub const FreeVerdict = enum(u8) { freed = 0, non_terminal = 1, refused = 2 };

pub const FreeVerdictsHead = extern struct { n: u16, reserved: [6]u8 };

pub const FreeVerdicts = struct {
    head: FreeVerdictsHead,
    verdicts: []const u8,

    pub fn encode(self: FreeVerdicts, b: *Builder) codec.BuildError!void {
        std.debug.assert(self.head.n == self.verdicts.len);
        try b.put(FreeVerdictsHead, self.head);
        try b.putBytes(self.verdicts);
        try b.putPadding(self.verdicts.len, 8);
    }

    pub fn decode(c: *Cursor) Error!FreeVerdicts {
        const head = try c.take(FreeVerdictsHead);
        const v = try c.takeBytes(head.n);
        for (v) |b| if (b > 2) return error.Malformed;
        try c.takePadding(head.n, 8);
        return .{ .head = head, .verdicts = v };
    }
};

// ---------------------------------------------------------------------------
// The handshake frames (MAC-less; `auth.zig` runs the ladder).
// ---------------------------------------------------------------------------

pub const NONCE_BYTES: usize = 16;
pub const PROOF_BYTES: usize = 32;

pub const Challenge = extern struct {
    schema: u32,
    freshness_ms: u32,
    server_nonce: [NONCE_BYTES]u8,
};

pub const ProveHead = extern struct {
    schema: u32,
    peer_id_len: u16,
    reserved: u16,
    server_nonce: [NONCE_BYTES]u8,
    peer_nonce: [NONCE_BYTES]u8,
    mac: [PROOF_BYTES]u8,
};
pub const Prove = HeadWithBytes(ProveHead, "peer_id_len", PEER_ID_MAX);

pub const Admitted = extern struct { schema: u32, reserved: u32 };

pub const RefusedHead = extern struct { reason_len: u16, reserved: [6]u8 };
pub const Refused = HeadWithBytes(RefusedHead, "reason_len", REASON_MAX);

// ---------------------------------------------------------------------------
// The verb → payload tables and the tagged unions.
// ---------------------------------------------------------------------------

pub fn ReqType(comptime v: Verb) type {
    return switch (v) {
        .ping => Empty,
        .join => JoinReq,
        .renew => RenewReq,
        .leave => LeaveReq,
        .census => CensusReq,
        .xv_step => XvStepReq,
        .lookup_exact => LookupExactReq,
        .xv_guards => XvGuardsReq,
        .xv_release => XvReleaseReq,
        .record_ship => RecordShipReq,
        .free_blocks => FreeBlocksReq,
        .token_grant => TokenGrantReq,
        .token_recall => TokenRecallReq,
        .token_recall_ack => TokenRecallAckReq,
        .token_release => TokenReleaseReq,
        .custody_grant => CustodyGrantReq,
        .join_appender => JoinAppenderReq,
        .extent_grant => ExtentGrantReq,
        .return_extents => ReturnExtentsReq,
        .acquire_slots => AcquireSlotsReq,
        .acquire_slot => AcquireSlotReq,
        .offer_slot => OfferSlotReq,
        .release_slot => ReleaseSlotReq,
        .resolve_slot => ResolveSlotReq,
        .dir_rename_lock, .dir_rename_unlock => DirRenameReq,
        .mark_shared => MarkSharedReq,
        .share_block => ShareBlockReq,
        .release_shared => ReleaseSharedReq,
        .block_grant => BlockGrantReq,
        .return_blocks => ReturnBlocksReq,
        .alloc_lease_acquire => AllocLeaseAcquireReq,
        .alloc_lease_bitmap => AllocLeaseBitmapReq,
        .alloc_lease_release => AllocLeaseReleaseReq,
        .record_recovered => RecordRecoveredReq,
        .record_death => RecordDeathReq,
        .leave_appender => LeaveAppenderReq,
        .publish_endpoint => PublishEndpointReq,
        .resolve_endpoint => ResolveEndpointReq,
        .publish_roots => PublishRootsReq,
        .grow_ring => GrowRingReq,
        .challenge => Challenge,
        .prove => Prove,
        .admitted => Admitted,
        .refused => Refused,
    };
}

pub fn RepType(comptime v: Verb) type {
    return switch (v) {
        .ping => Empty,
        .join, .renew => Grant,
        .leave => Ack,
        .census => CensusPage,
        .xv_step => StepOutcome,
        .lookup_exact => DentryExact,
        .xv_guards, .xv_release => Ack,
        .record_ship => RecordShipReply,
        .free_blocks => FreeVerdicts,
        .token_grant => GrantPage,
        .token_recall => RecallFrame,
        .token_recall_ack => Ack,
        .token_release => Released,
        .custody_grant => CustodyReply,
        .join_appender => Joined,
        .extent_grant => Granted,
        .return_extents => Returned,
        .acquire_slots, .acquire_slot => AcquireReply,
        .offer_slot => Ack,
        .release_slot => Ack,
        .resolve_slot => SlotHolderReply,
        .dir_rename_lock => DirRenameReply,
        .dir_rename_unlock => Ack,
        .mark_shared => MarkReply,
        .share_block => Shared,
        .release_shared => SharedReleased,
        .block_grant => BlocksGranted,
        .return_blocks => BlocksReturned,
        .alloc_lease_acquire => AllocLeaseGranted,
        .alloc_lease_bitmap, .alloc_lease_release, .record_recovered, .record_death => Ack,
        .leave_appender, .publish_endpoint => Ack,
        .resolve_endpoint => EndpointReply,
        .publish_roots => RootsPublished,
        .grow_ring => RingGrown,
        // The handshake frames are not calls: no reply type.
        .challenge, .prove, .admitted, .refused => Empty,
    };
}

fn PayloadUnion(comptime typeOf: fn (comptime Verb) type) type {
    const info = @typeInfo(Verb).@"enum";
    var names: [Verb.count][]const u8 = undefined;
    var types: [Verb.count]type = undefined;
    var attrs: [Verb.count]std.lang.Type.Union.FieldAttributes = undefined;
    for (info.field_names, info.field_values, 0..) |name, value, i| {
        names[i] = name;
        types[i] = typeOf(@fromBackingInt(@intCast(value)));
        attrs[i] = .{};
    }
    return @Union(.auto, Verb, &names, &types, &attrs);
}

/// Every verb's decoded request, tagged by the verb (the handler table's
/// argument).
pub const Request = PayloadUnion(ReqType);
/// Every verb's decoded `ok` reply.
pub const Reply = PayloadUnion(RepType);

pub fn decodeRequest(verb: Verb, bytes: []const u8) Error!Request {
    switch (verb) {
        inline else => |v| return @unionInit(Request, @tagName(v), try codec.decodePayload(ReqType(v), bytes)),
    }
}

pub fn decodeReply(verb: Verb, bytes: []const u8) Error!Reply {
    switch (verb) {
        inline else => |v| return @unionInit(Reply, @tagName(v), try codec.decodePayload(RepType(v), bytes)),
    }
}

pub fn encodeRequest(req: Request, b: *Builder) codec.BuildError!void {
    switch (req) {
        inline else => |payload, tag| try codec.encodePayload(ReqType(tag), payload, b),
    }
}

pub fn encodeReply(rep: Reply, b: *Builder) codec.BuildError!void {
    switch (rep) {
        inline else => |payload, tag| try codec.encodePayload(RepType(tag), payload, b),
    }
}

// ---------------------------------------------------------------------------
// Tests: the numbering law, every codec's round trip, totality by fuzz.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "verb numbers: block-coded, unique, the handshake frames at 0xFFF0, every block's schema" {
    inline for (@typeInfo(Verb).@"enum".field_values, 0..) |value, i| {
        const v: Verb = @fromBackingInt(@intCast(value));
        try testing.expectEqual(v, Verb.fromWire(value).?);
        try testing.expectEqual(i, v.index());
        _ = v.block();
        _ = v.schema();
    }
    try testing.expectEqual(@as(?Verb, null), Verb.fromWire(0x0508));
    try testing.expectEqual(@as(?Verb, null), Verb.fromWire(0x0200));
    try testing.expectEqual(Block.manager, Verb.join_appender.block());
    try testing.expectEqual(Block.membership, Verb.renew.block());
    try testing.expectEqual(Block.token, Verb.token_grant.block());
    try testing.expectEqual(Block.xv, Verb.record_ship.block());
    try testing.expectEqual(Block.wire, Verb.challenge.block());
    try testing.expectEqual(Block.wire, Verb.ping.block());
    try testing.expect(Verb.challenge.isHandshake() and Verb.refused.isHandshake() and !Verb.ping.isHandshake());
    try testing.expectEqual(MANAGER_SCHEMA, Verb.record_death.schema());
    try testing.expectEqual(CLUSTER_WIRE_SCHEMA, Verb.join.schema());
    try testing.expectEqual(META_SHIP_SCHEMA, Verb.xv_guards.schema());
    try testing.expectEqual(TOKEN_SCHEMA, Verb.custody_grant.schema());
    try testing.expectEqual(@as(u16, 0x0586), @backingInt(Verb.record_death));
    try testing.expectEqual(@as(u16, 0x05B4), @backingInt(Verb.grow_ring));
}

test "WireIdentity: the peer id is node_token BE ‖ mount_slot BE; the member pair never includes the incarnation" {
    const id: WireIdentity = .{ .node_token = 0x0102030405060708, .writer_id = 99, .mount_slot = 0xAABBCCDD, .reserved = 0 };
    const p = id.peerId();
    try testing.expectEqual(@as(u8, 1), p[0]);
    try testing.expectEqual(@as(u8, 0xAA), p[8]);
    const back = WireIdentity.fromPeerId(&p).?;
    try testing.expect(back.sameMember(id));
    try testing.expectEqual(@as(u64, 0), back.writer_id);
    try testing.expectEqual(@as(?WireIdentity, null), WireIdentity.fromPeerId("short"));
    var buf: [64]u8 = undefined;
    try testing.expectEqualStrings("node_0102030405060708.maabbccdd", try std.fmt.bufPrint(&buf, "{f}", .{id}));
}

fn roundTrip(comptime P: type, v: P) !P {
    var buf: [8192]u8 = undefined;
    var b: Builder = .init(&buf);
    try codec.encodePayload(P, v, &b);
    const d = try codec.decodePayload(P, b.written());
    // Re-encoding the decoded form yields the same bytes (canonical).
    var buf2: [8192]u8 = undefined;
    var b2: Builder = .init(&buf2);
    try codec.encodePayload(P, d, &b2);
    try testing.expectEqualSlices(u8, b.written(), b2.written());
    return d;
}

const sample_identity: WireIdentity = .{ .node_token = 0xDEAD, .writer_id = 0xBEEF, .mount_slot = 3, .reserved = 0 };

test "manager payloads round-trip canonically: region, slots, rename, clone, alloc, death" {
    _ = try roundTrip(JoinAppenderReq, .{ .volume = 1, .reserved = 0, .reserved2 = 0, .identity = sample_identity, .ring_want_bytes = 0 });
    const segs = [_]RunWire{ .{ .start = 4096, .len = 1 << 20 }, .{ .start = 1 << 30, .len = 4096 } };
    const grant = [_]GrantRunWire{.{ .start = 10, .len = 8, .reserved = 0 }};
    const j = try roundTrip(Joined, .{ .head = .{ .appender_id = 7, .already = 1, .n_segments = 2, .n_grant = 1, .reserved = 0, .page_addr = 8192, .node_seq_base = 1 << 38, .ring_bytes = 1 << 20 }, .segments = &segs, .grant = &grant });
    try testing.expectEqual(@as(u64, 1 << 30), j.segments[1].start);
    try testing.expectEqual(@as(u32, 8), j.grant[0].len);
    _ = try roundTrip(ExtentGrantReq, .{ .volume = 0, .reserved = 0, .appender_id = 7, .want = 0, .reserved2 = 0 });
    _ = try roundTrip(Granted, .{ .head = .{ .n = 1, .reserved = @splat(0) }, .items = &grant });
    const r = try roundTrip(ReturnExtentsReq, .{ .head = .{ .volume = 0, .n_runs = 1, .appender_id = 7, .reserved = 0 }, .items = &grant });
    try testing.expectEqual(@as(u64, 10), r.items[0].start);
    _ = try roundTrip(Returned, .{ .cleared = 5, .already = 3 });
    _ = try roundTrip(LeaveAppenderReq, .{ .head = .{ .volume = 0, .n_unclaimed = 0, .appender_id = 7, .identity = sample_identity, .reserved = 0 }, .items = &.{} });
    const ep = try endpoint.parseEndpoint("10.0.0.9:7001");
    const pe = try roundTrip(PublishEndpointReq, .{ .volume = 0, .reserved = 0, .appender_id = 7, .identity = sample_identity, .pr_key = 42, .endpoint = ep, .reserved2 = @splat(0) });
    try testing.expect(pe.endpoint.eql(ep));
    _ = try roundTrip(ResolveEndpointReq, .{ .volume = 0, .reserved = 0, .appender_id = 7 });
    _ = try roundTrip(EndpointReply, .{ .present = 1, .reserved = @splat(0), .endpoint = ep });
    _ = try roundTrip(EndpointReply, .{ .present = 0, .reserved = @splat(0), .endpoint = .none });
    const roots = [_]SlotRootWire{.{ .root = .{ .addr = 1 << 20, .seq = 9 }, .cursor = 2, .slot_tree_extents = 1, .g = 3, .slot = 17, .reserved = @splat(0) }};
    _ = try roundTrip(PublishRootsReq, .{ .head = .{ .volume = 0, .reserved = 0, .appender_id = 7, .n = 1, .reserved2 = 0 }, .items = &roots });
    _ = try roundTrip(RootsPublished, .{ .published = 1, .already = 0 });
    _ = try roundTrip(GrowRingReq, .{ .volume = 0, .reserved = 0, .appender_id = 7, .want_bytes = 1 << 21 });
    _ = try roundTrip(RingGrown, .{ .present = 1, .reserved = @splat(0), .segment = segs[0] });
    _ = try roundTrip(RingGrown, .{ .present = 0, .reserved = @splat(0), .segment = .{ .start = 0, .len = 0 } });
    _ = try roundTrip(AcquireSlotsReq, .{ .volume = 0, .want = 4, .appender_id = 7 });
    _ = try roundTrip(AcquireSlotReq, .{ .volume = 0, .slot = 17, .appender_id = 7 });
    const words: SlotWordsWire = .{ .root = .{ .addr = 1 << 20, .seq = 9 }, .cursor = 2, .seq_floor = 77, .slot_tree_extents = 1, .reserved = 0 };
    const grants = [_]SlotGrantWire{.{ .words = words, .g = 4, .slot = 17, .reserved = 0 }};
    _ = try roundTrip(AcquireReply, .{ .head = .{ .kind = 0, .reserved = 0, .n = 1, .already = 0, .slot = 0, .holder = 0, .g = 0 }, .items = &grants });
    _ = try roundTrip(AcquireReply, .{ .head = .{ .kind = 1, .reserved = 0, .n = 0, .already = 0, .slot = 17, .holder = 9, .g = 4 }, .items = &.{} });
    _ = try roundTrip(OfferSlotReq, .{ .volume = 0, .slot = 17, .appender_id = 7, .to = 9, .reserved = 0 });
    const tails = [_]LeafTailWire{.{ .leaf_addr = 1 << 21, .tail = 4000, .reserved = 0 }};
    const rel = try roundTrip(ReleaseSlotReq, .{ .head = .{ .volume = 0, .slot = 17, .appender_id = 7, .g = 4, .n_tails = 1, .words = words }, .items = &tails });
    try testing.expectEqual(@as(u64, 77), rel.head.words.seq_floor);
    _ = try roundTrip(ResolveSlotReq, .{ .volume = 0, .slot = 17, .reserved = 0 });
    _ = try roundTrip(SlotHolderReply, .{ .kind = 0, .reserved = @splat(0), .appender_id = 7, .g = 4, .reserved2 = 0 });
    _ = try roundTrip(DirRenameReq, .{ .volume = 0, .reserved = 0, .appender_id = 7 });
    _ = try roundTrip(DirRenameReply, .{ .kind = 1, .already = 0, .reserved = 0, .holder = 9 });
    _ = try roundTrip(MarkSharedReq, .{ .volume = 0, .reserved = 0, .block_index = 3, .vol_tag = 11, .block_idx = 12, .owner_ino = 13 });
    _ = try roundTrip(MarkReply, .{ .kind = 0, .already = 1, .reserved = @splat(0) });
    const refs = [_]BlockRefWire{ .{ .owner_ino = 13, .block_index = 3, .reserved = 0 }, .{ .owner_ino = 14, .block_index = 0, .reserved = 0 } };
    _ = try roundTrip(ShareBlockReq, .{ .head = .{ .volume = 0, .n = 2, .reserved = 0, .vol_tag = 11, .block_idx = 12 }, .items = &refs });
    _ = try roundTrip(Shared, .{ .inserted = 2, .already = 0 });
    _ = try roundTrip(ReleaseSharedReq, .{ .volume = 0, .present = 1, .reserved = 0, .reserved2 = 0, .vol_tag = 11, .block_idx = 12, .owner = refs[0] });
    _ = try roundTrip(SharedReleased, .{ .kind = 1, .reserved = @splat(0), .remaining = 1 });
    _ = try roundTrip(BlockGrantReq, .{ .volume = 0, .reserved = 0, .want = 0, .writer = sample_identity, .vol_tag = 11, .held_unconsumed = 3 });
    _ = try roundTrip(BlocksGranted, .{ .head = .{ .kind = 0, .already = 0, .n = 1, .reserved = 0 }, .items = &grant });
    _ = try roundTrip(ReturnBlocksReq, .{ .volume = 0, .reserved = 0, .len = 64, .writer = sample_identity, .vol_tag = 11, .start = 1000 });
    _ = try roundTrip(BlocksReturned, .{ .present = 1, .reserved = @splat(0), .cleared = 64 });
    _ = try roundTrip(AllocLeaseAcquireReq, .{ .volume = 0, .home_vol = 1, .appender_id = 7, .identity = sample_identity, .vol_tag = 11, .blocks = 1 << 20 });
    const bits = [_]BitmapRefWire{.{ .start = 1 << 20, .len = 1 << 16, .vol = 1, .reserved = @splat(0) }};
    _ = try roundTrip(AllocLeaseGranted, .{ .head = .{ .already = 0, .reserved = 0, .n_bitmap = 1, .reserved2 = 0, .term = 1, .predecessor_blocks = 0 }, .items = &bits });
    _ = try roundTrip(AllocLeaseBitmapReq, .{ .head = .{ .volume = 0, .n = 1, .reserved = 0, .identity = sample_identity, .vol_tag = 11, .term = 1 }, .items = &bits });
    _ = try roundTrip(AllocLeaseReleaseReq, .{ .volume = 0, .reserved = 0, .reserved2 = 0, .identity = sample_identity, .vol_tag = 11, .term = 1 });
    _ = try roundTrip(RecordDeathReq, .{ .volume = 0, .reserved = 0, .reserved2 = 0, .member = sample_identity, .epoch = 5, .pr_key = 0 });
    _ = try roundTrip(RecordRecoveredReq, .{ .volume = 0, .vol = 2, .reserved = 0, .member = sample_identity });
    _ = try roundTrip(Ack, .replay);
    _ = try roundTrip(Empty, .{});
}

test "membership payloads: join with a mount path, the grant's three carriages, nested renewal windows, the census page" {
    var id: [16]u8 = @splat(0);
    @memcpy(id[0..12], &sample_identity.peerId());
    const ep = try endpoint.parseEndpoint("[::1]:7001");
    const jr = try roundTrip(JoinReq, .{ .head = .{ .role = 0, .flags = JOIN_FLAG_HAS_ENDPOINT | JOIN_FLAG_HAS_PRIOR_EPOCH, .mount_len = 7, .pid = 1234, .id = id, .boot = @splat(9), .endpoint = ep, .reserved = @splat(0), .prior_epoch = 3, .pr_key = 0 }, .bytes = "/mnt/x1" });
    try testing.expectEqualStrings("/mnt/x1", jr.bytes);
    const acks = [_]SlotLeaseAckWire{.{ .g = 2, .slot = 17, .reserved = 0 }};
    const rels = [_]u16{ 5, 6, 7 };
    const offers = [_]OfferedSlotWire{.{ .slot = 9, .reserved = 0, .holder = 2 }};
    const g = try roundTrip(Grant, .{ .head = .{ .epoch = 4, .term = 2, .granted_at_owner_ms = 1000, .ack_prior_epoch = 3, .t_owner_ms = 45_000, .skew_max_ms = 23, .d_purge_ms = 20, .renew_ms = 10_000, .checkpoint_ceiling_ms = 1100, .n_ack = 1, .n_release = 3, .n_offered = 1, .reserved = @splat(0) }, .slot_leases_ack = &acks, .slot_release_notices = &rels, .offered_slots = &offers });
    try testing.expectEqual(@as(u16, 7), g.slot_release_notices[2]);
    try testing.expectEqual(@as(u16, 9), g.offered_slots[0].slot);
    // Renewal windows: two volumes, nested ranges.
    var wbuf: [512]u8 = undefined;
    var wb: Builder = .init(&wbuf);
    const ranges = [_]GrantRunWire{ .{ .start = 100, .len = 64, .reserved = 0 }, .{ .start = 300, .len = 1, .reserved = 0 } };
    try RenewReq.encodeWindows(&wb, &.{ .{ .vol_tag = 11, .ranges = &ranges }, .{ .vol_tag = 12, .ranges = ranges[0..1] } });
    const rn = try roundTrip(RenewReq, .{ .head = .{ .id = id, .epoch = 4, .acked_free_epoch = 1, .n_windows = 2, .reserved = @splat(0) }, .windows_bytes = wb.written() });
    var it = rn.windows();
    const w0 = it.next().?;
    try testing.expectEqual(@as(u64, 11), w0.vol_tag);
    try testing.expectEqual(@as(usize, 2), w0.ranges.len);
    const w1 = it.next().?;
    try testing.expectEqual(@as(u64, 12), w1.vol_tag);
    try testing.expectEqual(@as(?WindowDecl, null), it.next());
    // A lying window count is Malformed, never a view past the bytes.
    var lying = rn;
    lying.head.n_windows = 3;
    var buf: [1024]u8 = undefined;
    var b: Builder = .init(&buf);
    try lying.encode(&b);
    try testing.expectError(error.Malformed, codec.decodePayload(RenewReq, b.written()));
    _ = try roundTrip(LeaveReq, .{ .id = id });
    _ = try roundTrip(CensusReq, .{ .cursor = 0, .limit = 1024, .reserved = 0 });
    var rbuf: [1024]u8 = undefined;
    var rb: Builder = .init(&rbuf);
    const row: MemberSnapshot = .{ .head = .{ .id = id, .boot = @splat(1), .endpoint = ep, .role = 0, .state = 0, .mount_len = 6, .pid = 4, .reserved = 0, .epoch = 4, .age_ms = 10, .expires_in_ms = 44_000, .acked_free_epoch = 1 }, .mount = "/mnt/a" };
    try CensusPage.encodeRow(&rb, row);
    try CensusPage.encodeRow(&rb, row);
    const page = try roundTrip(CensusPage, .{ .head = .{ .n = 2, .has_next = 1, .reserved = @splat(0), .next = 2 }, .rows_bytes = rb.written() });
    var rows = page.rows();
    try testing.expectEqualStrings("/mnt/a", rows.next().?.mount);
    _ = rows.next().?;
    try testing.expectEqual(@as(?MemberSnapshot, null), rows.next());
}

test "token payloads: the grant ask, a page with xattrs and a dentry page, recall/ack/release, the custody reply carrying a page" {
    _ = try roundTrip(TokenGrantReq, .{ .head = .{ .volume = 0, .wants = WANT_RECORDS | WANT_DENTRIES, .mode = 0, .xattr_after_len = 3, .client_id = 77, .object = 1000, .after = 0 }, .bytes = "abc" });
    var xb: [512]u8 = undefined;
    var xbb: Builder = .init(&xb);
    try GrantPage.encodeXattr(&xbb, .{ .name = "user.a", .value = "1" });
    try GrantPage.encodeXattr(&xbb, .{ .name = "layout", .value = "LLLLLLLLLLLL" });
    var db: [512]u8 = undefined;
    var dbb: Builder = .init(&db);
    try GrantPage.encodeDir(&dbb, .{ .head = .{ .cookie = 1, .child_ino = 55, .file_type = 8, .name_len = 5, .reserved = @splat(0) }, .name = "hello" });
    const attrs: InodeValueWire = std.mem.zeroes(InodeValueWire);
    const page_v: GrantPage = .{ .head = .{ .kind = 0, .flags = PAGE_XATTRS_COMPLETE | PAGE_HAS_DIR | PAGE_DIR_COMPLETE, .reserved = 0, .n_xattrs = 2, .n_dir = 1, .holder = 0, .attrs = attrs, .reserved2 = @splat(0) }, .xattrs_bytes = xbb.written(), .dir_bytes = dbb.written() };
    var pgbuf: [1024]u8 = undefined;
    var pgb: Builder = .init(&pgbuf);
    try page_v.encode(&pgb);
    const pg = try codec.decodePayload(GrantPage, pgb.written());
    _ = try roundTrip(GrantPage, page_v);
    var xs = pg.xattrs();
    try testing.expectEqualStrings("user.a", xs.next().?.name);
    try testing.expectEqualStrings("LLLLLLLLLLLL", xs.next().?.value);
    try testing.expectEqual(@as(?XattrEntry, null), xs.next());
    var ds = pg.dir();
    const d0 = ds.next().?;
    try testing.expectEqualStrings("hello", d0.name);
    try testing.expectEqual(@as(u64, 55), d0.head.child_ino);
    try testing.expectEqual(@as(?DirEntry, null), ds.next());
    _ = try roundTrip(GrantPage, .{ .head = .{ .kind = 1, .flags = 0, .reserved = 0, .n_xattrs = 0, .n_dir = 0, .holder = 9, .attrs = attrs, .reserved2 = @splat(0) }, .xattrs_bytes = "", .dir_bytes = "" });
    _ = try roundTrip(TokenRecallReq, .{ .volume = 0, .reserved = 0, .wait_ms = 5000, .client_id = 77 });
    const objs = [_]u64{ 1000, 1001 };
    _ = try roundTrip(RecallFrame, .{ .head = .{ .frame_id = 3, .n = 2, .reserved = 0 }, .items = &objs });
    _ = try roundTrip(RecallFrame, .{ .head = .{ .frame_id = 0, .n = 0, .reserved = 0 }, .items = &.{} });
    _ = try roundTrip(TokenRecallAckReq, .{ .volume = 0, .reserved = 0, .reserved2 = 0, .client_id = 77, .frame_id = 3 });
    _ = try roundTrip(TokenReleaseReq, .{ .head = .{ .volume = 0, .n = 2, .reserved = 0, .client_id = 77 }, .items = &objs });
    _ = try roundTrip(Released, .{ .count = 2, .reserved = 0 });
    _ = try roundTrip(CustodyGrantReq, .{ .volume = 0, .concurrent_write = 1, .reserved = 0, .wait_ms = 100, .client_id = 77, .object = 1000, .span_start = 0, .span_len = 1 << 22, .lease_epoch = 5, .reserved2 = 0 });
    const cr = try roundTrip(CustodyReply, .{ .head = .{ .kind = 0, .reserved = @splat(0), .status = 0, .grant_epoch = 6, .page_len = @intCast(pgb.pos), .reserved2 = 0 }, .page_bytes = pgb.written() });
    try testing.expectEqual(@as(u32, 2), cr.page().?.head.n_xattrs);
    const cr2 = try roundTrip(CustodyReply, .{ .head = .{ .kind = 1, .reserved = @splat(0), .status = 7, .grant_epoch = 0, .page_len = 0, .reserved2 = 0 }, .page_bytes = "" });
    try testing.expectEqual(@as(?GrantPage, null), cr2.page());
}

test "cross-owner payloads: a step with a name, the exact lookup, travelling guards in ascending order, the record ship, shipped frees" {
    const step: XvStepWire = .{ .kind = 2, .parent_update = 3, .name_len = 4, .flags = 0, .ft_bits = 0o040000, .a = 100, .b = 200, .c = 0, .w0 = 0, .w1 = 0, .w2 = 0, .w3 = 0 };
    const xs = try roundTrip(XvStepReq, .{ .head = .{ .volume = 1, .step_idx = 1, .reserved = 0, .tx_id = 1 << 40 | 7, .scope = 1 << 40 | 3, .step = step }, .name = "name" });
    try testing.expectEqualStrings("name", xs.name);
    // A non-dentry kind with a name, or a dentry kind without one, is Malformed.
    var bad = step;
    bad.kind = 3;
    var buf: [256]u8 = undefined;
    var b: Builder = .init(&buf);
    try (XvStepReq{ .head = .{ .volume = 1, .step_idx = 1, .reserved = 0, .tx_id = 7, .scope = 0, .step = bad }, .name = "name" }).encode(&b);
    try testing.expectError(error.Malformed, codec.decodePayload(XvStepReq, b.written()));
    _ = try roundTrip(StepOutcome, .{ .status = 1, .has_inode = 0, .reserved = @splat(0), .inode = std.mem.zeroes(InodeValueWire), .reserved2 = @splat(0) });
    _ = try roundTrip(LookupExactReq, .{ .head = .{ .volume = 0, .name_len = 2, .reserved = 0, .reserved2 = 0, .parent = 1 }, .bytes = "ab" });
    _ = try roundTrip(DentryExact, .{ .present = 1, .file_type = 4, .reserved = @splat(0), .ino = 9 });
    _ = try roundTrip(DentryExact, .{ .present = 0, .file_type = 0, .reserved = @splat(0), .ino = 0 });
    var gbuf: [512]u8 = undefined;
    var gb: Builder = .init(&gbuf);
    try XvGuardsReq.encodeDentry(&gb, .{ .head = .{ .parent = 1, .exclusive = 1, .name_len = 3, .reserved = @splat(0) }, .name = "dir" });
    const inodes = [_]GuardInodeWire{ .{ .ino = 1, .exclusive = 0, .reserved = @splat(0) }, .{ .ino = 9, .exclusive = 1, .reserved = @splat(0) } };
    const gr = try roundTrip(XvGuardsReq, .{ .head = .{ .volume = 0, .n_inodes = 2, .n_dentries = 1, .reserved = 0, .scope = 1 << 40 | 3 }, .inodes = &inodes, .dentries_bytes = gb.written() });
    var dit = gr.dentries();
    try testing.expectEqualStrings("dir", dit.next().?.name);
    try testing.expectEqual(@as(?GuardDentry, null), dit.next());
    _ = try roundTrip(XvReleaseReq, .{ .volume = 0, .reserved = 0, .reserved2 = 0, .scope = 1 << 40 | 3, .ino = 9 });
    const sa: SetattrWire = .{ .valid = SETATTR_MODE | SETATTR_MTIME, .mode = 0o644, .uid = 0, .gid = 0, .size = 0, .atime = 0, .mtime = 123, .ctime = 0 };
    _ = try roundTrip(RecordShipReq, .{ .head = .{ .volume = 0, .kind = 1, .reserved = 0, .name_len = 0, .reserved2 = 0, .value_len = 0, .reserved3 = 0, .ino = 9, .lease_epoch = 2, .setattr = sa }, .name = "", .value = "" });
    const rs = try roundTrip(RecordShipReq, .{ .head = .{ .volume = 0, .kind = 2, .reserved = 0, .name_len = 6, .reserved2 = 0, .value_len = 3, .reserved3 = 0, .ino = 9, .lease_epoch = 2, .setattr = .none }, .name = "user.a", .value = "xyz" });
    try testing.expectEqualStrings("xyz", rs.value);
    _ = try roundTrip(RecordShipReply, .{ .kind = 1, .has_inode = 0, .reserved = 0, .holder = 4, .inode = std.mem.zeroes(InodeValueWire), .reserved2 = @splat(0) });
    const blocks = [_]u64{ 5, 6, 7 };
    _ = try roundTrip(FreeBlocksReq, .{ .head = .{ .volume = 0, .n = 3, .reserved = 0, .vol_tag = 11, .lease_epoch = 2 }, .items = &blocks });
    const fv = try roundTrip(FreeVerdicts, .{ .head = .{ .n = 3, .reserved = @splat(0) }, .verdicts = &.{ 0, 1, 2 } });
    try testing.expectEqual(@as(u8, 2), fv.verdicts[2]);
    var vb: Builder = .init(&buf);
    try (FreeVerdicts{ .head = .{ .n = 1, .reserved = @splat(0) }, .verdicts = &.{3} }).encode(&vb);
    try testing.expectError(error.Malformed, codec.decodePayload(FreeVerdicts, vb.written()));
}

test "handshake payloads round-trip; the peer id and the reason are bounded" {
    _ = try roundTrip(Challenge, .{ .schema = CLUSTER_WIRE_SCHEMA, .freshness_ms = 30_000, .server_nonce = @splat(5) });
    const pr = try roundTrip(Prove, .{ .head = .{ .schema = CLUSTER_WIRE_SCHEMA, .peer_id_len = 12, .reserved = 0, .server_nonce = @splat(5), .peer_nonce = @splat(6), .mac = @splat(7) }, .bytes = &sample_identity.peerId() });
    try testing.expectEqual(@as(usize, 12), pr.bytes.len);
    _ = try roundTrip(Admitted, .{ .schema = CLUSTER_WIRE_SCHEMA, .reserved = 0 });
    _ = try roundTrip(Refused, .{ .head = .{ .reason_len = 3, .reserved = @splat(0) }, .bytes = "nah" });
    var buf: [1024]u8 = undefined;
    var b: Builder = .init(&buf);
    const long: [300]u8 = @splat('p');
    try (Prove{ .head = .{ .schema = 6, .peer_id_len = 300, .reserved = 0, .server_nonce = @splat(0), .peer_nonce = @splat(0), .mac = @splat(0) }, .bytes = &long }).encode(&b);
    try testing.expectError(error.Malformed, codec.decodePayload(Prove, b.written()));
}

test "the tagged unions carry every verb; a request decodes under its verb and re-encodes to the same bytes" {
    var buf: [256]u8 = undefined;
    var b: Builder = .init(&buf);
    try encodeRequest(.{ .resolve_slot = .{ .volume = 2, .slot = 17, .reserved = 0 } }, &b);
    const req = try decodeRequest(.resolve_slot, b.written());
    try testing.expectEqual(@as(u16, 17), req.resolve_slot.slot);
    var b2: Builder = .init(&buf);
    try encodeReply(.{ .resolve_slot = .{ .kind = 1, .reserved = @splat(0), .appender_id = 0, .g = 3, .reserved2 = 0 } }, &b2);
    const rep = try decodeReply(.resolve_slot, b2.written());
    try testing.expectEqual(@as(u32, 3), rep.resolve_slot.g);
    // A payload under the wrong verb is Malformed (the shapes differ), never a reinterpretation.
    try testing.expectError(error.Malformed, decodeRequest(.join_appender, b.written()));
    try testing.expectEqual(@as(usize, Verb.count), @typeInfo(Request).@"union".field_names.len);
}

fn totalityOver(bytes: []const u8) void {
    inline for (@typeInfo(Verb).@"enum".field_values) |value| {
        const v: Verb = @fromBackingInt(@intCast(value));
        _ = decodeRequest(v, bytes) catch {};
        _ = decodeReply(v, bytes) catch {};
    }
}

test "totality: every verb's request and reply decoder returns or Malformed on every prefix of random bytes" {
    var buf: [600]u8 = undefined;
    var prng = std.Random.DefaultPrng.init(0x5A5A);
    prng.random().bytes(&buf);
    var len: usize = 0;
    while (len <= buf.len) : (len += 7) totalityOver(buf[0..len]);
    // All-zero bytes exercise every "count = 0" arm.
    const zeros: [256]u8 = @splat(0);
    len = 0;
    while (len <= zeros.len) : (len += 8) totalityOver(zeros[0..len]);
}

fn verbsFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [4096]u8 = undefined;
    const len = smith.slice(&buf);
    totalityOver(buf[0..len]);
}

test "fuzz: every verb decoder is total on arbitrary bytes" {
    try std.testing.fuzz({}, verbsFuzz, .{});
}
