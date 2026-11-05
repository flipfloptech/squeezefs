//! `daemon_cpu_ns` / `daemon_cpu_ns_by_class` (charter §9 "Thread naming is
//! part of the observability contract … attributes CPU per class";
//! `docs/porting/knobs_and_rails.md` §5.3): per-thread
//! `/proc/self/task/<tid>/schedstat` `sum_exec_runtime` folded by the comm
//! prefix table of `core/thread_name.zig` on the budget sampler only.
//! Held task directories anchor PID objects; every class receives only its
//! new runtime delta. Renames are not reuse, and a coarser/stale same-task read
//! retains the known high-water baseline. An omitted live tid retains its
//! anchor until absence is proved. Exited tasks keep their last observed CPU;
//! the CPU between the last observation and exit is the lost term.
//!
//! `Σ by_class ≤ total_ns` by construction: the per-thread fold runs BEFORE
//! `getrusage(RUSAGE_SELF)` is read.
//!
//! Portable parsers + the ledger; the `/proc` walk is Linux only.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const Io = std.Io;
const thread_name = @import("../core/thread_name.zig");

/// The charter classes plus `other` (last).
pub const class_count = thread_name.classes.len + 1;
pub const other_index: usize = class_count - 1;

pub fn classIndex(comm: []const u8) usize {
    for (thread_name.classes, 0..) |c, i| if (std.mem.startsWith(u8, comm, c.prefix)) return i;
    return other_index;
}

pub fn className(index: usize) []const u8 {
    return if (index == other_index) thread_name.other else thread_name.classes[index].class;
}

/// `/proc/<pid>/task/<tid>/schedstat`: "<sum_exec_runtime ns> <run_delay> <pcount>".
pub fn parseSchedstatNs(text: []const u8) ?u64 {
    var fields = std.mem.tokenizeAny(u8, text, " \t\r\n");
    const first = fields.next() orelse return null;
    return std.fmt.parseInt(u64, first, 10) catch null;
}

/// The `CONFIG_SCHED_INFO`-less fallback: `/proc/<pid>/task/<tid>/stat`
/// fields 14/15 (utime, stime in clock ticks) after the `(comm)` — which may
/// itself contain spaces and parentheses, so the split starts at the LAST
/// `)`. `clk_tck` is `sysconf(_SC_CLK_TCK)` (100 on every shipping kernel).
pub fn parseStatCpuNs(text: []const u8, clk_tck: u64) ?u64 {
    const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse return null;
    if (close + 2 > text.len) return null;
    var fields = std.mem.tokenizeAny(u8, text[close + 2 ..], " \t\r\n");
    // After `)` the next field is field 3 (state); utime is field 14.
    var i: usize = 3;
    while (i < 14) : (i += 1) _ = fields.next() orelse return null;
    const utime = std.fmt.parseInt(u64, fields.next() orelse return null, 10) catch return null;
    const stime = std.fmt.parseInt(u64, fields.next() orelse return null, 10) catch return null;
    const tck = if (clk_tck == 0) 100 else clk_tck;
    return (utime + stime) * (std.time.ns_per_s / tck);
}

pub const RuntimeSource = enum { schedstat, stat };
pub const RuntimeReading = struct { ns: u64, source: RuntimeSource };

