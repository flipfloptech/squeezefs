//! An admitted session as the daemon holds it (spec `docs/porting/
//! il_daemon.md` §1.3, §1.4, §5.1, §5.5): the lease-refcounted mapping,
//! the daemon-private ring cursor (ONE consumer for life), the binding
//! table the fd screen fills, the in-flight ledger (VAL-5c), the owner
//! pin, and the completion in the shipped order — ledger → result → DONE
//! + WAITER wake → the completion doorbell.
//!
//! Lifetime: the `SessionMap` refcount is the unmap authority (charter §7
//! "lease-refcounted unmap"). The registry holds one share; every service
//! pass that drains the session, every handoff fiber and every direct-drive
//! entry in flight retains another; the LAST release unmaps AND frees the
//! session struct — a late completion writes into live private memory,
//! never a freed mapping. Teardown drops the registry's share after the
//! handoff group joined; the dd lane's in-flight entries release theirs at
//! their CQE, so "unmap under DMA" is unrepresentable by structure.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;

const layout = @import("../layout.zig");
const slot_core = @import("../slot_core.zig");
const ring_core = @import("../ring_core.zig");
const doorbell = @import("../doorbell.zig");
const wire = @import("../wire.zig");
const rendezvous = @import("../rendezvous.zig");
const shm = @import("../shm.zig");
const inval = @import("inval.zig");
const stats = @import("stats.zig").stats;

/// Bindings per session: 4 × the default `RLIMIT_NOFILE` soft limit (1024)
/// — dup chains and the fd-sharded libaio fleets bind well under it; past
/// it BIND refuses `budget` and the shim passthroughs that fd.
pub const BINDINGS_PER_SESSION: u32 = 4096;

/// What the fd screen unlocked for one bound fd.
pub const Binding = struct {
    /// The kernel nodeid (`st_ino` of the screened fd).
    ino: u64,
    rights: wire.Rights,
    /// The peer's HELLO-time standing for `FUSE_WRITE_KILL_SUIDGID`: an
    /// unprivileged uid without `CAP_FSETID` (root is exempt).
    kill_priv: bool,
    durability: wire.Durability,
    /// The description was `O_NOATIME` at BIND (the shim unbinds on an
    /// `F_SETFL` toggle of it, as of `O_DIRECT`).
    noatime: bool = false,
    live: Atomic(bool) = .init(false),
    /// A ring write of this binding the limiter's window suppressed: the
    /// kernel may hold pages or times it changed until a whole fire — the
    /// window's trailing sweep, the UNBIND, the teardown (`inval.zig`).
    dirty: Atomic(bool) = .init(false),

    /// The description's open flags as the handler's arms read them — the
    /// kernel's `fuse_read_in.flags` / `fuse_write_in.flags` for a bound fd.
    pub fn openFlags(self: *const Binding) u32 {
        const o: linux.O = .{ .DIRECT = self.rights.odirect, .NOATIME = self.noatime };
        return @bitCast(o);
    }
};

pub const PoisonReason = enum {
    slot_out_of_range,
    slot_not_submitted,
    over_admission,
    ctl_malformed,
    host_shutdown,
    idle_reap,
    shed,
    /// A write lease recalled past its bound with a client write in flight.
    write_lease_forced,

    /// A client's protocol violation (the tripwire) vs the host's own act.
    pub fn violation(self: PoisonReason) bool {
        return switch (self) {
            .slot_out_of_range, .slot_not_submitted, .over_admission, .ctl_malformed, .write_lease_forced => true,
            .host_shutdown, .idle_reap, .shed => false,
        };
    }
};

