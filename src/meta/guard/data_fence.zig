//! DATA-namespace PR fencing (owner ruling 2026-11-03; decision 0003 §DJ;
//! charter §5.5 "Fencing", §6.2 "Block reclaim"): every DATA namespace is
//! reserved Write Exclusive – All Registrants (rtype 5) and every member
//! HOST registers on it under its host's key — the key it holds on the
//! metadata namespace, the one a death record names — so the target refuses
//! a fenced host's data writes (`nvmet_pr_check_cmd_access`) and ANY
//! registrant's preempt of a key unregisters every registrant of it, the
//! reservation kept (nvmet 7.3's `nvmet_pr_preempt` runs its all-registrants
//! arm before the single-holder re-type of §DF.11: the manager's own client
//! fences, no fencer, and the host's own identity on every path of a
//! multipath=Y head addresses it). Nothing holds in the single-holder sense,
//! so nothing moves at a manager succession; the data allocation authority
//! stays the metadata plane's (`alloc_lease:` under the metadata hold).
//!
//! THE ADMISSION, per writer process (never a `-o ro` reader), after the
//! metadata admission and the fabric bind and BEFORE the data plane opens
//! (its write cache's recovery, reclaimer and sweeps can write): per data
//! volume whose node is an NVMe namespace with RESCAP ≠ 0, a client on its
//! char node; under the host's registry of the set the host registered there
//! ⇒ adopt; else the stale-sibling rule judged by the METADATA facts, the
//! data node's drain, the register, the re-judge and the check that the host
//! is still registered on volume 0's metadata namespace
//! (`host_fence.registerDataUnderRule`); unreserved ⇒ acquire WEAR. The
//! MANAGER (volume 0's writer) then RECONCILES: every data registrant whose
//! key volume 0's metadata namespace no longer registers — a superseded
//! manager, a chain of them, a host whose metadata fence landed alone — is
//! preempted-and-aborted, the data report read before the metadata's (a
//! joiner registers metadata before data and leaves data before metadata),
//! after a report shows this host holding.
//!
//! THE DEATH (`recovery.zig` step 2c, `fencing.fenceDataNamespaces`): the
//! dead host's key preempted on every data namespace and proven before
//! anything the member can write is reallocated, else the recovery holds; a
//! co-located member (the own-key guard) is killed by its recoverer on this
//! host (`host_fence.killColocatedMember`). THE RELEASE, by the host's last
//! process of the set: another registrant stands ⇒ unregister (WEAR keeps
//! the reservation); the namespace's last registrant keeps its registration
//! unless this incarnation reserved the namespace fresh and preempted
//! nothing there (§DF.6's induction: a released reservation re-admits every
//! fenced host). Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const claim = @import("claim.zig");
const pr = @import("pr.zig");
const nvme = @import("nvme_cmd.zig");
const proc = @import("proc.zig");
const pr_linux = @import("pr_linux.zig");
const host_fence = @import("host_fence.zig");
const fencing = @import("../sym/membership/fencing.zig");
const recovery = @import("../sym/membership/recovery.zig");
const appender_page = @import("../sym/appender_page.zig");
const joined = @import("../sym/joined.zig");
const join = @import("../sym/join.zig");
const backend = @import("../backend.zig");
const exec = @import("../../exec/exec.zig");
const router = @import("../../data/router.zig");
const set_config = @import("../../data/set_config.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/data_fence is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const stats = struct {
    /// Data registrations that admitted this host afresh (after the rule).
    pub var data_pr_registrations: counters.ShardedCounter = .{};
    /// Admissions that found this host registered: no device mutation.
    pub var data_pr_adoptions: counters.ShardedCounter = .{};
    /// WEAR reservations this process made on an unreserved namespace.
    pub var data_pr_reservations: counters.ShardedCounter = .{};
    /// The manager's reconciliation: data registrants volume 0's metadata
    /// namespace no longer registers, preempted.
    pub var data_pr_reconcile_preempts: counters.ShardedCounter = .{};
    /// Admissions refused (a fenced host, a stale sibling alive, a foreign
    /// reservation, an unreadable report): the mount or the volume refused.
    pub var data_pr_admission_refusals: counters.ShardedCounter = .{};
    /// Departures that kept the registration (the kept law), and those that
    /// gave it back.
    pub var data_pr_kept_at_release: counters.ShardedCounter = .{};
    pub var data_pr_unregistered_at_release: counters.ShardedCounter = .{};
    /// Data volumes this process writes with no device fence (a file, a
    /// non-NVMe node, RESCAP 0, no host identifier).
    pub var data_pr_volumes_unfenced: counters.Gauge = .{};
    /// Data volumes this process is registered on.
    pub var data_pr_volumes: counters.Gauge = .{};
};

/// Why a data volume carries no device fence (the documented class).
pub const Absent = enum {
    not_nvme_namespace,
    no_char_node,
    rescap_zero,
    probe_error,
    /// The data controller answers no host identifier (FID 0x81): the
    /// registration cannot be judged.
    no_wire_host_id,
};

