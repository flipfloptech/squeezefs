//! The FAKE NVMe namespace (spec `guard_claims.md` §5.2; the reference's
//! `FakeNvmeNamespace`, R:1173-1517): an in-RAM controller that answers the
//! guard's opcodes by DECODING the same 72 B command the device would — the
//! opcode, CDW10/CDW11 fields and the LE payload at `addr` — with the real
//! status codes, including Reservation Conflict. Every ladder leg runs on a
//! file substrate against it; the real device path is exercised only where
//! a `/dev/ng*` exists (rung 8 brings nvmet-loop).
//!
//! Laws (R:1385-1517): an unregistered host's acquire conflicts; a
//! re-acquire of the same key + rtype is idempotent; a changed rtype
//! conflicts; a release under the wrong rtype is refused (Invalid Field);
//! unregistering the holder's key releases; `SpecStrict` register refuses a
//! different key from an already registered host (SPDK), `LenientReplace`
//! replaces it in place (kernel nvmet).

const std = @import("std");
const nvme = @import("nvme_cmd.zig");
const errors = @import("../../core/errors.zig");

pub const RegisterSemantics = enum { spec_strict, lenient_replace };

pub const Registered = struct { host_id: [16]u8, key: u64 };

/// The one shared namespace every `Host` of a test drives.
pub const Namespace = struct {
    gpa: std.mem.Allocator,
    holder: ?u64 = null,
    rtype: u8 = 0,
    registered: std.ArrayList(Registered) = .empty,
    semantics: RegisterSemantics = .spec_strict,
    /// Identify NS byte 31; 0 = no reservation support.
    rescap: u8 = 0x03,
    /// A target that refuses CPTPL/IEKEY with Invalid Field (quirk Q11's
    /// plain-shape retry).
    cptpl_invalid_field: bool = false,
    /// A fabrics target: the short-form report answers SC 0x18.
    short_report_inconsistent: bool = false,
    /// Every host's FID 0x81 answers zeros ("unknown").
    hostid_unknown: bool = false,
    /// FID 0x81 answers THIS id for every host instead of the one its
    /// registrations carry (the 2026-07-17 SPDK session measured the wire
    /// id diverging from `/etc/nvme/hostid`).
    wire_host_id_override: ?[16]u8 = null,
    /// An injected transport failure on the next command (a dead node).
    fail_next: ?errors.Error = null,
    generation: u32 = 0,
    preempts: u32 = 0,
    unregisters: u32 = 0,
    commands: u32 = 0,
    /// The last decoded command (the byte-exact proof the tests read).
    last: nvme.UringCmd = nvme.UringCmd.zeroed(),
    last_queue: nvme.Queue = .admin,

    pub fn init(gpa: std.mem.Allocator) Namespace {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *Namespace) void {
        self.registered.deinit(self.gpa);
        self.* = undefined;
    }

    /// A synthetic foreign host registered with `key` and holding WE.
    pub fn seedHolder(self: *Namespace, key: u64) !void {
        try self.registered.append(self.gpa, .{ .host_id = foreign_host, .key = key });
        self.holder = key;
        self.rtype = nvme.RTYPE_WRITE_EXCLUSIVE;
        self.generation += 1;
    }

    /// A target without PTPL power-cycled: the reservation and every
    /// registration are gone.
    pub fn powerCycle(self: *Namespace) void {
        self.holder = null;
        self.rtype = 0;
        self.registered.clearRetainingCapacity();
        self.generation += 1;
    }

    /// The device's write law: WE ⇒ the holder's host alone; WERO ⇒ any
    /// registrant; no reservation ⇒ anyone.
    pub fn writeAllowed(self: *const Namespace, host_id: [16]u8) bool {
        const holder = self.holder orelse return true;
        return switch (self.rtype) {
            nvme.RTYPE_WRITE_EXCLUSIVE => if (self.find(host_id)) |r| r.key == holder else false,
            nvme.RTYPE_WERO => self.find(host_id) != null,
            else => false,
        };
    }

    pub fn find(self: *const Namespace, host_id: [16]u8) ?Registered {
        for (self.registered.items) |r| if (std.mem.eql(u8, &r.host_id, &host_id)) return r;
        return null;
    }

    pub fn isRegistered(self: *const Namespace, key: u64) bool {
        for (self.registered.items) |r| if (r.key == key) return true;
        return false;
    }

    fn remove(self: *Namespace, key: u64) void {
        var i: usize = 0;
        while (i < self.registered.items.len) {
            if (self.registered.items[i].key == key) {
                _ = self.registered.swapRemove(i);
            } else i += 1;
        }
    }

    fn removeHost(self: *Namespace, host_id: [16]u8) void {
        var i: usize = 0;
        while (i < self.registered.items.len) {
            if (std.mem.eql(u8, &self.registered.items[i].host_id, &host_id)) {
                _ = self.registered.swapRemove(i);
            } else i += 1;
        }
    }

    fn payload(cmd: *const nvme.UringCmd, comptime n: usize) *[n]u8 {
        std.debug.assert(cmd.data_len == n);
        return @ptrFromInt(cmd.addr);
    }

    fn data(cmd: *const nvme.UringCmd) []u8 {
        const p: [*]u8 = @ptrFromInt(cmd.addr);
        return p[0..cmd.data_len];
    }

    fn register(self: *Namespace, host_id: [16]u8, cmd: *const nvme.UringCmd) !nvme.Status {
        const p = payload(cmd, 16);
        const crkey = std.mem.readInt(u64, p[0..8], .little);
        const nrkey = std.mem.readInt(u64, p[8..16], .little);
        const rrega = cmd.cdw10 & 0x7;
        const iekey = cmd.cdw10 & nvme.IEKEY != 0;
        if (self.cptpl_invalid_field and (cmd.cdw10 & nvme.CPTPL_PERSIST != 0 or iekey)) return nvme.SC_INVALID_FIELD;
        const mine = self.find(host_id);
        switch (rrega) {
            nvme.RREGA_REGISTER => {
                if (mine) |m| {
                    if (m.key == nrkey) return nvme.STATUS_SUCCESS;
                    switch (self.semantics) {
                        .spec_strict => return nvme.SC_RESERVATION_CONFLICT,
                        .lenient_replace => {
                            self.removeHost(host_id);
                            if (self.holder == m.key) self.holder = nrkey;
                        },
                    }
                }
                try self.registered.append(self.gpa, .{ .host_id = host_id, .key = nrkey });
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            nvme.RREGA_UNREGISTER => {
                const m = mine orelse return nvme.SC_RESERVATION_CONFLICT;
                if (!iekey and m.key != crkey) return nvme.SC_RESERVATION_CONFLICT;
                self.removeHost(host_id);
                self.unregisters += 1;
                if (self.holder == m.key) {
                    self.holder = null;
                    self.rtype = 0;
                }
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            nvme.RREGA_REPLACE => {
                const m = mine orelse return nvme.SC_RESERVATION_CONFLICT;
                if (!iekey and m.key != crkey) return nvme.SC_RESERVATION_CONFLICT;
                self.removeHost(host_id);
                try self.registered.append(self.gpa, .{ .host_id = host_id, .key = nrkey });
                if (self.holder == m.key) self.holder = nrkey;
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            else => return nvme.SC_INVALID_FIELD,
        }
    }

    fn acquire(self: *Namespace, host_id: [16]u8, cmd: *const nvme.UringCmd) nvme.Status {
        const p = payload(cmd, 16);
        const crkey = std.mem.readInt(u64, p[0..8], .little);
        const prkey = std.mem.readInt(u64, p[8..16], .little);
        const racqa = cmd.cdw10 & 0x7;
        const rtype: u8 = @intCast((cmd.cdw10 >> 8) & 0xff);
        const mine = self.find(host_id) orelse return nvme.SC_RESERVATION_CONFLICT;
        if (mine.key != crkey) return nvme.SC_RESERVATION_CONFLICT;
        switch (racqa) {
            nvme.RACQA_ACQUIRE => {
                if (self.holder) |h| {
                    if (h == crkey and self.rtype == rtype) return nvme.STATUS_SUCCESS;
                    return nvme.SC_RESERVATION_CONFLICT;
                }
                self.holder = crkey;
                self.rtype = rtype;
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            nvme.RACQA_PREEMPT, nvme.RACQA_PREEMPT_AND_ABORT => {
                if (!self.isRegistered(prkey)) return nvme.SC_INVALID_FIELD;
                self.remove(prkey);
                self.holder = crkey;
                self.rtype = rtype;
                self.preempts += 1;
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            else => return nvme.SC_INVALID_FIELD,
        }
    }

    fn release(self: *Namespace, host_id: [16]u8, cmd: *const nvme.UringCmd) nvme.Status {
        const p = payload(cmd, 8);
        const crkey = std.mem.readInt(u64, p, .little);
        const rtype: u8 = @intCast((cmd.cdw10 >> 8) & 0xff);
        const mine = self.find(host_id) orelse return nvme.SC_RESERVATION_CONFLICT;
        if (mine.key != crkey) return nvme.SC_RESERVATION_CONFLICT;
        if (self.holder) |h| {
            if (h != crkey) return nvme.STATUS_SUCCESS;
            if (self.rtype != rtype) return nvme.SC_INVALID_FIELD;
            self.holder = null;
            self.rtype = 0;
            self.generation += 1;
        }
        return nvme.STATUS_SUCCESS;
    }

    fn report(self: *Namespace, cmd: *const nvme.UringCmd) !nvme.Status {
        const extended = cmd.cdw11 & nvme.EDS != 0;
        if (!extended and self.short_report_inconsistent) return nvme.SC_HOSTID_INCONSISTENT;
        const out = data(cmd);
        // NUMD names the transfer; the buffer must agree with it.
        std.debug.assert(out.len == (@as(usize, cmd.cdw10) + 1) * 4);
        const regs = try self.gpa.alloc(nvme.Registrant, self.registered.items.len);
        defer self.gpa.free(regs);
        for (self.registered.items, regs) |r, *o| o.* = .{ .cntlid = 1, .holds = self.holder != null and self.holder.? == r.key, .rkey = r.key, .host_id = r.host_id };
        nvme.encodeReport(out, extended, self.generation, self.rtype, 0, regs);
        return nvme.STATUS_SUCCESS;
    }

    fn issue(self: *Namespace, host_id: [16]u8, queue: nvme.Queue, cmd: *const nvme.UringCmd) errors.Error!nvme.Completion {
        self.commands += 1;
        self.last = cmd.*;
        self.last_queue = queue;
        if (self.fail_next) |e| {
            self.fail_next = null;
            return e;
        }
        const status: nvme.Status = switch (queue) {
            .admin => switch (cmd.opcode) {
                nvme.OP_ADMIN_IDENTIFY => blk: {
                    const out = data(cmd);
                    @memset(out, 0);
                    if (cmd.cdw10 & 0xff == 0 and out.len == nvme.IDENTIFY_NS_LEN) out[nvme.RESCAP_OFFSET] = self.rescap;
                    break :blk nvme.STATUS_SUCCESS;
                },
                nvme.OP_ADMIN_GET_FEATURES => blk: {
                    if (cmd.cdw10 & 0xff != nvme.FID_HOST_IDENTIFIER) break :blk nvme.SC_INVALID_FIELD;
                    const out = data(cmd);
                    @memset(out, 0);
                    const answer = self.wire_host_id_override orelse host_id;
                    if (!self.hostid_unknown) @memcpy(out, answer[0..out.len]);
                    break :blk nvme.STATUS_SUCCESS;
                },
                else => nvme.SC_INVALID_FIELD,
            },
            .io => switch (cmd.opcode) {
                nvme.OP_RESV_REGISTER => self.register(host_id, cmd) catch return error.OutOfMemory,
                nvme.OP_RESV_REPORT => self.report(cmd) catch return error.OutOfMemory,
                nvme.OP_RESV_ACQUIRE => self.acquire(host_id, cmd),
                nvme.OP_RESV_RELEASE => self.release(host_id, cmd),
                else => nvme.SC_INVALID_FIELD,
            },
        };
        return .{ .status = status };
    }
};

/// The synthetic foreign host of `seedHolder`.
pub const foreign_host: [16]u8 = "FOREIGN-HOST-ID!".*;

/// One host's association with the shared namespace: its wire host id is
/// what FID 0x81 answers and what every registration carries.
pub const Host = struct {
    ns: *Namespace,
    host_id: [16]u8,

    pub fn face(self: *Host) nvme.Namespace {
        return .{ .ctx = self, .issue = issueFn };
    }

    fn issueFn(ctx: *anyopaque, queue: nvme.Queue, cmd: *const nvme.UringCmd) errors.Error!nvme.Completion {
        const self: *Host = @ptrCast(@alignCast(ctx));
        return self.ns.issue(self.host_id, queue, cmd);
    }
};

// ---------------------------------------------------------------------------
// Tests: the fake's own laws through the codec (the byte-exact proof).
// ---------------------------------------------------------------------------

const testing = std.testing;

const host_a: [16]u8 = "host-a-host-a-01".*;
const host_b: [16]u8 = "host-b-host-b-02".*;

fn issueIo(h: *Host, cmd: nvme.UringCmd) !nvme.Status {
    const c = try h.face().issue(h.face().ctx, .io, &cmd);
    return c.status;
}

test "fake laws: an unregistered acquire conflicts; register → acquire WE; the same key+rtype re-acquires idempotently; a changed rtype conflicts; a release under the wrong rtype is refused; unregistering the holder releases" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var p: [16]u8 = undefined;
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try issueIo(&a, nvme.reservationAcquire(1, nvme.RACQA_ACQUIRE, nvme.RTYPE_WRITE_EXCLUSIVE, 0xa, 0, &p)));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_REGISTER | nvme.IEKEY | nvme.CPTPL_PERSIST, 0, 0xa, &p)));
    try testing.expectEqual(nvme.OP_RESV_REGISTER, ns.last.opcode);
    try testing.expectEqual(@as(u32, 1), ns.last.nsid);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationAcquire(1, nvme.RACQA_ACQUIRE, nvme.RTYPE_WRITE_EXCLUSIVE, 0xa, 0, &p)));
    try testing.expectEqual(@as(?u64, 0xa), ns.holder);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationAcquire(1, nvme.RACQA_ACQUIRE, nvme.RTYPE_WRITE_EXCLUSIVE, 0xa, 0, &p)));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try issueIo(&a, nvme.reservationAcquire(1, nvme.RACQA_ACQUIRE, nvme.RTYPE_WERO, 0xa, 0, &p)));
    var r8: [8]u8 = undefined;
    try testing.expectEqual(nvme.SC_INVALID_FIELD, try issueIo(&a, nvme.reservationRelease(1, nvme.RTYPE_WERO, 0xa, &r8)));
    try testing.expect(ns.writeAllowed(host_a));
    try testing.expect(!ns.writeAllowed(host_b));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_UNREGISTER, 0xa, 0, &p)));
    try testing.expectEqual(@as(?u64, null), ns.holder);
    try testing.expectEqual(@as(u32, 1), ns.unregisters);
    try testing.expect(ns.writeAllowed(host_b));
}

