//! Lane placement (charter §9 "per-core FUSE handler lanes … handoffs target
//! the lane homed on the destination CPU/node"; §1.4 NUMA over arbitrary
//! domain counts; `docs/porting/exec.md` §1.4(a), §3.4; decision 0003 §D
//! Q-C): the machine's CPU/NUMA map, which lane is homed where, and the walk
//! that picks a lane for a spawn.
//!
//! Everything here is pure over a `Topology` value so synthetic machines
//! test it; the Linux probe (`Topology.probe`) is the only reader of
//! sysfs/procfs and is gated inside.
//!
//! The core reservation (Q-C): the reference dropped CPU 0 by index whenever
//! the mask had > 1 CPU (`session.rs` L5955). Ours is DERIVED: when the
//! kernel was told which CPUs are isolated (`isolcpus=` / `nohz_full=` on the
//! command line, mirrored in `/sys/devices/system/cpu/{isolated,nohz_full}`)
//! the lanes take exactly the isolated CPUs of the mask and every other CPU is
//! housekeeping; when nothing is isolated and the mask has > 1 CPU, the
//! lowest CPU of the MASK (not "CPU 0") is left to the kernel's housekeeping
//! (the boot CPU takes unbound IRQs, timers and RCU callbacks by default); a
//! single-CPU mask reserves nothing — a lane count of 0 parks every caller.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");
const assert = std.debug.assert;
const Io = std.Io;
const knobs = @import("../core/knobs.zig");
const derive = @import("../core/derive.zig");
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");

/// The kernel's `cpu_set_t` as std declares it is 128 bytes = 1024 CPUs
/// (`linux.CPU_SETSIZE`); every set here shares that ceiling so a mask
/// round-trips `sched_{get,set}affinity` without truncation.
pub const max_cpus = 1024;
pub const CpuSet = std.bit_set.ArrayBitSet(usize, max_cpus);
pub const no_node: u16 = std.math.maxInt(u16);
pub const no_lane: u16 = std.math.maxInt(u16);

/// `SQUEEZEFS_FUSE_PIN_SCOPE`: `node` (default — the serve-latency
/// decomposition convicted the pinned-thread runqueue hostage: `queue_wait`
/// 1.55 + `dispatch_lag` 1.70 ms/op on cold reads, ~99 % collapse with
/// widening, `affinity.rs` L4–12) or `core` (the pre-campaign hard pin, the
/// A/B control arm).
pub const PinScope = enum { node, core };

pub const Config = struct {
    scope: PinScope,
    /// `SQUEEZEFS_NUMA=0`: no affinity call is made; node groups are still
    /// computed so the instrument stays live (the A/B lever).
    numa: bool,

    pub fn fromEnv(env: *const std.process.Environ.Map) Config {
        const scope_s = knobs.enumKnob("SQUEEZEFS_FUSE_PIN_SCOPE", env);
        return .{
            .scope = if (std.mem.eql(u8, scope_s, "core")) .core else .node,
            .numa = knobs.boolKnob("SQUEEZEFS_NUMA", env),
        };
    }
};

