//! The OFFLINE set verbs (Linux; charter §5.4 "`add-meta`/`remove-meta` are
//! offline; set changes drain staging then rebind the generation stamp";
//! §8 `volume {add-meta, remove-meta, repair-set, list}`; `docs/porting/
//! dynamic_routing.md` §4): membership changes over a set NO daemon has
//! mounted — every member walks its D0 door (a live writer refuses, even
//! with `--force`), the slot trees move by the engine's tree copy, and the
//! stamps are written in the order that keeps the set self-describing at
//! every instant (target-first for a joiner's claim, survivors-first for a
//! retirement, the victim's tombstone LAST).
//!
//! `add-meta` takes `k` slots (or a named list) from the loaded members onto
//! the joiner, whose transient stamp is HOSTLESS until the copies are
//! durable (write 0 before the claim); `remove-meta` spreads the victim's
//! slots over the survivors with the most free extents after the §5.2
//! capacity preflight; `repair-set` re-stamps a coherent observed state
//! (the three stampless arms); `list` prints what every member's stamp
//! says. Member 0 — slot 0's host, the native keyspace, the `set_config`
//! home — is never retired and slot 0 never moves (`ledger.nativeSlotOf`).
//!
//! KD-8: a membership change changes the set epoch, never the set uuid, so
//! every member's staging root (0003 §T.1) is rebound in two phases: refuse
//! while any member is live or any staged custody is pending, write the
//! DUAL marker `{E, prev = E_old}` (the owner kept), flip, then `{E}`
//! (`staging.bindVerdict` reads a dual marker as same-epoch).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const checkpoint = @import("../checkpoint.zig");
const key = @import("../key.zig");
const ledger = @import("../ledger.zig");
const inos = @import("../inos.zig");
const backend = @import("../backend.zig");
const geometry = @import("../geometry.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const staging = @import("../../data/staging.zig");
const set_config = @import("../../data/set_config.zig");
const route_table = @import("route_table.zig");
const discovery = @import("discovery.zig");
const migrate = @import("migrate.zig");
const rstats = @import("stats.zig").stats;

comptime {
    if (builtin.os.tag != .linux) @compileError("routing/offline is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const KvBackend = backend.KvBackend;
const W = route_table.W;

pub const Error = error{
    /// Discovery refused (logged by name).
    SetRefused,
    /// A member is live-mounted (its D0 door is held).
    LiveMounted,
    AlreadyMember,
    NotMember,
    LastMember,
    MemberZero,
    SlotZero,
    SlotNotHosted,
    HostLeftEmpty,
    TakeZero,
    TooFewTakeable,
    StampBudget,
    CapacityRefused,
    StagingCustodyLive,
    StagingRefused,
    /// `repair-set`'s "not inferable" arms.
    NotInferable,
    Io,
    Corrupt,
    OutOfMemory,
};

pub const TakeSlots = union(enum) {
    count: u32,
    list: []const u16,
};

pub const Options = struct {
    env: *const std.process.Environ.Map,
    pool: *uring_fs.Pool,
    cache_budget_bytes: u64,
    cpus: u32,
    /// The set config's cache paths (the KD-8 rebind walks every member
    /// root under each); empty on a cache-less set.
    staging_roots: []const []const u8 = &.{},
};

/// One member as observed, with its decoded record owned.
const Member = struct {
    path: []const u8,
    owned: discovery.linux_dev.Owned,
};

/// The live-writer gate every verb runs on EVERY member before any
/// destructive step (a daemon's layer-A flock; `--force` never overrides
/// a live mount): a shared non-blocking flock answers EWOULDBLOCK under a
/// writer's exclusive one.
fn refuseLive(paths: []const []const u8) Error!void {
    for (paths) |p| {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = @import("../../io/align_probe.zig").pathZ(&zbuf, p) catch return error.Io;
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return error.Io;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        const op: i32 = std.posix.LOCK.SH | std.posix.LOCK.NB;
        if (linux.errno(linux.flock(fd, op)) == .AGAIN) {
            log.err("volume verb refused: metadata volume {s} is live-mounted (a daemon holds its single-writer guard) — unmount first; --force never overrides a live mount", .{p});
            return error.LiveMounted;
        }
    }
}

fn observeAll(gpa: Allocator, opts: Options, paths: []const []const u8, out: []Member) Error!void {
    for (paths, out) |p, *m| {
        m.* = .{ .path = p, .owned = discovery.linux_dev.observe(gpa, opts.env, opts.pool, p) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        } };
    }
}

fn freeAll(gpa: Allocator, members: []Member) void {
    for (members) |*m| m.owned.deinit(gpa);
}

fn observations(gpa: Allocator, members: []const Member) Allocator.Error![]discovery.Observation {
    const obs = try gpa.alloc(discovery.Observation, members.len);
    for (members, obs) |m, *o| o.* = m.owned.obs;
    return obs;
}

fn openMember(gpa: Allocator, io: Io, opts: Options, path: []const u8, index: u16) Error!*KvBackend {
    return KvBackend.open(gpa, io, path, .{ .env = opts.env, .pool = opts.pool, .index = index, .cache_budget_bytes = opts.cache_budget_bytes, .cpus = opts.cpus }) catch |e| switch (e) {
        error.Busy => error.LiveMounted,
        error.OutOfMemory => error.OutOfMemory,
        error.Io, error.DeviceRefused, error.LaneSpawnFailed, error.ThreadSpawnFailed => error.Io,
        else => error.Corrupt,
    };
}

fn checkpointMember(be: *KvBackend) Error!void {
    be.checkpointNow() catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io, error.VolumeFailed => error.Io,
        error.Corrupt => error.Corrupt,
    };
}

fn mapMigrate(e: migrate.Error) Error {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io, error.VolumeFailed => error.Io,
        else => error.Corrupt,
    };
}

