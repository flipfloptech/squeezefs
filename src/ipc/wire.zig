//! The rendezvous wire (charter §7; spec `docs/porting/il_session.md` §2):
//! the `user.squeezefs.il0` bootstrap blob the mount answers, and the
//! eleven control frames — one `SOCK_SEQPACKET` datagram each, a `u32`
//! tag first. Every frame is an `extern struct` decoded by `@sizeOf`
//! equality (`Truncated` otherwise); byte fields are bytes (NUL-padded,
//! always NUL-terminated inside their width); every decoder is total on
//! garbage (charter §11 "fuzz every untrusted decoder"). No wire
//! compatibility with the reference is owed (decision 0002 rows 300–305):
//!   - `SessionOk` carries no geometry — the SEALED header is the one
//!     authority (the client checks `compute(header.geometry).total_bytes ==
//!     fstat(memfd).st_size`); it carries the additive feature bits;
//!   - HELLO carries no pid/uid (`SO_PEERCRED` already says who connected)
//!     but the client KIND (shim | sdk), the SDK ABI word and the SDK's
//!     geometry request; `Refuse` names the daemon's accepted SDK ABI window
//!     so a refused SDK prints both values without a blob field;
//!   - `BindOk.rights` is a bitfield `{read, write, odirect}`; `Bind` and
//!     `BindOk` carry the SDK's strengthen-only durability word;
//!   - the refusal class for a wrong `st_dev` is `mount` (not "Mode") and the
//!     access-mode arm has its own `rights` class.

const std = @import("std");
const layout = @import("layout.zig");

const assert = std.debug.assert;

pub const IPC_ABI = layout.IPC_ABI;

/// The SDK's frozen-subset version word (bumps only on a breaking change to
/// the frozen subset; additive evolution rides `SessionOk.features`).
pub const SDK_ABI_CURRENT: u32 = 1;
/// The daemon's accepted window `{cur, cur−1}` (pruned as a release act).
pub const sdk_abi_accepted = [_]u32{SDK_ABI_CURRENT};

/// `BUILD_COMMIT_LEN` 48: the design's `[u8; 41]` could not hold the
/// 46-char `<12 hex>-dirty`-class form the skew gate must SEE.
pub const BUILD_COMMIT_LEN = 48;
/// `sockaddr_un.sun_path`: 107 + NUL.
pub const SOCKET_NAME_LEN = 108;
pub const NONCE_LEN = 32;
/// Admin JSON bodies; data-plane frames are tiny.
pub const CTL_MSG_MAX: usize = 64 * 1024;
pub const ADMIN_VERB_MAX: usize = 64;
pub const ADMIN_ARG_MAX: usize = 60 * 1024;

pub const ClientKind = enum(u32) {
    shim = 0,
    sdk = 1,
    _,
};

/// The SDK's per-open durability word (strengthen-only: no weakener value
/// exists — a handle may never weaken below its mount's class).
pub const Durability = enum(u32) {
    mount = 0,
    sync = 1,
    _,
};

/// `BindOk.rights` bits.
pub const Rights = packed struct(u8) {
    read: bool = false,
    write: bool = false,
    odirect: bool = false,
    _pad: u5 = 0,
};

pub const RefuseClass = enum(u32) {
    /// `abi`/build-commit/SDK-window mismatch, an unknown client kind.
    version = 1,
    /// A nonce that is neither the current nor the previous one.
    nonce = 2,
    /// No fd attached; `O_PATH`; not a regular file; `O_APPEND`/`O_SYNC`/
    /// `O_DSYNC`/`O_TMPFILE`; `st_nlink == 0`; a reserved flag bit.
    flags = 3,
    /// `st_dev` is not the mount's.
    mount = 4,
    /// The arena cap, the per-uid cap, or the shed target.
    budget = 5,
    /// `SO_PEERCRED` unreadable or outside the admitted set (the admin lane).
    peercred = 6,
    /// The daemon failed to build the session (memfd, spawn).
    internal = 7,
    /// The mount is not armed (`-o interception` absent).
    disabled = 8,
    /// An access mode outside `{RDONLY, WRONLY, RDWR}`.
    rights = 9,

    pub fn fromU32(v: u32) ?RefuseClass {
        return switch (v) {
            1...9 => @fromBackingInt(@intCast(v)),
            else => null,
        };
    }
};

