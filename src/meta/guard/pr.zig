//! Layer B1 of the D0 guard: the NVMe Persistent Reservation primitive
//! (charter §5.5 "Fencing", §5.7, §8; spec `guard_claims.md` §1). Portable
//! core over the `nvme_cmd.Namespace` face: the real `/dev/ngXnY` through
//! `IORING_OP_URING_CMD` (`pr_linux.zig`) or the fake (`fake_nvme.zig`).
//! Shared with rung 6 (WERO, registrants, the cap gate): the client methods
//! ship here, inert on a flat volume.
//!
//! Procedures (§1.3): `register` (quirk Q11: the shape is decided by RESCAP
//! — CPTPL iff PTPL-capable — and the plain shape is retried only on a
//! device-answered Invalid Field, never on an errno); the REGISTER LADDER
//! (plain register → on conflict read the wire host id (FID 0x81) and the
//! report → unregister only OWN stale keys → register fresh; every other
//! arm fails CLOSED); `acquire` WE rtype 1; `preempt`; `release` +
//! `unregister` (zero residue); `report` in two steps sized by REGCTL
//! (EDS = 1 first, the short form on any non-conflict error); the registrant
//! cap learned ONLY from a repeated device-answered command-/vendor-specific
//! status.

const std = @import("std");
const Allocator = std.mem.Allocator;
const nvme = @import("nvme_cmd.zig");
const errors = @import("../../core/errors.zig");
const knobs = @import("../../core/knobs.zig");
const log = @import("../../core/log.zig");
const checksum = @import("../../core/checksum.zig");

pub const Status = nvme.Status;

pub const Error = error{
    /// The device answered Reservation Conflict (SC 0x83).
    ReservationConflict,
    /// Any other non-success status (`Client.last_status` names it).
    DeviceStatus,
    /// The transport failed (`Client.last_errno`).
    Transport,
    /// FID 0x81 answered zeros or failed: fail closed (never fall back to
    /// `/etc/nvme`, R:1065-1069).
    HostIdUnknown,
    /// The acquire conflicted but the report names no holder: refusing to
    /// arbitrate blind.
    NoHolder,
    /// The conflict is a foreign registration — not ours to remove.
    ForeignRegistration,
    OutOfMemory,
};

/// One page-aligned arena for the guard's life (quirk Q12): Identify
/// 4 KiB + 4 KiB for the report's header and body; a report past the arena
/// half (≥ 63 extended registrants) borrows from the gpa.
pub const ARENA_LEN: usize = 8192;
const IDENTIFY_OFF: usize = 0;
const REPORT_OFF: usize = nvme.IDENTIFY_NS_LEN;
const REPORT_ARENA_LEN: usize = ARENA_LEN - REPORT_OFF;

pub const Stats = struct {
    registers: u64 = 0,
    unregisters: u64 = 0,
    acquires: u64 = 0,
    preempts: u64 = 0,
    releases: u64 = 0,
    reports: u64 = 0,
    /// A register ladder that recovered a crashed incarnation's registration.
    own_stale_recoveries: u64 = 0,
};

/// The registrant cap learn rule (R:376-420, charter §5.5): a cap is
/// LEARNED only when a device-answered command-/vendor-specific status is
/// repeated at the same REGCTL; a later register success at or above the
/// learned count UNLEARNS it (the device was not at its cap after all).
pub const CapLearner = struct {
    candidate: ?u16 = null,
    learned: ?u16 = null,
    refusals: u64 = 0,

    pub fn learn(self: *CapLearner, status: Status, regctl: u16) void {
        if (!nvme.isCapLearnable(status)) return;
        if (self.candidate) |c| if (c == regctl) {
            self.learned = regctl;
            return;
        };
        self.candidate = regctl;
    }

    pub fn unlearnOnSuccess(self: *CapLearner, regctl: u16) void {
        if (self.learned) |l| if (regctl >= l) {
            self.learned = null;
            self.candidate = null;
        };
    }

    /// `pr_registrant_cap`: declared (rung 6's knob), else learned, else 0.
    pub fn effective(self: CapLearner, declared: ?u16) u16 {
        return declared orelse self.learned orelse 0;
    }
};

pub const LadderOutcome = union(enum) {
    registered,
    /// A crashed incarnation's registration(s) under OUR wire host id were
    /// unregistered before the fresh register succeeded.
    recovered_own_stale: u32,
};