/// The load of a hosted slot without a probe (Q15): its cursor's
/// progression (a slot with no cursor minted nothing — empty by
/// construction) — the census the `--take-slots k` ranking reads.
fn slotLoad(stamp: ledger.MembershipStamp, slot: u16) u64 {
    return (stamp.cursorFor(slot) orelse inos.LOCAL_INO_BASE) - inos.LOCAL_INO_BASE;
}

// ---------------------------------------------------------------------------
// KD-8: the staging drain barrier + the generation rebind.
// ---------------------------------------------------------------------------

pub const Rebind = struct {
    set_uuid: [16]u8,
    old_epoch: u64,
    new_epoch: u64,
};

/// Phase 1: every MEMBER root (0003 §T.1) under every cache path bound to
/// this set must have no live owner and hold NO staged custody (a unit a
/// member has yet to drain would be discarded by the rebind); then the
/// DUAL marker, its owner kept.
pub fn stagingRebindPrepare(gpa: Allocator, pool: *uring_fs.Pool, containers: []const []const u8, r: Rebind) Error!void {
    for (containers) |container| {
        var it = staging.Ring.MemberIter.open(container) orelse continue;
        defer it.close();
        while (it.next()) |slot| {
            var buf: [linux.PATH_MAX]u8 = undefined;
            const root = staging.Ring.memberRootPath(&buf, container, slot) catch return error.StagingRefused;
            const m = (try boundMarker(gpa, pool, root, r)) orelse continue;
            var next = m;
            next.set_epoch = r.new_epoch;
            next.prev_set_epoch = r.old_epoch;
            try writeMarker(pool, root, next);
        }
    }
}

/// Phase 2: the single new-epoch marker on every rebound member root.
pub fn stagingRebindFinalize(gpa: Allocator, pool: *uring_fs.Pool, containers: []const []const u8, r: Rebind) Error!void {
    for (containers) |container| {
        var it = staging.Ring.MemberIter.open(container) orelse continue;
        defer it.close();
        while (it.next()) |slot| {
            var buf: [linux.PATH_MAX]u8 = undefined;
            const root = staging.Ring.memberRootPath(&buf, container, slot) catch return error.StagingRefused;
            const m = (try boundMarker(gpa, pool, root, r)) orelse continue;
            var next = m;
            next.set_epoch = r.new_epoch;
            next.prev_set_epoch = null;
            try writeMarker(pool, root, next);
        }
    }
    rstats.staging_rebind_barriers.inc();
}

/// A member root's marker iff it is bound to this set: a live owner or a
/// pending custody unit refuses the rebind; another set's root (or none)
/// is not this rebind's.
fn boundMarker(gpa: Allocator, pool: *uring_fs.Pool, root: []const u8, r: Rebind) Error!?staging.Marker {
    const staging_verb = @import("../../cli/staging_verb.zig");
    const row = staging_verb.inspectRoot(gpa, pool, root, r.set_uuid);
    if (row.binding == .foreign or row.binding == .none) return null;
    if (row.owner_live) {
        log.err("staging drain barrier refused (KD-8): the member root {s} is held LIVE (appender {d}, pid {d}) — unmount every member of the set first; nothing was changed", .{ root, row.appender_id, row.owner_pid });
        return error.StagingCustodyLive;
    }
    if (row.live_units != 0) {
        log.err("staging drain barrier refused (KD-8): {d} pending write-custody unit(s) remain under {s} — mount the filesystem as that member (mount slot {x:0>8}), let writeback drain (`sync -f` the mount, then a clean unmount), then re-run; nothing was changed", .{ row.live_units, root, row.mount_slot });
        return error.StagingCustodyLive;
    }
    return (staging.Ring.readMarker(pool, root) catch return error.StagingRefused) orelse null;
}

fn writeMarker(pool: *uring_fs.Pool, root: []const u8, m: staging.Marker) Error!void {
    const fd = staging.Ring.openRoot(root) catch return error.StagingRefused;
    defer _ = linux.close(fd);
    staging.Ring.writeMarker(pool, root, fd, m) catch return error.StagingRefused;
}

// ---------------------------------------------------------------------------
// `volume list`
// ---------------------------------------------------------------------------

/// One line per member: what its stamp says (stampless named as such).
pub fn list(gpa: Allocator, opts: Options, paths: []const []const u8, w: *Io.Writer) Error!void {
    const members = try gpa.alloc(Member, paths.len);
    defer gpa.free(members);
    try observeAll(gpa, opts, paths, members);
    defer freeAll(gpa, members);
    for (members) |m| try writeMemberLine(w, m);
}

