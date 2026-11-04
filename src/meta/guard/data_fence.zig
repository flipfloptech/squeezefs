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
//! after a report shows this host holding. Only a STABLE fact leaves a data
//! volume without a device fence (a file, a non-NVMe block node, RESCAP 0, a
//! controller answering no host identifier): a node absent or unreadable
//! right now, an Identify that failed, refuses the admission (review MAJOR 1:
//! a reconnecting target read as "not NVMe" stayed unfenced for the mount's
//! life). Only the manager reserves (a joiner registers), and only once every
//! host registered on volume 0's metadata namespace is registered on the data
//! namespace — else the lapse beat reserves after the grace.
//!
//! THE LAPSE BEAT (review MAJOR 2; heartbeat cadence, never per I/O): nvmet
//! keeps no PTPL — a target restart forgets every registration and
//! reservation. Every writer's beat reads each data namespace: unreserved and
//! this host unregistered ⇒ re-register through the rule (the metadata
//! registration checked before and after, its own page Live under its writer
//! id before and after); the manager, once it holds volume 0's metadata
//! namespace and every metadata registrant is back on data (or
//! `claim.LAPSE_GRACE_S` passed), re-reserves WEAR and reconciles. A joiner's
//! beat re-registers its host on the metadata namespace the same way
//! (unreserved, its page Live); the manager's guard re-acquires the hold once
//! every live member's host key in the census is registered again, else after
//! the same grace, and kicks this beat when it first sees the lapse and when
//! the hold returns (decision 0003 §DL). A member slow on data — refused by
//! the re-reserved namespace while a lapse is still pending for it (its beat
//! saw one and has not since seen itself registered under a reservation) — is
//! re-admitted at the refused command: registered on volume 0's metadata
//! namespace and current, before and after a register through the rule; the
//! device issues the command once more. Anything else latches the fence.
//!
//! THE DEATH (`recovery.zig` step 2c, `fencing.fenceDataNamespaces`): the
//! dead host's key preempted on every data namespace and proven before
//! anything the member can write is reallocated, else the recovery holds —
//! every non-retired data volume of the SET CONFIG fenced or provably
//! unfenceable (a retired one leaves the table; the operator's
//! `volume waive-data-fence` escape for a target gone for good); a
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
const knobs = @import("../../core/knobs.zig");
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const tick = @import("../../sync/tick.zig");
const meta_key = @import("../key.zig");
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
const nvme_dev = @import("../../io/nvme_dev.zig");
const gate = @import("gate.zig");

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
    /// Data volumes the last recovery's data fence could not prove (a
    /// set-config volume outside the table, an unproven preempt): the alert.
    pub var data_pr_fence_held_volumes: counters.Gauge = .{};
    /// `volume waive-data-fence`: data volumes the operator declared
    /// unfenceable on this manager.
    pub var data_pr_operator_waivers: counters.ShardedCounter = .{};
    /// The lapse beat: data namespaces the manager found unreserved, this
    /// host's re-registrations there, the manager's re-reservations; a
    /// joiner's re-registrations on volume 0's metadata namespace.
    pub var data_pr_lapses: counters.ShardedCounter = .{};
    pub var data_pr_lapse_reregistrations: counters.ShardedCounter = .{};
    pub var data_pr_lapse_rereservations: counters.ShardedCounter = .{};
    pub var meta_pr_lapse_reregistrations: counters.ShardedCounter = .{};
    /// Decision 0003 §DL: beats that found a data namespace unreserved (the
    /// lapse's length in beats); re-reservations with every metadata
    /// registrant back before the grace; the guard's kicks of this beat; a
    /// refused data command's re-admissions, and the refusals it declined
    /// (the fence latched).
    pub var data_pr_lapse_beats: counters.ShardedCounter = .{};
    pub var data_pr_lapse_early_rereservations: counters.ShardedCounter = .{};
    pub var data_pr_lapse_kicks: counters.ShardedCounter = .{};
    pub var data_pr_refusal_readmissions: counters.ShardedCounter = .{};
    pub var data_pr_refusal_readmissions_declined: counters.ShardedCounter = .{};
};

/// Why a data volume carries no device fence — stable facts only (review
/// MAJOR 1), and the operator's waiver.
pub const Absent = enum {
    not_nvme_namespace,
    rescap_zero,
    /// The data controller (or volume 0's metadata controller) answers no
    /// host identifier (FID 0x81 zeros): the registration cannot be judged.
    no_wire_host_id,
    /// `volume waive-data-fence`: the operator confirmed out of band that no
    /// dead member reaches the target (a target gone for good).
    operator_waived,
};

