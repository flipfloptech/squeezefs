//! `squeezefs claim convert-pr --all-mounts-down` (decision 0004 §6.4).
//! Offline, one metadata volume at a time: the flock, a fresh claim and a
//! live sibling refuse, then bit 4 is stamped BEFORE the reservation becomes
//! Write Exclusive — All Registrants, so a crash never leaves that type on a
//! volume an old writer would still mount. Pages move to layout 2 afterwards.
//! Not a `KvBackend.open`: that door writes before the caller could stamp.
//! Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const checksum = @import("../../core/checksum.zig");
const derive = @import("../../core/derive.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const align_probe = @import("../../io/align_probe.zig");
const exec = @import("../../exec/exec.zig");
const mem_budget = @import("../../mem/budget.zig");
const uri = @import("../../cli/uri.zig");
const initiator = @import("../../ops/nvmet/initiator.zig");
const superblock = @import("../superblock.zig");
const device_loader = @import("../device_loader.zig");
const backend = @import("../backend.zig");
const appender_page = @import("../sym/appender_page.zig");
const appender_dir = @import("../sym/appender_dir.zig");
const joined = @import("../sym/joined.zig");
const join = @import("../sym/join.zig");
const guard = @import("../guard.zig");
const gate = @import("gate.zig");
const host_fence = @import("host_fence.zig");
const pr = @import("pr.zig");
const pr_linux = @import("pr_linux.zig");
const proc = @import("proc.zig");
const nvme = @import("nvme_cmd.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/convert_pr is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const ConvertError = error{ InvalidUri, OutOfMemory, ExecutorInit, FencerDisconnect };

pub const ConvertOptions = struct {
    /// The process io pool; one is created when null.
    pool: ?*uring_fs.Pool = null,
};

pub const Outcome = enum {
    converted,
    already,
    live_mounted,
    fresh_claim,
    live_sibling,
    foreign_page,
    open_failed,

    pub fn refused(self: Outcome) bool {
        return self != .converted and self != .already;
    }
};

pub const VolumeReport = struct {
    path: []const u8,
    outcome: Outcome,

    pub fn refused(self: VolumeReport) bool {
        return self.outcome.refused();
    }

    pub fn write(self: VolumeReport, w: *Io.Writer) Io.Writer.Error!void {
        switch (self.outcome) {
            .converted => try w.print("{s}: converted to META_PR_ALL_REGISTRANTS (decision 0004 §6.4)", .{self.path}),
            .already => try w.print("{s}: already META_PR_ALL_REGISTRANTS — nothing to convert", .{self.path}),
            .live_mounted => try w.print("{s}: refusing convert-pr — the volume is live-mounted on this host (the writer lock is held). Unmount it first", .{self.path}),
            .fresh_claim => try w.print("{s}: refusing convert-pr over a fresh writer claim (decision 0004 §6.4)", .{self.path}),
            .live_sibling => try w.print("{s}: refusing convert-pr — a live sibling holds this set on this host (decision 0004 §6.4)", .{self.path}),
            .foreign_page => try w.print("{s}: refusing convert-pr — a page has a foreign layout (decision 0004 §6.4)", .{self.path}),
            .open_failed => try w.print("{s}: convert-pr failed", .{self.path}),
        }
    }
};

pub const Report = struct {
    gpa: Allocator,
    volumes: []VolumeReport,
    paths: [uri.MAX_MEMBERS][]const u8,
    uri_copy: []u8,

    pub fn deinit(self: *Report) void {
        if (self.volumes.len != 0) self.gpa.free(self.volumes);
        if (self.uri_copy.len != 0) self.gpa.free(self.uri_copy);
        self.* = undefined;
    }

    pub fn write(self: Report, w: *Io.Writer) Io.Writer.Error!void {
        for (self.volumes) |v| {
            try v.write(w);
            try w.writeByte('\n');
        }
    }
};

/// Per volume of the URI, in set order, after every fencer-prefixed
/// controller on this host is gone. The CLI prints the report and exits
/// nonzero when any volume refused. An earlier volume that converted stays
/// converted when a later one refuses.
pub fn claimConvert(gpa: Allocator, io: Io, env: *const std.process.Environ.Map, meta_uri: []const u8, opts: ConvertOptions) ConvertError!Report {
    var report: Report = .{ .gpa = gpa, .volumes = &.{}, .paths = undefined, .uri_copy = try gpa.dupe(u8, meta_uri) };
    errdefer if (report.uri_copy.len != 0) gpa.free(report.uri_copy);
    var diag: uri.Refusal = undefined;
    const parsed = uri.parse(.meta, report.uri_copy, &report.paths, &diag) catch return error.InvalidUri;
    try disconnectFencers(gpa);
    const vols = try gpa.alloc(VolumeReport, parsed.paths.len);
    errdefer if (vols.len != 0) gpa.free(vols);
    var owned: ?*uring_fs.Pool = null;
    const pool = if (opts.pool) |p| p else blk: {
        const created = uring_fs.Pool.init(gpa, uring_fs.Config.resolve(env, derive.Facts.probe())) catch return error.ExecutorInit;
        owned = created;
        break :blk created;
    };
    defer if (owned) |p| p.deinit();
    const facts = derive.facts();
    const sqz = exec.SqzIo.init(gpa, .{ .env = env, .facts = facts, .budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = io, .env = env, .facts = facts }), .meta_lanes = 1 }) catch return error.ExecutorInit;
    defer sqz.deinit();
    const Run = struct {
        fn main(s: *exec.SqzIo, g: Allocator, e: *const std.process.Environ.Map, paths: []const []const u8, pool_: *uring_fs.Pool, out: []VolumeReport) void {
            for (paths, 0..) |p, i| {
                out[i] = .{ .path = p, .outcome = convertOne(g, s.io(), e, pool_, p, @intCast(i)) };
            }
        }
    };
    sqz.run(Run.main, .{ sqz, gpa, env, parsed.paths, pool, vols }) catch return error.ExecutorInit;
    report.volumes = vols;
    return report;
}