fn writeMemberLine(w: *Io.Writer, m: Member) Error!void {
    const o = m.owned.obs;
    if (o.stamp) |st| {
        w.print("  {s}: set {x:0>2}{x:0>2}{x:0>2}{x:0>2}.. epoch {d} position {d}/{d} width {d} slots ", .{ m.path, st.set_uuid[0], st.set_uuid[1], st.set_uuid[2], st.set_uuid[3], st.set_epoch, st.member_position, st.member_count, W }) catch return error.Io;
        w.writeByte('[') catch return error.Io;
        for (st.slots_hosted.runs, 0..) |r, i| w.print("{s}{d}+{d}×{d}", .{ if (i == 0) "" else ", ", r.start, r.stride, r.count }) catch return error.Io;
        w.print("] ({d} hosted, {d} cursors{s}){s}\n", .{ st.slots_hosted.len(), st.slot_cursors.len, if (st.native_slot != null) ", the native keyspace" else "", if (st.member_count == 0) " — RETIRED" else "" }) catch return error.Io;
    } else {
        w.print("  {s}: NO membership stamp (ledger seq {d})\n", .{ m.path, o.ledger_seq }) catch return error.Io;
    }
}

// ---------------------------------------------------------------------------
// `volume add-meta`
// ---------------------------------------------------------------------------

pub const AddReport = struct {
    position: u16,
    epoch: u64,
    taken: []u16,
    records_copied: u64,
    resumed: bool,
};

/// `add-meta <members…> <device> --take-slots k|s1,s2,…` (§4.1's fifteen
/// steps). Returns the slots taken (caller frees).
pub fn addMeta(gpa: Allocator, io: Io, opts: Options, paths: []const []const u8, device: []const u8, take: TakeSlots) Error!AddReport {
    for (paths) |p| if (std.mem.eql(u8, p, device)) return error.AlreadyMember;
    try refuseLive(paths);
    // 2. The members as observed; one coherent set.
    const members = try gpa.alloc(Member, paths.len);
    defer gpa.free(members);
    try observeAll(gpa, opts, paths, members);
    defer freeAll(gpa, members);
    const obs = try observations(gpa, members);
    defer gpa.free(obs);
    var ddiag: discovery.Refusal = undefined;
    var disc = discovery.discover(gpa, obs, &ddiag) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        error.Refused => {
            logRefusal(ddiag);
            return error.SetRefused;
        },
    };
    defer disc.deinit();
    const n: u16 = @intCast(paths.len);
    // 3. Resume detection: the device's own stamp with THIS set uuid
    // declaring n + 1 members = writes 1..n of a crashed run landed.
    var resumed = false;
    var taken_list: std.ArrayList(u16) = .empty;
    defer taken_list.deinit(gpa);
    var epoch: u64 = disc.set_epoch + 1;
    var position: u16 = 0;
    for (disc.positions) |p| position = @max(position, p + 1);
    if (discovery.linux_dev.observe(gpa, opts.env, opts.pool, device)) |*dev_owned| {
        var owned = dev_owned.*;
        defer owned.deinit(gpa);
        if (owned.obs.stamp) |st| if (std.mem.eql(u8, &st.set_uuid, &disc.set_uuid) and st.member_count == n + 1) {
            // The hostless transient stamp (a crash between the format and
            // write 1) resumes the position and epoch with a fresh
            // selection; a claim (write 1 landed) resumes the slots too.
            epoch = st.set_epoch;
            position = st.member_position;
            if (st.slots_hosted.len() != 0) {
                resumed = true;
                var s: u32 = 0;
                while (s < W) : (s += 1) if (st.slots_hosted.contains(@intCast(s))) try taken_list.append(gpa, @intCast(s));
            }
        };
    } else |e| switch (e) {
        // A blank or foreign device formats fresh; a torn one refuses.
        error.NotFormatted, error.LegacyFormat => {},
        error.OutOfMemory => return error.OutOfMemory,
        error.DeviceRefused, error.Io => return error.Io,
        error.Corrupt => return error.Corrupt,
    }
    // 7. Slot selection (unless resuming a run that already claimed).
    const table = disc.table();
    if (!resumed) try selectSlots(gpa, &disc, &table, obs, take, &taken_list);
    const taken = taken_list.items;
    std.mem.sort(u16, taken, {}, std.sort.asc(u16));
    const runs = try route_table.coalesceRuns(gpa, taken);
    defer gpa.free(runs);
    if (runs.len > ledger.STAMP_MAX_RUNS or taken.len > ledger.STAMP_MAX_CURSORS) return error.StampBudget;

    // 8. The joiner formatted with the HOSTLESS transient stamp (its own
    // door checked first: a live joiner is somebody else's volume).
    if (!resumed) {
        try refuseLive(&.{device});
        var uuid: [16]u8 = undefined;
        io.randomSecure(&uuid) catch return error.Io;
        const hostless: ledger.MembershipStamp = .{ .set_uuid = disc.set_uuid, .set_epoch = epoch, .member_position = position, .member_count = n + 1, .slots_hosted = .empty, .native_slot = null, .slot_cursors = &.{} };
        backend.formatVolume(gpa, device, .{ .env = opts.env, .pool = opts.pool, .node_size = disc.node_size, .uuid = uuid, .set = .{ .set_uuid = disc.set_uuid, .hash_seed = disc.hash_seed }, .member_position = position, .member_count = n + 1, .stamp_override = hostless }) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
    }
    // 9. KD-8 phase 1.
    const rebind: Rebind = .{ .set_uuid = disc.set_uuid, .old_epoch = disc.set_epoch, .new_epoch = epoch };
    try stagingRebindPrepare(gpa, opts.pool, opts.staging_roots, rebind);

    // 11. Every member open through its door, the joiner too.
    const opened = try gpa.alloc(*KvBackend, n + 1);
    defer gpa.free(opened);
    var n_open: usize = 0;
    defer for (opened[0..n_open]) |be| be.close();
    for (disc.order, 0..) |oi, vi| {
        opened[vi] = try openMember(gpa, io, opts, paths[oi], @intCast(vi));
        n_open += 1;
    }
    const joiner = try openMember(gpa, io, opts, device, n);
    opened[n] = joiner;
    n_open += 1;
    var records: u64 = 0;
    for (taken) |slot| {
        const host = opened[table.volumeOf(slot)];
        // A re-run: the source may already have released (then the
        // joiner's copy is the one that stands). A virgin slot (no tree)
        // moves in the stamp alone; a travelled cursor moves with it.
        if (host.slotTree(slot) != null) {
            joiner.dropTree(slot) catch |e| return mapMigrate(migrate.mapBackendError(e));
            records += migrate.copySlotTree(gpa, host, joiner, slot) catch |e| return mapMigrate(e);
        }
        if (host.stamp.cursorSnapshot(slot)) |cursor| joiner.stamp.installFloor(slot, cursor) catch return error.OutOfMemory;
    }
    // 12. Write 1 (target-first): the joiner's claim at E, count n + 1.
    joiner.stamp.setHosted(taken) catch return error.OutOfMemory;
    joiner.stamp.setEpoch(epoch);
    joiner.stamp.setMembership(position, n + 1);
    try checkpointMember(joiner);
    // 13. Writes 2..n: every old member at E, count n + 1, minus the taken.
    for (opened[0..n]) |be| {
        for (taken) |slot| {
            if (!be.stamp.hosts(slot)) continue;
            be.stamp.releaseSlot(slot) catch return error.OutOfMemory;
            be.stamp.removeCursor(slot);
        }
        be.stamp.setEpoch(epoch);
        be.stamp.setMembership(be.stamp.member_position, n + 1);
        try checkpointMember(be);
    }
    // 14. Teardown of every source's copy.
    for (opened[0..n]) |be| for (taken) |slot| be.dropTree(slot) catch |e| return mapMigrate(migrate.mapBackendError(e));
    // 15. The doors release at close; KD-8 phase 2.
    for (opened[0..n_open]) |be| be.close();
    n_open = 0;
    try stagingRebindFinalize(gpa, opts.pool, opts.staging_roots, rebind);
    return .{ .position = position, .epoch = epoch, .taken = try gpa.dupe(u16, taken), .records_copied = records, .resumed = resumed };
}

