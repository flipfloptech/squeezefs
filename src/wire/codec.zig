//! The ONE wire codec law for every cluster-wire payload (charter §12
//! "on-disk and wire structs are `extern struct` with explicit endianness,
//! comptime size assertions, and canonical codecs — decode is total").
//!
//! A payload is a fixed `extern struct` head (every field a little-endian
//! integer, a byte array, or a nested extern struct; NO padding — the
//! comptime `wireSize == @sizeOf` assertion refuses a struct whose natural
//! layout differs from its wire layout) followed by length-framed tails
//! (`count ‖ items`, `len ‖ bytes`). Encode and decode walk the fields by
//! reflection, so the endianness is in ONE place and a struct declared is
//! a struct encoded. Decode is total: a short buffer, a nonzero reserved
//! byte, a failed `check`, a count whose items are not all present, or a
//! trailing byte is `error.Malformed` — never a panic, never a guess
//! ("a length in a frame is a claim, never an allocation authority").
//!
//! Array tails are VIEWED in place (`[]align(1) const T`): the wire is
//! little-endian and so is every product host (x86_64, aarch64), so a view
//! is the decoded form; the comptime assertion below refuses a big-endian
//! build rather than serving reversed integers. Every item is still
//! validated field by field (reserved bytes, `check`) before the view is
//! handed out.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");

comptime {
    if (builtin.cpu.arch.endian() != .little) @compileError("the cluster wire views array tails in place: a big-endian host needs a copying decoder (charter §1.2 names x86_64/aarch64 Linux only)");
}

pub const Error = error{Malformed};
pub const BuildError = error{Overflow};

/// Bytes the wire form of `T` occupies; asserts the extern layout has no
/// padding so an in-place view equals the field-wise decode.
pub fn wireSize(comptime T: type) usize {
    switch (@typeInfo(T)) {
        .int => |i| {
            if (i.bits % 8 != 0) @compileError("wire integers are whole bytes: " ++ @typeName(T));
            return i.bits / 8;
        },
        .array => |a| return a.len * wireSize(a.child),
        .@"struct" => |s| {
            if (s.layout != .@"extern") @compileError("a wire struct is an extern struct: " ++ @typeName(T));
            comptime var n: usize = 0;
            inline for (s.field_types) |F| n += comptime wireSize(F);
            if (n != @sizeOf(T)) @compileError("a wire struct carries no padding (reorder or add a reserved field): " ++ @typeName(T));
            return n;
        },
        else => @compileError("not a wire type: " ++ @typeName(T)),
    }
}

fn isReservedName(name: []const u8) bool {
    return std.mem.startsWith(u8, name, "reserved") or std.mem.startsWith(u8, name, "pad");
}

/// Field-wise little-endian encode of a fixed struct (or integer/array).
pub fn encodeInto(comptime T: type, v: T, out: *[wireSize(T)]u8) void {
    switch (@typeInfo(T)) {
        .int => std.mem.writeInt(T, out, v, .little),
        .array => |a| {
            const n = comptime wireSize(a.child);
            inline for (0..a.len) |i| encodeInto(a.child, v[i], out[i * n ..][0..n]);
        },
        .@"struct" => |s| {
            comptime var off: usize = 0;
            inline for (s.field_names, s.field_types) |name, F| {
                const n = comptime wireSize(F);
                encodeInto(F, @field(v, name), out[off..][0..n]);
                off += n;
            }
        },
        else => comptime unreachable,
    }
}

/// Field-wise little-endian decode of a fixed struct; every `reserved*` /
/// `pad*` field must read zero and `T.check(v)` (when declared) must hold.
pub fn decodeFrom(comptime T: type, bytes: *const [wireSize(T)]u8) Error!T {
    switch (@typeInfo(T)) {
        .int => return std.mem.readInt(T, bytes, .little),
        .array => |a| {
            const n = comptime wireSize(a.child);
            var v: T = undefined;
            inline for (0..a.len) |i| v[i] = try decodeFrom(a.child, bytes[i * n ..][0..n]);
            return v;
        },
        .@"struct" => |s| {
            var v: T = undefined;
            comptime var off: usize = 0;
            inline for (s.field_names, s.field_types) |name, F| {
                const n = comptime wireSize(F);
                const fv = try decodeFrom(F, bytes[off..][0..n]);
                if (comptime isReservedName(name)) {
                    if (!std.mem.allEqual(u8, bytes[off..][0..n], 0)) return error.Malformed;
                }
                @field(v, name) = fv;
                off += n;
            }
            if (@hasDecl(T, "check")) {
                if (!v.check()) return error.Malformed;
            }
            return v;
        },
        else => comptime unreachable,
    }
}

/// Is `T` a fixed wire struct (an extern struct) rather than a composite
/// payload with tails (a plain struct carrying `encode`/`decode`)?
pub fn isFixed(comptime T: type) bool {
    return @typeInfo(T) == .@"struct" and @typeInfo(T).@"struct".layout == .@"extern";
}