pub const AdmitError = error{
    /// This host is not registered on volume 0's metadata namespace (a
    /// takeover or a death preempt fenced it): it never re-admits itself on
    /// data.
    HostFenced,
    Refused,
    OutOfMemory,
};

/// What the admission rides: volume 0's metadata namespace (the host's own
/// node — the guard's or the joiner's char node — and the host's key there:
/// the fence reads its own client over it, never sharing the holder client's
/// state with the heartbeat), the host's registry of the set, the pages the
/// rule judges.
pub const Inputs = struct {
    gpa: Allocator,
    env: *const std.process.Environ.Map,
    meta_face: nvme.Namespace,
    meta_nsid: u32,
    key: u64,
    /// Null: the per-mount seam or no host identifier — no rule.
    reg: ?host_fence.Registry,
    local: claim.Identity,
    node_token: u64,
    /// Volume 0's appender pages.
    pages: host_fence.PageSource,
    /// Volume 0's writer (the manager, or a flat set's): it reconciles.
    manager: bool,
};

pub const Volume = struct {
    vol_tag: u64,
    /// The data node (owned).
    node: []const u8,
    /// Null for a fixture's client over a fake face.
    device: ?*pr_linux.Device,
    client: *pr.Client,
    /// This process registered the host (vs adopted).
    fresh: bool,
    /// This process reserved the unreserved namespace.
    acquired: bool,

    pub fn path(self: *const Volume) []const u8 {
        return self.node;
    }
};

const AbsentRow = struct { vol_tag: u64, why: Absent };

