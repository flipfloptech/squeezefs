//! The `.stats` surface (charter §10; `docs/porting/stats_cli.md` §1.4):
//! ONE comptime-composed JSON aggregator over every instrument family in
//! the daemon, rendered as one document the `.stats` inode serves, the
//! SIGUSR1 dump logs family by family, and `bench`/`umount`/the smoke read
//! name-keyed.
//!
//! Three laws (§7 Q1/Q4):
//!   - every family is ALWAYS present and zero-valued when its mechanism is
//!     inert (a consumer never has to know a posture to find a key); the
//!     ONE exception is `SQUEEZEFS_OP_PROFILE`'s armed-only phase family,
//!     whose knob doc promises a byte-identical disarmed surface;
//!   - the writers are GENERATED from the modules' `stats` namespaces
//!     (`counters.writeNamespaceJson`): a counter declared is a counter
//!     exported, and `tests/rails.zig` refuses a counter-bearing module that
//!     is not in `sources`/`covered` below;
//!   - the payload streams through ONE `std.Io.Writer` into ONE allocation
//!     sized from the previous render (`Renderer`) — never a value tree.
//!
//! Gauge NAMES stay their modules' (`transport_*`, `meta_kv_*`, `read_*`,
//! `write_pipeline_*`, …): the smoke's reads are name-keyed, not path-keyed.
//! Linux-only families render `{}` off Linux (the daemon is Linux-only; the
//! portable tests pin the schema, the order and the allocation law).

const std = @import("std");
const builtin = @import("builtin");
const is_linux = builtin.os.tag == .linux;
const Io = std.Io;
const Writer = Io.Writer;
const Allocator = std.mem.Allocator;