pub const Session = struct {
    gpa: Allocator,
    map: slot_core.SessionMap,
    mapped: shm.Mapped,
    layout: layout.SessionLayout,
    m: layout.Mapping,
    ring: ring_core.View,
    /// The ONE consumer's cursor: daemon-private, never in the mapping.
    consumer: ring_core.Consumer = .{},
    ctl: linux.fd_t,
    peer: rendezvous.PeerCred,
    kind: wire.ClientKind,
    footprint_bytes: u64,
    owner: u16,
    /// The arena's NUMA node (the owner's partition); null = one node.
    node: ?u16,
    bindings: []Binding,
    next_binding: Atomic(u64) = .init(1),
    /// Ops dequeued and not yet completed (VAL-5c: `> slots` ⇒ poison).
    inflight: Atomic(u32) = .init(0),
    last_active_ns: Atomic(u64),
    admitted_ns: u64,
    torn_down: Atomic(bool) = .init(false),
    /// The direct arm granted at HELLO (`DirectHello` / `LeaseReq` answered),
    /// and its write arm (WRITE leases, `WriteNote`).
    direct_granted: bool = false,
    direct_write_granted: bool = false,
    poisoned: Atomic(bool) = .init(false),
    /// Some row went dirty since the last trailing sweep (raised on a row's
    /// clean → dirty edge, after the mark; `IpcHost.sweepDirty`).
    sweep: Atomic(bool) = .init(false),
    /// The handoff fibers (joined at teardown).
    group: Io.Group = .init,

    /// The session owns its mapping: the last share unmaps and frees it.
    pub fn create(gpa: Allocator, a: anytype, ctl: linux.fd_t, owner: u16, node: ?u16, now_ns: u64) Allocator.Error!*Session {
        const self = try gpa.create(Session);
        errdefer gpa.destroy(self);
        const bindings = try gpa.alloc(Binding, BINDINGS_PER_SESSION);
        for (bindings) |*b| b.* = .{ .ino = 0, .rights = .{}, .kill_priv = false, .durability = .mount };
        const m = a.mapped.mapping;
        self.* = .{
            .gpa = gpa,
            .map = slot_core.SessionMap.init(m, self, &unmapAndFree),
            .mapped = a.mapped,
            .layout = a.layout,
            .m = m,
            .ring = ring_core.View.fromMapping(m),
            .ctl = ctl,
            .peer = a.peer,
            .kind = a.admission.kind,
            .footprint_bytes = a.admission.footprint_bytes,
            .owner = owner,
            .node = node,
            .bindings = bindings,
            .last_active_ns = .init(now_ns),
            .admitted_ns = now_ns,
        };
        return self;
    }

    fn unmapAndFree(ctx: *anyopaque, _: layout.Mapping) void {
        const self: *Session = @ptrCast(@alignCast(ctx));
        shm.unmap(self.mapped);
        self.gpa.free(self.bindings);
        self.gpa.destroy(self);
    }

    pub fn header(self: *Session) *layout.SessionHeader {
        return self.m.header();
    }

    /// The heartbeat stamp (decision 0003 §F): one relaxed store.
    pub fn stampHeartbeat(self: *Session, now_ns: u64) void {
        self.header().heartbeat.store(now_ns, .monotonic);
    }

    /// POISON: the generation bump every client op observes (`release`
    /// pairs with the client's `acquire` on `poisoned()`), counted on the
    /// tripwire for a protocol violation, silent for the host's own
    /// shutdown broadcast.
    pub fn poison(self: *Session, reason: PoisonReason) void {
        if (self.poisoned.swap(true, .acq_rel)) return;
        _ = self.header().generation.fetchAdd(1, .release);
        if (reason.violation()) stats.ipc_sessions_poisoned.fire(@tagName(reason));
    }

    // ---- bindings ----

    /// A screened fd becomes a binding; null = the table is full (`budget`).
    pub fn bind(self: *Session, ok: rendezvous.ScreenOk, kill_priv: bool, durability: wire.Durability) ?u64 {
        const id = self.next_binding.fetchAdd(1, .monotonic);
        if (id > self.bindings.len) return null;
        const b = &self.bindings[@intCast(id - 1)];
        b.ino = ok.ino;
        b.rights = ok.rights;
        b.noatime = ok.noatime;
        b.kill_priv = kill_priv;
        b.durability = durability;
        // release pairs with `binding`'s acquire: the row's fields are
        // visible before it reads live.
        b.live.store(true, .release);
        stats.ipc_binds.inc();
        return id;
    }

    /// UNBIND: the row dies (a later op naming it is a descriptor reject);
    /// answers the ino it bound, null for an unknown or dead id. SeqCst:
    /// the row is dead before its dirty mark is read (the Dekker pair with
    /// a write's `inval.Row`).
    pub fn unbind(self: *Session, id: u64) ?u64 {
        if (id == 0 or id > self.bindings.len) return null;
        const b = &self.bindings[@intCast(id - 1)];
        if (!b.live.swap(false, .seq_cst)) return null;
        stats.ipc_unbinds.inc();
        return b.ino;
    }

    /// The live binding `id` names, or null (dead / never issued).
    pub fn binding(self: *Session, id: u64) ?*const Binding {
        if (id == 0 or id > self.bindings.len) return null;
        const b = &self.bindings[@intCast(id - 1)];
        if (!b.live.load(.acquire)) return null;
        return b;
    }

    /// The row `id` names, live or not; null for an id outside the table.
    pub fn row(self: *Session, id: u64) ?*Binding {
        if (id == 0 or id > self.bindings.len) return null;
        return &self.bindings[@intCast(id - 1)];
    }

    /// The rows issued so far (live or not).
    pub fn rows(self: *Session) []Binding {
        return self.bindings[0..@intCast(@min(self.next_binding.load(.monotonic) - 1, self.bindings.len))];
    }

    /// A ring write's view of its binding (`inval.Row`): the row's
    /// liveness and dirty mark, this session's teardown and sweep flag.
    /// `id` was screened at the op's dequeue.
    pub fn ringRow(self: *Session, id: u64) inval.Row {
        const r = self.row(id).?;
        return .{ .torn_down = &self.torn_down, .live = &r.live, .dirty = &r.dirty, .sweep = &self.sweep };
    }

    // ---- the in-flight ledger and the completion ----

    /// Before the serve. False = `live > slots`: a slot republished before
    /// its completion — the client does not own it (VAL-5c).
    pub fn beginInflight(self: *Session) bool {
        const live = self.inflight.fetchAdd(1, .monotonic) + 1;
        if (live > self.layout.geometry.slots) {
            _ = self.inflight.fetchSub(1, .monotonic);
            return false;
        }
        return true;
    }

    /// Served ops refresh the idle clock.
    pub fn touch(self: *Session, now_ns: u64) void {
        self.last_active_ns.store(now_ns, .monotonic);
    }
};