pub const Client = struct {
    gpa: Allocator,
    ns: nvme.Namespace,
    nsid: u32,
    /// Our registration key (`claim.prKey`).
    key: u64,
    rtype: u8 = nvme.RTYPE_WRITE_EXCLUSIVE,
    /// Identify NS byte 31 (0 until probed).
    rescap: u8 = 0,
    /// The register shape the device accepted: CPTPL requested iff the
    /// namespace is PTPL-capable (`pr_ptpl_requested`).
    ptpl_requested: bool = false,
    /// The last non-success status / transport errno (the log line's detail).
    last_status: Status = 0,
    last_errno: ?errors.Error = null,
    /// `pr_registrants_per_namespace` / `pr_report_bytes`: the last report.
    regctl: u16 = 0,
    report_bytes: u64 = 0,
    /// The last report's PTPLS byte (the mount log).
    ptpls: u8 = 0,
    cap: CapLearner = .{},
    stats: Stats = .{},
    arena: []align(4096) u8,

    pub fn init(gpa: Allocator, ns: nvme.Namespace, nsid: u32, key: u64) Allocator.Error!Client {
        const arena = try gpa.alignedAlloc(u8, .fromByteUnits(4096), ARENA_LEN);
        return .{ .gpa = gpa, .ns = ns, .nsid = nsid, .key = key, .arena = arena };
    }

    pub fn deinit(self: *Client) void {
        self.gpa.free(self.arena);
        self.* = undefined;
    }

    fn identifyBuf(self: *Client) []u8 {
        return self.arena[IDENTIFY_OFF..][0..nvme.IDENTIFY_NS_LEN];
    }

    /// The raw answer: a transport error becomes `Transport`, a status
    /// comes back for the caller to classify.
    fn issueRaw(self: *Client, queue: nvme.Queue, cmd: *const nvme.UringCmd) Error!Status {
        const c = self.ns.issue(self.ns.ctx, queue, cmd) catch |e| {
            self.last_errno = e;
            return error.Transport;
        };
        return c.status;
    }

    /// Success or a typed error (conflict / status).
    fn issue(self: *Client, queue: nvme.Queue, cmd: *const nvme.UringCmd) Error!void {
        const s = try self.issueRaw(queue, cmd);
        try self.classify(s);
    }

    fn classify(self: *Client, s: Status) Error!void {
        if (s == nvme.STATUS_SUCCESS) return;
        self.last_status = s;
        if (nvme.isConflict(s)) return error.ReservationConflict;
        return error.DeviceStatus;
    }

    /// Identify Namespace → RESCAP (byte 31); decides the register shape.
    pub fn probeRescap(self: *Client) Error!u8 {
        const buf = self.identifyBuf();
        @memset(buf, 0);
        try self.issue(.admin, &nvme.identifyNamespace(self.nsid, buf));
        self.rescap = buf[nvme.RESCAP_OFFSET];
        self.ptpl_requested = self.rescap & nvme.RESCAP_PTPL != 0;
        return self.rescap;
    }

    /// Register `key` (R:1091-1112 reshaped by quirk Q11): `IEKEY |
    /// CPTPL(iff PTPL-capable)`; a device-answered Invalid Field retries the
    /// plain shape once (targets vary on IEKEY/CPTPL support — the shape is
    /// then recorded); a conflict and any errno pass through untouched.
    /// Idempotent for an already-registered same key.
    pub fn register(self: *Client) Error!void {
        var payload: [16]u8 = undefined;
        const cptpl: u32 = if (self.ptpl_requested) nvme.CPTPL_PERSIST else 0;
        self.stats.registers += 1;
        const s = try self.issueRaw(.io, &nvme.reservationRegister(self.nsid, nvme.RREGA_REGISTER | nvme.IEKEY | cptpl, 0, self.key, &payload));
        if (s == nvme.SC_INVALID_FIELD) {
            self.ptpl_requested = false;
            return self.issue(.io, &nvme.reservationRegister(self.nsid, nvme.RREGA_REGISTER, 0, self.key, &payload));
        }
        return self.classify(s);
    }

    /// `RREGA 1, crkey = stale`: device-validated against THIS host's
    /// registration (a foreign key conflicts — the ladder never reaches
    /// here with one).
    pub fn unregister(self: *Client, stale: u64) Error!void {
        var payload: [16]u8 = undefined;
        self.stats.unregisters += 1;
        return self.issue(.io, &nvme.reservationRegister(self.nsid, nvme.RREGA_UNREGISTER, stale, 0, &payload));
    }

    /// The register ladder (R:600-706; charter §8).
    pub fn registerLadder(self: *Client) Error!LadderOutcome {
        self.register() catch |e| switch (e) {
            error.ReservationConflict => return self.recoverOwnStale(),
            error.DeviceStatus => {
                // A device-answered command-/vendor-specific status may be
                // the registrant cap: read the report and feed the learner.
                const status = self.last_status;
                if (nvme.isCapLearnable(status)) {
                    if (self.report()) |*r| {
                        var rep = r.*;
                        defer rep.deinit(self.gpa);
                        self.cap.learn(status, rep.regctl);
                    } else |_| {}
                }
                self.last_status = status;
                return e;
            },
            else => return e,
        };
        if (self.cap.learned != null) {
            if (self.report()) |*r| {
                var rep = r.*;
                defer rep.deinit(self.gpa);
                self.cap.unlearnOnSuccess(rep.regctl);
            } else |_| {}
        }
        return .registered;
    }

    /// Steps 3-5 of the ladder: our wire host id (fail closed when unknown),
    /// the report, OUR stale keys only, unregister each, register fresh.
    fn recoverOwnStale(self: *Client) Error!LadderOutcome {
        const conflict = self.last_status;
        const me = self.wireHostId() catch |e| {
            self.last_status = conflict;
            return if (e == error.HostIdUnknown) error.HostIdUnknown else error.ReservationConflict;
        };
        var rep = self.report() catch {
            self.last_status = conflict;
            return error.ReservationConflict;
        };
        defer rep.deinit(self.gpa);
        var stale: u32 = 0;
        for (rep.registrants) |r| {
            if (!std.mem.eql(u8, &r.host_id, &me) or r.rkey == self.key) continue;
            self.unregister(r.rkey) catch {
                self.last_status = conflict;
                return error.ReservationConflict;
            };
            stale += 1;
        }
        if (stale == 0) {
            self.last_status = conflict;
            return error.ForeignRegistration;
        }
        try self.register();
        self.stats.own_stale_recoveries += 1;
        return .{ .recovered_own_stale = stale };
    }

    /// `RACQA 0` under our rtype; a conflict is the caller's arbitration.
    pub fn acquire(self: *Client) Error!void {
        var payload: [16]u8 = undefined;
        self.stats.acquires += 1;
        return self.issue(.io, &nvme.reservationAcquire(self.nsid, nvme.RACQA_ACQUIRE, self.rtype, self.key, 0, &payload));
    }

    /// `RACQA 1, prkey = victim` under our rtype (the recovery ladder's
    /// device-fenced takeover).
    pub fn preempt(self: *Client, victim: u64) Error!void {
        var payload: [16]u8 = undefined;
        self.stats.preempts += 1;
        return self.issue(.io, &nvme.reservationAcquire(self.nsid, nvme.RACQA_PREEMPT, self.rtype, self.key, victim, &payload));
    }

    /// `RACQA 2` (rung 6's dead-registrant arm; nvmet runs it as a plain
    /// preempt).
    pub fn preemptAndAbort(self: *Client, victim: u64) Error!void {
        var payload: [16]u8 = undefined;
        self.stats.preempts += 1;
        return self.issue(.io, &nvme.reservationAcquire(self.nsid, nvme.RACQA_PREEMPT_AND_ABORT, self.rtype, self.key, victim, &payload));
    }

    /// Release under the HELD rtype, then unregister: NVMe release does not
    /// unregister, and a stale registration makes the next fresh-key
    /// register conflict (R:1004-1022). Best-effort: the first failure is
    /// returned after both were attempted.
    pub fn release(self: *Client) Error!void {
        var payload: [8]u8 = undefined;
        self.stats.releases += 1;
        const rel = self.issue(.io, &nvme.reservationRelease(self.nsid, self.rtype, self.key, &payload));
        const unreg = self.unregister(self.key);
        try rel;
        try unreg;
    }

    /// FID 0x81: 16 B (EXHID = 1) first, 8 B on a device status (the PCIe
    /// 64-bit form); all-zero ⇒ unknown.
    pub fn wireHostId(self: *Client) Error![16]u8 {
        var out: [16]u8 = @splat(0);
        const s = try self.issueRaw(.admin, &nvme.getHostIdentifier(&out));
        if (s != nvme.STATUS_SUCCESS) {
            if (nvme.isConflict(s)) {
                self.last_status = s;
                return error.ReservationConflict;
            }
            var short: [8]u8 = @splat(0);
            try self.issue(.admin, &nvme.getHostIdentifier(&short));
            out = @splat(0);
            @memcpy(out[0..8], &short);
        }
        if (std.mem.allEqual(u8, &out, 0)) return error.HostIdUnknown;
        return out;
    }

    /// The report in two steps sized by REGCTL (R:968-982): the header
    /// alone, then `header + stride × regctl`; a grown count re-reads once.
    /// EDS = 1 first; any non-conflict error falls back to the short form.
    pub fn report(self: *Client) Error!nvme.Report {
        self.stats.reports += 1;
        return self.reportWith(true) catch |e| switch (e) {
            error.ReservationConflict, error.Transport, error.OutOfMemory => e,
            else => self.reportWith(false),
        };
    }

    fn reportWith(self: *Client, extended: bool) Error!nvme.Report {
        const h = nvme.reportHeaderLen(extended);
        const head = self.arena[REPORT_OFF..][0..h];
        try self.issue(.io, &nvme.reservationReport(self.nsid, head, extended));
        var regctl = nvme.reportRegctl(head, extended) orelse 0;
        var attempt: u8 = 0;
        while (true) : (attempt += 1) {
            const len = nvme.reportLen(extended, regctl);
            const borrowed = len > REPORT_ARENA_LEN;
            const buf: []u8 = if (borrowed) try self.gpa.alloc(u8, len) else self.arena[REPORT_OFF..][0..len];
            defer if (borrowed) self.gpa.free(buf);
            try self.issue(.io, &nvme.reservationReport(self.nsid, buf, extended));
            const seen = nvme.reportRegctl(buf, extended) orelse 0;
            if (seen > regctl and attempt == 0) {
                regctl = seen;
                continue;
            }
            self.report_bytes = len;
            self.regctl = seen;
            self.ptpls = buf[nvme.PTPLS_OFFSET];
            return nvme.parseReport(self.gpa, buf, extended) catch |e| switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Corrupt => error.DeviceStatus,
            };
        }
    }

    /// The report's holder key (null = no reservation).
    pub fn holder(self: *Client) Error!?u64 {
        var rep = try self.report();
        defer rep.deinit(self.gpa);
        return rep.holder();
    }

    /// Rung 6's `registrant_identity_shared` probe: another rkey under OUR
    /// wire host id (two mounts on one registrant — fencing between them is
    /// process-local).
    pub fn identityShared(self: *Client) Error!bool {
        const me = try self.wireHostId();
        var rep = try self.report();
        defer rep.deinit(self.gpa);
        for (rep.registrants) |r| if (std.mem.eql(u8, &r.host_id, &me) and r.rkey != self.key) return true;
        return false;
    }

    pub fn writeStatsJson(self: *const Client, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{{\"pr_key\":\"{x:0>16}\",\"pr_rtype\":{d},\"pr_rescap\":{d},\"pr_ptpl_requested\":{},\"pr_registrants_per_namespace\":{d},\"pr_report_bytes\":{d},\"pr_registrant_cap\":{d},\"pr_registrant_cap_refusals\":{d},\"pr_registers\":{d},\"pr_unregisters\":{d},\"pr_acquires\":{d},\"pr_preempts\":{d},\"pr_releases\":{d},\"pr_reports\":{d},\"pr_own_stale_recoveries\":{d}}}", .{ self.key, self.rtype, self.rescap, self.ptpl_requested, self.regctl, self.report_bytes, self.cap.effective(null), self.cap.refusals, self.stats.registers, self.stats.unregisters, self.stats.acquires, self.stats.preempts, self.stats.releases, self.stats.reports, self.stats.own_stale_recoveries });
    }

    /// The error's detail for a log line.
    pub fn writeError(self: *const Client, w: *std.Io.Writer, e: Error) std.Io.Writer.Error!void {
        switch (e) {
            error.Transport => try w.print("transport error {s}", .{if (self.last_errno) |le| @errorName(le) else "unknown"}),
            error.DeviceStatus => try w.print("device status 0x{x:0>3}", .{self.last_status}),
            else => try w.writeAll(@errorName(e)),
        }
    }
};