const counters = @import("../core/counters.zig");
const knobs = @import("../core/knobs.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
// ---- the families' modules ----
const session = if (is_linux) @import("../fuse/transport/session.zig") else struct {};
const slot = @import("../fuse/transport/slot.zig");
const worker = if (is_linux) @import("../fuse/transport/worker.zig") else struct {};
const plane_mod = if (is_linux) @import("../data/plane.zig") else struct {};
const transform = @import("../data/transform.zig");
const write_seams = @import("../data/write_seams.zig");
const write_admit = @import("../data/write_admit.zig");
const read_path = @import("../data/read_path.zig");
const block_alloc = @import("../data/block_alloc.zig");
const reclaim = @import("../data/reclaim.zig");
const write_pipeline = @import("../data/write_pipeline.zig");
const backend = if (is_linux) @import("../meta/backend.zig") else struct {};
const journal_lane = if (is_linux) @import("../meta/journal_lane.zig") else struct {};
const device_loader = if (is_linux) @import("../meta/device_loader.zig") else struct {};
const checkpoint = @import("../meta/checkpoint.zig");
const node_cache = @import("../meta/node_cache.zig");
const tree = @import("../meta/tree.zig");
const smo = @import("../meta/smo.zig");
const leaf_merge = @import("../meta/leaf_merge.zig");
const alloc_ext = @import("../meta/alloc_ext.zig");
const journal = @import("../meta/journal.zig");
const ledger = @import("../meta/ledger.zig");
const record = @import("../meta/record.zig");
const key = @import("../meta/key.zig");
const tx = @import("../meta/tx.zig");
const economy = @import("../meta/economy.zig");
const inos = @import("../meta/inos.zig");
const dlm = @import("../meta/dlm.zig");
const heap_law = @import("../meta/heap_law.zig");
const node = @import("../meta/node.zig");
const node_snapshot = @import("../meta/node_snapshot.zig");
const node_state_core = @import("../meta/node_state_core.zig");
const guard = @import("../meta/guard.zig");
const lifecycle = @import("../fuse/handler/lifecycle.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");
const open_table = @import("../fuse/handler/open_table.zig");
const watchdog = @import("../fuse/handler/watchdog.zig");
const kv_fs = @import("../fuse/handler/kv_fs.zig");
const virtual_inodes = @import("../fuse/handler/virtual_inodes.zig");
const op_trace = @import("../fuse/handler/op_trace.zig");
const codec = @import("../fuse/codec.zig");
const exec_stats = @import("../exec/stats.zig");
const placement = @import("../exec/placement.zig");
const cpu_class = @import("../exec/cpu_class.zig");
const sqz_io = if (is_linux) @import("../exec/sqz_io.zig") else struct {};
const tick = @import("../sync/tick.zig");
const arc_swap = @import("../sync/arc_swap.zig");
const uring_fs = if (is_linux) @import("../io/uring_fs.zig") else struct {};
const nvme_derive = @import("../io/nvme_derive.zig");
const mem_budget = @import("../mem/budget.zig");

pub const Error = Writer.Error;

// ---------------------------------------------------------------------------
// The inode's own instruments (§5.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// `stats_builds`: payloads rendered (every `.stats`/`.config`/`.trace`
    /// lookup and every SIGUSR1 dump).
    pub var stats_builds: counters.ShardedCounter = .{};
    /// The largest payload rendered so far (the registry cap's WHY is
    /// stated against the field-measured ≈ 72 KiB; this is the live figure).
    pub var stats_build_bytes_max: counters.Gauge = .{};
    /// Renders whose capacity hint was too small (one extra allocation).
    pub var stats_build_regrows: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// Sources: the instance-bound families (a module-level family needs none).
// ---------------------------------------------------------------------------

pub const Build = struct {
    train: []const u8 = "",
    commit: []const u8 = "",
    profile: []const u8 = "",
};

pub const Mount = struct {
    uid: u32 = 0,
    gid: u32 = 0,
    mountpoint: []const u8 = "",
    sqmeta_uri: []const u8 = "",
    sqdata_uri: []const u8 = "",
    read_only: bool = false,
    writeback: bool = false,
    direct_device_true: bool = false,
};

/// The key census (VAL-7a): the stores' live key sets, written only when
/// `SQUEEZEFS_STATS_KEY_CENSUS` is armed. A typed seam the data plane
/// binds; unbound, the census keys render present-and-empty (so an
/// operator can always key on them).
pub const KeyCensus = struct {
    ctx: *anyopaque,
    write: *const fn (ctx: *anyopaque, w: *Writer) Error!void,
};

pub const Sources = struct {
    build: Build = .{},
    mount: Mount = .{},
    /// The process environment: the key-census knob is read LIVE per
    /// render (§1.4 — a `.stats` build is cold, never an op) and the A/B
    /// levers in force are listed.
    env: ?*const std.process.Environ.Map = null,
    session: if (is_linux) ?*session.Session else ?*void = null,
    plane: if (is_linux) ?*plane_mod.DataPlane else ?*void = null,
    sqz: if (is_linux) ?*sqz_io.SqzIo else ?*void = null,
    backend: if (is_linux) ?*backend.KvBackend else ?*void = null,
    key_census: ?KeyCensus = null,
    /// The CPU ledger (+ its allocator) and an `Io` for the `/proc/self/
    /// task` walk; absent ⇒ zeros (every family stays present).
    cpu: ?*cpu_class.Ledger = null,
    gpa: ?Allocator = null,
    io: ?Io = null,

    pub fn keyCensusArmed(self: *const Sources) bool {
        const env = self.env orelse return false;
        return knobs.boolKnob("SQUEEZEFS_STATS_KEY_CENSUS", env);
    }
};

// ---------------------------------------------------------------------------
// The registry: top-level families in the schema's order.
// ---------------------------------------------------------------------------

pub const Family = struct {
    name: []const u8,
    write: *const fn (*Sources, *Writer) Error!void,
};

/// The schema's top-level order (§1.4 PORT schema). `daemon` is LAST: the
/// SIGUSR1 dump logs one line per family and the smoke awaits the daemon
/// line as the completion mark.
pub const families = [_]Family{
    .{ .name = "build", .write = writeBuild },
    .{ .name = "mount", .write = writeMount },
    .{ .name = "stats_key_census", .write = writeKeyCensusFlag },
    .{ .name = "transport", .write = writeTransport },
    .{ .name = "data", .write = writeData },
    .{ .name = "meta", .write = writeMeta },
    .{ .name = "fuse", .write = writeFuse },
    .{ .name = "exec", .write = writeExec },
    .{ .name = "sync", .write = writeSync },
    .{ .name = "io", .write = writeIo },
    .{ .name = "mem_budget", .write = writeMemBudget },
    .{ .name = "ops", .write = writeOps },
    .{ .name = "op_trace", .write = writeOpTrace },
    .{ .name = "cpu", .write = writeCpu },
    .{ .name = "stats", .write = writeSelf },
    .{ .name = "daemon", .write = writeDaemon },
};

/// A counter-bearing module and the family that exports its declarations
/// (the inverse rail's forward table; every path must exist).
pub const Source = struct { path: []const u8, family: []const u8 };

pub const sources = [_]Source{
    .{ .path = "src/fuse/transport/session.zig", .family = "transport" },
    .{ .path = "src/data/plane.zig", .family = "data" },
    .{ .path = "src/data/transform.zig", .family = "data" },
    .{ .path = "src/data/write_seams.zig", .family = "data" },
    .{ .path = "src/meta/backend.zig", .family = "meta" },
    .{ .path = "src/meta/checkpoint.zig", .family = "meta" },
    .{ .path = "src/meta/journal_lane.zig", .family = "meta" },
    .{ .path = "src/meta/device_loader.zig", .family = "meta" },
    .{ .path = "src/meta/node_cache.zig", .family = "meta" },
    .{ .path = "src/meta/tree.zig", .family = "meta" },
    .{ .path = "src/meta/smo.zig", .family = "meta" },
    .{ .path = "src/meta/leaf_merge.zig", .family = "meta" },
    .{ .path = "src/meta/alloc_ext.zig", .family = "meta" },
    .{ .path = "src/meta/journal.zig", .family = "meta" },
    .{ .path = "src/meta/ledger.zig", .family = "meta" },
    .{ .path = "src/meta/record.zig", .family = "meta" },
    .{ .path = "src/meta/key.zig", .family = "meta" },
    .{ .path = "src/meta/tx.zig", .family = "meta" },
    .{ .path = "src/meta/economy.zig", .family = "meta" },
    .{ .path = "src/meta/inos.zig", .family = "meta" },
    .{ .path = "src/meta/dlm.zig", .family = "meta" },
    .{ .path = "src/meta/heap_law.zig", .family = "meta" },
    .{ .path = "src/meta/node.zig", .family = "meta" },
    .{ .path = "src/meta/node_snapshot.zig", .family = "meta" },
    .{ .path = "src/meta/node_state_core.zig", .family = "meta" },
    .{ .path = "src/meta/conveyor.zig", .family = "meta" },
    .{ .path = "src/meta/guard.zig", .family = "meta" },
    .{ .path = "src/fuse/handler/lifecycle.zig", .family = "fuse" },
    .{ .path = "src/fuse/handler/op_guard.zig", .family = "fuse" },
    .{ .path = "src/fuse/handler/open_table.zig", .family = "fuse" },
    .{ .path = "src/fuse/handler/watchdog.zig", .family = "fuse" },
    .{ .path = "src/fuse/handler/kv_fs.zig", .family = "fuse" },
    .{ .path = "src/fuse/handler/virtual_inodes.zig", .family = "fuse" },
    .{ .path = "src/meta/checkpoint_cycle.zig", .family = "meta" },
    .{ .path = "src/fuse/handler/op_trace.zig", .family = "ops" },
    .{ .path = "src/fuse/codec.zig", .family = "fuse" },
    .{ .path = "src/exec/stats.zig", .family = "exec" },
    .{ .path = "src/exec/placement.zig", .family = "exec" },
    .{ .path = "src/sync/tick.zig", .family = "sync" },
    .{ .path = "src/sync/arc_swap.zig", .family = "sync" },
    .{ .path = "src/io/uring_fs.zig", .family = "io" },
    .{ .path = "src/io/nvme_derive.zig", .family = "io" },
    .{ .path = "src/mem/budget.zig", .family = "mem_budget" },
    .{ .path = "src/stats/stats.zig", .family = "stats" },
};

/// A counter-bearing module whose instruments another module's writer
/// composes (the transport's sub-modules under `Session.writeStatsJson`,
/// the data plane's under `DataPlane.writeFamiliesJson`).
pub const Covered = struct { path: []const u8, by: []const u8 };

pub const covered = [_]Covered{
    .{ .path = "src/fuse/transport/slot.zig", .by = "src/fuse/transport/session.zig" },
    .{ .path = "src/fuse/transport/worker.zig", .by = "src/fuse/transport/session.zig" },
    .{ .path = "src/fuse/transport/commit_queue.zig", .by = "src/fuse/transport/session.zig" },
    .{ .path = "src/fuse/transport/kmbuf.zig", .by = "src/fuse/transport/session.zig" },
    .{ .path = "src/fuse/transport/probes.zig", .by = "src/fuse/transport/session.zig" },
    .{ .path = "src/fuse/geometry.zig", .by = "src/fuse/transport/session.zig" },
    .{ .path = "src/data/accumulate.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/admission.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/block_alloc.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/block_refs.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/device_overlay.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/enospc.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/extent_overlay.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/fsync.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/hot_tier.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/kvmap.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/layout.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/nvme_read_cache.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/pack.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/patch_w1.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/prefetch.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/purge.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/read_fill_pool.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/read_hold.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/read_lru.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/read_path.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/reclaim.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/router.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/single_flight_fill.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/staging.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/warm_ladder.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/write_admit.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/write_epoch.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/write_pipeline.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/write_side_view.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/data/writeback.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/io/nvme_dev.zig", .by = "src/data/plane.zig" },
    .{ .path = "src/exec/lane.zig", .by = "src/exec/stats.zig" },
    .{ .path = "src/fuse/transport/filesystem_vtable.zig", .by = "src/fuse/transport/session.zig" },
};

comptime {
    // Every covering module is itself a source; every family named by a
    // source exists.
    @setEvalBranchQuota(20_000);
    for (covered) |c| {
        var found = false;
        for (sources) |s| found = found or std.mem.eql(u8, s.path, c.by);
        if (!found) @compileError("stats.covered: " ++ c.by ++ " is not a source");
    }
    for (sources) |s| {
        var found = false;
        for (families) |f| found = found or std.mem.eql(u8, f.name, s.family);
        if (!found) @compileError("stats.sources: family " ++ s.family ++ " is not registered");
    }
}

pub fn familyByName(name: []const u8) ?*const Family {
    for (&families) |*f| if (std.mem.eql(u8, f.name, name)) return f;
    return null;
}

// ---------------------------------------------------------------------------
// The document.
// ---------------------------------------------------------------------------

/// The whole `.stats` object (no trailing newline; `Renderer` adds the ONE
/// `\n` the payload law requires).
pub fn writeAll(src: *Sources, w: *Writer) Error!void {
    try w.writeByte('{');
    inline for (families, 0..) |f, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{f.name});
        try f.write(src, w);
    }
    try w.writeByte('}');
}

