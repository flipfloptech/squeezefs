//! The engine's pass order and the two harnesses (`ops_fsck.md` §1.2, §7):
//! `run` = pass 1 (the census + the dentry pass, the allocators' scan
//! latches armed) → every class's nominations in the normative order → the
//! settle → the ladder's re-check → C7 over a FRESH census post-settle →
//! the sorted + deduped report → the optional repair (dry-run default) →
//! `publish`. Portable over a `classes.Host`.
//!
//! The Linux half binds a host over the opened set: OFFLINE = the D0-guarded
//! writer-door open the CLI's `offline.withSet` performs (the flock + the
//! claim; nothing in flight by definition, so every record is prior-era and
//! every slot is this process's), the `MetaSet` as the store, the plane's
//! allocators, pack ledger, staging ring and transform, the appender pages
//! off the device for C14/C15; ONLINE = the same binding over the LIVE set
//! with the 4a manager, the floors and the settle armed — the in-process
//! face the job fabric's admin dispatcher calls when it lands (a typed
//! `Submit` seam, never a second wire inside fsck).

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;

const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const key = @import("../../meta/key.zig");
const inos = @import("../../meta/inos.zig");
const fsck = @import("../fsck.zig");
const finding = @import("finding.zig");
const census = @import("census.zig");
const classes = @import("classes.zig");
const repair = @import("repair.zig");
const scrub = @import("scrub.zig");

pub const Error = census.Error;
pub const Host = classes.Host;

pub const Options = struct {
    scrub: bool = false,
    /// `squeezefs scrub`: C7 alone.
    scrub_only: bool = false,
    throttle: scrub.Throttle = .none,
    /// Null = detection only.
    repair: ?repair.Options = null,
};

/// `SQUEEZEFS_FSCK_SETTLE_MS` as nanoseconds (the §5.6 step-2 window).
pub fn settleNsFromEnv(env: *const std.process.Environ.Map) u64 {
    return knobs.intKnob("SQUEEZEFS_FSCK_SETTLE_MS", u64, env) * std.time.ns_per_ms;
}

/// The engine: detect → settle → re-check → scrub → report → repair →
/// publish. `verifier` is C7's (and C2-lost's) block reader; null = no
/// scrub possible (the report says so through its counters).
pub fn run(host: *const Host, opts: Options, verifier: ?*scrub.Verifier) Error!finding.Report {
    const t0 = clock.monotonicNs();
    var pass = try classes.Pass.begin(host);
    defer pass.deinit();
    defer pass.end();
    if (!opts.scrub_only) try pass.nominate();
    pass.settle();
    try pass.recheck();
    if ((opts.scrub or opts.scrub_only) and verifier != null) {
        // A FRESH census post-settle: the mappings the scrub reads are the
        // ones that stood after the window.
        var tags_buf: [64]u64 = undefined;
        const tags = host.knownTags(&tags_buf);
        var fresh = try census.run(host.gpa, host.store, .{ .scope = host.scope, .shard = host.shard, .mem_budget = host.mem_budget, .geo = host.geo, .known_vol_tags = tags });
        defer fresh.deinit();
        try scrub.run(&pass, &fresh, verifier.?, opts.throttle);
    }
    var report = try pass.report(host.gpa);
    errdefer report.deinit(host.gpa);
    if (opts.repair) |ropts| {
        var r = ropts;
        // The scrub's verifier is C2-lost's content arm too.
        if (r.verifier == null) r.verifier = verifier;
        var eng = repair.Engine.init(&pass, r);
        report.repair = try eng.run(report.findings);
    }
    report.counters.scan_ms = (clock.monotonicNs() -| t0) / std.time.ns_per_ms;
    report.publish();
    return report;
}

/// The online submission seam the job fabric binds: the daemon's admin
/// dispatcher runs the engine over its live host and answers the report.
pub const Submit = struct {
    ctx: *anyopaque,
    run: *const fn (*anyopaque, Allocator, Options) Error!finding.Report,
};

