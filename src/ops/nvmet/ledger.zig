//! The share ledger (charter §8 "a write-ahead-intent share ledger";
//! `docs/porting/ops_nvmet_keys.md` §2): ONE binary file
//! `<state dir>/shares.sqzl` — v4 `extern struct`s, canonical, total,
//! checksummed (quirk Q2: no JSON, no `stack`, no SPDK fields, no
//! `loop_device`, no `nsid`) — under the six laws:
//!   1. LOAD NEVER WRITES (no lock file, no dir, no tmp cleanup; a
//!      read-only filesystem loads; a leftover `.tmp` is logged and
//!      ignored) — named for the truncation bug of the pre-rebuild
//!      registry that `fs::write("[]")` on every resolution;
//!   2. writes are ATOMIC and SERIALIZED: `flock(LOCK_EX)` on
//!      `shares.sqzl.lock`, read → mutate → encode → `.tmp` (0600) →
//!      `fdatasync` → `rename` → `fsync` the directory;
//!   3. FORWARD-ONLY: `version > 1` refuses "created by a newer squeezefs"
//!      BEFORE any field check (a future format's unknown fields produce
//!      the upgrade message, not a field error); an unknown flag bit
//!      refuses;
//!   4. RECONCILIATION, never blind trust: every verb reads live configfs
//!      first (`verbs.zig`);
//!   5. BOOKKEEPING LIVES HERE, never in configfs (port ids are ledger
//!      fields; nothing but real kernel attributes is ever written there);
//!   6. WRITE-AHEAD INTENT: `share` appends `pending` BEFORE the first
//!      configfs mutation and flips `active` after the last; `unshare`
//!      flips `removing` BEFORE the first teardown write and deletes
//!      after — "the product can never strand its own share as foreign".
//!
//! WHERE: `/var/lib/squeezefs/nvmeof/` (`SQUEEZEFS_NVMEOF_STATE_DIR`
//! relocates it — a relocation seam, never a behavior fork). The ledger is
//! HOST-scoped, not set-scoped: a target host serves backings of any
//! number of sets, and `restore` runs at boot BEFORE any set is mountable
//! (the oneshot unit), so the ledger cannot live inside a set's config.
//!
//! WHY the io_uring file layer for a config file: charter §1.2 names ONE
//! ad-hoc file I/O path (`io/uring_fs.zig`) and forbids classical
//! `read/write` on product I/O paths; the O_DIRECT metadata posture is
//! NOT asked of this file (it is never registered as a metadata device —
//! the pool opens it buffered), and the control plane pays nothing for
//! the ring. `rename`/`fsync`/`flock` are not data I/O and stay raw
//! syscalls. The pool is ONE worker: the tmp path's cached fd must be the
//! fd that was just written, and a second worker's cache could hold the
//! previous round's inode under the same name.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const checksum = @import("../../core/checksum.zig");
const clock = @import("../../core/clock.zig");
const errors = @import("../../core/errors.zig");
const knobs = @import("../../core/knobs.zig");
const log = @import("../../core/log.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const configfs = @import("configfs.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/ledger is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Listener = configfs.Listener;
pub const Trtype = configfs.Trtype;
pub const Adrfam = configfs.Adrfam;

pub const DEFAULT_STATE_DIR = "/var/lib/squeezefs/nvmeof";
pub const FILE_NAME = "shares.sqzl";
pub const LOCK_NAME = "shares.sqzl.lock";
pub const TMP_NAME = "shares.sqzl.tmp";

pub const MAGIC = "SQZNVMET";
pub const VERSION: u16 = 1;

/// The state dir in force: the knob, else the default.
pub fn stateDir(env: *const std.process.Environ.Map) []const u8 {
    return knobs.present(env.get("SQUEEZEFS_NVMEOF_STATE_DIR")) orelse DEFAULT_STATE_DIR;
}

// ---------------------------------------------------------------------------
// The wire.
// ---------------------------------------------------------------------------

pub const LedgerHeadWire = extern struct {
    magic: [8]u8,
    version_le: [2]u8,
    /// Checked zero.
    flags_le: [2]u8,
    record_count_le: [4]u8,
    body_len_le: [8]u8,
    /// +1 per write; `nvmeof list` prints it.
    generation_le: [8]u8,
    written_s_le: [8]u8,
    reserved: [16]u8,
    /// xxh3 over `head[0..56] ‖ body`.
    xxh3_le: [8]u8,
};

pub const HEAD_LEN: usize = @sizeOf(LedgerHeadWire);
const HEAD_HASHED: usize = 56;

pub const State = enum(u8) {
    pending = 1,
    active = 2,
    removing = 3,

    pub fn fromWire(b: u8) ?State {
        return switch (b) {
            1 => .pending,
            2 => .active,
            3 => .removing,
            else => null,
        };
    }
};

pub const AdoptClass = enum(u8) {
    none = 0,
    /// The product's own prefix on an unledgered object.
    ledger_loss = 1,
    foreign = 2,

    pub fn fromWire(b: u8) ?AdoptClass {
        return switch (b) {
            0 => .none,
            1 => .ledger_loss,
            2 => .foreign,
            else => null,
        };
    }
};

pub const FLAG_ADOPTED: u16 = 1 << 0;
pub const FLAG_ALLOW_ANY_HOST: u16 = 1 << 1;
const FLAGS_KNOWN: u16 = FLAG_ADOPTED | FLAG_ALLOW_ANY_HOST;

pub const ShareRecordWire = extern struct {
    /// Head + tail exactly.
    rec_len_le: [4]u8,
    state: u8,
    trtype: u8,
    flags_le: [2]u8,
    created_s_le: [8]u8,
    /// 0 unless ADOPTED.
    adopted_s_le: [8]u8,
    adopt_class: u8,
    n_listeners: u8,
    n_hosts: u8,
    reserved: [5]u8,
    ns_uuid: [16]u8,
};

pub const RECORD_HEAD_LEN: usize = @sizeOf(ShareRecordWire);

pub const ListenerWire = extern struct {
    adrfam: u8,
    reserved0: u8,
    trsvcid_le: [2]u8,
    port_id_le: [2]u8,
    reserved1: [2]u8,
    traddr: [16]u8,
};

pub const LISTENER_LEN: usize = @sizeOf(ListenerWire);

comptime {
    std.debug.assert(HEAD_LEN == 64);
    std.debug.assert(@offsetOf(LedgerHeadWire, "xxh3_le") == HEAD_HASHED);
    std.debug.assert(RECORD_HEAD_LEN == 48);
    std.debug.assert(@offsetOf(ShareRecordWire, "ns_uuid") == 32);
    std.debug.assert(LISTENER_LEN == 24);
}

/// Caps the value laws enforce (S:148-181 + the u8 counts of the wire).
pub const MAX_LISTENERS: usize = 255;
pub const MAX_HOSTS: usize = 255;
pub const MAX_PATH: usize = linux.PATH_MAX - 1;
/// A record count the file's own bytes bound: 48 B per record minimum.
pub const MAX_RECORDS: usize = 1 << 16;

// ---------------------------------------------------------------------------
// The RAM shape.
// ---------------------------------------------------------------------------

pub const Record = struct {
    state: State,
    trtype: Trtype,
    adopted: bool,
    allow_any_host: bool,
    created_s: i64,
    adopted_s: i64,
    adopt_class: AdoptClass,
    ns_uuid: [16]u8,
    subnqn: []u8,
    backing_path: []u8,
    backing_canonical: []u8,
    listeners: []Listener,
    hosts: [][]u8,

    pub fn deinit(self: *Record, gpa: Allocator) void {
        gpa.free(self.subnqn);
        gpa.free(self.backing_path);
        gpa.free(self.backing_canonical);
        gpa.free(self.listeners);
        for (self.hosts) |h| gpa.free(h);
        gpa.free(self.hosts);
        self.* = undefined;
    }

    pub fn clone(self: *const Record, gpa: Allocator) Allocator.Error!Record {
        var out = self.*;
        out.subnqn = try gpa.dupe(u8, self.subnqn);
        errdefer gpa.free(out.subnqn);
        out.backing_path = try gpa.dupe(u8, self.backing_path);
        errdefer gpa.free(out.backing_path);
        out.backing_canonical = try gpa.dupe(u8, self.backing_canonical);
        errdefer gpa.free(out.backing_canonical);
        out.listeners = try gpa.dupe(Listener, self.listeners);
        errdefer gpa.free(out.listeners);
        out.hosts = try gpa.alloc([]u8, self.hosts.len);
        var n: usize = 0;
        errdefer {
            for (out.hosts[0..n]) |h| gpa.free(h);
            gpa.free(out.hosts);
        }
        for (self.hosts, out.hosts) |s, *d| {
            d.* = try gpa.dupe(u8, s);
            n += 1;
        }
        return out;
    }

    pub fn portIds(self: *const Record, buf: []u16) []const u16 {
        var n: usize = 0;
        for (self.listeners) |l| {
            var dup = false;
            for (buf[0..n]) |p| dup = dup or p == l.port_id;
            if (dup) continue;
            buf[n] = l.port_id;
            n += 1;
        }
        return buf[0..n];
    }

    /// The host-NQN slices as `[]const []const u8`.
    pub fn hostsConst(self: *const Record) []const []const u8 {
        return @ptrCast(self.hosts);
    }

    pub fn encodedLen(self: *const Record) usize {
        var n: usize = RECORD_HEAD_LEN + 2 + self.subnqn.len + 2 + self.backing_path.len + 2 + self.backing_canonical.len + self.listeners.len * LISTENER_LEN;
        for (self.hosts) |h| n += 2 + h.len;
        return n;
    }

    pub fn writeJson(self: *const Record, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"subnqn\":\"{s}\",\"state\":\"{s}\",\"trtype\":\"{s}\",\"backing_path\":\"{s}\",\"backing_canonical\":\"{s}\",\"ns_uuid\":\"", .{ self.subnqn, @tagName(self.state), self.trtype.name(), self.backing_path, self.backing_canonical });
        try configfs.writeUuid(w, self.ns_uuid);
        try w.print("\",\"created_s\":{d},\"adopted\":{},\"adopt_class\":\"{s}\",\"adopted_s\":{d},\"allow_any_host\":{},\"listeners\":[", .{ self.created_s, self.adopted, @tagName(self.adopt_class), self.adopted_s, self.allow_any_host });
        for (self.listeners, 0..) |l, i| {
            if (i != 0) try w.writeByte(',');
            var ab: [64]u8 = undefined;
            try w.print("{{\"trtype\":\"{s}\",\"adrfam\":\"{s}\",\"traddr\":\"{s}\",\"trsvcid\":{d},\"port_id\":{d}}}", .{ l.trtype.name(), l.adrfam.name(), l.traddrText(&ab), l.trsvcid, l.port_id });
        }
        try w.writeAll("],\"allow_hosts\":[");
        for (self.hosts, 0..) |h, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("\"{s}\"", .{h});
        }
        try w.writeAll("]}");
    }
};

