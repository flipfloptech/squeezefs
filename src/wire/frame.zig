//! THE cluster-wire frame (charter §5.5 "one wire family over the cluster
//! wire"; `docs/porting/sym_wire_membership_recovery.md` §1.2, §1.7, §1.9):
//! one fixed 48 B little-endian header, the payload, and — on every
//! authenticated session frame — a 32 B HMAC-SHA256 trailer.
//!
//!     frame = header[48] ‖ payload[len] ‖ (MAC_PRESENT ? HMAC(session_key, header ‖ payload) : ∅)
//!
//! The header's own xxh3 covers `header[0..40] ‖ payload`, so a garbage or
//! misframed frame is refused by a checksum before any MAC work; the MAC
//! covers the header too, so direction, sequence, length, verb, status and
//! correlation are all authenticated (the reference MACs `dir ‖ seq ‖ len
//! ‖ body`; here they are IN the header the MAC covers). Decision 0002
//! rows 221–222: no bincode, no inner verb enum, the "documented codes"
//! are the verb numbers on the wire.
//!
//! The payload length law: `len` is a CLAIM. Before a byte of payload is
//! read it is checked against the verb block's class cap, and the receive
//! buffer grows in `FRAME_CHUNK_BYTES` rounds as bytes actually arrive, so
//! a lying length costs one chunk, never the cap (`lying_length_prefix_
//! allocates_one_chunk_not_the_cap`).
//!
//! Two-level versioning (§1.9): `version` is the FRAMING (changes only with
//! a transport change); `schema` is the VOCABULARY of the verb's block.
//! Either unknown ⇒ refuse loud, counted (`cluster_wire_schema_refusals`).
//!
//! Portable.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const checksum = @import("../core/checksum.zig");
const errors = @import("../core/errors.zig");
const codec = @import("codec.zig");
const linux = std.os.linux;

pub const MAGIC: [4]u8 = "SQZW".*;
/// The FRAMING version: bumps only when the transport changes (a new
/// header, a new MAC primitive), never per vocabulary.
pub const FRAMING_VERSION: u16 = 1;

/// bit0: a MAC trailer follows the payload (0 on the four handshake
/// frames, 1 on every session frame).
pub const FLAG_MAC_PRESENT: u16 = 1 << 0;
/// bit1: direction — 0 coordinator→peer, 1 peer→coordinator. In the MAC's
/// cover so a frame reflected back to its sender fails verification.
pub const FLAG_DIR_PEER_TO_COORD: u16 = 1 << 1;
/// bit2: 0 a call, 1 a reply.
pub const FLAG_REPLY: u16 = 1 << 2;
/// bits 3..15 reserved = 0 (checked: a flag this binary does not know is a
/// foreign framing, refused like a version).
pub const FLAGS_KNOWN: u16 = FLAG_MAC_PRESENT | FLAG_DIR_PEER_TO_COORD | FLAG_REPLY;

pub const HEADER_LEN: usize = 48;
/// HMAC-SHA256 untruncated.
pub const MAC_BYTES: usize = 32;
/// The receive buffer grows by this much per round, so a lying `len`
/// costs one chunk before the body deadline fires.
pub const FRAME_CHUNK_BYTES: usize = 64 << 10;

/// What an UNAUTHENTICATED peer may cost the listener per frame: the four
/// handshake frames carry two 16 B nonces, a 32 B proof, a ≤ 256 B peer id
/// and a reason string — 8 KiB is 16× the widest honest handshake.
pub const HANDSHAKE_CAP: u32 = 8 << 10;
/// Every symmetric-plane verb's class: one whole token-set page (the
/// grant's dentry budget is CONTROL_CAP / 2 = 512 KiB), a shipped
/// record-level mutation (≤ the 64 KiB record cap + a layout publish), a
/// 1024-row census page (≈ 120 B/row) — the FUSE `max_write` class, which
/// is the largest thing a daemon moves in one op anywhere. Blocks never
/// move on the wire.
pub const CONTROL_CAP: u32 = 1 << 20;
/// The job fabric's BULK class (rung 8; `ops/jobs/frames.zig`): a
/// 64-checksum shard proposal is ~1.6 KiB, a fleet read-shard REPORT can
/// be MiBs (one finding row per suspect inode of a residue) — 16 MiB holds
/// a 4-member fleet's share of a 2 M-inode census at ~32 B/row. Only the
/// three job verbs that carry it read at this cap; the receive buffer
/// still grows a chunk at a time (the lying-length law).
pub const BULK_CAP: u32 = 16 << 20;
/// The largest payload any frame may carry (the bulk class).
pub const MAX_FRAME_PAYLOAD: u32 = BULK_CAP;