// ---------------------------------------------------------------------------
// The Linux harness.
// ---------------------------------------------------------------------------

pub const linux = if (builtin.os.tag == .linux) struct {
    const linux_os = std.os.linux;
    const meta_set = @import("../../meta/meta_set.zig");
    const plane_mod = @import("../../data/plane.zig");
    const handler = @import("../../fuse/handler.zig");
    const staging = @import("../../data/staging.zig");
    const transform = @import("../../data/transform.zig");
    const layout = @import("../../data/layout.zig");
    const sym = @import("../../meta/sym.zig");
    const page = sym.appender_page;
    const dir = sym.appender_dir;
    const cr = sym.control_records;
    const journal = @import("../../meta/journal.zig");
    const tree = @import("../../meta/tree.zig");
    const record = @import("../../meta/record.zig");
    const nvme_dev = @import("../../io/nvme_dev.zig");
    const uring_fs = @import("../../io/uring_fs.zig");
    const fabric = @import("../jobs/fabric.zig");
    const job_seams = @import("../jobs/seams.zig");

    pub const BackendFs = handler.BackendFs;

    /// Everything the engine needs over an opened set, owned here.
    pub const Bound = struct {
        gpa: Allocator,
        be: *meta_set.MetaSet,
        plane: ?*plane_mod.DataPlane,
        scope: census.SlotScope,
        vols: []classes.Volume,
        xforms: []scrub.VolumeXform,
        xform_bind: ?TransformBind,
        xform_scratch: []u8,
        verifier: ?scrub.Verifier,
        staging_face: ?StagingBind,
        forest: ForestBind,
        host: Host,

        pub fn deinit(self: *Bound) void {
            if (self.verifier) |*v| v.deinit();
            self.gpa.free(self.xform_scratch);
            self.gpa.free(self.xforms);
            self.gpa.free(self.vols);
            self.scope.deinit();
            self.* = undefined;
        }
    };

    pub const BindOptions = struct {
        env: *const std.process.Environ.Map,
        mode: finding.Mode,
        mem_budget: u64,
        shard: ?finding.Shard = null,
        /// The live filesystem (its 4a manager): online only.
        fs: ?*BackendFs = null,
        /// The set's first cache path (the staging root the marker lives in).
        staging_root: ?[]const u8 = null,
        cancel: ?*const std.atomic.Value(bool) = null,
    };

    /// Bind the host over the set. The scope: every slot this open holds
    /// (the writer door is exclusive), the watermarks = each slot's cursor
    /// NOW (the census is bounded at its start), the era floors = the same
    /// snapshot — CONSERVATIVE for an online host built after the open
    /// (more records read current-era ⇒ fewer verdicts, never a false one);
    /// offline nothing is in flight, so the posture is the probe's.
    pub fn bind(gpa: Allocator, io: Io, be: *meta_set.MetaSet, plane: ?*plane_mod.DataPlane, opts: BindOptions) !*Bound {
        const b = try gpa.create(Bound);
        errdefer gpa.destroy(b);
        b.* = .{ .gpa = gpa, .be = be, .plane = plane, .scope = census.SlotScope.init(gpa), .vols = &.{}, .xforms = &.{}, .xform_bind = null, .xform_scratch = &.{}, .verifier = null, .staging_face = null, .forest = .{ .be = be, .gpa = gpa }, .host = undefined };
        errdefer b.scope.deinit();
        b.scope.posture = if (opts.mode == .offline) .probe else .manager;
        // Every routing slot is this open's (the writer door is exclusive;
        // a joined posture is the fabric's binding, not this harness's).
        var s: u32 = 0;
        while (s < inos.ROUTING_WIDTH) : (s += 1) try b.scope.leased.put(gpa, @intCast(s), {});
        try refreshScope(b);
        // The data volumes.
        var vols: std.ArrayList(classes.Volume) = .empty;
        errdefer vols.deinit(gpa);
        var xforms: std.ArrayList(scrub.VolumeXform) = .empty;
        errdefer xforms.deinit(gpa);
        if (plane) |pl| {
            if (pl.xform != null) {
                b.xform_scratch = try gpa.alloc(u8, @intCast(transform.scratchLen(@intCast(pl.block_size))));
                b.xform_bind = .{ .plane = pl, .scratch = b.xform_scratch };
            }
            for (pl.vols.rows[0..pl.vols.n()]) |r| {
                const row = r.?;
                // Index 0 is the data superblock chunk the plane reserves forever.
                try vols.append(gpa, .{ .vol_tag = row.vol_tag, .alloc = row.alloc, .bitmap = null, .passthrough = row.passthrough, .reserved_blocks = plane_mod.DataPlane.RESERVED_BLOCKS });
                const cfg: transform.VolumeTransform = if (pl.xform) |*x| x.t.cfg else .{ .block_size = @intCast(pl.block_size) };
                try xforms.append(gpa, .{ .vol_tag = row.vol_tag, .cfg = cfg, .face = if (b.xform_bind) |*xb| xb.face() else null });
            }
        }
        b.vols = try vols.toOwnedSlice(gpa);
        b.xforms = try xforms.toOwnedSlice(gpa);
        if (plane) |pl| {
            b.verifier = try scrub.Verifier.init(gpa, .{ .ctx = pl, .read = &readBlock }, pl.chunk, b.xforms);
            if (pl.stage != null) if (opts.staging_root) |root| {
                b.staging_face = .{ .plane = pl, .root = root };
            };
        }
        b.host = .{
            .gpa = gpa,
            .io = io,
            .store = blk: {
                var st = census.Store.from(meta_set.MetaSet, be);
                st.merged_kinds = true;
                break :blk st;
            },
            .geo = if (plane) |pl| pl.geo else .{},
            .hash_seed = be.hashSeed(),
            .volumes = b.vols,
            .scope = &b.scope,
            .mode = opts.mode,
            .dlm = if (opts.fs) |fs| &fs.locks else null,
            .pack = if (plane) |pl| .{ .ctx = pl, .isOpenPack = &isOpenPack } else null,
            .staging = if (b.staging_face) |*sf| sf.face() else null,
            .forest = b.forest.face(),
            .reader = if (plane) |pl| .{ .ctx = pl, .read = &readBlock } else null,
            .mem_budget = opts.mem_budget,
            .shard = opts.shard,
            .settle_ns = settleNsFromEnv(opts.env),
            .cancel = opts.cancel,
        };
        return b;
    }

    pub fn destroy(b: *Bound) void {
        const gpa = b.gpa;
        b.deinit();
        gpa.destroy(b);
    }

    /// The start watermarks AND the era floors = every slot's cursor NOW
    /// (the native slot's mint word; a rotor slot's cursor cell): a record
    /// at/past its slot's watermark is the next census's. Taken at `bind`
    /// and AGAIN at every online pass — a mount-time binding's words are
    /// stale by the first run (every file minted since read "the next
    /// census's" and the derived walk saw none of their blocks: C2/C8
    /// false positives on a healthy mount, found by the rung-8 live
    /// `fsck --online`).
    pub fn refreshScope(b: *Bound) Allocator.Error!void {
        const gpa = b.gpa;
        var s: u32 = 0;
        while (s < inos.ROUTING_WIDTH) : (s += 1) {
            const slot: u16 = @intCast(s);
            const v = b.be.volumeOfSlot(slot);
            const native = v.stamp.native_slot;
            const cursor: ?u64 = if (native != null and native.? == slot) v.ino.snapshot() else v.stamp.cursorSnapshot(slot);
            if (cursor) |c| {
                try b.scope.watermarks.put(gpa, slot, c);
                try b.scope.era_floor.put(gpa, slot, c);
            }
        }
    }

    /// The device read: the data volume's `readInto` (O_DIRECT grain).
    fn readBlock(ctx: *anyopaque, vol_tag: u64, device_off: u64, dest: []u8) Error!void {
        const pl: *plane_mod.DataPlane = @ptrCast(@alignCast(ctx));
        const v = pl.vols.byTag(vol_tag) orelse return error.Io;
        const dev = pl.devs[v.ordinal] orelse return error.Io;
        dev.readInto(device_off, dest) catch return error.Io;
        fsck.stats.fsck_scrub_read_bytes.add(dest.len);
    }

    fn isOpenPack(ctx: ?*anyopaque, vol_tag: u64, block_idx: u64) bool {
        const pl: *plane_mod.DataPlane = @ptrCast(@alignCast(ctx.?));
        const p = &(pl.packer orelse return false);
        if (p.openPack(.{ .vol_tag = vol_tag, .scope = pl.scope })) |op| return op.base / pl.chunk == block_idx;
        return false;
    }

    /// The plane's codec pool behind the scrub's face (a codec per decode,
    /// the scrub's own scratch).
    const TransformBind = struct {
        plane: *plane_mod.DataPlane,
        scratch: []u8,

        pub fn face(self: *TransformBind) scrub.TransformFace {
            return .{ .ctx = self, .verify = &verifyFn };
        }

        fn verifyFn(ctx: *anyopaque, image: []u8) scrub.Verdict {
            const self: *TransformBind = @ptrCast(@alignCast(ctx));
            _ = self.plane.decodeStored(image, self.scratch) catch return .failed;
            return if (self.plane.xform.?.t.cfg.encryption != .none) .aead else .frame;
        }
    };

    /// The staging ring (C4/C5) over the plane's segment + the marker file.
    const StagingBind = struct {
        plane: *plane_mod.DataPlane,
        root: []const u8,

        pub fn face(self: *StagingBind) classes.StagingFace {
            return .{ .ctx = self, .vtable = &vtable };
        }

        fn residentsFn(ctx: *anyopaque, gpa: Allocator) Error![]staging.Resident {
            const self: *StagingBind = @ptrCast(@alignCast(ctx));
            const seg = &self.plane.stage.?.seg;
            const out = try gpa.alloc(staging.Resident, seg.live() + 1);
            const got = seg.residents(out);
            return gpa.realloc(out, got.len) catch out[0..got.len];
        }

        fn markerFn(ctx: *anyopaque) ?finding.StagingVerdict {
            const self: *StagingBind = @ptrCast(@alignCast(ctx));
            const pl = self.plane;
            const ours: staging.Marker = .{ .set_uuid = pl.backend.setUuid(), .set_epoch = pl.backend.setEpoch(), .node_token = 0, .mount_slot = 0 };
            const found = staging.Ring.readMarker(pl.backend.pool, self.root) catch return .unreadable;
            return switch (staging.bindVerdict(found, ours, false)) {
                .absent, .match, .upgrade => null,
                .dead => .dead,
                .foreign_scope => .foreign_scope,
            };
        }

        fn imageFn(ctx: *anyopaque, gpa: Allocator, k: staging.StagingKey) Error!?[]u8 {
            const self: *StagingBind = @ptrCast(@alignCast(ctx));
            const lease = self.plane.stage.?.seg.get(k) orelse return null;
            defer lease.release();
            return try gpa.dupe(u8, lease.bytes());
        }

        fn removeFn(ctx: *anyopaque, k: staging.StagingKey, generation: u64) bool {
            const self: *StagingBind = @ptrCast(@alignCast(ctx));
            return self.plane.stage.?.seg.remove(k, generation);
        }

        fn quarantineDirFn(ctx: *anyopaque, q: *classes.Quarantine) Error!void {
            const self: *StagingBind = @ptrCast(@alignCast(ctx));
            // The segment image is the dir's whole content: copied, then
            // every record retired (the marker is restamped by the next open).
            const seg = &self.plane.stage.?.seg;
            try q.putRecord(.c5, "staging-segment", seg.image);
            const gpa = self.plane.gpa;
            const out = try gpa.alloc(staging.Resident, seg.live() + 1);
            defer gpa.free(out);
            for (seg.residents(out)) |r| _ = seg.remove(r.key, r.generation);
        }

        const vtable: classes.StagingFace.VTable = .{ .residents = &residentsFn, .marker = &markerFn, .image = &imageFn, .remove = &removeFn, .quarantineDir = &quarantineDirFn };
    };

    /// C14/C15 off the devices: every volume's appender pages (appender 0
    /// off the fixed ring's four slots, the rest off the directory chain),
    /// every volume's tree-0 `slot_state` rows, volume 0's death ledger.
    /// C13 on an unmounted set: every grant belongs to appender 0 (which
    /// holds none — the manager allocates from the heap) or to a dead
    /// appender the open's recovery wave returned, so the live-appender
    /// arm has nothing to census here; the repairs refuse (no ring to free
    /// through, no manager role to recover under).
    const ForestBind = struct {
        be: *meta_set.MetaSet,
        gpa: Allocator,

        pub fn face(self: *ForestBind) classes.ForestFace {
            return .{ .ctx = self, .vtable = &vtable };
        }

        const Reader = struct {
            be: *meta_set.KvBackend,
            fn read(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
                const self: *Reader = @ptrCast(@alignCast(ctx));
                const img = self.be.dev.readRange(self.be.gpa, offset, page.PAGE_LEN) catch return error.Io;
                defer self.be.gpa.free(img);
                @memcpy(out, img[0..page.PAGE_LEN]);
            }
        };

        fn custodyFn(ctx: *anyopaque, arena: Allocator) Error!classes.ForestCensus {
            const self: *ForestBind = @ptrCast(@alignCast(ctx));
            var pages: std.ArrayList(classes.PageView) = .empty;
            var tree0: std.ArrayList(classes.Tree0Row) = .empty;
            var deaths: std.ArrayList(cr.StableIdentity) = .empty;
            for (self.be.volumes, 0..) |be, vi| {
                const vol: u16 = @intCast(vi);
                // The flat class (`--single-writer`): no SYMMETRIC_PLANE bit,
                // no appender directory, appender 0's page slots never
                // written (the journal ring begins at the extent's first
                // page) and no tree-0 slot rows — C13–C15 have no domain on
                // this volume. Reading the slots as pages would judge the
                // ring's bytes as a torn page census (`Corrupt`).
                if (!be.sb.symmetricPlaneStamped()) continue;
                var rd: Reader = .{ .be = be };
                const reader: dir.PageReader = .{ .ctx = &rd, .read = &Reader.read };
                // Appender 0.
                {
                    const offs = page.appender0PageOffsets(be.sb.journal.start);
                    var bufs: [page.PAGE_SLOTS][page.PAGE_LEN]u8 = undefined;
                    var imgs: [page.PAGE_SLOTS]?[]const u8 = .{ null, null, null, null };
                    for (offs, 0..) |off, i| {
                        Reader.read(&rd, off, &bufs[i]) catch return error.Io;
                        imgs[i] = &bufs[i];
                    }
                    if (page.newestValid(imgs) catch return error.Corrupt) |p| try pages.append(arena, try view(arena, vol, p));
                }
                var links: std.ArrayList(dir.Link) = .empty;
                defer links.deinit(self.gpa);
                dir.walkChain(self.gpa, reader, .{ .start = be.sb.appender_dir.start, .len = be.sb.appender_dir.len }, be.sb.node_size, .{ .start = be.sb.heap.start, .len = be.sb.heap.len }, be.sb.totalExtents(), &links) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                    error.Corrupt => error.Corrupt,
                };
                for (links.items) |l| {
                    var pair: u32 = 0;
                    while (pair < l.header.pairs) : (pair += 1) {
                        const imgs = dir.readPair(reader, l, pair) catch return error.Io;
                        if (imgs.foreign) return error.Corrupt;
                        const p = imgs.newest() orelse continue;
                        try pages.append(arena, try view(arena, vol, p));
                    }
                }
                // Tree 0's slot_state rows.
                const r = cr.rangeOf(.slot_state);
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(self.gpa, &kvs);
                be.range(self.gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs) catch return error.Io;
                for (kvs.items) |kv| {
                    const slot = cr.slotOfKey(kv.key) orelse continue;
                    const st = cr.SlotStateWire.decode(kv.value) catch return error.Corrupt;
                    try tree0.append(arena, .{ .vol = vol, .slot = slot, .state = st });
                }
            }
            // Volume 0's death ledger.
            {
                const r = cr.rangeOf(.dead_member);
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(self.gpa, &kvs);
                self.be.range(self.gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs) catch return error.Io;
                for (kvs.items) |kv| {
                    const id = cr.identityOfKey(kv.key) orelse continue;
                    try deaths.append(arena, .{ .node_token = id.node_token, .mount_slot = id.mount_slot });
                }
            }
            return .{ .pages = pages.items, .tree0 = tree0.items, .deaths = deaths.items };
        }

        fn view(arena: Allocator, vol: u16, p: page.Page) Allocator.Error!classes.PageView {
            return .{
                .vol = vol,
                .appender_id = p.appender_id,
                .state = p.state,
                .node_token = p.identity.node_token,
                .mount_slot = p.identity.mount_slot,
                .is_manager = p.is_manager,
                .window_entries = p.head_hint -| p.ledger_tail_seq,
                .entries = try arena.dupe(page.SlotEntry, p.slotEntries()),
            };
        }

        fn orphanImagesFn(_: *anyopaque, _: Allocator, _: *std.ArrayList(classes.Finding)) Error!void {}

        fn returnOrphanFn(_: *anyopaque, _: u16, _: u32, _: u64) Error!bool {
            return false;
        }

        fn recoverDeadFn(_: *anyopaque, _: u16, _: u32) Error!bool {
            return false;
        }

        const vtable: classes.ForestFace.VTable = .{ .custody = &custodyFn, .orphanImages = &orphanImagesFn, .returnOrphan = &returnOrphanFn, .recoverDead = &recoverDeadFn };
    };

    /// The fabric's `JobType.fsck` body (`ops_fsck.md` §7; 0002 row 397): a
    /// serving daemon registers it over its LIVE binding; one pass = one
    /// online run with the job's cancel/pause intent as the engine's stop
    /// latch and the job's live duty cycle as the scrub's pacer; findings
    /// > 0 leave the job COMPLETED (detection succeeded; the findings are
    /// its output — the CLI owns exit codes) and log at error through the
    /// report's tripwire. The last report is the admin dispatcher's read.
    pub const JobBody = struct {
        gpa: Allocator,
        /// The bind's inputs: the binding is REBUILT when the plane's
        /// volume population changed since (an online `volume add-data`;
        /// a binding frozen at mount judged the new volume's blocks "unknown
        /// volume" — C2 lost false positives after a live add, rung 8).
        be: *meta_set.MetaSet,
        plane: *plane_mod.DataPlane,
        env: *const std.process.Environ.Map,
        mem_budget: u64,
        fs: ?*BackendFs,
        staging_root: ?[]const u8,
        bound: *Bound,
        last: ?finding.Report = null,

        pub fn deinit(self: *JobBody) void {
            if (self.last) |*r| r.deinit(self.gpa);
            destroy(self.bound);
            self.* = undefined;
        }

        pub fn body(self: *JobBody) fabric.Body {
            return .{ .ctx = self, .pass = &passFn };
        }

        fn bindOptions(self: *const JobBody) BindOptions {
            return .{ .env = self.env, .mode = .online, .mem_budget = self.mem_budget, .fs = self.fs, .staging_root = self.staging_root };
        }

        fn passFn(ctx: *anyopaque, io: Io, f: *fabric.Fabric, job: *fabric.JobCtl) fabric.PassError!fabric.PassOutcome {
            const self: *JobBody = @ptrCast(@alignCast(ctx));
            if (self.bound.vols.len != self.plane.vols.n()) {
                const fresh = bind(self.gpa, io, self.be, self.plane, self.bindOptions()) catch return error.OutOfMemory;
                destroy(self.bound);
                self.bound = fresh;
            }
            self.bound.host.io = io;
            self.bound.host.cancel = &job.cancel_intent;
            self.bound.host.pause = &job.pause_intent;
            try refreshScope(self.bound);
            var pacer: FabricThrottle = .{ .t = job_seams.Throttle.forJob(f, job) };
            const report = run(&self.bound.host, .{ .scrub = job.rec.arg0 & fsck.JOB_ARG_SCRUB != 0, .scrub_only = job.rec.arg0 & fsck.JOB_ARG_SCRUB_ONLY != 0, .throttle = pacer.throttle() }, if (self.bound.verifier) |*v| v else null) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Body,
            };
            if (self.last) |*old| old.deinit(self.gpa);
            self.last = report;
            if (report.hasFindings()) {
                log.err("fsck job: {d} finding(s) confirmed — the report is the job's output (`squeezefs fsck` prints it; exit 1 is the CLI's)", .{report.findings.len + report.findings_elided});
            }
            // One pass IS the whole detection: a cancel/pause mid-run left
            // no partial state to resume, the next pass re-runs whole.
            if (job.cancel_intent.load(.acquire) or job.pause_intent.load(.acquire)) return .deferred;
            return .completed;
        }

        /// The bounded admin view of the last report (owned by `gpa`).
        pub fn lastReportJson(self: *JobBody, gpa: Allocator) Allocator.Error!?[]u8 {
            const r = self.last orelse return null;
            return try r.boundedJson(gpa, fsck.ADMIN_WIRE_CAP);
        }
    };

    /// The fabric's live duty words behind the scrub's pacer seam.
    const FabricThrottle = struct {
        t: job_seams.Throttle,

        fn throttle(self: *FabricThrottle) scrub.Throttle {
            return .{ .ctx = self, .pct = &pctFn };
        }

        fn pctFn(ctx: ?*anyopaque) u8 {
            const self: *FabricThrottle = @ptrCast(@alignCast(ctx.?));
            return self.t.pct();
        }
    };

    /// The quarantine sink: a per-run dir `<home>/<unix_ns hex>-<pid>/`
    /// created on the first byte; each `put` writes `NNNN_<class>_<label>.bin`
    /// through the io_uring file layer and `fdatasync`s it, then rewrites
    /// + syncs `manifest.json`.
    pub const DirSink = struct {
        gpa: Allocator,
        pool: *uring_fs.Pool,
        home: []u8,
        run_dir: ?[]u8 = null,
        n: u32 = 0,
        manifest: std.Io.Writer.Allocating,

        pub fn init(gpa: Allocator, pool: *uring_fs.Pool, home: []const u8) Allocator.Error!DirSink {
            return .{ .gpa = gpa, .pool = pool, .home = try gpa.dupe(u8, home), .manifest = .init(gpa) };
        }

        pub fn deinit(self: *DirSink) void {
            if (self.run_dir) |d| self.gpa.free(d);
            self.gpa.free(self.home);
            self.manifest.deinit();
            self.* = undefined;
        }

        pub fn sink(self: *DirSink) classes.Quarantine.Sink {
            return .{ .ctx = self, .put = &putFn };
        }

        fn ensureDir(self: *DirSink) Error![]const u8 {
            if (self.run_dir) |d| return d;
            const d = std.fmt.allocPrint(self.gpa, "{s}/{x}-{d}", .{ self.home, clock.realtimeCoarseNs(), linux_os.getpid() }) catch return error.OutOfMemory;
            errdefer self.gpa.free(d);
            var zbuf: [linux_os.PATH_MAX:0]u8 = undefined;
            const zh = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{self.home}, 0) catch return error.Io;
            _ = linux_os.mkdir(zh, 0o700);
            const zd = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{d}, 0) catch return error.Io;
            if (linux_os.errno(linux_os.mkdir(zd, 0o700)) != .SUCCESS) return error.Io;
            self.manifest.writer.print("{{\"schema\":1,\"created_unix\":{d},\"entries\":[", .{clock.realtimeCoarseS()}) catch return error.OutOfMemory;
            self.run_dir = d;
            return d;
        }

        fn putFn(ctx: *anyopaque, class: finding.Class, label: []const u8, bytes: []const u8) Error!void {
            const self: *DirSink = @ptrCast(@alignCast(ctx));
            const d = try self.ensureDir();
            var name_buf: [320]u8 = undefined;
            var safe: [200]u8 = undefined;
            const n = @min(label.len, safe.len);
            for (label[0..n], 0..) |c, i| safe[i] = if (std.ascii.isAlphanumeric(c) or c == '-' or c == '_') c else '_';
            const name = std.fmt.bufPrint(&name_buf, "{s}/{d:0>4}_{s}_{s}.bin", .{ d, self.n, @tagName(class), safe[0..n] }) catch return error.Io;
            self.n += 1;
            self.pool.writeAll(name, bytes) catch return error.Io;
            self.pool.fdatasync(name) catch return error.Io;
            if (self.n != 1) self.manifest.writer.writeByte(',') catch return error.OutOfMemory;
            self.manifest.writer.print("{{\"class\":\"{s}\",\"label\":\"{s}\",\"file\":\"{s}\",\"bytes\":{d}}}", .{ @tagName(class), safe[0..n], std.fs.path.basename(name), bytes.len }) catch return error.OutOfMemory;
            // The manifest BEFORE the action's commit: rewritten whole and
            // synced at every entry (small).
            var man: std.Io.Writer.Allocating = .init(self.gpa);
            defer man.deinit();
            man.writer.print("{s}]}}\n", .{self.manifest.written()}) catch return error.OutOfMemory;
            var mbuf: [320]u8 = undefined;
            const mname = std.fmt.bufPrint(&mbuf, "{s}/manifest.json", .{d}) catch return error.Io;
            self.pool.writeAll(mname, man.written()) catch return error.Io;
            self.pool.fdatasync(mname) catch return error.Io;
        }
    };

    const testing = std.testing;

    test "fsck --offline over a `--single-writer` (flat) set is CLEAN: no appender region and no tree-0 slot rows exist, so the forest census reads nothing (repro-port: the pjdfstest harness's first fsck of a fresh flat format answered `Corrupt`)" {
        const gpa = testing.allocator;
        const s = try meta_set.TestSet.createClass(gpa, "flat-fsck", 1, .single_writer);
        defer s.destroy();
        const be = try s.open();
        defer be.close();
        try testing.expect(!be.volumes[0].sb.symmetricPlaneStamped());
        const bound = try bind(gpa, s.io(), be, null, .{ .env = &s.env, .mode = .offline, .mem_budget = 256 << 20 });
        defer destroy(bound);
        var report = try run(&bound.host, .{}, null);
        defer report.deinit(gpa);
        try testing.expectEqual(@as(usize, 0), report.findings.len);
        try testing.expect(report.counters.inodes_scanned >= 1);
    }
} else struct {};

test {
    if (builtin.os.tag == .linux) _ = linux;
}