fn writeJsonString(w: *Writer, s: []const u8) Error!void {
    try w.writeByte('"');
    for (s) |c| switch (c) {
        '"' => try w.writeAll("\\\""),
        '\\' => try w.writeAll("\\\\"),
        '\n' => try w.writeAll("\\n"),
        else => if (c < 0x20) try w.print("\\u{x:0>4}", .{c}) else try w.writeByte(c),
    };
    try w.writeByte('"');
}

fn writeBuild(src: *Sources, w: *Writer) Error!void {
    try w.writeAll("{\"train\":");
    try writeJsonString(w, src.build.train);
    try w.writeAll(",\"commit\":");
    try writeJsonString(w, src.build.commit);
    try w.writeAll(",\"profile\":");
    try writeJsonString(w, src.build.profile);
    try w.writeByte('}');
}

/// The mount identity and posture, plus the A/B levers set non-default
/// (charter §1.6: a row's posture travels in the row's own snapshot).
fn writeMount(src: *Sources, w: *Writer) Error!void {
    const m = src.mount;
    try w.print("{{\"uid\":{d},\"gid\":{d},\"mountpoint\":", .{ m.uid, m.gid });
    try writeJsonString(w, m.mountpoint);
    try w.writeAll(",\"sqmeta_uri\":");
    try writeJsonString(w, m.sqmeta_uri);
    try w.writeAll(",\"sqdata_uri\":");
    try writeJsonString(w, m.sqdata_uri);
    try w.print(",\"read_only\":{},\"writeback\":{},\"direct_device_true\":{},\"ab_levers\":{{", .{ m.read_only, m.writeback, m.direct_device_true });
    var first = true;
    if (src.env) |env| {
        for (knobs.registry) |k| {
            if (k.class != .ab_lever) continue;
            const v = knobs.present(env.get(k.key)) orelse continue;
            if (std.mem.eql(u8, v, k.default)) continue;
            if (!first) try w.writeByte(',');
            first = false;
            try w.print("\"{s}\":", .{k.key});
            try writeJsonString(w, v);
        }
    }
    try w.writeAll("}}");
}

fn writeKeyCensusFlag(src: *Sources, w: *Writer) Error!void {
    try w.print("{}", .{src.keyCensusArmed()});
}

fn writeTransport(src: *Sources, w: *Writer) Error!void {
    if (is_linux) {
        if (src.session) |s| return s.writeStatsJson(w);
    }
    try w.writeAll("{}");
}

/// The census keys (§1.4): present on every render, populated only when
/// the knob is armed AND the plane bound its iterators.
const census_keys = [_][]const u8{ "read_lru_keys", "write_lru_keys", "nvme_read_cache_block_keys", "nvme_staged_write_file_ids", "pack_open_blocks_census" };

fn writeData(src: *Sources, w: *Writer) Error!void {
    try w.writeAll("{\"families\":");
    if (is_linux) {
        if (src.plane) |p| try p.writeFamiliesJson(w) else try w.writeAll("{}");
    } else try w.writeAll("{}");
    try w.writeAll(",\"transform\":");
    try counters.writeNamespaceJson(transform.stats, w);
    try w.writeAll(",\"write_seams\":");
    try counters.writeNamespaceJson(write_seams.stats, w);
    try w.writeAll(",\"census\":");
    if (src.keyCensusArmed()) {
        if (src.key_census) |kc| {
            try kc.write(kc.ctx, w);
        } else try writeEmptyCensus(w);
    } else try writeEmptyCensus(w);
    try w.writeByte('}');
}