/// The durable set config's non-retired data volumes (tree 0 of volume 0,
/// `set_config.CONTROL_KEY`): what step 2c must find fenced or provably
/// unfenceable, and what a retirement takes out of the table.
pub const ConfigSource = struct {
    ctx: *anyopaque,
    read: *const fn (ctx: *anyopaque, gpa: Allocator, out: *[router.MAX_VOLUMES]u64) Error!usize,

    pub const Error = error{ Io, Undecodable, TooManyVolumes, OutOfMemory };
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
    /// Volume 0's metadata node (a joiner's lapse re-registration drains it).
    meta_path: []const u8,
    /// This incarnation's writer id: its page Live under it is its currency
    /// (the manager's appender-0 page, a joiner's page).
    writer_id: u64,
    config: ConfigSource,
    /// Volume 0's writer (the manager, or a flat set's): it reserves and
    /// reconciles.
    manager: bool,
    /// Volume 0's guard (the manager's): it kicks the lapse beat (§DL).
    guard: ?*gate.Guard = null,
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
    /// The manager's word: when it saw the namespace unreserved (monotonic
    /// ns; 0 = reserved).
    lapse_since_ns: u64 = 0,
    /// A lapse this process saw (here, on volume 0's metadata namespace, or
    /// the guard's) and has not since seen this host registered here under
    /// a reservation: a refused command here is a late re-registration's
    /// case, never a fence's (§DL).
    lapse_pending: bool = false,

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
    /// The alert's last held set and when it was said (one line per change,
    /// else one per `ALERT_EVERY_NS`).
    alert_sig: u64 = 0,
    alert_ns: u64 = 0,
    /// The lapse beat (`startBeat`; the offline verbs run none): its park
    /// word (a stop or a kick wakes it), the stop.
    io: ?Io = null,
    beat_wake: std.atomic.Value(u32) = .init(0),
    beat_stop: std.atomic.Value(bool) = .init(false),
    beat_fut: ?Io.Future(void) = null,
    /// The guard saw a metadata lapse (its kick): every volume's lapse
    /// pending, taken under the table's lock.
    lapse_flag: std.atomic.Value(bool) = .init(false),
    /// The devices' re-admission hooks, one per tabled volume, never moved
    /// (a device holds its address until the plane closes).
    readmit_cells: [router.MAX_VOLUMES]ReadmitCell = undefined,
    n_cells: usize = 0,
    /// The lapse grace (`claim.lapseGraceNs`).
    grace_ns: u64 = claim.LAPSE_GRACE_S * std.time.ns_per_s,
    /// `SQUEEZEFS_TEST_LAPSE_BEAT_HOLD`: the FIFO the first beat that
    /// re-registered the host on metadata parks on (empty = disarmed).
    beat_hold: [256:0]u8 = @splat(0),

    /// The alert's repeat: a held recovery polls every tick; one line per
    /// half minute keeps the log readable and the alert loud.
    const ALERT_EVERY_NS: u64 = 30 * std.time.ns_per_s;

    /// What a held recovery's alert names (with the volumes or the key, its
    /// signature).
    const AlertWhy = enum(u8) { config_unreadable, untabled, unproven };

    const ReadmitCell = struct { hook: nvme_dev.Readmit, fence: *DataFence, vol_tag: u64 };

    pub fn create(in: Inputs) Allocator.Error!*DataFence {
        const self = try in.gpa.create(DataFence);
        errdefer in.gpa.destroy(self);
        self.* = .{ .in = in, .meta = try metaClient(in.gpa, in.meta_face, in.meta_nsid, in.key), .grace_ns = claim.lapseGraceNs(in.env) };
        if (knobs.parseStr(in.env.get("SQUEEZEFS_TEST_LAPSE_BEAT_HOLD"))) |fifo| {
            if (!std.mem.eql(u8, fifo, "off") and fifo.len < self.beat_hold.len) @memcpy(self.beat_hold[0..fifo.len], fifo);
        }
        return self;
    }

    /// Volume 0's metadata namespace's host identifier: answered (the
    /// registrations are judged by it), zeros (a stable fact: no data volume
    /// can be fenced), or unreadable now (the admission refuses).
    pub fn metaHostId(self: *DataFence) enum { known, unknown, unreadable } {
        _ = self.meta.wireHostId() catch |e| return if (e == error.HostIdUnknown) .unknown else .unreadable;
        return .known;
    }

    /// The admission of one data volume by its live node path. Only a
    /// stable fact leaves it without a device fence; anything transient
    /// refuses (review MAJOR 1).
    pub fn admitVolume(self: *DataFence, vol_tag: u64, path: []const u8) AdmitError!void {
        const gpa = self.in.gpa;
        switch (pr_linux.classifyNode(path)) {
            .not_nvme => return self.noteAbsent(vol_tag, path, .not_nvme_namespace),
            .unknown => return transientRefusal(path, "its node is absent or unreadable right now (a controller reconnecting?)", null),
            .nvme => {},
        }
        // An NVMe namespace on 7.3 always has its generic char twin and its
        // controller node: one that cannot be opened now is transient.
        const dev = switch (pr_linux.resolve(gpa, path)) {
            .none => |why| return transientRefusal(path, "its char node, controller node or PR ring cannot be opened right now", @tagName(why)),
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
        const rescap = c.probeRescap() catch |e| {
            freeClient(gpa, dev, c);
            return transientRefusal(path, "its Identify Namespace (RESCAP) failed", @errorName(e));
        };
        if (rescap == 0) {
            freeClient(gpa, dev, c);
            return self.noteAbsent(vol_tag, path, .rescap_zero);
        }
        return self.admitClient(vol_tag, path, dev, c);
    }

    /// The admission's core over a client (owned from here: freed on a
    /// refusal and at the release), one PR sequence of this fence at a time.
    pub fn admitClient(self: *DataFence, vol_tag: u64, path: []const u8, device: ?*pr_linux.Device, client: *pr.Client) AdmitError!void {
        const gpa = self.in.gpa;
        var owned = true;
        defer if (owned) freeClient(gpa, device, client);
        client.key = self.in.key;
        client.rtype = nvme.RTYPE_WEAR;
        _ = client.wireHostId() catch |e| {
            if (e == error.HostIdUnknown) return self.noteAbsent(vol_tag, path, .no_wire_host_id);
            return transientRefusal(path, "its controller's host identifier (FID 0x81) could not be read", @errorName(e));
        };
        const node = try gpa.dupe(u8, path);
        errdefer gpa.free(node);
        self.lock.lock();
        defer self.lock.unlock();
        if (self.n == self.vols.len) return error.Refused;
        const fresh = self.register(path, client) catch |e| {
            stats.data_pr_admission_refusals.inc();
            return e;
        };
        errdefer if (fresh) client.unregister(client.key) catch {};
        var acquired = false;
        var lapse_since_ns: u64 = 0;
        if (self.in.manager) {
            // The reservation returns only once every live host is
            // registered here — else the beat takes it after the grace (a
            // reservation taken first refuses every member not yet back).
            const ready = self.everyMetaRegistrantOnData(client) catch return reportRefusal(path, "the reports before the reservation");
            if (ready) {
                acquired = self.reserve(path, client) catch |e| {
                    stats.data_pr_admission_refusals.inc();
                    return e;
                };
                self.reconcile(path, client) catch |e| {
                    stats.data_pr_admission_refusals.inc();
                    return e;
                };
            } else {
                const rtype = reportRtype(client) catch return reportRefusal(path, "the data namespace's report");
                if (rtype != 0 and rtype != nvme.RTYPE_WEAR) return foreignReservation(path, rtype);
                if (rtype == 0) {
                    lapse_since_ns = @max(clock.monotonicNs(), 1);
                } else self.reconcile(path, client) catch |e| {
                    stats.data_pr_admission_refusals.inc();
                    return e;
                };
            }
        }
        self.vols[self.n] = .{ .vol_tag = vol_tag, .node = node, .device = device, .client = client, .fresh = fresh, .acquired = acquired, .lapse_since_ns = lapse_since_ns };
        self.clients[self.n] = client;
        self.n += 1;
        owned = false;
        stats.data_pr_volumes.set(self.n);
        if (fresh) stats.data_pr_registrations.inc() else stats.data_pr_adoptions.inc();
        log.info("data volume {s}: vol-{x:0>16} {s} under the host's key {x:0>16} (WEAR{s}) — a fenced host's writes are refused by the device (decision 0003 §DJ)", .{ path, vol_tag, if (fresh) "registered this host" else "this host's registration adopted (no device mutation)", client.key, if (acquired) ", reserved by this mount" else if (lapse_since_ns != 0) "; reserved by the beat once every live host is back, or after the grace" else "" });
    }

    fn noteAbsent(self: *DataFence, vol_tag: u64, path: []const u8, why: Absent) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.noteAbsentLocked(vol_tag, why);
        // A file or a non-NVMe node is the single-kernel substrate class (said
        // once); an NVMe namespace that cannot be fenced is the operator's.
        if (why == .not_nvme_namespace) {
            log.info("data volume {s}: vol-{x:0>16} is not an NVMe namespace — no device fence (decision 0003 §DJ)", .{ path, vol_tag });
        } else log.warn("data volume {s}: vol-{x:0>16} carries no device fence ({t}) — a dead member's data writes are not refused there (the documented no-device-fence class, decision 0003 §DJ)", .{ path, vol_tag, why });
    }

    fn noteAbsentLocked(self: *DataFence, vol_tag: u64, why: Absent) void {
        if (self.absentIndex(vol_tag)) |i| {
            self.absent[i].why = why;
        } else if (self.n_absent < self.absent.len) {
            self.absent[self.n_absent] = .{ .vol_tag = vol_tag, .why = why };
            self.n_absent += 1;
        }
        stats.data_pr_volumes_unfenced.set(self.n_absent);
    }

    fn tabledIndex(self: *const DataFence, vol_tag: u64) ?usize {
        for (self.vols[0..self.n], 0..) |*v, i| if (v.vol_tag == vol_tag) return i;
        return null;
    }

    fn absentIndex(self: *const DataFence, vol_tag: u64) ?usize {
        for (self.absent[0..self.n_absent], 0..) |a, i| if (a.vol_tag == vol_tag) return i;
        return null;
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
                // The key may have left since the data report (a joiner
                // leaving data, then metadata, review minor 1): absent now
                // is the fence's end state.
                const gone = !(keyOnNamespace(c, r.rkey) catch true);
                if (gone) continue :outer;
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

    /// Recovery's step 2c (`recovery.DataFence.fence`): every non-retired
    /// data volume of the set config (review MAJOR 1: not only this table)
    /// fenced or provably unfenceable, else the recovery holds — alerted.
    pub fn fenceDead(self: *DataFence, meta: ?*pr.Client, victim_key: u64) fencing.DataFenceOutcome {
        self.lock.lock();
        defer self.lock.unlock();
        var live: [router.MAX_VOLUMES]u64 = undefined;
        const n_live = self.in.config.read(self.in.config.ctx, self.in.gpa, &live) catch |e| {
            stats.data_pr_fence_held_volumes.set(1);
            if (self.alertDue(.config_unreadable, 0)) log.err("recovery data fence: the set config (tree 0 of volume 0) could not be read ({s}) — which data volumes the dead member (key {x:0>16}) can write is unknown: the recovery HOLDS (decision 0003 §DJ)", .{ @errorName(e), victim_key });
            return .failed;
        };
        self.pruneLocked(live[0..n_live]);
        var held: usize = 0;
        var sig: u64 = 0;
        var first: u64 = 0;
        for (live[0..n_live]) |tag| {
            if (self.tabledIndex(tag) != null or self.absentIndex(tag) != null) continue;
            if (held == 0) first = tag;
            held += 1;
            sig = std.hash.Wyhash.hash(sig, std.mem.asBytes(&tag));
        }
        if (held != 0) {
            stats.data_pr_fence_held_volumes.set(held);
            if (self.alertDue(.untabled, sig)) log.err("recovery data fence: vol-{x:0>16} ({d} data volume(s) in all) is in the set config but neither admitted on this manager nor provably unfenceable — the dead member (key {x:0>16}) may write there: the recovery HOLDS, its slots leased and its grant windows set. {s}", .{ first, held, victim_key, escape_text });
            return .failed;
        }
        var unproven: usize = 0;
        const out = fencing.fenceDataNamespacesCounted(meta, self.clients[0..self.n], victim_key, &unproven);
        stats.data_pr_fence_held_volumes.set(unproven);
        if (out == .failed) {
            if (self.alertDue(.unproven, victim_key)) log.err("recovery data fence: the dead member's host (key {x:0>16}) is not proven fenced on {d} data volume(s) — the recovery HOLDS (the next poll re-runs). {s}", .{ victim_key, unproven, escape_text });
        } else self.alert_sig = 0;
        return out;
    }

    const escape_text = "A target gone for good: fence the dead host out of band (power it off, or cut its host NQN from the target's allowed hosts), then run `squeezefs volume waive-data-fence <mountpoint> vol-<16 hex>` on this manager (decision 0003 §DJ)";

    /// One line per held set, else one per `ALERT_EVERY_NS`.
    fn alertDue(self: *DataFence, why: AlertWhy, payload: u64) bool {
        const sig = std.hash.Wyhash.hash(@backingInt(why), std.mem.asBytes(&payload));
        const now = clock.monotonicNs();
        if (sig == self.alert_sig and now -| self.alert_ns < ALERT_EVERY_NS) return false;
        self.alert_sig = sig;
        self.alert_ns = now;
        return true;
    }

    /// A volume retired (or gone) from the set config leaves the table at
    /// once — never a fence of a namespace the set no longer holds. Its
    /// client closes without a command (the target may be unreachable): its
    /// registration stays there (§DJ.12 (c)).
    fn pruneLocked(self: *DataFence, live: []const u64) void {
        var i: usize = 0;
        while (i < self.n) {
            if (std.mem.indexOfScalar(u64, live, self.vols[i].vol_tag) != null) {
                i += 1;
                continue;
            }
            const v = self.vols[i];
            log.info("data volume vol-{x:0>16}: retired from the set config — it leaves the data fence's table", .{v.vol_tag});
            freeClient(self.in.gpa, v.device, v.client);
            self.in.gpa.free(v.node);
            self.n -= 1;
            self.vols[i] = self.vols[self.n];
            self.clients[i] = self.clients[self.n];
        }
        stats.data_pr_volumes.set(self.n);
        i = 0;
        while (i < self.n_absent) {
            if (std.mem.indexOfScalar(u64, live, self.absent[i].vol_tag) != null) {
                i += 1;
                continue;
            }
            self.n_absent -= 1;
            self.absent[i] = self.absent[self.n_absent];
        }
        stats.data_pr_volumes_unfenced.set(self.n_absent);
    }

    /// `volume waive-data-fence` (the operator's escape for a target gone for
    /// good, review MAJOR 1): the volume counts as unfenceable on this
    /// manager from now on — its client closed, the absent row
    /// `operator_waived`. The operator confirmed out of band that no dead
    /// member reaches the target. For this manager's life (no format
    /// change); the next mount admits the volume again.
    pub fn waive(self: *DataFence, vol_tag: u64) error{ UnknownVolume, ConfigUnreadable }!void {
        self.lock.lock();
        defer self.lock.unlock();
        var live: [router.MAX_VOLUMES]u64 = undefined;
        const n_live = self.in.config.read(self.in.config.ctx, self.in.gpa, &live) catch return error.ConfigUnreadable;
        if (std.mem.indexOfScalar(u64, live[0..n_live], vol_tag) == null) return error.UnknownVolume;
        if (self.tabledIndex(vol_tag)) |i| {
            const v = self.vols[i];
            freeClient(self.in.gpa, v.device, v.client);
            self.in.gpa.free(v.node);
            self.n -= 1;
            self.vols[i] = self.vols[self.n];
            self.clients[i] = self.clients[self.n];
            stats.data_pr_volumes.set(self.n);
        }
        self.noteAbsentLocked(vol_tag, .operator_waived);
        stats.data_pr_operator_waivers.inc();
        log.err("data volume vol-{x:0>16}: the operator WAIVED its data fence (`volume waive-data-fence`) — a dead member's writes to it are no longer refused by its device on this manager; the operator confirmed out of band that no dead member reaches that target (decision 0003 §DJ)", .{vol_tag});
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
    pub fn readmit(self: *DataFence, s: Successor) ?[]const u8 {
        const gpa = self.in.gpa;
        self.lock.lock();
        defer self.lock.unlock();
        const meta = metaClient(gpa, s.meta_face, s.meta_nsid, s.key) catch return "out of memory re-admitting the data volumes";
        // The predecessor's inputs stay until every volume re-admitted (a
        // refused re-join closes the successor's set).
        const prior = self.in;
        const prior_meta = self.meta;
        self.in.meta_face = s.meta_face;
        self.in.meta_nsid = s.meta_nsid;
        self.in.key = s.key;
        self.in.reg = s.reg;
        self.in.pages = s.pages;
        self.in.meta_path = s.meta_path;
        self.in.writer_id = s.writer_id;
        self.in.node_token = s.node_token;
        self.meta = meta;
        for (self.vols[0..self.n]) |*v| {
            v.client.key = s.key;
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

    /// A re-join's successor's inputs (decision 0003 §BS).
    pub const Successor = struct {
        meta_face: nvme.Namespace,
        meta_nsid: u32,
        key: u64,
        reg: ?host_fence.Registry,
        pages: host_fence.PageSource,
        meta_path: []const u8,
        writer_id: u64,
        node_token: u64,
    };

    // ---- the lapse beat (review MAJOR 2) ----

    /// Spawn the lapse beat on the `sqz-meta` pool: every
    /// `claim.HEARTBEAT_INTERVAL_S`, its PR work on the blocking pool.
    pub fn startBeat(self: *DataFence, io: Io) error{ThreadSpawnFailed}!void {
        self.io = io;
        const sqz = exec.SqzIo.fromIo(io);
        self.beat_fut = sqz.spawnMeta(beatMain, .{self}) catch return error.ThreadSpawnFailed;
        // After `io` is set: a kick wakes the parked beat (§DL).
        if (self.in.guard) |g| g.bindDataKick(.{ .ctx = self, .kick = &kickFn });
    }

    fn stopBeat(self: *DataFence) void {
        if (self.in.guard) |g| g.bindDataKick(null);
        const fut = &(self.beat_fut orelse return);
        const io = self.io.?;
        // Release pairs with `beatMain`'s acquire load of the stop; the wake
        // ends a parked beat at once.
        self.beat_stop.store(true, .release);
        tick.wake(io, &self.beat_wake);
        fut.await(io);
        self.beat_fut = null;
    }

    /// The guard's kick (decision 0003 §DL): it saw the metadata lapse
    /// (`lapse`) or its hold returned — the beat runs now, not at its next
    /// interval.
    fn kickFn(ctx: *anyopaque, lapse: bool) void {
        const self: *DataFence = @ptrCast(@alignCast(ctx));
        // Release pairs with the beat's acquire swap of the flag.
        if (lapse) self.lapse_flag.store(true, .release);
        stats.data_pr_lapse_kicks.inc();
        tick.wake(self.io.?, &self.beat_wake);
    }

    fn beatMain(self: *DataFence) void {
        const io = self.io.?;
        const interval_ns: u64 = claim.HEARTBEAT_INTERVAL_S * std.time.ns_per_s;
        while (true) {
            const deadline = clock.monotonicNs() + interval_ns;
            while (self.beat_wake.load(.acquire) == 0) {
                if (tick.parkUntilUncancelable(io, &self.beat_wake, deadline, null) == .deadline) break;
            }
            // Acquire pairs with `stopBeat`'s release store: no beat after
            // the release.
            if (self.beat_stop.load(.acquire)) return;
            // A kick is spent by the beat it starts (acquire pairs with
            // `tick.wake`'s release swap).
            _ = self.beat_wake.swap(0, .acquire);
            exec.SqzIo.fromIo(io).runBlocking(beat, .{self}) catch {};
        }
    }

    /// Every volume's lapse pending (the table's lock held).
    fn noteLapseLocked(self: *DataFence) void {
        for (self.vols[0..self.n]) |*v| v.lapse_pending = true;
    }

    /// The guard's lapse word taken (the table's lock held).
    fn takeLapseFlagLocked(self: *DataFence) void {
        // Acquire pairs with `kickFn`'s release store.
        if (self.lapse_flag.swap(false, .acquire)) self.noteLapseLocked();
    }

    /// One lapse beat: a joiner's metadata namespace, then every data
    /// namespace. A namespace whose report fails is skipped until the next
    /// beat. (A retired volume leaves the table at the next fence, which
    /// reads the set config under the table's lock.)
    pub fn beat(self: *DataFence) void {
        if (!self.in.manager) {
            self.lock.lock();
            const fresh = self.metaLapseBeat();
            self.lock.unlock();
            if (fresh) self.testBeatHold();
        }
        self.lock.lock();
        defer self.lock.unlock();
        self.takeLapseFlagLocked();
        const now = clock.monotonicNs();
        for (self.vols[0..self.n]) |*v| self.volumeBeat(v, now);
    }

    /// `SQUEEZEFS_TEST_LAPSE_BEAT_HOLD`, once: park until a writer opens the
    /// FIFO (a member slow on data past the manager's re-reservation).
    fn testBeatHold(self: *DataFence) void {
        if (self.beat_hold[0] == 0) return;
        const fifo: [:0]const u8 = std.mem.sliceTo(&self.beat_hold, 0);
        log.warn("test seam: SQUEEZEFS_TEST_LAPSE_BEAT_HOLD — the lapse beat parks after its metadata re-registration, before its data namespaces, until a writer opens {s}", .{fifo});
        const rc = linux.open(fifo.ptr, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) == .SUCCESS) _ = linux.close(@intCast(rc));
        self.beat_hold[0] = 0;
        log.warn("test seam: SQUEEZEFS_TEST_LAPSE_BEAT_HOLD — the lapse beat resumes", .{});
    }

    /// A data namespace after a lapse (nvmet keeps no PTPL): unreserved and
    /// this host unregistered ⇒ re-register through the rule while its
    /// metadata registration and its currency hold; the manager re-reserves
    /// once it holds the metadata namespace and every metadata registrant is
    /// back here, or the grace passed, then reconciles. Never under WEAR:
    /// this host unregistered there is fenced.
    fn volumeBeat(self: *DataFence, v: *Volume, now: u64) void {
        const c = v.client;
        const f = host_fence.reportFacts(c) catch return;
        if (f.facts.reserved) {
            v.lapse_since_ns = 0;
            // The lapse is over for this host here (§DL).
            if (f.registered) v.lapse_pending = false;
            return;
        }
        v.lapse_pending = true;
        stats.data_pr_lapse_beats.inc();
        if (self.in.manager and v.lapse_since_ns == 0) {
            v.lapse_since_ns = @max(now, 1);
            stats.data_pr_lapses.inc();
            log.warn("data volume {s}: the namespace carries no reservation (a PTPL-less target restart? nvmet keeps none) — every live host re-registers at its beat; re-reserved once they are back or after {d} s (decision 0003 §DJ)", .{ v.path(), self.grace_ns / std.time.ns_per_s });
        }
        if (!f.registered) {
            const mf = host_fence.reportFacts(self.meta) catch return;
            if (!mf.registered or !self.current()) return;
            const fresh = self.register(v.path(), c) catch return;
            if (!self.current()) {
                if (fresh) c.unregister(c.key) catch {};
                log.warn("data volume {s}: this incarnation's page left Live during its lapse re-registration — withdrawn (decision 0003 §DJ)", .{v.path()});
                return;
            }
            if (fresh) {
                v.fresh = true;
                stats.data_pr_lapse_reregistrations.inc();
                log.warn("data volume {s}: re-registered this host under key {x:0>16} after the lapse (decision 0003 §DJ)", .{ v.path(), c.key });
            }
        }
        if (!self.in.manager) return;
        const held = self.meta.holdsHere() catch return;
        if (!held) return;
        const back = self.everyMetaRegistrantOnData(c) catch return;
        const in_grace = now -| v.lapse_since_ns < self.grace_ns;
        if (!back and in_grace) return;
        const acquired = self.reserve(v.path(), c) catch return;
        self.reconcile(v.path(), c) catch return;
        if (acquired) v.acquired = true;
        v.lapse_since_ns = 0;
        stats.data_pr_lapse_rereservations.inc();
        if (back and in_grace) stats.data_pr_lapse_early_rereservations.inc();
        log.warn("data volume {s}: re-reserved Write Exclusive – All Registrants after the lapse ({s}); every data registrant the metadata namespace lacks preempted (decision 0003 §DJ)", .{ v.path(), if (back) "every live host back" else "the grace elapsed" });
    }

    /// A joiner's metadata namespace after a lapse: unreserved and this host
    /// unregistered ⇒ re-register through the stale-sibling rule while this
    /// incarnation is current (its page Live under its writer id), before
    /// and after. A hold standing with this host unregistered is a fence.
    /// True = this beat re-registered the host afresh.
    fn metaLapseBeat(self: *DataFence) bool {
        const meta = self.meta;
        const f = host_fence.reportFacts(meta) catch return false;
        if (!f.facts.reserved) self.noteLapseLocked();
        if (f.registered or f.facts.reserved) return false;
        var reg = self.in.reg orelse return false;
        if (!self.current()) return false;
        reg.lock() catch return false;
        defer reg.unlock();
        const ctx: host_fence.RuleContext = .{ .gpa = self.in.gpa, .env = self.in.env, .path = self.in.meta_path, .local = self.in.local, .node_token = self.in.node_token, .vol_index = 0, .taking_manager = false };
        const fresh = host_fence.registerUnderRule(ctx, &reg, meta, self.in.pages, host_fence.joinRegister(self.in.env)) catch |e| {
            log.warn("meta volume {s}: the lapse re-registration of this host was refused ({s}) — the next beat tries again; once the manager's hold returns this joiner is fenced (decision 0003 §DJ)", .{ self.in.meta_path, @errorName(e) });
            return false;
        };
        if (!self.current()) {
            if (fresh) meta.unregister(meta.key) catch {};
            log.warn("meta volume {s}: this incarnation's page left Live during its lapse re-registration — withdrawn (decision 0003 §DJ)", .{self.in.meta_path});
            return false;
        }
        if (fresh) {
            stats.meta_pr_lapse_reregistrations.inc();
            log.warn("meta volume {s}: re-registered this host under key {x:0>16} after the lapse (a PTPL-less target restart) — before the manager's hold returns (decision 0003 §DJ)", .{ self.in.meta_path, meta.key });
        }
        return fresh;
    }

    // ---- the re-admission at a refused data command (§DL) ----

    /// The devices' re-admission hooks (`DataPlane.bindReadmit`).
    pub fn readmitSource(self: *DataFence) nvme_dev.ReadmitSource {
        return .{ .ctx = self, .hookFor = &hookForFn };
    }

    /// A tabled volume's hook (null: no device fence there).
    fn hookForFn(ctx: *anyopaque, vol_tag: u64) ?*const nvme_dev.Readmit {
        const self: *DataFence = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        if (self.tabledIndex(vol_tag) == null) return null;
        for (self.readmit_cells[0..self.n_cells]) |*cell| if (cell.vol_tag == vol_tag) return &cell.hook;
        if (self.n_cells == self.readmit_cells.len) return null;
        const cell = &self.readmit_cells[self.n_cells];
        cell.* = .{ .hook = .{ .ctx = cell, .readmit = &readmitFn }, .fence = self, .vol_tag = vol_tag };
        self.n_cells += 1;
        return &cell.hook;
    }

    /// The device's call (a data command's thread): the PR work on the
    /// blocking pool, as the beat's.
    fn readmitFn(ctx: *anyopaque) bool {
        const cell: *ReadmitCell = @ptrCast(@alignCast(ctx));
        const self = cell.fence;
        const io = self.io orelse return self.readmitAfterRefusal(cell.vol_tag);
        return exec.SqzIo.fromIo(io).runBlocking(readmitAfterRefusal, .{ self, cell.vol_tag }) catch false;
    }

    /// A data write-class command the device refused with the reservation
    /// conflict on `vol_tag`. A member slow after a lapse — the lapse still
    /// pending here, this host registered on volume 0's metadata namespace
    /// and this incarnation current, before and after a register through the
    /// stale-sibling rule (the beat's checks; a host registered again
    /// meanwhile — another refused command's re-admission, the beat, a
    /// sibling's rule — adopted between the same checks) — is re-admitted:
    /// true, the command goes once more. The namespace unreserved (another
    /// lapse): true, the lapse's window. Anything else: false, the fence
    /// latches.
    pub fn readmitAfterRefusal(self: *DataFence, vol_tag: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        self.takeLapseFlagLocked();
        const i = self.tabledIndex(vol_tag) orelse return false;
        const v = &self.vols[i];
        const c = v.client;
        const f = host_fence.reportFacts(c) catch return declined(v, "the data namespace's report failed");
        if (!f.facts.reserved) {
            v.lapse_pending = true;
            return true;
        }
        if (!v.lapse_pending) return declined(v, "no lapse is pending for this process here — the refusal is a fence");
        const mf = host_fence.reportFacts(self.meta) catch return declined(v, "volume 0's metadata namespace's report failed");
        if (!mf.registered) return declined(v, "this host is not registered on volume 0's metadata namespace (fenced)");
        if (!self.current()) return declined(v, "this incarnation's page is not Live under its writer id");
        const fresh = self.register(v.path(), c) catch return declined(v, "its registration through the rule was refused");
        if (!self.current()) {
            if (fresh) c.unregister(c.key) catch {};
            return declined(v, "this incarnation's page left Live during the re-admission — withdrawn");
        }
        if (!fresh) return true;
        v.fresh = true;
        stats.data_pr_refusal_readmissions.inc();
        log.warn("data volume {s}: a data command was refused (reservation conflict) inside a lapse still pending for this process — this host, registered on volume 0's metadata namespace and current, re-registered under key {x:0>16} through the rule; the command goes once more (decision 0003 §DL)", .{ v.path(), c.key });
        return true;
    }

    fn declined(v: *const Volume, why: []const u8) bool {
        stats.data_pr_refusal_readmissions_declined.inc();
        log.warn("data volume {s}: a refused data command is not re-admitted: {s} (decision 0003 §DL)", .{ v.path(), why });
        return false;
    }

    /// This incarnation is current: a page of volume 0 Live under its own
    /// writer id and node token (a dead or superseded one's page left Live).
    fn current(self: *DataFence) bool {
        const gpa = self.in.gpa;
        const pages = self.in.pages.read(self.in.pages.ctx, gpa) catch return false;
        defer gpa.free(pages);
        for (pages) |p| if (p.state == .live and p.identity.node_token == self.in.node_token and p.identity.writer_id == self.in.writer_id) return true;
        return false;
    }

    /// Every key registered on volume 0's metadata namespace is registered
    /// on this data namespace (every live host back after a lapse).
    fn everyMetaRegistrantOnData(self: *DataFence, c: *pr.Client) pr.Error!bool {
        const gpa = self.in.gpa;
        var mrep = try self.meta.report();
        defer mrep.deinit(gpa);
        var drep = try c.report();
        defer drep.deinit(gpa);
        outer: for (mrep.registrants) |mr| {
            for (drep.registrants) |dr| if (dr.rkey == mr.rkey) continue :outer;
            return false;
        }
        return true;
    }

    /// The teardown, after the data plane closed its fds and before the
    /// metadata release: the host's last process of the set leaves each data
    /// namespace by the kept law; every client and node closed.
    pub fn release(self: *DataFence) void {
        const gpa = self.in.gpa;
        defer gpa.destroy(self);
        self.stopBeat();
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
    if (r) |what| {
        df.release();
        refused.* = what;
        return error.Refused;
    }
    return df;
}

/// Null = admitted; else what refused (logged).
fn admitAllBlocking(df: *DataFence, data: []const set_config.DataVolume) ?[]const u8 {
    switch (df.metaHostId()) {
        .known => {},
        .unknown => {
            log.warn("data fence: the metadata namespace answers no host identifier (FID 0x81 zeros) — the data namespaces' registrations cannot be judged: no data volume is fenced (decision 0003 §DJ)", .{});
            df.lock.lock();
            defer df.lock.unlock();
            for (data) |*d| if (d.state != .retired) df.noteAbsentLocked(d.vol_tag, .no_wire_host_id);
            return null;
        },
        .unreadable => {
            stats.data_pr_admission_refusals.inc();
            log.err("data fence: the metadata namespace's host identifier (FID 0x81) could not be read — refusing: the data namespaces' registrations cannot be judged blind (decision 0003 §DJ)", .{});
            return "volume 0's metadata namespace (its host identifier)";
        },
    }
    for (data) |*d| {
        if (d.state == .retired) continue;
        df.admitVolume(d.vol_tag, d.uri) catch return d.uri;
    }
    return null;
}

fn inputsOf(gpa: Allocator, env: *const std.process.Environ.Map, be0: *backend.KvBackend) ?Inputs {
    const pages: host_fence.PageSource = .{ .ctx = be0, .read = &volumePages };
    const config: ConfigSource = .{ .ctx = be0, .read = &durableConfig };
    if (be0.guard) |g| {
        const c = g.prClient() orelse return null;
        const dev = g.device orelse return null;
        return .{ .gpa = gpa, .env = env, .meta_face = dev.face(), .meta_nsid = dev.nsid, .key = c.key, .reg = g.fence_reg, .local = g.identity, .node_token = join.nodeToken(g.identity), .pages = pages, .meta_path = be0.path, .writer_id = g.identity.writerId(be0.index), .config = config, .manager = true, .guard = g };
    }
    const js = be0.joined orelse return null;
    const f = js.fence orelse return null;
    return .{ .gpa = gpa, .env = env, .meta_face = f.device.face(), .meta_nsid = f.device.nsid, .key = f.client.key, .reg = f.reg, .local = proc.localIdentity(), .node_token = js.identity.node_token, .pages = pages, .meta_path = be0.path, .writer_id = js.identity.writer_id, .config = config, .manager = false };
}

/// A re-join's successor (decision 0003 §BS): its volume 0's joiner posture.
pub fn readmitSuccessor(self: *DataFence, successor0: *backend.KvBackend) ?[]const u8 {
    const js = successor0.joined orelse return "the successor's volume 0 is not a joined incarnation";
    const f = js.fence orelse return null;
    return self.readmit(.{ .meta_face = f.device.face(), .meta_nsid = f.device.nsid, .key = f.client.key, .reg = f.reg, .pages = .{ .ctx = successor0, .read = &volumePages }, .meta_path = successor0.path, .writer_id = js.identity.writer_id, .node_token = js.identity.node_token });
}

/// The durable set config's non-retired data volumes (tree 0 of volume 0).
fn durableConfig(ctx: *anyopaque, gpa: Allocator, out: *[router.MAX_VOLUMES]u64) ConfigSource.Error!usize {
    const be0: *backend.KvBackend = @ptrCast(@alignCast(ctx));
    const value = (be0.lookup(gpa, meta_key.TREE_CONTROL, set_config.CONTROL_KEY) catch return error.Io) orelse return error.Undecodable;
    defer gpa.free(value);
    var diag: set_config.Refusal = undefined;
    var cfg = set_config.decode(gpa, value, &diag) catch |e| return if (e == error.OutOfMemory) error.OutOfMemory else error.Undecodable;
    defer cfg.deinit(gpa);
    var n: usize = 0;
    for (cfg.data) |d| {
        if (d.state == .retired) continue;
        if (n == out.len) return error.TooManyVolumes;
        out[n] = d.vol_tag;
        n += 1;
    }
    return n;
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

/// Is `rkey` registered on the namespace (one report)?
fn keyOnNamespace(c: *pr.Client, rkey: u64) pr.Error!bool {
    var rep = try c.report();
    defer rep.deinit(c.gpa);
    for (rep.registrants) |r| if (r.rkey == rkey) return true;
    return false;
}

/// A transient condition refuses the admission, loud (review MAJOR 1): the
/// mount, the online add or the re-join fails and is retried by its caller.
fn transientRefusal(path: []const u8, what: []const u8, why: ?[]const u8) AdmitError {
    stats.data_pr_admission_refusals.inc();
    log.err("data volume {s}: {s}{s}{s}{s} — refusing the admission: only a stable fact (a file, a non-NVMe block node, RESCAP 0, no host identifier) leaves a data volume without a device fence (decision 0003 §DJ)", .{ path, what, if (why != null) " (" else "", why orelse "", if (why != null) ")" else "" });
    return error.Refused;
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

test "the data admission registers this host on every DATA namespace before its first data write (decision 0003 §DJ): before it a reserved namespace refuses the host's write, after it admits it; a joiner only registers, the manager reserves WEAR once every metadata registrant is registered; a host already registered adopts with no device mutation; a host the metadata namespace no longer registers is refused, its data registration absent; the manager's admission preempts every data registrant the metadata namespace no longer registers (a superseded manager's host)" {
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
    var cfg: FixtureConfig = .{ .live = &.{ 1, 2 } };
    // M's admission (volume 0's writer): registered, the reservation kept,
    // O's key preempted (absent from the metadata namespace).
    const reconciles0 = stats.data_pr_reconcile_preempts.load();
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .meta_path = "fake-meta", .writer_id = 0, .config = cfg.source(), .manager = true });
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
    const dj = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = jh.face(), .meta_nsid = 1, .key = 0x2222, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .meta_path = "fake-meta", .writer_id = 0, .config = cfg.source(), .manager = false });
    defer dj.release();
    try dj.admitClient(1, "fake-data", null, try fixtureClient(gpa, djh.face()));
    try testing.expect(dns.writeAllowed(j_id));
    // A second process of host J adopts: nothing on the device moves.
    const gen = dns.generation;
    const commands_before = dns.commands;
    const dj2 = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = jh.face(), .meta_nsid = 1, .key = 0x2222, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .meta_path = "fake-meta", .writer_id = 0, .config = cfg.source(), .manager = false });
    defer dj2.release();
    try dj2.admitClient(1, "fake-data", null, try fixtureClient(gpa, djh.face()));
    try testing.expectEqual(gen, dns.generation);
    try testing.expect(dns.commands > commands_before);
    // Host F is not registered on the metadata namespace (fenced): refused,
    // never registered on data.
    const dff = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = fh.face(), .meta_nsid = 1, .key = 0x4444, .reg = null, .local = local, .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .meta_path = "fake-meta", .writer_id = 0, .config = cfg.source(), .manager = false });
    defer dff.release();
    var dfh: fake.Host = .{ .ns = &dns, .host_id = f_id };
    try testing.expectError(error.HostFenced, dff.admitClient(1, "fake-data", null, try fixtureClient(gpa, dfh.face())));
    try testing.expect(!dns.writeAllowed(f_id));
    try testing.expect(!dns.isRegistered(0x4444));
    // An unreserved namespace: a joiner registers and reserves nothing; the
    // manager reserves it WEAR once every metadata registrant is there.
    var d2h: fake.Host = .{ .ns = &dns2, .host_id = j_id };
    try dj.admitClient(2, "fake-data-2", null, try fixtureClient(gpa, d2h.face()));
    try testing.expectEqual(@as(u8, 0), dns2.rtype);
    var d2m: fake.Host = .{ .ns = &dns2, .host_id = m_id };
    try dm.admitClient(2, "fake-data-2", null, try fixtureClient(gpa, d2m.face()));
    try testing.expectEqual(nvme.RTYPE_WEAR, dns2.rtype);
    try testing.expect(!dns2.writeAllowed(o_id));
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
    var cfg: FixtureConfig = .{ .live = &.{ 1, 2 } };
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = proc.localIdentity(), .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .meta_path = "fake-meta", .writer_id = 0, .config = cfg.source(), .manager = true });
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
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = proc.localIdentity(), .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .meta_path = "fake-meta", .writer_id = 0, .config = cfg.source(), .manager = true });
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

