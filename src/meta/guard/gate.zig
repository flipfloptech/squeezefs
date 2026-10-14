//! The D0 gate bound to the metadata backend (Linux; charter §5.7; spec
//! `guard_claims.md` §2.2–§2.8): `Guard.gate` runs inside `backend.open`
//! between the conveyor's pass and the checkpoint thread's spawn — the
//! replayed claim classified, layer B1 arbitrated, OUR claim committed
//! through the conveyor and BARRIERED, before the checkpoint task exists
//! and before FUSE arms (the ordering law). The heartbeat task is the
//! BACKEND's (quirk Q6: the owner of the claim owns its refresh — one
//! `sqz-meta` fiber per writable volume, stopped inside `close` before
//! the claim is deleted). `claim clear` walks the same door in
//! `.claim_clear` mode (quirk Q14: the one writer open that spawns the
//! checkpoint task only to close it). The `clients` probe reads the claim
//! off the device WITHOUT the flock (a probe is never blocked by a live
//! mount).
//!
//! The recovery ladder, in that order of automation: same-host dead-pid
//! proof (instant, any age) → PR preempt (automatic on a PR substrate for
//! a TTL-stale/absent claim) → TTL (45 s makes a foreign claim stale) →
//! `squeezefs claim clear` (operator attestation). No bypass knob.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const derive = @import("../../core/derive.zig");
const errors = @import("../../core/errors.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const exec = @import("../../exec/exec.zig");
const tick = @import("../../sync/tick.zig");
const mem_budget = @import("../../mem/budget.zig");
const uri = @import("../../cli/uri.zig");
const key = @import("../key.zig");
const record = @import("../record.zig");
const tree = @import("../tree.zig");
const node = @import("../node.zig");
const node_cache = @import("../node_cache.zig");
const node_seq = @import("../node_seq.zig");
const ledger = @import("../ledger.zig");
const superblock = @import("../superblock.zig");
const journal = @import("../journal.zig");
const device_loader = @import("../device_loader.zig");
const inos = @import("../inos.zig");
const backend = @import("../backend.zig");
const guard = @import("../guard.zig");
const claim = @import("claim.zig");
const pr = @import("pr.zig");
const pr_linux = @import("pr_linux.zig");
const proc = @import("proc.zig");
const nvme = @import("nvme_cmd.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/gate is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const KvBackend = backend.KvBackend;

pub const Mode = enum {
    /// A mount: claim, heartbeat, PR.
    writer,
    /// `squeezefs claim clear`: the same door, deleting a clearable claim.
    claim_clear,
};

/// What `backend.OpenOptions.guard` carries.
pub const Options = struct {
    mode: Mode = .writer,
    /// `-o hostnqn=` / `-o hostid=` (option > env).
    identity: pr.IdentityOptions = .{},
    /// Test seams (one load each; `SQUEEZEFS_TEST_*` class, held
    /// in-process so the pins need no knob rows): another host's identity,
    /// a fake namespace as the PR substrate on a file, a pid table.
    test_identity: if (builtin.is_test) ?claim.Identity else void = if (builtin.is_test) null else {},
    test_namespace: if (builtin.is_test) ?nvme.Namespace else void = if (builtin.is_test) null else {},
    test_pid_probe: if (builtin.is_test) ?claim.PidProbe else void = if (builtin.is_test) null else {},
    /// Skip layer A (the flock): the "another host" simulation — two
    /// writers of one volume in one process, so the claim alone refuses.
    test_skip_flock: if (builtin.is_test) bool else void = if (builtin.is_test) false else {},
};

/// The gate's own conveyor traffic at a writer open: ONE claim tx and
/// ONE barrier (the backend's engagement identities add them).
pub const OPEN_COMMITS: u64 = 1;
pub const OPEN_BARRIERS: u64 = 1;

pub const ClearOutcome = union(enum) {
    /// No writer claim present — nothing to clear.
    no_claim,
    /// The claim removed (null = it was unparseable).
    cleared: ?claim.Claim,
    refused_fresh: claim.Claim,
    refused_own: claim.Claim,

    pub fn refused(self: ClearOutcome) bool {
        return self == .refused_fresh or self == .refused_own;
    }
};

/// The volume's guard state for its life.
pub const Guard = struct {
    gpa: Allocator,
    io: Io,
    be: *KvBackend,
    mode: Mode,
    identity: claim.Identity,
    probe: claim.PidProbe,
    /// The claim's xattr key (the chain scanned once at the gate).
    xkey: [key.XATTR_KEY_LEN]u8,
    claimed: std.atomic.Value(bool) = .init(false),
    heartbeat_seq: u64 = 0,
    posture: pr.Posture,
    /// The reservation is ours (register ladder + acquire done): release
    /// and unregister at teardown; a refusal before that point holds no
    /// registration to release.
    pr_active: bool = false,
    /// The char node behind an active posture (owned).
    device: ?*pr_linux.Device = null,
    /// The configured host identity recorded at the gate (§1.4).
    identity_opts: pr.IdentityOptions,
    identity_stamp: pr.IdentityStamp,
    hb_stop: std.atomic.Value(u32) = .init(0),
    hb_fut: ?Io.Future(void) = null,
    clear_outcome: ?ClearOutcome = null,
    /// `prepare` → `commit` carry: the replayed claim's raw bytes, the
    /// evidence and the gate's clock.
    found_raw: ?[]u8 = null,
    evidence: claim.Evidence = .{ .reclaimable = .absent },
    gate_now: i64 = 0,

    /// `writer_guard_mode` (§2.8).
    pub fn modeName(self: *const Guard) []const u8 {
        return switch (self.mode) {
            .claim_clear => "claim-clear",
            .writer => self.posture.mode(),
        };
    }

    pub fn prClient(self: *const Guard) ?*pr.Client {
        return switch (self.posture) {
            .active => |c| c,
            .none => null,
        };
    }

    pub fn ourClaim(self: *const Guard) claim.Claim {
        var c = self.identity.claim(self.be.index, clock.realtimeCoarseNs());
        c.heartbeat_seq = self.heartbeat_seq;
        return c;
    }

    /// Per-volume stats face: the mode, the claim words, the PR gauges.
    pub fn writeStatsJson(self: *const Guard, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"writer_guard_mode\":\"{s}\",\"writer_guard_claimed\":{},\"writer_guard_writer_id\":\"{x:0>16}\",\"writer_guard_heartbeat_seq\":{d},\"writer_guard_term\":0,\"writer_guard_verdict\":\"{s}\"", .{ self.modeName(), self.claimed.load(.acquire), self.identity.writerId(self.be.index), self.heartbeat_seq, @tagName(self.be.lane.latch.verdict()) });
        if (self.prClient()) |c| {
            try w.writeAll(",\"pr\":");
            try c.writeStatsJson(w);
        }
        try w.writeByte('}');
    }

    // ---- the gate ----

    /// Steps 4–5 of the ordering law (§2.2), in TWO phases so the B1
    /// arbitration precedes EVERY device write of the open: `prepare`
    /// reads the claim, classifies it, resolves the PR posture and
    /// arbitrates (register ladder, acquire, preempt a stale holder) —
    /// no write; the backend then runs its ring-recovery cover (the
    /// reference's `preclaim_ring_recovery`) and `commit` lands the claim
    /// tx and its barrier. WHY the split: a successor under a SECOND host
    /// identity (the paused-then-preempted row of the fidelity tier) is
    /// fenced at the device by the incumbent's Write Exclusive hold until
    /// the preempt lands — a cover written before it answered `-EBADE`
    /// and failed the open (measured on nvmet-loop); a same-identity
    /// successor (the kill -9 remount) never saw it because nvmet judges
    /// access by host id. Runs with the pass fiber live and the checkpoint
    /// thread NOT yet spawned. A refusal releases what the gate took (PR,
    /// the char node) before returning — the flock is the backend's
    /// `errdefer`, closed last.
    pub fn prepare(gpa: Allocator, io: Io, be: *KvBackend, opts: Options) backend.OpenError!*Guard {
        const identity: claim.Identity = if (builtin.is_test) (opts.test_identity orelse proc.localIdentity()) else proc.localIdentity();
        const probe: claim.PidProbe = if (builtin.is_test) (opts.test_pid_probe orelse proc.probe) else proc.probe;
        const g = try gpa.create(Guard);
        errdefer gpa.destroy(g);
        g.* = .{ .gpa = gpa, .io = io, .be = be, .mode = opts.mode, .identity = identity, .probe = probe, .xkey = undefined, .posture = .{ .none = .not_nvme_namespace }, .identity_opts = opts.identity, .identity_stamp = .{ .nqn_hash = 0, .id_hash = 0 } };
        if (!identity.bootIdReadable()) log.warn("meta volume {s}: the boot id is unreadable — the same-host dead-pid proof never applies on this host (every foreign claim is judged by the TTL alone)", .{be.path});

        // The configured host identity (pair-or-neither).
        switch (pr.resolveExplicitIdentity(be.env, opts.identity)) {
            .refusal => |r| {
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                r.write(&w) catch {};
                log.err("meta volume {s}: {s}", .{ be.path, w.buffered() });
                return error.InvalidIdentity;
            },
            .pair => |p| g.identity_stamp = pr.IdentityStamp.of(p.hostnqn, p.hostid),
            .none => {
                const etc = proc.EtcNvme.read();
                g.identity_stamp = pr.IdentityStamp.of(etc.hostnqn(), etc.hostid());
            },
        }

        // The replayed claim (the staleness evidence) and its key.
        const found = try g.readClaim(gpa);
        errdefer if (found.raw) |r| gpa.free(r);
        g.xkey = found.xkey;
        g.found_raw = found.raw;
        g.gate_now = clock.realtimeCoarseNs();
        g.evidence = claim.classify(found.raw, identity, g.gate_now, probe);

        // `claim clear` decides at commit (its delete needs the cover too).
        if (opts.mode == .claim_clear) return g;

        // B1 posture: a fake substrate in tests, else the char twin.
        try g.resolvePr(gpa, opts);
        errdefer g.releasePr();

        // The decision matrix (§2.4).
        const now = g.gate_now;
        switch (g.evidence) {
            .fresh_foreign => |f| {
                if (f.future) log.warn("meta volume {s}: the writer claim is stamped {d} s in the FUTURE — check this host's clock (and the holder's)", .{ be.path, @divFloor(f.claim.ts_ns - now, std.time.ns_per_s) });
                return g.refuseClaim(.{ .fresh = f.claim }, now);
            },
            .stale_foreign => |maybe| {
                if (g.posture == .none) return g.refuseClaim(.{ .stale_non_pr = maybe }, now);
                if (maybe) |c| if (!c.preemptable(now)) return g.refuseClaim(.{ .fresh = c }, now);
            },
            .reclaimable => |why| switch (why) {
                .absent => {},
                .own_residue => log.info("meta volume {s}: reclaiming this process's own residual writer claim (a drop-without-shutdown teardown)", .{be.path}),
                .dead_pid => {
                    guard.stats.reclaims_dead_pid.inc();
                    const c = claim.Claim.decode(found.raw.?) catch unreachable;
                    log.info("meta volume {s}: reclaiming the writer claim of a provably dead same-host holder (pid {d}, {d} s old; its /proc identity is gone or belongs to another incarnation)", .{ be.path, c.pid, c.ageS(now) });
                },
            },
        }

        // B1 arbitration: register ladder, acquire, preempt a stale holder.
        if (g.prClient()) |c| {
            try g.arbitrate(c, g.evidence);
            g.pr_active = true;
        }
        return g;
    }

    /// The second phase: the claim tx through the conveyor and its
    /// barrier (`claim clear`: the delete). The guard is the backend's
    /// after this returns; a failure here is the caller's unwind.
    pub fn commit(g: *Guard) backend.OpenError!void {
        defer if (g.found_raw) |r| {
            g.gpa.free(r);
            g.found_raw = null;
        };
        if (g.mode == .claim_clear) {
            g.clear_outcome = try g.clear(g.found_raw, g.evidence, g.gate_now);
            return;
        }
        try g.commitClaim();
        try g.barrier();
        guard.stats.claims_committed.inc();
        guard.stats.clients_registered.add(1);
        log.info("meta volume {s}: writer claim committed and barriered (id {x:0>16}, pid {d}, mode {s}, pr key {x:0>16})", .{ g.be.path, g.identity.writerId(g.be.index), g.identity.pid, g.modeName(), claim.prKey(g.identity.writerId(g.be.index), g.identity.boot_id) });
    }

    /// Both phases back to back (the tests' door; the backend interleaves
    /// its cover between them).
    pub fn gate(gpa: Allocator, io: Io, be: *KvBackend, opts: Options) backend.OpenError!*Guard {
        const g = try prepare(gpa, io, be, opts);
        errdefer g.deinit();
        try g.commit();
        return g;
    }

    const Refusal = union(enum) { fresh: claim.Claim, stale_non_pr: ?claim.Claim };

    fn refuseClaim(g: *Guard, r: Refusal, now: i64) backend.OpenError {
        guard.stats.claim_refusals.inc();
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        switch (r) {
            .fresh => |c| claim.writeFreshRefusal(&w, g.be.path, c, now) catch {},
            .stale_non_pr => |c| claim.writeStaleNonPrRefusal(&w, g.be.path, c, now) catch {},
        }
        log.err("{s}", .{w.buffered()});
        return error.Busy;
    }

    /// The PR substrate: the test namespace, else the block node's char
    /// twin; RESCAP decides the posture. Never fails the mount.
    fn resolvePr(g: *Guard, gpa: Allocator, opts: Options) backend.OpenError!void {
        const pr_key = claim.prKey(g.identity.writerId(g.be.index), g.identity.boot_id);
        if (builtin.is_test) if (opts.test_namespace) |ns| {
            const c = try gpa.create(pr.Client);
            errdefer gpa.destroy(c);
            c.* = try pr.Client.init(gpa, ns, 1, pr_key);
            g.posture = pr.resolvePosture(gpa, c, g.be.path);
            return;
        };
        switch (pr_linux.resolve(gpa, g.be.path)) {
            .none => |why| {
                if (why == .no_char_node) guard.stats.pr_unavailable.inc();
                g.posture = .{ .none = why };
            },
            .device => |dev| {
                g.device = dev;
                const c = try gpa.create(pr.Client);
                errdefer gpa.destroy(c);
                c.* = try pr.Client.init(gpa, dev.face(), dev.nsid, pr_key);
                g.posture = pr.resolvePosture(gpa, c, g.be.path);
                if (g.posture == .none) {
                    dev.deinit();
                    g.device = null;
                }
            },
        }
    }

    fn prErrorLog(g: *Guard, c: *pr.Client, what: []const u8, e: pr.Error) backend.OpenError {
        var buf: [256]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        c.writeError(&w, e) catch {};
        log.err("{s}: reservation {s} failed on the PR-capable namespace: {s} (single-writer guard)", .{ g.be.path, what, w.buffered() });
        return error.Busy;
    }

    /// §2.4's B1 half, on the blocking pool (a passthru command parks the
    /// issuing thread in its ring).
    fn arbitrate(g: *Guard, c: *pr.Client, evidence: claim.Evidence) backend.OpenError!void {
        const sqz = exec.SqzIo.fromIo(g.io);
        const r = sqz.runBlocking(arbitrateBlocking, .{ g, c, evidence }) catch return error.ThreadSpawnFailed;
        return r;
    }

    fn arbitrateBlocking(g: *Guard, c: *pr.Client, evidence: claim.Evidence) backend.OpenError!void {
        switch (c.registerLadder() catch |e| return g.prErrorLog(c, "register", e)) {
            .registered => {},
            .recovered_own_stale => |n| log.warn("{s}: the register ladder unregistered {d} stale registration(s) of this host (a crashed incarnation's residue on a spec-strict target) before registering key {x:0>16}", .{ g.be.path, n, c.key }),
        }
        c.acquire() catch |e| switch (e) {
            error.ReservationConflict => {
                const victim = (c.holder() catch |re| return g.prErrorLog(c, "report", re)) orelse {
                    log.err("{s}: reservation acquire conflicted but the report names no holder — refusing to arbitrate blind (single-writer guard)", .{g.be.path});
                    return error.Busy;
                };
                if (victim == c.key) return;
                c.preempt(victim) catch |pe| return g.prErrorLog(c, "preempt", pe);
                guard.stats.preempts.inc();
                log.warn("{s}: preempted stale reservation holder key {x:0>16} (device-fenced takeover; writer_claim evidence was {s})", .{ g.be.path, victim, @tagName(evidence.standing()) });
            },
            else => return g.prErrorLog(c, "acquire", e),
        };
        // Rung 6's shared-identity probe: another rkey under our wire id.
        if (c.identityShared() catch false) log.warn("{s}: another registration shares this mount's wire host id — fencing between the two is PROCESS-LOCAL (co-located mounts on one registrant)", .{g.be.path});
    }

    // ---- the claim record through the conveyor ----

    const Found = struct { xkey: [key.XATTR_KEY_LEN]u8, raw: ?[]u8 };

    /// The `(ino 1, hash56("writer_claim"))` chain: the matching record and
    /// its key, else the first free coll's key.
    fn readClaim(g: *Guard, gpa: Allocator) backend.OpenError!Found {
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        const h = key.xattrNameHash56(claim.CLAIM_XATTR, g.be.nameKey());
        const start = key.xattrKey(inos.ROOT_INO, h, 0);
        const end = key.xattrKey(inos.ROOT_INO, h, std.math.maxInt(u8));
        g.be.range(gpa, key.TREE_XATTRS, &start, &end, 256, &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        var occ: [256]u8 = undefined;
        var n: usize = 0;
        for (kvs.items) |kv| {
            occ[n] = kv.key[kv.key.len - 1];
            n += 1;
            const xv = record.XattrValue.decode(kv.value) catch return error.Corrupt;
            if (std.mem.eql(u8, xv.name, claim.CLAIM_XATTR)) {
                var k: [key.XATTR_KEY_LEN]u8 = undefined;
                @memcpy(&k, kv.key[0..key.XATTR_KEY_LEN]);
                return .{ .xkey = k, .raw = try gpa.dupe(u8, xv.value) };
            }
        }
        const coll = key.firstFreeCollSeq(occ[0..n]) orelse return error.Corrupt;
        return .{ .xkey = key.xattrKey(inos.ROOT_INO, h, coll), .raw = null };
    }

    fn stageClaimPut(g: *Guard, t: *backend.KvTx, c: claim.Claim) Allocator.Error!void {
        var buf: [1 + claim.CLAIM_XATTR.len + claim.CLAIM_LEN]u8 = undefined;
        const enc = (record.XattrValue{ .name = claim.CLAIM_XATTR, .value = &c.encode() }).encodeInto(&buf) catch unreachable;
        try t.stagePut(key.TREE_XATTRS, &g.xkey, enc);
    }

    fn mapCommit(e: backend.Error) backend.OpenError {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.VolumeFailed => error.Io,
            else => error.Io,
        };
    }

    fn commitClaim(g: *Guard) backend.OpenError!void {
        const t = try g.be.begin(@src());
        {
            errdefer g.be.abandon(t);
            try g.stageClaimPut(t, g.ourClaim());
        }
        g.be.commit(t) catch |e| {
            log.err("meta volume {s}: the writer claim could not be committed ({s}) — refusing the mount", .{ g.be.path, @errorName(e) });
            return mapCommit(e);
        };
        // Release pairs with `close`'s acquire: the deletion runs iff the
        // claim landed.
        g.claimed.store(true, .release);
    }

    fn barrier(g: *Guard) backend.OpenError!void {
        g.be.fsync() catch |e| {
            log.err("meta volume {s}: the writer claim's barrier failed ({s}) — refusing the mount", .{ g.be.path, @errorName(e) });
            return mapCommit(e);
        };
    }

    // ---- claim clear (§2.7 step 4) ----

    fn clear(g: *Guard, raw: ?[]const u8, evidence: claim.Evidence, now: i64) backend.OpenError!ClearOutcome {
        const holder: ?claim.Claim = switch (evidence) {
            .reclaimable => |why| switch (why) {
                .absent => return .no_claim,
                .own_residue => {
                    const c = claim.Claim.decode(raw.?) catch unreachable;
                    var buf: [512]u8 = undefined;
                    var w: Io.Writer = .fixed(&buf);
                    claim.writeClearOwnRefusal(&w, g.be.path, c, now) catch {};
                    log.err("{s}", .{w.buffered()});
                    guard.stats.clear_refusals.inc();
                    return .{ .refused_own = c };
                },
                .dead_pid => claim.Claim.decode(raw.?) catch unreachable,
            },
            .fresh_foreign => |f| {
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                claim.writeClearFreshRefusal(&w, g.be.path, f.claim, now) catch {};
                log.err("{s}", .{w.buffered()});
                guard.stats.clear_refusals.inc();
                return .{ .refused_fresh = f.claim };
            },
            .stale_foreign => |c| c,
        };
        const t = try g.be.begin(@src());
        {
            errdefer g.be.abandon(t);
            try t.stageDelete(key.TREE_XATTRS, &g.xkey);
        }
        g.be.commit(t) catch |e| return mapCommit(e);
        try g.barrier();
        guard.stats.clear_cleared.inc();
        return .{ .cleared = holder };
    }

    // ---- the heartbeat (§1.5 + §2.5) ----

    /// Spawn the beat on the `sqz-meta` pool: every 10 s the PR re-check
    /// then the claim refresh.
    pub fn startHeartbeat(g: *Guard) backend.OpenError!void {
        if (g.mode != .writer) return;
        const sqz = exec.SqzIo.fromIo(g.io);
        g.hb_fut = sqz.spawnMeta(heartbeatMain, .{g}) catch return error.ThreadSpawnFailed;
    }

    fn heartbeatMain(g: *Guard) void {
        const interval_ns: u64 = claim.HEARTBEAT_INTERVAL_S * std.time.ns_per_s;
        while (true) {
            const deadline = clock.monotonicNs() + interval_ns;
            while (g.hb_stop.load(.acquire) == 0) {
                if (tick.parkUntilUncancelable(g.io, &g.hb_stop, deadline, null) == .deadline) break;
            }
            // Acquire pairs with `stopHeartbeat`'s wake: a post-unmount beat
            // must never touch a released guard.
            if (g.hb_stop.load(.acquire) != 0) return;
            g.beat();
        }
    }

    /// One beat. Skipped on a FAILED volume (nothing may be written) and
    /// after the claim was released.
    pub fn beat(g: *Guard) void {
        if (g.be.isFailed() or !g.claimed.load(.acquire)) return;
        if (g.prClient()) |c| {
            if (!g.prRecheck(c)) return;
            if (g.be.isFailed()) return;
        }
        g.heartbeat_seq += 1;
        const t = g.be.begin(@src()) catch {
            guard.stats.heartbeat_failures.inc();
            return;
        };
        g.stageClaimPut(t, g.ourClaim()) catch {
            g.be.abandon(t);
            guard.stats.heartbeat_failures.inc();
            return;
        };
        g.be.commit(t) catch |e| {
            guard.stats.heartbeat_failures.inc();
            log.warn("meta volume {s}: writer claim heartbeat refresh failed ({s}) — the claim ages toward the {d} s TTL until a refresh lands", .{ g.be.path, @errorName(e), claim.STALE_TTL_S });
            return;
        };
        guard.stats.heartbeats.inc();
    }

    /// The PR half of the beat, BEFORE the claim refresh ("never refresh a
    /// claim past a foreign reservation"). False = skip this beat.
    fn prRecheck(g: *Guard, c: *pr.Client) bool {
        // Identity stability: the configured pair must not have changed.
        const stamp: ?pr.IdentityStamp = switch (pr.resolveExplicitIdentity(g.be.env, g.identity_opts)) {
            .pair => |p| pr.IdentityStamp.of(p.hostnqn, p.hostid),
            .none => blk: {
                const etc = proc.EtcNvme.read();
                break :blk pr.IdentityStamp.of(etc.hostnqn(), etc.hostid());
            },
            .refusal => null,
        };
        if (stamp) |s| {
            if (!s.eql(g.identity_stamp)) {
                guard.stats.identity_mismatch.fire("hostnqn/hostid changed under a live mount");
                g.failStop("host identity changed under the reservation (hostnqn/hostid) — our registration is not ours");
                return false;
            }
        } else {
            log.warn("meta volume {s}: the host identity pair is unreadable this beat — identity stability skipped once", .{g.be.path});
        }
        const sqz = exec.SqzIo.fromIo(g.io);
        const outcome = sqz.runBlocking(prRecheckBlocking, .{ g, c }) catch return true;
        switch (outcome) {
            .ours => return true,
            .report_failed => return false,
            .reacquired => {
                guard.stats.pr_reacquires.inc();
                log.warn("meta volume {s}: reservation holdership had lapsed (PTPL-less target power cycle?) — re-acquired key {x:0>16}; audit the fabric", .{ g.be.path, c.key });
                return true;
            },
            .reacquire_failed => {
                g.failStop("re-acquire after reservation lapse failed — another mount may have taken the namespace");
                return false;
            },
            .usurped => |k| {
                _ = g.be.lane.latch.noteFence("the heartbeat's reservation re-check");
                log.err("meta volume {s}: reservation is held by a foreign registrant (key {x:0>16}) — this holder has been usurped", .{ g.be.path, k });
                return false;
            },
        }
    }

    const Recheck = union(enum) { ours, report_failed, reacquired, reacquire_failed, usurped: u64 };

    fn prRecheckBlocking(g: *Guard, c: *pr.Client) Recheck {
        const holder = c.holder() catch |e| {
            var buf: [128]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            c.writeError(&w, e) catch {};
            log.warn("meta volume {s}: reservation report failed on the heartbeat ({s}) — retrying next beat", .{ g.be.path, w.buffered() });
            return .report_failed;
        };
        if (holder) |k| return if (k == c.key) .ours else .{ .usurped = k };
        _ = c.registerLadder() catch return .reacquire_failed;
        c.acquire() catch return .reacquire_failed;
        return .reacquired;
    }

    fn failStop(g: *Guard, why: []const u8) void {
        if (g.be.lane.latch.failStop()) log.err("meta volume {s}: {s} — latching the volume FAILED (single-writer guard fail-stop)", .{ g.be.path, why });
    }

    // ---- teardown (§2.5 "Clean unmount"), in the backend's `close` ----

    /// Stop the beat (joined) — before anything else of the dismount.
    pub fn stopHeartbeat(g: *Guard) void {
        const fut = &(g.hb_fut orelse return);
        // The wake IS the store: `tick.wake` swaps the token in (release) and
        // wakes only a word that held none — a store before it made the swap
        // read a token and skip the futex wake, and the parked beat waited
        // out the 2 s liveness tick on every clean dismount.
        tick.wake(g.io, &g.hb_stop);
        fut.await(g.io);
        g.hb_fut = null;
    }

    /// Delete the claim exactly once, FIRST, while the write gate is open; a
    /// FAILED volume cannot write (the claim ages out by TTL).
    pub fn deleteClaim(g: *Guard) void {
        if (!g.claimed.swap(false, .acq_rel)) return;
        guard.stats.clients_registered.sub(1);
        if (g.be.isFailed()) {
            log.warn("meta volume {s}: the volume is FAILED — the writer claim stays and ages out by the {d} s TTL", .{ g.be.path, claim.STALE_TTL_S });
            return;
        }
        const t = g.be.begin(@src()) catch return;
        t.stageDelete(key.TREE_XATTRS, &g.xkey) catch {
            g.be.abandon(t);
            return;
        };
        g.be.commit(t) catch |e| {
            log.warn("meta volume {s}: the writer claim could not be deleted at unmount ({s}) — it will age out by the {d} s TTL", .{ g.be.path, @errorName(e), claim.STALE_TTL_S });
        };
    }

    /// Release + unregister (once, best-effort) after the final barrier;
    /// the char node closed with it.
    pub fn releasePr(g: *Guard) void {
        switch (g.posture) {
            .active => |c| {
                if (g.pr_active) c.release() catch |e| {
                    var buf: [128]u8 = undefined;
                    var w: Io.Writer = .fixed(&buf);
                    c.writeError(&w, e) catch {};
                    log.warn("meta volume {s}: reservation release/unregister failed ({s}) — a stale registration may remain (the next mount's ladder recovers it)", .{ g.be.path, w.buffered() });
                };
                c.deinit();
                g.gpa.destroy(c);
                g.posture = .{ .none = .not_nvme_namespace };
            },
            .none => {},
        }
        if (g.device) |d| {
            d.deinit();
            g.device = null;
        }
    }

    pub fn deinit(g: *Guard) void {
        g.stopHeartbeat();
        g.releasePr();
        if (g.found_raw) |r| g.gpa.free(r);
        g.gpa.destroy(g);
    }
};

// ---------------------------------------------------------------------------
// The probe (`open_probe`): the claim off the device without the flock.
// ---------------------------------------------------------------------------

pub const ProbeError = error{ DeviceRefused, NotFormatted, Corrupt, Io, OutOfMemory };

/// The raw `writer_claim` value as the RAM state after bootstrap replay
/// would hold it: both superblock copies → the newest ledger record → the
/// ring replayed into private trees → the xattr chain of ino 1. A live
/// writer's concurrent pages are whole checksummed units: a torn one drops
/// as in any replay. Caller frees.
pub fn probeClaim(gpa: Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8) ProbeError!?[]u8 {
    const volume_len = device_loader.volumeLength(path) catch return error.DeviceRefused;
    const mode = uring_fs.registerMetaDevice(gpa, env, path) catch return error.DeviceRefused;
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };
    const sector0 = try dev.readRange(gpa, 0, superblock.sector_len);
    defer gpa.free(sector0);
    const backup: ?[]u8 = if (superblock.backupOffsetFor(volume_len)) |off| try dev.readRange(gpa, off, superblock.sector_len) else null;
    defer if (backup) |b| gpa.free(b);
    var diag: superblock.Refusal = undefined;
    const classified = superblock.classifyVolumeImages(sector0, backup, volume_len, &diag) catch return error.Corrupt;
    const sb = switch (classified.format) {
        .v5 => |s| s,
        else => return error.NotFormatted,
    };
    const ledger_img = try dev.readRange(gpa, sb.root_ledger.start, @intCast(sb.root_ledger.len));
    defer gpa.free(ledger_img);
    var mounted = (try ledger.newestValid(gpa, ledger_img)) orelse return error.Corrupt;
    defer mounted.deinit(gpa);
    const ring_ext = geometry.fixedRingExtent(sb);
    const ring_layout = journal.RingLayout.fromExtent(.{ .start = ring_ext.start, .len = ring_ext.len });
    const geo = journal.productionGeometry(ring_layout.pages, mode.grain());
    const ring_img = try dev.readRange(gpa, ring_layout.base, @intCast(ring_layout.ringBytes()));
    defer gpa.free(ring_img);
    var recovery = try journal.replayScanImage(gpa, ring_img, geo, mounted.journal_tail_seq, journal.FIXED_RING_APPENDER);
    defer recovery.deinit();
    const layout = node.NodeLayout.new(sb.node_size, null) catch return error.Corrupt;
    const cache = try node_cache.NodeCache.init(gpa, io, .{ .layout = layout, .heap_base = sb.heap.start, .budget_bytes = probe_cache_budget_bytes }, dev.loader());
    defer cache.deinit();
    cache.setDurableTail(mounted.journal_tail_seq);
    var seq = node_seq.NodeSeqHandle.shared(@max(mounted.seq, mounted.node_seq_watermark));
    var trees: std.ArrayList(*tree.KvTree) = .empty;
    defer {
        for (trees.items) |t| t.destroy();
        trees.deinit(gpa);
    }
    for (mounted.tree_roots) |r| {
        const t = tree.KvTree.openFromLedger(gpa, cache, &mounted, r.id, &seq) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
            else => return error.Corrupt,
        };
        errdefer t.destroy();
        seq.raiseTo(r.node_seq);
        try trees.append(gpa, t);
    }
    _ = tree.replayEntries(gpa, trees.items, recovery.entries) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        error.Io => return error.Io,
        else => return error.Corrupt,
    };
    // A mount-time path: the key is expanded once per call, never per name.
    const name_key: key.NameKey = .init(sb.hash_seed);
    const h = key.xattrNameHash56(claim.CLAIM_XATTR, &name_key);
    const start = key.xattrKey(inos.ROOT_INO, h, 0);
    const end = key.xattrKey(inos.ROOT_INO, h, std.math.maxInt(u8));
    const t = tree.routeContent(trees.items, key.TREE_XATTRS, &start) orelse return error.Corrupt;
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    t.range(gpa, &start, &end, 256, &kvs) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        else => error.Corrupt,
    };
    for (kvs.items) |kv| {
        const xv = record.XattrValue.decode(kv.value) catch return error.Corrupt;
        if (std.mem.eql(u8, xv.name, claim.CLAIM_XATTR)) return try gpa.dupe(u8, xv.value);
    }
    return null;
}

