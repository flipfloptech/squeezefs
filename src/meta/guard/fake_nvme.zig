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
//! replaces it in place (kernel nvmet). The holder is a REGISTRANT — a host
//! id and its key — as nvmet keeps it (`pr->holder`): another host under the
//! holder's key (a fencing registrant, decision 0003 §DG) is not the holder.
//! A preempt naming the holder's key takes the reservation and unregisters
//! every other registrant of the key; one naming another key unregisters
//! every registrant of it, the sender not skipped, the reservation kept.

const std = @import("std");
const nvme = @import("nvme_cmd.zig");
const errors = @import("../../core/errors.zig");

pub const RegisterSemantics = enum { spec_strict, lenient_replace };

pub const Registered = struct { host_id: [16]u8, key: u64 };

/// The one shared namespace every `Host` of a test drives.
pub const Namespace = struct {
    gpa: std.mem.Allocator,
    /// The holder's key and host id (null = no reservation).
    holder: ?u64 = null,
    holder_host: [16]u8 = @splat(0),
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
    /// Fired once, before the Reservation Report that follows
    /// `before_report_skip` more of them: a peer's act between two of a
    /// caller's reports.
    before_report: ?Hook = null,
    before_report_skip: u32 = 0,
    /// Commands that pass before `fail_next` fires.
    fail_skip: u32 = 0,
    /// nvmet 7.3's preempt under a single-holder type: the HOLDER's preempt
    /// only re-types its own hold (`nvmet_pr_preempt` answers `holder ==
    /// reg` before it compares the key) and succeeds, the victim left
    /// registered; with no holder a preempt unregisters by key and takes no
    /// hold. (Under an All Registrants type the target and the spec agree:
    /// the key's registrants go, the reservation stays.)
    holder_preempt_retypes: bool = false,
    /// The last Reservation Acquire (the byte-exact proof of its RACQA).
    last_acquire: nvme.UringCmd = nvme.UringCmd.zeroed(),
    /// A vendor registrant cap: a register that would make the table
    /// larger answers `CAP_STATUS` (a command-specific status, the learn
    /// rule's class) — the emulated capped array (spec §8 D:1047).
    registrant_cap: ?usize = null,
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
        self.holder_host = foreign_host;
        self.rtype = nvme.RTYPE_WRITE_EXCLUSIVE;
        self.generation += 1;
    }

    /// A target without PTPL power-cycled: the reservation and every
    /// registration are gone.
    pub fn powerCycle(self: *Namespace) void {
        self.clearHolder();
        self.registered.clearRetainingCapacity();
        self.generation += 1;
    }

    /// The device's write law: WE ⇒ the holder's host alone; WERO ⇒ any
    /// registrant; no reservation ⇒ anyone.
    pub fn writeAllowed(self: *const Namespace, host_id: [16]u8) bool {
        if (self.holder == null) return true;
        return switch (self.rtype) {
            nvme.RTYPE_WRITE_EXCLUSIVE => self.isHolder(host_id),
            nvme.RTYPE_WERO, nvme.RTYPE_WEAR => self.find(host_id) != null,
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

    /// Is `host_id`'s registrant the holder?
    pub fn isHolder(self: *const Namespace, host_id: [16]u8) bool {
        return self.holder != null and std.mem.eql(u8, &self.holder_host, &host_id);
    }

    fn setHolder(self: *Namespace, key: u64, host_id: [16]u8) void {
        self.holder = key;
        self.holder_host = host_id;
    }

    fn clearHolder(self: *Namespace) void {
        self.holder = null;
        self.holder_host = @splat(0);
        self.rtype = 0;
    }

    /// Every registrant of `key` but `spare`'s (null: none spared) out; the
    /// holder's going releases a single-holder reservation and passes an All
    /// Registrants one to a remaining registrant (`nvmet_pr_unregister_one`).
    fn remove(self: *Namespace, key: u64, spare: ?[16]u8) void {
        var i: usize = 0;
        while (i < self.registered.items.len) {
            const r = self.registered.items[i];
            if (r.key == key and !(spare != null and std.mem.eql(u8, &r.host_id, &spare.?))) {
                _ = self.registered.swapRemove(i);
                if (self.isHolder(r.host_id)) self.holderGone();
            } else i += 1;
        }
    }

    fn holderGone(self: *Namespace) void {
        if (nvme.allRegistrants(self.rtype) and self.registered.items.len != 0) {
            const next = self.registered.items[0];
            self.setHolder(next.key, next.host_id);
        } else self.clearHolder();
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
                            if (self.isHolder(host_id)) self.holder = nrkey;
                        },
                    }
                }
                if (self.registrant_cap) |cap| if (self.registered.items.len >= cap) return CAP_STATUS;
                try self.registered.append(self.gpa, .{ .host_id = host_id, .key = nrkey });
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            nvme.RREGA_UNREGISTER => {
                const m = mine orelse return nvme.SC_RESERVATION_CONFLICT;
                if (!iekey and m.key != crkey) return nvme.SC_RESERVATION_CONFLICT;
                self.removeHost(host_id);
                self.unregisters += 1;
                if (self.isHolder(host_id)) self.holderGone();
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            nvme.RREGA_REPLACE => {
                const m = mine orelse return nvme.SC_RESERVATION_CONFLICT;
                if (!iekey and m.key != crkey) return nvme.SC_RESERVATION_CONFLICT;
                self.removeHost(host_id);
                try self.registered.append(self.gpa, .{ .host_id = host_id, .key = nrkey });
                if (self.isHolder(host_id)) self.holder = nrkey;
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
        self.last_acquire = cmd.*;
        switch (racqa) {
            nvme.RACQA_ACQUIRE => {
                if (self.holder != null) {
                    if (self.isHolder(host_id) and self.rtype == rtype) return nvme.STATUS_SUCCESS;
                    return nvme.SC_RESERVATION_CONFLICT;
                }
                self.setHolder(crkey, host_id);
                self.rtype = rtype;
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            nvme.RACQA_PREEMPT, nvme.RACQA_PREEMPT_AND_ABORT => {
                if (self.holder != null and nvme.allRegistrants(self.rtype) and prkey != 0) {
                    if (!self.isRegistered(prkey)) return nvme.SC_RESERVATION_CONFLICT;
                    self.remove(prkey, null);
                    self.preempts += 1;
                    self.generation += 1;
                    return nvme.STATUS_SUCCESS;
                }
                if (self.holder_preempt_retypes) {
                    if (self.isHolder(host_id)) {
                        self.rtype = rtype;
                        return nvme.STATUS_SUCCESS;
                    }
                    if (self.holder == null) {
                        if (!self.isRegistered(prkey)) return nvme.SC_RESERVATION_CONFLICT;
                        self.remove(prkey, null);
                        self.preempts += 1;
                        self.generation += 1;
                        return nvme.STATUS_SUCCESS;
                    }
                }
                if (!self.isRegistered(prkey)) return nvme.SC_INVALID_FIELD;
                if (self.holder == null or self.holder.? == prkey) {
                    // The holder's key (or no reservation): the reservation
                    // passes to the sender, every other registrant of the
                    // key out.
                    self.remove(prkey, host_id);
                    self.setHolder(crkey, host_id);
                    self.rtype = rtype;
                } else self.remove(prkey, null);
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
        if (self.holder != null) {
            if (!self.isHolder(host_id)) return nvme.STATUS_SUCCESS;
            if (self.rtype != rtype) return nvme.SC_INVALID_FIELD;
            self.clearHolder();
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
        for (self.registered.items, regs) |r, *o| o.* = .{ .cntlid = 1, .holds = self.holder != null and (self.isHolder(r.host_id) or nvme.allRegistrants(self.rtype)), .rkey = r.key, .host_id = r.host_id };
        nvme.encodeReport(out, extended, self.generation, self.rtype, 0, regs);
        return nvme.STATUS_SUCCESS;
    }

    pub const Hook = struct { ctx: *anyopaque, fire: *const fn (ctx: *anyopaque) void };

    fn issue(self: *Namespace, host_id: [16]u8, queue: nvme.Queue, cmd: *const nvme.UringCmd) errors.Error!nvme.Completion {
        if (queue == .io and cmd.opcode == nvme.OP_RESV_REPORT) if (self.before_report) |h| {
            if (self.before_report_skip > 0) {
                self.before_report_skip -= 1;
            } else {
                self.before_report = null;
                h.fire(h.ctx);
            }
        };
        self.commands += 1;
        self.last = cmd.*;
        self.last_queue = queue;
        if (self.fail_next) |e| {
            if (self.fail_skip > 0) {
                self.fail_skip -= 1;
            } else {
                self.fail_next = null;
                return e;
            }
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

/// The capped array's answer: SCT 1 (command-specific), SC 0x80 — a
/// learnable class, never the conflict, never transient.
pub const CAP_STATUS: nvme.Status = 0x180;

/// One host's association with the shared namespace: its wire host id is
/// what FID 0x81 answers and what every registration carries.
pub const Host = struct {
    ns: *Namespace,
    host_id: [16]u8,
    /// Its device closed (a re-join's successor that closed unpublished):
    /// every later command is counted and refused, as a closed fd answers.
    closed: bool = false,
    issued: u32 = 0,
    after_close: u32 = 0,

    pub fn face(self: *Host) nvme.Namespace {
        return .{ .ctx = self, .issue = issueFn };
    }

    fn issueFn(ctx: *anyopaque, queue: nvme.Queue, cmd: *const nvme.UringCmd) errors.Error!nvme.Completion {
        const self: *Host = @ptrCast(@alignCast(ctx));
        if (self.closed) {
            self.after_close += 1;
            return error.NotADeviceOrFile;
        }
        self.issued += 1;
        return self.ns.issue(self.host_id, queue, cmd);
    }
};

// ---------------------------------------------------------------------------
// Tests: the fake's own laws through the codec (the byte-exact proof).
// ---------------------------------------------------------------------------

const testing = std.testing;

const host_a: [16]u8 = "host-a-host-a-01".*;
const host_b: [16]u8 = "host-b-host-b-02".*;
const host_c: [16]u8 = "host-c-host-c-03".*;
const host_d: [16]u8 = "host-d-host-d-04".*;

fn issueIo(h: *Host, cmd: nvme.UringCmd) !nvme.Status {
    const c = try h.face().issue(h.face().ctx, .io, &cmd);
    return c.status;
}

fn registerKey(h: *Host, key: u64) !nvme.Status {
    var p: [16]u8 = undefined;
    return issueIo(h, nvme.reservationRegister(1, nvme.RREGA_REGISTER | nvme.IEKEY, 0, key, &p));
}

fn acquireAs(h: *Host, racqa: u32, rtype: u8, crkey: u64, prkey: u64) !nvme.Status {
    var p: [16]u8 = undefined;
    return issueIo(h, nvme.reservationAcquire(1, racqa, rtype, crkey, prkey, &p));
}

/// Register-Replace with IEKEY 0 and CRKEY = NRKEY = `key`: the drain barrier's command (P15).
fn replaceSame(h: *Host, key: u64) !nvme.Status {
    var p: [16]u8 = undefined;
    return issueIo(h, nvme.reservationRegister(1, nvme.RREGA_REPLACE, key, key, &p));
}

test "fake laws (nvmet 7.3, decision 0004 §2.2 P4/P8/P3): a preempt-all — Preempt-and-Abort PRKEY 0 RTYPE 5 under an All-Registrants reservation — makes the sender the holder pointer and unregisters every other host id; the sender stays, the reservation is kept, the generation moves by one; a preempted host can neither preempt nor acquire (P8); re-registered it is no holder pointer, so its acquire conflicts (P3)" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var c: Host = .{ .ns = &ns, .host_id = host_c };
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&c, 0xc));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    const gen = ns.generation;
    const s = try acquireAs(&b, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xb, 0);
    try testing.expectEqual(@as(usize, 1), ns.registered.items.len);
    try testing.expectEqual(nvme.STATUS_SUCCESS, s);
    try testing.expect(ns.isHolder(host_b));
    try testing.expectEqual(@as(?u64, 0xb), ns.holder);
    try testing.expectEqual(nvme.RTYPE_WEAR, ns.rtype);
    try testing.expectEqual(gen + 1, ns.generation);
    try testing.expect(ns.writeAllowed(host_b) and !ns.writeAllowed(host_a) and !ns.writeAllowed(host_c));
    // P8: the preempted host's commands conflict.
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try acquireAs(&a, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xa, 0));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    // P3: re-registered, A writes by registration alone, but the holder
    // pointer is B's, so A's same-type acquire conflicts.
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    try testing.expect(ns.writeAllowed(host_a));
    try testing.expect(ns.isHolder(host_b));
}

test "fake laws (nvmet 7.3, decision 0004 §2.2 P6): on an unreserved namespace a preempt-all conflicts (PRKEY 0 matches nobody) and changes nothing; a PRKEY k preempt unregisters every registrant of k, the sender not skipped, and takes no hold" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    const gen = ns.generation;
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try acquireAs(&a, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xa, 0));
    try testing.expectEqual(@as(usize, 2), ns.registered.items.len);
    try testing.expectEqual(@as(?u64, null), ns.holder);
    try testing.expectEqual(gen, ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_PREEMPT, nvme.RTYPE_WEAR, 0xa, 0xb));
    try testing.expect(ns.find(host_b) == null and ns.find(host_a) != null);
    try testing.expectEqual(@as(?u64, null), ns.holder);
    try testing.expectEqual(@as(u8, 0), ns.rtype);
    try testing.expectEqual(gen + 1, ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xa, 0xa));
    try testing.expectEqual(@as(usize, 0), ns.registered.items.len);
    try testing.expectEqual(@as(?u64, null), ns.holder);
}

