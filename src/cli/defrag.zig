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
//! adopts and runs through the ONE mover. A MOUNTPOINT target SUBMITS the
//! arms through the daemon's admin lane (`admin_lane.zig`): the fabric
//! runs them at once; the daemon's bodies are D1/D2 (`--data`), the
//! compactor (`--pack`) and the rebalance; `--meta` / `--fold` submit and
//! fail typed (`body`) until their plane seams are bound (stated in the
//! rung-8 evidence) — never a silent queue. `--report-only` is the
//! offline measure (the set's gauges ride `.stats` on a live mount).
//! `--rekey --encrypt-key <path|->` (charter §6.5; `ops/defrag/rekey.zig`):
//! the NEW key's material read through the ONE reader in THIS process, a
//! fresh salt minted, the key id derived, the key-dir file installed
//! (0600, atomic; the daemon resolves the next key from there — material
//! never crosses the lane), then `admin_rekey` with the PUBLIC `{salt,
//! key_id}`: the daemon opens the window durably, swaps its wrap and
//! submits the sweep.

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
        if (d.report_only) {
            p.stderr.writeAll("squeezefs defrag --report-only: refused on a mountpoint: the live gauges ride the mount's .stats (`defrag_*`, `pack_*`); the measured report is the offline probe on the sqmeta:// set\n") catch {};
            return cli.exit_failed;
        }
        var live: Live = .{ .p = p, .d = d };
        return admin_lane.withLane(p, "defrag", d.target, .{ .ctx = &live, .run = Live.run });
    }
    if (d.fold) {
        p.stderr.writeAll("squeezefs defrag --fold: refused: fold custody is the mount's — the D3 arm runs on a live mount only\n") catch {};
        return cli.exit_failed;
    }
    var body: Body = .{ .p = p, .d = d };
    return offline.withSet(p, d.target, .{ .with_plane = true, .verb = "defrag" }, .{ .ctx = &body, .run = Body.run });
}

const admin_lane = @import("admin_lane.zig");
const errors = @import("../core/errors.zig");
const keys = @import("../ops/keys.zig");
const transform = @import("../data/transform.zig");
const uring_fs = @import("../io/uring_fs.zig");
const derive = @import("../core/derive.zig");