pub const Class = enum(u8) {
    handshake,
    control,
    bulk,

    pub fn cap(self: Class) u32 {
        return switch (self) {
            .handshake => HANDSHAKE_CAP,
            .control => CONTROL_CAP,
            .bulk => BULK_CAP,
        };
    }
};

comptime {
    std.debug.assert(HANDSHAKE_CAP < CONTROL_CAP);
    std.debug.assert(CONTROL_CAP < BULK_CAP);
    std.debug.assert(FRAME_CHUNK_BYTES <= CONTROL_CAP);
}

/// The 48 B header (`_le` byte arrays, like every other wire struct that
/// crosses the checksum boundary field by field).
pub const FrameHeaderWire = extern struct {
    magic: [4]u8,
    version_le: [2]u8,
    flags_le: [2]u8,
    len_le: [4]u8,
    verb_le: [2]u8,
    status_le: [2]u8,
    correlation_le: [8]u8,
    seq_le: [8]u8,
    schema_le: [4]u8,
    reserved_le: [4]u8,
    xxh3_le: [8]u8,
};

comptime {
    std.debug.assert(@sizeOf(FrameHeaderWire) == HEADER_LEN);
    std.debug.assert(@offsetOf(FrameHeaderWire, "version_le") == 4);
    std.debug.assert(@offsetOf(FrameHeaderWire, "flags_le") == 6);
    std.debug.assert(@offsetOf(FrameHeaderWire, "len_le") == 8);
    std.debug.assert(@offsetOf(FrameHeaderWire, "verb_le") == 12);
    std.debug.assert(@offsetOf(FrameHeaderWire, "status_le") == 14);
    std.debug.assert(@offsetOf(FrameHeaderWire, "correlation_le") == 16);
    std.debug.assert(@offsetOf(FrameHeaderWire, "seq_le") == 24);
    std.debug.assert(@offsetOf(FrameHeaderWire, "schema_le") == 32);
    std.debug.assert(@offsetOf(FrameHeaderWire, "reserved_le") == 36);
    std.debug.assert(@offsetOf(FrameHeaderWire, "xxh3_le") == 40);
}

pub const XXH3_OFF: usize = @offsetOf(FrameHeaderWire, "xxh3_le");

// ---------------------------------------------------------------------------
// The ONE status table (§1.7; decision 0002 row 223: the reference's words
// were scattered by stage — 0x11/0x12/0x13 membership, 32–39 meta-ship,
// 48–50 manager, 56–57 tokens; here ONE u16 space every block shares).
// ---------------------------------------------------------------------------