fn disconnectFencers(gpa: Allocator) error{ OutOfMemory, FencerDisconnect }!void {
    const ctrls = initiator.listControllers(gpa, initiator.SYSFS_ROOT) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        else => {
            log.err("claim convert-pr: the host's NVMe controllers could not be listed ({s}) — refusing before any volume (decision 0004 §6.4)", .{@errorName(e)});
            return error.FencerDisconnect;
        },
    };
    defer if (ctrls.len != 0) gpa.free(ctrls);
    for (ctrls) |c| {
        if (!std.mem.startsWith(u8, c.hostnqnOf(), align_probe.FENCER_HOSTNQN_PREFIX)) continue;
        initiator.deleteController(c.instance) catch |e| {
            log.err("claim convert-pr: fencer controller nvme{d} (hostnqn {s}) could not be disconnected ({s}) — refusing before any volume (decision 0004 §6.4)", .{ c.instance, c.hostnqnOf(), @errorName(e) });
            return error.FencerDisconnect;
        };
    }
}

fn convertOne(gpa: Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, vol_index: u32) Outcome {
    const fd = openConvertFd(path) orelse return .open_failed;
    defer _ = linux.close(fd);
    var locker: ?proc.Holder = null;
    switch (proc.flockExclusive(io, fd, .{}, &locker)) {
        .SUCCESS => {},
        .AGAIN => return .live_mounted,
        else => |e| {
            log.err("meta volume {s}: the writer lock could not be taken ({t}) — refusing convert-pr (decision 0004 §6.4)", .{ path, e });
            return .open_failed;
        },
    }
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return .open_failed;
    const loaded = loadSuperblock(pool, path) catch |e| {
        log.err("meta volume {s}: convert-pr could not read the superblock ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(e) });
        return .open_failed;
    };
    if (!loaded.sb.symmetricPlaneStamped()) {
        log.info("meta volume {s}: single-writer volume — convert-pr changes nothing (decision 0004 §6.4)", .{path});
        return .already;
    }
    const local = proc.localIdentity();
    if (freshClaim(gpa, io, env, pool, path, local)) |o| return o;
    if (refuseSiblings(gpa, env, path, loaded.sb.set_uuid)) |o| return o;

    var early: ?*host_fence.Early = null;
    defer if (early) |e| e.discard(gpa);
    var pages = RegPages{
        .dev = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 },
        .sb = loaded.sb,
    };
    early = host_fence.registerBeforeWrites(gpa, env, path, loaded.sb.set_uuid, vol_index, local, join.nodeToken(local), .{ .ctx = &pages, .read = RegPages.read }) catch |e| switch (e) {
        error.Busy => return .live_sibling,
        error.OutOfMemory => {
            log.err("meta volume {s}: convert-pr ran out of memory registering this host — refusing (decision 0004 §6.4)", .{path});
            return .open_failed;
        },
    };
    if (freshClaim(gpa, io, env, pool, path, local)) |o| return o;

    const wrote1 = stampBoth(pool, path, loaded.len) catch |err| {
        log.err("meta volume {s}: convert-pr could not stamp META_PR_ALL_REGISTRANTS ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(err) });
        return .open_failed;
    };
    if (early) |e| switch (e.posture) {
        .active => |client| switch (convertReservation(client, path)) {
            .withdraw => return .open_failed,
            .keep => {
                e.keep_hold = true;
                return .open_failed;
            },
            .ok => |o| {
                e.hold = o;
                e.keep_hold = o.kept();
            },
        },
        .none => {},
    };
    const wrote2 = stampBoth(pool, path, loaded.len) catch |err| {
        log.err("meta volume {s}: convert-pr could not stamp META_PR_ALL_REGISTRANTS ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(err) });
        if (early) |e| e.keep_hold = true;
        return .open_failed;
    };
    const page_wrote = rewritePages(gpa, pool, path, loaded.sb) catch |err| {
        if (early) |e| e.keep_hold = true;
        if (err == error.Foreign) {
            log.err("meta volume {s}: a directory or appender-0 page has a foreign layout — META_PR_ALL_REGISTRANTS stays stamped and the reservation stands; refusing the rest of convert-pr (decision 0004 §6.4)", .{path});
            return .foreign_page;
        }
        log.err("meta volume {s}: convert-pr could not rewrite pages ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(err) });
        return .open_failed;
    };
    if (wrote1 or wrote2 or page_wrote) {
        log.info("meta volume {s}: converted to META_PR_ALL_REGISTRANTS (decision 0004 §6.4)", .{path});
        return .converted;
    }
    log.info("meta volume {s}: already META_PR_ALL_REGISTRANTS — nothing to convert (decision 0004 §6.4)", .{path});
    return .already;
}

fn openConvertFd(path: []const u8) ?linux.fd_t {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = align_probe.pathZ(&zbuf, path) catch {
        log.err("meta volume {s}: the path does not fit a writer-lock open — refusing convert-pr (decision 0004 §6.4)", .{path});
        return null;
    };
    const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) {
        log.err("meta volume {s}: cannot open for convert-pr ({t})", .{ path, linux.errno(rc) });
        return null;
    }
    return @intCast(rc);
}

/// A returned claim is fresh (the helper does not log that). Unreadable is
/// already logged and becomes `.open_failed`.
fn freshClaim(gpa: Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, local: @import("claim.zig").Identity) ?Outcome {
    const found = gate.Guard.freshClaimEarly(gpa, io, env, pool, path, local, proc.probe) catch return .open_failed;
    if (found == null) return null;
    log.err("meta volume {s}: refusing convert-pr over a fresh writer claim (decision 0004 §6.4)", .{path});
    return .fresh_claim;
}

/// Null = no live sibling (a file has no wire host id and no registry).
/// A host id or a registry that cannot be read fails closed.
fn refuseSiblings(gpa: Allocator, env: *const std.process.Environ.Map, path: []const u8, set_uuid: [16]u8) ?Outcome {
    switch (pr_linux.resolve(gpa, path)) {
        .none => return null,
        .device => |dev| {
            const c = gpa.create(pr.Client) catch {
                dev.deinit();
                log.err("meta volume {s}: convert-pr ran out of memory reading the host identifier — refusing (decision 0004 §6.4)", .{path});
                return .open_failed;
            };
            c.* = pr.Client.init(gpa, dev.face(), dev.nsid, 0) catch {
                gpa.destroy(c);
                dev.deinit();
                log.err("meta volume {s}: convert-pr ran out of memory reading the host identifier — refusing (decision 0004 §6.4)", .{path});
                return .open_failed;
            };
            defer {
                c.deinit();
                gpa.destroy(c);
                dev.deinit();
            }
            const host = c.wireHostId() catch {
                log.err("meta volume {s}: the host identifier could not be read — refusing convert-pr closed (decision 0004 §6.4)", .{path});
                return .live_sibling;
            };
            var reg = host_fence.Registry.runtime(env, .set, host_fence.setRegistryKey(&host, set_uuid));
            const entries = reg.entries(gpa) catch |e| {
                log.err("meta volume {s}: the host registry of this set could not be read ({s}) — refusing convert-pr closed (decision 0004 §6.4)", .{ path, @errorName(e) });
                return .live_sibling;
            };
            defer if (entries.len != 0) gpa.free(entries);
            if (refuseLiveSiblings(entries, proc.localIdentity().pid)) {
                log.err("meta volume {s}: refusing convert-pr — a live process of this set is registered on this host (decision 0004 §6.4)", .{path});
                return .live_sibling;
            }
            return null;
        },
    }
}

fn refuseLiveSiblings(entries: []const host_fence.Entry, my_pid: u32) bool {
    for (entries) |e| {
        if (e.pid == my_pid) continue;
        return true;
    }
    return false;
}

const RegPages = struct {
    dev: device_loader.DeviceLoader,
    sb: superblock.Superblock,

    fn read(ctx: *anyopaque, gpa: Allocator) error{ Io, Corrupt, OutOfMemory }![]appender_page.Page {
        const self: *RegPages = @ptrCast(@alignCast(ctx));
        return joined.JoinedSet.devicePagesOn(gpa, &self.dev, self.sb);
    }
};

const Loaded = struct { sb: superblock.Superblock, len: u64 };

fn loadSuperblock(pool: *uring_fs.Pool, path: []const u8) error{ Io, Corrupt, OutOfMemory }!Loaded {
    const len = device_loader.volumeLength(path) catch return error.Io;
    var primary: [superblock.sector_len]u8 = @splat(0);
    try readInto(pool, path, 0, &primary);
    var tail: [superblock.sector_len]u8 = @splat(0);
    var backup: ?[]const u8 = null;
    if (superblock.backupOffsetFor(len)) |off| {
        try readInto(pool, path, off, &tail);
        backup = &tail;
    }
    const classified = superblock.classifyVolumeImages(&primary, backup, len, null) catch return error.Corrupt;
    return switch (classified.format) {
        .v5 => |sb| .{ .sb = sb, .len = len },
        else => error.Corrupt,
    };
}

/// True when a copy was rewritten. A valid v5 copy that lacks the bit is
/// stamped onto the newest valid v5 image and both copies are written at
/// the next generation, so an older copy cannot outlive the bit. Every
/// valid v5 copy already carrying it is left untouched.
fn stampBoth(pool: *uring_fs.Pool, path: []const u8, volume_len: u64) error{ Io, Corrupt, OutOfMemory }!bool {
    var primary: [superblock.sector_len]u8 = @splat(0);
    var backup_img: [superblock.sector_len]u8 = @splat(0);
    try readInto(pool, path, 0, &primary);
    const backup_off = superblock.backupOffsetFor(volume_len) orelse return error.Corrupt;
    try readInto(pool, path, backup_off, &backup_img);
    const p = v5Copy(&primary);
    const b = v5Copy(&backup_img);
    if (p == null and b == null) return error.Corrupt;
    var lacks = false;
    if (p) |sb| {
        if (!sb.metaPrAllRegistrantsStamped()) lacks = true;
    }
    if (b) |sb| {
        if (!sb.metaPrAllRegistrantsStamped()) lacks = true;
    }
    if (!lacks) return false;
    var sb = if (p) |ps| blk: {
        if (b) |bs| {
            break :blk if (superblock.sectorGeneration(&primary) >= superblock.sectorGeneration(&backup_img)) ps else bs;
        }
        break :blk ps;
    } else b orelse return error.Corrupt;
    _ = sb.stampBit(superblock.meta_pr_all_registrants);
    const plan = superblock.planWrite(sb, superblock.nextSuperblockGeneration(&primary, &backup_img), null) catch return error.Corrupt;
    for (plan.slice()) |step| {
        pool.writeAt(path, step.offset, plan.bytes()) catch |e| return mapIo(e);
        if (step.barrier_after) pool.fdatasync(path) catch |e| return mapIo(e);
    }
    return true;
}

fn v5Copy(sector: []const u8) ?superblock.Superblock {
    const fmt = superblock.classifySector0(sector, null) catch return null;
    return switch (fmt) {
        .v5 => |sb| sb,
        else => null,
    };
}

const Reservation = union(enum) {
    ok: host_fence.HoldOutcome,
    /// Nothing on the device changed. The caller drops the registration.
    withdraw,
    /// A reservation command already landed. The caller keeps the hold.
    keep,
};

/// Retype to Write Exclusive — All Registrants, then preempt-all.
/// `client.rtype` stays Write Exclusive until the confirming report, so a
/// failure cannot `release` a type the namespace does not hold.
fn convertReservation(c: *pr.Client, path: []const u8) Reservation {
    var r0 = c.report() catch |e| {
        log.err("meta volume {s}: the reservation report failed before convert-pr ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(e) });
        return .withdraw;
    };
    defer r0.deinit(c.gpa);
    const me = c.wireHostId() catch |e| {
        log.err("meta volume {s}: the host identifier could not be read before convert-pr ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(e) });
        return .withdraw;
    };
    const rtype0 = r0.rtype;
    const others = otherHosts(r0, me);
    const gen0 = r0.generation;
    var mutated = false;
    if (rtype0 == nvme.RTYPE_WEAR) {
        // Already the type. preempt-all below is the idempotent cleanup.
    } else if (rtype0 == 0) {
        c.acquireTyped(nvme.RTYPE_WEAR) catch |e| {
            log.err("meta volume {s}: convert-pr could not acquire Write Exclusive — All Registrants ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(e) });
            return .withdraw;
        };
        mutated = true;
    } else if (r0.heldByHost(c.key, me)) {
        c.preemptTyped(c.key, nvme.RTYPE_WEAR) catch |e| {
            log.err("meta volume {s}: convert-pr could not retype this host's hold ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(e) });
            return .withdraw;
        };
        mutated = true;
    } else if (r0.holder()) |hk| {
        c.preemptTyped(hk, nvme.RTYPE_WEAR) catch |e| {
            log.err("meta volume {s}: convert-pr could not preempt the holder ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(e) });
            return .withdraw;
        };
        mutated = true;
    } else {
        log.err("{s}: metadata reservation type {d} has no holder key — refusing convert-pr before any reservation command (decision 0004 §6.4)", .{ path, rtype0 });
        return .withdraw;
    }
    c.preemptAllAndAbort() catch |e| {
        log.err("meta volume {s}: the preempt-all failed during convert-pr ({s}) — refusing (decision 0004 §6.4)", .{ path, @errorName(e) });
        return if (mutated) .keep else .withdraw;
    };
    var r1 = c.report() catch |e| {
        log.err("meta volume {s}: the reservation report failed after convert-pr's preempt-all ({s}) — the hold stays (decision 0004 §6.4)", .{ path, @errorName(e) });
        return .keep;
    };
    defer r1.deinit(c.gpa);
    if (r1.rtype != nvme.RTYPE_WEAR or !r1.heldByHost(c.key, me)) {
        log.err("meta volume {s}: convert-pr did not leave this host holding Write Exclusive — All Registrants — the hold stays (decision 0004 §6.4)", .{path});
        return .keep;
    }
    c.rtype = nvme.RTYPE_WEAR;
    guard.stats.preempt_alls.inc();
    guard.stats.preempt_all_victims.add(others);
    const gap = r1.generation -% gen0;
    if (rtype0 == 0 and others == 0 and gap == 1) return .{ .ok = .acquired };
    return .{ .ok = .preempted };
}

fn otherHosts(rep: nvme.Report, me: [16]u8) u64 {
    var n: u64 = 0;
    for (rep.registrants, 0..) |r, i| {
        if (std.mem.eql(u8, &r.host_id, &me)) continue;
        var earlier = false;
        for (rep.registrants[0..i]) |prev| {
            if (std.mem.eql(u8, &prev.host_id, &r.host_id)) {
                earlier = true;
                break;
            }
        }
        if (!earlier) n += 1;
    }
    return n;
}

const RewriteError = error{ Io, Corrupt, OutOfMemory, Foreign };

fn rewritePages(gpa: Allocator, pool: *uring_fs.Pool, path: []const u8, sb: superblock.Superblock) RewriteError!bool {
    var reader_state = PoolPages{ .pool = pool, .path = path };
    const reader: appender_dir.PageReader = .{ .ctx = &reader_state, .read = PoolPages.read };
    var links: std.ArrayList(appender_dir.Link) = .empty;
    defer links.deinit(gpa);
    try appender_dir.walkChain(gpa, reader, .{ .start = sb.appender_dir.start, .len = sb.appender_dir.len }, sb.node_size, .{ .start = sb.heap.start, .len = sb.heap.len }, sb.totalExtents(), &links);
    var wrote = false;
    var buf: [appender_page.PAGE_LEN]u8 = undefined;
    for (links.items) |link| {
        var pair: u32 = 0;
        while (pair < link.header.pairs) : (pair += 1) {
            for (appender_dir.pairOffsets(link.extent, pair)) |off| {
                switch (try rewriteAt(pool, path, off, &buf)) {
                    .same => {},
                    .wrote => wrote = true,
                    .foreign => {
                        if (wrote) barrierRewritten(pool, path);
                        return error.Foreign;
                    },
                }
            }
        }
    }
    for (appender_page.appender0PageOffsets(sb.journal.start)) |off| {
        switch (try rewriteAt(pool, path, off, &buf)) {
            .same => {},
            .wrote => wrote = true,
            .foreign => {
                if (wrote) barrierRewritten(pool, path);
                return error.Foreign;
            },
        }
    }
    if (wrote) pool.fdatasync(path) catch |e| return mapIo(e);
    return wrote;
}

const SlotRewrite = enum { same, wrote, foreign };

fn rewriteAt(pool: *uring_fs.Pool, path: []const u8, offset: u64, buf: *[appender_page.PAGE_LEN]u8) error{ Io, OutOfMemory }!SlotRewrite {
    try readInto(pool, path, offset, buf);
    switch (appender_page.classify(buf)) {
        .blank, .corrupt => return .same,
        .foreign_layout => return .foreign,
        .valid => |decoded| {
            const ver = std.mem.readInt(u16, buf[4..][0..2], .little);
            if (ver == appender_page.LAYOUT_VERSION) return .same;
            if (ver != appender_page.LAYOUT_VERSION_V1) return .foreign;
            var page = decoded;
            page.host_key = 0;
            var out: [appender_page.PAGE_LEN]u8 = undefined;
            page.encode(&out) catch return error.Io;
            pool.writeAt(path, offset, &out) catch |e| return mapIo(e);
            return .wrote;
        },
    }
}

fn barrierRewritten(pool: *uring_fs.Pool, path: []const u8) void {
    pool.fdatasync(path) catch {
        log.err("meta volume {s}: convert-pr could not barrier pages already rewritten before a foreign layout — the foreign page still refuses the verb (decision 0004 §6.4)", .{path});
    };
}

const PoolPages = struct {
    pool: *uring_fs.Pool,
    path: []const u8,

    fn read(ctx: *anyopaque, offset: u64, out: *[appender_dir.PAGE_LEN]u8) error{Io}!void {
        const self: *PoolPages = @ptrCast(@alignCast(ctx));
        readInto(self.pool, self.path, offset, out) catch return error.Io;
    }
};

fn readInto(pool: *uring_fs.Pool, path: []const u8, offset: u64, dest: []u8) error{ Io, OutOfMemory }!void {
    @memset(dest, 0);
    var got = pool.readAt(path, offset, dest.len) catch |e| return mapIo(e);
    defer got.deinit();
    const bytes = got.bytes();
    const n = @min(bytes.len, dest.len);
    @memcpy(dest[0..n], bytes[0..n]);
}

fn mapIo(e: uring_fs.Error) error{ Io, OutOfMemory } {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => error.Io,
    };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const geometry = @import("../geometry.zig");
const fake = @import("fake_nvme.zig");

const test_dir = "/build/tmp";
const test_volume_len: u64 = 64 << 20;

const Vol = struct {
    gpa: Allocator,
    env: std.process.Environ.Map,
    sqz: *exec.SqzIo,
    pool: *uring_fs.Pool,
    path_buf: [160]u8 = undefined,
    path_len: usize = 0,

    fn path(self: *const Vol) []const u8 {
        return self.path_buf[0..self.path_len];
    }

    fn create(gpa: Allocator, name: []const u8, class: geometry.Class, without_meta_pr: bool) !*Vol {
        const self = try gpa.create(Vol);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .env = std.process.Environ.Map.init(gpa), .sqz = undefined, .pool = undefined };
        errdefer self.env.deinit();
        if (without_meta_pr) try self.env.put("SQUEEZEFS_TEST_FORMAT_WITHOUT_META_WEAR", "1");
        self.sqz = try exec.SqzIo.testInstance();
        errdefer self.sqz.deinit();
        self.pool = try uring_fs.Pool.init(gpa, uring_fs.Config.resolve(&self.env, derive.Facts.probe()));
        errdefer self.pool.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, test_dir), 0o755);
        self.path_len = (try std.fmt.bufPrint(&self.path_buf, "{s}/sqz-convert-{d}-{s}.img", .{ test_dir, linux.getpid(), name })).len;
        const z = try align_probe.pathZ(&zbuf, self.path());
        const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
        if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        if (linux.errno(linux.ftruncate(fd, @intCast(test_volume_len))) != .SUCCESS) return error.CannotSizeDevice;
        try backend.formatVolume(gpa, self.path(), .{ .env = &self.env, .pool = self.pool, .node_size = 65536, .journal_len_override = 1 << 20, .uuid = "convert-vol-uuid".*, .set = .{ .set_uuid = "convert-set-uuid".*, .hash_seed = 0x6A6A_6A6A_1234_5678 }, .class = class });
        return self;
    }

    fn destroy(self: *Vol) void {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        if (align_probe.pathZ(&zbuf, self.path())) |z| {
            _ = linux.unlink(z);
        } else |_| {}
        self.pool.deinit();
        self.sqz.deinit();
        self.env.deinit();
        self.gpa.destroy(self);
    }

    fn openOpts(self: *Vol, g: gate.Options, lane_error_logs: usize) backend.OpenOptions {
        return .{ .env = &self.env, .pool = self.pool, .cache_budget_bytes = 64 << 20, .cpus = 4, .guard = g, .test_error_logs = lane_error_logs };
    }
};

fn metaUri(buf: []u8, path: []const u8) ![]const u8 {
    return std.fmt.bufPrint(buf, "sqmeta://{s}", .{path});
}

fn newestGen(pool: *uring_fs.Pool, path: []const u8) !u64 {
    const len = try device_loader.volumeLength(path);
    var primary: [superblock.sector_len]u8 = @splat(0);
    try readInto(pool, path, 0, &primary);
    var best: u64 = if (v5Copy(&primary) != null) superblock.sectorGeneration(&primary) else 0;
    if (superblock.backupOffsetFor(len)) |off| {
        var backup: [superblock.sector_len]u8 = @splat(0);
        try readInto(pool, path, off, &backup);
        if (v5Copy(&backup) != null) best = @max(best, superblock.sectorGeneration(&backup));
    }
    return best;
}

fn metaBit(pool: *uring_fs.Pool, path: []const u8) !bool {
    var primary: [superblock.sector_len]u8 = @splat(0);
    try readInto(pool, path, 0, &primary);
    const sb = v5Copy(&primary) orelse return error.Corrupt;
    return sb.metaPrAllRegistrantsStamped();
}

fn featureWord(pool: *uring_fs.Pool, path: []const u8) !u64 {
    var primary: [superblock.sector_len]u8 = @splat(0);
    try readInto(pool, path, 0, &primary);
    const sb = v5Copy(&primary) orelse return error.Corrupt;
    return sb.features_incompat;
}

fn loadSb(pool: *uring_fs.Pool, path: []const u8) !superblock.Superblock {
    return (try loadSuperblock(pool, path)).sb;
}

fn dirSlotA(sb: superblock.Superblock) u64 {
    const dir: appender_dir.Extent = .{ .start = sb.appender_dir.start, .len = sb.appender_dir.len };
    return appender_dir.pairOffsets(dir, 0)[0];
}

fn plantLayout1(pool: *uring_fs.Pool, path: []const u8, offset: u64, page: appender_page.Page) !void {
    var v2: [appender_page.PAGE_LEN]u8 = undefined;
    try page.encode(&v2);
    var v1: [appender_page.PAGE_LEN]u8 = @splat(0);
    @memcpy(v1[0..appender_page.HOST_KEY_OFF], v2[0..appender_page.HOST_KEY_OFF]);
    std.mem.writeInt(u16, v1[4..][0..2], appender_page.LAYOUT_VERSION_V1, .little);
    const n = @as(usize, page.n_slots) * appender_page.SLOT_ENTRY_LEN;
    if (n != 0) @memcpy(v1[appender_page.HOST_KEY_OFF..][0..n], v2[appender_page.FIXED_LEN..][0..n]);
    std.mem.writeInt(u64, v1[appender_page.CHECKSUM_OFF..][0..8], checksum.hash64(v1[0..appender_page.CHECKSUM_OFF]), .little);
    try pool.writeAt(path, offset, &v1);
}

fn readPage(pool: *uring_fs.Pool, path: []const u8, offset: u64) !appender_page.Page {
    var buf: [appender_page.PAGE_LEN]u8 = undefined;
    try readInto(pool, path, offset, &buf);
    return switch (appender_page.classify(&buf)) {
        .valid => |p| p,
        else => error.Corrupt,
    };
}

fn expectLayout2(pool: *uring_fs.Pool, path: []const u8, offset: u64, appender_id: u32) !void {
    var buf: [appender_page.PAGE_LEN]u8 = undefined;
    try readInto(pool, path, offset, &buf);
    try testing.expectEqual(appender_page.LAYOUT_VERSION, std.mem.readInt(u16, buf[4..][0..2], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, buf[appender_page.HOST_KEY_OFF..][0..8], .little));
    const page = try readPage(pool, path, offset);
    try testing.expectEqual(appender_id, page.appender_id);
}

/// Clear bit 4 on both copies at the generation they already carry.
fn stripMetaBit(pool: *uring_fs.Pool, path: []const u8) !void {
    const len = try device_loader.volumeLength(path);
    var primary: [superblock.sector_len]u8 = @splat(0);
    try readInto(pool, path, 0, &primary);
    var sb = v5Copy(&primary) orelse return error.Corrupt;
    sb.features_incompat &= ~superblock.meta_pr_all_registrants;
    var img: [superblock.sector_len]u8 = undefined;
    try sb.encodeSector(superblock.sectorGeneration(&primary), &img, null);
    const off = superblock.backupOffsetFor(len) orelse return error.Corrupt;
    try pool.writeAt(path, off, &img);
    try pool.fdatasync(path);
    try pool.writeAt(path, 0, &img);
    try pool.fdatasync(path);
}

fn plantFreshClaim(vol: *Vol) !void {
    const be = try backend.KvBackend.open(vol.gpa, vol.sqz.io(), vol.path(), vol.openOpts(.{ .test_identity = .{ .boot_id = "boot-foreign-01!".*, .pid = 901, .start_ticks = 9, .hostname_hash = 0x1234 }, .test_pid_probe = proc.probe }, 0));
    be.guard.?.claimed.store(false, .release);
    be.close();
}

test "convert-pr report lines name each refusal and the two successes" {
    const samples = [_]struct { o: Outcome, needle: []const u8, no: bool }{
        .{ .o = .converted, .needle = "converted to META_PR_ALL_REGISTRANTS (decision 0004 §6.4)", .no = false },
        .{ .o = .already, .needle = "already META_PR_ALL_REGISTRANTS — nothing to convert", .no = false },
        .{ .o = .live_mounted, .needle = "live-mounted on this host", .no = true },
        .{ .o = .fresh_claim, .needle = "fresh writer claim", .no = true },
        .{ .o = .live_sibling, .needle = "live sibling", .no = true },
        .{ .o = .foreign_page, .needle = "foreign layout", .no = true },
        .{ .o = .open_failed, .needle = "convert-pr failed", .no = true },
    };
    for (samples) |s| {
        const v = VolumeReport{ .path = "/dev/nvme1n1", .outcome = s.o };
        try testing.expectEqual(s.no, v.refused());
        var buf: [512]u8 = undefined;
        var w: std.Io.Writer = .fixed(&buf);
        try v.write(&w);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), s.needle) != null);
    }
}

test "convert-pr's sibling refusal skips this process and refuses any other" {
    try testing.expect(!refuseLiveSiblings(&.{}, 7));
    const me = host_fence.Entry{ .pid = 7, .start_ticks = 1 };
    try testing.expect(!refuseLiveSiblings(&.{me}, 7));
    const other = host_fence.Entry{ .pid = 8, .start_ticks = 2 };
    try testing.expect(refuseLiveSiblings(&.{ me, other }, 7));
    try testing.expect(refuseLiveSiblings(&.{other}, 7));
}

test "convert-pr refuses an unconverted writer, lets a reader and claim clear through, and refuses open in convert-pr mode" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "door", .symmetric, true);
    defer v.destroy();
    try testing.expect(!try metaBit(v.pool, v.path()));
    try testing.expectEqual(@as(u64, 1), try newestGen(v.pool, v.path()));

    log.testing.expectErrorLogs(1);
    try testing.expectError(error.UnsupportedFormat, backend.KvBackend.open(gpa, v.sqz.io(), v.path(), v.openOpts(.{}, 0)));
    try log.testing.expectAllFired();

    const rd = try backend.KvBackend.openReader(gpa, v.sqz.io(), v.path(), v.openOpts(.{}, 0));
    rd.close();

    var uri_buf: [256]u8 = undefined;
    const meta = try metaUri(&uri_buf, v.path());
    var cleared = try gate.claimClear(gpa, testing.io, &v.env, meta, .{ .pool = v.pool });
    defer cleared.deinit();
    try testing.expectEqual(gate.ClearOutcome.no_claim, cleared.volumes[0].outcome.cleared);

    log.testing.expectErrorLogs(1);
    try testing.expectError(error.UnsupportedFormat, backend.KvBackend.open(gpa, v.sqz.io(), v.path(), v.openOpts(.{ .mode = .convert_pr }, 0)));
    try log.testing.expectAllFired();
}