/// A probe needs a handful of nodes (the roots, the ino-1 xattr leaf);
/// the floor the cache shards derive from.
const probe_cache_budget_bytes: u64 = 16 << 20;

/// `squeezefs clients`: the registrations of one volume (the claim holder
/// under the ONE staleness and dead-holder laws, then rung 6's census
/// through `source`). Caller frees the slice.
pub fn clients(gpa: Allocator, io: Io, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, path: []const u8, source: ?claim.RegistrationSource) ProbeError![]claim.Registration {
    var out: std.ArrayList(claim.Registration) = .empty;
    errdefer out.deinit(gpa);
    const raw = try probeClaim(gpa, io, env, pool, path);
    defer if (raw) |r| gpa.free(r);
    if (raw) |r| {
        if (claim.Claim.decode(r)) |c| {
            try out.append(gpa, claim.Registration.fromClaim(c, proc.localIdentity(), clock.realtimeCoarseNs(), proc.probe));
        } else |_| {
            // Unparseable: a stale foreign row with a placeholder identity.
            try out.append(gpa, .{ .kind = .writer, .id = 0, .pid = 0, .boot_id = @splat(0), .hostname_hash = 0, .heartbeat_ts_s = 0, .age_s = std.math.maxInt(u64), .state = .stale });
        }
    }
    if (source) |s| try s.append(s.ctx, gpa, &out);
    return out.toOwnedSlice(gpa);
}

