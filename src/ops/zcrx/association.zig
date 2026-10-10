//! The userspace NVMe/TCP host association (charter §6.3 "a second
//! userspace NVMe/TCP host association per device serving COLD read fills
//! only"; `docs/porting/ops_zcrx.md` §2.2–§2.3, §4). Linux only.
//!
//! Connect ladder: [phase A steering — zcrx only] → the ADMIN association
//! on a blocking socket (control plane, bring-up only: ICReq/ICResp, the
//! Fabrics Connect with the 1024 B data, Property Get CAP → MQES + TO,
//! Property Set CC, Property Get CSTS until RDY — the deadline `CAP.TO ×
//! 500 ms` the controller itself declares, polled on the Io timer at
//! `max(deadline/64, 10 ms)`; QUIRK 4 taken) → the depth
//! `max(min(derived, MQES + 1), 2)` → per `qid = 1..N` its own TCP
//! connection, ICReq/ICResp, Connect with the assigned `cntlid`, the lane
//! queue driver armed → [phase B flow rules — zcrx only] → `go`.
//!
//! Coexistence: the lane rides the kernel initiator's OWN attachment (the
//! same subsysnqn / traddr / nsid) as a SEPARATE host association under the
//! mount's identity pair, with its own `qid 1..N` space; it issues only
//! reads; KATO 0 (same-host-managed, fails loud on TCP errors); a target's
//! allow-list or cntlid ceiling surfaces at Connect — loud, the kernel path
//! intact. NVMe PR WERO permits other hosts' reads by definition; the lane
//! never joins a reservation.
//!
//! The read: whole-read atomic admission (one acquire per queue), a CID
//! per segment, the capsules encoded into the lane arenas, ONE gather per
//! segment into the caller's destination, `gather ≡ fill` counted at the
//! whole-read success boundary. No per-op timeout (decision 0002 row 34):
//! a target that stops answering is a volume event — the volume's
//! watchdog, not a lane timer.
//!
//! MEM-3: the admin watchdog thread `sqz-zcrx-admin` blocks in a read on
//! the admin socket; any data/EOF/error after bring-up is an association
//! event ⇒ poison UNLESS the teardown latch is set (orderly unwind is
//! never poison). `teardown` (idempotent): retire the watchdog FIRST →
//! close + JOIN every queue (the proof of quiescence) → restore steering →
//! release the queue lease → the census vote.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const Allocator = std.mem.Allocator;