test "fake laws (nvmet 7.3, decision 0004 §2.2 P10): under All-Registrants the holder pointer passes to the FIRST remaining registrant in registration order when the holder unregisters; the last registrant's unregister ends the reservation" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var c: Host = .{ .ns = &ns, .host_id = host_c };
    var p: [16]u8 = undefined;
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&c, 0xc));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_UNREGISTER, 0xa, 0, &p)));
    try testing.expectEqual(@as(?u64, 0xb), ns.holder);
    try testing.expect(ns.isHolder(host_b));
    try testing.expectEqual(nvme.RTYPE_WEAR, ns.rtype);
    try testing.expect(!ns.writeAllowed(host_a) and ns.writeAllowed(host_b) and ns.writeAllowed(host_c));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&b, nvme.reservationRegister(1, nvme.RREGA_UNREGISTER, 0xb, 0, &p)));
    try testing.expectEqual(@as(?u64, 0xc), ns.holder);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&c, nvme.reservationRegister(1, nvme.RREGA_UNREGISTER, 0xc, 0, &p)));
    try testing.expectEqual(@as(?u64, null), ns.holder);
    try testing.expectEqual(@as(u8, 0), ns.rtype);
    try testing.expect(ns.writeAllowed(host_a));
}

test "fake laws (nvmet 7.3, decision 0004 §2.2 P15): Register-Replace with IEKEY 0 and CRKEY = NRKEY = own key succeeds for a registrant, keeping its place in registration order and the holder pointer, and moves the generation; an unregistered host's conflicts and changes nothing" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var c: Host = .{ .ns = &ns, .host_id = host_c };
    var d: Host = .{ .ns = &ns, .host_id = host_d };
    var p: [16]u8 = undefined;
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&c, 0xc));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    const gen = ns.generation;
    try testing.expectEqual(nvme.STATUS_SUCCESS, try replaceSame(&b, 0xb));
    try testing.expectEqual(gen + 1, ns.generation);
    try testing.expectEqualSlices(u8, &host_b, &ns.registered.items[1].host_id);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try replaceSame(&a, 0xa));
    try testing.expect(ns.isHolder(host_a));
    try testing.expectEqual(gen + 2, ns.generation);
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try replaceSame(&d, 0xd));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try issueIo(&b, nvme.reservationRegister(1, nvme.RREGA_REPLACE, 0xbad, 0xb, &p)));
    try testing.expectEqual(gen + 2, ns.generation);
    try testing.expectEqual(@as(usize, 3), ns.registered.items.len);
    // The places kept: A's unregister hands the pointer to B, not C.
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_UNREGISTER, 0xa, 0, &p)));
    try testing.expectEqual(@as(?u64, 0xb), ns.holder);
}