/// The union of two ascending, disjoint slot lists (caller frees).
fn mergeAscending(gpa: Allocator, a: []const u16, b: []const u16) Allocator.Error![]u16 {
    const out = try gpa.alloc(u16, a.len + b.len);
    var i: usize = 0;
    var j: usize = 0;
    var n: usize = 0;
    while (i < a.len or j < b.len) : (n += 1) {
        if (j == b.len or (i < a.len and a[i] < b[j])) {
            out[n] = a[i];
            i += 1;
        } else {
            out[n] = b[j];
            j += 1;
        }
    }
    return out;
}

/// §4.1 step 7: an explicit list (each hosted, never slot 0, no member
/// left hostless) or the `k` most loaded takeable slots (every member
/// keeps its least loaded slot; slot 0 never leaves member 0).
fn selectSlots(gpa: Allocator, disc: *const discovery.Discovery, table: *const route_table.RouteTable, obs: []const discovery.Observation, take: TakeSlots, out: *std.ArrayList(u16)) Error!void {
    switch (take) {
        .list => |slots| {
            var per_member = try gpa.alloc(u32, disc.member_count);
            defer gpa.free(per_member);
            @memset(per_member, 0);
            for (slots) |s| {
                if (s == 0) return error.SlotZero;
                for (out.items) |x| if (x == s) return error.SlotNotHosted;
                per_member[table.volumeOf(s)] += 1;
                try out.append(gpa, s);
            }
            for (per_member, 0..) |taken, v| if (taken >= table.hostedCount(@intCast(v))) return error.HostLeftEmpty;
        },
        .count => |k| {
            if (k == 0) return error.TakeZero;
            const Cand = struct { slot: u16, load: u64 };
            var cands: std.ArrayList(Cand) = .empty;
            defer cands.deinit(gpa);
            for (disc.order, 0..) |oi, vi| {
                const st = obs[oi].stamp.?;
                const hosted = table.hostedSlots(gpa, @intCast(vi)) catch return error.OutOfMemory;
                defer gpa.free(hosted);
                if (hosted.len < 2) continue;
                // Every host keeps its least loaded slot (slot 0 never
                // leaves): rank the rest.
                var keep: ?u16 = null;
                var keep_load: u64 = std.math.maxInt(u64);
                for (hosted) |s| {
                    if (s == 0) continue;
                    const l = slotLoad(st, s);
                    if (keep == null or l < keep_load) {
                        keep = s;
                        keep_load = l;
                    }
                }
                for (hosted) |s| {
                    if (s == 0 or (keep != null and s == keep.?)) continue;
                    try cands.append(gpa, .{ .slot = s, .load = slotLoad(st, s) });
                }
            }
            if (cands.items.len < k) return error.TooFewTakeable;
            std.mem.sort(Cand, cands.items, {}, struct {
                fn gt(_: void, a: Cand, b: Cand) bool {
                    return a.load > b.load or (a.load == b.load and a.slot < b.slot);
                }
            }.gt);
            for (cands.items[0..k]) |c| try out.append(gpa, c.slot);
        },
    }
}

// ---------------------------------------------------------------------------
// `volume remove-meta`
// ---------------------------------------------------------------------------

pub const RemoveReport = struct {
    epoch: u64,
    moved_slots: u32,
    records_copied: u64,
};

