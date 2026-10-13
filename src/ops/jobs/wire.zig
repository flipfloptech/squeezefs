//! The job block's two sides over the ONE wire (charter §8 "remote workers
//! over TCP/TLS with HMAC enrollment against a `job:enroll` secret, shard
//! leases, result PROPOSALS only, 100 % verify-reads before publish on
//! plaintext"; spec §1.6, §1.8): the COORDINATOR's handlers registered on
//! its `wire/service.Service` (the same service the manager verbs ride; a
//! non-coordinator leaves them unregistered and the block answers
//! `not_manager`), and the WORKER loop (`job worker`) over any caller —
//! the TCP dialer or the in-process loopback. Portable: the TCP legs ride
//! `wire/dialer.zig` on Linux; the loopback pins every byte here.
//!
//! What a worker may do: read anywhere on shared storage, write ONLY the
//! pre-allocated unpublished tuples, PROPOSE. What it may not: allocate,
//! publish, free, touch metadata — the block carries no meta-write verb
//! (KD-15). A proposal is SCREENED before any device read: fencing
//! compared, `n ≤ 64`, every tuple inside the shard's own window; then
//! VERIFY-READ against the submitted checksums — 100 % on plaintext (the
//! session MAC authenticates, it does not make the bytes that crossed the
//! wire trustworthy), the configured permille only under CA-pinned mTLS;
//! a failed verification is treated as an expired lease (fencing bump +
//! quarantine). Enrollment is `wire/auth.zig`'s ladder verbatim; `job_
//! enroll` after admission adds the host's `pr_key` and routing `caps`.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const checksum = @import("../../core/checksum.zig");
const errors = @import("../../core/errors.zig");
const jc = @import("../../meta/journal_core.zig");
const sync = @import("../../sync/sync.zig");
const verbs = @import("../../wire/verbs.zig");
const service = @import("../../wire/service.zig");
const auth = @import("../../wire/auth.zig");
const records = @import("records.zig");
const frames = @import("frames.zig");
const fabric_mod = @import("fabric.zig");
const lease_mod = @import("lease.zig");
const seams = @import("seams.zig");
const stats_mod = @import("stats.zig");

const Atomic = std.atomic.Value;
const stats = stats_mod.stats;
const Fabric = fabric_mod.Fabric;
const JobCtl = fabric_mod.JobCtl;
const ShardTable = lease_mod.ShardTable;
const ShardId = seams.ShardId;
const DestTuple = records.DestTuple;
const StableIdentity = records.StableIdentity;

/// The channel's trust class decides the verify strength (`wire/tls.zig`:
/// the session key is bound to the channel's exporter under `mutual_tls`).
pub const Channel = @import("../../wire/tls.zig").Channel;

/// The verify-read permille the channel admits: 1000 pinned on plaintext;
/// the knob (`SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE`) honoured only under
/// mTLS.
pub fn verifyPermille(channel: Channel, configured: u16) u16 {
    std.debug.assert(configured >= 1 and configured <= 1000);
    return switch (channel) {
        .plaintext => 1000,
        .mutual_tls => configured,
    };
}

/// Deterministic sampling: tuple `i` of a proposal under `permille` is
/// verified iff its hash falls under the line (every tuple when 1000).
pub fn sampled(permille: u16, job_id: u64, shard: u32, i: usize) bool {
    if (permille >= 1000) return true;
    var b: [20]u8 = undefined;
    std.mem.writeInt(u64, b[0..8], job_id, .little);
    std.mem.writeInt(u32, b[8..12], shard, .little);
    std.mem.writeInt(u64, b[12..20], i, .little);
    return checksum.hash64(&b) % 1000 < permille;
}

/// The coordinator's device read-back of a proposed destination: the
/// stored image's hash64 over `len` bytes (the plane binds the device seam;
/// tests a RAM world).
pub const VerifyRead = struct {
    ctx: *anyopaque,
    read_csum: *const fn (ctx: *anyopaque, dest: DestTuple, len: u64) error{Io}!u64,
};

/// The most enrolled workers one coordinator tracks.
pub const MAX_WORKERS: usize = 256;

/// Fleet READ shards in flight at one coordinator: one fleet fsck ships
/// `readCapacity()` shards (≤ `MAX_WORKERS`) and the next job's wait for
/// this one's retire — two jobs' worth is the headroom.
pub const MAX_READ_SHARDS: usize = 2 * MAX_WORKERS;

/// A lost READ shard is re-leased once (another idle capable session, or
/// the same one re-pulling after a torn result) before it runs locally:
/// every residue lands exactly once either way, and a second remote try
/// buys nothing a local run does not.
pub const READ_SHARD_ATTEMPTS_MAX: u32 = 2;

/// The collect loop's PROGRESS bound in lease TTLs (spec §1.9a: "a worker
/// whose heartbeats keep beating while its shard never proposes hung the
/// fleet fsck 31 min"): four TTLs = 2 min at the charter's 30 s — a shard
/// still unproposed past it is lost whatever its heartbeats say.
pub const READ_SHARD_PROGRESS_TTLS: u64 = 4;

const Enrolled = struct {
    identity: StableIdentity,
    pr_key: u64,
    caps: u32,
    /// The session id the listener gave the connection (the departure key).
    session_id: u64,
};

/// A fleet READ shard from `queue` to its retire: queued until a capable
/// worker pulls it, leased while it runs, done with its payload, or lost
/// (the attempts spent, or no capable session left) — the collector runs
/// a lost residue locally.
const ReadShard = struct {
    d: seams.FleetDispatch.Descriptor,
    /// `fsck.JOB_ARG_*` for the engine's options on the worker.
    arg0: u64,
    state: enum { queued, leased, done, lost } = .queued,
    attempts: u32 = 0,
    /// The proposed report (owned by the coordinator's gpa) once `done`.
    payload: ?[]u8 = null,
    /// Woken on `done` / `lost` (the collector's park).
    settled: sync.Notify = .init,
};

