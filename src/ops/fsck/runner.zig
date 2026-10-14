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
    /// Fold the report into the instruments (`Report.publish`); a fleet
    /// run publishes its MERGED report once instead of every residue's.
    publish: bool = true,
    /// Carry the census's owners (ino → bindings) in the report: a fleet
    /// member's proposal and the coordinator's own view, the merged
    /// reference arms' inputs (`classes.crossMemberBlockPlane`).
    ship_owners: bool = false,
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
    if (host.store.tally) |t| t.* = .{};
    var pass = try classes.Pass.begin(host);
    defer pass.deinit();
    defer pass.end();
    if (!opts.scrub_only) try pass.nominate();
    pass.settle();
    const t_recheck = clock.monotonicNs();
    try pass.recheck();
    const t_scrub = clock.monotonicNs();
    pass.counters.recheck_ms = classes.msBetween(t_recheck, t_scrub);
    if ((opts.scrub or opts.scrub_only) and verifier != null) {
        // A FRESH census post-settle: the mappings the scrub reads are the
        // ones that stood after the window.
        var tags_buf: [64]u64 = undefined;
        const tags = host.knownTags(&tags_buf);
        var fresh = try census.run(host.gpa, host.store, .{ .scope = host.scope, .shard = host.shard, .custody_only = host.custody_only, .mem_budget = host.mem_budget, .geo = host.geo, .known_vol_tags = tags });
        defer fresh.deinit();
        try scrub.run(&pass, &fresh, verifier.?, opts.throttle);
        pass.counters.scrub_ms = classes.msBetween(t_scrub, clock.monotonicNs());
    }
    if (host.store.tally) |t| {
        pass.counters.nodes_walked = t.nodes;
        pass.counters.entries_visited = t.visited;
        pass.counters.entries_read = t.read;
        pass.counters.store_calls = t.calls;
        pass.counters.trees_consulted = t.sources;
    }
    var report = try pass.report(host.gpa);
    errdefer report.deinit(host.gpa);
    if (opts.ship_owners) report.owners = try ownerRefs(host.gpa, &pass.cen);
    if (opts.repair) |ropts| {
        var r = ropts;
        // The scrub's verifier is C2-lost's content arm too.
        if (r.verifier == null) r.verifier = verifier;
        var eng = repair.Engine.init(&pass, r);
        report.repair = try eng.run(report.findings);
    }
    report.counters.scan_ms = (clock.monotonicNs() -| t0) / std.time.ns_per_ms;
    if (opts.publish) report.publish();
    return report;
}