/// The completion handle: exactly one `complete` per dequeued op, in the
/// shipped order — (1) release the in-flight ledger FIRST (the slot goes
/// FREE the instant DONE lands; releasing after would make an honest
/// back-to-back resubmit look like an over-admission), (2) the result,
/// (3) DONE + the WAITER wake (breadth INT_MAX: a split submitter/reaper
/// pair may both park on one word), (4) the completion doorbell. Holds a
/// retained share of the mapping for its life.
pub const Completion = struct {
    session: *Session,
    slot_index: u32,
    /// `SQUEEZEFS_IPC_CQE_WAKE_LATCH`.
    latch: bool,

    pub fn complete(self: Completion, result: i64) void {
        const s = self.session;
        _ = s.inflight.fetchSub(1, .monotonic);
        const slot = s.m.slot(self.slot_index) catch unreachable;
        slot_core.completeAndWake(slot, result);
        _ = doorbell.completeAndWake(&s.header().cqe, self.latch);
        _ = s.map.releaseRef();
    }

    /// `-errno` as the client sees it.
    pub fn fail(self: Completion, e: linux.E) void {
        self.complete(-@as(i64, @backingInt(e)));
    }
};

comptime {
    if (builtin.os.tag != .linux) @compileError("ipc/host/session is Linux only (the memfd mapping); gate the import on builtin.os.tag");
}

// ---------------------------------------------------------------------------

const testing = std.testing;

/// A RAM session over a real sealed memfd (the test rig every host test
/// shares): no socket, no peer.
pub const TestSession = struct {
    s: *Session,

    pub fn init(gpa: Allocator, g: layout.Geometry) !TestSession {
        const l = try layout.SessionLayout.compute(g);
        const created = try shm.createSealed(l, false);
        _ = linux.close(created.fd);
        const a: struct { mapped: shm.Mapped, layout: layout.SessionLayout, peer: rendezvous.PeerCred, admission: struct { kind: wire.ClientKind, footprint_bytes: u64 } } = .{
            .mapped = created.mapped,
            .layout = l,
            .peer = .{ .pid = 1, .uid = 0, .gid = 0 },
            .admission = .{ .kind = .shim, .footprint_bytes = l.total_bytes },
        };
        return .{ .s = try Session.create(gpa, a, -1, 0, null, 0) };
    }

    /// The registry's share goes: the last accessor frees.
    pub fn deinit(self: *TestSession) void {
        _ = self.s.map.releaseRef();
    }
};