const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const thread_name = @import("../../core/thread_name.zig");
const pdu = @import("pdu.zig");
const area = @import("area.zig");
const fill_table = @import("fill_table.zig");
const governor = @import("governor.zig");
const probe = @import("probe.zig");
const steering = @import("steering.zig");
const lane = @import("lane.zig");
const ethtool = @import("ethtool.zig");
const stats = @import("stats.zig");
const configfs = @import("../nvmet/configfs.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("zcrx/association is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// The CSTS poll floor: finer than this buys nothing against a 500 ms-unit
/// controller timeout and burns the admin socket.
pub const CSTS_POLL_FLOOR_NS: u64 = 10 * std.time.ns_per_ms;
/// `CAP.TO` is in 500 ms units (NVMe base §3.1.1).
pub const CAP_TO_UNIT_NS: u64 = 500 * std.time.ns_per_ms;

pub const Identity = struct {
    hostnqn: [256]u8 = @splat(0),
    hostnqn_len: u16 = 0,
    hostid: [16]u8 = @splat(0),

    /// The mount's pair: the NQN text and the uuid text.
    pub fn fromPair(hostnqn: []const u8, hostid_text: []const u8) ?Identity {
        if (hostnqn.len == 0 or hostnqn.len > 256) return null;
        var id: Identity = .{ .hostid = configfs.parseUuidAttr(hostid_text) orelse return null };
        @memcpy(id.hostnqn[0..hostnqn.len], hostnqn);
        id.hostnqn_len = @intCast(hostnqn.len);
        return id;
    }

    pub fn nqn(self: *const Identity) []const u8 {
        return self.hostnqn[0..self.hostnqn_len];
    }
};

pub const ConnectError = error{
    /// The TCP connect, a send or a receive failed (ECONNREFUSED, EOF, …).
    Network,
    /// ICResp demanded digests or a PFV ≠ 0, or came short/wrong-typed.
    Negotiation,
    /// A PDU other than CapsuleResp during bring-up.
    UnexpectedPdu,
    /// The controller answered a status ≠ 0 (logged with its name).
    ControllerRefused,
    /// CSTS.CFS.
    ControllerFatal,
    /// CSTS.RDY never rose within `CAP.TO × 500 ms`.
    ReadyTimeout,
    /// The lane queue could not stand (ring, area, ifq, thread).
    LaneArm,
    /// Phase A/B steering refused.
    Steering,
    OutOfMemory,
};

pub const ReadError = error{
    /// Not LBA-aligned / zero / too many segments: the kernel path is the serve.
    Ineligible,
    /// The whole-read admission declined (counted once per read).
    WouldBlock,
    /// A queue is starved: the lane declines while recovering.
    Degraded,
    /// The session is poisoned.
    Poisoned,
    /// The controller answered a status ≠ 0 for a segment (the op fails, the session lives).
    OpStatus,
    /// Short data, a send failure, a drain.
    OpFailed,
};

/// The zcrx leg's NIC binding (null under sim).
pub const NicBinding = struct {
    reg: *steering.Registry,
    nic: u8,
    ctl: steering.NicControl,
};

const AdminFacts = struct { cntlid: u16, mqes: u16, cap_to: u8, rtt_ns: u64 };

pub const Session = struct {
    gpa: Allocator,
    io: Io,
    plan: probe.Plan,
    identity: Identity,
    admin_fd: linux.fd_t,
    facts: AdminFacts,
    depth: u32,
    chunk: u64,
    queues: []*lane.Queue,
    rotor: std.atomic.Value(u64) = .init(0),
    torn: std.atomic.Value(bool) = .init(false),
    watchdog: ?std.Thread = null,
    nic: ?NicBinding,
    guard: ?steering.Guard = null,

    /// THE connect ladder.
    pub fn connect(gpa: Allocator, io: Io, plan: probe.Plan, identity: Identity, sim_chunk: ?u64, nic: ?NicBinding) ConnectError!*Session {
        const target = &plan.target;
        const self = gpa.create(Session) catch return error.OutOfMemory;
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .io = io, .plan = plan, .identity = identity, .admin_fd = -1, .facts = undefined, .depth = 0, .chunk = 0, .queues = &.{}, .nic = nic };
        // Phase A: RSS exclusion BEFORE any TCP connect, admin included.
        if (plan.backend == .zcrx) {
            const b = nic orelse return error.Steering;
            self.guard = b.reg.armRssExclusion(b.nic, b.ctl, plan.rx_queues[0..plan.io_queues]) catch |e| {
                log.warn("zcrx lane: phase-A steering refused ({s}); the kernel path serves {s}", .{ @errorName(e), target.subnqnText() });
                return error.Steering;
            };
        }
        errdefer self.restoreSteering();
        // The admin association.
        const ip = ethtool.Ip.parse(target.traddrText()) orelse return error.Network;
        self.admin_fd = try tcpConnect(ip, target.trsvcid);
        errdefer _ = linux.close(self.admin_fd);
        try icExchange(self.admin_fd);
        self.facts = try self.adminBringUp();
        self.depth = area.depthFor(target.queue_depth, self.facts.mqes);
        self.chunk = switch (plan.backend) {
            .sim => area.simChunk(sim_chunk),
            .zcrx => @intCast(std.heap.pageSize()),
        };
        // The IO queues: qid 1..N, one connection each.
        const queues = gpa.alloc(*lane.Queue, plan.io_queues) catch return error.OutOfMemory;
        var built: usize = 0;
        errdefer {
            for (queues[0..built]) |q| {
                q.close();
                q.joinDriver();
                q.deinit();
            }
            gpa.free(queues);
        }
        const fail_bound = governor.parkFailBound(self.facts.rtt_ns);
        while (built < plan.io_queues) : (built += 1) {
            const qid: u16 = @intCast(built + 1);
            const fd = try tcpConnect(ip, target.trsvcid);
            errdefer _ = linux.close(fd);
            try icExchange(fd);
            const cqe = try roundTrip(fd, pdu.encodeConnect(.{ .cid = 0, .qid = qid, .sqsize0 = @intCast(self.depth - 1), .kato = 0, .hostid = identity.hostid, .cntlid = self.facts.cntlid, .subsysnqn = target.subnqnText(), .hostnqn = identity.nqn() }));
            if (!cqe.ok()) return refused("IO-queue Connect", qid, cqe.status);
            const q = lane.Queue.init(gpa, self.queueConfig(qid, fd, fail_bound)) catch return error.LaneArm;
            errdefer q.deinit();
            q.start() catch |e| {
                log.err("zcrx lane: q{d} could not arm ({s})", .{ qid, @errorName(e) });
                return error.LaneArm;
            };
            queues[built] = q;
        }
        self.queues = queues;
        // Phase B: the flow rules for the connected sockets' ephemeral ports.
        if (plan.backend == .zcrx) try self.armFlowRules();
        for (self.queues) |q| q.release();
        self.watchdog = std.Thread.spawn(.{}, watchdogMain, .{self}) catch return error.OutOfMemory;
        stats.zcrx_lane_armed.set(1);
        log.info("zcrx-lane armed: {s} nsid {d} lba_shift {d} queues {d} depth {d} (MQES {d}) max_xfer {d} chunk {d} ({s}) hostnqn {s}", .{ target.subnqnText(), target.nsid, target.lba_shift, plan.io_queues, self.depth, self.facts.mqes, target.max_xfer, self.chunk, @tagName(plan.backend), identity.nqn() });
        return self;
    }

    fn queueConfig(self: *Session, qid: u16, fd: linux.fd_t, fail_bound: u64) lane.Config {
        const t = &self.plan.target;
        const w = area.areaBytesPerQueue(self.depth, t.max_xfer);
        return switch (self.plan.backend) {
            .sim => .{ .qid = qid, .sock = fd, .backend = .sim, .depth = self.depth, .max_xfer = t.max_xfer, .chunk = self.chunk, .area_len = w, .admitted_window = area.simAdmittedWindow(w), .mtu = null, .numa_node = null, .fail_bound_ns = fail_bound },
            .zcrx => blk: {
                const total = area.areaBytes(self.depth, t.max_xfer, self.plan.mtu, self.chunk, self.plan.rx_descs);
                const sq = governor.sqEntriesFor(self.depth);
                const cq = governor.cqEntriesFor(self.depth, t.max_xfer, self.chunk, sq, self.plan.mtu);
                const admit = @min(w, governor.cqAdmittedWindowBytes(cq, sq, self.chunk, self.plan.mtu));
                break :blk .{ .qid = qid, .sock = fd, .backend = .{ .zcrx = .{ .ifindex = self.plan.ifindex, .rxq = self.plan.rx_queues[qid - 1] } }, .depth = self.depth, .max_xfer = t.max_xfer, .chunk = self.chunk, .area_len = total, .admitted_window = admit, .mtu = self.plan.mtu, .numa_node = self.plan.numa_node, .fail_bound_ns = fail_bound };
            },
        };
    }

    /// Connect(admin) → CAP → CC → CSTS until RDY.
    fn adminBringUp(self: *Session) ConnectError!AdminFacts {
        const t = &self.plan.target;
        const fd = self.admin_fd;
        const c0 = try roundTrip(fd, pdu.encodeConnect(.{ .cid = 0, .qid = 0, .sqsize0 = pdu.ADMIN_SQSIZE0, .kato = 0, .hostid = self.identity.hostid, .cntlid = pdu.CNTLID_DYNAMIC, .subsysnqn = t.subnqnText(), .hostnqn = self.identity.nqn() }));
        if (!c0.ok()) return refused("admin Connect", 0, c0.status);
        const cntlid: u16 = @truncate(c0.dw0);
        const t0 = clock.monotonicNs();
        const cap = try roundTrip(fd, pdu.encodePropertyGet(1, pdu.PROP_CAP, pdu.ATTRIB_8B));
        const rtt = clock.monotonicNs() - t0;
        if (!cap.ok()) return refused("Property Get CAP", 0, cap.status);
        const mqes: u16 = @truncate(cap.dw0);
        const cap_to: u8 = @truncate(cap.dw0 >> 24);
        const cc = try roundTrip(fd, pdu.encodePropertySet(2, pdu.PROP_CC, pdu.CC_ENABLE_NVM));
        if (!cc.ok()) return refused("Property Set CC", 0, cc.status);
        const deadline_ns = @as(u64, @max(cap_to, 1)) * CAP_TO_UNIT_NS;
        const poll_ns = @max(deadline_ns / 64, CSTS_POLL_FLOOR_NS);
        const start = clock.monotonicNs();
        var cid: u16 = 3;
        while (true) : (cid +%= 1) {
            const csts = try roundTrip(fd, pdu.encodePropertyGet(cid, pdu.PROP_CSTS, pdu.ATTRIB_4B));
            if (!csts.ok()) return refused("Property Get CSTS", 0, csts.status);
            if (csts.dw0 & pdu.CSTS_CFS != 0) {
                log.err("zcrx lane: {s}: CSTS.CFS — the controller reports a fatal status", .{t.subnqnText()});
                return error.ControllerFatal;
            }
            if (csts.dw0 & pdu.CSTS_RDY != 0) break;
            if (clock.monotonicNs() - start >= deadline_ns) {
                log.err("zcrx lane: {s}: CSTS.RDY did not rise within CAP.TO × 500 ms = {d} ms", .{ t.subnqnText(), deadline_ns / std.time.ns_per_ms });
                return error.ReadyTimeout;
            }
            self.io.sleep(.fromNanoseconds(@intCast(poll_ns)), .awake) catch {};
        }
        return .{ .cntlid = cntlid, .mqes = mqes, .cap_to = cap_to, .rtt_ns = rtt };
    }

    fn armFlowRules(self: *Session) ConnectError!void {
        const b = self.nic.?;
        var flows: [steering.MAX_FLOWS]steering.FlowRule = undefined;
        for (self.queues, 0..) |q, i| flows[i] = flowOf(q.cfg.sock, self.plan.rx_queues[i]) orelse return error.Steering;
        b.reg.armFlowRules(b.nic, b.ctl, &self.guard.?, flows[0..self.queues.len]) catch |e| {
            log.warn("zcrx lane: phase-B steering refused ({s}); unwinding (ifqs first, then RSS)", .{@errorName(e)});
            return error.Steering;
        };
    }

    fn restoreSteering(self: *Session) void {
        if (self.guard) |*g| {
            const b = self.nic.?;
            b.reg.restore(b.ctl, g);
            self.guard = null;
        }
    }

    // ---- the read ----

    pub fn isPoisoned(self: *const Session) bool {
        for (self.queues) |q| if (q.isPoisoned()) return true;
        return false;
    }

    /// Any queue starved: the lane declines while recovering.
    pub fn isDegraded(self: *const Session) bool {
        for (self.queues) |q| if (q.isStarved()) return true;
        return false;
    }

    pub fn isStructurallyStarved(self: *const Session) bool {
        for (self.queues) |q| if (q.isStructurallyStarved()) return true;
        return false;
    }

    /// `(free, total)` CID slots across the queues: the no-leak instrument.
    pub fn cidSlots(self: *const Session) struct { free: u32, total: u32 } {
        var free: u32 = 0;
        var total: u32 = 0;
        for (self.queues) |q| {
            const s = q.table.cids.slots();
            free += s.free;
            total += s.total;
        }
        return .{ .free = free, .total = total };
    }

    /// Area chunks held by in-flight spans or the recv cursor (0 at quiescence
    /// except the one chunk each queue's recv cursor pins).
    pub fn areaChunksHeld(self: *const Session) u32 {
        var held: u32 = 0;
        for (self.queues) |q| held += q.ledger.slots() - q.ledger.freeCount();
        return held;
    }

    /// Read `dest.len` bytes at device offset `off` through the lane into
    /// `dest` (`registered` = the caller's registered destination: counted
    /// as the fused subset).
    pub fn readInto(self: *Session, off: u64, dest: []u8, registered: bool) ReadError!void {
        const t = &self.plan.target;
        if (!fill_table.rangeEligible(off, dest.len, t.lba_shift)) return error.Ineligible;
        if (self.isPoisoned()) return error.Poisoned;
        if (self.isDegraded()) return error.Degraded;
        const cap = @max(@as(u64, t.max_xfer), @as(u64, 1) << t.lba_shift);
        const rotor = self.rotor.fetchAdd(1, .monotonic);
        const plan = fill_table.plan(off, dest.len, cap, @intCast(self.queues.len), rotor) orelse return error.Ineligible;
        // Whole-read atomic admission: ONE acquire per queue; any refusal
        // drops every held permit and the read declines.
        var held: [fill_table.MAX_SEGMENTS]bool = @splat(false);
        defer for (self.queues, 0..) |q, i| if (held[i]) q.admission.release(plan.units[i]);
        for (self.queues, 0..) |q, i| {
            if (plan.units[i] == 0) continue;
            if (!q.admission.tryAcquire(plan.units[i])) {
                stats.zcrx_area_admission_waits.inc();
                return error.WouldBlock;
            }
            held[i] = true;
        }
        // The CIDs (the depth permits) for every segment, or none.
        const segs = plan.segments();
        var cids: [fill_table.MAX_SEGMENTS]u16 = undefined;
        var taken: usize = 0;
        errdefer for (segs[0..taken], 0..) |s, i| self.queues[s.queue].table.cids.put(cids[i]);
        while (taken < segs.len) : (taken += 1) {
            cids[taken] = self.queues[segs[taken].queue].table.cids.take() orelse {
                stats.zcrx_area_admission_waits.inc();
                return error.WouldBlock;
            };
        }
        // Issue every segment, then await each (no per-op timeout).
        for (segs, 0..) |s, i| {
            const q = self.queues[s.queue];
            q.table.prepare(cids[i], s.len);
            q.capsules[cids[i]] = pdu.encodeRead(cids[i], t.nsid, s.off >> t.lba_shift, @intCast(@as(u64, s.len) >> t.lba_shift), s.len);
            q.submit(cids[i]);
        }
        var failure: ?ReadError = null;
        for (segs, 0..) |s, i| {
            const q = self.queues[s.queue];
            const v = q.table.cells[cids[i]].done.wait();
            switch (v) {
                .ok => {
                    q.table.gatherInto(cids[i], dest[@intCast(s.off - off)..][0..s.len], q.areaBytes());
                    q.table.releaseSpans(cids[i], &q.ledger);
                },
                .status => |sf| {
                    log.warn("zcrx lane: read at {d}+{d} on q{d}: {f} — the op fails to the kernel path", .{ s.off, s.len, q.cfg.qid, pdu.Status.describe(sf) });
                    failure = failure orelse error.OpStatus;
                },
                .short => |sh| {
                    log.warn("zcrx lane: read at {d}+{d} on q{d} completed with {d} of {d} bytes — the op fails", .{ s.off, s.len, q.cfg.qid, sh.received, sh.len });
                    failure = failure orelse error.OpFailed;
                },
                .send_failed, .drained, .span_records_exhausted => failure = failure orelse error.OpFailed,
            }
            q.table.cids.put(cids[i]);
        }
        taken = 0;
        if (failure) |e| return e;
        // The engagement law at the WHOLE-read success boundary.
        stats.zcrx_fills.inc();
        stats.zcrx_fill_bytes.add(dest.len);
        stats.zcrx_gather_bytes.add(dest.len);
        if (registered) stats.zcrx_dest_gather_bytes.add(dest.len);
    }

    // ---- lifecycle ----

    /// The admin watchdog: any traffic on the admin queue after bring-up
    /// is an association event.
    fn watchdogMain(self: *Session) void {
        var nb: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&nb, "sqz-zcrx-admin");
        var b: [8]u8 = undefined;
        _ = linux.recvfrom(self.admin_fd, &b, b.len, 0, null, null);
        if (self.torn.load(.acquire)) return;
        for (self.queues) |q| q.markPoisoned("an association event arrived on the admin queue (data, EOF or an error after bring-up)");
    }

    /// Idempotent: watchdog → queues (close, JOIN) → steering → lease → gauge.
    pub fn teardown(self: *Session) void {
        if (self.torn.swap(true, .acq_rel)) return;
        _ = linux.shutdown(self.admin_fd, linux.SHUT.RDWR);
        if (self.watchdog) |t| {
            t.join();
            self.watchdog = null;
        }
        for (self.queues) |q| q.close();
        for (self.queues) |q| q.joinDriver();
        self.restoreSteering();
        if (self.nic) |b| {
            if (self.plan.lease) |*l| b.reg.release(l);
            b.reg.noteReleased(b.nic);
        }
        stats.zcrx_lane_armed.set(0);
        log.info("zcrx-lane torn down: {s} ({d} queue(s) joined; cid slots {d}/{d} free)", .{ self.plan.target.subnqnText(), self.queues.len, self.cidSlots().free, self.cidSlots().total });
    }

    pub fn deinit(self: *Session) void {
        self.teardown();
        for (self.queues) |q| q.deinit();
        self.gpa.free(self.queues);
        _ = linux.close(self.admin_fd);
        self.gpa.destroy(self);
    }
};

