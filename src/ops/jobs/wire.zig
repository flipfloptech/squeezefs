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

/// The channel's trust class decides the verify strength (`auth.zig`:
/// the session key is bound to the channel's exporter; mTLS lands without
/// a wire change).
pub const Channel = enum { plaintext, mutual_tls };

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
    return checksum.xxh3(&b) % 1000 < permille;
}

/// The coordinator's device read-back of a proposed destination: the
/// stored image's xxh3 over `len` bytes (the plane binds the device seam;
/// tests a RAM world).
pub const VerifyRead = struct {
    ctx: *anyopaque,
    read_xxh3: *const fn (ctx: *anyopaque, dest: DestTuple, len: u64) error{Io}!u64,
};

/// The most enrolled workers one coordinator tracks.
pub const MAX_WORKERS: usize = 256;

const Enrolled = struct {
    identity: StableIdentity,
    pr_key: u64,
    caps: u32,
    /// The session id the listener gave the connection (the departure key).
    session_id: u64,
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
    /// (expired, abandoned) goes back to Queued for any population.
    fn requeueExpired(self: *Coordinator) void {
        var victims: [MAX_WORKERS]?*JobCtl = @splat(null);
        var n: usize = 0;
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
        }
        for (victims[0..n]) |v| self.fabric.requeue(self.io, v.?);
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

    /// The worker pulls: a wire-executable job claimed for it becomes shard
    /// 1 of that job; none ⇒ the empty descriptor.
    fn assign(self: *Coordinator, who: StableIdentity, p: frames.ShardAssignReq, _: []u8) service.Answer {
        _ = p;
        {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.findWorkerLocked(who) == null) return service.Answer.rejected("shard_assign before job_enroll");
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
            .shard_fencing_hi = @truncate(a.fencing >> 32),
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
            const got = self.verify.read_xxh3(self.verify.ctx, .{ .vol_tag = c.vol_tag, .block_idx = c.block_idx }, c.len) catch {
                stats.job_remote_verify_failures.inc();
                _ = self.table.lease().expire(id, clock.monotonicNs());
                self.requeueExpired();
                return refusedAck(.verify_failed);
            };
            stats.job_remote_verify_read_bytes.add(c.len);
            if (got != c.xxh3) {
                stats.job_remote_verify_failures.inc();
                log.err("job fabric: shard ({x}, {d}) proposed vol {x} block {d} with xxh3 {x} but the device reads {x} — refused; the lease is treated as expired (fencing bump + quarantine), nothing published", .{ id.job_id, id.shard, c.vol_tag, c.block_idx, c.xxh3, got });
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

    fn readResult(self: *Coordinator, p: frames.ReadShardResultReq) service.Answer {
        const id: ShardId = .{ .job_id = p.head.ref.job_id, .shard = p.head.ref.shard };
        if (self.table.lease().complete(id, p.head.ref.shard_fencing) != .live) return service.Answer.rejected("stale or unknown read shard");
        stats.job_fleet_shards_completed.inc();
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

/// `job worker`: enroll, then pull shards until the coordinator goes away
/// or `stop` latches. A local lease clock starts a full TTL at assignment
/// and every `HeartbeatAck` extends it; past half-TTL ONE heartbeat
/// round-trip precedes the next batch; expired ⇒ abort the shard (no
/// submission, a `shard_abandon`).
pub const Worker = struct {
    io: Io,
    caller: Caller,
    caps: u32 = 0,
    pr_key: u64 = 0,
    heartbeat_ms: u32 = seams.HEARTBEAT_MS,
    lease_ttl_ms: u32 = seams.LEASE_TTL_MS,
    stop: Atomic(bool) = .init(false),
    wake: sync.Notify = .init,
    /// `SQUEEZEFS_TEST_JOB_HOLD_SUBMISSION`: park before `result_submit`
    /// until `release_hold` fires (the zombie window).
    hold_submission: bool = false,
    release_hold: sync.Notify = .init,
    held: Atomic(bool) = .init(false),
    /// Instruments the loopback tests read.
    shards_done: u64 = 0,
    shards_refused: u64 = 0,
    heartbeats: u64 = 0,

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

    /// ONE pull: a descriptor or none; execute; propose.
    pub fn pullOnce(self: *Worker) WorkerError!Pulled {
        const rep = try self.caller.call(self.caller.ctx, .{ .shard_assign = .{ .caps = self.caps, .reserved = 0 } });
        const d = rep.shard_assign;
        if (d.isNone()) return .none;
        const ref: frames.ShardRef = .{ .job_id = d.head.job_id, .shard = d.head.shard, .reserved = 0, .shard_fencing = d.head.fencing() };
        const job_type: records.JobType = @fromBackingInt(@intCast(d.head.job_type));
        if (job_type != .noop) {
            // Only noop ships whole over the wire; a read shard rides the
            // fsck engine's runner (the fleet dispatcher's), not this loop.
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
            if (task_ns > 0) {
                var n = self.wake.notified();
                _ = n.waitUntilUncancelable(self.io, clock.monotonicNs() + task_ns);
            }
            if (self.stop.load(.acquire)) {
                _ = try self.caller.call(self.caller.ctx, .{ .shard_abandon = .{ .ref = ref, .reason = @backingInt(frames.AbandonReason.stopped), .reserved = @splat(0) } });
                return .abandoned;
            }
        }
        if (self.hold_submission) {
            self.held.store(true, .release);
            var n = self.release_hold.notified();
            n.waitUncancelable(self.io);
            self.held.store(false, .release);
        }
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
            if (r == .none) {
                var n = self.wake.notified();
                _ = n.waitUntilUncancelable(self.io, clock.monotonicNs() + @as(u64, self.heartbeat_ms) * std.time.ns_per_ms / 4);
            }
        }
    }

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
        return .{ .ctx = self, .read_xxh3 = &read };
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
        r.worker = .{ .io = io, .caller = r.cc.caller() };
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
    const r = try Rig.init(gpa, io, 1000);
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
    try testing.expectEqual(@as(usize, 1), r.coord.sweep(clock.monotonicNs() + 2 * std.time.ns_per_s));
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
    const outside = [_]frames.BlockChecksum{.{ .vol_tag = 5, .block_idx = 9, .len = 4096, .xxh3 = 1 }};
    const w = try r.cc.conn.call(.{ .result_submit = .{ .head = .{ .ref = ref, .n = 1, .reserved = 0 }, .checksums = &outside } });
    try testing.expectEqual(frames.RefusalReason.window, w.result_submit.verdict());
    try testing.expectEqual(seams.LeaseVerdict.live, r.table.lease().check(sid, a.fencing));
    // The right tuple, the wrong checksum.
    try r.verify.sums.put(gpa, (@as(u128, 5) << 64) | 1, 0xAAAA);
    const bad = [_]frames.BlockChecksum{.{ .vol_tag = 5, .block_idx = 1, .len = 4096, .xxh3 = 0xBBBB }};
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
    const good = [_]frames.BlockChecksum{.{ .vol_tag = 5, .block_idx = 10, .len = 4096, .xxh3 = 0xCCCC }};
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