pub const Ledger = struct {
    generation: u64 = 0,
    written_s: i64 = 0,
    records: std.ArrayList(Record) = .empty,

    pub fn deinit(self: *Ledger, gpa: Allocator) void {
        for (self.records.items) |*r| r.deinit(gpa);
        self.records.deinit(gpa);
        self.* = undefined;
    }

    pub fn find(self: *const Ledger, subnqn: []const u8) ?*Record {
        for (self.records.items) |*r| if (std.mem.eql(u8, r.subnqn, subnqn)) return r;
        return null;
    }

    pub fn findByBacking(self: *const Ledger, canonical: []const u8) ?*Record {
        for (self.records.items) |*r| if (std.mem.eql(u8, r.backing_canonical, canonical)) return r;
        return null;
    }

    pub fn isLedgered(self: *const Ledger, subnqn: []const u8) bool {
        return self.find(subnqn) != null;
    }

    /// Is `host` named by a record OTHER than `except`?
    pub fn hostNamedElsewhere(self: *const Ledger, host: []const u8, except: []const u8) bool {
        for (self.records.items) |r| {
            if (std.mem.eql(u8, r.subnqn, except)) continue;
            for (r.hosts) |h| if (std.mem.eql(u8, h, host)) return true;
        }
        return false;
    }

    pub fn encodedLen(self: *const Ledger) usize {
        var n: usize = HEAD_LEN;
        for (self.records.items) |*r| n += r.encodedLen();
        return n;
    }
};