fn refused(step: []const u8, qid: u16, sf: u16) ConnectError {
    stats.zcrx_conn_errors.inc();
    log.warn("zcrx lane: {s} (qid {d}) refused by the controller: {f} — the kernel path serves the device", .{ step, qid, pdu.Status.describe(sf) });
    return error.ControllerRefused;
}

// ---- the blocking admin-side socket helpers (bring-up only, never a data path) ----

fn tcpConnect(ip: ethtool.Ip, port: u16) ConnectError!linux.fd_t {
    var storage: [128]u8 align(8) = undefined;
    var len: linux.socklen_t = undefined;
    var family: u32 = undefined;
    switch (ip) {
        .v4 => |v4| {
            family = linux.AF.INET;
            const sa: *linux.sockaddr.in = @ptrCast(&storage);
            sa.* = .{ .port = std.mem.nativeToBig(u16, port), .addr = @bitCast(v4) };
            len = @sizeOf(linux.sockaddr.in);
        },
        .v6 => |v6| {
            family = linux.AF.INET6;
            const sa: *linux.sockaddr.in6 = @ptrCast(&storage);
            sa.* = .{ .port = std.mem.nativeToBig(u16, port), .flowinfo = 0, .addr = v6, .scope_id = 0 };
            len = @sizeOf(linux.sockaddr.in6);
        },
    }
    const rc = linux.socket(family, linux.SOCK.STREAM | linux.SOCK.CLOEXEC, 0);
    if (linux.errno(rc) != .SUCCESS) return error.Network;
    const fd: linux.fd_t = @intCast(rc);
    errdefer _ = linux.close(fd);
    if (linux.errno(linux.connect(fd, @ptrCast(&storage), len)) != .SUCCESS) {
        stats.zcrx_conn_errors.inc();
        return error.Network;
    }
    const one: c_int = 1;
    _ = linux.setsockopt(fd, linux.IPPROTO.TCP, linux.TCP.NODELAY, @ptrCast(&one), @sizeOf(c_int));
    return fd;
}