// ---------------------------------------------------------------------------
// Posture (§1.1): never fails the mount.
// ---------------------------------------------------------------------------

pub const NoPrReason = enum {
    /// A regular file, a loop device, a non-NVMe node: one kernel, the flock
    /// IS the fence (`single_kernel_substrate`).
    not_nvme_namespace,
    /// An NVMe namespace without its `/dev/ngXnY` twin (a kernel without the
    /// generic char nodes — a stock-kernel degradation, §1.4).
    no_char_node,
    rescap_zero,
    probe_error,

    pub fn mode(self: NoPrReason) []const u8 {
        _ = self;
        return "flock+claim";
    }
};

pub const Posture = union(enum) {
    none: NoPrReason,
    /// RESCAP ≠ 0: the client is live for the guard's life.
    active: *Client,

    /// `writer_guard_mode` (§2.8).
    pub fn mode(self: Posture) []const u8 {
        return switch (self) {
            .none => |r| r.mode(),
            .active => "flock+pr",
        };
    }
};

/// Probe RESCAP on an opened client: 0 ⇒ detection grade (the client is
/// freed), an error ⇒ the same at debug level.
pub fn resolvePosture(gpa: Allocator, client: *Client, path: []const u8) Posture {
    const rescap = client.probeRescap() catch |e| {
        var buf: [128]u8 = undefined;
        var w: std.Io.Writer = .fixed(&buf);
        client.writeError(&w, e) catch {};
        log.debug("meta volume {s}: RESCAP probe failed ({s}) — single-writer guard degrades to detection grade (flock+claim)", .{ path, w.buffered() });
        client.deinit();
        gpa.destroy(client);
        return .{ .none = .probe_error };
    };
    if (rescap == 0) {
        log.info("meta volume {s}: namespace advertises no reservation support (RESCAP=0) — single-writer guard degrades to detection grade (flock+claim)", .{path});
        client.deinit();
        gpa.destroy(client);
        return .{ .none = .rescap_zero };
    }
    return .{ .active = client };
}

