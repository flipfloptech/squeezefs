//! Layer B2 of the D0 single-writer guard: the `writer_claim` record, the
//! writer's identity, the staleness law and the gate's classification
//! (charter §5.7; `docs/porting/guard_claims.md` §2.1, §2.3, §2.5, §3.2).
//! Portable: the record codec and `classify` are pure; the Linux readers
//! of the identity (`proc.zig`) and the volume binding (`gate.zig`) live
//! beside them.
//!
//! The record is ONE tree-0 control record (`control_records.writerClaimKey`:
//! no slot, no lease, no door, never migrated — `dynamic_routing.md` Q4,
//! decision 0003 §AS). Its
//! value is a byte-exact `extern struct` (quirk Q1: never a JSON blob):
//! magic/version/checksum like every other on-disk unit (charter §12).
//!
//! The dead-pid PROOF (quirk Q4) is the Linux process identity triple
//! `(boot_id, pid, start_ticks)`: a `/proc/<pid>` that is absent, or whose
//! `stat` field 22 (start time in clock ticks since boot) differs from the
//! claim's, is a different process — `kill(pid, 0) == ESRCH` reads a reused
//! pid as the live author and leaves a dead holder's claim to the TTL. An
//! unreadable local boot id proves nothing (never a same-host verdict).

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const checksum = @import("../../core/checksum.zig");

// ---------------------------------------------------------------------------
// Constants, with WHY.
// ---------------------------------------------------------------------------

/// The ONE cadence of the claim refresh and the PR re-check (FC:191). The
/// TTL is 4.5 beats: four lost beats are tolerated before a holder reads
/// stale (`tie test`).
pub const HEARTBEAT_INTERVAL_S: u64 = 10;

/// The kernel's default NVMe I/O timeout (`nvme_core.io_timeout`, 30 s):
/// one beat can sit behind one timed-out I/O on the volume before it fails
/// loud, and that stall must not evict a live holder.
pub const NVME_IO_TIMEOUT_S: u64 = 30;

/// "One staleness law" (FC:195): a claim older than this is STALE. The
/// port's derivation (the reference states none): a beat stalled behind one
/// NVMe I/O timeout (30 s) plus one full beat (10 s) must still read fresh,
/// so `TTL > NVME_IO_TIMEOUT_S + HEARTBEAT_INTERVAL_S`; 45 = 4.5 × beat.
pub const STALE_TTL_S: u64 = 45;

/// Quirk Q10's skew term for cross-host claim ages: the membership
/// derivation's `2 × skew_max` is 45 ms at ±500 ppm over one TTL, floored
/// at 1 s because a wall clock STEPS (NTP) in whole-second grains. A claim
/// stamped further than this into the future reads FRESH (conservative) and
/// is logged; on PR a preempt needs `age > TTL + skew`.
pub const SKEW_BOUND_S: u64 = 1;

// ---------------------------------------------------------------------------
// The record.
// ---------------------------------------------------------------------------

pub const MAGIC: [4]u8 = "WCLM".*;
pub const VERSION: u8 = 1;
/// `kind` 0 = the writer claim; 1 = a membership row on the same shape
/// (`sym/membership/shard.zig`, decision 0003 §E).
pub const KIND_WRITER: u8 = 0;
pub const KIND_MEMBER: u8 = 1;

/// The on-disk form (little-endian throughout; `reserved*` read as 0).
pub const WriterClaimV1 = extern struct {
    magic: [4]u8,
    version: u8,
    kind: u8,
    reserved0: [2]u8,
    /// `hash64(boot_id ‖ hostname ‖ mount_slot ‖ pid ‖ start_ticks)`: one
    /// value per INCARNATION, so a crashed incarnation's PR registration is
    /// visible as "own stale" to the register ladder (a per-host key would
    /// make every kill -9 remount an idempotent same-key register and the
    /// ladder's recovery arm unreachable).
    writer_id_le: [8]u8,
    /// The heartbeat, realtime ns (the one cross-host comparable clock).
    ts_ns_le: [8]u8,
    pid_le: [4]u8,
    reserved1: [4]u8,
    /// `/proc/self/stat` field 22: the pid's incarnation word (module doc).
    start_ticks_le: [8]u8,
    /// `/proc/sys/kernel/random/boot_id`, zero when unreadable.
    boot_id: [16]u8,
    hostname_hash_le: [8]u8,
    heartbeat_seq_le: [8]u8,
    /// The DLM S2 era — rung 6; 0 until then.
    term_le: [8]u8,
    csum_le: [8]u8,
};

