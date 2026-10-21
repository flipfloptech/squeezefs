//! The portable contracts over the RAM set (`ram_set.zig`): every class's
//! red → finding → plan → apply → clean, the zero-FP rows (the era floor,
//! the storm), the ladder's shields, sharding + merge, the report, C14's
//! refusal shape. The device/mount rows are `runner_linux_e2e.zig`.

const std = @import("std");
const testing = std.testing;
const Allocator = std.mem.Allocator;

const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const inos = @import("../../meta/inos.zig");
const economy = @import("../../meta/economy.zig");
const layout = @import("../../data/layout.zig");
const block_refs = @import("../../data/block_refs.zig");
const block_alloc = @import("../../data/block_alloc.zig");
const data_bitmap = @import("../../data/sym_planes/data_bitmap.zig");
const transform = @import("../../data/transform.zig");
const appender_page = @import("../../meta/sym/appender_page.zig");
const cr = @import("../../meta/sym/control_records.zig");
const fsck = @import("../fsck.zig");
const finding = @import("finding.zig");
const census = @import("census.zig");
const classes = @import("classes.zig");
const repair = @import("repair.zig");
const scrub = @import("scrub.zig");
const ram = @import("ram_set.zig");

const Finding = finding.Finding;
const Class = finding.Class;
const RamSet = ram.RamSet;
const CHUNK = ram.CHUNK;
const VOL_A = ram.VOL_A;
const VOL_B = ram.VOL_B;

/// A healthy populated set: dirs, files on both volumes, a packed pair, a
/// hard link, a clone sharing a block.
fn populate(set: *RamSet) !struct { dir: u64, f1: u64, f2: u64, packed_a: u64, packed_b: u64 } {
    const d = try set.mkdir(inos.ROOT_INO, "dir");
    const f1 = try set.mkfile(d, "f1", VOL_A, &.{ 1, 2, 3 });
    const f2 = try set.mkfile(inos.ROOT_INO, "f2", VOL_B, &.{ 5, 6 });
    _ = try set.mkfile(d, "clone", VOL_A, &.{3});
    try set.link(d, "f1-link", f1);
    const pa = try set.mkpacked(d, "pa", VOL_A, 10, 0, 1000);
    const pb = try set.mkpacked(d, "pb", VOL_A, 10, 4096, 2000);
    try set.freezeEra();
    return .{ .dir = d, .f1 = f1, .f2 = f2, .packed_a = pa, .packed_b = pb };
}

const RamSink = struct {
    gpa: Allocator,
    puts: std.ArrayList(struct { class: Class, bytes: usize }) = .empty,
    fn put(ctx: *anyopaque, class: Class, _: []const u8, bytes: []const u8) census.Error!void {
        const self: *RamSink = @ptrCast(@alignCast(ctx));
        try self.puts.append(self.gpa, .{ .class = class, .bytes = bytes.len });
    }
    fn sink(self: *RamSink) classes.Quarantine.Sink {
        return .{ .ctx = self, .put = &put };
    }
    fn deinit(self: *RamSink) void {
        self.puts.deinit(self.gpa);
    }
};

fn repairOpts(sink: ?*RamSink, apply: bool) repair.Options {
    return .{ .apply = apply, .quarantine = if (sink) |s| s.sink() else null };
}

fn dumpRepair(r: finding.Report) void {
    for (r.findings) |f| std.debug.print("  finding: {f}\n", .{f});
    if (r.repair) |rep| for (rep.rows) |row| std.debug.print("  {s}: {s} — {s} ({f})\n", .{ @tagName(row.outcome), @tagName(row.verb), row.detail, row.finding });
}

test "a healthy populated set reports zero findings offline and online, with every census counter engaged and the exit verdict 0" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    inline for (.{ finding.Mode.offline, finding.Mode.online }) |mode| {
        var r = try ram.run(set, mode, .{ .scrub = true });
        defer r.deinit(gpa);
        try ram.expectClasses(r, &.{});
        try testing.expectEqual(@as(u8, 0), r.exitCode());
        try testing.expect(r.counters.inodes_scanned >= 7);
        try testing.expect(r.counters.dentry_refs_indexed >= 7);
        try testing.expect(r.counters.blocks_checked >= 6);
        try testing.expect(r.counters.refcounts_checked >= 6);
        try testing.expect(r.counters.scrub_blocks_scanned >= 7);
        try testing.expect(r.counters.scrub_readability_only == r.counters.scrub_blocks_scanned);
        try testing.expectEqual(@as(u64, 0), r.counters.scrub_failures);
        // The walk's cost rides the report: a store without trees reads
        // exactly what it delivers and fetches no node.
        try testing.expect(r.counters.entries_visited > 0 and r.counters.store_calls > 0);
        try testing.expectEqual(r.counters.entries_visited, r.counters.entries_read);
        try testing.expectEqual(@as(u64, 0), r.counters.nodes_walked);
    }
}

test "the fleet member's residue is its custody: `custody_only` takes the block census from the inos of the slots this mount leases alone (a live lessee's slots are readable only on the lessee), so the scrub reads nothing of a slot it does not hold and everything of one it does" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    var h = set.host(.online);
    h.custody_only = true;
    // Slot 0 is this mount's: the custody IS the whole fixture.
    var mine = try ram.runHost(set, &h, .{ .scrub = true });
    defer mine.deinit(gpa);
    try ram.expectClasses(mine, &.{});
    try testing.expect(mine.counters.scrub_blocks_scanned >= 7);
    try testing.expect(mine.counters.inodes_scanned >= 7);
    // Nothing leased here (another member's custody): no row, no block.
    _ = set.scope.leased.remove(0);
    defer set.scope.leased.put(gpa, 0, {}) catch unreachable;
    var theirs = try ram.runHost(set, &h, .{ .scrub = true });
    defer theirs.deinit(gpa);
    try ram.expectClasses(theirs, &.{});
    try testing.expectEqual(@as(u64, 0), theirs.counters.scrub_blocks_scanned);
    try testing.expectEqual(@as(u64, 0), theirs.counters.inodes_scanned);
}

test "the fleet's MERGED block census: a member's proposal carries its custody's owners; the coordinator's lost arm names a block the member's layout binds that the holder tracks no more (a nonterminal free executed under live tenants — the data-loss shape fsck read as clean), C3 the holder's count against the merged references; a fresh carve (the bitmap's bit) and an in-flight mint are exempt; a suspect stands only when the member's SECOND census names it the same way" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    var h = set.host(.online);
    // The coordinator's own view ships its owners too.
    var local = try ram.runHost(set, &h, .{ .ship_owners = true });
    defer local.deinit(gpa);
    try ram.expectClasses(local, &.{});
    try testing.expect(local.owners.len >= 4);
    // The member's census: ino (slot 7, raw 5) binds block 40 of A — a
    // block this holder freed under it (untracked) — and block 1 of A,
    // which the holder tracks at refcount 1 (its own file's).
    const theirs_binds = [_]finding.BindingRef{
        .{ .vol_tag = VOL_A, .block_idx = 40, .block_index = 0 },
        .{ .vol_tag = VOL_A, .block_idx = 1, .block_index = 1 },
    };
    const theirs = [_]finding.OwnerRef{.{ .ino = key.guestLocalIno(7, 5), .bindings = &theirs_binds }};
    var c: finding.Counters = .{};
    var first: std.ArrayList(Finding) = .empty;
    defer first.deinit(gpa);
    try classes.crossMemberBlockPlane(gpa, &h, local.owners, &theirs, &c, &first);
    try testing.expectEqual(@as(u64, 2), c.block_plane_member_bindings);
    try testing.expectEqual(@as(usize, 2), first.items.len);
    var seen_lost = false;
    var seen_c3 = false;
    for (first.items) |f| switch (f) {
        .c2_lost => |x| seen_lost = x.block_idx == 40 and x.ino == key.guestLocalIno(7, 5) and x.block_index == 0 and !x.unrepairable_shape,
        .c3_refcount => |x| seen_c3 = x.block_idx == 1 and x.actual == 1 and x.expected == 2,
        else => return error.TestUnexpectedResult,
    };
    try testing.expect(seen_lost and seen_c3);
    // The second census: the member deleted the file that bound block 1
    // (its reference gone) and still binds block 40 → the lost stands, the
    // C3 clears.
    const theirs2_binds = [_]finding.BindingRef{.{ .vol_tag = VOL_A, .block_idx = 40, .block_index = 0 }};
    const theirs2 = [_]finding.OwnerRef{.{ .ino = key.guestLocalIno(7, 5), .bindings = &theirs2_binds }};
    var second: std.ArrayList(Finding) = .empty;
    defer second.deinit(gpa);
    try classes.crossMemberBlockPlane(gpa, &h, local.owners, &theirs2, &c, &second);
    var confirmed: std.ArrayList(Finding) = .empty;
    defer confirmed.deinit(gpa);
    try classes.confirmCrossMember(gpa, first.items, second.items, &c, &confirmed);
    try testing.expectEqual(@as(usize, 1), confirmed.items.len);
    try testing.expect(confirmed.items[0] == .c2_lost);
    try testing.expectEqual(@as(u64, 1), c.suspects_cleared);
}