/// `"unknown"`, empty, or ending in `-dirty`: a build identity the same-
/// commit law refuses on EITHER side (charter §7 "`-dirty` refuses").
pub fn buildCommitDegenerate(c: []const u8) bool {
    return c.len == 0 or std.mem.eql(u8, c, "unknown") or std.mem.endsWith(u8, c, "-dirty");
}

/// A NUL-padded byte field: the bytes before the first NUL (the whole
/// width if none — the encoder never produces that; decoders refuse it).
pub fn fieldStr(field: []const u8) []const u8 {
    const n = std.mem.indexOfScalar(u8, field, 0) orelse field.len;
    return field[0..n];
}

pub const EncodeError = error{StringTooLong};

/// Copy `s` into a NUL-padded field, requiring a NUL inside the width.
pub fn setField(field: []u8, s: []const u8) EncodeError!void {
    if (s.len >= field.len) return error.StringTooLong;
    @memset(field, 0);
    @memcpy(field[0..s.len], s);
}

pub const DecodeError = error{
    /// Shorter (or longer) than the frame's `@sizeOf`.
    Truncated,
    /// A bad magic, an unknown tag, or a field with no NUL inside its width.
    BadString,
    BadClass,
    BadTag,
};

// ---------------------------------------------------------------------------
// The `user.squeezefs.il0` blob: synthesized by GETXATTR only when the
// mount's session host is armed; never on-disk bytes; 312 B, fixed.
// ---------------------------------------------------------------------------

pub const BLOB_MAGIC: u64 = std.mem.readInt(u64, "SQZIL0\x00\x00", .little);
pub const BLOB_LEN: usize = 312;
/// The virtual xattr the mount answers it under (filtered from listxattr;
/// EPERM on set — the reserved family's screen).
pub const IL0_XATTR = "user.squeezefs.il0";

pub const Il0Blob = extern struct {
    magic: u64 = BLOB_MAGIC,
    abi: u32 = IPC_ABI,
    /// Reserved feature bits (0).
    flags: u32 = 0,
    build_commit: [BUILD_COMMIT_LEN]u8 = @splat(0),
    /// The abstract AF_UNIX name WITHOUT the leading NUL.
    socket: [SOCKET_NAME_LEN]u8 = @splat(0),
    /// The filesystem-path socket; empty when the path rung is disabled or
    /// degraded.
    socket_path: [SOCKET_NAME_LEN]u8 = @splat(0),
    /// The current anti-replay nonce.
    nonce: [NONCE_LEN]u8 = @splat(0),

    pub fn bytes(b: *const Il0Blob) *const [BLOB_LEN]u8 {
        return @ptrCast(b);
    }

    pub fn decode(raw: []const u8) DecodeError!Il0Blob {
        if (raw.len < BLOB_LEN) return error.Truncated;
        var b: Il0Blob = undefined;
        @memcpy(std.mem.asBytes(&b), raw[0..BLOB_LEN]);
        if (b.magic != BLOB_MAGIC) return error.BadString;
        for ([_][]const u8{ &b.build_commit, &b.socket, &b.socket_path }) |f| {
            if (std.mem.indexOfScalar(u8, f, 0) == null) return error.BadString;
        }
        return b;
    }

    pub fn buildCommit(b: *const Il0Blob) []const u8 {
        return fieldStr(&b.build_commit);
    }
    pub fn socketName(b: *const Il0Blob) []const u8 {
        return fieldStr(&b.socket);
    }
    pub fn socketPath(b: *const Il0Blob) []const u8 {
        return fieldStr(&b.socket_path);
    }
};

comptime {
    assert(@sizeOf(Il0Blob) == BLOB_LEN);
    assert(@offsetOf(Il0Blob, "magic") == 0);
    assert(@offsetOf(Il0Blob, "abi") == 8);
    assert(@offsetOf(Il0Blob, "flags") == 12);
    assert(@offsetOf(Il0Blob, "build_commit") == 16);
    assert(@offsetOf(Il0Blob, "socket") == 64);
    assert(@offsetOf(Il0Blob, "socket_path") == 172);
    assert(@offsetOf(Il0Blob, "nonce") == 280);
}

// ---------------------------------------------------------------------------
// The eleven control frames.
// ---------------------------------------------------------------------------