test "convert-pr rewrites a planted layout-1 page, leaves a corrupt slot, and a second run is already at the same generation" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "v1", .symmetric, true);
    defer v.destroy();
    const sb = try loadSb(v.pool, v.path());
    const slot_a = dirSlotA(sb);
    const a0 = appender_page.appender0PageOffsets(sb.journal.start);
    const free_page = try readPage(v.pool, v.path(), a0[1]);
    try testing.expect(free_page.segments.len > 0);
    try plantLayout1(v.pool, v.path(), slot_a, .{ .appender_id = 1, .generation = 1 });
    try plantLayout1(v.pool, v.path(), a0[1], free_page);
    var junk: [appender_page.PAGE_LEN]u8 = @splat(0x5A);
    try v.pool.writeAt(v.path(), a0[0], &junk);
    try v.pool.fdatasync(v.path());

    var uri_buf: [256]u8 = undefined;
    const meta = try metaUri(&uri_buf, v.path());
    var first = try claimConvert(gpa, testing.io, &v.env, meta, .{ .pool = v.pool });
    defer first.deinit();
    try testing.expectEqual(Outcome.converted, first.volumes[0].outcome);
    try testing.expect(try metaBit(v.pool, v.path()));
    try testing.expectEqual(@as(u64, 2), try newestGen(v.pool, v.path()));
    try expectLayout2(v.pool, v.path(), slot_a, 1);
    try expectLayout2(v.pool, v.path(), a0[1], 0);
    const kept = try readPage(v.pool, v.path(), a0[1]);
    try testing.expect(free_page.segments.eql(&kept.segments));
    var still: [appender_page.PAGE_LEN]u8 = undefined;
    try readInto(v.pool, v.path(), a0[0], &still);
    try testing.expect(std.mem.allEqual(u8, &still, 0x5A));

    var second = try claimConvert(gpa, testing.io, &v.env, meta, .{ .pool = v.pool });
    defer second.deinit();
    try testing.expectEqual(Outcome.already, second.volumes[0].outcome);
    try testing.expectEqual(@as(u64, 2), try newestGen(v.pool, v.path()));
}

