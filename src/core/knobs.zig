//! The env-knob registry and the ONE parsing convention (charter §1.6).
//!
//! Every `SQUEEZEFS_*` knob the product reads is a row in `registry`. The
//! convention, byte-for-byte the reference's (`docs/porting/knobs_and_rails.md`
//! §1):
//!   - absent = unset / empty / whitespace-only (the value is trimmed first);
//!   - booleans: `1/true/yes/on` vs `0/false/no/off`, case-insensitive;
//!     `=0` DISABLES even a default-ON knob;
//!   - integers carry an inclusive admissible range; out-of-range REFUSES,
//!     never clamps, and the message names the range (what IS admissible),
//!     never the integer type;
//!   - enums match case-insensitively and yield the canonical spelling;
//!   - strings are presence-only (a bad path is the consumer's better error);
//!   - a RETIRED knob with any present value refuses naming its successor
//!     (forward-only — never a silent alias);
//!   - a malformed knob refuses the PROCESS at startup, naming every offender
//!     at once (`refusalReport`); no call site may panic on a knob value and
//!     none may silently default one;
//!   - an unregistered `SQUEEZEFS_*`/`SQZ_*` name is announced as a probable
//!     typo, never refused (mixed-version fleets and the shim's client-side
//!     knobs share one environment).
//!
//! Adding a knob means adding a registry row: `tests/rails.zig` fails the gate
//! on an unregistered knob literal in the source tree, and on a registered
//! non-retired knob nothing reads.
//!
//! Precedence for sizing families stays `explicit > percentage > derived`,
//! implemented per family in a pure `resolve*` function that never refuses
//! (the gate already did) — see `mem/budget.zig`.

const std = @import("std");
const log = @import("log.zig");

pub const Kind = union(enum) {
    bool,
    /// Inclusive admissible range.
    int: struct { lo: i128, hi: i128 },
    enumeration: struct {
        allowed: []const []const u8,
        /// `(value, why)` pairs: a retired VALUE refuses naming why.
        retired: []const struct { value: []const u8, why: []const u8 } = &.{},
    },
    /// Presence only.
    str,
    /// Any present value refuses naming the successor.
    retired: struct { successor: []const u8 },
    /// Read only by test binaries, rigs and scripts: registered so the census
    /// passes, never validated.
    harness,
};

pub const Class = enum {
    /// A real feature/posture toggle.
    switch_,
    /// Int whose absence means "derive it"; explicit wins verbatim.
    derived_override,
    /// Int with a literal shipped default.
    tuning,
    /// Product code armed only by suites (`SQUEEZEFS_TEST_*`); default-inert.
    test_seam,
    /// Same-binary measurement control arm; logged at mount when non-default.
    ab_lever,
    /// Paths, URIs, addresses.
    config,
    enum_select,
    harness,
    retired,
};

pub const Knob = struct {
    key: []const u8,
    kind: Kind,
    /// Documentation of the default ("derived", "on", "4096", "-"), never a
    /// parsed value: call sites carry their own typed default.
    default: []const u8,
    class: Class,
    doc: []const u8,
};

/// Range ceilings shared by the table (`docs/porting/knobs_and_rails.md` §2.1).
/// 10 minutes: the longest a timing knob may park anything.
pub const MS_MAX: i128 = 600_000;
/// 1 TiB: the largest byte-count any knob may name.
pub const BYTES_MAX: i128 = 1 << 40;
pub const MIB_MAX: i128 = BYTES_MAX >> 20;