// ---------------------------------------------------------------------------
// Value laws (S:148-181), at decode and at `beginShare`.
// ---------------------------------------------------------------------------

pub const LawError = error{
    EmptySubnqn,
    SubnqnTooLong,
    EmptyBacking,
    NoListener,
    TooManyListeners,
    TooManyHosts,
    EmptyHost,
    HostsVsAllowAny,
    ListenerVsTrtype,
    AdoptionShape,
};

pub fn checkLaws(r: *const Record) LawError!void {
    if (r.subnqn.len == 0) return error.EmptySubnqn;
    if (r.subnqn.len > configfs.NVMF_NQN_SIZE) return error.SubnqnTooLong;
    if (r.backing_path.len == 0 or r.backing_canonical.len == 0) return error.EmptyBacking;
    if (r.listeners.len == 0) return error.NoListener;
    if (r.listeners.len > MAX_LISTENERS) return error.TooManyListeners;
    if (r.hosts.len > MAX_HOSTS) return error.TooManyHosts;
    for (r.hosts) |h| if (h.len == 0) return error.EmptyHost;
    if ((r.hosts.len == 0) != r.allow_any_host) return error.HostsVsAllowAny;
    for (r.listeners) |l| {
        if (l.trtype != r.trtype) return error.ListenerVsTrtype;
        if ((l.adrfam == .none) != (l.trtype == .loop)) return error.ListenerVsTrtype;
    }
    if (r.adopted != (r.adopt_class != .none)) return error.AdoptionShape;
    if (!r.adopted and r.adopted_s != 0) return error.AdoptionShape;
}

// ---------------------------------------------------------------------------
// Encode / decode.
// ---------------------------------------------------------------------------

pub const EncodeError = error{ OutOfMemory, TooManyRecords } || LawError;

pub fn encode(gpa: Allocator, l: *const Ledger) EncodeError![]u8 {
    if (l.records.items.len > MAX_RECORDS) return error.TooManyRecords;
    for (l.records.items) |*r| try checkLaws(r);
    const len = l.encodedLen();
    const out = try gpa.alloc(u8, len);
    errdefer gpa.free(out);
    var h: LedgerHeadWire = undefined;
    h.magic = MAGIC.*;
    std.mem.writeInt(u16, &h.version_le, VERSION, .little);
    std.mem.writeInt(u16, &h.flags_le, 0, .little);
    std.mem.writeInt(u32, &h.record_count_le, @intCast(l.records.items.len), .little);
    std.mem.writeInt(u64, &h.body_len_le, len - HEAD_LEN, .little);
    std.mem.writeInt(u64, &h.generation_le, l.generation, .little);
    std.mem.writeInt(i64, &h.written_s_le, l.written_s, .little);
    h.reserved = @splat(0);
    h.xxh3_le = @splat(0);
    out[0..HEAD_LEN].* = std.mem.toBytes(h);
    var off: usize = HEAD_LEN;
    for (l.records.items) |*r| {
        var rw: ShareRecordWire = undefined;
        std.mem.writeInt(u32, &rw.rec_len_le, @intCast(r.encodedLen()), .little);
        rw.state = @backingInt(r.state);
        rw.trtype = @backingInt(r.trtype);
        var flags: u16 = 0;
        if (r.adopted) flags |= FLAG_ADOPTED;
        if (r.allow_any_host) flags |= FLAG_ALLOW_ANY_HOST;
        std.mem.writeInt(u16, &rw.flags_le, flags, .little);
        std.mem.writeInt(i64, &rw.created_s_le, r.created_s, .little);
        std.mem.writeInt(i64, &rw.adopted_s_le, r.adopted_s, .little);
        rw.adopt_class = @backingInt(r.adopt_class);
        rw.n_listeners = @intCast(r.listeners.len);
        rw.n_hosts = @intCast(r.hosts.len);
        rw.reserved = @splat(0);
        rw.ns_uuid = r.ns_uuid;
        out[off..][0..RECORD_HEAD_LEN].* = std.mem.toBytes(rw);
        off += RECORD_HEAD_LEN;
        off = putFramed(out, off, r.subnqn);
        off = putFramed(out, off, r.backing_path);
        off = putFramed(out, off, r.backing_canonical);
        for (r.listeners) |li| {
            var lw: ListenerWire = undefined;
            lw.adrfam = @backingInt(li.adrfam);
            lw.reserved0 = 0;
            std.mem.writeInt(u16, &lw.trsvcid_le, li.trsvcid, .little);
            std.mem.writeInt(u16, &lw.port_id_le, li.port_id, .little);
            lw.reserved1 = @splat(0);
            lw.traddr = li.traddr;
            out[off..][0..LISTENER_LEN].* = std.mem.toBytes(lw);
            off += LISTENER_LEN;
        }
        for (r.hosts) |host| off = putFramed(out, off, host);
    }
    std.debug.assert(off == len);
    const sum = hashImage(out);
    std.mem.writeInt(u64, out[HEAD_HASHED..][0..8], sum, .little);
    return out;
}

fn putFramed(out: []u8, off: usize, bytes: []const u8) usize {
    std.mem.writeInt(u16, out[off..][0..2], @intCast(bytes.len), .little);
    @memcpy(out[off + 2 ..][0..bytes.len], bytes);
    return off + 2 + bytes.len;
}

/// xxh3 over `head[0..56] ‖ body` (the checksum field excluded by position).
fn hashImage(bytes: []const u8) u64 {
    var h = std.hash.XxHash3.init(0);
    h.update(bytes[0..HEAD_HASHED]);
    h.update(bytes[HEAD_LEN..]);
    return h.final();
}

pub const DecodeError = error{
    BadLength,
    BadMagic,
    /// `version > 1`: "created by a newer squeezefs; upgrade".
    NewerBinary,
    BadVersion,
    UnknownFlag,
    ReservedNotZero,
    ChecksumMismatch,
    /// A count or length past the bytes present, or `rec_len` ≠ consumed.
    Truncated,
    TrailingBytes,
    BadState,
    BadTrtype,
    BadAdrfam,
    BadAdoptClass,
    BadRecord,
    OutOfMemory,
};