/// The monotone fold over passes.
pub const Ledger = struct {
    const Live = struct {
        class: u8,
        ns: u64,
        /// Runtime prefix already charged to earlier classes in this incarnation.
        base: u64,
        incarnation: u64,
        /// The most recent source; ns may retain an earlier, finer observation.
        last_source: RuntimeSource,
        pass: u32,
        anchor: ?Io.Dir = null,
    };
    const TestWalkHook = struct {
        after: usize,
        ctx: *anyopaque,
        call: *const fn (*anyopaque) anyerror!void,
    };

    const TestTaskHook = struct {
        tid: u32,
        ctx: *anyopaque,
        call: *const fn (*anyopaque, Io.Dir) anyerror!void,
    };

    live: std.AutoHashMapUnmanaged(u32, Live) = .empty,
    retired: [class_count]u64 = @splat(0),
    pass: u32 = 0,
    next_incarnation: u64 = 0,
    /// The sampler's Io outlives every task-directory anchor.
    io: ?Io = null,
    test_walk_hook: if (builtin.is_test) ?TestWalkHook else void = if (builtin.is_test) null else {},
    test_walk_limit: if (builtin.is_test) ?usize else void = if (builtin.is_test) null else {},
    test_total_ns: if (builtin.is_test) ?u64 else void = if (builtin.is_test) null else {},
    test_runtime_source: if (builtin.is_test) ?RuntimeSource else void = if (builtin.is_test) null else {},
    test_after_cpu: if (builtin.is_test) ?TestTaskHook else void = if (builtin.is_test) null else {},

    pub fn deinit(self: *Ledger, gpa: std.mem.Allocator) void {
        var it = self.live.valueIterator();
        while (it.next()) |v| if (v.anchor) |anchor| anchor.close(self.io.?);
        self.live.deinit(gpa);
        self.* = undefined;
    }

    /// Start a pass: every tid observed before this is a candidate for
    /// retirement at `finishPass`.
    pub fn beginPass(self: *Ledger) void {
        self.pass +%= 1;
    }

    /// An incarnation is the sampler's anchored PID object, never comm or runtime.
    pub fn observe(self: *Ledger, gpa: std.mem.Allocator, tid: u32, incarnation: u64, class: usize, reading: RuntimeReading) (std.mem.Allocator.Error || error{RuntimeRegressed})!void {
        const gop = try self.live.getOrPut(gpa, tid);
        var ns = reading.ns;
        var base: u64 = 0;
        var anchor: ?Io.Dir = null;
        if (gop.found_existing) {
            const old = gop.value_ptr.*;
            if (old.incarnation == incarnation) {
                // Changed/coarse sources keep the known baseline; precise regression refuses.
                if (ns < old.ns) {
                    if (reading.source == .schedstat and old.last_source == .schedstat) return error.RuntimeRegressed;
                    ns = old.ns;
                }
                base = old.base;
                anchor = old.anchor;
                if (old.class != class) {
                    self.retired[old.class] +|= old.ns - old.base;
                    base = old.ns;
                }
            } else {
                self.retired[old.class] +|= old.ns - old.base;
                if (old.anchor) |dir| dir.close(self.io.?);
            }
        }
        gop.value_ptr.* = .{ .class = @intCast(class), .ns = ns, .base = base, .incarnation = incarnation, .last_source = reading.source, .pass = self.pass, .anchor = anchor };
    }

    fn newIncarnation(self: *Ledger) !u64 {
        if (self.next_incarnation == std.math.maxInt(u64)) return error.IncarnationExhausted;
        self.next_incarnation += 1;
        return self.next_incarnation;
    }

    /// Retire tids not marked live after the caller's absence checks.
    pub fn finishPass(self: *Ledger) void {
        var it = self.live.iterator();
        var gone: [64]u32 = undefined;
        var n: usize = 0;
        // Removing while iterating invalidates the iterator: collect, then
        // remove; more than 64 departures in one pass take further rounds.
        while (true) {
            while (it.next()) |e| {
                if (e.value_ptr.pass == self.pass) continue;
                self.retired[e.value_ptr.class] +|= e.value_ptr.ns - e.value_ptr.base;
                if (e.value_ptr.anchor) |anchor| anchor.close(self.io.?);
                gone[n] = e.key_ptr.*;
                n += 1;
                if (n == gone.len) break;
            }
            for (gone[0..n]) |tid| _ = self.live.remove(tid);
            if (n < gone.len) break;
            n = 0;
            it = self.live.iterator();
        }
    }

    /// `retired + Σ live` per class.
    pub fn sums(self: *const Ledger) [class_count]u64 {
        var out = self.retired;
        var it = self.live.valueIterator();
        while (it.next()) |v| out[v.class] +|= v.ns - v.base;
        return out;
    }
};

pub const Sample = struct {
    total_ns: u64,
    by_class: [class_count]u64,

    /// `{"daemon_cpu_ns":N,"daemon_cpu_ns_by_class":{class:ns,…,"other":ns}}`.
    pub fn writeJson(self: Sample, w: *Io.Writer) Io.Writer.Error!void {
        try w.writeByte('{');
        try self.writeFields(w);
        try w.writeByte('}');
    }

    /// The object's members without the braces (a caller adds its own
    /// beside them: the sample's age in the `.stats` document).
    pub fn writeFields(self: Sample, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("\"daemon_cpu_ns\":{d},\"daemon_cpu_ns_by_class\":{{", .{self.total_ns});
        for (self.by_class, 0..) |ns, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("\"{s}\":{d}", .{ className(i), ns });
        }
        try w.writeByte('}');
    }
};

/// `getrusage(RUSAGE_SELF)` utime + stime in ns; 0 off Linux.
pub fn processCpuNs() u64 {
    if (builtin.os.tag != .linux) return 0;
    const linux = std.os.linux;
    var ru: linux.rusage = undefined;
    if (linux.errno(linux.getrusage(linux.rusage.SELF, &ru)) != .SUCCESS) return 0;
    const u: u64 = @as(u64, @intCast(@max(ru.utime.sec, 0))) * std.time.ns_per_s + @as(u64, @intCast(@max(ru.utime.usec, 0))) * std.time.ns_per_us;
    const s: u64 = @as(u64, @intCast(@max(ru.stime.sec, 0))) * std.time.ns_per_s + @as(u64, @intCast(@max(ru.stime.usec, 0))) * std.time.ns_per_us;
    return u + s;
}

