//! Symmetric metadata, part C2 — MEMBERSHIP sharded by home volume, the
//! DEATH LEDGER, DEAD-APPENDER RECOVERY, WERO fencing with registrants,
//! PARK-AND-RECLAIM and the JOIN LADDER over the wire (charter §5.5
//! "Membership is sharded by home volume", "A manager's death …",
//! "Death and recovery (§5.9)", "Fencing", "Flush ceiling"; §5.7; §8
//! `appenders` / `appender clear`; `docs/porting/sym_wire_membership_
//! recovery.md` §2–§9): the shard's RAM authority with its member rows,
//! eviction, the two-halves grace window and three-valued liveness
//! (`shard`), the death ledger's writer, screen, poll and retirement
//! (`death_ledger`), the §5.9 procedure as ONE driver with its RAII
//! rollback and `appender clear` (`recovery`), the park gate with its
//! model (`park_reclaim`), the WERO duty split, the cap gate, the frame
//! screen's inputs and the priming law (`fencing`), the seven rungs over
//! the wire, the joined wire, the same-node successor rule and the
//! flush-ceiling audit (`join_ladder`), and — Linux only — the driver's
//! `Volume` seam over the real backend with the poll's hooks
//! (`volume_bind`). Portable otherwise; every registry is a field of the
//! routed set's values (0002 row 259).

const std = @import("std");
const builtin = @import("builtin");
const counters = @import("../../core/counters.zig");

pub const shard = @import("membership/shard.zig");
pub const death_ledger = @import("membership/death_ledger.zig");
pub const recovery = @import("membership/recovery.zig");
pub const park_reclaim = @import("membership/park_reclaim.zig");
pub const fencing = @import("membership/fencing.zig");
pub const join_ladder = @import("membership/join_ladder.zig");
/// The recovery driver's `Volume` seam over the real backend (Linux).
pub const volume_bind = if (builtin.os.tag == .linux) @import("membership/volume_bind.zig") else struct {};

pub const Owner = shard.Owner;
pub const Member = shard.Member;
pub const Ledger = death_ledger.Ledger;
pub const ParkGate = park_reclaim.ParkGate;

/// The `.stats` `meta.sym.membership` family.
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeByte('{');
    inline for (.{
        .{ "shard", shard.stats },
        .{ "death_ledger", death_ledger.stats },
        .{ "recovery", recovery.stats },
        .{ "park", park_reclaim.stats },
        .{ "fencing", fencing.stats },
        .{ "join_ladder", join_ladder.stats },
        .{ "volume_bind", if (builtin.os.tag == .linux) volume_bind.stats else struct {} },
    }, 0..) |m, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{m[0]});
        try counters.writeNamespaceJson(m[1], w);
    }
    try w.writeByte('}');
}

test {
    _ = shard;
    _ = death_ledger;
    _ = recovery;
    _ = park_reclaim;
    _ = fencing;
    _ = join_ladder;
    if (builtin.os.tag == .linux) _ = volume_bind;
}

test "the stats family names every namespace's instruments" {
    var buf: [1 << 16]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    for ([_][]const u8{ "\"shard\":{", "\"membership_evictions\":", "\"death_ledger\":{", "\"dead_members_acted\":", "\"recovery\":{", "\"appender_recoveries\":", "\"park\":{", "\"appender_park_expiries\":", "\"fencing\":{", "\"appender_unstamped_frame\":", "\"join_ladder\":{", "\"joined_wire_verbs\":" }) |k| {
        try std.testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}
