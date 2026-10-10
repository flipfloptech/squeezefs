//! The REAL NVMe Persistent Reservation legs (charter §5.5 "Fencing",
//! §5.7 B1, §8; `docs/porting/ops_nvmet_keys.md` §4): what rung 5 proved
//! on the fake, proven here on the kernel target — nvmet-loop and
//! nvmet-tcp — through the product's own primitive (`pr.Client` over
//! `pr_linux.Device`, `IORING_OP_URING_CMD` on `/dev/ngXnY`) and the
//! product's own writer (`uring_fs`, O_DIRECT on `/dev/nvmeXnY`): the
//! RESCAP byte, EDS-first with the short form's device status pinned, the
//! register fast path, the SPEC-STRICT conflict that makes the ladder's
//! "unregister only OWN stale keys" arm fire for real (every `kill -9` →
//! remount registers a different per-incarnation key under the same host
//! identity, and nvmet's `pr.c` conflicts), the two-identity Write
//! Exclusive fence as a DEVICE REJECTION (`-EBADE` on the write CQE), WERO
//! rtype 3 with a registrant writing and an unregistered one refused (the
//! `fencing.zig` binding: `acquireWero` + `joinAsRegistrant` on two real
//! associations), `preempt_and_abort` as a plain preempt, release +
//! unregister = zero residue, and the shared-identity probe. The verb
//! `nvmeof resv-report <dev> [--json] [--pr-matrix --peer <dev> [--force]]`
//! runs them so the fidelity tier needs no nvme-cli (quirk Q14).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const nvme = @import("../../meta/guard/nvme_cmd.zig");
const pr = @import("../../meta/guard/pr.zig");
const pr_linux = @import("../../meta/guard/pr_linux.zig");
const fencing = @import("../../meta/sym/membership/fencing.zig");
const initiator = @import("initiator.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/pr_legs is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// RESCAP bit 1: Write Exclusive supported (NVMe Base §5.x Identify NS).
pub const RESCAP_WRITE_EXCLUSIVE: u8 = 1 << 1;
/// RESCAP bit 3: Write Exclusive — Registrants Only supported.
pub const RESCAP_WERO: u8 = 1 << 3;

/// The keys the matrix uses: distinct from any writer's `claim.prKey`.
const key_a1: u64 = 0xA1A1_A1A1_0000_0001;
const key_a2: u64 = 0xA1A1_A1A1_0000_0002;
const key_b1: u64 = 0xB1B1_B1B1_0000_0001;

/// A 4 KiB O_DIRECT write at offset 0 through the product's io_uring file
/// layer on the BLOCK node: the fence arrives on the write CQE.
pub const PROBE_WRITE_LEN: usize = 4096;

/// Open the namespace the way the guard does: a block node resolves to
/// its char twin; a `/dev/ngXnY` is mapped to its block node first.
pub fn resolve(gpa: Allocator, path: []const u8) pr_linux.Resolved {
    const base = std.fs.path.basename(path);
    if (std.mem.startsWith(u8, base, "ng")) {
        var bp: [48]u8 = undefined;
        const block = std.fmt.bufPrint(&bp, "/dev/nvme{s}", .{base[2..]}) catch return .{ .none = .not_nvme_namespace };
        return pr_linux.resolve(gpa, block);
    }
    return pr_linux.resolve(gpa, path);
}

/// `nvmeof resv-report <dev> [--json]`.
pub fn resvReport(gpa: Allocator, path: []const u8, json: bool, out: *Io.Writer, err: *Io.Writer) u8 {
    const r = resolve(gpa, path);
    const dev = switch (r) {
        .none => |why| {
            err.print("squeezefs nvmeof resv-report: {s} is not an NVMe namespace with a generic char node ({s}) — a file-backed or loop volume has no Persistent Reservations to report\n", .{ path, @tagName(why) }) catch {};
            err.flush() catch {};
            return 1;
        },
        .device => |d| d,
    };
    defer dev.deinit();
    var client = pr.Client.init(gpa, dev.face(), dev.nsid, 0) catch {
        err.writeAll("squeezefs nvmeof resv-report: out of memory\n") catch {};
        return 1;
    };
    defer client.deinit();
    const rescap = client.probeRescap() catch |e| {
        var buf: [128]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        client.writeError(&w, e) catch {};
        err.print("squeezefs nvmeof resv-report: Identify failed on /dev/{s}: {s}\n", .{ dev.nodeName(), w.buffered() }) catch {};
        err.flush() catch {};
        return 1;
    };
    var rep = client.report() catch |e| {
        var buf: [128]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        client.writeError(&w, e) catch {};
        err.print("squeezefs nvmeof resv-report: Reservation Report failed on /dev/{s}: {s}\n", .{ dev.nodeName(), w.buffered() }) catch {};
        err.flush() catch {};
        return 1;
    };
    defer rep.deinit(gpa);
    writeReport(out, path, dev.nodeName(), dev.nsid, rescap, &client, &rep, json) catch {};
    out.flush() catch {};
    return 0;
}

fn writeReport(w: *Io.Writer, path: []const u8, ng: []const u8, nsid: u32, rescap: u8, client: *const pr.Client, rep: *const nvme.Report, json: bool) Io.Writer.Error!void {
    if (json) {
        try w.print("{{\"device\":\"{s}\",\"char\":\"/dev/{s}\",\"nsid\":{d},\"rescap\":{d},\"regctl\":{d},\"report_bytes\":{d},\"rtype\":{d},\"wero\":{},\"ptpls\":{d},\"generation\":{d},\"registrant_cap\":{d},\"holder_key\":", .{ path, ng, nsid, rescap, rep.regctl, client.report_bytes, rep.rtype, rep.rtype == nvme.RTYPE_WERO, rep.ptpls, rep.generation, client.cap.effective(null) });
        if (rep.holder()) |h| try w.print("\"{x:0>16}\"", .{h}) else try w.writeAll("null");
        try w.writeAll(",\"registrants\":[");
        for (rep.registrants, 0..) |r, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"cntlid\":{d},\"rkey\":\"{x:0>16}\",\"host_id\":\"{x}\",\"holds_reservation\":{}}}", .{ r.cntlid, r.rkey, &r.host_id, r.holds });
        }
        try w.writeAll("]}\n");
        return;
    }
    try w.print("{s} (/dev/{s}, nsid {d}): RESCAP 0x{x:0>2} (PTPL {s}, WE {s}, WERO {s})\n  rtype {d}{s}; regctl {d}; report {d} B; ptpls {d}; generation {d}; registrant cap {d}\n  holder ", .{ path, ng, nsid, rescap, if (rescap & nvme.RESCAP_PTPL != 0) "yes" else "no", if (rescap & RESCAP_WRITE_EXCLUSIVE != 0) "yes" else "no", if (rescap & RESCAP_WERO != 0) "yes" else "no", rep.rtype, if (rep.rtype == nvme.RTYPE_WERO) " (WERO)" else if (rep.rtype == nvme.RTYPE_WRITE_EXCLUSIVE) " (Write Exclusive)" else "", rep.regctl, client.report_bytes, rep.ptpls, rep.generation, client.cap.effective(null) });
    if (rep.holder()) |h| try w.print("{x:0>16}\n", .{h}) else try w.writeAll("none\n");
    for (rep.registrants) |r| try w.print("  registrant cntlid {d} rkey {x:0>16} host_id {x}{s}\n", .{ r.cntlid, r.rkey, &r.host_id, if (r.holds) " (holds)" else "" });
}

