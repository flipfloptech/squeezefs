//! The FAKE NVMe namespace (spec `guard_claims.md` §5.2; the reference's
//! `FakeNvmeNamespace`, R:1173-1517): an in-RAM controller that answers the
//! guard's opcodes by DECODING the same 72 B command the device would — the
//! opcode, CDW10/CDW11 fields and the LE payload at `addr` — with the real
//! status codes, including Reservation Conflict. Every ladder leg runs on a
//! file substrate against it; the real device path is exercised only where
//! a `/dev/ng*` exists (rung 8 brings nvmet-loop).
//!
//! Laws (R:1385-1517; nvmet 7.3's `pr.c`, decision 0004 §2.2): an
//! unregistered host's acquire conflicts; a re-acquire of the same key +
//! rtype by the holder is idempotent; a changed rtype conflicts; a release
//! under the wrong rtype is refused (Invalid Field); `SpecStrict` register
//! refuses a different key from an already registered host (SPDK, nvmet),
//! `LenientReplace` replaces it in place. The holder is a REGISTRANT — a
//! host id and its key — as nvmet keeps it (`pr->holder`, the holder
//! pointer): another host under the holder's key (a fencing registrant,
//! decision 0003 §DG) is not the holder, and a registrant that is not the
//! pointer conflicts on an acquire even under All Registrants (P3; the
//! `conformant_all_registrants` mode answers as the specification does).
//! Registrants keep their registration order (P10: an All Registrants
//! holder's leaving hands the pointer to the first remaining one; a
//! Replace keeps its place). The generation moves on every successful
//! Register, Unregister, Replace and Preempt, never on an Acquire or a
//! Release (P16). Preempt follows `nvmet_pr_preempt`: no reservation ⇒
//! every registrant of PRKEY goes, the sender included, a PRKEY nobody holds
//! conflicts and no hold is taken (P6); All Registrants with PRKEY 0 ⇒ the
//! sender becomes the holder pointer with the command's rtype and every
//! other host id goes (P4); All Registrants with PRKEY k ⇒ every registrant
//! of k goes, the reservation kept (P5); a single-holder type ⇒ the
//! holder's key moves the hold, another key's registrants go, PRKEY 0 is
//! Invalid Field (P7; `holder_preempt_retypes` adds nvmet's quirk arm).
//!
//! I/O and the drain (P2, P9): a host's read or write is SUBMITTED — the
//! access check first (a write from a host with no registrant under a
//! reservation that needs one conflicts: EBADE), then its per-controller
//! ref (dead ⇒ INTERNAL: EIO after the host's retries) — and an admitted
//! command stays in flight until the test completes it. A Preempt-and-Abort
//! kills each victim's ref, then unregisters it (the `abort_hook` fires
//! between and after, outside the lock), then holds pr_sem until every
//! admitted command of a killed host has completed; only then do the refs
//! live again and the command complete. Register, Acquire and Release wait
//! for pr_sem (P9d); a Report does not (P14). Commands from several threads
//! are serialized by the namespace's lock; tests pass `std.testing.io`.

const std = @import("std");
const nvme = @import("nvme_cmd.zig");
const errors = @import("../../core/errors.zig");

pub const RegisterSemantics = enum { spec_strict, lenient_replace };

pub const Registered = struct { host_id: [16]u8, key: u64 };

pub const IoOp = enum { read, write };

/// A submitted command's answer at the target's parse.
pub const IoAnswer = union(enum) {
    /// In flight until `Namespace.complete(ticket)`.
    admitted: u32,
    /// The access check: Reservation Conflict + DNR — EBADE (P2, P9a).
    conflict,
    /// The per-controller ref is dead: `NVME_SC_INTERNAL` without DNR — EIO
    /// after the host's retries (P9b, P9c).
    internal,
};

/// A Preempt-and-Abort's steps, in order (P9): per victim host its refs
/// killed, then its registration gone; then the drain.
pub const AbortStep = enum { killed, unregistered, draining };

/// Fired outside the namespace's lock (a test issues commands from it);
/// `victim` is null at `.draining`.
pub const AbortHook = struct { ctx: *anyopaque, fire: *const fn (ctx: *anyopaque, step: AbortStep, victim: ?[16]u8) void };

