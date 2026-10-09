//! The SDK's version words and refusal texts (charter §7 "versioned over a
//! one-release window", §11; spec `docs/porting/il_sdk.md` §4). Portable.
//!
//! ONE two-part ABI word (decision 0002 ledger, the SDK rows): the WIRE
//! part is the frozen-subset version the daemon's `{cur, cur−1}` window is
//! defined over (`wire.SDK_ABI_CURRENT`, sent in HELLO's `sdk_abi`; bumps
//! only on a breaking change to the frozen subset — header lines 0–2, the
//! slot descriptor, the ring cells, the arena addressing, the five ctl
//! frames); the SDK part is the C surface's own revision, never on the wire
//! (additive evolution of the header rides it, together with `SessionOk`
//! feature bits). The daemon never sees the SDK part, so a header-only
//! change can never void the window — the design's quirk B.
//!
//! The SDK never pairs on commits: an SDK app establishes against a `-dirty`
//! daemon whose wire word is in the window (quirk G) — "the window is a
//! performance promise, not a correctness one", refusal is always safe.

const std = @import("std");
const linux = std.os.linux;
const wire = @import("../ipc/wire.zig");

/// The frozen-subset word: HELLO's `sdk_abi`.
pub const WIRE_ABI: u16 = @intCast(wire.SDK_ABI_CURRENT);
/// The C surface's revision (`squeezefs_sdk.h` `SQZ_ABI_SDK`); starts at 1.
pub const SDK_ABI: u16 = 1;

/// `wire << 16 | sdk`: what `sqz_abi()` answers and the header pins.
pub fn word() u32 {
    return (@as(u32, WIRE_ABI) << 16) | SDK_ABI;
}

pub fn wirePart(w: u32) u16 {
    return @intCast(w >> 16);
}

pub fn sdkPart(w: u32) u16 {
    return @truncate(w);
}

/// The daemon's law, restated client-side (the pre-check and the tests
/// agree with `rendezvous.admitHello`): accepted iff the app's wire word is
/// in the daemon's set.
pub fn windowAccepts(accepted: []const u32, app: u32) bool {
    for (accepted) |a| if (a == app) return true;
    return false;
}

/// The daemon's window as `Refuse` names it: the set is kept newest-first
/// (`.{2, 1}` at a bump), so max is the first and min the last entry.
pub fn windowBounds(accepted: []const u32) struct { min: u32, max: u32 } {
    std.debug.assert(accepted.len > 0);
    return .{ .min = accepted[accepted.len - 1], .max = accepted[0] };
}

/// Every way a session can fail to establish, each with its text and its
/// errno (the ONE convention: a failed `sqz_session_open` is `NULL` with
/// the negative errno in `sqz_last_error()` and this line in
/// `sqz_last_error_text()`, printed once per `(mount, reason)` to stderr).
pub const Reason = union(enum) {
    /// `fstatfs.f_type` is not FUSE_SUPER_MAGIC.
    not_squeezefs_mount,
    /// GETXATTR `user.squeezefs.il0` answered ENODATA (not armed).
    no_bootstrap_xattr,
    /// The daemon answered `Refuse{disabled}`.
    not_armed,
    /// The blob's coarse `ipc_abi` is not ours.
    ipc_abi: struct { sdk: u32, daemon: u32 },
    /// `Refuse{version}` for kind SDK: the daemon named its window.
    sdk_abi_window: struct { app: u32, min: u32, max: u32 },
    budget,
    nonce,
    /// The credential (the mount root fd) failed the daemon's screen.
    credential: wire.RefuseClass,
    internal,
    /// Rung 1 / rung 2 / rung 3: the client refused the daemon.
    untrusted_rendezvous,
    peer_untrusted,
    unsealed,
    socket,
    protocol,
    map,
    futex_waitv_unsupported,
    out_of_memory,

    /// The reason's negative-errno class (the text carries the detail).
    pub fn errno(r: Reason) linux.E {
        return switch (r) {
            .not_squeezefs_mount, .not_armed => .OPNOTSUPP,
            .no_bootstrap_xattr => .NODATA,
            .ipc_abi, .sdk_abi_window => .PROTONOSUPPORT,
            .budget => .AGAIN,
            .nonce, .protocol => .PROTO,
            .credential => .INVAL,
            .internal => .IO,
            .untrusted_rendezvous, .peer_untrusted, .unsealed => .ACCES,
            .socket => .CONNREFUSED,
            .map, .out_of_memory => .NOMEM,
            .futex_waitv_unsupported => .NOSYS,
        };
    }

    /// Stable per-variant index for the once-per-(mount, reason) ledger.
    pub fn tag(r: Reason) u8 {
        return @backingInt(@as(std.meta.Tag(Reason), r));
    }
};

/// The longest line (`sdk_abi_window` with three u32s) fits comfortably.
pub const TEXT_MAX: usize = 160;