pub const Tag = enum(u32) {
    hello = 1,
    session_ok = 2,
    refuse = 3,
    bind = 4,
    bind_ok = 5,
    bind_refused = 6,
    unbind = 7,
    admin_hello = 8,
    admin_ok = 9,
    admin_req = 10,
    admin_reply = 11,
    _,
};

/// Reads the tag of any frame (the dispatch read before the typed decode).
pub fn tagOf(raw: []const u8) DecodeError!Tag {
    if (raw.len < 4) return error.Truncated;
    const v = std.mem.readInt(u32, raw[0..4], .little);
    return switch (v) {
        1...11 => @fromBackingInt(@intCast(v)),
        else => error.BadTag,
    };
}

/// shim/sdk → daemon, with exactly ONE fd (a dup of the credential fd).
pub const Hello = extern struct {
    tag: u32 = @backingInt(Tag.hello),
    abi: u32 = IPC_ABI,
    client_kind: u32 = @backingInt(ClientKind.shim),
    /// 0 for kind shim.
    sdk_abi: u32 = 0,
    build_commit: [BUILD_COMMIT_LEN]u8 = @splat(0),
    nonce: [NONCE_LEN]u8 = @splat(0),
    /// The SDK's geometry request (0 = the daemon's default).
    arena_bytes_req: u64 = 0,
    slots_req: u32 = 0,
    _pad: u32 = 0,

    pub fn buildCommit(h: *const Hello) []const u8 {
        return fieldStr(&h.build_commit);
    }
};

/// daemon → client, with the sealed memfd. Geometry authority is the
/// sealed header, not the frame.
pub const SessionOk = extern struct {
    tag: u32 = @backingInt(Tag.session_ok),
    _pad: u32 = 0,
    /// Additive feature bits (0 today).
    features: u64 = 0,
};

pub const Refuse = extern struct {
    tag: u32 = @backingInt(Tag.refuse),
    class: u32,
    /// The daemon's accepted SDK ABI window (so a refused SDK names both
    /// values); 0/0 when not an SDK version refusal.
    sdk_abi_min: u32 = 0,
    sdk_abi_max: u32 = 0,

    pub fn refuseClass(r: *const Refuse) DecodeError!RefuseClass {
        return RefuseClass.fromU32(r.class) orelse error.BadClass;
    }
};

/// client → daemon, with exactly ONE fd (the fd to bind).
pub const Bind = extern struct {
    tag: u32 = @backingInt(Tag.bind),
    durability: u32 = @backingInt(Durability.mount),
};

pub const BindOk = extern struct {
    tag: u32 = @backingInt(Tag.bind_ok),
    rights: Rights,
    _pad: [3]u8 = @splat(0),
    binding_id: u64,
    ino: u64,
    durability_granted: u32 = @backingInt(Durability.mount),
    _pad2: u32 = 0,
};

pub const BindRefused = extern struct {
    tag: u32 = @backingInt(Tag.bind_refused),
    class: u32,

    pub fn refuseClass(r: *const BindRefused) DecodeError!RefuseClass {
        return RefuseClass.fromU32(r.class) orelse error.BadClass;
    }
};

/// The last close of the refcounted binding.
pub const Unbind = extern struct {
    tag: u32 = @backingInt(Tag.unbind),
    _pad: u32 = 0,
    binding_id: u64,
};

/// The CLI's admin lane: HELLO's identity with no fd.
pub const AdminHello = extern struct {
    tag: u32 = @backingInt(Tag.admin_hello),
    abi: u32 = IPC_ABI,
    build_commit: [BUILD_COMMIT_LEN]u8 = @splat(0),
    nonce: [NONCE_LEN]u8 = @splat(0),

    pub fn buildCommit(h: *const AdminHello) []const u8 {
        return fieldStr(&h.build_commit);
    }
};

pub const AdminOk = extern struct {
    tag: u32 = @backingInt(Tag.admin_ok),
    _pad: u32 = 0,
};

/// `AdminReq{verb ≤ 64, arg ≤ 60 KiB}`: a 16 B header, then the two
/// strings back to back; the frame's length is exactly the header plus
/// both lengths.
pub const AdminReqHeader = extern struct {
    tag: u32 = @backingInt(Tag.admin_req),
    verb_len: u32,
    arg_len: u32,
    _pad: u32 = 0,
};