/// The machine: the process mask, the NUMA map, the isolated set.
pub const Topology = struct {
    /// CPUs the PROCESS may run on (`sched_getaffinity(pid)` — by pid, never
    /// the calling thread's: the Hang-1 lesson, 0002 row 22).
    mask: CpuSet,
    /// The NUMA node of each CPU (`no_node` when sysfs names none).
    node_of: [max_cpus]u16,
    /// Dense node count (ids `0..node_count`).
    node_count: u16,
    /// `isolcpus` ∪ `nohz_full`: the kernel's own partition.
    isolated: CpuSet,

    pub const empty: Topology = .{ .mask = .empty, .node_of = @splat(no_node), .node_count = 0, .isolated = .empty };

    /// A synthetic machine: `nodes` lists each node's cpulist text; `mask`
    /// and `isolated` are cpulists too (null mask = every listed CPU).
    pub fn synthetic(nodes: []const []const u8, mask: ?[]const u8, isolated: ?[]const u8) Topology {
        var t: Topology = .empty;
        for (nodes, 0..) |cl, node| {
            const set = parseCpuSet(cl) orelse CpuSet.empty;
            var it = set.iterator(.{});
            while (it.next()) |cpu| {
                t.node_of[cpu] = @intCast(node);
                if (mask == null) t.mask.set(cpu);
            }
        }
        t.node_count = @intCast(nodes.len);
        if (mask) |m| t.mask = parseCpuSet(m) orelse CpuSet.empty;
        if (isolated) |iso| t.isolated = parseCpuSet(iso) orelse CpuSet.empty;
        return t;
    }

    /// The CPUs of one node intersected with `within`.
    pub fn nodeCpus(self: *const Topology, node: u16, within: CpuSet) CpuSet {
        var out = CpuSet.empty;
        var it = within.iterator(.{});
        while (it.next()) |cpu| if (self.node_of[cpu] == node) out.set(cpu);
        return out;
    }

    /// The lane CPUs after the derived reservation (module doc).
    pub fn laneCpus(self: *const Topology) CpuSet {
        const iso = self.isolated.intersectWith(self.mask);
        if (iso.count() > 0) return iso;
        var out = self.mask;
        if (out.count() > 1) out.unset(out.findFirstSet().?);
        return out;
    }

    /// The live machine (Linux): the pid mask, `/sys/devices/system/node/node*/cpulist`,
    /// `/sys/devices/system/cpu/{isolated,nohz_full}` with `/proc/cmdline` as
    /// the second source of the partition. Sysfs files absent (no NUMA, no
    /// isolation) leave their part empty — never an error.
    pub fn probe(io: Io) Topology {
        if (builtin.os.tag != .linux) return .empty;
        var t: Topology = .empty;
        t.mask = processMaskLinux();
        var buf: [8192]u8 = undefined;
        var node: u16 = 0;
        while (node < max_nodes_probed) : (node += 1) {
            var path_buf: [96]u8 = undefined;
            const path = std.fmt.bufPrint(&path_buf, "/sys/devices/system/node/node{d}/cpulist", .{node}) catch unreachable;
            const text = readSmall(io, path, &buf) orelse break;
            const set = parseCpuSet(text) orelse continue;
            var it = set.iterator(.{});
            while (it.next()) |cpu| t.node_of[cpu] = node;
            t.node_count = node + 1;
        }
        if (readSmall(io, "/sys/devices/system/cpu/isolated", &buf)) |text| if (parseCpuSet(text)) |s| {
            t.isolated.setUnion(s);
        };
        if (readSmall(io, "/sys/devices/system/cpu/nohz_full", &buf)) |text| if (parseCpuSet(text)) |s| {
            t.isolated.setUnion(s);
        };
        if (readSmall(io, "/proc/cmdline", &buf)) |text| t.isolated.setUnion(parseCmdlineIsolation(text));
        return t;
    }

    /// Node ids are dense from 0 in sysfs; the loop stops at the first
    /// absent directory. `MAX_NUMNODES` on every shipping config is ≤ 1024.
    const max_nodes_probed: u16 = 1024;

    fn processMaskLinux() CpuSet {
        const linux = std.os.linux;
        var set: linux.cpu_set_t = undefined;
        var out = CpuSet.empty;
        if (linux.errno(linux.sched_getaffinity(linux.getpid(), @sizeOf(linux.cpu_set_t), &set)) != .SUCCESS) {
            out.set(0);
            return out;
        }
        for (set, 0..) |word, wi| {
            var bits = word;
            while (bits != 0) {
                const bit: usize = @ctz(bits);
                out.set(wi * @bitSizeOf(usize) + bit);
                bits &= bits - 1;
            }
        }
        if (out.count() == 0) out.set(0);
        return out;
    }
};

fn readSmall(io: Io, path: []const u8, buf: []u8) ?[]const u8 {
    return Io.Dir.cwd().readFile(io, path, buf) catch null;
}

/// A kernel cpulist ("0-3,8,10-11", trailing newline allowed) as a set.
/// Total: malformed input is null, an empty list an empty set (the kernel
/// writes "\n" for an empty `isolated`).
pub fn parseCpuSet(text: []const u8) ?CpuSet {
    var out = CpuSet.empty;
    const trimmed = std.mem.trim(u8, text, " \n\r\t");
    if (trimmed.len == 0) return out;
    var it = std.mem.splitScalar(u8, trimmed, ',');
    while (it.next()) |range| {
        if (range.len == 0) return null;
        if (std.mem.indexOfScalar(u8, range, '-')) |dash| {
            const lo = std.fmt.parseInt(u32, range[0..dash], 10) catch return null;
            const hi = std.fmt.parseInt(u32, range[dash + 1 ..], 10) catch return null;
            if (hi < lo or hi >= max_cpus) return null;
            var c = lo;
            while (c <= hi) : (c += 1) out.set(c);
        } else {
            const c = std.fmt.parseInt(u32, range, 10) catch return null;
            if (c >= max_cpus) return null;
            out.set(c);
        }
    }
    return out;
}