// ---------------------------------------------------------------------------
// Host identity (§1.4): the configured pair, pair-or-neither.
// ---------------------------------------------------------------------------

pub const HostIdentityRefusal = enum {
    nqn_without_id,
    id_without_nqn,

    pub fn write(self: HostIdentityRefusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .nqn_without_id => try w.writeAll("per-mount host identity is PAIR-OR-NEITHER (KD-MW-3): hostnqn is configured (SQUEEZEFS_HOSTNQN / -o hostnqn=) but hostid is not — set SQUEEZEFS_HOSTID (or -o hostid=) too, or unset both. A hostnqn presented under the host's default hostid is how registrants alias on the device"),
            .id_without_nqn => try w.writeAll("per-mount host identity is PAIR-OR-NEITHER (KD-MW-3): hostid is configured (SQUEEZEFS_HOSTID / -o hostid=) but hostnqn is not — set SQUEEZEFS_HOSTNQN (or -o hostnqn=) too, or unset both. A hostid presented under the host's default hostnqn is how registrants alias on the device"),
        }
    }
};

/// The explicit pair as the knobs/options name it.
pub const ExplicitIdentity = union(enum) {
    none,
    pair: struct { hostnqn: []const u8, hostid: []const u8 },
    refusal: HostIdentityRefusal,
};

