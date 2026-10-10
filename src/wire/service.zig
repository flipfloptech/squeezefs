//! The request loop's law, ONE place for every transport (TCP sessions and
//! the in-process loopback hub): a call frame arrives already checksummed
//! and MAC-verified; the service names the verb, checks the block's
//! schema, decodes the payload TOTALLY, dispatches to the registered
//! handler (which runs its SCREEN against durable bounds before any
//! effect) and encodes the reply under the ONE status table. Nothing here
//! allocates per request: the reply is built into the session's send
//! buffer, the handler's scratch is the session's second buffer.
//!
//! Handlers are the later waves' (manager, slot, token, membership, …);
//! they register per verb. A verb nothing registered answers the block's
//! "not armed" word (`not_manager` / `not_holder` / `not_member`), never
//! a crash. `ping` is served here. Portable.

const std = @import("std");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const codec = @import("codec.zig");
const frame = @import("frame.zig");
const verbs = @import("verbs.zig");
const stats = @import("stats.zig");

pub const Verb = verbs.Verb;
pub const Status = frame.Status;

/// What a handler sees: the verb, the decoded request, the authenticated
/// peer (a member id when the peer presented one — the identity screens'
/// witness), the frame's correlation.
pub const Request = struct {
    verb: Verb,
    payload: verbs.Request,
    peer: ?verbs.WireIdentity,
    correlation: u64,
    /// The frame's `volume` for manager verbs rides the payload; the
    /// session's own number (the hub's "daemon" index, a connection id)
    /// is here for logs.
    session_id: u64,
};

/// The handler's answer: an `ok` reply (typed, tag == verb) or a status
/// with an optional typed error (encoded as `WireError`).
pub const Answer = union(enum) {
    ok: verbs.Reply,
    status: StatusAnswer,

    pub const StatusAnswer = struct {
        status: Status,
        err: ?errors.Error = null,
        reason: []const u8 = "",
        retry_after_ms: u32 = 0,
        slot: u16 = 0,
        holder: u32 = 0,
    };

    pub fn rejected(reason: []const u8) Answer {
        return .{ .status = .{ .status = .rejected, .err = error.Rejected, .reason = reason } };
    }
    pub fn refused(err: errors.Error, reason: []const u8) Answer {
        return .{ .status = .{ .status = .refused, .err = err, .reason = reason } };
    }
    pub fn deferred(reason: []const u8) Answer {
        return .{ .status = .{ .status = .deferred, .err = error.Deferred, .reason = reason } };
    }
};

/// The handler: a context pointer and a function; `scratch` is per-session
/// space for building variable tails (a token page, a census page).
pub const Handler = struct {
    ctx: *anyopaque,
    serve: *const fn (ctx: *anyopaque, req: *const Request, scratch: []u8) Answer,
};

/// The reply the service produced: the status word and the payload bytes
/// (inside the caller's reply buffer).
pub const Served = struct { status: Status, payload: []const u8 };

