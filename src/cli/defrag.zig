//! `squeezefs defrag TARGET {--data|--pack|--volume ID|--meta|--fold|
//! --rebalance|--report-only} [--throttle PCT] [--json]` (charter §8;
//! `ops_jobs_lifecycle.md` §3.3). Linux only.
//!
//! `--report-only` on a `sqmeta://` set is the OFFLINE probe: the plane is
//! opened under the writer door, D1 is measured off every allocator's
//! snapshot, D2 and the pack face off the binding walk, D3 reads 0 by
//! construction (staging is mount-owned), the gauges are published and
//! the bounded JSON printed — nothing moves. The mutating arms on a set
//! write a durable QUEUED job record (`defrag_data` / `defrag_pack` /
//! `defrag_meta` / `rebalance`) in one tx that the next mount's fabric
//! adopts and runs through the ONE mover; `--fold` is live-mount only
//! (fold custody is the mount's) and a mountpoint target rides the admin
//! lane this build does not serve yet — both refused by name.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const cli = @import("cli.zig");
const args = @import("args.zig");
const offline = @import("offline.zig");
const clock = @import("../core/clock.zig");
const key = @import("../meta/key.zig");
const plane_mod = @import("../data/plane.zig");
const pack = @import("../data/pack.zig");
const layout = @import("../data/layout.zig");
const jobs = @import("../ops/jobs.zig");
const defrag = @import("../ops/defrag.zig");
const census = @import("../ops/lifecycle.zig").census;

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/defrag is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub fn run(p: cli.Process, d: args.Defrag) u8 {
    if (args.targetIsMountpoint(d.target)) {
        p.stderr.writeAll("squeezefs defrag: refused: a mountpoint target rides the daemon's admin lane, which this build does not serve yet; run the arm on the sqmeta:// set (the next mount's fabric runs the queued job)\n") catch {};
        return cli.exit_failed;
    }
    if (d.fold) {
        p.stderr.writeAll("squeezefs defrag --fold: refused: fold custody is the mount's — the D3 arm runs on a live mount only\n") catch {};
        return cli.exit_failed;
    }
    var body: Body = .{ .p = p, .d = d };
    return offline.withSet(p, d.target, .{ .with_plane = true, .verb = "defrag" }, .{ .ctx = &body, .run = Body.run });
}