// ---------------------------------------------------------------------------
// The matrix.
// ---------------------------------------------------------------------------

pub const Leg = enum {
    rescap,
    eds_first,
    register_idempotent,
    spec_strict_ladder,
    two_identity_we_fence,
    wero_rtype3,
    preempt_and_abort,
    release_zero_residue,
    shared_identity,
};

pub const LegResult = struct {
    leg: Leg,
    ok: bool,
    detail_buf: [256]u8 = undefined,
    detail_len: usize = 0,

    pub fn detail(self: *const LegResult) []const u8 {
        return self.detail_buf[0..self.detail_len];
    }
};

/// Two associations to ONE namespace under DISTINCT host identities: A
/// (block + char) and B (block + char), plus the pool the probe writes
/// ride and the env the O_DIRECT registration reads.
pub const Pair = struct {
    gpa: Allocator,
    env: *const std.process.Environ.Map,
    pool: *uring_fs.Pool,
    a_block: []const u8,
    b_block: []const u8,
    a: *pr.Client,
    b: *pr.Client,
    /// A second client on A's association (the shared-identity probe).
    a_twin: *pr.Client,
    dev_a: *pr_linux.Device,
    dev_b: *pr_linux.Device,
    buf: uring_fs.AlignedBuf,

    pub fn open(gpa: Allocator, env: *const std.process.Environ.Map, pool: *uring_fs.Pool, a_block: []const u8, b_block: []const u8) !Pair {
        const dev_a = switch (pr_linux.resolve(gpa, a_block)) {
            .device => |d| d,
            .none => return error.NotAnNvmeNamespace,
        };
        errdefer dev_a.deinit();
        const dev_b = switch (pr_linux.resolve(gpa, b_block)) {
            .device => |d| d,
            .none => return error.NotAnNvmeNamespace,
        };
        errdefer dev_b.deinit();
        const a = try gpa.create(pr.Client);
        errdefer gpa.destroy(a);
        a.* = try pr.Client.init(gpa, dev_a.face(), dev_a.nsid, key_a1);
        errdefer a.deinit();
        const b = try gpa.create(pr.Client);
        errdefer gpa.destroy(b);
        b.* = try pr.Client.init(gpa, dev_b.face(), dev_b.nsid, key_b1);
        errdefer b.deinit();
        const twin = try gpa.create(pr.Client);
        errdefer gpa.destroy(twin);
        twin.* = try pr.Client.init(gpa, dev_a.face(), dev_a.nsid, key_a2);
        errdefer twin.deinit();
        // The block nodes under the O_DIRECT posture (the data plane's).
        _ = uring_fs.registerMetaDevice(gpa, env, a_block) catch return error.DirectOpenRefused;
        _ = uring_fs.registerMetaDevice(gpa, env, b_block) catch return error.DirectOpenRefused;
        var buf = try uring_fs.AlignedBuf.zeroed(gpa, PROBE_WRITE_LEN, 4096);
        errdefer buf.deinit();
        @memset(buf.bytes, 0x5a);
        return .{ .gpa = gpa, .env = env, .pool = pool, .a_block = a_block, .b_block = b_block, .a = a, .b = b, .a_twin = twin, .dev_a = dev_a, .dev_b = dev_b, .buf = buf };
    }

    pub fn close(self: *Pair) void {
        self.buf.deinit();
        self.a_twin.deinit();
        self.gpa.destroy(self.a_twin);
        self.a.deinit();
        self.gpa.destroy(self.a);
        self.b.deinit();
        self.gpa.destroy(self.b);
        self.dev_a.deinit();
        self.dev_b.deinit();
    }

    /// Best-effort zero residue: every client releases and unregisters.
    pub fn reset(self: *Pair) void {
        for ([_]*pr.Client{ self.a, self.a_twin, self.b }) |c| {
            c.rtype = nvme.RTYPE_WRITE_EXCLUSIVE;
            c.release() catch {};
            c.rtype = nvme.RTYPE_WERO;
            c.release() catch {};
            c.rtype = nvme.RTYPE_WRITE_EXCLUSIVE;
        }
    }

    /// One 4 KiB O_DIRECT write at offset 0 through the block node.
    pub fn write(self: *Pair, block: []const u8) uring_fs.Error!void {
        return self.pool.writeAt(block, 0, self.buf.bytes);
    }

    pub fn regctl(self: *Pair) !u16 {
        var rep = try self.a.report();
        rep.deinit(self.gpa);
        return self.a.regctl;
    }
};