test "fake laws: SpecStrict refuses a different key from a registered host, LenientReplace replaces in place and carries holdership; preempt removes the victim and transfers; power_cycle clears everything" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var p: [16]u8 = undefined;
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_REGISTER | nvme.IEKEY, 0, 0x1, &p)));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_REGISTER | nvme.IEKEY, 0, 0x2, &p)));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationAcquire(1, nvme.RACQA_ACQUIRE, nvme.RTYPE_WRITE_EXCLUSIVE, 0x1, 0, &p)));
    ns.semantics = .lenient_replace;
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_REGISTER | nvme.IEKEY, 0, 0x2, &p)));
    try testing.expectEqual(@as(?u64, 0x2), ns.holder);
    try testing.expectEqual(@as(usize, 1), ns.registered.items.len);
    // B registers and preempts A's key: the holder transfers, A is gone.
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&b, nvme.reservationRegister(1, nvme.RREGA_REGISTER | nvme.IEKEY, 0, 0xb, &p)));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try issueIo(&b, nvme.reservationAcquire(1, nvme.RACQA_ACQUIRE, nvme.RTYPE_WRITE_EXCLUSIVE, 0xb, 0, &p)));
    try testing.expectEqual(nvme.SC_INVALID_FIELD, try issueIo(&b, nvme.reservationAcquire(1, nvme.RACQA_PREEMPT, nvme.RTYPE_WRITE_EXCLUSIVE, 0xb, 0x999, &p)));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&b, nvme.reservationAcquire(1, nvme.RACQA_PREEMPT, nvme.RTYPE_WRITE_EXCLUSIVE, 0xb, 0x2, &p)));
    try testing.expectEqual(@as(?u64, 0xb), ns.holder);
    try testing.expectEqual(@as(u32, 1), ns.preempts);
    try testing.expect(ns.find(host_a) == null);
    try testing.expect(!ns.writeAllowed(host_a));
    ns.powerCycle();
    try testing.expectEqual(@as(?u64, null), ns.holder);
    try testing.expectEqual(@as(usize, 0), ns.registered.items.len);
}