fn writeEmptyCensus(w: *Writer) Error!void {
    try w.writeByte('{');
    for (census_keys, 0..) |k, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":[]", .{k});
    }
    try w.writeAll(",\"active_writes\":{}}");
}

fn writeMeta(src: *Sources, w: *Writer) Error!void {
    try w.writeAll("{\"conveyor\":");
    if (is_linux) {
        if (src.backend) |be| {
            try be.conveyorStats().writeJson(w);
            try w.writeAll(",\"conveyor_phases\":");
            try be.conveyorStats().writePhasesJson(w);
            try w.writeAll(",\"backend\":");
            try counters.writeFieldsJson(&backend.stats, w);
            // The checkpointer's per-instance gauges (`meta_kv_checkpoint_
            // {term,late_max,barrier,trigger,projected}_ms`, the units,
            // `reusable_upto`, `dirty_nodes`): the instrument fields alone.
            try w.writeAll(",\"checkpointer\":");
            try counters.writeFieldsJson(&be.cp, w);
        } else try w.writeAll("{},\"conveyor_phases\":{},\"backend\":{},\"checkpointer\":{}");
    } else try w.writeAll("{},\"conveyor_phases\":{},\"backend\":{},\"checkpointer\":{}");
    try w.writeAll(",\"meta_barrier\":");
    try checkpoint.stats.meta_barrier.snapshot().writeJson(w);
    try w.writeAll(",\"checkpoint\":");
    try counters.writeNamespaceJson(checkpoint.stats, w);
    try w.writeAll(",\"journal_lane\":");
    if (is_linux) try journal_lane.writeStatsJson(w) else try w.writeAll("{}");
    try w.writeAll(",\"device_loader\":");
    if (is_linux) try counters.writeFieldsJson(&device_loader.stats, w) else try w.writeAll("{}");
    try w.writeAll(",\"guard\":");
    try guard.writeStatsJson(w);
    inline for (.{
        .{ "node_cache", node_cache.stats },       .{ "tree", tree.stats },            .{ "smo", smo.stats },
        .{ "leaf_merge", leaf_merge.stats },       .{ "alloc_ext", alloc_ext },        .{ "journal", journal },
        .{ "ledger", ledger },                     .{ "record", record.stats },        .{ "key", key.stats },
        .{ "tx", tx.stats },                       .{ "economy", economy.stats },      .{ "inos", inos.stats },
        .{ "dlm", dlm },                           .{ "heap_law", heap_law.stats },    .{ "node", node },
        .{ "node_snapshot", node_snapshot.stats }, .{ "node_state", node_state_core },
    }) |m| {
        try w.print(",\"{s}\":", .{m[0]});
        try counters.writeNamespaceJson(m[1], w);
    }
    try w.writeByte('}');
}

fn writeFuse(_: *Sources, w: *Writer) Error!void {
    try w.writeByte('{');
    inline for (.{ .{ "lifecycle", lifecycle }, .{ "op_guard", op_guard }, .{ "open_table", open_table }, .{ "watchdog", watchdog }, .{ "kv_fs", kv_fs }, .{ "virtual_inodes", virtual_inodes.stats }, .{ "codec", codec } }, 0..) |m, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{m[0]});
        try counters.writeNamespaceJson(m[1], w);
    }
    try w.writeByte('}');
}

fn writeExec(src: *Sources, w: *Writer) Error!void {
    try w.writeAll("{\"sqz_io\":");
    if (is_linux) {
        if (src.sqz) |s| try s.writeStatsJson(w) else try exec_stats.writeStatsJson(w, &.{});
    } else try exec_stats.writeStatsJson(w, &.{});
    try w.writeAll(",\"placement\":");
    try counters.writeNamespaceJson(placement, w);
    try w.writeByte('}');
}

fn writeSync(_: *Sources, w: *Writer) Error!void {
    try w.writeAll("{\"tick\":");
    try counters.writeNamespaceJson(tick, w);
    try w.writeAll(",\"arc_swap\":");
    try counters.writeNamespaceJson(arc_swap, w);
    try w.writeByte('}');
}

fn writeIo(_: *Sources, w: *Writer) Error!void {
    try w.writeAll("{\"uring_fs\":");
    if (is_linux) try uring_fs.writeStatsJson(w) else try w.writeAll("{}");
    try w.writeAll(",\"nvme_derive\":");
    try nvme_derive.writeStatsJson(w);
    try w.writeByte('}');
}

fn writeMemBudget(_: *Sources, w: *Writer) Error!void {
    try mem_budget.authority.writeStatsJson(w);
}

/// `fuse_op_latency_ns` always; the phase family only when the profile
/// lever is armed (the ONE armed-only family).
fn writeOps(_: *Sources, w: *Writer) Error!void {
    try w.writeAll("{\"fuse_op_latency_ns\":");
    try op_trace.writeLatencyClassesJson(w);
    if (op_trace.armed()) {
        try w.writeAll(",\"fuse_op_phase_ns\":");
        try op_trace.writePhaseClassesJson(w);
    }
    try w.writeByte('}');
}

fn writeOpTrace(_: *Sources, w: *Writer) Error!void {
    try op_trace.writeGaugesJson(w);
}

/// ONE sample per render: the per-thread classes BEFORE the process total
/// so `Σ classes ≤ total` (the sample's own law).
fn writeCpu(src: *Sources, w: *Writer) Error!void {
    const zero: cpu_class.Sample = .{ .total_ns = 0, .by_class = @splat(0) };
    const ledger_ptr = src.cpu orelse return zero.writeJson(w);
    const gpa = src.gpa orelse return zero.writeJson(w);
    const io = src.io orelse return zero.writeJson(w);
    const sample = cpu_class.sample(ledger_ptr, gpa, io, "/proc") catch zero;
    try sample.writeJson(w);
}