/// The reader over one payload: every take is bounds-checked; `done`
/// refuses trailing bytes (exactly the encoder's form decodes).
pub const Cursor = struct {
    bytes: []const u8,
    pos: usize = 0,

    pub fn init(bytes: []const u8) Cursor {
        return .{ .bytes = bytes };
    }

    pub fn remaining(self: *const Cursor) usize {
        return self.bytes.len - self.pos;
    }

    pub fn take(self: *Cursor, comptime T: type) Error!T {
        const n = comptime wireSize(T);
        if (self.remaining() < n) return error.Malformed;
        const v = try decodeFrom(T, self.bytes[self.pos..][0..n]);
        self.pos += n;
        return v;
    }

    /// `count` items of `T` viewed in place after each is validated; the
    /// count was already read by the caller from the head. `count ×
    /// size ≤ remaining` is checked BEFORE the slice exists.
    pub fn takeArray(self: *Cursor, comptime T: type, count: usize) Error![]align(1) const T {
        const n = comptime wireSize(T);
        const total = std.math.mul(usize, count, n) catch return error.Malformed;
        if (self.remaining() < total) return error.Malformed;
        const raw = self.bytes[self.pos..][0..total];
        var i: usize = 0;
        while (i < count) : (i += 1) _ = try decodeFrom(T, raw[i * n ..][0..n]);
        self.pos += total;
        return std.mem.bytesAsSlice(T, raw);
    }

    /// `len` raw bytes (a string or an opaque value) whose length the head
    /// carried.
    pub fn takeBytes(self: *Cursor, len: usize) Error![]const u8 {
        if (self.remaining() < len) return error.Malformed;
        const b = self.bytes[self.pos..][0..len];
        self.pos += len;
        return b;
    }

    /// Skip the zero padding that rounds a tail to `align_to`; the pad
    /// bytes must be zero.
    pub fn takePadding(self: *Cursor, consumed: usize, align_to: usize) Error!void {
        const pad = (align_to - consumed % align_to) % align_to;
        const b = try self.takeBytes(pad);
        if (!std.mem.allEqual(u8, b, 0)) return error.Malformed;
    }

    pub fn done(self: *const Cursor) Error!void {
        if (self.pos != self.bytes.len) return error.Malformed;
    }
};

/// `n` rounded up to a multiple of `align_to` (the tail-padding law).
pub fn padTo(n: usize, align_to: usize) usize {
    return n + (align_to - n % align_to) % align_to;
}

/// The writer into a caller-owned buffer (the session's send buffer, sized
/// at the frame class cap): every put is bounds-checked; nothing is
/// allocated per frame (charter §12).
pub const Builder = struct {
    buf: []u8,
    pos: usize = 0,

    pub fn init(buf: []u8) Builder {
        return .{ .buf = buf };
    }

    pub fn put(self: *Builder, comptime T: type, v: T) BuildError!void {
        const n = comptime wireSize(T);
        if (self.buf.len - self.pos < n) return error.Overflow;
        encodeInto(T, v, self.buf[self.pos..][0..n]);
        self.pos += n;
    }

    pub fn putArray(self: *Builder, comptime T: type, items: []const T) BuildError!void {
        for (items) |it| try self.put(T, it);
    }

    /// An in-place view (a decoded tail re-encoded verbatim).
    pub fn putView(self: *Builder, comptime T: type, items: []align(1) const T) BuildError!void {
        for (items) |it| try self.put(T, it);
    }

    pub fn putBytes(self: *Builder, bytes: []const u8) BuildError!void {
        if (self.buf.len - self.pos < bytes.len) return error.Overflow;
        @memcpy(self.buf[self.pos..][0..bytes.len], bytes);
        self.pos += bytes.len;
    }

    pub fn putPadding(self: *Builder, consumed: usize, align_to: usize) BuildError!void {
        const pad = padTo(consumed, align_to) - consumed;
        if (self.buf.len - self.pos < pad) return error.Overflow;
        @memset(self.buf[self.pos..][0..pad], 0);
        self.pos += pad;
    }

    pub fn written(self: *const Builder) []const u8 {
        return self.buf[0..self.pos];
    }
};

/// Encode a whole payload: a fixed struct through the field walk, a
/// composite through its own `encode`.
pub fn encodePayload(comptime P: type, v: P, b: *Builder) BuildError!void {
    if (comptime isFixed(P)) return b.put(P, v);
    return v.encode(b);
}

/// Decode a whole payload — total, trailing bytes refused.
pub fn decodePayload(comptime P: type, bytes: []const u8) Error!P {
    var c: Cursor = .init(bytes);
    const v = if (comptime isFixed(P)) try c.take(P) else try P.decode(&c);
    try c.done();
    return v;
}

/// A count word is checked against its cap before anything proportional
/// to it is read: `n ≤ cap` or `Malformed`.
pub fn countAtMost(n: usize, cap: usize) Error!usize {
    if (n > cap) return error.Malformed;
    return n;
}