/// The coordinator side: the handlers, the enrolled table, the remote
/// shard → fabric job binding.
pub const Coordinator = struct {
    gpa: Allocator,
    io: Io,
    fabric: *Fabric,
    table: *ShardTable,
    verify: VerifyRead,
    channel: Channel,
    configured_permille: u16,
    lock: jc.SpinLock = .{},
    workers: [MAX_WORKERS]?Enrolled = @splat(null),
    n_workers: usize = 0,
    /// The remote whole-job shards in flight: `(job, shard 1)` → the
    /// claimed JobCtl (a noop job is ONE shard).
    bound: [MAX_WORKERS]?Bound = @splat(null),
    /// The fleet READ shards (`FleetDispatch`): queued, leased, done, lost.
    read_shards: [MAX_READ_SHARDS]?*ReadShard = @splat(null),
    /// The WERO on the data namespaces, acquired at FIRST enrollment and
    /// released at LAST departure (the plane binds it; null = no data
    /// namespaces armed — the deferred-reclaim class, loud once).
    wero: ?Wero,
    wero_held: bool = false,

    const Bound = struct { id: ShardId, job: *JobCtl };

    pub const Wero = struct {
        ctx: *anyopaque,
        /// Acquire rtype 3 on every data namespace; false ⇒ not all PR-capable.
        acquire: *const fn (ctx: *anyopaque) bool,
        release: *const fn (ctx: *anyopaque) void,
    };

    pub const Options = struct {
        verify: VerifyRead,
        channel: Channel = .plaintext,
        configured_permille: u16 = 1000,
        wero: ?Wero = null,
    };

    pub fn init(gpa: Allocator, io: Io, f: *Fabric, table: *ShardTable, opts: Options) Coordinator {
        return .{ .gpa = gpa, .io = io, .fabric = f, .table = table, .verify = opts.verify, .channel = opts.channel, .configured_permille = opts.configured_permille, .wero = opts.wero };
    }

    /// Drop every read shard still tracked (the node's teardown; a live
    /// collector is gone by then — the fabric stopped first).
    pub fn deinit(self: *Coordinator) void {
        for (&self.read_shards) |*slot| if (slot.*) |rs| {
            if (rs.payload) |p| self.gpa.free(p);
            self.gpa.destroy(rs);
            slot.* = null;
        };
    }

    /// Register the block's handlers on the coordinator's service.
    pub fn register(self: *Coordinator, svc: *service.Service) void {
        const h: service.Handler = .{ .ctx = self, .serve = &serve };
        inline for (.{ .job_enroll, .shard_assign, .heartbeat, .result_submit, .read_shard_result, .shard_abandon }) |v| svc.register(v, h);
    }

    pub fn unregister(_: *Coordinator, svc: *service.Service) void {
        inline for (.{ .job_enroll, .shard_assign, .heartbeat, .result_submit, .read_shard_result, .shard_abandon }) |v| svc.unregister(v);
    }

    pub fn permille(self: *const Coordinator) u16 {
        return verifyPermille(self.channel, self.configured_permille);
    }

    fn identityOf(peer: ?verbs.WireIdentity) ?StableIdentity {
        const p = peer orelse return null;
        return .{ .node_token = p.node_token, .mount_slot = p.mount_slot };
    }

    fn findWorkerLocked(self: *Coordinator, id: StableIdentity) ?*Enrolled {
        for (self.workers[0..self.n_workers]) |*slot| if (slot.*) |*e| if (e.identity.node_token == id.node_token and e.identity.mount_slot == id.mount_slot) return e;
        return null;
    }

    /// The host registration key an enrolled worker declared (the PR
    /// preempt's victim); null = not enrolled.
    pub fn prKeyOf(self: *Coordinator, id: StableIdentity) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.findWorkerLocked(id) orelse return null;
        return e.pr_key;
    }

    pub fn enrolledCount(self: *Coordinator) usize {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        for (self.workers[0..self.n_workers]) |slot| if (slot != null) {
            n += 1;
        };
        return n;
    }

    /// Enrolled read-capable workers (the fleet dispatcher's capacity).
    pub fn readCapacity(self: *Coordinator) u32 {
        self.lock.lock();
        defer self.lock.unlock();
        var n: u32 = 0;
        for (self.workers[0..self.n_workers]) |slot| if (slot) |e| if (e.caps & frames.CAP_FLEET_READ != 0) {
            n += 1;
        };
        return n;
    }

    /// A session departed (the listener's teardown hook): its held shards
    /// expire NOW; the last departure releases the WERO.
    pub fn sessionDeparted(self: *Coordinator, session_id: u64) void {
        self.lock.lock();
        var gone: ?StableIdentity = null;
        for (self.workers[0..self.n_workers]) |*slot| if (slot.*) |e| if (e.session_id == session_id) {
            gone = e.identity;
            slot.* = null;
        };
        var remaining: usize = 0;
        for (self.workers[0..self.n_workers]) |slot| if (slot != null) {
            remaining += 1;
        };
        const release_wero = remaining == 0 and self.wero_held;
        if (release_wero) self.wero_held = false;
        self.lock.unlock();
        if (gone) |g| {
            _ = self.table.expireHolder(g, clock.monotonicNs());
            self.requeueExpired();
            stats.job_remote_workers.set(remaining);
        }
        if (release_wero) if (self.wero) |w| w.release(w.ctx);
    }

    /// The sweeper's pass: expire what passed, requeue the fabric jobs
    /// whose shards were reclaimed.
    pub fn sweep(self: *Coordinator, now_ns: u64) usize {
        const n = self.table.sweep(now_ns);
        if (n > 0) self.requeueExpired();
        return n;
    }

    /// Every bound job whose shard is no longer live under its fencing
    /// (expired, abandoned) goes back to Queued for any population; a
    /// leased READ shard whose lease went re-queues for another capable
    /// pull while attempts remain and a capable session exists, else it is
    /// LOST (the collector runs it locally).
    fn requeueExpired(self: *Coordinator) void {
        var victims: [MAX_WORKERS]?*JobCtl = @splat(null);
        var n: usize = 0;
        var settled: [MAX_READ_SHARDS]?*ReadShard = @splat(null);
        var ns: usize = 0;
        {
            self.lock.lock();
            defer self.lock.unlock();
            for (self.bound[0..]) |*slot| if (slot.*) |b| {
                if (self.table.isAssigned(b.id)) continue;
                // Reclaimed: the fencing moved and nothing is assigned.
                victims[n] = b.job;
                n += 1;
                slot.* = null;
            };
            const capacity = self.readCapacityLocked();
            for (self.read_shards[0..]) |slot| if (slot) |rs| {
                if (rs.state != .leased or self.table.isAssigned(rs.d.id)) continue;
                stats.job_remote_reassignments.inc();
                if (rs.attempts < READ_SHARD_ATTEMPTS_MAX and capacity > 0) {
                    rs.state = .queued;
                } else {
                    rs.state = .lost;
                    settled[ns] = rs;
                    ns += 1;
                }
            };
        }
        for (victims[0..n]) |v| self.fabric.requeue(self.io, v.?);
        for (settled[0..ns]) |rs| rs.?.settled.notifyWaiters(self.io);
    }

    fn readCapacityLocked(self: *Coordinator) u32 {
        var n: u32 = 0;
        for (self.workers[0..self.n_workers]) |slot| if (slot) |e| if (e.caps & frames.CAP_FLEET_READ != 0) {
            n += 1;
        };
        return n;
    }

    // ---- the fleet READ-shard dispatcher (`seams.FleetDispatch`) ----

    pub fn fleet(self: *Coordinator) seams.FleetDispatch {
        return .{ .ctx = self, .vtable = &fleet_vt };
    }

    const fleet_vt: seams.FleetDispatch.VTable = .{ .readCapacity = &fleetCapacity, .queueReadShard = &fleetQueue, .collectReadShard = &fleetCollect, .release = &fleetRelease, .retireFleetShards = &fleetRetire, .shardLeaseTtlMs = &fleetTtl };

    fn fleetCapacity(ctx: *anyopaque) u32 {
        const self: *Coordinator = @ptrCast(@alignCast(ctx));
        return self.readCapacity();
    }

    /// Queue `d` for the next capable pull; false = the table is full
    /// (the caller runs the residue locally).
    fn fleetQueue(ctx: *anyopaque, d: seams.FleetDispatch.Descriptor, arg0: u64) bool {
        const self: *Coordinator = @ptrCast(@alignCast(ctx));
        const rs = self.gpa.create(ReadShard) catch return false;
        rs.* = .{ .d = d, .arg0 = arg0 };
        self.lock.lock();
        defer self.lock.unlock();
        for (&self.read_shards) |*slot| if (slot.* == null) {
            slot.* = rs;
            stats.job_fleet_shards_dispatched.inc();
            return true;
        };
        self.gpa.destroy(rs);
        return false;
    }

    fn findReadShardLocked(self: *Coordinator, id: ShardId) ?*ReadShard {
        for (self.read_shards[0..]) |slot| if (slot) |rs| if (rs.d.id.eql(id)) return rs;
        return null;
    }

    /// Park until the shard is done or lost; the progress bound (TTLs) and
    /// a queued shard with no capable session left are the lost arms.
    fn fleetCollect(ctx: *anyopaque, io: Io, _: Allocator, id: ShardId) seams.ReadShardOutcome {
        const self: *Coordinator = @ptrCast(@alignCast(ctx));
        const deadline = clock.monotonicNs() + READ_SHARD_PROGRESS_TTLS * @as(u64, self.table.ttl_ms) * std.time.ns_per_ms;
        while (true) {
            self.lock.lock();
            const rs = self.findReadShardLocked(id) orelse {
                self.lock.unlock();
                return .lost;
            };
            switch (rs.state) {
                .done => {
                    const p = rs.payload.?;
                    rs.payload = null;
                    self.lock.unlock();
                    stats.job_fleet_shards_completed.inc();
                    return .{ .payload = p };
                },
                .lost => {
                    self.lock.unlock();
                    stats.job_fleet_shards_relocal.inc();
                    return .lost;
                },
                .queued, .leased => {},
            }
            const timed_out = clock.monotonicNs() >= deadline;
            const no_capacity = rs.state == .queued and self.readCapacityLocked() == 0;
            if (timed_out or no_capacity) {
                const leased_id: ?ShardId = if (rs.state == .leased) rs.d.id else null;
                rs.state = .lost;
                self.lock.unlock();
                // A holder still mid-walk past the bound: its lease is
                // expired now, so its late proposal is refused stale.
                if (leased_id) |lid| _ = self.table.lease().expire(lid, clock.monotonicNs());
                stats.job_fleet_shards_relocal.inc();
                return .lost;
            }
            var n = rs.settled.notified();
            _ = n.enable(io);
            self.lock.unlock();
            _ = n.waitUntilUncancelable(io, @min(deadline, clock.monotonicNs() + lease_mod.sweepTickNs(self.table.ttl_ms)));
        }
    }

    fn fleetRelease(ctx: *anyopaque, _: Allocator, payload: []const u8) void {
        const self: *Coordinator = @ptrCast(@alignCast(ctx));
        self.gpa.free(payload);
    }

    /// Drop the job's read shards and their leases (pass end).
    fn fleetRetire(ctx: *anyopaque, job_id: u64) void {
        const self: *Coordinator = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        for (&self.read_shards) |*slot| if (slot.*) |rs| if (rs.d.id.job_id == job_id) {
            if (rs.payload) |p| self.gpa.free(p);
            self.gpa.destroy(rs);
            slot.* = null;
        };
        self.lock.unlock();
        self.table.lease().retire(job_id);
    }

    fn fleetTtl(ctx: *anyopaque) u32 {
        const self: *Coordinator = @ptrCast(@alignCast(ctx));
        return self.table.ttl_ms;
    }

    fn serve(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *Coordinator = @ptrCast(@alignCast(ctx));
        const who = identityOf(req.peer) orelse return service.Answer.rejected("job verbs need an admitted member identity");
        return switch (req.payload) {
            .job_enroll => |p| self.enroll(who, req.session_id, p),
            .shard_assign => |p| self.assign(who, p, scratch),
            .heartbeat => |p| self.heartbeat(p),
            .result_submit => |p| self.submit(p),
            .read_shard_result => |p| self.readResult(p),
            .shard_abandon => |p| self.abandon(p),
            else => unreachable,
        };
    }

    fn enroll(self: *Coordinator, who: StableIdentity, session_id: u64, p: frames.JobEnrollReq) service.Answer {
        if (!self.fabric.is_coordinator.load(.acquire)) {
            stats.job_remote_enroll_refused.inc();
            return .{ .status = .{ .status = .not_manager, .err = error.NotManager, .reason = "not the coordinator (volume 0's manager)" } };
        }
        self.lock.lock();
        var first = true;
        for (self.workers[0..self.n_workers]) |slot| if (slot != null) {
            first = false;
        };
        if (self.findWorkerLocked(who)) |e| {
            e.pr_key = p.pr_key;
            e.caps = p.caps;
            e.session_id = session_id;
        } else {
            var free: ?*?Enrolled = null;
            for (self.workers[0..self.n_workers]) |*slot| if (slot.* == null) {
                free = slot;
                break;
            };
            if (free == null) {
                if (self.n_workers >= MAX_WORKERS) {
                    self.lock.unlock();
                    stats.job_remote_enroll_refused.inc();
                    return service.Answer.deferred("the worker table is full");
                }
                free = &self.workers[self.n_workers];
                self.n_workers += 1;
            }
            free.?.* = .{ .identity = who, .pr_key = p.pr_key, .caps = p.caps, .session_id = session_id };
        }
        var n: usize = 0;
        for (self.workers[0..self.n_workers]) |slot| if (slot != null) {
            n += 1;
        };
        const take_wero = first and !self.wero_held;
        if (take_wero) self.wero_held = true;
        self.lock.unlock();
        stats.job_remote_workers.set(n);
        stats.job_remote_enrollments.inc();
        if (take_wero) {
            if (self.wero) |w| {
                const pr = w.acquire(w.ctx);
                stats.job_remote_fence_mode.set(if (pr) 1 else 0);
                if (!pr) log.warn("job fabric: not every data namespace is PR-capable — the remote fence runs in the deferred-reclaim class (an expired lease's destinations stay quarantined until fsck returns them); no bypass knob", .{});
            } else {
                stats.job_remote_fence_mode.set(0);
                log.warn("job fabric: no data namespaces armed for WERO — the remote fence runs in the deferred-reclaim class", .{});
            }
        }
        return .{ .ok = .{ .job_enroll = .{ .heartbeat_ms = seams.HEARTBEAT_MS, .lease_ttl_ms = self.table.ttl_ms } } };
    }

    /// The worker pulls: a queued fleet READ shard first when the worker
    /// is read-capable (leased to it with no destinations — the fencing
    /// arm alone), else a wire-executable job claimed for it becomes shard
    /// 1 of that job; none ⇒ the empty descriptor.
    fn assign(self: *Coordinator, who: StableIdentity, p: frames.ShardAssignReq, _: []u8) service.Answer {
        {
            self.lock.lock();
            defer self.lock.unlock();
            const e = self.findWorkerLocked(who) orelse return service.Answer.rejected("shard_assign before job_enroll");
            if (p.caps & frames.CAP_FLEET_READ != 0 and e.caps & frames.CAP_FLEET_READ != 0) {
                for (self.read_shards[0..]) |slot| if (slot) |rs| if (rs.state == .queued) {
                    const a = self.table.lease().assign(rs.d.id, who, &.{}, clock.monotonicNs()) catch continue;
                    rs.state = .leased;
                    rs.attempts += 1;
                    stats.job_remote_shards.inc();
                    const head: frames.ShardDescriptorHead = .{
                        .job_id = rs.d.id.job_id,
                        .shard = rs.d.id.shard,
                        .shard_fencing_lo = @truncate(a.fencing),
                        .shard_fencing_hi = @truncate(a.fencing >> 32), // the descriptor's two u32 halves (`ShardDescriptorHead.fencing`)
                        .lease_ttl_ms = a.lease_ttl_ms,
                        .job_type = @backingInt(records.JobType.fsck),
                        .throttle_pct = rs.d.throttle_pct,
                        .reserved0 = @splat(0),
                        .n_src = 0,
                        .n_dest = 0,
                        .block_len = 0,
                        .arg0 = rs.arg0,
                        .arg1 = 0,
                        .fleet_k = rs.d.fleet_k,
                        .fleet_n = rs.d.fleet_n,
                        .inode_plane = rs.d.inode_plane,
                    };
                    return .{ .ok = .{ .shard_assign = .{ .head = head, .sources = &.{}, .destinations = &.{} } } };
                };
            }
        }
        const job = self.fabric.claimNext(self.io, true) orelse return .{ .ok = .{ .shard_assign = frames.ShardDescriptor.none } };
        const id: ShardId = .{ .job_id = job.id(), .shard = 1 };
        const a = self.table.lease().assign(id, who, &.{}, clock.monotonicNs()) catch {
            self.fabric.requeue(self.io, job);
            return service.Answer.deferred("the shard table is full");
        };
        self.lock.lock();
        var slot: ?*?Bound = null;
        for (self.bound[0..]) |*b| if (b.* == null) {
            slot = b;
            break;
        };
        if (slot) |sl| sl.* = .{ .id = id, .job = job };
        self.lock.unlock();
        const head: frames.ShardDescriptorHead = .{
            .job_id = id.job_id,
            .shard = id.shard,
            .shard_fencing_lo = @truncate(a.fencing),
            .shard_fencing_hi = @truncate(a.fencing >> 32), // the descriptor's two u32 halves (`ShardDescriptorHead.fencing`)
            .lease_ttl_ms = a.lease_ttl_ms,
            .job_type = @backingInt(job.jobType()),
            .throttle_pct = job.ctl.loadPct(),
            .reserved0 = @splat(0),
            .n_src = 0,
            .n_dest = 0,
            .block_len = 0,
            .arg0 = job.rec.arg0,
            .arg1 = job.rec.arg1,
            .fleet_k = 0,
            .fleet_n = 0,
            .inode_plane = 0,
        };
        return .{ .ok = .{ .shard_assign = .{ .head = head, .sources = &.{}, .destinations = &.{} } } };
    }

    fn heartbeat(self: *Coordinator, p: frames.HeartbeatReq) service.Answer {
        const v = self.table.lease().renew(.{ .job_id = p.job_id, .shard = p.shard }, p.shard_fencing, clock.monotonicNs());
        return .{ .ok = .{ .heartbeat = .{ .lease_ttl_ms = self.table.ttl_ms, .verdict = @backingInt(v), .reserved = @splat(0) } } };
    }

    fn refusedAck(reason: frames.RefusalReason) service.Answer {
        return .{ .ok = .{ .result_submit = frames.ResultAck.of(reason) } };
    }

    /// The proposal: SCREEN (fencing, count, window) → VERIFY-READ →
    /// complete the shard → the bound job's terminal step.
    fn submit(self: *Coordinator, p: frames.ResultSubmitReq) service.Answer {
        const id: ShardId = .{ .job_id = p.head.ref.job_id, .shard = p.head.ref.shard };
        const fencing = p.head.ref.shard_fencing;
        switch (self.table.lease().check(id, fencing)) {
            .unknown => return refusedAck(.unknown_shard),
            .stale => return refusedAck(.stale),
            .live => {},
        }
        // Every tuple must be one the coordinator pre-allocated for THIS
        // fencing — compared before any device read.
        for (p.checksums) |c| {
            if (!self.table.ownsDest(id, fencing, .{ .vol_tag = c.vol_tag, .block_idx = c.block_idx })) {
                stats.job_remote_refused_stale.inc();
                return refusedAck(.window);
            }
        }
        const pm = self.permille();
        for (p.checksums, 0..) |c, i| {
            if (!sampled(pm, id.job_id, id.shard, i)) continue;
            const got = self.verify.read_csum(self.verify.ctx, .{ .vol_tag = c.vol_tag, .block_idx = c.block_idx }, c.len) catch {
                stats.job_remote_verify_failures.inc();
                _ = self.table.lease().expire(id, clock.monotonicNs());
                self.requeueExpired();
                return refusedAck(.verify_failed);
            };
            stats.job_remote_verify_read_bytes.add(c.len);
            if (got != c.csum) {
                stats.job_remote_verify_failures.inc();
                log.err("job fabric: shard ({x}, {d}) proposed vol {x} block {d} with checksum {x} but the device reads {x} — refused; the lease is treated as expired (fencing bump + quarantine), nothing published", .{ id.job_id, id.shard, c.vol_tag, c.block_idx, c.csum, got });
                _ = self.table.lease().expire(id, clock.monotonicNs());
                self.requeueExpired();
                return refusedAck(.verify_failed);
            }
        }
        if (self.table.lease().complete(id, fencing) != .live) return refusedAck(.stale);
        var moved: u64 = 0;
        for (p.checksums) |c| moved += c.len;
        stats.job_remote_bytes_moved.add(moved);
        self.finishBound(id);
        return refusedAck(.accepted);
    }

    /// A whole-job remote shard landed: the bound fabric job completes
    /// (the noop's cursor set to its total so the record tells the truth).
    fn finishBound(self: *Coordinator, id: ShardId) void {
        var job: ?*JobCtl = null;
        {
            self.lock.lock();
            defer self.lock.unlock();
            for (self.bound[0..]) |*slot| if (slot.*) |b| if (b.id.eql(id)) {
                job = b.job;
                slot.* = null;
            };
        }
        const j = job orelse return;
        self.fabric.lock.lockUncancelable(self.io);
        if (j.jobType() == .noop) {
            j.rec.tasks_done = j.rec.arg0;
            j.rec.tasks_total = j.rec.arg0;
        }
        self.fabric.lock.unlock(self.io);
        self.fabric.finishRemote(self.io, j, .completed);
        self.table.lease().retire(id.job_id);
    }

    /// A fleet READ shard's proposal: fencing-checked exactly like a
    /// mutating one (`complete` under the current fencing), the payload
    /// kept for the collector; a stale or unknown shard is refused, a
    /// shard the collector already gave up (lost) is a replay.
    fn readResult(self: *Coordinator, p: frames.ReadShardResultReq) service.Answer {
        const id: ShardId = .{ .job_id = p.head.ref.job_id, .shard = p.head.ref.shard };
        // A non-live verdict is counted by the table (`job_remote_refused_stale`).
        if (self.table.lease().complete(id, p.head.ref.shard_fencing) != .live) return service.Answer.rejected("stale or unknown read shard");
        stats.job_remote_submissions.inc();
        const copy = self.gpa.dupe(u8, p.payload) catch return service.Answer.refused(error.OutOfMemory, "out of memory");
        var wake: ?*ReadShard = null;
        {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.findReadShardLocked(id)) |rs| {
                if (rs.state == .leased) {
                    rs.state = .done;
                    rs.payload = copy;
                    wake = rs;
                }
            }
        }
        const rs = wake orelse {
            self.gpa.free(copy);
            return .{ .ok = .{ .read_shard_result = verbs.Ack.replay } };
        };
        rs.settled.notifyWaiters(self.io);
        return .{ .ok = .{ .read_shard_result = verbs.Ack.fresh } };
    }

    fn abandon(self: *Coordinator, p: frames.ShardAbandonReq) service.Answer {
        const id: ShardId = .{ .job_id = p.ref.job_id, .shard = p.ref.shard };
        if (self.table.lease().check(id, p.ref.shard_fencing) == .live) {
            stats.job_remote_abandons.inc();
            _ = self.table.lease().expire(id, clock.monotonicNs());
            self.requeueExpired();
            return .{ .ok = .{ .shard_abandon = verbs.Ack.fresh } };
        }
        return .{ .ok = .{ .shard_abandon = verbs.Ack.replay } };
    }
};

