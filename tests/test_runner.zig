//! The project test runner: Zig 0.17's `lib/compiler/test_runner.zig` (the
//! build-runner protocol over `--listen=-`, the error-log rule, the leak
//! check, `error.SkipZigTest`, the fuzzer hooks) plus the gate's sharding
//! (docs/evidence/rung-10-gate-speed.md). Keep the stock parts in step with
//! the compiler's copy; the additions are:
//!
//!   --shard=I/N | --shard=exclusive
//!       Run only this process's part of the binary (`shard_plan.zig`): in
//!       server mode the build runner is told about those tests alone, so a
//!       step's pass/skip/fail counts are its part's and the parts of a set
//!       sum to the unsharded run's.
//!   --no-exclusive-step
//!       The build gives this binary no exclusive step: a `[venue-exclusive]`
//!       test would run nowhere, so it refuses the run by name.
//!   --costs=PATH
//!       The committed cost table the partition balances by; a table too stale
//!       for this binary (`shard_plan.Staleness`) refuses the run by name.
//!   --costs-out=PATH
//!       Write every test this process ran with its measured wall and CPU
//!       time (`<name>\t<wall µs>\t<cpu µs>`) when the build runner ends the
//!       run (`zig build costs` merges a set's files into the table).
//!   --hang-limit-ms=N
//!       Every test's hang limit, instead of the derived one (the watchdog's
//!       self-test, `tests/hangwatch_selftest.zig`).
//!
//! THE HANG WATCHDOG (Linux, both modes). A test running past its limit —
//! `shard_plan.hang.limitUs` of its cost-table row — ends the process: one
//! `HUNG TEST: "<name>" ran <s> (cost table: <s>|no row; limit: <s>)` line
//! (to the heartbeat's sink too, so it lands live), every thread's name,
//! state, CPU, wait channel and kernel stack, each thread's user stack, then
//! exit 124. The build runner reports the test with that stderr and resumes
//! the shard in a new process: a hang fails its step in minutes, never at the
//! gate's outer timeout with the venue held.
//!
//! THE HEARTBEAT (Linux, server mode). The build runner buffers a test
//! process's stderr until the step ends and draws live progress only on a
//! terminal, so a scripted gate (no TTY) was silent for its whole run. When
//! the build runner's own stderr (`/proc/<ppid>/fd/2`) is a pipe (a scripted
//! run, `container exec` without a TTY, `| tee`), a thread writes one line
//! there every `heartbeat_period_s` — `[<binary> I/N] done/total, <elapsed>;
//! running <test> for <s>` — and one when the part ends. On a terminal the
//! progress tree already shows it; a redirect to a regular file gets none
//! (the reopened file has its own offset: lines would land under the build
//! runner's own writes).
const builtin = @import("builtin");

const std = @import("std");
const Io = std.Io;
const fatal = std.process.fatal;
const testing = std.testing;
const assert = std.debug.assert;
const panic = std.debug.panic;
const fuzz_abi = std.Build.abi.fuzz;
const shard_plan = @import("shard_plan.zig");

pub const std_options: std.Options = .{
    .logFn = log,
};

var log_err_count: usize = 0;
var fba: std.heap.FixedBufferAllocator = .init(&fba_buffer);
var fba_buffer: [8192]u8 = undefined;
var stdin_buffer: [4096]u8 = undefined;
var stdout_buffer: [4096]u8 = undefined;
var stdin_reader: Io.File.Reader = undefined;
var stdout_writer: Io.File.Writer = undefined;
const runner_threaded_io: Io = Io.Threaded.global_single_threaded.io();

/// The plan's allocations live for the process.
const plan_gpa = std.heap.page_allocator;

/// Keep in sync with logic in `std.Build.addRunArtifact` which decides whether
/// the test runner will communicate with the build runner via `std.zig.Server`.
const need_simple = switch (builtin.zig_backend) {
    .stage2_aarch64,
    .stage2_loongarch,
    .stage2_powerpc,
    .stage2_riscv64,
    => true,
    else => false,
};

/// What this process runs, and what it records.
const Run = struct {
    /// Indices into `builtin.test_functions`, ascending.
    indices: []const u32,
    spec: shard_plan.Spec,
    /// The table's prediction for this process; null without a table.
    predicted_us: ?u64,
    costs_out: ?[]const u8,
    /// Measured per entry of `indices`, for `--costs-out`.
    measured: []Measured,
    /// The table's wall per entry of `indices` (null: no row or no table).
    rows_us: []const ?u64,
};

/// `--hang-limit-ms`: replaces every test's derived hang limit.
var hang_limit_override_us: ?u64 = null;

const Measured = struct { wall_us: u64 = 0, cpu_us: u64 = 0 };

/// A test's start: the wall clock and the CPU this process and the children
/// it reaped have used (a smoke row's daemons are its reaped children).
const Mark = struct {
    wall: Io.Clock.Timestamp,
    cpu_us: u64,

    fn now() Mark {
        return .{ .wall = .now(runner_threaded_io, .awake), .cpu_us = cpuUs() };
    }

    fn since(m: Mark) Measured {
        return .{ .wall_us = elapsedUs(m.wall), .cpu_us = cpuUs() -| m.cpu_us };
    }

    fn cpuUs() u64 {
        var us: u64 = 0;
        for ([_]i32{ std.posix.rusage.SELF, std.posix.rusage.CHILDREN }) |who| {
            const ru = std.posix.getrusage(who);
            for ([_]std.posix.timeval{ ru.utime, ru.stime }) |tv| {
                us += @as(u64, @intCast(tv.sec)) * std.time.us_per_s + @as(u64, @intCast(tv.usec));
            }
        }
        return us;
    }
};