fn sendAll(fd: linux.fd_t, bytes: []const u8) ConnectError!void {
    var done: usize = 0;
    while (done < bytes.len) {
        const n = linux.sendto(fd, bytes[done..].ptr, bytes.len - done, linux.MSG.NOSIGNAL, null, 0);
        if (linux.errno(n) != .SUCCESS) return error.Network;
        done += n;
    }
}

fn recvExact(fd: linux.fd_t, buf: []u8) ConnectError!void {
    var got: usize = 0;
    while (got < buf.len) {
        const n = linux.recvfrom(fd, buf[got..].ptr, buf.len - got, 0, null, null);
        if (linux.errno(n) != .SUCCESS or n == 0) return error.Network;
        got += n;
    }
}

/// ICReq out, ICResp in: digests off, PFV 0.
fn icExchange(fd: linux.fd_t) ConnectError!void {
    var req: [pdu.IC_LEN]u8 = undefined;
    pdu.ICReq.encode(&req);
    try sendAll(fd, &req);
    var resp: [pdu.IC_LEN]u8 = undefined;
    try recvExact(fd, &resp);
    _ = pdu.parseICResp(&resp) catch |e| {
        stats.zcrx_conn_errors.inc();
        log.warn("zcrx lane: ICResp refused ({s}) — a controller demanding digests or a newer PFV is not served by this lane", .{@errorName(e)});
        return error.Negotiation;
    };
}