fn taskAbsent(io: Io, task_dir_path: []const u8, tid: u32) !bool {
    var path_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const path = try std.fmt.bufPrint(&path_buf, "{s}/{d}", .{ task_dir_path, tid });
    _ = Io.Dir.cwd().statFile(io, path, .{}) catch |err| switch (err) {
        error.FileNotFound => return true,
        else => return err,
    };
    return false;
}

const TaskReading = struct { class: usize, cpu: RuntimeReading };

fn statPresent(io: Io, anchor: Io.Dir) !bool {
    const file = anchor.openFile(io, "stat", .{}) catch |err| switch (err) {
        error.FileNotFound => return false,
        else => return err,
    };
    file.close(io);
    return true;
}

/// Relative reads stay on the directory inode's PID object across numeric reuse.
fn readTask(ledger: *Ledger, io: Io, anchor: Io.Dir, tid: u32) !TaskReading {
    var buf: [512]u8 = undefined;
    const cpu: RuntimeReading = blk: {
        if (!builtin.is_test or ledger.test_runtime_source != .stat) {
            if (anchor.readFile(io, "schedstat", &buf)) |text| {
                if (parseSchedstatNs(text)) |ns| break :blk .{ .ns = ns, .source = .schedstat };
            } else |_| {}
        }
        const text = try anchor.readFile(io, "stat", &buf);
        const ns = parseStatCpuNs(text, 100) orelse return error.InvalidCpuSample; // Linux USER_HZ is 100 ticks/s.
        break :blk .{ .ns = ns, .source = .stat };
    };
    if (builtin.is_test) if (ledger.test_after_cpu) |hook| {
        if (tid == hook.tid) try hook.call(hook.ctx, anchor);
    };
    var comm_buf: [64]u8 = undefined;
    const comm = try anchor.readFile(io, "comm", &comm_buf);
    return .{ .class = classIndex(std.mem.trim(u8, comm, " \t\r\n")), .cpu = cpu };
}

fn observeTask(ledger: *Ledger, gpa: std.mem.Allocator, io: Io, task_dir_path: []const u8, tid: u32) !void {
    var old_anchor: ?Io.Dir = null;
    if (ledger.live.get(tid)) |old| {
        const anchor = old.anchor orelse return error.UnanchoredTask;
        old_anchor = anchor;
        if (readTask(ledger, io, anchor, tid)) |reading| {
            try ledger.observe(gpa, tid, old.incarnation, reading.class, reading.cpu);
            return;
        } else |err| {
            // stat is a fixed proc entry: a live old PID still has it.
            if (try statPresent(io, anchor)) return err;
        }
    }
    var path_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const path = try std.fmt.bufPrint(&path_buf, "{s}/{d}", .{ task_dir_path, tid });
    const candidate = Io.Dir.cwd().openDir(io, path, .{}) catch |err| switch (err) {
        error.FileNotFound => return,
        else => return err,
    };
    var kept = false;
    defer if (!kept) candidate.close(io);
    // A present numeric directory without valid identity evidence is not death.
    if (!try statPresent(io, candidate)) {
        if (try taskAbsent(io, task_dir_path, tid)) return;
        return error.FileNotFound;
    }
    if (old_anchor) |anchor| {
        if (try statPresent(io, anchor)) return error.UnprovenTaskReplacement;
    }
    const reading = readTask(ledger, io, candidate, tid) catch |err| {
        if (try taskAbsent(io, task_dir_path, tid)) return;
        return err;
    };
    const incarnation = try ledger.newIncarnation();
    try ledger.observe(gpa, tid, incarnation, reading.class, reading.cpu);
    ledger.live.getPtr(tid).?.anchor = candidate;
    kept = true;
}

fn sampleTotalNs(ledger: *const Ledger) u64 {
    if (builtin.is_test) if (ledger.test_total_ns) |ns| return ns;
    return processCpuNs();
}