pub const DataFence = struct {
    in: Inputs,
    /// This fence's own client on volume 0's metadata namespace (owned).
    meta: *pr.Client,
    /// Guards the volume table: the admission of an online volume, the
    /// recovery's fence, the release.
    lock: pr_linux.IssueLock = .{},
    vols: [router.MAX_VOLUMES]Volume = undefined,
    clients: [router.MAX_VOLUMES]*pr.Client = undefined,
    n: usize = 0,
    absent: [router.MAX_VOLUMES]AbsentRow = undefined,
    n_absent: usize = 0,

    pub fn create(in: Inputs) Allocator.Error!*DataFence {
        const self = try in.gpa.create(DataFence);
        errdefer in.gpa.destroy(self);
        self.* = .{ .in = in, .meta = try metaClient(in.gpa, in.meta_face, in.meta_nsid, in.key) };
        return self;
    }

    /// The metadata namespace answers its host identifier (the registrations
    /// are judged by it); else no data volume can be fenced.
    pub fn metaJudgeable(self: *DataFence) bool {
        _ = self.meta.wireHostId() catch return false;
        return true;
    }

    /// The admission of one data volume by its live node path.
    pub fn admitVolume(self: *DataFence, vol_tag: u64, path: []const u8) AdmitError!void {
        const gpa = self.in.gpa;
        const dev = switch (pr_linux.resolve(gpa, path)) {
            .none => |why| return self.noteAbsent(vol_tag, path, switch (why) {
                .not_nvme_namespace => .not_nvme_namespace,
                .no_char_node => .no_char_node,
                .rescap_zero => .rescap_zero,
                .probe_error => .probe_error,
            }),
            .device => |d| d,
        };
        const c = gpa.create(pr.Client) catch {
            dev.deinit();
            return error.OutOfMemory;
        };
        c.* = pr.Client.init(gpa, dev.face(), dev.nsid, self.in.key) catch {
            gpa.destroy(c);
            dev.deinit();
            return error.OutOfMemory;
        };
        const rescap = c.probeRescap() catch 0;
        if (rescap == 0) {
            freeClient(gpa, dev, c);
            return self.noteAbsent(vol_tag, path, .rescap_zero);
        }
        return self.admitClient(vol_tag, path, dev, c);
    }

    /// The admission's core over a client (owned from here: freed on a
    /// refusal and at the release).
    pub fn admitClient(self: *DataFence, vol_tag: u64, path: []const u8, device: ?*pr_linux.Device, client: *pr.Client) AdmitError!void {
        const gpa = self.in.gpa;
        var owned = true;
        defer if (owned) freeClient(gpa, device, client);
        client.key = self.in.key;
        client.rtype = nvme.RTYPE_WEAR;
        _ = client.wireHostId() catch return self.noteAbsent(vol_tag, path, .no_wire_host_id);
        const fresh = self.register(path, client) catch |e| {
            stats.data_pr_admission_refusals.inc();
            return e;
        };
        errdefer if (fresh) client.unregister(client.key) catch {};
        const acquired = self.reserve(path, client) catch |e| {
            stats.data_pr_admission_refusals.inc();
            return e;
        };
        if (self.in.manager) self.reconcile(path, client) catch |e| {
            stats.data_pr_admission_refusals.inc();
            return e;
        };
        const node = try gpa.dupe(u8, path);
        errdefer gpa.free(node);
        self.lock.lock();
        defer self.lock.unlock();
        if (self.n == self.vols.len) return error.Refused;
        self.vols[self.n] = .{ .vol_tag = vol_tag, .node = node, .device = device, .client = client, .fresh = fresh, .acquired = acquired };
        self.clients[self.n] = client;
        self.n += 1;
        owned = false;
        stats.data_pr_volumes.set(self.n);
        if (fresh) stats.data_pr_registrations.inc() else stats.data_pr_adoptions.inc();
        log.info("data volume {s}: vol-{x:0>16} {s} under the host's key {x:0>16} (WEAR{s}) — a fenced host's writes are refused by the device (decision 0003 §DJ)", .{ path, vol_tag, if (fresh) "registered this host" else "this host's registration adopted (no device mutation)", client.key, if (acquired) ", reserved by this mount" else "" });
    }

    fn noteAbsent(self: *DataFence, vol_tag: u64, path: []const u8, why: Absent) void {
        self.lock.lock();
        defer self.lock.unlock();
        if (self.n_absent < self.absent.len) {
            self.absent[self.n_absent] = .{ .vol_tag = vol_tag, .why = why };
            self.n_absent += 1;
        }
        stats.data_pr_volumes_unfenced.set(self.n_absent);
        // A file or a non-NVMe node is the single-kernel substrate class (said
        // once); an NVMe namespace that cannot be fenced is the operator's.
        if (why == .not_nvme_namespace) {
            log.info("data volume {s}: vol-{x:0>16} is not an NVMe namespace — no device fence (decision 0003 §DJ)", .{ path, vol_tag });
        } else log.warn("data volume {s}: vol-{x:0>16} carries no device fence ({t}) — a dead member's data writes are not refused there (the documented no-device-fence class, decision 0003 §DJ)", .{ path, vol_tag, why });
    }

    /// The host's registration: adopted when registered, else through the
    /// rule (with the registry) or the plain register (without); either way
    /// only while volume 0's metadata namespace registers this host.
    fn register(self: *DataFence, path: []const u8, c: *pr.Client) AdmitError!bool {
        const meta = self.meta;
        var path_word = path;
        const run: host_fence.Register = .{ .ctx = @ptrCast(&path_word), .run = &ladderRegisterFn };
        if (self.in.reg) |reg_in| {
            var reg = reg_in;
            reg.lock() catch {
                log.err("data volume {s}: the host's registry of this set ({s}) cannot be locked — refusing (the stale-sibling rule cannot run)", .{ path, reg.dir() });
                return error.Refused;
            };
            defer reg.unlock();
            const ctx: host_fence.RuleContext = .{ .gpa = self.in.gpa, .env = self.in.env, .path = path, .local = self.in.local, .node_token = self.in.node_token, .vol_index = 0, .taking_manager = false, .label = "data volume" };
            return host_fence.registerDataUnderRule(ctx, &reg, meta, c, self.in.pages, run) catch |e| switch (e) {
                error.HostFenced => {
                    log.err("data volume {s}: this host is not registered on the metadata namespace under the set's key {x:0>16} — fenced by a takeover or a death preempt: it never re-admits itself on data; refusing", .{ path, c.key });
                    return error.HostFenced;
                },
                error.OutOfMemory => error.OutOfMemory,
                else => {
                    log.err("data volume {s}: this host's data registration (key {x:0>16}) was refused ({s})", .{ path, c.key, @errorName(e) });
                    return error.Refused;
                },
            };
        }
        const f = host_fence.reportFacts(c) catch return reportRefusal(path, "the data namespace's report");
        if (f.registered) return false;
        if (!(try metaRegistered(meta, path))) return error.HostFenced;
        run.run(run.ctx, c) catch {
            log.err("data volume {s}: this host's data registration (key {x:0>16}) failed", .{ path, c.key });
            return error.Refused;
        };
        const after = metaRegistered(meta, path) catch |e| {
            c.unregister(c.key) catch {};
            return e;
        };
        if (!after) {
            c.unregister(c.key) catch {};
            return error.HostFenced;
        }
        return true;
    }

    /// The reservation: WEAR standing ⇒ nothing; none ⇒ acquire (another
    /// registrant's acquire racing it is fine); another rtype ⇒ refused.
    /// True = this process reserved it.
    fn reserve(self: *DataFence, path: []const u8, c: *pr.Client) AdmitError!bool {
        _ = self;
        const rtype = reportRtype(c) catch return reportRefusal(path, "the data namespace's report");
        if (rtype == nvme.RTYPE_WEAR) return false;
        if (rtype != 0) return foreignReservation(path, rtype);
        c.acquire() catch |e| switch (e) {
            error.ReservationConflict => {
                const now = reportRtype(c) catch return reportRefusal(path, "the data namespace's report");
                if (now == nvme.RTYPE_WEAR) return false;
                return foreignReservation(path, now);
            },
            else => {
                log.err("data volume {s}: the WEAR reservation could not be taken ({s})", .{ path, @errorName(e) });
                return error.Refused;
            },
        };
        stats.data_pr_reservations.inc();
        return true;
    }

    /// The manager's reconciliation: every data registrant ≠ ours that
    /// volume 0's metadata namespace no longer registers, preempted-and-
    /// aborted — the data report first, then the metadata's, after a report
    /// shows this host holding. A takeover between that report and a preempt
    /// changes nothing it preempts: every key it names is absent from the
    /// metadata namespace — a fenced host.
    fn reconcile(self: *DataFence, path: []const u8, c: *pr.Client) AdmitError!void {
        const gpa = self.in.gpa;
        const meta = self.meta;
        const held = meta.holdsHere() catch return reportRefusal(path, "the metadata namespace's report");
        if (!held) {
            log.err("data volume {s}: this mount no longer holds the metadata reservation — superseded during its open: it reconciles nothing and refuses", .{path});
            return error.Refused;
        }
        var drep = c.report() catch return reportRefusal(path, "the data namespace's report");
        defer drep.deinit(gpa);
        var mrep = meta.report() catch return reportRefusal(path, "the metadata namespace's report");
        defer mrep.deinit(gpa);
        var done: [router.MAX_VOLUMES]u64 = undefined;
        var n_done: usize = 0;
        outer: for (drep.registrants) |r| {
            if (r.rkey == c.key) continue;
            for (mrep.registrants) |mr| if (mr.rkey == r.rkey) continue :outer;
            for (done[0..n_done]) |k| if (k == r.rkey) continue :outer;
            c.preemptAndAbort(r.rkey) catch |e| {
                var buf: [128]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                c.writeError(&w, e) catch {};
                log.err("data volume {s}: the preempt of key {x:0>16} — registered on the data namespace, absent from the metadata namespace — failed ({s}); a fenced host would stay admitted: refusing", .{ path, r.rkey, w.buffered() });
                return error.Refused;
            };
            if (n_done < done.len) {
                done[n_done] = r.rkey;
                n_done += 1;
            }
            stats.data_pr_reconcile_preempts.inc();
            log.warn("data volume {s}: preempted key {x:0>16} — registered on the data namespace, absent from the metadata namespace (a superseded manager, or a host whose metadata fence landed alone): fenced before this manager allocates", .{ path, r.rkey });
        }
    }

    /// Recovery's step 2c (`recovery.DataFence.fence`).
    pub fn fenceDead(self: *DataFence, meta: ?*pr.Client, victim_key: u64) fencing.DataFenceOutcome {
        self.lock.lock();
        defer self.lock.unlock();
        return fencing.fenceDataNamespaces(meta, self.clients[0..self.n], victim_key);
    }

    /// The own-key case (`recovery.DataFence.killColocated`): the dead
    /// member's process on this host killed and every data node drained.
    pub fn killColocated(self: *DataFence, vol: u16, dead: *const appender_page.Page) bool {
        const reg = self.in.reg orelse {
            log.err("recovery data fence: the dead appender {d} shares this mount's key and no host registry stands (the per-mount seam, no host identifier) — no kill can hold it; the recovery holds", .{dead.appender_id});
            return false;
        };
        self.lock.lock();
        defer self.lock.unlock();
        var nodes: [router.MAX_VOLUMES][]const u8 = undefined;
        for (self.vols[0..self.n], 0..) |*v, i| nodes[i] = v.path();
        const ctx: host_fence.RuleContext = .{ .gpa = self.in.gpa, .env = self.in.env, .path = "", .local = self.in.local, .node_token = self.in.node_token, .vol_index = vol, .taking_manager = false, .label = "data volume" };
        return host_fence.killColocatedMember(ctx, reg, dead, nodes[0..self.n]);
    }

    pub fn recoverySeam(self: *DataFence) recovery.DataFence {
        return .{ .ctx = self, .fence = &fenceFn, .killColocated = &killFn };
    }

    fn fenceFn(ctx: *anyopaque, meta: ?*pr.Client, victim_key: u64) fencing.DataFenceOutcome {
        const self: *DataFence = @ptrCast(@alignCast(ctx));
        return self.fenceDead(meta, victim_key);
    }

    fn killFn(ctx: *anyopaque, vol: u16, dead: *const appender_page.Page) bool {
        const self: *DataFence = @ptrCast(@alignCast(ctx));
        return self.killColocated(vol, dead);
    }

    /// A re-join in place (decision 0003 §BS): the successor's metadata
    /// client and registry (its admission ran), then every data volume's
    /// registration again — the death preempt removed the host's. Null = re-
    /// admitted; else why not.
    pub fn readmit(self: *DataFence, meta_face: nvme.Namespace, meta_nsid: u32, key: u64, reg: ?host_fence.Registry, pages: host_fence.PageSource) ?[]const u8 {
        const gpa = self.in.gpa;
        self.lock.lock();
        defer self.lock.unlock();
        const meta = metaClient(gpa, meta_face, meta_nsid, key) catch return "out of memory re-admitting the data volumes";
        // The predecessor's inputs stay until every volume re-admitted (a
        // refused re-join closes the successor's set).
        const prior = self.in;
        const prior_meta = self.meta;
        self.in.meta_face = meta_face;
        self.in.meta_nsid = meta_nsid;
        self.in.key = key;
        self.in.reg = reg;
        self.in.pages = pages;
        self.meta = meta;
        for (self.vols[0..self.n]) |*v| {
            v.client.key = key;
            const fresh = self.register(v.path(), v.client) catch |e| {
                self.in = prior;
                self.meta = prior_meta;
                freeClient(gpa, null, meta);
                for (self.vols[0..self.n]) |*u| u.client.key = prior.key;
                return switch (e) {
                    error.HostFenced => "this host is not registered on the metadata namespace (fenced)",
                    error.OutOfMemory => "out of memory re-admitting the data volumes",
                    error.Refused => "a data volume's registration was refused",
                };
            };
            if (fresh) {
                v.fresh = true;
                stats.data_pr_registrations.inc();
            }
        }
        freeClient(gpa, null, prior_meta);
        return null;
    }

    /// The teardown, after the data plane closed its fds and before the
    /// metadata release: the host's last process of the set leaves each data
    /// namespace by the kept law; every client and node closed.
    pub fn release(self: *DataFence) void {
        const gpa = self.in.gpa;
        defer gpa.destroy(self);
        self.lock.lock();
        defer self.lock.unlock();
        var last = true;
        if (self.in.reg) |reg_in| {
            var reg = reg_in;
            if (reg.lock()) |_| {
                defer reg.unlock();
                last = (reg.othersAlive(self.in.gpa, self.in.local.pid) catch 1) == 0;
            } else |_| last = false;
        }
        for (self.vols[0..self.n]) |*v| {
            if (last) leave(v);
            freeClient(gpa, v.device, v.client);
            gpa.free(v.node);
        }
        self.n = 0;
        stats.data_pr_volumes.set(0);
        freeClient(gpa, null, self.meta);
    }

    /// Another registrant stands ⇒ unregister (WEAR keeps the reservation);
    /// the last registrant keeps its registration unless this incarnation
    /// reserved the namespace fresh and preempted nothing there.
    fn leave(v: *Volume) void {
        const c = v.client;
        const me = c.wireHostId() catch return;
        var rep = c.report() catch return;
        defer rep.deinit(c.gpa);
        var mine = false;
        var others = false;
        for (rep.registrants) |r| {
            if (r.rkey == c.key and std.mem.eql(u8, &r.host_id, &me)) mine = true else others = true;
        }
        if (!mine) return;
        if (!others and !(v.acquired and c.stats.preempts == 0)) {
            stats.data_pr_kept_at_release.inc();
            log.info("data volume {s}: the host's registration stays — the namespace's last registrant, and a released reservation would re-admit every host a preempt fenced (decision 0003 §DJ)", .{v.path()});
            return;
        }
        c.unregister(c.key) catch return;
        stats.data_pr_unregistered_at_release.inc();
    }

    /// `.stats`: the data namespaces this process is registered on and the
    /// volumes it writes with no device fence.
    pub fn writeStatsJson(self: *DataFence, w: *Io.Writer) Io.Writer.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        try w.writeAll("\"data_pr\":[");
        for (self.vols[0..self.n], 0..) |*v, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"data_vol_tag\":\"vol-{x:0>16}\",\"data_node\":\"{s}\",\"data_pr_key\":\"{x:0>16}\",\"data_pr_rtype\":{d},\"data_registered_fresh\":{},\"data_reserved_by_this_mount\":{},\"data_preempts_issued\":{d}}}", .{ v.vol_tag, v.path(), v.client.key, v.client.rtype, v.fresh, v.acquired, v.client.stats.preempts });
        }
        try w.writeAll("],\"data_pr_absent\":[");
        for (self.absent[0..self.n_absent], 0..) |a, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"data_vol_tag\":\"vol-{x:0>16}\",\"data_unfenced_why\":\"{t}\"}}", .{ a.vol_tag, a.why });
        }
        try w.writeByte(']');
    }
};