pub fn writeDecodeRefusal(w: *Io.Writer, e: DecodeError, path: []const u8) Io.Writer.Error!void {
    switch (e) {
        error.NewerBinary => try w.print("{s} was created by a newer squeezefs (ledger format > {d}) — upgrade squeezefs", .{ path, VERSION }),
        error.OutOfMemory => try w.writeAll("out of memory"),
        else => try w.print("{s} is not a valid share ledger ({s}) — a corrupt or foreign file; move it aside and `nvmeof adopt` the live shares", .{ path, @errorName(e) }),
    }
}

/// The ledger's version read LOOSELY (law 3): the gate runs before any
/// field check.
fn versionOf(bytes: []const u8) ?u16 {
    if (bytes.len < 10 or !std.mem.eql(u8, bytes[0..8], MAGIC)) return null;
    return std.mem.readInt(u16, bytes[8..10], .little);
}

fn takeFramed(bytes: []const u8, off: *usize, end: usize) DecodeError![]const u8 {
    if (end - off.* < 2) return error.Truncated;
    const len: usize = std.mem.readInt(u16, bytes[off.*..][0..2], .little);
    off.* += 2;
    if (len > end - off.*) return error.Truncated;
    const s = bytes[off.*..][0..len];
    off.* += len;
    return s;
}

/// Total over arbitrary bytes; every count checked against the bytes
/// present BEFORE any allocation.
pub fn decode(gpa: Allocator, bytes: []const u8) DecodeError!Ledger {
    if (versionOf(bytes)) |v| {
        if (v > VERSION) return error.NewerBinary;
        if (v < VERSION) return error.BadVersion;
    }
    if (bytes.len < HEAD_LEN) return error.BadLength;
    if (!std.mem.eql(u8, bytes[0..8], MAGIC)) return error.BadMagic;
    const h = std.mem.bytesToValue(LedgerHeadWire, bytes[0..HEAD_LEN]);
    if (std.mem.readInt(u16, &h.flags_le, .little) != 0) return error.UnknownFlag;
    if (!std.mem.allEqual(u8, &h.reserved, 0)) return error.ReservedNotZero;
    const body_len = std.mem.readInt(u64, &h.body_len_le, .little);
    if (body_len != bytes.len - HEAD_LEN) return error.BadLength;
    if (std.mem.readInt(u64, &h.xxh3_le, .little) != hashImage(bytes)) return error.ChecksumMismatch;
    const count: usize = std.mem.readInt(u32, &h.record_count_le, .little);
    if (count > MAX_RECORDS or count * RECORD_HEAD_LEN > body_len) return error.Truncated;
    var out: Ledger = .{ .generation = std.mem.readInt(u64, &h.generation_le, .little), .written_s = std.mem.readInt(i64, &h.written_s_le, .little) };
    errdefer out.deinit(gpa);
    try out.records.ensureTotalCapacity(gpa, count);
    var off: usize = HEAD_LEN;
    var i: usize = 0;
    while (i < count) : (i += 1) {
        if (bytes.len - off < RECORD_HEAD_LEN) return error.Truncated;
        const rw = std.mem.bytesToValue(ShareRecordWire, bytes[off..][0..RECORD_HEAD_LEN]);
        const rec_len: usize = std.mem.readInt(u32, &rw.rec_len_le, .little);
        if (rec_len < RECORD_HEAD_LEN or rec_len > bytes.len - off) return error.Truncated;
        const end = off + rec_len;
        if (!std.mem.allEqual(u8, &rw.reserved, 0)) return error.ReservedNotZero;
        const flags = std.mem.readInt(u16, &rw.flags_le, .little);
        if (flags & ~FLAGS_KNOWN != 0) return error.UnknownFlag;
        const state = State.fromWire(rw.state) orelse return error.BadState;
        const trtype = Trtype.fromWire(rw.trtype) orelse return error.BadTrtype;
        const adopt_class = AdoptClass.fromWire(rw.adopt_class) orelse return error.BadAdoptClass;
        var p = off + RECORD_HEAD_LEN;
        const subnqn = try takeFramed(bytes, &p, end);
        const backing_path = try takeFramed(bytes, &p, end);
        const backing_canonical = try takeFramed(bytes, &p, end);
        const n_listeners: usize = rw.n_listeners;
        if (n_listeners * LISTENER_LEN > end - p) return error.Truncated;
        const listeners = try gpa.alloc(Listener, n_listeners);
        errdefer gpa.free(listeners);
        for (listeners) |*li| {
            const lw = std.mem.bytesToValue(ListenerWire, bytes[p..][0..LISTENER_LEN]);
            p += LISTENER_LEN;
            if (lw.reserved0 != 0 or !std.mem.allEqual(u8, &lw.reserved1, 0)) return error.ReservedNotZero;
            li.* = .{ .trtype = trtype, .adrfam = Adrfam.fromWire(lw.adrfam) orelse return error.BadAdrfam, .traddr = lw.traddr, .trsvcid = std.mem.readInt(u16, &lw.trsvcid_le, .little), .port_id = std.mem.readInt(u16, &lw.port_id_le, .little) };
        }
        const hosts = try gpa.alloc([]u8, rw.n_hosts);
        var nh: usize = 0;
        errdefer {
            for (hosts[0..nh]) |hh| gpa.free(hh);
            gpa.free(hosts);
        }
        for (hosts) |*hh| {
            hh.* = try gpa.dupe(u8, try takeFramed(bytes, &p, end));
            nh += 1;
        }
        if (p != end) return error.Truncated;
        var rec: Record = .{
            .state = state,
            .trtype = trtype,
            .adopted = flags & FLAG_ADOPTED != 0,
            .allow_any_host = flags & FLAG_ALLOW_ANY_HOST != 0,
            .created_s = std.mem.readInt(i64, &rw.created_s_le, .little),
            .adopted_s = std.mem.readInt(i64, &rw.adopted_s_le, .little),
            .adopt_class = adopt_class,
            .ns_uuid = rw.ns_uuid,
            .subnqn = try gpa.dupe(u8, subnqn),
            .backing_path = undefined,
            .backing_canonical = undefined,
            .listeners = listeners,
            .hosts = hosts,
        };
        errdefer gpa.free(rec.subnqn);
        rec.backing_path = try gpa.dupe(u8, backing_path);
        errdefer gpa.free(rec.backing_path);
        rec.backing_canonical = try gpa.dupe(u8, backing_canonical);
        errdefer gpa.free(rec.backing_canonical);
        checkLaws(&rec) catch return error.BadRecord;
        for (out.records.items) |prev| if (std.mem.eql(u8, prev.subnqn, rec.subnqn)) return error.BadRecord;
        out.records.appendAssumeCapacity(rec);
        off = end;
    }
    if (off != bytes.len) return error.TrailingBytes;
    return out;
}