test "fake laws (nvmet 7.3, decision 0004 §2.2 P16): Register (an idempotent same-key one too), Unregister, Replace and Preempt move the generation on success; an Acquire and a Release do not" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var p: [16]u8 = undefined;
    var r8: [8]u8 = undefined;
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(@as(u32, 1), ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(@as(u32, 2), ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    try testing.expectEqual(@as(u32, 2), ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationRelease(1, nvme.RTYPE_WEAR, 0xa, &r8)));
    try testing.expectEqual(@as(?u64, null), ns.holder);
    try testing.expectEqual(@as(u32, 2), ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(@as(u32, 3), ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try replaceSame(&b, 0xb));
    try testing.expectEqual(@as(u32, 4), ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_PREEMPT, nvme.RTYPE_WEAR, 0xa, 0xb));
    try testing.expectEqual(@as(u32, 5), ns.generation);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_UNREGISTER, 0xa, 0, &p)));
    try testing.expectEqual(@as(u32, 6), ns.generation);
    // A refused command moves nothing.
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try issueIo(&a, nvme.reservationRegister(1, nvme.RREGA_UNREGISTER, 0xa, 0, &p)));
    try testing.expectEqual(@as(u32, 6), ns.generation);
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
    // A key nobody holds: nvmet's by-key arm finds no registrant and conflicts.
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try issueIo(&b, nvme.reservationAcquire(1, nvme.RACQA_PREEMPT, nvme.RTYPE_WRITE_EXCLUSIVE, 0xb, 0x999, &p)));
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

