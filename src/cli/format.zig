//! `squeezefs format` (charter §8; §5.1 the v4 image; §6.5 the transform
//! declared at format; `docs/porting/kv_alloc_superblock.md` §2.2–§2.4;
//! `docs/porting/data_transform_config.md` §5.6 f): the set format. Linux
//! only.
//!
//! ONE OR MORE metadata volumes as a v4 set (decision 0003 Q2; charter
//! §5.4: member `p` of `n` stamps the stride run `{p, n, ceil((W − p)/n)}`
//! — the identity distribution, one run per member; member 0 hosts slot
//! 0's native keyspace and the `set_config` record; the SYMMETRIC plane is
//! stamped by default — the appender directory and appender 0's page —
//! and `--single-writer` is the flat opt-out, charter §5.5) plus ZERO OR
//! MORE data volumes, each given
//! its identity (`data/data_superblock.zig`), and the set's ONE
//! `set_config` control record written by the format transaction into
//! tree 0 of member 0 (`data/set_config.zig`): block size, chunk, the
//! transform, the key reference, every data volume with its `vol_tag`,
//! the cache paths. A set with no data volume is legal and records an
//! empty list.
//!
//! The order, with NO destructive step before every verdict: parse → key
//! material + salt + key id (an encrypted set) → the io pool → the
//! metadata preflight (a live writer refuses even with `--force`; Blank
//! formats; anything else needs `--force`) → every data device read and
//! classified (Blank formats; anything else needs `--force`; too small
//! refuses) → identities minted (set, volume uuid, per-device `vol_tag` +
//! `device_uuid`) → every DATA superblock written (device-before-record: a
//! crash here leaves an unlisted formatted device, never a listed blank
//! one) → the metadata volume formatted with the record → the summary.
//! Every device byte is O_DIRECT (charter §5.1).

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
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const transform = @import("../data/transform.zig");
const set_config = @import("../data/set_config.zig");
const wire_auth = @import("../wire/auth.zig");
const data_superblock = @import("../data/data_superblock.zig");
const foreign_signature = @import("../meta/foreign_signature.zig");

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
        .foreign => |hit| {
            if (force) return true;
            try w.print("Metadata volume carries {f}; refusing to destroy it. Pass --force to reformat.", .{hit});
            return false;
        },
    }
}

/// What a data device read as, for its force gate.
pub const DataPrior = union(enum) {
    blank,
    formatted: data_superblock.DataSuperblock,
    /// The classifier's refusal (a foreign signature, a torn superblock).
    refused: []const u8,
};