/// One admin round trip: the capsule out; a CapsuleResp (and nothing else) in.
fn roundTrip(fd: linux.fd_t, capsule: pdu.Capsule) ConnectError!pdu.Cqe {
    try sendAll(fd, capsule.bytes());
    var hdr: [8]u8 = undefined;
    try recvExact(fd, &hdr);
    const ch = pdu.CommonHeader.decode(&hdr);
    if (ch.type != @backingInt(pdu.Type.capsule_resp) or ch.hlen != pdu.CAPSULE_RESP_LEN or ch.plen != pdu.CAPSULE_RESP_LEN) {
        stats.zcrx_conn_errors.inc();
        log.warn("zcrx lane: a PDU of type 0x{x:0>2} (hlen {d}, plen {d}) arrived during bring-up where a CapsuleResp was due", .{ ch.type, ch.hlen, ch.plen });
        return error.UnexpectedPdu;
    }
    var psh: [16]u8 = undefined;
    try recvExact(fd, &psh);
    return pdu.parseCqe(&psh);
}

/// A connected socket's `{src, dst, sport, dport}` as a lane flow rule.
fn flowOf(fd: linux.fd_t, queue: u32) ?steering.FlowRule {
    var local: [128]u8 align(8) = undefined;
    var peer: [128]u8 align(8) = undefined;
    var ll: linux.socklen_t = local.len;
    var pl: linux.socklen_t = peer.len;
    if (linux.errno(linux.getsockname(fd, @ptrCast(&local), &ll)) != .SUCCESS) return null;
    if (linux.errno(linux.getpeername(fd, @ptrCast(&peer), &pl)) != .SUCCESS) return null;
    const fam = std.mem.readInt(u16, local[0..2], .little);
    var rule: steering.FlowRule = .{ .v6 = false, .src = @splat(0), .dst = @splat(0), .sport = 0, .dport = 0, .queue = queue };
    if (fam == linux.AF.INET) {
        const l: *const linux.sockaddr.in = @ptrCast(&local);
        const p: *const linux.sockaddr.in = @ptrCast(&peer);
        rule.src[0..4].* = @bitCast(l.addr);
        rule.dst[0..4].* = @bitCast(p.addr);
        rule.sport = std.mem.bigToNative(u16, l.port);
        rule.dport = std.mem.bigToNative(u16, p.port);
    } else {
        const l: *const linux.sockaddr.in6 = @ptrCast(&local);
        const p: *const linux.sockaddr.in6 = @ptrCast(&peer);
        rule.v6 = true;
        rule.src = l.addr;
        rule.dst = p.addr;
        rule.sport = std.mem.bigToNative(u16, l.port);
        rule.dport = std.mem.bigToNative(u16, p.port);
    }
    return rule;
}

