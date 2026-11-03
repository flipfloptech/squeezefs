//! The FENCING REGISTRANT (owner ruling 2026-11-03; decision 0003 §DG;
//! charter §5.5 "Fencing"): a write-free second NVMe host identity of the
//! manager's host, registered on the metadata namespace under the HOST's
//! key, whose only command is a member's death preempt. nvmet 7.3 answers a
//! reservation HOLDER's preempt of another key by re-typing its own hold
//! (`nvmet_pr_preempt`'s `holder == reg` arm, PRKEY never read); a NON-holder
//! registrant's preempt of a key that is not the holder's unregisters every
//! registrant of that key and keeps the reservation — nvmet's and the spec's
//! rule alike — so the manager's death preempts go through this identity on
//! every target.
//!
//! The key is its host's own: a successor's takeover preempt of the
//! holder's key, and a death preempt of a host's key, unregister the host and
//! its fencer in one command, so a fenced host can never fence anyone and the
//! fencer can never take the reservation (its only preempts name a victim's
//! key; a holder under that key took over by preempting ours, which removed
//! this registration with it). The own-key guard stays the caller's
//! (`fencing.preemptDeadRegistrant`): nvmet does not skip the sender, and a
//! preempt of our own key unregisters the holder — unreserved.
//!
//! The association: a connect of the metadata subsystem under the fencer's
//! identity (transport and address read off the metadata controller,
//! verified by the namespace uuid), shared by the host's processes through
//! the fabric registry (`<run>/hosts/fabric-<key>/<pid>`, reuse-or-connect
//! under its lock); its block node is never opened — the PR commands ride its
//! char node; the LAST user unregisters it and deletes the controller. Under
//! `nvme_core.multipath=Y` it is refused by name: the kernel joins every
//! identity of a subsystem under one head, whose path selector would carry
//! the host's block I/O and its own PR commands through the fencer's
//! controller, and makes no per-path char node to address it (`nvme_alloc_ns`
//! adds a namespace's char node only without a head). Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const pr = @import("pr.zig");
const nvme = @import("nvme_cmd.zig");
const proc = @import("proc.zig");
const pr_linux = @import("pr_linux.zig");
const host_fence = @import("host_fence.zig");
const fencing = @import("../sym/membership/fencing.zig");
const initiator = @import("../../ops/nvmet/initiator.zig");
const multipath = @import("../../ops/nvmet/multipath.zig");
const configfs = @import("../../ops/nvmet/configfs.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/fencer is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// The fencer's host identity: a uuid hostid and a hostnqn under the
/// product's fencer prefix (`pr.FENCER_HOSTNQN_PREFIX`: recognisable by the
/// refusal of a mount on its node and by the udev rule, review M3).
pub const Identity = struct {
    hostid_buf: [36]u8 = undefined,
    hostnqn_buf: [pr.FENCER_HOSTNQN_PREFIX.len + 36]u8 = undefined,

    pub fn hostid(self: *const Identity) []const u8 {
        return &self.hostid_buf;
    }

    pub fn hostnqn(self: *const Identity) []const u8 {
        return &self.hostnqn_buf;
    }
};

/// `uuid4(sha256("squeezefs:nvme-fencer-hostid:" ‖ wire host id ‖ 0 ‖ set
/// uuid)[0..16])`: the host as the namespace names it (FID 0x81, the input
/// of `host_fence.hostKey`) and the set — stable across remounts and
/// reboots, never the host's own (another hash domain), never written
/// anywhere.
pub fn deriveHostId(wire_host_id: []const u8, set_uuid: [16]u8) [16]u8 {
    var h = std.crypto.hash.sha2.Sha256.init(.{});
    h.update("squeezefs:nvme-fencer-hostid:");
    h.update(wire_host_id);
    h.update(&[_]u8{0});
    h.update(&set_uuid);
    var digest: [32]u8 = undefined;
    h.final(&digest);
    var out: [16]u8 = digest[0..16].*;
    out[6] = (out[6] & 0x0f) | 0x40;
    out[8] = (out[8] & 0x3f) | 0x80;
    return out;
}

pub fn identityOf(wire_host_id: []const u8, set_uuid: [16]u8) Identity {
    var id: Identity = .{};
    var w: Io.Writer = .fixed(&id.hostid_buf);
    configfs.writeUuid(&w, deriveHostId(wire_host_id, set_uuid)) catch unreachable;
    _ = std.fmt.bufPrint(&id.hostnqn_buf, "{s}{s}", .{ pr.FENCER_HOSTNQN_PREFIX, id.hostid() }) catch unreachable;
    return id;
}

/// Why no fencer stands (the no-fencer posture: the holder's own preempt —
/// device-grade on a spec-conformant target, §DF.11's detection on nvmet).
pub const Absent = enum {
    /// `nvme_core.multipath=Y`: a second identity joins the host's head.
    multipath_on,
    /// The metadata namespace is not a fabric controller's (PCIe, a file).
    not_fabric,
    /// A fabric transport the initiator does not connect (rdma, fc).
    transport,
    /// The namespace answers no host identifier (FID 0x81).
    no_wire_host_id,
    /// The derivation met the host's own hostid.
    same_identity,
    /// The namespace's uuid is unreadable: a connect could bind another disk.
    no_namespace_uuid,
    /// The host's registry of the association's users cannot be opened.
    users_unavailable,
    connect_failed,
    /// The fencer's namespace has no generic char node.
    char_node,
    register_failed,

    pub fn text(self: Absent) []const u8 {
        return switch (self) {
            .multipath_on => "nvme_core.multipath=Y joins a second host identity into the host's head (its paths would carry this host's block I/O and PR commands; no per-path char node addresses it) — boot nvme_core.multipath=N",
            .not_fabric => "the metadata namespace is not a fabric controller's",
            .transport => "the metadata controller's transport is neither tcp nor loop",
            .no_wire_host_id => "the namespace answers no host identifier (FID 0x81)",
            .same_identity => "the derived fencer identity equals the host's",
            .no_namespace_uuid => "the metadata namespace's uuid is unreadable",
            .users_unavailable => "the host's registry of the fencer association's users cannot be opened",
            .connect_failed => "the fencer's connect failed",
            .char_node => "the fencer's namespace has no generic char node",
            .register_failed => "the fencer's registration failed",
        };
    }
};

/// What the guard knows of its metadata namespace.
pub const Input = struct {
    /// The metadata block path (the log's name).
    path: []const u8,
    /// The guard's char node (`ngXnY`): its controller is the template.
    node: []const u8,
    /// The namespace's nsid: this process's user line names it (one process
    /// may stand fencers for two namespaces of one subsystem — review minor
    /// 2 of §DG).
    nsid: u32,
    wire_host_id: [16]u8,
    set_uuid: [16]u8,
    /// The host's key on the namespace (the guard client's).
    key: u64,
};

pub const Stood = union(enum) {
    stood: *Fencer,
    absent: Absent,
};

pub const Fencer = struct {
    gpa: Allocator,
    identity: Identity,
    client: *pr.Client,
    /// The association on a real namespace; null = a fixture's fencer over a
    /// fake face.
    assoc: ?Association,
    /// Orders recovery's check-then-preempt (`fencing.preemptDeadRegistrant`)
    /// against the heartbeat's re-register-then-check (review M1 of §DG).
    seq_lock: pr_linux.IssueLock = .{},

    const Association = struct {
        device: *pr_linux.Device,
        users: host_fence.Registry,
        subnqn_buf: [configfs.NVMF_NQN_SIZE]u8 = undefined,
        subnqn_len: usize = 0,
        instance: u32,
        reused: bool,
        nsid: u32,

        fn subnqn(self: *const Association) []const u8 {
            return self.subnqn_buf[0..self.subnqn_len];
        }
    };

    /// A fixture's fencer: the client over `face` under `key`, registered.
    pub fn forTest(gpa: Allocator, face: nvme.Namespace, key: u64) !*Fencer {
        if (!builtin.is_test) @compileError("Fencer.forTest is a fixture's");
        const c = try gpa.create(pr.Client);
        errdefer gpa.destroy(c);
        c.* = try pr.Client.init(gpa, face, 1, key);
        errdefer c.deinit();
        c.rtype = nvme.RTYPE_WERO;
        _ = try c.registerLadder();
        fencing.stats.pr_fencer_registrations.inc();
        const f = try gpa.create(Fencer);
        f.* = .{ .gpa = gpa, .identity = identityOf("fixture", @splat(0)), .client = c, .assoc = null };
        c.op_lock = f.seq_lock.opLock();
        return f;
    }

    /// After its registration: the host still holds (a report shows the
    /// holder's key under the HOST's id), else the registration is taken
    /// back — a successor that preempted the host's key before it landed
    /// left it alone under a superseded key (review M1 of §DG). False = not
    /// held (or unreadable): the fencer stands unregistered.
    pub fn confirmHeld(self: *Fencer, host: *pr.Client) bool {
        self.client.lockOps();
        defer self.client.unlockOps();
        return self.confirmHeldLocked(host);
    }

    fn confirmHeldLocked(self: *Fencer, host: *pr.Client) bool {
        if (host.holdsHere() catch false) return true;
        self.client.unregister(self.client.key) catch {};
        fencing.stats.pr_fencer_unheld_unregistered.inc();
        log.warn("fencing registrant {s}: the host does not hold the reservation after its registration (a successor preempted it) — unregistered", .{self.identity.hostnqn()});
        return false;
    }

    /// The registration again after a lapse forgot it (the manager's
    /// re-acquire, through the stale-sibling rule), then the host's hold
    /// confirmed — one section under the sequence lock. False = the host
    /// does not hold: the fencer stands unregistered.
    pub fn reregisterHeld(self: *Fencer, host: *pr.Client) bool {
        self.client.lockOps();
        defer self.client.unlockOps();
        self.reregister();
        return self.confirmHeldLocked(host);
    }

    fn reregister(self: *Fencer) void {
        _ = self.client.registerLadder() catch |e| {
            var buf: [128]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            self.client.writeError(&w, e) catch {};
            log.warn("fencing registrant {s}: its re-registration after the lapse failed ({s}) — a member's death preempt fails until the next mount (the full tail scan is the fence)", .{ self.identity.hostnqn(), w.buffered() });
            return;
        };
        fencing.stats.pr_fencer_registrations.inc();
    }

    /// Out of the association's users; the last unregisters the fencer and
    /// deletes its controller. Frees the fencer.
    pub fn release(self: *Fencer) void {
        const gpa = self.gpa;
        defer gpa.destroy(self);
        defer {
            self.client.deinit();
            gpa.destroy(self.client);
        }
        const a = &(self.assoc orelse {
            self.client.unregister(self.client.key) catch {};
            return;
        });
        defer a.device.deinit();
        a.users.lock() catch {
            log.err("fencing registrant {s}: the host's registry of its users ({s}) cannot be opened — its registration and controller are left standing (`squeezefs nvmeof disconnect --identity-only {s}` under its pair once no mount rides it)", .{ self.identity.hostnqn(), a.users.dir(), a.subnqn() });
            return;
        };
        defer a.users.unlock();
        const me = proc.localIdentity().pid;
        var lb: [24]u8 = undefined;
        const ours = a.users.removeLine(me, userLine(&lb, a.nsid)) catch true;
        const others = (a.users.othersAlive(gpa, me) catch 1) + @intFromBool(ours);
        if (others != 0) {
            log.info("fencing registrant {s}: {d} other process(es) of this host ride it — its registration and controller stay", .{ self.identity.hostnqn(), others });
            return;
        }
        // A takeover or a death preempt of this host's key removed it
        // already: the unregister conflicts, nothing to give back.
        self.client.unregister(self.client.key) catch {};
        const n = initiator.disconnect(gpa, .{ .subsysnqn = a.subnqn(), .identity = .{ .hostnqn = self.identity.hostnqn(), .hostid = self.identity.hostid() } }) catch |e| {
            log.err("fencing registrant {s}: the disconnect of {s} failed ({s}) — `squeezefs nvmeof disconnect --identity-only {s}` under its pair", .{ self.identity.hostnqn(), a.subnqn(), @errorName(e), a.subnqn() });
            return;
        };
        _ = initiator.awaitControllerGone(a.instance);
        log.info("fencing registrant {s}: unregistered, {d} controller(s) of {s} released", .{ self.identity.hostnqn(), n, a.subnqn() });
    }

    pub fn writeStatsJson(self: *const Fencer, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{{\"hostnqn\":\"{s}\",\"hostid\":\"{s}\",\"key\":\"{x:0>16}\",\"preempts_issued\":{d}", .{ self.identity.hostnqn(), self.identity.hostid(), self.client.key, self.client.stats.preempts });
        if (self.assoc) |*a| try w.print(",\"subnqn\":\"{s}\",\"controller\":{d},\"reused\":{}", .{ a.subnqn(), a.instance, a.reused });
        try w.writeByte('}');
    }
};

/// Stand the fencer of the metadata namespace `in` names: the identity, the
/// association (reused or connected), the client on its char node, the
/// registration under the host's key. Never fails the mount: an absent
/// fencer is the holder's-preempt posture, said by the caller.
pub fn stand(gpa: Allocator, env: *const std.process.Environ.Map, in: Input) Stood {
    const f = standOrRefuse(gpa, env, in) catch |e| return .{ .absent = switch (e) {
        error.MultipathOn => .multipath_on,
        error.NotFabric => .not_fabric,
        error.Transport => .transport,
        error.SameIdentity => .same_identity,
        error.NoNamespaceUuid => .no_namespace_uuid,
        error.UsersUnavailable => .users_unavailable,
        error.ConnectFailed => .connect_failed,
        error.CharNode => .char_node,
        error.RegisterFailed, error.OutOfMemory => .register_failed,
    } };
    return .{ .stood = f };
}

const StandError = error{ MultipathOn, NotFabric, Transport, SameIdentity, NoNamespaceUuid, UsersUnavailable, ConnectFailed, CharNode, RegisterFailed, OutOfMemory };

fn standOrRefuse(gpa: Allocator, env: *const std.process.Environ.Map, in: Input) StandError!*Fencer {
    if (multipath.readMultipathParam() == .on) return error.MultipathOn;
    var cb: [32]u8 = undefined;
    const ctrl_name = pr_linux.controllerName(in.node, &cb) orelse return error.NotFabric;
    const ctrl = initiator.readController(initiator.SYSFS_ROOT, ctrl_name) orelse return error.NotFabric;
    if (!ctrl.isFabric()) return error.NotFabric;
    const trtype: configfs.Trtype = if (std.mem.eql(u8, ctrl.transportOf(), "tcp")) .tcp else if (std.mem.eql(u8, ctrl.transportOf(), "loop")) .loop else return error.Transport;
    const id = identityOf(&in.wire_host_id, in.set_uuid);
    if (std.ascii.eqlIgnoreCase(id.hostid(), ctrl.hostidOf())) return error.SameIdentity;
    var bb: [32]u8 = undefined;
    const block = std.fmt.bufPrint(&bb, "nvme{s}", .{in.node["ng".len..]}) catch return error.NotFabric;
    var ub: [64]u8 = undefined;
    const want_uuid = configfs.parseUuidAttr(initiator.readSysfs(&ub, "{s}/class/block/{s}/uuid", .{ initiator.SYSFS_ROOT, block }) orelse "") orelse return error.NoNamespaceUuid;
    const subnqn = ctrl.subsysnqnOf();
    const parts = multipath.addressParts(ctrl.addressOf());
    const spec: initiator.ConnectSpec = .{
        .trtype = trtype,
        .traddr = parts.traddr,
        .trsvcid = std.fmt.parseInt(u16, parts.trsvcid, 10) catch configfs.DEFAULT_TRSVCID, // the controller's address word, decimal as sysfs prints it
        .subnqn = subnqn,
        .hostnqn = id.hostnqn(),
        .hostid = id.hostid(),
        .timeouts = if (trtype == .tcp) multipath.resolveConnectTimeouts(env) else null,
    };
    // The reuse-or-connect and this process's entry as ONE step under the
    // users' lock: a sibling's release never deletes a controller this
    // process is about to ride.
    var users = host_fence.Registry.runtime(.fabric, host_fence.fabricRegistryKey(subnqn, id.hostnqn(), id.hostid()));
    users.lock() catch return error.UsersUnavailable;
    defer users.unlock();
    const me = proc.localIdentity();
    var lb: [24]u8 = undefined;
    const line = userLine(&lb, in.nsid);
    users.append(me.pid, me.start_ticks, line) catch return error.UsersUnavailable;
    var reused = true;
    const got = findOurs(gpa, spec, want_uuid) orelse blk: {
        reused = false;
        break :blk initiator.connect(gpa, spec, want_uuid) catch |e| {
            var eb: [1024]u8 = undefined;
            var ew: Io.Writer = .fixed(&eb);
            initiator.writeConnectRefusal(&ew, e, spec) catch {};
            log.warn("meta volume {s}: the fencing registrant's connect of {s} under {s} failed: {s}", .{ in.path, subnqn, id.hostnqn(), ew.buffered() });
            _ = users.removeLine(me.pid, line) catch {};
            return error.ConnectFailed;
        };
    };
    errdefer giveBack(gpa, &users, me.pid, line, got.instance);
    var nb: [40]u8 = undefined;
    const dev = switch (pr_linux.resolve(gpa, got.ns.blockPath(&nb))) {
        .device => |d| d,
        .none => return error.CharNode,
    };
    errdefer dev.deinit();
    const c = try gpa.create(pr.Client);
    errdefer gpa.destroy(c);
    c.* = try pr.Client.init(gpa, dev.face(), dev.nsid, in.key);
    errdefer c.deinit();
    c.rtype = nvme.RTYPE_WERO;
    registerUnder(c) catch |e| {
        var eb: [128]u8 = undefined;
        var ew: Io.Writer = .fixed(&eb);
        c.writeError(&ew, e) catch {};
        log.warn("meta volume {s}: the fencing registrant {s} could not register key {x:0>16} on {s} ({s})", .{ in.path, id.hostnqn(), in.key, dev.nodeName(), ew.buffered() });
        return error.RegisterFailed;
    };
    const f = try gpa.create(Fencer);
    f.* = .{ .gpa = gpa, .identity = id, .client = c, .assoc = .{ .device = dev, .users = users, .instance = got.instance, .reused = reused, .nsid = in.nsid } };
    c.op_lock = f.seq_lock.opLock();
    const a = &f.assoc.?;
    @memcpy(a.subnqn_buf[0..subnqn.len], subnqn);
    a.subnqn_len = subnqn.len;
    // The lock is this frame's copy's: the fencer's copy holds none.
    a.users.lock_fd = -1;
    fencing.stats.pr_fencer_registrations.inc();
    log.info("meta volume {s}: fencing registrant stood — hostnqn {s} hostid {s} on nvme{d} ({s}, {s}) under the host's key {x:0>16}: a member's death preempt is this non-holder's (decision 0003 §DG)", .{ in.path, id.hostnqn(), id.hostid(), got.instance, dev.nodeName(), if (reused) "reused" else "connected", in.key });
    return f;
}

/// RESCAP decides the register shape; the register ladder under the key.
fn registerUnder(c: *pr.Client) pr.Error!void {
    _ = try c.probeRescap();
    _ = try c.registerLadder();
}

/// This process's user line for the fencer of namespace `nsid`.
fn userLine(buf: *[24]u8, nsid: u32) []const u8 {
    return std.fmt.bufPrint(buf, "f {d}\n", .{nsid}) catch unreachable;
}

/// A refused stand's give-back: out of the users; the last deletes the
/// controller it reused or connected.
fn giveBack(gpa: Allocator, users: *host_fence.Registry, pid: u32, line: []const u8, instance: u32) void {
    const ours = users.removeLine(pid, line) catch true;
    if ((users.othersAlive(gpa, pid) catch 1) == 0 and !ours) {
        initiator.deleteController(instance) catch {};
        _ = initiator.awaitControllerGone(instance);
    }
}

/// A live controller of the spec's subsystem under the fencer's identity on
/// its address, its namespace the expected one (a sibling's association, or
/// a killed predecessor's).
fn findOurs(gpa: Allocator, spec: initiator.ConnectSpec, want_uuid: [16]u8) ?initiator.Connected {
    const ctrls = initiator.listControllers(gpa, initiator.SYSFS_ROOT) catch return null;
    defer gpa.free(ctrls);
    for (ctrls) |c| {
        if (!std.mem.eql(u8, c.subsysnqnOf(), spec.subnqn) or !c.hasIdentity(spec.hostnqn, spec.hostid) or !c.isLive()) continue;
        if (!std.mem.eql(u8, c.transportOf(), spec.trtype.name())) continue;
        if (spec.trtype == .tcp) {
            const a = multipath.addressParts(c.addressOf());
            var sb: [8]u8 = undefined;
            const svc = std.fmt.bufPrint(&sb, "{d}", .{spec.trsvcid}) catch unreachable;
            if (!std.mem.eql(u8, a.traddr, spec.traddr) or !std.mem.eql(u8, a.trsvcid, svc)) continue;
        }
        const ns = (initiator.controllerNamespace(gpa, initiator.SYSFS_ROOT, c.instance) catch null) orelse continue;
        if (!ns.char_present) continue;
        const u = ns.uuid orelse continue;
        if (!std.mem.eql(u8, &u, &want_uuid)) continue;
        return .{ .instance = c.instance, .cntlid = 0, .ns = ns };
    }
    return null;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the fencer's identity: a uuid4 hostid (version 4, variant 10) and a hostnqn under the fencer prefix, derived from the wire host id and the set — the same on every derivation, another for another host or another set, never the host's own" {
    const wire_a: [16]u8 = "wire-host-id-a01".*;
    const wire_b: [16]u8 = "wire-host-id-b02".*;
    const set_1: [16]u8 = @splat(1);
    const set_2: [16]u8 = @splat(2);
    const a1 = identityOf(&wire_a, set_1);
    try testing.expectEqualStrings(a1.hostid(), identityOf(&wire_a, set_1).hostid());
    try testing.expect(!std.mem.eql(u8, a1.hostid(), identityOf(&wire_b, set_1).hostid()));
    try testing.expect(!std.mem.eql(u8, a1.hostid(), identityOf(&wire_a, set_2).hostid()));
    // The derivation restated: sha256 of the domain, the wire id, 0, the set.
    var h = std.crypto.hash.sha2.Sha256.init(.{});
    h.update("squeezefs:nvme-fencer-hostid:");
    h.update(&wire_a);
    h.update(&[_]u8{0});
    h.update(&set_1);
    var d: [32]u8 = undefined;
    h.final(&d);
    const id = deriveHostId(&wire_a, set_1);
    try testing.expectEqualSlices(u8, d[0..6], id[0..6]);
    try testing.expectEqual(@as(u8, 0x40), id[6] & 0xf0);
    try testing.expectEqual(@as(u8, 0x80), id[8] & 0xc0);
    try testing.expect(proc.parseUuid(a1.hostid()) != null);
    try testing.expect(std.mem.startsWith(u8, a1.hostnqn(), pr.FENCER_HOSTNQN_PREFIX));
    try testing.expectEqualStrings(a1.hostid(), a1.hostnqn()[pr.FENCER_HOSTNQN_PREFIX.len..]);
    try testing.expect(a1.hostnqn().len <= 223); // NVMF_NQN_SIZE − 1
    // The host identity's own derivation (machine id) is another domain.
    const host = initiator.deriveHostId(&wire_a);
    try testing.expect(!std.mem.eql(u8, &host, &id));
}

test "the fencer refuses a non-fabric namespace by name (its char node names no fabric controller): the holder's-preempt posture" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const st = stand(testing.allocator, &env, .{ .path = "/nonexistent", .node = "ng999999n1", .nsid = 1, .wire_host_id = @splat(7), .set_uuid = @splat(9), .key = 0x55 });
    switch (st) {
        .stood => |f| {
            f.release();
            return error.TestUnexpectedResult;
        },
        .absent => |why| try testing.expect(why == .not_fabric or why == .multipath_on),
    }
    try testing.expect(std.mem.indexOf(u8, Absent.multipath_on.text(), "nvme_core.multipath=N") != null);
}