// ---------------------------------------------------------------------------
// The worker side.
// ---------------------------------------------------------------------------

/// Any caller: the TCP dialer's session or the loopback `Conn`.
pub const Caller = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply,
};

/// Destinations per lease re-validation: rung 1 of the zombie ladder —
/// exposure ≤ one batch.
pub const REVALIDATE_BATCH: usize = 16;

pub const WorkerError = error{ EnrollRefused, Incapable } || errors.Error;

/// A fleet READ shard as the worker's engine sees it.
pub const ReadDescriptor = struct {
    job_id: u64,
    shard: u32,
    fleet_k: u32,
    fleet_n: u32,
    /// `fsck.JOB_ARG_*`.
    arg0: u64,
    throttle_pct: u8,
    inode_plane: u64,
};

/// The engine a read-capable worker runs a shard on (the fsck runner over
/// the member's OWN binding — its plane's read path, its posture's scope;
/// a RAM runner in the loopback tests). `cancel` is raised when the lease
/// clock expired or the worker stops: the walk ends at its next step and
/// the runner answers null (nothing is proposed). The report (JSON, owned
/// by `gpa`) otherwise.
pub const ReadRunner = struct {
    ctx: *anyopaque,
    run: *const fn (ctx: *anyopaque, io: Io, gpa: Allocator, d: ReadDescriptor, cancel: *const Atomic(bool)) error{ OutOfMemory, Io }!?[]u8,
};