/// One pass over `<proc_root>/self/task/*`, then the process total.
/// Valid stat data may replace unavailable/malformed schedstat. Other live
/// read/proof errors refuse publication; omitted live tids keep their baseline.
pub fn sample(ledger: *Ledger, gpa: std.mem.Allocator, io: Io, proc_root: []const u8) !Sample {
    ledger.io = io;
    ledger.beginPass();
    var path_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const task_dir_path = try std.fmt.bufPrint(&path_buf, "{s}/self/task", .{proc_root});
    var dir = try Io.Dir.cwd().openDir(io, task_dir_path, .{ .iterate = true });
    defer dir.close(io);
    var it = dir.iterate();
    var walked: usize = 0;
    while (try it.next(io)) |entry| {
        if (builtin.is_test) if (ledger.test_walk_limit) |limit| {
            if (walked == limit) break;
        };
        walked += 1;
        if (builtin.is_test) if (ledger.test_walk_hook) |hook| {
            if (walked == hook.after) try hook.call(hook.ctx);
        };
        const tid = std.fmt.parseInt(u32, entry.name, 10) catch continue;
        try observeTask(ledger, gpa, io, task_dir_path, tid);
    }
    // proc's directory cursor can skip live tids when its continuation exits.
    var live = ledger.live.iterator();
    while (live.next()) |e| {
        if (e.value_ptr.pass != ledger.pass and !try taskAbsent(io, task_dir_path, e.key_ptr.*))
            e.value_ptr.pass = ledger.pass;
    }
    ledger.finishPass();
    const by_class = ledger.sums();
    // The total is read AFTER the fold so Σ by_class ≤ total holds.
    return .{ .total_ns = sampleTotalNs(ledger), .by_class = by_class };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "the class table covers every charter name; other is last" {
    try testing.expectEqual(@as(usize, 0), classIndex("sqz-fuse-ln12"));
    try testing.expectEqual(className(classIndex("sqz-fuse-ur3")), "sqz-fuse-ur");
    try testing.expectEqualStrings("sqz-meta", className(classIndex("sqz-meta1")));
    try testing.expectEqualStrings("sqz-jrnl", className(classIndex("sqz-jrnl0")));
    try testing.expectEqualStrings("sqz-blk", className(classIndex("sqz-blk17")));
    try testing.expectEqualStrings("sqz-timer", className(classIndex("sqz-timer")));
    try testing.expectEqualStrings("other", className(classIndex("squeezefs")));
    try testing.expectEqualStrings("other", className(classIndex("sqz-lease-io")));
    try testing.expectEqual(other_index, classIndex("fuse3-tpc1"));
    try testing.expectEqualStrings("other", className(other_index));
}

test "parsers: schedstat field 1; the stat fallback splits after the LAST ')' and scales ticks to ns" {
    try testing.expectEqual(@as(?u64, 123456789), parseSchedstatNs("123456789 42 7\n"));
    try testing.expectEqual(@as(?u64, null), parseSchedstatNs(""));
    try testing.expectEqual(@as(?u64, null), parseSchedstatNs("x 1 2"));
    // comm with a space and a ')' inside; utime 30, stime 20 ticks at 100 Hz = 0.5 s.
    const stat = "1234 (sqz (x) y) S 1 1 1 0 -1 4194560 100 0 0 0 30 20 0 0 20 0 1 0 5 1000 0 18446744073709551615";
    try testing.expectEqual(@as(?u64, 500 * std.time.ns_per_ms), parseStatCpuNs(stat, 100));
    try testing.expectEqual(@as(?u64, 500 * std.time.ns_per_ms), parseStatCpuNs(stat, 0));
    try testing.expectEqual(@as(?u64, null), parseStatCpuNs("garbage", 100));
}

test "ledger: monotone retired fold — an exited thread keeps its last ns, a reused tid retires the old figure, a class never falls" {
    var l: Ledger = .{};
    defer l.deinit(testing.allocator);
    const meta = classIndex("sqz-meta0");
    const blk = classIndex("sqz-blk0");
    l.beginPass();
    try l.observe(testing.allocator, 10, 10, meta, .{ .ns = 100, .source = .schedstat });
    try l.observe(testing.allocator, 11, 11, blk, .{ .ns = 50, .source = .schedstat });
    l.finishPass();
    try testing.expectEqual(@as(u64, 100), l.sums()[meta]);
    try testing.expectEqual(@as(u64, 50), l.sums()[blk]);
    // Pass 2: thread 11 exited (retired at 50); thread 10 advanced.
    l.beginPass();
    try l.observe(testing.allocator, 10, 10, meta, .{ .ns = 160, .source = .schedstat });
    l.finishPass();
    try testing.expectEqual(@as(u64, 160), l.sums()[meta]);
    try testing.expectEqual(@as(u64, 50), l.sums()[blk]);
    // Pass 3: tid 11 reappears as a blk thread with a LOWER runtime (reuse):
    // the old 50 is already retired; the new figure adds on top.
    l.beginPass();
    try l.observe(testing.allocator, 10, 10, meta, .{ .ns = 200, .source = .schedstat });
    try l.observe(testing.allocator, 11, 12, blk, .{ .ns = 5, .source = .schedstat });
    l.finishPass();
    try testing.expectEqual(@as(u64, 55), l.sums()[blk]);
    // Pass 4: a proven new incarnation of tid 10 retires the old 200; 7 is live.
    l.beginPass();
    try l.observe(testing.allocator, 10, 13, meta, .{ .ns = 7, .source = .schedstat });
    try l.observe(testing.allocator, 11, 12, blk, .{ .ns = 9, .source = .schedstat });
    l.finishPass();
    try testing.expectEqual(@as(u64, 207), l.sums()[meta]);
    // Pass 5: same incarnation, new class and a coarser read: no new CPU is billed.
    l.beginPass();
    try l.observe(testing.allocator, 10, 13, blk, .{ .ns = 0, .source = .stat });
    l.finishPass();
    try testing.expectEqual(@as(u64, 207), l.sums()[meta]);
    try testing.expectEqual(@as(u64, 59), l.sums()[blk]);
    l.beginPass();
    try l.observe(testing.allocator, 10, 13, blk, .{ .ns = 10, .source = .schedstat });
    l.finishPass();
    try testing.expectEqual(@as(u64, 62), l.sums()[blk]);
}

test "ledger: more than 64 departures in one pass all retire" {
    var l: Ledger = .{};
    defer l.deinit(testing.allocator);
    l.beginPass();
    var tid: u32 = 1;
    while (tid <= 150) : (tid += 1) try l.observe(testing.allocator, tid, tid, other_index, .{ .ns = 1, .source = .schedstat });
    l.finishPass();
    l.beginPass();
    l.finishPass();
    try testing.expectEqual(@as(u64, 150), l.sums()[other_index]);
    try testing.expectEqual(@as(usize, 0), l.live.count());
}

/// Synthetic task fields; start-time ticks deliberately coincide across reuse.
fn writeTestTask(dir: Io.Dir, tid: u32, ns: u64, comm: []const u8, ticks: u64) !void {
    var path_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const task_path = try std.fmt.bufPrint(&path_buf, "self/task/{d}", .{tid});
    try dir.createDirPath(testing.io, task_path);
    var file_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    var data_buf: [512]u8 = undefined;
    const sched_path = try std.fmt.bufPrint(&file_buf, "{s}/schedstat", .{task_path});
    const sched = try std.fmt.bufPrint(&data_buf, "{d} 0 0\n", .{ns});
    try dir.writeFile(testing.io, .{ .sub_path = sched_path, .data = sched });
    const comm_path = try std.fmt.bufPrint(&file_buf, "{s}/comm", .{task_path});
    try dir.writeFile(testing.io, .{ .sub_path = comm_path, .data = comm });
    const stat_path = try std.fmt.bufPrint(&file_buf, "{s}/stat", .{task_path});
    const stat = try std.fmt.bufPrint(&data_buf, "{d} ({s}) S 1 1 1 0 -1 0 0 0 0 0 {d} 0 0 0 20 0 1 0 42 0 0 0\n", .{ tid, comm, ticks });
    try dir.writeFile(testing.io, .{ .sub_path = stat_path, .data = stat });
}

fn expectCpuTransition(before: Sample, after: Sample) !void {
    var sum: u64 = 0;
    for (before.by_class, after.by_class) |old, new| {
        try testing.expect(new >= old);
        sum += new;
    }
    try testing.expect(sum <= after.total_ns);
}

test "sample over a synthetic proc tree: classes fold, a renamed thread moves class on the next sample, JSON shape" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.createDirPath(testing.io, "self/task/100");
    try tmp.dir.createDirPath(testing.io, "self/task/101");
    try writeTestTask(tmp.dir, 100, 1000, "sqz-meta0", 0);
    // Thread 101 has no schedstat: the stat fallback (3 + 2 ticks = 50 ms).
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/101/stat", .data = "101 (sqz-blk3) S 1 1 1 0 -1 0 0 0 0 0 3 2 0 0 20 0 1 0 5 0 0 0" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/101/comm", .data = "sqz-blk3\n" });
    var root_buf: [std.fs.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];

    var l: Ledger = .{ .test_total_ns = 50 * std.time.ns_per_ms + 1000 };
    defer l.deinit(testing.allocator);
    const s1 = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(u64, 1000), s1.by_class[classIndex("sqz-meta0")]);
    try testing.expectEqual(@as(u64, 50 * std.time.ns_per_ms), s1.by_class[classIndex("sqz-blk3")]);
    // The renamed task's old prefix stays in meta; only its new CPU is journal.
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/comm", .data = "sqz-jrnl0\n" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/schedstat", .data = "1500 0 0\n" });
    l.test_total_ns = 50 * std.time.ns_per_ms + 1500;
    const s2 = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(u64, 1000), s2.by_class[classIndex("sqz-meta0")]);
    try testing.expectEqual(@as(u64, 500), s2.by_class[classIndex("sqz-jrnl0")]);
    try expectCpuTransition(s1, s2);
    var buf: [1024]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try s2.writeJson(&w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "{\"daemon_cpu_ns\":"));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"sqz-jrnl\":500") != null);
    try testing.expect(std.mem.endsWith(u8, w.buffered(), "\"other\":0}}"));
}