pub fn main(init: std.process.Init.Minimal) void {
    @disableInstrumentation();

    if (builtin.cpu.arch.isSpirV()) {
        // SPIR-V needs an special test-runner
        return;
    }

    if (need_simple) {
        return mainSimple() catch |err| panic("test failure: {t}", .{err});
    }

    const args = init.args.toSlice(fba.allocator()) catch |err| panic("unable to parse command line args: {t}", .{err});

    var listen = false;
    var opt_cache_dir: ?[]const u8 = null;
    var spec: shard_plan.Spec = .all;
    var costs_path: ?[]const u8 = null;
    var costs_out: ?[]const u8 = null;
    var no_exclusive_step = false;

    for (args[1..]) |arg| {
        if (std.mem.eql(u8, arg, "--listen=-")) {
            listen = true;
        } else if (std.mem.startsWith(u8, arg, "--seed=")) {
            testing.random_seed = std.fmt.parseUnsigned(u32, arg["--seed=".len..], 0) catch
                @panic("unable to parse --seed command line argument");
        } else if (std.mem.startsWith(u8, arg, "--cache-dir")) {
            opt_cache_dir = arg["--cache-dir=".len..];
        } else if (std.mem.startsWith(u8, arg, "--shard=")) {
            spec = shard_plan.Spec.parse(arg["--shard=".len..]) catch
                fatal("--shard expects I/N (1 ≤ I ≤ N) or `exclusive`, got {s}", .{arg});
        } else if (std.mem.startsWith(u8, arg, "--costs=")) {
            costs_path = arg["--costs=".len..];
        } else if (std.mem.startsWith(u8, arg, "--costs-out=")) {
            costs_out = arg["--costs-out=".len..];
        } else if (std.mem.eql(u8, arg, "--no-exclusive-step")) {
            no_exclusive_step = true;
        } else if (std.mem.startsWith(u8, arg, "--hang-limit-ms=")) {
            const ms = std.fmt.parseUnsigned(u64, arg["--hang-limit-ms=".len..], 10) catch
                fatal("--hang-limit-ms expects a whole number of milliseconds, got {s}", .{arg});
            hang_limit_override_us = ms * std.time.us_per_ms;
        } else {
            panic("unrecognized command line argument: {s}", .{arg});
        }
    }

    if (builtin.fuzz) {
        const cache_dir = opt_cache_dir orelse @panic("missing --cache-dir=[path] argument");
        fuzz_abi.fuzzer_init(.fromSlice(cache_dir));
    }

    const run = makeRun(spec, costs_path, costs_out, no_exclusive_step);
    heartbeat.start(std.fs.path.basename(args[0]), run, listen);
    if (listen) {
        return mainServer(init, run) catch |err| panic("internal test runner failure: {t}", .{err});
    } else {
        return mainTerminal(init, run);
    }
}

fn makeRun(spec: shard_plan.Spec, costs_path: ?[]const u8, costs_out: ?[]const u8, no_exclusive_step: bool) Run {
    const test_fns = builtin.test_functions;
    const names = plan_gpa.alloc([]const u8, test_fns.len) catch @panic("OOM");
    for (test_fns, names) |t, *n| n.* = t.name;
    if (no_exclusive_step and spec == .part) for (names) |n| if (shard_plan.isExclusive(n)) fatal(
        "'{s}' is {s} but build.zig gives this binary no exclusive step (Shards.add's `.exclusive`): it would run in no step",
        .{ n, shard_plan.exclusive_marker },
    );

    var costs: ?shard_plan.Costs = null;
    if (costs_path) |path| {
        const text = Io.Dir.cwd().readFileAlloc(runner_threaded_io, path, plan_gpa, .unlimited) catch |err|
            fatal("cannot read the cost table {s}: {t}", .{ path, err });
        costs = shard_plan.Costs.parse(plan_gpa, text) catch |err|
            fatal("the cost table {s} does not parse ({t}): regenerate it with `zig build costs`", .{ path, err });
        const s = shard_plan.staleness(plan_gpa, names, &costs.?) catch @panic("OOM");
        if (s.isStale()) fatal(
            "the cost table {s} is stale for this binary: {d} of its {d} tests have no row, {d} of the table's {d} µs name no test (the rail: at most 1/{d} of each) — regenerate it with `zig build costs` and commit it",
            .{ path, s.unknown, s.tests, s.orphan_us, s.total_us, shard_plan.Staleness.max_fraction_den },
        );
    }
    const p = shard_plan.plan(plan_gpa, names, if (costs) |*c| c else null, spec) catch @panic("OOM");
    const measured = plan_gpa.alloc(Measured, p.indices.len) catch @panic("OOM");
    @memset(measured, .{});
    const rows = plan_gpa.alloc(?u64, p.indices.len) catch @panic("OOM");
    for (p.indices, rows) |i, *r| r.* = if (costs) |c| c.map.get(names[i]) else null;
    return .{ .indices = p.indices, .spec = spec, .predicted_us = if (costs != null) p.predicted_us else null, .costs_out = costs_out, .measured = measured, .rows_us = rows };
}

fn writeCosts(run: Run) void {
    const path = run.costs_out orelse return;
    var text: Io.Writer.Allocating = .init(plan_gpa);
    for (run.indices, run.measured) |i, m| {
        text.writer.print("{s}\t{d}\t{d}\n", .{ builtin.test_functions[i].name, m.wall_us, m.cpu_us }) catch @panic("OOM");
    }
    Io.Dir.cwd().writeFile(runner_threaded_io, .{ .sub_path = path, .data = text.written() }) catch |err|
        fatal("cannot write the measured costs to {s}: {t}", .{ path, err });
}