test "the block plane under a live lessee: a block a lessee's record references is the lessee's (its ring lags this census — the first N-daemon fleet run read every pack B shared as `refcount 64 vs 63`), so C2/C3/C8 scope it out and count it; the member's custody run judges no allocator arm at all (a non-holder's allocator is not the truth)" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    // The lessee's file under this mount's `dir`, sharing block 20 with a
    // record this census cannot see (the refcount says 2, the view says 1).
    const lessee: u16 = 7;
    _ = try set.mkfileGuest(lessee, 5, p.dir, "theirs", VOL_A, &.{20});
    set.setRefcount(VOL_A, 20, 2);
    try set.freezeEra();
    // Unleased (nobody's): the record is judged and the refcount drifts.
    try set.scope.unleased.put(gpa, lessee, {});
    var judged = try ram.run(set, .online, .{});
    defer judged.deinit(gpa);
    try ram.expectClasses(judged, &.{.c3});
    _ = set.scope.unleased.remove(lessee);
    // A LIVE lessee's: scoped, counted, no finding.
    try set.scope.foreign_live.put(gpa, lessee, {});
    var scoped = try ram.run(set, .online, .{});
    defer scoped.deinit(gpa);
    try ram.expectClasses(scoped, &.{});
    try testing.expect(scoped.counters.block_plane_foreign_scoped >= 1);
    _ = set.scope.foreign_live.remove(lessee);
    // The member's custody run: a leak the holder's allocator reports is
    // not this residue's to judge.
    set.leak(VOL_A, 30);
    var whole = try ram.run(set, .online, .{});
    defer whole.deinit(gpa);
    try ram.expectClasses(whole, &.{ .c2, .c3 });
    var h = set.host(.online);
    h.custody_only = true;
    var custody = try ram.runHost(set, &h, .{});
    defer custody.deinit(gpa);
    try ram.expectClasses(custody, &.{});
}

test "C9 and the C10 counts under a live lessee: a judged inode whose only name lives in a directory of a live lessee's slot is NOT unreferenced — the referenced set is incomplete by construction while any slot is foreign-live, so the name-dependent arms take no verdict (counted), and judge again once the slot is nobody's" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    // A directory in slot 1 — this mount's own rotor slot once, a joiner's
    // lease now (its tree, with every name it holds, left this forest) —
    // naming a file whose record sits in slot 0 (the two-daemon row's
    // `d/e`, read as C9 on A).
    const lessee: u16 = 1;
    const theirs_dir = try set.mkdirGuest(lessee, 2, p.dir, "theirs");
    const mine = try set.mkfile(theirs_dir, "mine", VOL_A, &.{21});
    try set.setNlink(mine, 2);
    try set.freezeEra();
    try set.scope.foreign_live.put(gpa, lessee, {});
    var scoped = try ram.run(set, .online, .{});
    defer scoped.deinit(gpa);
    try ram.expectClasses(scoped, &.{});
    try testing.expect(scoped.counters.inode_plane_names_declined >= 1);
    try testing.expect(scoped.counters.inode_plane_foreign_dentry_scoped >= 1);
    // The slot nobody's: the name is in view, the file referenced, and its
    // nlink 2 against one name is the count arm's (C10 nlink too high).
    _ = set.scope.foreign_live.remove(lessee);
    try set.scope.unleased.put(gpa, lessee, {});
    var judged = try ram.run(set, .online, .{});
    defer judged.deinit(gpa);
    try ram.expectClasses(judged, &.{.c10});
    try testing.expectEqual(@as(u64, 0), judged.counters.inode_plane_names_declined);
}

test "C9: a prior-era unreferenced inode with blocks is ONE finding; dry-run plans and mutates nothing; apply destroys it, releases its references and frees its blocks; the next run is clean; a current-era one is never reported" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    try set.orphan(inos.ROOT_INO, "f2");
    const refs_before = set.count(key.TREE_BLOCK_REFS);
    var r = try ram.run(set, .offline, .{ .repair = repairOpts(null, false) });
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{.c9});
    try testing.expectEqual(p.f2, r.findings[0].c9_unreferenced.ino);
    try testing.expectEqual(@as(u32, 2), r.findings[0].c9_unreferenced.blocks);
    try testing.expectEqual(@as(u64, 1), r.repair.?.count(.planned));
    try testing.expectEqual(refs_before, set.count(key.TREE_BLOCK_REFS));
    try testing.expectEqual(@as(u8, 1), r.exitCode());
    var sink: RamSink = .{ .gpa = gpa };
    defer sink.deinit();
    var r2 = try ram.run(set, .offline, .{ .repair = repairOpts(&sink, true) });
    defer r2.deinit(gpa);
    try testing.expectEqual(@as(u64, 1), r2.repair.?.count(.applied));
    try testing.expect(sink.puts.items.len >= 4); // inode + value + xattr + value (+ block keys)
    try testing.expectEqual(refs_before - 2, set.count(key.TREE_BLOCK_REFS));
    try testing.expect((try set.readInode(RamSet.local(p.f2))) == null);
    try testing.expect(set.allocs[1].refcount(5) == null and set.allocs[1].refcount(6) == null);
    var r3 = try ram.run(set, .offline, .{});
    defer r3.deinit(gpa);
    try ram.expectClasses(r3, &.{});
    // The current era: a fresh orphan (minted past the floor) is shielded.
    const fresh = try set.mkfile(inos.ROOT_INO, "fresh", VOL_B, &.{7});
    try set.orphan(inos.ROOT_INO, "fresh");
    var r4 = try ram.run(set, .online, .{});
    defer r4.deinit(gpa);
    try ram.expectClasses(r4, &.{});
    try testing.expect(r4.counters.current_era_exempted >= 1);
    _ = fresh;
}

test "C10: nlink above the names is the leak direction and lowers; below raises; zero with a live name is the loss class; a dangling name is removed under the parent; directories and hard links are never findings" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    // f1 has two names (f1 + f1-link) → nlink 2 is healthy.
    try set.setNlink(p.f1, 5);
    try set.setNlink(p.packed_a, 0);
    const extra = try set.mkfile(p.dir, "extra", VOL_B, &.{8});
    try set.link(p.dir, "extra-2", extra);
    try set.setNlink(extra, 1);
    try set.dangle(p.dir, "ghost", RamSet.global(900));
    try set.freezeEra();
    var r = try ram.run(set, .offline, .{});
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{ .c10, .c10, .c10, .c10 });
    var seen_high = false;
    var seen_low = false;
    var seen_zero = false;
    var seen_dangling = false;
    for (r.findings) |f| switch (f) {
        .c10_nlink_too_high => |x| {
            seen_high = x.ino == p.f1 and x.nlink == 5 and x.names == 2;
            try testing.expectEqual(finding.Severity.info, f.severity());
        },
        .c10_nlink_too_low => |x| seen_low = x.ino == extra and x.names == 2,
        .c10_zero_nlink_named => |x| {
            seen_zero = x.ino == p.packed_a;
            try testing.expectEqual(finding.Severity.loss, f.severity());
        },
        .c10_dangling => |x| seen_dangling = x.child == RamSet.global(900),
        else => unreachable,
    };
    try testing.expect(seen_high and seen_low and seen_zero and seen_dangling);
    var sink: RamSink = .{ .gpa = gpa };
    defer sink.deinit();
    var r2 = try ram.run(set, .offline, .{ .repair = repairOpts(&sink, true) });
    defer r2.deinit(gpa);
    try testing.expectEqual(@as(u64, 4), r2.repair.?.count(.applied));
    try testing.expectEqual(@as(u32, 2), (try set.readInode(RamSet.local(p.f1))).?.nlink);
    try testing.expectEqual(@as(u32, 2), (try set.readInode(RamSet.local(extra))).?.nlink);
    try testing.expectEqual(@as(u32, 1), (try set.readInode(RamSet.local(p.packed_a))).?.nlink);
    var r3 = try ram.run(set, .offline, .{});
    defer r3.deinit(gpa);
    try ram.expectClasses(r3, &.{});
}

test "C2 leaked frees after the ledger check; C2 lost with good content repairs the allocator; C3 recounts; the untouched blocks stay" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    set.leak(VOL_A, 20);
    set.untrack(VOL_B, 6);
    set.setRefcount(VOL_A, 3, 7); // f1 + clone = 2 references
    var r = try ram.run(set, .offline, .{});
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{ .c2, .c2, .c3 });
    try testing.expectEqual(@as(u64, 20), r.findings[0].c2_leaked.block_idx);
    try testing.expectEqual(@as(u64, 6), r.findings[1].c2_lost.block_idx);
    try testing.expect(!r.findings[1].c2_lost.unrepairable_shape);
    try testing.expectEqual(@as(u32, 2), r.findings[2].c3_refcount.expected);
    var sink: RamSink = .{ .gpa = gpa };
    defer sink.deinit();
    var r2 = try ram.run(set, .offline, .{ .repair = repairOpts(&sink, true) });
    defer r2.deinit(gpa);
    if (r2.repair.?.count(.applied) != 3) dumpRepair(r2);
    try testing.expectEqual(@as(u64, 3), r2.repair.?.count(.applied));
    try testing.expect(set.allocs[0].refcount(20) == null);
    try testing.expectEqual(@as(?u32, 1), set.allocs[1].refcount(6));
    try testing.expectEqual(@as(?u32, 2), set.allocs[0].refcount(3));
    var r3 = try ram.run(set, .offline, .{});
    defer r3.deinit(gpa);
    try ram.expectClasses(r3, &.{});
}

test "C2 leaked REFUSES to free a block the durable ledger still names (that block is C8's), and an out-of-range binding is lost-unrepairable → quarantine-mapping" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    // A stray durable record on a leaked block: C2 leaked + C8 drift.
    set.leak(VOL_A, 21);
    try set.strayRef(RamSet.local(p.f1), 9, VOL_A, 21);
    var r = try ram.run(set, .offline, .{ .repair = repairOpts(null, true) });
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{ .c2, .c8 });
    var refused_leak = false;
    for (r.repair.?.rows) |row| if (row.finding == .c2_leaked) {
        refused_leak = row.outcome == .refused and std.mem.indexOf(u8, row.detail, "C8") != null;
    };
    try testing.expect(refused_leak);
    try testing.expectEqual(@as(?u32, 1), set.allocs[0].refcount(21));
}

