//! THE volume-set config (charter §8 `format`/`config`/`volume *`; §5.4
//! order-independent discovery; `docs/porting/data_transform_config.md` §5):
//! ONE control record `set_config` on volume 0's tree 0 (`TREE_CONTROL`),
//! written by the format transaction (a fresh set is never "legacy-
//! shaped"), mutated only by the admin verbs under the D0 guard, the ONE
//! source of the data-volume set. The META set's membership stays the
//! ledger stamp's truth (decision 0002 row 46): nothing here mirrors it.
//!
//! The codec is canonical, total, versioned and checksummed, every field by
//! position (`ledger.zig`'s discipline): a 128 B little-endian head, then
//! `n_data` data-volume records (64 B fixed + the uri), then `n_cache_paths`
//! length-framed paths, then xxh3-64 over everything before it. A lying
//! length is never an allocation authority: every count is checked against
//! the bytes actually present before anything is allocated. The reference
//! kept this as a `serde_json` xattr `user.squeezefs.format_config` on ino
//! 1, additive, unversioned, unchecksummed (decision 0002 rows 121–124).
//!
//! Mutations are PURE: `(config, verb) → next config` with
//! `config_generation + 1` and the verdict "the VOLUME_LIFECYCLE bit must
//! be stamped before this record" (`Mutation.lifecycle_bit_needed`). The
//! durability order every verb follows (spec §5.6 c; the verbs themselves
//! are rung 8's `cli/volume.zig`, `cli/config.zig`):
//!   1. the live-writer gate on EVERY meta volume (a live daemon refuses
//!      even with `--force`);
//!   2. read + decode this record (undecodable ⇒ refuse by name);
//!   3. preflight (device identity, the capacity closed form, the path
//!      denylist);
//!   4. stamp `VOLUME_LIFECYCLE` (bit 3) on every member superblock,
//!      barriered, iff `lifecycle_bit_needed` — BEFORE the record tx (a
//!      crash after the bit and before the record leaves a set old
//!      binaries refuse and this binary mounts unchanged);
//!   5. ONE conveyor tx on volume 0: `Put(set_config)` at `generation + 1`
//!      (for `add-data`: the device's superblock was written BEFORE this
//!      tx — device-before-record — and placement is enabled strictly
//!      AFTER the ack);
//!   6. clean close (checkpoint).
//! Portable.

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const transform = @import("transform.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const Compression = transform.Compression;
const Encryption = transform.Encryption;

/// The control-tree key (kind `TREE_CONTROL` rides the record frame; the
/// charter's `slot_state:{s}` naming style).
pub const CONTROL_KEY = "set_config";
pub const MAGIC = "SQZSETCF";
pub const VERSION: u16 = 1;

/// The caps the verbs refuse at (never the mount): one argv of 4096
/// volumes is past any operator; 4096 is `PATH_MAX`.
pub const MAX_DATA_VOLUMES: usize = 4096;
pub const MAX_URI_LEN: usize = 4096;
pub const MAX_CACHE_PATHS: usize = 256;

/// `active | disabled | draining | retired`: `Active → Draining → Retired`,
/// `Draining → Active` on undrain, `Active ↔ Disabled` (the fail-stop
/// health override: reads EIO, no placements, NOT an evacuation);
/// `Retired` is terminal and its record is kept FOREVER (ids are
/// permanent: a stale key can never resolve to the wrong device).
pub const State = enum(u8) {
    active = 1,
    disabled = 2,
    draining = 3,
    retired = 4,

    pub fn fromWire(b: u8) ?State {
        return switch (b) {
            1 => .active,
            2 => .disabled,
            3 => .draining,
            4 => .retired,
            else => null,
        };
    }

    pub fn name(self: State) []const u8 {
        return @tagName(self);
    }
};

pub const DataVolume = struct {
    /// The id (`vol-{16 hex}` is its spelling): random, minted once, never
    /// reused — a KEY component of every block ref.
    vol_tag: u64,
    state: State,
    /// The device's own identity (the superblock's), minted at format;
    /// distinct from the tag: the tag names the volume in keys, the uuid
    /// names the device in discovery.
    device_uuid: [16]u8,
    /// `realtimeCoarseS` at add.
    added_ts: u64,
    /// Usable blocks at add (`transform.capacityBlocks`).
    capacity_blocks: u64,
    /// Device length at add (the superblock's self-location).
    volume_len: u64,
    /// Absolute path, no NUL; empty on a retired record.
    uri: []const u8,
};

/// The head: 128 B, little-endian, reserved bytes zero.
pub const HeadWire = extern struct {
    magic: [8]u8,
    version_le: [2]u8,
    reserved0_le: [2]u8,
    /// Monotone; +1 per committed mutation; the live mirror's compare word.
    config_generation_le: [8]u8,
    block_size_le: [4]u8,
    /// `transform.chunkSizeFor`: block + headroom on a transformed set.
    chunk_size_le: [4]u8,
    /// Σ usable bytes over the non-retired data volumes (≤ Σ physical).
    capacity_le: [8]u8,
    compression: u8,
    encryption: u8,
    /// 0 none, 2 = hkdf-sha256/aead-kw/v2.
    key_scheme: u8,
    reserved1: u8,
    /// Zero when `encryption == none`.
    kdf_salt: [transform.KDF_SALT_LEN]u8,
    key_id: [transform.KEY_ID_LEN]u8,
    /// The META set's identity: this record must agree with the stamp of
    /// the volume it lives on (a foreign record is refused by name).
    set_uuid: [16]u8,
    /// The durable floor of the block-nonce counter: a mount starts its
    /// counter here and a verb raises it to the live word, so a (salt,
    /// counter) pair is never reused across incarnations even though the
    /// per-mount data key already makes reuse harmless.
    nonce_floor_le: [8]u8,
    n_data_le: [2]u8,
    n_cache_paths_le: [2]u8,
    /// 0 = derived (`layout.inlineMax`); a format-time override pins the
    /// inline ceiling for the set's life (a file's layout class is a
    /// format-era fact).
    inline_max_le: [4]u8,
    reserved: [16]u8,
};

pub const HEAD_LEN: usize = @sizeOf(HeadWire);

/// A data-volume record's fixed part: 64 B, then `uri_len` bytes.
pub const DataVolumeWire = extern struct {
    vol_tag_le: [8]u8,
    state: u8,
    reserved0: [7]u8,
    device_uuid: [16]u8,
    added_ts_le: [8]u8,
    capacity_blocks_le: [8]u8,
    volume_len_le: [8]u8,
    uri_len_le: [2]u8,
    reserved1: [6]u8,
};

pub const DATA_VOLUME_FIXED_LEN: usize = @sizeOf(DataVolumeWire);
pub const CHECKSUM_LEN: usize = 8;

comptime {
    assert(HEAD_LEN == 128);
    assert(DATA_VOLUME_FIXED_LEN == 64);
    assert(@offsetOf(HeadWire, "config_generation_le") == 12);
    assert(@offsetOf(HeadWire, "kdf_salt") == 40);
    assert(@offsetOf(HeadWire, "set_uuid") == 80);
    assert(@offsetOf(HeadWire, "n_data_le") == 104);
}

pub const Config = struct {
    config_generation: u64,
    set_uuid: [16]u8,
    block_size: u32,
    chunk_size: u32,
    capacity: u64,
    compression: Compression,
    encryption: Encryption,
    key_scheme: u8,
    kdf_salt: [transform.KDF_SALT_LEN]u8,
    key_id: [transform.KEY_ID_LEN]u8,
    nonce_floor: u64,
    inline_max: u32,
    data: []DataVolume,
    cache_paths: [][]const u8,

    pub fn deinit(self: *Config, gpa: Allocator) void {
        for (self.data) |d| gpa.free(d.uri);
        gpa.free(self.data);
        for (self.cache_paths) |p| gpa.free(p);
        gpa.free(self.cache_paths);
        self.* = undefined;
    }

    pub fn transformCfg(self: *const Config) transform.VolumeTransform {
        return .{ .compression = self.compression, .encryption = self.encryption, .block_size = self.block_size };
    }

    pub fn find(self: *const Config, vol_tag: u64) ?*DataVolume {
        for (self.data) |*d| if (d.vol_tag == vol_tag) return d;
        return null;
    }

    /// Deep copy (the mutations' starting point).
    pub fn clone(self: *const Config, gpa: Allocator) Allocator.Error!Config {
        var out = self.*;
        out.data = try gpa.alloc(DataVolume, self.data.len);
        var n_data: usize = 0;
        errdefer {
            for (out.data[0..n_data]) |d| gpa.free(d.uri);
            gpa.free(out.data);
        }
        for (self.data, out.data) |s, *d| {
            d.* = s;
            d.uri = try gpa.dupe(u8, s.uri);
            n_data += 1;
        }
        out.cache_paths = try gpa.alloc([]const u8, self.cache_paths.len);
        var n_paths: usize = 0;
        errdefer {
            for (out.cache_paths[0..n_paths]) |p| gpa.free(p);
            gpa.free(out.cache_paths);
        }
        for (self.cache_paths, out.cache_paths) |s, *p| {
            p.* = try gpa.dupe(u8, s);
            n_paths += 1;
        }
        return out;
    }

    /// The encoded length (head + records + paths + checksum).
    pub fn encodedLen(self: *const Config) usize {
        var n: usize = HEAD_LEN + CHECKSUM_LEN;
        for (self.data) |d| n += DATA_VOLUME_FIXED_LEN + d.uri.len;
        for (self.cache_paths) |p| n += 2 + p.len;
        return n;
    }

    /// Σ usable bytes over non-retired data volumes.
    pub fn recomputeCapacity(self: *Config) void {
        var sum: u64 = 0;
        for (self.data) |d| if (d.state != .retired) {
            sum += d.capacity_blocks * self.block_size;
        };
        self.capacity = sum;
    }

    /// Has the data set diverged from its birth state (a second member, or
    /// any non-`active` state)? The ONE meaning of `VOLUME_LIFECYCLE`
    /// (bit 3): block keys may name a `vol_tag` this URI list does not
    /// serve — consult the record before resolving a tag (decision 0002
    /// row 124).
    pub fn lifecycleDiverged(self: *const Config) bool {
        if (self.data.len >= 2) return true;
        for (self.data) |d| if (d.state != .active) return true;
        return false;
    }

    /// The JSON twin of `writeSummary` (`.config`, `config show --json`,
    /// `df --json`): `{set_uuid, config_generation, block_size, chunk_size,
    /// capacity_bytes, compression, encryption, key_id?, inline_max, data[{
    /// vol_tag, state, device_uuid, uri, capacity_blocks, volume_len}],
    /// cache_paths[]}` — exactly the registered volumes, snake_case
    /// (`stats_cli.md` §1.5, §7 Q18).
    pub fn writeJson(self: *const Config, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.writeAll("{\"set_uuid\":\"");
        try writeUuid(w, self.set_uuid);
        try w.print("\",\"config_generation\":{d},\"block_size\":{d},\"chunk_size\":{d},\"capacity_bytes\":{d},\"compression\":\"{s}\",\"encryption\":\"{s}\"", .{ self.config_generation, self.block_size, self.chunk_size, self.capacity, @tagName(self.compression), self.encryption.name() });
        if (self.encryption != .none) try w.print(",\"key_id\":\"{x}\"", .{&self.key_id});
        try w.print(",\"inline_max\":{d},\"lifecycle_diverged\":{},\"data\":[", .{ self.inline_max, self.lifecycleDiverged() });
        for (self.data, 0..) |d, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"vol_tag\":\"vol-{x:0>16}\",\"state\":\"{s}\",\"device_uuid\":\"", .{ d.vol_tag, d.state.name() });
            try writeUuid(w, d.device_uuid);
            try w.print("\",\"uri\":\"{s}\",\"capacity_blocks\":{d},\"volume_len\":{d}}}", .{ d.uri, d.capacity_blocks, d.volume_len });
        }
        try w.writeAll("],\"cache_paths\":[");
        for (self.cache_paths, 0..) |p, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("\"{s}\"", .{p});
        }
        try w.writeAll("]}");
    }

    /// The human summary (`status`, `format`).
    pub fn writeSummary(self: *const Config, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("  set config generation {d}; block {d} B; chunk {d} B; capacity {d} B ({d} MiB); compression {s}; encryption {s}", .{ self.config_generation, self.block_size, self.chunk_size, self.capacity, self.capacity >> 20, @tagName(self.compression), self.encryption.name() });
        if (self.encryption != .none) {
            try w.print("; key {s} id {x} (resolution: --encrypt-key → SQUEEZEFS_ENCRYPT_KEY_FILE → {s}/{x}.key)", .{ transform.KEY_WRAP_SCHEME_NAME, &self.key_id, transform.KEY_DIR, &self.key_id });
        }
        if (self.inline_max != 0) try w.print("; inline_max override {d}", .{self.inline_max});
        try w.print("\n  data volumes {d}", .{self.data.len});
        if (self.lifecycleDiverged()) try w.writeAll(" (VOLUME_LIFECYCLE: the set has diverged from its birth state)");
        try w.writeAll("\n");
        for (self.data) |d| {
            try w.print("    vol-{x:0>16} {s} {s} {d} blocks ({d} MiB) device ", .{ d.vol_tag, d.state.name(), if (d.uri.len == 0) "(retired; no uri)" else d.uri, d.capacity_blocks, (d.capacity_blocks * self.block_size) >> 20 });
            try writeUuid(w, d.device_uuid);
            try w.writeAll("\n");
        }
        if (self.cache_paths.len == 0) {
            try w.writeAll("  cache-less (no disk cache paths)\n");
        } else {
            try w.print("  disk cache paths {d}:", .{self.cache_paths.len});
            for (self.cache_paths) |p| try w.print(" {s}", .{p});
            try w.writeAll("\n");
        }
    }
};