test "convert-pr of a volume that already has bit 4 rewrites layout-1 pages without bumping the superblock generation" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "stay", .symmetric, false);
    defer v.destroy();
    try testing.expect(try metaBit(v.pool, v.path()));
    try testing.expectEqual(@as(u64, 1), try newestGen(v.pool, v.path()));
    const sb = try loadSb(v.pool, v.path());
    const slot_a = dirSlotA(sb);
    const a0 = appender_page.appender0PageOffsets(sb.journal.start);
    try plantLayout1(v.pool, v.path(), slot_a, .{ .appender_id = 1, .generation = 1 });
    try plantLayout1(v.pool, v.path(), a0[1], try readPage(v.pool, v.path(), a0[1]));
    try v.pool.fdatasync(v.path());

    var uri_buf: [256]u8 = undefined;
    var report = try claimConvert(gpa, testing.io, &v.env, try metaUri(&uri_buf, v.path()), .{ .pool = v.pool });
    defer report.deinit();
    try testing.expectEqual(Outcome.converted, report.volumes[0].outcome);
    try testing.expectEqual(@as(u64, 1), try newestGen(v.pool, v.path()));
    try expectLayout2(v.pool, v.path(), slot_a, 1);
    try expectLayout2(v.pool, v.path(), a0[1], 0);
}