/// THE REGISTRY. Grouped by subsystem; rows land with the module that reads
/// them (the inverse-census rail refuses a row nothing reads).
pub const registry = [_]Knob{
    // ---- build identity / observability ----
    .{ .key = "SQUEEZEFS_OP_PROFILE", .kind = .bool, .default = "off", .class = .switch_, .doc = "Per-op phase histograms (zero cost off)." },
    .{ .key = "SQUEEZEFS_STATS_KEY_CENSUS", .kind = .bool, .default = "off", .class = .switch_, .doc = "Arm the `.stats` key census (live object keys + per-inode write custody — a debugging surface)." },

    // ---- R5 memory budget ----
    .{ .key = "SQUEEZEFS_MEM_BUDGET_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "R5 memory budget, MiB (absolute > cgroup memory.max × 0.8 > 70 % RAM)." },
    .{ .key = "SQUEEZEFS_TRANSPORT_MEM_MAX", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "Transport payload-buffer cap, MiB (absolute > pct > derived budget/8)." },
    .{ .key = "SQUEEZEFS_TRANSPORT_MEM_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "derived", .class = .derived_override, .doc = "Transport payload-buffer cap as a percentage of the R5 budget." },
    .{ .key = "SQUEEZEFS_PARKED_GATE_ASSIST_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "200", .class = .tuning, .doc = "R5-Red parked-gate self-flush assist window, ms." },

    // ---- io_uring file layer / metadata device posture ----
    .{ .key = "SQUEEZEFS_URING_FS_WORKERS", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .derived_override, .doc = "io_uring file-layer worker count for ad-hoc local file I/O (derived clamp(cpus/4, 4, 64))." },
    .{ .key = "SQUEEZEFS_TEST_META_BUFFERED", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: keep every registered metadata device on BUFFERED I/O (the shared-LUN coherence repro's control arm)." },

    // ---- NVMe block device workers ----
    .{ .key = "SQUEEZEFS_NVME_READ_LANES", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "derived", .class = .derived_override, .doc = "Per-device READ submission fan-out lanes (derived cpus ÷ data devices); 1 = the single-worker A/B posture." },
    .{ .key = "SQUEEZEFS_NVME_WRITE_LANES", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "derived", .class = .derived_override, .doc = "Per-device DATA-WRITE submission fan-out lanes, block-offset affinity (derived cpus ÷ data devices); 1 = the single-worker A/B posture." },
    .{ .key = "SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS", .kind = .{ .int = .{ .lo = 1, .hi = MS_MAX } }, .default = "30000", .class = .test_seam, .doc = "Test seam: NVMe read timeout, ms." },
    // @anchor:nvme_dev_knobs

    // @anchor:mem_budget_knobs

    // @anchor:uring_fs_knobs
};

pub fn lookup(key: []const u8) ?*const Knob {
    for (&registry) |*k| if (std.mem.eql(u8, k.key, key)) return k;
    return null;
}

/// Is this name in our namespace? (`SQUEEZEFS_` or `SQZ_` prefix.)
pub fn isOurs(key: []const u8) bool {
    return std.mem.startsWith(u8, key, "SQUEEZEFS_") or std.mem.startsWith(u8, key, "SQZ_");
}

// ---------------------------------------------------------------------------
// The ONE parser. Pure: no logging, no env reads, no syscalls.
// ---------------------------------------------------------------------------

pub const KnobError = struct {
    key: []const u8,
    /// The TRIMMED raw value.
    raw: []const u8,
    want: []const u8,

    pub fn format(self: KnobError, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{s}='{s}' is invalid: {s}", .{ self.key, self.raw, self.want });
    }
};

/// Absent = unset / empty / whitespace-only; otherwise the trimmed value.
pub fn present(raw: ?[]const u8) ?[]const u8 {
    const r = raw orelse return null;
    const t = std.mem.trim(u8, r, " \t\r\n");
    return if (t.len == 0) null else t;
}

pub const bool_true = [_][]const u8{ "1", "true", "yes", "on" };
pub const bool_false = [_][]const u8{ "0", "false", "no", "off" };
pub const bool_want = "expected a boolean — 1/true/yes/on or 0/false/no/off";

fn eqlIgnoreCase(a: []const u8, b: []const u8) bool {
    return std.ascii.eqlIgnoreCase(a, b);
}

pub fn parseBool(key: []const u8, raw: ?[]const u8) error{Knob}!?bool {
    const v = present(raw) orelse return null;
    for (bool_true) |t| if (eqlIgnoreCase(v, t)) return true;
    for (bool_false) |f| if (eqlIgnoreCase(v, f)) return false;
    last_error = .{ .key = key, .raw = v, .want = bool_want };
    return error.Knob;
}

/// Parse an integer of type `T`; the failure message names the type class
/// only ("expected an integer") — call sites that know a range use
/// `parseIntIn`, whose message names the range.
pub fn parseInt(comptime T: type, key: []const u8, raw: ?[]const u8) error{Knob}!?T {
    const v = present(raw) orelse return null;
    return std.fmt.parseInt(T, v, 10) catch {
        last_error = .{ .key = key, .raw = v, .want = "expected an integer" };
        return error.Knob;
    };
}

/// Inclusive range. Parse failure AND out-of-range share ONE message naming
/// the admissible range. Out-of-range REFUSES, never clamps.
pub fn parseIntIn(comptime T: type, key: []const u8, raw: ?[]const u8, lo: T, hi: T) error{Knob}!?T {
    const v = present(raw) orelse return null;
    const parsed = std.fmt.parseInt(T, v, 10) catch null;
    if (parsed) |p| if (p >= lo and p <= hi) return p;
    last_error = .{ .key = key, .raw = v, .want = rangeWant(T, lo, hi) };
    return error.Knob;
}

/// Case-insensitive match returning the CANONICAL spelling from `allowed`.
pub fn parseEnum(key: []const u8, raw: ?[]const u8, allowed: []const []const u8) error{Knob}!?[]const u8 {
    const v = present(raw) orelse return null;
    for (allowed) |a| if (eqlIgnoreCase(v, a)) return a;
    last_error = .{ .key = key, .raw = v, .want = enumWant(allowed) };
    return error.Knob;
}

pub fn parseStr(raw: ?[]const u8) ?[]const u8 {
    return present(raw);
}

/// The parser is pure and allocation-free; the detail of the most recent
/// failure on this thread lives here so callers that only need `error.Knob`
/// pay nothing and the report builder can read the detail right after.
threadlocal var last_error: KnobError = .{ .key = "", .raw = "", .want = "" };
threadlocal var want_buf: [160]u8 = undefined;

pub fn lastError() KnobError {
    return last_error;
}

fn rangeWant(comptime T: type, lo: T, hi: T) []const u8 {
    return std.fmt.bufPrint(&want_buf, "expected an integer in {d}..={d}", .{ lo, hi }) catch "expected an integer in range";
}

fn enumWant(allowed: []const []const u8) []const u8 {
    var w: std.Io.Writer = .fixed(&want_buf);
    w.writeAll("expected one of ") catch return "expected one of the allowed values";
    for (allowed, 0..) |a, i| {
        if (i != 0) w.writeAll("|") catch break;
        w.writeAll(a) catch break;
    }
    return w.buffered();
}

// ---------------------------------------------------------------------------
// The startup gate.
// ---------------------------------------------------------------------------

pub const Validation = struct {
    /// One line per offender, sorted.
    errors: std.ArrayList([]u8) = .empty,
    /// Unregistered names in our namespace, sorted.
    unknown: std.ArrayList([]u8) = .empty,

    pub fn deinit(self: *Validation, gpa: std.mem.Allocator) void {
        for (self.errors.items) |e| gpa.free(e);
        for (self.unknown.items) |u| gpa.free(u);
        self.errors.deinit(gpa);
        self.unknown.deinit(gpa);
    }

    pub fn isClean(self: Validation) bool {
        return self.errors.items.len == 0 and self.unknown.items.len == 0;
    }
};

/// Validate ONE registered knob against its raw value. Returns the error line
/// (caller-owned) or null when acceptable.
pub fn validateOne(gpa: std.mem.Allocator, knob: *const Knob, raw: []const u8) !?[]u8 {
    switch (knob.kind) {
        .bool => {
            _ = parseBool(knob.key, raw) catch return try std.fmt.allocPrint(gpa, "{f}", .{last_error});
        },
        .int => |r| {
            _ = parseIntIn(i128, knob.key, raw, r.lo, r.hi) catch return try std.fmt.allocPrint(gpa, "{f}", .{last_error});
        },
        .enumeration => |e| {
            const v = present(raw) orelse return null;
            // A retired VALUE is checked BEFORE the allowed set; its message
            // must not read "expected one of" (it was valid once; it is not an
            // unknown spelling).
            for (e.retired) |rv| if (eqlIgnoreCase(v, rv.value)) {
                return try std.fmt.allocPrint(gpa, "{s}='{s}' was RETIRED (forward-only — never a silent alias): {s}", .{ knob.key, v, rv.why });
            };
            _ = parseEnum(knob.key, raw, e.allowed) catch return try std.fmt.allocPrint(gpa, "{f}", .{last_error});
        },
        .str, .harness => {},
        .retired => |r| {
            const v = present(raw) orelse return null;
            return try std.fmt.allocPrint(gpa, "{s}='{s}' was RETIRED (forward-only — never a silent alias): use {s} instead", .{ knob.key, v, r.successor });
        },
    }
    return null;
}

/// Validate every `(key, value)` pair the iterator yields. Keys outside our
/// namespace are skipped; registered keys validate; unregistered ours land in
/// `unknown`. Both lists are sorted so the report is deterministic.
pub fn validateVars(gpa: std.mem.Allocator, env: *const std.process.Environ.Map) !Validation {
    var v: Validation = .{};
    errdefer v.deinit(gpa);
    var it = env.iterator();
    while (it.next()) |entry| {
        const key = entry.key_ptr.*;
        if (!isOurs(key)) continue;
        if (lookup(key)) |knob| {
            if (try validateOne(gpa, knob, entry.value_ptr.*)) |line| try v.errors.append(gpa, line);
        } else {
            try v.unknown.append(gpa, try gpa.dupe(u8, key));
        }
    }
    std.mem.sort([]u8, v.errors.items, {}, lessThanBytes);
    std.mem.sort([]u8, v.unknown.items, {}, lessThanBytes);
    return v;
}

fn lessThanBytes(_: void, a: []u8, b: []u8) bool {
    return std.mem.lessThan(u8, a, b);
}

/// The startup gate. Announces every unknown name on `warn` ("probable typo",
/// never refused) and returns the refusal report when any registered knob is
/// malformed — the caller prints `Error: {report}` and exits 1 BEFORE anything
/// is opened or mounted.
pub fn refusalReport(gpa: std.mem.Allocator, env: *const std.process.Environ.Map, warn: *std.Io.Writer) !?[]u8 {
    var v = try validateVars(gpa, env);
    defer v.deinit(gpa);
    for (v.unknown.items) |name| {
        try warn.print("Warning: {s} is not a SqueezeFS knob (typo? `src/core/knobs.zig` is the registry) — ignored\n", .{name});
    }
    if (v.errors.items.len == 0) return null;
    var out: std.Io.Writer.Allocating = .init(gpa);
    errdefer out.deinit();
    const w = &out.writer;
    try w.writeAll("refusing to start: invalid SqueezeFS environment knob(s)\n");
    for (v.errors.items) |e| try w.print("  - {s}\n", .{e});
    try w.writeAll("  (a malformed knob is never silently defaulted — fix or unset it; empty means unset)");
    return try out.toOwnedSlice();
}

// ---------------------------------------------------------------------------
// In-process readers. They never panic and never refuse: the gate already
// did. A malformed value here (only reachable when the gate was bypassed —
// e.g. a knob set after startup) is logged and the default kept.
// ---------------------------------------------------------------------------

pub fn boolKnob(env: *const std.process.Environ.Map, key: []const u8, default: bool) bool {
    std.debug.assert(lookup(key) != null);
    return (parseBool(key, env.get(key)) catch {
        log.err("{f} — keeping the default ({})", .{ last_error, default });
        return default;
    }) orelse default;
}

pub fn intKnob(comptime T: type, env: *const std.process.Environ.Map, key: []const u8, default: T) T {
    std.debug.assert(lookup(key) != null);
    return (parseInt(T, key, env.get(key)) catch {
        log.err("{f} — keeping the default ({d})", .{ last_error, default });
        return default;
    }) orelse default;
}

pub fn optIntKnob(comptime T: type, env: *const std.process.Environ.Map, key: []const u8) ?T {
    std.debug.assert(lookup(key) != null);
    return parseInt(T, key, env.get(key)) catch {
        log.err("{f} — ignoring the override", .{last_error});
        return null;
    };
}

pub fn enumKnob(env: *const std.process.Environ.Map, key: []const u8, allowed: []const []const u8, default: []const u8) []const u8 {
    std.debug.assert(lookup(key) != null);
    return (parseEnum(key, env.get(key), allowed) catch {
        log.err("{f} — keeping the default ({s})", .{ last_error, default });
        return default;
    }) orelse default;
}

// ---------------------------------------------------------------------------
// Tests: the contract of the convention.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "absent: unset, empty, whitespace-only are all absent; values are trimmed" {
    try testing.expectEqual(@as(?[]const u8, null), present(null));
    try testing.expectEqual(@as(?[]const u8, null), present(""));
    try testing.expectEqual(@as(?[]const u8, null), present("   "));
    try testing.expectEqual(@as(?[]const u8, null), present("\t\n"));
    try testing.expectEqualStrings("7", present(" 7 ").?);
    try testing.expectEqual(@as(?bool, null), try parseBool("SQUEEZEFS_X", "  "));
    try testing.expectEqual(@as(?i64, null), try parseIntIn(i64, "SQUEEZEFS_X", "", 0, 10));
}

test "booleans: all eight spellings, case-insensitive; =0 disables; junk refuses" {
    for (bool_true) |t| try testing.expectEqual(@as(?bool, true), try parseBool("SQUEEZEFS_X", t));
    for (bool_false) |f| try testing.expectEqual(@as(?bool, false), try parseBool("SQUEEZEFS_X", f));
    try testing.expectEqual(@as(?bool, true), try parseBool("SQUEEZEFS_X", "YES"));
    try testing.expectEqual(@as(?bool, true), try parseBool("SQUEEZEFS_X", "On"));
    try testing.expectEqual(@as(?bool, false), try parseBool("SQUEEZEFS_X", "FALSE"));
    try testing.expectError(error.Knob, parseBool("SQUEEZEFS_X", "yess"));
    try testing.expectEqualStrings(bool_want, lastError().want);
    try testing.expectEqualStrings("yess", lastError().raw);
}

test "integers: range inclusive; out-of-range refuses naming the range, never clamps" {
    try testing.expectEqual(@as(?u32, 1), try parseIntIn(u32, "SQUEEZEFS_X", "1", 1, 32));
    try testing.expectEqual(@as(?u32, 32), try parseIntIn(u32, "SQUEEZEFS_X", "32", 1, 32));
    try testing.expectError(error.Knob, parseIntIn(u32, "SQUEEZEFS_X", "64", 1, 32));
    try testing.expectEqualStrings("expected an integer in 1..=32", lastError().want);
    try testing.expectError(error.Knob, parseIntIn(u32, "SQUEEZEFS_X", "0", 1, 32));
    try testing.expectError(error.Knob, parseIntIn(u32, "SQUEEZEFS_X", "4 2", 1, 32));
    try testing.expectError(error.Knob, parseIntIn(u32, "SQUEEZEFS_X", "1e6", 1, 32));
    try testing.expectError(error.Knob, parseIntIn(u32, "SQUEEZEFS_X", "5s", 1, 600_000));
    try testing.expectError(error.Knob, parseInt(u64, "SQUEEZEFS_X", "-1"));
    try testing.expectError(error.Knob, parseIntIn(i128, "SQUEEZEFS_X", "12.5", 1, 100));
}

test "enums: case-insensitive, canonical spelling returned; miss names the set" {
    const allowed = [_][]const u8{ "first-touch", "second-touch" };
    try testing.expectEqualStrings("second-touch", (try parseEnum("SQUEEZEFS_X", "SECOND-TOUCH", &allowed)).?);
    try testing.expectError(error.Knob, parseEnum("SQUEEZEFS_X", "third", &allowed));
    try testing.expectEqualStrings("expected one of first-touch|second-touch", lastError().want);
}

test "KnobError formats as key='raw' is invalid: want" {
    const e = KnobError{ .key = "SQUEEZEFS_X", .raw = "zz", .want = "expected an integer in 1..=4" };
    var buf: [128]u8 = undefined;
    const s = try std.fmt.bufPrint(&buf, "{f}", .{e});
    try testing.expectEqualStrings("SQUEEZEFS_X='zz' is invalid: expected an integer in 1..=4", s);
}

test "registry hygiene: unique keys, non-empty default and doc, our namespace" {
    for (registry, 0..) |a, i| {
        try testing.expect(isOurs(a.key));
        try testing.expect(a.default.len > 0);
        try testing.expect(a.doc.len > 10);
        for (registry[i + 1 ..]) |b| try testing.expect(!std.mem.eql(u8, a.key, b.key));
    }
}

test "registry collision law: a live key that strictly prefixes another must be a Bool switch" {
    for (registry) |a| {
        switch (a.kind) {
            .retired, .harness => continue,
            else => {},
        }
        for (registry) |b| {
            if (a.key.len >= b.key.len) continue;
            if (!std.mem.startsWith(u8, b.key, a.key)) continue;
            switch (b.kind) {
                .retired, .harness => continue,
                else => {},
            }
            try testing.expect(a.kind == .bool);
        }
    }
}

test "gate: every offender at once, sorted; unknown announced not refused" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_OP_PROFILE", "yess");
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "65");
    try env.put("SQUEEZEFS_TRANSPORT_MEM_PCT", "250");
    try env.put("SQUEEZEFS_NOT_A_KNOB", "1");
    try env.put("PATH", "/usr/bin");
    try env.put("SQUEEZEFS_STATS_KEY_CENSUS", "   ");

    var v = try validateVars(gpa, &env);
    defer v.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), v.errors.items.len);
    try testing.expectEqualStrings("SQUEEZEFS_OP_PROFILE='yess' is invalid: " ++ bool_want, v.errors.items[0]);
    try testing.expectEqualStrings("SQUEEZEFS_TRANSPORT_MEM_PCT='250' is invalid: expected an integer in 1..=100", v.errors.items[1]);
    try testing.expectEqualStrings("SQUEEZEFS_URING_FS_WORKERS='65' is invalid: expected an integer in 1..=64", v.errors.items[2]);
    try testing.expectEqual(@as(usize, 1), v.unknown.items.len);
    try testing.expectEqualStrings("SQUEEZEFS_NOT_A_KNOB", v.unknown.items[0]);
    try testing.expect(!v.isClean());

    var warn_buf: [512]u8 = undefined;
    var warn: std.Io.Writer = .fixed(&warn_buf);
    const report = (try refusalReport(gpa, &env, &warn)).?;
    defer gpa.free(report);
    try testing.expect(std.mem.startsWith(u8, report, "refusing to start: invalid SqueezeFS environment knob(s)\n  - SQUEEZEFS_OP_PROFILE="));
    try testing.expect(std.mem.endsWith(u8, report, "(a malformed knob is never silently defaulted — fix or unset it; empty means unset)"));
    try testing.expect(std.mem.indexOf(u8, warn.buffered(), "Warning: SQUEEZEFS_NOT_A_KNOB is not a SqueezeFS knob (typo?") != null);
}

