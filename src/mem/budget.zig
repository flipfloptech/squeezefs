//! R5 memory-budget authority (charter §6.2 "Memory budget authority (R5)";
//! porting spec `docs/porting/mem_budget.md`).
//!
//! One budget, one pressure signal, three levels. RAM consumers register a
//! `Component` {gauge, floor, weight, shed} into an append-only registry; a
//! 1 Hz sampler resolves the budget, samples RSS and the cgroup-v2
//! unreclaimable set into two 5-slot max-of-window buffers, computes
//! `pressure = max(Σ non-reclaimable gauges, rss_max, unreclaim_max)`, runs
//! the hysteresis level machine, and on Red ticks sheds ∝ weight over floors.
//! Enforcement is advisory-at-admission: growth paths call `level()` (one
//! relaxed load) and apply their own response.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const thread_name = @import("../core/thread_name.zig");

/// The budget-derived caps (transport, IPC, dentry) and the fleet-share
/// floor refusal (spec §4 helpers).
pub const sizing = @import("sizing.zig");

const MiB = derive.MiB;
const GiB = derive.GiB;

// ---------------------------------------------------------------------------
// Constants (spec §7), each with its WHY.
// ---------------------------------------------------------------------------

/// Level edges, percent of budget. 4-point enter/exit gaps so a pressure
/// hovering AT a boundary cannot flap the level (R-7).
pub const yellow_enter_pct: u64 = 80;
pub const yellow_exit_pct: u64 = 76;
pub const red_enter_pct: u64 = 95;
pub const red_exit_pct: u64 = 91;
/// Red shed target: mid-Yellow — a target below the Yellow-exit edge would
/// oscillate the level itself.
pub const shed_target_pct: u64 = 85;
/// Floors may claim at most this share of the budget before the
/// proportional clamp engages.
pub const floor_cap_pct: u64 = 90;
/// Window length in 1 Hz samples — the decay horizon (5 s). Max over the
/// window: a spike ages out on the 5th subsequent tick; explicitly NOT a
/// ratchet.
pub const window_len = 5;
/// One or two over-budget ticks is a spike the normal Red response absorbs;
/// three is a convergence failure.
pub const backstop_sustain_ticks: u64 = 3;
/// Re-read the cgroup cage within a second; × `window_len` = the 5 s decay.
pub const sampler_period_ns: u64 = std.time.ns_per_s;
/// The sampler's comm name ("other" CPU class, like the reference's
/// blocking-pool thread).
pub const sampler_thread_name = "sqz-membudget";
/// Registry capacity: the production registry (spec §2.5) is ~30 components;
/// 64 is the next power of two with headroom. Overflow fires `registry_full`
/// and drops the component — never a panic at mount.
pub const max_components = 64;

/// Fired when a 65th component registers: the registry is sized to the
/// production census, so this is a porting error, not a runtime condition.
pub var registry_full: counters.Tripwire = .init("mem_budget_registry_full");

/// The idle-trim law (charter §6.2 "memory returns under pressure"; the
/// reference's lever here was jemalloc's `arena.4096.purge` on Red ticks —
/// this port's pools hold their idle memory explicitly, so each returns it
/// itself). The idle clock is `ticks`, bumped at the start of every tick;
/// a pool stamps an entry with the clock at its release, so `now − stamp`
/// = 1 means "freed somewhere inside the last period" and 2 "free for at
/// least one FULL period". On a YELLOW tick every trimmable component
/// returns the entries idle ≥ 2 (returning what the live workload touched
/// inside the last second would only refault it under "freeze growth");
/// on a RED tick everything idle goes (min 0), after the shed pass so the
/// entries the shed just evicted return in the same tick.
pub const trim_yellow_min_idle_ticks: u64 = 2;
pub const trim_red_min_idle_ticks: u64 = 0;

pub const Level = enum(u8) {
    green = 0,
    yellow = 1,
    red = 2,

    /// 2 ⇒ Red, 1 ⇒ Yellow, anything else ⇒ Green.
    pub fn fromU8(v: u8) Level {
        return switch (v) {
            2 => .red,
            1 => .yellow,
            else => .green,
        };
    }
};

// ---------------------------------------------------------------------------
// §1 Budget derivation (pure).
// ---------------------------------------------------------------------------

/// flag → env → cgroup `memory.max` / 10 × 8 → RAM / 10 × 7. Flag and env
/// are the explicit tiers (verbatim); cgroup and RAM are the derived tier.
pub fn resolveBudgetFrom(flag: ?u64, env_bytes: ?u64, cgroup_max: ?u64, ram: u64) u64 {
    if (flag) |f| return f;
    if (env_bytes) |e| return e;
    if (cgroup_max) |c| return derive.tenths(c, 8);
    return derive.tenths(ram, 7);
}

/// `ceil(value / max(share, 1))`: ONE divisor at the system roots so N
/// co-located daemons split the machine; share ≤ 1 is the identity. Written
/// without `value + d - 1` so `u64::MAX` cannot overflow.
pub fn fleetSharedRoot(value: u64, share: u32) u64 {
    const d: u64 = @max(share, 1);
    return value / d + @intFromBool(value % d != 0);
}

/// The fleet-share form: the SYSTEM roots (cgroup cage, RAM) are divided
/// BEFORE the ×0.8/×0.7 laws; flag/env pass through undivided.
pub fn resolveBudgetFromShared(flag: ?u64, env_bytes: ?u64, cgroup_max: ?u64, ram: u64, share: u32) u64 {
    const shared_cgroup: ?u64 = if (cgroup_max) |c| fleetSharedRoot(c, share) else null;
    return resolveBudgetFrom(flag, env_bytes, shared_cgroup, fleetSharedRoot(ram, share));
}

/// The `Facts`-shaped tie: explicit `SQUEEZEFS_MEM_BUDGET_MB` → cgroup × 0.8
/// → 70 % RAM, solo posture.
pub fn resolveBudget(facts: derive.Facts, explicit_mb: ?u64, cgroup_max: ?u64) u64 {
    const env_bytes: ?u64 = if (explicit_mb) |mb| mb *| MiB else null;
    return resolveBudgetFrom(null, env_bytes, cgroup_max, facts.total_ram);
}

pub const TickInputs = struct { budget: u64, rss: u64, unreclaimable: u64 };

/// Per-tick input composition: the cgroup unreclaimable sample is ONE
/// `memory.stat` shared by N co-located daemons, so it enters divided by the
/// share like every system root; RSS is per-process and never divided (the
/// N=32 fleet row: all 32 daemons in Red with hard backstops at 0.5 GB of
/// own ledger each when the whole cage was compared against a 1/32 budget).
pub fn tickInputs(budget: u64, rss: u64, raw_unreclaimable: u64, share: u32) TickInputs {
    return .{ .budget = budget, .rss = rss, .unreclaimable = fleetSharedRoot(raw_unreclaimable, share) };
}

/// Red halves the parked-buffer spill threshold (the never-lossy staging
/// spill engages at half the count); floor 1 so a cap of 1 stays usable.
pub fn effectiveParkedCap(cap: u64, level_now: Level) u64 {
    return if (level_now == .red) @max(cap / 2, 1) else cap;
}

// ---------------------------------------------------------------------------
// Knob faces: the registry rows this module consumes.
// ---------------------------------------------------------------------------

/// The fleet share in force: the `SQUEEZEFS_TEST_FLEET_SHARE` seam when
/// present, else the per-tick census of squeezefs daemons sharing this
/// process's cgroup (`readFleetShare` over `roots`); 1 off-cgroup/off-Linux.
pub fn fleetShare(io: std.Io, env: *const std.process.Environ.Map, roots: CensusRoots) u32 {
    if (knobs.optIntKnob("SQUEEZEFS_TEST_FLEET_SHARE", u32, env)) |pinned| return pinned;
    return readFleetShare(io, roots);
}

/// `SQUEEZEFS_MEM_BUDGET_MB` × 1 MiB; null when absent.
fn envBudgetBytes(env: *const std.process.Environ.Map) ?u64 {
    const mb = knobs.optIntKnob("SQUEEZEFS_MEM_BUDGET_MB", u64, env) orelse return null;
    return mb *| MiB;
}

// ---------------------------------------------------------------------------
// §1.4–1.6 Sampler inputs: pure parsers + the Linux readers.
// ---------------------------------------------------------------------------

/// The kill-relevant subset of cgroup-v2 `memory.stat`: bytes the kernel
/// cannot reclaim without I/O or at all. Clean file cache is deliberately
/// EXCLUDED — counting it would pin healthy warm mounts (5 GiB of clean
/// tier mmap by design) in permanent Red. Lines without two fields are
/// skipped, unknown keys ignored, unparsable values count 0.
pub fn parseUnreclaimableMemoryStat(stat: []const u8) u64 {
    var sum: u64 = 0;
    var lines = std.mem.splitScalar(u8, stat, '\n');
    while (lines.next()) |line| {
        var fields = std.mem.tokenizeAny(u8, line, " \t\r");
        const key = fields.next() orelse continue;
        const val = fields.next() orelse continue;
        if (isUnreclaimableKey(key)) sum +|= std.fmt.parseInt(u64, val, 10) catch 0;
    }
    return sum;
}

fn isUnreclaimableKey(key: []const u8) bool {
    const keys = [_][]const u8{ "anon", "file_dirty", "file_writeback", "shmem", "unevictable", "slab_unreclaimable" };
    for (keys) |k| if (std.mem.eql(u8, k, key)) return true;
    return false;
}

/// The `0::<path>` unified line of `/proc/self/cgroup` (cgroup v2 ONLY: no
/// v1 `memory.limit_in_bytes`, no mountinfo walk); null when absent.
pub fn parseCgroupV2Path(text: []const u8) ?[]const u8 {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        if (std.mem.startsWith(u8, line, "0::")) return std.mem.trim(u8, line[3..], " \t\r");
    }
    return null;
}

/// `memory.max` / `memory.high`: the literal `max` (uncapped at this level)
/// and junk both mean "no cap here".
pub fn parseMemoryMax(text: []const u8) ?u64 {
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (std.mem.eql(u8, t, "max")) return null;
    return std.fmt.parseInt(u64, t, 10) catch null;
}

/// `/proc/self/statm` field 2 (resident pages); 0 on any malformed input.
pub fn parseStatmResidentPages(text: []const u8) u64 {
    var fields = std.mem.tokenizeAny(u8, text, " \t\r\n");
    _ = fields.next() orelse return 0;
    const resident = fields.next() orelse return 0;
    return std.fmt.parseInt(u64, resident, 10) catch 0;
}

/// The cage a cgroup v2 path lives in, read along the ANCESTOR chain. A
/// systemd slice caps the slice and leaves the service leaf at `max`, so
/// the leaf alone reads "uncapped" and would fall silently to 70 % RAM — the
/// weaker posture charter §1.8 bans. `max` is the hard cap, `high` the
/// kernel's throttle point (its own Yellow): the effective cage is the
/// tightest of both over every level, and `level` names the directory it
/// came from (relative to the cgroup root; "" = the root itself).
pub const CgroupCage = struct {
    max: ?u64 = null,
    high: ?u64 = null,
    /// The tightest `memory.max`'s level.
    max_level: []const u8 = "",
    /// The tightest `memory.high`'s level.
    high_level: []const u8 = "",

    /// The value the budget derives from: `min(max, high)`; null = uncapped.
    pub fn effective(self: CgroupCage) ?u64 {
        if (self.max) |m| return if (self.high) |h| @min(m, h) else m;
        return self.high;
    }

    /// The file and level that bound `effective()` (for the log line).
    pub fn boundBy(self: CgroupCage) ?struct { file: []const u8, level: []const u8 } {
        const e = self.effective() orelse return null;
        if (self.max) |m| if (m == e) return .{ .file = "memory.max", .level = self.max_level };
        return .{ .file = "memory.high", .level = self.high_level };
    }
};

/// Walk `<root><path>`, `<root><parent>`, …, `<root>` reading both files at
/// each level; `level_buf` holds the bound levels' text. Pure over the
/// filesystem rooted at `root` so a synthetic tree tests it.
pub fn walkCgroupCage(io: std.Io, root: []const u8, path: []const u8, level_buf: *[2][std.Io.Dir.max_path_bytes]u8) CgroupCage {
    var cage: CgroupCage = .{};
    var lvl = std.mem.trimEnd(u8, path, "/");
    while (true) {
        var file_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
        var buf: [64]u8 = undefined;
        if (std.fmt.bufPrint(&file_buf, "{s}{s}/memory.max", .{ root, lvl })) |f| {
            if (readSmallFile(io, f, &buf)) |text| if (parseMemoryMax(text)) |v| if (cage.max == null or v < cage.max.?) {
                cage.max = v;
                cage.max_level = copyLevel(&level_buf[0], lvl);
            };
        } else |_| {}
        if (std.fmt.bufPrint(&file_buf, "{s}{s}/memory.high", .{ root, lvl })) |f| {
            if (readSmallFile(io, f, &buf)) |text| if (parseMemoryMax(text)) |v| if (cage.high == null or v < cage.high.?) {
                cage.high = v;
                cage.high_level = copyLevel(&level_buf[1], lvl);
            };
        } else |_| {}
        if (lvl.len == 0) break;
        lvl = lvl[0 .. std.mem.lastIndexOfScalar(u8, lvl, '/') orelse 0];
    }
    return cage;
}