// ---------------------------------------------------------------------------

const testing = std.testing;

const Inner = extern struct { a: u32, reserved: u32 };
const Fixture = extern struct {
    x: u16,
    flag: u8,
    pad0: u8,
    nested: Inner,
    tail: [3]u8,
    reserved: [1]u8,
    big: u64,

    pub fn check(self: Fixture) bool {
        return self.flag <= 1;
    }
};

test "wireSize is the padding-free sum and equals @sizeOf" {
    try testing.expectEqual(@as(usize, 8), wireSize(Inner));
    try testing.expectEqual(@as(usize, 24), wireSize(Fixture));
    try testing.expectEqual(@sizeOf(Fixture), wireSize(Fixture));
}

test "field-wise LE round trip; reserved bytes and check are enforced; trailing bytes refused" {
    const v: Fixture = .{ .x = 0x1234, .flag = 1, .pad0 = 0, .nested = .{ .a = 7, .reserved = 0 }, .tail = .{ 1, 2, 3 }, .reserved = @splat(0), .big = 0x0102030405060708 };
    var buf: [wireSize(Fixture)]u8 = undefined;
    encodeInto(Fixture, v, &buf);
    try testing.expectEqual(@as(u8, 0x34), buf[0]);
    try testing.expectEqual(@as(u8, 0x12), buf[1]);
    try testing.expectEqual(@as(u8, 0x08), buf[16]);
    const back = try decodeFrom(Fixture, &buf);
    try testing.expectEqual(v, back);
    var bad = buf;
    bad[8] = 1; // nested.reserved
    try testing.expectError(error.Malformed, decodeFrom(Fixture, &bad));
    bad = buf;
    bad[2] = 2; // flag past check
    try testing.expectError(error.Malformed, decodeFrom(Fixture, &bad));
    bad = buf;
    bad[3] = 9; // pad0
    try testing.expectError(error.Malformed, decodeFrom(Fixture, &bad));
    // Trailing byte, short buffer.
    var long: [25]u8 = undefined;
    @memcpy(long[0..24], &buf);
    long[24] = 0;
    try testing.expectError(error.Malformed, decodePayload(Fixture, &long));
    try testing.expectError(error.Malformed, decodePayload(Fixture, buf[0..23]));
    try testing.expectEqual(v, try decodePayload(Fixture, &buf));
}

test "cursor arrays: the count is checked against the bytes before the view is formed; items are validated; padding must be zero" {
    var buf: [64]u8 = undefined;
    var b: Builder = .init(&buf);
    try b.put(u16, 2);
    try b.putArray(Inner, &.{ .{ .a = 1, .reserved = 0 }, .{ .a = 2, .reserved = 0 } });
    try b.putBytes("abc");
    try b.putPadding(3, 8);
    var c: Cursor = .init(b.written());
    const n = try c.take(u16);
    const items = try c.takeArray(Inner, n);
    try testing.expectEqual(@as(u32, 2), items[1].a);
    try testing.expectEqualStrings("abc", try c.takeBytes(3));
    try c.takePadding(3, 8);
    try c.done();
    // A lying count: 1000 items are not present.
    var c2: Cursor = .init(b.written());
    _ = try c2.take(u16);
    try testing.expectError(error.Malformed, c2.takeArray(Inner, 1000));
    try testing.expectError(error.Malformed, c2.takeArray(Inner, std.math.maxInt(usize)));
    // A nonzero reserved word inside an item refuses the whole array.
    var bad = buf;
    bad[2 + 4] = 1;
    var c3: Cursor = .init(bad[0..b.pos]);
    _ = try c3.take(u16);
    try testing.expectError(error.Malformed, c3.takeArray(Inner, 2));
    // Overflow on the builder side is a typed error, never a write past the buffer.
    var small: [3]u8 = undefined;
    var sb: Builder = .init(&small);
    try testing.expectError(error.Overflow, sb.put(u32, 1));
    try testing.expectError(error.Malformed, countAtMost(5, 4));
}

/// The generic field-wise decoders are total on arbitrary bytes, and a
/// fixed-size value that decodes re-encodes byte-identically.
fn codecTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [128]u8 = undefined;
    const len = smith.slice(&buf);
    const bytes = buf[0..len];
    const n = comptime wireSize(Fixture);
    if (bytes.len >= n) {
        const fixed: *const [n]u8 = bytes[0..n];
        if (decodeFrom(Fixture, fixed)) |v| {
            var re: [n]u8 = undefined;
            encodeInto(Fixture, v, &re);
            try std.testing.expectEqualSlices(u8, fixed, &re);
        } else |_| {}
    }
    _ = decodePayload(Fixture, bytes) catch {};
    _ = decodePayload(Inner, bytes) catch {};
}

test "fuzz: decodeFrom and decodePayload are total on arbitrary bytes and a decoded fixed value re-encodes to its bytes" {
    try fuzzkit.fuzz({}, codecTotalFuzz, .{});
}