pub fn writeUuid(w: *std.Io.Writer, u: [16]u8) std.Io.Writer.Error!void {
    try w.print("{x}-{x}-{x}-{x}-{x}", .{ u[0..4], u[4..6], u[6..8], u[8..10], u[10..16] });
}

/// `vol-{16 hex}`.
pub fn writeVolTag(w: *std.Io.Writer, tag: u64) std.Io.Writer.Error!void {
    try w.print("vol-{x:0>16}", .{tag});
}

// ---------------------------------------------------------------------------
// The codec.
// ---------------------------------------------------------------------------

pub const EncodeError = error{ RecordTooLarge, TooManyDataVolumes, UriTooLong, TooManyCachePaths, CachePathTooLong, OutOfMemory };

/// Encode into a fresh buffer (caller frees). `cap` is the per-volume
/// record-value cap (`geometry.recordValueCap(node_size)`): refused HERE,
/// at the verb, never at mount.
pub fn encode(gpa: Allocator, cfg: *const Config, cap: usize) EncodeError![]u8 {
    if (cfg.data.len > MAX_DATA_VOLUMES) return error.TooManyDataVolumes;
    for (cfg.data) |d| if (d.uri.len > MAX_URI_LEN) return error.UriTooLong;
    if (cfg.cache_paths.len > MAX_CACHE_PATHS) return error.TooManyCachePaths;
    for (cfg.cache_paths) |p| if (p.len > MAX_URI_LEN) return error.CachePathTooLong;
    const len = cfg.encodedLen();
    if (len > cap) return error.RecordTooLarge;
    const out = try gpa.alloc(u8, len);
    errdefer gpa.free(out);
    var h: HeadWire = undefined;
    h.magic = MAGIC.*;
    std.mem.writeInt(u16, &h.version_le, VERSION, .little);
    std.mem.writeInt(u16, &h.reserved0_le, 0, .little);
    std.mem.writeInt(u64, &h.config_generation_le, cfg.config_generation, .little);
    std.mem.writeInt(u32, &h.block_size_le, cfg.block_size, .little);
    std.mem.writeInt(u32, &h.chunk_size_le, cfg.chunk_size, .little);
    std.mem.writeInt(u64, &h.capacity_le, cfg.capacity, .little);
    h.compression = @backingInt(cfg.compression);
    h.encryption = @backingInt(cfg.encryption);
    h.key_scheme = cfg.key_scheme;
    h.reserved1 = 0;
    h.kdf_salt = cfg.kdf_salt;
    h.key_id = cfg.key_id;
    h.set_uuid = cfg.set_uuid;
    std.mem.writeInt(u64, &h.nonce_floor_le, cfg.nonce_floor, .little);
    std.mem.writeInt(u16, &h.n_data_le, @intCast(cfg.data.len), .little);
    std.mem.writeInt(u16, &h.n_cache_paths_le, @intCast(cfg.cache_paths.len), .little);
    std.mem.writeInt(u32, &h.inline_max_le, cfg.inline_max, .little);
    h.reserved = @splat(0);
    out[0..HEAD_LEN].* = std.mem.toBytes(h);
    var off: usize = HEAD_LEN;
    for (cfg.data) |d| {
        var r: DataVolumeWire = undefined;
        std.mem.writeInt(u64, &r.vol_tag_le, d.vol_tag, .little);
        r.state = @backingInt(d.state);
        r.reserved0 = @splat(0);
        r.device_uuid = d.device_uuid;
        std.mem.writeInt(u64, &r.added_ts_le, d.added_ts, .little);
        std.mem.writeInt(u64, &r.capacity_blocks_le, d.capacity_blocks, .little);
        std.mem.writeInt(u64, &r.volume_len_le, d.volume_len, .little);
        std.mem.writeInt(u16, &r.uri_len_le, @intCast(d.uri.len), .little);
        r.reserved1 = @splat(0);
        out[off..][0..DATA_VOLUME_FIXED_LEN].* = std.mem.toBytes(r);
        off += DATA_VOLUME_FIXED_LEN;
        @memcpy(out[off..][0..d.uri.len], d.uri);
        off += d.uri.len;
    }
    for (cfg.cache_paths) |p| {
        std.mem.writeInt(u16, out[off..][0..2], @intCast(p.len), .little);
        off += 2;
        @memcpy(out[off..][0..p.len], p);
        off += p.len;
    }
    assert(off + CHECKSUM_LEN == len);
    checksum.stamp(out, off);
    return out;
}