/// `remove-meta <members…> <victim>` (§4.2): the victim's slots over the
/// survivors with the most free extents after the capacity preflight;
/// survivors-first stamps at E, count n − 1; the victim's tombstone LAST.
pub fn removeMeta(gpa: Allocator, io: Io, opts: Options, paths: []const []const u8, victim: []const u8) Error!RemoveReport {
    if (paths.len < 2) return error.LastMember;
    var victim_i: ?usize = null;
    for (paths, 0..) |p, i| if (std.mem.eql(u8, p, victim)) {
        victim_i = i;
    };
    const vi_uri = victim_i orelse return error.NotMember;
    try refuseLive(paths);
    const members = try gpa.alloc(Member, paths.len);
    defer gpa.free(members);
    try observeAll(gpa, opts, paths, members);
    defer freeAll(gpa, members);
    const obs = try observations(gpa, members);
    defer gpa.free(obs);
    var ddiag: discovery.Refusal = undefined;
    var disc = discovery.discover(gpa, obs, &ddiag) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        error.Refused => {
            logRefusal(ddiag);
            return error.SetRefused;
        },
    };
    defer disc.deinit();
    // The victim's canonical index; member 0 (slot 0's host) never retires.
    var victim_v: route_table.VolumeIndex = 0;
    for (disc.order, 0..) |oi, vi| if (oi == vi_uri) {
        victim_v = @intCast(vi);
    };
    if (victim_v == disc.native_volume) return error.MemberZero;
    const epoch = disc.set_epoch + 1;
    const table = disc.table();
    const victim_slots = table.hostedSlots(gpa, victim_v) catch return error.OutOfMemory;
    defer gpa.free(victim_slots);

    // The members open (every door), then the §5.2 capacity preflight on
    // the live allocators.
    const n = paths.len;
    const opened = try gpa.alloc(*KvBackend, n);
    defer gpa.free(opened);
    var n_open: usize = 0;
    defer for (opened[0..n_open]) |be| be.close();
    for (disc.order, 0..) |oi, vi| {
        opened[vi] = try openMember(gpa, io, opts, paths[oi], @intCast(vi));
        n_open += 1;
    }
    const victim_be = opened[victim_v];
    {
        const used = victim_be.alloc.totalExtents() - victim_be.alloc.freeExtents();
        var avail: u64 = 0;
        var headroom: u64 = 0;
        for (opened, 0..) |be, v| {
            if (v == victim_v) continue;
            avail += be.alloc.freeExtents();
            // The same cap the survivors' checkpoint tasks run.
            const cap = checkpoint.maxDirtyNodesFromEnv(opts.env, opts.cache_budget_bytes, be.nodeSize());
            headroom = @max(headroom, @min(2 * cap, be.alloc.totalExtents() / 4));
        }
        if (avail < used + headroom) {
            log.err("volume remove-meta preflight refused (§5.2 meta side): survivors' free extents {d} < victim used {d} + checkpoint headroom {d} (extents of {d} B) — grow the survivors first; nothing was changed", .{ avail, used, headroom, victim_be.nodeSize() });
            return error.CapacityRefused;
        }
    }
    // KD-8 phase 1 with the survivor set's epoch.
    const rebind: Rebind = .{ .set_uuid = disc.set_uuid, .old_epoch = disc.set_epoch, .new_epoch = epoch };
    try stagingRebindPrepare(gpa, opts.pool, opts.staging_roots, rebind);
    // Every victim slot onto the survivor with the most free extents at
    // its placement: a slot WITH a tree is copied and its cursor travels;
    // a virgin slot (no tree, no cell — most of a member's hosted set)
    // moves in the stamp alone, so the survivors' extents and cursor
    // budget scale with the victim's trees, never with W.
    var records: u64 = 0;
    const gains = try gpa.alloc(std.ArrayList(u16), n);
    defer gpa.free(gains);
    for (gains) |*g| g.* = .empty;
    defer for (gains) |*g| g.deinit(gpa);
    for (victim_slots) |slot| {
        var best: ?route_table.VolumeIndex = null;
        var best_free: u64 = 0;
        for (opened, 0..) |be, v| {
            if (v == victim_v) continue;
            const free = be.alloc.freeExtents();
            if (best == null or free > best_free) {
                best = @intCast(v);
                best_free = free;
            }
        }
        const dst = opened[best.?];
        if (victim_be.slotTree(slot) != null) {
            dst.dropTree(slot) catch |e| return mapMigrate(migrate.mapBackendError(e));
            records += migrate.copySlotTree(gpa, victim_be, dst, slot) catch |e| return mapMigrate(e);
        }
        if (victim_be.stamp.cursorSnapshot(slot)) |cursor| dst.stamp.installFloor(slot, cursor) catch return error.OutOfMemory;
        try gains[best.?].append(gpa, slot);
    }
    // Survivors FIRST at E, count n − 1, positions kept, each hosted set
    // rewritten ONCE (its runs ∪ its gains, both ascending).
    for (opened, 0..) |be, v| {
        if (v == victim_v) continue;
        if (gains[v].items.len != 0) {
            const cur = be.stamp.hostedSlots(gpa) catch return error.OutOfMemory;
            defer gpa.free(cur);
            const merged = try mergeAscending(gpa, cur, gains[v].items);
            defer gpa.free(merged);
            be.stamp.setHosted(merged) catch return error.OutOfMemory;
        }
        be.stamp.setEpoch(epoch);
        be.stamp.setMembership(be.stamp.member_position, @intCast(n - 1));
        try checkpointMember(be);
    }
    // The victim's tombstone LAST.
    victim_be.stamp.setHosted(&.{}) catch return error.OutOfMemory;
    for (victim_slots) |slot| victim_be.stamp.removeCursor(slot);
    victim_be.stamp.setEpoch(epoch);
    victim_be.stamp.setMembership(victim_be.stamp.member_position, 0);
    try checkpointMember(victim_be);
    for (opened[0..n_open]) |be| be.close();
    n_open = 0;
    try stagingRebindFinalize(gpa, opts.pool, opts.staging_roots, rebind);
    return .{ .epoch = epoch, .moved_slots = @intCast(victim_slots.len), .records_copied = records };
}