/// `isolcpus=` and `nohz_full=` from the kernel command line. `isolcpus`
/// may carry flags before the list (`isolcpus=domain,managed_irq,2-7`): the
/// flag words are skipped and the cpulist part parsed.
pub fn parseCmdlineIsolation(cmdline: []const u8) CpuSet {
    var out = CpuSet.empty;
    var words = std.mem.tokenizeAny(u8, cmdline, " \n\t");
    while (words.next()) |word| {
        const value = if (std.mem.startsWith(u8, word, "isolcpus="))
            word["isolcpus=".len..]
        else if (std.mem.startsWith(u8, word, "nohz_full="))
            word["nohz_full=".len..]
        else
            continue;
        var parts = std.mem.splitScalar(u8, value, ',');
        while (parts.next()) |part| {
            if (part.len == 0) continue;
            if (!std.ascii.isDigit(part[0])) continue;
            if (parseCpuSet(part)) |s| out.setUnion(s);
        }
    }
    return out;
}

/// `scoped_affinity_cpus` (`affinity.rs` L70–94) exactly: `core` ⇒ the home
/// alone; `node` ⇒ the available set ∩ the home's node; an empty result or
/// an unknown node ⇒ the whole available set ("freedom is the safe
/// direction").
pub fn scopedAffinityCpus(scope: PinScope, home: u16, avail: CpuSet, topo: *const Topology) CpuSet {
    switch (scope) {
        .core => {
            var s = CpuSet.empty;
            s.set(home);
            return s;
        },
        .node => {
            const node = topo.node_of[home];
            if (node == no_node) return avail;
            const s = topo.nodeCpus(node, avail);
            return if (s.count() == 0) avail else s;
        },
    }
}

/// `meta_lanes_from` (`meta_exec.rs` L43–49): a handful of mostly-parked
/// loops, not a throughput venue — 2, or 1 on a uniprocessor. `cpus` is the
/// fleet-share-divided root.
pub fn metaLanesFrom(cpus: u32) u16 {
    return if (cpus > 1) 2 else 1;
}

/// `exec_dead_lane_redispatches`: a spawn re-routed off a dead lane
/// (must-stay-0; a dead lane is already a loud event).
pub var dead_lane_redispatches: counters.ShardedCounter = .{};

