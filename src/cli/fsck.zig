//! `squeezefs fsck` / `squeezefs scrub` / `squeezefs fsck merge-reports`
//! (charter §8; `ops_fsck.md` §6–§7). OFFLINE (a `sqmeta://` URI): the set
//! opened through the writer door exactly as `clone` opens it (`offline.
//! withSet`: the flock + the claim — a live mount refuses by name), the
//! plane bound, the engine run, the report printed, exit 1 iff findings
//! (also a dry-run `--repair` with findings), `--repair --apply` under
//! that same D0-guarded open with the quarantine dir at
//! `/var/lib/squeezefs/quarantine/<set uuid>` (or `--quarantine-dir`).
//! ONLINE (a mountpoint): the engine is
//! `runner.linux.JobBody`, the fabric's `JobType.fsck` body the serving
//! daemon registered over its live binding; the CLI SUBMITS through the
//! daemon's admin lane (`admin_lane.zig`), waits for the job's terminal
//! state (the fabric's typed enum, never a string — decision 0002 row
//! 404), reads the bounded report back and prints it; `--repair` online
//! is refused by name (repair is the offline, D0-guarded act). `merge-
//! reports` is portable (durable JSON in, one report out).

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const cli = @import("cli.zig");
const args = @import("args.zig");
const fsck = @import("../ops/fsck.zig");
const finding = fsck.finding;
const set_config = @import("../data/set_config.zig");

const is_linux = builtin.os.tag == .linux;

/// Where `--repair --apply` quarantines without `--quarantine-dir`: the
/// host's state directory, one subdirectory per set (0700, created by the
/// sink with its parents).
pub const default_quarantine_root = "/var/lib/squeezefs/quarantine";

