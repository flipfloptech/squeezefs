//! Symmetric metadata, part C1 — CROSS-OWNER NAMESPACE OPS as intents,
//! the RECORD-LEVEL SHIP of a foreign-slot file's mutations, and DIRECTORY
//! STRIPING (charter §5.5 "Cross-owner namespace ops", "Record-level
//! mutations of a foreign-slot file", "Directory striping"; §5.2 the
//! journal economy; `docs/porting/sym_tokens_intents_striping.md` §2–§4):
//! the intent record, register and minter (`intent`), the six step kinds
//! with their witnesses and the per-op plans (`steps`), the ONE applier
//! every live path and every roll-forward run through (`applier`), the
//! travelling 4a guards (`guards`), the set-wide `dir_rename` lease and the
//! exact ancestry walk (`dir_rename`), the executor — step 0 + barrier,
//! dispatch classified by the arm taken, the live-refusal compensation
//! under the ABORTING flag, the retirement, the roll-forward at mount and
//! on the cadence, the served step under the holder's door (`ops`), the
//! record-level ship with the holder's own-cache and kernel invalidation
//! (`record_ship`), directory striping — the `K + 2` NUL-led marker map,
//! the flip as ONE intent, the census trigger, lazy re-home, the streamed
//! K-way readdir merge, the `stat` fold, the one-intent `rmdir`, fsck C17
//! (`stripe`). PORTABLE throughout: the contracts run IN-PROCESS over two
//! holders on the loopback hub (`two_holders`).

const std = @import("std");
const builtin = @import("builtin");
const counters = @import("../../core/counters.zig");

pub const intent = @import("xv/intent.zig");
pub const steps = @import("xv/steps.zig");
pub const applier = @import("xv/applier.zig");
pub const guards = @import("xv/guards.zig");
pub const dir_rename = @import("xv/dir_rename.zig");
pub const ops = @import("xv/ops.zig");
pub const record_ship = @import("xv/record_ship.zig");
pub const stripe = @import("xv/stripe.zig");
pub const set_bind = if (builtin.os.tag == .linux) @import("xv/set_bind.zig") else struct {};

/// The `.stats` `meta.sym.xv` family.
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeByte('{');
    inline for (.{
        .{ "intents", intent.stats },
        .{ "applier", applier.stats },
        .{ "guards", guards.stats },
        .{ "dir_rename", dir_rename.stats },
        .{ "ops", ops.stats },
        .{ "record_ship", record_ship.stats },
        .{ "stripe", stripe.stats },
    }, 0..) |m, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{m[0]});
        try counters.writeNamespaceJson(m[1], w);
    }
    try w.writeByte('}');
}

test {
    _ = intent;
    _ = steps;
    _ = applier;
    _ = guards;
    _ = dir_rename;
    _ = ops;
    _ = record_ship;
    _ = stripe;
    if (builtin.os.tag == .linux) _ = set_bind;
    _ = @import("xv/two_holders.zig");
}