fn copyLevel(buf: *[std.Io.Dir.max_path_bytes]u8, lvl: []const u8) []const u8 {
    @memcpy(buf[0..lvl.len], lvl);
    return buf[0..lvl.len];
}

/// The cgroup v2 root every production read starts from.
pub const cgroup_root = "/sys/fs/cgroup";

/// This process's cage (`walkCgroupCage` from its `/proc/self/cgroup` path),
/// re-read every tick so a runtime-lowered cap tightens the budget within
/// one tick. Null off-Linux or off-cgroup.
pub fn readCgroupCage(io: std.Io, level_buf: *[2][std.Io.Dir.max_path_bytes]u8) ?CgroupCage {
    if (builtin.os.tag != .linux) return null;
    var cg_buf: [4096]u8 = undefined;
    const cg = readSmallFile(io, "/proc/self/cgroup", &cg_buf) orelse return null;
    const path = parseCgroupV2Path(cg) orelse return null;
    return walkCgroupCage(io, cgroup_root, path, level_buf);
}

/// The effective cgroup cap of THIS process (`CgroupCage.effective`).
pub fn readCgroupMemoryMax(io: std.Io) ?u64 {
    var level_buf: [2][std.Io.Dir.max_path_bytes]u8 = undefined;
    return (readCgroupCage(io, &level_buf) orelse return null).effective();
}

/// cgroup v2 `memory.stat` unreclaimable sample of THIS process; null
/// off-cgroup (the arm disengages; statm + gauges remain).
pub fn readCgroupUnreclaimable(io: std.Io) ?u64 {
    if (builtin.os.tag != .linux) return null;
    // memory.stat is ~60 lines × ~30 B ≈ 2 KiB on the 7.x kernels; 16 KiB
    // leaves room for future keys without a second read.
    var buf: [16 * 1024]u8 = undefined;
    const text = cgroupFile(io, "memory.stat", &buf) orelse return null;
    return parseUnreclaimableMemoryStat(text);
}

/// Where the fleet census reads: `<proc>/self/{cgroup,stat}` name this
/// process, its cage's `<cgroup><path>/cgroup.procs` lists the pids, and
/// `<proc>/<pid>/stat` gives each one's comm and state. Production reads
/// the live trees; a test roots both in a fixture, so the share it pins
/// never counts another process on the box.
pub const CensusRoots = struct {
    proc: []const u8 = "/proc",
    cgroup: []const u8 = cgroup_root,
};

/// The fields of `/proc/<pid>/stat` the census reads.
pub const ProcStat = struct { pid: []const u8, comm: []const u8, state: u8 };

/// The head of `/proc/<pid>/stat` the census parses: a pid (≤ 7 digits at
/// the kernel's 2^22 `pid_max`) + ` (` + the comm (≤ 64 B, the kernel's
/// `%.64s`) + `) ` + the state: ≤ 76 B; the numeric fields after it go unread.
const proc_stat_head_bytes = 128;

/// `<pid> (<comm>) <state> …`: the comm may itself hold spaces and
/// parentheses, so it ends at the LAST `) ` (every later field is numeric).
/// Null on any other shape.
pub fn parseProcStat(text: []const u8) ?ProcStat {
    const open = std.mem.indexOf(u8, text, " (") orelse return null;
    const close = std.mem.lastIndexOf(u8, text, ") ") orelse return null;
    if (close < open + 2 or close + 2 >= text.len) return null;
    return .{ .pid = text[0..open], .comm = text[open + 2 .. close], .state = text[close + 2] };
}

/// The fleet share census of the cage at `path`: the pids of its
/// `cgroup.procs` whose stat names a live process with this process's comm
/// — a zombie (exited, unreaped, listed in the window between the two
/// reads) holds no memory and never counts. Pure over `roots`. Null when the
/// list is unreadable or does not list this process (a view of another cage
/// or pid namespace, whose count means nothing here; the caller falls back
/// to 1, loudly) — so a census is ≥ 1 by construction.
pub fn censusFleetShare(io: std.Io, roots: CensusRoots, path: []const u8) ?u32 {
    var file_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    var self_buf: [proc_stat_head_bytes]u8 = undefined;
    const self_path = std.fmt.bufPrint(&file_buf, "{s}/self/stat", .{roots.proc}) catch return null;
    const me = parseProcStat(readSmallFile(io, self_path, &self_buf) orelse return null) orelse return null;
    const procs_path = std.fmt.bufPrint(&file_buf, "{s}{s}/cgroup.procs", .{ roots.cgroup, std.mem.trimEnd(u8, path, "/") }) catch return null;
    // A cage with thousands of pids is not a daemon's: 64 KiB of pids
    // (~8k entries) is the most this census needs to see.
    var procs_buf: [64 * 1024]u8 = undefined;
    const procs = readSmallFile(io, procs_path, &procs_buf) orelse return null;
    var n: u32 = 0;
    var self_listed = false;
    var pids = std.mem.tokenizeAny(u8, procs, " \t\r\n");
    while (pids.next()) |pid| {
        if (std.mem.eql(u8, pid, me.pid)) {
            self_listed = true;
            n += 1;
            continue;
        }
        const stat_path = std.fmt.bufPrint(&file_buf, "{s}/{s}/stat", .{ roots.proc, pid }) catch continue;
        var stat_buf: [proc_stat_head_bytes]u8 = undefined;
        const st = parseProcStat(readSmallFile(io, stat_path, &stat_buf) orelse continue) orelse continue;
        if (st.state != 'Z' and st.state != 'X' and std.mem.eql(u8, st.comm, me.comm)) n += 1;
    }
    return if (self_listed) n else null;
}

var fleet_census_warned: std.atomic.Value(bool) = .init(false);

/// This process's fleet share from the census of its own cgroup over
/// `roots`; 1 off-cgroup (no `0::` line in `<proc>/self/cgroup`, as
/// off-Linux: the cage, if any, already divides); 1 with ONE warning when
/// the census is null.
pub fn readFleetShare(io: std.Io, roots: CensusRoots) u32 {
    var file_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    const cg_path = std.fmt.bufPrint(&file_buf, "{s}/self/cgroup", .{roots.proc}) catch return 1;
    var cg_buf: [4096]u8 = undefined;
    const cg = readSmallFile(io, cg_path, &cg_buf) orelse return 1;
    const path = parseCgroupV2Path(cg) orelse return 1;
    return censusFleetShare(io, roots, path) orelse {
        if (!fleet_census_warned.swap(true, .monotonic)) {
            log.warn("mem_budget: {s}{s}/cgroup.procs is unreadable or does not list this process — fleet share falls back to 1 (the whole-machine posture; co-located daemons will not divide the roots)", .{ roots.cgroup, path });
        }
        return 1;
    };
}

/// `/proc/self/statm` resident pages × the page size; 0 off-Linux or on a
/// read failure.
pub fn readRssBytes(io: std.Io, page_size: u64) u64 {
    if (builtin.os.tag != .linux) return 0;
    var buf: [256]u8 = undefined;
    const text = readSmallFile(io, "/proc/self/statm", &buf) orelse return 0;
    return parseStatmResidentPages(text) * page_size;
}

/// `/sys/fs/cgroup<path>/<leaf>` for this process's v2 cgroup.
fn cgroupFile(io: std.Io, comptime leaf: []const u8, buf: []u8) ?[]const u8 {
    var cg_buf: [4096]u8 = undefined;
    const cg = readSmallFile(io, "/proc/self/cgroup", &cg_buf) orelse return null;
    const path = parseCgroupV2Path(cg) orelse return null;
    var path_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    const full = std.fmt.bufPrint(&path_buf, cgroup_root ++ "{s}/" ++ leaf, .{path}) catch return null;
    return readSmallFile(io, full, buf);
}

/// Read a procfs/sysfs-class file into `buf` (these files report their size
/// as 0, so read until EOF or the buffer is full); null on any error. The
/// sampler is not a hot path, so it reads through the `Io` like every other
/// non-hot-path I/O (decision 0001).
fn readSmallFile(io: std.Io, path: []const u8, buf: []u8) ?[]const u8 {
    return std.Io.Dir.cwd().readFile(io, path, buf) catch null;
}

// ---------------------------------------------------------------------------
// §2 Component model.
// ---------------------------------------------------------------------------

pub const GaugeFn = struct { ctx: *anyopaque, call: *const fn (ctx: *anyopaque) u64 };
pub const ShedFn = struct { ctx: *anyopaque, call: *const fn (ctx: *anyopaque, target: u64) void };
/// Return the pool's idle (free-but-resident) memory to the kernel: every
/// entry idle for ≥ `min_idle_ticks` sampler ticks (`MemBudget.ticks` is
/// the clock the pools stamp their frees with). Never touches a live
/// entry; never lossy. Invoked from the sampler thread with no lock held.
pub const TrimFn = struct { ctx: *anyopaque, call: *const fn (ctx: *anyopaque, min_idle_ticks: u64) void };

/// Where a component's logical bytes come from: a plain ledger read
/// directly, or a computed level. Callable from the sampler thread without
/// locks; may be called twice per Red tick (sum pass + shed pass), so it
/// must be cheap and side-effect free.
pub const Source = union(enum) {
    gauge: *const counters.Gauge,
    func: GaugeFn,

    pub fn current(self: Source) u64 {
        return switch (self) {
            .gauge => |g| g.load(),
            .func => |f| f.call(f.ctx),
        };
    }
};

pub const Component = struct {
    name: []const u8,
    source: Source,
    /// Bytes it may always keep (clamped proportionally when Σ floors > 90 %
    /// of the budget).
    floor: u64,
    /// Red-shed share; 0 ⇒ never shed (attribution-only).
    weight: u64,
    /// The never-lossy mechanism (early flush, drop probation, trim); null
    /// ⇒ no-op (in-flight DMAs drain by completion). Invoked with no lock
    /// held, from the sampler thread; must be synchronous and non-blocking.
    shed: ?ShedFn,
    /// Attribution-only: counted in `gauge_sum`, excluded from pressure
    /// (logical bytes over reclaimed page cache are phantom pressure — the
    /// f1 trace: 5 GiB of reclaimed tier bytes pinned Red; their kill-relevant
    /// residue arrives via the unreclaimable arm).
    kernel_reclaimable: bool = false,
    /// The idle-trim hook (`trim_*_min_idle_ticks`); null ⇒ the component
    /// holds no idle memory of its own (in-flight DMAs, kernel regions).
    trim: ?TrimFn = null,
    sheds: std.atomic.Value(u64) = .init(0),
    trims: std.atomic.Value(u64) = .init(0),

    pub fn init(name: []const u8, floor: u64, weight: u64, source: Source, shed: ?ShedFn) Component {
        return .{ .name = name, .source = source, .floor = floor, .weight = weight, .shed = shed };
    }

    /// Builder flag for the mmap tiers.
    pub fn kernelReclaimable(self: Component) Component {
        var c = self;
        c.kernel_reclaimable = true;
        return c;
    }

    /// Builder: the idle-trim hook.
    pub fn withTrim(self: Component, trim: TrimFn) Component {
        var c = self;
        c.trim = trim;
        return c;
    }

    pub fn current(self: *const Component) u64 {
        return self.source.current();
    }

    /// Shed calls received.
    pub fn shedCount(self: *const Component) u64 {
        return self.sheds.load(.monotonic);
    }

    /// Trim calls received (Yellow and Red ticks).
    pub fn trimCount(self: *const Component) u64 {
        return self.trims.load(.monotonic);
    }

    fn callShed(self: *const Component, target: u64) void {
        if (self.shed) |s| s.call(s.ctx, target);
    }

    fn callTrim(self: *Component, min_idle_ticks: u64) void {
        const t = self.trim orelse return;
        _ = self.trims.fetchAdd(1, .monotonic);
        t.call(t.ctx, min_idle_ticks);
    }
};

/// What the live `tick` needs from the process: the Io handle (the sampler's
/// timed wait), the environment (knobs re-read every tick), the machine and
/// the fleet census's process source (the live trees unless a test roots it
/// in a fixture).
pub const Host = struct {
    io: std.Io,
    env: *const std.process.Environ.Map,
    facts: derive.Facts,
    census: CensusRoots = .{},
};

// ---------------------------------------------------------------------------
// §3 The authority.
// ---------------------------------------------------------------------------