/// `fsck merge-reports [--json] FILE…`.
pub fn runMerge(p: cli.Process, m: args.FsckMerge) u8 {
    const gpa = p.gpa;
    var reports: std.ArrayList(finding.Report) = .empty;
    defer {
        for (reports.items) |*r| r.deinit(gpa);
        reports.deinit(gpa);
    }
    for (m.files) |path| {
        const text = Io.Dir.cwd().readFileAlloc(p.io, path, gpa, .limited(64 << 20)) catch |e| {
            p.stderr.print("squeezefs fsck merge-reports: cannot read {s}: {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        defer gpa.free(text);
        const r = finding.Report.parse(gpa, text) catch |e| {
            p.stderr.print("squeezefs fsck merge-reports: {s} is not a schema-{d} fsck report ({s})\n", .{ path, fsck.REPORT_SCHEMA, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        reports.append(gpa, r) catch return cli.exit_failed;
    }
    var merged = finding.merge(gpa, reports.items) catch return cli.exit_failed;
    defer merged.deinit(gpa);
    return emit(p, merged, m.json);
}

/// Print the report (JSON or text) and answer its exit verdict.
fn emit(p: cli.Process, r: finding.Report, json: bool) u8 {
    if (json) r.writeJson(p.stdout) catch return cli.exit_failed else r.writeText(p.stdout) catch return cli.exit_failed;
    if (json) p.stdout.writeByte('\n') catch return cli.exit_failed;
    p.stdout.flush() catch return cli.exit_failed;
    return if (r.hasFindings()) cli.exit_failed else cli.exit_ok;
}

/// `fsck` / `scrub` over a target.
pub fn run(p: cli.Process, a: args.Fsck) u8 {
    if (!is_linux) {
        p.stderr.writeAll("squeezefs fsck: Linux only (charter §1.2) — `fsck merge-reports` is the portable arm\n") catch {};
        return cli.exit_failed;
    }
    if (!a.offline()) return linux_arm.online(p, a);
    return linux_arm.offline(p, a);
}

const linux_arm = if (is_linux) struct {
    const offline_set = @import("offline.zig");
    const derive = @import("../core/derive.zig");
    const mem_budget = @import("../mem/budget.zig");
    const runner = fsck.runner;

    const Body = struct {
        p: cli.Process,
        a: args.Fsck,
        verb: []const u8,

        fn failed(self: *Body, comptime fmt: []const u8, x: anytype) u8 {
            self.p.stderr.print("squeezefs {s}: ", .{self.verb}) catch {};
            self.p.stderr.print(fmt, x) catch {};
            self.p.stderr.writeAll("\n") catch {};
            self.p.stderr.flush() catch {};
            return cli.exit_failed;
        }

        fn run(ctx: *anyopaque, set: *offline_set.Set) u8 {
            const self: *Body = @ptrCast(@alignCast(ctx));
            const p = self.p;
            const gpa = p.gpa;
            const a = self.a;
            inject(p.env, set) catch |e| return self.failed("the injection seam failed ({s})", .{@errorName(e)});
            const facts = derive.facts();
            const budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = set.io, .env = p.env, .facts = facts });
            const bound = runner.linux.bind(gpa, set.io, set.be, set.plane, .{
                .env = p.env,
                .mode = .offline,
                .mem_budget = budget_bytes,
                .shard = if (a.shards) |s| .{ .k = s.k, .n = s.n } else null,
            }) catch |e| return self.failed("cannot bind the set ({s})", .{@errorName(e)});
            defer runner.linux.destroy(bound);
            // The quarantine home: `--quarantine-dir`, else the host's
            // `/var/lib/squeezefs/quarantine/<set uuid>` (an operator
            // artefact, 0700; the cache dir that once held it is gone).
            var sink: ?runner.linux.DirSink = null;
            defer if (sink) |*s| s.deinit();
            var ropts: ?fsck.classes.Quarantine.Sink = null;
            if (a.repair and a.apply) {
                var home_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
                const home: []const u8 = a.quarantine_dir orelse blk: {
                    var w: std.Io.Writer = .fixed(&home_buf);
                    w.writeAll(default_quarantine_root ++ "/") catch return self.failed("the quarantine path is too long", .{});
                    set_config.writeUuid(&w, set.cfg.set_uuid) catch return self.failed("the quarantine path is too long", .{});
                    break :blk w.buffered();
                };
                sink = runner.linux.DirSink.init(gpa, set.be.pool, home) catch return self.failed("out of memory", .{});
                ropts = sink.?.sink();
            }
            const throttle: Throttle = .{ .pct = a.throttle };
            var report = runner.run(&bound.host, .{
                .scrub = a.scrub,
                .scrub_only = a.scrub_only,
                .throttle = throttle.face(),
                .repair = if (a.repair) .{ .apply = a.apply, .quarantine = ropts } else null,
            }, if (bound.verifier) |*v| v else null) catch |e| return self.failed("the run failed ({s}) — see the log", .{@errorName(e)});
            defer report.deinit(gpa);
            return emit(p, report, a.json);
        }
    };

    const knobs = @import("../core/knobs.zig");
    const key = @import("../meta/key.zig");
    const record = @import("../meta/record.zig");
    const tree = @import("../meta/tree.zig");
    const economy = @import("../meta/economy.zig");
    const block_refs = @import("../data/block_refs.zig");

    /// `SQUEEZEFS_TEST_FSCK_INJECT`: ONE defect planted under the guarded
    /// open before the census (the e2e's red row over a real set).
    fn inject(env: *const std.process.Environ.Map, set: *offline_set.Set) !void {
        const arm = knobs.enumKnob("SQUEEZEFS_TEST_FSCK_INJECT", env);
        if (std.mem.eql(u8, arm, "off")) return;
        const gpa = set.gpa;
        const be = set.be;
        if (std.mem.eql(u8, arm, "stray-ref")) {
            // The first durable reference, re-stated at a block index no
            // layout names: durable 2 vs derived 1 on that block.
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(gpa, &kvs);
            const r = key.volumeRange(0);
            const hi = key.volumeRange(std.math.maxInt(u64)).hi;
            try be.range(gpa, key.TREE_BLOCK_REFS, r.lo.slice(), hi.slice(), 1, &kvs);
            if (kvs.items.len == 0) return error.NothingToInject;
            const k = try key.BlockRefKey.decode(kvs.items[0].key);
            const t = try be.begin(@src());
            {
                errdefer be.abandon(t);
                // 1000 past the reference's block index: an index no layout names.
                try block_refs.stageOps(t, &.{block_refs.taken(.{ .vol_tag = k.volTag(), .block_idx = k.blockIdx(), .owner_ino = k.ownerIno(), .block_index = k.blockIndex() +| 1000 })});
            }
            try be.commit(t);
            return;
        }
        // `orphan-inode`: the root's first regular-file dentry dropped,
        // its record kept (the crashed cross-volume create's shape).
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        try be.range(gpa, key.TREE_DENTRIES, &key.dentryKey(1, 0, 0), &key.dentryKey(1, key.HASH54_MAX, std.math.maxInt(u8)), 4096, &kvs);
        for (kvs.items) |kv| {
            if (kv.key.len != key.DENTRY_KEY_LEN or kv.key[key.INO_MAJOR_KIND_OFF] != key.TREE_DENTRIES) continue;
            const dv = record.DentryValue.decode(kv.value) catch continue;
            if (dv.file_type != economy.ftByte(economy.S.IFREG)) continue;
            const t = try be.begin(@src());
            {
                errdefer be.abandon(t);
                try t.stageDelete(key.TREE_DENTRIES, kv.key);
            }
            try be.commit(t);
            return;
        }
        return error.NothingToInject;
    }

    /// The CLI's fixed duty cycle (the fabric's live one binds online).
    const Throttle = struct {
        pct: u8,
        fn face(self: *const Throttle) fsck.scrub.Throttle {
            return .{ .ctx = @constCast(self), .pct = &pctFn };
        }
        fn pctFn(ctx: ?*anyopaque) u8 {
            const self: *const Throttle = @ptrCast(@alignCast(ctx.?));
            return self.pct;
        }
    };

    const admin_lane = @import("admin_lane.zig");
    const clock = @import("../core/clock.zig");
    const errors = @import("../core/errors.zig");

    /// The online run's wait bound: the engine's settle (2 s default) plus
    /// a scrub of the set at the duty cycle — an hour covers a 2 GiB venue
    /// set a hundred times over and a 100 TiB set's first pass; past it the
    /// job stays live and `job status` is the operator's view.
    const ONLINE_WAIT_NS: u64 = 3600 * std.time.ns_per_s;

    const Online = struct {
        p: cli.Process,
        a: args.Fsck,
        verb: []const u8,

        fn run(ctx: *anyopaque, lane: *admin_lane.Lane) u8 {
            const self: *Online = @ptrCast(@alignCast(ctx));
            const p = self.p;
            const gpa = p.gpa;
            var flags: u32 = 0;
            if (self.a.scrub) flags |= admin_lane.frames.FSCK_FLAG_SCRUB;
            if (self.a.scrub_only) flags |= admin_lane.frames.FSCK_FLAG_SCRUB_ONLY;
            // `admin_fsck`: the typed verb carries the shard word (its codec
            // refuses a malformed one); the whole set when none was asked.
            const shard_k: u32 = if (self.a.shards) |s| s.k else 0;
            const shard_n: u32 = if (self.a.shards) |s| s.n else 0;
            const sub = lane.admin().fsck(flags, self.a.throttle, shard_k, shard_n) catch |e| {
                p.stderr.print("squeezefs {s}: the submission failed ({s}, {t})\n", .{ self.verb, @errorName(e), errors.toErrno(e) }) catch {};
                return cli.exit_failed;
            };
            if (sub.verdict != .ok) {
                p.stderr.print("squeezefs {s}: refused: {s}\n", .{ self.verb, admin_lane.verdictText(sub.verdict) }) catch {};
                p.stderr.flush() catch {};
                return cli.exit_failed;
            }
            if (self.a.shards) |s| {
                p.stderr.print("squeezefs {s}: job-{x:0>16} submitted to the live mount (shard {d}/{d}); waiting for its terminal state\n", .{ self.verb, sub.job_id, s.k, s.n }) catch {};
            } else {
                p.stderr.print("squeezefs {s}: job-{x:0>16} submitted to the live mount; waiting for its terminal state\n", .{ self.verb, sub.job_id }) catch {};
            }
            p.stderr.flush() catch {};
            const rec = admin_lane.waitTerminal(lane, sub.job_id, clock.monotonicNs() + ONLINE_WAIT_NS) catch |e| {
                p.stderr.print("squeezefs {s}: the live table could not be read ({s}, {t})\n", .{ self.verb, @errorName(e), errors.toErrno(e) }) catch {};
                return cli.exit_failed;
            } orelse {
                p.stderr.print("squeezefs {s}: job-{x:0>16} vanished from the live table\n", .{ self.verb, sub.job_id }) catch {};
                return cli.exit_failed;
            };
            if (!rec.isTerminal()) {
                p.stderr.print("squeezefs {s}: job-{x:0>16} is still {s} after the wait bound — `squeezefs job status <mountpoint> job-{x:0>16}` follows it\n", .{ self.verb, sub.job_id, @tagName(rec.state), sub.job_id }) catch {};
                return cli.exit_failed;
            }
            if (rec.state != .completed) {
                p.stderr.print("squeezefs {s}: job-{x:0>16} ended {s} (error class {s}) — see the daemon's log\n", .{ self.verb, sub.job_id, @tagName(rec.state), @tagName(rec.error_class) }) catch {};
                return cli.exit_failed;
            }
            const json = (lane.admin().fsckReport(gpa, sub.job_id) catch |e| {
                p.stderr.print("squeezefs {s}: the report could not be read ({s}, {t})\n", .{ self.verb, @errorName(e), errors.toErrno(e) }) catch {};
                return cli.exit_failed;
            }) orelse {
                p.stderr.print("squeezefs {s}: job-{x:0>16} completed but carries no report\n", .{ self.verb, sub.job_id }) catch {};
                return cli.exit_failed;
            };
            defer gpa.free(json);
            var report = finding.Report.parse(gpa, json) catch |e| {
                p.stderr.print("squeezefs {s}: the daemon's report does not parse ({s})\n", .{ self.verb, @errorName(e) }) catch {};
                return cli.exit_failed;
            };
            defer report.deinit(gpa);
            return emit(p, report, self.a.json);
        }
    };

    fn online(p: cli.Process, a: args.Fsck) u8 {
        const verb: []const u8 = if (a.scrub_only) "scrub" else "fsck";
        if (a.repair) {
            p.stderr.print("squeezefs {s}: refused: `--repair` is the OFFLINE act (verify-before-repair under the D0-guarded open) — run `squeezefs fsck --offline --repair [--apply] sqmeta://…` on the unmounted set; the online run detects and reports\n", .{verb}) catch {};
            p.stderr.flush() catch {};
            return cli.exit_failed;
        }
        var body: Online = .{ .p = p, .a = a, .verb = verb };
        return admin_lane.withLane(p, verb, a.target, .{ .ctx = &body, .run = Online.run });
    }

    fn offline(p: cli.Process, a: args.Fsck) u8 {
        var body: Body = .{ .p = p, .a = a, .verb = if (a.scrub_only) "scrub" else "fsck" };
        // `--repair --apply` is a WRITER: the same guarded open (the writer
        // door), never a probe; a dry run opens the same way (the plane
        // must be bound for the census) and mutates nothing.
        return offline_set.withSet(p, a.target, .{ .with_plane = true, .verb = body.verb }, .{ .ctx = &body, .run = Body.run });
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests (portable): the grammar's lattice and the merge verb.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the grammar: the mode defaults by target kind; the lattice refuses --apply without --repair, --repair on a shard, --offline on a mountpoint, --online on a URI; a shard is admitted online (the admin lane's `admin_fsck`); scrub takes no repair flags" {
    var d: args.Refusal = undefined;
    const f = try args.parse(&.{ "squeezefs", "fsck", "sqmeta:///dev/m" }, &d);
    try testing.expect(f.fsck.offline() and !f.fsck.scrub_only);
    const m = try args.parse(&.{ "squeezefs", "fsck", "--json", "--scrub", "/mnt/x" }, &d);
    try testing.expect(!m.fsck.offline() and m.fsck.json and m.fsck.scrub);
    const sh = try args.parse(&.{ "squeezefs", "fsck", "--shards", "1/4", "sqmeta:///dev/m" }, &d);
    try testing.expectEqual(args.FsckShard{ .k = 1, .n = 4 }, sh.fsck.shards.?);
    const rep = try args.parse(&.{ "squeezefs", "fsck", "--repair", "--apply", "--quarantine-dir", "/q", "sqmeta:///dev/m" }, &d);
    try testing.expect(rep.fsck.repair and rep.fsck.apply and std.mem.eql(u8, rep.fsck.quarantine_dir.?, "/q"));
    const sc = try args.parse(&.{ "squeezefs", "scrub", "--throttle", "25", "sqmeta:///dev/m" }, &d);
    try testing.expect(sc.fsck.scrub_only and sc.fsck.throttle == 25);
    try testing.expectError(error.Refused, args.parse(&.{ "squeezefs", "fsck", "--apply", "sqmeta:///dev/m" }, &d));
    try testing.expect(d == .fsck_lattice);
    try testing.expectError(error.Refused, args.parse(&.{ "squeezefs", "fsck", "--repair", "--shards", "0/2", "sqmeta:///dev/m" }, &d));
    const osh = try args.parse(&.{ "squeezefs", "fsck", "--shards", "0/2", "/mnt/x" }, &d);
    try testing.expect(!osh.fsck.offline());
    try testing.expectEqual(args.FsckShard{ .k = 0, .n = 2 }, osh.fsck.shards.?);
    try testing.expectError(error.Refused, args.parse(&.{ "squeezefs", "fsck", "--offline", "/mnt/x" }, &d));
    try testing.expectError(error.Refused, args.parse(&.{ "squeezefs", "fsck", "--online", "sqmeta:///dev/m" }, &d));
    try testing.expectError(error.Refused, args.parse(&.{ "squeezefs", "fsck", "--shards", "2/2", "sqmeta:///dev/m" }, &d));
    try testing.expect(d == .bad_shards);
    try testing.expectError(error.Refused, args.parse(&.{ "squeezefs", "scrub", "--repair", "sqmeta:///dev/m" }, &d));
    try testing.expect(d == .unknown_flag);
    const mg = try args.parse(&.{ "squeezefs", "fsck", "merge-reports", "--json", "a.json", "b.json" }, &d);
    try testing.expectEqual(@as(usize, 2), mg.fsck_merge.files.len);
    try testing.expectError(error.Refused, args.parse(&.{ "squeezefs", "fsck", "merge-reports" }, &d));
}

test "merge-reports: two shard files merge into one report with the exit verdict; a non-report file refuses" {
    const gpa = testing.allocator;
    const io = testing.io;
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    const one = [_]finding.Finding{.{ .c9_unreferenced = .{ .ino = 70000, .blocks = 1 } }};
    const two = [_]finding.Finding{ .{ .c9_unreferenced = .{ .ino = 70000, .blocks = 1 } }, .{ .c10_nlink_too_high = .{ .ino = 9, .nlink = 3, .names = 1 } } };
    const ra: finding.Report = .{ .mode = .offline, .shard = .{ .k = 0, .n = 2 }, .findings = @constCast(&one), .counters = .{ .inodes_scanned = 4 } };
    const rb: finding.Report = .{ .mode = .offline, .shard = .{ .k = 1, .n = 2 }, .findings = @constCast(&two), .counters = .{ .inodes_scanned = 5 } };
    inline for (.{ ra, rb }, .{ "a.json", "b.json" }) |r, name| {
        var out: std.Io.Writer.Allocating = .init(gpa);
        defer out.deinit();
        try r.writeJson(&out.writer);
        try tmp.dir.writeFile(io, .{ .sub_path = name, .data = out.written() });
    }
    try tmp.dir.writeFile(io, .{ .sub_path = "junk.json", .data = "{\"schema\":1}" });
    const pa = try tmp.dir.realPathFileAlloc(io, "a.json", gpa);
    defer gpa.free(pa);
    const pb = try tmp.dir.realPathFileAlloc(io, "b.json", gpa);
    defer gpa.free(pb);
    const pj = try tmp.dir.realPathFileAlloc(io, "junk.json", gpa);
    defer gpa.free(pj);
    var stdout: std.Io.Writer.Allocating = .init(gpa);
    defer stdout.deinit();
    var stderr: std.Io.Writer.Allocating = .init(gpa);
    defer stderr.deinit();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const V = struct {
        fn write(w: *std.Io.Writer) std.Io.Writer.Error!void {
            try w.writeAll("t\n");
        }
    };
    var p: cli.Process = .{ .gpa = gpa, .io = io, .env = &env, .argv = &.{ "squeezefs", "fsck", "merge-reports", "--json", pa, pb }, .stdout = &stdout.writer, .stderr = &stderr.writer, .version = V.write };
    try testing.expectEqual(cli.exit_failed, cli.run(p));
    var merged = try finding.Report.parse(gpa, stdout.written());
    defer merged.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), merged.findings.len);
    try testing.expectEqual(@as(u64, 9), merged.counters.inodes_scanned);
    try testing.expect(merged.shard == null);
    p.argv = &.{ "squeezefs", "fsck", "merge-reports", pj };
    try testing.expectEqual(cli.exit_failed, cli.run(p));
    try testing.expect(std.mem.indexOf(u8, stderr.written(), "is not a schema-1 fsck report") != null);
}