// ---------------------------------------------------------------------------
// `squeezefs claim clear <sqmeta-uri>` (§2.7 step 4).
// ---------------------------------------------------------------------------

pub const ClearError = error{ InvalidUri, OutOfMemory, ExecutorInit };

pub const ClearOptions = struct {
    /// The process io pool; one is created when null.
    pool: ?*uring_fs.Pool = null,
    cpus: u32 = 0,
};

/// One volume's verdict.
pub const VolumeReport = struct {
    path: []const u8,
    outcome: union(enum) {
        /// The flock is held: live-mounted on this host.
        live_mounted,
        cleared: ClearOutcome,
        /// The door refused (the backend's reason).
        open_failed: backend.OpenError,
    },

    pub fn refused(self: VolumeReport) bool {
        return switch (self.outcome) {
            .live_mounted, .open_failed => true,
            .cleared => |c| c.refused(),
        };
    }

    pub fn write(self: VolumeReport, w: *Io.Writer, now_ns: i64) Io.Writer.Error!void {
        switch (self.outcome) {
            .live_mounted => try claim.writeClearLiveRefusal(w, self.path),
            .open_failed => |e| try w.print("{s}: the writer door refused: {s}", .{ self.path, @errorName(e) }),
            .cleared => |c| switch (c) {
                .no_claim => try w.print("{s}: no writer claim present — nothing to clear", .{self.path}),
                .cleared => |h| {
                    try w.print("{s}: cleared stale writer claim (holder ", .{self.path});
                    if (h) |hc| try hc.writeSummary(w, now_ns) else try w.writeAll("id=<unparseable claim value>");
                    try w.writeAll("). This was an operator attestation that the holder is down — if it was merely partitioned, stop it before it reconnects.");
                },
                .refused_fresh => |fc| try claim.writeClearFreshRefusal(w, self.path, fc, now_ns),
                .refused_own => |oc| try claim.writeClearOwnRefusal(w, self.path, oc, now_ns),
            },
        }
    }
};