/// Every scalar is a single-word atomic read and written Relaxed: racy-
/// tolerant by design (a stale level read costs one admission decision,
/// never correctness) and no cross-word invariant exists. The ONLY
/// release/acquire pair is the registry publish.
pub const MemBudget = struct {
    slots: [max_components]Component = undefined,
    /// Tickets handed to registrants; `count` trails it and only ever covers
    /// fully written slots.
    reserved: std.atomic.Value(usize) = .init(0),
    count: std.atomic.Value(usize) = .init(0),

    flag_budget: std.atomic.Value(u64) = .init(0),
    /// Ticks taken (the idle clock the pools stamp their frees with; one
    /// tick ≈ one sampler period).
    tick_count: std.atomic.Value(u64) = .init(0),
    /// The effective cgroup cage of the last resolution (0 = uncapped).
    cgroup_cap: std.atomic.Value(u64) = .init(0),
    budget: std.atomic.Value(u64) = .init(0),
    pressure: std.atomic.Value(u64) = .init(0),
    gauge_sum: std.atomic.Value(u64) = .init(0),
    unreclaimable: std.atomic.Value(u64) = .init(0),
    level_raw: std.atomic.Value(u8) = .init(@backingInt(Level.green)),
    yellow_events: std.atomic.Value(u64) = .init(0),
    red_events: std.atomic.Value(u64) = .init(0),
    hard_backstops: std.atomic.Value(u64) = .init(0),
    backstop_over_ticks: std.atomic.Value(u64) = .init(0),
    rss_window: [window_len]std.atomic.Value(u64) = @splat(.init(0)),
    rss_idx: std.atomic.Value(usize) = .init(0),
    unreclaim_window: [window_len]std.atomic.Value(u64) = @splat(.init(0)),
    unreclaim_idx: std.atomic.Value(usize) = .init(0),
    floors_clamped: std.atomic.Value(bool) = .init(false),
    tier_paused: std.atomic.Value(bool) = .init(false),
    backstop_on: std.atomic.Value(bool) = .init(false),

    sampler_started: std.atomic.Value(bool) = .init(false),
    sampler_stop: std.Io.Event = .unset,
    sampler_thread: ?std.Thread = null,

    // ---- registry ----

    /// Append-only, mount-time, never removed. Readers never block: one
    /// acquire load of `count` covers every published slot.
    pub fn register(self: *MemBudget, c: Component) void {
        const idx = self.reserved.fetchAdd(1, .monotonic);
        if (idx >= max_components) {
            registry_full.fire(c.name);
            return;
        }
        self.slots[idx] = c;
        // Publish in ticket order so `count` never covers an unwritten
        // slot: wait for every earlier ticket, then release ours. The
        // release pairs with the acquire load in `registry`/`components`.
        while (self.count.cmpxchgWeak(idx, idx + 1, .release, .monotonic) != null) std.atomic.spinLoopHint();
    }

    fn registry(self: *MemBudget) []Component {
        // Acquire pairs with the release CAS in `register`.
        return self.slots[0..self.count.load(.acquire)];
    }

    /// The published registry (stats, floors, callers).
    pub fn components(self: *const MemBudget) []const Component {
        // Acquire pairs with the release CAS in `register`.
        return self.slots[0..self.count.load(.acquire)];
    }

    // ---- accessors (relaxed loads; hot-path safe) ----

    /// The one-atomic-load admission check every growth path uses.
    pub fn level(self: *const MemBudget) Level {
        return Level.fromU8(self.level_raw.load(.monotonic));
    }
    pub fn budgetBytes(self: *const MemBudget) u64 {
        return self.budget.load(.monotonic);
    }
    /// The sampler's tick count: the idle clock (`TrimFn`).
    pub fn ticks(self: *const MemBudget) u64 {
        return self.tick_count.load(.monotonic);
    }
    pub fn pressureBytes(self: *const MemBudget) u64 {
        return self.pressure.load(.monotonic);
    }
    pub fn gaugeSumBytes(self: *const MemBudget) u64 {
        return self.gauge_sum.load(.monotonic);
    }
    pub fn unreclaimableBytes(self: *const MemBudget) u64 {
        return self.unreclaimable.load(.monotonic);
    }
    pub fn yellowEvents(self: *const MemBudget) u64 {
        return self.yellow_events.load(.monotonic);
    }
    pub fn redEvents(self: *const MemBudget) u64 {
        return self.red_events.load(.monotonic);
    }
    pub fn hardBackstops(self: *const MemBudget) u64 {
        return self.hard_backstops.load(.monotonic);
    }
    pub fn floorsClamped(self: *const MemBudget) bool {
        return self.floors_clamped.load(.monotonic);
    }
    pub fn backstopActive(self: *const MemBudget) bool {
        return self.backstop_on.load(.monotonic);
    }
    pub fn tierPublishPaused(self: *const MemBudget) bool {
        return self.tier_paused.load(.monotonic);
    }

    /// `--mem-budget`, bytes; 0 = unset.
    pub fn setFlagBudget(self: *MemBudget, bytes: u64) void {
        self.flag_budget.store(bytes, .monotonic);
    }

    /// Test seams: consumers' tests pin a level / the publish gate without a
    /// live sampler.
    pub fn forceLevelForTest(self: *MemBudget, l: Level) void {
        self.level_raw.store(@backingInt(l), .monotonic);
    }
    pub fn forceTierPublishPausedForTest(self: *MemBudget, paused: bool) void {
        self.tier_paused.store(paused, .monotonic);
    }

    // ---- §2.4 floor law ----

    /// Index-aligned with the registry. Σ floors ≤ 90 % of the budget ⇒
    /// verbatim; otherwise `floor_i × cap / sum` (proportional, not
    /// truncation-ordered), observable via `floorsClamped` and ONE warn line
    /// on the false→true edge — never a mount failure. Budget 0 ⇒ cap 0 ⇒
    /// every non-zero floor clamps to 0.
    pub fn effectiveFloors(self: *MemBudget, budget: u64, out: *[max_components]u64) []const u64 {
        const reg = self.registry();
        var sum: u64 = 0;
        for (reg) |*c| sum +|= c.floor;
        const cap = mulPct(budget, floor_cap_pct);
        if (sum <= cap or sum == 0) {
            self.floors_clamped.store(false, .monotonic);
            for (reg, 0..) |*c, i| out[i] = c.floor;
            return out[0..reg.len];
        }
        if (!self.floors_clamped.swap(true, .monotonic)) {
            log.warn("mem_budget: component floors sum to {d} B > {d}% of the {d} B budget — floors proportionally clamped (raise --mem-budget or lower component floors)", .{ sum, floor_cap_pct, budget });
        }
        for (reg, 0..) |*c, i| out[i] = @intCast(@as(u128, c.floor) * cap / sum);
        return out[0..reg.len];
    }

    // ---- §3.1 one tick ----

    /// One sampler tick with injected inputs — the pure form every test
    /// drives. Order: budget → windows → gauges → pressure → (0-check) →
    /// tier pause → backstop → level + events → shed.
    pub fn tickInner(self: *MemBudget, budget_now: u64, rss_sample: u64, unreclaimable_sample: u64) void {
        self.budget.store(budget_now, .monotonic);
        _ = self.tick_count.fetchAdd(1, .monotonic);
        const rss_max = pushWindow(&self.rss_window, &self.rss_idx, rss_sample);
        const unreclaim_max = pushWindow(&self.unreclaim_window, &self.unreclaim_idx, unreclaimable_sample);
        self.unreclaimable.store(unreclaim_max, .monotonic);

        // Full registry sum = the attribution surface; the pressure basis
        // excludes kernel-reclaimable components.
        const reg = self.registry();
        var gauge_sum: u64 = 0;
        var pressure_gauges: u64 = 0;
        for (reg) |*c| {
            const cur = c.current();
            gauge_sum +|= cur;
            if (!c.kernel_reclaimable) pressure_gauges +|= cur;
        }
        self.gauge_sum.store(gauge_sum, .monotonic);
        const pressure_now = @max(pressure_gauges, rss_max, unreclaim_max);
        self.pressure.store(pressure_now, .monotonic);

        // No budget (off-Linux probe, 0 RAM fact): idle Green with every
        // escalation cleared — before events and sheds.
        if (budget_now == 0) {
            self.level_raw.store(@backingInt(Level.green), .monotonic);
            self.tier_paused.store(false, .monotonic);
            self.backstop_over_ticks.store(0, .monotonic);
            self.backstop_on.store(false, .monotonic);
            return;
        }

        // Disk-tier publish pause: the unreclaimable arm ALONE (the dirty-
        // flood / anon-balloon kill signature). Gauge- or RSS-driven Red must
        // not pause publishes: logical tier bytes over clean page cache are
        // kernel-reclaimable. `swap` makes each edge log exactly once.
        const upct = (unreclaim_max *| 100) / budget_now;
        if (upct >= red_enter_pct) {
            if (!self.tier_paused.swap(true, .monotonic)) {
                log.warn("mem_budget: unreclaimable {d} B >= {d}% of the {d} B budget — disk-tier publishes paused", .{ unreclaim_max, red_enter_pct, budget_now });
            }
        } else if (upct < red_exit_pct and self.tier_paused.swap(false, .monotonic)) {
            log.info("mem_budget: unreclaimable pressure receded — disk-tier publishes resume", .{});
        }

        // Hard backstop on the WINDOWED unreclaimable max: ≥ 100 % for
        // `backstop_sustain_ticks` means Red sheds are not converging against
        // admission. Kernel accounting, not gauges, so it holds when a gauge
        // undercounts. Any sub-budget tick resets the run; release needs the
        // window below the Red-exit edge, not merely below 100 %.
        if (unreclaim_max >= budget_now) {
            const over = self.backstop_over_ticks.fetchAdd(1, .monotonic) + 1;
            if (over >= backstop_sustain_ticks and !self.backstop_on.swap(true, .monotonic)) {
                _ = self.hard_backstops.fetchAdd(1, .monotonic);
                log.warn("mem_budget: HARD BACKSTOP — unreclaimable {d} B >= the full {d} B budget for {d} consecutive ticks; Red sheds escalate to component floors until the pressure recedes", .{ unreclaim_max, budget_now, over });
            }
        } else {
            self.backstop_over_ticks.store(0, .monotonic);
            if (upct < red_exit_pct and self.backstop_on.swap(false, .monotonic)) {
                log.info("mem_budget: hard backstop released", .{});
            }
        }

        // The level machine. Red can drop straight to Green.
        const pct = (pressure_now *| 100) / budget_now;
        const prev = self.level();
        const next: Level = switch (prev) {
            .green => if (pct >= red_enter_pct) .red else if (pct >= yellow_enter_pct) .yellow else .green,
            .yellow => if (pct >= red_enter_pct) .red else if (pct < yellow_exit_pct) .green else .yellow,
            .red => if (pct >= red_exit_pct) .red else if (pct >= yellow_exit_pct) .yellow else .green,
        };
        if (next != prev) {
            switch (next) {
                .yellow => if (prev == .green) {
                    _ = self.yellow_events.fetchAdd(1, .monotonic);
                },
                .red => {
                    // Entering Red from Green counts both edges.
                    if (prev == .green) _ = self.yellow_events.fetchAdd(1, .monotonic);
                    _ = self.red_events.fetchAdd(1, .monotonic);
                },
                .green => {},
            }
            self.level_raw.store(@backingInt(next), .monotonic);
        }

        // EVERY Red tick sheds, not just the entry edge. The reference also
        // purged jemalloc's retained dirty pages here (`arena.4096.purge`);
        // the std allocators expose no release-to-OS call, so the pools
        // return their own idle memory: the idle-trim pass, Yellow and Red
        // (after the shed, so what the shed evicted returns in this tick).
        if (next == .red) self.shedToWeights(budget_now, pressure_now, reg);
        switch (next) {
            .green => {},
            .yellow => self.trimIdle(reg, trim_yellow_min_idle_ticks),
            .red => self.trimIdle(reg, trim_red_min_idle_ticks),
        }
    }

    fn trimIdle(_: *MemBudget, reg: []Component, min_idle_ticks: u64) void {
        for (reg) |*c| c.callTrim(min_idle_ticks);
    }

    /// Red response: distribute the excess over the 85 % target (or over Σ
    /// effective floors under the backstop) across sheddable components ∝
    /// weight, floor-clamped, in ONE pass — a shortfall reappears as
    /// pressure next tick. The last sheddable component takes the integer
    /// remainder so cuts sum exactly to the excess when headroom allows.
    fn shedToWeights(self: *MemBudget, budget_now: u64, pressure_now: u64, reg: []Component) void {
        var floors_buf: [max_components]u64 = undefined;
        const floors = self.effectiveFloors(budget_now, &floors_buf);
        var target_total: u64 = 0;
        if (self.backstop_on.load(.monotonic)) {
            for (floors) |f| target_total +|= f;
        } else {
            target_total = mulPct(budget_now, shed_target_pct);
        }
        if (pressure_now <= target_total) return;
        var excess = pressure_now - target_total;

        const Sheddable = struct { idx: usize, headroom: u64, weight: u64 };
        var sheddable: [max_components]Sheddable = undefined;
        var n: usize = 0;
        var weight_sum: u64 = 0;
        var headroom_sum: u64 = 0;
        for (reg, 0..) |*c, i| {
            const headroom = c.current() -| floors[i];
            if (headroom > 0 and c.weight > 0) {
                weight_sum +|= c.weight;
                headroom_sum +|= headroom;
                sheddable[n] = .{ .idx = i, .headroom = headroom, .weight = c.weight };
                n += 1;
            }
        }
        if (weight_sum == 0) return;
        // Never ask for more than exists over the floors.
        excess = @min(excess, headroom_sum);
        var remaining = excess;
        for (sheddable[0..n], 0..) |s, k| {
            const proportional: u64 = @intCast(@as(u128, excess) * s.weight / weight_sum);
            var share = if (k + 1 == n) remaining else @min(proportional, s.headroom);
            share = @min(share, s.headroom, remaining);
            remaining -= share;
            // At-target components get no call.
            if (share == 0) continue;
            const c = &reg[s.idx];
            const target = c.current() -| share;
            _ = c.sheds.fetchAdd(1, .monotonic);
            c.callShed(target);
        }
    }

    // ---- live resolution and the production tick ----

    /// flag → `SQUEEZEFS_MEM_BUDGET_MB` → cgroup cage × 0.8 (re-read NOW,
    /// along the ancestor chain) → 70 % RAM, system roots divided by the
    /// fleet share. Called every tick and by mount-time sizers so their
    /// footprint agrees with what the sampler will enforce. The binding
    /// cgroup level is logged when it changes.
    pub fn resolveBudgetNow(self: *MemBudget, host: Host) u64 {
        return self.resolveBudgetShared(host, fleetShare(host.io, host.env, host.census));
    }

    /// `resolveBudgetNow` for a share the caller already read: the tick
    /// divides its budget and its unreclaimable sample by ONE census.
    fn resolveBudgetShared(self: *MemBudget, host: Host, share: u32) u64 {
        const flag = self.flag_budget.load(.monotonic);
        var level_buf: [2][std.Io.Dir.max_path_bytes]u8 = undefined;
        const cage = readCgroupCage(host.io, &level_buf);
        self.noteCage(cage);
        return resolveBudgetFromShared(if (flag == 0) null else flag, envBudgetBytes(host.env), if (cage) |c| c.effective() else null, host.facts.total_ram, share);
    }

    /// Publish the cage and log the level that binds it on every change
    /// (never per tick).
    fn noteCage(self: *MemBudget, cage: ?CgroupCage) void {
        const effective: u64 = if (cage) |c| c.effective() orelse 0 else 0;
        if (self.cgroup_cap.swap(effective, .monotonic) == effective) return;
        if (cage) |c| if (c.boundBy()) |b| {
            log.info("mem_budget: cgroup cage {d} B bound by {s}{s}/{s}", .{ effective, cgroup_root, b.level, b.file });
            return;
        };
        log.info("mem_budget: no cgroup cage (uncapped at every level) — the derived tier is 70 % of RAM", .{});
    }

    /// The effective cgroup cage the last resolution saw (0 = uncapped).
    pub fn cgroupCapBytes(self: *const MemBudget) u64 {
        return self.cgroup_cap.load(.monotonic);
    }

    /// An explicit tier (flag or absolute env) is engaged — the tiers the
    /// fleet-share divisor never touches, so the floor refusal stands down.
    pub fn explicitBudgetEngaged(self: *const MemBudget, env: *const std.process.Environ.Map) bool {
        return self.flag_budget.load(.monotonic) != 0 or envBudgetBytes(env) != null;
    }

    /// The mount-time face of `sizing.fleetShareFloorRefusal`; null = mount
    /// proceeds.
    pub fn fleetShareMountRefusal(self: *const MemBudget, host: Host, buf: *sizing.RefusalBuf) ?[]const u8 {
        const share = fleetShare(host.io, host.env, host.census);
        if (share <= 1 or self.explicitBudgetEngaged(host.env)) return null;
        const divided = resolveBudgetFromShared(null, null, readCgroupMemoryMax(host.io), host.facts.total_ram, share);
        return sizing.fleetShareFloorRefusal(share, divided, host.facts.possible_cpus, buf);
    }

    /// Production tick: census + resolve + probe + `tickInputs` + `tickInner`.
    pub fn tick(self: *MemBudget, host: Host) void {
        const share = fleetShare(host.io, host.env, host.census);
        const in = tickInputs(self.resolveBudgetShared(host, share), readRssBytes(host.io, host.facts.page_size), readCgroupUnreclaimable(host.io) orelse 0, share);
        self.tickInner(in.budget, in.rss, in.unreclaimable);
    }

    // ---- §3.5 the sampler thread ----

    /// Spawn the sampler (mount-time; idempotent — one thread per
    /// authority). Returns whether THIS call spawned it. The tick runs on its
    /// own thread, never on a request worker: it does procfs reads plus every
    /// gauge call, precisely when the daemon is under memory pressure.
    ///
    /// The thread reads every registered source without a lock, so the
    /// owner of a source must `stopSampler` (stop + JOIN) before freeing it;
    /// the daemon spawns the sampler after its last mount-time registrant
    /// and stops it first on every exit (`cli/mount.zig`).
    pub fn spawnSampler(self: *MemBudget, host: Host, period_ns: u64) std.Thread.SpawnError!bool {
        if (self.sampler_started.swap(true, .acq_rel)) return false;
        const tick_on_stop = knobs.boolKnob("SQUEEZEFS_TEST_MEM_SAMPLER_TICK_ON_STOP", host.env);
        self.sampler_thread = std.Thread.spawn(.{}, samplerMain, .{ self, host, period_ns, tick_on_stop }) catch |err| {
            self.sampler_started.store(false, .release);
            return err;
        };
        return true;
    }

    /// Stop and join the sampler (umount). Wakes the timed wait immediately;
    /// returns once the thread has exited, so no source is read after this.
    pub fn stopSampler(self: *MemBudget, io: std.Io) void {
        self.sampler_stop.set(io);
        if (self.sampler_thread) |t| {
            t.join();
            self.sampler_thread = null;
        }
    }

    fn samplerMain(self: *MemBudget, host: Host, period_ns: u64, tick_on_stop: bool) void {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, sampler_thread_name);
        const period: std.Io.Timeout = .{ .duration = .{ .raw = .fromNanoseconds(@intCast(period_ns)), .clock = .awake } };
        while (true) {
            // Interval semantics: the next period starts after the previous
            // tick RETURNED (first tick after one full period, no catch-up
            // bursts, ticks never overlap — the level ladder, the backstop
            // run and the shed pass are single-tick state machines).
            if (self.sampler_stop.waitTimeout(host.io, period)) |_| {
                // The seam's final tick: every source read at the stop, so
                // a teardown that freed one before stopping the sampler is
                // caught on the dangling read instead of by timing.
                if (tick_on_stop) self.tick(host);
                return;
            } else |err| switch (err) {
                error.Timeout => {},
                error.Canceled => return,
            }
            self.tick(host);
        }
    }

    // ---- §5 stats ----

    /// Every spec key as one JSON object; `floor` is the RAW floor, not the
    /// clamped one.
    pub fn writeStatsJson(self: *const MemBudget, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{{\"mem_budget_bytes\":{d},\"mem_budget_pressure_bytes\":{d},\"mem_budget_gauge_sum_bytes\":{d},\"mem_budget_level\":{d},", .{ self.budgetBytes(), self.pressureBytes(), self.gaugeSumBytes(), @backingInt(self.level()) });
        try w.print("\"mem_budget_yellow_events\":{d},\"mem_budget_red_events\":{d},\"mem_budget_floors_clamped\":{},\"mem_budget_unreclaimable_bytes\":{d},", .{ self.yellowEvents(), self.redEvents(), self.floorsClamped(), self.unreclaimableBytes() });
        try w.print("\"mem_budget_hard_backstops\":{d},\"mem_budget_backstop_active\":{},\"mem_budget_tier_publish_paused\":{},\"mem_budget_ticks\":{d},", .{ self.hardBackstops(), self.backstopActive(), self.tierPublishPaused(), self.ticks() });
        try w.writeAll("\"mem_budget_components\":{");
        for (self.components(), 0..) |*c, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("\"{s}\":{{\"current\":{d},\"floor\":{d},\"weight\":{d},\"sheds\":{d},\"trims\":{d}}}", .{ c.name, c.current(), c.floor, c.weight, c.shedCount(), c.trimCount() });
        }
        try w.writeAll("}}");
    }
};