// ---------------------------------------------------------------------------
// The intent lattice (§2.3): pure over a loaded ledger.
// ---------------------------------------------------------------------------

pub const TransitionError = error{
    /// `begin_share` found the subnqn (any state).
    DuplicateSubnqn,
    /// `begin_share` found the canonical backing (any state).
    DuplicateBacking,
    NotFound,
    /// `finalize` from anything but `pending` (law 6).
    IllegalTransition,
    /// `update_record` may not change the key.
    KeyChanged,
    OutOfMemory,
} || LawError;

/// ∅ → pending. The ledger half of the duplicate guard, under the lock.
pub fn beginShare(gpa: Allocator, l: *Ledger, rec: *const Record) TransitionError!void {
    try checkLaws(rec);
    if (l.find(rec.subnqn) != null) return error.DuplicateSubnqn;
    if (l.findByBacking(rec.backing_canonical) != null) return error.DuplicateBacking;
    var copy = try rec.clone(gpa);
    copy.state = .pending;
    errdefer copy.deinit(gpa);
    try l.records.append(gpa, copy);
}

/// pending → active, only.
pub fn finalizeShare(l: *Ledger, subnqn: []const u8) TransitionError!void {
    const r = l.find(subnqn) orelse return error.NotFound;
    if (r.state != .pending) return error.IllegalTransition;
    r.state = .active;
}

/// any → removing (idempotent).
pub fn markRemoving(l: *Ledger, subnqn: []const u8) TransitionError!void {
    const r = l.find(subnqn) orelse return error.NotFound;
    r.state = .removing;
}

/// Delete after the teardown completed.
pub fn delete(gpa: Allocator, l: *Ledger, subnqn: []const u8) TransitionError!void {
    for (l.records.items, 0..) |*r, i| if (std.mem.eql(u8, r.subnqn, subnqn)) {
        var removed = l.records.orderedRemove(i);
        removed.deinit(gpa);
        return;
    };
    return error.NotFound;
}

/// A bookkeeping refresh (an adopted record's listener capture): the key
/// may not change; the state is the record's.
pub fn updateRecord(gpa: Allocator, l: *Ledger, subnqn: []const u8, next: *const Record) TransitionError!void {
    if (!std.mem.eql(u8, subnqn, next.subnqn)) return error.KeyChanged;
    try checkLaws(next);
    const r = l.find(subnqn) orelse return error.NotFound;
    var copy = try next.clone(gpa);
    errdefer copy.deinit(gpa);
    r.deinit(gpa);
    r.* = copy;
}

// ---------------------------------------------------------------------------
// The store: the file under laws 1 and 2.
// ---------------------------------------------------------------------------

pub const StoreError = errors.Error || DecodeError || EncodeError || error{ LockFailed, DirUnwritable, NameTooLong };