pub const Report = struct {
    gpa: Allocator,
    volumes: []VolumeReport,
    paths: [uri.MAX_MEMBERS][]const u8,
    uri_copy: []u8,

    pub fn refused(self: Report) bool {
        for (self.volumes) |v| if (v.refused()) return true;
        return false;
    }

    pub fn write(self: Report, w: *Io.Writer) Io.Writer.Error!void {
        const now = clock.realtimeCoarseNs();
        for (self.volumes) |v| {
            try v.write(w, now);
            try w.writeByte('\n');
        }
    }

    pub fn deinit(self: *Report) void {
        self.gpa.free(self.volumes);
        self.gpa.free(self.uri_copy);
        self.* = undefined;
    }
};

/// Per volume of the URI, in set order: take the flock (held ⇒ refuse
/// "live-mounted"), walk the writer door in `.claim_clear` mode, close.
/// The CLI prints the report and exits nonzero if any volume refused.
pub fn claimClear(gpa: Allocator, io: Io, env: *const std.process.Environ.Map, meta_uri: []const u8, opts: ClearOptions) ClearError!Report {
    var report: Report = .{ .gpa = gpa, .volumes = &.{}, .paths = undefined, .uri_copy = try gpa.dupe(u8, meta_uri) };
    errdefer gpa.free(report.uri_copy);
    var diag: uri.Refusal = undefined;
    const parsed = uri.parse(.meta, report.uri_copy, &report.paths, &diag) catch return error.InvalidUri;
    const vols = try gpa.alloc(VolumeReport, parsed.paths.len);
    errdefer gpa.free(vols);
    const facts = derive.facts();
    const sqz = exec.SqzIo.init(gpa, .{ .env = env, .facts = facts, .budget_bytes = mem_budget.authority.resolveBudgetNow(.{ .io = io, .env = env, .facts = facts }), .meta_lanes = 1 }) catch return error.ExecutorInit;
    defer sqz.deinit();
    const Run = struct {
        fn clearVolume(s: *exec.SqzIo, g: Allocator, e: *const std.process.Environ.Map, path: []const u8, o: ClearOptions) VolumeReport {
            const be = KvBackend.open(g, s.io(), path, .{ .env = e, .pool = o.pool, .cpus = o.cpus, .guard = .{ .mode = .claim_clear } }) catch |err| return .{ .path = path, .outcome = if (err == error.Busy) .live_mounted else .{ .open_failed = err } };
            defer be.close();
            return .{ .path = path, .outcome = .{ .cleared = be.guard.?.clear_outcome.? } };
        }
        fn main(s: *exec.SqzIo, g: Allocator, e: *const std.process.Environ.Map, paths: []const []const u8, o: ClearOptions, out: []VolumeReport) void {
            for (paths, out) |p, *v| v.* = clearVolume(s, g, e, p, o);
        }
    };
    sqz.run(Run.main, .{ sqz, gpa, env, parsed.paths, opts, vols }) catch return error.ExecutorInit;
    report.volumes = vols;
    return report;
}