/// `job worker`: enroll, then pull shards until the coordinator goes away
/// or `stop` latches. A local lease clock starts a full TTL at assignment
/// and every `HeartbeatAck` extends it; past half-TTL ONE heartbeat
/// round-trip precedes the next batch; expired ⇒ abort the shard (no
/// submission, a `shard_abandon`).
pub const Worker = struct {
    io: Io,
    caller: Caller,
    /// The read-shard engine; null = a `noop`-only worker (a read shard
    /// pulled without one is abandoned `incapable`).
    read_runner: ?ReadRunner = null,
    /// The read shards' report buffers.
    gpa: Allocator,
    caps: u32 = 0,
    pr_key: u64 = 0,
    heartbeat_ms: u32 = seams.HEARTBEAT_MS,
    lease_ttl_ms: u32 = seams.LEASE_TTL_MS,
    stop: Atomic(bool) = .init(false),
    wake: sync.Notify = .init,
    /// `SQUEEZEFS_TEST_JOB_HOLD_SUBMISSION`: park before `result_submit`
    /// (or a read shard's `read_shard_result`) until `release_hold` fires
    /// (the zombie window).
    hold_submission: bool = false,
    release_hold: sync.Notify = .init,
    held: Atomic(bool) = .init(false),
    /// The read shard heartbeat fiber's park station (null in the product).
    heartbeat_station: ?HeartbeatStation = null,
    /// Instruments the loopback tests read.
    shards_done: u64 = 0,
    shards_refused: u64 = 0,
    heartbeats: u64 = 0,
    read_shards_run: u64 = 0,

    pub fn enroll(self: *Worker) WorkerError!frames.JobEnrolled {
        const rep = self.caller.call(self.caller.ctx, .{ .job_enroll = .{ .pr_key = self.pr_key, .caps = self.caps, .reserved = 0 } }) catch |e| return switch (e) {
            error.NotManager, error.Rejected => error.EnrollRefused,
            else => e,
        };
        const en = rep.job_enroll;
        self.heartbeat_ms = en.heartbeat_ms;
        self.lease_ttl_ms = en.lease_ttl_ms;
        return en;
    }

    pub const Pulled = enum { none, done, refused, abandoned };

    /// The heartbeat fiber's station seam: `parking` runs on the fiber once
    /// per park, after it decided to park (its `stop` read false) and
    /// before the park begins — the lost-wake pin's rendezvous (a test
    /// raises the stop inside that window and reads `wake.waiters`).
    pub const HeartbeatStation = struct {
        ctx: *anyopaque,
        parking: *const fn (ctx: *anyopaque, wake: *sync.Notify, stop: *const Atomic(bool)) void,
    };

    /// ONE pull: a descriptor or none; execute; propose.
    pub fn pullOnce(self: *Worker) WorkerError!Pulled {
        const rep = try self.caller.call(self.caller.ctx, .{ .shard_assign = .{ .caps = self.caps, .reserved = 0 } });
        const d = rep.shard_assign;
        if (d.isNone()) return .none;
        const ref: frames.ShardRef = .{ .job_id = d.head.job_id, .shard = d.head.shard, .reserved = 0, .shard_fencing = d.head.fencing() };
        const job_type: records.JobType = @fromBackingInt(@intCast(d.head.job_type));
        if (job_type == .fsck and d.head.fleet_n != 0) return self.runReadShard(d, ref);
        if (job_type != .noop) {
            // Only noop ships whole over the wire and fsck ships as READ
            // shards; a mover is never a worker's.
            _ = try self.caller.call(self.caller.ctx, .{ .shard_abandon = .{ .ref = ref, .reason = @backingInt(frames.AbandonReason.incapable), .reserved = @splat(0) } });
            return .abandoned;
        }
        var deadline = clock.monotonicNs() + @as(u64, d.head.lease_ttl_ms) * std.time.ns_per_ms;
        const tasks = d.head.arg0;
        const task_ns = d.head.arg1 * std.time.ns_per_ms;
        var done: u64 = 0;
        while (done < tasks) : (done += 1) {
            if (done % REVALIDATE_BATCH == 0) switch (try self.revalidate(ref, &deadline)) {
                .live => {},
                .expired => {
                    _ = try self.caller.call(self.caller.ctx, .{ .shard_abandon = .{ .ref = ref, .reason = @backingInt(frames.AbandonReason.lease_expired), .reserved = @splat(0) } });
                    return .abandoned;
                },
            };
            if (task_ns > 0) self.parkUnlessStopped(clock.monotonicNs() + task_ns);
            if (self.stop.load(.acquire)) {
                _ = try self.caller.call(self.caller.ctx, .{ .shard_abandon = .{ .ref = ref, .reason = @backingInt(frames.AbandonReason.stopped), .reserved = @splat(0) } });
                return .abandoned;
            }
        }
        if (self.hold_submission) self.holdForRelease();
        if (clock.monotonicNs() >= deadline) {
            _ = try self.caller.call(self.caller.ctx, .{ .shard_abandon = .{ .ref = ref, .reason = @backingInt(frames.AbandonReason.lease_expired), .reserved = @splat(0) } });
            return .abandoned;
        }
        const ack = try self.caller.call(self.caller.ctx, .{ .result_submit = .{ .head = .{ .ref = ref, .n = 0, .reserved = 0 }, .checksums = &.{} } });
        if (ack.result_submit.verdict() == .accepted) {
            self.shards_done += 1;
            return .done;
        }
        self.shards_refused += 1;
        return .refused;
    }

    /// A fleet READ shard (spec §1.9a): the engine's sharded walk on this
    /// member's own binding while a heartbeat fiber renews the lease
    /// underneath (the half-TTL law: one round trip per `heartbeat_ms`; a
    /// `stale`/`unknown` verdict or a dead wire raises the cancel and the
    /// walk ends at its next step — nothing is proposed); the report is
    /// PROPOSED as `read_shard_result` under the lease's fencing. The
    /// coordinator fencing-checks it like any proposal.
    fn runReadShard(self: *Worker, d: frames.ShardDescriptor, ref: frames.ShardRef) WorkerError!Pulled {
        const runner = self.read_runner orelse {
            _ = try self.caller.call(self.caller.ctx, .{ .shard_abandon = .{ .ref = ref, .reason = @backingInt(frames.AbandonReason.incapable), .reserved = @splat(0) } });
            return .abandoned;
        };
        var hb: Heartbeat = .{ .worker = self, .ref = ref, .deadline_ns = .init(clock.monotonicNs() + @as(u64, d.head.lease_ttl_ms) * std.time.ns_per_ms) };
        var hb_task = self.io.concurrent(Heartbeat.run, .{&hb}) catch return error.OutOfMemory;
        const desc: ReadDescriptor = .{ .job_id = d.head.job_id, .shard = d.head.shard, .fleet_k = d.head.fleet_k, .fleet_n = d.head.fleet_n, .arg0 = d.head.arg0, .throttle_pct = d.head.throttle_pct, .inode_plane = d.head.inode_plane };
        stats.job_fleet_worker_shards.inc();
        const payload = runner.run(runner.ctx, self.io, self.gpa, desc, &hb.cancel) catch |e| switch (e) {
            error.OutOfMemory => blk: {
                hb.stopRequested(self.io);
                hb_task.cancel(self.io);
                break :blk null;
            },
            error.Io => null,
        };
        hb.stopRequested(self.io);
        hb_task.cancel(self.io);
        self.heartbeats += hb.beats;
        defer if (payload) |p| self.gpa.free(p);
        if (self.hold_submission) self.holdForRelease();
        if (payload == null or hb.cancel.load(.acquire) or clock.monotonicNs() >= hb.deadline_ns.load(.acquire) or self.stop.load(.acquire)) {
            // Expired, stopped, or the engine failed: abandon, never a
            // proposal under a lease this worker cannot vouch for.
            const reason: frames.AbandonReason = if (self.stop.load(.acquire)) .stopped else if (payload == null and !hb.cancel.load(.acquire)) .incapable else .lease_expired;
            _ = try self.caller.call(self.caller.ctx, .{ .shard_abandon = .{ .ref = ref, .reason = @backingInt(reason), .reserved = @splat(0) } });
            return .abandoned;
        }
        const p = payload.?;
        if (p.len > frames.READ_SHARD_PAYLOAD_MAX) {
            _ = try self.caller.call(self.caller.ctx, .{ .shard_abandon = .{ .ref = ref, .reason = @backingInt(frames.AbandonReason.incapable), .reserved = @splat(0) } });
            return .abandoned;
        }
        self.read_shards_run += 1;
        // A stale or unknown shard is the coordinator's REJECTION (the
        // fencing moved under a zombie): refused, never a worker failure.
        const ack = self.caller.call(self.caller.ctx, .{ .read_shard_result = .{ .head = .{ .ref = ref, .len = @intCast(p.len), .reserved = 0 }, .payload = p } }) catch |e| switch (e) {
            error.Rejected => {
                self.shards_refused += 1;
                return .refused;
            },
            else => return e,
        };
        if (ack.read_shard_result.already == 0) {
            self.shards_done += 1;
            return .done;
        }
        self.shards_refused += 1;
        return .refused;
    }

    /// The read shard's heartbeat fiber: a renewal every `heartbeat_ms`
    /// (the lease clock extended by each ack); a non-live verdict or a dead
    /// wire raises `cancel`. Parks on the worker's own notify (never a
    /// sleep), so both ends wake it at once: `stopRequested` (the walk's
    /// end) ends it, and the worker's `requestStop` raises the walk's
    /// cancel (the walk ends at its next step, not a heartbeat period later).
    const Heartbeat = struct {
        worker: *Worker,
        ref: frames.ShardRef,
        /// The local lease clock (release by the fiber's ack, acquire by
        /// the proposer's check).
        deadline_ns: Atomic(u64),
        cancel: Atomic(bool) = .init(false),
        stop: Atomic(bool) = .init(false),
        beats: u64 = 0,

        fn stopRequested(self: *Heartbeat, io: Io) void {
            self.stop.store(true, .release);
            self.worker.wake.notifyWaiters(io);
        }

        /// The walk's end, or the worker's stop (which cancels the walk).
        fn ended(self: *Heartbeat) bool {
            if (self.stop.load(.acquire)) return true;
            if (!self.worker.stop.load(.acquire)) return false;
            self.cancel.store(true, .release);
            return true;
        }

        fn run(self: *Heartbeat) void {
            const w = self.worker;
            const period_ns = @as(u64, w.heartbeat_ms) * std.time.ns_per_ms;
            while (true) {
                var n = w.wake.notified();
                // Registered BEFORE both stops are re-read (`sync/notify.zig`'s
                // law): a `stopRequested` or a `requestStop` from here on
                // bumps this waiter's epoch, so the park below ends on it.
                _ = n.enable(w.io);
                if (self.ended()) {
                    n.cancel(w.io);
                    return;
                }
                if (w.heartbeat_station) |s| s.parking(s.ctx, &w.wake, &self.stop);
                _ = n.waitUntilUncancelable(w.io, clock.monotonicNs() + period_ns);
                if (self.ended()) return;
                const rep = w.caller.call(w.caller.ctx, .{ .heartbeat = self.ref }) catch {
                    self.cancel.store(true, .release);
                    return;
                };
                self.beats += 1;
                if (rep.heartbeat.verdict != @backingInt(seams.LeaseVerdict.live)) {
                    self.cancel.store(true, .release);
                    return;
                }
                self.deadline_ns.store(clock.monotonicNs() + @as(u64, rep.heartbeat.lease_ttl_ms) * std.time.ns_per_ms, .release);
            }
        }
    };

    const Lease = enum { live, expired };

    /// Expired ⇒ abort; past half-TTL ⇒ one heartbeat round trip.
    fn revalidate(self: *Worker, ref: frames.ShardRef, deadline: *u64) WorkerError!Lease {
        const now = clock.monotonicNs();
        if (now >= deadline.*) return .expired;
        const ttl_ns = @as(u64, self.lease_ttl_ms) * std.time.ns_per_ms;
        if (deadline.* - now > ttl_ns / 2) return .live;
        const rep = try self.caller.call(self.caller.ctx, .{ .heartbeat = ref });
        self.heartbeats += 1;
        if (rep.heartbeat.verdict != @backingInt(seams.LeaseVerdict.live)) return .expired;
        deadline.* = clock.monotonicNs() + @as(u64, rep.heartbeat.lease_ttl_ms) * std.time.ns_per_ms;
        return .live;
    }

    /// The loop: pull until stop; park `heartbeat_ms / 4` on none (bounded,
    /// never a sleep).
    pub fn serve(self: *Worker) WorkerError!void {
        while (!self.stop.load(.acquire)) {
            const r = try self.pullOnce();
            if (r == .none) self.parkUnlessStopped(clock.monotonicNs() + @as(u64, self.heartbeat_ms) * std.time.ns_per_ms / 4);
        }
    }

    /// Park on `wake` until `deadline_ns` unless the stop is raised:
    /// registered before the stop is re-read (`sync/notify.zig`'s law), so
    /// a `requestStop` landing anywhere ends the park.
    pub fn parkUnlessStopped(self: *Worker, deadline_ns: u64) void {
        var n = self.wake.notified();
        _ = n.enable(self.io);
        if (self.stop.load(.acquire)) {
            n.cancel(self.io);
            return;
        }
        _ = n.waitUntilUncancelable(self.io, deadline_ns);
    }

    /// The zombie-window seam's park: registered on `release_hold` BEFORE
    /// `held` is published (the test's cue to release) and the stop is
    /// re-read, so neither the release nor a `requestStop` can land in a
    /// window the park misses (an unbounded park otherwise).
    fn holdForRelease(self: *Worker) void {
        var n = self.release_hold.notified();
        _ = n.enable(self.io);
        self.held.store(true, .release);
        if (self.stop.load(.acquire)) n.cancel(self.io) else n.waitUncancelable(self.io);
        self.held.store(false, .release);
    }

    /// Raise the stop and wake every park that reads it: the serve loop's
    /// and a noop shard's (`parkUnlessStopped`), a read shard's heartbeat
    /// fiber (which then cancels the walk) and the zombie seam's hold.
    pub fn requestStop(self: *Worker) void {
        self.stop.store(true, .release);
        self.wake.notifyWaiters(self.io);
        self.release_hold.notifyWaiters(self.io);
    }
};