pub const CLAIM_LEN: usize = @sizeOf(WriterClaimV1);

comptime {
    std.debug.assert(CLAIM_LEN == 88);
    std.debug.assert(@offsetOf(WriterClaimV1, "writer_id_le") == 8);
    std.debug.assert(@offsetOf(WriterClaimV1, "ts_ns_le") == 16);
    std.debug.assert(@offsetOf(WriterClaimV1, "pid_le") == 24);
    std.debug.assert(@offsetOf(WriterClaimV1, "start_ticks_le") == 32);
    std.debug.assert(@offsetOf(WriterClaimV1, "boot_id") == 40);
    std.debug.assert(@offsetOf(WriterClaimV1, "hostname_hash_le") == 56);
    std.debug.assert(@offsetOf(WriterClaimV1, "heartbeat_seq_le") == 64);
    std.debug.assert(@offsetOf(WriterClaimV1, "term_le") == 72);
    std.debug.assert(@offsetOf(WriterClaimV1, "csum_le") == 80);
}

/// The RAM form.
pub const Claim = struct {
    writer_id: u64,
    ts_ns: i64,
    pid: u32,
    start_ticks: u64,
    boot_id: [16]u8,
    hostname_hash: u64,
    heartbeat_seq: u64,
    term: u64 = 0,

    pub fn encode(self: Claim) [CLAIM_LEN]u8 {
        var w: WriterClaimV1 = undefined;
        w.magic = MAGIC;
        w.version = VERSION;
        w.kind = KIND_WRITER;
        w.reserved0 = @splat(0);
        std.mem.writeInt(u64, &w.writer_id_le, self.writer_id, .little);
        std.mem.writeInt(i64, &w.ts_ns_le, self.ts_ns, .little);
        std.mem.writeInt(u32, &w.pid_le, self.pid, .little);
        w.reserved1 = @splat(0);
        std.mem.writeInt(u64, &w.start_ticks_le, self.start_ticks, .little);
        w.boot_id = self.boot_id;
        std.mem.writeInt(u64, &w.hostname_hash_le, self.hostname_hash, .little);
        std.mem.writeInt(u64, &w.heartbeat_seq_le, self.heartbeat_seq, .little);
        std.mem.writeInt(u64, &w.term_le, self.term, .little);
        w.csum_le = @splat(0);
        var bytes = std.mem.toBytes(w);
        checksum.stamp(&bytes, @offsetOf(WriterClaimV1, "csum_le"));
        return bytes;
    }

    /// Total: exactly the encoder's form decodes (length, magic, version,
    /// kind, zero reserved bytes, checksum); anything else is `Corrupt`,
    /// which the gate treats as an UNDECODABLE claim (stale foreign, never
    /// auto-taken).
    pub fn decode(bytes: []const u8) error{Corrupt}!Claim {
        if (bytes.len != CLAIM_LEN) return error.Corrupt;
        if (!checksum.verify(bytes, @offsetOf(WriterClaimV1, "csum_le"))) return error.Corrupt;
        const w = std.mem.bytesToValue(WriterClaimV1, bytes[0..CLAIM_LEN]);
        if (!std.mem.eql(u8, &w.magic, &MAGIC) or w.version != VERSION or w.kind != KIND_WRITER) return error.Corrupt;
        if (!std.mem.allEqual(u8, &w.reserved0, 0) or !std.mem.allEqual(u8, &w.reserved1, 0)) return error.Corrupt;
        return .{
            .writer_id = std.mem.readInt(u64, &w.writer_id_le, .little),
            .ts_ns = std.mem.readInt(i64, &w.ts_ns_le, .little),
            .pid = std.mem.readInt(u32, &w.pid_le, .little),
            .start_ticks = std.mem.readInt(u64, &w.start_ticks_le, .little),
            .boot_id = w.boot_id,
            .hostname_hash = std.mem.readInt(u64, &w.hostname_hash_le, .little),
            .heartbeat_seq = std.mem.readInt(u64, &w.heartbeat_seq_le, .little),
            .term = std.mem.readInt(u64, &w.term_le, .little),
        };
    }

    /// `now − ts` in whole seconds, saturating at 0: a future stamp reads
    /// age 0 (fresh — the conservative direction, B:832-834).
    pub fn ageS(self: Claim, now_ns: i64) u64 {
        if (self.ts_ns >= now_ns) return 0;
        return @intCast(@divFloor(now_ns - self.ts_ns, std.time.ns_per_s));
    }

    /// Stamped further than the skew bound into the future: this host's
    /// clock (or the author's) stepped — logged by the gate.
    pub fn fromTheFuture(self: Claim, now_ns: i64) bool {
        return self.ts_ns > now_ns +| @as(i64, SKEW_BOUND_S * std.time.ns_per_s);
    }

    pub fn isFresh(self: Claim, now_ns: i64) bool {
        return self.ageS(now_ns) <= STALE_TTL_S;
    }

    /// Quirk Q10: a PR preempt of a TTL-stale foreign holder waits the skew
    /// term past the TTL too (the device still fences either way).
    pub fn preemptable(self: Claim, now_ns: i64) bool {
        return self.ageS(now_ns) > STALE_TTL_S + SKEW_BOUND_S;
    }

    /// The operator-facing summary `id=… pid=… boot=… age=…s`.
    pub fn writeSummary(self: Claim, w: *std.Io.Writer, now_ns: i64) std.Io.Writer.Error!void {
        try w.print("id={x:0>16}, pid={d}, boot={x}, host={x:0>16}, age={d}s, beats={d}", .{ self.writer_id, self.pid, &self.boot_id, self.hostname_hash, self.ageS(now_ns), self.heartbeat_seq });
    }
};