// ---------------------------------------------------------------------------
// Tests: the ladder on file volumes with the fake namespace (the
// mount_writer_guard_tests contracts).
// ---------------------------------------------------------------------------

const testing = std.testing;
const align_probe = @import("../../io/align_probe.zig");
const geometry = @import("../geometry.zig");
const fake = @import("fake_nvme.zig");
const journal_lane = @import("../journal_lane.zig");

const test_dir = "/build/tmp";
const test_volume_len: u64 = 64 << 20;

const host_a_id: [16]u8 = "guard-host-a-01!".*;
const host_b_id: [16]u8 = "guard-host-b-02!".*;
const boot_a: [16]u8 = "boot-a-boot-a-01".*;
const boot_b: [16]u8 = "boot-b-boot-b-02".*;

/// A fixed pid table: 500 live at tick 123; everything else absent.
const PidTable = struct {
    fn startOf(_: ?*anyopaque, pid: u32) ?u64 {
        return if (pid == 500) 123 else null;
    }
    const probe: claim.PidProbe = .{ .ctx = null, .startTicksOf = startOf };
};

const Vol = struct {
    gpa: Allocator,
    env: std.process.Environ.Map,
    sqz: *exec.SqzIo,
    pool: *uring_fs.Pool,
    path_buf: [128]u8 = undefined,
    path_len: usize = 0,

    fn path(self: *const Vol) []const u8 {
        return self.path_buf[0..self.path_len];
    }

    fn create(gpa: Allocator, name: []const u8, flush_ms: ?[]const u8) !*Vol {
        const self = try gpa.create(Vol);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .env = std.process.Environ.Map.init(gpa), .sqz = undefined, .pool = undefined };
        errdefer self.env.deinit();
        if (flush_ms) |v| try self.env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", v);
        self.sqz = try exec.SqzIo.testInstance();
        errdefer self.sqz.deinit();
        self.pool = try uring_fs.Pool.init(gpa, uring_fs.Config.resolve(&self.env, derive.Facts.probe()));
        errdefer self.pool.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, test_dir), 0o755);
        self.path_len = (try std.fmt.bufPrint(&self.path_buf, "{s}/sqz-guard-{d}-{s}.img", .{ test_dir, linux.getpid(), name })).len;
        const z = try align_probe.pathZ(&zbuf, self.path());
        const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
        if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        if (linux.errno(linux.ftruncate(fd, @intCast(test_volume_len))) != .SUCCESS) return error.CannotSizeDevice;
        try backend.formatVolume(gpa, self.path(), .{ .env = &self.env, .pool = self.pool, .node_size = 65536, .journal_len_override = 1 << 20, .uuid = "guard-vol-uuid!!".*, .set = .{ .set_uuid = "guard-set-uuid!!".*, .hash_seed = 0x6A6A_6A6A_1234_5678 } });
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

    fn open(self: *Vol, g: Options, lane_error_logs: usize) !*KvBackend {
        return KvBackend.open(self.gpa, self.sqz.io(), self.path(), .{ .env = &self.env, .pool = self.pool, .cache_budget_bytes = 64 << 20, .cpus = 4, .guard = g, .test_error_logs = lane_error_logs });
    }

    fn probe(self: *Vol) !?claim.Claim {
        const raw = try probeClaim(self.gpa, self.sqz.io(), &self.env, self.pool, self.path());
        defer if (raw) |r| self.gpa.free(r);
        const r = raw orelse return null;
        return try claim.Claim.decode(r);
    }
};

fn ident(boot: [16]u8, pid: u32, start: u64) claim.Identity {
    return .{ .boot_id = boot, .pid = pid, .start_ticks = start, .hostname_hash = 0x1234 };
}