// ---------------------------------------------------------------------------
// Tests over the loopback.
// ---------------------------------------------------------------------------

const testing = std.testing;
const loopback = @import("../../wire/loopback.zig");

var seed: Atomic(u64) = .init(0xc0ffee);
fn rnd() u64 {
    return seed.fetchAdd(0x9E37_79B9_7F4A_7C15, .monotonic) | 1;
}

const RamVerify = struct {
    sums: std.AutoHashMapUnmanaged(u128, u64) = .empty,
    fail_io: bool = false,
    fn vr(self: *RamVerify) VerifyRead {
        return .{ .ctx = self, .read_csum = &read };
    }
    fn read(ctx: *anyopaque, d: DestTuple, _: u64) error{Io}!u64 {
        const self: *RamVerify = @ptrCast(@alignCast(ctx));
        if (self.fail_io) return error.Io;
        return self.sums.get((@as(u128, d.vol_tag) << 64) | d.block_idx) orelse 0;
    }
};

const ConnCaller = struct {
    conn: *loopback.Conn,
    fn caller(self: *ConnCaller) Caller {
        return .{ .ctx = self, .call = &call };
    }
    fn call(ctx: *anyopaque, req: verbs.Request) errors.Error!verbs.Reply {
        const self: *ConnCaller = @ptrCast(@alignCast(ctx));
        return self.conn.call(req);
    }
};

/// The sweeper's clock two TTLs ahead: every lease the rig holds has
/// expired there, while a worker's own lease clock runs on the wall from
/// its assignment (a whole TTL) — a zombie row's window never depends on
/// the venue's pace.
fn pastTtl() u64 {
    return clock.monotonicNs() + 2 * @as(u64, seams.LEASE_TTL_MS) * std.time.ns_per_ms;
}

const Rig = struct {
    hub: loopback.LoopbackHub,
    coord_svc: service.Service = .init,
    worker_svc: service.Service = .init,
    store: fabric_mod.RamStore,
    fabric: Fabric,
    sink: lease_mod.RamSink,
    table: ShardTable,
    verify: RamVerify = .{},
    coord: Coordinator,
    conn: loopback.Conn,
    cc: ConnCaller,
    worker: Worker,

    fn init(gpa: Allocator, io: Io, ttl_ms: u32) !*Rig {
        const r = try gpa.create(Rig);
        r.* = .{ .hub = loopback.LoopbackHub.init(gpa, io), .store = fabric_mod.RamStore.init(gpa), .fabric = undefined, .sink = lease_mod.RamSink.init(gpa), .table = undefined, .coord = undefined, .conn = undefined, .cc = undefined, .worker = undefined };
        r.fabric = Fabric.init(gpa, r.store.store(), .{ .identity = .{ .node_token = 1, .mount_slot = 1 }, .pid = 1, .random = &rnd });
        r.table = ShardTable.init(ttl_ms, r.sink.sink(), null);
        r.coord = Coordinator.init(gpa, io, &r.fabric, &r.table, .{ .verify = r.verify.vr() });
        r.coord.register(&r.coord_svc);
        const secret: auth.Secret = @splat(0x42);
        _ = try r.hub.addDaemon(&r.coord_svc, secret, .{ .node_token = 1, .writer_id = 1, .mount_slot = 1, .reserved = 0 });
        _ = try r.hub.addDaemon(&r.worker_svc, secret, .{ .node_token = 2, .writer_id = 2, .mount_slot = 1, .reserved = 0 });
        r.conn = try r.hub.connect(1, 0);
        r.cc = .{ .conn = &r.conn };
        r.worker = .{ .io = io, .caller = r.cc.caller(), .gpa = gpa };
        return r;
    }

    fn deinit(self: *Rig, gpa: Allocator) void {
        self.conn.deinit();
        self.fabric.deinit();
        self.store.deinit();
        self.sink.deinit();
        self.verify.sums.deinit(gpa);
        self.hub.deinit();
        gpa.destroy(self);
    }
};