pub const DecodeError = error{
    BadLength,
    BadMagic,
    WrongVersion,
    ChecksumMismatch,
    ReservedNotZero,
    BadMode,
    BadKeyScheme,
    BadState,
    BadGeometry,
    /// A count or length past the bytes present.
    Truncated,
    /// Bytes after the declared structure, before the checksum.
    TrailingBytes,
    /// A uri with a NUL or not absolute; a non-empty uri on a retired
    /// record; a duplicate tag or device uuid.
    BadRecord,
    OutOfMemory,
};

pub const Refusal = union(enum) {
    bad_length: usize,
    bad_magic,
    wrong_version: u16,
    checksum_mismatch,
    reserved_not_zero,
    bad_mode,
    bad_key_scheme: u8,
    bad_state: u8,
    bad_geometry: struct { block: u32, chunk: u32 },
    truncated,
    trailing_bytes,
    bad_record: usize,

    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .bad_length => |n| try w.print("set_config record of {d} bytes is shorter than its {d}-byte head + checksum", .{ n, HEAD_LEN + CHECKSUM_LEN }),
            .bad_magic => try w.writeAll("set_config record has a bad magic (a foreign or corrupt control record)"),
            .wrong_version => |v| try w.print("set_config record version {d} is not {d} — upgrade squeezefs", .{ v, VERSION }),
            .checksum_mismatch => try w.writeAll("set_config record checksum mismatch — corrupt control record"),
            .reserved_not_zero => try w.writeAll("set_config record reserved bytes are not zero (a writer this binary does not know)"),
            .bad_mode => try w.writeAll("set_config record names a compression or encryption mode this binary does not know — upgrade squeezefs"),
            .bad_key_scheme => |s| try w.print("set_config record names key-wrap scheme {d}; this binary knows 0 (none) and {d} ({s}) — upgrade squeezefs", .{ s, transform.KEY_WRAP_SCHEME_V2, transform.KEY_WRAP_SCHEME_NAME }),
            .bad_state => |s| try w.print("set_config data volume state {d} is not one of active|disabled|draining|retired", .{s}),
            .bad_geometry => |g| try w.print("set_config geometry: block {d} / chunk {d} is not a shape this binary formats", .{ g.block, g.chunk }),
            .truncated => try w.writeAll("set_config record declares more data volumes or paths than its bytes hold"),
            .trailing_bytes => try w.writeAll("set_config record has bytes after its declared structure"),
            .bad_record => |i| try w.print("set_config data volume record {d} is malformed (uri not absolute, a NUL, a retired record with a uri, or a duplicate tag/device)", .{i}),
        }
    }
};

fn refuse(diag: ?*Refusal, r: Refusal) DecodeError {
    if (diag) |d| d.* = r;
    return switch (r) {
        .bad_length => error.BadLength,
        .bad_magic => error.BadMagic,
        .wrong_version => error.WrongVersion,
        .checksum_mismatch => error.ChecksumMismatch,
        .reserved_not_zero => error.ReservedNotZero,
        .bad_mode => error.BadMode,
        .bad_key_scheme => error.BadKeyScheme,
        .bad_state => error.BadState,
        .bad_geometry => error.BadGeometry,
        .truncated => error.Truncated,
        .trailing_bytes => error.TrailingBytes,
        .bad_record => error.BadRecord,
    };
}

/// The decoder in its exact check order: length → magic → version →
/// checksum → head fields → `n_data × 64 ≤ remaining` (the allocation
/// authority is the bytes present) → each record's fixed part and its uri
/// length against the remaining bytes → the paths likewise → exactly the
/// checksum left → record laws. Total.
pub fn decode(gpa: Allocator, bytes: []const u8, diag: ?*Refusal) DecodeError!Config {
    if (bytes.len < HEAD_LEN + CHECKSUM_LEN) return refuse(diag, .{ .bad_length = bytes.len });
    if (!std.mem.eql(u8, bytes[0..8], MAGIC)) return refuse(diag, .bad_magic);
    const version = std.mem.readInt(u16, bytes[8..10], .little);
    if (version != VERSION) return refuse(diag, .{ .wrong_version = version });
    const sum_off = bytes.len - CHECKSUM_LEN;
    if (!checksum.verify(bytes, sum_off)) return refuse(diag, .checksum_mismatch);
    const h = std.mem.bytesToValue(HeadWire, bytes[0..HEAD_LEN]);
    if (std.mem.readInt(u16, &h.reserved0_le, .little) != 0 or h.reserved1 != 0 or !std.mem.allEqual(u8, &h.reserved, 0)) return refuse(diag, .reserved_not_zero);
    const compression = Compression.fromWire(h.compression) orelse return refuse(diag, .bad_mode);
    const encryption = Encryption.fromWire(h.encryption) orelse return refuse(diag, .bad_mode);
    if (h.key_scheme != 0 and h.key_scheme != transform.KEY_WRAP_SCHEME_V2) return refuse(diag, .{ .bad_key_scheme = h.key_scheme });
    if ((h.key_scheme != 0) != (encryption != .none)) return refuse(diag, .{ .bad_key_scheme = h.key_scheme });
    const block_size = std.mem.readInt(u32, &h.block_size_le, .little);
    const chunk_size = std.mem.readInt(u32, &h.chunk_size_le, .little);
    const tcfg: transform.VolumeTransform = .{ .compression = compression, .encryption = encryption, .block_size = block_size };
    if (!transform.blockSizeValid(block_size) or chunk_size != transform.chunkSizeFor(tcfg)) return refuse(diag, .{ .bad_geometry = .{ .block = block_size, .chunk = chunk_size } });
    const n_data: usize = std.mem.readInt(u16, &h.n_data_le, .little);
    const n_paths: usize = std.mem.readInt(u16, &h.n_cache_paths_le, .little);
    var off: usize = HEAD_LEN;
    // The counts against the bytes present, before any allocation.
    if (n_data > MAX_DATA_VOLUMES or n_data * DATA_VOLUME_FIXED_LEN > sum_off - off) return refuse(diag, .truncated);
    if (n_paths > MAX_CACHE_PATHS or n_paths * 2 > sum_off - off - n_data * DATA_VOLUME_FIXED_LEN) return refuse(diag, .truncated);
    var cfg: Config = .{
        .config_generation = std.mem.readInt(u64, &h.config_generation_le, .little),
        .set_uuid = h.set_uuid,
        .block_size = block_size,
        .chunk_size = chunk_size,
        .capacity = std.mem.readInt(u64, &h.capacity_le, .little),
        .compression = compression,
        .encryption = encryption,
        .key_scheme = h.key_scheme,
        .kdf_salt = h.kdf_salt,
        .key_id = h.key_id,
        .nonce_floor = std.mem.readInt(u64, &h.nonce_floor_le, .little),
        .inline_max = std.mem.readInt(u32, &h.inline_max_le, .little),
        .data = &.{},
        .cache_paths = &.{},
    };
    const data = try gpa.alloc(DataVolume, n_data);
    var n_done: usize = 0;
    errdefer {
        for (data[0..n_done]) |d| gpa.free(d.uri);
        gpa.free(data);
    }
    for (data, 0..) |*d, i| {
        if (sum_off - off < DATA_VOLUME_FIXED_LEN) return refuse(diag, .truncated);
        const r = std.mem.bytesToValue(DataVolumeWire, bytes[off..][0..DATA_VOLUME_FIXED_LEN]);
        off += DATA_VOLUME_FIXED_LEN;
        if (!std.mem.allEqual(u8, &r.reserved0, 0) or !std.mem.allEqual(u8, &r.reserved1, 0)) return refuse(diag, .reserved_not_zero);
        const state = State.fromWire(r.state) orelse return refuse(diag, .{ .bad_state = r.state });
        const uri_len: usize = std.mem.readInt(u16, &r.uri_len_le, .little);
        if (uri_len > sum_off - off) return refuse(diag, .truncated);
        const uri = bytes[off..][0..uri_len];
        off += uri_len;
        if (uri.len > 0 and (uri[0] != '/' or std.mem.indexOfScalar(u8, uri, 0) != null)) return refuse(diag, .{ .bad_record = i });
        if (state == .retired and uri.len != 0) return refuse(diag, .{ .bad_record = i });
        if (state != .retired and uri.len == 0) return refuse(diag, .{ .bad_record = i });
        const tag = std.mem.readInt(u64, &r.vol_tag_le, .little);
        for (data[0..i]) |prev| if (prev.vol_tag == tag or std.mem.eql(u8, &prev.device_uuid, &r.device_uuid)) return refuse(diag, .{ .bad_record = i });
        d.* = .{
            .vol_tag = tag,
            .state = state,
            .device_uuid = r.device_uuid,
            .added_ts = std.mem.readInt(u64, &r.added_ts_le, .little),
            .capacity_blocks = std.mem.readInt(u64, &r.capacity_blocks_le, .little),
            .volume_len = std.mem.readInt(u64, &r.volume_len_le, .little),
            .uri = try gpa.dupe(u8, uri),
        };
        n_done += 1;
    }
    const paths = try gpa.alloc([]const u8, n_paths);
    var p_done: usize = 0;
    errdefer {
        for (paths[0..p_done]) |p| gpa.free(p);
        gpa.free(paths);
    }
    for (paths) |*p| {
        if (sum_off - off < 2) return refuse(diag, .truncated);
        const len: usize = std.mem.readInt(u16, bytes[off..][0..2], .little);
        off += 2;
        if (len > sum_off - off) return refuse(diag, .truncated);
        const path = bytes[off..][0..len];
        off += len;
        if (len == 0 or path[0] != '/' or std.mem.indexOfScalar(u8, path, 0) != null) return refuse(diag, .{ .bad_record = n_data });
        p.* = try gpa.dupe(u8, path);
        p_done += 1;
    }
    if (off != sum_off) return refuse(diag, .trailing_bytes);
    cfg.data = data;
    cfg.cache_paths = paths;
    return cfg;
}