test "convert-pr refuses a fresh claim and leaves bit 4 unset at generation 1" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "fresh", .symmetric, false);
    defer v.destroy();
    try plantFreshClaim(v);
    try testing.expectEqual(@as(u64, 1), try newestGen(v.pool, v.path()));
    try stripMetaBit(v.pool, v.path());
    try testing.expect(!try metaBit(v.pool, v.path()));

    var uri_buf: [256]u8 = undefined;
    log.testing.expectErrorLogs(1);
    var report = try claimConvert(gpa, testing.io, &v.env, try metaUri(&uri_buf, v.path()), .{ .pool = v.pool });
    defer report.deinit();
    try log.testing.expectAllFired();
    try testing.expectEqual(Outcome.fresh_claim, report.volumes[0].outcome);
    try testing.expect(!try metaBit(v.pool, v.path()));
    try testing.expectEqual(@as(u64, 1), try newestGen(v.pool, v.path()));
}

test "convert-pr keeps an earlier volume converted when a later volume refuses" {
    const gpa = testing.allocator;
    const a = try Vol.create(gpa, "pair-a", .symmetric, true);
    defer a.destroy();
    const b = try Vol.create(gpa, "pair-b", .symmetric, false);
    defer b.destroy();
    try plantFreshClaim(b);
    try stripMetaBit(b.pool, b.path());

    var uri_buf: [512]u8 = undefined;
    const meta = try std.fmt.bufPrint(&uri_buf, "sqmeta://{s},{s}", .{ a.path(), b.path() });
    log.testing.expectErrorLogs(1);
    var report = try claimConvert(gpa, testing.io, &a.env, meta, .{ .pool = a.pool });
    defer report.deinit();
    try log.testing.expectAllFired();
    try testing.expectEqual(Outcome.converted, report.volumes[0].outcome);
    try testing.expectEqual(Outcome.fresh_claim, report.volumes[1].outcome);
    try testing.expect(try metaBit(a.pool, a.path()));
    try testing.expect(!try metaBit(a.pool, b.path()));
}

