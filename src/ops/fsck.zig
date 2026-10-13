//! fsck C1–C17 and the data scrub (charter §8 "fsck", §5.5 "fsck classes on
//! the forest", §5.6, §6.1, §1.8, §1.10; `docs/porting/ops_fsck.md`) — OUR
//! oracle: the reference's fsck is retired for v4 trees (decision 0002 rows
//! 50, 104, 105, 176, 177, 202, 352).
//!
//! One engine, two harnesses: ONLINE reads RAM-authoritative state with the
//! suspects machinery armed (the allocation-epoch side map = the allocator's
//! per-cell word, the in-flight registry, the settle, the re-check under the
//! object's exclusive 4a lease); OFFLINE holds the D0 guard, nothing is in
//! flight by definition, so suspects report directly. Detection never
//! mutates; findings are the only repair input; `--repair` plans (dry-run
//! default) and `--apply` executes with quarantine FIRST and verify-before-
//! repair per class; nothing is ever fabricated.
//!
//! The classes WRAP the oracles the earlier rungs landed and duplicate none
//! of them: C8/C3 `data/block_refs.verifyRefs/confirm/repairPlan`, C11
//! `data/kvmap.verifyBlockMap/nextOwner`, C2/C6 `data/block_alloc`'s fsck
//! seams + `sym_planes/data_bitmap.verifyBitmap`, C16 `shared_index.
//! sharedIndexDrift`, C17 `xv/stripe.verifyStripes`, C14 `slots/lease_table.
//! liveOnTwoPages`, C13/C15 the membership recovery's arms. The modules:
//! `finding` (ONE tagged union, the report, `merge_reports`), `census` (the
//! ino bitmaps, the kind passes, the era floor, forest scoping, shards),
//! `classes` (C1–C17 + the ladder + the repair engine), `scrub` (C7),
//! `runner` (the pass order, the settle, the harnesses). Portable over the
//! `census.Store` face; the device/mount harnesses are Linux (`runner`).

const std = @import("std");
const counters = @import("../core/counters.zig");
const admin_frames = @import("jobs/admin_frames.zig");

pub const finding = @import("fsck/finding.zig");
pub const census = @import("fsck/census.zig");
pub const classes = @import("fsck/classes.zig");
pub const scrub = @import("fsck/scrub.zig");
pub const runner = @import("fsck/runner.zig");

// ---------------------------------------------------------------------------
// Constants with their WHY (`ops_fsck.md` §9).
// ---------------------------------------------------------------------------

/// Records per tree-range fetch = the throttle's duty-cycle unit: one page
/// of owned copies stays under 32 KiB while a walk streams (FSCK:L354-356;
/// the same page the mount-recovery ledger scan uses).
pub const SCAN_PAGE: usize = 512;
/// Blocks per scrub duty-cycle unit: 8 × 4 MiB = one device queue's worth
/// of sequential reads between pacer checks (FSCK:L379).
pub const SCRUB_BATCH: usize = 8;
/// Staging dirs hold thousands of files; a bound on the header scan, never
/// an allocation authority (FSCK:L381).
pub const CUSTODY_SCAN_MAX: usize = 1_000_000;
/// The report's forward-only schema (FSCK:L352).
pub const REPORT_SCHEMA: u32 = 1;
/// The admin lane's bounded report view (`ipc/wire.zig` ADMIN_ARG_MAX is 60
/// KiB; the durable report stays complete, FSCK:L1216).
pub const ADMIN_WIRE_CAP: usize = 60 * 1024;
/// The online job's `arg0` option bits (`JobType.fsck`; the CLI's
/// `--scrub` / the `scrub` verb ride the record, never a string) — the
/// admin lane's `AdminFsckReq.flags` verbatim.
pub const JOB_ARG_SCRUB: u64 = admin_frames.FSCK_FLAG_SCRUB;
pub const JOB_ARG_SCRUB_ONLY: u64 = admin_frames.FSCK_FLAG_SCRUB_ONLY;