pub const Status = enum(u16) {
    ok = 0,
    /// The verb number is not in this binary's vocabulary.
    unknown_verb = 1,
    /// The frame's `schema` is not the verb block's (refusing rather than
    /// interpreting a foreign vocabulary — charter §1.7).
    schema = 2,
    /// The payload did not decode (length, shape, reserved bytes).
    malformed = 3,
    /// This node holds no manager lease for the named volume ordinal.
    not_manager = 4,
    /// This mount leases no such slot / its plane is not armed (the
    /// served guard's "re-resolve through tree 0, ship again").
    not_holder = 5,
    /// The owner's era moved under the caller (`StaleOwnerEra`): the SAME
    /// request is admissible on the retry.
    stale_term = 6,
    /// The durable witness contradicts the caller (must-stay-0 at the
    /// server: `cluster_wire_service_refusals`). The payload is a
    /// `WireErrorWire` naming the errno (and a typed class when retryable).
    refused = 7,
    /// A wire word names what durable state cannot — the SCREEN failed
    /// (the buggy/hostile-peer class, counted apart from must-stay-0).
    rejected = 8,
    /// "Not now": nothing written, the requester retries (`Deferred`).
    deferred = 9,
    /// The membership lease of the caller is not live (`MembershipPending`).
    not_member = 10,
    /// The member's lease is unknown to this owner: self-fence, then re-join
    /// — never a retry.
    unknown_lease = 11,
    /// A live holder stands in the way (the manager's `Busy`: EBUSY).
    busy = 12,

    pub fn fromWire(w: u16) ?Status {
        return switch (w) {
            0...12 => @fromBackingInt(@intCast(w)),
            else => null,
        };
    }

    /// The caller-side verdict of a reply status: null = success (the
    /// payload is the verb's reply); an error otherwise. `refused` carries
    /// its own errno in the payload and is mapped by `WireError.toError`.
    pub fn toError(self: Status) ?errors.Error {
        return switch (self) {
            .ok => null,
            .unknown_verb, .schema => error.WireSchemaRefused,
            .malformed => error.WireMalformed,
            .not_manager => error.NotManager,
            .not_holder => error.StaleHolderView,
            .stale_term => error.StaleOwnerEra,
            .refused => error.WireRefused,
            .rejected => error.Rejected,
            .deferred => error.Deferred,
            .not_member => error.MembershipPending,
            .unknown_lease => error.UnknownLease,
            .busy => error.Busy,
        };
    }
};

/// The typed retryable classes (charter §5.5; E:60-86) — ONE word beside
/// the errno so no classifier on either side reads the prose. `none` = an
/// ordinary errno; an unknown word decodes as `none` — never a guessed
/// class. The charter names `Deferred` where the reference had
/// `HolderDeferred` (7) beside the manager's `STATUS_DEFERRED`; the port
/// maps both onto `error.Deferred` (decision 0002 row 224).
pub const RefusalClass = enum(u8) {
    none = 0,
    slot_moved = 1,
    stale_holder_view = 2,
    holder_unreachable = 3,
    membership_pending = 4,
    listener_refused = 5,
    stale_owner_era = 6,
    holder_deferred = 7,

    pub fn fromWire(w: u8) RefusalClass {
        return switch (w) {
            0...7 => @fromBackingInt(@intCast(w)),
            else => .none,
        };
    }

    pub fn toError(self: RefusalClass) ?errors.Error {
        return switch (self) {
            .none => null,
            .slot_moved => error.SlotMoved,
            .stale_holder_view => error.StaleHolderView,
            .holder_unreachable => error.HolderUnreachable,
            .membership_pending => error.MembershipPending,
            .listener_refused => error.ListenerRefused,
            .stale_owner_era => error.StaleOwnerEra,
            .holder_deferred => error.Deferred,
        };
    }

    pub fn fromError(err: errors.Error) RefusalClass {
        return switch (err) {
            error.SlotMoved => .slot_moved,
            error.StaleHolderView => .stale_holder_view,
            error.HolderUnreachable => .holder_unreachable,
            error.MembershipPending => .membership_pending,
            error.ListenerRefused => .listener_refused,
            error.StaleOwnerEra => .stale_owner_era,
            error.Deferred => .holder_deferred,
            else => .none,
        };
    }
};

/// Every retryable class answers EAGAIN (charter §5.5 "surface as EAGAIN,
/// never EIO/EINVAL"); a failed screen is `rejected` (EINVAL at the
/// caller — a peer bug, never retried).
pub fn errnoOf(err: errors.Error) linux.E {
    return errors.toErrno(err);
}

/// The reply payload under `refused`/`deferred`/`busy`/`rejected`: the
/// errno, the typed class, a slot/holder pair for `SlotMoved` and
/// `HolderUnreachable`, a retry hint (the membership grace refusal's
/// `retry_after_ms`), and a reason string (a log rendering, never parsed).
pub const WireErrorHead = extern struct {
    errno: i32,
    class: u8,
    reserved: [3]u8,
    retry_after_ms: u32,
    slot: u16,
    reason_len: u16,
    holder: u32,
    reserved2: u32,
};