/// Overwrite the oldest slot (monotone index mod the length) and return the
/// max over every slot; zeros count.
fn pushWindow(window: *[window_len]std.atomic.Value(u64), idx: *std.atomic.Value(usize), sample: u64) u64 {
    const i = idx.fetchAdd(1, .monotonic) % window_len;
    window[i].store(sample, .monotonic);
    var max: u64 = 0;
    for (window) |*s| max = @max(max, s.load(.monotonic));
    return max;
}

/// `v × p / 100` — the reference's integer form for the 85 % shed target and
/// the 90 % floor cap (multiply first; `derive.pct` truncates to a hundredth
/// first and would disagree by up to 99 B). Widened so a flag budget near
/// `u64::MAX` cannot overflow.
fn mulPct(v: u64, p: u64) u64 {
    return @intCast(@as(u128, v) * p / 100);
}

// ---------------------------------------------------------------------------
// The process authority.
// ---------------------------------------------------------------------------

/// Consumers register at mount, the sampler ticks it at 1 Hz, growth paths
/// read `level()`.
pub var authority: MemBudget = .{};

/// The one-atomic-load admission check (process global).
pub fn level() Level {
    return authority.level();
}

/// Disk-tier publish gate (process global).
pub fn tierPublishPaused() bool {
    return authority.tierPublishPaused();
}

// ---------------------------------------------------------------------------
// Tests: the contract (spec §9.1 pure tests, §9.3 derivation ties).
// ---------------------------------------------------------------------------

const testing = std.testing;

test {
    _ = sizing;
}

/// The 320 GiB fleet box of the fleet-share ties.
const fleet_ram: u64 = 320 * GiB;

const ShedLog = struct {
    const Entry = struct { name: []const u8, target: u64 };
    entries: [32]Entry = undefined,
    len: usize = 0,
    name_bytes: [1024]u8 = undefined,
    name_len: usize = 0,

    fn push(self: *ShedLog, name: []const u8, target: u64) void {
        self.entries[self.len] = .{ .name = name, .target = target };
        self.len += 1;
    }
    fn clear(self: *ShedLog) void {
        self.len = 0;
    }
    fn find(self: *const ShedLog, name: []const u8) ?u64 {
        for (self.entries[0..self.len]) |e| if (std.mem.eql(u8, e.name, name)) return e.target;
        return null;
    }
    fn slice(self: *const ShedLog) []const Entry {
        return self.entries[0..self.len];
    }
    /// A copy of `name` that outlives the caller's buffer.
    fn intern(self: *ShedLog, name: []const u8) []const u8 {
        const dst = self.name_bytes[self.name_len..][0..name.len];
        @memcpy(dst, name);
        self.name_len += name.len;
        return dst;
    }
};

/// A test component: an atomic gauge plus a shed callback that either
/// records `(name, target)` or clamps the gauge to the target.
const TestComp = struct {
    gauge: std.atomic.Value(u64),
    name: []const u8,
    shed_log: *ShedLog,

    fn init(name: []const u8, value: u64, shed_log: *ShedLog) TestComp {
        return .{ .gauge = .init(value), .name = name, .shed_log = shed_log };
    }
    fn current(ctx: *anyopaque) u64 {
        const self: *TestComp = @ptrCast(@alignCast(ctx));
        return self.gauge.load(.monotonic);
    }
    fn record(ctx: *anyopaque, target: u64) void {
        const self: *TestComp = @ptrCast(@alignCast(ctx));
        self.shed_log.push(self.name, target);
    }
    fn clamp(ctx: *anyopaque, target: u64) void {
        const self: *TestComp = @ptrCast(@alignCast(ctx));
        self.gauge.store(target, .monotonic);
    }
    fn set(self: *TestComp, v: u64) void {
        self.gauge.store(v, .monotonic);
    }
    fn get(self: *const TestComp) u64 {
        return self.gauge.load(.monotonic);
    }
    /// Shed records the target (the reference tests' `comp` helper).
    fn recording(self: *TestComp, floor: u64, weight: u64) Component {
        return Component.init(self.name, floor, weight, .{ .func = .{ .ctx = self, .call = current } }, .{ .ctx = self, .call = record });
    }
    /// Shed clamps the gauge (the storm test's working lever).
    fn clamping(self: *TestComp, floor: u64, weight: u64) Component {
        return Component.init(self.name, floor, weight, .{ .func = .{ .ctx = self, .call = current } }, .{ .ctx = self, .call = clamp });
    }
    /// No shed at all (attribution-only).
    fn inert(self: *TestComp, floor: u64, weight: u64) Component {
        return Component.init(self.name, floor, weight, .{ .func = .{ .ctx = self, .call = current } }, null);
    }
    fn recordTrim(ctx: *anyopaque, min_idle_ticks: u64) void {
        const self: *TestComp = @ptrCast(@alignCast(ctx));
        var buf: [64]u8 = undefined;
        self.shed_log.push(self.shed_log.intern(std.fmt.bufPrint(&buf, "{s}:trim", .{self.name}) catch unreachable), min_idle_ticks);
    }
    /// A recording shed plus a recording trim (`<name>:trim`, target = the
    /// idle-tick minimum).
    fn trimming(self: *TestComp, floor: u64, weight: u64) Component {
        return Component.init(self.name, floor, weight, .{ .func = .{ .ctx = self, .call = current } }, .{ .ctx = self, .call = record }).withTrim(.{ .ctx = self, .call = recordTrim });
    }
};

// ---- §1 budget derivation ----

test "budget resolution order: flag > env > cgroup memory.max / 10 * 8 > RAM / 10 * 7" {
    try testing.expectEqual(2 * GiB, resolveBudgetFrom(2 * GiB, 3 * GiB, 8 * GiB, 64 * GiB));
    try testing.expectEqual(3 * GiB, resolveBudgetFrom(null, 3 * GiB, 8 * GiB, 64 * GiB));
    try testing.expectEqual(8 * GiB, resolveBudgetFrom(null, null, 10 * GiB, 64 * GiB));
    try testing.expectEqual(7 * GiB, resolveBudgetFrom(null, null, null, 10 * GiB));
    // The integer shape: divide by 10 first, then multiply (never ×0.8 float).
    const odd: u64 = 25_769_803_777;
    try testing.expectEqual(odd / 10 * 8, resolveBudgetFrom(null, null, odd, 1));
    try testing.expectEqual(odd / 10 * 7, resolveBudgetFrom(null, null, null, odd));
}