const Body = struct {
    p: cli.Process,
    d: args.Defrag,

    fn run(ctx: *anyopaque, set: *offline.Set) u8 {
        const self: *Body = @ptrCast(@alignCast(ctx));
        const p = self.p;
        const plane = set.plane orelse {
            p.stderr.writeAll("squeezefs defrag: the data plane did not open\n") catch {};
            return cli.exit_failed;
        };
        if (self.d.report_only) return self.report(set, plane);
        return self.queue(set);
    }

    /// The offline measure + the bounded JSON / text.
    fn report(self: *Body, set: *offline.Set, plane: *plane_mod.DataPlane) u8 {
        const p = self.p;
        const gpa = p.gpa;
        var d1: std.ArrayList(defrag.report.D1Volume) = .empty;
        defer d1.deinit(gpa);
        for (plane.vols.rows[0..plane.vols.count]) |r| {
            const v = r.?;
            const m = defrag.metrics.measureD1(v.vol_tag, v.alloc.highestBlockIndex(), v.alloc, isFree);
            d1.append(gpa, .{ .vol_tag = v.vol_tag, .contiguity_permille = m.contiguityPermille(), .reclaimable_tail_permille = m.reclaimableTailPermille(), .total_free_blocks = m.total_free, .highest = m.highest }) catch return oom(p);
        }
        // D2 + the pack face off the binding walk.
        var refs: std.ArrayList(census.Ref) = .empty;
        defer refs.deinit(gpa);
        const Collect = struct {
            gpa: Allocator,
            refs: *std.ArrayList(census.Ref),
            fn visit(c: *@This(), ino: u64, bi: u32, m: layout.Mapping) plane_mod.Error!void {
                try c.refs.append(c.gpa, .{ .ino = ino, .block_index = bi, .mapping = m });
            }
        };
        var col: Collect = .{ .gpa = gpa, .refs = &refs };
        plane.forEachBinding(*Collect, &col, Collect.visit) catch |e| {
            p.stderr.print("squeezefs defrag: the layout walk failed ({s})\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        var pairs: u64 = 0;
        var local: u64 = 0;
        {
            var by_ino: std.AutoArrayHashMapUnmanaged(u64, std.ArrayList(defrag.metrics.Entry)) = .empty;
            defer {
                for (by_ino.values()) |*l| l.deinit(gpa);
                by_ino.deinit(gpa);
            }
            for (refs.items) |r| {
                if (!r.mapping.whole_block) continue;
                const gop = by_ino.getOrPut(gpa, r.ino) catch return oom(p);
                if (!gop.found_existing) gop.value_ptr.* = .empty;
                gop.value_ptr.append(gpa, .{ .block_index = r.block_index, .vol_tag = r.mapping.vol_tag, .block_idx = r.mapping.blockIdx(plane.chunk) }) catch return oom(p);
            }
            for (by_ino.values()) |*l| {
                std.mem.sort(defrag.metrics.Entry, l.items, {}, struct {
                    fn less(_: void, a: defrag.metrics.Entry, b: defrag.metrics.Entry) bool {
                        return a.block_index < b.block_index;
                    }
                }.less);
                const m = defrag.metrics.measureD2(l.items);
                pairs += m.pairs;
                local += m.local;
            }
        }
        var scope: std.ArrayList(u64) = .empty;
        defer scope.deinit(gpa);
        for (plane.vols.rows[0..plane.vols.count]) |r| scope.append(gpa, r.?.vol_tag) catch return oom(p);
        const policy = pack.Policy.fromEnv(set.env, plane.chunk);
        var pplan = defrag.plan.planPackCompaction(gpa, .{ .refs = refs.items, .scope = scope.items, .chunk = plane.chunk, .max_slot = policy.max_slot, .open = &.{}, .open_room_bytes = 0 }) catch return oom(p);
        defer pplan.deinit(gpa);
        var rows: std.ArrayList(defrag.report.PackRow) = .empty;
        defer rows.deinit(gpa);
        var blocks: std.ArrayList(defrag.metrics.PackBlock) = .empty;
        defer blocks.deinit(gpa);
        for (pplan.victims) |v| {
            const pb: defrag.metrics.PackBlock = .{ .vol_tag = v.vol_tag, .block_idx = v.block_idx, .live_bytes = v.live_bytes, .windows = @intCast(v.windows.len), .referencers = @intCast(v.refs.len) };
            blocks.append(gpa, pb) catch return oom(p);
            rows.append(gpa, .{ .vol_tag = v.vol_tag, .block_idx = v.block_idx, .occupancy_permille = pb.occupancyPermille(plane.chunk), .live_bytes = v.live_bytes, .referencers = @intCast(v.refs.len), .victim = true }) catch return oom(p);
        }
        const rep: defrag.report.Report = .{
            .d1 = d1.items,
            .d2_locality_permille = defrag.metrics.decodeRatio(defrag.metrics.encodeRatio(local, pairs)).?,
            .d2_pairs = pairs,
            .d3_pressure_bytes = 0,
            .d4 = &.{},
            .pack = defrag.metrics.summarizePacks(blocks.items, plane.chunk, policy.max_slot),
            .pack_rows = rows.items,
        };
        rep.publish();
        const buf = gpa.alloc(u8, 60 << 10) catch return oom(p);
        defer gpa.free(buf);
        const n = defrag.report.writeBounded(rep, buf) catch return oom(p);
        if (self.d.json) {
            p.stdout.writeAll(buf[0..n]) catch {};
            p.stdout.writeAll("\n") catch {};
        } else {
            for (rep.d1) |v| p.stdout.print("D1 vol-{x:0>16}: contiguity {d} ‰, reclaimable tail {d} ‰, {d} free of {d} allocated-space blocks\n", .{ v.vol_tag, v.contiguity_permille, v.reclaimable_tail_permille, v.total_free_blocks, v.highest }) catch {};
            p.stdout.print("D2 locality {d} ‰ over {d} pairs; D3 0 B (offline); pack blocks below half: {d} ({d} B reclaimable)\n", .{ rep.d2_locality_permille, rep.d2_pairs, rep.pack.below_half, rep.pack.reclaimable_bytes }) catch {};
        }
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }

    /// The mutating arms: one durable queued record per arm, in ONE tx.
    fn queue(self: *Body, set: *offline.Set) u8 {
        const p = self.p;
        var ids: [4]u64 = undefined;
        var n: usize = 0;
        const t = set.be.begin(@src()) catch return oom(p);
        const now = clock.realtimeCoarseS();
        const types = [_]struct { on: bool, t: jobs.JobType }{ .{ .on = self.d.data, .t = .defrag_data }, .{ .on = self.d.pack, .t = .defrag_pack }, .{ .on = self.d.meta, .t = .defrag_meta }, .{ .on = self.d.rebalance, .t = .rebalance } };
        for (types) |row| {
            if (!row.on) continue;
            var b: [8]u8 = undefined;
            set.io.randomSecure(&b) catch {
                set.be.abandon(t);
                p.stderr.writeAll("squeezefs defrag: the random source failed\n") catch {};
                return cli.exit_failed;
            };
            var id = std.mem.readInt(u64, &b, .little);
            if (id == 0) id = 1;
            const rec: jobs.JobRecord = .{ .job_id = id, .job_type = row.t, .state = .queued, .throttle_pct = self.d.throttle, .created_realtime_s = now, .created_by = .{ .node_token = 0, .mount_slot = 0 }, .created_pid = 0, .arg0 = if (row.t == .rebalance or row.t == .defrag_meta) 0 else (self.d.volume orelse 0), .updated_realtime_s = now };
            const k = jobs.records.jobKey(id);
            const bytes = rec.encode();
            t.stagePut(key.TREE_CONTROL, k.slice(), &bytes) catch {
                set.be.abandon(t);
                return oom(p);
            };
            ids[n] = id;
            n += 1;
        }
        set.be.commit(t) catch |e| {
            p.stderr.print("squeezefs defrag: the commit failed: {t}\n", .{e}) catch {};
            return cli.exit_failed;
        };
        set.be.fsync() catch |e| {
            p.stderr.print("squeezefs defrag: the durability barrier failed: {t}\n", .{e}) catch {};
            return cli.exit_failed;
        };
        p.stdout.print("queued {d} job(s) at throttle {d} %:", .{ n, self.d.throttle }) catch {};
        for (ids[0..n]) |id| p.stdout.print(" job-{x:0>16}", .{id}) catch {};
        p.stdout.writeAll(" — the next mount's fabric adopts and runs them\n") catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

fn isFree(alloc: *@import("../data/block_alloc.zig").BlockAllocator, idx: u64) bool {
    return !alloc.bitIsSet(idx);
}

fn oom(p: cli.Process) u8 {
    p.stderr.writeAll("squeezefs defrag: out of memory\n") catch {};
    return cli.exit_failed;
}