/// A writer's admission over its opened set (`mount`, the offline verbs
/// that open the data plane): volume 0's metadata posture — the guard's char
/// node and key (the manager, a flat set's writer), or the joiner's — then
/// every non-retired data volume admitted on the blocking pool (a PR command
/// parks its thread), before the data plane opens. Null = no data fence (no
/// metadata PR client: a file substrate, RESCAP 0, the per-mount seam's
/// joiner). `error.Refused`: `refused` names the volume (logged).
pub fn admitSet(gpa: Allocator, env: *const std.process.Environ.Map, io: Io, be0: *backend.KvBackend, data: []const set_config.DataVolume, refused: *?[]const u8) error{ Refused, OutOfMemory }!?*DataFence {
    const in = inputsOf(gpa, env, be0) orelse return null;
    const df = try DataFence.create(in);
    const sqz = exec.SqzIo.fromIo(io);
    const r = sqz.runBlocking(admitAllBlocking, .{ df, data }) catch {
        df.release();
        refused.* = "the blocking pool refused the admission";
        return error.Refused;
    };
    if (r) |d| {
        df.release();
        refused.* = d.uri;
        return error.Refused;
    }
    return df;
}

fn admitAllBlocking(df: *DataFence, data: []const set_config.DataVolume) ?*const set_config.DataVolume {
    if (!df.metaJudgeable()) {
        log.warn("data fence: the metadata namespace answers no host identifier (FID 0x81) — the data namespaces' registrations cannot be judged: no data volume is fenced (decision 0003 §DJ)", .{});
        return null;
    }
    for (data) |*d| {
        if (d.state == .retired) continue;
        df.admitVolume(d.vol_tag, d.uri) catch return d;
    }
    return null;
}