fn mainServer(init: std.process.Init.Minimal, run: Run) !void {
    @disableInstrumentation();
    stdin_reader = .initStreaming(.stdin(), runner_threaded_io, &stdin_buffer);
    stdout_writer = .initStreaming(.stdout(), runner_threaded_io, &stdout_buffer);
    var server: std.zig.Server = .{
        .in = &stdin_reader.interface,
        .out = &stdout_writer.interface,
    };
    try server.serveStringMessage(.zig_version, builtin.zig_version_string);

    while (true) {
        const hdr = try server.receiveMessage();
        switch (hdr.tag) {
            .exit => {
                writeCosts(run);
                heartbeat.finish();
                return std.process.exit(0);
            },
            .query_test_metadata => {
                var sa: std.heap.SafeAllocator = .init(std.heap.page_allocator, .{});
                defer if (sa.deinit() != 0) @panic("internal test runner memory leak");
                const gpa = sa.allocator();

                var string_bytes: std.ArrayList(u8) = .empty;
                defer string_bytes.deinit(gpa);
                try string_bytes.append(gpa, 0); // Reserve 0 for null.

                // The build runner sees this process's tests alone, indexed
                // by their position in `run.indices`.
                const names = try gpa.alloc(u32, run.indices.len);
                defer gpa.free(names);
                const expected_panic_msgs = try gpa.alloc(u32, run.indices.len);
                defer gpa.free(expected_panic_msgs);

                for (run.indices, names, expected_panic_msgs) |test_index, *name, *expected_panic_msg| {
                    const test_fn = builtin.test_functions[test_index];
                    name.* = @intCast(string_bytes.items.len);
                    try string_bytes.ensureUnusedCapacity(gpa, test_fn.name.len + 1);
                    string_bytes.appendSliceAssumeCapacity(test_fn.name);
                    string_bytes.appendAssumeCapacity(0);
                    expected_panic_msg.* = 0;
                }

                try server.serveTestMetadata(.{
                    .names = names,
                    .expected_panic_msgs = expected_panic_msgs,
                    .string_bytes = string_bytes.items,
                });
            },

            .run_test => {
                testing.environ = init.environ;
                testing.allocator_instance = .init(std.heap.page_allocator, .{
                    .canary = 0xc3a701ba,
                    .check_write_after_free = true,
                });
                testing.io_instance = .init(testing.allocator, .{
                    .argv0 = .init(init.args),
                    .environ = init.environ,
                });
                log_err_count = 0;
                const index = try server.receiveBody_u32();
                const test_fn = builtin.test_functions[run.indices[index]];
                is_fuzz_test = false;

                // let the build server know we're starting the test now
                try server.serveStringMessage(.test_started, &.{});
                heartbeat.testStarted(index);
                const started: Mark = .now();

                const TestResults = std.zig.Server.Message.TestResults;
                const status: TestResults.Status = if (test_fn.func()) |v| s: {
                    v;
                    break :s .pass;
                } else |err| switch (err) {
                    error.SkipZigTest => .skip,
                    else => s: {
                        if (@errorReturnTrace()) |trace| {
                            std.debug.dumpErrorReturnTrace(trace);
                        }
                        break :s .fail;
                    },
                };
                testing.io_instance.deinit();
                const leak_count = testing.allocator_instance.deinit();
                run.measured[index] = started.since();
                heartbeat.testEnded(status);
                try server.serveTestResults(.{
                    .index = index,
                    .flags = .{
                        .status = status,
                        .fuzz = is_fuzz_test,
                        .log_err_count = std.math.lossyCast(
                            @FieldType(TestResults.Flags, "log_err_count"),
                            log_err_count,
                        ),
                        .leak_count = std.math.lossyCast(
                            @FieldType(TestResults.Flags, "leak_count"),
                            leak_count,
                        ),
                    },
                });
            },
            .start_fuzzing => {
                // This ensures that this code won't be analyzed and hence reference fuzzer symbols
                // since they are not present.
                if (!builtin.fuzz) unreachable;

                var gpa_instance: std.heap.SafeAllocator = .init(std.heap.page_allocator, .{});
                defer if (gpa_instance.deinit() != 0) {
                    @panic("internal test runner memory leak");
                };
                const gpa = gpa_instance.allocator();
                var io_instance: Io.Threaded = .init(gpa, .{
                    .argv0 = .init(init.args),
                    .environ = init.environ,
                });
                defer io_instance.deinit();

                const mode: fuzz_abi.LimitKind = @fromBackingInt(@intCast(try server.receiveBody_u8()));
                const amount_or_instance = try server.receiveBody_u64();
                const main_instance = mode == .iterations or amount_or_instance == 0;

                if (main_instance) {
                    const coverage = fuzz_abi.fuzzer_coverage();
                    try server.serveCoverageIdMessage(
                        coverage.id,
                        coverage.runs,
                        coverage.unique,
                        coverage.seen,
                    );
                }

                const n_tests: u32 = try server.receiveBody_u32();
                const test_indexes = try gpa.alloc(u32, n_tests);
                defer gpa.free(test_indexes);
                fuzz_runner = .{
                    .indexes = test_indexes,
                    .server = &server,
                    .gpa = gpa,
                    .threaded_io = &io_instance,
                    .input_poller = undefined,
                };

                {
                    var large_name_buf: std.ArrayList(u8) = .empty;
                    defer large_name_buf.deinit(gpa);
                    for (test_indexes) |*i| {
                        const name_len = try server.receiveBody_u32();
                        const name = if (name_len <= server.in.buffer.len)
                            try server.in.take(name_len)
                        else large_name: {
                            try large_name_buf.resize(gpa, name_len);
                            try server.in.readSliceAll(large_name_buf.items);
                            break :large_name large_name_buf.items;
                        };

                        for (0.., builtin.test_functions) |test_i, test_fn| {
                            if (std.mem.eql(u8, name, test_fn.name)) {
                                i.* = @intCast(test_i);
                                break;
                            }
                        } else {
                            panic("fuzz test {s} no longer exists", .{name});
                        }

                        if (main_instance) {
                            const relocated_entry_addr = @intFromPtr(builtin.test_functions[i.*].func);
                            const entry_addr = fuzz_abi.fuzzer_unslide_address(relocated_entry_addr);
                            try server.serveU64Message(.fuzz_start_addr, entry_addr);
                        }
                    }
                }

                fuzz_abi.fuzzer_main(n_tests, testing.random_seed, mode, amount_or_instance);

                assert(mode != .forever);
                std.process.exit(0);
            },

            else => {
                std.debug.print("unsupported message: {x}\n", .{@backingInt(hdr.tag)});
                std.process.exit(1);
            },
        }
    }
}

