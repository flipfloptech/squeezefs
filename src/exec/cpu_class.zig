//! `daemon_cpu_ns` / `daemon_cpu_ns_by_class` (charter §9 "Thread naming is
//! part of the observability contract … attributes CPU per class";
//! `docs/porting/knobs_and_rails.md` §5.3): per-thread
//! `/proc/self/task/<tid>/schedstat` `sum_exec_runtime` folded by the comm
//! prefix table of `core/thread_name.zig`, sampled at stats-read time ONLY,
//! with the monotone retired ledger — a class never falls: a thread that
//! exits keeps contributing its last sample (the CPU between its last sample
//! and its exit is the one lost term). A missing directory entry is not death:
//! only an absent task directory retires the tid. A tid whose runtime DROPPED or
//! whose class changed is a reused tid: the old figure retires first.
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

/// The monotone fold over passes.
pub const Ledger = struct {
    const Live = struct { class: u8, ns: u64, pass: u32 };
    const TestWalkHook = struct {
        after: usize,
        ctx: *anyopaque,
        call: *const fn (*anyopaque) anyerror!void,
    };

    live: std.AutoHashMapUnmanaged(u32, Live) = .empty,
    retired: [class_count]u64 = @splat(0),
    pass: u32 = 0,
    test_walk_hook: if (builtin.is_test) ?TestWalkHook else void = if (builtin.is_test) null else {},
    test_walk_limit: if (builtin.is_test) ?usize else void = if (builtin.is_test) null else {},
    test_total_ns: if (builtin.is_test) ?u64 else void = if (builtin.is_test) null else {},

    pub fn deinit(self: *Ledger, gpa: std.mem.Allocator) void {
        self.live.deinit(gpa);
        self.* = undefined;
    }

    /// Start a pass: every tid observed before this is a candidate for
    /// retirement at `finishPass`.
    pub fn beginPass(self: *Ledger) void {
        self.pass +%= 1;
    }

    /// One thread's sample this pass.
    pub fn observe(self: *Ledger, gpa: std.mem.Allocator, tid: u32, class: usize, ns: u64) std.mem.Allocator.Error!void {
        const gop = try self.live.getOrPut(gpa, tid);
        if (gop.found_existing) {
            const old = gop.value_ptr.*;
            // Same thread, runtime monotone: update in place. A class change
            // or a drop means the tid was reused: retire the old figure.
            if (old.class != class or ns < old.ns) self.retired[old.class] +|= old.ns;
        }
        gop.value_ptr.* = .{ .class = @intCast(class), .ns = ns, .pass = self.pass };
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
                self.retired[e.value_ptr.class] +|= e.value_ptr.ns;
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
        while (it.next()) |v| out[v.class] +|= v.ns;
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

fn sampleTotalNs(ledger: *const Ledger) u64 {
    if (builtin.is_test) if (ledger.test_total_ns) |ns| return ns;
    return processCpuNs();
}

/// One pass over `<proc_root>/self/task/*`, then the process total.
/// Only task-directory absence proves an exit; live read errors refuse
/// publication and an omitted live tid retains its last observation.
pub fn sample(ledger: *Ledger, gpa: std.mem.Allocator, io: Io, proc_root: []const u8) !Sample {
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
        var file_buf: [Io.Dir.max_path_bytes]u8 = undefined;
        var buf: [512]u8 = undefined;
        const ns: u64 = blk: {
            const sched = std.fmt.bufPrint(&file_buf, "{s}/{s}/schedstat", .{ task_dir_path, entry.name }) catch continue;
            if (Io.Dir.cwd().readFile(io, sched, &buf)) |text| {
                if (parseSchedstatNs(text)) |v| break :blk v;
            } else |_| {}
            const stat = try std.fmt.bufPrint(&file_buf, "{s}/{s}/stat", .{ task_dir_path, entry.name });
            const text = Io.Dir.cwd().readFile(io, stat, &buf) catch |err| {
                if (try taskAbsent(io, task_dir_path, tid)) continue;
                return err;
            };
            break :blk parseStatCpuNs(text, 100) orelse {
                if (try taskAbsent(io, task_dir_path, tid)) continue;
                return error.InvalidCpuSample;
            };
        };
        const comm_path = try std.fmt.bufPrint(&file_buf, "{s}/{s}/comm", .{ task_dir_path, entry.name });
        var comm_buf: [64]u8 = undefined;
        const comm = Io.Dir.cwd().readFile(io, comm_path, &comm_buf) catch |err| {
            if (try taskAbsent(io, task_dir_path, tid)) continue;
            return err;
        };
        try ledger.observe(gpa, tid, classIndex(std.mem.trim(u8, comm, " \t\r\n")), ns);
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
    try l.observe(testing.allocator, 10, meta, 100);
    try l.observe(testing.allocator, 11, blk, 50);
    l.finishPass();
    try testing.expectEqual(@as(u64, 100), l.sums()[meta]);
    try testing.expectEqual(@as(u64, 50), l.sums()[blk]);
    // Pass 2: thread 11 exited (retired at 50); thread 10 advanced.
    l.beginPass();
    try l.observe(testing.allocator, 10, meta, 160);
    l.finishPass();
    try testing.expectEqual(@as(u64, 160), l.sums()[meta]);
    try testing.expectEqual(@as(u64, 50), l.sums()[blk]);
    // Pass 3: tid 11 reappears as a blk thread with a LOWER runtime (reuse):
    // the old 50 is already retired; the new figure adds on top.
    l.beginPass();
    try l.observe(testing.allocator, 10, meta, 200);
    try l.observe(testing.allocator, 11, blk, 5);
    l.finishPass();
    try testing.expectEqual(@as(u64, 55), l.sums()[blk]);
    // Pass 4: tid 10 drops (reuse within the same class): the old 200 retires, 7 is live.
    l.beginPass();
    try l.observe(testing.allocator, 10, meta, 7);
    try l.observe(testing.allocator, 11, blk, 9);
    l.finishPass();
    try testing.expectEqual(@as(u64, 207), l.sums()[meta]);
    // Pass 5: tid 10 changes class: the meta figure retires, the new class starts.
    l.beginPass();
    try l.observe(testing.allocator, 10, blk, 1);
    l.finishPass();
    try testing.expectEqual(@as(u64, 207), l.sums()[meta]);
    try testing.expectEqual(@as(u64, 59 + 1), l.sums()[blk]);
}

test "ledger: more than 64 departures in one pass all retire" {
    var l: Ledger = .{};
    defer l.deinit(testing.allocator);
    l.beginPass();
    var tid: u32 = 1;
    while (tid <= 150) : (tid += 1) try l.observe(testing.allocator, tid, other_index, 1);
    l.finishPass();
    l.beginPass();
    l.finishPass();
    try testing.expectEqual(@as(u64, 150), l.sums()[other_index]);
    try testing.expectEqual(@as(usize, 0), l.live.count());
}

test "sample over a synthetic proc tree: classes fold, a renamed thread moves class on the next sample, JSON shape" {
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.createDirPath(testing.io, "self/task/100");
    try tmp.dir.createDirPath(testing.io, "self/task/101");
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/schedstat", .data = "1000 0 0\n" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/comm", .data = "sqz-meta0\n" });
    // Thread 101 has no schedstat: the stat fallback (3 + 2 ticks = 50 ms).
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/101/stat", .data = "101 (sqz-blk3) S 1 1 1 0 -1 0 0 0 0 0 3 2 0 0 20 0 1 0 5 0 0 0" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/101/comm", .data = "sqz-blk3\n" });
    var root_buf: [std.fs.max_path_bytes]u8 = undefined;
    const root_len = try tmp.dir.realPath(testing.io, &root_buf);
    const root = root_buf[0..root_len];

    var l: Ledger = .{};
    defer l.deinit(testing.allocator);
    const s1 = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(u64, 1000), s1.by_class[classIndex("sqz-meta0")]);
    try testing.expectEqual(@as(u64, 50 * std.time.ns_per_ms), s1.by_class[classIndex("sqz-blk3")]);
    // Thread 100 renames itself: the next sample retires its meta figure
    // and starts the new class with its full (monotone) runtime.
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/comm", .data = "sqz-jrnl0\n" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/schedstat", .data = "1500 0 0\n" });
    const s2 = try sample(&l, testing.allocator, testing.io, root);
    try testing.expectEqual(@as(u64, 1000), s2.by_class[classIndex("sqz-meta0")]);
    try testing.expectEqual(@as(u64, 1500), s2.by_class[classIndex("sqz-jrnl0")]);
    var buf: [1024]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try s2.writeJson(&w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "{\"daemon_cpu_ns\":"));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"sqz-jrnl\":1500") != null);
    try testing.expect(std.mem.endsWith(u8, w.buffered(), "\"other\":0}}"));
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
    try tmp.dir.createDirPath(testing.io, "self/task/100");
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/schedstat", .data = "100 0 0\n" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/comm", .data = "sqz-meta0\n" });
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
    try tmp.dir.createDirPath(testing.io, "self/task/100");
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/schedstat", .data = "100 0 0\n" });
    try tmp.dir.writeFile(testing.io, .{ .sub_path = "self/task/100/comm", .data = "sqz-meta0\n" });
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