test "review minor 1 (decision 0003 §DJ): the manager's reconcile preempts a key that left between its two reports — a joiner leaving data then metadata after the data report — and the target refuses (the key is gone): the data report is read again, the key absent is success, the admission proceeds and counts no preempt" {
    const gpa = testing.allocator;
    const fake = @import("fake_nvme.zig");
    const m_id: [16]u8 = "manager-host-id1".*;
    const l_id: [16]u8 = "leaving-joiner-5".*;
    var mns = fake.Namespace.init(gpa);
    defer mns.deinit();
    var mh: fake.Host = .{ .ns = &mns, .host_id = m_id };
    var meta_m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer meta_m.deinit();
    try fencing.acquireManagerHold(&meta_m);
    var lh: fake.Host = .{ .ns = &mns, .host_id = l_id };
    var meta_l = try pr.Client.init(gpa, lh.face(), 1, 0x5555);
    defer meta_l.deinit();
    _ = try fencing.joinAsRegistrant(&meta_l, null);
    var dns = fake.Namespace.init(gpa);
    defer dns.deinit();
    var dlh: fake.Host = .{ .ns = &dns, .host_id = l_id };
    var data_l = try pr.Client.init(gpa, dlh.face(), 1, 0x5555);
    defer data_l.deinit();
    data_l.rtype = nvme.RTYPE_WEAR;
    _ = try data_l.registerLadder();
    try data_l.acquire();
    var no_pages: u8 = 0;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var cfg: FixtureConfig = .{ .live = &.{1} };
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = proc.localIdentity(), .node_token = 0, .pages = .{ .ctx = &no_pages, .read = &noPages }, .meta_path = "fake-meta", .writer_id = 0, .config = cfg.source(), .manager = true });
    defer dm.release();
    var dmh: fake.Host = .{ .ns = &dns, .host_id = m_id };
    try dm.admitClient(1, "fake-data", null, try fixtureClient(gpa, dmh.face()));
    try testing.expect(dns.isRegistered(0x5555));
    // L leaves — data, then metadata — right before the reconcile's
    // metadata report (after its hold check's report: a report is two
    // commands, the header and the body).
    const Leave = struct {
        data: *pr.Client,
        meta: *pr.Client,
        fn fire(ctx: *anyopaque) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.data.unregister(self.data.key) catch unreachable;
            self.meta.unregister(self.meta.key) catch unreachable;
        }
    };
    var leave: Leave = .{ .data = &data_l, .meta = &meta_l };
    mns.before_report = .{ .ctx = &leave, .fire = Leave.fire };
    mns.before_report_skip = 2;
    const preempts0 = stats.data_pr_reconcile_preempts.load();
    try dm.reconcile("fake-data", dm.clients[0]);
    try testing.expect(mns.before_report == null);
    try testing.expect(!dns.isRegistered(0x5555));
    try testing.expectEqual(preempts0, stats.data_pr_reconcile_preempts.load());
}