/// The census's owners as the report's rows (every binding of every ino).
fn ownerRefs(gpa: Allocator, cen: *const census.Census) Allocator.Error![]finding.OwnerRef {
    const out = try gpa.alloc(finding.OwnerRef, cen.owners.items.len);
    var built: usize = 0;
    errdefer {
        for (out[0..built]) |o| gpa.free(o.bindings);
        gpa.free(out);
    }
    for (cen.owners.items) |o| {
        const bs = try gpa.alloc(finding.BindingRef, o.bindings.len);
        for (o.bindings, bs) |b, *r| r.* = .{ .vol_tag = b.vol_tag, .block_idx = b.block_idx, .block_index = b.block_index };
        out[built] = .{ .ino = o.ino, .bindings = bs };
        built += 1;
    }
    return out;
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
    const data_bitmap = @import("../../data/sym_planes/data_bitmap.zig");
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
    const wire_mod = @import("../jobs/wire.zig");

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
        /// The run's walk accounting the host's store face adds to.
        tally: census.WalkTally = .{},
        host: Host,

        /// The holder's bitmaps NOW: a volume's bitmap is the holder
        /// plane's while this mount holds the allocation lease, null else.
        pub fn refreshBitmaps(self: *Bound, face: ?BitmapFace) void {
            for (self.vols) |*v| v.bitmap = if (face) |f| f.bitmapOf(f.ctx, v.vol_tag) else null;
        }

        pub fn deinit(self: *Bound) void {
            if (self.verifier) |*v| v.deinit();
            self.gpa.free(self.xform_scratch);
            self.gpa.free(self.xforms);
            self.gpa.free(self.vols);
            self.scope.deinit();
            self.* = undefined;
        }
    };

    /// The allocation holder's live bitmaps, resolved at EVERY pass (the
    /// holder plane comes with the allocation lease and goes with a
    /// failover): the C6 bitmap arm and the lessee-carve exemption.
    pub const BitmapFace = struct {
        ctx: *anyopaque,
        bitmapOf: *const fn (ctx: *anyopaque, vol_tag: u64) ?*data_bitmap.DataBitmap,
    };

    pub const BindOptions = struct {
        env: *const std.process.Environ.Map,
        mode: finding.Mode,
        mem_budget: u64,
        shard: ?finding.Shard = null,
        /// The live filesystem (its 4a manager): online only.
        fs: ?*BackendFs = null,
        /// The holder's bitmaps (online, the manager): null = the flat arm.
        bitmaps: ?BitmapFace = null,
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
        // The forest's classes off tree 0 and the pages: an appender LIVE
        // on the directory with its ring unrecovered keeps its slots (the
        // open refuses them until the recovery ladder runs) — they are a
        // live lessee's to this census, never judged (the offline fsck
        // after a killed member read its directory's name as dangling).
        try refreshLiveScope(b);
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
        b.refreshBitmaps(opts.bitmaps);
        b.xforms = try xforms.toOwnedSlice(gpa);
        if (plane) |pl| {
            b.verifier = try scrub.Verifier.init(gpa, .{ .ctx = pl, .read = &readBlock }, pl.chunk, b.xforms);
            // C4/C5 over THIS member's root (0003 §T.1): the plane's ring
            // and the marker its bind stamped.
            if (pl.stagingRoot()) |root| b.staging_face = .{ .plane = pl, .root = root };
        }
        b.host = .{
            .gpa = gpa,
            .io = io,
            .store = blk: {
                var st = census.Store.from(meta_set.MetaSet, be);
                st.merged_kinds = true;
                st.tally = &b.tally;
                break :blk st;
            },
            .geo = if (plane) |pl| pl.geo else .{},
            .name_key = be.nameKey(),
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

    /// This process's appender id on `be` (the manager's region 0, a
    /// joiner's region); null on a flat volume.
    fn ownAppenderId(be: *meta_set.KvBackend) ?u32 {
        if (be.sym) |set| return set.region0().appenderId();
        if (be.joined) |js| return js.appenderId();
        return null;
    }

    /// The JOINED posture's binding (`ops_fsck.md` §2.5, the fabric's at
    /// every online pass): the posture itself (a joiner is a `member`; the
    /// D0 winner a `manager`), each routing slot's class off tree 0 and
    /// the appender pages — leased HERE (this process's appender id, or a
    /// native slot of a volume this process manages), FOREIGN-LIVE (a
    /// lessee whose page is Live or Recovering: its tree changes under
    /// this census — never judged), UNLEASED (nobody's: every record
    /// prior-era) — and the WINDOWS: every live foreign appender's ring
    /// scanned from its page's tail off the device, the GLOBAL inos its
    /// slot-tree frames name (a dentry's child from the frame's value, a
    /// DELETE's child through the tree) scoped out of C9/C10. A ring that
    /// cannot be read makes the WHOLE inode plane abstain
    /// (`window_unknown`). On a flat set every slot stays this open's.
    pub fn refreshLiveScope(b: *Bound) Allocator.Error!void {
        try refreshScope(b);
        const gpa = b.gpa;
        const set = b.be;
        var symmetric = false;
        for (set.volumes) |be| symmetric = symmetric or be.sb.symmetricPlaneStamped();
        if (!symmetric) return;
        // The probe (an offline open) keeps its posture: every judged slot
        // is prior-era there; the lessee classes below still apply.
        if (b.scope.posture != .probe) b.scope.posture = if (set.posture == .joined) .member else .manager;
        b.scope.leased.clearRetainingCapacity();
        b.scope.foreign_live.clearRetainingCapacity();
        b.scope.unleased.clearRetainingCapacity();
        b.scope.window_inos.clearRetainingCapacity();
        b.scope.window_unknown = false;
        var arena = std.heap.ArenaAllocator.init(gpa);
        defer arena.deinit();
        const a = arena.allocator();
        var pages: std.ArrayList(page.Page) = .empty;
        defer pages.deinit(gpa);
        var vol_of_page: std.ArrayList(u16) = .empty;
        defer vol_of_page.deinit(gpa);
        var tree0: std.ArrayList(classes.Tree0Row) = .empty;
        for (set.volumes, 0..) |be, vi| {
            if (!be.sb.symmetricPlaneStamped()) continue;
            const before = pages.items.len;
            ForestBind.readPages(gpa, be, &pages) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => {
                    b.scope.window_unknown = true;
                    continue;
                },
            };
            for (before..pages.items.len) |_| try vol_of_page.append(gpa, @intCast(vi));
            ForestBind.readTree0(gpa, a, be, @intCast(vi), &tree0) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => b.scope.window_unknown = true,
            };
        }
        var s: u32 = 0;
        while (s < inos.ROUTING_WIDTH) : (s += 1) {
            const slot: u16 = @intCast(s);
            const vi = set.volumeIndexOfSlot(slot);
            const be = set.volumes[vi];
            const mine = ownAppenderId(be);
            var row: ?cr.SlotState = null;
            for (tree0.items) |r| if (r.vol == vi and r.slot == slot) {
                row = r.state;
            };
            const native_here = be.stamp.native_slot != null and be.stamp.native_slot.? == slot;
            if (row == null or row.?.variant == .unleased) {
                // A native slot is its volume's manager's; a joiner sees the
                // manager's native slot as a live foreign lessee's.
                if (native_here) {
                    if (be.posture == .joined) try b.scope.foreign_live.put(gpa, slot, {}) else try b.scope.leased.put(gpa, slot, {});
                } else try b.scope.unleased.put(gpa, slot, {});
                continue;
            }
            const st = row.?;
            if (st.variant == .leased and mine != null and st.appender_id == mine.?) {
                try b.scope.leased.put(gpa, slot, {});
                continue;
            }
            // Another lessee, or the recovery's structural mark: its tree is
            // not this census's to judge (a dead lessee's recovery rewrites
            // it under the manager's serialization).
            try b.scope.foreign_live.put(gpa, slot, {});
        }
        // The live lease table is this mount's own authority on what it
        // holds: a joiner's tree 0 is a projection of the manager's pen
        // and lags its own grants (a fleet member read none of its slots
        // as leased, and its custody residue scrubbed nothing).
        var held: std.ArrayList(u16) = .empty;
        defer held.deinit(gpa);
        for (set.volumes) |be| {
            if (!be.sb.symmetricPlaneStamped()) continue;
            try be.slots.table.heldBy(be.slots.me, &held);
        }
        for (held.items) |slot| {
            try b.scope.leased.put(gpa, slot, {});
            _ = b.scope.foreign_live.remove(slot);
            _ = b.scope.unleased.remove(slot);
        }
        // The windows of every live foreign appender.
        for (pages.items, vol_of_page.items) |*p, vi| {
            if (p.state != .live and p.state != .recovering) continue;
            const be = set.volumes[vi];
            if (ownAppenderId(be)) |mine| if (p.appender_id == mine) continue;
            if (p.head_hint <= p.ledger_tail_seq) continue;
            foreignWindowInos(b, be, p) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => {
                    b.scope.window_unknown = true;
                    log.warn("fsck online: appender {d}'s ring on meta volume {d} could not be read ({s}) — the inode plane takes no verdict this run", .{ p.appender_id, vi, @errorName(e) });
                },
            };
        }
        log.debug("fsck online: the forest scope — posture {t}, {d} leased, {d} foreign-live, {d} unleased slot(s), {d} window ino(s), {d} tree-0 row(s), {d} page(s){s}", .{ b.scope.posture, b.scope.leased.count(), b.scope.foreign_live.count(), b.scope.unleased.count(), b.scope.window_inos.count(), tree0.items.len, pages.items.len, if (b.scope.window_unknown) ", a window UNREADABLE" else "" });
    }

    /// One live foreign ring's window into `scope.window_inos`: the ring's
    /// segments read whole off the device (a snapshot of a ring being
    /// appended — the scan stops at the first torn entry, which is the
    /// appender's in-flight write, never an acked record), scanned from
    /// the page's tail with the journal's own total decoder.
    fn foreignWindowInos(b: *Bound, be: *meta_set.KvBackend, p: *const page.Page) Error!void {
        const gpa = b.gpa;
        const part = page.ringPartOf(p.appender_id, &p.segments);
        var exts: [page.RING_SEGMENTS_MAX]journal.ExtentRef = undefined;
        for (part.slice(), 0..) |seg, i| exts[i] = .{ .start = seg.start, .len = seg.len };
        const ring_layout = journal.RingLayout.fromSegments(exts[0..part.len]) catch return error.Corrupt;
        const geo = journal.productionGeometry(ring_layout.pages, be.dev.mode.grain());
        const image = try gpa.alloc(u8, @intCast(ring_layout.ringBytes()));
        defer gpa.free(image);
        var off: usize = 0;
        for (part.slice()) |seg| {
            const bytes = be.dev.readRange(gpa, seg.start, @intCast(seg.len)) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
            };
            defer gpa.free(bytes);
            @memcpy(image[off .. off + bytes.len], bytes);
            off += bytes.len;
        }
        var rec = try journal.replayScanImage(gpa, image, geo, p.ledger_tail_seq, p.appender_id);
        defer rec.deinit();
        var named: u64 = 0;
        for (rec.entries) |e| {
            var it = e.records();
            while (it.next()) |f| {
                const tag = journal.untag(f.tag);
                if (tag.level != 0 or !key.isSlotTreeKind(tag.tree_id)) continue;
                _ = key.checkSlotTreeKey(tag.tree_id, f.key) catch continue;
                const key_ino = std.mem.readInt(u64, f.key[key.routeOff(tag.tree_id)..][0..8], .big);
                // A record under a slot's reserved local (a cross-owner
                // intent's home, local 0) names no inode (0003 §X.6).
                const owned = census.globalOfOwnedKeyIno(key_ino) orelse continue;
                try b.scope.window_inos.put(gpa, owned, {});
                named += 1;
                if (tag.tree_id != key.TREE_DENTRIES) continue;
                // The child: off a Put's value, else (a DELETE) the tree's
                // record of the key, if it still stands.
                const child: ?u64 = blk: {
                    if (f.value.len != 0) {
                        const dv = record.DentryValue.decode(f.value) catch break :blk null;
                        break :blk dv.child_ino;
                    }
                    const v = (b.host.store.lookup(gpa, key.TREE_DENTRIES, f.key) catch break :blk null) orelse break :blk null;
                    defer gpa.free(v);
                    const dv = record.DentryValue.decode(v) catch break :blk null;
                    break :blk dv.child_ino;
                };
                if (child) |c| {
                    try b.scope.window_inos.put(gpa, c, {});
                    named += 1;
                }
            }
        }
        fsck.stats.fsck_inode_plane_window_scoped.add(named);
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
            const found = staging.Ring.readMarker(pl.backend.pool, self.root) catch return .unreadable;
            return switch (staging.bindVerdict(found, pl.stage.?.marker)) {
                .absent, .match, .upgrade => null,
                .dead => .dead,
                .foreign_owner => .foreign_scope,
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

        /// Every appender page of `be` (appender 0's newest of its four
        /// slots, then the directory chain's pairs), appended to `out`.
        fn readPages(gpa: Allocator, be: *meta_set.KvBackend, out: *std.ArrayList(page.Page)) Error!void {
            var rd: Reader = .{ .be = be };
            const reader: dir.PageReader = .{ .ctx = &rd, .read = &Reader.read };
            {
                const offs = page.appender0PageOffsets(be.sb.journal.start);
                var bufs: [page.PAGE_SLOTS][page.PAGE_LEN]u8 = undefined;
                var imgs: [page.PAGE_SLOTS]?[]const u8 = .{ null, null, null, null };
                for (offs, 0..) |off, i| {
                    Reader.read(&rd, off, &bufs[i]) catch return error.Io;
                    imgs[i] = &bufs[i];
                }
                if (page.newestValid(imgs) catch return error.Corrupt) |p| try out.append(gpa, p);
            }
            var links: std.ArrayList(dir.Link) = .empty;
            defer links.deinit(gpa);
            dir.walkChain(gpa, reader, .{ .start = be.sb.appender_dir.start, .len = be.sb.appender_dir.len }, be.sb.node_size, .{ .start = be.sb.heap.start, .len = be.sb.heap.len }, be.sb.totalExtents(), &links) catch |e| return switch (e) {
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
                    try out.append(gpa, p);
                }
            }
        }

        /// Tree 0's `slot_state` rows of `be`.
        fn readTree0(gpa: Allocator, arena: Allocator, be: *meta_set.KvBackend, vol: u16, out: *std.ArrayList(classes.Tree0Row)) Error!void {
            const r = cr.rangeOf(.slot_state);
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(gpa, &kvs);
            be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs) catch return error.Io;
            for (kvs.items) |kv| {
                const slot = cr.slotOfKey(kv.key) orelse continue;
                const st = cr.SlotStateWire.decode(kv.value) catch return error.Corrupt;
                try out.append(arena, .{ .vol = vol, .slot = slot, .state = st });
            }
        }

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
                var raw: std.ArrayList(page.Page) = .empty;
                defer raw.deinit(self.gpa);
                try readPages(self.gpa, be, &raw);
                for (raw.items) |p| try pages.append(arena, try view(arena, vol, p));
                try readTree0(self.gpa, arena, be, vol, &tree0);
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
        bound: *Bound,
        last: ?finding.Report = null,
        /// The fleet READ-shard dispatcher (the coordinator's; `none`
        /// without a wire): a pass with read capacity ships residues.
        fleet: job_seams.FleetDispatch = job_seams.FleetDispatch.none,
        /// The holder's bitmaps, read at every pass (null = the flat arm).
        bitmaps: ?BitmapFace = null,

        pub fn deinit(self: *JobBody) void {
            if (self.last) |*r| r.deinit(self.gpa);
            destroy(self.bound);
            self.* = undefined;
        }

        pub fn body(self: *JobBody) fabric.Body {
            return .{ .ctx = self, .pass = &passFn };
        }

        fn bindOptions(self: *const JobBody) BindOptions {
            return .{ .env = self.env, .mode = .online, .mem_budget = self.mem_budget, .fs = self.fs, .bitmaps = self.bitmaps };
        }

        /// The binding is REBUILT when the plane's volume population
        /// changed; the host takes this run's io and stop words, then the
        /// fabric's binding (the live forest's posture, leased / foreign-
        /// live / unleased slots, the foreign windows).
        fn rebind(self: *JobBody, io: Io, cancel: ?*const std.atomic.Value(bool), pause: ?*const std.atomic.Value(bool)) Allocator.Error!void {
            if (self.bound.vols.len != self.plane.vols.n()) {
                const fresh = bind(self.gpa, io, self.be, self.plane, self.bindOptions()) catch return error.OutOfMemory;
                destroy(self.bound);
                self.bound = fresh;
            }
            self.bound.host.io = io;
            self.bound.host.cancel = cancel;
            self.bound.host.pause = pause;
            self.bound.refreshBitmaps(self.bitmaps);
            try refreshLiveScope(self.bound);
        }

        fn passFn(ctx: *anyopaque, io: Io, f: *fabric.Fabric, job: *fabric.JobCtl) fabric.PassError!fabric.PassOutcome {
            const self: *JobBody = @ptrCast(@alignCast(ctx));
            try self.rebind(io, &job.cancel_intent, &job.pause_intent);
            var pacer: FabricThrottle = .{ .t = job_seams.Throttle.forJob(f, job) };
            const opts: Options = .{ .scrub = job.rec.arg0 & fsck.JOB_ARG_SCRUB != 0, .scrub_only = job.rec.arg0 & fsck.JOB_ARG_SCRUB_ONLY != 0, .throttle = pacer.throttle() };
            // The record's shard word (`admin_fsck`'s): a sharded run judges
            // its residue of the inode plane alone, like the offline shard;
            // a WHOLE run with read capacity ships residues to the fleet.
            const requested = fsck.jobShardOf(job.rec.arg1);
            const capacity: u32 = if (requested == null) self.fleet.readCapacity() else 0;
            const report = if (capacity == 0) blk: {
                self.bound.host.shard = requested;
                self.bound.host.custody_only = false;
                break :blk run(&self.bound.host, opts, if (self.bound.verifier) |*v| v else null) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Body,
                };
            } else self.runFleet(io, job, opts, 1 + capacity) catch |e| return switch (e) {
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

        /// The fleet run (spec §1.9a, `ops_fsck.md` §7) over the set's
        /// CUSTODY: a live lessee's slots are readable only on the lessee
        /// (its trees leave the coordinator's forest at the grant), so the
        /// residues are the members' custodies, never ino residues — `n − 1`
        /// member shards QUEUED for the read-capable workers first (each
        /// runs its engine over its own leased slots alone and ships its
        /// derived census), the whole of THIS mount's view run HERE online
        /// with the suspect/settle machinery armed, then every shipped
        /// residue collected: a proposal merges (its report parsed by the
        /// ONE decoder); a lost one is counted (`job_fleet_shards_relocal`)
        /// and the local view re-run once — a dead member's slots are the
        /// manager's again after its recovery, a live-but-silent member's
        /// stay its own. The MERGED reference arms (`crossMemberBlockPlane`)
        /// run over the coordinator's owners and the members': a suspect
        /// is CONFIRMED against a second census round from the members (the
        /// ladder's re-check with their own fresh evidence) and lands as a
        /// finding; an unconfirmable one (a member lost between the rounds)
        /// is cleared and counted. The shards retire at the pass end; the
        /// MERGED report publishes once, its census rows dropped.
        fn runFleet(self: *JobBody, io: Io, job: *fabric.JobCtl, opts: Options, n: u32) Error!finding.Report {
            const gpa = self.gpa;
            const shards: u32 = @min(n, @as(u32, @intCast(wire_mod.MAX_READ_SHARDS)));
            var queued: [wire_mod.MAX_READ_SHARDS]bool = @splat(false);
            self.queueCustodies(job, shards, 0, &queued);
            defer self.fleet.retireFleetShards(job.id());
            var parts: std.ArrayList(finding.Report) = .empty;
            defer {
                for (parts.items) |*p| p.deinit(gpa);
                parts.deinit(gpa);
            }
            var local = opts;
            local.publish = false;
            local.ship_owners = true;
            self.bound.host.shard = null;
            self.bound.host.custody_only = false;
            try parts.append(gpa, try run(&self.bound.host, local, if (self.bound.verifier) |*v| v else null));
            var member_owners: std.ArrayList(finding.OwnerRef) = .empty;
            defer {
                for (member_owners.items) |o| gpa.free(o.bindings);
                member_owners.deinit(gpa);
            }
            var remote: u32 = 0;
            var lost: u32 = 0;
            var k: u32 = 1;
            while (k < shards) : (k += 1) {
                var landed = false;
                if (queued[k]) switch (self.fleet.collectReadShard(io, gpa, .{ .job_id = job.id(), .shard = k })) {
                    .payload => |p| {
                        defer self.fleet.release(gpa, p);
                        if (finding.Report.parse(gpa, p)) |rep| {
                            var r = rep;
                            try takeOwners(gpa, &r, &member_owners);
                            try parts.append(gpa, r);
                            landed = true;
                            remote += 1;
                        } else |_| log.warn("fsck fleet: residue {d}/{d} of job-{x:0>16} proposed a report that does not parse — lost", .{ k, shards, job.id() });
                    },
                    .lost => {},
                };
                if (!landed) lost += 1;
            }
            if (lost != 0 and !self.bound.host.cancelled()) {
                // The local view again, in place of the first pass: what a
                // dead member's recovery returned to this mount is in it.
                var again = try run(&self.bound.host, local, if (self.bound.verifier) |*v| v else null);
                parts.items[0].deinit(gpa);
                parts.items[0] = again;
                again = undefined;
            }
            // The merged reference arms, confirmed by a second census round.
            var cross: finding.Counters = .{};
            var confirmed: std.ArrayList(finding.Finding) = .empty;
            defer confirmed.deinit(gpa);
            if (member_owners.items.len != 0 and !self.bound.host.cancelled()) {
                var first: std.ArrayList(finding.Finding) = .empty;
                defer first.deinit(gpa);
                try classes.crossMemberBlockPlane(gpa, &self.bound.host, parts.items[0].owners, member_owners.items, &cross, &first);
                if (first.items.len != 0) {
                    self.fleet.retireFleetShards(job.id());
                    var queued2: [wire_mod.MAX_READ_SHARDS]bool = @splat(false);
                    self.queueCustodies(job, shards, shards, &queued2);
                    var owners2: std.ArrayList(finding.OwnerRef) = .empty;
                    defer {
                        for (owners2.items) |o| gpa.free(o.bindings);
                        owners2.deinit(gpa);
                    }
                    var whole = true;
                    k = 1;
                    while (k < shards) : (k += 1) {
                        var landed = false;
                        if (queued2[k]) switch (self.fleet.collectReadShard(io, gpa, .{ .job_id = job.id(), .shard = shards + k })) {
                            .payload => |p| {
                                defer self.fleet.release(gpa, p);
                                if (finding.Report.parse(gpa, p)) |rep| {
                                    var r = rep;
                                    defer r.deinit(gpa);
                                    try takeOwners(gpa, &r, &owners2);
                                    landed = true;
                                } else |_| {}
                            },
                            .lost => {},
                        };
                        if (!landed) whole = false;
                    }
                    var second: std.ArrayList(finding.Finding) = .empty;
                    defer second.deinit(gpa);
                    if (whole) {
                        try classes.crossMemberBlockPlane(gpa, &self.bound.host, parts.items[0].owners, owners2.items, &cross, &second);
                        try classes.confirmCrossMember(gpa, first.items, second.items, &cross, &confirmed);
                    } else {
                        // A member silent between the rounds: nothing to
                        // confirm against; its slots are the manager's once
                        // it is recovered, and the next run judges them.
                        cross.suspects_cleared += @intCast(first.items.len);
                        log.warn("fsck fleet: job-{x:0>16}: {d} merged-census suspect(s) could not be confirmed — a member's second census did not land", .{ job.id(), first.items.len });
                    }
                }
            }
            var merged = try finding.merge(gpa, parts.items);
            errdefer merged.deinit(gpa);
            if (confirmed.items.len != 0) {
                const all = try gpa.alloc(finding.Finding, merged.findings.len + confirmed.items.len);
                @memcpy(all[0..merged.findings.len], merged.findings);
                @memcpy(all[merged.findings.len..], confirmed.items);
                gpa.free(merged.findings);
                const nn = finding.Report.normalize(all);
                merged.findings = try gpa.realloc(all, nn);
            }
            merged.counters.merge(cross);
            merged.mode = .online;
            merged.publish();
            log.info("fsck fleet: job-{x:0>16} judged {d} residue(s): {d} proposed by the fleet, {d} run here; {d} member binding(s) merged, {d} confirmed cross-member finding(s)", .{ job.id(), shards, remote, shards - remote, cross.block_plane_member_bindings, confirmed.items.len });
            return merged;
        }

        /// Queue one custody residue per read-capable member, shard ids
        /// `base + k` (the second census round takes `base = shards`).
        fn queueCustodies(self: *JobBody, job: *fabric.JobCtl, shards: u32, base: u32, queued: *[wire_mod.MAX_READ_SHARDS]bool) void {
            var k: u32 = 1;
            while (k < shards) : (k += 1) {
                queued[k] = self.fleet.queueReadShard(.{ .id = .{ .job_id = job.id(), .shard = base + k }, .fleet_k = k, .fleet_n = shards, .inode_plane = 0, .throttle_pct = job.ctl.loadPct() }, job.rec.arg0);
            }
        }

        /// Move a proposal's census rows out of the report (the report's
        /// deinit then frees none of them).
        fn takeOwners(gpa: Allocator, r: *finding.Report, into: *std.ArrayList(finding.OwnerRef)) Allocator.Error!void {
            try into.appendSlice(gpa, r.owners);
            gpa.free(r.owners);
            r.owners = &.{};
        }

        /// The worker side (`wire.ReadRunner`): THIS member's engine over
        /// its own binding — its plane's read path, its posture's scope —
        /// running the residue the coordinator leased it, the cancel the
        /// lease clock raises as the stop latch; the report as JSON.
        pub fn readRunner(self: *JobBody) wire_mod.ReadRunner {
            return .{ .ctx = self, .run = &readShardFn };
        }

        fn readShardFn(ctx: *anyopaque, io: Io, gpa: Allocator, d: wire_mod.ReadDescriptor, cancel: *const std.atomic.Value(bool)) error{ OutOfMemory, Io }!?[]u8 {
            const self: *JobBody = @ptrCast(@alignCast(ctx));
            try self.rebind(io, cancel, null);
            // The member's residue is its custody: the slots it leases.
            self.bound.host.shard = null;
            self.bound.host.custody_only = true;
            var pacer: FixedThrottle = .{ .pct = d.throttle_pct };
            // The member's census rides the proposal (the merged arms'
            // input at the coordinator); its counters fold into this
            // member's own instruments (the work was done here).
            var report = run(&self.bound.host, .{ .scrub = d.arg0 & fsck.JOB_ARG_SCRUB != 0, .scrub_only = d.arg0 & fsck.JOB_ARG_SCRUB_ONLY != 0, .throttle = pacer.throttle(), .ship_owners = true }, if (self.bound.verifier) |*v| v else null) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Io,
            };
            defer report.deinit(gpa);
            if (cancel.load(.acquire)) return null;
            var out: std.Io.Writer.Allocating = .init(gpa);
            errdefer out.deinit();
            report.writeJson(&out.writer) catch return error.OutOfMemory;
            return try out.toOwnedSlice();
        }
    };

    /// The descriptor's duty cycle as the scrub's pacer (a remote shard
    /// keeps the duty it was granted).
    const FixedThrottle = struct {
        pct: u8,
        fn throttle(self: *FixedThrottle) scrub.Throttle {
            return .{ .ctx = self, .pct = &pctFn };
        }
        fn pctFn(ctx: ?*anyopaque) u8 {
            const self: *FixedThrottle = @ptrCast(@alignCast(ctx.?));
            return self.pct;
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