pub const Store = struct {
    gpa: Allocator,
    pool: *uring_fs.Pool,
    dir: []const u8,
    path: [linux.PATH_MAX]u8 = undefined,
    path_len: usize = 0,

    pub fn init(gpa: Allocator, pool: *uring_fs.Pool, dir: []const u8) error{NameTooLong}!Store {
        var s: Store = .{ .gpa = gpa, .pool = pool, .dir = dir };
        const p = std.fmt.bufPrint(&s.path, "{s}/{s}", .{ dir, FILE_NAME }) catch return error.NameTooLong;
        s.path_len = p.len;
        return s;
    }

    pub fn filePath(self: *const Store) []const u8 {
        return self.path[0..self.path_len];
    }

    fn sibling(self: *const Store, buf: *[linux.PATH_MAX]u8, name: []const u8) error{NameTooLong}![:0]const u8 {
        return std.fmt.bufPrintSentinel(buf, "{s}/{s}", .{ self.dir, name }, 0) catch error.NameTooLong;
    }

    /// Law 1: read the file (absent ⇒ an empty ledger at generation 0);
    /// a leftover `.tmp` is logged and ignored. Nothing is created.
    pub fn load(self: *Store) StoreError!Ledger {
        var tb: [linux.PATH_MAX]u8 = undefined;
        const tmp = try self.sibling(&tb, TMP_NAME);
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(linux.AT.FDCWD, tmp, 0, .{ .SIZE = true }, &st)) == .SUCCESS) {
            log.warn("nvmeof ledger: a leftover {s} ({d} B) sits beside {s} — an interrupted write; the durable file is authoritative and the leftover is ignored (the next write replaces it)", .{ tmp, st.size, self.filePath() });
        }
        var res = self.pool.readAll(self.filePath()) catch |e| switch (e) {
            error.FileNotFound => return .{},
            else => return e,
        };
        defer res.deinit();
        return decode(self.gpa, res.bytes());
    }

    /// The mutation under law 2: lock → load → `body(ctx, &ledger)` →
    /// generation + 1 → encode → tmp → fdatasync → rename → fsync(dir).
    /// The dir is created 0700 on the first mutation only.
    pub fn mutate(self: *Store, ctx: anytype, comptime body: fn (@TypeOf(ctx), *Ledger) anyerror!void) anyerror!void {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const dir_z = std.fmt.bufPrintSentinel(&zb, "{s}", .{self.dir}, 0) catch return error.NameTooLong;
        if (linux.errno(linux.mkdir(dir_z, 0o700)) == .NOENT) {
            // One missing parent (`/var/lib/squeezefs`): created 0700 too.
            if (std.fs.path.dirname(self.dir)) |parent| {
                var pz: [linux.PATH_MAX]u8 = undefined;
                const parent_z = std.fmt.bufPrintSentinel(&pz, "{s}", .{parent}, 0) catch return error.NameTooLong;
                _ = linux.mkdir(parent_z, 0o700);
                _ = linux.mkdir(dir_z, 0o700);
            }
        }
        var lb: [linux.PATH_MAX]u8 = undefined;
        const lock_z = try self.sibling(&lb, LOCK_NAME);
        const lrc = linux.open(lock_z, .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(lrc) != .SUCCESS) {
            log.err("nvmeof ledger: cannot open {s} ({t}) — is {s} writable?", .{ lock_z, linux.errno(lrc), self.dir });
            return error.DirUnwritable;
        }
        const lock_fd: linux.fd_t = @intCast(lrc);
        defer _ = linux.close(lock_fd);
        while (true) {
            switch (linux.errno(linux.flock(lock_fd, std.posix.LOCK.EX))) {
                .SUCCESS => break,
                .INTR => continue,
                else => |e| {
                    log.err("nvmeof ledger: flock({s}) failed ({t})", .{ lock_z, e });
                    return error.LockFailed;
                },
            }
        }
        var l = try self.load();
        defer l.deinit(self.gpa);
        try body(ctx, &l);
        l.generation += 1;
        l.written_s = @intCast(clock.realtimeCoarseS());
        const image = try encode(self.gpa, &l);
        defer self.gpa.free(image);
        var tb: [linux.PATH_MAX]u8 = undefined;
        const tmp_z = try self.sibling(&tb, TMP_NAME);
        // Created 0600 here (the pool's create mode is the control plane's
        // 0644); the pool then truncates and writes it.
        const trc = linux.open(tmp_z, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(trc) != .SUCCESS) return errors.fromErrno(linux.errno(trc));
        _ = linux.close(@intCast(trc));
        try self.pool.writeAll(tmp_z, image);
        try self.pool.fdatasync(tmp_z);
        var fb: [linux.PATH_MAX]u8 = undefined;
        const final_z = try self.sibling(&fb, FILE_NAME);
        const rrc = linux.renameat(linux.AT.FDCWD, tmp_z, linux.AT.FDCWD, final_z);
        if (linux.errno(rrc) != .SUCCESS) return errors.fromErrno(linux.errno(rrc));
        const drc = linux.open(dir_z, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(drc) == .SUCCESS) {
            const dfd: linux.fd_t = @intCast(drc);
            defer _ = linux.close(dfd);
            _ = linux.fsync(dfd);
        }
        stats.nvmet_ledger_writes.inc();
        stats.nvmet_ledger_generation.set(l.generation);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn sampleRecord(gpa: Allocator, subnqn: []const u8, backing: []const u8, trtype: Trtype) !Record {
    var listeners = try gpa.alloc(Listener, if (trtype == .tcp) 2 else 1);
    if (trtype == .tcp) {
        listeners[0] = try Listener.tcp("127.0.0.1", 4420);
        listeners[0].port_id = 53773;
        listeners[1] = try Listener.tcp("::1", 4421);
        listeners[1].port_id = 53010;
    } else {
        listeners[0] = Listener.loop;
        listeners[0].port_id = 53500;
    }
    const hosts = try gpa.alloc([]u8, 1);
    hosts[0] = try gpa.dupe(u8, "nqn.2014-08.org.nvmexpress:uuid:h1");
    return .{
        .state = .active,
        .trtype = trtype,
        .adopted = false,
        .allow_any_host = false,
        .created_s = 1_760_000_000,
        .adopted_s = 0,
        .adopt_class = .none,
        .ns_uuid = .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 },
        .subnqn = try gpa.dupe(u8, subnqn),
        .backing_path = try gpa.dupe(u8, backing),
        .backing_canonical = try gpa.dupe(u8, backing),
        .listeners = listeners,
        .hosts = hosts,
    };
}

fn expectRecordEql(a: *const Record, b: *const Record) !void {
    try testing.expectEqual(a.state, b.state);
    try testing.expectEqual(a.trtype, b.trtype);
    try testing.expectEqual(a.adopted, b.adopted);
    try testing.expectEqual(a.allow_any_host, b.allow_any_host);
    try testing.expectEqual(a.created_s, b.created_s);
    try testing.expectEqual(a.adopted_s, b.adopted_s);
    try testing.expectEqual(a.adopt_class, b.adopt_class);
    try testing.expectEqual(a.ns_uuid, b.ns_uuid);
    try testing.expectEqualStrings(a.subnqn, b.subnqn);
    try testing.expectEqualStrings(a.backing_path, b.backing_path);
    try testing.expectEqualStrings(a.backing_canonical, b.backing_canonical);
    try testing.expectEqualSlices(Listener, a.listeners, b.listeners);
    try testing.expectEqual(a.hosts.len, b.hosts.len);
    for (a.hosts, b.hosts) |x, y| try testing.expectEqualStrings(x, y);
}