fn set(r: *LegResult, ok: bool, comptime fmt: []const u8, args: anytype) void {
    r.ok = ok;
    const d = std.fmt.bufPrint(&r.detail_buf, fmt, args) catch &r.detail_buf;
    r.detail_len = d.len;
}

fn errName(e: anyerror) []const u8 {
    return @errorName(e);
}

pub fn legRescap(p: *Pair, r: *LegResult) void {
    const rescap = p.a.probeRescap() catch |e| return set(r, false, "Identify failed: {s}", .{errName(e)});
    _ = p.b.probeRescap() catch {};
    _ = p.a_twin.probeRescap() catch {};
    const we = rescap & RESCAP_WRITE_EXCLUSIVE != 0;
    const ptpl = rescap & nvme.RESCAP_PTPL != 0;
    set(r, rescap != 0 and we and !ptpl, "RESCAP 0x{x:0>2}: WE {s}, WERO {s}, PTPL {s} (nvmet advertises none — the heartbeat re-check is the heal)", .{ rescap, if (we) "yes" else "NO", if (rescap & RESCAP_WERO != 0) "yes" else "no", if (ptpl) "YES" else "no" });
}

pub fn legEdsFirst(p: *Pair, r: *LegResult) void {
    var rep = p.a.report() catch |e| return set(r, false, "extended report failed: {s}", .{errName(e)});
    defer rep.deinit(p.gpa);
    if (!rep.extended) return set(r, false, "the report fell back to the short form", .{});
    // The short form against a fabrics association: a device status, not
    // success (nvmet answers Host Identifier Inconsistent Format, SC 0x18).
    var short: [nvme.REPORT_HEADER_SHORT]u8 = undefined;
    const cmd = nvme.reservationReport(p.a.nsid, &short, false);
    const c = p.a.ns.issue(p.a.ns.ctx, .io, &cmd) catch |e| return set(r, false, "short-form report transport error: {s}", .{errName(e)});
    set(r, c.status != nvme.STATUS_SUCCESS, "extended report {d} B decoded; the short form answered status 0x{x:0>3} ({s})", .{ p.a.report_bytes, c.status, if (c.status == nvme.SC_HOSTID_INCONSISTENT) "Host Identifier Inconsistent Format" else "a device status" });
}

