//! `squeezefs appenders [--json] <sqmeta uri>` and `squeezefs appender
//! clear <sqmeta uri> <id>` (charter §8; §5.5 "Operator remedy";
//! `docs/porting/sym_wire_membership_recovery.md` §3.4): the first lists
//! every appender page of every volume of the set off the device — id,
//! identity, state, term, ring segments, leased slots — WITHOUT the flock
//! (a listing is never blocked by a live mount); the second attests a dead
//! joined appender's page: the seven refusals of `membership.recovery.
//! clearAppender` over this process's offline probes (the flock, the
//! claim's age through the gate's probe, the page, this node's identity,
//! the parked-joiner window, a fresh registration), then the four writes —
//! the page `Recovering{0}` to both directory slots + sync FIRST (its
//! `Live` entries attest nothing at the bring-up's settle), the writer
//! bring-up (the set opened through the writer door exactly as `mount`
//! opens it), the EARLY death record on volume 0's control tree + barrier
//! + `checkpoint_now`, the clean leave (the close). Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const linux = std.os.linux;
const cli = @import("cli.zig");
const args = @import("args.zig");
const uri = @import("uri.zig");
const exec = @import("../exec/exec.zig");
const derive = @import("../core/derive.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const mem_budget = @import("../mem/budget.zig");
const uring_fs = @import("../io/uring_fs.zig");
const align_probe = @import("../io/align_probe.zig");
const device_loader = @import("../meta/device_loader.zig");
const superblock = @import("../meta/superblock.zig");
const meta_set = @import("../meta/meta_set.zig");
const node_cache = @import("../meta/node_cache.zig");
const key = @import("../meta/key.zig");
const guard = @import("../meta/guard.zig");
const sym = @import("../meta/sym.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/appenders is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const page = sym.appender_page;
const dir = sym.appender_dir;
const cr = sym.control_records;
const recovery = sym.membership.recovery;
const shard = sym.membership.shard;
const join = sym.join;
const claim = guard.claim;

// ---------------------------------------------------------------------------
// The offline volume: the superblock and the pages off the device.
// ---------------------------------------------------------------------------

const OfflineVolume = struct {
    gpa: Allocator,
    path: []const u8,
    dev: device_loader.DeviceLoader,
    sb: superblock.Superblock,

    const Error = error{ DeviceRefused, NotFormatted, Corrupt, Io, OutOfMemory };

    fn open(gpa: Allocator, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8) Error!OfflineVolume {
        const volume_len = device_loader.volumeLength(path) catch return error.DeviceRefused;
        const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
        var self: OfflineVolume = .{ .gpa = gpa, .path = path, .dev = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 }, .sb = undefined };
        const sector0 = try self.dev.readRange(gpa, 0, superblock.sector_len);
        defer gpa.free(sector0);
        const backup: ?[]u8 = if (superblock.backupOffsetFor(volume_len)) |off| try self.dev.readRange(gpa, off, superblock.sector_len) else null;
        defer if (backup) |b| gpa.free(b);
        var diag: superblock.Refusal = undefined;
        const classified = superblock.classifyVolumeImages(sector0, backup, volume_len, &diag) catch return error.Corrupt;
        self.sb = switch (classified.format) {
            .v5 => |s| s,
            else => return error.NotFormatted,
        };
        return self;
    }

    fn readPageSeam(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self: *OfflineVolume = @ptrCast(@alignCast(ctx));
        const img = self.dev.readRange(self.gpa, offset, page.PAGE_LEN) catch return error.Io;
        defer self.gpa.free(img);
        @memcpy(out, img[0..page.PAGE_LEN]);
    }

    fn reader(self: *OfflineVolume) dir.PageReader {
        return .{ .ctx = self, .read = &readPageSeam };
    }

    const Found = struct { p: page.Page, dir_slots: [2]u64 };

    /// Appender 0 off the fixed ring's four slots; appenders ≥ 1 off the
    /// directory chain (null = no such page).
    fn findPage(self: *OfflineVolume, appender_id: u32) Error!?Found {
        if (appender_id == 0) {
            const offs = page.appender0PageOffsets(self.sb.journal.start);
            var bufs: [page.PAGE_SLOTS][page.PAGE_LEN]u8 = undefined;
            var imgs: [page.PAGE_SLOTS]?[]const u8 = .{ null, null, null, null };
            for (offs, 0..) |off, i| {
                try readPageSeam(self, off, &bufs[i]);
                imgs[i] = &bufs[i];
            }
            const p = (page.newestValid(imgs) catch return error.Corrupt) orelse return null;
            return .{ .p = p, .dir_slots = .{ offs[0], offs[1] } };
        }
        var links: std.ArrayList(dir.Link) = .empty;
        defer links.deinit(self.gpa);
        try self.walk(&links);
        for (links.items) |l| {
            var pair: u32 = 0;
            while (pair < l.header.pairs) : (pair += 1) {
                const imgs = dir.readPair(self.reader(), l, pair) catch return error.Io;
                if (imgs.appender_id != appender_id) continue;
                if (imgs.foreign) return error.Corrupt;
                const p = imgs.newest() orelse return null;
                return .{ .p = p, .dir_slots = imgs.offsets };
            }
        }
        return null;
    }

    fn walk(self: *OfflineVolume, links: *std.ArrayList(dir.Link)) Error!void {
        dir.walkChain(self.gpa, self.reader(), .{ .start = self.sb.appender_dir.start, .len = self.sb.appender_dir.len }, self.sb.node_size, .{ .start = self.sb.heap.start, .len = self.sb.heap.len }, self.sb.totalExtents(), links) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
        };
    }

    /// Every page in id order through `visit`.
    fn forEachPage(self: *OfflineVolume, ctx: anytype, comptime visit: fn (@TypeOf(ctx), page.Page) anyerror!void) !void {
        if (try self.findPage(0)) |f| try visit(ctx, f.p);
        var links: std.ArrayList(dir.Link) = .empty;
        defer links.deinit(self.gpa);
        try self.walk(&links);
        for (links.items) |l| {
            var pair: u32 = 0;
            while (pair < l.header.pairs) : (pair += 1) {
                const imgs = dir.readPair(self.reader(), l, pair) catch return error.Io;
                if (imgs.foreign) return error.Corrupt;
                const p = imgs.newest() orelse continue;
                try visit(ctx, p);
            }
        }
    }

    /// The Layer-A probe: an exclusive flock on the volume (a live mount on
    /// this host) read off `/proc/locks` — taking the lock to test it would
    /// refuse a racing writer, and a held SHARED lock is udev's block-device
    /// probe, never a mount (`guard.proc.flockExclusive`).
    fn flockHeld(self: *const OfflineVolume, io: Io) bool {
        return guard.proc.writerFlockHeld(io, self.path, null);
    }

    /// Write (1): a raw page image to both directory slots, then `fsync`.
    fn writePageRaw(self: *OfflineVolume, pool: *uring_fs.Pool, dir_slots: [2]u64, p: *const page.Page) error{Io}!void {
        var img: [page.PAGE_LEN]u8 = undefined;
        p.encode(&img) catch return error.Io;
        for (dir_slots) |off| pool.writeAt(self.path, off, &img) catch return error.Io;
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = align_probe.pathZ(&zbuf, self.path) catch return error.Io;
        const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return error.Io;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        if (linux.errno(linux.fsync(fd)) != .SUCCESS) return error.Io;
    }
};