/// Probes a victim's I/O at each step of a preempt-and-abort (the seam
/// between the ref kill and the unregister is F7b's unit form).
const DrainSeam = struct {
    a: *Host,
    b: *Host,
    /// A's write admitted before the preempt-all: the drain waits for it.
    ticket: u32,
    steps: [4]AbortStep = undefined,
    victims: [4][16]u8 = undefined,
    n: usize = 0,
    killed_write: ?IoAnswer = null,
    killed_read: ?IoAnswer = null,
    unreg_write: ?IoAnswer = null,
    unreg_read: ?IoAnswer = null,
    drain_read: ?IoAnswer = null,
    drain_regctl: ?u16 = null,
    drain_gen: ?u32 = null,
    pr_sem_in_drain: bool = false,
    b_write: ?IoAnswer = null,

    fn fire(ctx: *anyopaque, step: AbortStep, victim: ?[16]u8) void {
        const s: *DrainSeam = @ptrCast(@alignCast(ctx));
        s.steps[s.n] = step;
        s.victims[s.n] = victim orelse @splat(0);
        s.n += 1;
        switch (step) {
            .killed => {
                s.killed_write = s.a.submit(.write);
                s.killed_read = s.a.submit(.read);
            },
            .unregistered => {
                s.unreg_write = s.a.submit(.write);
                s.unreg_read = s.a.submit(.read);
            },
            .draining => {
                s.pr_sem_in_drain = s.a.ns.prSemHeld();
                s.drain_read = s.a.submit(.read);
                var rep: [64 * 4]u8 = undefined;
                const face = s.b.face();
                const c = face.issue(face.ctx, .io, &nvme.reservationReport(1, &rep, true)) catch return;
                if (c.status == nvme.STATUS_SUCCESS) {
                    s.drain_regctl = nvme.reportRegctl(&rep, true);
                    s.drain_gen = std.mem.readInt(u32, rep[0..4], .little);
                }
                // A non-victim's write is admitted and lands; then the
                // victim's admitted write completes, and the drain with it.
                s.b_write = s.b.submit(.write);
                if (s.b_write) |w| if (w == .admitted) s.a.ns.complete(w.admitted);
                s.a.ns.complete(s.ticket);
            },
        }
    }
};