pub const AdminReq = struct {
    verb: []const u8,
    arg: []const u8,

    pub fn encodedLen(r: AdminReq) usize {
        return @sizeOf(AdminReqHeader) + r.verb.len + r.arg.len;
    }

    pub fn encode(r: AdminReq, out: []u8) (EncodeError || error{BufferTooSmall})!usize {
        if (r.verb.len > ADMIN_VERB_MAX or r.arg.len > ADMIN_ARG_MAX) return error.StringTooLong;
        const n = r.encodedLen();
        if (out.len < n) return error.BufferTooSmall;
        const h: AdminReqHeader = .{ .verb_len = @intCast(r.verb.len), .arg_len = @intCast(r.arg.len) };
        @memcpy(out[0..@sizeOf(AdminReqHeader)], std.mem.asBytes(&h));
        @memcpy(out[@sizeOf(AdminReqHeader)..][0..r.verb.len], r.verb);
        @memcpy(out[@sizeOf(AdminReqHeader) + r.verb.len ..][0..r.arg.len], r.arg);
        return n;
    }

    /// Total: a lying length is never a slice authority — both lengths are
    /// capped and must sum to the datagram exactly.
    pub fn decode(raw: []const u8) DecodeError!AdminReq {
        if (raw.len < @sizeOf(AdminReqHeader)) return error.Truncated;
        var h: AdminReqHeader = undefined;
        @memcpy(std.mem.asBytes(&h), raw[0..@sizeOf(AdminReqHeader)]);
        if (h.tag != @backingInt(Tag.admin_req)) return error.BadTag;
        if (h.verb_len > ADMIN_VERB_MAX or h.arg_len > ADMIN_ARG_MAX) return error.BadString;
        const n = @sizeOf(AdminReqHeader) + @as(usize, h.verb_len) + @as(usize, h.arg_len);
        if (raw.len != n) return error.Truncated;
        return .{
            .verb = raw[@sizeOf(AdminReqHeader)..][0..h.verb_len],
            .arg = raw[@sizeOf(AdminReqHeader) + h.verb_len ..][0..h.arg_len],
        };
    }
};

pub const AdminReplyHeader = extern struct {
    tag: u32 = @backingInt(Tag.admin_reply),
    ok: u32,
    body_len: u32,
    _pad: u32 = 0,
};

pub const AdminReply = struct {
    ok: bool,
    body: []const u8,

    pub fn encodedLen(r: AdminReply) usize {
        return @sizeOf(AdminReplyHeader) + r.body.len;
    }

    pub fn encode(r: AdminReply, out: []u8) (EncodeError || error{BufferTooSmall})!usize {
        if (r.body.len > ADMIN_ARG_MAX) return error.StringTooLong;
        const n = r.encodedLen();
        if (out.len < n) return error.BufferTooSmall;
        const h: AdminReplyHeader = .{ .ok = @intFromBool(r.ok), .body_len = @intCast(r.body.len) };
        @memcpy(out[0..@sizeOf(AdminReplyHeader)], std.mem.asBytes(&h));
        @memcpy(out[@sizeOf(AdminReplyHeader)..][0..r.body.len], r.body);
        return n;
    }

    pub fn decode(raw: []const u8) DecodeError!AdminReply {
        if (raw.len < @sizeOf(AdminReplyHeader)) return error.Truncated;
        var h: AdminReplyHeader = undefined;
        @memcpy(std.mem.asBytes(&h), raw[0..@sizeOf(AdminReplyHeader)]);
        if (h.tag != @backingInt(Tag.admin_reply)) return error.BadTag;
        if (h.ok > 1) return error.BadString;
        if (h.body_len > ADMIN_ARG_MAX) return error.BadString;
        if (raw.len != @sizeOf(AdminReplyHeader) + @as(usize, h.body_len)) return error.Truncated;
        return .{ .ok = h.ok == 1, .body = raw[@sizeOf(AdminReplyHeader)..][0..h.body_len] };
    }
};