// ---------------------------------------------------------------------------
// `volume repair-set`
// ---------------------------------------------------------------------------

pub const RepairReport = struct {
    restamped: u32,
    inferred: bool,
};

/// `repair-set <members…>` (§4.3): print the observed state; refuse an
/// incoherent one; 0 stampless ⇒ re-stamp every member at the max epoch
/// with the slots it WON (converging interrupted flips); 1 stampless ⇒
/// the kill-9 window — infer the one free position and the unhosted
/// complement; more ⇒ not inferable.
pub fn repairSet(gpa: Allocator, io: Io, opts: Options, paths: []const []const u8, w: *Io.Writer) Error!RepairReport {
    try refuseLive(paths);
    const members = try gpa.alloc(Member, paths.len);
    defer gpa.free(members);
    try observeAll(gpa, opts, paths, members);
    defer freeAll(gpa, members);
    for (members) |m| try writeMemberLine(w, m);
    var stampless: usize = 0;
    var stampless_i: usize = 0;
    for (members, 0..) |m, i| if (m.owned.obs.stamp == null) {
        stampless += 1;
        stampless_i = i;
    };
    if (stampless == 0) {
        const obs = try observations(gpa, members);
        defer gpa.free(obs);
        var ddiag: discovery.Refusal = undefined;
        var disc = discovery.discover(gpa, obs, &ddiag) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Refused => {
                w.writeAll("volume repair-set refused: ") catch return error.Io;
                ddiag.write(w) catch return error.Io;
                w.writeAll(" — VL5a re-stamps only a coherent observed state\n") catch return error.Io;
                return error.SetRefused;
            },
        };
        defer disc.deinit();
        const table = disc.table();
        var restamped: u32 = 0;
        for (disc.order, 0..) |oi, vi| {
            const be = try openMember(gpa, io, opts, paths[oi], @intCast(vi));
            defer be.close();
            const won = table.hostedSlots(gpa, @intCast(vi)) catch return error.OutOfMemory;
            defer gpa.free(won);
            // Every claim this member lost to a higher epoch goes (its copy
            // with it), every cursor of a kept slot stays.
            const cur = be.stamp.hostedSlots(gpa) catch return error.OutOfMemory;
            defer gpa.free(cur);
            for (cur) |s| {
                if (table.volumeOf(s) != vi) {
                    be.stamp.removeCursor(s);
                    be.dropTree(s) catch |e| return mapMigrate(migrate.mapBackendError(e));
                }
            }
            be.stamp.setHosted(won) catch return error.OutOfMemory;
            be.stamp.setEpoch(disc.set_epoch);
            try checkpointMember(be);
            restamped += 1;
        }
        return .{ .restamped = restamped, .inferred = false };
    }
    if (stampless > 1) {
        w.print("volume repair-set refused: {d} stampless members beside a stamped set — positions are not inferable\n", .{stampless}) catch return error.Io;
        return error.NotInferable;
    }
    // One stampless member: the stamped ones must agree; the free position
    // and the unhosted complement are inferred.
    var first: ?ledger.MembershipStamp = null;
    var hosted: std.StaticBitSet(W) = .empty;
    var positions: std.StaticBitSet(W) = .empty;
    for (members) |m| {
        const st = m.owned.obs.stamp orelse continue;
        if (first) |f| {
            if (!std.mem.eql(u8, &f.set_uuid, &st.set_uuid) or f.member_count != st.member_count) {
                w.writeAll("volume repair-set refused: stamps disagree (uuid/count/geometry) — VL5a re-stamps only a coherent observed state\n") catch return error.Io;
                return error.SetRefused;
            }
        } else first = st;
        if (st.member_count != paths.len) {
            w.print("volume repair-set refused: stamps declare {d} members but the URI lists {d}\n", .{ st.member_count, paths.len }) catch return error.Io;
            return error.SetRefused;
        }
        if (st.member_position >= paths.len or positions.isSet(st.member_position)) {
            w.print("volume repair-set refused: member position {d} is out of range or duplicated\n", .{st.member_position}) catch return error.Io;
            return error.SetRefused;
        }
        positions.set(st.member_position);
        for (st.slots_hosted.runs) |r| {
            var k: u32 = 0;
            while (k < r.count) : (k += 1) hosted.set(@intCast(@as(u64, r.start) + @as(u64, k) * @as(u64, r.stride)));
        }
    }
    const f = first.?;
    var free_pos: u16 = 0;
    while (positions.isSet(free_pos)) free_pos += 1;
    var complement: std.ArrayList(u16) = .empty;
    defer complement.deinit(gpa);
    var s: u32 = 0;
    while (s < W) : (s += 1) if (!hosted.isSet(s)) try complement.append(gpa, @intCast(s));
    const runs = try route_table.coalesceRuns(gpa, complement.items);
    defer gpa.free(runs);
    if (runs.len > ledger.STAMP_MAX_RUNS) {
        w.print("volume repair-set refused: the unhosted slot complement ({d} slots in {d} runs) cannot belong to one member\n", .{ complement.items.len, runs.len }) catch return error.Io;
        return error.NotInferable;
    }
    // The stampless member needs a ledger record before its door opens:
    // a volume with a v4 superblock and no valid record is corruption to
    // the mount, so the inferred stamp is written as a bootstrap record
    // from the member's superblock geometry.
    try writeInferredBootstrap(gpa, opts, members[stampless_i].path, .{
        .set_uuid = f.set_uuid,
        .set_epoch = f.set_epoch,
        .member_position = free_pos,
        .member_count = f.member_count,
        .slots_hosted = .{ .runs = runs },
        .native_slot = ledger.nativeSlotOf(!hosted.isSet(0)),
        .slot_cursors = &.{},
    });
    w.print("  inferred: {s} is member {d}/{d} hosting the {d}-slot complement in {d} run(s) at epoch {d} — re-created EMPTY (its ledger named no root, so what it held is unreachable; fsck's census)\n", .{ members[stampless_i].path, free_pos, f.member_count, complement.items.len, runs.len, f.set_epoch }) catch return error.Io;
    return .{ .restamped = 1, .inferred = true };
}