test "convert-pr on a single-writer volume is a no-op, a bad URI is refused, and a live mount is refused without an error log" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "flat", .single_writer, false);
    defer v.destroy();
    try testing.expectEqual(@as(u64, 0), try featureWord(v.pool, v.path()));
    try testing.expectEqual(@as(u64, 1), try newestGen(v.pool, v.path()));

    var uri_buf: [256]u8 = undefined;
    const meta = try metaUri(&uri_buf, v.path());
    var first = try claimConvert(gpa, testing.io, &v.env, meta, .{ .pool = v.pool });
    defer first.deinit();
    try testing.expectEqual(Outcome.already, first.volumes[0].outcome);
    try testing.expectEqual(@as(u64, 0), try featureWord(v.pool, v.path()));
    try testing.expectEqual(@as(u64, 1), try newestGen(v.pool, v.path()));

    try testing.expectError(error.InvalidUri, claimConvert(gpa, testing.io, &v.env, "sqdata:///x", .{}));

    const be = try backend.KvBackend.open(gpa, v.sqz.io(), v.path(), v.openOpts(.{}, 0));
    defer be.close();
    var live = try claimConvert(gpa, testing.io, &v.env, meta, .{ .pool = v.pool });
    defer live.deinit();
    try testing.expectEqual(Outcome.live_mounted, live.volumes[0].outcome);
    try testing.expectEqual(@as(u64, 0), try featureWord(v.pool, v.path()));
    try testing.expectEqual(@as(u64, 1), try newestGen(v.pool, v.path()));
}