comptime {
    assert(@sizeOf(Hello) == 112);
    assert(@offsetOf(Hello, "build_commit") == 16);
    assert(@offsetOf(Hello, "nonce") == 64);
    assert(@offsetOf(Hello, "arena_bytes_req") == 96);
    assert(@offsetOf(Hello, "slots_req") == 104);
    assert(@sizeOf(SessionOk) == 16);
    assert(@sizeOf(Refuse) == 16);
    assert(@sizeOf(Bind) == 8);
    assert(@sizeOf(BindOk) == 32);
    assert(@offsetOf(BindOk, "rights") == 4);
    assert(@offsetOf(BindOk, "binding_id") == 8);
    assert(@offsetOf(BindOk, "ino") == 16);
    assert(@offsetOf(BindOk, "durability_granted") == 24);
    assert(@sizeOf(BindRefused) == 8);
    assert(@sizeOf(Unbind) == 16);
    assert(@sizeOf(AdminHello) == 88);
    assert(@sizeOf(AdminOk) == 8);
    assert(@sizeOf(AdminReqHeader) == 16);
    assert(@sizeOf(AdminReplyHeader) == 16);
    assert(@sizeOf(AdminReqHeader) + ADMIN_VERB_MAX + ADMIN_ARG_MAX <= CTL_MSG_MAX);
}

/// The fixed frames' ONE total decoder: exactly `@sizeOf(T)` bytes, the
/// tag must be `T`'s, every NUL-padded field NUL-terminated, every class
/// word a known class.
pub fn decodeFixed(comptime T: type, raw: []const u8) DecodeError!T {
    if (raw.len != @sizeOf(T)) return error.Truncated;
    var v: T = undefined;
    @memcpy(std.mem.asBytes(&v), raw);
    const want: u32 = @backingInt(tagFor(T));
    if (v.tag != want) return error.BadTag;
    if (@hasField(T, "build_commit")) {
        if (std.mem.indexOfScalar(u8, &v.build_commit, 0) == null) return error.BadString;
    }
    if (T == Refuse or T == BindRefused) {
        if (RefuseClass.fromU32(v.class) == null) return error.BadClass;
    }
    if (T == BindOk) {
        if (v.rights._pad != 0) return error.BadString;
    }
    return v;
}

pub fn tagFor(comptime T: type) Tag {
    return switch (T) {
        Hello => .hello,
        SessionOk => .session_ok,
        Refuse => .refuse,
        Bind => .bind,
        BindOk => .bind_ok,
        BindRefused => .bind_refused,
        Unbind => .unbind,
        AdminHello => .admin_hello,
        AdminOk => .admin_ok,
        else => @compileError("not a fixed ctl frame: " ++ @typeName(T)),
    };
}

pub fn frameBytes(v: anytype) []const u8 {
    return std.mem.asBytes(v);
}

// ---------------------------------------------------------------------------
// Tests (spec §8: blob_roundtrip_and_negative_probe, ctl_roundtrip_every_
// variant, decoders_are_total_on_garbage, degenerate_identities).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "blob round-trip: 312 B, every field at its offset, the strings NUL-terminated inside their widths; a bad magic or an unterminated field refuses; short input is Truncated" {
    var b: Il0Blob = .{};
    try setField(&b.build_commit, "0123456789ab-dirty");
    try setField(&b.socket, "sqz-il0-4242-deadbeef");
    try setField(&b.socket_path, "/run/squeezefs/sqz-il0-4242-deadbeef.sock");
    @memset(&b.nonce, 0x5a);
    const raw = b.bytes();
    try testing.expectEqual(@as(usize, 312), raw.len);
    try testing.expectEqualStrings("SQZIL0\x00\x00", raw[0..8]);
    try testing.expectEqual(IPC_ABI, std.mem.readInt(u32, raw[8..12], .little));
    try testing.expectEqualStrings("0123456789ab-dirty", fieldStr(raw[16..64]));
    try testing.expectEqualStrings("sqz-il0-4242-deadbeef", fieldStr(raw[64..172]));
    try testing.expectEqualStrings("/run/squeezefs/sqz-il0-4242-deadbeef.sock", fieldStr(raw[172..280]));
    try testing.expectEqual(@as(u8, 0x5a), raw[280]);
    const d = try Il0Blob.decode(raw);
    try testing.expectEqualStrings("0123456789ab-dirty", d.buildCommit());
    try testing.expectEqualStrings("sqz-il0-4242-deadbeef", d.socketName());
    try testing.expect(std.mem.eql(u8, &b.nonce, &d.nonce));
    try testing.expectError(error.Truncated, Il0Blob.decode(raw[0..311]));
    var bad = b;
    bad.magic +%= 1;
    try testing.expectError(error.BadString, Il0Blob.decode(bad.bytes()));
    var unterminated = b;
    @memset(&unterminated.socket_path, 'x');
    try testing.expectError(error.BadString, Il0Blob.decode(unterminated.bytes()));
    // The encoder refuses a string that would need the whole width.
    const too_long: [SOCKET_NAME_LEN]u8 = @splat('p');
    try testing.expectError(error.StringTooLong, setField(&b.socket_path, &too_long));
}

