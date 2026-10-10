//! One lane queue = one io_uring (`DEFER_TASKRUN | SINGLE_ISSUER | CQE32`) =
//! one TCP connection = one driver thread `sqz-zcrx-q<qid>` (charter §6.3;
//! `docs/porting/ops_zcrx.md` §3.3, §6). Linux only.
//!
//! Two receive legs, ONE loop: under the `zcrx` backend the ring registers
//! the NIC RX queue as an ifq (`REGISTER_ZCRX_IFQ`), arms multishot
//! `RECV_ZC` and recycles CQE spans through the refill ring; under the
//! `sim` backend (the venue correctness leg) a one-shot `RECV` lands in the
//! remainder of a granted area chunk and the "CQE span" is `(slot, cursor,
//! res)`. Everything above the opcode — the grant ledger, the parser, the
//! fill table, the gather, the park governor, poison, quiescence — is the
//! same product code.
//!
//! The pass (§6.3): exit when poisoned or closed (closing the ring fd IS
//! the zcrx teardown: the kernel restarts the NIC queue and reclaims the
//! provider) → refill → re-arm the recv when due (an unarmed recv ALWAYS
//! retries; the retry IS the probe) → starvation bookkeeping → drain the
//! command ring into `SEND` SQEs (`MSG_NOSIGNAL`; the capsule bytes live in
//! a session-lifetime arena of `depth × 72 B`, zero per-op allocation;
//! partial sends resubmit the remainder under the same tag) → ONE enter
//! (a timed EXT_ARG park at 200 µs while parked with work pending, else
//! unbounded) → reap: a recv with `res > 0` is progress (ends any
//! episode); without `F_MORE` the end is classified (EOF ⇒ orderly iff
//! the table is empty; ENOMEM/ENOBUFS/ENOSPC ⇒ park by class, never
//! device death; else terminal ⇒ poison).
//!
//! SINGLE_ISSUER law: the ring is built, armed, driven and quiesced on the
//! driver thread; the requesters only push a CID onto the command ring and
//! ring the doorbell (the `ring_park` wake pair); the JOINER `deinit`s the
//! ring after the join (the `ring_park` order: the eventfd stays open until
//! then, so a closer's wake never races the driver's exit). Teardown: the
//! close latch + doorbell → the driver retires its wake READ, cancels every
//! in-flight op (`IORING_REGISTER_SYNC_CANCEL`, `ALL | ANY`), reaps, fails
//! every pending fill (the drain's recycle edge), returns its recv chunk
//! ref and exits; the join is the PROOF of quiescence.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;