test "C8: a dropped reference record is the fabricate direction (report-only refusal); a stray record is released offline and refused online; the confirmed row feeds the drift tripwire" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    try set.dropRef(p.f2, 1, VOL_B, 6);
    var r = try ram.run(set, .offline, .{ .repair = repairOpts(null, true) });
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{.c8});
    try testing.expectEqual(@as(u32, 0), r.findings[0].c8_drift.durable);
    try testing.expectEqual(@as(u32, 1), r.findings[0].c8_drift.derived);
    try testing.expectEqual(finding.Outcome.refused, r.repair.?.rows[0].outcome);
    try testing.expect(std.mem.indexOf(u8, r.repair.?.rows[0].detail, "never fabricated") != null);
    // Restore the record by hand, then plant a stray one.
    try set.strayRef(RamSet.local(p.f2), 1, VOL_B, 6);
    try set.strayRef(RamSet.local(p.f2), 7, VOL_B, 30);
    var online = try ram.run(set, .online, .{ .repair = repairOpts(null, true) });
    defer online.deinit(gpa);
    try ram.expectClasses(online, &.{.c8});
    try testing.expectEqual(finding.Outcome.refused, online.repair.?.rows[0].outcome);
    var offline = try ram.run(set, .offline, .{ .repair = repairOpts(null, true) });
    defer offline.deinit(gpa);
    try testing.expectEqual(finding.Outcome.applied, offline.repair.?.rows[0].outcome);
    var clean = try ram.run(set, .offline, .{});
    defer clean.deinit(gpa);
    try ram.expectClasses(clean, &.{});
    try testing.expect(block_refs.stats.fsck_repair_class_c8.load() >= 1);
}

test "C1: a torn page is quarantine-report-only and persists; a record failing its schema is rebuilt in place (deleted after quarantine); a malformed known-kind key passes the deletion gate" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    const victim = try set.mkfile(inos.ROOT_INO, "victim", VOL_B, &.{9});
    try set.freezeEra();
    try set.tearInode(victim);
    try set.rawKey();
    var r = try ram.run(set, .offline, .{ .repair = repairOpts(null, true) });
    defer r.deinit(gpa);
    var semantic: u32 = 0;
    var raw: u32 = 0;
    for (r.findings) |f| switch (f) {
        .c1_semantic => semantic += 1,
        .c1_raw_key => |x| {
            raw += 1;
            try testing.expectEqual(finding.RawKeyDefect.malformed_known_kind, x.defect);
        },
        else => {},
    };
    try testing.expectEqual(@as(u32, 1), semantic);
    try testing.expectEqual(@as(u32, 1), raw);
    for (r.repair.?.rows) |row| if (row.finding.class() == .c1) try testing.expectEqual(finding.Outcome.applied, row.outcome);
    try testing.expect((try set.mem.lookup(gpa, key.TREE_INODES, &key.inodeKey(RamSet.local(victim)))) == null);
    // The torn walk: the dentry kind fails from `dir`'s first key.
    set.tearAt(p.dir);
    var torn = try ram.run(set, .offline, .{ .repair = repairOpts(null, true) });
    defer torn.deinit(gpa);
    var saw_torn = false;
    for (torn.findings) |f| if (f == .c1_torn) {
        saw_torn = true;
    };
    try testing.expect(saw_torn);
    for (torn.repair.?.rows) |row| if (row.finding == .c1_torn) try testing.expectEqual(finding.Verb.quarantine_report_only, row.verb);
    set.fault.tear_at = null;
}

test "C7: a corrupt plain block is readability-only (the honesty gauge says so); a corrupt frame on a transformed volume is a finding, its repair flips the mapping to DAMAGED and the next scrub skips it" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    set.corrupt(VOL_A, 1);
    var r = try ram.run(set, .offline, .{ .scrub = true });
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{});
    try testing.expect(r.counters.scrub_readability_only >= 7 and r.counters.scrub_frame_verified == 0);
    // Volume B becomes an lz4 volume: block 5 holds a valid frame, block 6 a torn one.
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const cfg: transform.VolumeTransform = .{ .compression = .lz4, .block_size = @intCast(CHUNK) };
    const xf = try transform.Transform.init(&env, cfg, null);
    var t = try transform.Codec.init(gpa, &xf);
    defer t.deinit(gpa);
    const scratch = try gpa.alloc(u8, @intCast(transform.scratchLen(@intCast(CHUNK))));
    defer gpa.free(scratch);
    var tb: scrub.TransformBinding = .{ .codec = &t, .scratch = scratch };
    set.xforms[1] = .{ .vol_tag = VOL_B, .cfg = cfg, .face = tb.face() };
    const plain = try gpa.alloc(u8, 3000);
    defer gpa.free(plain);
    @memset(plain, 'z');
    const w = try gpa.alloc(u8, @intCast(transform.scratchLen(@intCast(CHUNK))));
    defer gpa.free(w);
    const frame = try t.processWrite(plain, w);
    @memcpy(set.devs[1][@intCast(5 * CHUNK)..][0..frame.len], frame);
    @memcpy(set.devs[1][@intCast(6 * CHUNK)..][0..frame.len], frame);
    set.devs[1][@intCast(6 * CHUNK + 8)] ^= 0xFF;
    var r2 = try ram.run(set, .offline, .{ .scrub = true, .repair = repairOpts(null, true) });
    defer r2.deinit(gpa);
    try ram.expectClasses(r2, &.{.c7});
    try testing.expectEqual(RamSet.local(p.f2), r2.findings[0].c7_scrub.ino);
    try testing.expectEqual(@as(u32, 1), r2.findings[0].c7_scrub.block_index);
    try testing.expect(r2.counters.scrub_frame_verified >= 1);
    try testing.expectEqual(finding.Outcome.applied, r2.repair.?.rows[0].outcome);
    var h = set.host(.offline);
    const rec = (try h.readLayout(gpa, RamSet.local(p.f2))).?;
    defer gpa.free(rec.value);
    try testing.expect(rec.layout.entry(CHUNK, 1).mapping.damaged);
    var r3 = try ram.run(set, .offline, .{ .scrub = true });
    defer r3.deinit(gpa);
    try ram.expectClasses(r3, &.{});
}

test "C11 and C12 are report-only: orphan map records, an empty kvmap head, two tenants intersecting at different offsets, an off-grain window; identical windows (a clone) and the same-off prefix are legal" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    // A legal clone of the packed tenant: the identical window.
    const pa = (try set.readInode(RamSet.local(p.packed_a))).?;
    _ = pa;
    _ = try set.mkpacked(p.dir, "pa-clone", VOL_A, 10, 0, 1000);
    _ = try set.mkpacked(p.dir, "pa-clip", VOL_A, 10, 0, 500);
    var ok = try ram.run(set, .offline, .{});
    defer ok.deinit(gpa);
    try ram.expectClasses(ok, &.{});
    // The overlap: a window starting inside pa's reach at a different off.
    _ = try set.mkpacked(p.dir, "overlap", VOL_A, 10, 4096 + 4096, 100);
    _ = try set.mkpacked(p.dir, "overlap2", VOL_A, 11, 0, 4096 + 1);
    _ = try set.mkpacked(p.dir, "overlap3", VOL_A, 11, 4096, 100);
    _ = try set.overrunTenant(p.dir, "overrun", VOL_A, 12);
    try set.orphanMapRecords(RamSet.local(p.f1), 3);
    _ = try set.emptyKvmapHead(p.dir, "empty-head");
    try set.freezeEra();
    var r = try ram.run(set, .offline, .{ .repair = repairOpts(null, true) });
    defer r.deinit(gpa);
    var overlaps: u32 = 0;
    var overruns: u32 = 0;
    var orphan_maps: u32 = 0;
    var empty_heads: u32 = 0;
    for (r.findings) |f| switch (f) {
        .c12_overlap => |x| {
            overlaps += 1;
            try testing.expect(x.block_idx == 11 or x.block_idx == 10);
        },
        .c12_overrun => |x| {
            overruns += 1;
            try testing.expectEqual(@as(u64, 12), x.block_idx);
        },
        .c11_orphan_map_records => |x| {
            orphan_maps += 1;
            try testing.expectEqual(@as(u64, 3), x.records);
        },
        .c11_empty_kvmap_head => empty_heads += 1,
        else => {
            std.debug.print("unexpected: {f}\n", .{f});
            return error.TestUnexpectedResult;
        },
    };
    try testing.expectEqual(@as(u32, 1), overlaps);
    try testing.expectEqual(@as(u32, 1), overruns);
    try testing.expectEqual(@as(u32, 1), orphan_maps);
    try testing.expectEqual(@as(u32, 1), empty_heads);
    for (r.repair.?.rows) |row| try testing.expectEqual(finding.Outcome.refused, row.outcome);
    try testing.expectEqual(@as(u8, 1), r.exitCode());
}

test "C6: the bitmap oracle — a referenced block reading CLEAR is the LOSS finding; a set-but-unreferenced bit is a counted leak, not a finding" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    const bm = try data_bitmap.DataBitmap.init(gpa, set.allocs[0], .{ .vol_tag = VOL_A, .term = 1, .blocks = ram.BLOCKS, .ring_capacity = 64 });
    defer bm.deinit();
    set.vols[0].bitmap = bm;
    defer set.vols[0].bitmap = null;
    var clean = try ram.run(set, .offline, .{});
    defer clean.deinit(gpa);
    try ram.expectClasses(clean, &.{});
    _ = bm.setRun(33, 1); // a leak: set, unreferenced
    var leak = try ram.run(set, .offline, .{});
    defer leak.deinit(gpa);
    try ram.expectClasses(leak, &.{});
    _ = bm.clearRun(2, 1); // f1's second block reads clear: LOSS
    var loss = try ram.run(set, .offline, .{});
    defer loss.deinit(gpa);
    try ram.expectClasses(loss, &.{.c6});
    try testing.expectEqual(@as(u64, 1), loss.findings[0].c6_drift.loss);
    try testing.expectEqual(finding.Severity.loss, loss.findings[0].severity());
}