test "convert-pr refuses a foreign page layout and keeps the stamped bit" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "foreign", .symmetric, true);
    defer v.destroy();
    const sb = try loadSb(v.pool, v.path());
    const slot_a = dirSlotA(sb);
    var foreign: [appender_page.PAGE_LEN]u8 = @splat(0);
    @memcpy(foreign[0..4], appender_page.MAGIC);
    std.mem.writeInt(u16, foreign[4..][0..2], 99, .little);
    std.mem.writeInt(u64, foreign[appender_page.CHECKSUM_OFF..][0..8], checksum.hash64(foreign[0..appender_page.CHECKSUM_OFF]), .little);
    try v.pool.writeAt(v.path(), slot_a, &foreign);
    try v.pool.fdatasync(v.path());

    var uri_buf: [256]u8 = undefined;
    log.testing.expectErrorLogs(1);
    var report = try claimConvert(gpa, testing.io, &v.env, try metaUri(&uri_buf, v.path()), .{ .pool = v.pool });
    defer report.deinit();
    try log.testing.expectAllFired();
    try testing.expectEqual(Outcome.foreign_page, report.volumes[0].outcome);
    try testing.expect(try metaBit(v.pool, v.path()));
    try testing.expectEqual(@as(u64, 2), try newestGen(v.pool, v.path()));
    var after: [appender_page.PAGE_LEN]u8 = undefined;
    try readInto(v.pool, v.path(), slot_a, &after);
    try testing.expectEqualSlices(u8, &foreign, &after);
}