pub fn legRegisterIdempotent(p: *Pair, r: *LegResult) void {
    const first = p.a.registerLadder() catch |e| return set(r, false, "register failed: {s}", .{errName(e)});
    if (first != .registered) return set(r, false, "a fresh namespace's register took the ladder's recovery arm", .{});
    p.a.register() catch |e| return set(r, false, "the same-key re-register failed: {s}", .{errName(e)});
    const n = p.regctl() catch |e| return set(r, false, "report failed: {s}", .{errName(e)});
    set(r, n == 1, "register fast path; a second same-key register is idempotent; regctl {d}", .{n});
}

pub fn legSpecStrictLadder(p: *Pair, r: *LegResult) void {
    // A's incarnation 1 (key a1) registered and holding; incarnation 2
    // (key a2, SAME host identity) registers: nvmet conflicts, the ladder
    // reads FID 0x81 + the report and unregisters ONLY a1.
    _ = p.a.registerLadder() catch |e| return set(r, false, "a1 register failed: {s}", .{errName(e)});
    p.a.acquire() catch |e| return set(r, false, "a1 acquire failed: {s}", .{errName(e)});
    _ = p.b.registerLadder() catch |e| return set(r, false, "b1 register failed: {s}", .{errName(e)});
    if (p.a_twin.register()) |_| {
        return set(r, false, "a2's plain register SUCCEEDED: this target is lenient, not spec-strict", .{});
    } else |e| switch (e) {
        error.ReservationConflict => {},
        else => return set(r, false, "a2's plain register answered {s}, not a conflict", .{errName(e)}),
    }
    const out = p.a_twin.registerLadder() catch |e| return set(r, false, "a2's ladder failed: {s}", .{errName(e)});
    const n = p.regctl() catch 0;
    var rep = p.a_twin.report() catch |e| return set(r, false, "report failed: {s}", .{errName(e)});
    defer rep.deinit(p.gpa);
    var b_still = false;
    var a1_gone = true;
    for (rep.registrants) |reg| {
        if (reg.rkey == key_b1) b_still = true;
        if (reg.rkey == key_a1) a1_gone = false;
    }
    set(r, out == .recovered_own_stale and out.recovered_own_stale == 1 and b_still and a1_gone and p.a_twin.stats.own_stale_recoveries == 1, "a2's plain register conflicted (0x83); the ladder unregistered exactly a1 (own stale) and left b1 (foreign); regctl {d}; pr_own_stale_recoveries {d}", .{ n, p.a_twin.stats.own_stale_recoveries });
}