test "claim written at mount (this identity, beat 0, a fresh ts) and deleted on a clean unmount; a probe reads it without the flock while the mount is live and nothing after" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "claim", null);
    defer v.destroy();
    const me = ident(boot_a, 4000, 777);
    const be = try v.open(.{ .test_identity = me, .test_pid_probe = PidTable.probe }, 0);
    const c = (try v.probe()).?;
    try testing.expectEqual(me.pid, c.pid);
    try testing.expectEqualSlices(u8, &boot_a, &c.boot_id);
    try testing.expectEqual(me.writerId(0), c.writer_id);
    try testing.expectEqual(@as(u64, 0), c.heartbeat_seq);
    try testing.expect(c.ageS(clock.realtimeCoarseNs()) <= 2);
    try testing.expectEqualStrings("flock+claim", be.guard.?.modeName());
    try testing.expect(be.guard.?.claimed.load(.acquire));
    // The probe is never blocked by the held flock.
    const rows = try clients(gpa, v.sqz.io(), &v.env, v.pool, v.path(), null);
    defer gpa.free(rows);
    try testing.expectEqual(@as(usize, 1), rows.len);
    try testing.expectEqual(claim.RegistrationState.live, rows[0].state);
    be.close();
    try testing.expectEqual(@as(?claim.Claim, null), try v.probe());
    const none = try clients(gpa, v.sqz.io(), &v.env, v.pool, v.path(), null);
    defer gpa.free(none);
    try testing.expectEqual(@as(usize, 0), none.len);
}

test "a clean dismount wakes the parked heartbeat: the close never waits out the liveness tick (a stop whose own store pre-set the word made `tick.wake`'s swap read a token and skip the futex wake — every clean unmount, `fsck --offline` and offline verb waited up to 2 s)" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "hb-stop", null);
    defer v.destroy();
    const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_pid_probe = PidTable.probe }, 0);
    const t0 = clock.monotonicNs();
    be.close();
    const close_ns = clock.monotonicNs() - t0;
    // The heartbeat parked at the open for its 10 s beat; only a delivered
    // wake ends that park before the tick rescues it.
    if (close_ns >= tick.tick_ns / 2) std.debug.print("\nthe dismount took {d} ms (the liveness tick is {d} ms)\n", .{ close_ns / std.time.ns_per_ms, tick.tick_ns / std.time.ns_per_ms });
    try testing.expect(close_ns < tick.tick_ns / 2);
}

test "the ordering law: the claim is committed and barriered before the checkpoint thread spawns — the first ledger record of the mount already covers the claim entry (reusable_upto == head at return)" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "order", "600000");
    defer v.destroy();
    const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_pid_probe = PidTable.probe }, 0);
    defer be.close();
    // Covered: nothing reclaimable sits behind the head (the cover ran
    // AFTER the claim's barrier), the lane's barrier count is ≥ 1.
    try testing.expectEqual(be.core.headPos(), be.core.reusableUpto());
    try testing.expect(be.laneSnapshot().barriers_coalesced >= 1);
    try testing.expect(be.ckpt_thread != null);
}

test "a second writer of a LIVE volume refuses Busy by the claim when the flock is bypassed (another host: a foreign boot id); the refusal names the holder; the first writer is untouched" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "foreign-fresh", null);
    defer v.destroy();
    const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_pid_probe = PidTable.probe }, 0);
    defer be.close();
    const refusals0 = guard.stats.claim_refusals.load();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Busy, v.open(.{ .test_identity = ident(boot_b, 4000, 777), .test_pid_probe = PidTable.probe, .test_skip_flock = true }, 0));
    try log.testing.expectAllFired();
    try testing.expectEqual(refusals0 + 1, guard.stats.claim_refusals.load());
    try testing.expect(be.guard.?.claimed.load(.acquire));
    try testing.expect((try v.probe()) != null);
}

test "the recovery ladder step 1: a FRESH claim of a provably dead same-host pid (kill -9's residue) is reclaimed at once; a live same-host pid is NOT proof (Busy); own residue is reclaimed" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "dead-pid", null);
    defer v.destroy();
    // Incarnation 1 (pid 700, absent from the table) dies without shutdown:
    // simulate by a writer that never deletes (a FAILED-looking exit is
    // hard to fake; instead plant its claim through a clean open whose
    // close is replaced by the kill — the kill matrix's child does this
    // for real in the smoke). Here: open as pid 700, then skip the delete
    // by clearing `claimed` before close.
    {
        const be = try v.open(.{ .test_identity = ident(boot_a, 700, 55), .test_pid_probe = PidTable.probe }, 0);
        be.guard.?.claimed.store(false, .release);
        be.close();
    }
    const left = (try v.probe()).?;
    try testing.expectEqual(@as(u32, 700), left.pid);
    try testing.expect(left.isFresh(clock.realtimeCoarseNs()));
    const reclaims0 = guard.stats.reclaims_dead_pid.load();
    // pid 500 is live in the table (another incarnation at tick 123 vs
    // the claim's 55 would be dead; the same tick is alive).
    {
        const be = try v.open(.{ .test_identity = ident(boot_a, 500, 123), .test_pid_probe = PidTable.probe }, 0);
        try testing.expectEqual(reclaims0 + 1, guard.stats.reclaims_dead_pid.load());
        be.guard.?.claimed.store(false, .release);
        be.close();
    }
    // Now the residue is pid 500 at tick 123 — LIVE: a second writer (pid
    // 4000, same boot, flock bypassed) is refused.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Busy, v.open(.{ .test_identity = ident(boot_a, 4000, 1), .test_pid_probe = PidTable.probe, .test_skip_flock = true }, 0));
    try log.testing.expectAllFired();
    // Own residue (same pid + boot): reclaimed.
    const be = try v.open(.{ .test_identity = ident(boot_a, 500, 123), .test_pid_probe = PidTable.probe }, 0);
    be.close();
    try testing.expectEqual(@as(?claim.Claim, null), try v.probe());
}

test "a stale cross-host claim on a NON-PR volume refuses naming `claim clear`; `claim clear` refuses a FRESH claim, clears a stale one, is a no-op without a claim, and refuses a live-mounted volume (flock)" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "clear", null);
    defer v.destroy();
    // Plant a foreign-boot claim 100 s old: open as boot B, then rewrite
    // its ts back through the heartbeat path is not possible; instead
    // plant it by a direct tx on an open volume whose own claim is then
    // released without a delete.
    {
        const be = try v.open(.{ .test_identity = ident(boot_b, 900, 9), .test_pid_probe = PidTable.probe }, 0);
        var old = be.guard.?.ourClaim();
        old.ts_ns -= 100 * std.time.ns_per_s;
        const t = try be.begin(@src());
        try be.guard.?.stageClaimPut(t, old);
        try be.commit(t);
        be.guard.?.claimed.store(false, .release);
        be.close();
    }
    const planted = (try v.probe()).?;
    try testing.expect(!planted.isFresh(clock.realtimeCoarseNs()));
    // The mount: stale foreign on a non-PR substrate ⇒ refuse, naming the verb.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Busy, v.open(.{ .test_identity = ident(boot_a, 4000, 1), .test_pid_probe = PidTable.probe }, 0));
    try log.testing.expectAllFired();
    // claim clear: stale ⇒ cleared.
    var uri_buf: [160]u8 = undefined;
    const meta_uri = try std.fmt.bufPrint(&uri_buf, "sqmeta://{s}", .{v.path()});
    {
        var r = try claimClear(gpa, testing.io, &v.env, meta_uri, .{ .pool = v.pool, .cpus = 4 });
        defer r.deinit();
        try testing.expect(!r.refused());
        try testing.expect(r.volumes[0].outcome.cleared == .cleared);
        try testing.expectEqual(@as(u32, 900), r.volumes[0].outcome.cleared.cleared.?.pid);
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try r.write(&w);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "cleared stale writer claim") != null);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "operator attestation") != null);
    }
    try testing.expectEqual(@as(?claim.Claim, null), try v.probe());
    // No claim ⇒ no-op.
    {
        var r = try claimClear(gpa, testing.io, &v.env, meta_uri, .{ .pool = v.pool, .cpus = 4 });
        defer r.deinit();
        try testing.expect(!r.refused());
        try testing.expectEqual(ClearOutcome.no_claim, r.volumes[0].outcome.cleared);
    }
    // A fresh foreign claim ⇒ refused.
    {
        const be = try v.open(.{ .test_identity = ident(boot_b, 901, 9), .test_pid_probe = PidTable.probe }, 0);
        be.guard.?.claimed.store(false, .release);
        be.close();
    }
    {
        log.testing.expectErrorLogs(1);
        var r = try claimClear(gpa, testing.io, &v.env, meta_uri, .{ .pool = v.pool, .cpus = 4 });
        defer r.deinit();
        try log.testing.expectAllFired();
        try testing.expect(r.refused());
        try testing.expect(r.volumes[0].outcome.cleared == .refused_fresh);
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try r.write(&w);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "FRESH writer claim") != null);
    }
    // Live-mounted (the flock held): refused before any read. The planted
    // fresh foreign claim is dead by the table (pid 901 absent, but boot B
    // is not ours — so it stays fresh foreign); mount as boot B's own pid
    // to get past it.
    {
        const be = try v.open(.{ .test_identity = ident(boot_b, 901, 9), .test_pid_probe = PidTable.probe }, 0);
        defer be.close();
        log.testing.expectErrorLogs(1);
        var r = try claimClear(gpa, testing.io, &v.env, meta_uri, .{ .pool = v.pool, .cpus = 4 });
        defer r.deinit();
        try log.testing.expectAllFired();
        try testing.expect(r.refused());
        try testing.expect(r.volumes[0].outcome == .live_mounted);
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try r.write(&w);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "live-mounted on this host") != null);
    }
    // A bad URI is typed.
    try testing.expectError(error.InvalidUri, claimClear(gpa, testing.io, &v.env, "sqdata:///x", .{}));
}