fn mainTerminal(init: std.process.Init.Minimal, run: Run) void {
    @disableInstrumentation();
    if (builtin.fuzz) @panic("fuzz test requires server");

    const test_fn_list = builtin.test_functions;
    const total = run.indices.len;
    var ok_count: usize = 0;
    var skip_count: usize = 0;
    var fail_count: usize = 0;
    var fuzz_count: usize = 0;
    const root_node = if (builtin.fuzz) std.Progress.Node.none else std.Progress.start(runner_threaded_io, .{
        .root_name = "Test",
        .estimated_total_items = total,
    });
    const have_tty = Io.File.stderr().isTty(runner_threaded_io) catch unreachable;

    var leaks: usize = 0;
    for (run.indices, 0..) |test_index, i| {
        const test_fn = test_fn_list[test_index];
        testing.allocator_instance = .init(std.heap.page_allocator, .{
            .canary = 0xc3a701ba,
            .check_write_after_free = true,
        });
        testing.io_instance = .init(testing.allocator, .{
            .argv0 = .init(init.args),
            .environ = init.environ,
        });
        defer {
            testing.io_instance.deinit();
            if (testing.allocator_instance.deinit() != 0) leaks += 1;
        }
        testing.log_level = .warn;
        testing.environ = init.environ;

        const test_node = root_node.start(test_fn.name, 0);
        if (!have_tty) {
            std.debug.print("{d}/{d} {s}...", .{ i + 1, total, test_fn.name });
        }
        is_fuzz_test = false;
        heartbeat.testStarted(@intCast(i));
        const started: Mark = .now();
        defer run.measured[i] = started.since();
        if (test_fn.func()) |_| {
            heartbeat.testEnded(.pass);
            ok_count += 1;
            test_node.end();
            if (!have_tty) std.debug.print("OK\n", .{});
        } else |err| switch (err) {
            error.SkipZigTest => {
                heartbeat.testEnded(.skip);
                skip_count += 1;
                if (have_tty) {
                    std.debug.print("{d}/{d} {s}...SKIP\n", .{ i + 1, total, test_fn.name });
                } else {
                    std.debug.print("SKIP\n", .{});
                }
                test_node.end();
            },
            else => {
                heartbeat.testEnded(.fail);
                fail_count += 1;
                if (have_tty) {
                    std.debug.print("{d}/{d} {s}...FAIL ({t})\n", .{
                        i + 1, total, test_fn.name, err,
                    });
                } else {
                    std.debug.print("FAIL ({t})\n", .{err});
                }
                if (@errorReturnTrace()) |trace| {
                    std.debug.dumpErrorReturnTrace(trace);
                }
                test_node.end();
            },
        }
        fuzz_count += @intFromBool(is_fuzz_test);
    }
    root_node.end();
    writeCosts(run);
    if (ok_count == total) {
        std.debug.print("All {d} tests passed.\n", .{ok_count});
    } else {
        std.debug.print("{d} passed; {d} skipped; {d} failed.\n", .{ ok_count, skip_count, fail_count });
    }
    if (log_err_count != 0) {
        std.debug.print("{d} errors were logged.\n", .{log_err_count});
    }
    if (leaks != 0) {
        std.debug.print("{d} tests leaked memory.\n", .{leaks});
    }
    if (fuzz_count != 0) {
        std.debug.print("{d} fuzz tests found.\n", .{fuzz_count});
    }
    if (leaks != 0 or log_err_count != 0 or fail_count != 0) {
        std.process.exit(1);
    }
}

fn elapsedUs(started: Io.Clock.Timestamp) u64 {
    const ns = started.untilNow(runner_threaded_io).raw.nanoseconds;
    return @intCast(@divTrunc(@max(ns, 0), std.time.ns_per_us));
}