pub const WireError = struct {
    head: WireErrorHead,
    reason: []const u8,

    /// A reason string longer than this is a bug, not a payload.
    pub const REASON_MAX: usize = 1024;

    pub fn init(err: errors.Error, reason: []const u8) WireError {
        return .{
            .head = .{
                .errno = @intCast(@backingInt(errors.toErrno(err))),
                .class = @backingInt(RefusalClass.fromError(err)),
                .reserved = @splat(0),
                .retry_after_ms = 0,
                .slot = 0,
                .reason_len = @intCast(@min(reason.len, REASON_MAX)),
                .holder = 0,
                .reserved2 = 0,
            },
            .reason = reason[0..@min(reason.len, REASON_MAX)],
        };
    }

    pub fn encode(self: WireError, b: *codec.Builder) codec.BuildError!void {
        try b.put(WireErrorHead, self.head);
        try b.putBytes(self.reason);
        try b.putPadding(self.reason.len, codec.TAIL_ALIGN);
    }

    pub fn decode(c: *codec.Cursor) codec.Error!WireError {
        const head = try c.take(WireErrorHead);
        const n = try codec.countAtMost(head.reason_len, REASON_MAX);
        const reason = try c.takeBytes(n);
        try c.takePadding(n, codec.TAIL_ALIGN);
        return .{ .head = head, .reason = reason };
    }

    /// The typed class first (a class word beats the errno), else the errno
    /// through the ONE table.
    pub fn toError(self: WireError) errors.Error {
        if (RefusalClass.fromWire(self.head.class).toError()) |e| return e;
        // An errno word that names no kernel errno is Unexpected (EIO),
        // never a reinterpretation — and never handed to the table's
        // logger, which renders the NAME.
        const e = std.enums.fromInt(linux.E, self.head.errno) orelse return error.Unexpected;
        if (std.enums.tagName(linux.E, e) == null) return error.Unexpected;
        return errors.fromErrno(e);
    }
};

// ---------------------------------------------------------------------------
// The header codec.
// ---------------------------------------------------------------------------

pub const Header = struct {
    flags: u16,
    len: u32,
    verb: u16,
    status: u16,
    correlation: u64,
    seq: u64,
    schema: u32,

    pub fn macPresent(self: Header) bool {
        return self.flags & FLAG_MAC_PRESENT != 0;
    }
    pub fn isReply(self: Header) bool {
        return self.flags & FLAG_REPLY != 0;
    }
    pub fn peerToCoord(self: Header) bool {
        return self.flags & FLAG_DIR_PEER_TO_COORD != 0;
    }
};

/// Why a header was refused (each a counted refusal class at the session).
pub const HeaderRefusal = enum {
    bad_magic,
    bad_version,
    unknown_flags,
    reserved_nonzero,
    len_over_cap,
};

/// The pre-payload screen: magic, framing version, known flags, reserved
/// zero, `len ≤ cap`. The xxh3 is verified AFTER the payload arrives
/// (`verifyChecksum`) because it covers the payload too.
pub fn decodeHeader(bytes: *const [HEADER_LEN]u8, cap: u32) union(enum) { ok: Header, refused: HeaderRefusal } {
    const w = std.mem.bytesToValue(FrameHeaderWire, bytes);
    if (!std.mem.eql(u8, &w.magic, &MAGIC)) return .{ .refused = .bad_magic };
    if (std.mem.readInt(u16, &w.version_le, .little) != FRAMING_VERSION) return .{ .refused = .bad_version };
    const flags = std.mem.readInt(u16, &w.flags_le, .little);
    if (flags & ~FLAGS_KNOWN != 0) return .{ .refused = .unknown_flags };
    if (std.mem.readInt(u32, &w.reserved_le, .little) != 0) return .{ .refused = .reserved_nonzero };
    const len = std.mem.readInt(u32, &w.len_le, .little);
    if (len > cap) return .{ .refused = .len_over_cap };
    return .{ .ok = .{
        .flags = flags,
        .len = len,
        .verb = std.mem.readInt(u16, &w.verb_le, .little),
        .status = std.mem.readInt(u16, &w.status_le, .little),
        .correlation = std.mem.readInt(u64, &w.correlation_le, .little),
        .seq = std.mem.readInt(u64, &w.seq_le, .little),
        .schema = std.mem.readInt(u32, &w.schema_le, .little),
    } };
}