test "bindings: ids are dense from 1, a dead row is a descriptor reject, the table refuses past its bound; the ring row reads a row dead once unbound and every row dead once the session tears down; the row's open flags carry O_DIRECT and O_NOATIME as screened" {
    var ts = try TestSession.init(testing.allocator, .{ .ring_entries = 8, .slots = 4, .arena_bytes = 8 * layout.PAGE_BYTES, .max_op_bytes = layout.PAGE_BYTES });
    defer ts.deinit();
    const s = ts.s;
    const binds0 = stats.ipc_binds.load();
    const ok: rendezvous.ScreenOk = .{ .ino = 77, .rights = .{ .read = true, .write = true } };
    try testing.expectEqual(@as(?u64, 1), s.bind(ok, false, .mount));
    try testing.expectEqual(@as(?u64, 2), s.bind(.{ .ino = 78, .rights = .{ .read = true } }, true, .sync));
    try testing.expectEqual(binds0 + 2, stats.ipc_binds.load());
    try testing.expectEqual(@as(u64, 77), s.binding(1).?.ino);
    try testing.expect(s.binding(2).?.kill_priv and s.binding(2).?.durability == .sync);
    // The description's flags as the handler's arms read them.
    try testing.expectEqual(@as(u32, 0), s.binding(1).?.openFlags());
    try testing.expectEqual(@as(?u64, 3), s.bind(.{ .ino = 79, .rights = .{ .read = true, .odirect = true }, .noatime = true }, false, .mount));
    const o: linux.O = .{ .DIRECT = true, .NOATIME = true };
    try testing.expectEqual(@as(u32, @bitCast(o)), s.binding(3).?.openFlags());
    try testing.expectEqual(@as(?u64, 79), s.unbind(3));
    try testing.expect(s.binding(0) == null and s.binding(4) == null);
    try testing.expect(s.ringRow(2).alive());
    try testing.expectEqual(@as(?u64, 78), s.unbind(2));
    try testing.expect(s.unbind(2) == null);
    try testing.expect(s.binding(2) == null);
    try testing.expect(!s.ringRow(2).alive());
    try testing.expectEqual(@as(usize, 3), s.rows().len);
    try testing.expect(s.ringRow(1).alive());
    s.torn_down.store(true, .seq_cst);
    try testing.expect(!s.ringRow(1).alive());
    s.torn_down.store(false, .seq_cst);
    try testing.expectEqual(@as(u64, BINDINGS_PER_SESSION), s.bindings.len);
}

test "the in-flight ledger admits exactly `slots` and refuses the next; the completion releases it first, publishes the result, DONE and the doorbell, and drops its share" {
    var ts = try TestSession.init(testing.allocator, .{ .ring_entries = 8, .slots = 2, .arena_bytes = 8 * layout.PAGE_BYTES, .max_op_bytes = layout.PAGE_BYTES });
    defer ts.deinit();
    const s = ts.s;
    try testing.expect(s.beginInflight());
    try testing.expect(s.beginInflight());
    try testing.expect(!s.beginInflight());
    try testing.expectEqual(@as(u32, 2), s.inflight.load(.monotonic));
    const slot = s.m.slot(1) catch unreachable;
    const gen = slot_core.tryClaim(slot).?;
    slot_core.publishSubmitted(slot);
    try testing.expect(slot_core.tryBeginServe(slot));
    s.map.retain();
    const seq0 = s.header().cqe.seq.load(.monotonic);
    const c: Completion = .{ .session = s, .slot_index = 1, .latch = true };
    c.complete(4096);
    try testing.expectEqual(@as(u32, 1), s.inflight.load(.monotonic));
    try testing.expect(slot_core.isDoneFor(slot, gen));
    try testing.expectEqual(@as(i64, 4096), slot_core.result(slot));
    try testing.expectEqual(seq0 + 1, s.header().cqe.seq.load(.monotonic));
    try testing.expectEqual(@as(u32, 1), s.map.count());
    // Poison bumps the generation once, silently for the host's own shutdown.
    const gen0 = s.header().generation.load(.monotonic);
    s.poison(.host_shutdown);
    s.poison(.host_shutdown);
    try testing.expectEqual(gen0 + 1, s.header().generation.load(.monotonic));
}