test "verify strength keys on the channel: 1000 pinned on plaintext, the knob honoured under mTLS; sampling is deterministic and every tuple at 1000" {
    try testing.expectEqual(@as(u16, 1000), verifyPermille(.plaintext, 50));
    try testing.expectEqual(@as(u16, 50), verifyPermille(.mutual_tls, 50));
    try testing.expect(sampled(1000, 1, 1, 0) and sampled(1000, 1, 1, 63));
    var hits: usize = 0;
    var i: usize = 0;
    while (i < 1000) : (i += 1) if (sampled(100, 7, 1, i)) {
        hits += 1;
    };
    try testing.expect(hits > 50 and hits < 150);
    try testing.expectEqual(sampled(100, 7, 1, 5), sampled(100, 7, 1, 5));
}

test "remote worker executes a noop shard end to end over the loopback: enroll (WERO deferred-reclaim loud once), pull, execute, propose; the coordinator completes the job durably; a pull with nothing queued is none; a verb before enroll is rejected" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io, 30_000);
    defer r.deinit(gpa);
    try testing.expectError(error.Rejected, r.cc.conn.call(.{ .shard_assign = .{ .caps = 0, .reserved = 0 } }));
    const en = try r.worker.enroll();
    try testing.expectEqual(seams.HEARTBEAT_MS, en.heartbeat_ms);
    try testing.expectEqual(@as(u32, 30_000), en.lease_ttl_ms);
    try testing.expectEqual(@as(usize, 1), r.coord.enrolledCount());
    try testing.expectEqual(@as(u64, 0), stats.job_remote_fence_mode.load());
    try testing.expectEqual(Worker.Pulled.none, try r.worker.pullOnce());
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 40, .arg1 = 0 });
    // A mover is never shipped whole.
    const mover = try r.fabric.submit(io, .{ .job_type = .defrag_fold });
    try testing.expectEqual(Worker.Pulled.done, try r.worker.pullOnce());
    try testing.expectEqual(records.JobState.completed, r.store.record(id).?.state);
    try testing.expectEqual(@as(u64, 40), r.store.record(id).?.tasks_done);
    try testing.expectEqual(records.JobState.queued, r.store.record(mover).?.state);
    try testing.expectEqual(Worker.Pulled.none, try r.worker.pullOnce());
    try testing.expectEqual(@as(u64, 1), r.worker.shards_done);
    try testing.expectEqual(@as(u64, 0), r.table.inFlight());
    // Departure releases the enrolment; a re-enroll is idempotent.
    r.coord.sessionDeparted(0);
    try testing.expectEqual(@as(usize, 0), r.coord.enrolledCount());
    _ = try r.worker.enroll();
    _ = try r.worker.enroll();
    try testing.expectEqual(@as(usize, 1), r.coord.enrolledCount());
}

test "lease expiry reassigns to the pool and refuses the late submit: a worker held before its submit loses the lease to the sweeper; the job re-queues; the late proposal is refused stale; a fresh pull re-leases at fencing 1" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io, seams.LEASE_TTL_MS);
    defer r.deinit(gpa);
    _ = try r.worker.enroll();
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1, .arg1 = 0 });
    r.worker.hold_submission = true;
    const Pull = struct {
        fn run(w: *Worker, out: *Worker.Pulled) void {
            out.* = w.pullOnce() catch .refused;
        }
    };
    var out: Worker.Pulled = .none;
    const th = try std.Thread.spawn(.{}, Pull.run, .{ &r.worker, &out });
    while (!r.worker.held.load(.acquire)) std.atomic.spinLoopHint();
    try testing.expectEqual(records.JobState.running, r.fabric.snapshot(io, id).?.state);
    try testing.expectEqual(@as(u64, 1), r.table.inFlight());
    // Past the TTL the sweeper expires it; the job is Queued for any population.
    const exp0 = stats.job_remote_lease_expiries.load();
    try testing.expectEqual(@as(usize, 1), r.coord.sweep(pastTtl()));
    try testing.expectEqual(exp0 + 1, stats.job_remote_lease_expiries.load());
    try testing.expectEqual(records.JobState.queued, r.fabric.snapshot(io, id).?.state);
    try testing.expect(!r.fabric.find(id).?.claimed.load(.acquire));
    // The zombie submits: refused stale, nothing completes.
    const stale0 = stats.job_remote_refused_stale.load();
    r.worker.release_hold.notifyWaiters(io);
    th.join();
    try testing.expectEqual(Worker.Pulled.refused, out);
    try testing.expectEqual(stale0 + 1, stats.job_remote_refused_stale.load());
    try testing.expectEqual(records.JobState.queued, r.store.record(id).?.state);
    // A fresh pull re-leases at fencing 1 and completes.
    r.worker.hold_submission = false;
    try testing.expectEqual(Worker.Pulled.done, try r.worker.pullOnce());
    try testing.expectEqual(records.JobState.completed, r.store.record(id).?.state);
}

test "a proposal is screened before any device read: a tuple outside the window is refused; a checksum mismatch on verify-read refuses, bumps the fencing and quarantines; heartbeats renew under the current fencing only; an abandon re-queues" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io, 30_000);
    defer r.deinit(gpa);
    _ = try r.worker.enroll();
    // A data-shaped shard assigned straight on the table (the mover's
    // remote arm is the fsck spec's; the SCREEN is pinned here).
    const dests = [_]DestTuple{ .{ .vol_tag = 5, .block_idx = 1 }, .{ .vol_tag = 5, .block_idx = 2 } };
    const sid: ShardId = .{ .job_id = 77, .shard = 3 };
    const a = try r.table.lease().assign(sid, .{ .node_token = 2, .mount_slot = 1 }, &dests, clock.monotonicNs());
    const ref: frames.ShardRef = .{ .job_id = 77, .shard = 3, .reserved = 0, .shard_fencing = a.fencing };
    const hb = try r.cc.conn.call(.{ .heartbeat = ref });
    try testing.expectEqual(@as(u8, 0), hb.heartbeat.verdict);
    const outside = [_]frames.BlockChecksum{.{ .vol_tag = 5, .block_idx = 9, .len = 4096, .csum = 1 }};
    const w = try r.cc.conn.call(.{ .result_submit = .{ .head = .{ .ref = ref, .n = 1, .reserved = 0 }, .checksums = &outside } });
    try testing.expectEqual(frames.RefusalReason.window, w.result_submit.verdict());
    try testing.expectEqual(seams.LeaseVerdict.live, r.table.lease().check(sid, a.fencing));
    // The right tuple, the wrong checksum.
    try r.verify.sums.put(gpa, (@as(u128, 5) << 64) | 1, 0xAAAA);
    const bad = [_]frames.BlockChecksum{.{ .vol_tag = 5, .block_idx = 1, .len = 4096, .csum = 0xBBBB }};
    log.testing.expectErrorLogs(1);
    const v = try r.cc.conn.call(.{ .result_submit = .{ .head = .{ .ref = ref, .n = 1, .reserved = 0 }, .checksums = &bad } });
    try log.testing.expectAllFired();
    try testing.expectEqual(frames.RefusalReason.verify_failed, v.result_submit.verdict());
    try testing.expectEqual(@as(?u64, a.fencing + 1), r.table.fencingOf(sid));
    try testing.expect(r.sink.sink().vtable.isQuarantined(r.sink.sink().ctx, dests[0]) or r.sink.released.items.len == 1);
    const hb2 = try r.cc.conn.call(.{ .heartbeat = ref });
    try testing.expectEqual(@as(u8, @backingInt(seams.LeaseVerdict.stale)), hb2.heartbeat.verdict);
    // Re-lease with fresh destinations; the right checksum lands.
    const fresh = [_]DestTuple{.{ .vol_tag = 5, .block_idx = 10 }};
    const b = try r.table.lease().assign(sid, .{ .node_token = 2, .mount_slot = 1 }, &fresh, clock.monotonicNs());
    try r.verify.sums.put(gpa, (@as(u128, 5) << 64) | 10, 0xCCCC);
    const good = [_]frames.BlockChecksum{.{ .vol_tag = 5, .block_idx = 10, .len = 4096, .csum = 0xCCCC }};
    const ref2: frames.ShardRef = .{ .job_id = 77, .shard = 3, .reserved = 0, .shard_fencing = b.fencing };
    const ok = try r.cc.conn.call(.{ .result_submit = .{ .head = .{ .ref = ref2, .n = 1, .reserved = 0 }, .checksums = &good } });
    try testing.expectEqual(frames.RefusalReason.accepted, ok.result_submit.verdict());
    const dup = try r.cc.conn.call(.{ .result_submit = .{ .head = .{ .ref = ref2, .n = 1, .reserved = 0 }, .checksums = &good } });
    try testing.expectEqual(frames.RefusalReason.stale, dup.result_submit.verdict());
    const unk = try r.cc.conn.call(.{ .result_submit = .{ .head = .{ .ref = .{ .job_id = 78, .shard = 1, .reserved = 0, .shard_fencing = 0 }, .n = 0, .reserved = 0 }, .checksums = &.{} } });
    try testing.expectEqual(frames.RefusalReason.unknown_shard, unk.result_submit.verdict());
    // An abandon of a live lease re-queues it; a second is a replay.
    const sid2: ShardId = .{ .job_id = 78, .shard = 1 };
    const c = try r.table.lease().assign(sid2, .{ .node_token = 2, .mount_slot = 1 }, &.{}, clock.monotonicNs());
    const ab = try r.cc.conn.call(.{ .shard_abandon = .{ .ref = .{ .job_id = 78, .shard = 1, .reserved = 0, .shard_fencing = c.fencing }, .reason = @backingInt(frames.AbandonReason.memory_red), .reserved = @splat(0) } });
    try testing.expectEqual(@as(u8, 0), ab.shard_abandon.already);
    const ab2 = try r.cc.conn.call(.{ .shard_abandon = .{ .ref = .{ .job_id = 78, .shard = 1, .reserved = 0, .shard_fencing = c.fencing }, .reason = @backingInt(frames.AbandonReason.memory_red), .reserved = @splat(0) } });
    try testing.expectEqual(@as(u8, 1), ab2.shard_abandon.already);
    try testing.expectEqual(@as(?u64, c.fencing + 1), r.table.fencingOf(sid2));
}