/// The mount options' `-o hostnqn=` / `-o hostid=` (option > env).
pub const IdentityOptions = struct { hostnqn: ?[]const u8 = null, hostid: ?[]const u8 = null };

/// Pure: `explicit_host_identity_from` (INIT:124-158). Exactly one
/// configured REFUSES.
pub fn resolveExplicitIdentity(env: *const std.process.Environ.Map, opts: IdentityOptions) ExplicitIdentity {
    const nqn = knobs.present(opts.hostnqn) orelse knobs.present(env.get("SQUEEZEFS_HOSTNQN"));
    const id = knobs.present(opts.hostid) orelse knobs.present(env.get("SQUEEZEFS_HOSTID"));
    if (nqn == null and id == null) return .none;
    if (nqn == null) return .{ .refusal = .id_without_nqn };
    if (id == null) return .{ .refusal = .nqn_without_id };
    return .{ .pair = .{ .hostnqn = nqn.?, .hostid = id.? } };
}

/// The identity recorded at the gate and re-read every beat: hashes of
/// the two configured strings (an absent default reads as the hash of
/// the empty string, so "still absent" compares equal — quirk Q7: nothing
/// is ever minted or written to `/etc/nvme`).
pub const IdentityStamp = struct {
    nqn_hash: u64,
    id_hash: u64,

    pub fn of(hostnqn: []const u8, hostid: []const u8) IdentityStamp {
        return .{ .nqn_hash = checksum.hash64(hostnqn), .id_hash = checksum.hash64(hostid) };
    }

    pub fn eql(a: IdentityStamp, b: IdentityStamp) bool {
        return a.nqn_hash == b.nqn_hash and a.id_hash == b.id_hash;
    }
};

// ---------------------------------------------------------------------------
// Tests (the register-ladder and PR contracts of T:777-1296 on the fake).
// ---------------------------------------------------------------------------

const testing = std.testing;
const fake = @import("fake_nvme.zig");

const host_a: [16]u8 = "host-a-host-a-01".*;
const host_b: [16]u8 = "host-b-host-b-02".*;

const Rig = struct {
    ns: fake.Namespace,
    a: fake.Host,
    b: fake.Host,

    fn init(gpa: Allocator) Rig {
        return .{ .ns = fake.Namespace.init(gpa), .a = undefined, .b = undefined };
    }

    fn hosts(self: *Rig) void {
        self.a = .{ .ns = &self.ns, .host_id = host_a };
        self.b = .{ .ns = &self.ns, .host_id = host_b };
    }

    fn client(self: *Rig, h: *fake.Host, key: u64) !Client {
        _ = self;
        return Client.init(testing.allocator, h.face(), 1, key);
    }
};