pub const Service = struct {
    handlers: [Verb.count]?Handler = @splat(null),

    pub const init: Service = .{};

    pub fn register(self: *Service, verb: Verb, h: Handler) void {
        std.debug.assert(!verb.isHandshake() and verb != .ping);
        self.handlers[verb.index()] = h;
    }

    pub fn unregister(self: *Service, verb: Verb) void {
        self.handlers[verb.index()] = null;
    }

    pub fn handles(self: *const Service, verb: Verb) bool {
        return self.handlers[verb.index()] != null;
    }

    /// The block's "not armed on this node" word.
    fn notArmed(verb: Verb) Status {
        return switch (verb.block()) {
            .manager, .membership => .not_manager,
            .token, .xv => .not_holder,
            // The coordinator is volume 0's manager: a job or admin verb
            // nothing registered says so.
            .job, .admin => .not_manager,
            .wire => .unknown_verb,
        };
    }

    /// Serve one verified call frame. `reply_buf` receives the reply
    /// payload; `scratch` is the handler's tail-building space.
    pub fn serve(self: *const Service, header: frame.Header, payload: []const u8, peer: ?verbs.WireIdentity, session_id: u64, reply_buf: []u8, scratch: []u8) Served {
        var b: codec.Builder = .init(reply_buf);
        const verb = Verb.fromWire(header.verb) orelse {
            stats.cluster_wire_schema_refusals.inc();
            log.warn("cluster wire: unknown verb 0x{x:0>4} from session {d} — refused (charter §1.7)", .{ header.verb, session_id });
            return .{ .status = .unknown_verb, .payload = b.written() };
        };
        if (verb.isHandshake()) {
            stats.cluster_wire_schema_refusals.inc();
            return .{ .status = .unknown_verb, .payload = b.written() };
        }
        if (header.schema != verb.schema()) {
            stats.cluster_wire_schema_refusals.inc();
            log.warn("cluster wire: verb {t} carried schema {d}, this binary speaks {d} — refusing rather than interpreting a foreign vocabulary", .{ verb, header.schema, verb.schema() });
            return .{ .status = .schema, .payload = b.written() };
        }
        const decoded = verbs.decodeRequest(verb, payload) catch {
            stats.cluster_wire_malformed.inc();
            return .{ .status = .malformed, .payload = b.written() };
        };
        if (verb == .ping) {
            stats.cluster_wire_requests_served.inc();
            return .{ .status = .ok, .payload = b.written() };
        }
        const h = self.handlers[verb.index()] orelse return .{ .status = notArmed(verb), .payload = b.written() };
        stats.cluster_wire_requests_served.inc();
        const req: Request = .{ .verb = verb, .payload = decoded, .peer = peer, .correlation = header.correlation, .session_id = session_id };
        const answer = h.serve(h.ctx, &req, scratch);
        return encodeAnswer(verb, answer, &b);
    }

    fn encodeAnswer(verb: Verb, answer: Answer, b: *codec.Builder) Served {
        switch (answer) {
            .ok => |rep| {
                std.debug.assert(@as(Verb, rep) == verb);
                verbs.encodeReply(rep, b) catch {
                    // A reply the control cap cannot carry is a handler bug
                    // (every tail is capped below it); answer refused, loud.
                    stats.cluster_wire_service_refusals.fire("reply overflowed the control cap");
                    var b2: codec.Builder = .init(b.buf);
                    frame.WireError.init(error.ValueTooLarge, "reply overflowed the control cap").encode(&b2) catch {};
                    return .{ .status = .refused, .payload = b2.written() };
                };
                return .{ .status = .ok, .payload = b.written() };
            },
            .status => |s| {
                std.debug.assert(s.status != .ok);
                switch (s.status) {
                    .rejected => stats.cluster_wire_screens_rejected.inc(),
                    .refused => stats.cluster_wire_service_refusals.fire(s.reason),
                    else => {},
                }
                if (s.err) |e| {
                    const class = frame.RefusalClass.fromError(e);
                    if (class != .none) stats.noteRefusalClass(class);
                    var we = frame.WireError.init(e, s.reason);
                    we.head.retry_after_ms = s.retry_after_ms;
                    we.head.slot = s.slot;
                    we.head.holder = s.holder;
                    we.encode(b) catch unreachable; // REASON_MAX + the head fit any reply buffer
                }
                return .{ .status = s.status, .payload = b.written() };
            },
        }
    }
};

/// The caller side of a served reply: `ok` → the decoded typed reply;
/// otherwise the typed error (the class word, the errno, or the status's
/// own mapping).
pub fn interpretReply(verb: Verb, status: Status, payload: []const u8) errors.Error!verbs.Reply {
    if (status == .ok) return verbs.decodeReply(verb, payload) catch return error.WireMalformed;
    // The status word is the verdict; only `refused` defers to the payload's
    // class/errno (the status itself names no errno).
    if (status == .refused) {
        if (replyError(payload)) |we| return we.toError();
    }
    return status.toError().?;
}

/// The `WireError` a non-ok reply carried (the retry hint, the slot/holder
/// words, the reason), when it decodes.
pub fn replyError(payload: []const u8) ?frame.WireError {
    if (payload.len == 0) return null;
    return codec.decodePayload(frame.WireError, payload) catch null;
}

// ---------------------------------------------------------------------------

const testing = std.testing;

const Echo = struct {
    served: u32 = 0,
    fn serve(ctx: *anyopaque, req: *const Request, _: []u8) Answer {
        const self: *Echo = @ptrCast(@alignCast(ctx));
        self.served += 1;
        switch (req.payload) {
            .resolve_slot => |r| {
                if (r.slot == 9) return Answer.rejected("slot 9 is poisoned in this test");
                if (r.slot == 8) return .{ .status = .{ .status = .deferred, .err = error.Deferred, .reason = "ring full", .retry_after_ms = 50 } };
                if (r.slot == 7) return .{ .status = .{ .status = .refused, .err = error.SlotMoved, .reason = "moved", .slot = 7, .holder = 3 } };
                return .{ .ok = .{ .resolve_slot = .{ .kind = 0, .reserved = @splat(0), .appender_id = 42, .g = r.slot, .reserved2 = 0 } } };
            },
            else => return .{ .status = .{ .status = .not_manager } },
        }
    }
};

fn callHeader(verb: Verb) frame.Header {
    return .{ .flags = frame.FLAG_MAC_PRESENT, .len = 0, .verb = @backingInt(verb), .status = 0, .correlation = 1, .seq = 1, .schema = verb.schema() };
}