test "fake drain (nvmet 7.3, decision 0004 §2.2 P9, P9a–P9c, P14): a preempt-all kills each victim's refs, then unregisters it; a victim's write in that instant gets INTERNAL (P9c), after it a conflict (P9a); its read gets INTERNAL for the whole drain (P9b); a Report during the drain reads the state after the unregistration (P14); the command completes only once the victim's admitted write completed; then reads are admitted again and writes stay refused" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    const w = a.submit(.write);
    try testing.expect(w == .admitted);
    var seam: DrainSeam = .{ .a = &a, .b = &b, .ticket = w.admitted };
    ns.abort_hook = .{ .ctx = &seam, .fire = DrainSeam.fire };
    const gen = ns.generation;
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&b, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xb, 0));
    try testing.expectEqual(@as(usize, 3), seam.n);
    try testing.expectEqualSlices(AbortStep, &.{ .killed, .unregistered, .draining }, seam.steps[0..3]);
    try testing.expectEqualSlices(u8, &host_a, &seam.victims[0]);
    try testing.expectEqualSlices(u8, &host_a, &seam.victims[1]);
    try testing.expectEqual(@as(?IoAnswer, .internal), seam.killed_write);
    try testing.expectEqual(@as(?IoAnswer, .internal), seam.killed_read);
    try testing.expectEqual(@as(?IoAnswer, .conflict), seam.unreg_write);
    try testing.expectEqual(@as(?IoAnswer, .internal), seam.unreg_read);
    try testing.expectEqual(@as(?IoAnswer, .internal), seam.drain_read);
    try testing.expect(seam.pr_sem_in_drain);
    try testing.expectEqual(@as(?u16, 1), seam.drain_regctl);
    try testing.expectEqual(@as(?u32, gen + 1), seam.drain_gen);
    try testing.expect(seam.b_write != null and seam.b_write.? == .admitted);
    try testing.expectEqual(@as(u32, 2), ns.landed);
    try testing.expect(!ns.prSemHeld());
    // The refs live again: a read is admitted; a write stays refused (P9a).
    const r = a.submit(.read);
    try testing.expect(r == .admitted);
    ns.complete(r.admitted);
    try testing.expectEqual(IoAnswer.conflict, a.submit(.write));
}