test "a heartbeat rewrites ts and heartbeat_seq through the conveyor (coalesced barrier), counted; a failed refresh counts a heartbeat failure and warns" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "beat", "600000");
    defer v.destroy();
    const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_pid_probe = PidTable.probe }, 0);
    defer be.close();
    const g = be.guard.?;
    const c0 = (try v.probe()).?;
    const beats0 = guard.stats.heartbeats.load();
    g.beat();
    const c1 = (try v.probe()).?;
    try testing.expectEqual(c0.heartbeat_seq + 1, c1.heartbeat_seq);
    try testing.expect(c1.ts_ns >= c0.ts_ns);
    try testing.expectEqual(beats0 + 1, guard.stats.heartbeats.load());
    const fails0 = guard.stats.heartbeat_failures.load();
    journal_lane.test_seams.fail_windows.store(1, .release);
    g.beat();
    try testing.expectEqual(fails0 + 1, guard.stats.heartbeat_failures.load());
    try testing.expect(!be.isFailed());
    // The window the fake failure ate: a later beat lands again.
    g.beat();
    try testing.expectEqual(c1.heartbeat_seq + 2, (try v.probe()).?.heartbeat_seq);
}

test "B1 on a PR substrate (the fake): acquired WE at mount with our derived key, released at a clean unmount with zero residue; writer_guard_mode is flock+pr; RESCAP 0 degrades to flock+claim" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "pr", null);
    defer v.destroy();
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var host: fake.Host = .{ .ns = &ns, .host_id = host_a_id };
    const me = ident(boot_a, 4000, 777);
    {
        const be = try v.open(.{ .test_identity = me, .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0);
        try testing.expectEqualStrings("flock+pr", be.guard.?.modeName());
        try testing.expectEqual(@as(?u64, claim.prKey(me.writerId(0), boot_a)), ns.holder);
        try testing.expectEqual(nvme.RTYPE_WRITE_EXCLUSIVE, ns.rtype);
        try testing.expect(ns.writeAllowed(host_a_id));
        try testing.expect(!ns.writeAllowed(host_b_id));
        var buf: [2048]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try be.guard.?.writeStatsJson(&w);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"writer_guard_mode\":\"flock+pr\"") != null);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"pr_registrants_per_namespace\":") != null);
        be.close();
    }
    try testing.expectEqual(@as(?u64, null), ns.holder);
    try testing.expectEqual(@as(usize, 0), ns.registered.items.len);
    ns.rescap = 0;
    const be = try v.open(.{ .test_identity = me, .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0);
    defer be.close();
    try testing.expectEqualStrings("flock+claim", be.guard.?.modeName());
    try testing.expectEqual(@as(?u64, null), ns.holder);
}

test "the recovery ladder step 2 on PR: a TTL-stale foreign claim with a foreign reservation holder is PREEMPTED (one preempt, holder transferred); a FRESH foreign claim is refused BEFORE any PR arbitration; a conflict with no holder in the report refuses blind" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "preempt", null);
    defer v.destroy();
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var host: fake.Host = .{ .ns = &ns, .host_id = host_a_id };
    // The stale foreign claim + the foreign holder on the device.
    {
        const be = try v.open(.{ .test_identity = ident(boot_b, 900, 9), .test_pid_probe = PidTable.probe }, 0);
        var old = be.guard.?.ourClaim();
        old.ts_ns -= 100 * std.time.ns_per_s;
        const t = try be.begin(@src());
        try be.guard.?.stageClaimPut(t, old);
        try be.commit(t);
        be.guard.?.claimed.store(false, .release);
        be.close();
    }
    try ns.seedHolder(0xdead_beef);
    const preempts0 = guard.stats.preempts.load();
    {
        const me = ident(boot_a, 4000, 777);
        const be = try v.open(.{ .test_identity = me, .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0);
        try testing.expectEqual(@as(u32, 1), ns.preempts);
        try testing.expectEqual(preempts0 + 1, guard.stats.preempts.load());
        try testing.expectEqual(@as(?u64, claim.prKey(me.writerId(0), boot_a)), ns.holder);
        try testing.expect(!ns.isRegistered(0xdead_beef));
        be.close();
    }
    // A fresh foreign claim: refused with no PR command issued.
    {
        const be = try v.open(.{ .test_identity = ident(boot_b, 900, 9), .test_pid_probe = PidTable.probe }, 0);
        be.guard.?.claimed.store(false, .release);
        be.close();
    }
    try ns.seedHolder(0xdead_beef);
    const cmds = ns.commands;
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Busy, v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0));
    try log.testing.expectAllFired();
    // Only the RESCAP probe ran (the posture), never a register/acquire.
    try testing.expectEqual(cmds + 1, ns.commands);
    try testing.expectEqual(@as(?u64, 0xdead_beef), ns.holder);
    // Clear the claim through the verb, then a conflict whose report has
    // no holder: a reservation held with rtype 0 cannot exist, so forge an
    // acquire conflict by an UNREGISTERED foreign host… the fake conflicts
    // an acquire only for registration reasons; make the report blind by
    // seeding the holder under rtype 0.
    var uri_buf: [160]u8 = undefined;
    const meta_uri = try std.fmt.bufPrint(&uri_buf, "sqmeta://{s}", .{v.path()});
    {
        // The planted claim is fresh: age it past the TTL first.
        const be = try v.open(.{ .test_identity = ident(boot_b, 900, 9), .test_pid_probe = PidTable.probe }, 0);
        var old = be.guard.?.ourClaim();
        old.ts_ns -= 100 * std.time.ns_per_s;
        const t = try be.begin(@src());
        try be.guard.?.stageClaimPut(t, old);
        try be.commit(t);
        be.guard.?.claimed.store(false, .release);
        be.close();
        var r = try claimClear(gpa, testing.io, &v.env, meta_uri, .{ .pool = v.pool, .cpus = 4 });
        defer r.deinit();
        try testing.expect(!r.refused());
    }
    ns.rtype = 0;
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Busy, v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0));
    try log.testing.expectAllFired();
}

test "the heartbeat's PR re-check: a PTPL lapse (power cycle) is re-acquired and counted on writer_guard_pr_reacquires; a foreign holder fail-stops the volume (fenced, counted); a report failure skips the beat" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "lapse", "600000");
    defer v.destroy();
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var host: fake.Host = .{ .ns = &ns, .host_id = host_a_id };
    const me = ident(boot_a, 4000, 777);
    {
        const be = try v.open(.{ .test_identity = me, .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0);
        defer be.close();
        const g = be.guard.?;
        const re0 = guard.stats.pr_reacquires.load();
        ns.powerCycle();
        try testing.expectEqual(@as(?u64, null), ns.holder);
        g.beat();
        try testing.expectEqual(re0 + 1, guard.stats.pr_reacquires.load());
        try testing.expectEqual(@as(?u64, claim.prKey(me.writerId(0), boot_a)), ns.holder);
        try testing.expectEqual(@as(u64, 1), (try v.probe()).?.heartbeat_seq);
        // A report failure: the beat is skipped (no refresh), nothing latched.
        ns.fail_next = error.InputOutput;
        g.beat();
        try testing.expectEqual(@as(u64, 1), (try v.probe()).?.heartbeat_seq);
        try testing.expect(!be.isFailed());
    }
    // A usurper: the foreign holder fences this writer at the re-check.
    {
        const be = try v.open(.{ .test_identity = me, .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0);
        defer be.close();
        const g = be.guard.?;
        ns.powerCycle();
        try ns.seedHolder(0xbad);
        const fenced0 = guard.stats.fenced.load();
        log.testing.expectErrorLogs(2);
        g.beat();
        try log.testing.expectAllFired();
        try testing.expect(be.isFailed());
        try testing.expectEqual(journal.FailureLatch.Verdict.fenced, be.lane.latch.verdict());
        try testing.expectEqual(fenced0 + 1, guard.stats.fenced.load());
        // No refresh past the foreign reservation; later commits answer FAILED.
        try testing.expectEqual(@as(u64, 0), (try v.probe()).?.heartbeat_seq);
        const t = try be.begin(@src());
        try g.stageClaimPut(t, g.ourClaim());
        try testing.expectError(error.VolumeFailed, be.commit(t));
        // The dismount of a fenced volume is clean: the claim stays (TTL).
        log.testing.expectErrorLogs(0);
    }
    try testing.expect((try v.probe()) != null);
}

test "a stats render is no atomic cut across the fence: the volume's verdict (`meta.set`) renders BEFORE the process-wide writer_guard_fenced (`meta.guard`), so a fence landing between the two sections reads `healthy` beside the count — a render begun after the count was seen reads `fenced` (the nvmet sigstop-preempt row's law)" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "render", "600000");
    defer v.destroy();
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var host: fake.Host = .{ .ns = &ns, .host_id = host_a_id };
    const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0);
    defer be.close();
    const g = be.guard.?;
    const fenced0 = guard.stats.fenced.load();
    var vol_buf: [2048]u8 = undefined;
    var vol_w: Io.Writer = .fixed(&vol_buf);
    try g.writeStatsJson(&vol_w);
    // The usurper's fence lands between the two sections of one render.
    ns.powerCycle();
    try ns.seedHolder(0xbad);
    log.testing.expectErrorLogs(2);
    g.beat();
    try log.testing.expectAllFired();
    var proc_buf: [2048]u8 = undefined;
    var proc_w: Io.Writer = .fixed(&proc_buf);
    try guard.writeStatsJson(&proc_w);
    try testing.expect(std.mem.indexOf(u8, vol_w.buffered(), "\"writer_guard_verdict\":\"healthy\"") != null);
    var want_buf: [64]u8 = undefined;
    try testing.expect(std.mem.indexOf(u8, proc_w.buffered(), try std.fmt.bufPrint(&want_buf, "\"writer_guard_fenced\":{d},", .{fenced0 + 1})) != null);
    // A render begun after the count: the latch is set before the count is
    // paid, and never clears.
    vol_w = .fixed(&vol_buf);
    try g.writeStatsJson(&vol_w);
    try testing.expect(std.mem.indexOf(u8, vol_w.buffered(), "\"writer_guard_verdict\":\"fenced\"") != null);
    log.testing.expectErrorLogs(0);
}