test "C14/C15 over ONE forest census: a slot Live on two pages and a Live entry at tree 0's g under another lessee are one pair finding each; a dead appender's page with window entries is C15; a Recovering page the ledger does not name is C15; the mount refuses on C14" {
    const gpa = testing.allocator;
    const e = struct {
        fn entry(slot: u16, g: u32) appender_page.SlotEntry {
            return .{ .slot = slot, .state = .live, .g = g, .slot_tree_extents = 0, .root_addr = 0, .root_seq = 0, .cursor = 2 };
        }
    };
    const pages = [_]classes.PageView{
        .{ .vol = 0, .appender_id = 1, .state = .live, .node_token = 0xA, .mount_slot = 1, .is_manager = false, .window_entries = 0, .entries = &.{ e.entry(7, 3), e.entry(8, 1) } },
        .{ .vol = 0, .appender_id = 2, .state = .live, .node_token = 0xB, .mount_slot = 1, .is_manager = false, .window_entries = 0, .entries = &.{e.entry(7, 3)} },
        .{ .vol = 0, .appender_id = 3, .state = .live, .node_token = 0xC, .mount_slot = 1, .is_manager = false, .window_entries = 12, .entries = &.{} },
        .{ .vol = 0, .appender_id = 4, .state = .recovering, .node_token = 0xD, .mount_slot = 1, .is_manager = false, .window_entries = 0, .entries = &.{} },
        .{ .vol = 0, .appender_id = 5, .state = .recovered, .node_token = 0xE, .mount_slot = 1, .is_manager = false, .window_entries = 99, .entries = &.{} },
    };
    const tree0 = [_]classes.Tree0Row{
        .{ .vol = 0, .slot = 7, .state = .{ .variant = .leased, .appender_id = 1, .g = 3, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 } },
        .{ .vol = 0, .slot = 8, .state = .{ .variant = .leased, .appender_id = 9, .g = 1, .root = .{ .addr = 0, .seq = 0 }, .cursor = 2 } },
    };
    const deaths = [_]cr.StableIdentity{ .{ .node_token = 0xC, .mount_slot = 1 }, .{ .node_token = 0xE, .mount_slot = 1 } };
    var out: std.ArrayList(Finding) = .empty;
    defer out.deinit(gpa);
    try classes.custodyFindings(gpa, .{ .pages = &pages, .tree0 = &tree0, .deaths = &deaths }, &out);
    const n = finding.Report.normalize(out.items);
    var c14: u32 = 0;
    var c15: u32 = 0;
    for (out.items[0..n]) |f| switch (f) {
        .c14_slot_custody_conflict => |x| {
            c14 += 1;
            try testing.expect((x.slot == 7 and x.appender_a == 1 and x.appender_b == 2) or (x.slot == 8 and x.appender_a == 1 and x.appender_b == 9));
            try testing.expectEqual(finding.Verb.report_only, f.verb());
        },
        .c15_unrecovered_appender => |x| {
            c15 += 1;
            try testing.expect(x.appender == 3 or x.appender == 4);
            try testing.expectEqual(finding.Verb.recover_dead_appender, f.verb());
        },
        else => unreachable,
    };
    try testing.expectEqual(@as(u32, 2), c14);
    try testing.expectEqual(@as(u32, 2), c15);
    // The mount path's refusal is the same oracle: a Live entry at tree
    // 0's g under another lessee is `.conflict`.
    const lt = @import("../../meta/sym/slots/lease_table.zig");
    try testing.expectEqual(lt.Settle.conflict, lt.settleEntry(tree0[1].state, e.entry(8, 1), 1));
}

test "C14 reads every grant record whole: a record whose chunks do not assemble is one report-only row naming its appender (decision 0003 §AE.4); one that assembles is none" {
    const gpa = testing.allocator;
    var out: std.ArrayList(Finding) = .empty;
    defer out.deinit(gpa);
    const grants = [_]classes.GrantView{ .{ .vol = 0, .appender = 5, .assembles = false }, .{ .vol = 0, .appender = 6, .assembles = true }, .{ .vol = 1, .appender = 5, .assembles = true } };
    try classes.custodyFindings(gpa, .{ .pages = &.{}, .tree0 = &.{}, .deaths = &.{}, .grants = &grants }, &out);
    try testing.expectEqual(@as(usize, 1), out.items.len);
    const f = out.items[0];
    try testing.expectEqual(@as(u32, 5), f.c14_grant_record_corrupt.appender);
    try testing.expectEqual(@as(u16, 0), f.c14_grant_record_corrupt.vol);
    try testing.expectEqual(finding.Class.c14, f.class());
    try testing.expectEqual(finding.Verb.report_only, f.verb());
}

test "C13 / C16 / C17 through their faces: an orphan image is planned to return and healed on the second pass; shared-index drift is report-only and verified twice; a stripe map with a misrouted name is one C17 row" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    const Forest = struct {
        orphans: u32 = 1,
        fn custody(_: *anyopaque, _: Allocator) census.Error!classes.ForestCensus {
            return .{ .pages = &.{}, .tree0 = &.{}, .deaths = &.{} };
        }
        fn orphanImages(ctx: *anyopaque, g: Allocator, out: *std.ArrayList(Finding)) census.Error!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            if (self.orphans != 0) try out.append(g, .{ .c13_orphan_image = .{ .vol = 0, .appender = 2, .extent = 77 } });
        }
        fn returnOrphan(ctx: *anyopaque, _: u16, _: u32, _: u64) census.Error!bool {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            if (self.orphans == 0) return false;
            self.orphans = 0;
            return true;
        }
        fn recoverDead(_: *anyopaque, _: u16, _: u32) census.Error!bool {
            return false;
        }
        const vtable: classes.ForestFace.VTable = .{ .custody = &custody, .orphanImages = &orphanImages, .returnOrphan = &returnOrphan, .recoverDead = &recoverDead };
    };
    var forest: Forest = .{};
    const Shared = struct {
        fn lists(_: *anyopaque, g: Allocator, vol_tag: u64, flagged: *std.ArrayList([]const u8), index: *std.ArrayList([]const u8)) census.Error!void {
            if (vol_tag != VOL_A) return;
            try flagged.append(g, try g.dupe(u8, &key.blockRefKey(VOL_A, 3, 5, 0)));
            var k = key.blockRefKey(VOL_A, 3, 5, 0);
            k[0] = key.TREE_SHARED_INDEX;
            try index.append(g, try g.dupe(u8, &k));
            var k2 = key.blockRefKey(VOL_A, 4, 6, 0);
            k2[0] = key.TREE_SHARED_INDEX;
            try index.append(g, try g.dupe(u8, &k2));
        }
    };
    var shared_ctx: u8 = 0;
    // C17: a striped directory whose stripe 1 holds a name routed to stripe 0.
    const sd = try set.mkdir(inos.ROOT_INO, "striped");
    const s0 = try set.mkdir(inos.ROOT_INO, "s0");
    const s1 = try set.mkdir(inos.ROOT_INO, "s1");
    try set.dangle(sd, "\x00sqz.stripe:00", s0);
    try set.dangle(sd, "\x00sqz.stripe:01", s1);
    try set.dangle(sd, "\x00sqz.striped", s0);
    // Names in both stripes: whichever routes wrong is the misrouted one.
    var misrouted: u32 = 0;
    const seed = set.mem.nameKey();
    const stripe_mod = @import("../../meta/sym/xv/stripe.zig");
    inline for (.{ "alpha", "bravo", "charlie", "delta" }, 0..) |nm, i| {
        const target: u8 = stripe_mod.stripeOf(key.dentryNameHash54(nm, seed), 2);
        // Put every name in stripe 0: those routing to 1 are misrouted.
        _ = try set.mkfile(s0, nm, VOL_B, &.{25 + i});
        if (target != 0) misrouted += 1;
    }
    try set.freezeEra();
    var h = set.host(.online);
    h.forest = .{ .ctx = &forest, .vtable = &Forest.vtable };
    h.shared = .{ .ctx = &shared_ctx, .lists = &Shared.lists };
    var r = try ram.runHost(set, &h, .{ .repair = repair.Options{ .apply = true } });
    defer r.deinit(gpa);
    var c13: u32 = 0;
    var c16: u32 = 0;
    var c17: u32 = 0;
    for (r.findings) |f| switch (f) {
        .c13_orphan_image => c13 += 1,
        .c16_shared_index_drift => |x| {
            c16 += 1;
            try testing.expectEqual(@as(u64, 0), x.flag_without_entry);
            try testing.expectEqual(@as(u64, 1), x.entry_without_flag);
        },
        .c17_stripe => |x| {
            c17 += 1;
            try testing.expectEqual(stripe_mod.C17Shape.misrouted_name, x.shape);
        },
        else => {
            std.debug.print("unexpected: {f}\n", .{f});
            return error.TestUnexpectedResult;
        },
    };
    try testing.expectEqual(@as(u32, 1), c13);
    try testing.expectEqual(@as(u32, 1), c16);
    try testing.expectEqual(misrouted, c17);
    for (r.repair.?.rows) |row| switch (row.finding) {
        .c13_orphan_image => try testing.expectEqual(finding.Outcome.applied, row.outcome),
        else => try testing.expectEqual(finding.Outcome.refused, row.outcome),
    };
    _ = p;
}