/// The heartbeat and the hang watchdog (module doc): one thread, ticking
/// every second. The heartbeat is off unless `start` found a non-terminal
/// build-runner stderr; the counters it reads are single-writer (the test
/// loop) and read racily by the thread — a heartbeat line is a sample.
const heartbeat = struct {
    /// The owner's cadence (rung 10 gate speed): often enough that a quiet
    /// screen means a stuck test, rare enough to stay a few lines a minute.
    const heartbeat_period_s = 30;
    const idle = std.math.maxInt(u32);
    /// timeout(1)'s status: the gate train tells a hang from a crash by it.
    const hung_exit_code = 124;
    /// How long one thread's user-stack dump may take before the dump gives
    /// up (its handler is stuck on a lock the hung code holds).
    const user_stack_wait_ms = 2000;

    var sink: ?std.posix.fd_t = null;
    var label_buf: [96]u8 = undefined;
    var label: []const u8 = "";
    var total: u32 = 0;
    var predicted_us: ?u64 = null;
    var t0: Io.Clock.Timestamp = undefined;
    var done: std.atomic.Value(u32) = .init(0);
    var skipped: std.atomic.Value(u32) = .init(0);
    var failed: std.atomic.Value(u32) = .init(0);
    var current: std.atomic.Value(u32) = .init(idle);
    var current_t0_us: std.atomic.Value(u64) = .init(0);
    var run_indices: []const u32 = &.{};
    var run_rows: []const ?u64 = &.{};
    var user_stacks_done: std.atomic.Value(u32) = .init(0);

    /// `live`: server mode, where the build runner holds the test's stderr
    /// until the step ends — the heartbeat writes to the runner's own.
    fn start(binary: []const u8, run: Run, live: bool) void {
        t0 = .now(runner_threaded_io, .awake);
        if (builtin.os.tag != .linux) return;
        label = switch (run.spec) {
            .all => std.fmt.bufPrint(&label_buf, "{s}", .{binary}),
            .part => |p| std.fmt.bufPrint(&label_buf, "{s} {d}/{d}", .{ binary, p.index + 1, p.count }),
            .exclusive => std.fmt.bufPrint(&label_buf, "{s} exclusive", .{binary}),
        } catch binary;
        total = @intCast(run.indices.len);
        predicted_us = run.predicted_us;
        run_indices = run.indices;
        run_rows = run.rows_us;
        if (live) sink = openSink();
        const thread = std.Thread.spawn(.{}, loop, .{}) catch |err|
            fatal("cannot start the heartbeat and hang-watch thread: {t}", .{err});
        thread.detach();
    }

    /// The build runner's stderr when it is a pipe (module doc), else null.
    fn openSink() ?std.posix.fd_t {
        const linux = std.os.linux;
        var path_buf: [32]u8 = undefined;
        const path = std.fmt.bufPrintSentinel(&path_buf, "/proc/{d}/fd/2", .{linux.getppid()}, 0) catch return null;
        // APPEND: a stderr redirected to a file is written at its end, never
        // over its head; NONBLOCK: a reader that stopped draining costs a
        // dropped line, never a stalled test.
        const rc = linux.open(path, .{ .ACCMODE = .WRONLY, .APPEND = true, .NONBLOCK = true, .NOCTTY = true, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return null;
        const fd: std.posix.fd_t = @intCast(rc);
        var stx: linux.Statx = undefined;
        const is_pipe = linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true }, &stx)) == .SUCCESS and linux.S.ISFIFO(stx.mode);
        if (is_pipe) return fd;
        _ = linux.close(fd);
        return null;
    }

    fn sinceT0Us() u64 {
        return elapsedUs(t0);
    }

    fn testStarted(index: u32) void {
        current_t0_us.store(sinceT0Us(), .monotonic);
        current.store(index, .release); // pairs with the loop's acquire: the start time is set first
    }

    fn testEnded(status: std.zig.Server.Message.TestResults.Status) void {
        current.store(idle, .monotonic);
        switch (status) {
            .pass => {},
            .skip => _ = skipped.fetchAdd(1, .monotonic),
            .fail => _ = failed.fetchAdd(1, .monotonic),
        }
        _ = done.fetchAdd(1, .monotonic);
    }

    fn loop() void {
        var ticks: u32 = 0;
        while (true) {
            const req: std.os.linux.timespec = .{ .sec = 1, .nsec = 0 };
            _ = std.os.linux.nanosleep(&req, null);
            watch();
            ticks += 1;
            if (sink != null and ticks % heartbeat_period_s == 0) beat();
        }
    }

    fn beat() void {
        var buf: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        w.print("[{s}] {d}/{d}, {f}", .{ label, done.load(.monotonic), total, fmtUs(sinceT0Us()) }) catch {};
        const cur = current.load(.acquire);
        if (cur != idle) {
            const name = builtin.test_functions[run_indices[cur]].name;
            const shown = name[0..@min(name.len, max_name_shown)];
            w.print("; running \"{s}{s}\" for {f}", .{ shown, if (shown.len < name.len) "…" else "", fmtUs(sinceT0Us() -| current_t0_us.load(.monotonic)) }) catch {};
        }
        emit(&w);
    }

    fn watch() void {
        const cur = current.load(.acquire);
        if (cur == idle) return;
        // A test that started after the load above only makes `ran` smaller.
        const ran = sinceT0Us() -| current_t0_us.load(.monotonic);
        const limit = hang_limit_override_us orelse shard_plan.hang.limitUs(run_rows[cur]);
        if (ran >= limit) fire(cur, ran, limit);
    }

    fn fire(cur: u32, ran_us: u64, limit_us: u64) noreturn {
        var buf: [2048]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        w.print("HUNG TEST: \"{s}\" ran {f} (cost table: ", .{ builtin.test_functions[run_indices[cur]].name, fmtTenths(ran_us) }) catch {};
        if (run_rows[cur]) |row| w.print("{f}", .{fmtTenths(row)}) catch {} else w.writeAll("no row") catch {};
        w.print("; limit: {f}) [{s}]", .{ fmtTenths(limit_us), label }) catch {};
        emit(&w);
        // A leading newline ends terminal mode's `i/N name...` line.
        writeStderr("\n");
        writeStderr(w.buffered());
        dumpThreads();
        dumpUserStacks();
        abortOwnFuseConnections();
        std.os.linux.exit_group(hung_exit_code);
    }

    /// Before the exit: abort every FUSE connection of a squeezefs mount this
    /// test process owns (`/build/tmp/mnt-<pid>…`: the in-process session
    /// rigs' and the smoke rigs' pid-tagged mountpoints). A thread of ours
    /// parked in a request on a mount we serve would otherwise keep the
    /// process a zombie past `exit_group` (the fd table lives while it
    /// does, so the connection never ends and the build runner never sees
    /// the shard end — two gates of 2026-10-20 ran to their caps that way);
    /// a child of ours parked on a rig's dead daemon is freed the same way.
    fn abortOwnFuseConnections() void {
        const linux = std.os.linux;
        var buf: [1 << 20]u8 = undefined;
        const text = Io.Dir.cwd().readFile(runner_threaded_io, "/proc/self/mountinfo", &buf) catch return;
        var tag_buf: [32]u8 = undefined;
        const tag = std.fmt.bufPrint(&tag_buf, "/mnt-{d}", .{linux.getpid()}) catch return;
        var lines = std.mem.tokenizeScalar(u8, text, '\n');
        while (lines.next()) |line| {
            // `ID PARENT MAJ:MIN ROOT MOUNTPOINT OPTS… - FSTYPE SOURCE SUPEROPTS`.
            var it = std.mem.tokenizeScalar(u8, line, ' ');
            _ = it.next() orelse continue;
            _ = it.next() orelse continue;
            const majmin = it.next() orelse continue;
            _ = it.next() orelse continue;
            const mp = it.next() orelse continue;
            while (it.next()) |tok| {
                if (std.mem.eql(u8, tok, "-")) break;
            } else continue;
            const fstype = it.next() orelse continue;
            if (!std.mem.startsWith(u8, fstype, "fuse")) continue;
            const at = std.mem.indexOf(u8, mp, tag) orelse continue;
            const rest = mp[at + tag.len ..];
            if (rest.len != 0 and rest[0] != '-' and rest[0] != '/') continue;
            const colon = std.mem.indexOfScalar(u8, majmin, ':') orelse continue;
            var pb: [96:0]u8 = undefined;
            const path = std.fmt.bufPrintSentinel(&pb, "/sys/fs/fuse/connections/{s}/abort", .{majmin[colon + 1 ..]}, 0) catch continue;
            const fd_rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
            if (linux.errno(fd_rc) != .SUCCESS) continue;
            const fd: linux.fd_t = @intCast(fd_rc);
            defer _ = linux.close(fd);
            var msg: [256]u8 = undefined;
            var w: Io.Writer = .fixed(&msg);
            if (linux.errno(linux.write(fd, "1", 1)) == .SUCCESS) {
                w.print("hang watch: aborted the FUSE connection of {s} ({s}) so every parked request returns and the process can exit\n", .{ mp, majmin }) catch {};
            } else {
                w.print("hang watch: could not abort the FUSE connection of {s} ({s})\n", .{ mp, majmin }) catch {};
            }
            writeStderr(w.buffered());
        }
    }

    /// Raw: the stuck test may hold `std.debug`'s stderr lock.
    fn writeStderr(bytes: []const u8) void {
        _ = std.os.linux.write(2, bytes.ptr, bytes.len);
    }

    /// Every other thread's name, state, CPU, wait channel and kernel stack
    /// (the stack file needs root: the venues run as root).
    fn dumpThreads() void {
        const io = runner_threaded_io;
        var dir = Io.Dir.openDirAbsolute(io, "/proc/self/task", .{ .iterate = true }) catch return writeStderr("(/proc/self/task unreadable)\n");
        defer dir.close(io);
        var it = dir.iterate();
        while (it.next(io) catch null) |entry| {
            // Not this thread: its stack is the read of its own stack.
            if ((std.fmt.parseUnsigned(std.os.linux.pid_t, entry.name, 10) catch 0) == std.os.linux.gettid()) continue;
            var out: [8192]u8 = undefined;
            var w: Io.Writer = .fixed(&out);
            var stat_buf: [1024]u8 = undefined;
            var wchan_buf: [128]u8 = undefined;
            var stack_buf: [6144]u8 = undefined;
            const stat = procFile(dir, entry.name, "stat", &stat_buf);
            // `tid (comm) S …`: comm may hold spaces and parens, so the fields start after the last ')'.
            const open = std.mem.indexOfScalar(u8, stat, '(') orelse 0;
            const close = std.mem.lastIndexOfScalar(u8, stat, ')') orelse stat.len;
            var fields = std.mem.tokenizeScalar(u8, stat[@min(close + 1, stat.len)..], ' ');
            var f: [13][]const u8 = @splat("?");
            for (&f) |*x| x.* = fields.next() orelse break;
            // utime + stime (stat fields 14, 15) in USER_HZ ticks: 100 on every Linux ABI.
            const ticks = (std.fmt.parseUnsigned(u64, f[11], 10) catch 0) + (std.fmt.parseUnsigned(u64, f[12], 10) catch 0);
            w.print("thread {s} \"{s}\" state {s} cpu {d}.{d:0>2}s wchan {s}\n", .{
                entry.name,  if (close > open) stat[open + 1 .. close] else "?",
                f[0],        ticks / 100,
                ticks % 100, procFile(dir, entry.name, "wchan", &wchan_buf),
            }) catch {};
            var lines = std.mem.tokenizeScalar(u8, procFile(dir, entry.name, "stack", &stack_buf), '\n');
            while (lines.next()) |l| w.print("    {s}\n", .{l}) catch {};
            writeStderr(w.buffered());
        }
    }

    fn procFile(dir: Io.Dir, tid: []const u8, file: []const u8, buf: []u8) []const u8 {
        var path: [64]u8 = undefined;
        const p = std.fmt.bufPrint(&path, "{s}/{s}", .{ tid, file }) catch return "?";
        const text = dir.readFile(runner_threaded_io, p, buf) catch |err| return @errorName(err);
        return std.mem.trimEnd(u8, text, "\n");
    }

    /// Each thread's user stack, the test's own thread first: SIGQUIT to one
    /// thread at a time, whose handler unwinds from the interrupted context.
    fn dumpUserStacks() void {
        const linux = std.os.linux;
        const act: linux.Sigaction = .{ .handler = .{ .sigaction = onQuit }, .mask = linux.sigemptyset(), .flags = linux.SA.SIGINFO | linux.SA.RESTART };
        _ = linux.sigaction(.QUIT, &act, null);
        const pid = linux.getpid();
        if (!userStack(pid, pid)) return;
        const io = runner_threaded_io;
        var dir = Io.Dir.openDirAbsolute(io, "/proc/self/task", .{ .iterate = true }) catch return;
        defer dir.close(io);
        var it = dir.iterate();
        while (it.next(io) catch null) |entry| {
            const tid = std.fmt.parseUnsigned(linux.pid_t, entry.name, 10) catch continue;
            if (tid == pid or tid == linux.gettid()) continue;
            if (!userStack(pid, tid)) return;
        }
    }

    /// False when the handler did not finish in time: the dump ends there.
    fn userStack(pid: std.os.linux.pid_t, tid: std.os.linux.pid_t) bool {
        const linux = std.os.linux;
        const before = user_stacks_done.load(.acquire);
        // A thread that exited since the listing has no stack to dump.
        if (linux.errno(linux.tgkill(pid, tid, .QUIT)) != .SUCCESS) return true;
        var waited_ms: u32 = 0;
        while (user_stacks_done.load(.acquire) == before) : (waited_ms += 10) {
            if (waited_ms >= user_stack_wait_ms) {
                writeStderr("(a user stack did not finish within 2 s: the rest are skipped)\n");
                return false;
            }
            const req: linux.timespec = .{ .sec = 0, .nsec = 10 * std.time.ns_per_ms };
            _ = linux.nanosleep(&req, null);
        }
        return true;
    }

    fn onQuit(_: std.os.linux.SIG, _: *const std.os.linux.siginfo_t, ctx_ptr: ?*anyopaque) callconv(.c) void {
        var head: [48]u8 = undefined;
        writeStderr(std.fmt.bufPrint(&head, "user stack of thread {d}:\n", .{std.os.linux.gettid()}) catch "");
        const ctx = std.debug.cpu_context.fromPosixSignalContext(ctx_ptr);
        std.debug.dumpCurrentStackTrace(.{ .context = if (ctx) |*c| c else null });
        _ = user_stacks_done.fetchAdd(1, .release); // pairs with userStack's acquire
    }

    /// The test name's head: enough to find it, short enough for one line.
    const max_name_shown = 96;

    fn finish() void {
        if (sink == null) return;
        var buf: [256]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        w.print("[{s}] done {d}/{d} in {f}", .{ label, done.load(.monotonic), total, fmtUs(sinceT0Us()) }) catch {};
        if (predicted_us) |p| w.print(" (the table's plan {f})", .{fmtUs(p)}) catch {};
        w.print("; {d} skipped, {d} failed", .{ skipped.load(.monotonic), failed.load(.monotonic) }) catch {};
        emit(&w);
    }

    fn emit(w: *Io.Writer) void {
        w.writeByte('\n') catch return;
        const fd = sink orelse return;
        _ = std.os.linux.write(fd, w.buffered().ptr, w.buffered().len);
    }

    fn fmtUs(us: u64) std.fmt.Alt(u64, formatUs) {
        return .{ .data = us };
    }

    fn formatUs(us: u64, w: *Io.Writer) Io.Writer.Error!void {
        const s = us / std.time.us_per_s;
        if (s >= 60) return w.print("{d}m{d:0>2}s", .{ s / 60, s % 60 });
        return w.print("{d}s", .{s});
    }

    fn fmtTenths(us: u64) std.fmt.Alt(u64, formatTenths) {
        return .{ .data = us };
    }

    fn formatTenths(us: u64, w: *Io.Writer) Io.Writer.Error!void {
        const tenths = us / (std.time.us_per_s / 10);
        return w.print("{d}.{d}s", .{ tenths / 10, tenths % 10 });
    }
};