/// A stampless member has NO valid ledger record, so its roots are
/// unreachable whatever its heap holds: the member is re-created with its
/// own uuid and the set's identity as the format-time bootstrap (an empty
/// ring, empty control and native roots) carrying the inferred stamp —
/// the one coherent state a torn ledger can be given. The records it
/// hosted are gone with the ledger (fsck's census, never a silent
/// resurrection), which the verb prints.
fn writeInferredBootstrap(gpa: Allocator, opts: Options, path: []const u8, stamp: ledger.MembershipStamp) Error!void {
    var o = discovery.linux_dev.observe(gpa, opts.env, opts.pool, path) catch return error.Io;
    defer o.deinit(gpa);
    backend.formatVolume(gpa, path, .{ .env = opts.env, .pool = opts.pool, .node_size = o.obs.node_size, .uuid = o.obs.uuid, .set = .{ .set_uuid = stamp.set_uuid, .hash_seed = o.obs.hash_seed }, .member_position = stamp.member_position, .member_count = stamp.member_count, .stamp_override = stamp }) catch return error.Io;
}

fn logRefusal(d: discovery.Refusal) void {
    var buf: [2048]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    d.write(&w) catch {};
    log.err("metadata set: {s}", .{w.buffered()});
}

pub fn writeError(w: *Io.Writer, e: Error, verb: []const u8) Io.Writer.Error!void {
    try w.print("volume {s}: ", .{verb});
    switch (e) {
        error.SetRefused => try w.writeAll("the members do not form one coherent set (see the log)"),
        error.LiveMounted => try w.writeAll("a member is live-mounted — the single-writer guard forbids an offline set change under a live write mount; unmount first (--force never overrides a live mount)"),
        error.AlreadyMember => try w.writeAll("the device is already a member of the metadata set"),
        error.NotMember => try w.writeAll("the victim is not a member of the metadata set"),
        error.LastMember => try w.writeAll("cannot remove the last metadata volume"),
        error.MemberZero => try w.writeAll("member 0 (slot 0's host: the root pin, the native keyspace, the set_config home) is never retired"),
        error.SlotZero => try w.writeAll("slot 0 is the root pin and never moves"),
        error.SlotNotHosted => try w.writeAll("a slot was named twice"),
        error.HostLeftEmpty => try w.writeAll("taking those slots would leave a member hosting nothing — every member keeps at least one slot (use `volume remove-meta` to retire a member)"),
        error.TakeZero => try w.writeAll("--take-slots 0 takes nothing — an added meta volume is only useful WITH slot migration"),
        error.TooFewTakeable => try w.writeAll("too few slots are takeable (every member keeps at least one) — the W-granularity limit"),
        error.StampBudget => try w.print("the taken slots do not fit one stamp ({d} runs / {d} cursors)", .{ ledger.STAMP_MAX_RUNS, ledger.STAMP_MAX_CURSORS }),
        error.CapacityRefused => try w.writeAll("the §5.2 capacity preflight refused (see the log for the numbers); nothing was changed"),
        error.StagingCustodyLive => try w.writeAll("the KD-8 staging drain barrier refused (see the log); nothing was changed"),
        error.StagingRefused => try w.writeAll("a staging root's marker could not be written"),
        error.NotInferable => try w.writeAll("the stampless state is not inferable (see above)"),
        error.Io => try w.writeAll("device I/O failed"),
        error.Corrupt => try w.writeAll("a routing/protocol inconsistency (see the log)"),
        error.OutOfMemory => try w.writeAll("out of memory"),
    }
}

// ---------------------------------------------------------------------------
// Tests (Linux; the `meta_set.TestSet` harness).
// ---------------------------------------------------------------------------

const testing = std.testing;
const meta_set = @import("../meta_set.zig");
const align_probe = @import("../../io/align_probe.zig");
const derive = @import("../../core/derive.zig");

fn testOptions(s: *meta_set.TestSet, pool: *uring_fs.Pool) Options {
    return .{ .env = &s.env, .pool = pool, .cache_budget_bytes = 64 << 20, .cpus = 4 };
}