test "resolveBudget(facts) tie: explicit MiB > tenths(cgroup, 8) > tenths(ram, 7)" {
    const facts = derive.Facts.synthetic(32, 251_000_000_000);
    try testing.expectEqual(3 * GiB, resolveBudget(facts, 3072, 10 * GiB));
    try testing.expectEqual(derive.tenths(10 * GiB, 8), resolveBudget(facts, null, 10 * GiB));
    try testing.expectEqual(derive.tenths(251_000_000_000, 7), resolveBudget(facts, null, null));
    // A 0 RAM fact (off-Linux probe) derives a 0 budget: the authority idles Green.
    try testing.expectEqual(@as(u64, 0), resolveBudget(derive.Facts.synthetic(1, 0), null, null));
}

test "fleet share divides the memory ROOT, derived tier only; ceil, never floor" {
    // 320 GiB ÷ 4 = 80 GiB effective RAM ⇒ 70 % = 56 GiB.
    try testing.expectEqual(56 * GiB, resolveBudgetFromShared(null, null, null, fleet_ram, 4));
    // 10 GiB cage ÷ 4 = 2.5 GiB ⇒ × 0.8 = 2 GiB.
    try testing.expectEqual(2 * GiB, resolveBudgetFromShared(null, null, 10 * GiB, fleet_ram, 4));
    // Explicit tiers are never divided.
    try testing.expectEqual(2 * GiB, resolveBudgetFromShared(2 * GiB, 3 * GiB, 8 * GiB, fleet_ram, 4));
    try testing.expectEqual(3 * GiB, resolveBudgetFromShared(null, 3 * GiB, 8 * GiB, fleet_ram, 4));
    // fleet_shared_root(v, share) = ceil(v / max(share, 1)).
    try testing.expectEqual(@as(u64, 3), fleetSharedRoot(10, 4));
    try testing.expectEqual(@as(u64, 5), fleetSharedRoot(5, 1));
    try testing.expectEqual(@as(u64, 5), fleetSharedRoot(5, 0));
    try testing.expectEqual(std.math.maxInt(u64) / 2 + 1, fleetSharedRoot(std.math.maxInt(u64), 2));
}

test "fleet share 1 is byte-identical to the whole-machine posture" {
    for ([_]u64{ 4 * GiB, 251_000_000_000, fleet_ram }) |ram| {
        try testing.expectEqual(resolveBudgetFrom(null, null, null, ram), resolveBudgetFromShared(null, null, null, ram, 1));
        try testing.expectEqual(resolveBudgetFrom(null, null, 10 * GiB, ram), resolveBudgetFromShared(null, null, 10 * GiB, ram, 1));
        try testing.expectEqual(resolveBudgetFrom(2 * GiB, null, null, ram), resolveBudgetFromShared(2 * GiB, null, null, ram, 1));
    }
}

test "fleet share quarters every divisible derived cap: budget, transport cap, ipc cap" {
    const whole = resolveBudgetFromShared(null, null, null, fleet_ram, 1);
    const shared = resolveBudgetFromShared(null, null, null, fleet_ram, 4);
    try testing.expectEqual(224 * GiB, whole);
    try testing.expectEqual(56 * GiB, shared);
    try testing.expectEqual(7 * GiB, sizing.transportBufferCap(shared));
    try testing.expectEqual(sizing.transportBufferCap(whole) / 4, sizing.transportBufferCap(shared));
    try testing.expectEqual(7 * GiB, sizing.ipcArenaCap(shared));
}

test "fleet share floors hold and are never divided at every share" {
    // The population floor 128 holds for a 32-CPU root at any share.
    for ([_]u32{ 2, 4, 32, 4096 }) |share| {
        try testing.expectEqual(@as(u64, 128), sizing.ipcSessionPopulationTarget(@intCast(fleetSharedRoot(32, share))));
    }
    try testing.expectEqual(@as(u64, 4), sizing.transport_q_depth_floor);
    try testing.expectEqual(1 * MiB, sizing.transport_payload_base);
}

test "tick_inputs divides the unreclaimable sample by the fleet share; budget and RSS pass through" {
    const solo = tickInputs(2 * GiB, GiB, 5 * GiB, 1);
    try testing.expectEqual(2 * GiB, solo.budget);
    try testing.expectEqual(GiB, solo.rss);
    try testing.expectEqual(5 * GiB, solo.unreclaimable);
    const fleet = tickInputs(2 * GiB, GiB, 5 * GiB, 32);
    try testing.expectEqual(2 * GiB, fleet.budget);
    try testing.expectEqual(GiB, fleet.rss);
    try testing.expectEqual(5 * GiB / 32, fleet.unreclaimable);
    try testing.expectEqual(5 * GiB / 32 + 1, tickInputs(2 * GiB, GiB, 5 * GiB + 1, 32).unreclaimable);
}

test "parked cap halves under Red: max(cap / 2, 1)" {
    try testing.expectEqual(@as(u64, 256), effectiveParkedCap(256, .green));
    try testing.expectEqual(@as(u64, 256), effectiveParkedCap(256, .yellow));
    try testing.expectEqual(@as(u64, 128), effectiveParkedCap(256, .red));
    try testing.expectEqual(@as(u64, 1), effectiveParkedCap(1, .red));
    try testing.expectEqual(@as(u64, 1), effectiveParkedCap(0, .red));
}

test "Level: Green < Yellow < Red; fromU8 maps unknown codes to Green" {
    try testing.expect(@backingInt(Level.green) < @backingInt(Level.yellow));
    try testing.expect(@backingInt(Level.yellow) < @backingInt(Level.red));
    try testing.expectEqual(Level.red, Level.fromU8(2));
    try testing.expectEqual(Level.yellow, Level.fromU8(1));
    try testing.expectEqual(Level.green, Level.fromU8(0));
    try testing.expectEqual(Level.green, Level.fromU8(7));
}

// ---- §2 component model ----

test "floor sum clamp is proportional and loud; the flag tracks the live budget" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var a = TestComp.init("a", 0, &shed_log);
    var c = TestComp.init("b", 0, &shed_log);
    b.register(a.recording(600, 1));
    b.register(c.recording(300, 1));
    var out: [max_components]u64 = undefined;
    // Σ floors = 900 ≤ 0.9 × 1000: verbatim.
    const f1 = b.effectiveFloors(1000, &out);
    try testing.expectEqualSlices(u64, &.{ 600, 300 }, f1);
    try testing.expect(!b.floorsClamped());
    // Σ floors = 900 > 0.9 × 800 = 720: proportional (× 0.8), not truncation-ordered.
    const f2 = b.effectiveFloors(800, &out);
    try testing.expectEqualSlices(u64, &.{ 480, 240 }, f2);
    try testing.expect(b.floorsClamped());
    // Budget 0: cap 0 ⇒ every floor clamps to 0.
    try testing.expectEqualSlices(u64, &.{ 0, 0 }, b.effectiveFloors(0, &out));
    // Back to a roomy budget: the flag clears.
    _ = b.effectiveFloors(1000, &out);
    try testing.expect(!b.floorsClamped());
}

test "registry: fixed capacity; overflow trips the must-stay-0 tripwire and drops the component" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var comps: [max_components + 1]TestComp = undefined;
    for (&comps) |*tc| tc.* = TestComp.init("x", 1, &shed_log);
    for (comps[0..max_components]) |*tc| b.register(tc.inert(0, 0));
    try testing.expectEqual(max_components, b.components().len);
    const before = registry_full.load();
    log.testing.expectErrorLogs(1);
    b.register(comps[max_components].inert(0, 0));
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, registry_full.load());
    try testing.expectEqual(max_components, b.components().len);
    b.tickInner(1000, 0, 0);
    try testing.expectEqual(@as(u64, max_components), b.gaugeSumBytes());
}

test "a counters.Gauge is a component source directly" {
    var b: MemBudget = .{};
    var g: counters.Gauge = .{};
    g.set(420);
    b.register(Component.init("ledger", 0, 0, .{ .gauge = &g }, null));
    b.tickInner(1000, 0, 0);
    try testing.expectEqual(@as(u64, 420), b.pressureBytes());
    try testing.expectEqual(@as(u64, 420), b.components()[0].current());
}

// ---- §3 pressure, windows, level machine, escalations ----

test "level transitions: 80/76 and 95/91 hysteresis; entry edges counted once" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var g = TestComp.init("g", 0, &shed_log);
    b.register(g.recording(0, 1));
    const steps = [_]struct { p: u64, l: Level }{
        .{ .p = 790, .l = .green },
        .{ .p = 800, .l = .yellow }, // enter at ≥ 80 %
        .{ .p = 940, .l = .yellow },
        .{ .p = 950, .l = .red }, // enter at ≥ 95 %
        .{ .p = 920, .l = .red }, // stays Red until < 91 %
        .{ .p = 905, .l = .yellow },
        .{ .p = 770, .l = .yellow }, // stays Yellow until < 76 %
        .{ .p = 759, .l = .green },
        .{ .p = 800, .l = .yellow },
        .{ .p = 700, .l = .green },
    };
    for (steps) |s| {
        g.set(s.p);
        b.tickInner(1000, 0, 0);
        try testing.expectEqual(s.l, b.level());
    }
    try testing.expectEqual(@as(u64, 2), b.yellowEvents());
    try testing.expectEqual(@as(u64, 1), b.redEvents());
}

test "entering Red from Green counts both edges; Red can drop straight to Green" {
    var b: MemBudget = .{};
    b.tickInner(1000, 990, 0);
    try testing.expectEqual(Level.red, b.level());
    try testing.expectEqual(@as(u64, 1), b.yellowEvents());
    try testing.expectEqual(@as(u64, 1), b.redEvents());
    for (0..5) |_| b.tickInner(1000, 10, 0);
    try testing.expectEqual(Level.green, b.level());
    // Downward moves count nothing.
    try testing.expectEqual(@as(u64, 1), b.yellowEvents());
    try testing.expectEqual(@as(u64, 1), b.redEvents());
}

test "windowed RSS max decays, never ratchets" {
    var b: MemBudget = .{};
    b.tickInner(1000, 960, 0);
    try testing.expectEqual(Level.red, b.level());
    for (0..4) |_| {
        b.tickInner(1000, 100, 0);
        try testing.expectEqual(Level.red, b.level());
    }
    b.tickInner(1000, 100, 0);
    try testing.expectEqual(Level.green, b.level());
    try testing.expect(b.pressureBytes() <= 100);
}

test "Red sheds ∝ weight over floors, once per tick; at-floor components get no call; Yellow never sheds" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var a = TestComp.init("a", 500, &shed_log);
    var bb = TestComp.init("b", 400, &shed_log);
    var c = TestComp.init("c", 80, &shed_log);
    b.register(a.recording(100, 3));
    b.register(bb.recording(100, 1));
    b.register(c.recording(80, 1));
    // 980 / 1000 = 98 % ⇒ Red; target 85 % ⇒ excess 130 over {a:3, b:1}; c is at its floor.
    b.tickInner(1000, 0, 0);
    try testing.expectEqual(Level.red, b.level());
    try testing.expectEqual(@as(usize, 2), shed_log.len);
    try testing.expectEqual(@as(?u64, null), shed_log.find("c"));
    const ta = shed_log.find("a").?;
    const tb = shed_log.find("b").?;
    try testing.expect(ta >= 100 and tb >= 100);
    const cut_a: i64 = @intCast(500 - ta);
    const cut_b: i64 = @intCast(400 - tb);
    try testing.expect(@abs(cut_a - 3 * cut_b) <= 3);
    try testing.expectEqual(@as(i64, 130), cut_a + cut_b);
    try testing.expectEqual(@as(u64, 1), b.components()[0].shedCount());
    try testing.expectEqual(@as(u64, 0), b.components()[2].shedCount());
    // Yellow must not shed: 980 / 1200 = 82 %.
    shed_log.clear();
    b.tickInner(1200, 0, 0);
    try testing.expectEqual(Level.yellow, b.level());
    try testing.expectEqual(@as(usize, 0), shed_log.len);
}