pub fn legTwoIdentityWeFence(p: *Pair, r: *LegResult) void {
    _ = p.a.registerLadder() catch |e| return set(r, false, "a register failed: {s}", .{errName(e)});
    p.a.acquire() catch |e| return set(r, false, "a acquire failed: {s}", .{errName(e)});
    p.write(p.a_block) catch |e| return set(r, false, "the holder's own write failed: {s}", .{errName(e)});
    // B is NOT registered: a Write Exclusive hold refuses its write at the device.
    const b_fenced = if (p.write(p.b_block)) false else |e| e == error.ReservationConflict;
    if (!b_fenced) return set(r, false, "B's write under A's Write Exclusive hold did not answer ReservationConflict (-EBADE)", .{});
    _ = p.b.registerLadder() catch |e| return set(r, false, "b register failed: {s}", .{errName(e)});
    // Registered but not the holder: WE still refuses B.
    const b_fenced_reg = if (p.write(p.b_block)) false else |e| e == error.ReservationConflict;
    p.b.preempt(key_a1) catch |e| return set(r, false, "b preempt failed: {s}", .{errName(e)});
    p.write(p.b_block) catch |e| return set(r, false, "B's write after the preempt failed: {s}", .{errName(e)});
    const a_fenced = if (p.write(p.a_block)) false else |e| e == error.ReservationConflict;
    const holder = p.b.holder() catch null;
    set(r, b_fenced and b_fenced_reg and a_fenced and holder == key_b1, "B fenced (-EBADE) unregistered and registered under A's WE; B preempted A; B's writes land; A's next write fenced (-EBADE); holder {x:0>16}", .{holder orelse 0});
}

/// The `fencing.zig` binding on two real associations: the manager holds
/// WERO (`acquireWero`), the joiner registers through `joinAsRegistrant`
/// and WRITES; an unregistered association is refused.
pub fn legWeroRtype3(p: *Pair, r: *LegResult) void {
    fencing.acquireWero(p.a) catch |e| return set(r, false, "acquireWero failed: {s}", .{errName(e)});
    const before_join = if (p.write(p.b_block)) false else |e| e == error.ReservationConflict;
    const joined = fencing.joinAsRegistrant(p.b, fencing.declaredRegistrantCap(p.env)) catch |e| return set(r, false, "joinAsRegistrant failed: {s}", .{errName(e)});
    p.write(p.b_block) catch |e| return set(r, false, "the registrant's write under WERO failed: {s}", .{errName(e)});
    var rep = p.a.report() catch |e| return set(r, false, "report failed: {s}", .{errName(e)});
    defer rep.deinit(p.gpa);
    // B leaves: its writes are refused again.
    p.b.unregister(key_b1) catch |e| return set(r, false, "b unregister failed: {s}", .{errName(e)});
    const after_leave = if (p.write(p.b_block)) false else |e| e == error.ReservationConflict;
    set(r, rep.rtype == nvme.RTYPE_WERO and joined.regctl == 2 and joined.cap_in_force == 0 and !joined.shared and before_join and after_leave, "rtype {d} (WERO); the registrant joined (regctl {d}, cap {d}) and wrote; unregistered writes refused before the join and after leaving", .{ rep.rtype, joined.regctl, joined.cap_in_force });
}