fn writeSelf(_: *Sources, w: *Writer) Error!void {
    try counters.writeNamespaceJson(stats, w);
}

/// The headline: the op count, the in-flight count, the volume latch, the
/// tripwire total with its per-site census (§7 Q2 — the reference exported
/// only the total), and the closure identities as computed booleans.
fn writeDaemon(src: *Sources, w: *Writer) Error!void {
    var failed = false;
    if (is_linux) {
        if (src.backend) |be| failed = be.isFailed();
    }
    try w.print("{{\"fuse_ops\":{d},\"fuse_ops_inflight\":{d},\"volume_failed\":{},\"open_count_stranded\":{d},\"invariant_tripwires\":{d},\"invariant_tripwire_sites\":{{", .{
        lifecycle.fuse_ops.load(),
        watchdog.fuse_ops_inflight.load(),
        failed,
        counters.tripwireCount("open_count_stranded"),
        counters.invariantTripwires(),
    });
    var sites: SiteWriter = .{ .w = w };
    counters.forEachTripwireSite(&sites, SiteWriter.visit);
    try w.print("}},\"tripwire_sites_overflow\":{d},\"closure\":", .{counters.tripwireSitesOverflow()});
    try closure().writeJson(w);
    try w.writeByte('}');
}

const SiteWriter = struct {
    w: *Writer,
    first: bool = true,
    failed: bool = false,

    fn visit(self: *SiteWriter, site: []const u8, count: u64) void {
        if (count == 0) return;
        if (!self.first) self.w.writeByte(',') catch {
            self.failed = true;
            return;
        };
        self.first = false;
        self.w.print("\"{s}\":{d}", .{ site, count }) catch {
            self.failed = true;
        };
    }
};

// ---------------------------------------------------------------------------
// Closure identities (charter §10): computed from the same snapshot the
// terms are read in. A render is not a quiescent point, so a false value
// on a live mount is a race, not a bug; the smoke asserts them quiescent.
// ---------------------------------------------------------------------------

pub const Closure = struct {
    /// `Σ admit_write_*_bytes ≡ transport WRITE payload bytes + the
    /// truncate's zero tails` (every user byte admitted once).
    admission_bytes: u64,
    transport_write_bytes: u64,
    /// `read_user_bytes ≡ Σ read copy/dma/zc serve bytes`.
    read_user_bytes: u64,
    read_copy_bytes: u64,
    /// `terminal_frees ≡ queued + elided`.
    terminal_frees: u64,
    reclaim_queued_plus_elided: u64,
    /// `unpublished_mint_abandons ≤ supersessions + discards`.
    mint_abandons: u64,
    mint_abandons_bound: u64,
    /// `parked_commits ≡ unparked_commits` (the transport's park pair).
    parked_commits: u64,
    unparked_commits: u64,

    pub fn admissionClosed(self: Closure) bool {
        return self.admission_bytes == self.transport_write_bytes;
    }
    pub fn readCopyClosed(self: Closure) bool {
        return self.read_user_bytes == self.read_copy_bytes;
    }
    pub fn reclaimClosed(self: Closure) bool {
        return self.terminal_frees == self.reclaim_queued_plus_elided;
    }
    pub fn mintAbandonsBounded(self: Closure) bool {
        return self.mint_abandons <= self.mint_abandons_bound;
    }
    pub fn parksClosed(self: Closure) bool {
        return self.parked_commits == self.unparked_commits;
    }

    pub fn writeJson(self: Closure, w: *Writer) Error!void {
        try w.print("{{\"admission_ledger_closed\":{},\"read_copy_ledger_closed\":{},\"reclaim_ledger_closed\":{},\"mint_abandons_bounded\":{},\"transport_parks_closed\":{}}}", .{ self.admissionClosed(), self.readCopyClosed(), self.reclaimClosed(), self.mintAbandonsBounded(), self.parksClosed() });
    }
};

pub fn closure() Closure {
    const wa = &write_admit.stats;
    const rp = &read_path.stats;
    const zc_direct: u64 = if (is_linux) worker.zc_write_direct_bytes.load() else 0;
    return .{
        .admission_bytes = wa.admit_write_overlay_bytes.load() + wa.admit_write_inplace_bytes.load() + wa.admit_write_accumulate_bytes.load() + wa.admit_write_inline_bytes.load(),
        .transport_write_bytes = slot.zc_write_extract_bytes.load() + zc_direct + kv_fs.fuse_truncate_zero_tail_bytes.load(),
        .read_user_bytes = rp.read_user_bytes.load(),
        .read_copy_bytes = rp.read_copy_dest_bytes.load() + rp.read_copy_bounce_bytes.load() + rp.read_dest_dma_bytes.load() + rp.read_zc_serve_bytes.load() + rp.read_zc_pool_serve_bytes.load(),
        .terminal_frees = reclaim.stats.terminal_frees.load(),
        .reclaim_queued_plus_elided = reclaim.stats.block_free_reclaim_queued.load() + reclaim.stats.block_free_reclaim_elided.load(),
        .mint_abandons = block_alloc.stats.unpublished_mint_abandons.load(),
        .mint_abandons_bound = write_pipeline.stats.write_pipeline_supersessions.load() + write_pipeline.stats.write_pipeline_discards.load(),
        .parked_commits = slot.parked_commits.load(),
        .unparked_commits = slot.unparked_commits.load(),
    };
}

// ---------------------------------------------------------------------------
// The renderer: ONE allocation per payload, sized from the last render.
// ---------------------------------------------------------------------------

/// A rendered payload: the capacity the render reserved and the bytes it
/// used (`bytes()`), freed whole. The payload ends with ONE `\n` and no
/// padding (`finish_virtual_payload`'s law: `attr.size` is exactly the
/// served bytes).
pub const Payload = struct {
    buf: []u8,
    len: usize,

    pub fn bytes(self: Payload) []const u8 {
        return self.buf[0..self.len];
    }

    pub fn deinit(self: Payload, gpa: Allocator) void {
        gpa.free(self.buf);
    }
};