/// The data-device force gate, pure: Blank formats; a squeezefs data
/// volume or anything else needs `--force`.
pub fn dataForceGate(path: []const u8, prior: DataPrior, force: bool, w: *Io.Writer) Io.Writer.Error!bool {
    switch (prior) {
        .blank => return true,
        .formatted => |sb| {
            if (force) return true;
            try w.print("data volume {s} is already a SqueezeFS data volume (vol-{x:0>16}); refusing to destroy it. Pass --force to reformat.", .{ path, sb.vol_tag });
            return false;
        },
        .refused => |reason| {
            if (force) return true;
            try w.print("data volume {s} carries prior on-disk state ({s}); refusing to destroy it. Pass --force to reformat.", .{ path, reason });
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

/// One data device after its preflight.
const DataDevice = struct {
    path: []const u8,
    volume_len: u64,
    next_generation: u64,
};

pub fn run(p: cli.Process, f: args.Format) u8 {
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, f.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs format: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    for (meta.paths, 0..) |mp, i| for (meta.paths[0..i]) |prev| if (std.mem.eql(u8, prev, mp)) {
        p.stderr.print("squeezefs format: metadata volume {s} is named twice\n", .{mp}) catch {};
        return cli.exit_failed;
    };
    const path = meta.first();

    // Every data path, flattened across the `sqdata://` words.
    var data_paths: [set_config.MAX_DATA_VOLUMES][]const u8 = undefined;
    var n_data: usize = 0;
    for (f.data_uris) |word| {
        var buf: [uri.MAX_MEMBERS][]const u8 = undefined;
        const parsed = uri.parse(.data, word, &buf, &udiag) catch {
            p.stderr.print("squeezefs format: {f}\n", .{udiag}) catch {};
            return cli.exit_usage;
        };
        for (parsed.paths) |dp| {
            if (n_data == data_paths.len) {
                p.stderr.print("squeezefs format: more than {d} data volumes\n", .{data_paths.len}) catch {};
                return cli.exit_failed;
            }
            for (data_paths[0..n_data]) |prev| if (std.mem.eql(u8, prev, dp)) {
                p.stderr.print("squeezefs format: data volume {s} is named twice\n", .{dp}) catch {};
                return cli.exit_failed;
            };
            for (meta.paths) |mp| if (std.mem.eql(u8, dp, mp)) {
                p.stderr.print("squeezefs format: {s} is named as both a metadata volume and a data volume\n", .{dp}) catch {};
                return cli.exit_failed;
            };
            data_paths[n_data] = dp;
            n_data += 1;
        }
    }

    // The transform and its geometry (the chunk grows by the headroom on a
    // transformed set; the law is a tautology of the derivation, checked
    // anyway so a future derivation drift refuses at format, not at mount).
    const tcfg: transform.VolumeTransform = .{ .compression = f.compression, .encryption = f.encryption, .block_size = f.block_size };
    const chunk = transform.chunkSizeFor(tcfg);
    transform.geometryCheck(tcfg, chunk) catch {
        var gb: [256]u8 = undefined;
        var gw: Io.Writer = .fixed(&gb);
        transform.writeGeometryRefusal(&gw, tcfg, chunk) catch {};
        p.stderr.print("squeezefs format: {s}\n", .{gw.buffered()}) catch {};
        return cli.exit_failed;
    };

    // Key material, salt and key id BEFORE any destructive step. Only the
    // salt and the id are recorded; the material is zeroized here.
    var key_ref: ?set_config.KeyRef = null;
    if (f.encryption != .none) {
        var salt: [transform.KDF_SALT_LEN]u8 = undefined;
        p.io.randomSecure(&salt) catch {
            p.stderr.writeAll("squeezefs format: getrandom failed (the KDF salt); refusing to format with a weaker salt\n") catch {};
            return cli.exit_failed;
        };
        var material_buf: [transform.MAX_KEY_FILE_BYTES + 1]u8 = undefined;
        defer std.crypto.secureZero(u8, &material_buf);
        const key_path = f.encrypt_key.?;
        const material = (if (std.mem.eql(u8, key_path, "-")) transform.linux_keys.readKeyStdin(&material_buf) else transform.linux_keys.readKeyFile(key_path, &material_buf)) catch |e| {
            var kb: [512]u8 = undefined;
            var kw: Io.Writer = .fixed(&kb);
            transform.writeKeyFileRefusal(&kw, e, key_path) catch {};
            p.stderr.print("squeezefs format: {s}\n", .{kw.buffered()}) catch {};
            return cli.exit_failed;
        };
        var derived = transform.deriveVolumeKey(material, &salt);
        defer derived.zeroize();
        key_ref = .{ .kdf_salt = salt, .key_id = derived.key_id };
    }

    // The io pool: registration (O_DIRECT) + the preflight read + the image.
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
        p.stderr.print("squeezefs format: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();

    // The metadata preflight, every member.
    for (meta.paths) |mp| {
        if (status.writerGuardHeld(mp)) {
            p.stderr.print("squeezefs format: cannot format {s}: a live daemon holds its single-writer guard (umount it first; --force does not override a live mount)\n", .{mp}) catch {};
            return cli.exit_failed;
        }
        var reason_buf: [512]u8 = undefined;
        const prior = status.classify(p.gpa, p.env, pool, mp, &reason_buf) catch |e| {
            p.stderr.print("squeezefs format: cannot read {s}: {s}\n", .{ mp, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        var gate_buf: [1024]u8 = undefined;
        var gw: Io.Writer = .fixed(&gate_buf);
        const go = forceGate(prior.prior, f.force, &gw) catch false;
        if (!go) {
            p.stderr.print("squeezefs format: {s}: {s}\n", .{ mp, gw.buffered() }) catch {};
            return cli.exit_failed;
        }
    }

    // Every data device: read, classified, gated, sized.
    var devices: [set_config.MAX_DATA_VOLUMES]DataDevice = undefined;
    for (data_paths[0..n_data], 0..) |dp, i| {
        var dreason: [512]u8 = undefined;
        var dprior: DataPrior = undefined;
        var opened: ?data_superblock.linux_dev.Opened = null;
        if (data_superblock.linux_dev.open(p.gpa, dp)) |o| {
            opened = o;
            dprior = switch (o.classified.format) {
                // Blank to our magic: the foreign probe has the last word
                // (the same law as the metadata volume's).
                .blank => if (status.probeForeignOnPath(p.gpa, p.env, pool, dp) catch null) |hit| .{ .refused = std.fmt.bufPrint(&dreason, "{f}", .{hit}) catch "" } else .blank,
                .v1 => |sb| .{ .formatted = sb },
            };
        } else |e| switch (e) {
            error.DeviceRefused, error.Io, error.OutOfMemory, error.ReadbackMismatch, error.Unexpected => {
                p.stderr.print("squeezefs format: cannot read data volume {s}: {s} (a regular file or block device that opens O_DIRECT is required)\n", .{ dp, @errorName(e) }) catch {};
                return cli.exit_failed;
            },
            else => dprior = .{ .refused = std.fmt.bufPrint(&dreason, "{s}", .{@errorName(e)}) catch "" },
        }
        var gate_buf: [1024]u8 = undefined;
        var gw: Io.Writer = .fixed(&gate_buf);
        const go = dataForceGate(dp, dprior, f.force, &gw) catch false;
        if (!go) {
            p.stderr.print("squeezefs format: {s}\n", .{gw.buffered()}) catch {};
            return cli.exit_failed;
        }
        // A refused superblock (a foreign signature under --force) still
        // needs the device's length; a fresh generation starts at 1.
        const volume_len = if (opened) |o| o.volume_len else device_loader.volumeLength(dp) catch {
            p.stderr.print("squeezefs format: cannot size data volume {s}\n", .{dp}) catch {};
            return cli.exit_failed;
        };
        if (transform.capacityBlocks(volume_len, chunk) < 1) {
            p.stderr.print("squeezefs format: data volume {s} ({d} B) holds fewer than 3 chunks of {d} B (two are reserved; at least one usable block is required) — grow it or pass a smaller --block-size\n", .{ dp, volume_len, chunk }) catch {};
            return cli.exit_failed;
        }
        devices[i] = .{ .path = dp, .volume_len = volume_len, .next_generation = if (opened) |o| o.next_generation else 1 };
    }

    // Identities: a uuid per metadata volume, the set's (uuid, hash seed),
    // a tag and a device uuid per data volume — every one an independent
    // draw.
    var vol_uuids: [uri.MAX_MEMBERS][16]u8 = undefined;
    for (vol_uuids[0..meta.paths.len]) |*u| p.io.randomSecure(u) catch {
        p.stderr.writeAll("squeezefs format: getrandom failed\n") catch {};
        return cli.exit_failed;
    };
    const set = geometry.mintSetIdentity(p.io) catch {
        p.stderr.writeAll("squeezefs format: getrandom failed\n") catch {};
        return cli.exit_failed;
    };
    const owner = invokingOwner(p.env);
    var records: [set_config.MAX_DATA_VOLUMES]set_config.DataVolume = undefined;
    const now_s = clock.realtimeCoarseS();
    for (devices[0..n_data], 0..) |d, i| {
        var tag: u64 = undefined;
        var device_uuid: [16]u8 = undefined;
        p.io.randomSecure(std.mem.asBytes(&tag)) catch {
            p.stderr.writeAll("squeezefs format: getrandom failed\n") catch {};
            return cli.exit_failed;
        };
        p.io.randomSecure(&device_uuid) catch {
            p.stderr.writeAll("squeezefs format: getrandom failed\n") catch {};
            return cli.exit_failed;
        };
        for (records[0..i]) |prev| if (prev.vol_tag == tag) {
            // 2^-64 per pair; still never two volumes under one tag.
            p.stderr.writeAll("squeezefs format: getrandom produced a repeated vol_tag; re-run\n") catch {};
            return cli.exit_failed;
        };
        records[i] = .{ .vol_tag = tag, .state = .active, .device_uuid = device_uuid, .added_ts = now_s, .capacity_blocks = transform.capacityBlocks(d.volume_len, chunk), .volume_len = d.volume_len, .uri = d.path };
    }

    // The record (its cap is the per-volume record-value cap).
    var cache_paths: [args.MAX_CACHE_PATHS][]const u8 = undefined;
    var n_paths: usize = 0;
    var it = args.cachePaths(f.disk_cache_paths);
    while (it.next()) |cp| {
        cache_paths[n_paths] = cp;
        n_paths += 1;
    }
    var cfg = set_config.birth(p.gpa, set.set_uuid, tcfg, key_ref, records[0..n_data], cache_paths[0..n_paths]) catch {
        p.stderr.writeAll("squeezefs format: out of memory\n") catch {};
        return cli.exit_failed;
    };
    defer cfg.deinit(p.gpa);
    const node_size = f.node_size orelse superblock.default_node_size;
    const record_bytes = set_config.encode(p.gpa, &cfg, @intCast(geometry.recordValueCap(node_size))) catch |e| {
        p.stderr.print("squeezefs format: the set_config record cannot be written: {s} (the per-volume record cap at node {d} B is {d} B)\n", .{ @errorName(e), node_size, geometry.recordValueCap(node_size) }) catch {};
        return cli.exit_failed;
    };
    defer p.gpa.free(record_bytes);
    // The storage-trust secret (charter §8 `job:enroll`): minted ONCE at
    // format from the secure source; whoever can read it is inside the
    // trust domain, so every cluster-wire peer proves possession of it.
    const enroll_secret = wire_auth.mintSecret(p.io) catch |e| {
        p.stderr.print("squeezefs format: the job:enroll secret cannot be minted: {s}\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    const enroll_bytes = wire_auth.encodeEnroll(enroll_secret);

    // Destructive steps, device-before-record: every data superblock first.
    for (devices[0..n_data], records[0..n_data]) |d, r| {
        const sb: data_superblock.DataSuperblock = .{
            .block_size = tcfg.block_size,
            .chunk_size = chunk,
            .compression = tcfg.compression,
            .encryption = tcfg.encryption,
            .volume_len = d.volume_len,
            .vol_tag = r.vol_tag,
            .set_uuid = set.set_uuid,
            .device_uuid = r.device_uuid,
            .features_incompat = data_superblock.featuresFor(tcfg),
        };
        data_superblock.linux_dev.format(p.gpa, d.path, sb, d.next_generation) catch |e| {
            p.stderr.print("squeezefs format: data volume {s}: {s}\n", .{ d.path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
    }

    // The metadata members, member 0 (slot 0's host) with the record in
    // tree 0, every member its stride run.
    for (meta.paths, 0..) |mp, i| {
        backend.formatVolume(p.gpa, mp, .{
            .env = p.env,
            .pool = pool,
            .class = if (f.single_writer) .single_writer else .symmetric,
            .node_size = node_size,
            .journal_len_override = f.journal_len,
            .uuid = vol_uuids[i],
            .set = set,
            .root_uid = owner.uid,
            .root_gid = owner.gid,
            .member_position = @intCast(i),
            .member_count = @intCast(meta.paths.len),
            // Tree 0's format-time records, key-sorted: `job:enroll` < `set_config`.
            .control_records = if (i == 0) &.{ .{ .key = wire_auth.ENROLL_KEY, .seq = 0, .kind = .put, .value = &enroll_bytes }, .{ .key = set_config.CONTROL_KEY, .seq = 0, .kind = .put, .value = record_bytes } } else &.{},
        }) catch |e| {
            p.stderr.print("squeezefs format: {s}: {s}\n", .{ mp, formatErrorText(e) }) catch {};
            return cli.exit_failed;
        };
    }

    // The summary.
    const vlen = device_loader.volumeLength(path) catch 0;
    p.stdout.print("formatted {s}: SqueezeFS format v4, {d}-member set ({s}), member 0 {d} B, node {d} B, journal {s}, root owner {d}:{d}\n", .{
        f.meta_uri,
        meta.paths.len,
        if (f.single_writer) "--single-writer: the flat class, no appender region" else "the symmetric plane: SYMMETRIC_PLANE stamped, the appender directory and appender 0's page written",
        vlen,
        node_size,
        if (f.journal_len != null) "explicit" else "derived clamp(volume/64, 8 MiB, 32 MiB)",
        owner.uid,
        owner.gid,
    }) catch {};
    cfg.writeSummary(p.stdout) catch {};
    if (n_data == 0) p.stdout.writeAll("  (no data volumes: the set_config record lists none; `squeezefs volume add-data` adds them at rung 8)\n") catch {};
    p.stdout.flush() catch {};
    return cli.exit_ok;
}

fn formatErrorText(e: backend.FormatError) []const u8 {
    return switch (e) {
        error.DeviceRefused => "the device refused registration (O_DIRECT posture) or does not exist",
        error.VolumeTooSmall => "the volume is too small for format v4 (see the log for the geometry)",
        error.InvalidNodeSize => "invalid node size",
        error.InvalidJournalOverride => "invalid --meta-journal-mb (below the floor — 384 KiB, plus 16 KiB for appender 0's page slots on the symmetric class — or not a 4 KiB multiple)",
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

test "the force gate names a FOREIGN filesystem signature and refuses without --force (fstests generic/740: a bare mkfs over ext2/bfs/cramfs); --force destroys it knowingly" {
    var buf: [512]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    const hit: foreign_signature.Hit = .{ .name = "ext2/3/4", .offset = 1080 };
    try testing.expect(!try forceGate(.{ .foreign = hit }, false, &w));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "ext2/3/4 signature at offset 1080") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "--force") != null);
    try testing.expect(try forceGate(.{ .foreign = hit }, true, &w));
}

test "the data force gate: Blank formats; a squeezefs data volume names its tag and --force; a foreign signature names the reason and --force" {
    var buf: [512]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try testing.expect(try dataForceGate("/dev/d", .blank, false, &w));
    var sb: data_superblock.DataSuperblock = undefined;
    sb.vol_tag = 0xabcd;
    try testing.expect(!try dataForceGate("/dev/d", .{ .formatted = sb }, false, &w));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "vol-000000000000abcd") != null and std.mem.indexOf(u8, w.buffered(), "--force") != null);
    w = .fixed(&buf);
    try testing.expect(!try dataForceGate("/dev/d", .{ .refused = "InvalidMagic" }, false, &w));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "InvalidMagic") != null);
    try testing.expect(try dataForceGate("/dev/d", .{ .formatted = sb }, true, &w));
    try testing.expect(try dataForceGate("/dev/d", .{ .refused = "x" }, true, &w));
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