/// Which lane runs what. Lanes are referenced by index; `alive` is the
/// lane's own flag (flipped false by the lane thread on exit or a dead ring).
pub const Placement = struct {
    homes: []const LaneHome,
    alive: []const *const std.atomic.Value(bool),
    lane_of_cpu: [max_cpus]u16,
    /// `node_lanes[node]` = the indices of lanes homed on that node.
    node_lanes: [][]u16,
    node_next: []std.atomic.Value(u32),
    next_idx: std.atomic.Value(u32) = .init(0),
    /// Logged once per dead lane.
    dead_logged: []std.atomic.Value(bool),

    pub const LaneHome = struct { home_cpu: ?u16, node: ?u16 };

    /// Build from the lane homes: the first lane homed on a CPU wins
    /// `lane_of_cpu` ("one lane per queue", `session.rs` L6000).
    pub fn init(gpa: std.mem.Allocator, homes: []const LaneHome, alive: []const *const std.atomic.Value(bool), node_count: u16) std.mem.Allocator.Error!Placement {
        assert(homes.len == alive.len);
        var p: Placement = .{
            .homes = homes,
            .alive = alive,
            .lane_of_cpu = @splat(no_lane),
            .node_lanes = &.{},
            .node_next = &.{},
            .dead_logged = &.{},
        };
        p.dead_logged = try gpa.alloc(std.atomic.Value(bool), homes.len);
        errdefer gpa.free(p.dead_logged);
        for (p.dead_logged) |*d| d.* = .init(false);
        var counts = try gpa.alloc(u16, node_count);
        defer gpa.free(counts);
        @memset(counts, 0);
        for (homes, 0..) |h, i| {
            if (h.home_cpu) |cpu| if (p.lane_of_cpu[cpu] == no_lane) {
                p.lane_of_cpu[cpu] = @intCast(i);
            };
            if (h.node) |n| counts[n] += 1;
        }
        p.node_lanes = try gpa.alloc([]u16, node_count);
        errdefer gpa.free(p.node_lanes);
        for (p.node_lanes, 0..) |*nl, n| {
            nl.* = try gpa.alloc(u16, counts[n]);
            counts[n] = 0;
        }
        for (homes, 0..) |h, i| if (h.node) |n| {
            p.node_lanes[n][counts[n]] = @intCast(i);
            counts[n] += 1;
        };
        p.node_next = try gpa.alloc(std.atomic.Value(u32), node_count);
        for (p.node_next) |*c| c.* = .init(0);
        return p;
    }

    pub fn deinit(self: *Placement, gpa: std.mem.Allocator) void {
        for (self.node_lanes) |nl| gpa.free(nl);
        gpa.free(self.node_lanes);
        gpa.free(self.node_next);
        gpa.free(self.dead_logged);
        self.* = undefined;
    }

    pub fn count(self: *const Placement) u16 {
        return @intCast(self.homes.len);
    }

    pub fn isAlive(self: *const Placement, lane: u16) bool {
        // acquire pairs with the lane thread's release store on exit.
        return self.alive[lane].load(.acquire);
    }

    /// `spawn_boxed_home` (`session.rs` L6127–6150): the lane homed on the
    /// requester's CPU, else round-robin within the node's group, else the
    /// global rotation — locality is a preference, never an availability
    /// constraint. Null ⇔ every lane is dead (the caller fails loud).
    pub fn pickHome(self: *Placement, cpu: ?u16, node: ?u16) ?u16 {
        if (self.homes.len == 0) return null;
        if (cpu) |c| {
            const l = self.lane_of_cpu[c];
            if (l != no_lane) return self.walkAlive(l);
        }
        if (node) |n| if (n < self.node_lanes.len and self.node_lanes[n].len > 0) {
            const group = self.node_lanes[n];
            const k = self.node_next[n].fetchAdd(1, .monotonic) % group.len;
            return self.walkAlive(group[k]);
        };
        return self.pickRotation();
    }

    /// The global round-robin (`spawn`, L6078–6098).
    pub fn pickRotation(self: *Placement) ?u16 {
        if (self.homes.len == 0) return null;
        const first: u16 = @intCast(self.next_idx.fetchAdd(1, .monotonic) % self.homes.len);
        return self.walkAlive(first);
    }

    /// `dispatch` (L6166–6197): from `first`, skip dead lanes counting and
    /// logging each (once); every lane dead ⇒ null.
    fn walkAlive(self: *Placement, first: u16) ?u16 {
        var attempt: u16 = 0;
        while (attempt < self.homes.len) : (attempt += 1) {
            const idx: u16 = @intCast((first + attempt) % self.homes.len);
            if (self.isAlive(idx)) {
                if (attempt > 0) dead_lane_redispatches.add(attempt);
                return idx;
            }
            if (!self.dead_logged[idx].swap(true, .monotonic)) {
                log.err("exec: lane {d} is dead — spawns homed on it are re-routed (exec_dead_lane_redispatches); a lane death is a dead ring, charter §1.8", .{idx});
            }
        }
        return null;
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

fn setOf(list: []const u8) CpuSet {
    return parseCpuSet(list).?;
}

test "parseCpuSet: ranges, singles, the empty list, malformed and out-of-range refused" {
    try testing.expectEqual(@as(usize, 4), setOf("0-3").count());
    try testing.expectEqual(@as(usize, 7), setOf("0-3,8,10-11\n").count());
    try testing.expect(setOf("0-3,8,10-11").isSet(10));
    try testing.expect(!setOf("0-3,8,10-11").isSet(9));
    try testing.expectEqual(@as(usize, 0), setOf("\n").count());
    try testing.expectEqual(@as(?CpuSet, null), parseCpuSet("3-1"));
    try testing.expectEqual(@as(?CpuSet, null), parseCpuSet("a"));
    try testing.expectEqual(@as(?CpuSet, null), parseCpuSet("0,,1"));
    try testing.expectEqual(@as(?CpuSet, null), parseCpuSet("1024"));
}

test "parseCmdlineIsolation: isolcpus with flags and nohz_full are unioned; other words ignored" {
    const s = parseCmdlineIsolation("BOOT_IMAGE=/vmlinuz root=/dev/sda1 isolcpus=domain,managed_irq,2-3 nohz_full=4 quiet\n");
    try testing.expectEqual(@as(usize, 3), s.count());
    try testing.expect(s.isSet(2) and s.isSet(3) and s.isSet(4));
    try testing.expectEqual(@as(usize, 0), parseCmdlineIsolation("quiet splash").count());
}

test "the derived reservation: isolated CPUs win; else the lowest of the mask goes to housekeeping when > 1; a single CPU reserves nothing" {
    // 1 node, 4 CPUs, nothing isolated: lanes on 1-3, CPU 0 is housekeeping.
    const one = Topology.synthetic(&.{"0-3"}, null, null);
    try testing.expectEqual(setOf("1-3"), one.laneCpus());
    // The mask starts above 0: the LOWEST OF THE MASK is reserved, not CPU 0.
    const shifted = Topology.synthetic(&.{"0-7"}, "4-7", null);
    try testing.expectEqual(setOf("5-7"), shifted.laneCpus());
    // An isolated partition: exactly those CPUs, nothing dropped by index.
    const iso = Topology.synthetic(&.{"0-7"}, null, "2-5");
    try testing.expectEqual(setOf("2-5"), iso.laneCpus());
    // Isolation outside the mask is ignored; the mask alone decides.
    const iso_out = Topology.synthetic(&.{"0-7"}, "0-3", "6-7");
    try testing.expectEqual(setOf("1-3"), iso_out.laneCpus());
    // A uniprocessor keeps its one CPU.
    const uni = Topology.synthetic(&.{"0"}, null, null);
    try testing.expectEqual(setOf("0"), uni.laneCpus());
}

test "scopedAffinityCpus: core pins the home; node widens to the home's node within the available set; unknown or empty widens to everything" {
    const topo = Topology.synthetic(&.{ "0-3", "4-7", "", "8-11" }, null, null);
    const avail = topo.laneCpus();
    try testing.expectEqual(setOf("5"), scopedAffinityCpus(.core, 5, avail, &topo));
    try testing.expectEqual(setOf("4-7"), scopedAffinityCpus(.node, 5, avail, &topo));
    try testing.expectEqual(setOf("1-3"), scopedAffinityCpus(.node, 1, avail, &topo));
    // CPU 20 belongs to no node: the whole available set.
    try testing.expectEqual(avail, scopedAffinityCpus(.node, 20, avail, &topo));
    // The home's node has no CPU in `avail`: widen.
    try testing.expectEqual(setOf("9"), scopedAffinityCpus(.node, 9, setOf("9"), &topo));
    try testing.expectEqual(setOf("1-3"), scopedAffinityCpus(.node, 9, setOf("1-3"), &topo));
}

test "metaLanesFrom: 2, or 1 on a uniprocessor" {
    try testing.expectEqual(@as(u16, 1), metaLanesFrom(1));
    try testing.expectEqual(@as(u16, 2), metaLanesFrom(2));
    try testing.expectEqual(@as(u16, 2), metaLanesFrom(64));
}

test "Placement over four nodes with a CPU-less node and holes: lane_of_cpu, node groups, the home walk, dead-lane re-routing, all dead" {
    const topo = Topology.synthetic(&.{ "0-1", "2-3", "", "6-7" }, "0-3,6-7", null);
    const lane_cpus = topo.laneCpus();
    try testing.expectEqual(setOf("1-3,6-7"), lane_cpus);
    var homes: [5]Placement.LaneHome = undefined;
    var alive_flags: [5]std.atomic.Value(bool) = undefined;
    var alive_ptrs: [5]*const std.atomic.Value(bool) = undefined;
    var it = lane_cpus.iterator(.{});
    var i: usize = 0;
    while (it.next()) |cpu| : (i += 1) {
        homes[i] = .{ .home_cpu = @intCast(cpu), .node = topo.node_of[cpu] };
        alive_flags[i] = .init(true);
        alive_ptrs[i] = &alive_flags[i];
    }
    var p = try Placement.init(testing.allocator, &homes, &alive_ptrs, topo.node_count);
    defer p.deinit(testing.allocator);
    try testing.expectEqual(@as(u16, 5), p.count());
    // lane_of_cpu: the lane homed on each lane CPU; reserved CPU 0 has none.
    try testing.expectEqual(no_lane, p.lane_of_cpu[0]);
    try testing.expectEqual(@as(u16, 0), p.lane_of_cpu[1]);
    try testing.expectEqual(@as(u16, 3), p.lane_of_cpu[6]);
    // Node groups: node 0 = {lane 0}, node 1 = {1, 2}, node 2 = {}, node 3 = {3, 4}.
    try testing.expectEqual(@as(usize, 1), p.node_lanes[0].len);
    try testing.expectEqual(@as(usize, 2), p.node_lanes[1].len);
    try testing.expectEqual(@as(usize, 0), p.node_lanes[2].len);
    try testing.expectEqual(@as(usize, 2), p.node_lanes[3].len);
    // The home CPU wins outright.
    try testing.expectEqual(@as(?u16, 4), p.pickHome(7, 3));
    // A reserved CPU: the node's round-robin.
    try testing.expectEqual(@as(?u16, 0), p.pickHome(0, 0));
    const a = p.pickHome(null, 1).?;
    const b = p.pickHome(null, 1).?;
    try testing.expect((a == 1 and b == 2) or (a == 2 and b == 1));
    // A CPU-less node: the global rotation.
    const before = p.next_idx.load(.monotonic);
    _ = p.pickHome(null, 2).?;
    try testing.expectEqual(before + 1, p.next_idx.load(.monotonic));
    // A dead lane re-routes, counts, logs once.
    const rc_before = dead_lane_redispatches.load();
    alive_flags[4].store(false, .release);
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(?u16, 0), p.pickHome(7, 3));
    try testing.expectEqual(@as(?u16, 0), p.pickHome(7, 3));
    try log.testing.expectAllFired();
    try testing.expectEqual(rc_before + 2, dead_lane_redispatches.load());
    // Every lane dead: null (the caller aborts loud).
    log.testing.expectErrorLogs(4);
    for (&alive_flags) |*f| f.store(false, .release);
    try testing.expectEqual(@as(?u16, null), p.pickRotation());
    try log.testing.expectAllFired();
}

test "Placement with no node information: every lane homeless, rotation only" {
    var homes = [_]Placement.LaneHome{ .{ .home_cpu = null, .node = null }, .{ .home_cpu = null, .node = null } };
    var flags = [_]std.atomic.Value(bool){ .init(true), .init(true) };
    const ptrs = [_]*const std.atomic.Value(bool){ &flags[0], &flags[1] };
    var p = try Placement.init(testing.allocator, &homes, &ptrs, 0);
    defer p.deinit(testing.allocator);
    try testing.expectEqual(@as(?u16, 0), p.pickHome(3, 9));
    try testing.expectEqual(@as(?u16, 1), p.pickHome(null, null));
    try testing.expectEqual(@as(?u16, 0), p.pickRotation());
}

test "Topology.probe on this machine yields a non-empty mask and consistent nodes" {
    const t = Topology.probe(testing.io);
    if (builtin.os.tag != .linux) {
        try testing.expectEqual(@as(usize, 0), t.mask.count());
        return;
    }
    try testing.expect(t.mask.count() >= 1);
    try testing.expectEqual(@as(usize, @intCast(derive.facts().cpus)), t.mask.count());
    var it = t.mask.iterator(.{});
    while (it.next()) |cpu| if (t.node_of[cpu] != no_node) try testing.expect(t.node_of[cpu] < t.node_count);
    try testing.expect(t.laneCpus().count() >= 1);
}

fn cpuSetTextFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    _ = parseCpuSet(text);
    _ = parseCmdlineIsolation(text);
}

test "fuzz: the cpuset and kernel-cmdline isolation parsers are total on arbitrary text" {
    try fuzzkit.fuzz({}, cpuSetTextFuzz, .{ .corpus = &.{ fuzzkit.slice("0-3,8\n"), fuzzkit.slice("quiet isolcpus=1-2,5 nohz_full=1-2 rcu_nocbs=1-2\n"), fuzzkit.slice("isolcpus=") } });
}