/// The online job's `arg1`: the shard `k/N` the run judges — `N` in the
/// high word, `k` in the low; 0 = the whole set (an unsharded run). One
/// durable word, so an adopted sharded run resumes as the same shard.
pub fn jobShardArg(shard: ?finding.Shard) u64 {
    const s = shard orelse return 0;
    return (@as(u64, s.n) << 32) | s.k;
}

pub fn jobShardOf(arg1: u64) ?finding.Shard {
    const n: u32 = @truncate(arg1 >> 32); // the high word (`jobShardArg`'s layout)
    const k: u32 = @truncate(arg1);
    if (n == 0 or k >= n) return null;
    return .{ .k = k, .n = n };
}

// ---------------------------------------------------------------------------
// Observability (`ops_fsck.md` §6): the `fsck_*`, `scrub_*` and the repair
// families. Engagement law: a healthy fleet has `fsck_findings == 0` while
// the exemption counters GROW under churn — their growth is the proof the
// shield does work rather than sitting vacuous.
// ---------------------------------------------------------------------------

pub const stats = struct {
    // ---- the census ----
    pub var fsck_inodes_scanned: counters.ShardedCounter = .{};
    pub var fsck_nodes_walked: counters.ShardedCounter = .{};
    pub var fsck_dentry_refs_indexed: counters.ShardedCounter = .{};
    pub var fsck_blocks_checked: counters.ShardedCounter = .{};
    pub var fsck_refcounts_checked: counters.ShardedCounter = .{};
    /// Data-device bytes the C7 scrub read (the device-read ledger's
    /// maintenance arm, decision 0003 §K — outside the plane's choke point).
    pub var fsck_scrub_read_bytes: counters.ShardedCounter = .{};
    /// The last run's wall clock, seconds (stored).
    pub var fsck_scan_secs: counters.Gauge = .{};
    /// The inode plane's coverage: `leased ∪ tree-0 unleased` slots (stored).
    pub var fsck_inode_plane_slots_covered: counters.Gauge = .{};
    // ---- the ladder: suspects and the shields ----
    pub var fsck_suspects: counters.ShardedCounter = .{};
    pub var fsck_suspects_cleared: counters.ShardedCounter = .{};
    pub var fsck_epoch_exempted: counters.ShardedCounter = .{};
    pub var fsck_inflight_exempted: counters.ShardedCounter = .{};
    pub var fsck_pack_ledger_exempted: counters.ShardedCounter = .{};
    pub var fsck_current_era_exempted: counters.ShardedCounter = .{};
    pub var fsck_unreferenced_intent_exempted: counters.ShardedCounter = .{};
    pub var fsck_nlink_transient_cleared: counters.ShardedCounter = .{};
    pub var fsck_crossing_exempted: counters.ShardedCounter = .{};
    pub var fsck_alloc_bitmap_tracked_exempted: counters.ShardedCounter = .{};
    pub var fsck_alloc_census_declined: counters.ShardedCounter = .{};
    // ---- forest scoping ----
    pub var fsck_c1_projection_slots_scoped: counters.ShardedCounter = .{};
    pub var fsck_c4_custody_scoped: counters.ShardedCounter = .{};
    pub var fsck_block_plane_foreign_scoped: counters.ShardedCounter = .{};
    pub var fsck_inode_plane_names_declined: counters.ShardedCounter = .{};
    pub var fsck_inode_plane_foreign_slot_scoped: counters.ShardedCounter = .{};
    pub var fsck_inode_plane_window_scoped: counters.ShardedCounter = .{};
    pub var fsck_inode_plane_foreign_dentry_scoped: counters.ShardedCounter = .{};
    // ---- the inode plane ----
    pub var fsck_nlink_names_counted: counters.ShardedCounter = .{};
    pub var fsck_nlink_mismatch_high: counters.ShardedCounter = .{};
    pub var fsck_nlink_mismatch_low: counters.ShardedCounter = .{};
    pub var fsck_nlink_zero_named: counters.ShardedCounter = .{};
    pub var fsck_dangling_dentries: counters.ShardedCounter = .{};
    // ---- the class gauges (the last run's confirmed counts) ----
    pub var fsck_slot_custody_conflicts: counters.Gauge = .{};
    pub var fsck_unrecovered_appenders: counters.Gauge = .{};
    /// THE tripwire: a confirmed finding on a healthy volume cannot happen.
    pub var fsck_findings: counters.Tripwire = .init("fsck_findings");
    /// C12's live must-stay-0 (the overlap arm).
    pub var fsck_tenant_overlap_findings: counters.Tripwire = .init("fsck_tenant_overlap_findings");
    /// The last run's finding count (stored; the exit verdict's number).
    pub var fsck_findings_last: counters.Gauge = .{};
    // ---- the scrub ----
    pub var scrub_blocks_scanned: counters.ShardedCounter = .{};
    pub var scrub_bytes_scanned: counters.ShardedCounter = .{};
    pub var scrub_aead_verified: counters.ShardedCounter = .{};
    pub var scrub_frame_verified: counters.ShardedCounter = .{};
    /// The honesty gauge (OQ-B): plain blocks have no stored checksum, so
    /// read success is the only verdict — counted apart from a verified one.
    pub var scrub_readability_only: counters.ShardedCounter = .{};
    pub var scrub_failures: counters.ShardedCounter = .{};
    pub var scrub_pin_unstable_cleared: counters.ShardedCounter = .{};
    // ---- the repair engine ----
    pub var fsck_repairs_planned: counters.ShardedCounter = .{};
    pub var fsck_repairs_applied: counters.ShardedCounter = .{};
    pub var fsck_repairs_refused: counters.ShardedCounter = .{};
    pub var fsck_quarantined_records: counters.ShardedCounter = .{};
    pub var fsck_quarantined_blocks: counters.ShardedCounter = .{};
    pub var fsck_quarantined_bytes: counters.ShardedCounter = .{};
    /// One per class, `fsck_repair_class_c<N>` (0002 row: a comptime table
    /// over the finding union's classes, never a hand-indexed array); C8's
    /// lives beside its oracle (`block_refs.stats.fsck_repair_class_c8`).
    pub var fsck_repair_class: [17]counters.ShardedCounter = @splat(.{});
};

