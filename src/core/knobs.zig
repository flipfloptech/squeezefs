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

/// The charter's classes (§1.6: delete / `SQUEEZEFS_TEST_*` seam / A/B
/// lever) plus the sizing and bookkeeping shapes. There is no "tuning" class:
/// a literal shipped default with a knob on top is a free-floating constant
/// (§1.5) — such a row is an A/B lever whose doc names the control arm, or it
/// is deleted (decision 0003 Q7).
pub const Class = enum {
    /// A real feature/posture toggle.
    switch_,
    /// Int whose absence means "derive it"; explicit wins verbatim.
    derived_override,
    /// Product code armed only by suites (`SQUEEZEFS_TEST_*`); default-inert.
    test_seam,
    /// Same-binary measurement control arm; the doc names the arm; logged at
    /// mount when non-default.
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
    /// The default: "derived" (absent ⇒ derive; read through `optIntKnob`),
    /// a literal the typed readers parse at comptime (`intKnob`, `boolKnob`,
    /// `enumKnob` — the ONE source of the default, never a call-site
    /// literal), or "-" (no default: harness, retired).
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
    // (`SQUEEZEFS_OP_PROFILE` and `SQUEEZEFS_STATS_KEY_CENSUS` land with the
    // `.stats` inode in rung 5 — a row nothing reads is dead code.)

    // ---- R5 memory budget ----
    .{ .key = "SQUEEZEFS_MEM_BUDGET_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "R5 memory budget, MiB (absolute > cgroup memory.max × 0.8 > 70 % RAM)." },
    .{ .key = "SQUEEZEFS_TRANSPORT_MEM_MAX", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "Transport payload-buffer cap, MiB (absolute > pct > derived budget/8)." },
    .{ .key = "SQUEEZEFS_TRANSPORT_MEM_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "derived", .class = .derived_override, .doc = "Transport payload-buffer cap as a percentage of the R5 budget." },

    // ---- io_uring file layer / metadata device posture ----
    .{ .key = "SQUEEZEFS_URING_FS_WORKERS", .kind = .{ .int = .{ .lo = 1, .hi = 64 } }, .default = "derived", .class = .derived_override, .doc = "io_uring file-layer worker count for ad-hoc local file I/O (derived clamp(cpus/4, 4, 64))." },
    .{ .key = "SQUEEZEFS_TEST_META_BUFFERED", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: keep every registered metadata device on BUFFERED I/O (the shared-LUN coherence repro's control arm)." },

    // ---- metadata commit pipeline (journal lane) ----
    .{ .key = "SQUEEZEFS_META_FLUSH_INTERVAL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "50", .class = .ab_lever, .doc = "Coalesced journal barrier period, ms (the durability lane sets needs_flush and barriers at this cadence). A/B lever: 0 is the strict control arm — ONE barrier per group before any ack." },

    // ---- NVMe block device workers ----
    .{ .key = "SQUEEZEFS_NVME_READ_LANES", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "derived", .class = .ab_lever, .doc = "Per-device READ submission fan-out lanes (derived cpus ÷ data devices). A/B lever: 1 is the single-submitter control arm (11.5 GB/s vs 16.7 GB/s at 4 lanes on the 8-namespace box)." },
    .{ .key = "SQUEEZEFS_NVME_WRITE_LANES", .kind = .{ .int = .{ .lo = 1, .hi = 1024 } }, .default = "derived", .class = .ab_lever, .doc = "Per-device DATA-WRITE submission fan-out lanes, block-offset affinity (derived cpus ÷ data devices). A/B lever: 1 is the single-submitter control arm." },

    // ---- R5 fleet share + IPC arena sizing (read by mem/) ----
    .{ .key = "SQUEEZEFS_TEST_FLEET_SHARE", .kind = .{ .int = .{ .lo = 1, .hi = 4096 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: pin the fleet share — N co-located daemons sharing one machine — instead of the per-tick census of squeezefs daemons in this process's cgroup (`cgroup.procs` ∩ comm). The share divides the cgroup/RAM roots, the CPU root and the cgroup unreclaimable sample (ceil) BEFORE the ×0.8/×0.7 laws; explicit budgets and floors are never divided." },
    .{ .key = "SQUEEZEFS_IPC_MEM_MAX", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "ipc_session_arenas admission cap, MiB (absolute > pct > derived budget/8)." },
    .{ .key = "SQUEEZEFS_IPC_MEM_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "derived", .class = .derived_override, .doc = "ipc_session_arenas admission cap as a percentage of the R5 budget." },
    .{ .key = "SQUEEZEFS_IPC_ARENA_MB", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 20 } }, .default = "derived", .class = .derived_override, .doc = "Per-session IPC payload arena, MiB (derived max(64 MiB, align_down(ipc cap / session population target, 4 MiB)))." },

    // ---- 4a metadata object locks (read by meta/dlm.zig) ----
    .{ .key = "SQUEEZEFS_DLM_STRIPES", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 24 } }, .default = "derived", .class = .ab_lever, .doc = "D-3: the stripe population of the 4a DlmLockManager inode + dentry classes (shipped floor 4096). Explicit wins verbatim (rounded UP to a power of two — the index is a mask); derived = next_pow2(max(4096, 16 × possible_cpus × q_depth)) so a false-sharing wait — which on the 4a tables costs a whole commit — hits at most one acquire in 16 at full ring occupancy. A/B lever: 4096 is the shipped-4a control arm, 1 the everything-serializes crucible. Engagement: dlm_{inode,dentry}_stripes; conviction: dlm_*_stripe_collisions vs dlm_*_key_waits." },

    // ---- MetaLV node cache (read by meta/node_cache.zig) ----
    .{ .key = "SQUEEZEFS_META_NODE_CACHE_MB", .kind = .{ .int = .{ .lo = 1, .hi = MIB_MAX } }, .default = "derived", .class = .derived_override, .doc = "Per-volume node-cache budget, MiB, verbatim (absolute > SQUEEZEFS_META_NODE_CACHE_PCT > derived max(mem_budget/16, 512 MiB))." },
    .{ .key = "SQUEEZEFS_META_NODE_CACHE_PCT", .kind = .{ .int = .{ .lo = 1, .hi = 100 } }, .default = "derived", .class = .derived_override, .doc = "Per-volume node-cache budget as a percentage of the R5 memory budget (below the absolute MiB knob, above the derived default)." },

    // ---- MetaLV checkpoint task (read by meta/checkpoint.zig) ----
    .{ .key = "SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 32 } }, .default = "derived", .class = .derived_override, .doc = "Dirty-node checkpoint cap = the mount-replay working-set bound: a cycle is due once more nodes than this hold un-durable records. Explicit wins verbatim; derived max(4096, mem_budget/32 ÷ node_size) — bounds the RAM pinned by dirty nodes AND the replay working set, 4096 the shipped floor (never regress below shipped: a lower cap checkpoints more often than any box ran, pure overhead)." },
    .{ .key = "SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", .kind = .{ .int = .{ .lo = 1, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Checkpoint cadence ceiling, ms: a cycle runs at least this often with no other trigger (charter §5.3 'every ≤ 1 s'); also the reader's staleness term and the base of the landing ceiling max_age + 2 × tick (KD-SYM-10, 1,100 ms shipped). A/B lever: 1000 is the charter's shipped arm; a shorter age is the replay-bound crucible (tighter mount replay for more checkpoint I/O). Floor 1 ms = the physical minimum of a cadence; ceiling the timing-knob bound." },

    // ---- the commit conveyor (read by meta/conveyor.zig, meta/backend.zig) ----
    .{ .key = "SQUEEZEFS_META_COMMIT_BATCH_TXS", .kind = .{ .int = .{ .lo = 1, .hi = 1 << 20 } }, .default = "derived", .class = .ab_lever, .doc = "M7 commit-conveyor batch cap, transactions per drained batch. Explicit wins verbatim; derived max(64, cpus × 2) — arrivals scale with handler parallelism, 64 the shipped M7 floor. A/B lever: 1 is the per-tx pre-conveyor control arm." },
    .{ .key = "SQUEEZEFS_META_COMMIT_BATCH_BYTES", .kind = .{ .int = .{ .lo = 1, .hi = BYTES_MAX } }, .default = "derived", .class = .ab_lever, .doc = "M7 commit-conveyor batch cap, Σ entry bytes per drained batch. Explicit wins verbatim; derived max(256 KiB, ring/16) so ≥ 16 batch reservations always cycle the ring; both forms are clamped at open to the ring's user-admissible capacity so one Σ-admission can always eventually succeed. A/B lever: the byte half of the batch-vs-per-tx arm (`_TXS = 1`); a byte cap never splits a single transaction." },
    .{ .key = "SQUEEZEFS_TEST_CRASH_CHILD", .kind = .harness, .default = "-", .class = .harness, .doc = "Harness: the kill -9 matrix's re-exec'd child marker — the metadata volume path the child opens and churns (its ack ledger is `<path>.acks`); the parent test SIGKILLs it mid-stream and reopens." },

    // ---- the FUSE library: transport geometry (read by fuse/geometry.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_MAX_WRITE", .kind = .{ .int = .{ .lo = 4096, .hi = std.math.maxInt(u32) } }, .default = "derived", .class = .derived_override, .doc = "INIT max_write DESIRE, bytes, verbatim (derived max(block_size, 1 MiB)); the plan still clamps it to the kernel's max_pages_limit sysctl and degrades it under the payload-buffer cap, so the published transport_max_write is what the kernel runs." },
    .{ .key = "SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH", .kind = .{ .int = .{ .lo = 1, .hi = 32 } }, .default = "derived", .class = .derived_override, .doc = "FUSE-over-io_uring per-queue ring depth, verbatim (derived clamp(cap / (nqueues × payload_sz), 4, 32): 32 the measured-best, 4 the shipped floor); explicit bypasses the budget ladder. Ceiling 32 = the kernel's per-queue entry population the measured-best posture was taken at." },
    .{ .key = "SQUEEZEFS_TEST_FUSE_QUEUES", .kind = .{ .int = .{ .lo = 1, .hi = 512 } }, .default = "derived", .class = .test_seam, .doc = "Test seam: FUSE-over-io_uring queue count instead of the kernel's possible CPUs. Fewer queues than possible CPUs never becomes ready (the kernel routes to the ring only once every queue holds an entry) — a product mount never sets it." },
    .{ .key = "SQUEEZEFS_FUSE_DRAIN_GROUP", .kind = .{ .int = .{ .lo = 1, .hi = 512 } }, .default = "derived", .class = .derived_override, .doc = "Drain-group width (member queues per sqz-fuse-ur worker), verbatim within a NUMA node — node containment is structural, an explicit width never spans a node (derived max(node_cpus / 4, 1); 1 under kmbuf/zc where registration is per ring per queue)." },

    // ---- the FUSE library: kernel TTL classes (read by fuse/mount_options.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_ATTR_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel attribute-cache TTL, ms (GETATTR/SETATTR replies and the attr half of every entry reply); `-o attr_timeout=` overrides it. A/B lever: 1000 is the shipped arm; 0 the no-cache control arm (every stat reaches the daemon)." },
    .{ .key = "SQUEEZEFS_FUSE_ENTRY_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel dentry TTL for non-directory entries, ms; `-o entry_timeout=` overrides it. A/B lever: 1000 is the shipped arm; 0 the no-cache control arm (every path walk reaches the daemon)." },
    .{ .key = "SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel dentry TTL for directory entries, ms (a directory dentry's invalidation covers a subtree, hence its own class); `-o dir_entry_timeout=` overrides it. A/B lever: 1000 is the shipped arm; 0 the no-cache control arm." },
    .{ .key = "SQUEEZEFS_FUSE_NEGATIVE_TTL_MS", .kind = .{ .int = .{ .lo = 0, .hi = MS_MAX } }, .default = "1000", .class = .ab_lever, .doc = "Kernel negative-dentry TTL, ms: a LOOKUP miss replies a cacheable negative entry (nodeid 0 + entry_valid) for this long; 0 restores the bare ENOENT. `-o negative_timeout=` overrides it. A/B lever: 1000 is the shipped arm; 0 the bare-ENOENT control arm." },

    // ---- the executor (read by exec/placement.zig, exec/sqz_io.zig, exec/lane.zig) ----
    .{ .key = "SQUEEZEFS_FUSE_PIN_SCOPE", .kind = .{ .enumeration = .{ .allowed = &.{ "node", "core" } } }, .default = "node", .class = .enum_select, .doc = "Handler-lane and transport-worker affinity scope: `node` (the lane may run on any CPU of its home NUMA node — the serve-latency decomposition convicted the 1-CPU pin: queue_wait 1.55 + dispatch_lag 1.70 ms/op on cold reads, ~99 % collapse with widening) or `core` (the pre-campaign hard pin, the control arm of that measurement)." },
    .{ .key = "SQUEEZEFS_NUMA", .kind = .bool, .default = "on", .class = .ab_lever, .doc = "NUMA placement actions (lane affinity by node, node-grouped dispatch). A/B lever: 0 makes no affinity call and ignores node groups while the node map and gauges stay live — the single-node-posture control arm; structurally inert on single-node hosts." },
    .{ .key = "SQUEEZEFS_FUSE_SAME_LANE_DISPATCH", .kind = .bool, .default = "1", .class = .ab_lever, .doc = "A spawn issued from a lane thread lands on THAT lane instead of the fuse-lane rotation (deletes one cross-thread wake per op; the rotation measured 412 µs dispatch_lag at 256 in flight). A/B lever: 0 is the rotation control arm." },
    .{ .key = "SQUEEZEFS_TEST_EXEC_DROP_WAKE", .kind = .bool, .default = "0", .class = .test_seam, .doc = "Test seam: every lane drops the FIRST cross-thread eventfd wake write after it starts, so the park tick's lost-wake rescue (exec_lane_tick_rescues) is exercised deterministically." },
};

pub fn lookup(key: []const u8) ?*const Knob {
    for (&registry) |*k| if (std.mem.eql(u8, k.key, key)) return k;
    return null;
}

/// The row of a key known at compile time: an unregistered key is a compile
/// error, so a reader cannot exist without its row.
pub fn row(comptime key: []const u8) *const Knob {
    return comptime lookup(key) orelse @compileError(key ++ " is not a registry row (src/core/knobs.zig)");
}

/// Is this name in our namespace? (`SQUEEZEFS_` or `SQZ_` prefix.)
pub fn isOurs(key: []const u8) bool {
    return std.mem.startsWith(u8, key, "SQUEEZEFS_") or std.mem.startsWith(u8, key, "SQZ_");
}

// ---------------------------------------------------------------------------
// The ONE parser. Pure: no logging, no env reads, no syscalls, no side
// channel — a parse yields a VALUE (`Parsed(T)`), and the detail of a
// refusal travels inside it as a `Diagnostic`.
// ---------------------------------------------------------------------------

/// What an admissible value would have looked like; formats on demand.
pub const Want = union(enum) {
    boolean,
    integer,
    int_range: struct { lo: i128, hi: i128 },
    enum_set: []const []const u8,

    pub fn format(self: Want, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .boolean => try w.writeAll(bool_want),
            .integer => try w.writeAll("expected an integer"),
            .int_range => |r| try w.print("expected an integer in {d}..={d}", .{ r.lo, r.hi }),
            .enum_set => |allowed| {
                try w.writeAll("expected one of ");
                for (allowed, 0..) |a, i| {
                    if (i != 0) try w.writeByte('|');
                    try w.writeAll(a);
                }
            },
        }
    }
};

/// The refusal detail: the key, the TRIMMED raw value, what was wanted.
pub const Diagnostic = struct {
    key: []const u8,
    raw: []const u8,
    want: Want,

    pub fn format(self: Diagnostic, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{s}='{s}' is invalid: {f}", .{ self.key, self.raw, self.want });
    }
};

/// A parse outcome: absent, a value, or a refusal with its diagnostic.
pub fn Parsed(comptime T: type) type {
    return union(enum) {
        absent,
        value: T,
        invalid: Diagnostic,
    };
}

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

pub fn parseBool(key: []const u8, raw: ?[]const u8) Parsed(bool) {
    const v = present(raw) orelse return .absent;
    for (bool_true) |t| if (eqlIgnoreCase(v, t)) return .{ .value = true };
    for (bool_false) |f| if (eqlIgnoreCase(v, f)) return .{ .value = false };
    return .{ .invalid = .{ .key = key, .raw = v, .want = .boolean } };
}

/// Inclusive range. Parse failure AND out-of-range share ONE message naming
/// the admissible range. Out-of-range REFUSES, never clamps.
pub fn parseIntIn(comptime T: type, key: []const u8, raw: ?[]const u8, lo: T, hi: T) Parsed(T) {
    const v = present(raw) orelse return .absent;
    const parsed: ?T = std.fmt.parseInt(T, v, 10) catch null;
    if (parsed) |p| if (p >= lo and p <= hi) return .{ .value = p };
    return .{ .invalid = .{ .key = key, .raw = v, .want = .{ .int_range = .{ .lo = lo, .hi = hi } } } };
}

/// Case-insensitive match returning the CANONICAL spelling from `allowed`.
pub fn parseEnum(key: []const u8, raw: ?[]const u8, allowed: []const []const u8) Parsed([]const u8) {
    const v = present(raw) orelse return .absent;
    for (allowed) |a| if (eqlIgnoreCase(v, a)) return .{ .value = a };
    return .{ .invalid = .{ .key = key, .raw = v, .want = .{ .enum_set = allowed } } };
}

pub fn parseStr(raw: ?[]const u8) ?[]const u8 {
    return present(raw);
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
        .bool => switch (parseBool(knob.key, raw)) {
            .invalid => |d| return try std.fmt.allocPrint(gpa, "{f}", .{d}),
            .absent, .value => {},
        },
        .int => |r| switch (parseIntIn(i128, knob.key, raw, r.lo, r.hi)) {
            .invalid => |d| return try std.fmt.allocPrint(gpa, "{f}", .{d}),
            .absent, .value => {},
        },
        .enumeration => |e| {
            const v = present(raw) orelse return null;
            // A retired VALUE is checked BEFORE the allowed set; its message
            // must not read "expected one of" (it was valid once; it is not an
            // unknown spelling).
            for (e.retired) |rv| if (eqlIgnoreCase(v, rv.value)) {
                return try std.fmt.allocPrint(gpa, "{s}='{s}' was RETIRED (forward-only — never a silent alias): {s}", .{ knob.key, v, rv.why });
            };
            switch (parseEnum(knob.key, raw, e.allowed)) {
                .invalid => |d| return try std.fmt.allocPrint(gpa, "{f}", .{d}),
                .absent, .value => {},
            }
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
// In-process readers. The key is comptime: the row is resolved at compile
// time (an unregistered key cannot compile), the row's range is enforced
// and the row's default is THE default — a call site carries none. They
// never panic and never refuse: the gate already did. A malformed value here
// (only reachable when the gate was bypassed — e.g. a knob set after
// startup) is logged and the default kept.
// ---------------------------------------------------------------------------

fn intRange(comptime key: []const u8, comptime T: type) struct { lo: T, hi: T } {
    const r = comptime row(key);
    comptime if (r.kind != .int) @compileError(key ++ " is not an integer row");
    return comptime .{ .lo = @intCast(r.kind.int.lo), .hi = @intCast(r.kind.int.hi) };
}

/// A Bool row's default (`"0"`/`"1"`/`"on"`/…), typed at compile time.
pub fn boolDefault(comptime key: []const u8) bool {
    const r = comptime row(key);
    comptime if (r.kind != .bool) @compileError(key ++ " is not a Bool row");
    return comptime switch (parseBool(key, r.default)) {
        .value => |v| v,
        else => @compileError(key ++ " has no literal boolean default"),
    };
}

/// An Int row's literal default, typed and range-checked at compile time; a
/// "derived" row has none (read it through `optIntKnob`).
pub fn intDefault(comptime key: []const u8, comptime T: type) T {
    const r = comptime row(key);
    const range = comptime intRange(key, T);
    return comptime switch (parseIntIn(T, key, r.default, range.lo, range.hi)) {
        .value => |v| v,
        else => @compileError(key ++ " has no literal integer default inside its range — derived rows are read with optIntKnob"),
    };
}

pub fn boolKnob(comptime key: []const u8, env: *const std.process.Environ.Map) bool {
    const default = comptime boolDefault(key);
    return switch (parseBool(key, env.get(key))) {
        .value => |v| v,
        .absent => default,
        .invalid => |d| blk: {
            log.err("{f} — keeping the default ({})", .{ d, default });
            break :blk default;
        },
    };
}

pub fn intKnob(comptime key: []const u8, comptime T: type, env: *const std.process.Environ.Map) T {
    const default = comptime intDefault(key, T);
    const range = comptime intRange(key, T);
    return switch (parseIntIn(T, key, env.get(key), range.lo, range.hi)) {
        .value => |v| v,
        .absent => default,
        .invalid => |d| blk: {
            log.err("{f} — keeping the default ({d})", .{ d, default });
            break :blk default;
        },
    };
}

/// A derived-default Int: null = absent (derive it); an out-of-range or
/// malformed value is logged and ignored (derive it), never clamped.
pub fn optIntKnob(comptime key: []const u8, comptime T: type, env: *const std.process.Environ.Map) ?T {
    const range = comptime intRange(key, T);
    return switch (parseIntIn(T, key, env.get(key), range.lo, range.hi)) {
        .value => |v| v,
        .absent => null,
        .invalid => |d| blk: {
            log.err("{f} — ignoring the override", .{d});
            break :blk null;
        },
    };
}

pub fn enumKnob(comptime key: []const u8, env: *const std.process.Environ.Map) []const u8 {
    const r = comptime row(key);
    comptime if (r.kind != .enumeration) @compileError(key ++ " is not an enumeration row");
    const allowed = r.kind.enumeration.allowed;
    const default = comptime switch (parseEnum(key, r.default, allowed)) {
        .value => |v| v,
        else => @compileError(key ++ " has no default among its allowed values"),
    };
    return switch (parseEnum(key, env.get(key), allowed)) {
        .value => |v| v,
        .absent => default,
        .invalid => |d| blk: {
            log.err("{f} — keeping the default ({s})", .{ d, default });
            break :blk default;
        },
    };
}

// ---------------------------------------------------------------------------
// Tests: the contract of the convention.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn wantOf(comptime T: type, parsed: Parsed(T)) ![]const u8 {
    return switch (parsed) {
        .invalid => |d| std.fmt.bufPrint(&want_buf, "{f}", .{d.want}),
        else => error.TestExpectedInvalid,
    };
}
var want_buf: [160]u8 = undefined;

fn rawOf(comptime T: type, parsed: Parsed(T)) ![]const u8 {
    return switch (parsed) {
        .invalid => |d| d.raw,
        else => error.TestExpectedInvalid,
    };
}

test "absent: unset, empty, whitespace-only are all absent; values are trimmed" {
    try testing.expectEqual(@as(?[]const u8, null), present(null));
    try testing.expectEqual(@as(?[]const u8, null), present(""));
    try testing.expectEqual(@as(?[]const u8, null), present("   "));
    try testing.expectEqual(@as(?[]const u8, null), present("\t\n"));
    try testing.expectEqualStrings("7", present(" 7 ").?);
    try testing.expectEqual(Parsed(bool).absent, parseBool("SQUEEZEFS_X", "  "));
    try testing.expectEqual(Parsed(i64).absent, parseIntIn(i64, "SQUEEZEFS_X", "", 0, 10));
}

test "booleans: all eight spellings, case-insensitive; =0 disables; junk refuses with its diagnostic" {
    for (bool_true) |t| try testing.expectEqual(Parsed(bool){ .value = true }, parseBool("SQUEEZEFS_X", t));
    for (bool_false) |f| try testing.expectEqual(Parsed(bool){ .value = false }, parseBool("SQUEEZEFS_X", f));
    try testing.expectEqual(Parsed(bool){ .value = true }, parseBool("SQUEEZEFS_X", "YES"));
    try testing.expectEqual(Parsed(bool){ .value = true }, parseBool("SQUEEZEFS_X", "On"));
    try testing.expectEqual(Parsed(bool){ .value = false }, parseBool("SQUEEZEFS_X", "FALSE"));
    const bad = parseBool("SQUEEZEFS_X", "yess");
    try testing.expectEqualStrings(bool_want, try wantOf(bool, bad));
    try testing.expectEqualStrings("yess", try rawOf(bool, bad));
}

test "integers: range inclusive; out-of-range refuses naming the range, never clamps" {
    try testing.expectEqual(Parsed(u32){ .value = 1 }, parseIntIn(u32, "SQUEEZEFS_X", "1", 1, 32));
    try testing.expectEqual(Parsed(u32){ .value = 32 }, parseIntIn(u32, "SQUEEZEFS_X", "32", 1, 32));
    try testing.expectEqualStrings("expected an integer in 1..=32", try wantOf(u32, parseIntIn(u32, "SQUEEZEFS_X", "64", 1, 32)));
    try testing.expect(parseIntIn(u32, "SQUEEZEFS_X", "0", 1, 32) == .invalid);
    try testing.expect(parseIntIn(u32, "SQUEEZEFS_X", "4 2", 1, 32) == .invalid);
    try testing.expect(parseIntIn(u32, "SQUEEZEFS_X", "1e6", 1, 32) == .invalid);
    try testing.expect(parseIntIn(u32, "SQUEEZEFS_X", "5s", 1, 600_000) == .invalid);
    try testing.expect(parseIntIn(u64, "SQUEEZEFS_X", "-1", 0, 10) == .invalid);
    // A percentage knob refuses a fraction and refuses > 100: never clamped, never parsed as a float.
    try testing.expect(parseIntIn(i128, "SQUEEZEFS_X", "12.5", 1, 100) == .invalid);
    try testing.expectEqualStrings("expected an integer in 1..=100", try wantOf(i128, parseIntIn(i128, "SQUEEZEFS_X", "250", 1, 100)));
}

test "enums: case-insensitive, canonical spelling returned; miss names the set" {
    const allowed = [_][]const u8{ "first-touch", "second-touch" };
    try testing.expectEqualStrings("second-touch", parseEnum("SQUEEZEFS_X", "SECOND-TOUCH", &allowed).value);
    try testing.expectEqualStrings("expected one of first-touch|second-touch", try wantOf([]const u8, parseEnum("SQUEEZEFS_X", "third", &allowed)));
}

test "Diagnostic formats as key='raw' is invalid: want — a value, no thread-local, two diagnostics coexist" {
    const a = Diagnostic{ .key = "SQUEEZEFS_X", .raw = "zz", .want = .{ .int_range = .{ .lo = 1, .hi = 4 } } };
    const b = Diagnostic{ .key = "SQUEEZEFS_N", .raw = "q", .want = .boolean };
    var buf: [256]u8 = undefined;
    try testing.expectEqualStrings("SQUEEZEFS_X='zz' is invalid: expected an integer in 1..=4", try std.fmt.bufPrint(&buf, "{f}", .{a}));
    try testing.expectEqualStrings("SQUEEZEFS_N='q' is invalid: " ++ bool_want, try std.fmt.bufPrint(&buf, "{f}", .{b}));
    // The first diagnostic is intact after the second was formatted.
    try testing.expectEqualStrings("SQUEEZEFS_X='zz' is invalid: expected an integer in 1..=4", try std.fmt.bufPrint(&buf, "{f}", .{a}));
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
    try env.put("SQUEEZEFS_TEST_META_BUFFERED", "yess");
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "65");
    try env.put("SQUEEZEFS_TRANSPORT_MEM_PCT", "250");
    try env.put("SQUEEZEFS_NOT_A_KNOB", "1");
    try env.put("PATH", "/usr/bin");
    try env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "   ");

    var v = try validateVars(gpa, &env);
    defer v.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), v.errors.items.len);
    try testing.expectEqualStrings("SQUEEZEFS_TEST_META_BUFFERED='yess' is invalid: " ++ bool_want, v.errors.items[0]);
    try testing.expectEqualStrings("SQUEEZEFS_TRANSPORT_MEM_PCT='250' is invalid: expected an integer in 1..=100", v.errors.items[1]);
    try testing.expectEqualStrings("SQUEEZEFS_URING_FS_WORKERS='65' is invalid: expected an integer in 1..=64", v.errors.items[2]);
    try testing.expectEqual(@as(usize, 1), v.unknown.items.len);
    try testing.expectEqualStrings("SQUEEZEFS_NOT_A_KNOB", v.unknown.items[0]);
    try testing.expect(!v.isClean());

    var warn_buf: [512]u8 = undefined;
    var warn: std.Io.Writer = .fixed(&warn_buf);
    const report = (try refusalReport(gpa, &env, &warn)).?;
    defer gpa.free(report);
    try testing.expect(std.mem.startsWith(u8, report, "refusing to start: invalid SqueezeFS environment knob(s)\n  - SQUEEZEFS_TEST_META_BUFFERED="));
    try testing.expect(std.mem.endsWith(u8, report, "(a malformed knob is never silently defaulted — fix or unset it; empty means unset)"));
    try testing.expect(std.mem.indexOf(u8, warn.buffered(), "Warning: SQUEEZEFS_NOT_A_KNOB is not a SqueezeFS knob (typo?") != null);
}

test "gate: a clean environment yields no report; an unknown alone is not a refusal" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_TYPO_KNOB", "1");
    try env.put("SQUEEZEFS_TEST_META_BUFFERED", "on");
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

test "readers: the row is the default and the range; =0 disables a default-ON knob; bogus keeps the default loudly; out-of-range is ignored, never clamped" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    // The Bool row's default comes from the row ("0").
    try testing.expect(!boolKnob("SQUEEZEFS_TEST_META_BUFFERED", &env));
    for (bool_true) |t| {
        try env.put("SQUEEZEFS_TEST_META_BUFFERED", t);
        try testing.expect(boolKnob("SQUEEZEFS_TEST_META_BUFFERED", &env));
    }
    for (bool_false) |f| {
        try env.put("SQUEEZEFS_TEST_META_BUFFERED", f);
        try testing.expect(!boolKnob("SQUEEZEFS_TEST_META_BUFFERED", &env));
    }
    try env.put("SQUEEZEFS_TEST_META_BUFFERED", "bogus");
    log.testing.expectErrorLogs(1);
    try testing.expect(!boolKnob("SQUEEZEFS_TEST_META_BUFFERED", &env));
    try log.testing.expectAllFired();
    // A derived row: absent is null; the row's range (1..=64) is enforced.
    try testing.expectEqual(@as(?u32, null), optIntKnob("SQUEEZEFS_URING_FS_WORKERS", u32, &env));
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "12");
    try testing.expectEqual(@as(?u32, 12), optIntKnob("SQUEEZEFS_URING_FS_WORKERS", u32, &env));
    try env.put("SQUEEZEFS_URING_FS_WORKERS", "65");
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(?u32, null), optIntKnob("SQUEEZEFS_URING_FS_WORKERS", u32, &env));
    try log.testing.expectAllFired();
    // A literal-default row: the row's "1000" is THE default; out-of-range keeps it.
    try testing.expectEqual(@as(u64, 1000), intDefault("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", u64));
    try testing.expectEqual(@as(u64, 1000), intKnob("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", u64, &env));
    try env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "500");
    try testing.expectEqual(@as(u64, 500), intKnob("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", u64, &env));
    try env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "0");
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(u64, 1000), intKnob("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", u64, &env));
    try log.testing.expectAllFired();
    try testing.expect(!boolDefault("SQUEEZEFS_TEST_META_BUFFERED"));
}

test "classes: every row is one of the charter's shapes; a seam is named SQUEEZEFS_TEST_ and default-inert; every A/B lever names its arm" {
    for (registry) |k| {
        const is_seam = std.mem.startsWith(u8, k.key, "SQUEEZEFS_TEST_");
        switch (k.class) {
            .test_seam => {
                try testing.expect(is_seam);
                // Default-inert: absent derives, or the off/zero arm.
                try testing.expect(std.mem.eql(u8, k.default, "derived") or std.mem.eql(u8, k.default, "0") or std.mem.eql(u8, k.default, "off"));
            },
            .ab_lever => try testing.expect(std.mem.indexOf(u8, k.doc, "A/B lever:") != null),
            .harness => try testing.expect(is_seam and k.kind == .harness),
            .derived_override => {
                try testing.expectEqualStrings("derived", k.default);
                try testing.expect(k.kind == .int);
            },
            .switch_, .config, .enum_select, .retired => {},
        }
    }
}