// ---------------------------------------------------------------------------
// Birth: what `format` records.
// ---------------------------------------------------------------------------

pub const KeyRef = struct {
    kdf_salt: [transform.KDF_SALT_LEN]u8,
    key_id: [transform.KEY_ID_LEN]u8,
};

/// The record of a fresh set: generation 1, every data volume `active`,
/// capacity Σ usable bytes, the key ref iff encrypted. `data` and
/// `cache_paths` are copied.
pub fn birth(gpa: Allocator, set_uuid: [16]u8, tcfg: transform.VolumeTransform, key: ?KeyRef, data: []const DataVolume, cache_paths: []const []const u8) Allocator.Error!Config {
    assert((key != null) == (tcfg.encryption != .none));
    var cfg: Config = .{
        .config_generation = 1,
        .set_uuid = set_uuid,
        .block_size = tcfg.block_size,
        .chunk_size = transform.chunkSizeFor(tcfg),
        .capacity = 0,
        .compression = tcfg.compression,
        .encryption = tcfg.encryption,
        .key_scheme = if (key != null) transform.KEY_WRAP_SCHEME_V2 else 0,
        .kdf_salt = if (key) |k| k.kdf_salt else @splat(0),
        .key_id = if (key) |k| k.key_id else @splat(0),
        .nonce_floor = 0,
        .inline_max = 0,
        .data = @constCast(data),
        .cache_paths = @constCast(cache_paths),
    };
    // `clone` only reads the borrowed slices; the result owns its own.
    var out = try cfg.clone(gpa);
    for (out.data) |*d| d.state = .active;
    out.recomputeCapacity();
    return out;
}

// ---------------------------------------------------------------------------
// The verbs' pure mutations.
// ---------------------------------------------------------------------------

pub const Mutation = struct {
    next: Config,
    /// Stamp `VOLUME_LIFECYCLE` on every member superblock, barriered,
    /// BEFORE the record tx: the data set diverges from its birth state
    /// with this record (the bit flips once, forward-only).
    lifecycle_bit_needed: bool,
};

pub const VerbError = error{
    OutOfMemory,
    /// A `vol_tag`, device uuid or uri already in the set.
    DuplicateVolume,
    UnknownVolume,
    /// The transition the state machine forbids (incl. anything out of
    /// `retired`).
    IllegalTransition,
    /// A path that is not absolute, is a denylisted system root, or
    /// repeats.
    BadCachePath,
    TooManyDataVolumes,
    TooManyCachePaths,
};

fn finish(prev: *const Config, next: Config) Mutation {
    var n = next;
    n.config_generation = prev.config_generation + 1;
    n.recomputeCapacity();
    return .{ .next = n, .lifecycle_bit_needed = n.lifecycleDiverged() and !prev.lifecycleDiverged() };
}

/// ENG-4: a cache path may never be a system root the wipe would destroy.
pub const cache_path_denylist = [_][]const u8{ "/", "/home", "/etc", "/usr", "/var", "/boot", "/root" };

pub fn cachePathAdmissible(path: []const u8) bool {
    if (path.len == 0 or path[0] != '/' or path.len > MAX_URI_LEN or std.mem.indexOfScalar(u8, path, 0) != null) return false;
    // Strip trailing slashes for the denylist compare (`/home/` is `/home`).
    const trimmed = if (path.len > 1) std.mem.trimEnd(u8, path, "/") else path;
    for (cache_path_denylist) |d| if (std.mem.eql(u8, d, trimmed)) return false;
    return true;
}

/// `config set-cache-paths`: replace the list (the wipe/recreate of the
/// directories is the verb's, before this record; content safety does not
/// depend on it — staging generation-binding discards foreign content).
pub fn setCachePaths(gpa: Allocator, prev: *const Config, paths: []const []const u8) VerbError!Mutation {
    if (paths.len > MAX_CACHE_PATHS) return error.TooManyCachePaths;
    for (paths, 0..) |p, i| {
        if (!cachePathAdmissible(p)) return error.BadCachePath;
        for (paths[0..i]) |q| if (std.mem.eql(u8, p, q)) return error.BadCachePath;
    }
    var next = try prev.clone(gpa);
    errdefer next.deinit(gpa);
    for (next.cache_paths) |p| gpa.free(p);
    gpa.free(next.cache_paths);
    next.cache_paths = &.{};
    const list = try gpa.alloc([]const u8, paths.len);
    var done: usize = 0;
    errdefer {
        for (list[0..done]) |p| gpa.free(p);
        gpa.free(list);
    }
    for (paths, list) |p, *q| {
        q.* = try gpa.dupe(u8, p);
        done += 1;
    }
    next.cache_paths = list;
    return finish(prev, next);
}

/// `volume add-data`: a new `active` record. Refuses a tag, device or uri
/// already present (a retired record's tag stays taken forever).
pub fn addDataVolume(gpa: Allocator, prev: *const Config, new: DataVolume) VerbError!Mutation {
    if (prev.data.len >= MAX_DATA_VOLUMES) return error.TooManyDataVolumes;
    for (prev.data) |d| {
        if (d.vol_tag == new.vol_tag or std.mem.eql(u8, &d.device_uuid, &new.device_uuid)) return error.DuplicateVolume;
        if (d.uri.len != 0 and std.mem.eql(u8, d.uri, new.uri)) return error.DuplicateVolume;
    }
    var next = try prev.clone(gpa);
    errdefer next.deinit(gpa);
    const list = try gpa.realloc(next.data, next.data.len + 1);
    next.data = list;
    list[list.len - 1] = new;
    list[list.len - 1].state = .active;
    list[list.len - 1].uri = try gpa.dupe(u8, new.uri);
    return finish(prev, next);
}

/// The lifecycle state machine: `active → draining | disabled`, `draining
/// → active | retired`, `disabled → active`; nothing leaves `retired`.
pub fn transitionAllowed(from: State, to: State) bool {
    return switch (from) {
        .active => to == .draining or to == .disabled,
        .draining => to == .active or to == .retired,
        .disabled => to == .active,
        .retired => false,
    };
}

/// `volume remove-data` (→ draining), `undrain` (→ active), `enable` /
/// `disable`, and the drain's completion (→ retired: the uri is cleared,
/// the record kept forever).
pub fn setState(gpa: Allocator, prev: *const Config, vol_tag: u64, to: State) VerbError!Mutation {
    const cur = prev.find(vol_tag) orelse return error.UnknownVolume;
    if (!transitionAllowed(cur.state, to)) return error.IllegalTransition;
    var next = try prev.clone(gpa);
    errdefer next.deinit(gpa);
    const d = next.find(vol_tag).?;
    d.state = to;
    if (to == .retired) {
        gpa.free(d.uri);
        d.uri = "";
    }
    return finish(prev, next);
}

/// Raise the durable nonce floor to a live counter (a verb's record carries
/// the mount's progress); never lowers it.
pub fn raiseNonceFloor(cfg: *Config, live_counter: u64) void {
    cfg.nonce_floor = @max(cfg.nonce_floor, live_counter);
}