test "rescap probe decides the register shape: PTPL-capable requests CPTPL, a RESCAP without bit 0 does not; an Invalid Field answer retries the plain shape once and records it" {
    var rig = Rig.init(testing.allocator);
    defer rig.ns.deinit();
    rig.hosts();
    var c = try rig.client(&rig.a, 0xa);
    defer c.deinit();
    try testing.expectEqual(@as(u8, 0x03), try c.probeRescap());
    try testing.expect(c.ptpl_requested);
    try c.register();
    try testing.expectEqual(nvme.RREGA_REGISTER | nvme.IEKEY | nvme.CPTPL_PERSIST, rig.ns.last.cdw10);
    try c.unregister(0xa);
    rig.ns.rescap = 0x02;
    _ = try c.probeRescap();
    try testing.expect(!c.ptpl_requested);
    try c.register();
    try testing.expectEqual(nvme.RREGA_REGISTER | nvme.IEKEY, rig.ns.last.cdw10);
    try c.unregister(0xa);
    rig.ns.cptpl_invalid_field = true;
    rig.ns.rescap = 0x03;
    _ = try c.probeRescap();
    try c.register();
    try testing.expectEqual(nvme.RREGA_REGISTER, rig.ns.last.cdw10);
    try testing.expect(!c.ptpl_requested);
    try testing.expect(rig.ns.isRegistered(0xa));
    // An errno never triggers a retry: it passes through as Transport.
    rig.ns.fail_next = error.InputOutput;
    try testing.expectError(error.Transport, c.register());
    try testing.expectEqual(@as(?errors.Error, error.InputOutput), c.last_errno);
}

test "PR acquired at mount and released at a clean unmount: the fake holder is our key, then null with zero registrants (release + unregister)" {
    var rig = Rig.init(testing.allocator);
    defer rig.ns.deinit();
    rig.hosts();
    var c = try rig.client(&rig.a, 0xa);
    defer c.deinit();
    _ = try c.probeRescap();
    try testing.expectEqual(LadderOutcome.registered, try c.registerLadder());
    try c.acquire();
    try testing.expectEqual(@as(?u64, 0xa), rig.ns.holder);
    try testing.expectEqual(nvme.RTYPE_WRITE_EXCLUSIVE, rig.ns.rtype);
    try testing.expectEqual(@as(?u64, 0xa), try c.holder());
    try c.release();
    try testing.expectEqual(@as(?u64, null), rig.ns.holder);
    try testing.expectEqual(@as(usize, 0), rig.ns.registered.items.len);
    try testing.expectEqual(@as(u64, 1), c.stats.releases);
}

test "register ladder: a kill -9 remount on a SpecStrict target recovers its own stale registration (unregister exactly it, register fresh, holder transferred by the acquire); on a lenient target the fast path registers and unregisters nothing" {
    var rig = Rig.init(testing.allocator);
    defer rig.ns.deinit();
    rig.hosts();
    // Incarnation 1 registers + acquires and dies (no release).
    var dead = try rig.client(&rig.a, 0x1);
    defer dead.deinit();
    _ = try dead.probeRescap();
    _ = try dead.registerLadder();
    try dead.acquire();
    // Incarnation 2, same host (same wire host id), a fresh key.
    var c = try rig.client(&rig.a, 0x2);
    defer c.deinit();
    _ = try c.probeRescap();
    const out = try c.registerLadder();
    try testing.expectEqual(LadderOutcome{ .recovered_own_stale = 1 }, out);
    try testing.expectEqual(@as(u32, 1), rig.ns.unregisters);
    try testing.expect(rig.ns.isRegistered(0x2));
    try testing.expect(!rig.ns.isRegistered(0x1));
    // The dead key's unregister released the reservation: a plain acquire.
    try c.acquire();
    try testing.expectEqual(@as(?u64, 0x2), rig.ns.holder);
    try testing.expectEqual(@as(u64, 1), c.stats.own_stale_recoveries);

    var lenient = Rig.init(testing.allocator);
    defer lenient.ns.deinit();
    lenient.hosts();
    lenient.ns.semantics = .lenient_replace;
    var d1 = try lenient.client(&lenient.a, 0x1);
    defer d1.deinit();
    _ = try d1.probeRescap();
    _ = try d1.registerLadder();
    try d1.acquire();
    var d2 = try lenient.client(&lenient.a, 0x2);
    defer d2.deinit();
    _ = try d2.probeRescap();
    try testing.expectEqual(LadderOutcome.registered, try d2.registerLadder());
    try testing.expectEqual(@as(u32, 0), lenient.ns.unregisters);
    try testing.expectEqual(@as(?u64, 0x2), lenient.ns.holder);
}