// ---------------------------------------------------------------------------
// The writer's identity.
// ---------------------------------------------------------------------------

/// What this process IS on this host (read once at open by `proc.zig`;
/// tests build it by hand). A zero `boot_id` = unreadable.
pub const Identity = struct {
    boot_id: [16]u8,
    pid: u32,
    start_ticks: u64,
    hostname_hash: u64,

    pub fn bootIdReadable(self: Identity) bool {
        return !std.mem.allEqual(u8, &self.boot_id, 0);
    }

    /// `writer_id` for mount slot `slot` (the volume's lane index — the
    /// per-daemon coordinate two concurrent volumes of one set differ by).
    pub fn writerId(self: Identity, slot: u16) u64 {
        var h: checksum.Hasher = .init();
        h.update(&self.boot_id);
        h.update(std.mem.asBytes(&std.mem.nativeToLittle(u64, self.hostname_hash)));
        h.update(std.mem.asBytes(&std.mem.nativeToLittle(u16, slot)));
        h.update(std.mem.asBytes(&std.mem.nativeToLittle(u32, self.pid)));
        h.update(std.mem.asBytes(&std.mem.nativeToLittle(u64, self.start_ticks)));
        return h.finalLow64();
    }

    /// A fresh claim of ours: beat 0 at `now_ns`.
    pub fn claim(self: Identity, slot: u16, now_ns: i64) Claim {
        return .{ .writer_id = self.writerId(slot), .ts_ns = now_ns, .pid = self.pid, .start_ticks = self.start_ticks, .boot_id = self.boot_id, .hostname_hash = self.hostname_hash, .heartbeat_seq = 0 };
    }
};

/// The PR key: `hash64(writer_id ‖ NUL ‖ boot_id)` from the two words the
/// claim carries, so a report's holder cross-checks against the record
/// (B:846-850).
pub fn prKey(writer_id: u64, boot_id: [16]u8) u64 {
    var h: checksum.Hasher = .init();
    h.update(std.mem.asBytes(&std.mem.nativeToLittle(u64, writer_id)));
    h.update(&[_]u8{0});
    h.update(&boot_id);
    return h.finalLow64();
}