test "C4 through the staging face: an orphan custody record is quarantined then retired; a joiner's and a current-era custody are scoped out" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    const staging = @import("../../data/staging.zig");
    const Face = struct {
        gpa: Allocator,
        residents: std.ArrayList(staging.Census),
        fn residentsFn(ctx: *anyopaque, g: Allocator) census.Error![]staging.Census {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            return g.dupe(staging.Census, self.residents.items);
        }
        fn image(ctx: *anyopaque, g: Allocator, k: staging.StagingKey) census.Error!?[]u8 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            for (self.residents.items) |r| if (std.mem.eql(u8, std.mem.asBytes(&r.key), std.mem.asBytes(&k))) return try g.dupe(u8, "custody-image");
            return null;
        }
        fn remove(ctx: *anyopaque, k: staging.StagingKey, _: u64) bool {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            for (self.residents.items, 0..) |r, i| if (std.mem.eql(u8, std.mem.asBytes(&r.key), std.mem.asBytes(&k))) {
                _ = self.residents.swapRemove(i);
                return true;
            };
            return false;
        }
        const vtable: classes.StagingFace.VTable = .{ .residents = &residentsFn, .image = &image, .remove = &remove };
    };
    var face: Face = .{ .gpa = gpa, .residents = .empty };
    defer face.residents.deinit(gpa);
    // A live file's custody (fine) and a custody naming a reclaimed ino —
    // prior-era (the start watermark sits above it), so it IS judged.
    const floor: u64 = 4001;
    try set.scope.era_floor.put(gpa, 0, floor);
    try face.residents.append(gpa, .{ .key = staging.StagingKey.init(@splat(1), RamSet.local(p.f1)), .cost = 4096, .generation = 1 });
    try face.residents.append(gpa, .{ .key = staging.StagingKey.init(@splat(2), 4000), .cost = 4096, .generation = 2 });
    // Two custodies this census cannot judge: a joiner's ino (its slot is a
    // LIVE foreign lessee's — the first online run of a two-daemon set read
    // every file B was creating as a C4 orphan) and a native ino minted past
    // the start watermark (the current era: its record may not be written
    // yet). Both are scoped out and counted, never findings.
    const joiner_slot: u16 = 3;
    try set.scope.foreign_live.put(gpa, joiner_slot, {});
    try face.residents.append(gpa, .{ .key = staging.StagingKey.init(@splat(3), key.guestLocalIno(joiner_slot, 77)), .cost = 4096, .generation = 3 });
    try face.residents.append(gpa, .{ .key = staging.StagingKey.init(@splat(4), floor + 5), .cost = 4096, .generation = 4 });
    var h = set.host(.online);
    h.staging = .{ .ctx = &face, .vtable = &Face.vtable };
    var sink: RamSink = .{ .gpa = gpa };
    defer sink.deinit();
    var r = try ram.runHost(set, &h, .{ .repair = repairOpts(&sink, true) });
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{.c4});
    try testing.expectEqual(@as(u64, 4000), r.findings[0].c4_orphan.ino);
    try testing.expectEqual(@as(u64, 2), r.counters.c4_custody_scoped);
    try testing.expectEqual(finding.Outcome.applied, r.repair.?.rows[0].outcome);
    try testing.expectEqual(@as(usize, 3), face.residents.items.len);
    try testing.expectEqual(@as(usize, 1), sink.puts.items.len);
    _ = set.scope.foreign_live.remove(joiner_slot);
}

test "the kill-9 convergence seam: an apply aborted after the quarantine mutates nothing; the next run re-detects; a second apply plans zero (idempotent)" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    try set.orphan(inos.ROOT_INO, "f2");
    const Hook = struct {
        fn abort(_: Finding) bool {
            return true;
        }
    };
    var sink: RamSink = .{ .gpa = gpa };
    defer sink.deinit();
    var r = try ram.run(set, .offline, .{ .repair = repair.Options{ .apply = true, .quarantine = sink.sink(), .abort_hook = &Hook.abort } });
    defer r.deinit(gpa);
    try testing.expectEqual(@as(u64, 1), r.repair.?.count(.refused));
    try testing.expect(sink.puts.items.len >= 2);
    try testing.expect((try set.readInode(RamSet.local(p.f2))) != null);
    var r2 = try ram.run(set, .offline, .{ .repair = repairOpts(null, true) });
    defer r2.deinit(gpa);
    try testing.expectEqual(@as(u64, 1), r2.repair.?.count(.applied));
    var r3 = try ram.run(set, .offline, .{ .repair = repairOpts(null, true) });
    defer r3.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), r3.repair.?.rows.len);
}

test "sharding k/N: the residue shards' merge is the unsharded report for the inode plane; a shard judges only the lost arm of the block plane" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    try set.orphan(inos.ROOT_INO, "f2");
    try set.setNlink(p.f1, 9);
    try set.dangle(p.dir, "ghost", RamSet.global(777));
    set.leak(VOL_A, 30);
    try set.freezeEra();
    var whole = try ram.run(set, .offline, .{});
    defer whole.deinit(gpa);
    try ram.expectClasses(whole, &.{ .c2, .c9, .c10, .c10 });
    var shards: [3]finding.Report = undefined;
    var made: usize = 0;
    defer for (shards[0..made]) |*s| s.deinit(gpa);
    for (&shards, 0..) |*s, k| {
        var h = set.host(.offline);
        h.shard = .{ .k = @intCast(k), .n = 3 };
        s.* = try ram.runHost(set, &h, .{});
        made += 1;
        try testing.expectEqual(finding.Shard{ .k = @intCast(k), .n = 3 }, s.shard.?);
        for (s.findings) |f| try testing.expect(f.class() != .c2);
    }
    var merged = try finding.merge(gpa, &shards);
    defer merged.deinit(gpa);
    try ram.expectClasses(merged, &.{ .c9, .c10, .c10 });
    try testing.expect(merged.shard == null);
    try testing.expectEqual(whole.counters.inodes_scanned, merged.counters.inodes_scanned);
    for (merged.findings, whole.findings[1..]) |a, b| try testing.expect(a.eql(b));
}

/// A writer storm against the RAM set under the 4a protocol: create (the
/// record, then the dentry in a second commit — the create window), a
/// block minted under the in-flight registry and published, then unlink
/// (the dentry out + nlink 0 in ONE tx), then the destroy (the record, the
/// layout, the references, the block's terminal free).
const Storm = struct {
    set: *RamSet,
    stop: std.atomic.Value(bool) = .init(false),
    ops: std.atomic.Value(u64) = .init(0),
    err: ?anyerror = null,

    fn run(self: *Storm, lane: u64) void {
        self.body(lane) catch |e| {
            self.err = e;
        };
    }

    fn body(self: *Storm, lane: u64) !void {
        const set = self.set;
        const gpa = set.gpa;
        const io = testing.io;
        var i: u64 = 0;
        while (!self.stop.load(.acquire)) : (i += 1) {
            var name_buf: [32]u8 = undefined;
            const name = try std.fmt.bufPrint(&name_buf, "s{d}-{d}", .{ lane, i });
            // Mint under the lane's own locks (the fixture's cursor is a
            // shared word: a CAS mint).
            const l = set.mintAtomic();
            const g = RamSet.global(l);
            const a = set.allocs[0];
            // The canonical pair acquisition (deduped by stripe: two inos on
            // one stripe self-deadlock as two acquisitions).
            var guards = try set.locks.lockMany(io, &.{ .{ .ino = inos.ROOT_INO, .mode = .exclusive }, .{ .ino = l, .mode = .exclusive } }, &.{});
            // 1. The record first.
            {
                const t = try set.mem.begin(@src());
                {
                    errdefer set.mem.abandon(t);
                    try t.stagePut(key.TREE_INODES, &key.inodeKey(l), &(record.InodeValue{ .mode = economy.S.IFREG | 0o644, .nlink = 1, .ctime = i }).encode());
                }
                try set.mem.commit(t);
            }
            // 2. The name.
            {
                const t = try set.mem.begin(@src());
                {
                    errdefer set.mem.abandon(t);
                    const dv: record.DentryValue = .{ .child_ino = g, .file_type = economy.ftByte(economy.S.IFREG), .name = name };
                    var buf: [64]u8 = undefined;
                    try t.stagePut(key.TREE_DENTRIES, &key.dentryKey(inos.ROOT_INO, key.dentryNameHash54(name, set.mem.nameKey()), 0), try dv.encodeInto(&buf));
                }
                try set.mem.commit(t);
            }
            // 3. A block: minted, in flight, published with its reference.
            const minted = try a.allocateBlock();
            var inflight = block_alloc.InflightAllocGuard.register(a, minted.idx);
            {
                const t = try set.mem.begin(@src());
                {
                    errdefer set.mem.abandon(t);
                    const m = layout.Mapping.wholeBlock(VOL_A, minted.offset, 0);
                    const entries = [_]layout.MapEntryWire{(layout.MapEntry{ .block = 0, .mapping = m }).encode()};
                    try layout.publishLayout(t, .{ .ino = l, .name_key = set.mem.nameKey(), .prev = null, .layout = layout.Layout.stripedInline(CHUNK, &entries), .proofs = &.{.{ .vol_tag = VOL_A, .offset = minted.offset, .len = CHUNK, .completed_ns = 1 }} }, classes.refs_seam, set.geo);
                }
                try set.mem.commit(t);
            }
            a.publishBlock(minted.idx);
            inflight.deinit();
            guards.release(io);
            _ = self.ops.fetchAdd(1, .monotonic);
            if (self.stop.load(.acquire)) break;
            // 4. The unlink: the dentry out + nlink 0 in ONE tx.
            var guards2 = try set.locks.lockMany(io, &.{ .{ .ino = inos.ROOT_INO, .mode = .exclusive }, .{ .ino = l, .mode = .exclusive } }, &.{});
            {
                const t = try set.mem.begin(@src());
                {
                    errdefer set.mem.abandon(t);
                    try t.stageDelete(key.TREE_DENTRIES, &key.dentryKey(inos.ROOT_INO, key.dentryNameHash54(name, set.mem.nameKey()), 0));
                    try t.stagePut(key.TREE_INODES, &key.inodeKey(l), &(record.InodeValue{ .mode = economy.S.IFREG | 0o644, .nlink = 0, .ctime = i + 1 }).encode());
                }
                try set.mem.commit(t);
            }
            // 5. The destroy: the record + the layout + the reference, then
            // the block's terminal free — the release held in flight from
            // before its commit until after the free (decision 0003 §AH.1).
            var release = block_alloc.InflightAllocGuard.register(a, minted.idx);
            defer release.deinit();
            {
                const t = try set.mem.begin(@src());
                {
                    errdefer set.mem.abandon(t);
                    try block_refs.stageOps(t, &.{block_refs.released(.{ .vol_tag = VOL_A, .block_idx = minted.idx, .owner_ino = l, .block_index = 0 })});
                    const xk = key.xattrKey(l, key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, set.mem.nameKey()), 0);
                    try inos.stageDestroy(t, l, &.{&xk});
                }
                try set.mem.commit(t);
            }
            _ = try a.freeBlock(minted.idx);
            guards2.release(io);
            _ = self.ops.fetchAdd(1, .monotonic);
            _ = gpa;
        }
    }
};