test "fake drain (nvmet 7.3, decision 0004 §2.2 P5, P9): a PRKEY k preempt-and-abort under All-Registrants kills and unregisters only k's host, keeps the reservation and the holder pointer, and drains k's admitted command" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var c: Host = .{ .ns = &ns, .host_id = host_c };
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&c, 0xc));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    const w = c.submit(.write);
    try testing.expect(w == .admitted);
    var seam: DrainSeam = .{ .a = &c, .b = &b, .ticket = w.admitted };
    ns.abort_hook = .{ .ctx = &seam, .fire = DrainSeam.fire };
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&b, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xb, 0xc));
    try testing.expectEqualSlices(AbortStep, &.{ .killed, .unregistered, .draining }, seam.steps[0..seam.n]);
    try testing.expectEqualSlices(u8, &host_c, &seam.victims[0]);
    try testing.expect(ns.find(host_c) == null and ns.find(host_a) != null and ns.find(host_b) != null);
    try testing.expect(ns.isHolder(host_a));
    try testing.expectEqual(nvme.RTYPE_WEAR, ns.rtype);
    try testing.expectEqual(@as(?u16, 2), seam.drain_regctl);
}

const Issued = struct {
    host: *Host,
    cmd: nvme.UringCmd,
    payload: [16]u8 = undefined,
    status: nvme.Status = 0xffff,
    done: std.atomic.Value(bool) = .init(false),

    fn run(self: *Issued) void {
        self.status = issueIo(self.host, self.cmd) catch 0xfffe;
        self.done.store(true, .release);
    }
};