fn inputsOf(gpa: Allocator, env: *const std.process.Environ.Map, be0: *backend.KvBackend) ?Inputs {
    const pages: host_fence.PageSource = .{ .ctx = be0, .read = &volumePages };
    if (be0.guard) |g| {
        const c = g.prClient() orelse return null;
        const dev = g.device orelse return null;
        return .{ .gpa = gpa, .env = env, .meta_face = dev.face(), .meta_nsid = dev.nsid, .key = c.key, .reg = g.fence_reg, .local = g.identity, .node_token = join.nodeToken(g.identity), .pages = pages, .manager = true };
    }
    const js = be0.joined orelse return null;
    const f = js.fence orelse return null;
    const local = proc.localIdentity();
    return .{ .gpa = gpa, .env = env, .meta_face = f.device.face(), .meta_nsid = f.device.nsid, .key = f.client.key, .reg = f.reg, .local = local, .node_token = join.nodeToken(local), .pages = pages, .manager = false };
}

/// A re-join's successor (decision 0003 §BS): its volume 0's joiner posture.
pub fn readmitSuccessor(self: *DataFence, successor0: *backend.KvBackend) ?[]const u8 {
    const js = successor0.joined orelse return "the successor's volume 0 is not a joined incarnation";
    const f = js.fence orelse return null;
    return self.readmit(f.device.face(), f.device.nsid, f.client.key, f.reg, .{ .ctx = successor0, .read = &volumePages });
}