// ---------------------------------------------------------------------------
// §5.6 (e) Mount-time discovery of the DATA volumes — order-independent,
// every record checked against the identity its device carries.
// ---------------------------------------------------------------------------

/// What a data device's superblock says (the `data_superblock` decoder's
/// output in the fields discovery compares).
pub const DeviceIdentity = struct {
    vol_tag: u64,
    device_uuid: [16]u8,
    set_uuid: [16]u8,
    block_size: u32,
    chunk_size: u32,
    compression: Compression,
    encryption: Encryption,
};

pub const Observation = union(enum) {
    /// A zeroed sector 0 (and no valid tail copy).
    blank,
    /// Sector 0 refused (foreign magic, bad checksum, unknown bits …).
    refused,
    identity: DeviceIdentity,
};

pub const DiscoveryRefusal = union(enum) {
    record_set_mismatch: struct { record: [16]u8, stamp: [16]u8 },
    not_formatted: struct { uri: []const u8 },
    device_refused: struct { uri: []const u8 },
    renumbered: struct { uri: []const u8, found: u64, want: u64 },
    foreign_set: struct { uri: []const u8, found: [16]u8, want: [16]u8 },
    device_identity: struct { uri: []const u8 },
    geometry: struct { uri: []const u8 },
    duplicate_tag: struct { uri_a: []const u8, uri_b: []const u8, tag: u64 },

    pub fn write(self: DiscoveryRefusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .record_set_mismatch => |m| {
                try w.writeAll("the set_config record names set ");
                try writeUuid(w, m.record);
                try w.writeAll(" but this volume's membership stamp names ");
                try writeUuid(w, m.stamp);
                try w.writeAll(": a control record from another set — corruption or a foreign image");
            },
            .not_formatted => |r| try w.print("{s} is not a formatted data volume (zeroed superblock) — the set names it; run `squeezefs volume add-data` to format it, or fix the URI", .{r.uri}),
            .device_refused => |r| try w.print("{s} carries a data superblock this binary refuses (see the log) — not a member of this set", .{r.uri}),
            .renumbered => |r| {
                try w.print("{s} carries ", .{r.uri});
                try writeVolTag(w, r.found);
                try w.writeAll(" but the set names ");
                try writeVolTag(w, r.want);
                try w.writeAll(": the device was renumbered or belongs to another set");
            },
            .foreign_set => |r| {
                try w.print("{s} belongs to set ", .{r.uri});
                try writeUuid(w, r.found);
                try w.writeAll(", not this set ");
                try writeUuid(w, r.want);
            },
            .device_identity => |r| try w.print("{s} carries the right vol-tag but a different device identity: a cloned or re-formatted device", .{r.uri}),
            .geometry => |r| try w.print("{s}: the device's block/chunk/transform disagree with the set_config record — the device was formatted for another set", .{r.uri}),
            .duplicate_tag => |r| {
                try w.print("{s} and {s} both carry ", .{ r.uri_a, r.uri_b });
                try writeVolTag(w, r.tag);
                try w.writeAll(": two URIs name one device (a dm/multipath alias, or a cloned device)");
            },
        }
    }
};

pub const DiscoveryError = error{Refused};

/// Check ONE non-retired record against what its uri read.
pub fn checkDataVolume(cfg: *const Config, rec: *const DataVolume, obs: Observation, diag: *DiscoveryRefusal) DiscoveryError!void {
    const id = switch (obs) {
        .blank => {
            diag.* = .{ .not_formatted = .{ .uri = rec.uri } };
            return error.Refused;
        },
        .refused => {
            diag.* = .{ .device_refused = .{ .uri = rec.uri } };
            return error.Refused;
        },
        .identity => |i| i,
    };
    if (!std.mem.eql(u8, &id.set_uuid, &cfg.set_uuid)) {
        diag.* = .{ .foreign_set = .{ .uri = rec.uri, .found = id.set_uuid, .want = cfg.set_uuid } };
        return error.Refused;
    }
    if (id.vol_tag != rec.vol_tag) {
        diag.* = .{ .renumbered = .{ .uri = rec.uri, .found = id.vol_tag, .want = rec.vol_tag } };
        return error.Refused;
    }
    if (!std.mem.eql(u8, &id.device_uuid, &rec.device_uuid)) {
        diag.* = .{ .device_identity = .{ .uri = rec.uri } };
        return error.Refused;
    }
    if (id.block_size != cfg.block_size or id.chunk_size != cfg.chunk_size or id.compression != cfg.compression or id.encryption != cfg.encryption) {
        diag.* = .{ .geometry = .{ .uri = rec.uri } };
        return error.Refused;
    }
}