test "codec: encode∘decode = id for every record shape (tcp v4+v6 listeners with hosts; loop allow-any; adopted ledger-loss; removing); the head's generation and written_s ride; a 2-listener record is 48 + framed + 2 × 24" {
    const gpa = testing.allocator;
    var l: Ledger = .{ .generation = 7, .written_s = 1_760_000_123 };
    defer l.deinit(gpa);
    try l.records.append(gpa, try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-a", "/build/a.img", .tcp));
    var b = try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-b", "/build/b.img", .loop);
    for (b.hosts) |h| gpa.free(h);
    gpa.free(b.hosts);
    b.hosts = try gpa.alloc([]u8, 0);
    b.allow_any_host = true;
    b.adopted = true;
    b.adopt_class = .ledger_loss;
    b.adopted_s = 1_760_000_050;
    b.state = .removing;
    try l.records.append(gpa, b);
    try testing.expectEqual(@as(usize, 48 + 2 + 32 + 2 + 12 + 2 + 12 + 48 + 2 + 34), l.records.items[0].encodedLen());
    const image = try encode(gpa, &l);
    defer gpa.free(image);
    try testing.expectEqual(l.encodedLen(), image.len);
    var back = try decode(gpa, image);
    defer back.deinit(gpa);
    try testing.expectEqual(@as(u64, 7), back.generation);
    try testing.expectEqual(@as(i64, 1_760_000_123), back.written_s);
    try testing.expectEqual(@as(usize, 2), back.records.items.len);
    try expectRecordEql(&l.records.items[0], &back.records.items[0]);
    try expectRecordEql(&l.records.items[1], &back.records.items[1]);
    // A bit flip anywhere refuses (the checksum covers the whole image).
    var flipped = try gpa.dupe(u8, image);
    defer gpa.free(flipped);
    flipped[image.len - 1] ^= 1;
    try testing.expectError(error.ChecksumMismatch, decode(gpa, flipped));
}

test "forward-only: version 2 refuses NewerBinary before any field check (even with a bad checksum); an unknown head flag, an unknown record flag and a lying rec_len refuse; a truncated image is Truncated; trailing bytes refuse" {
    const gpa = testing.allocator;
    var l: Ledger = .{};
    defer l.deinit(gpa);
    try l.records.append(gpa, try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-a", "/build/a.img", .tcp));
    const image = try encode(gpa, &l);
    defer gpa.free(image);
    var v2 = try gpa.dupe(u8, image);
    defer gpa.free(v2);
    std.mem.writeInt(u16, v2[8..10], 2, .little);
    v2[20] ^= 0xff;
    try testing.expectError(error.NewerBinary, decode(gpa, v2));
    var hf = try gpa.dupe(u8, image);
    defer gpa.free(hf);
    std.mem.writeInt(u16, hf[10..12], 4, .little);
    std.mem.writeInt(u64, hf[HEAD_HASHED..][0..8], hashImage(hf), .little);
    try testing.expectError(error.UnknownFlag, decode(gpa, hf));
    var rf = try gpa.dupe(u8, image);
    defer gpa.free(rf);
    std.mem.writeInt(u16, rf[HEAD_LEN + 6 ..][0..2], 0x8000, .little);
    std.mem.writeInt(u64, rf[HEAD_HASHED..][0..8], hashImage(rf), .little);
    try testing.expectError(error.UnknownFlag, decode(gpa, rf));
    var lying = try gpa.dupe(u8, image);
    defer gpa.free(lying);
    std.mem.writeInt(u32, lying[HEAD_LEN..][0..4], @intCast(image.len - HEAD_LEN - 1), .little);
    std.mem.writeInt(u64, lying[HEAD_HASHED..][0..8], hashImage(lying), .little);
    try testing.expectError(error.Truncated, decode(gpa, lying));
    try testing.expectError(error.BadLength, decode(gpa, image[0 .. image.len - 1]));
    try testing.expectError(error.BadLength, decode(gpa, image[0..10]));
    try testing.expectError(error.BadMagic, decode(gpa, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));
    var trailing = try gpa.alloc(u8, image.len + 1);
    defer gpa.free(trailing);
    @memcpy(trailing[0..image.len], image);
    trailing[image.len] = 0;
    try testing.expectError(error.BadLength, decode(gpa, trailing));
    // An empty ledger round-trips to generation 0 and no records.
    var empty: Ledger = .{};
    const ei = try encode(gpa, &empty);
    defer gpa.free(ei);
    try testing.expectEqual(HEAD_LEN, ei.len);
    var eb = try decode(gpa, ei);
    defer eb.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), eb.records.items.len);
}

fn ledgerFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [2048]u8 = undefined;
    const len = smith.slice(&buf);
    if (decode(testing.allocator, buf[0..len])) |*l| {
        var ll = l.*;
        ll.deinit(testing.allocator);
    } else |_| {}
}

test "fuzz: the ledger decoder is total on arbitrary bytes" {
    try std.testing.fuzz({}, ledgerFuzz, .{});
}

test "the intent lattice: begin(pending) → finalize(active); finalize from active or removing refuses; mark_removing is idempotent; delete after teardown; begin refuses a duplicate subnqn and a duplicate backing (any state); update may not change the key; the value laws refuse at begin" {
    const gpa = testing.allocator;
    var l: Ledger = .{};
    defer l.deinit(gpa);
    var a = try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-a", "/build/a.img", .tcp);
    defer a.deinit(gpa);
    try beginShare(gpa, &l, &a);
    try testing.expectEqual(State.pending, l.find(a.subnqn).?.state);
    try testing.expectError(error.DuplicateSubnqn, beginShare(gpa, &l, &a));
    var same_backing = try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-c", "/build/a.img", .tcp);
    defer same_backing.deinit(gpa);
    try testing.expectError(error.DuplicateBacking, beginShare(gpa, &l, &same_backing));
    try finalizeShare(&l, a.subnqn);
    try testing.expectEqual(State.active, l.find(a.subnqn).?.state);
    try testing.expectError(error.IllegalTransition, finalizeShare(&l, a.subnqn));
    try markRemoving(&l, a.subnqn);
    try markRemoving(&l, a.subnqn);
    try testing.expectEqual(State.removing, l.find(a.subnqn).?.state);
    try testing.expectError(error.IllegalTransition, finalizeShare(&l, a.subnqn));
    // Still a duplicate while removing (any state).
    try testing.expectError(error.DuplicateSubnqn, beginShare(gpa, &l, &a));
    try testing.expectError(error.NotFound, finalizeShare(&l, "nqn.none"));
    try testing.expectError(error.NotFound, markRemoving(&l, "nqn.none"));
    try testing.expectError(error.KeyChanged, updateRecord(gpa, &l, a.subnqn, &same_backing));
    try delete(gpa, &l, a.subnqn);
    try testing.expectError(error.NotFound, delete(gpa, &l, a.subnqn));
    try testing.expectEqual(@as(usize, 0), l.records.items.len);
    // The laws at begin.
    var bad = try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-d", "/build/d.img", .tcp);
    defer bad.deinit(gpa);
    gpa.free(bad.listeners);
    bad.listeners = try gpa.alloc(Listener, 0);
    try testing.expectError(error.NoListener, beginShare(gpa, &l, &bad));
    var any = try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-e", "/build/e.img", .loop);
    defer any.deinit(gpa);
    any.allow_any_host = true;
    try testing.expectError(error.HostsVsAllowAny, beginShare(gpa, &l, &any));
    var mixed = try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-f", "/build/f.img", .loop);
    defer mixed.deinit(gpa);
    mixed.listeners[0] = try Listener.tcp("127.0.0.1", 1);
    try testing.expectError(error.ListenerVsTrtype, beginShare(gpa, &l, &mixed));
    try testing.expect(l.hostNamedElsewhere("nqn.none", "x") == false);
}