/// A volume's appender pages off its device (the rule's input).
fn volumePages(ctx: *anyopaque, gpa: Allocator) error{ Io, Corrupt, OutOfMemory }![]appender_page.Page {
    return joined.JoinedSet.devicePages(gpa, @ptrCast(@alignCast(ctx)));
}

fn freeClient(gpa: Allocator, device: ?*pr_linux.Device, c: *pr.Client) void {
    c.deinit();
    gpa.destroy(c);
    if (device) |d| d.deinit();
}

fn metaClient(gpa: Allocator, face: nvme.Namespace, nsid: u32, key: u64) Allocator.Error!*pr.Client {
    const c = try gpa.create(pr.Client);
    errdefer gpa.destroy(c);
    c.* = try pr.Client.init(gpa, face, nsid, key);
    return c;
}

fn ladderRegisterFn(ctx: *anyopaque, c: *pr.Client) anyerror!void {
    const path: *const []const u8 = @ptrCast(@alignCast(ctx));
    switch (try c.registerLadder()) {
        .registered => {},
        .recovered_own_stale => |n| log.warn("data volume {s}: the register ladder unregistered {d} stale registration(s) of this host (another key under its wire id) before registering key {x:0>16}", .{ path.*, n, c.key }),
    }
}

fn metaRegistered(meta: *pr.Client, path: []const u8) AdmitError!bool {
    const f = host_fence.reportFacts(meta) catch return reportRefusal(path, "the metadata namespace's report");
    return f.registered;
}

fn reportRtype(c: *pr.Client) pr.Error!u8 {
    var rep = try c.report();
    defer rep.deinit(c.gpa);
    return rep.rtype;
}

fn reportRefusal(path: []const u8, what: []const u8) AdmitError {
    log.err("data volume {s}: {s} failed — refusing (the data fence cannot be judged blind)", .{ path, what });
    return error.Refused;
}