test "the idle-trim law: Green ticks trim nothing; a Yellow tick asks every trimmable component for entries idle ≥ 1 tick; a Red tick for everything idle (min 0), after the shed; the tick counter advances once per tick" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var pool = TestComp.init("pool", 500, &shed_log);
    var plain = TestComp.init("plain", 300, &shed_log);
    b.register(pool.trimming(0, 1));
    b.register(plain.recording(0, 1));
    try testing.expectEqual(@as(u64, 0), b.ticks());
    // 800 / 1000 = 80 % ⇒ Yellow: a trim call with min_idle_ticks 1, no shed.
    b.tickInner(1000, 0, 0);
    try testing.expectEqual(@as(u64, 1), b.ticks());
    try testing.expectEqual(Level.yellow, b.level());
    try testing.expectEqual(@as(usize, 1), shed_log.len);
    try testing.expectEqual(@as(?u64, trim_yellow_min_idle_ticks), shed_log.find("pool:trim"));
    try testing.expectEqual(@as(u64, 0), b.components()[0].shedCount());
    // Green: nothing.
    shed_log.clear();
    b.tickInner(10_000, 0, 0);
    try testing.expectEqual(@as(u64, 2), b.ticks());
    try testing.expectEqual(Level.green, b.level());
    try testing.expectEqual(@as(usize, 0), shed_log.len);
    // Red (800 / 820 = 97 %): the shed first (both components, weight 1
    // each), then the trim with min 0 — the order the log records.
    b.tickInner(820, 0, 0);
    try testing.expectEqual(Level.red, b.level());
    try testing.expectEqual(@as(usize, 3), shed_log.len);
    try testing.expectEqualStrings("pool", shed_log.slice()[0].name);
    try testing.expectEqualStrings("plain", shed_log.slice()[1].name);
    try testing.expectEqualStrings("pool:trim", shed_log.slice()[2].name);
    try testing.expectEqual(@as(u64, trim_red_min_idle_ticks), shed_log.slice()[2].target);
    try testing.expectEqual(@as(u64, 1), b.components()[0].shedCount());
    try testing.expectEqual(@as(u64, 2), b.components()[0].trimCount());
    try testing.expectEqual(@as(u64, 0), b.components()[1].trimCount());
    // A 0 budget never trims (idle Green with every escalation cleared).
    shed_log.clear();
    b.tickInner(0, 0, 0);
    try testing.expectEqual(@as(usize, 0), shed_log.len);
}

test "shed remainder goes to the last sheddable component so cuts sum exactly to the excess" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var x = TestComp.init("x", 320, &shed_log);
    var y = TestComp.init("y", 320, &shed_log);
    var z = TestComp.init("z", 320, &shed_log);
    b.register(x.recording(0, 1));
    b.register(y.recording(0, 1));
    b.register(z.recording(0, 1));
    // 960 / 1000 ⇒ Red; excess 110 over three equal weights: 36 + 36 + 38.
    b.tickInner(1000, 0, 0);
    try testing.expectEqual(@as(u64, 320 - 36), shed_log.find("x").?);
    try testing.expectEqual(@as(u64, 320 - 36), shed_log.find("y").?);
    try testing.expectEqual(@as(u64, 320 - 38), shed_log.find("z").?);
}

test "shed excess is capped by Σ headroom when pressure is RSS-driven and gauges are small" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var g = TestComp.init("g", 150, &shed_log);
    b.register(g.recording(100, 1));
    // Pressure 2000 from RSS, excess 1150, but only 50 B of headroom exist.
    b.tickInner(1000, 2000, 0);
    try testing.expectEqual(Level.red, b.level());
    try testing.expectEqual(@as(u64, 100), shed_log.find("g").?);
    // Weight 0 is never shed even with headroom.
    var b2: MemBudget = .{};
    var shed_log2: ShedLog = .{};
    var h = TestComp.init("h", 900, &shed_log2);
    b2.register(h.recording(0, 0));
    b2.tickInner(1000, 990, 0);
    try testing.expectEqual(Level.red, b2.level());
    try testing.expectEqual(@as(usize, 0), shed_log2.len);
}

test "budget 0 short-circuits: Green, escalation state cleared, no events, no sheds; levels still published" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var g = TestComp.init("g", 500, &shed_log);
    b.register(g.recording(100, 1));
    for (0..3) |_| b.tickInner(1000, 0, 1100);
    try testing.expect(b.backstopActive());
    try testing.expect(b.tierPublishPaused());
    try testing.expectEqual(Level.red, b.level());
    shed_log.clear();
    b.tickInner(0, 0, 1100);
    try testing.expectEqual(Level.green, b.level());
    try testing.expect(!b.backstopActive());
    try testing.expect(!b.tierPublishPaused());
    try testing.expectEqual(@as(usize, 0), shed_log.len);
    try testing.expectEqual(@as(u64, 0), b.budgetBytes());
    try testing.expectEqual(@as(u64, 1100), b.pressureBytes());
    try testing.expectEqual(@as(u64, 500), b.gaugeSumBytes());
    const y = b.yellowEvents();
    const r = b.redEvents();
    b.tickInner(0, 0, 1100);
    try testing.expectEqual(y, b.yellowEvents());
    try testing.expectEqual(r, b.redEvents());
    // The run counter was cleared: a fresh over-budget run needs 3 ticks again.
    b.tickInner(1000, 0, 1100);
    b.tickInner(1000, 0, 1100);
    try testing.expect(!b.backstopActive());
    b.tickInner(1000, 0, 1100);
    try testing.expect(b.backstopActive());
}

test "lock-free level reads stay valid and immediately visible across 100 ticks" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var g = TestComp.init("g", 990, &shed_log);
    // Weight 1 with a no-op shed (the in-flight-DMA shape): every tick is Red and sheds.
    b.register(g.inert(0, 1));
    const Reader = struct {
        fn run(mb: *const MemBudget) void {
            for (0..10_000) |_| {
                const raw = @backingInt(mb.level());
                std.debug.assert(raw <= 2);
            }
        }
    };
    var threads: [4]std.Thread = undefined;
    for (&threads) |*t| t.* = try std.Thread.spawn(.{}, Reader.run, .{&b});
    for (0..100) |_| b.tickInner(1000, 0, 0);
    for (threads) |t| t.join();
    try testing.expectEqual(Level.red, b.level());
}

test "pressure counts the windowed unreclaimable arm alone; it decays like RSS" {
    var b: MemBudget = .{};
    b.tickInner(1000, 0, 960);
    try testing.expectEqual(Level.red, b.level());
    try testing.expect(b.pressureBytes() >= 960);
    try testing.expectEqual(@as(u64, 960), b.unreclaimableBytes());
    for (0..4) |_| {
        b.tickInner(1000, 0, 100);
        try testing.expectEqual(Level.red, b.level());
    }
    b.tickInner(1000, 0, 100);
    try testing.expectEqual(Level.green, b.level());
    try testing.expect(b.unreclaimableBytes() <= 100);
}

test "tier-publish pause keys on the unreclaimable arm only, with 95/91 hysteresis" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var g = TestComp.init("hot", 990, &shed_log);
    b.register(g.recording(0, 1));
    b.tickInner(1000, 0, 100);
    try testing.expectEqual(Level.red, b.level());
    try testing.expect(!b.tierPublishPaused());
    b.tickInner(1000, 0, 950);
    try testing.expect(b.tierPublishPaused());
    g.set(0);
    for (0..5) |_| b.tickInner(1000, 0, 920);
    try testing.expect(b.tierPublishPaused());
    for (0..5) |_| b.tickInner(1000, 0, 900);
    try testing.expect(!b.tierPublishPaused());
    // The test seam pins it without a sampler.
    b.forceTierPublishPausedForTest(true);
    try testing.expect(b.tierPublishPaused());
    b.forceTierPublishPausedForTest(false);
    try testing.expect(!b.tierPublishPaused());
}

test "hard backstop: 3 consecutive ticks ≥ 100 % collapse the shed target to the floors; release < 91 %" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var a = TestComp.init("a", 500, &shed_log);
    b.register(a.recording(100, 1));
    b.tickInner(1000, 0, 1100);
    b.tickInner(1000, 0, 1100);
    try testing.expectEqual(Level.red, b.level());
    try testing.expectEqual(@as(u64, 0), b.hardBackstops());
    try testing.expect(!b.backstopActive());
    for (shed_log.slice()) |e| try testing.expect(e.target > 100);
    shed_log.clear();
    b.tickInner(1000, 0, 1100);
    try testing.expectEqual(@as(u64, 1), b.hardBackstops());
    try testing.expect(b.backstopActive());
    try testing.expectEqual(@as(u64, 100), shed_log.find("a").?);
    for (0..5) |_| b.tickInner(1000, 0, 200);
    try testing.expect(!b.backstopActive());
    try testing.expectEqual(@as(u64, 1), b.hardBackstops());
    // A gauge-only overage never trips the backstop.
    var b2: MemBudget = .{};
    var phantom = TestComp.init("phantom", 1500, &shed_log);
    b2.register(phantom.recording(0, 1));
    for (0..5) |_| b2.tickInner(1000, 0, 100);
    try testing.expectEqual(Level.red, b2.level());
    try testing.expectEqual(@as(u64, 0), b2.hardBackstops());
}

test "backstop subtlety: one over-budget SAMPLE trips on the 3rd tick because the window holds the max" {
    var b: MemBudget = .{};
    b.tickInner(1000, 0, 1100);
    b.tickInner(1000, 0, 10);
    try testing.expect(!b.backstopActive());
    b.tickInner(1000, 0, 10);
    try testing.expect(b.backstopActive());
    try testing.expectEqual(@as(u64, 1), b.hardBackstops());
    // Still ≥ 100 % on the 4th and 5th tick; the 6th tick decays below 91 % and releases.
    b.tickInner(1000, 0, 10);
    b.tickInner(1000, 0, 10);
    try testing.expect(b.backstopActive());
    b.tickInner(1000, 0, 10);
    try testing.expect(!b.backstopActive());
    // The run resets on any tick whose window max is below the budget. With
    // a constant budget the window itself keeps the max ≥ budget for 5
    // ticks, so only a budget move (a raised cage) can show the reset.
    var b2: MemBudget = .{};
    b2.tickInner(1000, 0, 1100); // over = 1
    b2.tickInner(2000, 0, 10); // 1100 < 2000 ⇒ run reset
    b2.tickInner(2000, 0, 10);
    b2.tickInner(2000, 0, 10);
    b2.tickInner(1000, 0, 10); // the window still holds 1100 ⇒ over = 1
    try testing.expect(!b2.backstopActive());
    b2.tickInner(1000, 0, 10); // the spike aged out ⇒ reset again
    try testing.expect(!b2.backstopActive());
    try testing.expectEqual(@as(u64, 0), b2.hardBackstops());
}

test "Red convergence under an admission storm: every tick sheds to ≤ 85 %, the backstop pins the floor" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var storm = TestComp.init("storm", 0, &shed_log);
    b.register(storm.clamping(100, 1));
    var post_shed: [6]u64 = undefined;
    for (&post_shed) |*p| {
        storm.set(storm.get() + 400);
        b.tickInner(1000, 0, 1200);
        p.* = storm.get();
    }
    try testing.expectEqual(Level.red, b.level());
    for (post_shed) |v| try testing.expect(v <= 850);
    try testing.expect(b.hardBackstops() >= 1);
    try testing.expectEqual(@as(u64, 100), storm.get());
}

test "kernel_reclaimable components count in gauge_sum but never in pressure" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var anon = TestComp.init("anon_ish", 400, &shed_log);
    var tier = TestComp.init("tier_mmap", 5000, &shed_log);
    b.register(anon.recording(0, 1));
    b.register(tier.inert(0, 0).kernelReclaimable());
    b.tickInner(1000, 0, 0);
    try testing.expectEqual(Level.green, b.level());
    try testing.expectEqual(@as(u64, 400), b.pressureBytes());
    try testing.expectEqual(@as(u64, 5400), b.gaugeSumBytes());
    b.tickInner(1000, 0, 990);
    try testing.expectEqual(Level.red, b.level());
}

test "memory.stat parser sums exactly the unreclaimable set; junk and empty degrade to 0" {
    const fixture =
        \\anon 1048576
        \\file 8388608
        \\kernel 262144
        \\kernel_stack 65536
        \\pagetables 131072
        \\shmem 4096
        \\file_mapped 2097152
        \\file_dirty 524288
        \\file_writeback 262144
        \\anon_thp 0
        \\inactive_anon 1000000
        \\active_anon 48576
        \\inactive_file 6000000
        \\active_file 2388608
        \\unevictable 8192
        \\slab_reclaimable 100000
        \\slab_unreclaimable 200000
        \\slab 300000
        \\workingset_refault_anon 5
        \\pgscan 12345
        \\
    ;
    try testing.expectEqual(@as(u64, 1048576 + 524288 + 262144 + 4096 + 8192 + 200000), parseUnreclaimableMemoryStat(fixture));
    try testing.expectEqual(@as(u64, 0), parseUnreclaimableMemoryStat(""));
    try testing.expectEqual(@as(u64, 12), parseUnreclaimableMemoryStat("anon garbage\nfile_dirty 12\n"));
    try testing.expectEqual(@as(u64, 0), parseUnreclaimableMemoryStat("anon\n"));
}

test "cgroup v2 path, memory.max and statm parsers" {
    try testing.expectEqualStrings("/user.slice/x", parseCgroupV2Path("12:memory:/foo\n0::/user.slice/x\n").?);
    try testing.expectEqualStrings("/", parseCgroupV2Path("0::/\n").?);
    try testing.expectEqual(@as(?[]const u8, null), parseCgroupV2Path("12:memory:/foo\n"));
    try testing.expectEqual(@as(?[]const u8, null), parseCgroupV2Path(""));
    try testing.expectEqual(@as(?u64, null), parseMemoryMax("max\n"));
    try testing.expectEqual(@as(?u64, 17179869184), parseMemoryMax("17179869184\n"));
    try testing.expectEqual(@as(?u64, null), parseMemoryMax("garbage"));
    try testing.expectEqual(@as(u64, 307), parseStatmResidentPages("605 307 285 7 0 119 0\n"));
    try testing.expectEqual(@as(u64, 0), parseStatmResidentPages("605"));
    try testing.expectEqual(@as(u64, 0), parseStatmResidentPages(""));
}