pub fn log(
    comptime message_level: std.log.Level,
    comptime scope: @EnumLiteral(),
    comptime format: []const u8,
    args: anytype,
) void {
    @disableInstrumentation();
    if (@backingInt(message_level) <= @backingInt(std.log.Level.err)) {
        log_err_count +|= 1;
    }
    if (@backingInt(message_level) <= @backingInt(testing.log_level)) {
        std.debug.print(
            "[" ++ @tagName(scope) ++ "] (" ++ @tagName(message_level) ++ "): " ++ format ++ "\n",
            args,
        );
    }
}

/// Simpler main(), exercising fewer language features, so that
/// work-in-progress backends can handle it.
pub fn mainSimple() anyerror!void {
    @disableInstrumentation();
    // is the backend capable of calling `Io.File.writeAll`?
    const enable_write = switch (builtin.zig_backend) {
        .stage2_aarch64, .stage2_riscv64 => true,
        else => false,
    };
    // is the backend capable of calling `Io.Writer.print`?
    const enable_print = switch (builtin.zig_backend) {
        .stage2_aarch64, .stage2_riscv64 => true,
        else => false,
    };

    testing.allocator_instance = .init(std.heap.page_allocator, .{});
    testing.io_instance = .init(testing.allocator, .{});

    var passed: u64 = 0;
    var skipped: u64 = 0;
    var failed: u64 = 0;

    // we don't want to bring in File and Writer if the backend doesn't support it
    const stdout = if (enable_write) Io.File.stdout() else {};

    for (builtin.test_functions) |test_fn| {
        if (enable_write) {
            stdout.writeStreamingAll(runner_threaded_io, test_fn.name) catch {};
            stdout.writeStreamingAll(runner_threaded_io, "... ") catch {};
        }
        if (test_fn.func()) |_| {
            if (enable_write) stdout.writeStreamingAll(runner_threaded_io, "PASS\n") catch {};
        } else |err| {
            if (err != error.SkipZigTest) {
                if (enable_write) stdout.writeStreamingAll(runner_threaded_io, "FAIL\n") catch {};
                failed += 1;
                if (!enable_write) return err;
                continue;
            }
            if (enable_write) stdout.writeStreamingAll(runner_threaded_io, "SKIP\n") catch {};
            skipped += 1;
            continue;
        }
        passed += 1;
    }
    if (enable_print) {
        var unbuffered_stdout_writer = stdout.writer(runner_threaded_io, &.{});
        unbuffered_stdout_writer.interface.print(
            "{} passed, {} skipped, {} failed\n",
            .{ passed, skipped, failed },
        ) catch {};
    }
    if (failed != 0) std.process.exit(1);
}