const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const thread_name = @import("../../core/thread_name.zig");
const futex = @import("../../sync/futex.zig");
const completion = @import("../../sync/completion.zig");
const mpmc = @import("../../sync/mpmc.zig");
const uring128 = @import("../../io/uring128.zig");
const placement = @import("../../exec/placement.zig");
const abi = @import("abi.zig");
const pdu = @import("pdu.zig");
const pdu_stream = @import("pdu_stream.zig");
const span_ledger = @import("span_ledger.zig");
const area = @import("area.zig");
const fill_table = @import("fill_table.zig");
const governor = @import("governor.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("zcrx/lane is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// The lane ring's posture (charter §6.3). `Ring128.initWithCq` adds
/// `SQE128 | CQE32 | CQSIZE | CLAMP`.
pub const RING_FLAGS: u32 = linux.IORING_SETUP_SINGLE_ISSUER | linux.IORING_SETUP_DEFER_TASKRUN;

/// `user_data` tags (below `ring_park.reserved_ud`): the recv, the sends by CID.
pub const TAG_RECV: u64 = 1;
pub const TAG_SEND_BASE: u64 = 0x1000;

pub const Backend = union(enum) {
    sim,
    zcrx: struct { ifindex: u32, rxq: u32 },
};

pub const Config = struct {
    qid: u16,
    sock: linux.fd_t,
    backend: Backend,
    depth: u32,
    max_xfer: u32,
    chunk: u64,
    area_len: u64,
    admitted_window: u64,
    mtu: ?u32,
    numa_node: ?u16,
    /// The failover bound from the measured RTT class.
    fail_bound_ns: u64,
    /// Test declaration: the loud paths (a refusal, a poison) are EXPECTED
    /// on this queue — the log seam is armed on the driver thread that
    /// logs them (`log.testing`), so the loudness is asserted, never
    /// suppressed. Default-inert.
    expect_loud: bool = false,
};

pub const ArmError = error{ RingSetup, AreaMap, IfqRefused, OutOfMemory, ThreadSpawn };

/// What the driver reports once its ring stands (or why it could not).
pub const Armed = union(enum) { ok, failed: ArmError };

/// The refill ring view after registration (zcrx leg).
pub const Refill = struct {
    region: []align(std.heap.page_size_min) u8,
    head_off: u32,
    tail_off: u32,
    rqes_off: u32,
    entries: u32,
    tail_cache: u32 = 0,
    area_token: u64,
    zcrx_id: u32,

    fn head(self: *const Refill) *std.atomic.Value(u32) {
        return @ptrCast(@alignCast(&self.region[self.head_off]));
    }
    fn tail(self: *const Refill) *std.atomic.Value(u32) {
        return @ptrCast(@alignCast(&self.region[self.tail_off]));
    }
    fn rqes(self: *const Refill) [*]abi.Rqe {
        return @ptrCast(@alignCast(&self.region[self.rqes_off]));
    }

    /// Post one span back; false when full (the caller DEFERS — overwriting
    /// an unconsumed rqe would hand the NIC a span the lane still owns).
    /// Full iff `tail − head ≥ entries` (wrapping distance, never a raw compare).
    pub fn post(self: *Refill, raw_off: u64, len: u32) bool {
        // Acquire pairs with the kernel's release of `head` after consuming.
        const h = self.head().load(.acquire);
        if (self.tail_cache -% h >= self.entries) return false;
        self.rqes()[self.tail_cache & (self.entries - 1)] = .{ .off = raw_off, .len = len };
        self.tail_cache +%= 1;
        // Release pairs with the kernel's acquire of `tail`: the rqe is
        // written before the count moves.
        self.tail().store(self.tail_cache, .release);
        return true;
    }
};

/// A span record the driver keeps per granted slot (zcrx leg): the CQE's
/// `(raw_off, len)` to echo back; `len == 0` = fresh (nothing to return).
const SpanRecord = struct { raw_off: u64 = 0, len: u32 = 0 };

/// The cold-path latch between a closer's doorbell and the joiner's ring
/// deinit (a raw futex word; never on an op path).
const LifeLock = struct {
    word: futex.Word = .init(0),

    fn lock(self: *LifeLock) void {
        while (self.word.cmpxchgWeak(0, 1, .acquire, .monotonic) != null) futex.wait(&self.word, 1);
    }

    fn unlock(self: *LifeLock) void {
        self.word.store(0, .release);
        futex.wakeOne(&self.word);
    }
};

pub const Queue = struct {
    gpa: Allocator,
    cfg: Config,
    mapped: area.Mapped,
    ledger: span_ledger.SpanLedger,
    table: fill_table.Table,
    admission: fill_table.Admission,
    parser: pdu_stream.Parser = .{},
    /// Requester → driver: CIDs whose capsule is encoded in `capsules[cid]`.
    cmds: mpmc.BoundedMpmc(u16),
    capsules: []pdu.Capsule,
    ring: uring128.ParkedRing128 = undefined,
    /// Serialises the closer's doorbell against the joiner's ring deinit.
    life_mu: LifeLock = .{},
    ring_live: bool = false,
    gov: governor.Governor,
    /// 0 = parked before `go`; 1 = go; 2 = abort (unwind before go).
    go: futex.Word = .init(0),
    closed: std.atomic.Value(bool) = .init(false),
    poisoned: std.atomic.Value(bool) = .init(false),
    /// The funnel's degraded gate: new reads bypass this queue while recovering.
    starved: std.atomic.Value(bool) = .init(false),
    starved_structural: std.atomic.Value(bool) = .init(false),
    ready: completion.Completion(Armed) = .{},
    thread: ?std.Thread = null,
    // The sim recv cursor.
    recv_slot: ?u32 = null,
    recv_cursor: u32 = 0,
    recv_armed: bool = false,
    // The zcrx leg: the refill ring, the span records, the deferred returns,
    // the records granted ahead of their CQEs.
    refill: ?Refill = null,
    records: []SpanRecord,
    deferred: std.ArrayList(u32) = .empty,
    ready_slots: std.ArrayList(u32) = .empty,
    // Sends: bytes already on the wire per CID; partial remainders to resubmit.
    sent: []u32,
    resend: std.ArrayList(u16) = .empty,
    sends_inflight: u32 = 0,

    pub fn init(gpa: Allocator, cfg: Config) ArmError!*Queue {
        const self = gpa.create(Queue) catch return error.OutOfMemory;
        errdefer gpa.destroy(self);
        var mapped = area.Mapped.map(cfg.area_len, cfg.chunk, cfg.numa_node) catch return error.AreaMap;
        errdefer mapped.unmap();
        const chunks = mapped.chunks();
        var ledger = span_ledger.SpanLedger.init(gpa, chunks) catch return error.OutOfMemory;
        errdefer ledger.deinit(gpa);
        // Spans per fill ≤ chunks touched + PDU starts: chunks + depth bounds it.
        var table = fill_table.Table.init(gpa, cfg.depth, chunks + cfg.depth) catch return error.OutOfMemory;
        errdefer table.deinit(gpa);
        var cmds = mpmc.BoundedMpmc(u16).init(gpa, cfg.depth) catch return error.OutOfMemory;
        errdefer cmds.deinit(gpa);
        const capsules = gpa.alloc(pdu.Capsule, cfg.depth) catch return error.OutOfMemory;
        errdefer gpa.free(capsules);
        const records = gpa.alloc(SpanRecord, chunks) catch return error.OutOfMemory;
        errdefer gpa.free(records);
        @memset(records, .{});
        const sent = gpa.alloc(u32, cfg.depth) catch return error.OutOfMemory;
        @memset(sent, 0);
        self.* = .{
            .gpa = gpa,
            .cfg = cfg,
            .mapped = mapped,
            .ledger = ledger,
            .table = table,
            .admission = fill_table.Admission.init(area.admissionPermits(cfg.admitted_window, cfg.chunk)),
            .cmds = cmds,
            .capsules = capsules,
            .gov = governor.Governor.init(clock.monotonicNs()),
            .records = records,
            .sent = sent,
        };
        return self;
    }

    /// After `joinDriver` (or when the driver never started).
    pub fn deinit(self: *Queue) void {
        std.debug.assert(self.thread == null and !self.ring_live);
        self.resend.deinit(self.gpa);
        self.ready_slots.deinit(self.gpa);
        self.deferred.deinit(self.gpa);
        self.gpa.free(self.sent);
        self.gpa.free(self.records);
        self.gpa.free(self.capsules);
        self.cmds.deinit(self.gpa);
        self.table.deinit(self.gpa);
        self.ledger.deinit(self.gpa);
        self.mapped.unmap();
        _ = linux.close(self.cfg.sock);
        self.gpa.destroy(self);
    }

    // ---- the requester face ----

    /// Queue an encoded read capsule for `cid` and ring the doorbell.
    pub fn submit(self: *Queue, cid: u16) void {
        std.debug.assert(self.cmds.tryPush(cid));
        self.doorbell(false);
    }

    pub fn isPoisoned(self: *const Queue) bool {
        return self.poisoned.load(.acquire);
    }

    pub fn isStarved(self: *const Queue) bool {
        return self.starved.load(.acquire);
    }

    pub fn isStructurallyStarved(self: *const Queue) bool {
        return self.starved_structural.load(.acquire);
    }

    /// The area bytes (for the gather).
    pub fn areaBytes(self: *const Queue) []const u8 {
        return self.mapped.bytes();
    }

    // ---- lifecycle ----

    /// Spawn the driver and wait for its ring (and ifq) to stand.
    pub fn start(self: *Queue) ArmError!void {
        self.thread = std.Thread.spawn(.{}, driverMain, .{self}) catch return error.ThreadSpawn;
        switch (self.ready.wait()) {
            .ok => {},
            .failed => |e| {
                self.joinDriver();
                return e;
            },
        }
    }

    /// Release the parked driver into its loop.
    pub fn release(self: *Queue) void {
        self.go.store(1, .release);
        futex.wakeAll(&self.go);
    }

    /// Close: the latch + the doorbell (a parked driver observes teardown).
    /// A driver still parked before `go` exits silently (nothing armed,
    /// nothing to poison).
    pub fn close(self: *Queue) void {
        self.closed.store(true, .release);
        if (self.go.load(.acquire) == 0) {
            self.go.store(2, .release);
            futex.wakeAll(&self.go);
        }
        self.doorbell(true);
    }

    fn doorbell(self: *Queue, force: bool) void {
        self.life_mu.lock();
        defer self.life_mu.unlock();
        if (!self.ring_live) return;
        if (force) self.ring.forceWake() else _ = self.ring.wake();
    }

    /// The joiner: join, then tear the ring down (after quiescence).
    pub fn joinDriver(self: *Queue) void {
        const t = self.thread orelse return;
        t.join();
        self.thread = null;
        self.life_mu.lock();
        defer self.life_mu.unlock();
        if (self.ring_live) {
            self.ring_live = false;
            self.ring.deinit();
        }
        if (self.refill) |*r| {
            uring128.Ring128.munmapRegion(r.region);
            self.refill = null;
        }
    }

    /// The poison lattice: idempotent; the winning transition counts
    /// `zcrx_lane_poisoned`, drops `zcrx_lane_armed` and ALWAYS logs the
    /// canonical line (a poisoned increment without a greppable reason is
    /// unrepresentable).
    pub fn markPoisoned(self: *Queue, why: []const u8) void {
        if (self.poisoned.swap(true, .acq_rel)) return;
        // The poison tripwire's line and the canonical line.
        self.declareLoud(2);
        stats.zcrx_lane_armed.set(0);
        stats.zcrx_lane_poisoned.fire(why);
        log.err("zcrx lane q{d}: POISONED — {s}; the kernel path serves this device for the rest of the mount", .{ self.cfg.qid, why });
        self.doorbell(true);
    }

    fn declareLoud(self: *Queue, n: usize) void {
        if (builtin.is_test and self.cfg.expect_loud) log.testing.expectErrorLogs(n);
    }

    // ---- the driver ----

    fn driverMain(self: *Queue) void {
        var nb: thread_name.CommBuf = undefined;
        var name_buf: [32]u8 = undefined;
        const name = std.fmt.bufPrint(&name_buf, "sqz-zcrx-q{d}", .{self.cfg.qid}) catch "sqz-zcrx-q";
        _ = thread_name.nameCurrentThread(&nb, name);
        if (self.cfg.numa_node) |n| pinToNode(n);
        // The ring: SQ = next_pow2(depth + 8), CQ derived from the span demand.
        const sq = governor.sqEntriesFor(self.cfg.depth);
        const cq = governor.cqEntriesFor(self.cfg.depth, self.cfg.max_xfer, self.cfg.chunk, sq, self.cfg.mtu);
        const raw = uring128.Ring128.initWithCq(sq, cq, RING_FLAGS) catch {
            _ = self.ready.complete(.{ .failed = error.RingSetup });
            return;
        };
        const parked = uring128.ParkedRing128.initOver(self.gpa, raw) catch {
            _ = self.ready.complete(.{ .failed = error.RingSetup });
            return;
        };
        {
            self.life_mu.lock();
            defer self.life_mu.unlock();
            self.ring = parked;
            self.ring_live = true;
        }
        if (self.cfg.backend == .zcrx) {
            self.registerIfq() catch |e| {
                self.declareLoud(1);
                log.err("zcrx lane q{d}: REGISTER_ZCRX_IFQ (if_idx={d}, rxq={d}) refused: {s}", .{ self.cfg.qid, self.cfg.backend.zcrx.ifindex, self.cfg.backend.zcrx.rxq, @errorName(e) });
                _ = self.ready.complete(.{ .failed = error.IfqRefused });
                return;
            };
        }
        _ = self.ready.complete(.ok);
        // Park on `go`: 1 = run, 2 = unwind (nothing armed, nothing to poison).
        while (self.go.load(.acquire) == 0) futex.wait(&self.go, 0);
        if (self.go.load(.acquire) != 1) return;
        self.gov = governor.Governor.init(clock.monotonicNs());
        self.run();
        self.quiesce();
    }

    fn pinToNode(node: u16) void {
        var pb: [96]u8 = undefined;
        const path = std.fmt.bufPrintSentinel(&pb, "/sys/devices/system/node/node{d}/cpulist", .{node}, 0) catch return;
        const rc = linux.open(path.ptr, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var buf: [4096]u8 = undefined;
        const n = linux.read(fd, &buf, buf.len);
        if (linux.errno(n) != .SUCCESS) return;
        const set = placement.parseCpuSet(std.mem.trim(u8, buf[0..n], " \r\n")) orelse return;
        var cpu_set: linux.cpu_set_t = @splat(0);
        var it = set.iterator(.{});
        while (it.next()) |cpu| cpu_set[cpu / @bitSizeOf(usize)] |= @as(usize, 1) << @intCast(cpu % @bitSizeOf(usize));
        linux.sched_setaffinity(0, &cpu_set) catch {};
    }

    /// The zcrx registration: anonymous refill region, every kernel-OUT
    /// field down as zero, the pointers wired AFTER the structs reached
    /// their final slots.
    fn registerIfq(self: *Queue) !void {
        const z = self.cfg.backend.zcrx;
        const page: u32 = @intCast(std.heap.pageSize());
        const rq_entries = area.rqEntriesFor(self.mapped.chunks());
        const region_len: usize = @intCast(abi.refillRegionLen(rq_entries, page));
        const rrc = linux.mmap(null, region_len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
        if (linux.errno(rrc) != .SUCCESS) return error.RegionMap;
        const region: []align(std.heap.page_size_min) u8 = @as([*]align(std.heap.page_size_min) u8, @ptrFromInt(rrc))[0..region_len];
        errdefer _ = linux.munmap(region.ptr, region.len);
        var area_reg = abi.buildAreaReg(@intFromPtr(self.mapped.base), self.mapped.len);
        var region_desc = abi.buildRegionDesc(@intFromPtr(region.ptr), region_len);
        var ifq = abi.buildIfqReg(z.ifindex, z.rxq, rq_entries);
        ifq.area_ptr = @intFromPtr(&area_reg);
        ifq.region_ptr = @intFromPtr(&region_desc);
        try self.ring.ring.registerRaw(abi.REGISTER_ZCRX_IFQ, &ifq, 1);
        self.refill = .{ .region = region, .head_off = ifq.offsets.head, .tail_off = ifq.offsets.tail, .rqes_off = ifq.offsets.rqes, .entries = ifq.rq_entries, .area_token = area_reg.rq_area_token, .zcrx_id = ifq.zcrx_id };
    }

    fn exiting(self: *const Queue) bool {
        return self.closed.load(.acquire) or self.poisoned.load(.acquire);
    }

    fn run(self: *Queue) void {
        var cqes: [256]uring128.Cqe32 = undefined;
        while (!self.exiting()) {
            const now = clock.monotonicNs();
            _ = self.ring.service();
            if (!self.ring.armWake()) {
                self.markPoisoned("the lane ring's SQ stayed full after a flush: the doorbell could not be armed");
                break;
            }
            self.refillPass();
            if (!self.recv_armed and (self.gov.rearmDue() or !self.gov.episode_active)) self.armRecv(now);
            if (self.gov.failoverDue(now, self.cfg.fail_bound_ns)) |window| self.onFailover(now, window);
            self.drainCommands();
            // The one enter: a bounded poll while parked with work pending
            // (the park never woke by itself — finding 5), else unbounded.
            const pending = self.table.pendingCount() > 0 or self.sends_inflight > 0;
            _ = self.ring.park(if (self.gov.pollBounded(pending)) governor.PARK_POLL_NS else null);
            if (self.ring.isDead()) {
                self.markPoisoned("the lane ring died (io_uring_enter failed)");
                break;
            }
            while (!self.exiting()) {
                const n = self.ring.ring.copyCqes(&cqes, 0) catch {
                    self.markPoisoned("the lane ring's CQ could not be read");
                    break;
                };
                if (n == 0) break;
                for (cqes[0..n]) |cqe| self.reap(cqe);
            }
            // Drained while starved: one read may re-enter (the episode stays
            // open — ending it recounted a park per poll, round 6).
            if (self.gov.episode_active and self.table.pendingCount() == 0) {
                self.gov.onIdleDrain();
                self.starved.store(false, .release);
            }
            self.drainInstruments();
        }
    }

    fn drainInstruments(self: *Queue) void {
        if (self.parser.hdr_copy_bytes != 0) {
            stats.zcrx_hdr_copy_bytes.add(self.parser.hdr_copy_bytes);
            self.parser.hdr_copy_bytes = 0;
        }
    }

    // ---- refill (zcrx) ----

    /// Post every deferred return whose rqe bounced; then grant free
    /// records: a fresh one is ready for the next CQE, one with a pending
    /// span posts it first (full ⇒ deferred, the slot goes back).
    fn refillPass(self: *Queue) void {
        const r = &(self.refill orelse return);
        while (self.deferred.items.len > 0) {
            const slot = self.deferred.items[self.deferred.items.len - 1];
            const rec = self.records[slot];
            if (!r.post(rec.raw_off, rec.len)) return;
            self.records[slot] = .{};
            _ = self.ledger.release(slot);
            _ = self.deferred.pop();
        }
        while (self.ledger.grant()) |slot| {
            const rec = self.records[slot];
            if (rec.len != 0) {
                if (!r.post(rec.raw_off, rec.len)) {
                    self.deferred.append(self.gpa, slot) catch {
                        _ = self.ledger.release(slot);
                    };
                    return;
                }
                self.records[slot] = .{};
            }
            self.ready_slots.append(self.gpa, slot) catch {
                _ = self.ledger.release(slot);
                return;
            };
        }
    }

    // ---- the recv arm ----

    fn armRecv(self: *Queue, now: u64) void {
        switch (self.cfg.backend) {
            .sim => {
                if (self.recv_slot == null) {
                    self.recv_slot = self.ledger.grant() orelse {
                        if (self.gov.onPark(now)) self.countPark(.pool_dry);
                        return;
                    };
                    self.recv_cursor = 0;
                }
                const chunk = self.mapped.chunkBytes(self.recv_slot.?);
                const sqe = self.ring.getSqe() orelse return;
                uring128.prepRecv(sqe, .{ .raw = self.cfg.sock }, chunk[self.recv_cursor..], linux.MSG.NOSIGNAL);
                sqe.user_data = TAG_RECV;
            },
            .zcrx => {
                const sqe = self.ring.getSqe() orelse return;
                uring128.prepRecvZc(sqe, .{ .raw = self.cfg.sock }, self.refill.?.zcrx_id);
                sqe.user_data = TAG_RECV;
            },
        }
        self.recv_armed = true;
        self.gov.onArmed();
    }

    fn countPark(self: *Queue, class: governor.ParkClass) void {
        stats.zcrx_recv_parks.inc();
        switch (class) {
            .pool_dry => stats.zcrx_parks_pool_dry.inc(),
            .rq_empty => stats.zcrx_parks_rq_empty.inc(),
            .cq_full => stats.zcrx_parks_cq_full.inc(),
        }
        self.starved.store(true, .release);
        log.warn("zcrx lane q{d}: recv parked — {s}; polling bounded until the pool refills", .{ self.cfg.qid, class.name() });
    }

    fn onFailover(self: *Queue, now: u64, window_ns: u64) void {
        stats.zcrx_recv_failovers.inc();
        stats.zcrx_starved_ms.add(window_ns / std.time.ns_per_ms);
        self.starved.store(true, .release);
        const structural = self.gov.structural() or self.gov.economicStructural(now, self.cfg.fail_bound_ns);
        if (structural and !self.starved_structural.swap(true, .acq_rel)) {
            stats.zcrx_structural_teardowns.inc();
            log.err("zcrx lane q{d}: refill starvation is STRUCTURAL ({s}) — the session tears down; the kernel path serves this device at full width", .{ self.cfg.qid, if (self.gov.structural()) "a second consecutive failover window" else "the economic arm: starved for the majority of the lane's life" });
        }
        log.warn("zcrx lane q{d}: refill starvation — failed over to the kernel path for the window ({d} ms); failing {d} pending fill(s) releases their spans", .{ self.cfg.qid, window_ns / std.time.ns_per_ms, self.table.pendingCount() });
        // Failing the fills is ALSO the recovery: their spans release, rqes post.
        self.table.failAll(&self.ledger);
    }

    // ---- commands ----

    fn drainCommands(self: *Queue) void {
        while (self.resend.items.len > 0) {
            const cid = self.resend.items[self.resend.items.len - 1];
            if (!self.pushSend(cid)) return;
            _ = self.resend.pop();
        }
        while (self.cmds.tryPop()) |cid| {
            self.sent[cid] = 0;
            if (!self.pushSend(cid)) {
                // No SQE this pass: the capsule waits in the arena.
                std.debug.assert(self.cmds.tryPush(cid));
                return;
            }
            self.table.activate(cid);
        }
    }

    fn pushSend(self: *Queue, cid: u16) bool {
        const sqe = self.ring.getSqe() orelse return false;
        const bytes = self.capsules[cid].bytes();
        uring128.prepSend(sqe, .{ .raw = self.cfg.sock }, bytes[self.sent[cid]..], linux.MSG.NOSIGNAL);
        sqe.user_data = TAG_SEND_BASE + cid;
        self.sends_inflight += 1;
        return true;
    }

    // ---- reap ----

    fn reap(self: *Queue, cqe: uring128.Cqe32) void {
        if (self.ring.consumeWake(cqe)) return;
        if (cqe.user_data == TAG_RECV) return self.reapRecv(cqe);
        if (cqe.user_data >= TAG_SEND_BASE and cqe.user_data < TAG_SEND_BASE + self.cfg.depth) return self.reapSend(cqe);
        self.markPoisoned("a CQE with an unknown user_data landed on the lane ring");
    }

    fn reapSend(self: *Queue, cqe: uring128.Cqe32) void {
        const cid: u16 = @intCast(cqe.user_data - TAG_SEND_BASE);
        self.sends_inflight -= 1;
        if (cqe.res < 0) {
            log.warn("zcrx lane q{d}: SEND of cid {d} failed ({t}); the op fails to the kernel path", .{ self.cfg.qid, cid, cqe.err() });
            self.table.failSend(cid, &self.ledger);
            return;
        }
        self.sent[cid] += @intCast(cqe.res);
        if (self.sent[cid] < self.capsules[cid].len) {
            if (!self.pushSend(cid)) self.resend.append(self.gpa, cid) catch {
                self.table.failSend(cid, &self.ledger);
            };
        }
    }

    fn reapRecv(self: *Queue, cqe: uring128.Cqe32) void {
        const more = cqe.flags & linux.IORING_CQE_F_MORE != 0;
        if (!more) self.recv_armed = false;
        const now = clock.monotonicNs();
        if (cqe.res > 0) {
            if (self.gov.onProgress(now)) |tail| {
                stats.zcrx_starved_ms.add(tail / std.time.ns_per_ms);
                self.starved.store(false, .release);
                log.info("zcrx lane q{d}: refill recovered after {d} ms", .{ self.cfg.qid, tail / std.time.ns_per_ms });
            }
            const res: u32 = @intCast(cqe.res);
            switch (self.cfg.backend) {
                .sim => {
                    const slot = self.recv_slot.?;
                    const abs: u32 = @intCast(@as(u64, slot) * self.cfg.chunk + self.recv_cursor);
                    const bytes = self.mapped.chunkBytes(slot)[self.recv_cursor..][0..res];
                    self.recv_cursor += res;
                    self.feed(slot, abs, bytes);
                    if (self.recv_cursor == self.cfg.chunk) {
                        _ = self.ledger.release(slot);
                        self.recv_slot = null;
                    }
                },
                .zcrx => self.feedZcrx(cqe, res),
            }
            if (self.exiting()) return;
        }
        if (more) return;
        switch (governor.classifyRecvEnd(cqe.res)) {
            .eof => {
                if (self.table.pendingCount() == 0) {
                    log.info("zcrx lane q{d}: the controller closed the connection; orderly exit", .{self.cfg.qid});
                    self.closed.store(true, .release);
                } else self.markPoisoned("the controller closed the connection mid-operation");
            },
            .park => |class| {
                if (self.gov.onPark(now)) self.countPark(class);
            },
            .rearm => {},
            .terminal => |res| {
                var buf: [96]u8 = undefined;
                const why = std.fmt.bufPrint(&buf, "RECV terminal: errno {d}", .{-res}) catch "RECV terminal";
                self.markPoisoned(why);
            },
        }
    }

    /// A zcrx CQE: the provenance gate, the span gate, the slot record, the parse.
    fn feedZcrx(self: *Queue, cqe: uring128.Cqe32, res: u32) void {
        const r = &self.refill.?;
        const raw_off = uring128.zcrxOff(&cqe);
        if (!abi.cqeAreaMatches(raw_off, r.area_token)) return self.frameViolation("a RECV_ZC CQE named a foreign area token");
        const off = abi.cqeSpanOffset(raw_off, res, self.mapped.len) orelse return self.markPoisoned("a RECV_ZC CQE span lies outside the area");
        const slot = if (self.ready_slots.pop()) |s| s else self.ledger.grant() orelse return self.markPoisoned("span records exhausted: no ledger slot for a RECV_ZC CQE");
        if (self.records[slot].len != 0) {
            // A late grant with a pending return: post it first (or defer).
            if (!r.post(self.records[slot].raw_off, self.records[slot].len)) self.deferred.append(self.gpa, slot) catch {};
        }
        self.records[slot] = .{ .raw_off = raw_off, .len = res };
        const bytes = self.mapped.bytes()[@intCast(off)..][0..res];
        self.feed(slot, @intCast(off), bytes);
        // The recv-pass ref: payload spans hold their own.
        _ = self.ledger.release(slot);
    }

    fn feed(self: *Queue, slot: u32, abs_off: u32, bytes: []const u8) void {
        const sink: pdu_stream.Sink = .{ .ctx = self, .emit = &emitEvent };
        self.parser.push(slot, abs_off, bytes, sink) catch |e| self.frameViolation(@errorName(e));
    }

    fn emitEvent(ctx: *anyopaque, ev: pdu_stream.Event) error{Violation}!void {
        const self: *Queue = @ptrCast(@alignCast(ctx));
        return self.table.apply(ev, &self.ledger);
    }

    fn frameViolation(self: *Queue, what: []const u8) void {
        var buf: [160]u8 = undefined;
        const why = std.fmt.bufPrint(&buf, "framing violation: {s}", .{what}) catch "framing violation";
        self.declareLoud(1);
        stats.zcrx_frame_violations.fire(why);
        self.markPoisoned(why);
    }

    // ---- quiescence ----

    /// On the driver thread at exit: retire the wake READ (ring_park's
    /// protocol), cancel every other in-flight op and reap (so nothing
    /// lands in the area or the arena after they are freed), fail every
    /// pending fill, return the recv chunk ref, shut the socket.
    fn quiesce(self: *Queue) void {
        self.ring.quiesce();
        var reg: linux.io_uring_sync_cancel_reg = .{ .addr = 0, .fd = -1, .flags = linux.IORING_ASYNC_CANCEL_ALL | linux.IORING_ASYNC_CANCEL_ANY, .timeout = .{ .sec = 1, .nsec = 0 }, .pad = @splat(0) };
        self.ring.ring.registerRaw(@backingInt(linux.IORING_REGISTER.REGISTER_SYNC_CANCEL), &reg, 1) catch {};
        var cqes: [64]uring128.Cqe32 = undefined;
        while ((self.ring.ring.copyCqes(&cqes, 0) catch 0) > 0) {}
        self.table.failAll(&self.ledger);
        if (self.recv_slot) |s| {
            _ = self.ledger.release(s);
            self.recv_slot = null;
        }
        while (self.ready_slots.pop()) |s| _ = self.ledger.release(s);
        self.drainInstruments();
        _ = linux.shutdown(self.cfg.sock, linux.SHUT.RDWR);
    }
};

// ---------------------------------------------------------------------------
// Tests (venue): the ring stands with the lane flags; the refill ring
// arithmetic over an anonymous region; the ifq registration refusal
// against ifindex 0 reaches `ready` and the queue unwinds clean; an
// unwound-before-go driver exits without poisoning; the sim leg end to
// end over a socketpair against a mock controller.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn socketPair() ![2]linux.fd_t {
    var fds: [2]i32 = undefined;
    if (linux.errno(linux.socketpair(linux.AF.UNIX, linux.SOCK.STREAM | linux.SOCK.CLOEXEC, 0, &fds)) != .SUCCESS) return error.SocketPair;
    return .{ fds[0], fds[1] };
}

fn simConfig(sock: linux.fd_t) Config {
    const chunk: u64 = 64 << 10;
    const w = area.areaBytesPerQueue(4, 1 << 20);
    return .{ .qid = 1, .sock = sock, .backend = .sim, .depth = 4, .max_xfer = 1 << 20, .chunk = chunk, .area_len = w, .admitted_window = area.simAdmittedWindow(w), .mtu = null, .numa_node = null, .fail_bound_ns = governor.FAILOVER_BOUND_MIN_NS };
}

test "refill ring arithmetic: post encodes the rqe and advances the tail; full refuses by wrapping distance and recovers when the kernel consumes; indices wrap by mask" {
    const page: usize = std.heap.pageSize();
    const region_len: usize = @intCast(abi.refillRegionLen(4, @intCast(page)));
    const rrc = linux.mmap(null, region_len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rrc));
    const region: []align(std.heap.page_size_min) u8 = @as([*]align(std.heap.page_size_min) u8, @ptrFromInt(rrc))[0..region_len];
    defer _ = linux.munmap(region.ptr, region.len);
    @memset(region, 0);
    var r: Refill = .{ .region = region, .head_off = 0, .tail_off = 64, .rqes_off = @intCast(page), .entries = 4, .area_token = 7 << 48, .zcrx_id = 0 };
    try testing.expect(r.post(4096, 1500));
    try testing.expectEqual(@as(u32, 1), r.tail().load(.acquire));
    try testing.expectEqual(@as(u64, 4096), r.rqes()[0].off);
    try testing.expectEqual(@as(u32, 1500), r.rqes()[0].len);
    try testing.expectEqual(@as(u32, 0), r.rqes()[0].__pad);
    try testing.expect(r.post(8192, 100));
    try testing.expect(r.post(12288, 100));
    try testing.expect(r.post(16384, 100));
    try testing.expect(!r.post(20480, 100));
    // The kernel consumes two: two more fit; the fifth lands at index 0 (mask).
    r.head().store(2, .release);
    try testing.expect(r.post(20480, 100));
    try testing.expectEqual(@as(u64, 20480), r.rqes()[0].off);
    try testing.expect(r.post(24576, 100));
    try testing.expect(!r.post(1, 1));
    // Counters wrapping before the indices do: a tail near u32::MAX.
    r.tail_cache = std.math.maxInt(u32) - 1;
    r.head().store(std.math.maxInt(u32) - 1, .release);
    try testing.expect(r.post(1, 1));
    try testing.expect(r.post(2, 2));
    try testing.expectEqual(@as(u32, 0), r.tail_cache);
    try testing.expectEqual(@as(u64, 2), r.rqes()[3].off);
}

test "the lane ring stands with SINGLE_ISSUER|DEFER_TASKRUN|CQE32 and the derived CQ; a driver unwound before `go` exits clean without poisoning; every chunk and CID is free" {
    const fds = try socketPair();
    defer _ = linux.close(fds[1]);
    const q = try Queue.init(testing.allocator, simConfig(fds[0]));
    defer q.deinit();
    try q.start();
    try testing.expect(q.ring.ring.flags & linux.IORING_SETUP_SINGLE_ISSUER != 0);
    try testing.expect(q.ring.ring.flags & linux.IORING_SETUP_DEFER_TASKRUN != 0);
    try testing.expect(q.ring.ring.flags & linux.IORING_SETUP_CQE32 != 0);
    try testing.expectEqual(@as(usize, governor.sqEntriesFor(4)), q.ring.ring.sq.sqes.len);
    try testing.expectEqual(@as(usize, governor.cqEntriesFor(4, 1 << 20, 64 << 10, governor.sqEntriesFor(4), null)), q.ring.ring.cq.cqes.len);
    const poisoned_before = stats.zcrx_lane_poisoned.load();
    q.close();
    q.joinDriver();
    try testing.expect(!q.isPoisoned());
    try testing.expectEqual(poisoned_before, stats.zcrx_lane_poisoned.load());
    try testing.expectEqual(q.ledger.slots(), q.ledger.freeCount());
    try testing.expectEqual(q.table.cids.slots().total, q.table.cids.slots().free);
}

test "the ifq registration refusal against ifindex 0 reaches `ready` naming the step (IfqRefused) and the queue unwinds without poisoning" {
    const fds = try socketPair();
    defer _ = linux.close(fds[1]);
    var cfg = simConfig(fds[0]);
    cfg.backend = .{ .zcrx = .{ .ifindex = 0, .rxq = 0 } };
    cfg.chunk = @intCast(std.heap.pageSize());
    cfg.expect_loud = true;
    const q = try Queue.init(testing.allocator, cfg);
    defer q.deinit();
    try testing.expectError(error.IfqRefused, q.start());
    try testing.expect(q.thread == null);
    try testing.expect(!q.isPoisoned());
}

test "the sim leg end to end over a socketpair: a mock controller answers two reads (one SUCCESS|LAST split across chunks, one SUCCESS-elided + padded + CapsuleResp); content byte-exact, every chunk back, free == total; a framing violation poisons and drains" {
    const fds = try socketPair();
    const peer = fds[1];
    defer _ = linux.close(peer);
    var cfg = simConfig(fds[0]);
    cfg.expect_loud = true;
    const q = try Queue.init(testing.allocator, cfg);
    defer q.deinit();
    try q.start();
    q.release();
    // Read A: 100 KiB at datao 0 — straddles two 64 KiB chunks.
    const len_a: u32 = 100 << 10;
    const cid_a = q.table.cids.take().?;
    try testing.expect(q.admission.tryAcquire(fill_table.unitsFor(len_a)));
    q.table.prepare(cid_a, len_a);
    q.capsules[cid_a] = pdu.encodeRead(cid_a, 1, 0, len_a >> 9, len_a);
    q.submit(cid_a);
    var cap: [72]u8 = undefined;
    try readExact(peer, &cap);
    try testing.expectEqual(pdu.OPC_READ, cap[8]);
    try testing.expectEqual(cid_a, std.mem.readInt(u16, cap[10..12], .little));
    const payload_a = try testing.allocator.alloc(u8, len_a);
    defer testing.allocator.free(payload_a);
    for (payload_a, 0..) |*b, i| b.* = @truncate(i * 7 + 3);
    var hdr: [24]u8 = undefined;
    pdu.encodeC2HDataHeader(&hdr, cid_a, 0, len_a, 0, true, true);
    try writeAll(peer, &hdr);
    try writeAll(peer, payload_a);
    try testing.expectEqual(fill_table.Verdict.ok, q.table.cells[cid_a].done.wait());
    const dest_a = try testing.allocator.alloc(u8, len_a);
    defer testing.allocator.free(dest_a);
    q.table.gatherInto(cid_a, dest_a, q.areaBytes());
    try testing.expectEqualSlices(u8, payload_a, dest_a);
    q.table.releaseSpans(cid_a, &q.ledger);
    q.admission.release(fill_table.unitsFor(len_a));
    q.table.cids.put(cid_a);
    // Read B: SUCCESS elided, completed by a CapsuleResp; two C2HData PDUs,
    // the first padded.
    const len_b: u32 = 8192;
    const cid_b = q.table.cids.take().?;
    q.table.prepare(cid_b, len_b);
    q.capsules[cid_b] = pdu.encodeRead(cid_b, 1, 8, len_b >> 9, len_b);
    q.submit(cid_b);
    try readExact(peer, &cap);
    var payload_b: [8192]u8 = undefined;
    for (&payload_b, 0..) |*b, i| b.* = @truncate(i ^ 0x5A);
    pdu.encodeC2HDataHeader(&hdr, cid_b, 0, 4096, 8, false, false);
    try writeAll(peer, &hdr);
    const pad: [8]u8 = @splat(0);
    try writeAll(peer, &pad);
    try writeAll(peer, payload_b[0..4096]);
    pdu.encodeC2HDataHeader(&hdr, cid_b, 4096, 4096, 0, true, false);
    try writeAll(peer, &hdr);
    try writeAll(peer, payload_b[4096..]);
    var resp: [24]u8 = undefined;
    pdu.encodeCapsuleResp(&resp, cid_b, 0, 0, 0, true);
    try writeAll(peer, &resp);
    try testing.expectEqual(fill_table.Verdict.ok, q.table.cells[cid_b].done.wait());
    var dest_b: [8192]u8 = undefined;
    q.table.gatherInto(cid_b, &dest_b, q.areaBytes());
    try testing.expectEqualSlices(u8, &payload_b, &dest_b);
    q.table.releaseSpans(cid_b, &q.ledger);
    q.table.cids.put(cid_b);
    // Every chunk back except the one the recv cursor holds.
    try testing.expect(q.ledger.freeCount() + 1 >= q.ledger.slots());
    try testing.expectEqual(q.table.cids.slots().total, q.table.cids.slots().free);
    // Read C: a framing violation (an H2CData on the IO queue) poisons the
    // session and drains the pending fill.
    const cid_c = q.table.cids.take().?;
    q.table.prepare(cid_c, 4096);
    q.capsules[cid_c] = pdu.encodeRead(cid_c, 1, 0, 8, 4096);
    q.submit(cid_c);
    try readExact(peer, &cap);
    pdu.encodeC2HDataHeader(&hdr, cid_c, 0, 4096, 0, true, true);
    hdr[0] = @backingInt(pdu.Type.h2c_data);
    const fv_before = stats.zcrx_frame_violations.load();
    try writeAll(peer, &hdr);
    try testing.expectEqual(fill_table.Verdict.drained, q.table.cells[cid_c].done.wait());
    q.joinDriver();
    try testing.expect(q.isPoisoned());
    try testing.expectEqual(fv_before + 1, stats.zcrx_frame_violations.load());
    q.table.cids.put(cid_c);
    try testing.expectEqual(q.ledger.slots(), q.ledger.freeCount());
    try testing.expectEqual(q.table.cids.slots().total, q.table.cids.slots().free);
}

fn readExact(fd: linux.fd_t, buf: []u8) !void {
    var got: usize = 0;
    while (got < buf.len) {
        const n = linux.read(fd, buf[got..].ptr, buf.len - got);
        if (linux.errno(n) != .SUCCESS) return error.ReadFailed;
        if (n == 0) return error.Eof;
        got += n;
    }
}

fn writeAll(fd: linux.fd_t, bytes: []const u8) !void {
    var done: usize = 0;
    while (done < bytes.len) {
        const n = linux.sendto(fd, bytes[done..].ptr, bytes.len - done, linux.MSG.NOSIGNAL, null, 0);
        if (linux.errno(n) != .SUCCESS) return error.WriteFailed;
        done += n;
    }
}