// ---- §5 stats surface ----

test "stats JSON carries every spec key with live values" {
    var b: MemBudget = .{};
    var shed_log: ShedLog = .{};
    var g = TestComp.init("hot_block_tier", 960, &shed_log);
    b.register(g.recording(100, 4));
    b.tickInner(1000, 0, 0);
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try b.writeStatsJson(&w);
    var parsed = try std.json.parseFromSlice(std.json.Value, testing.allocator, w.buffered(), .{});
    defer parsed.deinit();
    const o = parsed.value.object;
    try testing.expectEqual(@as(i64, 1000), o.get("mem_budget_bytes").?.integer);
    try testing.expectEqual(@as(i64, 960), o.get("mem_budget_pressure_bytes").?.integer);
    try testing.expectEqual(@as(i64, 960), o.get("mem_budget_gauge_sum_bytes").?.integer);
    try testing.expectEqual(@as(i64, 2), o.get("mem_budget_level").?.integer);
    try testing.expectEqual(@as(i64, 1), o.get("mem_budget_yellow_events").?.integer);
    try testing.expectEqual(@as(i64, 1), o.get("mem_budget_red_events").?.integer);
    try testing.expectEqual(false, o.get("mem_budget_floors_clamped").?.bool);
    try testing.expectEqual(@as(i64, 0), o.get("mem_budget_unreclaimable_bytes").?.integer);
    try testing.expectEqual(@as(i64, 0), o.get("mem_budget_hard_backstops").?.integer);
    try testing.expectEqual(false, o.get("mem_budget_backstop_active").?.bool);
    try testing.expectEqual(false, o.get("mem_budget_tier_publish_paused").?.bool);
    try testing.expectEqual(@as(i64, 1), o.get("mem_budget_ticks").?.integer);
    const comps = o.get("mem_budget_components").?.object;
    const hot = comps.get("hot_block_tier").?.object;
    try testing.expectEqual(@as(i64, 960), hot.get("current").?.integer);
    try testing.expectEqual(@as(i64, 100), hot.get("floor").?.integer);
    try testing.expectEqual(@as(i64, 4), hot.get("weight").?.integer);
    try testing.expectEqual(@as(i64, 1), hot.get("sheds").?.integer);
    try testing.expectEqual(@as(i64, 0), hot.get("trims").?.integer);
}

// ---- §6 knob faces ----

/// A census fixture in a tmp dir: `proc/` and `cgroup/` trees holding this
/// process (pid 4242, comm `squeezefs-unit`, in `cage`) plus `others` — the
/// census tests' ONLY process source, so no co-tenant on the box (another
/// agent's unit run) moves a share they pin. `init` in place: `roots()`
/// slices the fixture's own buffers.
const CensusFixture = struct {
    tmp: testing.TmpDir,
    proc_buf: [std.Io.Dir.max_path_bytes]u8,
    proc_len: usize,
    cgroup_buf: [std.Io.Dir.max_path_bytes]u8,
    cgroup_len: usize,

    const self_pid: u32 = 4242;
    const comm = "squeezefs-unit";
    const cage = "/sqz.slice/a.service";
    const cage_procs = "cgroup" ++ cage ++ "/cgroup.procs";

    const Proc = struct {
        pid: u32,
        comm: []const u8 = comm,
        /// The stat state; null = listed in `cgroup.procs` but reaped before
        /// the census read its stat (no `/proc` entry).
        state: ?u8 = 'S',
        cgroup: []const u8 = cage,
    };

    fn init(fx: *CensusFixture, others: []const Proc) !void {
        fx.tmp = testing.tmpDir(.{});
        errdefer fx.tmp.cleanup();
        var root_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
        const root = root_buf[0..try fx.tmp.dir.realPath(testing.io, &root_buf)];
        fx.proc_len = (try std.fmt.bufPrint(&fx.proc_buf, "{s}/proc", .{root})).len;
        fx.cgroup_len = (try std.fmt.bufPrint(&fx.cgroup_buf, "{s}/cgroup", .{root})).len;
        // The kernel's `/proc/self` links to `/proc/<pid>`; a copy reads the same.
        try fx.writeStat("self", self_pid, comm, 'R');
        try fx.writeStat(std.fmt.comptimePrint("{d}", .{self_pid}), self_pid, comm, 'R');
        try fx.tmp.dir.writeFile(testing.io, .{ .sub_path = "proc/self/cgroup", .data = "0::" ++ cage ++ "\n" });
        for (others) |p| if (p.state) |state| {
            var name_buf: [16]u8 = undefined;
            try fx.writeStat(try std.fmt.bufPrint(&name_buf, "{d}", .{p.pid}), p.pid, p.comm, state);
        };
        try fx.writeProcs(cage, true, others);
        for (others, 0..) |p, i| {
            if (std.mem.eql(u8, p.cgroup, cage)) continue;
            const first = for (others[0..i]) |q| {
                if (std.mem.eql(u8, q.cgroup, p.cgroup)) break false;
            } else true;
            if (first) try fx.writeProcs(p.cgroup, false, others);
        }
    }

    fn deinit(fx: *CensusFixture) void {
        fx.tmp.cleanup();
    }

    fn roots(fx: *const CensusFixture) CensusRoots {
        return .{ .proc = fx.proc_buf[0..fx.proc_len], .cgroup = fx.cgroup_buf[0..fx.cgroup_len] };
    }

    fn writeStat(fx: *CensusFixture, dir: []const u8, pid: u32, c: []const u8, state: u8) !void {
        var path_buf: [64]u8 = undefined;
        try fx.tmp.dir.createDirPath(testing.io, try std.fmt.bufPrint(&path_buf, "proc/{s}", .{dir}));
        var text_buf: [256]u8 = undefined;
        const text = try std.fmt.bufPrint(&text_buf, "{d} ({s}) {c} 1 {d} {d} 0 -1 4194560 211 0 0 0\n", .{ pid, c, state, pid, pid });
        try fx.tmp.dir.writeFile(testing.io, .{ .sub_path = try std.fmt.bufPrint(&path_buf, "proc/{s}/stat", .{dir}), .data = text });
    }

    /// `cgroup<cgroup>/cgroup.procs`: every `others` pid in that cgroup, this
    /// process's first when `with_self`.
    fn writeProcs(fx: *CensusFixture, cgroup: []const u8, with_self: bool, others: []const Proc) !void {
        var text_buf: [1024]u8 = undefined;
        var w: std.Io.Writer = .fixed(&text_buf);
        if (with_self) try w.print("{d}\n", .{self_pid});
        for (others) |p| if (std.mem.eql(u8, p.cgroup, cgroup)) try w.print("{d}\n", .{p.pid});
        var dir_buf: [128]u8 = undefined;
        const dir = try std.fmt.bufPrint(&dir_buf, "cgroup{s}", .{std.mem.trimEnd(u8, cgroup, "/")});
        try fx.tmp.dir.createDirPath(testing.io, dir);
        var file_buf: [160]u8 = undefined;
        try fx.tmp.dir.writeFile(testing.io, .{ .sub_path = try std.fmt.bufPrint(&file_buf, "{s}/cgroup.procs", .{dir}), .data = w.buffered() });
    }
};

test "knob faces: SQUEEZEFS_MEM_BUDGET_MB wins verbatim and undivided; absent falls to the derived tier" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var b: MemBudget = .{};
    // One co-tenant daemon in the fixture's cage: a census of 2.
    var fx: CensusFixture = undefined;
    try fx.init(&.{.{ .pid = 100 }});
    defer fx.deinit();
    const host = Host{ .io = testing.io, .env = &env, .facts = derive.Facts.synthetic(8, 10 * GiB), .census = fx.roots() };
    try testing.expect(!b.explicitBudgetEngaged(&env));
    // The derived tier divides the system roots by the census (off a cgroup 70 % of RAM; on Linux the venue's cage may win; both are the pure form).
    try testing.expectEqual(resolveBudgetFromShared(null, null, readCgroupMemoryMax(testing.io), 10 * GiB, 2), b.resolveBudgetNow(host));
    try env.put("SQUEEZEFS_MEM_BUDGET_MB", "3072");
    try testing.expect(b.explicitBudgetEngaged(&env));
    // Undivided by the census's 2 and by the seam's 4.
    try testing.expectEqual(3 * GiB, b.resolveBudgetNow(host));
    try env.put("SQUEEZEFS_TEST_FLEET_SHARE", "4");
    try testing.expectEqual(3 * GiB, b.resolveBudgetNow(host));
    // The flag beats the env.
    b.setFlagBudget(2 * GiB);
    try testing.expectEqual(2 * GiB, b.resolveBudgetNow(host));
    b.setFlagBudget(0);
    try testing.expectEqual(3 * GiB, b.resolveBudgetNow(host));
    // A live tick publishes the resolved budget.
    b.tick(host);
    try testing.expectEqual(3 * GiB, b.budgetBytes());
}

test "knob faces: the fleet share is the census unless the SQUEEZEFS_TEST_FLEET_SHARE seam pins it; out of range is ignored loudly" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var fx: CensusFixture = undefined;
    try fx.init(&.{ .{ .pid = 100 }, .{ .pid = 101 } });
    defer fx.deinit();
    // Absent: the census — the fixture's three daemons.
    try testing.expectEqual(@as(u32, 3), fleetShare(testing.io, &env, fx.roots()));
    try env.put("SQUEEZEFS_TEST_FLEET_SHARE", "4");
    try testing.expectEqual(@as(u32, 4), fleetShare(testing.io, &env, fx.roots()));
    try env.put("SQUEEZEFS_TEST_FLEET_SHARE", "0");
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(u32, 3), fleetShare(testing.io, &env, fx.roots()));
    try log.testing.expectAllFired();
}

test "fleet census over fixtures: 0, 1, 2 and 8 co-tenant daemons are a share of 1, 2, 3 and 9" {
    for ([_]u32{ 0, 1, 2, 8 }) |co_tenants| {
        var others: [8]CensusFixture.Proc = undefined;
        for (others[0..co_tenants], 0..) |*p, i| p.* = .{ .pid = 100 + @as(u32, @intCast(i)) };
        var fx: CensusFixture = undefined;
        try fx.init(others[0..co_tenants]);
        defer fx.deinit();
        try testing.expectEqual(@as(?u32, co_tenants + 1), censusFleetShare(testing.io, fx.roots(), CensusFixture.cage));
        try testing.expectEqual(co_tenants + 1, readFleetShare(testing.io, fx.roots()));
    }
}

test "fleet census over a fixture: a zombie, a reaped pid, another comm and our comm in another cgroup never count" {
    var fx: CensusFixture = undefined;
    try fx.init(&.{
        .{ .pid = 100 }, // a co-tenant daemon: counts
        .{ .pid = 101, .state = 'Z' }, // exited, unreaped: listed in the window between the two reads
        .{ .pid = 102, .state = 'X' }, // dead
        .{ .pid = 103, .state = null }, // reaped between the two reads: no /proc entry
        .{ .pid = 104, .comm = "bash" },
        .{ .pid = 105, .comm = "squeezefs-unit2" },
        .{ .pid = 106, .cgroup = "/other.slice/b.service" }, // our comm, another cage
    });
    defer fx.deinit();
    try testing.expectEqual(@as(?u32, 2), censusFleetShare(testing.io, fx.roots(), CensusFixture.cage));
    try testing.expectEqual(@as(u32, 2), readFleetShare(testing.io, fx.roots()));
    // A trailing slash names the same cage.
    try testing.expectEqual(@as(?u32, 2), censusFleetShare(testing.io, fx.roots(), CensusFixture.cage ++ "/"));
    // The root cgroup (`0::/`, the venue container's own line) reads `<cgroup>/cgroup.procs`.
    try fx.tmp.dir.writeFile(testing.io, .{ .sub_path = "proc/self/cgroup", .data = "0::/\n" });
    try fx.tmp.dir.writeFile(testing.io, .{ .sub_path = "cgroup/cgroup.procs", .data = "100\n4242\n106\n101\n" });
    try testing.expectEqual(@as(u32, 3), readFleetShare(testing.io, fx.roots()));
}

test "fleet census over a fixture: a list that is unreadable or does not list this process is null (share 1, warned); off-cgroup is 1" {
    var fx: CensusFixture = undefined;
    try fx.init(&.{ .{ .pid = 100 }, .{ .pid = 101 } });
    defer fx.deinit();
    try testing.expectEqual(@as(?u32, 3), censusFleetShare(testing.io, fx.roots(), CensusFixture.cage));
    // A list without this process is a view of another cage or pid namespace: its count means nothing here.
    try fx.tmp.dir.writeFile(testing.io, .{ .sub_path = CensusFixture.cage_procs, .data = "100\n101\n" });
    try testing.expectEqual(@as(?u32, null), censusFleetShare(testing.io, fx.roots(), CensusFixture.cage));
    try testing.expectEqual(@as(u32, 1), readFleetShare(testing.io, fx.roots()));
    try fx.tmp.dir.deleteFile(testing.io, CensusFixture.cage_procs);
    try testing.expectEqual(@as(?u32, null), censusFleetShare(testing.io, fx.roots(), CensusFixture.cage));
    try testing.expectEqual(@as(u32, 1), readFleetShare(testing.io, fx.roots()));
    // Off-cgroup (no `0::` line, as off-Linux): 1 — the cage, if any, already divides.
    try fx.tmp.dir.writeFile(testing.io, .{ .sub_path = "proc/self/cgroup", .data = "4:memory:/x\n" });
    try testing.expectEqual(@as(u32, 1), readFleetShare(testing.io, fx.roots()));
    try fx.tmp.dir.deleteFile(testing.io, "proc/self/cgroup");
    try testing.expectEqual(@as(u32, 1), readFleetShare(testing.io, fx.roots()));
}