test "ctl round-trip of every fixed variant through the ONE decoder; the tag dispatch; a wrong tag or a wrong length refuses" {
    var hello: Hello = .{ .client_kind = @backingInt(ClientKind.sdk), .sdk_abi = SDK_ABI_CURRENT, .arena_bytes_req = 64 << 20, .slots_req = 256 };
    try setField(&hello.build_commit, "abcdef123456");
    @memset(&hello.nonce, 7);
    const h = try decodeFixed(Hello, frameBytes(&hello));
    try testing.expectEqualStrings("abcdef123456", h.buildCommit());
    try testing.expectEqual(@as(u32, 256), h.slots_req);
    try testing.expectEqual(Tag.hello, try tagOf(frameBytes(&hello)));
    const ok: SessionOk = .{ .features = 3 };
    try testing.expectEqual(@as(u64, 3), (try decodeFixed(SessionOk, frameBytes(&ok))).features);
    const refuse: Refuse = .{ .class = @backingInt(RefuseClass.version), .sdk_abi_min = 1, .sdk_abi_max = 2 };
    const r = try decodeFixed(Refuse, frameBytes(&refuse));
    try testing.expectEqual(RefuseClass.version, try r.refuseClass());
    try testing.expectEqual(@as(u32, 2), r.sdk_abi_max);
    const bind: Bind = .{ .durability = @backingInt(Durability.sync) };
    try testing.expectEqual(@as(u32, 1), (try decodeFixed(Bind, frameBytes(&bind))).durability);
    const bok: BindOk = .{ .rights = .{ .read = true, .odirect = true }, .binding_id = 9, .ino = 77 };
    const bo = try decodeFixed(BindOk, frameBytes(&bok));
    try testing.expect(bo.rights.read and !bo.rights.write and bo.rights.odirect);
    try testing.expectEqual(@as(u64, 77), bo.ino);
    const bref: BindRefused = .{ .class = @backingInt(RefuseClass.rights) };
    try testing.expectEqual(RefuseClass.rights, try (try decodeFixed(BindRefused, frameBytes(&bref))).refuseClass());
    const unbind: Unbind = .{ .binding_id = 9 };
    try testing.expectEqual(@as(u64, 9), (try decodeFixed(Unbind, frameBytes(&unbind))).binding_id);
    var ah: AdminHello = .{};
    try setField(&ah.build_commit, "abcdef123456");
    try testing.expectEqualStrings("abcdef123456", (try decodeFixed(AdminHello, frameBytes(&ah))).buildCommit());
    const aok: AdminOk = .{};
    _ = try decodeFixed(AdminOk, frameBytes(&aok));
    // The two variable frames.
    var buf: [256]u8 = undefined;
    const n = try (AdminReq{ .verb = "status", .arg = "{\"json\":true}" }).encode(&buf);
    const req = try AdminReq.decode(buf[0..n]);
    try testing.expectEqualStrings("status", req.verb);
    try testing.expectEqualStrings("{\"json\":true}", req.arg);
    try testing.expectEqual(Tag.admin_req, try tagOf(buf[0..n]));
    const m = try (AdminReply{ .ok = true, .body = "{}" }).encode(&buf);
    const rep = try AdminReply.decode(buf[0..m]);
    try testing.expect(rep.ok);
    try testing.expectEqualStrings("{}", rep.body);
    // Wrong tag for the type; wrong length.
    try testing.expectError(error.BadTag, decodeFixed(Bind, frameBytes(&unbind)[0..8]));
    try testing.expectError(error.Truncated, decodeFixed(Hello, frameBytes(&hello)[0..111]));
    try testing.expectError(error.Truncated, decodeFixed(Bind, frameBytes(&unbind)));
    try testing.expectError(error.BadTag, tagOf(&[_]u8{ 12, 0, 0, 0 }));
    try testing.expectError(error.Truncated, tagOf(&[_]u8{ 1, 0 }));
    // A bad class word; an unterminated commit; a rights pad bit.
    const badref: Refuse = .{ .class = 42 };
    try testing.expectError(error.BadClass, decodeFixed(Refuse, frameBytes(&badref)));
    var badhello: Hello = .{};
    @memset(&badhello.build_commit, 'c');
    try testing.expectError(error.BadString, decodeFixed(Hello, frameBytes(&badhello)));
    var badok: BindOk = .{ .rights = .{ .read = true }, .binding_id = 1, .ino = 1 };
    badok.rights._pad = 1;
    try testing.expectError(error.BadString, decodeFixed(BindOk, frameBytes(&badok)));
    // AdminReq: a lying length never slices past the datagram.
    const lying: AdminReqHeader = .{ .verb_len = 4, .arg_len = 10_000 };
    try testing.expectError(error.Truncated, AdminReq.decode(frameBytes(&lying)));
    const oversize: AdminReqHeader = .{ .verb_len = 65, .arg_len = 0 };
    try testing.expectError(error.BadString, AdminReq.decode(frameBytes(&oversize)));
    const long_verb: [ADMIN_VERB_MAX + 1]u8 = @splat('x');
    try testing.expectError(error.StringTooLong, (AdminReq{ .verb = &long_verb, .arg = "" }).encode(&buf));
}