/// xxh3-64 over `header[0..40] ‖ payload`.
pub fn frameChecksum(header: *const [HEADER_LEN]u8, payload: []const u8) u64 {
    var h = std.hash.XxHash3.init(0);
    h.update(header[0..XXH3_OFF]);
    h.update(payload);
    return h.final();
}

pub fn verifyChecksum(header: *const [HEADER_LEN]u8, payload: []const u8) bool {
    return std.mem.readInt(u64, header[XXH3_OFF..][0..8], .little) == frameChecksum(header, payload);
}

/// Stamp a header (the checksum last, over the payload the caller will
/// send after it).
pub fn encodeHeader(h: Header, payload: []const u8, out: *[HEADER_LEN]u8) void {
    var w: FrameHeaderWire = undefined;
    w.magic = MAGIC;
    std.mem.writeInt(u16, &w.version_le, FRAMING_VERSION, .little);
    std.mem.writeInt(u16, &w.flags_le, h.flags, .little);
    std.mem.writeInt(u32, &w.len_le, @intCast(payload.len), .little);
    std.mem.writeInt(u16, &w.verb_le, h.verb, .little);
    std.mem.writeInt(u16, &w.status_le, h.status, .little);
    std.mem.writeInt(u64, &w.correlation_le, h.correlation, .little);
    std.mem.writeInt(u64, &w.seq_le, h.seq, .little);
    std.mem.writeInt(u32, &w.schema_le, h.schema, .little);
    w.reserved_le = @splat(0);
    w.xxh3_le = @splat(0);
    out.* = std.mem.toBytes(w);
    std.mem.writeInt(u64, out[XXH3_OFF..][0..8], frameChecksum(out, payload), .little);
}

// ---------------------------------------------------------------------------
// The MAC trailer.
// ---------------------------------------------------------------------------

pub const Hmac = std.crypto.auth.hmac.sha2.HmacSha256;
pub const SessionKey = [Hmac.key_length]u8;

comptime {
    std.debug.assert(Hmac.mac_length == MAC_BYTES);
}

/// `HMAC-SHA256(session_key, header[0..48] ‖ payload)`.
pub fn computeMac(key: *const SessionKey, header: *const [HEADER_LEN]u8, payload: []const u8, out: *[MAC_BYTES]u8) void {
    var h = Hmac.init(key);
    h.update(header);
    h.update(payload);
    h.final(out);
}

/// Constant-time compare (`tag_eq`'s law: byte-for-byte AND length-checked
/// by the type).
pub fn verifyMac(key: *const SessionKey, header: *const [HEADER_LEN]u8, payload: []const u8, mac: *const [MAC_BYTES]u8) bool {
    var expect: [MAC_BYTES]u8 = undefined;
    computeMac(key, header, payload, &expect);
    return std.crypto.timing_safe.eql([MAC_BYTES]u8, expect, mac.*);
}

/// A whole frame into a caller buffer: header ‖ payload ‖ MAC?. Returns the
/// bytes written; `error.Overflow` when the buffer is short.
pub fn encodeFrame(h: Header, payload: []const u8, key: ?*const SessionKey, out: []u8) codec.BuildError!usize {
    std.debug.assert((key != null) == h.macPresent());
    const total = HEADER_LEN + payload.len + @as(usize, if (key != null) MAC_BYTES else 0);
    if (out.len < total) return error.Overflow;
    encodeHeader(h, payload, out[0..HEADER_LEN]);
    @memcpy(out[HEADER_LEN..][0..payload.len], payload);
    if (key) |k| computeMac(k, out[0..HEADER_LEN], payload, out[HEADER_LEN + payload.len ..][0..MAC_BYTES]);
    return total;
}

pub const Decoded = struct { header: Header, payload: []const u8 };

pub const DecodeRefusal = union(enum) {
    header: HeaderRefusal,
    /// Fewer bytes than the header claims (payload or trailer).
    short,
    checksum,
    mac,
    /// The expected session frame carried no MAC (or a handshake frame did).
    mac_flag,
};