pub fn hostnameHash(name: []const u8) u64 {
    return checksum.hash64(name);
}

// ---------------------------------------------------------------------------
// Classification.
// ---------------------------------------------------------------------------

/// The pid prover: `startTicksOf(pid)` = the live process's start time in
/// ticks since boot, null when `/proc/<pid>` is absent. Linux reads
/// `/proc/<pid>/stat`; tests inject a table.
pub const PidProbe = struct {
    ctx: ?*anyopaque,
    startTicksOf: *const fn (ctx: ?*anyopaque, pid: u32) ?u64,

    /// The ONE dead-holder law (module doc): absent, or another
    /// incarnation of the pid.
    pub fn provablyDead(self: PidProbe, pid: u32, start_ticks: u64) bool {
        const live = self.startTicksOf(self.ctx, pid) orelse return true;
        return live != start_ticks;
    }
};

pub const ReclaimReason = enum { absent, own_residue, dead_pid };

pub const Evidence = union(enum) {
    reclaimable: ReclaimReason,
    /// A live holder (heartbeated within the TTL, or stamped from the
    /// future); `future` asks the gate for the clock warning.
    fresh_foreign: struct { claim: Claim, future: bool },
    /// Past the TTL (`claim`) or undecodable (`null`) — never auto-taken
    /// without device enforcement.
    stale_foreign: ?Claim,

    pub fn standing(self: Evidence) Standing {
        return switch (self) {
            .reclaimable => .reclaimable,
            .fresh_foreign => .fresh,
            .stale_foreign => .stale,
        };
    }
};

/// The public vocabulary the join target and the quiescent reader (rung 6)
/// read through the probe.
pub const Standing = enum { reclaimable, fresh, stale };

/// `classify_claim` (B:17795-17828) over the replayed raw value.
pub fn classify(raw: ?[]const u8, local: Identity, now_ns: i64, probe: PidProbe) Evidence {
    const bytes = raw orelse return .{ .reclaimable = .absent };
    const c = Claim.decode(bytes) catch return .{ .stale_foreign = null };
    if (local.bootIdReadable() and std.mem.eql(u8, &c.boot_id, &local.boot_id)) {
        if (c.pid == local.pid) return .{ .reclaimable = .own_residue };
        // ANY age: a kill -9 leaves a FRESH claim, and no wait is ever owed
        // to a provably dead holder.
        if (probe.provablyDead(c.pid, c.start_ticks)) return .{ .reclaimable = .dead_pid };
    }
    if (c.fromTheFuture(now_ns)) return .{ .fresh_foreign = .{ .claim = c, .future = true } };
    if (c.isFresh(now_ns)) return .{ .fresh_foreign = .{ .claim = c, .future = false } };
    return .{ .stale_foreign = c };
}

// ---------------------------------------------------------------------------
// Refusal texts (the gate's and `claim clear`'s; one rendering each).
// ---------------------------------------------------------------------------

pub fn writeFreshRefusal(w: *std.Io.Writer, path: []const u8, c: Claim, now_ns: i64) std.Io.Writer.Error!void {
    try w.print("{s}: metadata volume is claimed by a live writer (claim: ", .{path});
    try c.writeSummary(w, now_ns);
    try w.print(") — concurrent mounts of one metadata volume are refused (single-writer guard). A crashed holder on THIS host is reclaimed automatically once its pid is provably dead; otherwise stop that writer or wait for its claim to expire (ttl {d}s)", .{STALE_TTL_S});
}

pub fn writeStaleNonPrRefusal(w: *std.Io.Writer, path: []const u8, c: ?Claim, now_ns: i64) std.Io.Writer.Error!void {
    try w.print("{s}: stale writer claim (", .{path});
    if (c) |cl| try cl.writeSummary(w, now_ns) else try w.writeAll("unparseable claim value");
    try w.writeAll(") from another holder — automatic cross-host takeover is disabled on volumes without NVMe Persistent Reservations (a paused holder cannot be detected); verify the holder is down, then run `squeezefs claim clear <sqmeta-uri>` (operator attestation). The claim is never auto-taken");
}