/// A test fake's bound: more admitted commands at once is a test's bug.
const MAX_IN_FLIGHT = 64;

const InFlight = struct { ticket: u32, host_id: [16]u8 };

/// The one shared namespace every `Host` of a test drives.
pub const Namespace = struct {
    gpa: std.mem.Allocator,
    /// The holder's key and host id (null = no reservation).
    holder: ?u64 = null,
    holder_host: [16]u8 = @splat(0),
    rtype: u8 = 0,
    /// In registration order (nvmet's `registrant_list`).
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
    /// registered. Off: the specification's arms (the holder's preempt of
    /// another key unregisters that key's registrants).
    holder_preempt_retypes: bool = false,
    /// A spec-conformant target (decision 0004 §4.4.6, KD22): every All
    /// Registrants registrant holds, so its same-type acquire succeeds and
    /// changes nothing. Off: nvmet's holder pointer (P3).
    conformant_all_registrants: bool = false,
    /// The holder pointer's next Register-Replaces answer INTERNAL (nvmet's
    /// `GFP_ATOMIC` copy failing, P15a), changing nothing; counts down.
    replace_holder_internal: u32 = 0,
    /// The last Reservation Acquire (the byte-exact proof of its RACQA) and
    /// its payload's keys.
    last_acquire: nvme.UringCmd = nvme.UringCmd.zeroed(),
    last_acquire_crkey: u64 = 0,
    last_acquire_prkey: u64 = 0,
    /// The last Reservation Register's payload keys.
    last_register_crkey: u64 = 0,
    last_register_nrkey: u64 = 0,
    /// A vendor registrant cap: a register that would make the table
    /// larger answers `CAP_STATUS` (a command-specific status, the learn
    /// rule's class) — the emulated capped array (spec §8 D:1047).
    registrant_cap: ?usize = null,
    generation: u32 = 0,
    /// Successful Preempt commands (every arm).
    preempts: u32 = 0,
    unregisters: u32 = 0,
    commands: u32 = 0,
    /// Admitted commands completed.
    landed: u32 = 0,
    /// The last decoded command (the byte-exact proof the tests read).
    last: nvme.UringCmd = nvme.UringCmd.zeroed(),
    last_queue: nvme.Queue = .admin,
    /// Fired at each step of a Preempt-and-Abort (P9's seams).
    abort_hook: ?AbortHook = null,

    io: std.Io = std.testing.io,
    /// Serializes every command, I/O submission and completion.
    mu: std.Io.Mutex = .init,
    /// Broadcast on every change a waiter waits for: pr_sem's release, a
    /// completion, a parked PR command, a drain that began waiting.
    changed: std.Io.Condition = .init,
    /// pr_sem held by a Preempt-and-Abort while it runs outside the lock
    /// (its hooks and its drain).
    pr_sem_held: bool = false,
    /// PR commands parked on pr_sem.
    pr_waiters: u32 = 0,
    /// A Preempt-and-Abort waits for a killed host's admitted commands.
    drain_waiting: bool = false,
    /// Hosts whose per-controller refs are dead (P9: until the drain ends).
    killed: std.ArrayList([16]u8) = .empty,
    in_flight: [MAX_IN_FLIGHT]InFlight = undefined,
    in_flight_n: usize = 0,
    next_ticket: u32 = 1,

    pub fn init(gpa: std.mem.Allocator) Namespace {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *Namespace) void {
        self.registered.deinit(self.gpa);
        self.killed.deinit(self.gpa);
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
        self.lock();
        defer self.unlock();
        self.clearHolder();
        self.registered.clearRetainingCapacity();
        self.generation += 1;
    }

    /// The device's write law: WE ⇒ the holder's host alone; WERO, WEAR ⇒
    /// any registrant; no reservation ⇒ anyone.
    pub fn writeAllowed(self: *const Namespace, host_id: [16]u8) bool {
        if (self.holder == null) return true;
        return switch (self.rtype) {
            nvme.RTYPE_WRITE_EXCLUSIVE => self.isHolder(host_id),
            nvme.RTYPE_WERO, nvme.RTYPE_WEAR => self.find(host_id) != null,
            else => false,
        };
    }

    pub fn find(self: *const Namespace, host_id: [16]u8) ?Registered {
        const i = self.indexOf(host_id) orelse return null;
        return self.registered.items[i];
    }

    fn indexOf(self: *const Namespace, host_id: [16]u8) ?usize {
        for (self.registered.items, 0..) |r, i| if (std.mem.eql(u8, &r.host_id, &host_id)) return i;
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

    /// pr_sem is held by a running Preempt-and-Abort.
    pub fn prSemHeld(self: *Namespace) bool {
        self.lock();
        defer self.unlock();
        return self.pr_sem_held;
    }

    /// Block until `n` PR commands are parked on pr_sem.
    pub fn awaitPrWaiters(self: *Namespace, n: u32) void {
        self.lock();
        defer self.unlock();
        while (self.pr_waiters < n) self.changed.waitUncancelable(self.io, &self.mu);
    }

    /// Block until a Preempt-and-Abort waits for a victim's admitted command.
    pub fn awaitDrainWaiting(self: *Namespace) void {
        self.lock();
        defer self.unlock();
        while (!self.drain_waiting) self.changed.waitUncancelable(self.io, &self.mu);
    }

    /// An admitted command completes (it lands).
    pub fn complete(self: *Namespace, ticket: u32) void {
        self.lock();
        defer self.unlock();
        const i = for (self.in_flight[0..self.in_flight_n], 0..) |f, i| {
            if (f.ticket == ticket) break i;
        } else @panic("fake namespace: completing a command that is not in flight");
        self.in_flight[i] = self.in_flight[self.in_flight_n - 1];
        self.in_flight_n -= 1;
        self.landed += 1;
        self.changed.broadcast(self.io);
    }

    fn lock(self: *Namespace) void {
        self.mu.lockUncancelable(self.io);
    }

    fn unlock(self: *Namespace) void {
        self.mu.unlock(self.io);
    }

    fn submit(self: *Namespace, host_id: [16]u8, op: IoOp) IoAnswer {
        self.lock();
        defer self.unlock();
        // The access check at parse (P2), then the per-controller ref (P9).
        if (op == .write and !self.writeAllowed(host_id)) return .conflict;
        if (self.isKilled(host_id)) return .internal;
        if (self.in_flight_n == MAX_IN_FLIGHT) @panic("fake namespace: more than MAX_IN_FLIGHT commands in flight");
        const t = self.next_ticket;
        self.next_ticket += 1;
        self.in_flight[self.in_flight_n] = .{ .ticket = t, .host_id = host_id };
        self.in_flight_n += 1;
        return .{ .admitted = t };
    }

    fn isKilled(self: *const Namespace, host_id: [16]u8) bool {
        for (self.killed.items) |k| if (std.mem.eql(u8, &k, &host_id)) return true;
        return false;
    }

    fn killedAdmitted(self: *const Namespace) bool {
        for (self.in_flight[0..self.in_flight_n]) |f| if (self.isKilled(f.host_id)) return true;
        return false;
    }

    /// Register, Acquire and Release wait for pr_sem (P9d).
    fn awaitPrSem(self: *Namespace) void {
        while (self.pr_sem_held) {
            self.pr_waiters += 1;
            self.changed.broadcast(self.io);
            self.changed.waitUncancelable(self.io, &self.mu);
            self.pr_waiters -= 1;
        }
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

    /// The registrant at `i` out, its place closed (`list_del_rcu`); the
    /// holder's going releases a single-holder reservation and passes an
    /// All Registrants one to the first remaining registrant
    /// (`nvmet_pr_unregister_one`, P10).
    fn dropAt(self: *Namespace, i: usize) void {
        const r = self.registered.orderedRemove(i);
        if (self.isHolder(r.host_id)) self.holderGone();
    }

    fn holderGone(self: *Namespace) void {
        if (nvme.allRegistrants(self.rtype) and self.registered.items.len != 0) {
            const next = self.registered.items[0];
            self.setHolder(next.key, next.host_id);
        } else self.clearHolder();
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
        self.last_register_crkey = crkey;
        self.last_register_nrkey = nrkey;
        const rrega = cmd.cdw10 & 0x7;
        const iekey = cmd.cdw10 & nvme.IEKEY != 0;
        if (self.cptpl_invalid_field and (cmd.cdw10 & nvme.CPTPL_PERSIST != 0 or iekey)) return nvme.SC_INVALID_FIELD;
        const mine = self.indexOf(host_id);
        switch (rrega) {
            nvme.RREGA_REGISTER => {
                if (mine) |i| {
                    const m = self.registered.items[i];
                    if (m.key != nrkey) switch (self.semantics) {
                        .spec_strict => return nvme.SC_RESERVATION_CONFLICT,
                        .lenient_replace => {
                            self.registered.items[i].key = nrkey;
                            if (self.isHolder(host_id)) self.holder = nrkey;
                        },
                    };
                    self.generation += 1;
                    return nvme.STATUS_SUCCESS;
                }
                if (self.registrant_cap) |cap| if (self.registered.items.len >= cap) return CAP_STATUS;
                try self.registered.append(self.gpa, .{ .host_id = host_id, .key = nrkey });
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            nvme.RREGA_UNREGISTER => {
                const i = mine orelse return nvme.SC_RESERVATION_CONFLICT;
                if (!iekey and self.registered.items[i].key != crkey) return nvme.SC_RESERVATION_CONFLICT;
                self.dropAt(i);
                self.unregisters += 1;
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            nvme.RREGA_REPLACE => {
                // `nvmet_pr_replace` (P15): the registrant rewritten in its
                // place; the holder pointer's is a GFP_ATOMIC copy (P15a).
                if (mine) |i| {
                    if (!iekey and self.registered.items[i].key != crkey) return nvme.SC_RESERVATION_CONFLICT;
                    if (self.isHolder(host_id) and self.replace_holder_internal > 0) {
                        self.replace_holder_internal -= 1;
                        return nvme.SC_INTERNAL;
                    }
                    self.registered.items[i].key = nrkey;
                    if (self.isHolder(host_id)) self.holder = nrkey;
                    self.generation += 1;
                    return nvme.STATUS_SUCCESS;
                }
                if (!iekey) return nvme.SC_RESERVATION_CONFLICT;
                if (nrkey == 0) return nvme.SC_INVALID_FIELD;
                try self.registered.append(self.gpa, .{ .host_id = host_id, .key = nrkey });
                self.generation += 1;
                return nvme.STATUS_SUCCESS;
            },
            else => return nvme.SC_INVALID_FIELD,
        }
    }

    fn acquire(self: *Namespace, host_id: [16]u8, cmd: *const nvme.UringCmd) !nvme.Status {
        const p = payload(cmd, 16);
        const crkey = std.mem.readInt(u64, p[0..8], .little);
        const prkey = std.mem.readInt(u64, p[8..16], .little);
        const racqa = cmd.cdw10 & 0x7;
        const rtype: u8 = @intCast((cmd.cdw10 >> 8) & 0xff);
        if (cmd.cdw10 & nvme.IEKEY != 0 or rtype < nvme.RTYPE_WRITE_EXCLUSIVE or rtype > nvme.RTYPE_WEAR + 1) return nvme.SC_INVALID_FIELD;
        // P8: the sender must be registered under (its host id, CRKEY).
        const mine = self.find(host_id) orelse return nvme.SC_RESERVATION_CONFLICT;
        if (mine.key != crkey) return nvme.SC_RESERVATION_CONFLICT;
        self.last_acquire = cmd.*;
        self.last_acquire_crkey = crkey;
        self.last_acquire_prkey = prkey;
        return switch (racqa) {
            nvme.RACQA_ACQUIRE => self.hold(host_id, crkey, rtype),
            nvme.RACQA_PREEMPT => self.preempt(host_id, crkey, prkey, rtype, false),
            nvme.RACQA_PREEMPT_AND_ABORT => self.preempt(host_id, crkey, prkey, rtype, true),
            else => nvme.SC_INVALID_FIELD,
        };
    }

    /// `nvmet_pr_acquire` (P3): only the holder pointer re-acquires (same
    /// rtype); the conformant mode lets every All Registrants registrant.
    fn hold(self: *Namespace, host_id: [16]u8, crkey: u64, rtype: u8) nvme.Status {
        if (self.holder != null) {
            if (self.isHolder(host_id)) return if (self.rtype == rtype) nvme.STATUS_SUCCESS else nvme.SC_RESERVATION_CONFLICT;
            if (self.conformant_all_registrants and nvme.allRegistrants(self.rtype) and self.rtype == rtype) return nvme.STATUS_SUCCESS;
            return nvme.SC_RESERVATION_CONFLICT;
        }
        self.setHolder(crkey, host_id);
        self.rtype = rtype;
        return nvme.STATUS_SUCCESS;
    }

    /// `nvmet_pr_preempt`'s arms (P4–P7), then the abort (P9).
    fn preempt(self: *Namespace, sender: [16]u8, crkey: u64, prkey: u64, rtype: u8, abort: bool) !nvme.Status {
        var victims: std.ArrayList([16]u8) = .empty;
        defer victims.deinit(self.gpa);
        var take_hold = false;
        var retype = false;
        if (self.holder == null) {
            if (!try self.byKey(&victims, prkey, null)) return nvme.SC_RESERVATION_CONFLICT;
        } else if (nvme.allRegistrants(self.rtype)) {
            if (prkey == 0) {
                // P4: the sender is made the holder first, then every other
                // host id goes.
                take_hold = true;
                retype = true;
                for (self.registered.items) |r| if (!std.mem.eql(u8, &r.host_id, &sender)) try victims.append(self.gpa, r.host_id);
            } else if (!try self.byKey(&victims, prkey, null)) return nvme.SC_RESERVATION_CONFLICT;
        } else if (self.holder_preempt_retypes and self.isHolder(sender)) {
            retype = true;
        } else if (self.holder.? == prkey) {
            take_hold = true;
            retype = true;
            _ = try self.byKey(&victims, prkey, sender);
        } else if (prkey != 0) {
            if (!try self.byKey(&victims, prkey, null)) return nvme.SC_RESERVATION_CONFLICT;
        } else return nvme.SC_INVALID_FIELD;
        // No fallible bookkeeping after holder/ref/semaphore effects begin.
        if (abort) try self.killed.ensureUnusedCapacity(self.gpa, victims.items.len);
        if (take_hold) self.setHolder(crkey, sender);
        if (retype) self.rtype = rtype;
        self.unregisterVictims(victims.items, abort);
        return nvme.STATUS_SUCCESS;
    }

    /// The host ids registered under `key` (but `spare`), in order; any?
    fn byKey(self: *Namespace, out: *std.ArrayList([16]u8), key: u64, spare: ?[16]u8) !bool {
        var any = false;
        for (self.registered.items) |r| if (r.key == key) {
            any = true;
            if (spare) |s| if (std.mem.eql(u8, &r.host_id, &s)) continue;
            try out.append(self.gpa, r.host_id);
        };
        return any;
    }

    /// Each victim unregistered in order; with abort (P9) its refs are
    /// killed first, and the command holds pr_sem until every admitted
    /// command of a killed host completed. The generation moves before the
    /// drain (`__nvmet_execute_pr_acquire`).
    fn unregisterVictims(self: *Namespace, victims: []const [16]u8, abort: bool) void {
        if (abort) self.pr_sem_held = true;
        for (victims) |v| {
            if (abort) {
                self.killed.appendAssumeCapacity(v);
                self.fireAbort(.killed, v);
            }
            if (self.indexOf(v)) |i| self.dropAt(i);
            if (abort) self.fireAbort(.unregistered, v);
        }
        self.generation += 1;
        self.preempts += 1;
        if (!abort) return;
        self.fireAbort(.draining, null);
        while (self.killedAdmitted()) {
            self.drain_waiting = true;
            self.changed.broadcast(self.io);
            self.changed.waitUncancelable(self.io, &self.mu);
        }
        self.drain_waiting = false;
        self.killed.clearRetainingCapacity();
        self.pr_sem_held = false;
        self.changed.broadcast(self.io);
    }

    fn fireAbort(self: *Namespace, step: AbortStep, victim: ?[16]u8) void {
        const h = self.abort_hook orelse return;
        self.unlock();
        defer self.lock();
        h.fire(h.ctx, step, victim);
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
        self.lock();
        defer self.unlock();
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
                nvme.OP_RESV_REPORT => self.report(cmd) catch return error.OutOfMemory,
                nvme.OP_RESV_REGISTER, nvme.OP_RESV_ACQUIRE, nvme.OP_RESV_RELEASE => blk: {
                    self.awaitPrSem();
                    break :blk switch (cmd.opcode) {
                        nvme.OP_RESV_REGISTER => self.register(host_id, cmd) catch return error.OutOfMemory,
                        nvme.OP_RESV_ACQUIRE => self.acquire(host_id, cmd) catch return error.OutOfMemory,
                        else => self.release(host_id, cmd),
                    };
                },
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

    /// A read or write from this host (P2, P9a–P9c).
    pub fn submit(self: *Host, op: IoOp) IoAnswer {
        return self.ns.submit(self.host_id, op);
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

fn preemptOom(fail_after: usize) !void {
    var failing = std.testing.FailingAllocator.init(testing.allocator, .{});
    var ns = Namespace.init(failing.allocator());
    defer ns.deinit();
    var a: Host = .{ .ns = &ns, .host_id = host_a };
    var b: Host = .{ .ns = &ns, .host_id = host_b };
    var c: Host = .{ .ns = &ns, .host_id = host_c };
    _ = try registerKey(&a, 0xa);
    _ = try registerKey(&b, 0xb);
    _ = try registerKey(&c, 0xc);
    _ = try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0);
    const gen = ns.generation;
    failing.fail_index = failing.alloc_index + fail_after;
    try testing.expectError(error.OutOfMemory, acquireAs(&b, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xb, 0));
    try testing.expect(!ns.prSemHeld());
    try testing.expect(ns.isHolder(host_a));
    try testing.expectEqual(gen, ns.generation);
    try testing.expectEqual(@as(usize, 3), ns.registered.items.len);
    try testing.expectEqual(@as(usize, 0), ns.killed.items.len);
    failing.fail_index = std.math.maxInt(usize);
    try testing.expectEqual(nvme.STATUS_SUCCESS, try replaceSame(&a, 0xa));
    try testing.expectEqual(nvme.STATUS_SUCCESS, try acquireAs(&b, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xb, 0));
}

test "PR-2 C3: victim-bookkeeping OOM changes no holder, registration, generation or drain state" {
    try preemptOom(0);
}

test "PR-2 C3: killed-bookkeeping OOM never strands pr_sem or the next command" {
    try preemptOom(1);
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

fn heldDrainFailure(ns: *Namespace, a: *Host, b: *Host, stage: u8) !void {
    const w = a.submit(.write);
    try testing.expect(w == .admitted);
    var b2: Host = .{ .ns = ns, .host_id = host_b };
    var pa: Issued = .{ .host = b, .cmd = undefined };
    pa.cmd = nvme.reservationAcquire(1, nvme.RACQA_PREEMPT_AND_ABORT, nvme.RTYPE_WEAR, 0xb, 0, &pa.payload);
    var bar: Issued = .{ .host = &b2, .cmd = undefined };
    bar.cmd = nvme.reservationRegister(1, nvme.RREGA_REPLACE, 0xb, 0xb, &bar.payload);
    var cleanup: DrainCleanup = .{ .ns = ns, .ticket = w.admitted };
    defer cleanup.deinit();
    cleanup.threads[0] = try std.Thread.spawn(.{}, Issued.run, .{&pa});
    ns.awaitDrainWaiting();
    if (stage == 0) return error.InjectedSpawnFailure;
    cleanup.threads[1] = try std.Thread.spawn(.{}, Issued.run, .{&bar});
    ns.awaitPrWaiters(1);
    return error.InjectedReportFailure;
}

test "PR-2 C4: failure after either fake worker starts releases its ticket before joining all workers" {
    for (0..2) |stage| {
        var ns = Namespace.init(testing.allocator);
        defer ns.deinit();
        var a: Host = .{ .ns = &ns, .host_id = host_a };
        var b: Host = .{ .ns = &ns, .host_id = host_b };
        _ = try registerKey(&a, 0xa);
        _ = try registerKey(&b, 0xb);
        _ = try acquireAs(&a, nvme.RACQA_ACQUIRE, nvme.RTYPE_WEAR, 0xa, 0);
        try testing.expectError(if (stage == 0) error.InjectedSpawnFailure else error.InjectedReportFailure, heldDrainFailure(&ns, &a, &b, @intCast(stage)));
        try testing.expectEqual(@as(usize, 0), ns.in_flight_n);
        try testing.expect(!ns.prSemHeld());
        try testing.expectEqual(nvme.STATUS_SUCCESS, try replaceSame(&b, 0xb));
    }
}

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