test "B1: schedstat to stat to schedstat keeps one same-task baseline" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try writeTestTask(tmp.dir, 100, 12_345_678, "sqz-meta0", 1);
    var root_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];
    var l: Ledger = .{ .test_total_ns = 12_345_678 };
    defer l.deinit(testing.allocator);
    const precise = try sample(&l, testing.allocator, testing.io, root);
    const incarnation = l.live.get(100).?.incarnation;
    l.test_runtime_source = .stat;
    const coarse = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(RuntimeSource.stat, l.live.get(100).?.last_source);
    try testing.expectEqual(incarnation, l.live.get(100).?.incarnation);
    try testing.expectEqual(precise.by_class, coarse.by_class);
    try expectCpuTransition(precise, coarse);
    l.test_runtime_source = null;
    l.test_total_ns = 13_000_001;
    try writeTestTask(tmp.dir, 100, 13_000_001, "sqz-meta0", 1);
    const resumed = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(u64, 13_000_001), resumed.by_class[classIndex("sqz-meta0")]);
    try expectCpuTransition(coarse, resumed);
    try tmp.dir.deleteFile(testing.io, "self/task/100/stat");
    l.test_runtime_source = .stat;
    try testing.expectError(error.FileNotFound, sample(&l, testing.allocator, testing.io, root));
    try testing.expectEqual(incarnation, l.live.get(100).?.incarnation);
    try testing.expectEqual(resumed.by_class, l.sums());
}