pub fn legPreemptAndAbort(p: *Pair, r: *LegResult) void {
    _ = p.a.registerLadder() catch |e| return set(r, false, "a register failed: {s}", .{errName(e)});
    p.a.acquire() catch |e| return set(r, false, "a acquire failed: {s}", .{errName(e)});
    _ = p.b.registerLadder() catch |e| return set(r, false, "b register failed: {s}", .{errName(e)});
    p.b.preemptAndAbort(key_a1) catch |e| return set(r, false, "preempt-and-abort failed: {s}", .{errName(e)});
    var rep = p.b.report() catch |e| return set(r, false, "report failed: {s}", .{errName(e)});
    defer rep.deinit(p.gpa);
    var a_gone = true;
    for (rep.registrants) |reg| if (reg.rkey == key_a1) {
        a_gone = false;
    };
    set(r, a_gone and rep.holder() == key_b1 and rep.rtype == nvme.RTYPE_WRITE_EXCLUSIVE, "executed as a plain preempt on nvmet: the victim's registration is gone, the hold stands under B (rtype {d}, regctl {d})", .{ rep.rtype, rep.regctl });
}

pub fn legReleaseZeroResidue(p: *Pair, r: *LegResult) void {
    _ = p.a.registerLadder() catch |e| return set(r, false, "a register failed: {s}", .{errName(e)});
    p.a.acquire() catch |e| return set(r, false, "a acquire failed: {s}", .{errName(e)});
    p.a.release() catch |e| return set(r, false, "release failed: {s}", .{errName(e)});
    var rep = p.a.report() catch |e| return set(r, false, "report failed: {s}", .{errName(e)});
    defer rep.deinit(p.gpa);
    set(r, rep.regctl == 0 and rep.rtype == 0, "release + unregister: regctl {d}, rtype {d}", .{ rep.regctl, rep.rtype });
}

pub fn legSharedIdentity(p: *Pair, r: *LegResult) void {
    // Two clients on ONE association (A) with distinct keys: a lenient
    // path is not available on nvmet (spec-strict), so the second one
    // registers via the ladder (which unregisters the first) — the shared
    // probe reads the OTHER key under our wire host id BEFORE that.
    _ = p.a.registerLadder() catch |e| return set(r, false, "a register failed: {s}", .{errName(e)});
    const shared = p.a_twin.identityShared() catch |e| return set(r, false, "identityShared failed: {s}", .{errName(e)});
    const not_shared_b = p.b.identityShared() catch |e| return set(r, false, "B identityShared failed: {s}", .{errName(e)});
    set(r, shared and !not_shared_b, "another rkey under OUR wire host id reads shared on A's association ({}), not on B's ({})", .{ shared, not_shared_b });
}

pub const MatrixReport = struct {
    results: [9]LegResult,

    pub fn allOk(self: *const MatrixReport) bool {
        for (self.results) |r| if (!r.ok) return false;
        return true;
    }

    pub fn write(self: *const MatrixReport, w: *Io.Writer, json: bool) Io.Writer.Error!void {
        if (json) {
            try w.writeAll("{\"legs\":[");
            for (self.results, 0..) |r, i| {
                if (i != 0) try w.writeByte(',');
                try w.print("{{\"leg\":\"{s}\",\"ok\":{},\"detail\":\"{s}\"}}", .{ @tagName(r.leg), r.ok, r.detail() });
            }
            try w.print("],\"all_ok\":{}}}\n", .{self.allOk()});
            return;
        }
        for (self.results) |r| try w.print("  {s:<24} {s}  {s}\n", .{ @tagName(r.leg), if (r.ok) "PASS" else "FAIL", r.detail() });
        try w.print("pr-matrix: {s}\n", .{if (self.allOk()) "all legs passed" else "FAILED"});
    }
};