/// The reason line, the shim's lineage: names both values where two exist.
pub fn text(buf: *[TEXT_MAX]u8, r: Reason) []const u8 {
    return switch (r) {
        .not_squeezefs_mount => "not a squeezefs mount (statfs)",
        .no_bootstrap_xattr => "no bootstrap xattr (mount with -o interception)",
        .not_armed => "interception not armed on this mount (mount with -o interception)",
        .ipc_abi => |v| std.fmt.bufPrint(buf, "ipc abi mismatch (sdk {d}, daemon {d})", .{ v.sdk, v.daemon }) catch unreachable,
        .sdk_abi_window => |v| std.fmt.bufPrint(buf, "sdk abi outside the daemon's window (app {d}, daemon accepts {d}..{d})", .{ v.app, v.min, v.max }) catch unreachable,
        .budget => "daemon refused: session budget / admission cap",
        .nonce => "daemon refused: stale nonce",
        .credential => |c| std.fmt.bufPrint(buf, "daemon refused the mount-root credential ({t})", .{c}) catch unreachable,
        .internal => "daemon refused: internal (session memfd / thread)",
        .untrusted_rendezvous => "path rendezvous directory is not owned by root or the mount owner, or is group/world-writable — not connecting",
        .peer_untrusted => "daemon authentication failed: socket peer uid is neither root nor the mount owner — not sending the credential fd",
        .unsealed => "daemon authentication failed: session memfd is not sealed (F_SEAL_SEAL|SHRINK|GROW) — not mapping",
        .socket => "ctl socket failed (connect / EOF)",
        .protocol => "malformed reply from the daemon",
        .map => "mapping the session memfd failed",
        .futex_waitv_unsupported => "futex_waitv(2) unsupported by this kernel",
        .out_of_memory => "out of memory",
    };
}

// ---------------------------------------------------------------------------
// Tests (spec §8: the window law, the texts).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the two-part word: wire part = the frozen-subset version, sdk part = the header revision; both decode back; HELLO carries the wire part alone" {
    const w = word();
    try testing.expectEqual(wire.SDK_ABI_CURRENT, @as(u32, wirePart(w)));
    try testing.expectEqual(SDK_ABI, sdkPart(w));
    try testing.expectEqual(@as(u32, 1 << 16 | 1), w);
    // A header-only (SDK part) bump leaves the wire word the daemon judges alone.
    try testing.expectEqual(wirePart(w), wirePart((@as(u32, WIRE_ABI) << 16) | 7));
}

test "version window: accepts cur and cur−1, refuses cur−2 and cur+1 (the N / N+1 / N+2 release timeline); today's set is {1}" {
    const cur: u32 = 2;
    const train_n1 = [_]u32{ cur, cur - 1 };
    try testing.expect(windowAccepts(&train_n1, cur));
    try testing.expect(windowAccepts(&train_n1, cur - 1));
    try testing.expect(!windowAccepts(&train_n1, cur + 1));
    try testing.expect(!windowAccepts(&train_n1, 0));
    const b = windowBounds(&train_n1);
    try testing.expectEqual(@as(u32, 1), b.min);
    try testing.expectEqual(@as(u32, 2), b.max);
    // N+2 sunsets 1 as a release act.
    const train_n2 = [_]u32{cur};
    try testing.expect(!windowAccepts(&train_n2, 1));
    try testing.expect(windowAccepts(&train_n2, 2));
    // Today.
    try testing.expect(windowAccepts(&wire.sdk_abi_accepted, WIRE_ABI));
    try testing.expect(!windowAccepts(&wire.sdk_abi_accepted, WIRE_ABI + 1));
    try testing.expectEqual(@as(u32, WIRE_ABI), windowBounds(&wire.sdk_abi_accepted).min);
}

test "refusal texts name both values; every reason has an errno class and a distinct tag; the longest line fits TEXT_MAX" {
    var buf: [TEXT_MAX]u8 = undefined;
    try testing.expectEqualStrings("sdk abi outside the daemon's window (app 1, daemon accepts 2..3)", text(&buf, .{ .sdk_abi_window = .{ .app = 1, .min = 2, .max = 3 } }));
    try testing.expectEqualStrings("ipc abi mismatch (sdk 7, daemon 6)", text(&buf, .{ .ipc_abi = .{ .sdk = 7, .daemon = 6 } }));
    try testing.expectEqualStrings("daemon refused the mount-root credential (flags)", text(&buf, .{ .credential = .flags }));
    try testing.expectEqual(linux.E.PROTONOSUPPORT, (Reason{ .sdk_abi_window = .{ .app = 1, .min = 2, .max = 2 } }).errno());
    try testing.expectEqual(linux.E.NODATA, (Reason{ .no_bootstrap_xattr = {} }).errno());
    try testing.expectEqual(linux.E.AGAIN, (Reason{ .budget = {} }).errno());
    try testing.expectEqual(linux.E.ACCES, (Reason{ .unsealed = {} }).errno());
    const max_line = text(&buf, .{ .sdk_abi_window = .{ .app = std.math.maxInt(u32), .min = std.math.maxInt(u32), .max = std.math.maxInt(u32) } });
    try testing.expect(max_line.len < TEXT_MAX);
    var seen: [20]bool = @splat(false);
    inline for (@typeInfo(Reason).@"union".field_names) |name| {
        const r: Reason = switch (@field(std.meta.Tag(Reason), name)) {
            .ipc_abi => .{ .ipc_abi = .{ .sdk = 1, .daemon = 1 } },
            .sdk_abi_window => .{ .sdk_abi_window = .{ .app = 1, .min = 1, .max = 1 } },
            .credential => .{ .credential = .mount },
            inline else => |t| @unionInit(Reason, @tagName(t), {}),
        };
        try testing.expect(!seen[r.tag()]);
        seen[r.tag()] = true;
        _ = r.errno();
        try testing.expect(text(&buf, r).len > 0);
    }
}