test "B1: a same-source precise regression refuses without inventing an incarnation" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try writeTestTask(tmp.dir, 100, 1000, "sqz-meta0", 0);
    var root_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];
    var l: Ledger = .{ .test_total_ns = 1000 };
    defer l.deinit(testing.allocator);
    const first = try sample(&l, testing.allocator, testing.io, root);
    const incarnation = l.live.get(100).?.incarnation;
    try writeTestTask(tmp.dir, 100, 999, "sqz-meta0", 0);
    try testing.expectError(error.RuntimeRegressed, sample(&l, testing.allocator, testing.io, root));
    try testing.expectEqual(incarnation, l.live.get(100).?.incarnation);
    try testing.expectEqual(first.by_class, l.sums());
    l.test_total_ns = 2000;
    try writeTestTask(tmp.dir, 100, 2000, "sqz-meta0", 0);
    const resumed = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(u64, 2000), resumed.by_class[classIndex("sqz-meta0")]);
    try expectCpuTransition(first, resumed);
}

test "B1: a reused numeric tid with a higher runtime is a new task incarnation" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try writeTestTask(tmp.dir, 100, 1000, "sqz-meta0", 0);
    var root_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];
    var l: Ledger = .{ .test_total_ns = 1000 };
    defer l.deinit(testing.allocator);
    const first = try sample(&l, testing.allocator, testing.io, root);
    const incarnation = l.live.get(100).?.incarnation;
    try tmp.dir.deleteTree(testing.io, "self/task/100");
    try writeTestTask(tmp.dir, 100, 1500, "sqz-meta0", 0);
    l.test_total_ns = 2500;
    const reused = try sample(&l, testing.allocator, testing.io, root);
    try testing.expect(l.live.get(100).?.incarnation != incarnation);
    try testing.expectEqual(@as(u64, 2500), reused.by_class[classIndex("sqz-meta0")]);
    try expectCpuTransition(first, reused);
}

test "B1: replacement between runtime and comm reads never blends two task incarnations" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try writeTestTask(tmp.dir, 100, 1000, "sqz-meta0", 0);
    var root_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];
    var l: Ledger = .{ .test_total_ns = 1000 };
    defer l.deinit(testing.allocator);
    const first = try sample(&l, testing.allocator, testing.io, root);
    const Replace = struct {
        dir: Io.Dir,
        ledger: *Ledger,
        fn call(ctx: *anyopaque, _: Io.Dir) !void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.ledger.test_after_cpu = null;
            try self.dir.deleteTree(testing.io, "self/task/100");
            try writeTestTask(self.dir, 100, 1500, "sqz-jrnl0", 0);
        }
    };
    var replace: Replace = .{ .dir = tmp.dir, .ledger = &l };
    l.test_after_cpu = .{ .tid = 100, .ctx = &replace, .call = Replace.call };
    l.test_total_ns = 2500;
    const replaced = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(u64, 1000), replaced.by_class[classIndex("sqz-meta0")]);
    try testing.expectEqual(@as(u64, 1500), replaced.by_class[classIndex("sqz-jrnl0")]);
    try expectCpuTransition(first, replaced);
}