test "zero false positives: a live writer storm (creates, publishes, unlinks, destroys) during ten online runs that each overlap its progress ⇒ 0 findings; the suspects clear through the era floor, the epoch map, the in-flight registry and the fresh re-reads" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    // The storm mints past the floor: the era shield's object.
    var storm: Storm = .{ .set = set };
    const t1 = try std.Thread.spawn(.{}, Storm.run, .{ &storm, 1 });
    const t2 = try std.Thread.spawn(.{}, Storm.run, .{ &storm, 2 });
    // A run counts when the storm completed an op during it (a run that
    // met no progress — the storm's threads not yet scheduled on a loaded
    // host — judged a quiet set and proves nothing), and the runs go on
    // until the shields engaged: ten overlapping runs, then the first
    // exemption or clear (the storm's in-flight windows are the shields'
    // object).
    var total_suspects: u64 = 0;
    var overlapping: u32 = 0;
    while (overlapping < 10 or total_suspects == 0) {
        const ops0 = storm.ops.load(.acquire);
        var r = try ram.run(set, .online, .{ .scrub = true });
        defer r.deinit(gpa);
        if (r.findings.len != 0) {
            for (r.findings) |f| std.debug.print("FALSE POSITIVE: {f}\n", .{f});
            storm.stop.store(true, .release);
            t1.join();
            t2.join();
            return error.FalsePositive;
        }
        if (storm.ops.load(.acquire) != ops0) overlapping += 1;
        total_suspects += r.counters.suspects + r.counters.current_era_exempted + r.counters.epoch_exempted + r.counters.inflight_exempted + r.counters.nlink_transient_cleared;
    }
    storm.stop.store(true, .release);
    t1.join();
    t2.join();
    try testing.expect(storm.err == null);
    // The storm's residue (a half-created lane at stop) is current-era:
    // still no verdict, and a later era sees it as the next mount's.
    var r = try ram.run(set, .online, .{});
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{});
}

/// The C8 race the macOS gate's storm met (`[C8] durable refs 0 vs derived
/// 1`), scheduled exactly through the pass's C8 hook: one block, its owner
/// destroyed between the first round's two walks (the suspect), the block
/// re-minted to a second owner as the re-check starts, and that owner
/// destroyed between the re-check's durable and derived reads — try-only,
/// as a writer meeting the owner's lease.
const C8Racer = struct {
    set: *RamSet,
    blk: u64,
    first: u64,
    second: u64 = 0,
    second_destroyed: bool = false,
    /// The re-check held the second owner's lease: its destroy waited.
    destroy_waited: bool = false,
    err: ?anyerror = null,

    fn hook(self: *C8Racer) classes.Host.RecheckHook {
        return .{ .ctx = self, .at = &at };
    }

    fn at(ctx: *anyopaque, point: classes.Host.RecheckPoint, block_idx: u64) void {
        const self: *C8Racer = @ptrCast(@alignCast(ctx));
        self.step(point, block_idx) catch |e| {
            self.err = e;
        };
    }

    fn step(self: *C8Racer, point: classes.Host.RecheckPoint, block_idx: u64) !void {
        switch (point) {
            .c8_round1_refs => _ = try self.destroy(self.first, "racer", true),
            .c8_recheck_start => if (block_idx == self.blk and self.second == 0) {
                self.second = try self.set.mkfile(inos.ROOT_INO, "racer2", VOL_A, &.{self.blk});
            },
            .c8_recheck_between_reads => if (block_idx == self.blk and self.second != 0 and !self.second_destroyed) {
                if (try self.destroy(self.second, "racer2", false)) self.second_destroyed = true else self.destroy_waited = true;
            },
            .phase_a_registry, .count_recheck_derived => {},
        }
    }

    /// The unlink + destroy of a one-block file in ONE tx under its
    /// exclusive lease (blocking, or try-only), then the block's terminal
    /// free. False = the lease was held.
    fn destroy(self: *C8Racer, global: u64, name: []const u8, blocking: bool) !bool {
        const set = self.set;
        const io = testing.io;
        const l = RamSet.local(global);
        const req = [_]@import("../../meta/dlm.zig").InodeReq{.{ .ino = l, .mode = .exclusive }};
        var guards = if (blocking) try set.locks.lockMany(io, &req, &.{}) else (try set.locks.tryLockMany(io, &req, &.{})) orelse return false;
        defer guards.release(io);
        const t = try set.mem.begin(@src());
        {
            errdefer set.mem.abandon(t);
            try t.stageDelete(key.TREE_DENTRIES, &key.dentryKey(inos.ROOT_INO, key.dentryNameHash54(name, set.mem.nameKey()), 0));
            try block_refs.stageOps(t, &.{block_refs.released(.{ .vol_tag = VOL_A, .block_idx = self.blk, .owner_ino = l, .block_index = 0 })});
            const xk = key.xattrKey(l, key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, set.mem.nameKey()), 0);
            try inos.stageDestroy(t, l, &.{&xk});
        }
        try set.mem.commit(t);
        set.untrack(VOL_A, self.blk);
        return true;
    }
};

test "C8's online re-check reads a block's durable references and its owners' layouts in ONE state (the macOS gate's `[C8] durable refs 0 vs derived 1` under the live writer storm): the owner destroyed between the first round's two walks makes the suspect, the block re-minted to a second owner as the re-check starts and destroyed between the re-check's reads confirmed it on the whole-view re-check (a fresh census, then a refs scan) — the per-block re-check holds the owners' leases, the destroy waits, the suspect clears" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    const blk: u64 = 40;
    var racer: C8Racer = .{ .set = set, .blk = blk, .first = try set.mkfile(inos.ROOT_INO, "racer", VOL_A, &.{blk}) };
    var h = set.host(.online);
    h.recheck_hook = racer.hook();
    var r = try ram.runHost(set, &h, .{});
    defer r.deinit(gpa);
    try testing.expect(racer.err == null);
    for (r.findings) |f| std.debug.print("FALSE POSITIVE: {f}\n", .{f});
    try ram.expectClasses(r, &.{});
    // The schedule ran: a suspect, the re-mint, the destroy held off.
    try testing.expect(r.counters.suspects >= 1 and r.counters.suspects_cleared >= 1);
    try testing.expect(racer.second != 0 and racer.destroy_waited and !racer.second_destroyed);
    // The held-off destroy lands after the run; the next run is clean.
    try testing.expect(try racer.destroy(racer.second, "racer2", true));
    var again = try ram.run(set, .online, .{});
    defer again.deinit(gpa);
    try ram.expectClasses(again, &.{});
}

test "Phase A: a block minted under the scan with a live in-flight owner is epoch/registry-exempt online and never a C2 finding; dropping the guard with no publish is a genuine leak the next OFFLINE run reports" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    const a = set.allocs[0];
    // Begin a scan by hand so the mint stamps the epoch, as a live run's would.
    try a.fsckBeginScan();
    const minted = try a.allocateBlock();
    var guard = block_alloc.InflightAllocGuard.register(a, minted.idx);
    a.fsckEndScan();
    var online = try ram.run(set, .online, .{});
    defer online.deinit(gpa);
    try ram.expectClasses(online, &.{});
    try testing.expect(online.counters.epoch_exempted + online.counters.inflight_exempted >= 1);
    guard.deinit();
    var offline = try ram.run(set, .offline, .{});
    defer offline.deinit(gpa);
    try ram.expectClasses(offline, &.{.c2});
    try testing.expectEqual(minted.idx, offline.findings[0].c2_leaked.block_idx);
}

/// The unlink + destroy of a one-block file in ONE tx under its exclusive
/// lease — the dentry, the record, the layout and the block's reference —
/// with the block's terminal free left to the caller (the window between
/// a releasing commit and its free).
fn commitDestroy(set: *RamSet, global: u64, name: []const u8, blk: u64) !void {
    const io = testing.io;
    const l = RamSet.local(global);
    var guards = try set.locks.lockMany(io, &.{.{ .ino = l, .mode = .exclusive }}, &.{});
    defer guards.release(io);
    const t = try set.mem.begin(@src());
    {
        errdefer set.mem.abandon(t);
        try t.stageDelete(key.TREE_DENTRIES, &key.dentryKey(inos.ROOT_INO, key.dentryNameHash54(name, set.mem.nameKey()), 0));
        try block_refs.stageOps(t, &.{block_refs.released(.{ .vol_tag = VOL_A, .block_idx = blk, .owner_ino = l, .block_index = 0 })});
        const xk = key.xattrKey(l, key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, set.mem.nameKey()), 0);
        try inos.stageDestroy(t, l, &.{&xk});
    }
    try set.mem.commit(t);
}

test "a release is a live owner of its block from before its releasing commit until its free (decision 0003 §AH.1; the macOS gate's `[C2] leaked` under the live writer storm): an online run inside the window — the destroy committed, the free not yet run — reads the block tracked and referenced by nothing, a leak unless the release holds it in flight; held, the run is clean, the free lands and the next run is clean; a release that drops its hold without the free is a genuine leak online and offline" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    const a = set.allocs[0];
    // The window, unheld: the run inside it reports the gate's line.
    const bare = try set.mkfile(inos.ROOT_INO, "bare", VOL_A, &.{40});
    try commitDestroy(set, bare, "bare", 40);
    {
        var r = try ram.run(set, .online, .{});
        defer r.deinit(gpa);
        try ram.expectClasses(r, &.{.c2});
        try testing.expectEqual(@as(u64, 40), r.findings[0].c2_leaked.block_idx);
    }
    _ = try a.freeBlock(40);
    // The window, held from before the releasing commit until after the free.
    const held = try set.mkfile(inos.ROOT_INO, "held", VOL_A, &.{41});
    var release = block_alloc.InflightAllocGuard.register(a, 41);
    try commitDestroy(set, held, "held", 41);
    {
        var r = try ram.run(set, .online, .{});
        defer r.deinit(gpa);
        try ram.expectClasses(r, &.{});
        try testing.expect(r.counters.inflight_exempted >= 1);
    }
    try testing.expectEqual(block_alloc.BeginFree.terminal, try a.freeBlock(41));
    {
        // Freed, the hold not yet dropped: the block is the free list's,
        // counted once by C6's arithmetic — never a second time as in flight.
        var r = try ram.run(set, .online, .{});
        defer r.deinit(gpa);
        for (r.findings) |f| std.debug.print("FALSE POSITIVE: {f}\n", .{f});
        try ram.expectClasses(r, &.{});
    }
    release.deinit();
    {
        var r = try ram.run(set, .online, .{});
        defer r.deinit(gpa);
        try ram.expectClasses(r, &.{});
    }
    // A hold dropped without the free shields nothing: the block is leaked.
    const lost = try set.mkfile(inos.ROOT_INO, "lost", VOL_A, &.{42});
    var dropped = block_alloc.InflightAllocGuard.register(a, 42);
    try commitDestroy(set, lost, "lost", 42);
    dropped.deinit();
    for ([_]finding.Mode{ .online, .offline }) |mode| {
        var r = try ram.run(set, mode, .{});
        defer r.deinit(gpa);
        try ram.expectClasses(r, &.{.c2});
        try testing.expectEqual(@as(u64, 42), r.findings[0].c2_leaked.block_idx);
    }
}