/// A fixture's appender pages (copied out per read, as the device's are).
const FixturePages = struct {
    pages: []appender_page.Page,

    fn source(self: *FixturePages) host_fence.PageSource {
        return .{ .ctx = self, .read = &readFn };
    }

    fn readFn(ctx: *anyopaque, gpa: Allocator) error{ Io, Corrupt, OutOfMemory }![]appender_page.Page {
        const self: *FixturePages = @ptrCast(@alignCast(ctx));
        return gpa.dupe(appender_page.Page, self.pages);
    }
};

fn livePage(aid: u32, node: u64, wid: u64) appender_page.Page {
    var p = appender_page.Page.free(aid, 1, .{});
    p.state = .live;
    p.identity = .{ .node_token = node, .mount_slot = 7, .writer_id = wid };
    return p;
}

test "review MAJOR 2 (decision 0003 §DJ), the lapse beat: a PTPL-less target restart forgets every registration and reservation — a joiner's beat re-registers its host on the metadata namespace (its page Live, the stale-sibling rule) and on the data namespace (its metadata registration checked), the manager's beat re-registers its own and re-reserves WEAR once every metadata registrant is back; a dead member's beat (its page left Live) re-registers nothing; nothing re-registers under a standing reservation" {
    const gpa = testing.allocator;
    const fake = @import("fake_nvme.zig");
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/data-fence-lapse-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var probe = host_fence.Registry.at(base, .set, 0x1a);
    probe.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    probe.unlock();
    const m_id: [16]u8 = "manager-host-id1".*;
    const j_id: [16]u8 = "joiner-host-id-2".*;
    const x_id: [16]u8 = "dead-member-id-3".*;
    var mns = fake.Namespace.init(gpa);
    defer mns.deinit();
    var dns = fake.Namespace.init(gpa);
    defer dns.deinit();
    var mh: fake.Host = .{ .ns = &mns, .host_id = m_id };
    var jh: fake.Host = .{ .ns = &mns, .host_id = j_id };
    var xh: fake.Host = .{ .ns = &mns, .host_id = x_id };
    var meta_m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer meta_m.deinit();
    try fencing.acquireManagerHold(&meta_m);
    var meta_j = try pr.Client.init(gpa, jh.face(), 1, 0x2222);
    defer meta_j.deinit();
    _ = try fencing.joinAsRegistrant(&meta_j, null);
    // M (host A, node 0xA) manages; J (host C, node 0xC) joined; X (host B,
    // node 0xB) died: its page left Live, its host fenced on metadata.
    var pages = [_]appender_page.Page{ livePage(0, 0xA, 0xA0), livePage(1, 0xC, 0xC0), pgRecovering(2, 0xB, 0xB0) };
    var src: FixturePages = .{ .pages = &pages };
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var cfg: FixtureConfig = .{ .live = &.{1} };
    const local = proc.localIdentity();
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = local, .node_token = 0xA, .pages = src.source(), .meta_path = "fake-meta", .writer_id = 0xA0, .config = cfg.source(), .manager = true });
    defer dm.release();
    const dj = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = jh.face(), .meta_nsid = 1, .key = 0x2222, .reg = host_fence.Registry.at(base, .set, 0x1a), .local = local, .node_token = 0xC, .pages = src.source(), .meta_path = "fake-meta", .writer_id = 0xC0, .config = cfg.source(), .manager = false });
    defer dj.release();
    const dx = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = xh.face(), .meta_nsid = 1, .key = 0x3333, .reg = host_fence.Registry.at(base, .set, 0x1b), .local = local, .node_token = 0xB, .pages = src.source(), .meta_path = "fake-meta", .writer_id = 0xB0, .config = cfg.source(), .manager = false });
    defer dx.release();
    var dmh: fake.Host = .{ .ns = &dns, .host_id = m_id };
    var djh: fake.Host = .{ .ns = &dns, .host_id = j_id };
    var dxh: fake.Host = .{ .ns = &dns, .host_id = x_id };
    try dm.admitClient(1, "fake-data", null, try fixtureClient(gpa, dmh.face()));
    try dj.admitClient(1, "fake-data", null, try fixtureClient(gpa, djh.face()));
    // The manager reserved once J registered too (its admission ran first:
    // the beat takes the reservation once every metadata registrant is on).
    dm.beat();
    try testing.expectEqual(nvme.RTYPE_WEAR, dns.rtype);
    // X is not registered on metadata: its admission is refused.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.HostFenced, dx.admitClient(1, "fake-data", null, try fixtureClient(gpa, dxh.face())));
    try log.testing.expectAllFired();
    // The target restarts: every registration and both reservations gone.
    mns.powerCycle();
    dns.powerCycle();
    try testing.expect(dns.writeAllowed(x_id));
    const meta_re0 = stats.meta_pr_lapse_reregistrations.load();
    const data_re0 = stats.data_pr_lapse_reregistrations.load();
    const rereserve0 = stats.data_pr_lapse_rereservations.load();
    // The manager's guard re-registers its host at its own beat (gate.zig's
    // grace); here by hand.
    _ = try fencing.joinAsRegistrant(&meta_m, null);
    // J's beat: metadata first, then data.
    dj.beat();
    try testing.expect(mns.isRegistered(0x2222));
    try testing.expect(dns.isRegistered(0x2222));
    try testing.expectEqual(meta_re0 + 1, stats.meta_pr_lapse_reregistrations.load());
    try testing.expectEqual(data_re0 + 1, stats.data_pr_lapse_reregistrations.load());
    // X's beat: its page is not Live — nothing.
    dx.beat();
    try testing.expect(!mns.isRegistered(0x3333));
    try testing.expect(!dns.isRegistered(0x3333));
    // The manager's beat before its metadata hold returns: its own data
    // registration back, no reservation yet.
    dm.beat();
    try testing.expect(dns.isRegistered(0x1111));
    try testing.expectEqual(@as(u8, 0), dns.rtype);
    try testing.expectEqual(data_re0 + 2, stats.data_pr_lapse_reregistrations.load());
    // The hold returns (the guard's grace passed): every metadata registrant
    // is on data — the next beat re-reserves at once.
    try meta_m.acquire();
    dm.beat();
    try testing.expectEqual(nvme.RTYPE_WEAR, dns.rtype);
    try testing.expectEqual(rereserve0 + 1, stats.data_pr_lapse_rereservations.load());
    try testing.expect(dns.writeAllowed(j_id) and dns.writeAllowed(m_id) and !dns.writeAllowed(x_id));
    // Under the standing reservation a host the beat finds unregistered is
    // fenced: J preempted, its beat re-registers nothing.
    try dm.clients[0].preemptAndAbort(0x2222);
    dj.beat();
    try testing.expect(!dns.isRegistered(0x2222));
    try testing.expectEqual(data_re0 + 2, stats.data_pr_lapse_reregistrations.load());
}