test "B1: task anchors close on OOM, read refusal, retirement and ledger teardown" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "descriptor ownership is checked through Linux fcntl");
    const linux = std.os.linux;
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try writeTestTask(tmp.dir, 100, 1000, "sqz-meta0", 0);
    var root_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];
    const Capture = struct {
        fd: linux.fd_t = -1,
        refuse: bool = false,
        fn call(ctx: *anyopaque, anchor: Io.Dir) !void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.fd = anchor.handle;
            if (self.refuse) return error.TestReadRefusal;
        }
    };
    var capture: Capture = .{};
    var failed: Ledger = .{ .test_after_cpu = .{ .tid = 100, .ctx = &capture, .call = Capture.call } };
    defer failed.deinit(testing.allocator);
    try testing.expectError(error.OutOfMemory, sample(&failed, testing.failing_allocator, testing.io, root));
    try testing.expect(capture.fd >= 0);
    try testing.expectEqual(linux.E.BADF, linux.errno(linux.fcntl(capture.fd, linux.F.GETFD, 0)));
    try testing.expectEqual(@as(usize, 0), failed.live.count());
    capture = .{ .refuse = true };
    try testing.expectError(error.TestReadRefusal, sample(&failed, testing.allocator, testing.io, root));
    try testing.expect(capture.fd >= 0);
    try testing.expectEqual(linux.E.BADF, linux.errno(linux.fcntl(capture.fd, linux.F.GETFD, 0)));
    try testing.expectEqual(@as(usize, 0), failed.live.count());
    var l: Ledger = .{};
    var owned = true;
    defer if (owned) l.deinit(testing.allocator);
    _ = try sample(&l, testing.allocator, testing.io, root);
    const retired_fd = l.live.get(100).?.anchor.?.handle;
    try tmp.dir.deleteTree(testing.io, "self/task/100");
    _ = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(usize, 0), l.live.count());
    try testing.expectEqual(linux.E.BADF, linux.errno(linux.fcntl(retired_fd, linux.F.GETFD, 0)));
    try writeTestTask(tmp.dir, 100, 1500, "sqz-meta0", 0);
    _ = try sample(&l, testing.allocator, testing.io, root);
    const live_fd = l.live.get(100).?.anchor.?.handle;
    l.deinit(testing.allocator);
    owned = false;
    try testing.expectEqual(linux.E.BADF, linux.errno(linux.fcntl(live_fd, linux.F.GETFD, 0)));
}

test "sample: real proc directory cursor skips live tail threads when its cached continuation tid exits" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "the proc task directory cursor is Linux only");
    const Waiter = struct {
        ready: Io.Event = .unset,
        stop: *Io.Event,
        named: bool = false,
        tid: u32 = 0,
        fn run(self: *@This()) void {
            self.tid = @intCast(std.os.linux.gettid());
            var comm_buf: thread_name.CommBuf = undefined;
            if (self.named) _ = thread_name.nameCurrentThread(&comm_buf, "sqz-blk-proof");
            self.ready.set(testing.io);
            self.stop.waitUncancelable(testing.io);
        }
    };
    // More than one 2048-byte getdents buffer of numeric tid entries.
    const middle_count = Io.Dir.Iterator.reader_buffer_len / 24 + 8;
    const tail_count = 4;
    var middle_stop: Io.Event = .unset;
    var tail_stop: Io.Event = .unset;
    var middle: [middle_count]Waiter = undefined;
    var tail: [tail_count]Waiter = undefined;
    var middle_threads: [middle_count]std.Thread = undefined;
    var tail_threads: [tail_count]std.Thread = undefined;
    var middle_n: usize = 0;
    var tail_n: usize = 0;
    defer {
        middle_stop.set(testing.io);
        tail_stop.set(testing.io);
        for (middle_threads[0..middle_n]) |t| t.join();
        for (tail_threads[0..tail_n]) |t| t.join();
    }
    for (&middle, &middle_threads) |*w, *t| {
        w.* = .{ .stop = &middle_stop };
        t.* = try std.Thread.spawn(.{}, Waiter.run, .{w});
        middle_n += 1;
        w.ready.waitUncancelable(testing.io);
    }
    for (&tail, &tail_threads) |*w, *t| {
        w.* = .{ .stop = &tail_stop, .named = true };
        t.* = try std.Thread.spawn(.{}, Waiter.run, .{w});
        tail_n += 1;
        w.ready.waitUncancelable(testing.io);
    }
    var l: Ledger = .{};
    defer l.deinit(testing.allocator);
    while (true) {
        _ = try sample(&l, testing.allocator, testing.io, "/proc");
        var primed = true;
        for (middle) |w| primed = primed and l.live.contains(w.tid);
        for (tail) |w| primed = primed and l.live.contains(w.tid);
        if (primed) break;
    }
    const Cut = struct {
        stop: *Io.Event,
        threads: []std.Thread,
        waiters: []const Waiter,
        n: *usize,
        fn call(ctx: *anyopaque) !void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.stop.set(testing.io);
            for (self.threads) |t| t.join();
            self.n.* = 0;
            // clear_child_tid wakes join before exit_notify unlinks the task.
            for (self.waiters) |w| while (!try taskAbsent(testing.io, "/proc/self/task", w.tid)) {
                std.atomic.spinLoopHint();
            };
        }
    };
    var cut: Cut = .{ .stop = &middle_stop, .threads = &middle_threads, .waiters = &middle, .n = &middle_n };
    l.test_walk_hook = .{ .after = l.live.count() - middle_count - tail_count + 8, .ctx = &cut, .call = Cut.call };
    _ = try sample(&l, testing.allocator, testing.io, "/proc");
    l.test_walk_hook = null;
    try testing.expectEqual(@as(usize, 0), middle_n);
    var live_tail: usize = 0;
    for (tail) |w| live_tail += @intFromBool(l.live.contains(w.tid));
    try testing.expectEqual(@as(usize, tail_count), live_tail);
    _ = try sample(&l, testing.allocator, testing.io, "/proc");
    for (middle) |w| try testing.expect(!l.live.contains(w.tid));
    for (tail) |w| try testing.expect(l.live.contains(w.tid));
}