fn foreignReservation(path: []const u8, rtype: u8) AdmitError {
    log.err("data volume {s}: the namespace is reserved with rtype {d}, not Write Exclusive – All Registrants — another tool's reservation; refusing (decision 0003 §DJ)", .{ path, rtype });
    return error.Refused;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// A fixture's client over a fake face (owned by the fence once admitted).
fn fixtureClient(gpa: Allocator, face: nvme.Namespace) !*pr.Client {
    if (!builtin.is_test) @compileError("fixtureClient is a fixture's");
    const c = try gpa.create(pr.Client);
    errdefer gpa.destroy(c);
    c.* = try pr.Client.init(gpa, face, 1, 0);
    return c;
}

const testing = std.testing;

fn noPages(_: *anyopaque, gpa: std.mem.Allocator) error{ Io, Corrupt, OutOfMemory }![]appender_page.Page {
    return gpa.alloc(appender_page.Page, 0);
}

test "the data admission registers this host on every DATA namespace before its first data write (decision 0003 §DJ): before it a reserved namespace refuses the host's write, after it admits it; the first registrant reserves WEAR, a later one only registers; a host already registered adopts with no device mutation; a host the metadata namespace no longer registers is refused, its data registration absent; the manager's admission preempts every data registrant the metadata namespace no longer registers (a superseded manager's host)" {
    const gpa = testing.allocator;
    const fake = @import("fake_nvme.zig");
    const m_id: [16]u8 = "manager-host-id1".*;
    const j_id: [16]u8 = "joiner-host-id-2".*;
    const o_id: [16]u8 = "old-manager-id-3".*;
    const f_id: [16]u8 = "fenced-host-id-4".*;
    // The metadata namespace: M holds WERO (it took over from O, whose key
    // the takeover removed), J registered.
    var mns = fake.Namespace.init(gpa);
    defer mns.deinit();
    var mh: fake.Host = .{ .ns = &mns, .host_id = m_id };
    var meta_m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer meta_m.deinit();
    try fencing.acquireManagerHold(&meta_m);
    var jh: fake.Host = .{ .ns = &mns, .host_id = j_id };
    var meta_j = try pr.Client.init(gpa, jh.face(), 1, 0x2222);
    defer meta_j.deinit();
    _ = try fencing.joinAsRegistrant(&meta_j, null);
    var fh: fake.Host = .{ .ns = &mns, .host_id = f_id };
    var meta_f = try pr.Client.init(gpa, fh.face(), 1, 0x4444);
    defer meta_f.deinit();
    // The data namespace: reserved WEAR by O, the superseded manager, still
    // registered there.
    var dns = fake.Namespace.init(gpa);
    defer dns.deinit();
    var dns2 = fake.Namespace.init(gpa);
    defer dns2.deinit();
    var doh: fake.Host = .{ .ns = &dns, .host_id = o_id };
    var data_o = try pr.Client.init(gpa, doh.face(), 1, 0x3333);
    defer data_o.deinit();
    data_o.rtype = nvme.RTYPE_WEAR;
    _ = try data_o.registerLadder();
    try data_o.acquire();
    try testing.expect(!dns.writeAllowed(m_id));
    const local = proc.localIdentity();
    var no_pages: u8 = 0;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    // M's admission (volume 0's writer): registered, the reservation kept,
    // O's key preempted (absent from the metadata namespace).
    const reconciles0 = stats.data_pr_reconcile_preempts.load();
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = true });
    defer dm.release();
    var dmh: fake.Host = .{ .ns = &dns, .host_id = m_id };
    try dm.admitClient(1, "fake-data", null, try fixtureClient(gpa, dmh.face()));
    try testing.expect(dns.writeAllowed(m_id));
    try testing.expect(!dns.writeAllowed(o_id));
    try testing.expectEqual(nvme.RTYPE_WEAR, dns.rtype);
    try testing.expectEqual(reconciles0 + 1, stats.data_pr_reconcile_preempts.load());
    // J (a joiner): refused by the device before its admission, admitted after.
    var djh: fake.Host = .{ .ns = &dns, .host_id = j_id };
    try testing.expect(!dns.writeAllowed(j_id));
    const dj = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = jh.face(), .meta_nsid = 1, .key = 0x2222, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = false });
    defer dj.release();
    try dj.admitClient(1, "fake-data", null, try fixtureClient(gpa, djh.face()));
    try testing.expect(dns.writeAllowed(j_id));
    // A second process of host J adopts: nothing on the device moves.
    const gen = dns.generation;
    const commands_before = dns.commands;
    const dj2 = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = jh.face(), .meta_nsid = 1, .key = 0x2222, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = false });
    defer dj2.release();
    try dj2.admitClient(1, "fake-data", null, try fixtureClient(gpa, djh.face()));
    try testing.expectEqual(gen, dns.generation);
    try testing.expect(dns.commands > commands_before);
    // Host F is not registered on the metadata namespace (fenced): refused,
    // never registered on data.
    const dff = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = fh.face(), .meta_nsid = 1, .key = 0x4444, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = false });
    defer dff.release();
    var dfh: fake.Host = .{ .ns = &dns, .host_id = f_id };
    try testing.expectError(error.HostFenced, dff.admitClient(1, "fake-data", null, try fixtureClient(gpa, dfh.face())));
    try testing.expect(!dns.writeAllowed(f_id));
    try testing.expect(!dns.isRegistered(0x4444));
    // An unreserved namespace: the first registrant reserves WEAR.
    var d2h: fake.Host = .{ .ns = &dns2, .host_id = j_id };
    try dj.admitClient(2, "fake-data-2", null, try fixtureClient(gpa, d2h.face()));
    try testing.expectEqual(nvme.RTYPE_WEAR, dns2.rtype);
    try testing.expect(!dns2.writeAllowed(m_id));
}