fn registerOn(h: *fake.Host, key: u64) !void {
    var payload: [16]u8 = undefined;
    const cmd = nvme.reservationRegister(1, nvme.RREGA_REGISTER | nvme.IEKEY, 0, key, &payload);
    const c = try h.face().issue(h.face().ctx, .io, &cmd);
    try testing.expectEqual(nvme.STATUS_SUCCESS, c.status);
}

fn expectOk(res: Reservation, want: host_fence.HoldOutcome) !void {
    switch (res) {
        .ok => |o| try testing.expectEqual(want, o),
        else => return error.TestExpectedEqual,
    }
}

test "convert-pr reservation arms: unreserved acquires, a holder retypes, a victim key moves the hold, a holderless type withdraws, and an existing WEAR preempts" {
    const gpa = testing.allocator;
    const host_a: [16]u8 = "host-a-host-a-01".*;
    const host_b: [16]u8 = "host-b-host-b-02".*;
    const key_a: u64 = 0xA;
    const key_b: u64 = 0xB;

    // Unreserved: register only. The acquire does not move the generation;
    // preempt-all moves it by one, so the outcome is acquired.
    {
        var ns = fake.Namespace.init(gpa);
        defer ns.deinit();
        var host = fake.Host{ .ns = &ns, .host_id = host_a };
        try registerOn(&host, key_a);
        var client = try pr.Client.init(gpa, host.face(), 1, key_a);
        defer client.deinit();
        const gen = ns.generation;
        const preempts = ns.preempts;
        const alls = guard.stats.preempt_alls.load();
        const victims = guard.stats.preempt_all_victims.load();
        try expectOk(convertReservation(&client, "unreserved"), .acquired);
        try testing.expectEqual(nvme.RTYPE_WEAR, client.rtype);
        try testing.expect(ns.isHolder(host_a));
        try testing.expectEqual(gen + 1, ns.generation);
        try testing.expectEqual(preempts + 1, ns.preempts);
        try testing.expectEqual(alls + 1, guard.stats.preempt_alls.load());
        try testing.expectEqual(victims, guard.stats.preempt_all_victims.load());
    }

    // Holder self-preempt retypes and still bumps, then preempt-all bumps
    // again: gap 2, so the outcome is preempted.
    {
        var ns = fake.Namespace.init(gpa);
        defer ns.deinit();
        ns.holder_preempt_retypes = true;
        var host = fake.Host{ .ns = &ns, .host_id = host_a };
        try registerOn(&host, key_a);
        ns.holder = key_a;
        ns.holder_host = host_a;
        ns.rtype = nvme.RTYPE_WERO;
        var client = try pr.Client.init(gpa, host.face(), 1, key_a);
        defer client.deinit();
        const gen = ns.generation;
        const alls = guard.stats.preempt_alls.load();
        const victims = guard.stats.preempt_all_victims.load();
        try expectOk(convertReservation(&client, "retype"), .preempted);
        try testing.expectEqual(nvme.RTYPE_WEAR, client.rtype);
        try testing.expect(ns.isHolder(host_a));
        try testing.expectEqual(gen + 2, ns.generation);
        try testing.expectEqual(alls + 1, guard.stats.preempt_alls.load());
        try testing.expectEqual(victims, guard.stats.preempt_all_victims.load());
    }

    // Another host holds WERO. Preempt its key, then preempt-all.
    {
        var ns = fake.Namespace.init(gpa);
        defer ns.deinit();
        var a = fake.Host{ .ns = &ns, .host_id = host_a };
        var b = fake.Host{ .ns = &ns, .host_id = host_b };
        try registerOn(&a, key_a);
        try registerOn(&b, key_b);
        ns.holder = key_b;
        ns.holder_host = host_b;
        ns.rtype = nvme.RTYPE_WERO;
        var client = try pr.Client.init(gpa, a.face(), 1, key_a);
        defer client.deinit();
        const alls = guard.stats.preempt_alls.load();
        const victims = guard.stats.preempt_all_victims.load();
        try expectOk(convertReservation(&client, "victim"), .preempted);
        try testing.expectEqual(nvme.RTYPE_WEAR, client.rtype);
        try testing.expect(ns.isHolder(host_a));
        try testing.expect(ns.find(host_b) == null);
        try testing.expectEqual(alls + 1, guard.stats.preempt_alls.load());
        try testing.expectEqual(victims + 1, guard.stats.preempt_all_victims.load());
    }

    // A type with no holder key is refused before any reservation command.
    {
        var ns = fake.Namespace.init(gpa);
        defer ns.deinit();
        var host = fake.Host{ .ns = &ns, .host_id = host_a };
        try registerOn(&host, key_a);
        ns.rtype = 2;
        ns.holder = null;
        var client = try pr.Client.init(gpa, host.face(), 1, key_a);
        defer client.deinit();
        const gen = ns.generation;
        const preempts = ns.preempts;
        const alls = guard.stats.preempt_alls.load();
        log.testing.expectErrorLogs(1);
        switch (convertReservation(&client, "holderless")) {
            .withdraw => {},
            else => return error.TestExpectedEqual,
        }
        try log.testing.expectAllFired();
        try testing.expectEqual(nvme.RTYPE_WRITE_EXCLUSIVE, client.rtype);
        try testing.expectEqual(gen, ns.generation);
        try testing.expectEqual(preempts, ns.preempts);
        try testing.expectEqual(alls, guard.stats.preempt_alls.load());
    }

    // Already WEAR: no retype, preempt-all still runs. The holder must be
    // set or a prkey of 0 conflicts on an unreserved namespace.
    {
        var ns = fake.Namespace.init(gpa);
        defer ns.deinit();
        var host = fake.Host{ .ns = &ns, .host_id = host_a };
        try registerOn(&host, key_a);
        ns.holder = key_a;
        ns.holder_host = host_a;
        ns.rtype = nvme.RTYPE_WEAR;
        var client = try pr.Client.init(gpa, host.face(), 1, key_a);
        defer client.deinit();
        const gen = ns.generation;
        try expectOk(convertReservation(&client, "wear"), .preempted);
        try testing.expectEqual(nvme.RTYPE_WEAR, client.rtype);
        try testing.expect(ns.isHolder(host_a));
        try testing.expectEqual(gen + 1, ns.generation);
    }
}