test "decoders are total on garbage: no decoder panics on arbitrary bytes of arbitrary length; a round-tripped frame decodes to itself" {
    var prng = std.Random.DefaultPrng.init(0x11e0);
    const r = prng.random();
    var buf: [400]u8 = undefined;
    var round: usize = 0;
    while (round < 4000) : (round += 1) {
        const len = r.uintLessThan(usize, buf.len + 1);
        r.bytes(buf[0..len]);
        const raw = buf[0..len];
        _ = Il0Blob.decode(raw) catch {};
        _ = tagOf(raw) catch {};
        _ = decodeFixed(Hello, raw) catch {};
        _ = decodeFixed(SessionOk, raw) catch {};
        _ = decodeFixed(Refuse, raw) catch {};
        _ = decodeFixed(Bind, raw) catch {};
        _ = decodeFixed(BindOk, raw) catch {};
        _ = decodeFixed(BindRefused, raw) catch {};
        _ = decodeFixed(Unbind, raw) catch {};
        _ = decodeFixed(AdminHello, raw) catch {};
        _ = decodeFixed(AdminOk, raw) catch {};
        _ = AdminReq.decode(raw) catch {};
        _ = AdminReply.decode(raw) catch {};
        // A valid frame with its tag forced: either refuses or decodes to itself.
        if (len == @sizeOf(Hello)) {
            std.mem.writeInt(u32, buf[0..4], @backingInt(Tag.hello), .little);
            if (decodeFixed(Hello, raw)) |h| {
                try testing.expectEqualSlices(u8, raw, frameBytes(&h));
            } else |_| {}
        }
    }
}

test "degenerate identities: unknown, empty and -dirty refuse the same-commit law; a clean short hash does not" {
    try testing.expect(buildCommitDegenerate("unknown"));
    try testing.expect(buildCommitDegenerate(""));
    try testing.expect(buildCommitDegenerate("0123456789ab-dirty"));
    try testing.expect(!buildCommitDegenerate("0123456789ab"));
    try testing.expect(!buildCommitDegenerate("dirty"));
    // The widest commit form the gate must see fits the field with its NUL.
    try testing.expect("0123456789abcdef0123456789abcdef01234567-dirty".len < BUILD_COMMIT_LEN);
}

test "RefuseClass: nine classes, each round-trips; anything else is BadClass; the SDK window is the current ABI alone" {
    var v: u32 = 1;
    while (v <= 9) : (v += 1) try testing.expectEqual(v, @backingInt(RefuseClass.fromU32(v).?));
    try testing.expectEqual(@as(?RefuseClass, null), RefuseClass.fromU32(0));
    try testing.expectEqual(@as(?RefuseClass, null), RefuseClass.fromU32(10));
    try testing.expectEqual(RefuseClass.mount, RefuseClass.fromU32(4).?);
    try testing.expectEqual(RefuseClass.rights, RefuseClass.fromU32(9).?);
    try testing.expectEqual(@as(usize, 1), sdk_abi_accepted.len);
    try testing.expectEqual(SDK_ABI_CURRENT, sdk_abi_accepted[0]);
}
