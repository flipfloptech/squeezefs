//! `daemon_cpu_ns` / `daemon_cpu_ns_by_class` (charter §9 "Thread naming is
//! part of the observability contract … attributes CPU per class";
//! `docs/porting/knobs_and_rails.md` §5.3): per-thread
//! `/proc/self/task/<tid>/schedstat` `sum_exec_runtime` folded by the comm
//! prefix table of `core/thread_name.zig`, sampled at stats-read time ONLY,
//! with the monotone retired ledger — a class never falls: a thread that
//! exits keeps contributing its last sample (the CPU between its last sample
//! and its exit is the one lost term), and a tid whose runtime DROPPED or
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

    live: std.AutoHashMapUnmanaged(u32, Live) = .empty,
    retired: [class_count]u64 = @splat(0),
    pass: u32 = 0,

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

    /// Retire every tid not seen this pass (an exited thread keeps its last ns).
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
        try w.print("{{\"daemon_cpu_ns\":{d},\"daemon_cpu_ns_by_class\":{{", .{self.total_ns});
        for (self.by_class, 0..) |ns, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("\"{s}\":{d}", .{ className(i), ns });
        }
        try w.writeAll("}}");
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

/// One pass over `<proc_root>/self/task/*` (Linux; pure over `proc_root` so
/// a synthetic tree tests it), then the process total. Threads whose files
/// vanish mid-walk (exited) are skipped and retire at `finishPass`.
pub fn sample(ledger: *Ledger, gpa: std.mem.Allocator, io: Io, proc_root: []const u8) !Sample {
    ledger.beginPass();
    var path_buf: [Io.Dir.max_path_bytes]u8 = undefined;
    const task_dir_path = try std.fmt.bufPrint(&path_buf, "{s}/self/task", .{proc_root});
    var dir = Io.Dir.cwd().openDir(io, task_dir_path, .{ .iterate = true }) catch |err| switch (err) {
        error.FileNotFound => {
            ledger.finishPass();
            return .{ .total_ns = processCpuNs(), .by_class = ledger.sums() };
        },
        else => |e| return e,
    };
    defer dir.close(io);
    var it = dir.iterate();
    while (try it.next(io)) |entry| {
        const tid = std.fmt.parseInt(u32, entry.name, 10) catch continue;
        var file_buf: [Io.Dir.max_path_bytes]u8 = undefined;
        var buf: [512]u8 = undefined;
        const ns: u64 = blk: {
            const sched = std.fmt.bufPrint(&file_buf, "{s}/{s}/schedstat", .{ task_dir_path, entry.name }) catch continue;
            if (Io.Dir.cwd().readFile(io, sched, &buf)) |text| {
                if (parseSchedstatNs(text)) |v| break :blk v;
            } else |_| {}
            const stat = std.fmt.bufPrint(&file_buf, "{s}/{s}/stat", .{ task_dir_path, entry.name }) catch continue;
            const text = Io.Dir.cwd().readFile(io, stat, &buf) catch continue;
            break :blk parseStatCpuNs(text, 100) orelse continue;
        };
        const comm_path = std.fmt.bufPrint(&file_buf, "{s}/{s}/comm", .{ task_dir_path, entry.name }) catch continue;
        var comm_buf: [64]u8 = undefined;
        const comm = Io.Dir.cwd().readFile(io, comm_path, &comm_buf) catch continue;
        try ledger.observe(gpa, tid, classIndex(std.mem.trim(u8, comm, " \t\r\n")), ns);
    }
    ledger.finishPass();
    const by_class = ledger.sums();
    // The total is read AFTER the fold so Σ by_class ≤ total holds.
    return .{ .total_ns = processCpuNs(), .by_class = by_class };
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