test "register ladder fails CLOSED: a foreign registration is never unregistered; an own-stale key beside a FOREIGN holder is recovered and the holder left alone; an unknown own identity fails closed with the conflict; a non-conflict register error propagates" {
    var rig = Rig.init(testing.allocator);
    defer rig.ns.deinit();
    rig.hosts();
    // B holds; A (strict) registers fine — a foreign registration never
    // conflicts a register, so plant the conflict: A's own prior key.
    var b = try rig.client(&rig.b, 0xb);
    defer b.deinit();
    _ = try b.probeRescap();
    _ = try b.registerLadder();
    try b.acquire();
    var a_old = try rig.client(&rig.a, 0xa0);
    defer a_old.deinit();
    _ = try a_old.probeRescap();
    _ = try a_old.registerLadder();
    var a = try rig.client(&rig.a, 0xa1);
    defer a.deinit();
    _ = try a.probeRescap();
    try testing.expectEqual(LadderOutcome{ .recovered_own_stale = 1 }, try a.registerLadder());
    try testing.expectEqual(@as(?u64, 0xb), rig.ns.holder);
    try testing.expect(rig.ns.isRegistered(0xb));
    try testing.expect(!rig.ns.isRegistered(0xa0));
    // The acquire conflicts (B holds): the gate's arbitration decides.
    try testing.expectError(error.ReservationConflict, a.acquire());

    // Unknown own identity: the conflict stands, nothing is unregistered.
    var rig2 = Rig.init(testing.allocator);
    defer rig2.ns.deinit();
    rig2.hosts();
    var old2 = try rig2.client(&rig2.a, 0x1);
    defer old2.deinit();
    _ = try old2.probeRescap();
    _ = try old2.registerLadder();
    rig2.ns.hostid_unknown = true;
    var new2 = try rig2.client(&rig2.a, 0x2);
    defer new2.deinit();
    _ = try new2.probeRescap();
    try testing.expectError(error.HostIdUnknown, new2.registerLadder());
    try testing.expectEqual(@as(u32, 0), rig2.ns.unregisters);
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, new2.last_status);

    // A conflict whose report shows no key under OUR wire id: the device's
    // FID 0x81 answer diverges from what the registration carries (the
    // SPDK measurement) — foreign, not ours to remove, nothing unregistered.
    var rig3 = Rig.init(testing.allocator);
    defer rig3.ns.deinit();
    rig3.hosts();
    var old3 = try rig3.client(&rig3.a, 0x30);
    defer old3.deinit();
    _ = try old3.probeRescap();
    _ = try old3.registerLadder();
    rig3.ns.wire_host_id_override = "host-x-host-x-09".*;
    var c3 = try rig3.client(&rig3.a, 0x31);
    defer c3.deinit();
    _ = try c3.probeRescap();
    try testing.expectError(error.ForeignRegistration, c3.registerLadder());
    try testing.expectEqual(@as(u32, 0), rig3.ns.unregisters);
    try testing.expect(rig3.ns.isRegistered(0x30));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, c3.last_status);

    // A non-conflict device status propagates unchanged (no ladder arm
    // runs): a transport error on the register.
    var rig4 = Rig.init(testing.allocator);
    defer rig4.ns.deinit();
    rig4.hosts();
    var c4 = try rig4.client(&rig4.a, 0x4);
    defer c4.deinit();
    _ = try c4.probeRescap();
    rig4.ns.fail_next = error.InputOutput;
    try testing.expectError(error.Transport, c4.registerLadder());
    try testing.expectEqual(@as(u32, 0), rig4.ns.unregisters);
    try testing.expectEqual(LadderOutcome.registered, try c4.registerLadder());
}

test "report: EDS first, the short form on 0x18; two steps sized by REGCTL (the header alone, then header + stride × regctl); a report of 70 registrants borrows past the arena and decodes completely; gauges record the last transfer" {
    var rig = Rig.init(testing.allocator);
    defer rig.ns.deinit();
    rig.hosts();
    var c = try rig.client(&rig.a, 0xa);
    defer c.deinit();
    _ = try c.probeRescap();
    _ = try c.registerLadder();
    try c.acquire();
    const before = rig.ns.commands;
    var rep = try c.report();
    defer rep.deinit(testing.allocator);
    try testing.expectEqual(@as(u32, 2), rig.ns.commands - before);
    try testing.expect(rep.extended);
    try testing.expectEqual(@as(u16, 1), c.regctl);
    try testing.expectEqual(@as(u64, nvme.reportLen(true, 1)), c.report_bytes);
    try testing.expectEqual(@as(?u64, 0xa), rep.holder());
    rig.ns.short_report_inconsistent = false;
    // A target that refuses EDS with a non-conflict status: the short form.
    var rig2 = Rig.init(testing.allocator);
    defer rig2.ns.deinit();
    rig2.hosts();
    for (0..70) |i| try rig2.ns.registered.append(testing.allocator, .{ .host_id = @splat(@intCast(i + 1)), .key = 1000 + i });
    rig2.ns.holder = 1069;
    rig2.ns.rtype = nvme.RTYPE_WERO;
    var c2 = try rig2.client(&rig2.a, 0x5);
    defer c2.deinit();
    var big = try c2.report();
    defer big.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 70), big.registrants.len);
    try testing.expectEqual(@as(?u64, 1069), big.holder());
    try testing.expect(c2.report_bytes > REPORT_ARENA_LEN);
    try testing.expectEqual(@as(u16, 70), c2.regctl);
}