var is_fuzz_test: bool = undefined;
var fuzz_runner: if (builtin.fuzz) struct {
    indexes: []u32,
    server: *std.zig.Server,
    gpa: std.mem.Allocator,
    threaded_io: *Io.Threaded,
    input_poller: Io.Future(Io.Cancelable!void),

    comptime {
        assert(builtin.fuzz); // `fuzz_runner` was analyzed in non-fuzzing compilation
    }

    export fn runner_test_run(i: u32) void {
        @disableInstrumentation();

        fuzz_runner.server.serveU32Message(.fuzz_test_change, i) catch |e| switch (e) {
            error.WriteFailed => panic("failed to write to stdout: {t}", .{stdout_writer.err.?}),
        };

        testing.allocator_instance = .init(std.heap.page_allocator, .{
            .canary = 0xc3a701ba,
            .check_write_after_free = true,
        });
        defer if (testing.allocator_instance.deinit() != 0) std.process.exit(1);
        is_fuzz_test = false;

        testing.io_instance = .init(testing.allocator, .{
            .argv0 = fuzz_runner.threaded_io.argv0,
            .environ = fuzz_runner.threaded_io.environ.process_environ,
        });
        defer testing.io_instance.deinit();

        builtin.test_functions[fuzz_runner.indexes[i]].func() catch |err| switch (err) {
            error.SkipZigTest => return,
            else => {
                if (@errorReturnTrace()) |trace| {
                    std.debug.dumpErrorReturnTrace(trace);
                }
                std.debug.print("failed with error.{t}\n", .{err});
                std.process.exit(1);
            },
        };

        if (!is_fuzz_test) @panic("missed call to std.testing.fuzz");
        if (log_err_count != 0) @panic("error logs detected");
    }

    export fn runner_test_name(i: u32) fuzz_abi.Slice {
        @disableInstrumentation();
        return .fromSlice(builtin.test_functions[fuzz_runner.indexes[i]].name);
    }

    export fn runner_broadcast_input(test_i: u32, bytes_slice: fuzz_abi.Slice) void {
        @disableInstrumentation();
        const bytes = bytes_slice.toSlice();
        fuzz_runner.server.serveBroadcastFuzzInputMessage(test_i, bytes) catch |e| switch (e) {
            error.WriteFailed => panic("failed to write to stdout: {t}", .{stdout_writer.err.?}),
        };
    }

    export fn runner_start_input_poller() void {
        @disableInstrumentation();
        const io = fuzz_runner.threaded_io.io();
        const future = io.concurrent(inputPoller, .{}) catch |e| switch (e) {
            error.ConcurrencyUnavailable => @panic("failed to spawn concurrent fuzz input poller"),
        };
        fuzz_runner.input_poller = future;
    }

    export fn runner_stop_input_poller() void {
        @disableInstrumentation();
        const io = fuzz_runner.threaded_io.io();
        assert(fuzz_runner.input_poller.cancel(io) == error.Canceled);
    }

    export fn runner_futex_wait(ptr: *const u32, expected: u32) bool {
        @disableInstrumentation();
        const io = fuzz_runner.threaded_io.io();
        return io.futexWait(u32, ptr, expected) == error.Canceled;
    }

    export fn runner_futex_wake(ptr: *const u32, waiters: u32) void {
        @disableInstrumentation();
        const io = fuzz_runner.threaded_io.io();
        io.futexWake(u32, ptr, waiters);
    }

    fn inputPoller() Io.Cancelable!void {
        @disableInstrumentation();
        switch (inputPollerInner()) {
            error.Canceled => |e| return e,
            error.ReadFailed => {
                if (stdin_reader.err.? == error.Canceled) return error.Canceled;
                panic("failed to read from stdin: {t}", .{stdin_reader.err.?});
            },
            error.EndOfStream => @panic("unexpected end of stdin"),
        }
    }

    fn inputPollerInner() (Io.Cancelable || Io.Reader.Error) {
        @disableInstrumentation();
        const server = fuzz_runner.server;
        var large_bytes_list: std.ArrayList(u8) = .empty;
        defer large_bytes_list.deinit(fuzz_runner.gpa);
        while (true) {
            const hdr = try server.receiveMessage();
            if (hdr.tag != .new_fuzz_input) {
                panic("unexpected message: {x}\n", .{@backingInt(hdr.tag)});
            }
            const test_i = try server.receiveBody_u32();
            const input_len = hdr.bytes_len - 4;
            const bytes = if (input_len <= server.in.buffer.len)
                try server.in.take(input_len)
            else bytes: {
                large_bytes_list.resize(fuzz_runner.gpa, @intCast(input_len)) catch @panic("OOM");
                try server.in.readSliceAll(large_bytes_list.items);
                break :bytes large_bytes_list.items;
            };
            if (fuzz_abi.fuzzer_receive_input(test_i, .fromSlice(bytes))) {
                return error.Canceled;
            }
        }
    }
} else void = undefined;