test "host identity stability: a changed SQUEEZEFS_HOSTNQN/HOSTID under a live PR mount fail-stops and fires the tripwire; exactly one of the pair refuses the mount (InvalidIdentity)" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "identity", "600000");
    defer v.destroy();
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var host: fake.Host = .{ .ns = &ns, .host_id = host_a_id };
    try v.env.put("SQUEEZEFS_HOSTNQN", "nqn.2014-08.org.nvmexpress:uuid:test");
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.InvalidIdentity, v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0));
    try log.testing.expectAllFired();
    try v.env.put("SQUEEZEFS_HOSTID", "0000-1111");
    const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_namespace = host.face(), .test_pid_probe = PidTable.probe }, 0);
    defer be.close();
    try v.env.put("SQUEEZEFS_HOSTID", "2222-3333");
    const trip0 = guard.stats.identity_mismatch.load();
    log.testing.expectErrorLogs(2);
    be.guard.?.beat();
    try log.testing.expectAllFired();
    try testing.expect(be.isFailed());
    try testing.expectEqual(trip0 + 1, guard.stats.identity_mismatch.load());
}

test "THE FENCE LATCH on the device path: an injected reservation conflict on (a) a lane barrier, (b) an entry write, (c) a heartbeat write, (d) the checkpointer's barrier ⇒ FAILED at once (no 3-count), typed fenced, counted; every later commit answers VolumeFailed; the dismount is clean" {
    const gpa = testing.allocator;
    const Case = enum { lane_barrier, entry_write, heartbeat_write, checkpoint_barrier };
    for ([_]Case{ .lane_barrier, .entry_write, .heartbeat_write, .checkpoint_barrier }) |case| {
        var nb: [32]u8 = undefined;
        const name = try std.fmt.bufPrint(&nb, "fence-{s}", .{@tagName(case)});
        // Strict mode for the barrier cases (a barrier per commit).
        const v = try Vol.create(gpa, name, if (case == .lane_barrier) "0" else "600000");
        defer v.destroy();
        const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_pid_probe = PidTable.probe }, if (case == .checkpoint_barrier) 0 else 1);
        defer be.close();
        const fenced0 = guard.stats.fenced.load();
        const gauge0 = guard.stats.volume_fenced.load();
        const bade: u32 = @backingInt(linux.E.BADE);
        switch (case) {
            .lane_barrier => {
                journal_lane.test_seams.fail_errno.store(bade, .release);
                journal_lane.test_seams.fail_barriers.store(1, .release);
                defer journal_lane.test_seams.fail_errno.store(0, .release);
                const t = try be.begin(@src());
                try be.guard.?.stageClaimPut(t, be.guard.?.ourClaim());
                try testing.expectError(error.JournalWriteFailed, be.commit(t));
            },
            .entry_write => {
                journal_lane.test_seams.fail_errno.store(bade, .release);
                journal_lane.test_seams.fail_windows.store(1, .release);
                defer journal_lane.test_seams.fail_errno.store(0, .release);
                const t = try be.begin(@src());
                try be.guard.?.stageClaimPut(t, be.guard.?.ourClaim());
                try testing.expectError(error.JournalWriteFailed, be.commit(t));
            },
            .heartbeat_write => {
                journal_lane.test_seams.fail_errno.store(bade, .release);
                journal_lane.test_seams.fail_windows.store(1, .release);
                defer journal_lane.test_seams.fail_errno.store(0, .release);
                const fails0 = guard.stats.heartbeat_failures.load();
                be.guard.?.beat();
                try testing.expectEqual(fails0 + 1, guard.stats.heartbeat_failures.load());
            },
            .checkpoint_barrier => {
                device_loader.test_seams.fail_errno.store(bade, .release);
                device_loader.test_seams.fail_barriers.store(1, .release);
                defer device_loader.test_seams.fail_errno.store(0, .release);
                // The checkpointer's own barrier runs on THIS thread: its
                // device-error line and the fence line land here.
                log.testing.expectErrorLogs(2);
                try testing.expectError(error.Io, be.checkpointNow());
                try log.testing.expectAllFired();
            },
        }
        try testing.expect(be.isFailed());
        try testing.expectEqual(journal.FailureLatch.Verdict.fenced, be.lane.latch.verdict());
        try testing.expectEqual(fenced0 + 1, guard.stats.fenced.load());
        try testing.expectEqual(gauge0 + 1, guard.stats.volume_fenced.load());
        const t2 = try be.begin(@src());
        try be.guard.?.stageClaimPut(t2, be.guard.?.ourClaim());
        try testing.expectError(error.VolumeFailed, be.commit(t2));
        try testing.expectEqual(@as(u32, 0), journal_lane.test_seams.fail_windows.load(.acquire));
        try testing.expectEqual(@as(u32, 0), journal_lane.test_seams.fail_barriers.load(.acquire));
        try testing.expectEqual(@as(u32, 0), device_loader.test_seams.fail_barriers.load(.acquire));
    }
}

test "the writer claim's commit failing EIO at open refuses the mount loud (Io), its transaction freed exactly once — `commit` consumes the tx on every path, so the staging errdefer must not span it (the handover move matrix crashed through a freed guard set under a device EIO)" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "claim-eio", "600000");
    defer v.destroy();
    journal_lane.test_seams.fail_windows.store(1, .release);
    defer journal_lane.test_seams.fail_windows.store(0, .release);
    // The refusal's one line, on this thread.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Io, v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_pid_probe = PidTable.probe }, 0));
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u32, 0), journal_lane.test_seams.fail_windows.load(.acquire));
    // Nothing landed: the next open claims the volume.
    try testing.expectEqual(@as(?claim.Claim, null), try v.probe());
    const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_pid_probe = PidTable.probe }, 0);
    be.close();
}

test "a generic (EIO) barrier failure is NOT the fence: three in a row latch FAILED (not fenced) with a success reset in between" {
    const gpa = testing.allocator;
    const v = try Vol.create(gpa, "generic", "0");
    defer v.destroy();
    const be = try v.open(.{ .test_identity = ident(boot_a, 4000, 777), .test_pid_probe = PidTable.probe }, 1);
    defer be.close();
    const g = be.guard.?;
    const fenced0 = guard.stats.fenced.load();
    journal_lane.test_seams.fail_barriers.store(2, .release);
    for (0..2) |_| {
        const t = try be.begin(@src());
        try g.stageClaimPut(t, g.ourClaim());
        try testing.expectError(error.JournalWriteFailed, be.commit(t));
    }
    try testing.expect(!be.isFailed());
    // A success resets the run.
    g.beat();
    journal_lane.test_seams.fail_barriers.store(3, .release);
    for (0..3) |_| {
        const t = try be.begin(@src());
        try g.stageClaimPut(t, g.ourClaim());
        try testing.expectError(error.JournalWriteFailed, be.commit(t));
    }
    try testing.expect(be.isFailed());
    try testing.expectEqual(journal.FailureLatch.Verdict.failed, be.lane.latch.verdict());
    try testing.expectEqual(fenced0, guard.stats.fenced.load());
}