/// A mint whose publish lands at a chosen suspect's Phase-A registry check:
/// block `blk` minted to `owner` (its record and name committed, its layout
/// not), held in flight until the hook publishes it.
const LatePublish = struct {
    set: *RamSet,
    owner: u64,
    blk: u64,
    guard: block_alloc.InflightAllocGuard,
    published: bool = false,
    err: ?anyerror = null,

    fn hook(self: *LatePublish) classes.Host.RecheckHook {
        return .{ .ctx = self, .at = &at };
    }

    fn at(ctx: *anyopaque, point: classes.Host.RecheckPoint, block_idx: u64) void {
        const self: *LatePublish = @ptrCast(@alignCast(ctx));
        if (point != .phase_a_registry or block_idx != self.blk or self.published) return;
        self.publish() catch |e| {
            self.err = e;
        };
    }

    fn publish(self: *LatePublish) !void {
        const set = self.set;
        const t = try set.mem.begin(@src());
        {
            errdefer set.mem.abandon(t);
            const m = layout.Mapping.wholeBlock(VOL_A, self.blk * CHUNK, 0);
            const entries = [_]layout.MapEntryWire{(layout.MapEntry{ .block = 0, .mapping = m }).encode()};
            try layout.publishLayout(t, .{ .ino = RamSet.local(self.owner), .name_key = set.mem.nameKey(), .prev = null, .layout = layout.Layout.stripedInline(CHUNK, &entries), .proofs = &.{.{ .vol_tag = VOL_A, .offset = self.blk * CHUNK, .len = CHUNK, .completed_ns = 1 }} }, classes.refs_seam, set.geo);
        }
        try set.mem.commit(t);
        set.allocs[0].publishBlock(self.blk);
        self.guard.deinit();
        self.published = true;
    }
};

test "the ladder reads its fresh evidence after EVERY suspect's registry check (decision 0003 §AH.2): a mint in flight across the census whose publish lands at its own Phase-A check is its owner's block — the fresh census memoized for the suspect before it (a genuine leak) predated the publish and confirmed the mint as `[C2] leaked`; the genuine leak is still the one finding" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    const a = set.allocs[0];
    set.leak(VOL_A, 50);
    // The owner's record and name, then its block minted and in flight.
    const owner = try set.mkfile(inos.ROOT_INO, "late", VOL_A, &.{});
    _ = try a.allocateSpecificBlock(55);
    var late: LatePublish = .{ .set = set, .owner = owner, .blk = 55, .guard = block_alloc.InflightAllocGuard.register(a, 55) };
    var h = set.host(.online);
    h.recheck_hook = late.hook();
    var r = try ram.runHost(set, &h, .{});
    defer r.deinit(gpa);
    try testing.expect(late.err == null and late.published);
    for (r.findings) |f| if (f != .c2_leaked or f.c2_leaked.block_idx != 50) std.debug.print("FALSE POSITIVE: {f}\n", .{f});
    try ram.expectClasses(r, &.{.c2});
    try testing.expectEqual(@as(u64, 50), r.findings[0].c2_leaked.block_idx);
}

/// A leaked block freed and minted again (by a writer that relies on the
/// scan's epoch map, no registry entry) between the re-check's fresh
/// derivation and its verdict.
const ReMint = struct {
    alloc: *block_alloc.BlockAllocator,
    blk: u64,
    reminted: bool = false,
    err: ?anyerror = null,

    fn hook(self: *ReMint) classes.Host.RecheckHook {
        return .{ .ctx = self, .at = &at };
    }

    fn at(ctx: *anyopaque, point: classes.Host.RecheckPoint, block_idx: u64) void {
        const self: *ReMint = @ptrCast(@alignCast(ctx));
        if (point != .count_recheck_derived or block_idx != self.blk or self.reminted) return;
        self.step() catch |e| {
            self.err = e;
        };
    }

    fn step(self: *ReMint) !void {
        try testing.expectEqual(block_alloc.BeginFree.terminal, try self.alloc.freeBlock(self.blk));
        _ = try self.alloc.allocateSpecificBlock(self.blk);
        self.reminted = true;
    }
};

test "the C2-leaked re-check reads the allocator AFTER its fresh derivation (decision 0003 §AH.2): a leaked block freed and minted to a new writer between the derivation and the verdict is the writer's mint (epoch-young), never a leak — read before, the stale tracking confirmed the new owner's block" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    const a = set.allocs[0];
    set.leak(VOL_A, 50);
    var remint: ReMint = .{ .alloc = a, .blk = 50 };
    var h = set.host(.online);
    h.recheck_hook = remint.hook();
    {
        var r = try ram.runHost(set, &h, .{});
        defer r.deinit(gpa);
        try testing.expect(remint.err == null and remint.reminted);
        for (r.findings) |f| std.debug.print("FALSE POSITIVE: {f}\n", .{f});
        try ram.expectClasses(r, &.{});
        try testing.expect(r.counters.suspects_cleared >= 1);
    }
    // The writer abandons its mint: the next run is clean.
    a.abandonUnpublished(50);
    var again = try ram.run(set, .online, .{});
    defer again.deinit(gpa);
    try ram.expectClasses(again, &.{});
}

test "the pack-open ledger shields an open pack's +1 pin from C2/C3/C8; an open cross-volume intent shields C9/C10" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    set.leak(VOL_A, 40);
    try set.open_packs.put(gpa, VOL_A ^ (40 << 8), {});
    try set.orphan(inos.ROOT_INO, "f2");
    try set.intents.put(gpa, p.f2, {});
    var r = try ram.run(set, .online, .{});
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{});
    try testing.expect(r.counters.pack_ledger_exempted >= 1);
    try testing.expect(r.counters.unreferenced_intent_exempted >= 1);
    set.open_packs.clearRetainingCapacity();
    set.intents.clearRetainingCapacity();
    var r2 = try ram.run(set, .online, .{});
    defer r2.deinit(gpa);
    try ram.expectClasses(r2, &.{ .c2, .c9 });
}

test "C8's exemptions match the block's OWN volume: an open pack or an in-flight owner on B at index N never hides A's drift at N (a stray record on a leaked block of A)" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    // Block 5 is f2's on B and free on A: A's block 5 leaks with a stray record.
    set.leak(VOL_A, 5);
    try set.strayRef(RamSet.local(p.f1), 9, VOL_A, 5);
    try set.open_packs.put(gpa, VOL_B ^ (5 << 8), {});
    {
        var r = try ram.run(set, .online, .{});
        defer r.deinit(gpa);
        try ram.expectClasses(r, &.{ .c2, .c8 });
    }
    set.open_packs.clearRetainingCapacity();
    set.allocs[1].holdInflight(5);
    defer set.allocs[1].dropInflight(5);
    var r = try ram.run(set, .online, .{});
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{ .c2, .c8 });
}

test "C10's cross-owner names: a dentry in a judged directory naming a child in a LIVE foreign lessee's slot is the lessee's to judge (scoped, counted — a joiner's files under the manager's directory read as 50 dangling names on the two-daemon set), and a child minted past the census ceiling in the current era is the create window, never a blind dangling" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const p = try populate(set);
    // A joiner (slot 7, live) created `theirs` under this mount's `dir`:
    // the dentry is here, the child's record is in the joiner's tree.
    const joiner_slot: u16 = 7;
    try set.scope.foreign_live.put(gpa, joiner_slot, {});
    try set.dangle(p.dir, "theirs", inos.makeGlobalIno(40, joiner_slot, inos.ROUTING_WIDTH));
    // A name created during the walk whose record follows it: its raw
    // lies past the census ceiling AND at or past the era floor.
    try set.freezeEra();
    try set.dangle(p.dir, "future", RamSet.global(5000));
    var r = try ram.run(set, .online, .{});
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{});
    try testing.expect(r.counters.inode_plane_foreign_dentry_scoped >= 1);
    try testing.expect(r.counters.current_era_exempted >= 1);
    _ = set.scope.foreign_live.remove(joiner_slot);
}

test "forest scoping: a slot a LIVE foreign lessee holds is never judged (its orphans are its lessee's), a reader's uncovered window takes no inode-plane verdict, and tree 0's unleased witness makes an unknown-floor slot prior-era" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    try set.orphan(inos.ROOT_INO, "f2");
    try set.scope.foreign_live.put(gpa, 0, {});
    var scoped = try ram.run(set, .offline, .{});
    defer scoped.deinit(gpa);
    try ram.expectClasses(scoped, &.{});
    // The orphan is its lessee's; the name arms took no verdict at all (a
    // lessee's names are out of this forest).
    try testing.expect(scoped.counters.inode_plane_names_declined >= 1);
    _ = set.scope.foreign_live.remove(0);
    set.scope.reader_uncovered = true;
    var reader = try ram.run(set, .offline, .{});
    defer reader.deinit(gpa);
    try ram.expectClasses(reader, &.{});
    set.scope.reader_uncovered = false;
    // No floor at all: fail-closed … unless tree 0 says the slot is unleased.
    _ = set.scope.era_floor.remove(0);
    set.scope.posture = .manager;
    var unknown = try ram.run(set, .online, .{});
    defer unknown.deinit(gpa);
    try ram.expectClasses(unknown, &.{});
    try set.scope.unleased.put(gpa, 0, {});
    var witnessed = try ram.run(set, .online, .{});
    defer witnessed.deinit(gpa);
    try ram.expectClasses(witnessed, &.{.c9});
}