fn pgRecovering(aid: u32, node: u64, wid: u64) appender_page.Page {
    var p = livePage(aid, node, wid);
    p.state = .recovering;
    return p;
}

test "decision 0003 §DL, a member slow on data: after a PTPL-less restart the joiner re-registers on metadata but not yet on data and the manager re-reserves WEAR without it — its refused write is re-admitted (registered on metadata, its page Live, the lapse pending; the registration fresh, counted); once its beat saw itself registered under the reservation the lapse is over and a refusal is a fence (declined); a host fenced on metadata, and a member whose page left Live, are never re-admitted" {
    const gpa = testing.allocator;
    const fake = @import("fake_nvme.zig");
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/data-fence-slow-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var probe = host_fence.Registry.at(base, .set, 0x2a);
    probe.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    probe.unlock();
    const m_id: [16]u8 = "manager-host-id1".*;
    const j_id: [16]u8 = "joiner-host-id-2".*;
    const x_id: [16]u8 = "dead-member-id-3".*;
    var mns = fake.Namespace.init(gpa);
    defer mns.deinit();
    var dns = fake.Namespace.init(gpa);
    defer dns.deinit();
    var mh: fake.Host = .{ .ns = &mns, .host_id = m_id };
    var jh: fake.Host = .{ .ns = &mns, .host_id = j_id };
    var xh: fake.Host = .{ .ns = &mns, .host_id = x_id };
    var meta_m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer meta_m.deinit();
    try fencing.acquireManagerHold(&meta_m);
    var meta_j = try pr.Client.init(gpa, jh.face(), 1, 0x2222);
    defer meta_j.deinit();
    _ = try fencing.joinAsRegistrant(&meta_j, null);
    var meta_x = try pr.Client.init(gpa, xh.face(), 1, 0x3333);
    defer meta_x.deinit();
    _ = try fencing.joinAsRegistrant(&meta_x, null);
    // M (node 0xA) manages; J (node 0xC) and X (node 0xB) joined, Live.
    var pages = [_]appender_page.Page{ livePage(0, 0xA, 0xA0), livePage(1, 0xC, 0xC0), livePage(2, 0xB, 0xB0) };
    var src: FixturePages = .{ .pages = &pages };
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var cfg: FixtureConfig = .{ .live = &.{1} };
    const local = proc.localIdentity();
    const dm = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = mh.face(), .meta_nsid = 1, .key = 0x1111, .reg = null, .local = local, .node_token = 0xA, .pages = src.source(), .meta_path = "fake-meta", .writer_id = 0xA0, .config = cfg.source(), .manager = true });
    defer dm.release();
    const dj = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = jh.face(), .meta_nsid = 1, .key = 0x2222, .reg = host_fence.Registry.at(base, .set, 0x2a), .local = local, .node_token = 0xC, .pages = src.source(), .meta_path = "fake-meta", .writer_id = 0xC0, .config = cfg.source(), .manager = false });
    defer dj.release();
    const dx = try DataFence.create(.{ .gpa = gpa, .env = &env, .meta_face = xh.face(), .meta_nsid = 1, .key = 0x3333, .reg = host_fence.Registry.at(base, .set, 0x2b), .local = local, .node_token = 0xB, .pages = src.source(), .meta_path = "fake-meta", .writer_id = 0xB0, .config = cfg.source(), .manager = false });
    defer dx.release();
    var dmh: fake.Host = .{ .ns = &dns, .host_id = m_id };
    var djh: fake.Host = .{ .ns = &dns, .host_id = j_id };
    var dxh: fake.Host = .{ .ns = &dns, .host_id = x_id };
    try dm.admitClient(1, "fake-data", null, try fixtureClient(gpa, dmh.face()));
    try dj.admitClient(1, "fake-data", null, try fixtureClient(gpa, djh.face()));
    try dx.admitClient(1, "fake-data", null, try fixtureClient(gpa, dxh.face()));
    dm.beat();
    try testing.expectEqual(nvme.RTYPE_WEAR, dns.rtype);
    // Nothing pending before a lapse: a refusal is a fence.
    try testing.expect(!dj.vols[0].lapse_pending);
    // X died (its page Recovering); then the restart.
    pages[2].state = .recovering;
    mns.powerCycle();
    dns.powerCycle();
    _ = try fencing.joinAsRegistrant(&meta_m, null);
    // J's beat, slow: its metadata half re-registers (the lapse seen), its
    // data half not yet. X's sees the lapse and re-registers nothing.
    dj.lock.lock();
    try testing.expect(dj.metaLapseBeat());
    dj.lock.unlock();
    try testing.expect(dj.vols[0].lapse_pending);
    dx.lock.lock();
    try testing.expect(!dx.metaLapseBeat());
    dx.lock.unlock();
    try testing.expect(dx.vols[0].lapse_pending);
    try testing.expect(!mns.isRegistered(0x3333));
    // M holds again and re-reserves after its grace, J not back on data.
    try meta_m.acquire();
    dm.grace_ns = 0;
    dm.beat();
    try testing.expectEqual(nvme.RTYPE_WEAR, dns.rtype);
    try testing.expect(!dns.writeAllowed(j_id));
    // J's refused write: re-admitted.
    const re0 = stats.data_pr_refusal_readmissions.load();
    try testing.expect(dj.readmitAfterRefusal(1));
    try testing.expect(dns.writeAllowed(j_id));
    try testing.expectEqual(re0 + 1, stats.data_pr_refusal_readmissions.load());
    // X: its host not registered on metadata — never re-admitted; registered
    // there again (a sibling's rule) but its page Recovering — never
    // re-admitted.
    const declined0 = stats.data_pr_refusal_readmissions_declined.load();
    try testing.expect(!dx.readmitAfterRefusal(1));
    _ = try fencing.joinAsRegistrant(&meta_x, null);
    try testing.expect(!dx.readmitAfterRefusal(1));
    try testing.expect(!dns.isRegistered(0x3333));
    try testing.expectEqual(declined0 + 2, stats.data_pr_refusal_readmissions_declined.load());
    // J's beat sees itself registered under the reservation: the lapse is
    // over; a later refusal (its host fenced on data) latches.
    dj.beat();
    try testing.expect(!dj.vols[0].lapse_pending);
    try dm.clients[0].preemptAndAbort(0x2222);
    try testing.expect(!dj.readmitAfterRefusal(1));
    try testing.expect(!dns.isRegistered(0x2222));
}