/// The slack over the previous length: counters grow by digits (a 12.5 %
/// margin covers every term doubling) and a page absorbs a family that
/// appears (a tripwire site, a lane); a first render has no history and
/// takes the field-measured payload class (≈ 72 KiB → 128 KiB).
pub const FIRST_RENDER_CAPACITY: usize = 128 << 10;

pub fn capacityHint(last_len: usize) usize {
    if (last_len == 0) return FIRST_RENDER_CAPACITY;
    return last_len + last_len / 8 + 4096;
}

pub const Renderer = struct {
    gpa: Allocator,
    sources: Sources,
    last_len: usize = 0,
    cpu: cpu_class.Ledger,

    pub fn init(gpa: Allocator, src: Sources) Renderer {
        var r: Renderer = .{ .gpa = gpa, .sources = src, .cpu = .{} };
        r.sources.gpa = gpa;
        return r;
    }

    pub fn deinit(self: *Renderer) void {
        self.cpu.deinit(self.gpa);
        self.* = undefined;
    }

    /// The `.stats` document. One allocation when the hint holds
    /// (`stats_build_regrows` counts the renders it did not).
    pub fn render(self: *Renderer) Allocator.Error!Payload {
        // The ledger pointer must follow the renderer (a moved Renderer).
        self.sources.cpu = &self.cpu;
        var out: Writer.Allocating = try .initCapacity(self.gpa, capacityHint(self.last_len));
        errdefer out.deinit();
        const cap = out.writer.buffer.len;
        writeAll(&self.sources, &out.writer) catch return error.OutOfMemory;
        out.writer.writeByte('\n') catch return error.OutOfMemory;
        if (out.writer.buffer.len != cap) stats.stats_build_regrows.inc();
        return self.finish(&out);
    }

    /// `.config`: the live twin of `status`' set summary.
    pub fn renderConfig(self: *Renderer, write_set: *const fn (*anyopaque, *Writer) Error!void, ctx: *anyopaque) Allocator.Error!Payload {
        var out: Writer.Allocating = try .initCapacity(self.gpa, 4096);
        errdefer out.deinit();
        const w = &out.writer;
        w.writeAll("{\"build\":") catch return error.OutOfMemory;
        writeBuild(&self.sources, w) catch return error.OutOfMemory;
        w.writeAll(",\"mount\":") catch return error.OutOfMemory;
        writeMount(&self.sources, w) catch return error.OutOfMemory;
        w.writeAll(",\"set\":") catch return error.OutOfMemory;
        write_set(ctx, w) catch return error.OutOfMemory;
        w.writeAll(",\"sqmeta_uri\":") catch return error.OutOfMemory;
        writeJsonString(w, self.sources.mount.sqmeta_uri) catch return error.OutOfMemory;
        w.writeAll(",\"sqdata_uri\":") catch return error.OutOfMemory;
        writeJsonString(w, self.sources.mount.sqdata_uri) catch return error.OutOfMemory;
        w.writeAll("}\n") catch return error.OutOfMemory;
        return self.finish(&out);
    }

    fn finish(self: *Renderer, out: *Writer.Allocating) Payload {
        const len = out.writer.end;
        const buf = out.writer.buffer;
        out.writer.buffer = &.{};
        out.writer.end = 0;
        self.last_len = len;
        stats.stats_builds.inc();
        if (len > stats.stats_build_bytes_max.load()) stats.stats_build_bytes_max.set(len);
        return .{ .buf = buf, .len = len };
    }
};