pub fn fuzz(
    context: anytype,
    comptime testOne: fn (context: @TypeOf(context), *std.testing.Smith) anyerror!void,
    options: testing.FuzzInputOptions,
) anyerror!void {
    // Prevent this function from confusing the fuzzer by omitting its own code
    // coverage from being considered.
    @disableInstrumentation();

    // Some compiler backends are not capable of handling fuzz testing yet but
    // we still want CI test coverage enabled.
    if (need_simple) return;

    // Smoke test to ensure the test did not use conditional compilation to
    // contradict itself by making it not actually be a fuzz test when the test
    // is built in fuzz mode.
    is_fuzz_test = true;

    // Ensure no test failure occurred before starting fuzzing.
    if (log_err_count != 0) @panic("error logs detected");

    // libfuzzer is in a separate compilation unit so that its own code can be
    // excluded from code coverage instrumentation. It needs a function pointer
    // it can call for checking exactly one input. Inside this function we do
    // our standard unit test checks such as memory leaks, and interaction with
    // error logs.
    const global = struct {
        var ctx: @TypeOf(context) = undefined;

        fn test_one() callconv(.c) bool {
            @disableInstrumentation();
            testing.allocator_instance = .init(std.heap.page_allocator, .{
                .canary = 0xcacce5e0,
                .check_write_after_free = true,
            });
            defer if (testing.allocator_instance.deinit() != 0) std.process.exit(1);
            log_err_count = 0;
            testOne(ctx, @constCast(&testing.Smith{ .in = null })) catch |err| switch (err) {
                error.SkipZigTest => return true,
                else => {
                    const stderr = std.debug.lockStderr(&.{}).terminal();
                    p: {
                        if (@errorReturnTrace()) |trace| {
                            std.debug.writeErrorReturnTrace(trace, stderr) catch break :p;
                        }
                        stderr.writer.print("failed with error.{t}\n", .{err}) catch break :p;
                    }
                    std.process.exit(1);
                },
            };
            if (log_err_count != 0) {
                const stderr = std.debug.lockStderr(&.{}).terminal();
                stderr.writer.print("error logs detected\n", .{}) catch {};
                std.process.exit(1);
            }
            return false;
        }
    };

    if (builtin.fuzz) {
        // Preserve the calling test's allocator state
        const prev_allocator_state = testing.allocator_instance;
        defer testing.allocator_instance = prev_allocator_state;

        global.ctx = context;
        fuzz_abi.fuzzer_set_test(&global.test_one);
        for (options.corpus) |elem|
            fuzz_abi.fuzzer_new_input(.fromSlice(elem));
        fuzz_abi.fuzzer_start_test();
        return;
    }

    // When the unit test executable is not built in fuzz mode, only run the
    // provided corpus.
    for (options.corpus) |input| {
        var smith: testing.Smith = .{ .in = input };
        try testOne(context, &smith);
    }

    // In case there is no provided corpus, also use an empty
    // string as a smoke test.
    var smith: testing.Smith = .{ .in = "" };
    try testOne(context, &smith);
}