test "gate: a clean environment yields no report; an unknown alone is not a refusal" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_TYPO_KNOB", "1");
    try env.put("SQUEEZEFS_OP_PROFILE", "on");
    var warn_buf: [256]u8 = undefined;
    var warn: std.Io.Writer = .fixed(&warn_buf);
    try testing.expectEqual(@as(?[]u8, null), try refusalReport(gpa, &env, &warn));
    try testing.expect(warn.buffered().len > 0);
}

test "validateOne: retired knob refuses only when present; retired enum value names why" {
    const gpa = testing.allocator;
    const retired_knob = Knob{ .key = "SQUEEZEFS_D", .kind = .{ .retired = .{ .successor = "SQUEEZEFS_E" } }, .default = "-", .class = .retired, .doc = "retired for the test" };
    try testing.expectEqual(@as(?[]u8, null), try validateOne(gpa, &retired_knob, ""));
    const line = (try validateOne(gpa, &retired_knob, "1")).?;
    defer gpa.free(line);
    try testing.expectEqualStrings("SQUEEZEFS_D='1' was RETIRED (forward-only — never a silent alias): use SQUEEZEFS_E instead", line);

    const allowed = [_][]const u8{ "a", "b" };
    const enum_knob = Knob{ .key = "SQUEEZEFS_N", .kind = .{ .enumeration = .{ .allowed = &allowed, .retired = &.{.{ .value = "legacy", .why = "the legacy arm was deleted" }} } }, .default = "a", .class = .enum_select, .doc = "enum for the test" };
    const l2 = (try validateOne(gpa, &enum_knob, "LEGACY")).?;
    defer gpa.free(l2);
    try testing.expect(std.mem.indexOf(u8, l2, "RETIRED") != null);
    try testing.expect(std.mem.indexOf(u8, l2, "expected one of") == null);
    try testing.expectEqual(@as(?[]u8, null), try validateOne(gpa, &enum_knob, "B"));
}

test "readers: =0 disables a default-ON knob; bogus keeps the default; absent keeps the default" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try testing.expect(boolKnob(&env, "SQUEEZEFS_OP_PROFILE", true));
    for (bool_false) |f| {
        try env.put("SQUEEZEFS_OP_PROFILE", f);
        try testing.expect(!boolKnob(&env, "SQUEEZEFS_OP_PROFILE", true));
    }
    for (bool_true) |t| {
        try env.put("SQUEEZEFS_OP_PROFILE", t);
        try testing.expect(boolKnob(&env, "SQUEEZEFS_OP_PROFILE", false));
    }
    try env.put("SQUEEZEFS_OP_PROFILE", "bogus");
    log.testing.expectErrorLogs(1);
    try testing.expect(boolKnob(&env, "SQUEEZEFS_OP_PROFILE", true));
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(?u32, null), optIntKnob(u32, &env, "SQUEEZEFS_URING_FS_WORKERS"));
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "12");
    try testing.expectEqual(@as(?u32, 12), optIntKnob(u32, &env, "SQUEEZEFS_URING_FS_WORKERS"));
    try testing.expectEqual(@as(u32, 12), intKnob(u32, &env, "SQUEEZEFS_URING_FS_WORKERS", 4));
}