/// `.stats`-ready JSON of every instrument above (the `fsck` family).
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try counters.writeNamespaceJson(stats, w);
}

/// The repair-class counter of class `n` (1-based).
pub fn repairClassCounter(n: u8) *counters.ShardedCounter {
    std.debug.assert(n >= 1 and n <= 17);
    return &stats.fsck_repair_class[n - 1];
}

test {
    _ = finding;
    _ = census;
    _ = classes;
    _ = scrub;
    _ = runner;
    _ = @import("fsck/tests.zig");
}

test "the online job's shard word: k/N round-trips through arg1, 0 is the whole set, a malformed word (N 0, k ≥ N) decodes to none" {
    try std.testing.expectEqual(@as(u64, 0), jobShardArg(null));
    try std.testing.expectEqual(@as(?finding.Shard, null), jobShardOf(0));
    const s: finding.Shard = .{ .k = 2, .n = 3 };
    const w = jobShardArg(s);
    try std.testing.expectEqual(@as(u64, (3 << 32) | 2), w);
    try std.testing.expectEqual(s, jobShardOf(w).?);
    try std.testing.expectEqual(@as(?finding.Shard, null), jobShardOf((3 << 32) | 3));
    try std.testing.expectEqual(@as(?finding.Shard, null), jobShardOf(5));
}

test "stats JSON names the tripwire by its site, the class array as one array and the honesty gauge" {
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    inline for (.{ "\"fsck_findings\":", "\"fsck_repair_class\":[", "\"scrub_readability_only\":", "\"fsck_epoch_exempted\":", "\"fsck_tenant_overlap_findings\":" }) |k| {
        try std.testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}