// ---------------------------------------------------------------------------
// `appenders`.
// ---------------------------------------------------------------------------

const Lister = struct {
    w: *Io.Writer,
    json: bool,
    path: []const u8,
    rows: usize = 0,

    fn visit(self: *Lister, p: page.Page) anyerror!void {
        if (self.json) {
            if (self.rows != 0) try self.w.writeByte(',');
            try self.w.print("{{\"volume\":\"{s}\",\"appender_id\":{d},\"node\":\"{x:0>16}\",\"mount_slot\":{d},\"writer_id\":\"{x:0>16}\",\"state\":\"{s}\",\"term\":{d},\"is_manager\":{},\"home_volume\":{d},\"ring_bytes\":{d},\"segments\":{d},\"leased_slots\":{d},\"recovered_by_term\":{d}}}", .{ self.path, p.appender_id, p.identity.node_token, p.identity.mount_slot, p.identity.writer_id, @tagName(p.state), p.term, p.is_manager, p.home_volume, p.ringBytes(), p.segments.len, p.n_slots, p.recovered_by_term });
        } else {
            try self.w.print("{d:<4} {x:0>16}.m{x:0>8} {s:<10} {d:<5} {s:<3} {d:>10} {d:>3} {d:>4}  {s}\n", .{ p.appender_id, p.identity.node_token, p.identity.mount_slot, @tagName(p.state), p.term, if (p.is_manager) "mgr" else "", p.ringBytes(), p.segments.len, p.n_slots, self.path });
        }
        self.rows += 1;
    }
};