pub fn writeClearFreshRefusal(w: *std.Io.Writer, path: []const u8, c: Claim, now_ns: i64) std.Io.Writer.Error!void {
    try w.print("{s}: refusing to clear a FRESH writer claim (claim: ", .{path});
    try c.writeSummary(w, now_ns);
    try w.print(") — the holder heartbeated within the {d}s ttl and may be alive. Stop that writer (or wait for the claim to expire), then retry", .{STALE_TTL_S});
}

pub fn writeClearLiveRefusal(w: *std.Io.Writer, path: []const u8) std.Io.Writer.Error!void {
    try w.print("{s}: refusing to clear the writer claim — the volume is live-mounted on this host (the writer lock is held). Unmount it first", .{path});
}

pub fn writeClearOwnRefusal(w: *std.Io.Writer, path: []const u8, c: Claim, now_ns: i64) std.Io.Writer.Error!void {
    try w.print("{s}: refusing to clear the writer claim — it is THIS process's own residue (claim: ", .{path});
    try c.writeSummary(w, now_ns);
    try w.writeAll("); a writer never clears the claim it holds");
}

// ---------------------------------------------------------------------------
// Registrations: the `clients` rows (`mount_registrations`, B:18638-18719).
// ---------------------------------------------------------------------------

pub const RegistrationKind = enum { writer, member };

/// `dead` (provably dead — reclaimable) > `live` (fresh) > `stale`.
pub const RegistrationState = enum {
    live,
    stale,
    dead,

    pub fn name(self: RegistrationState) []const u8 {
        return @tagName(self);
    }
};

pub const Registration = struct {
    kind: RegistrationKind,
    id: u64,
    pid: u32,
    boot_id: [16]u8,
    hostname_hash: u64,
    heartbeat_ts_s: i64,
    age_s: u64,
    state: RegistrationState,

    /// The writer row from a claim under the ONE staleness law and the ONE
    /// dead-holder law.
    pub fn fromClaim(c: Claim, local: Identity, now_ns: i64, probe: PidProbe) Registration {
        const same_host = local.bootIdReadable() and std.mem.eql(u8, &c.boot_id, &local.boot_id);
        const dead = same_host and c.pid != local.pid and probe.provablyDead(c.pid, c.start_ticks);
        return .{
            .kind = .writer,
            .id = c.writer_id,
            .pid = c.pid,
            .boot_id = c.boot_id,
            .hostname_hash = c.hostname_hash,
            .heartbeat_ts_s = @divFloor(c.ts_ns, std.time.ns_per_s),
            .age_s = c.ageS(now_ns),
            .state = if (dead) .dead else if (c.isFresh(now_ns) or c.fromTheFuture(now_ns)) .live else .stale,
        };
    }

    pub fn writeJson(self: Registration, w: *std.Io.Writer, volume: []const u8) std.Io.Writer.Error!void {
        try w.print("{{\"kind\":\"{s}\",\"id\":\"{x:0>16}\",\"pid\":{d},\"boot\":\"{x}\",\"host\":\"{x:0>16}\",\"heartbeat_ts\":{d},\"age_secs\":{d},\"state\":\"{s}\",\"volume\":\"{s}\"}}", .{ @tagName(self.kind), self.id, self.pid, &self.boot_id, self.hostname_hash, self.heartbeat_ts_s, self.age_s, self.state.name(), volume });
    }

    pub fn writeText(self: Registration, w: *std.Io.Writer, volume: []const u8) std.Io.Writer.Error!void {
        try w.print("{s:<7} {x:0>16} {d:<8} {s:<6} {d}s {s}\n", .{ @tagName(self.kind), self.id, self.pid, self.state.name(), self.age_s, volume });
    }
};

/// The typed seam for rung 6's membership census: a source appends its
/// rows after the claim's (readers appear only through it).
pub const RegistrationSource = struct {
    ctx: ?*anyopaque,
    append: *const fn (ctx: ?*anyopaque, gpa: std.mem.Allocator, out: *std.ArrayList(Registration)) std.mem.Allocator.Error!void,
};