test "fake drain (nvmet 7.3, decision 0004 §2.2 P9, P9d, P14, P15): the preempt-all's issuer waits for the victim's admitted write; a Register-Replace (the drain barrier) issued meanwhile parks on pr_sem until the drain ends; a Report does not wait; both complete once the write completes" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var b2: Host = .{ .ns = &ns, .host_id = host_b };
    var c: Host = .{ .ns = &ns, .host_id = host_c };
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    const w = a.submit(.write);
    try testing.expect(w == .admitted);
    const gen = ns.generation;
    var pa: Issued = .{ .host = &b, .cmd = undefined };
    pa.cmd = nvme.reservationAcquire(1, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xb, 0, &pa.payload);
    const ts = try std.Thread.spawn(.{}, Issued.run, .{&pa});
    ns.awaitDrainWaiting();
    try testing.expect(!pa.done.load(.acquire));
    var bar: Issued = .{ .host = &b2, .cmd = undefined };
    bar.cmd = nvme.reservationRegister(1, nvme.RREGA_REPLACE, 0xb, 0xb, &bar.payload);
    const tb = try std.Thread.spawn(.{}, Issued.run, .{&bar});
    ns.awaitPrWaiters(1);
    try testing.expect(!bar.done.load(.acquire));
    // P14: a Report answers during the drain.
    var rep: [64 * 4]u8 = undefined;
    const face = c.face();
    const rc = try face.issue(face.ctx, .io, &nvme.reservationReport(1, &rep, true));
    try testing.expectEqual(nvme.STATUS_SUCCESS, rc.status);
    try testing.expectEqual(@as(?u16, 1), nvme.reportRegctl(&rep, true));
    try testing.expect(!pa.done.load(.acquire) and !bar.done.load(.acquire));
    ns.complete(w.admitted);
    ts.join();
    tb.join();
    try testing.expectEqual(nvme.STATUS_SUCCESS, pa.status);
    try testing.expectEqual(nvme.STATUS_SUCCESS, bar.status);
    try testing.expectEqual(gen + 2, ns.generation);
    try testing.expectEqual(@as(u32, 1), ns.landed);
}

test "fake answers (decision 0004 §2.2 P15a): the holder pointer's Register-Replace answers INTERNAL while an injected GFP_ATOMIC copy failure lasts, changing nothing; a non-holder's Replace is untouched" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    ns.replace_holder_internal = 1;
    const gen = ns.generation;
    try testing.expectEqual(nvme.SC_INTERNAL, try replaceSame(&a, 0xa));
    try testing.expectEqual(gen, ns.generation);
    try testing.expect(ns.isHolder(host_a));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try replaceSame(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try replaceSame(&a, 0xa));
    try testing.expectEqual(gen + 2, ns.generation);
}

test "fake answer modes (decision 0004 §4.4.6, KD22): nvmet answers a non-holder registrant's same-type All-Registrants acquire with a conflict; the conformant mode answers success; neither changes the reservation, the holder pointer, the registrants or the generation; an unregistered host's acquire conflicts in both (P8)" {
    var ns = Namespace.init(testing.allocator);
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var c: Host = .{ .ns = &ns, .host_id = host_c };
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try registerKey(&b, 0xb));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0));
    const gen = ns.generation;
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try acquireAs(&b, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xb, 0));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try acquireAs(&c, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xc, 0));
    ns.conformant_all_registrants = true;
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&b, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xb, 0));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try acquireAs(&b, nvme.RACQA_ACQUIRE, nvme.RTYPE_WERO, 0xb, 0));
    try testing.expectEqual(nvme.SC_RESERVATION_CONFLICT, try acquireAs(&c, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xc, 0));
    try testing.expectEqual(gen, ns.generation);
    try testing.expect(ns.isHolder(host_a));
    try testing.expectEqual(nvme.RTYPE_WEAR, ns.rtype);
    try testing.expectEqual(@as(usize, 2), ns.registered.items.len);
}