/// The loopback's read-shard engine: a canned report naming the residue,
/// a cancel-honouring park (the kill row's zombie), a failure arm.
const RamReadRunner = struct {
    io: Io,
    /// Park inside the run (the shard "mid-walk") until `releaseHold` or
    /// the cancel — a real engine reads the cancel at its next step.
    hold: bool = false,
    released: Atomic(bool) = .init(false),
    release: sync.Notify = .init,
    holding: Atomic(bool) = .init(false),
    fail: bool = false,
    runs: u64 = 0,
    cancelled: u64 = 0,

    /// The hold's poll grain: the engine's "next step".
    const STEP_NS: u64 = 10 * std.time.ns_per_ms;

    fn runner(self: *RamReadRunner) ReadRunner {
        return .{ .ctx = self, .run = &run };
    }

    fn releaseHold(self: *RamReadRunner, io: Io) void {
        self.released.store(true, .release);
        self.release.notifyWaiters(io);
    }

    fn run(ctx: *anyopaque, io: Io, gpa: Allocator, d: ReadDescriptor, cancel: *const Atomic(bool)) error{ OutOfMemory, Io }!?[]u8 {
        const self: *RamReadRunner = @ptrCast(@alignCast(ctx));
        self.runs += 1;
        if (self.fail) return error.Io;
        if (self.hold) {
            self.holding.store(true, .release);
            while (!self.released.load(.acquire) and !cancel.load(.acquire)) {
                var n = self.release.notified();
                _ = n.waitUntilUncancelable(io, clock.monotonicNs() + STEP_NS);
            }
            self.released.store(false, .release);
            self.holding.store(false, .release);
        }
        if (cancel.load(.acquire)) {
            self.cancelled += 1;
            return null;
        }
        return try std.fmt.allocPrint(gpa, "{{\"shard\":\"{d}/{d}\",\"job\":{d},\"arg0\":{d}}}", .{ d.fleet_k, d.fleet_n, d.job_id, d.arg0 });
    }
};

test "a fleet READ shard end to end over the loopback: the coordinator queues shard 1/2 (no destinations, the fencing arm alone), a read-capable worker pulls it, runs its engine, PROPOSES the report; the collector gets the payload (dispatched ≡ completed + relocal + pending); a worker without the capability never sees it; a queued shard with no capable session is LOST at once (relocal); the pass end retires the shard and a late proposal is the unknown-shard rejection" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io, 30_000);
    defer r.deinit(gpa);
    defer r.coord.deinit();
    var engine: RamReadRunner = .{ .io = io };
    r.worker.read_runner = engine.runner();
    const fd = r.coord.fleet();
    // No capable session: a queued shard is lost at once.
    try testing.expectEqual(@as(u32, 0), fd.readCapacity());
    const d1: seams.FleetDispatch.Descriptor = .{ .id = .{ .job_id = 0x900, .shard = 1 }, .fleet_k = 1, .fleet_n = 2, .inode_plane = 0, .throttle_pct = 100 };
    try testing.expect(fd.queueReadShard(d1, 1));
    const relocal0 = stats.job_fleet_shards_relocal.load();
    try testing.expectEqual(seams.ReadShardOutcome.lost, fd.collectReadShard(io, gpa, d1.id));
    try testing.expectEqual(relocal0 + 1, stats.job_fleet_shards_relocal.load());
    fd.retireFleetShards(0x900);
    // A noop-only worker (no capability) never receives a read shard.
    _ = try r.worker.enroll();
    try testing.expectEqual(@as(u32, 0), fd.readCapacity());
    try testing.expect(fd.queueReadShard(d1, 1));
    try testing.expectEqual(Worker.Pulled.none, try r.worker.pullOnce());
    // The capable worker: pull → run → propose → the collector's payload.
    r.worker.caps = frames.CAP_FLEET_READ;
    _ = try r.worker.enroll();
    try testing.expectEqual(@as(u32, 1), fd.readCapacity());
    const done0 = stats.job_fleet_shards_completed.load();
    try testing.expectEqual(Worker.Pulled.done, try r.worker.pullOnce());
    try testing.expectEqual(@as(u64, 1), engine.runs);
    try testing.expectEqual(@as(u64, 1), r.worker.read_shards_run);
    const out = fd.collectReadShard(io, gpa, d1.id);
    try testing.expect(out == .payload);
    defer fd.release(gpa, out.payload);
    try testing.expectEqualStrings("{\"shard\":\"1/2\",\"job\":2304,\"arg0\":1}", out.payload);
    try testing.expectEqual(done0 + 1, stats.job_fleet_shards_completed.load());
    try testing.expect(stats_mod.fleetClosure(0));
    // Retired: a late proposal under the old fencing is unknown.
    fd.retireFleetShards(0x900);
    try testing.expectError(error.Rejected, r.cc.conn.call(.{ .read_shard_result = .{ .head = .{ .ref = .{ .job_id = 0x900, .shard = 1, .reserved = 0, .shard_fencing = 0 }, .len = 2, .reserved = 0 }, .payload = "{}" } }));
    try testing.expectEqual(Worker.Pulled.none, try r.worker.pullOnce());
}

/// The dying-worker row's lease TTL (ms): a minute, so no wall-clock
/// stall of a loaded gate reaches it; the row's expiries are explicit
/// sweeps past it.
const ROW_TTL_MS: u32 = 60_000;

test "a fleet READ shard whose worker dies mid-walk: the lease expires at the sweeper (fencing +1), the shard RE-QUEUES for the next capable pull, the zombie's late proposal is refused stale and nothing merges, the fresh pull runs it under fencing 1 and the collector gets exactly one payload; a second loss spends the attempts and the shard is LOST to the local run; a worker whose heartbeat reads stale cancels its walk and abandons" {
    const gpa = testing.allocator;
    const io = testing.io;
    // The lease TTL is a minute: every expiry below is the sweeper's clock
    // advanced EXPLICITLY (`sweep(now + TTL + 1 s)`), never the wall clock
    // — a 1 s TTL let a scheduling stall under a loaded gate expire the
    // fresh pull's OWN lease clock before its proposal (`.abandoned`).
    const r = try Rig.init(gpa, io, ROW_TTL_MS);
    defer r.deinit(gpa);
    defer r.coord.deinit();
    var engine: RamReadRunner = .{ .io = io, .hold = true };
    r.worker.read_runner = engine.runner();
    r.worker.caps = frames.CAP_FLEET_READ;
    _ = try r.worker.enroll();
    const fd = r.coord.fleet();
    const d: seams.FleetDispatch.Descriptor = .{ .id = .{ .job_id = 0x901, .shard = 1 }, .fleet_k = 1, .fleet_n = 2, .inode_plane = 0, .throttle_pct = 100 };
    try testing.expect(fd.queueReadShard(d, 0));
    const Pull = struct {
        fn run(w: *Worker, out: *Worker.Pulled) void {
            out.* = w.pullOnce() catch .refused;
        }
    };
    var out1: Worker.Pulled = .none;
    const th = try std.Thread.spawn(.{}, Pull.run, .{ &r.worker, &out1 });
    while (!engine.holding.load(.acquire)) std.atomic.spinLoopHint();
    try testing.expectEqual(@as(?u64, 0), r.table.fencingOf(d.id));
    // The worker "dies": the sweeper past the TTL expires the lease; the
    // shard re-queues (attempt 1 of 2, a capable session stands).
    const exp0 = stats.job_remote_lease_expiries.load();
    try testing.expectEqual(@as(usize, 1), r.coord.sweep(clock.monotonicNs() + @as(u64, ROW_TTL_MS + 1000) * std.time.ns_per_ms));
    try testing.expectEqual(exp0 + 1, stats.job_remote_lease_expiries.load());
    try testing.expectEqual(@as(?u64, 1), r.table.fencingOf(d.id));
    // The zombie wakes and proposes under fencing 0: refused stale.
    const stale0 = stats.job_remote_refused_stale.load();
    engine.releaseHold(io);
    th.join();
    try testing.expectEqual(Worker.Pulled.refused, out1);
    try testing.expectEqual(stale0 + 1, stats.job_remote_refused_stale.load());
    // The fresh pull runs it under fencing 1 and the collector gets it.
    engine.hold = false;
    try testing.expectEqual(Worker.Pulled.done, try r.worker.pullOnce());
    const got = fd.collectReadShard(io, gpa, d.id);
    try testing.expect(got == .payload);
    fd.release(gpa, got.payload);
    try testing.expectEqual(@as(u64, 2), engine.runs);
    fd.retireFleetShards(0x901);
    // Two losses spend the attempts: the shard is lost to the local run.
    const d2: seams.FleetDispatch.Descriptor = .{ .id = .{ .job_id = 0x902, .shard = 1 }, .fleet_k = 1, .fleet_n = 2, .inode_plane = 0, .throttle_pct = 100 };
    try testing.expect(fd.queueReadShard(d2, 0));
    engine.hold = true;
    var k: u32 = 0;
    while (k < READ_SHARD_ATTEMPTS_MAX) : (k += 1) {
        var out: Worker.Pulled = .none;
        const t = try std.Thread.spawn(.{}, Pull.run, .{ &r.worker, &out });
        while (!engine.holding.load(.acquire)) std.atomic.spinLoopHint();
        _ = r.coord.sweep(clock.monotonicNs() + @as(u64, ROW_TTL_MS + 1000) * std.time.ns_per_ms);
        engine.releaseHold(io);
        t.join();
        try testing.expectEqual(Worker.Pulled.refused, out);
    }
    const relocal0 = stats.job_fleet_shards_relocal.load();
    try testing.expectEqual(seams.ReadShardOutcome.lost, fd.collectReadShard(io, gpa, d2.id));
    try testing.expectEqual(relocal0 + 1, stats.job_fleet_shards_relocal.load());
    fd.retireFleetShards(0x902);
    // A heartbeat that reads stale (the lease expired under a long walk)
    // cancels the engine: the worker abandons, nothing is proposed.
    const d3: seams.FleetDispatch.Descriptor = .{ .id = .{ .job_id = 0x903, .shard = 1 }, .fleet_k = 1, .fleet_n = 2, .inode_plane = 0, .throttle_pct = 100 };
    try testing.expect(fd.queueReadShard(d3, 0));
    r.worker.heartbeat_ms = 50;
    const cancelled0 = engine.cancelled;
    var out3: Worker.Pulled = .none;
    const t3 = try std.Thread.spawn(.{}, Pull.run, .{ &r.worker, &out3 });
    while (!engine.holding.load(.acquire)) std.atomic.spinLoopHint();
    _ = r.coord.sweep(clock.monotonicNs() + @as(u64, ROW_TTL_MS + 1000) * std.time.ns_per_ms);
    // The next heartbeat (50 ms) reads stale and raises the cancel; the
    // engine sees it at its next step and answers null — never released
    // by the test, the walk ends on the lease alone.
    t3.join();
    try testing.expectEqual(Worker.Pulled.abandoned, out3);
    try testing.expectEqual(cancelled0 + 1, engine.cancelled);
    try testing.expect(r.worker.heartbeats >= 1);
    fd.retireFleetShards(0x903);
}