test "the census is bounded at its start watermarks: an ino at/past a slot's watermark is the next census's (skipped, the slot jumped), and the report JSON round-trips the run" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    _ = try populate(set);
    const late = try set.mkfile(inos.ROOT_INO, "late", VOL_B, &.{});
    try set.orphan(inos.ROOT_INO, "late");
    try set.freezeEra();
    try set.scope.watermarks.put(gpa, 0, RamSet.local(late));
    var r = try ram.run(set, .offline, .{});
    defer r.deinit(gpa);
    try ram.expectClasses(r, &.{});
    _ = set.scope.watermarks.remove(0);
    var r2 = try ram.run(set, .offline, .{});
    defer r2.deinit(gpa);
    try ram.expectClasses(r2, &.{.c9});
    var out: std.Io.Writer.Allocating = .init(gpa);
    defer out.deinit();
    try r2.writeJson(&out.writer);
    var back = try finding.Report.parse(gpa, out.written());
    defer back.deinit(gpa);
    try testing.expect(back.findings[0].eql(r2.findings[0]));
    try testing.expectEqual(r2.counters.inodes_scanned, back.counters.inodes_scanned);
}

// ---------------------------------------------------------------------------
// The per-suspect lookups are linear (0003 §AC.6): every lookup a class makes
// per suspect resolves through an index built once — the work examined is at
// most LOOKUP_LINEAR × the inputs (owners or names or views, plus the
// suspects), where a scan per suspect examined their product. Counters, not
// wall time.
// ---------------------------------------------------------------------------

/// The bound's factor: one pass to build the index and one probe per suspect
/// is 2 × the inputs; 3 leaves room for a duplicate's second look.
const LOOKUP_LINEAR: u64 = 3;
/// The fixtures' fill (files no suspect names, minted first so a scan per
/// suspect walks past them) and their damage count.
const LOOKUP_FILL: usize = 400;
const LOOKUP_DAMAGE: usize = 60;

fn fillNamed(set: *RamSet, comptime prefix: []const u8, n: usize, vol_tag: u64, block: ?u64) !void {
    var nb: [32]u8 = undefined;
    for (0..n) |i| {
        const nm = try std.fmt.bufPrint(&nb, prefix ++ "{d}", .{i});
        _ = try set.mkfile(inos.ROOT_INO, nm, vol_tag, if (block) |b| &.{b} else &.{@as(u64, i) % ram.BLOCKS});
    }
}

fn expectLinear(what: []const u8, steps: u64, inputs: u64) !void {
    if (steps <= LOOKUP_LINEAR * inputs) return;
    std.debug.print("\n{s}: {d} element(s) examined for {d} input(s) — bound {d}\n", .{ what, steps, inputs, LOOKUP_LINEAR * inputs });
    return error.TestUnexpectedResult;
}

test "C9's block count per candidate is linear (0003 §AC.6): 400 files with blocks, then 60 whose names are gone — the 60 C9 findings name each candidate's block count examining ≤ 3 × (owners + candidates) owners, where a scan per candidate walked past every owner minted before it" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    try fillNamed(set, "fill", LOOKUP_FILL, VOL_A, 0);
    try fillNamed(set, "gone", LOOKUP_DAMAGE, VOL_B, null);
    var nb: [32]u8 = undefined;
    for (0..LOOKUP_DAMAGE) |i| try set.orphan(inos.ROOT_INO, try std.fmt.bufPrint(&nb, "gone{d}", .{i}));
    try set.freezeEra();
    var r = try ram.run(set, .offline, .{});
    defer r.deinit(gpa);
    var c9: usize = 0;
    for (r.findings) |f| if (f == .c9_unreferenced) {
        c9 += 1;
        try testing.expectEqual(@as(u32, 1), f.c9_unreferenced.blocks);
    };
    try testing.expectEqual(LOOKUP_DAMAGE, c9);
    try expectLinear("C9 owner lookups", r.counters.c9_owner_steps, LOOKUP_FILL + 2 * LOOKUP_DAMAGE);
}

test "C2-lost's owner per block is linear (0003 §AC.6): 400 files on one block of A, then 60 files on 60 blocks of B the allocator forgets — the 60 lost findings name their owners examining ≤ 3 × (bindings + suspects) bindings, where a scan per lost block walked every binding before its owner" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    try fillNamed(set, "fill", LOOKUP_FILL, VOL_A, 0);
    try fillNamed(set, "lost", LOOKUP_DAMAGE, VOL_B, null);
    for (0..LOOKUP_DAMAGE) |i| set.untrack(VOL_B, @as(u64, i) % ram.BLOCKS);
    try set.freezeEra();
    var r = try ram.run(set, .offline, .{});
    defer r.deinit(gpa);
    var lost: usize = 0;
    for (r.findings) |f| if (f == .c2_lost) {
        lost += 1;
        try testing.expect(f.c2_lost.ino != 0);
    };
    try testing.expectEqual(LOOKUP_DAMAGE, lost);
    try expectLinear("C2 owner lookups", r.counters.c2_owner_steps, LOOKUP_FILL + 2 * LOOKUP_DAMAGE);
}

test "C10's distinct names per candidate are linear (0003 §AC.6): 60 files with two names each and nlink 5, online — the ladder's re-check counts each candidate's deduplicated names examining ≤ 3 × (names + candidates) names, where a scan per candidate walked every collected name (and every earlier one per match)" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    var nb: [32]u8 = undefined;
    for (0..LOOKUP_DAMAGE) |i| {
        const f = try set.mkfile(inos.ROOT_INO, try std.fmt.bufPrint(&nb, "multi{d}", .{i}), VOL_B, &.{});
        try set.link(inos.ROOT_INO, try std.fmt.bufPrint(&nb, "multi{d}-link", .{i}), f);
        try set.setNlink(f, 5);
    }
    try set.freezeEra();
    var r = try ram.run(set, .online, .{});
    defer r.deinit(gpa);
    var high: usize = 0;
    for (r.findings) |f| if (f == .c10_nlink_too_high) {
        high += 1;
        try testing.expectEqual(@as(u32, 2), f.c10_nlink_too_high.names);
    };
    try testing.expectEqual(LOOKUP_DAMAGE, high);
    try expectLinear("C10 name lookups", r.counters.c10_name_steps, 2 * LOOKUP_DAMAGE + LOOKUP_DAMAGE);
}

test "C17's routing check is linear in the views (0003 §AC.6): 20 directories striped over 4 stripes each, a name in every stripe where its hash routes it — 0 findings, the routing check examining ≤ 3 × (views + stripes + names), where every striped directory scanned every view's place in its map" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    const stripe_mod = @import("../../meta/sym/xv/stripe.zig");
    const DIRS = 20;
    const K = 4;
    const seed = set.mem.nameKey();
    var nb: [32]u8 = undefined;
    var markers: [K][stripe_mod.MARKER_NAME_LEN]u8 = undefined;
    for (0..DIRS) |d| {
        const sd = try set.mkdir(inos.ROOT_INO, try std.fmt.bufPrint(&nb, "striped{d}", .{d}));
        var stripes: [K]u64 = undefined;
        for (&stripes, 0..) |*s, i| s.* = try set.mkdir(inos.ROOT_INO, try std.fmt.bufPrint(&nb, "s{d}-{d}", .{ d, i }));
        for (stripes, 0..) |s, i| try set.dangle(sd, stripe_mod.markerName(@intCast(i), &markers[i]), s);
        try set.dangle(sd, stripe_mod.STRIPED, stripes[0]);
        for (0..K) |i| {
            const nm = try std.fmt.bufPrint(&nb, "n{d}-{d}", .{ d, i });
            _ = try set.mkdir(stripes[stripe_mod.stripeOf(key.dentryNameHash54(nm, seed), K)], nm);
        }
    }
    try set.freezeEra();
    var r = try ram.run(set, .offline, .{});
    defer r.deinit(gpa);
    for (r.findings) |f| if (f == .c17_stripe) {
        std.debug.print("unexpected: {f}\n", .{f});
        return error.TestUnexpectedResult;
    };
    // The views: the root, every striped directory, every stripe.
    const views = 1 + DIRS + DIRS * K;
    try expectLinear("C17 routing", r.counters.c17_route_steps, views + DIRS * K + DIRS * K);
}

test "the ladder's C2-leaked / C3 / C6 re-checks derive the fresh census's references once per volume (0003 §AC.6): 400 files on one block of A and 60 leaked blocks of B, online — the 60 C2-leaked findings re-check examining ≤ 3 × (bindings + suspects) bindings, where each suspect re-derived every binding of the fresh census" {
    const gpa = testing.allocator;
    const set = try RamSet.init(gpa);
    defer set.deinit();
    try fillNamed(set, "fill", LOOKUP_FILL, VOL_A, 0);
    for (0..LOOKUP_DAMAGE) |i| set.leak(VOL_B, @as(u64, i) % ram.BLOCKS);
    try set.freezeEra();
    var r = try ram.run(set, .online, .{});
    defer r.deinit(gpa);
    var leaked: usize = 0;
    for (r.findings) |f| if (f == .c2_leaked) {
        leaked += 1;
    };
    try testing.expectEqual(LOOKUP_DAMAGE, leaked);
    try expectLinear("re-check derivations", r.counters.recheck_derived_steps, LOOKUP_FILL + LOOKUP_DAMAGE);
}