test "review MAJOR 1 (decision 0003 §DJ): the reconnect-at-mount interleaving — the manager mounts while data target T2 reconnects (its node absent at the admission): the admission refuses T2, loud, and never records it as carrying no device fence ('not an NVMe namespace' — a member registered there by its own admission would then die unfenced on T2 while the recovery fences T1 alone)" {
    const gpa = testing.allocator;
    const fake = @import("fake_nvme.zig");
    const m_id: [16]u8 = "manager-host-id1".*;
    var mns = fake.Namespace.init(gpa);
    defer mns.deinit();
    var mh: fake.Host = .{ .ns = &mns, .host_id = m_id };
    var meta_m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer meta_m.deinit();
    try fencing.acquireManagerHold(&meta_m);
    var no_pages: u8 = 0;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = proc.localIdentity(), .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .manager = true });
    defer dm.release();
    // T1 is admitted.
    var t1 = fake.Namespace.init(gpa);
    defer t1.deinit();
    var t1h: fake.Host = .{ .ns = &t1, .host_id = m_id };
    try dm.admitClient(1, "fake-t1", null, try fixtureClient(gpa, t1h.face()));
    // T2's controller reconnects: its node is absent right now.
    var pb: [96]u8 = undefined;
    const t2 = try std.fmt.bufPrint(&pb, "/dev/nvme-sqz-reconnecting-{d}n1", .{linux.getpid()});
    const refusals0 = stats.data_pr_admission_refusals.load();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Refused, dm.admitVolume(2, t2));
    try log.testing.expectAllFired();
    try testing.expectEqual(refusals0 + 1, stats.data_pr_admission_refusals.load());
    try testing.expectEqual(@as(usize, 0), dm.n_absent);
}

/// A fixture's set config: the non-retired data volumes' tags.
const FixtureConfig = struct {
    live: []const u64,
    fail: bool = false,

    fn source(self: *FixtureConfig) ConfigSource {
        return .{ .ctx = self, .read = &readFn };
    }

    fn readFn(ctx: *anyopaque, _: Allocator, out: *[router.MAX_VOLUMES]u64) ConfigSource.Error!usize {
        const self: *FixtureConfig = @ptrCast(@alignCast(ctx));
        if (self.fail) return error.Io;
        @memcpy(out[0..self.live.len], self.live);
        return self.live.len;
    }
};

test "review MAJOR 1 (decision 0003 §DJ): step 2c holds unless every non-retired data volume of the SET CONFIG is fenced or provably unfenceable — a volume the manager's table lacks holds the recovery, loud, its gauge raised, nothing preempted; the operator's waiver releases it at once; a volume retired meanwhile leaves the table at the next fence (no remount); an unreadable set config holds" {
    const gpa = testing.allocator;
    const fake = @import("fake_nvme.zig");
    const m_id: [16]u8 = "manager-host-id1".*;
    const x_id: [16]u8 = "dead-member-id-2".*;
    var mns = fake.Namespace.init(gpa);
    defer mns.deinit();
    var mh: fake.Host = .{ .ns = &mns, .host_id = m_id };
    var meta_m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer meta_m.deinit();
    try fencing.acquireManagerHold(&meta_m);
    var no_pages: u8 = 0;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var cfg: FixtureConfig = .{ .live = &.{ 1, 2 } };
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = proc.localIdentity(), .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .config = cfg.source(), .manager = true });
    defer dm.release();
    // T1 admitted (M reserves WEAR); the member X registered there.
    var t1 = fake.Namespace.init(gpa);
    defer t1.deinit();
    var t1h: fake.Host = .{ .ns = &t1, .host_id = m_id };
    try dm.admitClient(1, "fake-t1", null, try fixtureClient(gpa, t1h.face()));
    var xh: fake.Host = .{ .ns = &t1, .host_id = x_id };
    var data_x = try pr.Client.init(gpa, xh.face(), 1, 0x2222);
    defer data_x.deinit();
    data_x.rtype = nvme.RTYPE_WEAR;
    _ = try data_x.registerLadder();
    // T2 is in the set config but in no table row (its admission never
    // landed): X's death holds, nothing preempted.
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(fencing.DataFenceOutcome.failed, dm.fenceDead(&meta_m, 0x2222));
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 1), stats.data_pr_fence_held_volumes.load());
    try testing.expect(t1.isRegistered(0x2222));
    // The operator's escape (`volume waive-data-fence`): T2 counts as
    // unfenceable; the next poll fences T1 and converges.
    const waivers0 = stats.data_pr_operator_waivers.load();
    log.testing.expectErrorLogs(1);
    try dm.waive(2);
    try log.testing.expectAllFired();
    try testing.expectEqual(waivers0 + 1, stats.data_pr_operator_waivers.load());
    try testing.expectEqual(fencing.DataFenceOutcome.fenced, dm.fenceDead(&meta_m, 0x2222));
    try testing.expect(!t1.isRegistered(0x2222));
    try testing.expectEqual(@as(u64, 0), stats.data_pr_fence_held_volumes.load());
    // T1 retired (and T2 waived): T1 leaves the table at the next fence.
    cfg.live = &.{2};
    try testing.expectEqual(fencing.DataFenceOutcome.no_device, dm.fenceDead(&meta_m, 0x3333));
    try testing.expectEqual(@as(usize, 0), dm.n);
    // An unreadable set config holds.
    cfg.fail = true;
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(fencing.DataFenceOutcome.failed, dm.fenceDead(&meta_m, 0x3333));
    try log.testing.expectAllFired();
}