/// A worker's pull on its own thread (the shard "mid-walk" while the test
/// drives the window).
const PullOnce = struct {
    fn run(w: *Worker, out: *Worker.Pulled) void {
        out.* = w.pullOnce() catch .refused;
    }
};

/// The heartbeat station that holds the fiber's FIRST park inside its
/// window — after the fiber decided to park, before the park — until the
/// read shard's stop is raised, recording how many waiters `wake` had at
/// the station (the fiber's own registration, or none).
const StopInWindow = struct {
    io: Io,
    /// Set at the first park's station (release; the test's acquire pairs).
    reached: Atomic(bool) = .init(false),
    /// `wake.waiters` at the station, before the hold registers its own
    /// (read by the test after the pull's join).
    waiters_at_station: u32 = 0,

    fn station(self: *StopInWindow) Worker.HeartbeatStation {
        return .{ .ctx = self, .parking = &parking };
    }

    fn parking(ctx: *anyopaque, wake: *sync.Notify, stop: *const Atomic(bool)) void {
        const self: *StopInWindow = @ptrCast(@alignCast(ctx));
        if (self.reached.load(.acquire)) return;
        self.waiters_at_station = wake.waiters(self.io);
        self.reached.store(true, .release);
        while (true) {
            var n = wake.notified();
            _ = n.enable(self.io);
            if (stop.load(.acquire)) {
                n.cancel(self.io);
                return;
            }
            n.waitUncancelable(self.io);
        }
    }
};

test "a read shard's stop raised between the heartbeat fiber's decision to park and its park ends the fiber on the wake: the fiber is registered on its wake BEFORE it re-reads the stop (`sync/notify.zig`'s law), so the walk's end proposes at once — never a lost wake that parks the fiber its whole period while the worker's lease clock runs out (the gate's `expected .done, found .abandoned`)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io, seams.LEASE_TTL_MS);
    defer r.deinit(gpa);
    defer r.coord.deinit();
    var engine: RamReadRunner = .{ .io = io, .hold = true };
    r.worker.read_runner = engine.runner();
    r.worker.caps = frames.CAP_FLEET_READ;
    _ = try r.worker.enroll();
    var window: StopInWindow = .{ .io = io };
    r.worker.heartbeat_station = window.station();
    const fd = r.coord.fleet();
    const d: seams.FleetDispatch.Descriptor = .{ .id = .{ .job_id = 0x904, .shard = 1 }, .fleet_k = 1, .fleet_n = 2, .inode_plane = 0, .throttle_pct = 100 };
    try testing.expect(fd.queueReadShard(d, 0));
    var out: Worker.Pulled = .none;
    const th = try std.Thread.spawn(.{}, PullOnce.run, .{ &r.worker, &out });
    // The walk is held and the fiber sits in its first park's window.
    while (!engine.holding.load(.acquire) or !window.reached.load(.acquire)) std.atomic.spinLoopHint();
    // The walk ends: the worker raises the stop inside the window.
    engine.releaseHold(io);
    th.join();
    try testing.expectEqual(@as(u32, 1), window.waiters_at_station);
    try testing.expectEqual(Worker.Pulled.done, out);
    const got = fd.collectReadShard(io, gpa, d.id);
    try testing.expect(got == .payload);
    fd.release(gpa, got.payload);
    fd.retireFleetShards(0x904);
}

/// The heartbeat station that only records the fiber's first park (it has
/// decided to park and registered on its wake) and lets the park begin.
const ParkSeen = struct {
    /// Set at the first park's station (release; the test's acquire pairs).
    reached: Atomic(bool) = .init(false),

    fn station(self: *ParkSeen) Worker.HeartbeatStation {
        return .{ .ctx = self, .parking = &parking };
    }

    fn parking(ctx: *anyopaque, _: *sync.Notify, _: *const Atomic(bool)) void {
        const self: *ParkSeen = @ptrCast(@alignCast(ctx));
        self.reached.store(true, .release);
    }
};

/// A stop's bound in the row below: a tenth of the heartbeat period a stop
/// used to wait out (the defect's cost), a hundred engine steps
/// (`RamReadRunner.STEP_NS`) of headroom for a loaded venue's scheduling.
const STOP_BOUND_NS: u64 = @as(u64, seams.HEARTBEAT_MS) * std.time.ns_per_ms / 10;

test "a requestStop during a read shard's scan WAKES the heartbeat fiber: the fiber raises the walk's cancel at once, the engine ends at its next step and the pull abandons (stopped) within a step and the venue's scheduling — never the heartbeat's whole period (10 s)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io, ROW_TTL_MS);
    defer r.deinit(gpa);
    defer r.coord.deinit();
    var engine: RamReadRunner = .{ .io = io, .hold = true };
    r.worker.read_runner = engine.runner();
    r.worker.caps = frames.CAP_FLEET_READ;
    _ = try r.worker.enroll();
    // The product's period: the stop must not depend on it.
    try testing.expectEqual(seams.HEARTBEAT_MS, r.worker.heartbeat_ms);
    var seen: ParkSeen = .{};
    r.worker.heartbeat_station = seen.station();
    const fd = r.coord.fleet();
    const d: seams.FleetDispatch.Descriptor = .{ .id = .{ .job_id = 0x905, .shard = 1 }, .fleet_k = 1, .fleet_n = 2, .inode_plane = 0, .throttle_pct = 100 };
    try testing.expect(fd.queueReadShard(d, 0));
    var out: Worker.Pulled = .none;
    const th = try std.Thread.spawn(.{}, PullOnce.run, .{ &r.worker, &out });
    // Mid-scan, the fiber in its first park.
    while (!engine.holding.load(.acquire) or !seen.reached.load(.acquire)) std.atomic.spinLoopHint();
    const cancelled0 = engine.cancelled;
    const t0 = clock.monotonicNs();
    r.worker.requestStop();
    th.join();
    const elapsed = clock.monotonicNs() - t0;
    try testing.expectEqual(Worker.Pulled.abandoned, out);
    // The walk ended on the cancel (the test never releases it), with no
    // renewal in between.
    try testing.expectEqual(cancelled0 + 1, engine.cancelled);
    try testing.expectEqual(@as(u64, 0), r.worker.heartbeats);
    if (elapsed >= STOP_BOUND_NS) {
        std.debug.print("the stop took {d} ms (bound {d} ms; the heartbeat period is {d} ms)\n", .{ elapsed / std.time.ns_per_ms, STOP_BOUND_NS / std.time.ns_per_ms, seams.HEARTBEAT_MS });
        return error.StopWaitedOutTheHeartbeat;
    }
    fd.retireFleetShards(0x905);
}

test "a non-coordinator answers the job block not_manager; the worker's enroll reports it as refused" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, io, 30_000);
    defer r.deinit(gpa);
    r.fabric.setCoordinator(false);
    try testing.expectError(error.EnrollRefused, r.worker.enroll());
    r.coord.unregister(&r.coord_svc);
    try testing.expectError(error.NotManager, r.cc.conn.call(.{ .heartbeat = .{ .job_id = 1, .shard = 1, .reserved = 0, .shard_fencing = 0 } }));
}
