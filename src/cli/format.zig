//! `squeezefs format` (charter §8; §5.1 the v4 image; `docs/porting/
//! kv_alloc_superblock.md` §2.2–§2.4): the preflight over the device, then
//! `meta/backend.formatVolume`. Linux only.
//!
//! Rung 3 formats ONE metadata volume as a v4 one-member forest (decision
//! 0003 Q2): `--single-writer` names the class the image already is; the
//! symmetric plane (the appender directory, `SYMMETRIC_PLANE`) is rung 6's
//! and lands on a live volume, so the plain form produces the same image
//! today and says so. `sqdata://` volumes are accepted through the ONE URI
//! parser and NAMED in the summary, not formatted and not yet recorded:
//! the volume-set configuration that records them is rung 4's with the
//! data plane (there is no `format_config` xattr to smuggle them into —
//! decision 0002 row 46). A metadata set of more than one member is rung
//! 5's (`W = 2^16` stamps) and refused here.
//!
//! The preflight (§2.4), with no side effect before its verdict: a live
//! writer (layer A's flock held) refuses even with `--force`; a Blank
//! sector 0 formats; anything else — a v4 volume, a pre-v4 one, a torn or
//! foreign superblock — refuses naming `--force`, which reformats it. The
//! device is read O_DIRECT through the io pool like every metadata byte
//! (charter §5.1: a buffered read of a shared LUN returned a stale page).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const status = @import("status.zig");
const backend = @import("../meta/backend.zig");
const superblock = @import("../meta/superblock.zig");
const geometry = @import("../meta/geometry.zig");
const device_loader = @import("../meta/device_loader.zig");
const uring_fs = @import("../io/uring_fs.zig");
const derive = @import("../core/derive.zig");
const log = @import("../core/log.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/format is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Prior = status.Prior;

/// The force gate, pure: Blank formats; everything else needs `--force`.
pub fn forceGate(prior: Prior, force: bool, w: *Io.Writer) Io.Writer.Error!bool {
    switch (prior) {
        .blank => return true,
        .formatted => {
            if (force) return true;
            try w.writeAll("Metadata volume is already formatted as SqueezeFS; refusing to destroy it. Pass --force to reformat.");
            return false;
        },
        .legacy => |v| {
            if (force) return true;
            try w.print("Metadata volume carries prior on-disk state (a pre-v4 squeezefs format, version {d}); refusing to destroy it. Pass --force to reformat.", .{v});
            return false;
        },
        .refused => |reason| {
            if (force) return true;
            try w.print("Metadata volume carries prior on-disk state ({s}); refusing to destroy it. Pass --force to reformat.", .{reason});
            return false;
        },
    }
}

/// The invoking owner for the root directory: `SUDO_UID:SUDO_GID` under
/// sudo (the operator, not root), else the effective ids.
pub fn invokingOwner(env: *const std.process.Environ.Map) struct { uid: u32, gid: u32 } {
    const uid = parseId(env.get("SUDO_UID")) orelse linux.geteuid();
    const gid = parseId(env.get("SUDO_GID")) orelse linux.getegid();
    return .{ .uid = uid, .gid = gid };
}

fn parseId(raw: ?[]const u8) ?u32 {
    const t = std.mem.trim(u8, raw orelse return null, " \t\r\n");
    if (t.len == 0) return null;
    return std.fmt.parseInt(u32, t, 10) catch null;
}

pub fn run(p: cli.Process, f: args.Format) u8 {
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, f.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs format: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    if (meta.paths.len != 1) {
        p.stderr.print("squeezefs format: {s} names {d} metadata volumes; this rung formats ONE (a metadata set is rung 5's)\n", .{ f.meta_uri, meta.paths.len }) catch {};
        return cli.exit_failed;
    }
    const path = meta.first();

    // The io pool: registration (O_DIRECT) + the preflight read + the
    // image, all through one pool.
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs format: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();

    // Preflight.
    if (status.writerGuardHeld(path)) {
        p.stderr.print("squeezefs format: cannot format {s}: a live daemon holds its single-writer guard (umount it first; --force does not override a live mount)\n", .{path}) catch {};
        return cli.exit_failed;
    }
    var reason_buf: [512]u8 = undefined;
    const prior = status.classify(p.gpa, p.env, pool, path, &reason_buf) catch |e| {
        p.stderr.print("squeezefs format: cannot read {s}: {s}\n", .{ path, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    {
        var gate_buf: [1024]u8 = undefined;
        var gw: Io.Writer = .fixed(&gate_buf);
        const go = forceGate(prior.prior, f.force, &gw) catch false;
        if (!go) {
            p.stderr.print("squeezefs format: {s}\n", .{gw.buffered()}) catch {};
            return cli.exit_failed;
        }
    }

    // Identities: per-volume uuid, the set's (uuid, hash seed) — three
    // independent draws from the OS entropy source.
    var vol_uuid: [16]u8 = undefined;
    p.io.randomSecure(&vol_uuid) catch {
        p.stderr.writeAll("squeezefs format: getrandom failed\n") catch {};
        return cli.exit_failed;
    };
    const set = geometry.mintSetIdentity(p.io) catch {
        p.stderr.writeAll("squeezefs format: getrandom failed\n") catch {};
        return cli.exit_failed;
    };
    const owner = invokingOwner(p.env);

    backend.formatVolume(p.gpa, path, .{
        .env = p.env,
        .pool = pool,
        .node_size = f.node_size orelse superblock.default_node_size,
        .journal_len_override = f.journal_len,
        .uuid = vol_uuid,
        .set = set,
        .root_uid = owner.uid,
        .root_gid = owner.gid,
    }) catch |e| {
        p.stderr.print("squeezefs format: {s}: {s}\n", .{ path, formatErrorText(e) }) catch {};
        return cli.exit_failed;
    };

    // The summary.
    const vlen = device_loader.volumeLength(path) catch 0;
    p.stdout.print("formatted {s}: SqueezeFS format v4, one-member forest ({s}), {d} B, node {d} B, journal {s}, root owner {d}:{d}\n", .{
        f.meta_uri,
        if (f.single_writer) "--single-writer: the single-writer class" else "the symmetric plane is stamped on a live volume at rung 6; today's image is the single-writer class",
        vlen,
        f.node_size orelse superblock.default_node_size,
        if (f.journal_len != null) "explicit" else "derived clamp(volume/64, 8 MiB, 32 MiB)",
        owner.uid,
        owner.gid,
    }) catch {};
    if (f.data_uris.len > 0) {
        p.stdout.print("  data volumes accepted, NOT formatted or recorded ({d} URI(s)): the data plane and the volume-set configuration that records them land with rung 4\n", .{f.data_uris.len}) catch {};
        for (f.data_uris) |d| p.stdout.print("    {s}\n", .{d}) catch {};
    }
    p.stdout.flush() catch {};
    return cli.exit_ok;
}

fn formatErrorText(e: backend.FormatError) []const u8 {
    return switch (e) {
        error.DeviceRefused => "the device refused registration (O_DIRECT posture) or does not exist",
        error.VolumeTooSmall => "the volume is too small for format v4 (see the log for the geometry)",
        error.InvalidNodeSize => "invalid node size",
        error.InvalidJournalOverride => "invalid --meta-journal-mb (below the 384 KiB floor or not a 4 KiB multiple)",
        error.CannotStat, error.NotADeviceOrFile => "not a regular file or block device",
        error.Io => "device I/O failed",
        error.OutOfMemory => "out of memory",
        error.Corrupt, error.Unexpected => "the image build failed (see the log)",
    };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the force gate: Blank formats; formatted/legacy/refused need --force and say so" {
    var buf: [512]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try testing.expect(try forceGate(.blank, false, &w));
    try testing.expect(!try forceGate(.formatted, false, &w));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "already formatted") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "--force") != null);
    w = .fixed(&buf);
    try testing.expect(!try forceGate(.{ .legacy = 3 }, false, &w));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "version 3") != null);
    w = .fixed(&buf);
    try testing.expect(!try forceGate(.{ .refused = "checksum mismatch" }, false, &w));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "checksum mismatch") != null);
    try testing.expect(try forceGate(.formatted, true, &w));
    try testing.expect(try forceGate(.{ .legacy = 2 }, true, &w));
    try testing.expect(try forceGate(.{ .refused = "x" }, true, &w));
}

test "the invoking owner is SUDO_UID:SUDO_GID when set, else the effective ids" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const me = invokingOwner(&env);
    try testing.expectEqual(linux.geteuid(), me.uid);
    try env.put("SUDO_UID", "1000");
    try env.put("SUDO_GID", "1001");
    const sudo = invokingOwner(&env);
    try testing.expectEqual(@as(u32, 1000), sudo.uid);
    try testing.expectEqual(@as(u32, 1001), sudo.gid);
    try env.put("SUDO_UID", "junk");
    try testing.expectEqual(linux.geteuid(), invokingOwner(&env).uid);
}