/// The whole set: the record belongs to the stamp's set; every non-retired
/// record's device reads back as itself (`observations[i]` is what
/// `cfg.data[i].uri` read; retired records are not read and their entry
/// is ignored); no two uris read one tag.
pub fn discover(cfg: *const Config, stamp_set_uuid: [16]u8, observations: []const Observation, diag: *DiscoveryRefusal) DiscoveryError!void {
    assert(observations.len == cfg.data.len);
    if (!std.mem.eql(u8, &cfg.set_uuid, &stamp_set_uuid)) {
        diag.* = .{ .record_set_mismatch = .{ .record = cfg.set_uuid, .stamp = stamp_set_uuid } };
        return error.Refused;
    }
    for (cfg.data, observations, 0..) |*rec, obs, i| {
        if (rec.state == .retired) continue;
        try checkDataVolume(cfg, rec, obs, diag);
        for (cfg.data[0..i], observations[0..i]) |*prev, pobs| {
            if (prev.state == .retired) continue;
            if (pobs.identity.vol_tag == obs.identity.vol_tag) {
                diag.* = .{ .duplicate_tag = .{ .uri_a = prev.uri, .uri_b = rec.uri, .tag = rec.vol_tag } };
                return error.Refused;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §5.5 The capacity closed form (`remove-data`'s preflight, re-verified at
// every coordinator checkpoint).
// ---------------------------------------------------------------------------

/// An in-flight moved block occupies source AND destination until the
/// post-publish free: 64 blocks per ACTUAL worker.
pub const EVACUATE_INFLIGHT_WINDOW_BLOCKS: u64 = 64;
/// Foreground growth slack at a zero rate estimate.
pub const DRAIN_HEADROOM_FLOOR_BYTES: u64 = 1 << 30;
/// ½ of a ~1 GiB/s devsub-class raw copy: the INITIAL eta's rate.
pub const DRAIN_FLOOR_RATE_BYTES_PER_SEC: u64 = 512 << 20;

pub fn transientBytes(workers: u64, block_size: u64) u64 {
    return EVACUATE_INFLIGHT_WINDOW_BLOCKS * workers * block_size;
}

/// `needed / rate + 1` seconds.
pub fn initialDrainEtaS(needed: u64) u64 {
    return needed / DRAIN_FLOOR_RATE_BYTES_PER_SEC + 1;
}

/// `max(write_rate × eta, 1 GiB)`.
pub fn headroomBytes(write_rate_bps: u64, eta_s: u64) u64 {
    return @max(std.math.mul(u64, write_rate_bps, eta_s) catch std.math.maxInt(u64), DRAIN_HEADROOM_FLOOR_BYTES);
}

pub const CapacityPreflight = struct {
    /// Σ free over healthy `active` survivors.
    avail: u64,
    /// The deduped census of referenced bytes on the victim.
    needed: u64,
    transient: u64,
    headroom: u64,

    /// `refuse iff avail < needed + transient + headroom` (saturating).
    pub fn refuses(self: CapacityPreflight) bool {
        return self.avail < self.needed +| self.transient +| self.headroom;
    }

    /// The honest refusal: every term.
    pub fn write(self: CapacityPreflight, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("capacity preflight {s}: avail {d} B {s} needed {d} B + transient {d} B + headroom {d} B = {d} B", .{ if (self.refuses()) "REFUSES" else "passes", self.avail, if (self.refuses()) "<" else ">=", self.needed, self.transient, self.headroom, self.needed +| self.transient +| self.headroom });
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const test_set_uuid: [16]u8 = "set-uuid-fixture".*;
const test_cap: usize = 65_792;

fn fixture(gpa: Allocator) !Config {
    const vols = [_]DataVolume{
        .{ .vol_tag = 0x1111_2222_3333_4444, .state = .active, .device_uuid = @splat(1), .added_ts = 1_700_000_000, .capacity_blocks = 254, .volume_len = 1 << 30, .uri = "/dev/nvme0n1" },
        .{ .vol_tag = 0x5555_6666_7777_8888, .state = .active, .device_uuid = @splat(2), .added_ts = 1_700_000_001, .capacity_blocks = 510, .volume_len = 2 << 30, .uri = "/build/tmp/d1.img" },
    };
    return birth(gpa, test_set_uuid, .{ .compression = .lz4, .encryption = .aes256gcm, .block_size = 4 << 20 }, .{ .kdf_salt = @splat(0x55), .key_id = .{ 1, 2, 3, 4, 5, 6, 7, 8 } }, &vols, &.{ "/build/cache/a", "/build/cache/b" });
}

test "the head is 128 B and the record fixed part 64 B at the spec's offsets; birth: generation 1, every volume active, capacity Σ usable; encode∘decode = id; the record is small (≈ 300 B for two volumes)" {
    const gpa = testing.allocator;
    var cfg = try fixture(gpa);
    defer cfg.deinit(gpa);
    try testing.expectEqual(@as(usize, 128), HEAD_LEN);
    try testing.expectEqual(@as(usize, 64), DATA_VOLUME_FIXED_LEN);
    try testing.expectEqual(@as(u64, 1), cfg.config_generation);
    try testing.expectEqual(@as(u32, 4_198_400), cfg.chunk_size);
    try testing.expectEqual(@as(u64, (254 + 510) * (4 << 20)), cfg.capacity);
    try testing.expectEqual(transform.KEY_WRAP_SCHEME_V2, cfg.key_scheme);
    try testing.expect(cfg.lifecycleDiverged()); // two members from birth
    const bytes = try encode(gpa, &cfg, test_cap);
    defer gpa.free(bytes);
    try testing.expectEqual(cfg.encodedLen(), bytes.len);
    try testing.expect(bytes.len < 400);
    try testing.expectEqualStrings("SQZSETCF", bytes[0..8]);
    try testing.expectEqual(@as(u16, 1), std.mem.readInt(u16, bytes[8..10], .little));
    try testing.expectEqual(@as(u16, 2), std.mem.readInt(u16, bytes[104..106], .little));
    try testing.expectEqual(@as(u16, 2), std.mem.readInt(u16, bytes[106..108], .little));
    var diag: Refusal = undefined;
    var back = try decode(gpa, bytes, &diag);
    defer back.deinit(gpa);
    try testing.expectEqual(cfg.config_generation, back.config_generation);
    try testing.expectEqual(cfg.capacity, back.capacity);
    try testing.expectEqual(cfg.set_uuid, back.set_uuid);
    try testing.expectEqual(cfg.kdf_salt, back.kdf_salt);
    try testing.expectEqual(cfg.key_id, back.key_id);
    try testing.expectEqual(cfg.data.len, back.data.len);
    for (cfg.data, back.data) |a, b| {
        try testing.expectEqual(a.vol_tag, b.vol_tag);
        try testing.expectEqual(a.state, b.state);
        try testing.expectEqual(a.device_uuid, b.device_uuid);
        try testing.expectEqual(a.capacity_blocks, b.capacity_blocks);
        try testing.expectEqualStrings(a.uri, b.uri);
    }
    try testing.expectEqual(@as(usize, 2), back.cache_paths.len);
    try testing.expectEqualStrings("/build/cache/b", back.cache_paths[1]);
    // Re-encoding the decoded record is byte-identical (canonical).
    const again = try encode(gpa, &back, test_cap);
    defer gpa.free(again);
    try testing.expectEqualSlices(u8, bytes, again);
    // A one-member plaintext set has not diverged and carries no key.
    const one = [_]DataVolume{cfg.data[0]};
    var solo = try birth(gpa, test_set_uuid, .{ .block_size = 1 << 20 }, null, &one, &.{});
    defer solo.deinit(gpa);
    try testing.expect(!solo.lifecycleDiverged());
    try testing.expectEqual(@as(u8, 0), solo.key_scheme);
    try testing.expectEqual(@as(u32, 1 << 20), solo.chunk_size);
    var out: std.Io.Writer.Allocating = .init(gpa);
    defer out.deinit();
    try cfg.writeSummary(&out.writer);
    try testing.expect(std.mem.indexOf(u8, out.written(), "vol-1111222233334444 active /dev/nvme0n1 254 blocks") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "aes-256-gcm") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "/etc/squeezefs/keys/0102030405060708.key") != null);
    try testing.expect(std.mem.indexOf(u8, out.written(), "disk cache paths 2: /build/cache/a /build/cache/b") != null);
}

test "writeJson names exactly the registered volumes with their tags, states, device uuids and uris, the cache paths, and the key id only on an encrypted set" {
    const vols = [_]DataVolume{
        .{ .vol_tag = 0xfeed, .state = .active, .device_uuid = @splat(1), .added_ts = 0, .capacity_blocks = 10, .volume_len = 1 << 30, .uri = "/dev/d0" },
        .{ .vol_tag = 0xbeef, .state = .active, .device_uuid = @splat(2), .added_ts = 0, .capacity_blocks = 20, .volume_len = 2 << 30, .uri = "/dev/d1" },
    };
    var cfg = try birth(std.testing.allocator, @splat(7), .{ .compression = .lz4, .block_size = 1 << 20 }, null, &vols, &.{ "/build/c1", "/build/c2" });
    defer cfg.deinit(std.testing.allocator);
    var out: std.Io.Writer.Allocating = .init(std.testing.allocator);
    defer out.deinit();
    try cfg.writeJson(&out.writer);
    const text = out.written();
    try std.testing.expect(std.mem.startsWith(u8, text, "{\"set_uuid\":\"07070707-0707-0707-0707-070707070707\",\"config_generation\":1,\"block_size\":1048576,"));
    try std.testing.expect(std.mem.indexOf(u8, text, "\"compression\":\"lz4\",\"encryption\":\"none\",\"inline_max\":0,\"lifecycle_diverged\":true,\"data\":[") != null);
    try std.testing.expect(std.mem.indexOf(u8, text, "{\"vol_tag\":\"vol-000000000000feed\",\"state\":\"active\",\"device_uuid\":\"01010101-0101-0101-0101-010101010101\",\"uri\":\"/dev/d0\",\"capacity_blocks\":10,\"volume_len\":1073741824}") != null);
    // `birth` stamps every member active; a later transition shows by name.
    try std.testing.expect(std.mem.indexOf(u8, text, "\"vol_tag\":\"vol-000000000000beef\",\"state\":\"active\"") != null);
    try std.testing.expect(std.mem.endsWith(u8, text, "\"cache_paths\":[\"/build/c1\",\"/build/c2\"]}"));
    try std.testing.expect(std.mem.indexOf(u8, text, "key_id") == null);
    try std.testing.expectEqual(@as(usize, 2), std.mem.count(u8, text, "\"vol_tag\""));
    cfg.data[1].state = .draining;
    out.clearRetainingCapacity();
    try cfg.writeJson(&out.writer);
    try std.testing.expect(std.mem.indexOf(u8, out.written(), "\"vol_tag\":\"vol-000000000000beef\",\"state\":\"draining\"") != null);
}

test "decode refuses by class in order and names each; the record cap refuses at encode; a lying count never allocates past the bytes present" {
    const gpa = testing.allocator;
    var cfg = try fixture(gpa);
    defer cfg.deinit(gpa);
    const bytes = try encode(gpa, &cfg, test_cap);
    defer gpa.free(bytes);
    var diag: Refusal = undefined;
    try testing.expectError(error.BadLength, decode(gpa, bytes[0..100], &diag));
    try testing.expect(diag == .bad_length);
    const mut = try gpa.dupe(u8, bytes);
    defer gpa.free(mut);
    mut[0] = 'X';
    try testing.expectError(error.BadMagic, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    std.mem.writeInt(u16, mut[8..10], 2, .little);
    try testing.expectError(error.WrongVersion, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    mut[50] ^= 1;
    try testing.expectError(error.ChecksumMismatch, decode(gpa, mut, &diag));
    // Under a valid checksum: reserved, modes, key scheme, geometry, state, counts, records.
    const Restamp = struct {
        fn go(b: []u8) void {
            checksum.stamp(b, b.len - CHECKSUM_LEN);
        }
    };
    @memcpy(mut, bytes);
    mut[10] = 1;
    Restamp.go(mut);
    try testing.expectError(error.ReservedNotZero, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    mut[36] = 9;
    Restamp.go(mut);
    try testing.expectError(error.BadMode, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    mut[38] = 1;
    Restamp.go(mut);
    try testing.expectError(error.BadKeyScheme, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    mut[38] = 0; // plaintext scheme on an encrypted record
    Restamp.go(mut);
    try testing.expectError(error.BadKeyScheme, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    std.mem.writeInt(u32, mut[24..28], 4 << 20, .little); // chunk == block on a transformed set
    Restamp.go(mut);
    try testing.expectError(error.BadGeometry, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    mut[HEAD_LEN + 8] = 7;
    Restamp.go(mut);
    try testing.expectError(error.BadState, decode(gpa, mut, &diag));
    // A lying n_data: 4096 records claimed in 300 bytes ⇒ Truncated before any allocation.
    @memcpy(mut, bytes);
    std.mem.writeInt(u16, mut[104..106], 4096, .little);
    Restamp.go(mut);
    var failing = testing.FailingAllocator.init(gpa, .{ .fail_index = 0 });
    try testing.expectError(error.Truncated, decode(failing.allocator(), mut, &diag));
    try testing.expectEqual(@as(usize, 0), failing.allocations);
    // A lying uri_len.
    @memcpy(mut, bytes);
    std.mem.writeInt(u16, mut[HEAD_LEN + 56 ..][0..2], 5000, .little);
    Restamp.go(mut);
    try testing.expectError(error.Truncated, decode(gpa, mut, &diag));
    // A relative uri; a retired record with a uri; a duplicate tag.
    @memcpy(mut, bytes);
    mut[HEAD_LEN + DATA_VOLUME_FIXED_LEN] = 'd';
    Restamp.go(mut);
    try testing.expectError(error.BadRecord, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    mut[HEAD_LEN + 8] = @backingInt(State.retired);
    Restamp.go(mut);
    try testing.expectError(error.BadRecord, decode(gpa, mut, &diag));
    @memcpy(mut, bytes);
    const second = HEAD_LEN + DATA_VOLUME_FIXED_LEN + "/dev/nvme0n1".len;
    @memcpy(mut[second..][0..8], bytes[HEAD_LEN..][0..8]);
    Restamp.go(mut);
    try testing.expectError(error.BadRecord, decode(gpa, mut, &diag));
    try testing.expect(diag == .bad_record and diag.bad_record == 1);
    // Trailing bytes under a valid checksum.
    const longer = try gpa.alloc(u8, bytes.len + 1);
    defer gpa.free(longer);
    @memcpy(longer[0..bytes.len], bytes);
    longer[bytes.len - CHECKSUM_LEN] = 0;
    Restamp.go(longer);
    try testing.expectError(error.TrailingBytes, decode(gpa, longer, &diag));
    // The cap: refused at the verb (encode), named.
    try testing.expectError(error.RecordTooLarge, encode(gpa, &cfg, 100));
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try (Refusal{ .bad_key_scheme = 7 }).write(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "scheme 7") != null and std.mem.indexOf(u8, w.buffered(), "hkdf-sha256/aead-kw/v2") != null);
}

fn checkDecodeTotal(gpa: Allocator, bytes: []const u8) !void {
    var diag: Refusal = undefined;
    if (decode(gpa, bytes, &diag)) |*c| {
        var cc = c.*;
        defer cc.deinit(gpa);
        // What decodes re-encodes to the same bytes (canonical).
        const again = try encode(gpa, &cc, std.math.maxInt(usize));
        defer gpa.free(again);
        try testing.expectEqualSlices(u8, bytes, again);
    } else |_| {}
}

fn decodeFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [1024]u8 = undefined;
    const len = smith.slice(&buf);
    try checkDecodeTotal(testing.allocator, buf[0..len]);
}

test "fuzz: the set_config decoder is total on arbitrary bytes" {
    const gpa = testing.allocator;
    var cfg = try fixture(gpa);
    defer cfg.deinit(gpa);
    const bytes = try encode(gpa, &cfg, test_cap);
    defer gpa.free(bytes);
    try std.testing.fuzz({}, decodeFuzz, .{});
    // Mutations of the valid encoding under a re-stamped checksum reach every structural gate.
    var prng = std.Random.DefaultPrng.init(0x5e7c);
    const r = prng.random();
    const mut = try gpa.dupe(u8, bytes);
    defer gpa.free(mut);
    for (0..5000) |_| {
        @memcpy(mut, bytes);
        for (0..r.intRangeAtMost(usize, 1, 3)) |_| mut[r.intRangeLessThan(usize, 0, mut.len - CHECKSUM_LEN)] ^= @as(u8, 1) << r.int(u3);
        checksum.stamp(mut, mut.len - CHECKSUM_LEN);
        try checkDecodeTotal(gpa, mut);
        try checkDecodeTotal(gpa, mut[0..r.intRangeAtMost(usize, 0, mut.len)]);
    }
}

test "verbs: set-cache-paths (denylist, relative, duplicate refused), add-data (duplicates refused; the second member needs the bit), the lifecycle state machine (retired terminal, uri cleared), generation strictly monotone, capacity recomputed, the nonce floor never lowers" {
    const gpa = testing.allocator;
    const one = [_]DataVolume{.{ .vol_tag = 0xaaaa, .state = .active, .device_uuid = @splat(0xa), .added_ts = 1, .capacity_blocks = 100, .volume_len = 1 << 30, .uri = "/dev/a" }};
    var c0 = try birth(gpa, test_set_uuid, .{ .compression = .zstd, .block_size = 1 << 20 }, null, &one, &.{});
    defer c0.deinit(gpa);
    try testing.expect(!c0.lifecycleDiverged());
    // Cache paths.
    try testing.expectError(error.BadCachePath, setCachePaths(gpa, &c0, &.{"/home"}));
    try testing.expectError(error.BadCachePath, setCachePaths(gpa, &c0, &.{"/var/"}));
    try testing.expectError(error.BadCachePath, setCachePaths(gpa, &c0, &.{"relative"}));
    try testing.expectError(error.BadCachePath, setCachePaths(gpa, &c0, &.{ "/build/c", "/build/c" }));
    var m1 = try setCachePaths(gpa, &c0, &.{ "/build/c1", "/build/c2" });
    defer m1.next.deinit(gpa);
    try testing.expect(!m1.lifecycle_bit_needed);
    try testing.expectEqual(@as(u64, 2), m1.next.config_generation);
    try testing.expectEqual(@as(usize, 2), m1.next.cache_paths.len);
    try testing.expectEqualStrings("/build/c2", m1.next.cache_paths[1]);
    try testing.expect(cachePathAdmissible("/home/x") and !cachePathAdmissible("/") and !cachePathAdmissible("/root/"));
    // add-data.
    const new: DataVolume = .{ .vol_tag = 0xbbbb, .state = .draining, .device_uuid = @splat(0xb), .added_ts = 2, .capacity_blocks = 200, .volume_len = 2 << 30, .uri = "/dev/b" };
    try testing.expectError(error.DuplicateVolume, addDataVolume(gpa, &m1.next, .{ .vol_tag = 0xaaaa, .state = .active, .device_uuid = @splat(0xc), .added_ts = 2, .capacity_blocks = 1, .volume_len = 1, .uri = "/dev/c" }));
    try testing.expectError(error.DuplicateVolume, addDataVolume(gpa, &m1.next, .{ .vol_tag = 0xcccc, .state = .active, .device_uuid = @splat(0xa), .added_ts = 2, .capacity_blocks = 1, .volume_len = 1, .uri = "/dev/c" }));
    try testing.expectError(error.DuplicateVolume, addDataVolume(gpa, &m1.next, .{ .vol_tag = 0xcccc, .state = .active, .device_uuid = @splat(0xc), .added_ts = 2, .capacity_blocks = 1, .volume_len = 1, .uri = "/dev/a" }));
    var m2 = try addDataVolume(gpa, &m1.next, new);
    defer m2.next.deinit(gpa);
    try testing.expect(m2.lifecycle_bit_needed); // the second member
    try testing.expectEqual(@as(u64, 3), m2.next.config_generation);
    try testing.expectEqual(State.active, m2.next.data[1].state); // added active regardless
    try testing.expectEqual(@as(u64, 300 << 20), m2.next.capacity);
    // The state machine.
    try testing.expectError(error.UnknownVolume, setState(gpa, &m2.next, 0xdead, .draining));
    try testing.expectError(error.IllegalTransition, setState(gpa, &m2.next, 0xbbbb, .retired));
    var m3 = try setState(gpa, &m2.next, 0xbbbb, .draining);
    defer m3.next.deinit(gpa);
    try testing.expect(!m3.lifecycle_bit_needed); // already diverged
    try testing.expectEqual(@as(u64, 4), m3.next.config_generation);
    var m3b = try setState(gpa, &m3.next, 0xbbbb, .active); // undrain
    defer m3b.next.deinit(gpa);
    try testing.expectEqual(State.active, m3b.next.data[1].state);
    try testing.expectError(error.IllegalTransition, setState(gpa, &m3.next, 0xbbbb, .disabled));
    var m4 = try setState(gpa, &m3.next, 0xbbbb, .retired);
    defer m4.next.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), m4.next.data[1].uri.len);
    try testing.expectEqual(@as(usize, 2), m4.next.data.len); // kept forever
    try testing.expectEqual(@as(u64, 100 << 20), m4.next.capacity); // retired excluded
    try testing.expectError(error.IllegalTransition, setState(gpa, &m4.next, 0xbbbb, .active));
    try testing.expectError(error.DuplicateVolume, addDataVolume(gpa, &m4.next, new)); // the tag stays taken
    // A one-member set that disables its volume diverges (the bit).
    var m5 = try setState(gpa, &c0, 0xaaaa, .disabled);
    defer m5.next.deinit(gpa);
    try testing.expect(m5.lifecycle_bit_needed);
    var m6 = try setState(gpa, &m5.next, 0xaaaa, .active);
    defer m6.next.deinit(gpa);
    try testing.expect(!m6.lifecycle_bit_needed and !m6.next.lifecycleDiverged());
    // The retired record encodes/decodes (uri_len 0).
    const bytes = try encode(gpa, &m4.next, test_cap);
    defer gpa.free(bytes);
    var back = try decode(gpa, bytes, null);
    defer back.deinit(gpa);
    try testing.expectEqual(State.retired, back.data[1].state);
    // The nonce floor.
    raiseNonceFloor(&back, 500);
    try testing.expectEqual(@as(u64, 500), back.nonce_floor);
    raiseNonceFloor(&back, 10);
    try testing.expectEqual(@as(u64, 500), back.nonce_floor);
    // Every transition of the table.
    try testing.expect(transitionAllowed(.active, .draining) and transitionAllowed(.active, .disabled) and transitionAllowed(.draining, .active) and transitionAllowed(.draining, .retired) and transitionAllowed(.disabled, .active));
    try testing.expect(!transitionAllowed(.active, .retired) and !transitionAllowed(.disabled, .draining) and !transitionAllowed(.retired, .active) and !transitionAllowed(.active, .active));
}

test "discovery: a matching set passes in any order; a renumbered device, a foreign set, a blank device, a refused superblock, a cloned device, a geometry disagreement, two uris reading one tag, and a record from another set each refuse with the exact text" {
    const gpa = testing.allocator;
    var cfg = try fixture(gpa);
    defer cfg.deinit(gpa);
    const id = struct {
        fn of(c: *const Config, d: DataVolume) DeviceIdentity {
            return .{ .vol_tag = d.vol_tag, .device_uuid = d.device_uuid, .set_uuid = c.set_uuid, .block_size = c.block_size, .chunk_size = c.chunk_size, .compression = c.compression, .encryption = c.encryption };
        }
    };
    var obs = [_]Observation{ .{ .identity = id.of(&cfg, cfg.data[0]) }, .{ .identity = id.of(&cfg, cfg.data[1]) } };
    var diag: DiscoveryRefusal = undefined;
    try discover(&cfg, test_set_uuid, &obs, &diag);
    var buf: [512]u8 = undefined;
    const Text = struct {
        fn of(b: []u8, d: DiscoveryRefusal) []const u8 {
            var w: std.Io.Writer = .fixed(b);
            d.write(&w) catch unreachable;
            return w.buffered();
        }
    };
    // Renumbered: uri 0 reads volume 1's identity.
    obs[0] = .{ .identity = id.of(&cfg, cfg.data[1]) };
    try testing.expectError(error.Refused, discover(&cfg, test_set_uuid, &obs, &diag));
    try testing.expectEqualStrings("/dev/nvme0n1 carries vol-5555666677778888 but the set names vol-1111222233334444: the device was renumbered or belongs to another set", Text.of(&buf, diag));
    // Foreign set.
    obs[0] = .{ .identity = id.of(&cfg, cfg.data[0]) };
    obs[0].identity.set_uuid = @splat(9);
    try testing.expectError(error.Refused, discover(&cfg, test_set_uuid, &obs, &diag));
    try testing.expect(diag == .foreign_set);
    try testing.expect(std.mem.startsWith(u8, Text.of(&buf, diag), "/dev/nvme0n1 belongs to set 09090909-0909-0909-0909-090909090909, not this set"));
    // Blank; refused.
    obs[0] = .blank;
    try testing.expectError(error.Refused, discover(&cfg, test_set_uuid, &obs, &diag));
    try testing.expectEqualStrings("/dev/nvme0n1 is not a formatted data volume (zeroed superblock) — the set names it; run `squeezefs volume add-data` to format it, or fix the URI", Text.of(&buf, diag));
    obs[0] = .refused;
    try testing.expectError(error.Refused, discover(&cfg, test_set_uuid, &obs, &diag));
    try testing.expect(diag == .device_refused);
    // Cloned device: right tag, other uuid.
    obs[0] = .{ .identity = id.of(&cfg, cfg.data[0]) };
    obs[0].identity.device_uuid = @splat(7);
    try testing.expectError(error.Refused, discover(&cfg, test_set_uuid, &obs, &diag));
    try testing.expect(diag == .device_identity);
    // Geometry.
    obs[0] = .{ .identity = id.of(&cfg, cfg.data[0]) };
    obs[0].identity.compression = .zstd;
    try testing.expectError(error.Refused, discover(&cfg, test_set_uuid, &obs, &diag));
    try testing.expectEqualStrings("/dev/nvme0n1: the device's block/chunk/transform disagree with the set_config record — the device was formatted for another set", Text.of(&buf, diag));
    // Two uris, one device (both read as volume 0's identity ⇒ uri 1 is renumbered first…
    // so make records agree with their devices and alias the TAG only).
    obs[0] = .{ .identity = id.of(&cfg, cfg.data[0]) };
    obs[1] = .{ .identity = id.of(&cfg, cfg.data[1]) };
    var aliased = try cfg.clone(gpa);
    defer aliased.deinit(gpa);
    aliased.data[1].vol_tag = aliased.data[0].vol_tag;
    aliased.data[1].device_uuid = aliased.data[0].device_uuid;
    obs[1] = .{ .identity = id.of(&aliased, aliased.data[1]) };
    try testing.expectError(error.Refused, discover(&aliased, test_set_uuid, &obs, &diag));
    try testing.expectEqualStrings("/dev/nvme0n1 and /build/tmp/d1.img both carry vol-1111222233334444: two URIs name one device (a dm/multipath alias, or a cloned device)", Text.of(&buf, diag));
    // The record from another set.
    obs[1] = .{ .identity = id.of(&cfg, cfg.data[1]) };
    try testing.expectError(error.Refused, discover(&cfg, @splat(3), &obs, &diag));
    try testing.expect(diag == .record_set_mismatch);
    // A retired record is not read: its observation is ignored.
    var draining = try setState(gpa, &cfg, cfg.data[1].vol_tag, .draining);
    defer draining.next.deinit(gpa);
    var retired = try setState(gpa, &draining.next, cfg.data[1].vol_tag, .retired);
    defer retired.next.deinit(gpa);
    obs[1] = .blank;
    try discover(&retired.next, test_set_uuid, &obs, &diag);
}

test "the capacity closed form: refuse iff avail < needed + transient + headroom; the terms tie to their derivations; a 2000-census property; the refusal prints every number" {
    try testing.expectEqual(@as(u64, 64 * 8 * (4 << 20)), transientBytes(8, 4 << 20));
    try testing.expectEqual(@as(u64, 1 << 30), headroomBytes(0, 100));
    try testing.expectEqual(@as(u64, 1 << 30), headroomBytes(1 << 20, 100)); // 100 MiB < the floor
    try testing.expectEqual(@as(u64, 2 << 30), headroomBytes(2 << 20, 1024));
    try testing.expectEqual(@as(u64, 1), initialDrainEtaS(0));
    try testing.expectEqual(@as(u64, 3), initialDrainEtaS(1 << 30)); // 1 GiB / 512 MiB/s + 1
    const exact: CapacityPreflight = .{ .avail = 1000, .needed = 500, .transient = 300, .headroom = 200 };
    try testing.expect(!exact.refuses());
    const short: CapacityPreflight = .{ .avail = 999, .needed = 500, .transient = 300, .headroom = 200 };
    try testing.expect(short.refuses());
    const sat: CapacityPreflight = .{ .avail = std.math.maxInt(u64), .needed = std.math.maxInt(u64), .transient = 1, .headroom = 1 };
    try testing.expect(!sat.refuses()); // saturating: never wraps into a false pass… or a false refusal
    var prng = std.Random.DefaultPrng.init(0xcafe);
    const r = prng.random();
    for (0..2000) |_| {
        const p: CapacityPreflight = .{ .avail = r.int(u40), .needed = r.int(u40), .transient = transientBytes(r.intRangeAtMost(u64, 1, 64), 4 << 20), .headroom = headroomBytes(r.int(u30), r.intRangeAtMost(u64, 1, 1000)) };
        try testing.expectEqual(p.avail < p.needed + p.transient + p.headroom, p.refuses());
    }
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try short.write(&w);
    try testing.expectEqualStrings("capacity preflight REFUSES: avail 999 B < needed 500 B + transient 300 B + headroom 200 B = 1000 B", w.buffered());
}