pub const RegistrationTotals = struct {
    live: usize = 0,
    stale: usize = 0,
    dead: usize = 0,

    pub fn fold(rows: []const Registration) RegistrationTotals {
        var t: RegistrationTotals = .{};
        for (rows) |r| switch (r.state) {
            .live => t.live += 1,
            .stale => t.stale += 1,
            .dead => t.dead += 1,
        };
        return t;
    }
};

/// The `clients` text report: header, rows, footer (MAIN:8337-8491).
pub fn writeClientsText(w: *std.Io.Writer, rows: []const Registration, volume: []const u8, volumes: usize) std.Io.Writer.Error!void {
    if (rows.len == 0) {
        try w.print("No client registrations on {d} metadata volume(s) — no mount (live or crashed) holds this filesystem.\n", .{volumes});
        return;
    }
    try w.print("{s:<7} {s:<16} {s:<8} {s:<6} {s} {s}\n", .{ "KIND", "ID", "PID", "STATE", "AGE", "VOLUME" });
    for (rows) |r| try r.writeText(w, volume);
    const t = RegistrationTotals.fold(rows);
    try w.print("{d} registration(s): {d} live, {d} stale, {d} dead (reclaimable).\n", .{ rows.len, t.live, t.stale, t.dead });
}

pub fn writeClientsJson(w: *std.Io.Writer, rows: []const Registration, volume: []const u8, volumes: usize) std.Io.Writer.Error!void {
    const t = RegistrationTotals.fold(rows);
    try w.print("{{\"volumes\":{d},\"clients\":[", .{volumes});
    for (rows, 0..) |r, i| {
        if (i != 0) try w.writeByte(',');
        try r.writeJson(w, volume);
    }
    try w.print("],\"live\":{d},\"stale\":{d},\"dead\":{d}}}", .{ t.live, t.stale, t.dead });
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const PidTable = struct {
    rows: []const struct { pid: u32, start: u64 },

    fn probe(self: *const PidTable) PidProbe {
        return .{ .ctx = @ptrCast(@constCast(self)), .startTicksOf = startOf };
    }

    fn startOf(ctx: ?*anyopaque, pid: u32) ?u64 {
        const self: *const PidTable = @ptrCast(@alignCast(ctx.?));
        for (self.rows) |r| if (r.pid == pid) return r.start;
        return null;
    }
};

const boot_a: [16]u8 = "boot-a-boot-a-01".*;
const boot_b: [16]u8 = "boot-b-boot-b-02".*;
const me: Identity = .{ .boot_id = boot_a, .pid = 4000, .start_ticks = 777, .hostname_hash = 0xabc };
const now: i64 = 1_700_000_000 * std.time.ns_per_s;

fn claimOf(boot: [16]u8, pid: u32, start: u64, age_s: i64) Claim {
    return .{ .writer_id = 0x1234, .ts_ns = now - age_s * std.time.ns_per_s, .pid = pid, .start_ticks = start, .boot_id = boot, .hostname_hash = 0xdef, .heartbeat_seq = 3 };
}

test "the record round-trips byte-exactly; decode is total (length, magic, version, kind, reserved, checksum each refuse)" {
    const c = claimOf(boot_a, 42, 99, 5);
    const bytes = c.encode();
    try testing.expectEqual(CLAIM_LEN, bytes.len);
    try testing.expectEqualSlices(u8, &MAGIC, bytes[0..4]);
    const back = try Claim.decode(&bytes);
    try testing.expectEqual(c, back);
    try testing.expectError(error.Corrupt, Claim.decode(bytes[0 .. CLAIM_LEN - 1]));
    var longer: [CLAIM_LEN + 1]u8 = undefined;
    @memcpy(longer[0..CLAIM_LEN], &bytes);
    longer[CLAIM_LEN] = 0;
    try testing.expectError(error.Corrupt, Claim.decode(&longer));
    inline for (.{ 0, 4, 5, 6, 28, 40, 81 }) |off| {
        var bad = bytes;
        bad[off] ^= 0x01;
        try testing.expectError(error.Corrupt, Claim.decode(&bad));
    }
    // A re-stamped forgery of a reserved byte still refuses (reserved read as 0).
    var forged = bytes;
    forged[6] = 1;
    checksum.stamp(&forged, @offsetOf(WriterClaimV1, "csum_le"));
    try testing.expectError(error.Corrupt, Claim.decode(&forged));
    try testing.expectError(error.Corrupt, Claim.decode(&[_]u8{}));
}

test "ties: TTL > nvme_io_timeout + beat (four lost beats tolerated), TTL = 4.5 × beat, the skew floor is one wall-clock grain" {
    try testing.expect(STALE_TTL_S > NVME_IO_TIMEOUT_S + HEARTBEAT_INTERVAL_S);
    try testing.expectEqual(STALE_TTL_S * 2, 9 * HEARTBEAT_INTERVAL_S);
    try testing.expectEqual(@as(u64, 4), STALE_TTL_S / HEARTBEAT_INTERVAL_S);
    try testing.expectEqual(@as(u64, 1), SKEW_BOUND_S);
    // 2 × skew_max at 500 ppm over one TTL is 45 ms — below the floor.
    try testing.expect(2 * STALE_TTL_S * 500 < SKEW_BOUND_S * 1_000_000);
}

test "classify: absent → reclaimable; undecodable → stale foreign (null); own pid+boot → own residue; same boot + dead pid (absent or another incarnation) → reclaimable at ANY age; a live same-host pid is not proof" {
    const table: PidTable = .{ .rows = &.{ .{ .pid = 500, .start = 123 }, .{ .pid = 600, .start = 9000 } } };
    const probe = table.probe();
    try testing.expectEqual(Evidence{ .reclaimable = .absent }, classify(null, me, now, probe));
    try testing.expectEqual(Evidence{ .stale_foreign = null }, classify("garbage", me, now, probe));
    try testing.expectEqual(Evidence{ .reclaimable = .own_residue }, classify(&claimOf(boot_a, 4000, 1, 0).encode(), me, now, probe));
    // pid 700 absent, fresh age 1 s: reclaimed at once.
    try testing.expectEqual(Evidence{ .reclaimable = .dead_pid }, classify(&claimOf(boot_a, 700, 1, 1).encode(), me, now, probe));
    // pid 600 live but started at tick 9000, the claim's author at 50: a reused pid.
    try testing.expectEqual(Evidence{ .reclaimable = .dead_pid }, classify(&claimOf(boot_a, 600, 50, 1).encode(), me, now, probe));
    // pid 500 live, same incarnation, fresh: refused.
    const live_same = classify(&claimOf(boot_a, 500, 123, 1).encode(), me, now, probe);
    try testing.expectEqual(Standing.fresh, live_same.standing());
    try testing.expect(!live_same.fresh_foreign.future);
    // ... and stale once past the TTL (not dead, so no reclaim).
    try testing.expectEqual(Standing.stale, classify(&claimOf(boot_a, 500, 123, 46).encode(), me, now, probe).standing());
}

test "classify: a foreign boot is never same-host (no pid proof applies): fresh ≤ 45 s, stale after; a future stamp beyond the skew bound reads fresh and flags the clock; an unreadable local boot id proves nothing" {
    const table: PidTable = .{ .rows = &.{} };
    const probe = table.probe();
    try testing.expectEqual(Standing.fresh, classify(&claimOf(boot_b, 700, 1, 45).encode(), me, now, probe).standing());
    try testing.expectEqual(Standing.stale, classify(&claimOf(boot_b, 700, 1, 46).encode(), me, now, probe).standing());
    const future = classify(&claimOf(boot_b, 700, 1, -3).encode(), me, now, probe);
    try testing.expectEqual(Standing.fresh, future.standing());
    try testing.expect(future.fresh_foreign.future);
    // Inside the skew bound a future stamp is plain fresh (no warning).
    try testing.expect(!classify(&claimOf(boot_b, 700, 1, -1).encode(), me, now, probe).fresh_foreign.future);
    const blind: Identity = .{ .boot_id = @splat(0), .pid = 4000, .start_ticks = 1, .hostname_hash = 0 };
    const zero_boot_claim = claimOf(@splat(0), 700, 1, 1);
    try testing.expectEqual(Standing.fresh, classify(&zero_boot_claim.encode(), blind, now, probe).standing());
    // Even our own pid under an unreadable boot id is not "ours".
    try testing.expectEqual(Standing.fresh, classify(&claimOf(@splat(0), 4000, 1, 1).encode(), blind, now, probe).standing());
}

test "identity: the writer id is one value per incarnation (pid and start ticks both enter it); the PR key derives from the claim's two words" {
    const a = me.writerId(0);
    try testing.expect(a != me.writerId(1));
    var other = me;
    other.pid += 1;
    try testing.expect(a != other.writerId(0));
    other = me;
    other.start_ticks += 1;
    try testing.expect(a != other.writerId(0));
    try testing.expectEqual(a, me.writerId(0));
    const c = me.claim(0, now);
    try testing.expectEqual(a, c.writer_id);
    try testing.expectEqual(@as(u64, 0), c.heartbeat_seq);
    try testing.expectEqual(prKey(c.writer_id, c.boot_id), prKey(a, boot_a));
    try testing.expect(prKey(a, boot_a) != prKey(a, boot_b));
}

test "registrations: state ranks dead > live > stale; the text and JSON forms carry every column; totals fold" {
    const table: PidTable = .{ .rows = &.{.{ .pid = 500, .start = 123 }} };
    const probe = table.probe();
    const dead = Registration.fromClaim(claimOf(boot_a, 700, 1, 1), me, now, probe);
    try testing.expectEqual(RegistrationState.dead, dead.state);
    const live = Registration.fromClaim(claimOf(boot_a, 500, 123, 10), me, now, probe);
    try testing.expectEqual(RegistrationState.live, live.state);
    try testing.expectEqual(@as(u64, 10), live.age_s);
    const stale = Registration.fromClaim(claimOf(boot_b, 500, 123, 100), me, now, probe);
    try testing.expectEqual(RegistrationState.stale, stale.state);
    const rows = [_]Registration{ dead, live, stale };
    const t = RegistrationTotals.fold(&rows);
    try testing.expectEqual(@as(usize, 1), t.live);
    try testing.expectEqual(@as(usize, 1), t.stale);
    try testing.expectEqual(@as(usize, 1), t.dead);
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeClientsText(&w, &rows, "/dev/x", 1);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "3 registration(s): 1 live, 1 stale, 1 dead (reclaimable).") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "KIND") != null);
    w = .fixed(&buf);
    try writeClientsJson(&w, &rows, "/dev/x", 1);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"state\":\"dead\"") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"live\":1,\"stale\":1,\"dead\":1}") != null);
    w = .fixed(&buf);
    try writeClientsText(&w, &.{}, "/dev/x", 2);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "No client registrations on 2 metadata volume(s)") != null);
}