/// The SIGUSR1 dump: one log line per family (`stats <name>: {…}`), the
/// daemon line last. The same writers as the inode — there is ONE source.
pub fn logFamilies(src: *Sources, gpa: Allocator) void {
    var out: Writer.Allocating = .init(gpa);
    defer out.deinit();
    inline for (families) |f| {
        out.clearRetainingCapacity();
        if (f.write(src, &out.writer)) {
            log.info("stats {s}: {s}", .{ f.name, out.written() });
        } else |_| {
            log.err("stats {s}: the family could not render (out of memory)", .{f.name});
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (portable): the schema, the order, the tripwire census, the census
// keys, the armed-only family, the allocation law.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// A tiny validating JSON scanner: balanced braces/brackets outside
/// strings, no trailing garbage; returns the depth-1 keys in order.
fn scanJson(gpa: Allocator, text: []const u8, keys: *std.ArrayList([]const u8)) !void {
    var depth: usize = 0;
    var in_str = false;
    var esc = false;
    var key_start: ?usize = null;
    var expect_key = false;
    var i: usize = 0;
    while (i < text.len) : (i += 1) {
        const c = text[i];
        if (in_str) {
            if (esc) {
                esc = false;
            } else if (c == '\\') {
                esc = true;
            } else if (c == '"') {
                in_str = false;
                if (depth == 1 and expect_key) {
                    try keys.append(gpa, text[key_start.?..i]);
                    expect_key = false;
                }
            }
            continue;
        }
        switch (c) {
            '"' => {
                in_str = true;
                key_start = i + 1;
            },
            '{', '[' => {
                depth += 1;
                if (depth == 1 and c == '{') expect_key = true;
            },
            '}', ']' => {
                if (depth == 0) return error.Unbalanced;
                depth -= 1;
            },
            ',' => if (depth == 1) {
                expect_key = true;
            },
            ':' => if (depth == 1) {
                expect_key = false;
            },
            '\n' => if (depth != 0) return error.NewlineInside,
            else => {},
        }
    }
    if (depth != 0 or in_str) return error.Unbalanced;
}

test "render: the document parses, names every family in the schema's order with daemon last, ends with ONE newline, and the size is exactly the served bytes" {
    var r = Renderer.init(testing.allocator, .{ .build = .{ .train = "0.1.0", .commit = "abc", .profile = "gate" }, .mount = .{ .uid = 7, .mountpoint = "/mnt/x" } });
    defer r.deinit();
    const p = try r.render();
    defer p.deinit(testing.allocator);
    const text = p.bytes();
    try testing.expect(std.mem.endsWith(u8, text, "}\n"));
    try testing.expect(!std.mem.endsWith(u8, text, "}\n\n"));
    var keys: std.ArrayList([]const u8) = .empty;
    defer keys.deinit(testing.allocator);
    try scanJson(testing.allocator, text[0 .. text.len - 1], &keys);
    try testing.expectEqual(families.len, keys.items.len);
    for (families, keys.items) |f, k| try testing.expectEqualStrings(f.name, k);
    try testing.expectEqualStrings("daemon", keys.items[keys.items.len - 1]);
    try testing.expect(std.mem.indexOf(u8, text, "\"build\":{\"train\":\"0.1.0\",\"commit\":\"abc\",\"profile\":\"gate\"}") != null);
    try testing.expect(std.mem.indexOf(u8, text, "\"mountpoint\":\"/mnt/x\"") != null);
    try testing.expect(std.mem.indexOf(u8, text, "\"invariant_tripwires\":") != null);
    try testing.expect(std.mem.indexOf(u8, text, "\"lock_ticked_reregisters\":") != null);
    try testing.expect(std.mem.indexOf(u8, text, "\"fuse_op_watchdog_overdue\":") != null);
    try testing.expect(std.mem.indexOf(u8, text, "\"open_count_stranded\":") != null);
    try testing.expect(std.mem.indexOf(u8, text, "\"meta_kv_checkpoints\":") != null);
    try testing.expect(std.mem.indexOf(u8, text, "\"stats_builds\":") != null);
    try testing.expectEqual(p.len, text.len);
}

test "every instrument declared in a generated namespace is a key of the document (declared ⇒ exported)" {
    var r = Renderer.init(testing.allocator, .{});
    defer r.deinit();
    const p = try r.render();
    defer p.deinit(testing.allocator);
    const text = p.bytes();
    inline for (.{ node_cache.stats, tree.stats, smo.stats, leaf_merge.stats, alloc_ext, journal, ledger, record.stats, key.stats, tx.stats, economy.stats, inos.stats, dlm, heap_law.stats, node, node_snapshot.stats, node_state_core, checkpoint.stats, lifecycle, op_guard, open_table, watchdog, kv_fs, virtual_inodes.stats, codec, placement, tick, arc_swap, transform.stats, write_seams.stats, stats }) |Ns| {
        inline for (comptime counters.instrumentNames(Ns)) |name| {
            const T = @TypeOf(@field(Ns, name));
            const k = if (T == counters.Tripwire) @field(Ns, name).site else name;
            var needle_buf: [256]u8 = undefined;
            const needle = try std.fmt.bufPrint(&needle_buf, "\"{s}\":", .{k});
            if (std.mem.indexOf(u8, text, needle) == null) {
                std.debug.print("\n--- instrument {s} not exported ---\n", .{k});
                return error.InstrumentNotExported;
            }
        }
    }
}

test "invariant_tripwire_sites carries a fired site with its count beside the total; unfired sites are absent" {
    const S = struct {
        var tw: counters.Tripwire = .init("stats_test_fired_site");
    };
    log.testing.expectErrorLogs(1);
    S.tw.fire("the stats test fired it");
    try log.testing.expectAllFired();
    var r = Renderer.init(testing.allocator, .{});
    defer r.deinit();
    const p = try r.render();
    defer p.deinit(testing.allocator);
    const text = p.bytes();
    const sites_at = std.mem.indexOf(u8, text, "\"invariant_tripwire_sites\":{").?;
    const sites_end = std.mem.indexOfPos(u8, text, sites_at, "}").?;
    const sites = text[sites_at..sites_end];
    try testing.expect(std.mem.indexOf(u8, sites, "\"stats_test_fired_site\":1") != null);
    try testing.expect(std.mem.indexOf(u8, sites, "never_fired_site") == null);
    try testing.expect(std.mem.indexOf(u8, text, "\"tripwire_sites_overflow\":0") != null);
}

test "the key census is opt-in: the keys are present-and-empty off, read live (not memoised) from the environment, and a bound seam writes when armed" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const Seam = struct {
        fn write(_: *anyopaque, w: *Writer) Error!void {
            try w.writeAll("{\"read_lru_keys\":[\"k1\"],\"active_writes\":{\"7\":[0,1]}}");
        }
    };
    var dummy: u8 = 0;
    var r = Renderer.init(testing.allocator, .{ .env = &env, .key_census = .{ .ctx = &dummy, .write = Seam.write } });
    defer r.deinit();
    {
        const p = try r.render();
        defer p.deinit(testing.allocator);
        try testing.expect(std.mem.indexOf(u8, p.bytes(), "\"stats_key_census\":false") != null);
        try testing.expect(std.mem.indexOf(u8, p.bytes(), "\"census\":{\"read_lru_keys\":[],\"write_lru_keys\":[],\"nvme_read_cache_block_keys\":[],\"nvme_staged_write_file_ids\":[],\"pack_open_blocks_census\":[],\"active_writes\":{}}") != null);
    }
    try env.put("SQUEEZEFS_STATS_KEY_CENSUS", "1");
    {
        const p = try r.render();
        defer p.deinit(testing.allocator);
        try testing.expect(std.mem.indexOf(u8, p.bytes(), "\"stats_key_census\":true") != null);
        try testing.expect(std.mem.indexOf(u8, p.bytes(), "\"census\":{\"read_lru_keys\":[\"k1\"]") != null);
    }
    try env.put("SQUEEZEFS_STATS_KEY_CENSUS", "0");
    const p = try r.render();
    defer p.deinit(testing.allocator);
    try testing.expect(std.mem.indexOf(u8, p.bytes(), "\"stats_key_census\":false") != null);
}

test "op_profile is the ONE armed-only family: absent off, present on; the disarmed `ops` family's bytes are identical across a toggle" {
    op_trace.setArmedForTest(false);
    defer op_trace.setArmedForTest(false);
    var src: Sources = .{};
    var a: Writer.Allocating = .init(testing.allocator);
    defer a.deinit();
    try writeOps(&src, &a.writer);
    try testing.expect(std.mem.indexOf(u8, a.written(), "fuse_op_phase_ns") == null);
    try testing.expect(std.mem.startsWith(u8, a.written(), "{\"fuse_op_latency_ns\":{\"lookup\":{\"buckets\":"));
    op_trace.setArmedForTest(true);
    var b: Writer.Allocating = .init(testing.allocator);
    defer b.deinit();
    try writeOps(&src, &b.writer);
    try testing.expect(std.mem.indexOf(u8, b.written(), "\"fuse_op_phase_ns\":{\"lookup\":{\"handler_to_backend\":{\"buckets\":") != null);
    op_trace.setArmedForTest(false);
    var c: Writer.Allocating = .init(testing.allocator);
    defer c.deinit();
    try writeOps(&src, &c.writer);
    try testing.expectEqualStrings(a.written(), c.written());
}

test "the A/B levers set non-default ride the mount family; a default-valued lever does not" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try env.put("SQUEEZEFS_WRITE_SHARED", "0");
    try env.put("SQUEEZEFS_NUMA", "on");
    var src: Sources = .{ .env = &env };
    var a: Writer.Allocating = .init(testing.allocator);
    defer a.deinit();
    try writeMount(&src, &a.writer);
    try testing.expect(std.mem.indexOf(u8, a.written(), "\"ab_levers\":{\"SQUEEZEFS_WRITE_SHARED\":\"0\"}") != null);
}

test "closure identities are computed booleans over one snapshot" {
    const c: Closure = .{ .admission_bytes = 10, .transport_write_bytes = 10, .read_user_bytes = 5, .read_copy_bytes = 4, .terminal_frees = 3, .reclaim_queued_plus_elided = 3, .mint_abandons = 1, .mint_abandons_bound = 0, .parked_commits = 2, .unparked_commits = 2 };
    try testing.expect(c.admissionClosed() and !c.readCopyClosed() and c.reclaimClosed() and !c.mintAbandonsBounded() and c.parksClosed());
    var a: Writer.Allocating = .init(testing.allocator);
    defer a.deinit();
    try c.writeJson(&a.writer);
    try testing.expectEqualStrings("{\"admission_ledger_closed\":true,\"read_copy_ledger_closed\":false,\"reclaim_ledger_closed\":true,\"mint_abandons_bounded\":false,\"transport_parks_closed\":true}", a.written());
    // The live fold runs on every host (zeros close every identity on an idle process).
    _ = closure();
}

/// Counts allocator calls by kind (the allocation-law pin).
const CountingAllocator = struct {
    inner: Allocator,
    allocs: usize = 0,
    resizes: usize = 0,
    remaps: usize = 0,
    frees: usize = 0,

    fn allocator(self: *CountingAllocator) Allocator {
        return .{ .ptr = self, .vtable = &vtable };
    }
    const vtable: Allocator.VTable = .{ .alloc = alloc, .resize = resize, .remap = remap, .free = free };
    fn alloc(ctx: *anyopaque, len: usize, a: std.mem.Alignment, ra: usize) ?[*]u8 {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        self.allocs += 1;
        return self.inner.rawAlloc(len, a, ra);
    }
    fn resize(ctx: *anyopaque, buf: []u8, a: std.mem.Alignment, new_len: usize, ra: usize) bool {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        self.resizes += 1;
        return self.inner.rawResize(buf, a, new_len, ra);
    }
    fn remap(ctx: *anyopaque, buf: []u8, a: std.mem.Alignment, new_len: usize, ra: usize) ?[*]u8 {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        self.remaps += 1;
        return self.inner.rawRemap(buf, a, new_len, ra);
    }
    fn free(ctx: *anyopaque, buf: []u8, a: std.mem.Alignment, ra: usize) void {
        const self: *CountingAllocator = @ptrCast(@alignCast(ctx));
        self.frees += 1;
        self.inner.rawFree(buf, a, ra);
    }
};

test "allocation law: after the warm-up render a render is exactly ONE allocation (the payload) and no resize; the hint is last + 12.5 % + a page" {
    try testing.expectEqual(FIRST_RENDER_CAPACITY, capacityHint(0));
    try testing.expectEqual(@as(usize, 80_000 + 10_000 + 4096), capacityHint(80_000));
    var ca: CountingAllocator = .{ .inner = testing.allocator };
    var r = Renderer.init(ca.allocator(), .{});
    defer r.deinit();
    const warm = try r.render();
    warm.deinit(ca.allocator());
    const before = ca;
    const p = try r.render();
    defer p.deinit(ca.allocator());
    try testing.expectEqual(before.allocs + 1, ca.allocs);
    try testing.expectEqual(before.resizes, ca.resizes);
    try testing.expectEqual(before.remaps, ca.remaps);
    try testing.expect(p.buf.len >= p.len);
    try testing.expectEqual(r.last_len, p.len);
}

test "renderConfig: build + mount + the set writer's object + the URIs, one newline" {
    var r = Renderer.init(testing.allocator, .{ .mount = .{ .sqmeta_uri = "sqmeta:///dev/m", .sqdata_uri = "sqdata:///dev/d" } });
    defer r.deinit();
    const Set = struct {
        fn write(_: *anyopaque, w: *Writer) Error!void {
            try w.writeAll("{\"block_size\":4194304}");
        }
    };
    var dummy: u8 = 0;
    const p = try r.renderConfig(Set.write, &dummy);
    defer p.deinit(testing.allocator);
    var keys: std.ArrayList([]const u8) = .empty;
    defer keys.deinit(testing.allocator);
    try scanJson(testing.allocator, p.bytes()[0 .. p.len - 1], &keys);
    try testing.expectEqual(@as(usize, 5), keys.items.len);
    try testing.expectEqualStrings("set", keys.items[2]);
    try testing.expect(std.mem.indexOf(u8, p.bytes(), "\"set\":{\"block_size\":4194304},\"sqmeta_uri\":\"sqmeta:///dev/m\",\"sqdata_uri\":\"sqdata:///dev/d\"}\n") != null);
}

test "the family table: every name once; familyByName resolves" {
    for (families, 0..) |f, i| {
        for (families[i + 1 ..]) |g| try testing.expect(!std.mem.eql(u8, f.name, g.name));
        try testing.expectEqual(&families[i], familyByName(f.name).?);
    }
    try testing.expect(familyByName("nope") == null);
}
