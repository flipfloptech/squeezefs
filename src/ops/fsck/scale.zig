//! fsck's scaling rows (Linux; `ops_fsck.md` §6.1): a real set — one
//! member, the native slot and 63 rotor slots as slot trees, every file
//! minted round-robin over them — filled with N names in one flat
//! directory, then with N names over a 64-stripe directory, each judged by
//! the offline engine at N and at 4N. The walk's cost rides the report
//! (`census.WalkTally`): the entries the trees read stay within
//! `census.walkReadBound` (3 × visited + the trees consulted), the nodes
//! within `census.walkNodeBound` (the height per entry visited and per
//! call), and 4N costs at most 4.5 × N — counters, not wall time, so the
//! verdict is the same on an idle host and a saturated one.

const std = @import("std");
const builtin = @import("builtin");
const testing = std.testing;
const Allocator = std.mem.Allocator;

const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const inos = @import("../../meta/inos.zig");
const economy = @import("../../meta/economy.zig");
const meta_set = @import("../../meta/meta_set.zig");
const stripe = @import("../../meta/sym/xv/stripe.zig");
const census = @import("census.zig");
const fsck = @import("../fsck.zig");
const finding = @import("finding.zig");
const runner = @import("runner.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("fsck/scale is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// The scaling verdict's slack over linear: 4N may cost 4.5 × N (a
/// quadratic walk costs 16 ×; fixed per-run costs only lower the ratio).
const LINEAR_SLACK_NUM: u64 = 9;
const LINEAR_SLACK_DEN: u64 = 2;

/// A paged walk's calls past `visited / SCAN_PAGE`: its last, short page
/// and the empty one that ends a walk ending on a full page.
const C11_PAGER_TAIL: u64 = 2;

const Shape = enum { flat, striped };

/// The fixture's names (`f00000000`…): distinct, short, hashed apart.
fn nameOf(i: usize, buf: *[16]u8) []const u8 {
    return std.fmt.bufPrint(buf, "f{d:0>8}", .{i}) catch unreachable;
}

/// A directory under the root holding names `[from, to)`: plain, or striped
/// over `stripe.K_MAX` stripe directories exactly as a flip leaves it —
/// the stripe records, one marker per stripe, the commit marker naming
/// stripe 0, every name in the stripe its hash routes it to.
const Dir = struct {
    shape: Shape,
    dir: u64,
    stripes: [stripe.K_MAX]u64 = undefined,

    fn make(set: *meta_set.MetaSet, shape: Shape) !Dir {
        var d: Dir = .{ .shape = shape, .dir = try meta_set.createChild(set, inos.ROOT_INO, @tagName(shape), economy.S.IFDIR | 0o755) };
        if (shape == .flat) return d;
        const dir_eff = set.effectiveIno(d.dir);
        for (&d.stripes) |*s| s.* = set.globalIno(try set.allocateIno(dir_eff, false, false));
        try stripe.writeStripeRecords(meta_set.MetaSet, set, &d.stripes, .{ .mode = economy.S.IFDIR | 0o755, .uid = 0, .gid = 0, .ts_ns = 1000 });
        const t = try set.begin(@src());
        {
            errdefer set.abandon(t);
            var names: [stripe.K_MAX][stripe.MARKER_NAME_LEN]u8 = undefined;
            for (d.stripes, 0..) |s, i| try putName(set, t, dir_eff, stripe.markerName(@intCast(i), &names[i]), s);
            try putName(set, t, dir_eff, stripe.STRIPED, d.stripes[0]);
        }
        try set.commit(t);
        return d;
    }

    fn putName(set: *meta_set.MetaSet, t: *meta_set.KvTx, parent_eff: u64, name: []const u8, child: u64) !void {
        var buf: [record.NAME_MAX + 16]u8 = undefined;
        const v = try (record.DentryValue{ .child_ino = child, .file_type = economy.ftByte(economy.S.IFDIR), .name = name }).encodeInto(&buf);
        try t.stagePut(key.TREE_DENTRIES, &key.dentryKey(parent_eff, key.dentryNameHash54(name, set.nameKey()), 0), v);
    }

    /// Creates per entry: the fixture is the shape fsck walks, not the
    /// commit path's (one create per entry cost 10 k journal round trips in
    /// a Debug test); a parent staged twice in one entry keeps the last.
    const FILL_BATCH: usize = 128;

    fn fill(self: *const Dir, set: *meta_set.MetaSet, from: usize, to: usize) !void {
        var names: [FILL_BATCH][16]u8 = undefined;
        var i = from;
        while (i < to) {
            const t = try set.begin(@src());
            var owned = true;
            errdefer if (owned) set.abandon(t);
            const end = @min(to, i + FILL_BATCH);
            while (i < end) : (i += 1) {
                const nm = nameOf(i, &names[i % FILL_BATCH]);
                const parent = switch (self.shape) {
                    .flat => self.dir,
                    .striped => self.stripes[stripe.stripeOf(key.dentryNameHash54(nm, set.nameKey()), stripe.K_MAX)],
                };
                const parent_eff = set.effectiveIno(parent);
                const pv_bytes = (try set.lookup(set.gpa, key.TREE_INODES, &key.inodeKey(parent_eff))) orelse return error.ParentMissing;
                defer set.gpa.free(pv_bytes);
                const child = try set.allocateIno(parent_eff, false, false);
                _ = try economy.stageCreate(t, .{ .name_key = set.nameKey(), .now = 1000, .parent = parent_eff, .parent_value = try record.InodeValue.decode(pv_bytes), .name = nm, .mode = economy.S.IFREG | 0o644, .uid = 0, .gid = 0, .local_ino = child, .global_ino = set.globalIno(child) });
            }
            owned = false;
            try set.commit(t);
        }
    }
};

/// One offline run's walk cost and the set's shape at the time.
const Row = struct {
    tally: census.WalkTally,
    height: u64,
    trees: u64,
    /// The store calls C11's owner scan made (0003 §AC.6).
    c11_calls: u64,
};

fn fsckRow(gpa: Allocator, s: *meta_set.TestSet, set: *meta_set.MetaSet) !Row {
    const b = try runner.linux.bind(gpa, s.io(), set, null, .{ .env = &s.env, .mode = .offline, .mem_budget = 1 << 30 });
    defer runner.linux.destroy(b);
    var r = try runner.run(&b.host, .{ .publish = false }, null);
    defer r.deinit(gpa);
    if (r.hasFindings()) {
        for (r.findings) |f| std.debug.print("unexpected finding: {f}\n", .{f});
        return error.TestUnexpectedResult;
    }
    const c = r.counters;
    var row: Row = .{ .tally = .{ .calls = c.store_calls, .visited = c.entries_visited, .read = c.entries_read, .nodes = c.nodes_walked, .sources = c.trees_consulted }, .height = 0, .trees = 0, .c11_calls = c.c11_store_calls };
    for (set.volumes[0].trees.load()) |t| if (t.forest_slot != null) {
        row.trees += 1;
        row.height = @max(row.height, @as(u64, try t.rootLevel()) + 1);
    };
    return row;
}

fn expectWithinBounds(shape: Shape, n: usize, row: Row) !void {
    const t = row.tally;
    const read_bound = census.walkReadBound(t);
    const node_bound = census.walkNodeBound(t, row.height);
    if (t.read > read_bound or t.nodes > node_bound) {
        std.debug.print("fsck {t} at {d} names over {d} slot trees (height {d}): {d} entries visited in {d} calls consulting {d} trees — read {d} (bound {d}), nodes {d} (bound {d})\n", .{ shape, n, row.trees, row.height, t.visited, t.calls, t.sources, t.read, read_bound, t.nodes, node_bound });
        return error.TestUnexpectedResult;
    }
    // C11's owners come off ONE paged walk of the map kind (the fixture has
    // no block-map owner): a page per SCAN_PAGE entries the walk delivers,
    // never a probe per ino (0003 §AC.6).
    const c11_bound = t.visited / fsck.SCAN_PAGE + C11_PAGER_TAIL;
    if (row.c11_calls > c11_bound) {
        std.debug.print("fsck {t} at {d} names: C11's owner scan made {d} store calls for {d} entries visited (bound {d})\n", .{ shape, n, row.c11_calls, t.visited, c11_bound });
        return error.TestUnexpectedResult;
    }
}

fn scalingRow(comptime shape: Shape) !void {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.createClass(gpa, "fsck-scale-" ++ @tagName(shape), 1, .single_writer);
    defer s.destroy();
    const set = try s.open();
    defer set.close();
    const n: usize = 2_000;
    const d = try Dir.make(set, shape);
    try d.fill(set, 0, n);
    const small = try fsckRow(gpa, s, set);
    try expectWithinBounds(shape, n, small);
    try d.fill(set, n, 4 * n);
    const large = try fsckRow(gpa, s, set);
    try expectWithinBounds(shape, 4 * n, large);
    // Linear: 4N within 4.5 × N on reads; nodes too, scaled by a level
    // the trees may have grown.
    const read_cap = small.tally.read * LINEAR_SLACK_NUM / LINEAR_SLACK_DEN;
    const node_cap = small.tally.nodes * LINEAR_SLACK_NUM * large.height / (LINEAR_SLACK_DEN * small.height);
    if (large.tally.read > read_cap or large.tally.nodes > node_cap) {
        std.debug.print("fsck {t}: {d} → {d} names read {d} → {d} entries (cap {d}), {d} → {d} nodes (cap {d})\n", .{ shape, n, 4 * n, small.tally.read, large.tally.read, read_cap, small.tally.nodes, large.tally.nodes, node_cap });
        return error.TestUnexpectedResult;
    }
}

test "fsck scales linearly over one flat directory: 2 k → 8 k names on 64 slot trees, the walk's reads within 3 × the entries visited + the trees consulted, node reads within the tree height per entry, 4N ≤ 4.5 × N (rung-10 gate-speed §2.2: 0.04 → 1.13 s for 2 k → 16 k entries, near-quadratic)" {
    try scalingRow(.flat);
}

test "fsck scales linearly over one 64-stripe directory: 2 k → 8 k names, the same bounds (the hot-directory row's closing fsck took 83.5 s for 53 k striped entries)" {
    try scalingRow(.striped);
}