test "refusal texts name the holder and the remedy" {
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    const c = claimOf(boot_b, 31337, 1, 7);
    try writeFreshRefusal(&w, "/dev/v", c, now);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "pid=31337") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "age=7s") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "ttl 45s") != null);
    w = .fixed(&buf);
    try writeStaleNonPrRefusal(&w, "/dev/v", null, now);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "unparseable claim value") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "squeezefs claim clear <sqmeta-uri>") != null);
    w = .fixed(&buf);
    try writeClearFreshRefusal(&w, "/dev/v", c, now);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "FRESH writer claim") != null);
    w = .fixed(&buf);
    try writeClearLiveRefusal(&w, "/dev/v");
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "live-mounted on this host") != null);
    w = .fixed(&buf);
    try writeClearOwnRefusal(&w, "/dev/v", c, now);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "own residue") != null);
}

fn claimFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [CLAIM_LEN + 8]u8 = undefined;
    const len = smith.slice(&buf);
    _ = Claim.decode(buf[0..len]) catch {};
    if (len >= CLAIM_LEN) {
        const img = buf[0..CLAIM_LEN];
        checksum.stamp(img, @offsetOf(WriterClaimV1, "csum_le"));
        if (Claim.decode(img)) |c| try std.testing.expectEqualSlices(u8, img, &c.encode()) else |_| {}
    }
}

test "fuzz: the writer-claim decoder is total on arbitrary and restamped records; a decoded claim re-encodes byte-identically" {
    try fuzzkit.fuzz({}, claimFuzz, .{});
}