test "the service law: unknown verb, foreign schema, malformed payload, an unregistered block, ping, a handler's ok/rejected/deferred/refused — each the ONE table's word; the caller side maps them to the typed errors" {
    var svc: Service = .init;
    var echo: Echo = .{};
    svc.register(.resolve_slot, .{ .ctx = &echo, .serve = Echo.serve });
    var reply: [1024]u8 = undefined;
    var scratch: [1024]u8 = undefined;
    var payload: [64]u8 = undefined;
    var b: codec.Builder = .init(&payload);
    try verbs.encodeRequest(.{ .resolve_slot = .{ .volume = 0, .slot = 5, .reserved = 0 } }, &b);
    const req_bytes = b.written();

    var h = callHeader(.resolve_slot);
    h.verb = 0x0508;
    try testing.expectEqual(Status.unknown_verb, svc.serve(h, req_bytes, null, 1, &reply, &scratch).status);
    h = callHeader(.resolve_slot);
    h.schema = 99;
    try testing.expectEqual(Status.schema, svc.serve(h, req_bytes, null, 1, &reply, &scratch).status);
    h = callHeader(.resolve_slot);
    try testing.expectEqual(Status.malformed, svc.serve(h, req_bytes[0..3], null, 1, &reply, &scratch).status);
    var payload2: [64]u8 = undefined;
    var jb: codec.Builder = .init(&payload2);
    try verbs.encodeRequest(.{ .join_appender = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .identity = .zero, .ring_want_bytes = 0 } }, &jb);
    try testing.expectEqual(Status.not_manager, svc.serve(callHeader(.join_appender), jb.written(), null, 1, &reply, &scratch).status);
    var payload3: [64]u8 = undefined;
    var tb: codec.Builder = .init(&payload3);
    const recalled = [_]u64{9};
    try verbs.encodeRequest(.{ .token_recall = .{ .head = .{ .volume = 0, .reserved = 0, .n = 1, .frame_id = 1, .holder = 1 }, .items = &recalled } }, &tb);
    try testing.expectEqual(Status.not_holder, svc.serve(callHeader(.token_recall), tb.written(), null, 1, &reply, &scratch).status);
    try testing.expectEqual(Status.unknown_verb, svc.serve(callHeader(.challenge), req_bytes, null, 1, &reply, &scratch).status);
    try testing.expectEqual(@as(u32, 0), echo.served);
    const ping = svc.serve(callHeader(.ping), "", null, 1, &reply, &scratch);
    try testing.expectEqual(Status.ok, ping.status);
    try testing.expectEqual(@as(usize, 0), ping.payload.len);
    // ok
    const ok = svc.serve(callHeader(.resolve_slot), req_bytes, null, 1, &reply, &scratch);
    try testing.expectEqual(Status.ok, ok.status);
    const rep = try interpretReply(.resolve_slot, ok.status, ok.payload);
    try testing.expectEqual(@as(u32, 5), rep.resolve_slot.g);
    try testing.expectEqual(@as(u32, 1), echo.served);
    // rejected → EINVAL class
    var b9: codec.Builder = .init(&payload);
    try verbs.encodeRequest(.{ .resolve_slot = .{ .volume = 0, .slot = 9, .reserved = 0 } }, &b9);
    const rejected_before = stats.cluster_wire_screens_rejected.load();
    const rj = svc.serve(callHeader(.resolve_slot), b9.written(), null, 1, &reply, &scratch);
    try testing.expectEqual(Status.rejected, rj.status);
    try testing.expectError(error.Rejected, interpretReply(.resolve_slot, rj.status, rj.payload));
    try testing.expectEqual(rejected_before + 1, stats.cluster_wire_screens_rejected.load());
    // deferred → EAGAIN with the retry hint
    var b8: codec.Builder = .init(&payload);
    try verbs.encodeRequest(.{ .resolve_slot = .{ .volume = 0, .slot = 8, .reserved = 0 } }, &b8);
    const df = svc.serve(callHeader(.resolve_slot), b8.written(), null, 1, &reply, &scratch);
    try testing.expectEqual(Status.deferred, df.status);
    try testing.expectError(error.Deferred, interpretReply(.resolve_slot, df.status, df.payload));
    const we = try codec.decodePayload(frame.WireError, df.payload);
    try testing.expectEqual(@as(u32, 50), we.head.retry_after_ms);
    // refused with a typed class: the must-stay-0 fires loudly, the class is counted
    var b7: codec.Builder = .init(&payload);
    try verbs.encodeRequest(.{ .resolve_slot = .{ .volume = 0, .slot = 7, .reserved = 0 } }, &b7);
    const class_before = stats.cluster_wire_refusals_by_class[1].load();
    stats.cluster_wire_service_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    const rf = svc.serve(callHeader(.resolve_slot), b7.written(), null, 1, &reply, &scratch);
    try log.testing.expectAllFired();
    try testing.expectEqual(Status.refused, rf.status);
    try testing.expectError(error.SlotMoved, interpretReply(.resolve_slot, rf.status, rf.payload));
    try testing.expectEqual(class_before + 1, stats.cluster_wire_refusals_by_class[1].load());
    const we7 = try codec.decodePayload(frame.WireError, rf.payload);
    try testing.expectEqual(@as(u16, 7), we7.head.slot);
    try testing.expectEqual(@as(u32, 3), we7.head.holder);
    // A status without a payload maps through the table alone.
    try testing.expectError(error.NotManager, interpretReply(.join_appender, .not_manager, ""));
    try testing.expectError(error.WireMalformed, interpretReply(.resolve_slot, .ok, "zz"));
    svc.unregister(.resolve_slot);
    try testing.expect(!svc.handles(.resolve_slot));
}