test "fake answers: Identify NS byte 31 = rescap; FID 0x81 = the host id (16 B, 8 B low half, zeros when unknown); the report in both forms (0x18 on a fabrics short form); a transport failure passes through once" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    const face = a.face();
    var id: [nvme.IDENTIFY_NS_LEN]u8 = undefined;
    var c = try face.issue(face.ctx, .admin, &nvme.identifyNamespace(1, &id));
    try testing.expectEqual(nvme.STATUS_SUCCESS, c.status);
    try testing.expectEqual(@as(u8, 0x03), id[nvme.RESCAP_OFFSET]);
    try testing.expectEqual(nvme.Queue.admin, ns.last_queue);
    var hid: [16]u8 = undefined;
    c = try face.issue(face.ctx, .admin, &nvme.getHostIdentifier(&hid));
    try testing.expectEqualSlices(u8, &host_a, &hid);
    var hid8: [8]u8 = undefined;
    _ = try face.issue(face.ctx, .admin, &nvme.getHostIdentifier(&hid8));
    try testing.expectEqualSlices(u8, host_a[0..8], &hid8);
    ns.hostid_unknown = true;
    _ = try face.issue(face.ctx, .admin, &nvme.getHostIdentifier(&hid));
    try testing.expect(std.mem.allEqual(u8, &hid, 0));
    try ns.seedHolder(0x55);
    var rep: [64 * 2]u8 = undefined;
    c = try face.issue(face.ctx, .io, &nvme.reservationReport(1, &rep, true));
    try testing.expectEqual(nvme.STATUS_SUCCESS, c.status);
    var r = try nvme.parseReport(testing.allocator, &rep, true);
    defer r.deinit(testing.allocator);
    try testing.expectEqual(@as(?u64, 0x55), r.holder());
    try testing.expectEqualSlices(u8, &foreign_host, &r.registrants[0].host_id);
    ns.short_report_inconsistent = true;
    var short: [24 * 2]u8 = undefined;
    c = try face.issue(face.ctx, .io, &nvme.reservationReport(1, &short, false));
    try testing.expectEqual(nvme.SC_HOSTID_INCONSISTENT, c.status);
    ns.short_report_inconsistent = false;
    c = try face.issue(face.ctx, .io, &nvme.reservationReport(1, &short, false));
    try testing.expectEqual(nvme.STATUS_SUCCESS, c.status);
    ns.fail_next = error.InputOutput;
    try testing.expectError(error.InputOutput, face.issue(face.ctx, .io, &nvme.reservationReport(1, &short, false)));
    c = try face.issue(face.ctx, .io, &nvme.reservationReport(1, &short, false));
    try testing.expectEqual(nvme.STATUS_SUCCESS, c.status);
}