// ---------------------------------------------------------------------------
// Tests: the identity pair; THE association against the venue's nvmet-tcp.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the identity pair: a uuid text becomes the 16 hostid bytes; a malformed uuid or an empty NQN refuses" {
    const id = Identity.fromPair("nqn.2014-08.org.nvmexpress:uuid:a0a0a0a0-a0a0-4a0a-8a0a-a0a0a0a0a0a0", "a0a0a0a0-a0a0-4a0a-8a0a-a0a0a0a0a0a0").?;
    try testing.expectEqualStrings("nqn.2014-08.org.nvmexpress:uuid:a0a0a0a0-a0a0-4a0a-8a0a-a0a0a0a0a0a0", id.nqn());
    try testing.expectEqual(@as(u8, 0xa0), id.hostid[0]);
    try testing.expectEqual(@as(u8, 0x4a), id.hostid[6]);
    try testing.expect(Identity.fromPair("nqn.x", "not-a-uuid") == null);
    try testing.expect(Identity.fromPair("", "a0a0a0a0-a0a0-4a0a-8a0a-a0a0a0a0a0a0") == null);
}

test "the association against REAL nvmet-tcp over loopback: the kernel initiator connects the namespace; the userspace association connects a SECOND time read-only, reads whole blocks through the lane's RECV leg into the area and gathers byte-exact against the kernel path; gather ≡ fill; the MQES clamp; cid/chunk custody closes; teardown clean" {
    const e2e = @import("association_e2e_linux.zig");
    try e2e.run();
}