/// The live arms: one submission per arm through the lane; the fabric's
/// verdict decides the exit.
const Live = struct {
    p: cli.Process,
    d: args.Defrag,

    fn run(ctx: *anyopaque, lane: *admin_lane.Lane) u8 {
        const self: *Live = @ptrCast(@alignCast(ctx));
        const p = self.p;
        const d = self.d;
        if (d.rekey) return self.rekey(lane);
        const types = [_]struct { on: bool, t: jobs.JobType }{ .{ .on = d.data, .t = .defrag_data }, .{ .on = d.pack, .t = .defrag_pack }, .{ .on = d.meta, .t = .defrag_meta }, .{ .on = d.fold, .t = .defrag_fold }, .{ .on = d.rebalance, .t = .rebalance } };
        var n: usize = 0;
        p.stdout.print("submitted at throttle {d} %:", .{d.throttle}) catch {};
        for (types) |row| {
            if (!row.on) continue;
            const arg0: u64 = if (row.t == .rebalance or row.t == .defrag_meta or row.t == .defrag_fold) 0 else (d.volume orelse 0);
            const sub = lane.admin().submit(.{ .job_type = row.t, .throttle_pct = d.throttle, .arg0 = arg0 }) catch |e| {
                p.stdout.writeAll("\n") catch {};
                p.stderr.print("squeezefs defrag: the submission of {s} failed ({s}, {t})\n", .{ row.t.name(), @errorName(e), errors.toErrno(e) }) catch {};
                return cli.exit_failed;
            };
            if (sub.verdict != .ok) {
                p.stdout.writeAll("\n") catch {};
                p.stderr.print("squeezefs defrag: {s} refused: {s}\n", .{ row.t.name(), admin_lane.verdictText(sub.verdict) }) catch {};
                p.stderr.flush() catch {};
                return cli.exit_failed;
            }
            p.stdout.print(" {s}=job-{x:0>16}", .{ row.t.name(), sub.job_id }) catch {};
            n += 1;
        }
        p.stdout.print(" ({d} job(s); `squeezefs job list {s}` follows them)\n", .{ n, d.target }) catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }

    /// The rotation: material → salt → id → the key-dir file → the lane.
    fn rekey(self: *Live, lane: *admin_lane.Lane) u8 {
        const p = self.p;
        const d = self.d;
        var stash = keys.linux_verbs.readStash(p.gpa, d.encrypt_key.?) catch |e| switch (e) {
            error.OutOfMemory => return oom(p),
            else => |ke| {
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                transform.writeKeyFileRefusal(&w, ke, d.encrypt_key.?) catch {};
                p.stderr.print("squeezefs defrag --rekey: {s}\n", .{w.buffered()}) catch {};
                return cli.exit_failed;
            },
        };
        defer stash.deinit();
        var salt: [transform.KDF_SALT_LEN]u8 = undefined;
        p.io.randomSecure(&salt) catch {
            p.stderr.writeAll("squeezefs defrag --rekey: the random source failed (the salt is never a weaker fallback)\n") catch {};
            return cli.exit_failed;
        };
        var derived = transform.deriveVolumeKey(stash.material(), &salt);
        defer derived.zeroize();
        const key_id = derived.key_id;
        // The key-dir file FIRST: the daemon resolves the next key there.
        const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.facts())) catch |e| {
            p.stderr.print("squeezefs defrag --rekey: io pool init failed ({s})\n", .{@errorName(e)}) catch {};
            return cli.exit_failed;
        };
        defer pool.deinit();
        var pb: [128]u8 = undefined;
        const dest = keys.keyPath(&pb, key_id);
        switch (keys.linux_verbs.installMaterial(pool, key_id, stash.material(), false) catch |e| {
            p.stderr.print("squeezefs defrag --rekey: cannot install the new key at {s}: {s} (root with {s} writable is required; a different file under that id is never replaced)\n", .{ dest, @errorName(e), keys.KEY_DIR_PARENT }) catch {};
            return cli.exit_failed;
        }) {
            .installed => p.stdout.print("installed the new key at {s} (id {x})\n", .{ dest, &key_id }) catch {},
            .identical => p.stdout.print("{s} already holds the new key (id {x})\n", .{ dest, &key_id }) catch {},
        }
        stash.zeroize();
        const rep = lane.admin().rekey(salt, key_id, d.throttle) catch |e| {
            p.stderr.print("squeezefs defrag --rekey: the lane call failed ({s}, {t})\n", .{ @errorName(e), errors.toErrno(e) }) catch {};
            return cli.exit_failed;
        };
        if (rep.get() != .ok) {
            p.stderr.print("squeezefs defrag --rekey: refused: {s}{s}\n", .{ admin_lane.verdictText(rep.get()), switch (rep.why()) {
                .none => "",
                .plaintext => " — the set is plaintext (encryption none): there is no key to rotate",
                .window_open => " — a rotation window is already open with ANOTHER next key; finish it (`job list`) before starting a new one",
                .key_unresolved => " — the daemon could not resolve the new key from its key dir (is the daemon on this host, with the same /etc/squeezefs/keys?)",
                .same_key => " — the new key derives the set's CURRENT key id: that is not a rotation",
                .plane => " — the window opened but the daemon's crypto state did not swap; retry",
            } }) catch {};
            p.stderr.flush() catch {};
            return cli.exit_failed;
        }
        p.stdout.print("rotation window open (config generation {d}): job-{x:0>16} re-encrypts every block onto key id {x} at throttle {d} %; the primary flips LAST, after the last block's publish is durable — `squeezefs job list {s}` follows it; every OTHER host of this set needs {s} installed before it mounts\n", .{ rep.config_generation, rep.job_id, &key_id, d.throttle, d.target, dest }) catch {};
        p.stdout.flush() catch {};
        return cli.exit_ok;
    }
};

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
        for (plane.vols.rows[0..plane.vols.n()]) |r| {
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
        for (plane.vols.rows[0..plane.vols.n()]) |r| scope.append(gpa, r.?.vol_tag) catch return oom(p);
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