test "sample: an omitted live tid is not retired and counted twice when the proc walk resumes" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try writeTestTask(tmp.dir, 100, 100, "sqz-meta0", 0);
    var root_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];
    var l: Ledger = .{ .test_total_ns = 100 };
    defer l.deinit(testing.allocator);
    const first = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(u64, 100), first.by_class[classIndex("sqz-meta0")]);
    // proc's next_tid can end the walk when its current task exits.
    l.test_walk_limit = 0;
    const omitted = try sample(&l, testing.allocator, testing.io, root);
    l.test_walk_limit = null;
    const resumed = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(first.by_class, omitted.by_class);
    try testing.expectEqual(first.by_class, resumed.by_class);
    try testing.expect(resumed.by_class[classIndex("sqz-meta0")] <= resumed.total_ns);
}

test "sample: an unreadable live tid is not proof of death; recovery counts its runtime once" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try writeTestTask(tmp.dir, 100, 100, "sqz-meta0", 0);
    var root_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];
    var l: Ledger = .{ .test_total_ns = 100 };
    defer l.deinit(testing.allocator);
    const first = try sample(&l, testing.allocator, testing.io, root);
    try tmp.dir.deleteFile(testing.io, "self/task/100/comm");
    try testing.expectError(error.FileNotFound, sample(&l, testing.allocator, testing.io, root));
    try testing.expectEqual(@as(u64, 0), l.retired[classIndex("sqz-meta0")]);
    try testing.expect(l.live.contains(100));
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/comm", .data = "sqz-meta0\n" });
    const resumed = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(first.by_class, resumed.by_class);
    try tmp.dir.deleteTree(testing.io, "self/task");
    try testing.expectError(error.FileNotFound, sample(&l, testing.allocator, testing.io, root));
    try testing.expectEqual(first.by_class, l.sums());
    try testing.expect(l.live.contains(100));
}

test "the live process: Σ by_class ≤ total on Linux" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "cpu_class: Linux only");
    var l: Ledger = .{};
    defer l.deinit(testing.allocator);
    const s = try sample(&l, testing.allocator, testing.io, "/proc");
    var sum: u64 = 0;
    for (s.by_class) |v| sum += v;
    try testing.expect(sum <= s.total_ns);
    try testing.expect(s.total_ns > 0);
}

fn cpuAccountingFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    _ = parseSchedstatNs(text);
    _ = parseStatCpuNs(text, 100);
    _ = parseStatCpuNs(text, 0);
}

test "fuzz: the schedstat and stat cpu-time parsers are total on arbitrary text" {
    try fuzzkit.fuzz({}, cpuAccountingFuzz, .{ .corpus = &.{ fuzzkit.slice("123 456 7\n"), fuzzkit.slice("1 (a b) S 0 0 0 0 0 0 0 0 0 0 11 22 0 0\n") } });
}