test "the registrant cap is learned only from a REPEATED device-answered command-specific status at one REGCTL, unlearned by a later success at or above it; the effective cap prefers a declared one" {
    var l: CapLearner = .{};
    l.learn(0x1a0, 16);
    try testing.expectEqual(@as(?u16, null), l.learned);
    l.learn(0x1a0, 17);
    try testing.expectEqual(@as(?u16, null), l.learned);
    l.learn(0x1a0, 17);
    try testing.expectEqual(@as(?u16, 17), l.learned);
    // A generic or transient status never teaches.
    var g: CapLearner = .{};
    g.learn(0x02, 5);
    g.learn(0x02, 5);
    g.learn(0x301, 5);
    g.learn(0x301, 5);
    try testing.expectEqual(@as(?u16, null), g.learned);
    l.unlearnOnSuccess(16);
    try testing.expectEqual(@as(?u16, 17), l.learned);
    l.unlearnOnSuccess(17);
    try testing.expectEqual(@as(?u16, null), l.learned);
    try testing.expectEqual(@as(u16, 0), l.effective(null));
    try testing.expectEqual(@as(u16, 64), l.effective(64));
}

test "posture: RESCAP 0 degrades to flock+claim (the client freed); RESCAP ≠ 0 is flock+pr; a probe error degrades too" {
    var rig = Rig.init(testing.allocator);
    defer rig.ns.deinit();
    rig.hosts();
    rig.ns.rescap = 0;
    const c0 = try testing.allocator.create(Client);
    c0.* = try rig.client(&rig.a, 0x1);
    const p0 = resolvePosture(testing.allocator, c0, "/dev/fake");
    try testing.expectEqual(Posture{ .none = .rescap_zero }, p0);
    try testing.expectEqualStrings("flock+claim", p0.mode());
    rig.ns.rescap = 0x03;
    const c1 = try testing.allocator.create(Client);
    c1.* = try rig.client(&rig.a, 0x1);
    const p1 = resolvePosture(testing.allocator, c1, "/dev/fake");
    try testing.expect(p1 == .active);
    try testing.expectEqualStrings("flock+pr", p1.mode());
    p1.active.deinit();
    testing.allocator.destroy(p1.active);
    const c2 = try testing.allocator.create(Client);
    c2.* = try rig.client(&rig.a, 0x1);
    rig.ns.fail_next = error.InputOutput;
    try testing.expectEqual(Posture{ .none = .probe_error }, resolvePosture(testing.allocator, c2, "/dev/fake"));
}

test "host identity is pair-or-neither: neither → none; both → the pair (option over env); exactly one → the refusal naming the other; the stamp compares absence as still absent" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(ExplicitIdentity.none, resolveExplicitIdentity(&env, .{}));
    try env.put("SQUEEZEFS_HOSTNQN", "nqn.2014-08.org.nvmexpress:uuid:abc");
    try testing.expectEqual(ExplicitIdentity{ .refusal = .nqn_without_id }, resolveExplicitIdentity(&env, .{}));
    try env.put("SQUEEZEFS_HOSTID", "  ");
    try testing.expectEqual(ExplicitIdentity{ .refusal = .nqn_without_id }, resolveExplicitIdentity(&env, .{}));
    try env.put("SQUEEZEFS_HOSTID", "0123");
    const both = resolveExplicitIdentity(&env, .{});
    try testing.expectEqualStrings("0123", both.pair.hostid);
    const opt = resolveExplicitIdentity(&env, .{ .hostid = "9999" });
    try testing.expectEqualStrings("9999", opt.pair.hostid);
    try testing.expectEqualStrings("nqn.2014-08.org.nvmexpress:uuid:abc", opt.pair.hostnqn);
    var env2 = std.process.Environ.Map.init(testing.allocator);
    defer env2.deinit();
    try env2.put("SQUEEZEFS_HOSTID", "0123");
    try testing.expectEqual(ExplicitIdentity{ .refusal = .id_without_nqn }, resolveExplicitIdentity(&env2, .{}));
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try HostIdentityRefusal.id_without_nqn.write(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "PAIR-OR-NEITHER") != null);
    try testing.expect(IdentityStamp.of("", "").eql(IdentityStamp.of("", "")));
    try testing.expect(!IdentityStamp.of("a", "").eql(IdentityStamp.of("", "")));
}

test "stats JSON names every pr_* gauge" {
    var rig = Rig.init(testing.allocator);
    defer rig.ns.deinit();
    rig.hosts();
    var c = try rig.client(&rig.a, 0xa);
    defer c.deinit();
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try c.writeStatsJson(&w);
    inline for (.{ "pr_registrants_per_namespace", "pr_report_bytes", "pr_registrant_cap", "pr_registrant_cap_refusals", "pr_preempts", "pr_ptpl_requested" }) |k| {
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"" ++ k ++ "\":") != null);
    }
}