test "add-meta grows a 2-set to 3 (two loaded slots taken, child-first claim), remove-meta shrinks it back (survivors-first, the victim's tombstone refuses discovery), every ino stable throughout; member 0 and slot 0 refuse; repair-set re-stamps a coherent set" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "offline", 2);
    defer s.destroy();
    const pool = try uring_fs.Pool.init(gpa, uring_fs.Config.resolve(&s.env, derive.Facts.probe()));
    defer pool.deinit();
    const third = try std.fmt.allocPrint(gpa, "{s}/sqz-set-{d}-offline-third.img", .{ meta_set.test_dir, linux.getpid() });
    defer gpa.free(third);
    defer {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        if (align_probe.pathZ(&zbuf, third)) |z| {
            _ = linux.unlink(z);
        } else |_| {}
    }
    {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = try align_probe.pathZ(&zbuf, third);
        const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
        try testing.expect(linux.errno(rc) == .SUCCESS);
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        try testing.expect(linux.errno(linux.ftruncate(fd, 64 << 20)) == .SUCCESS);
    }
    var acked: std.ArrayList(u64) = .empty;
    defer acked.deinit(gpa);
    {
        const set = try s.open();
        defer set.close();
        var nb: [24]u8 = undefined;
        for (0..240) |i| try acked.append(gpa, try meta_set.createFile(set, inos.ROOT_INO, try std.fmt.bufPrint(&nb, "o{d}", .{i})));
    }
    const two = s.pathList();
    // A live mount refuses the verb (loud).
    {
        const set = try s.open();
        defer set.close();
        log.testing.expectErrorLogs(1);
        try testing.expectError(error.LiveMounted, addMeta(gpa, s.io(), testOptions(s, pool), two, third, .{ .count = 2 }));
        try log.testing.expectAllFired();
    }
    try testing.expectError(error.TakeZero, addMeta(gpa, s.io(), testOptions(s, pool), two, third, .{ .count = 0 }));
    try testing.expectError(error.SlotZero, addMeta(gpa, s.io(), testOptions(s, pool), two, third, .{ .list = &.{0} }));
    try testing.expectError(error.AlreadyMember, addMeta(gpa, s.io(), testOptions(s, pool), two, two[1], .{ .count = 1 }));
    const add = try addMeta(gpa, s.io(), testOptions(s, pool), two, third, .{ .count = 2 });
    defer gpa.free(add.taken);
    try testing.expectEqual(@as(u16, 2), add.position);
    try testing.expectEqual(@as(u64, 2), add.epoch);
    try testing.expectEqual(@as(usize, 2), add.taken.len);
    try testing.expect(add.records_copied > 0);
    try testing.expect(!add.resumed);
    // The three-member set mounts in any order; every ino stable; the
    // taken slots live on the joiner; a mint spreads over three members.
    const three = [_][]const u8{ third, two[0], two[1] };
    {
        const set = try meta_set.MetaSet.open(gpa, s.io(), &three, .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
        defer set.close();
        try testing.expectEqual(@as(usize, 3), set.volumeCount());
        for (add.taken) |slot| try testing.expectEqual(@as(route_table.VolumeIndex, 2), set.volumeIndexOfSlot(slot));
        for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
        try testing.expectEqual(@as(u64, 2), set.setEpoch());
        var buf: [4096]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try list(gpa, testOptions(s, pool), &three, &w);
    }
    // A second add of the same device is refused: it is a member now.
    try testing.expectError(error.AlreadyMember, addMeta(gpa, s.io(), testOptions(s, pool), &three, third, .{ .count = 1 }));
    // Remove: member 0 never; the last member never; the joiner goes.
    try testing.expectError(error.MemberZero, removeMeta(gpa, s.io(), testOptions(s, pool), &three, two[0]));
    try testing.expectError(error.NotMember, removeMeta(gpa, s.io(), testOptions(s, pool), &three, "/nowhere"));
    try testing.expectError(error.LastMember, removeMeta(gpa, s.io(), testOptions(s, pool), two[0..1], two[0]));
    const rm = try removeMeta(gpa, s.io(), testOptions(s, pool), &three, third);
    try testing.expectEqual(@as(u64, 3), rm.epoch);
    try testing.expectEqual(@as(u32, 2), rm.moved_slots);
    // The tombstone refuses discovery when listed (loud); the survivors mount.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.SetRefused, meta_set.MetaSet.open(gpa, s.io(), &three, .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4 }));
    try log.testing.expectAllFired();
    {
        const set = try s.openReversed();
        defer set.close();
        try testing.expectEqual(@as(usize, 2), set.volumeCount());
        for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
        try testing.expectEqual(@as(u64, 3), set.setEpoch());
        const fresh = try meta_set.createFile(set, inos.ROOT_INO, "after-remove");
        for (acked.items) |g| try testing.expect(g != fresh);
    }
    // repair-set over the coherent survivors: every member re-stamped at
    // the max epoch; the set still mounts with everything.
    var rbuf: [4096]u8 = undefined;
    var rw: Io.Writer = .fixed(&rbuf);
    const rep = try repairSet(gpa, s.io(), testOptions(s, pool), two, &rw);
    try testing.expectEqual(@as(u32, 2), rep.restamped);
    try testing.expect(!rep.inferred);
    try testing.expect(std.mem.indexOf(u8, rw.buffered(), "the native keyspace") != null);
    {
        const set = try s.open();
        defer set.close();
        for (acked.items) |g| try testing.expect(try meta_set.present(set, g));
    }
}