/// Decode a whole in-memory frame (the loopback hub's path and the tests';
/// the socket path reads the header, then the payload, then the trailer
/// and calls the same primitives). `key` null ⇒ a MAC-less frame is
/// expected; present ⇒ required and verified.
pub fn decodeFrame(bytes: []const u8, cap: u32, key: ?*const SessionKey) union(enum) { ok: Decoded, refused: DecodeRefusal } {
    if (bytes.len < HEADER_LEN) return .{ .refused = .short };
    const hb = bytes[0..HEADER_LEN];
    const h = switch (decodeHeader(hb, cap)) {
        .ok => |h| h,
        .refused => |r| return .{ .refused = .{ .header = r } },
    };
    if (h.macPresent() != (key != null)) return .{ .refused = .mac_flag };
    const trailer: usize = if (key != null) MAC_BYTES else 0;
    if (bytes.len != HEADER_LEN + @as(usize, h.len) + trailer) return .{ .refused = .short };
    const payload = bytes[HEADER_LEN..][0..h.len];
    if (!verifyChecksum(hb, payload)) return .{ .refused = .checksum };
    if (key) |k| {
        if (!verifyMac(k, hb, payload, bytes[HEADER_LEN + h.len ..][0..MAC_BYTES])) return .{ .refused = .mac };
    }
    return .{ .ok = .{ .header = h, .payload = payload } };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "the header is 48 B with the spec's offsets; encode → decode round-trips every field" {
    const h: Header = .{ .flags = FLAG_MAC_PRESENT | FLAG_REPLY, .len = 3, .verb = 0x0500, .status = 9, .correlation = 0xABCDEF, .seq = 42, .schema = 1 };
    var hb: [HEADER_LEN]u8 = undefined;
    encodeHeader(h, "abc", &hb);
    try testing.expectEqualSlices(u8, "SQZW", hb[0..4]);
    const back = decodeHeader(&hb, CONTROL_CAP).ok;
    try testing.expectEqual(h, back);
    try testing.expect(verifyChecksum(&hb, "abc"));
    try testing.expect(!verifyChecksum(&hb, "abd"));
}

test "header screen: magic, version, unknown flags, reserved, len over cap — each refused by name BEFORE any payload read" {
    const h: Header = .{ .flags = 0, .len = 0, .verb = 0, .status = 0, .correlation = 0, .seq = 0, .schema = 6 };
    var hb: [HEADER_LEN]u8 = undefined;
    encodeHeader(h, "", &hb);
    var bad = hb;
    bad[0] = 'X';
    try testing.expectEqual(HeaderRefusal.bad_magic, decodeHeader(&bad, CONTROL_CAP).refused);
    bad = hb;
    bad[4] = 2;
    try testing.expectEqual(HeaderRefusal.bad_version, decodeHeader(&bad, CONTROL_CAP).refused);
    bad = hb;
    bad[7] = 0x80;
    try testing.expectEqual(HeaderRefusal.unknown_flags, decodeHeader(&bad, CONTROL_CAP).refused);
    bad = hb;
    bad[36] = 1;
    try testing.expectEqual(HeaderRefusal.reserved_nonzero, decodeHeader(&bad, CONTROL_CAP).refused);
    bad = hb;
    std.mem.writeInt(u32, bad[8..12], HANDSHAKE_CAP + 1, .little);
    try testing.expectEqual(HeaderRefusal.len_over_cap, decodeHeader(&bad, HANDSHAKE_CAP).refused);
    try testing.expectEqual(Header{ .flags = 0, .len = HANDSHAKE_CAP + 1, .verb = 0, .status = 0, .correlation = 0, .seq = 0, .schema = 6 }, decodeHeader(&bad, CONTROL_CAP).ok);
}

test "class caps are ordered and named; the chunk is inside the cap; the bulk class is the job fabric's 16 MiB" {
    try testing.expect(Class.handshake.cap() < Class.control.cap());
    try testing.expect(Class.control.cap() < Class.bulk.cap());
    try testing.expectEqual(@as(u32, 8 << 10), Class.handshake.cap());
    try testing.expectEqual(@as(u32, 1 << 20), Class.control.cap());
    try testing.expectEqual(@as(u32, 16 << 20), Class.bulk.cap());
    try testing.expectEqual(MAX_FRAME_PAYLOAD, Class.bulk.cap());
}

test "a MACed frame round-trips; tamper, wrong key, reflection (dir flip), reorder (seq) and a stripped MAC are all refused" {
    const key: SessionKey = @splat(7);
    var other: SessionKey = key;
    other[0] ^= 1;
    const h: Header = .{ .flags = FLAG_MAC_PRESENT, .len = 5, .verb = 0x0500, .status = 0, .correlation = 1, .seq = 3, .schema = 1 };
    var buf: [256]u8 = undefined;
    const n = try encodeFrame(h, "hello", &key, &buf);
    try testing.expectEqual(HEADER_LEN + 5 + MAC_BYTES, n);
    const d = decodeFrame(buf[0..n], CONTROL_CAP, &key).ok;
    try testing.expectEqualStrings("hello", d.payload);
    try testing.expectEqual(@as(u64, 3), d.header.seq);
    // Tamper a payload byte: the checksum catches it first; re-stamp the
    // checksum and the MAC catches it.
    var t = buf;
    t[HEADER_LEN] ^= 1;
    try testing.expectEqual(DecodeRefusal.checksum, decodeFrame(t[0..n], CONTROL_CAP, &key).refused);
    std.mem.writeInt(u64, t[XXH3_OFF..][0..8], frameChecksum(t[0..HEADER_LEN], t[HEADER_LEN..][0..5]), .little);
    try testing.expectEqual(DecodeRefusal.mac, decodeFrame(t[0..n], CONTROL_CAP, &key).refused);
    // Wrong key.
    try testing.expectEqual(DecodeRefusal.mac, decodeFrame(buf[0..n], CONTROL_CAP, &other).refused);
    // Reflection: the direction bit flipped (checksum re-stamped).
    var r = buf;
    r[6] ^= @intCast(FLAG_DIR_PEER_TO_COORD);
    std.mem.writeInt(u64, r[XXH3_OFF..][0..8], frameChecksum(r[0..HEADER_LEN], r[HEADER_LEN..][0..5]), .little);
    try testing.expectEqual(DecodeRefusal.mac, decodeFrame(r[0..n], CONTROL_CAP, &key).refused);
    // Reorder: the seq word changed.
    var s = buf;
    s[24] = 4;
    std.mem.writeInt(u64, s[XXH3_OFF..][0..8], frameChecksum(s[0..HEADER_LEN], s[HEADER_LEN..][0..5]), .little);
    try testing.expectEqual(DecodeRefusal.mac, decodeFrame(s[0..n], CONTROL_CAP, &key).refused);
    // A session frame without the trailer; a trailer where none is expected.
    try testing.expectEqual(DecodeRefusal.short, decodeFrame(buf[0 .. n - MAC_BYTES], CONTROL_CAP, &key).refused);
    try testing.expectEqual(DecodeRefusal.mac_flag, decodeFrame(buf[0..n], CONTROL_CAP, null).refused);
    const hs: Header = .{ .flags = 0, .len = 0, .verb = 0xFFF0, .status = 0, .correlation = 0, .seq = 0, .schema = 6 };
    const m = try encodeFrame(hs, "", null, &buf);
    try testing.expectEqual(HEADER_LEN, m);
    try testing.expectEqual(DecodeRefusal.mac_flag, decodeFrame(buf[0..m], HANDSHAKE_CAP, &key).refused);
    try testing.expectEqual(@as(usize, 0), decodeFrame(buf[0..m], HANDSHAKE_CAP, null).ok.payload.len);
}

test "the status table is total over the wire word; every retryable class is EAGAIN; a screen failure is EINVAL; unknown class words decode as none" {
    inline for (@typeInfo(Status).@"enum".field_values) |value| {
        const s: Status = @fromBackingInt(@intCast(value));
        try testing.expectEqual(s, Status.fromWire(value).?);
    }
    try testing.expectEqual(@as(?Status, null), Status.fromWire(13));
    try testing.expectEqual(@as(?Status, null), Status.fromWire(0xFFFF));
    const retryable = [_]Status{ .deferred, .stale_term, .not_member, .not_holder };
    for (retryable) |s| try testing.expectEqual(linux.E.AGAIN, errnoOf(s.toError().?));
    try testing.expectEqual(linux.E.INVAL, errnoOf(Status.rejected.toError().?));
    try testing.expectEqual(linux.E.INVAL, errnoOf(Status.schema.toError().?));
    try testing.expectEqual(linux.E.INVAL, errnoOf(Status.malformed.toError().?));
    try testing.expectEqual(linux.E.BUSY, errnoOf(Status.busy.toError().?));
    try testing.expectEqual(@as(?errors.Error, null), Status.ok.toError());
    inline for (@typeInfo(RefusalClass).@"enum".field_values) |value| {
        const c: RefusalClass = @fromBackingInt(@intCast(value));
        if (c != .none) {
            try testing.expectEqual(linux.E.AGAIN, errnoOf(c.toError().?));
            try testing.expectEqual(c, RefusalClass.fromError(c.toError().?));
        }
    }
    try testing.expectEqual(RefusalClass.none, RefusalClass.fromWire(8));
    try testing.expectEqual(RefusalClass.none, RefusalClass.fromWire(255));
    try testing.expectEqual(RefusalClass.none, RefusalClass.fromError(error.InputOutput));
}

test "WireError: the class word beats the errno; a plain errno rides the ONE table; the reason is bounded and padded" {
    var buf: [2048]u8 = undefined;
    var b: codec.Builder = .init(&buf);
    try WireError.init(error.SlotMoved, "slot 7 moved").encode(&b);
    const d = try codec.decodePayload(WireError, b.written());
    try testing.expectEqual(error.SlotMoved, d.toError());
    try testing.expectEqualStrings("slot 7 moved", d.reason);
    try testing.expectEqual(@as(i32, @intCast(@backingInt(linux.E.AGAIN))), d.head.errno);
    var b2: codec.Builder = .init(&buf);
    try WireError.init(error.NoSpaceLeft, "").encode(&b2);
    const d2 = try codec.decodePayload(WireError, b2.written());
    try testing.expectEqual(error.NoSpaceLeft, d2.toError());
    try testing.expectEqual(RefusalClass.none, RefusalClass.fromWire(d2.head.class));
    // An out-of-table errno is Unexpected (EIO), never a guess.
    var b3: codec.Builder = .init(&buf);
    var e = WireError.init(error.NoSpaceLeft, "x");
    e.head.errno = 9999;
    try e.encode(&b3);
    // (`errors.fromErrno` WARNS on an out-of-table errno; the test runner
    // fails only error-level logs.)
    try testing.expectEqual(error.Unexpected, (try codec.decodePayload(WireError, b3.written())).toError());
    // A reason past the cap is truncated at encode, and a lying reason_len is Malformed.
    const long: [2000]u8 = @splat('r');
    var b4: codec.Builder = .init(&buf);
    try WireError.init(error.Busy, &long).encode(&b4);
    try testing.expectEqual(WireError.REASON_MAX, (try codec.decodePayload(WireError, b4.written())).reason.len);
    var lying = buf;
    std.mem.writeInt(u16, lying[14..16], 1025, .little);
    try testing.expectError(error.Malformed, codec.decodePayload(WireError, lying[0..b4.pos]));
}

fn frameFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [4096]u8 = undefined;
    const len = smith.slice(&buf);
    const key: SessionKey = @splat(1);
    _ = decodeFrame(buf[0..len], CONTROL_CAP, null);
    _ = decodeFrame(buf[0..len], CONTROL_CAP, &key);
    _ = decodeFrame(buf[0..len], HANDSHAKE_CAP, null);
    if (len >= HEADER_LEN) _ = decodeHeader(buf[0..HEADER_LEN], CONTROL_CAP);
    _ = codec.decodePayload(WireError, buf[0..len]) catch {};
}

test "fuzz: the frame and header decoders are total on arbitrary bytes" {
    try fuzzkit.fuzz({}, frameFuzz, .{});
}