/// The whole matrix over a pair: every leg starts from zero residue.
pub fn runMatrix(p: *Pair) MatrixReport {
    var out: MatrixReport = .{ .results = undefined };
    const legs = [_]struct { leg: Leg, run: *const fn (*Pair, *LegResult) void }{
        .{ .leg = .rescap, .run = legRescap },
        .{ .leg = .eds_first, .run = legEdsFirst },
        .{ .leg = .register_idempotent, .run = legRegisterIdempotent },
        .{ .leg = .spec_strict_ladder, .run = legSpecStrictLadder },
        .{ .leg = .two_identity_we_fence, .run = legTwoIdentityWeFence },
        .{ .leg = .wero_rtype3, .run = legWeroRtype3 },
        .{ .leg = .preempt_and_abort, .run = legPreemptAndAbort },
        .{ .leg = .release_zero_residue, .run = legReleaseZeroResidue },
        .{ .leg = .shared_identity, .run = legSharedIdentity },
    };
    for (legs, 0..) |l, i| {
        p.reset();
        out.results[i] = .{ .leg = l.leg, .ok = false };
        l.run(p, &out.results[i]);
    }
    p.reset();
    return out;
}

/// `nvmeof resv-report --pr-matrix <devA> --peer <devB> [--force]`: root,
/// DESTRUCTIVE; refuses a namespace with `regctl > 0` unless forced.
pub fn matrixVerb(gpa: Allocator, env: *const std.process.Environ.Map, a_block: []const u8, b_block: []const u8, force: bool, json: bool, out: *Io.Writer, err: *Io.Writer) u8 {
    const pool = uring_fs.Pool.init(gpa, uring_fs.Config.fixed(1, uring_fs.nofileSoftAsFound())) catch {
        err.writeAll("squeezefs nvmeof resv-report --pr-matrix: io pool init failed\n") catch {};
        return 1;
    };
    defer pool.deinit();
    var p = Pair.open(gpa, env, pool, a_block, b_block) catch |e| {
        err.print("squeezefs nvmeof resv-report --pr-matrix: cannot open the pair {s} / {s}: {s} (both must be NVMe namespace block nodes with generic char twins, connected under DISTINCT host identities)\n", .{ a_block, b_block, @errorName(e) }) catch {};
        err.flush() catch {};
        return 1;
    };
    defer p.close();
    if (p.a.probeRescap() catch 0 == 0) {
        err.print("squeezefs nvmeof resv-report --pr-matrix: {s} advertises no reservation support (RESCAP 0)\n", .{a_block}) catch {};
        err.flush() catch {};
        return 1;
    }
    const n = p.regctl() catch 0;
    if (n != 0 and !force) {
        err.print("squeezefs nvmeof resv-report --pr-matrix: {s} carries {d} registrant(s) — the matrix is DESTRUCTIVE (it preempts and unregisters); pass --force on a namespace nobody mounts\n", .{ a_block, n }) catch {};
        err.flush() catch {};
        return 1;
    }
    const rep = runMatrix(&p);
    rep.write(out, json) catch {};
    out.flush() catch {};
    return if (rep.allOk()) 0 else 1;
}

const testing = std.testing;

test "resolve maps a /dev/ng* spelling to its block node; a file is not a namespace" {
    try testing.expectEqual(pr_linux.Resolved{ .none = .not_nvme_namespace }, resolve(testing.allocator, "/proc/self/exe"));
    try testing.expectEqual(pr_linux.Resolved{ .none = .not_nvme_namespace }, resolve(testing.allocator, "/dev/ng999n1"));
}