test "proc stat head: the comm ends at the LAST `) ` (a comm may hold spaces and parentheses); any other shape is null" {
    const st = parseProcStat("4242 (squeezefs-unit) S 1 4242 4242 0 -1\n").?;
    try testing.expectEqualStrings("4242", st.pid);
    try testing.expectEqualStrings("squeezefs-unit", st.comm);
    try testing.expectEqual(@as(u8, 'S'), st.state);
    const odd = parseProcStat("7 (a) (b) c) Z 1 7").?;
    try testing.expectEqualStrings("a) (b) c", odd.comm);
    try testing.expectEqual(@as(u8, 'Z'), odd.state);
    try testing.expectEqualStrings("", parseProcStat("9 () R 1").?.comm);
    for ([_][]const u8{ "", "4242", "4242 (squeezefs-unit)", "4242 (squeezefs-unit) ", "(x) S 1", "1 (x S 1" }) |bad| {
        try testing.expect(parseProcStat(bad) == null);
    }
}

test "cgroup cage walks the ancestor chain: a slice cap binds over an uncapped leaf; memory.high tightens; the binding level is named" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    var root_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];
    try tmp.dir.createDirPath(testing.io, "sqz.slice/a.service");
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "memory.max", .data = "max\n" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "sqz.slice/memory.max", .data = "17179869184\n" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "sqz.slice/a.service/memory.max", .data = "max\n" });
    var level_buf: [2][std.Io.Dir.max_path_bytes]u8 = undefined;
    // The leaf alone reads uncapped; the walk finds the slice's 16 GiB.
    const slice_bound = walkCgroupCage(testing.io, root, "/sqz.slice/a.service", &level_buf);
    try testing.expectEqual(@as(?u64, 16 * GiB), slice_bound.max);
    try testing.expectEqual(@as(?u64, null), slice_bound.high);
    try testing.expectEqual(@as(?u64, 16 * GiB), slice_bound.effective());
    try testing.expectEqualStrings("/sqz.slice", slice_bound.max_level);
    try testing.expectEqualStrings("memory.max", slice_bound.boundBy().?.file);
    // A tighter leaf cap wins over the slice.
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "sqz.slice/a.service/memory.max", .data = "4294967296\n" });
    const leaf_bound = walkCgroupCage(testing.io, root, "/sqz.slice/a.service", &level_buf);
    try testing.expectEqual(@as(?u64, 4 * GiB), leaf_bound.effective());
    try testing.expectEqualStrings("/sqz.slice/a.service", leaf_bound.max_level);
    // memory.high at the root tightens below every max: it is the kernel's own Yellow.
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "memory.high", .data = "2147483648\n" });
    const high_bound = walkCgroupCage(testing.io, root, "/sqz.slice/a.service", &level_buf);
    try testing.expectEqual(@as(?u64, 4 * GiB), high_bound.max);
    try testing.expectEqual(@as(?u64, 2 * GiB), high_bound.high);
    try testing.expectEqual(@as(?u64, 2 * GiB), high_bound.effective());
    try testing.expectEqualStrings("", high_bound.high_level);
    try testing.expectEqualStrings("memory.high", high_bound.boundBy().?.file);
    // The root cgroup itself ("/") and a trailing slash read the same tree; junk is "no cap here".
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "sqz.slice/memory.high", .data = "garbage\n" });
    try testing.expectEqual(@as(?u64, 2 * GiB), walkCgroupCage(testing.io, root, "/sqz.slice/a.service/", &level_buf).effective());
    try testing.expectEqual(@as(?u64, 2 * GiB), walkCgroupCage(testing.io, root, "/", &level_buf).effective());
    // Uncapped everywhere: null, so the budget falls to 70 % RAM — never silently from a capped slice.
    var tmp2 = testing.tmpDir(.{});
    defer tmp2.cleanup();
    var root2_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    const root2_len = try tmp2.dir.realPath(testing.io, &root2_buf);
    try tmp2.dir.createDirPath(testing.io, "x");
    try tmp2.dir.writeFile(testing.io, .{ .sub_path = "x/memory.max", .data = "max\n" });
    const none = walkCgroupCage(testing.io, root2_buf[0..root2_len], "/x", &level_buf);
    try testing.expectEqual(@as(?u64, null), none.effective());
    try testing.expect(none.boundBy() == null);
    // The resolution tie: the slice-bound cage × 0.8, not 70 % of RAM.
    try testing.expectEqual(derive.tenths(16 * GiB, 8), resolveBudgetFromShared(null, null, slice_bound.effective(), 320 * GiB, 1));
}

test "fleet share mount refusal: share 1 or an explicit tier stands down; a divided root below the floor refuses" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var buf: sizing.RefusalBuf = undefined;
    var b: MemBudget = .{};
    var solo: CensusFixture = undefined;
    try solo.init(&.{});
    defer solo.deinit();
    var paired: CensusFixture = undefined;
    try paired.init(&.{.{ .pid = 100 }});
    defer paired.deinit();
    // 4096 possible CPUs × 4 × 1 MiB = 16 GiB floor against a 1 GiB RAM root (the venue's 16 GiB cage × 0.8 ÷ 2 is below it too).
    var tiny = Host{ .io = testing.io, .env = &env, .facts = .{ .cpus = 4096, .possible_cpus = 4096, .page_size = 4096, .total_ram = GiB }, .census = solo.roots() };
    // Alone in the cage: share 1 stands down.
    try testing.expectEqual(@as(?[]const u8, null), b.fleetShareMountRefusal(tiny, &buf));
    // A co-tenant in the census divides the root below the floor.
    tiny.census = paired.roots();
    try testing.expect(b.fleetShareMountRefusal(tiny, &buf) != null);
    tiny.census = solo.roots();
    try env.put("SQUEEZEFS_TEST_FLEET_SHARE", "2");
    try testing.expect(b.fleetShareMountRefusal(tiny, &buf) != null);
    try env.put("SQUEEZEFS_MEM_BUDGET_MB", "1");
    try testing.expectEqual(@as(?[]const u8, null), b.fleetShareMountRefusal(tiny, &buf));
}

// ---- Linux readers and the sampler thread ----

test "linux: cgroup/proc readers return plausible values" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "budget: Linux only");
    const page: u64 = derive.facts().page_size;
    const rss = readRssBytes(testing.io, page);
    try testing.expect(rss > 0);
    try testing.expectEqual(@as(u64, 0), rss % page);
    // The venue kernel is cgroup-v2 only: the unified line is always present.
    var cg_buf: [4096]u8 = undefined;
    try testing.expect(parseCgroupV2Path(readSmallFile(testing.io, "/proc/self/cgroup", &cg_buf).?) != null);
    // A capped cage reads as a positive byte count (the venue runs under a
    // 16 GiB cap); an uncapped one as null. Either way the unreclaimable set
    // of a running process is non-zero (its own anon pages) and plausible.
    if (readCgroupMemoryMax(testing.io)) |max| try testing.expect(max > 0);
    // The ONE live census read: other agents' unit runs share the venue's
    // cage, so only what holds under any co-tenancy is asserted. Non-null =
    // this process's own pid was listed and counted, so N >= 1.
    const n = censusFleetShare(testing.io, .{}, parseCgroupV2Path(readSmallFile(testing.io, "/proc/self/cgroup", &cg_buf).?).?).?;
    try testing.expect(n >= 1);
    const unreclaimable = readCgroupUnreclaimable(testing.io).?;
    try testing.expect(unreclaimable > 0 and unreclaimable < derive.TiB);
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var b: MemBudget = .{};
    const facts = derive.Facts.probe();
    const host = Host{ .io = testing.io, .env = &env, .facts = facts };
    // The derived tier for the N it read is exactly the system root ÷ N form.
    try testing.expectEqual(resolveBudgetFromShared(null, null, readCgroupMemoryMax(testing.io), facts.total_ram, n), b.resolveBudgetShared(host, n));
    try testing.expect(b.resolveBudgetNow(host) > 0);
    b.tick(host);
    try testing.expect(b.budgetBytes() > 0);
    // The tick re-samples RSS; it cannot have halved between two reads.
    try testing.expect(b.pressureBytes() >= rss / 2);
    try testing.expect(b.unreclaimableBytes() > 0);
}

test "sampler thread: idempotent spawn, ticks on its period, stops" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_MEM_BUDGET_MB", "1");
    var fx: CensusFixture = undefined;
    try fx.init(&.{});
    defer fx.deinit();
    var b: MemBudget = .{};
    const host = Host{ .io = testing.io, .env = &env, .facts = derive.Facts.synthetic(2, GiB), .census = fx.roots() };
    try testing.expect(try b.spawnSampler(host, 1 * std.time.ns_per_ms));
    try testing.expect(!try b.spawnSampler(host, 1 * std.time.ns_per_ms));
    // Bounded wait for two ticks: a wait on a condition, not a timed assumption.
    const deadline = std.time.ns_per_s * 30;
    const t0 = std.Io.Timestamp.now(testing.io, .awake);
    while (b.rss_idx.load(.monotonic) < 2) {
        try std.Thread.yield();
        if (t0.durationTo(.now(testing.io, .awake)).nanoseconds > deadline) return error.SamplerNeverTicked;
    }
    b.stopSampler(testing.io);
    try testing.expectEqual(1 * MiB, b.budgetBytes());
    try testing.expect(@backingInt(b.level()) <= 2);
}

/// A source that counts its reads (the seam's tick-at-stop contract).
const CountingSource = struct {
    reads: std.atomic.Value(u64) = .init(0),

    fn current(ctx: *anyopaque) u64 {
        const self: *CountingSource = @ptrCast(@alignCast(ctx));
        _ = self.reads.fetchAdd(1, .monotonic);
        return 0;
    }
};

test "sampler thread: SQUEEZEFS_TEST_MEM_SAMPLER_TICK_ON_STOP reads every source exactly once more at the stop, after join nothing reads; absent, the stop reads nothing" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    // A budget no test process's RSS reaches: the tick stays Green, so the
    // sum pass is the only read (Red's shed pass would read again).
    try env.put("SQUEEZEFS_MEM_BUDGET_MB", "1048576");
    // A period no test outlives: the only tick is the stop's.
    const never: u64 = 3600 * std.time.ns_per_s;
    var fx: CensusFixture = undefined;
    try fx.init(&.{});
    defer fx.deinit();
    {
        var src: CountingSource = .{};
        var b: MemBudget = .{};
        b.register(Component.init("counting", 0, 0, .{ .func = .{ .ctx = &src, .call = CountingSource.current } }, null));
        const host = Host{ .io = testing.io, .env = &env, .facts = derive.Facts.synthetic(2, GiB), .census = fx.roots() };
        try testing.expect(try b.spawnSampler(host, never));
        b.stopSampler(testing.io);
        try testing.expectEqual(@as(u64, 0), src.reads.load(.monotonic));
    }
    try env.put("SQUEEZEFS_TEST_MEM_SAMPLER_TICK_ON_STOP", "1");
    {
        var src: CountingSource = .{};
        var b: MemBudget = .{};
        b.register(Component.init("counting", 0, 0, .{ .func = .{ .ctx = &src, .call = CountingSource.current } }, null));
        const host = Host{ .io = testing.io, .env = &env, .facts = derive.Facts.synthetic(2, GiB), .census = fx.roots() };
        try testing.expect(try b.spawnSampler(host, never));
        b.stopSampler(testing.io);
        // The stop's tick ran (the budget published) and read the source once
        // (one sum pass; Green sheds nothing); the join means no later read.
        try testing.expectEqual(1048576 * MiB, b.budgetBytes());
        try testing.expectEqual(Level.green, b.level());
        try testing.expectEqual(@as(u64, 1), src.reads.load(.monotonic));
    }
}

fn procTextFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [1024]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    _ = parseUnreclaimableMemoryStat(text);
    if (parseCgroupV2Path(text)) |p| try std.testing.expect(p.len <= text.len);
    _ = parseMemoryMax(text);
    _ = parseStatmResidentPages(text);
    // pid + ` (` + comm + `) ` + state: every field a disjoint slice of the text.
    if (parseProcStat(text)) |st| try std.testing.expect(st.pid.len + st.comm.len + 5 <= text.len);
}

test "fuzz: the cgroup and proc text readers are total on arbitrary text" {
    try fuzzkit.fuzz({}, procTextFuzz, .{ .corpus = &.{ fuzzkit.slice("anon 4096\nkernel_stack 8192\nslab_unreclaimable 1\n"), fuzzkit.slice("0::/user.slice/x\n"), fuzzkit.slice("max\n"), fuzzkit.slice("12 34 5 6 0 7 0\n"), fuzzkit.slice("4242 (squeezefs-unit) S 1 4242 4242 0 -1\n") } });
}