pub fn run(p: cli.Process, a: args.Appenders) u8 {
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, a.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs appenders: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.Facts.probe())) catch {
        p.stderr.writeAll("squeezefs appenders: the I/O pool could not start\n") catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    var lister: Lister = .{ .w = p.stdout, .json = a.json, .path = "" };
    if (a.json) p.stdout.writeAll("{\"appenders\":[") catch return cli.exit_failed else p.stdout.print("{s:<4} {s:<25} {s:<10} {s:<5} {s:<3} {s:>10} {s:>3} {s:>4}  {s}\n", .{ "ID", "IDENTITY", "STATE", "TERM", "", "RING", "SEG", "SLOT", "VOLUME" }) catch return cli.exit_failed;
    for (meta.paths) |path| {
        var vol = OfflineVolume.open(p.gpa, p.env, pool, path) catch |e| {
            p.stderr.print("squeezefs appenders: {s}: {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
        lister.path = path;
        vol.forEachPage(&lister, Lister.visit) catch |e| {
            p.stderr.print("squeezefs appenders: {s}: {s}\n", .{ path, @errorName(e) }) catch {};
            return cli.exit_failed;
        };
    }
    if (a.json) p.stdout.print("],\"count\":{d}}}\n", .{lister.rows}) catch return cli.exit_failed else p.stdout.print("{d} appender page(s) on {d} volume(s).\n", .{ lister.rows, meta.paths.len }) catch return cli.exit_failed;
    p.stdout.flush() catch return cli.exit_failed;
    return cli.exit_ok;
}

// ---------------------------------------------------------------------------
// `appender clear`.
// ---------------------------------------------------------------------------

const ClearCtx = struct {
    p: cli.Process,
    paths: []const []const u8,
    sqz: *exec.SqzIo,
    pool: *uring_fs.Pool,
    facts: derive.Facts,
    budget_bytes: u64,
    appender_id: u32,
    vol: *OfflineVolume,
    vol0: *OfflineVolume,
    local: claim.Identity,
    /// The set opened by write (2); closed by write (4).
    set: ?*meta_set.MetaSet = null,
    dir_slots: [2]u64 = .{ 0, 0 },

    fn self_(ctx: *anyopaque) *ClearCtx {
        return @ptrCast(@alignCast(ctx));
    }

    fn volOf(self: *ClearCtx, vol0: bool) *OfflineVolume {
        return if (vol0) self.vol0 else self.vol;
    }

    fn flockHeld(ctx: *anyopaque, vol0: bool) bool {
        const self = self_(ctx);
        return self.volOf(vol0).flockHeld(self.sqz.io());
    }

    /// The claim's age through the gate's probe and the ONE classifier.
    fn claimAgeMs(ctx: *anyopaque, vol0: bool) ?u64 {
        const self = self_(ctx);
        const raw = guard.gate.probeClaim(self.p.gpa, self.sqz.io(), self.p.env, self.pool, self.volOf(vol0).path) catch return null;
        defer if (raw) |r| self.p.gpa.free(r);
        const c = claim.Claim.decode(raw orelse return null) catch return 0;
        return c.ageS(clock.realtimeCoarseNs()) * 1000;
    }

    fn readPage(ctx: *anyopaque, appender_id: u32) recovery.Error!?page.Page {
        const self = self_(ctx);
        const f = (self.vol.findPage(appender_id) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Corrupt,
            else => error.Io,
        }) orelse return null;
        self.dir_slots = f.dir_slots;
        return f.p;
    }

    /// This node's own residue: the page's node token is this host's.
    fn pageIsOwn(ctx: *anyopaque, p: *const page.Page) bool {
        const self = self_(ctx);
        return p.identity.node_token == join.nodeToken(self.local);
    }

    /// A registration (a writer claim or a member row) fresh within the
    /// TTL whose boot + host derive the page's node token.
    fn freshRegistrationFor(ctx: *anyopaque, node_token: u64) bool {
        const self = self_(ctx);
        for (self.paths) |path| {
            const rows = guard.gate.clients(self.p.gpa, self.sqz.io(), self.p.env, self.pool, path, null) catch continue;
            defer self.p.gpa.free(rows);
            for (rows) |r| {
                if (r.state != .live) continue;
                const id: claim.Identity = .{ .boot_id = r.boot_id, .pid = r.pid, .start_ticks = 0, .hostname_hash = r.hostname_hash };
                if (join.nodeToken(id) == node_token) return true;
            }
        }
        return false;
    }

    fn writeRecoveringPage(ctx: *anyopaque, p: *const page.Page) recovery.Error!void {
        const self = self_(ctx);
        return self.vol.writePageRaw(self.pool, self.dir_slots, p);
    }

    /// Write (2): the set through the writer door, exactly as `mount`.
    fn bringUp(ctx: *anyopaque) recovery.Error!void {
        const self = self_(ctx);
        self.set = meta_set.MetaSet.open(self.p.gpa, self.sqz.io(), self.paths, .{ .env = self.p.env, .cache_budget_bytes = node_cache.nodeCacheBudgetFromEnv(self.p.env, self.budget_bytes), .cpus = self.facts.possible_cpus }) catch |e| {
            self.p.stderr.print("squeezefs appender clear: the writer bring-up refused: {s}\n", .{@errorName(e)}) catch {};
            return error.Io;
        };
    }

    /// Write (3): the EARLY record on volume 0's control tree, committed,
    /// barriered, checkpointed.
    fn recordEarlyDeath(ctx: *anyopaque, identity: recovery.WriterIdentity) recovery.Error!void {
        const self = self_(ctx);
        const set = self.set orelse return error.Io;
        const rec = cr.DeadMemberWire.init(.{ .epoch = 0, .ts_realtime_ns = @intCast(clock.realtimeCoarseNs()), .pr_key = 0, .early = true }).encode();
        const t = set.begin(@src()) catch return error.OutOfMemory;
        t.stagePut(key.TREE_CONTROL, cr.deadMemberKey(identity.stable()).slice(), &rec) catch {
            set.abandon(t);
            return error.OutOfMemory;
        };
        set.commit(t) catch return error.Io;
        set.fsync() catch return error.Io;
        set.checkpointNow() catch return error.Io;
    }

    /// Write (4): the clean leave (the close's final checkpoint).
    fn leave(ctx: *anyopaque) recovery.Error!void {
        const self = self_(ctx);
        if (self.set) |s| {
            s.close();
            self.set = null;
        }
    }

    const vtable: recovery.ClearSeam.VTable = .{ .flockHeld = &flockHeld, .claimAgeMs = &claimAgeMs, .readPage = &readPage, .pageIsOwn = &pageIsOwn, .freshRegistrationFor = &freshRegistrationFor, .writeRecoveringPage = &writeRecoveringPage, .bringUp = &bringUp, .recordEarlyDeath = &recordEarlyDeath, .leave = &leave };

    fn seam(self: *ClearCtx) recovery.ClearSeam {
        return .{ .ctx = self, .vt = &vtable };
    }
};

fn clearMain(c: *ClearCtx) u8 {
    const p = c.p;
    defer if (c.set) |s| s.close();
    const t_owner = shard.tOwnerMsFromEnv(p.env);
    const t_park_max = sym.membership.park_reclaim.tParkMaxMs(sym.extent_grant.failoverBoundMs(0, 0), sym.membership.park_reclaim.graceMs(t_owner));
    const out = recovery.clearAppender(c.seam(), c.appender_id, t_owner, t_park_max) catch |e| {
        p.stderr.print("squeezefs appender clear: appender {d}: {s} — the page may be Recovering; the re-run is idempotent\n", .{ c.appender_id, @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    switch (out) {
        .refused => |r| {
            p.stderr.writeAll("squeezefs appender clear: ") catch {};
            recovery.writeClearRefusal(p.stderr, r, c.appender_id) catch {};
            p.stderr.writeAll("\n") catch {};
            p.stderr.flush() catch {};
            return cli.exit_failed;
        },
        .cleared => |cl| {
            p.stdout.print("appender {d} cleared (was {s}): its page attests nothing, the death record stands on volume 0 — the next ledger poll recovers its slots\n", .{ c.appender_id, @tagName(cl.was) }) catch {};
            p.stdout.flush() catch {};
            return cli.exit_ok;
        },
    }
}

pub fn runClear(p: cli.Process, a: args.AppenderClear) u8 {
    var members: [uri.MAX_MEMBERS][]const u8 = undefined;
    var udiag: uri.Refusal = undefined;
    const meta = uri.parse(.meta, a.meta_uri, &members, &udiag) catch {
        p.stderr.print("squeezefs appender clear: {f}\n", .{udiag}) catch {};
        return cli.exit_usage;
    };
    const facts = derive.facts();
    const budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = p.io, .env = p.env, .facts = facts });
    const sqz = exec.SqzIo.init(p.gpa, .{ .env = p.env, .facts = facts, .budget_bytes = budget_bytes, .budget = &mem_budget.authority, .meta_lanes = 1 }) catch |e| {
        p.stderr.print("squeezefs appender clear: executor init failed ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
    defer sqz.deinit();
    const pool = uring_fs.Pool.init(p.gpa, uring_fs.Config.resolve(p.env, derive.Facts.probe())) catch {
        p.stderr.writeAll("squeezefs appender clear: the I/O pool could not start\n") catch {};
        return cli.exit_failed;
    };
    defer pool.deinit();
    // The verb attests a page of the LAST volume named when several are
    // (the set's volume 0 is the first); one volume = both roles.
    var vol = OfflineVolume.open(p.gpa, p.env, pool, meta.paths[meta.paths.len - 1]) catch |e| {
        p.stderr.print("squeezefs appender clear: {s}: {s}\n", .{ meta.paths[meta.paths.len - 1], @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    var vol0 = OfflineVolume.open(p.gpa, p.env, pool, meta.paths[0]) catch |e| {
        p.stderr.print("squeezefs appender clear: {s}: {s}\n", .{ meta.paths[0], @errorName(e) }) catch {};
        return cli.exit_failed;
    };
    var c: ClearCtx = .{ .p = p, .paths = meta.paths, .sqz = sqz, .pool = pool, .facts = facts, .budget_bytes = budget_bytes, .appender_id = a.appender_id, .vol = &vol, .vol0 = &vol0, .local = guard.proc.localIdentity() };
    return sqz.run(clearMain, .{&c}) catch |e| {
        p.stderr.print("squeezefs appender clear: the root task could not start ({s})\n", .{@errorName(e)}) catch {};
        return cli.exit_failed;
    };
}