const StoreRig = struct {
    gpa: Allocator,
    pool: *uring_fs.Pool,
    dir: [:0]u8,
    store: Store,

    fn init(gpa: Allocator) !StoreRig {
        _ = linux.mkdir("/build/tmp", 0o755);
        const dir = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/nvmet-ledger-{d}", .{linux.getpid()}, 0);
        errdefer gpa.free(dir);
        const pool = try uring_fs.Pool.init(gpa, uring_fs.Config.fixed(1, uring_fs.nofileSoftAsFound()));
        errdefer pool.deinit();
        return .{ .gpa = gpa, .pool = pool, .dir = dir, .store = try Store.init(gpa, pool, dir) };
    }

    fn deinit(self: *StoreRig) void {
        Io.Dir.cwd().deleteTree(testing.io, self.dir) catch {};
        self.pool.deinit();
        self.gpa.free(self.dir);
    }
};

fn addA(gpa: Allocator, l: *Ledger) anyerror!void {
    var a = try sampleRecord(gpa, "nqn.2026-10.io.squeezefs:share-a", "/build/a.img", .tcp);
    defer a.deinit(gpa);
    try beginShare(gpa, l, &a);
}

fn finalizeA(_: void, l: *Ledger) anyerror!void {
    try finalizeShare(l, "nqn.2026-10.io.squeezefs:share-a");
}

test "store: load never writes (an absent dir loads an empty ledger and creates nothing; 100 loads leave the file byte-identical); the first mutation creates the dir 0700 and the file 0600 by tmp + rename; the generation increments once per write; a leftover .tmp is ignored and the durable file is authoritative" {
    const gpa = testing.allocator;
    var rig = StoreRig.init(gpa) catch |e| switch (e) {
        error.RingSetupFailed => return error.SkipZigTest,
        else => return e,
    };
    defer rig.deinit();
    var st: linux.Statx = undefined;
    var l0 = try rig.store.load();
    l0.deinit(gpa);
    try testing.expectEqual(linux.E.NOENT, linux.errno(linux.statx(linux.AT.FDCWD, rig.dir, 0, .{ .TYPE = true }, &st)));
    try rig.store.mutate(gpa, addA);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, rig.dir, 0, .{ .MODE = true }, &st)));
    try testing.expectEqual(@as(u16, 0o700), st.mode & 0o777);
    var fz: [linux.PATH_MAX]u8 = undefined;
    const file_z = try std.fmt.bufPrintSentinel(&fz, "{s}/{s}", .{ rig.dir, FILE_NAME }, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, file_z, 0, .{ .MODE = true }, &st)));
    try testing.expectEqual(@as(u16, 0o600), st.mode & 0o777);
    var l1 = try rig.store.load();
    defer l1.deinit(gpa);
    try testing.expectEqual(@as(u64, 1), l1.generation);
    try testing.expectEqual(State.pending, l1.records.items[0].state);
    try rig.store.mutate({}, finalizeA);
    const image1 = try Io.Dir.cwd().readFileAlloc(testing.io, file_z, gpa, .limited(1 << 20));
    defer gpa.free(image1);
    var i: usize = 0;
    while (i < 100) : (i += 1) {
        var ln = try rig.store.load();
        ln.deinit(gpa);
    }
    const image2 = try Io.Dir.cwd().readFileAlloc(testing.io, file_z, gpa, .limited(1 << 20));
    defer gpa.free(image2);
    try testing.expectEqualSlices(u8, image1, image2);
    var l2 = try rig.store.load();
    defer l2.deinit(gpa);
    try testing.expectEqual(@as(u64, 2), l2.generation);
    try testing.expectEqual(State.active, l2.records.items[0].state);
    // A leftover tmp: logged (warn), ignored; the durable file still loads.
    var tz: [linux.PATH_MAX]u8 = undefined;
    const tmp_z = try std.fmt.bufPrintSentinel(&tz, "{s}/{s}", .{ rig.dir, TMP_NAME }, 0);
    try Io.Dir.cwd().writeFile(testing.io, .{ .sub_path = tmp_z, .data = "garbage" });
    var l3 = try rig.store.load();
    defer l3.deinit(gpa);
    try testing.expectEqual(@as(u64, 2), l3.generation);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, tmp_z, 0, .{ .TYPE = true }, &st)));
    // The next write replaces it.
    try rig.store.mutate({}, struct {
        fn f(_: void, l: *Ledger) anyerror!void {
            try markRemoving(l, "nqn.2026-10.io.squeezefs:share-a");
        }
    }.f);
    try testing.expectEqual(linux.E.NOENT, linux.errno(linux.statx(linux.AT.FDCWD, tmp_z, 0, .{ .TYPE = true }, &st)));
    try testing.expectEqual(@as(u64, 3), stats.nvmet_ledger_generation.load());
}

test "store: two mutators under the flock serialize (two threads × 50 writes = generation 100, every record present)" {
    const gpa = testing.allocator;
    var rig = StoreRig.init(gpa) catch |e| switch (e) {
        error.RingSetupFailed => return error.SkipZigTest,
        else => return e,
    };
    defer rig.deinit();
    const Worker = struct {
        store: *Store,
        gpa: Allocator,
        base: u32,
        fn run(self: *@This()) void {
            var i: u32 = 0;
            while (i < 50) : (i += 1) {
                const Ctx = struct { gpa: Allocator, n: u32 };
                self.store.mutate(Ctx{ .gpa = self.gpa, .n = self.base + i }, struct {
                    fn f(c: Ctx, l: *Ledger) anyerror!void {
                        var nb: [64]u8 = undefined;
                        var bb: [64]u8 = undefined;
                        var r = try sampleRecord(c.gpa, try std.fmt.bufPrint(&nb, "nqn.2026-10.io.squeezefs:share-{d}", .{c.n}), try std.fmt.bufPrint(&bb, "/build/{d}.img", .{c.n}), .loop);
                        defer r.deinit(c.gpa);
                        try beginShare(c.gpa, l, &r);
                    }
                }.f) catch |e| std.debug.panic("mutate failed: {s}", .{@errorName(e)});
            }
        }
    };
    var w1: Worker = .{ .store = &rig.store, .gpa = gpa, .base = 0 };
    var w2: Worker = .{ .store = &rig.store, .gpa = gpa, .base = 1000 };
    const t1 = try std.Thread.spawn(.{}, Worker.run, .{&w1});
    const t2 = try std.Thread.spawn(.{}, Worker.run, .{&w2});
    t1.join();
    t2.join();
    var l = try rig.store.load();
    defer l.deinit(gpa);
    try testing.expectEqual(@as(u64, 100), l.generation);
    try testing.expectEqual(@as(usize, 100), l.records.items.len);
}
