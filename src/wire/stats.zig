//! The cluster wire's instruments (charter §10; `docs/porting/sym_wire_
//! membership_recovery.md` §7.2 `cluster_wire_*`): every mechanism's
//! engagement counter, every "cannot happen" a must-stay-0 tripwire,
//! exported whole by the `wire` family (`writeStatsJson`). Process-wide:
//! one daemon runs one listener (decision 0002 row 227: the reference's
//! second listener and second bind are gone), and the N-daemon harness's
//! in-process hubs share the process's counters like every other family.
//! Portable.

const std = @import("std");
const counters = @import("../core/counters.zig");
const frame = @import("frame.zig");

/// Frames and bytes on the wire, both directions (the framer's sequences).
pub var cluster_wire_frames_sent: counters.ShardedCounter = .{};
pub var cluster_wire_frames_received: counters.ShardedCounter = .{};
pub var cluster_wire_bytes_sent: counters.ShardedCounter = .{};
pub var cluster_wire_bytes_received: counters.ShardedCounter = .{};
/// Connections the listener currently serves (the cap's gauge).
pub var cluster_wire_live_connections: counters.Gauge = .{};
/// Accepted-then-closed at the connection cap (one accept + one close).
pub var cluster_wire_connections_refused: counters.ShardedCounter = .{};
/// Accept errors that rode the 5 ms → 1 s backoff ladder.
pub var cluster_wire_accept_backoffs: counters.ShardedCounter = .{};
/// Storage-trust handshakes that admitted / refused a peer.
pub var cluster_wire_sessions_admitted: counters.ShardedCounter = .{};
pub var cluster_wire_admissions_refused: counters.ShardedCounter = .{};
/// Challenges issued and not yet answered or expired.
pub var cluster_wire_outstanding_challenges: counters.Gauge = .{};
/// Calls the service dispatched to a handler.
pub var cluster_wire_requests_served: counters.ShardedCounter = .{};
/// A frame whose MAC did not verify on an authenticated session — a
/// session error, must-stay-0 (a tampering or desynchronized peer).
pub var cluster_wire_mac_failures: counters.Tripwire = .init("cluster_wire_mac_failures");
/// A verb the durable witness refused (`STATUS_REFUSED`): the caller's
/// view contradicts durable state — must-stay-0 on a healthy fleet.
pub var cluster_wire_service_refusals: counters.Tripwire = .init("cluster_wire_service_refusals");
/// Frames refused for a foreign framing/vocabulary (version, flags,
/// schema, unknown verb) — charter §1.7, counted, never interpreted.
pub var cluster_wire_schema_refusals: counters.ShardedCounter = .{};
/// Payloads that did not decode (`STATUS_MALFORMED`).
pub var cluster_wire_malformed: counters.ShardedCounter = .{};
/// Verbs whose SCREEN refused a wire word (`STATUS_REJECTED`): the
/// buggy/hostile-peer class, kept OFF the must-stay-0 gauge.
pub var cluster_wire_screens_rejected: counters.ShardedCounter = .{};
/// Retryable answers by class word (`frame.RefusalClass`'s ordinal).
pub var cluster_wire_refusals_by_class: [8]counters.ShardedCounter = @splat(.{});
/// The dialer's bounded retry engaged (a listener closed before its challenge).
pub var cluster_wire_dial_retries: counters.ShardedCounter = .{};
/// Dials that ran the whole bound and answered `ListenerRefused`.
pub var cluster_wire_listener_refused: counters.ShardedCounter = .{};
/// A poisoned session re-dialed by its client.
pub var cluster_wire_reconnects: counters.ShardedCounter = .{};
/// Sessions found dead (EOF or unsolicited bytes queued) by the peek
/// before a one-attempt verb's send.
pub var cluster_wire_dead_on_arrival: counters.ShardedCounter = .{};
/// Calls whose reply did not arrive inside the reply bound
/// (`HolderUnreachable` to the caller).
pub var cluster_wire_call_timeouts: counters.ShardedCounter = .{};
/// Replies that arrived for a call that had already given up.
pub var cluster_wire_late_replies: counters.ShardedCounter = .{};
/// Sessions a transport failure poisoned (every parked call failed).
pub var cluster_wire_sessions_poisoned: counters.ShardedCounter = .{};
/// Header reads refused before any payload byte (magic/version/flags/
/// reserved/len-over-cap).
pub var cluster_wire_header_refusals: counters.ShardedCounter = .{};
/// Body reads that hit the 30 s class deadline (a dribbling peer).
pub var cluster_wire_body_timeouts: counters.ShardedCounter = .{};

comptime {
    std.debug.assert(cluster_wire_refusals_by_class.len == @typeInfo(frame.RefusalClass).@"enum".field_names.len);
}

pub fn noteRefusalClass(class: frame.RefusalClass) void {
    cluster_wire_refusals_by_class[@backingInt(class)].inc();
}

/// `{"cluster_wire_frames_sent":…, …}` — every instrument declared here.
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try counters.writeNamespaceJson(@This(), w);
}

test "every instrument is exported by the generated writer; the class table has one counter per class word" {
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    try std.testing.expect(std.mem.indexOf(u8, out, "\"cluster_wire_frames_sent\":") != null);
    try std.testing.expect(std.mem.indexOf(u8, out, "\"cluster_wire_mac_failures\":") != null);
    try std.testing.expect(std.mem.indexOf(u8, out, "\"cluster_wire_refusals_by_class\":[") != null);
    const before = cluster_wire_refusals_by_class[@backingInt(frame.RefusalClass.slot_moved)].load();
    noteRefusalClass(.slot_moved);
    try std.testing.expectEqual(before + 1, cluster_wire_refusals_by_class[1].load());
}
