//! The metadata backend facade (Linux): one writable MetaLV volume —
//! `format`, `open` (the mount-time recovery ladder), `begin` / `commit` /
//! `commitGroup`, `lookup` / `range`, `fsync`, `checkpointNow`, `close`
//! (charter §5.1–§5.3, §5.7 layer A, §1.10; survey §1.3 `open_inner`,
//! §2.14 inos, §6 the crash contract; the reference's `backend.rs`
//! `open_inner` / `shutdown`).
//!
//! ## The mount ladder (`open`), in order
//!
//! 1. The D0 single-writer guard's layer A: `flock(LOCK_EX | LOCK_NB)` on a
//!    DEDICATED daemon-lifetime fd (never a cache-owned one) — `EWOULDBLOCK`
//!    refuses `Busy` loud before a byte is read. Layers B1/B2 (PR, the
//!    writer claim) are later rungs.
//! 2. The device: registered O_DIRECT (`uring_fs.registerMetaDevice`), the
//!    io pool, the volume length.
//! 3. Both superblock copies (DUR-5: a primary that classifies is
//!    authoritative; the tail copy serves only when sector 0 errors) →
//!    v3, no unknown incompat bit, not the symmetric forest (rung 6).
//! 4. The newest valid ledger slot (none on a v3 volume is corruption).
//! 5. The ring replayed from the record's tail (`replayScanImage`: nothing
//!    inside the window fails loud; torn entries drop and count —
//!    `replay_dropped_torn > 0` after a CLEAN unmount is the tripwire).
//! 6. The allocator from the newest A/B pages + the window's deltas.
//! 7. The node cache over the device loader; every tree the record names
//!    opened at its root; the two-phase replay into the cache; the
//!    replayed in-window frees parked (a live root's dropped); `next_ino =
//!    max(ledger, replayed + 1)`.
//! 8. The journal core resumed at the recovered head, the registry, the
//!    head's sector pad when a torn window left it mid-sector.
//! 9. The journal lane (`sqz-jrnl<N>`) → the checkpointer bound to the
//!    device sinks → the bring-up residue covered → the conveyor → the
//!    checkpoint task's thread.
//!
//! `close` is the clean dismount: no new commits, the conveyor quiesced,
//! the lane drained (its trailing barrier), the checkpoint task's final
//! tick (the fixpoint: `tail == head`, no dirty bitmap page — an empty
//! replay window for the next mount), the flock released.
//!
//! ## Durability posture
//!
//! Coalesced cadence (`SQUEEZEFS_META_FLUSH_INTERVAL_MS` > 0, 50 ms
//! shipped): an ack means the entry LANDED (its O_DIRECT write completed at
//! the device) — durable against a process kill, NOT against a power cut
//! with a volatile device cache until the lane's trailing barrier (≤ the
//! interval) or `fsync`. Strict (`= 0`): the barrier precedes every ack.
//! The kill matrix below pins both.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const align_probe = @import("../io/align_probe.zig");
const uring_fs = @import("../io/uring_fs.zig");
const sync = @import("../sync/sync.zig");
const key = @import("key.zig");
const record = @import("record.zig");
const node = @import("node.zig");
const node_cache = @import("node_cache.zig");
const node_seq = @import("node_seq.zig");
const tree = @import("tree.zig");
const smo = @import("smo.zig");
const jc = @import("journal_core.zig");
const journal = @import("journal.zig");
const journal_lane = @import("journal_lane.zig");
const ledger = @import("ledger.zig");
const superblock = @import("superblock.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");
const checkpoint = @import("checkpoint.zig");
const checkpoint_cycle = @import("checkpoint_cycle.zig");
const heap_law = @import("heap_law.zig");
const inos = @import("inos.zig");
const tx = @import("tx.zig");
const conveyor = @import("conveyor.zig");
const format = @import("format.zig");
const geometry = @import("geometry.zig");
const device_loader = @import("device_loader.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("backend is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const KvTx = tx.KvTx;
pub const KvTree = tree.KvTree;
pub const Outcome = conveyor.Outcome;
pub const Error = conveyor.Error;

/// Why a mount refused (loud, typed; the errno table is one row each).
pub const OpenError = error{
    /// Layer A: another writer holds the flock.
    Busy,
    /// Blank superblock: `squeezefs format` first.
    NotFormatted,
    /// A format-v2 volume: reformat required.
    LegacyFormat,
    /// Unknown incompat bits / the symmetric forest (rung 6): reformat or
    /// a later binary.
    UnsupportedFormat,
    /// Corruption outside the replay window (no ledger record, a root that
    /// does not verify, a malformed allocator delta).
    Corrupt,
    DeviceRefused,
    Io,
    OutOfMemory,
    LaneSpawnFailed,
    ThreadSpawnFailed,
};

pub fn openErrno(e: OpenError) linux.E {
    return switch (e) {
        error.Busy => .BUSY,
        error.NotFormatted, error.LegacyFormat, error.UnsupportedFormat => .INVAL,
        error.Corrupt, error.Io, error.DeviceRefused, error.LaneSpawnFailed, error.ThreadSpawnFailed => .IO,
        error.OutOfMemory => .NOMEM,
    };
}

pub const Stats = struct {
    /// `writer_guard_busy_refusals`: mounts refused by a held flock.
    guard_busy_refusals: counters.ShardedCounter = .{},
    mounts: counters.ShardedCounter = .{},
    clean_unmounts: counters.ShardedCounter = .{},
};

pub var stats: Stats = .{};

/// The mount's replay verdict (`meta_kv_replay_*`).
pub const ReplayStats = struct {
    entries: u64,
    dropped_torn: u64,
    /// Content records of slot-tree kinds (0 after a clean unmount).
    content_records: u64,
    /// Whether the previous writer left an empty window at the ledger's
    /// tail (the shutdown fixpoint).
    clean_unmount: bool,
    replay_ns: u64,
};

/// Test seams: cut points of the kill matrix (`SQUEEZEFS_TEST_*` class —
/// a hook that runs at the exact protocol instant, never a sleep). Each is
/// one load when unarmed.
pub const TestSeams = struct {
    /// C9: right BEFORE the ledger slot write (pages + appends barriered,
    /// the record absent).
    before_ledger: ?node_cache.Hook = null,
    /// C10: right AFTER the ledger slot write, before any barrier covers it
    /// (`reusable_upto` still at the previous record's tail).
    after_ledger: ?node_cache.Hook = null,
    /// P4: an SMO entry about to be written — the swap happened, the
    /// successor images are barriered, the entry is not on the device.
    before_smo_entry: ?node_cache.Hook = null,
};

pub var test_seams: TestSeams = .{};

pub const OpenOptions = struct {
    env: *const std.process.Environ.Map,
    /// The process's io pool; one is created (and owned) when null.
    pool: ?*uring_fs.Pool = null,
    /// `sqz-jrnl<index>`.
    index: u32 = 0,
    /// The node-cache budget (the R5 share `max(budget/16, 512 MiB)` at the
    /// daemon; tests shrink it).
    cache_budget_bytes: u64 = node_cache.DEFAULT_CACHE_BUDGET_BYTES,
    /// Possible CPUs for the batch-cap derivation.
    cpus: u32 = 0,
    /// Error logs the LANE thread is expected to emit in a test (the log
    /// seam is thread-local; `journal_lane.Options.test_error_logs`).
    test_error_logs: if (builtin.is_test) usize else void = if (builtin.is_test) 0 else {},
};

pub const FormatOptions = struct {
    env: *const std.process.Environ.Map,
    pool: ?*uring_fs.Pool = null,
    node_size: u64 = superblock.default_node_size,
    journal_len_override: ?u64 = null,
    class: geometry.FormatClass = .single_writer,
    uuid: [16]u8,
    set_uuid: [16]u8,
    root_uid: u32 = 0,
    root_gid: u32 = 0,
    format_config_xattr: ?[]const u8 = null,
};

pub const FormatError = error{ DeviceRefused, Io, OutOfMemory, Corrupt, VolumeTooSmall, InvalidNodeSize, InvalidJournalOverride, CannotStat, NotADeviceOrFile, Unexpected };

/// `squeezefs format`: register the device (O_DIRECT from the first byte),
/// plan the image, write the steps with their barriers in the plan's
/// order (zero the fixed region → roots → bitmap → ledger → barrier →
/// tail superblock → barrier → sector 0 → barrier).
pub fn formatVolume(gpa: Allocator, path: []const u8, opts: FormatOptions) FormatError!void {
    const mode = uring_fs.registerMetaDevice(gpa, opts.env, path) catch return error.DeviceRefused;
    const pool = opts.pool orelse (uring_fs.Pool.init(gpa, uring_fs.Config.resolve(opts.env, derive.Facts.probe())) catch return error.DeviceRefused);
    defer if (opts.pool == null) pool.deinit();
    const volume_len = try device_loader.volumeLength(path);
    var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };
    var diag: geometry.PlanRefusal = undefined;
    var plan = format.planFormat(gpa, volume_len, .{
        .node_size = opts.node_size,
        .journal_len_override = opts.journal_len_override,
        .class = opts.class,
        .uuid = opts.uuid,
        .set_uuid = opts.set_uuid,
        .root_uid = opts.root_uid,
        .root_gid = opts.root_gid,
        .format_config_xattr = opts.format_config_xattr,
    }, &diag) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.VolumeTooSmall => error.VolumeTooSmall,
        error.InvalidNodeSize => error.InvalidNodeSize,
        error.InvalidJournalOverride => error.InvalidJournalOverride,
        else => error.Corrupt,
    };
    defer plan.deinit();
    try dev.writeSteps(plan.steps);
    log.info("formatted {s}: {d} B, node {d} B, journal {d} B, {d} extents, class {s}", .{ path, volume_len, opts.node_size, plan.superblock.journal.len, plan.superblock.totalExtents(), @tagName(plan.class) });
}

// ---------------------------------------------------------------------------
// The backend.
// ---------------------------------------------------------------------------

pub const KvBackend = struct {
    gpa: Allocator,
    io: Io,
    env: *const std.process.Environ.Map,
    path: []u8,
    index: u32,
    /// Layer A: the dedicated flock fd, held until `close`.
    guard_fd: linux.fd_t,
    pool: *uring_fs.Pool,
    pool_owned: bool,
    volume_len: u64,
    sb: superblock.SuperblockV3,
    /// The mounted record (its stamp rides every checkpoint record).
    mounted: ledger.LedgerRecord,
    dev: device_loader.DeviceLoader,
    layout: node.NodeLayout,
    cache: *node_cache.NodeCache,
    alloc: alloc_ext.ExtentAllocator,
    seq: node_seq.NodeSeqHandle,
    trees: []*KvTree,
    ring_layout: journal.RingLayout,
    geo: jc.CoreGeometry,
    core: jc.JournalCore,
    reg: jc.InflightRegistry,
    ring_stats: journal.RingStats = .{},
    lane: *journal_lane.JournalLane,
    smo_journal: smo.SmoJournal,
    smo_ctx: smo.SmoContext,
    ckpt_seam: smo.CheckpointSeam,
    flusher: checkpoint_cycle.CacheFlusher,
    cp: checkpoint_cycle.Checkpointer,
    task: checkpoint_cycle.CheckpointTask,
    /// The volume's structural (SMO) context lock: the task's ticks, the
    /// lane's hole checkpoints, a committer's space retry, `checkpointNow`.
    smo_lock: Io.Mutex = .init,
    /// Admission parkers wait here; the checkpointer's `afterDurableBarrier`
    /// is the ONE notifier (every barrier that moves `reusable_upto`).
    space: sync.Notify = .init,
    ckpt_thread: ?std.Thread = null,
    cv: *conveyor.Conveyor,
    ino: inos.InoWatermark,
    replay: ReplayStats,
    /// The threshold drain's round-robin position over `trees` (task-owned).
    maintenance_cursor: usize = 0,
    closed: bool = false,

    // ---- open ----

    pub fn open(gpa: Allocator, io: Io, path: []const u8, opts: OpenOptions) OpenError!*KvBackend {
        const t0 = clock.monotonicNs();
        // 1. Layer A.
        const guard_fd = try acquireWriterFlock(path);
        errdefer _ = linux.close(guard_fd);
        // 2. The device.
        const mode = uring_fs.registerMetaDevice(gpa, opts.env, path) catch return error.DeviceRefused;
        const pool = opts.pool orelse (uring_fs.Pool.init(gpa, uring_fs.Config.resolve(opts.env, derive.Facts.probe())) catch return error.DeviceRefused);
        errdefer if (opts.pool == null) pool.deinit();
        const volume_len = device_loader.volumeLength(path) catch return error.DeviceRefused;
        const be = try gpa.create(KvBackend);
        errdefer gpa.destroy(be);
        const owned_path = try gpa.dupe(u8, path);
        errdefer gpa.free(owned_path);
        var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = owned_path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };

        // 3. The superblock, both copies.
        const sector0 = try dev.readRange(gpa, 0, superblock.sector_len);
        defer gpa.free(sector0);
        const backup: ?[]u8 = if (superblock.backupOffset(volume_len)) |off| try dev.readRange(gpa, off, superblock.sector_len) else null;
        defer if (backup) |b| gpa.free(b);
        var sb_diag: superblock.Refusal = undefined;
        const classified = superblock.classifyVolumeImages(sector0, backup, volume_len, &sb_diag) catch {
            var buf: [512]u8 = undefined;
            var w: std.Io.Writer = .fixed(&buf);
            sb_diag.write(&w) catch {};
            log.err("meta volume {s}: superblock refused: {s}", .{ path, w.buffered() });
            return error.Corrupt;
        };
        const sb = switch (classified.format) {
            .blank => {
                log.err("{s} is not formatted (zeroed superblock) — run `squeezefs format` first", .{path});
                return error.NotFormatted;
            },
            .v2_legacy => {
                log.err("{s} is a legacy format-v2 volume — v2 support was removed; reformat required", .{path});
                return error.LegacyFormat;
            },
            .v3 => |s| s,
        };
        if (classified.slot == .backup) log.err("{s}: sector 0 is unreadable — mounting from the redundant superblock copy; investigate the device", .{path});
        if (sb.unknownIncompat() != 0) {
            log.err("{s}: unknown incompat feature bits {x} — this binary cannot mount it (reformat required)", .{ path, sb.unknownIncompat() });
            return error.UnsupportedFormat;
        }
        if (sb.symmetricForestStamped()) {
            log.err("{s}: a symmetric-forest volume (bit 17) — the forest mount lands with rung 6; this rung mounts the --single-writer class", .{path});
            return error.UnsupportedFormat;
        }
        dev.ledger_base = sb.root_ledger.start;
        dev.bitmap_base = sb.alloc_bitmap.start;

        // 4. The ledger.
        const ledger_img = try dev.readRange(gpa, sb.root_ledger.start, @intCast(sb.root_ledger.len));
        defer gpa.free(ledger_img);
        var mounted = (try ledger.newestValid(gpa, ledger_img)) orelse {
            log.err("{s}: no valid root-ledger record in any of the 32 slots — the volume carries a v3 superblock, so this is corruption, not a fresh format", .{path});
            return error.Corrupt;
        };
        errdefer mounted.deinit(gpa);

        // 5. The ring: one read, the chain walk from the tail.
        const ring_layout = journal.RingLayout.fromExtent(.{ .start = sb.journal.start, .len = sb.journal.len });
        const geo = journal.productionGeometry(ring_layout.pages, mode.grain());
        const ring_img = try dev.readRange(gpa, ring_layout.base, @intCast(ring_layout.ringBytes()));
        defer gpa.free(ring_img);
        var recovery = try journal.replayScanImage(gpa, ring_img, geo, mounted.journal_tail_seq, journal.ROOT_AUTHORITY_WRITER);
        defer recovery.deinit();
        const clean_unmount = recovery.entries.len == 0 and recovery.head_pos == mounted.journal_tail_seq;
        journal.noteReplay(&recovery, clean_unmount);

        // 6. The allocator.
        const total_extents = sb.totalExtents();
        const bitmap_img = try dev.readRange(gpa, sb.alloc_bitmap.start, @intCast(sb.alloc_bitmap.len));
        defer gpa.free(bitmap_img);
        var replay_arena = std.heap.ArenaAllocator.init(gpa);
        defer replay_arena.deinit();
        const alloc_replay = try allocReplayEntries(replay_arena.allocator(), recovery.entries);
        var alloc_diag: alloc_ext.LoadRefusal = undefined;
        var alloc = alloc_ext.ExtentAllocator.load(gpa, bitmap_img, total_extents, alloc_core.compactionReserveExtents(total_extents), alloc_ext.pending_free_cap, mounted.journal_tail_seq, alloc_replay, &alloc_diag) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => {
                var buf: [512]u8 = undefined;
                var w: std.Io.Writer = .fixed(&buf);
                alloc_diag.write(&w) catch {};
                log.err("{s}: allocator load refused: {s}", .{ path, w.buffered() });
                return error.Corrupt;
            },
        };
        errdefer alloc.deinit();

        // 7. The cache, the trees, the replay, the frees, the inos.
        const layout = node.NodeLayout.new(sb.node_size, null) catch return error.Corrupt;
        be.* = .{
            .gpa = gpa,
            .io = io,
            .env = opts.env,
            .path = owned_path,
            .index = opts.index,
            .guard_fd = guard_fd,
            .pool = pool,
            .pool_owned = opts.pool == null,
            .volume_len = volume_len,
            .sb = sb,
            .mounted = mounted,
            .dev = dev,
            .layout = layout,
            .cache = undefined,
            .alloc = alloc,
            .seq = node_seq.NodeSeqHandle.shared(@max(mounted.seq, mounted.node_seq_watermark)),
            .trees = &.{},
            .ring_layout = ring_layout,
            .geo = geo,
            .core = jc.JournalCore.init(geo, recovery.head_pos, mounted.journal_tail_seq),
            .reg = undefined,
            .lane = undefined,
            .smo_journal = undefined,
            .smo_ctx = undefined,
            .ckpt_seam = undefined,
            .flusher = undefined,
            .cp = undefined,
            .task = undefined,
            .cv = undefined,
            .ino = inos.InoWatermark.init(mounted.next_ino),
            .replay = .{ .entries = recovery.entries.len, .dropped_torn = recovery.dropped_torn, .content_records = 0, .clean_unmount = clean_unmount, .replay_ns = 0 },
        };
        // The device path the pool keys on is the backend's owned copy.
        be.dev.path = be.path;
        be.cache = try node_cache.NodeCache.init(gpa, io, .{ .layout = layout, .heap_base = sb.heap.start, .budget_bytes = opts.cache_budget_bytes }, be.dev.loader());
        errdefer be.cache.deinit();
        be.cache.setDurableTail(mounted.journal_tail_seq);
        var trees: std.ArrayList(*KvTree) = .empty;
        errdefer {
            for (trees.items) |t| t.destroy();
            trees.deinit(gpa);
        }
        for (be.mounted.tree_roots) |r| {
            const t = KvTree.openFromLedger(gpa, be.cache, &be.mounted, r.tree_id, &be.seq) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.Io => return error.Io,
                else => return error.Corrupt,
            };
            errdefer t.destroy();
            be.seq.raiseTo(r.node_seq);
            try trees.append(gpa, t);
        }
        be.trees = try trees.toOwnedSlice(gpa);
        errdefer {
            for (be.trees) |t| t.destroy();
            gpa.free(be.trees);
        }
        _ = tree.replayEntries(gpa, be.trees, recovery.entries) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
            else => return error.Corrupt,
        };
        var content_records: u64 = 0;
        for (recovery.entries) |e| {
            var it = e.records();
            while (it.next()) |f| {
                const t = journal.untag(f.tag);
                if (t.level == 0 and key.isSlotTreeKind(t.tree_id)) content_records += 1;
            }
        }
        be.replay.content_records = content_records;
        {
            var roots_buf: [8]u64 = undefined;
            var n: usize = 0;
            for (be.trees) |t| {
                if (n == roots_buf.len) break;
                roots_buf[n] = be.cache.addrExtent(t.rootPtr().addr);
                n += 1;
            }
            _ = try be.alloc.parkReplayedFrees(roots_buf[0..n]);
        }
        var fold: inos.RecoveryFold = .{};
        defer fold.deinit(gpa);
        fold.foldEntries(gpa, .flat, recovery.entries) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Corrupt => return error.Corrupt,
        };
        be.ino.installFloor(fold.nextIno(mounted.next_ino));

        // 8. The registry at the recovered head; the sector-pad alignment of
        // a head a torn window left mid-sector.
        be.reg = try jc.InflightRegistry.init(gpa, recovery.head_pos, conveyor.queueCapacity(geo.pages));
        errdefer be.reg.deinit();
        be.reg.recoverSeqOffset(journal.seqOffsetOfWindow(recovery.entries));
        try be.alignRingHead();

        // 9. The lane, the checkpointer, the bring-up cover, the conveyor,
        // the task thread.
        be.lane = journal_lane.JournalLane.spawn(gpa, opts.env, .{
            .path = be.path,
            .index = opts.index,
            .core = &be.core,
            .registry = &be.reg,
            .ring_stats = &be.ring_stats,
            .hooks = .{ .ctx = be, .rollback = laneRollbackHook, .checkpoint_past = laneCheckpointPastHook, .barrier_started = laneBarrierStartedHook, .barrier_landed = laneBarrierLandedHook },
            .test_error_logs = opts.test_error_logs,
        }) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => return error.LaneSpawnFailed,
        };
        errdefer be.lane.deinit();
        be.smo_journal = .{ .gpa = gpa, .layout = ring_layout, .geo = geo, .core = &be.core, .reg = &be.reg, .image = &.{}, .barrier_hook = be.dev.barrierSink(), .latch = &be.lane.latch, .device = .{ .ctx = be, .write = smoWriteHook, .completed = smoCompletedHook } };
        be.smo_journal.retire_seq.store(mounted.seq + 1, .release);
        be.smo_ctx = smo.SmoContext.withJournal(&be.alloc, &be.smo_journal);
        be.ckpt_seam = .{ .ctx = &be.smo_ctx, .trees = be.trees };
        be.flusher = .{ .cache = be.cache, .smo = be.ckpt_seam.seam() };
        const flush_ms = checkpoint.flushIntervalMsFromEnv(opts.env);
        be.cp = checkpoint_cycle.Checkpointer.init(gpa, .{
            .volume = be.path,
            .max_age_ms = checkpoint.maxAgeMsFromEnv(opts.env),
            .flush_interval_ms = flush_ms,
            .max_dirty_nodes = checkpoint.maxDirtyNodesFromEnv(opts.env, opts.cache_budget_bytes, sb.node_size),
            .checkpoint_seq = alloc_ext.resumeCheckpointSeq(mounted.seq, be.alloc.resumeGeneration()),
            .last_ledger_tail = mounted.journal_tail_seq,
        }, .{
            .core = &be.core,
            .inflight = &be.reg,
            .cache = be.cache,
            .alloc = &be.alloc,
            .latch = &be.lane.latch,
            .flush = be.flusher.sink(),
            .barrier = be.dev.barrierSink(),
            .ledger = .{ .ctx = be, .writeSlot = ledgerSlotHook },
            .pages = be.dev.pageSink(),
            .record = .{ .ctx = be, .treeRoots = treeRootsFn, .next_ino = &be.ino, .node_seq = &be.seq, .membership_stamp = be.mounted.membership_stamp },
            .smo = be.ckpt_seam.seam(),
            .maintenance = .{ .ctx = be, .pop = popMaintenance, .pending = maintenancePending, .requeue = requeueMaintenance },
        });
        errdefer be.cp.deinit();
        be.smo_ctx.space_retry = be.cp.spaceRetryHold();
        be.cp.space_wake = .{ .notify = &be.space, .io = io };
        be.task = checkpoint_cycle.CheckpointTask.init(&be.cp, io);
        be.task.serial = &be.smo_lock;
        be.cache.installCheckpointKick(be.task.hook());
        // The bring-up cover: the volume serves with an empty replay window
        // and zero reclaimable residue (the D1.b wedge-crumb class).
        be.cp.coverBringUpResidue() catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
            else => return error.Corrupt,
        };
        const cpus: u32 = if (opts.cpus != 0) opts.cpus else derive.Facts.probe().cpus;
        be.cv = try conveyor.Conveyor.init(gpa, .{
            .io = io,
            .gpa = gpa,
            .volume = be.path,
            .trees = be.trees,
            .cache = be.cache,
            .alloc = &be.alloc,
            .core = &be.core,
            .reg = &be.reg,
            .ring_layout = ring_layout,
            .geo = geo,
            .writer_id = journal.ROOT_AUTHORITY_WRITER,
            .latch = &be.lane.latch,
            .lane = .{ .ctx = be, .submit = laneSubmit, .barrier = laneBarrier },
            .checkpoint = .{ .ctx = be, .kickPark = kickPark, .kickThreshold = kickThreshold, .checkpointNow = checkpointNowHook, .checkpointPast = checkpointPastHook, .space = &be.space, .posture = &be.cp.posture, .space_retry = be.cp.spaceRetryHold() },
            .smo_journal = &be.smo_journal,
            .caps = conveyor.resolveCaps(opts.env, cpus, geo),
            .park_escalation_ns = conveyor.resolveParkEscalationNs(opts.env),
            .framing = .flat,
        });
        errdefer be.cv.deinit();
        be.ckpt_thread = std.Thread.spawn(.{}, ckptMain, .{be}) catch return error.ThreadSpawnFailed;
        be.replay.replay_ns = clock.monotonicNs() - t0;
        stats.mounts.inc();
        log.info("meta volume {s}: mounted — ledger seq {d}, tail {d}, replayed {d} entries ({d} content records, {d} torn dropped{s}), next_ino {d}, {d} trees, {d}/{d} extents free, {s} cadence", .{ be.path, mounted.seq, mounted.journal_tail_seq, be.replay.entries, be.replay.content_records, be.replay.dropped_torn, if (clean_unmount) ", clean unmount" else "", be.ino.snapshot(), be.trees.len, be.alloc.freeExtents(), total_extents, if (flush_ms == 0) "strict" else "coalesced" });
        return be;
    }

    fn ckptMain(be: *KvBackend) void {
        be.task.runUntilStopped();
    }

    /// Layer A of the D0 guard: `flock(LOCK_EX | LOCK_NB)` on a dedicated
    /// fd opened for the mount's life (a pool-cached fd would drop the lock
    /// at LRU eviction).
    fn acquireWriterFlock(path: []const u8) OpenError!linux.fd_t {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = align_probe.pathZ(&zbuf, path) catch return error.DeviceRefused;
        const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) {
            log.err("meta volume {s}: cannot open for the writer guard ({t})", .{ path, linux.errno(rc) });
            return error.DeviceRefused;
        }
        const fd: linux.fd_t = @intCast(rc);
        errdefer _ = linux.close(fd);
        const op: i32 = std.posix.LOCK.EX | std.posix.LOCK.NB;
        switch (linux.errno(linux.flock(fd, op))) {
            .SUCCESS => return fd,
            .AGAIN => {
                stats.guard_busy_refusals.inc();
                log.err("meta volume {s}: another writer holds the single-writer guard (flock) — refusing the mount (EBUSY)", .{path});
                return error.Busy;
            },
            else => |e| {
                log.err("meta volume {s}: flock failed ({t})", .{ path, e });
                return error.DeviceRefused;
            },
        }
    }

    /// A head a torn window left mid-sector gets its PAD entry (one honest
    /// read-modify-write at open, nothing in flight) so the first
    /// reservation begins aligned.
    fn alignRingHead(be: *KvBackend) OpenError!void {
        var plan = (journal.planHeadAlignment(be.gpa, &be.core, &be.reg, be.ring_layout, journal.ROOT_AUTHORITY_WRITER) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.JournalReserveExhausted => {
                log.err("meta volume {s}: the recovered journal head stands mid-sector and the ring admits not even its pad — refusing the mount (the checkpoint-cycle deferral of the pad lands with the covering loop)", .{be.path});
                return error.Corrupt;
            },
            else => return error.Corrupt,
        }) orelse return;
        defer plan.deinit();
        var landed = true;
        for (plan.spans) |s| {
            const len: usize = @intCast(s.hi - s.lo);
            const img = be.dev.readRange(be.gpa, s.lo, len) catch {
                landed = false;
                break;
            };
            defer be.gpa.free(img);
            journal.overlayOps(img, s.lo, plan.ops);
            be.pool.writeAt(be.path, s.lo, img) catch {
                landed = false;
                break;
            };
        }
        if (landed and plan.spans.len == 0) {
            be.dev.writeOps(plan.ops) catch {
                landed = false;
            };
        }
        if (landed) be.dev.barrier() catch {
            landed = false;
        };
        journal.finishHeadAlignment(&plan, &be.core, &be.reg, &be.ring_stats, landed);
        if (!landed) return error.Io;
    }

    fn allocReplayEntries(arena: Allocator, entries: []const journal.ReplayedEntry) Allocator.Error![]alloc_ext.ReplayEntry {
        const out = try arena.alloc(alloc_ext.ReplayEntry, entries.len);
        for (entries, out) |e, *o| {
            var count: usize = 0;
            var it = e.records();
            while (it.next()) |_| count += 1;
            const recs = try arena.alloc(alloc_ext.ReplayRecord, count);
            var it2 = e.records();
            var i: usize = 0;
            while (it2.next()) |f| : (i += 1) recs[i] = .{ .tree_id = journal.untag(f.tag).tree_id, .key = f.key, .value = f.value };
            o.* = .{ .seq = e.seq, .records = recs };
        }
        return out;
    }

    // ---- the hooks the components call back ----

    fn treeRootsFn(ctx: *anyopaque, gpa: Allocator, out: *std.ArrayList(ledger.TreeRoot)) Allocator.Error!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        for (be.trees) |t| try out.append(gpa, t.ledgerRoot());
    }

    /// The threshold drain's source: the trees' queues, round-robin from
    /// the tree after the last pop so a storm on one tree never starves
    /// another's items (every queue is lock-free; the task is the one popper).
    fn popMaintenance(ctx: *anyopaque) ?u64 {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const n = be.trees.len;
        for (0..n) |_| {
            be.maintenance_cursor = (be.maintenance_cursor + 1) % n;
            if (be.trees[be.maintenance_cursor].popMaintenance()) |addr| return addr;
        }
        return null;
    }

    fn maintenancePending(ctx: *anyopaque) bool {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        for (be.trees) |t| if (t.maintenancePending()) return true;
        return false;
    }

    /// A deferred item goes back on its owner's queue; a node the cache no
    /// longer maps has nothing left to maintain.
    fn requeueMaintenance(ctx: *anyopaque, addr: u64) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const n = be.cache.tryGet(addr) orelse return;
        defer n.release();
        for (be.trees) |t| {
            if (t.owns(n)) {
                t.enqueueMaintenance(addr);
                return;
            }
        }
    }

    /// The ledger slot through the device, with the kill matrix's C9/C10
    /// cut points around it.
    fn ledgerSlotHook(ctx: *anyopaque, slot: u64, image: *const [ledger.ROOT_LEDGER_SLOT_LEN]u8) checkpoint_cycle.IoError!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        if (test_seams.before_ledger) |h| h.call(h.ctx);
        const sink = be.dev.ledgerSink();
        try sink.writeSlot(sink.ctx, slot, image);
        if (test_seams.after_ledger) |h| h.call(h.ctx);
    }

    fn smoWriteHook(ctx: *anyopaque, ops: []const journal.WriteOp) error{Io}!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        if (test_seams.before_smo_entry) |h| h.call(h.ctx);
        return be.dev.writeOps(ops);
    }

    /// A reservation completed outside the lane: kick it (a stage-B prefix
    /// wait may sit behind it).
    fn smoCompletedHook(ctx: *anyopaque) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.lane.kick();
    }

    fn laneRollbackHook(ctx: ?*anyopaque, span: jc.SeqSpan, window: jc.Reservation) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx.?));
        be.cv.rollbackHook(span, window);
    }

    fn laneCheckpointPastHook(ctx: ?*anyopaque, padded_end: u64) bool {
        const be: *KvBackend = @ptrCast(@alignCast(ctx.?));
        return be.checkpointPast(padded_end);
    }

    /// DUR-3 on the lane's barriers (coalesced, strict, fsync): the same
    /// tickets and the same drain as the checkpointer's own `syncDevice`.
    fn laneBarrierStartedHook(ctx: ?*anyopaque) u64 {
        const be: *KvBackend = @ptrCast(@alignCast(ctx.?));
        return be.cp.barrierStarted();
    }

    fn laneBarrierLandedHook(ctx: ?*anyopaque, ticket: u64) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx.?));
        be.cp.barrierLanded(ticket);
    }

    fn mapSubmitError(e: journal_lane.Error) Error {
        return switch (e) {
            error.VolumeFailed => error.VolumeFailed,
            error.LaneStopped => error.VolumeClosed,
            error.LaneQueueFull => error.ConveyorFull,
            error.OutOfMemory => error.OutOfMemory,
            error.HoleCheckpointFailed => error.HoleCheckpointFailed,
            else => error.Corrupt,
        };
    }

    fn mapVerdict(e: journal_lane.Error) Error {
        return switch (e) {
            error.VolumeFailed => error.VolumeFailed,
            error.LaneStopped => error.VolumeClosed,
            error.HoleCheckpointFailed => error.HoleCheckpointFailed,
            error.OutOfMemory => error.OutOfMemory,
            else => error.JournalWriteFailed,
        };
    }

    fn laneSubmit(ctx: *anyopaque, w: *conveyor.Window) Error!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.lane.submitWriteBatchDetached(.{
            .window = w.res,
            .seq_base = w.seq_base,
            .ops = w.ops(),
            .entries = w.entries,
            .bytes = w.bytes,
            .apply_holes = w.applyHoles(),
        }, .{ .ctx = w, .call = windowDone }) catch |e| return mapSubmitError(e);
    }

    /// On the lane thread at the ack instant: the window's terminal outcome.
    fn windowDone(ctx: *anyopaque, outcome: journal_lane.Outcome) void {
        const w: *conveyor.Window = @ptrCast(@alignCast(ctx));
        w.finish(if (outcome.result) |_| {} else |e| mapVerdict(e));
    }

    fn laneBarrier(ctx: *anyopaque) Error!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const t = be.lane.requestBarrier() catch |e| return mapSubmitError(e);
        const o = t.wait();
        o.result catch |e| return mapVerdict(e);
    }

    fn kickPark(ctx: *anyopaque) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.task.kick(.ring_park);
    }

    fn kickThreshold(ctx: *anyopaque) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.task.kick(.threshold);
    }

    fn checkpointNowHook(ctx: *anyopaque) bool {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.checkpointNow() catch return false;
        return true;
    }

    fn checkpointPastHook(ctx: *anyopaque, pos: u64) bool {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        return be.checkpointPast(pos);
    }

    /// Checkpoint past a journal hole under the SMO lock (the lane's hole
    /// discipline; the conveyor's abandon arm).
    fn checkpointPast(be: *KvBackend, pos: u64) bool {
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        be.cp.checkpointPast(pos) catch return false;
        return true;
    }

    // ---- the API ----

    /// A fresh transaction (`site` = the committer's `@src()`); `commit`
    /// consumes it.
    pub fn begin(be: *KvBackend, site: tx.Site) Allocator.Error!*KvTx {
        const t = try be.gpa.create(KvTx);
        t.* = KvTx.init(be.gpa, site);
        return t;
    }

    /// Abandon a transaction without committing (its guards and bytes go).
    pub fn abandon(be: *KvBackend, t: *KvTx) void {
        t.deinit();
        be.gpa.destroy(t);
    }

    pub fn commit(be: *KvBackend, t: *KvTx) Error!void {
        defer be.gpa.destroy(t);
        return be.cv.commit(t);
    }

    pub fn commitGroup(be: *KvBackend, txs: []const *KvTx, results: []Outcome) void {
        be.cv.commitGroup(txs, results);
        for (txs) |t| be.gpa.destroy(t);
    }

    pub fn treeOf(be: *KvBackend, tree_id: u8) ?*KvTree {
        for (be.trees) |t| if (t.tree_id == tree_id) return t;
        return null;
    }

    /// Point lookup through the tree (latch-free); an owned copy.
    pub fn lookup(be: *KvBackend, gpa: Allocator, tree_id: u8, k: []const u8) Error!?[]u8 {
        const t = be.treeOf(tree_id) orelse return error.Corrupt;
        return t.lookup(gpa, k) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    pub fn range(be: *KvBackend, gpa: Allocator, tree_id: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
        const t = be.treeOf(tree_id) orelse return error.Corrupt;
        t.range(gpa, start, end, max, out) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    /// `fsync`: the trailing coalesced barrier (every acked entry durable on
    /// return).
    pub fn fsync(be: *KvBackend) Error!void {
        return be.cv.barrier();
    }

    /// One full cycle with an immediate barrier.
    pub fn checkpointNow(be: *KvBackend) checkpoint_cycle.CycleError!void {
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        return be.cp.checkpointNow();
    }

    pub fn allocateIno(be: *KvBackend) u64 {
        return be.ino.allocate();
    }

    pub fn nextIno(be: *const KvBackend) u64 {
        return be.ino.snapshot();
    }

    pub fn hashSeed(be: *const KvBackend) u64 {
        return be.sb.hash_seed;
    }

    pub fn isFailed(be: *const KvBackend) bool {
        return be.lane.latch.isFailed();
    }

    pub fn replayStats(be: *const KvBackend) ReplayStats {
        return be.replay;
    }

    pub fn conveyorStats(be: *const KvBackend) *const conveyor.Stats {
        return &be.cv.stats;
    }

    pub fn laneSnapshot(be: *const KvBackend) journal_lane.LaneSnapshot {
        return be.lane.snapshot();
    }

    /// The clean dismount (module doc). Idempotent; frees the backend.
    pub fn close(be: *KvBackend) void {
        const gpa = be.gpa;
        if (!be.closed) {
            be.closed = true;
            be.cv.close();
            be.cv.quiesce();
            // The lane first: its windows are answered and its trailing
            // barrier runs before the task's final tick, which asserts the
            // registry quiet (`checkpoint_final_tick_open_reservations`)
            // rather than waiting on a completer that no longer exists.
            be.lane.shutdown();
            be.task.stop();
            if (be.ckpt_thread) |th| {
                th.join();
                be.ckpt_thread = null;
            }
            be.dev.barrier() catch {};
            stats.clean_unmounts.inc();
            log.info("meta volume {s}: clean unmount (checkpoint seq {d}, tail {d}, head {d})", .{ be.path, be.cp.checkpointSeq(), be.cp.lastLedgerTail(), be.core.headPos() });
        }
        be.cv.deinit();
        be.lane.deinit();
        be.cp.deinit();
        for (be.trees) |t| t.destroy();
        gpa.free(be.trees);
        be.cache.deinit();
        be.alloc.deinit();
        be.reg.deinit();
        be.mounted.deinit(gpa);
        if (be.pool_owned) be.pool.deinit();
        _ = linux.close(be.guard_fd);
        gpa.free(be.path);
        gpa.destroy(be);
    }
};

// ---------------------------------------------------------------------------
// Tests (Linux; regular files on the venue's ext4 under /build/tmp, O_DIRECT;
// the reference `fsck --offline` as the oracle when present).
// ---------------------------------------------------------------------------

const testing = std.testing;
const economy = @import("economy.zig");

const test_dir = "/build/tmp";
const ref_binary = "/build/ref/squeezefs";
const test_volume_len: u64 = 64 << 20;
/// The data image the format config names (the reference's fsck resolves
/// `data_lv` from the config; an existing file keeps it quiet).
const test_data_image = "/build/tmp/ref-sw-data.img";

const TestVol = struct {
    gpa: Allocator,
    env: std.process.Environ.Map,
    path_buf: [128]u8 = undefined,
    path_len: usize = 0,
    config_buf: [512]u8 = undefined,
    config_len: usize = 0,
    /// Error logs the lane thread will emit (the FAILED latch's one line).
    lane_error_logs: usize = 0,

    fn path(self: *const TestVol) []const u8 {
        return self.path_buf[0..self.path_len];
    }

    /// A fresh `--single-writer` volume formatted through the device path.
    fn create(gpa: Allocator, name: []const u8, flush_ms: ?[]const u8) !*TestVol {
        return createWithRing(gpa, name, flush_ms, 1 << 20);
    }

    fn createWithRing(gpa: Allocator, name: []const u8, flush_ms: ?[]const u8, journal_len: u64) !*TestVol {
        const self = try gpa.create(TestVol);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .env = std.process.Environ.Map.init(gpa) };
        errdefer self.env.deinit();
        if (flush_ms) |v| try self.env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", v);
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, test_dir), 0o755);
        self.path_len = (try std.fmt.bufPrint(&self.path_buf, "{s}/sqz-be-{d}-{s}.img", .{ test_dir, linux.getpid(), name })).len;
        try truncateFile(self.path(), test_volume_len);
        self.config_len = (try std.fmt.bufPrint(&self.config_buf, "{{\"name\":\"squeezefs\",\"block_size\":4194304,\"capacity\":67108864,\"inodes\":1000000,\"compression\":\"none\",\"encrypt_algo\":\"none\",\"data_lv\":[\"{s}\"],\"upload_delay\":\"500ms\",\"meta_routing_width\":65536,\"meta_slot_runs\":[[[0,1,65536]]],\"meta_volumes\":[{{\"id\":\"vol-{x}\",\"backing_dev\":\"{s}\",\"member_position\":0,\"added_ts\":1791205148}}]}}", .{ test_data_image, @as(u32, @intCast(linux.getpid())), self.path() })).len;
        try formatVolume(gpa, self.path(), .{
            .env = &self.env,
            .node_size = 65536,
            .journal_len_override = journal_len,
            .uuid = "kv-backend-uuid!".*,
            .set_uuid = "kv-backend-set!!".*,
            .format_config_xattr = self.config_buf[0..self.config_len],
        });
        return self;
    }

    fn destroy(self: *TestVol) void {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        if (align_probe.pathZ(&zbuf, self.path())) |z| {
            _ = linux.unlink(z);
        } else |_| {}
        self.env.deinit();
        self.gpa.destroy(self);
    }

    fn open(self: *TestVol) !*KvBackend {
        return KvBackend.open(self.gpa, testing.io, self.path(), .{ .env = &self.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .test_error_logs = self.lane_error_logs });
    }
};

fn truncateFile(path: []const u8, len: u64) !void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = try align_probe.pathZ(&zbuf, path);
    const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.CannotSizeDevice;
}

fn unlinkPath(path: []const u8) void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    if (align_probe.pathZ(&zbuf, path)) |z| {
        _ = linux.unlink(z);
    } else |_| {}
}

fn fileExists(path: []const u8) bool {
    return (align_probe.statPath(path) catch null) != null;
}

/// The oracle: `fsck --offline` on the image; null when the reference is
/// absent (the test skips).
fn fsckClean(gpa: Allocator, path: []const u8) !?bool {
    if (!fileExists(ref_binary)) return null;
    var uri_buf: [256]u8 = undefined;
    const uri = try std.fmt.bufPrint(&uri_buf, "sqmeta://{s}", .{path});
    const r = try std.process.run(gpa, testing.io, .{ .argv = &.{ ref_binary, "fsck", "--offline", uri }, .cwd = .{ .path = test_dir } });
    defer gpa.free(r.stdout);
    defer gpa.free(r.stderr);
    unlinkPath(test_dir ++ "/dhat-heap.json");
    const clean = r.term.success() and std.mem.indexOf(u8, r.stdout, "findings: 0 (clean)") != null;
    if (!clean) std.debug.print("\nfsck {s}: term {f}\n--- stdout ---\n{s}\n--- stderr (tail) ---\n{s}\n", .{ path, r.term, r.stdout, r.stderr[r.stderr.len -| 2048..] });
    return clean;
}

/// One create under the root through `economy.stageCreate`; the ino.
fn stageCreate(be: *KvBackend, t: *KvTx, name: []const u8, value_len: usize) !u64 {
    const ino = be.allocateIno();
    const parent_value: record.InodeValue = .{ .mode = economy.S.IFDIR | 0o755, .nlink = 2 };
    // The dentry names the GLOBAL ino (charter §5.4: `(local − 2)·W + slot
    // + 2`, slot 0 on the single volume) — what the reference fsck resolves.
    _ = try economy.stageCreate(t, .{ .hash_seed = be.hashSeed(), .now = 1000, .parent = 1, .parent_value = parent_value, .name = name, .mode = economy.S.IFREG | 0o644, .uid = 0, .gid = 0, .local_ino = ino, .global_ino = inos.makeGlobalIno(ino, 0, inos.ROUTING_WIDTH) });
    if (value_len > 0) {
        const v = try be.gpa.alloc(u8, value_len);
        defer be.gpa.free(v);
        @memset(v, 0x5A);
        var xb: [8192]u8 = undefined;
        const xv = try (record.XattrValue{ .name = "user.pad", .value = v }).encodeInto(&xb);
        try t.stagePut(key.TREE_XATTRS, &key.xattrKey(ino, key.xattrNameHash56("user.pad", be.hashSeed()), 0), xv);
    }
    return ino;
}

fn commitCreate(be: *KvBackend, name: []const u8) !u64 {
    const t = try be.begin(@src());
    const ino = try stageCreate(be, t, name, 0);
    try be.commit(t);
    return ino;
}

fn present(be: *KvBackend, ino: u64) !bool {
    const got = try be.lookup(testing.allocator, key.TREE_INODES, &key.inodeKey(ino));
    defer if (got) |g| testing.allocator.free(g);
    return got != null;
}

fn dentryPresent(be: *KvBackend, name: []const u8) !bool {
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(testing.allocator, &kvs);
    const r = tx.dentryChainRange(1, key.dentryNameHash54(name, be.hashSeed()));
    try be.range(testing.allocator, key.TREE_DENTRIES, &r.start, &r.end, 8, &kvs);
    return kvs.items.len != 0;
}

test "format (device path) → open → N creates (singles + a group of 8) → lookup each → checkpointNow → close → reopen: every record present, a clean unmount, fsck clean" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "e2e", null);
    defer v.destroy();
    if (try fsckClean(gpa, v.path())) |clean| try testing.expect(clean);
    var acked: [24]u64 = undefined;
    {
        const be = try v.open();
        try testing.expect(be.replayStats().clean_unmount);
        try testing.expectEqual(@as(u64, 0), be.replayStats().entries);
        var i: usize = 0;
        while (i < 16) : (i += 1) {
            var nb: [16]u8 = undefined;
            acked[i] = try commitCreate(be, try std.fmt.bufPrint(&nb, "single-{d}", .{i}));
        }
        var txs: [8]*KvTx = undefined;
        var names: [8][16]u8 = undefined;
        for (&txs, &names, 0..) |*t, *nb, k| {
            t.* = try be.begin(@src());
            acked[16 + k] = try stageCreate(be, t.*, try std.fmt.bufPrint(nb, "group-{d}", .{k}), 0);
        }
        var results: [8]Outcome = undefined;
        be.commitGroup(&txs, &results);
        for (results) |r| try r;
        for (acked) |ino| try testing.expect(try present(be, ino));
        // The engagement identities: lane writes ≡ handoffs; passes ≡ Σ
        // group sizes; acks ≡ commits − refusals; nothing in flight.
        const cs = be.conveyorStats();
        try testing.expectEqual(cs.lane_handoffs.load(), be.laneSnapshot().lane_writes);
        try testing.expectEqual(cs.leader_passes.load(), cs.group_size.total());
        try testing.expectEqual(@as(u64, 24), cs.acks.load());
        try testing.expectEqual(@as(u64, 0), cs.refusals.load());
        try testing.expectEqual(@as(u64, 0), cs.windows_inflight.load());
        try testing.expectEqual(@as(u64, 0), cs.queued.load());
        try testing.expect(cs.group_size.load(7) >= 1);
        try be.checkpointNow();
        try testing.expectEqual(be.core.headPos(), be.core.reusableUpto());
        be.close();
    }
    {
        const be = try v.open();
        defer be.close();
        const rs = be.replayStats();
        try testing.expect(rs.clean_unmount);
        try testing.expectEqual(@as(u64, 0), rs.dropped_torn);
        try testing.expectEqual(@as(u64, 0), rs.content_records);
        for (acked) |ino| try testing.expect(try present(be, ino));
        try testing.expect(be.nextIno() > acked[23]);
        try testing.expect(try dentryPresent(be, "group-7"));
    }
    if (try fsckClean(gpa, v.path())) |clean| try testing.expect(clean);
}

test "the single-writer guard: a second open of a held volume refuses Busy loud; a closed volume opens again" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "guard", null);
    defer v.destroy();
    const be = try v.open();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Busy, v.open());
    try log.testing.expectAllFired();
    be.close();
    const again = try v.open();
    again.close();
}

test "strict vs coalesced: a strict ack follows one barrier per group; a coalesced ack precedes the barrier and fsync is the trailing one" {
    const gpa = testing.allocator;
    const s = try TestVol.create(gpa, "strict", "0");
    defer s.destroy();
    {
        const be = try s.open();
        defer be.close();
        const b0 = be.laneSnapshot().barriers_strict;
        _ = try commitCreate(be, "a");
        try testing.expectEqual(b0 + 1, be.laneSnapshot().barriers_strict);
        _ = try commitCreate(be, "b");
        try testing.expectEqual(b0 + 2, be.laneSnapshot().barriers_strict);
    }
    const c = try TestVol.create(gpa, "coalesced", "600000");
    defer c.destroy();
    {
        const be = try c.open();
        defer be.close();
        _ = try commitCreate(be, "a");
        try testing.expectEqual(@as(u64, 0), be.laneSnapshot().barriers_strict);
        try testing.expectEqual(@as(u64, 0), be.laneSnapshot().barriers_coalesced);
        try be.fsync();
        try testing.expectEqual(@as(u64, 1), be.laneSnapshot().barriers_coalesced);
    }
}

test "DUR-3 on the lane's barriers: a cadence cycle's record (no immediate barrier) becomes reusable_upto when the lane's coalesced barrier lands, with no checkpointer barrier involved" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "lane-dur3", "600000");
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    _ = try commitCreate(be, "a");
    // One cadence-shaped cycle: barrier #1, the record, NO barrier #2 — the
    // record's durability rides the next barrier on the device, whoever
    // issues it.
    be.smo_lock.lockUncancelable(be.io);
    be.cp.checkpointCadence() catch |e| {
        be.smo_lock.unlock(be.io);
        return e;
    };
    be.smo_lock.unlock(be.io);
    const tail = be.cp.lastLedgerTail();
    try testing.expect(tail > 0);
    try testing.expect(be.core.reusableUpto() < tail);
    try testing.expectEqual(@as(usize, 1), be.cp.reclaim.len());
    const syncs0 = checkpoint.stats.meta_device_syncs.load();
    // The lane's trailing coalesced barrier (fsync): the ONE barrier stream
    // the ring reclamation, the allocator's coverage clock and the cache's
    // elision tail all move on.
    try be.fsync();
    try testing.expectEqual(@as(u64, 1), be.laneSnapshot().barriers_coalesced);
    try testing.expectEqual(syncs0, checkpoint.stats.meta_device_syncs.load());
    try testing.expectEqual(tail, be.core.reusableUpto());
    try testing.expectEqual(tail, be.cache.durableTail());
    try testing.expectEqual(@as(usize, 0), be.cp.reclaim.len());
    try testing.expectEqual(tail, be.cp.reusable_upto.load());
}

test "rollback on the device: a synthetically failed window answers JournalWriteFailed and is rolled back, the volume stays open; three in a row latch FAILED and later commits are refused" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "rollback", "600000");
    defer v.destroy();
    v.lane_error_logs = 1;
    const be = try v.open();
    defer be.close();
    const base = try commitCreate(be, "base");
    journal_lane.test_seams.fail_windows.store(1, .release);
    try testing.expectError(error.JournalWriteFailed, commitCreate(be, "victim"));
    try testing.expect(!try present(be, be.nextIno() - 1));
    try testing.expect(try present(be, base));
    try testing.expect(!be.isFailed());
    const after = try commitCreate(be, "after");
    try testing.expect(try present(be, after));
    try testing.expectEqual(@as(u64, 1), be.conveyorStats().rollbacks.load());
    // RAM ≡ replay: a probe of the device's bytes folds to the live state.
    try be.checkpointNow();
    journal_lane.test_seams.fail_windows.store(3, .release);
    defer journal_lane.test_seams.fail_windows.store(0, .release);
    for (0..3) |i| {
        var nb: [8]u8 = undefined;
        try testing.expectError(error.JournalWriteFailed, commitCreate(be, try std.fmt.bufPrint(&nb, "f{d}", .{i})));
    }
    try testing.expect(be.isFailed());
    try testing.expectError(error.VolumeFailed, commitCreate(be, "post"));
}

/// Snapshot the device image to `dst` (a power cut: the device's landed
/// bytes, nothing in any cache).
fn snapshotImage(src: []const u8, dst: []const u8) !void {
    try std.Io.Dir.copyFileAbsolute(src, dst, testing.io, .{});
}

const CutCtx = struct {
    src: []const u8,
    dst: []const u8,
    fired: std.atomic.Value(u32) = .init(0),

    fn hook(self: *CutCtx) node_cache.Hook {
        return .{ .ctx = self, .call = call };
    }

    fn call(ctx: *anyopaque) void {
        const self: *CutCtx = @ptrCast(@alignCast(ctx));
        if (self.fired.swap(1, .acq_rel) != 0) return;
        snapshotImage(self.src, self.dst) catch |e| {
            log.warn("cut snapshot failed: {s}", .{@errorName(e)});
        };
    }
};

const Cut = enum { after_ack_before_barrier, before_ledger, after_ledger, before_smo_entry };

/// One row of the kill matrix: the cut's image reopened — every acked
/// (coalesced: landed) tx present whole, no partial tx, fsck clean.
fn killMatrixRow(gpa: Allocator, cut: Cut) !void {
    var name_buf: [32]u8 = undefined;
    const name = try std.fmt.bufPrint(&name_buf, "cut-{s}", .{@tagName(cut)});
    const v = try TestVol.create(gpa, name, "600000");
    defer v.destroy();
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{v.path()});
    defer unlinkPath(dst);
    var ctx: CutCtx = .{ .src = v.path(), .dst = dst };
    defer test_seams = .{};
    var acked: std.ArrayList(u64) = .empty;
    defer acked.deinit(gpa);
    var names: std.ArrayList([]u8) = .empty;
    defer {
        for (names.items) |n| gpa.free(n);
        names.deinit(gpa);
    }
    var minted_max: u64 = 0;
    {
        const be = try v.open();
        // Durable baseline, then the window of work the cut falls in.
        for (0..8) |i| {
            const n = try std.fmt.allocPrint(gpa, "base-{d}", .{i});
            try names.append(gpa, n);
            try acked.append(gpa, try commitCreate(be, n));
        }
        try be.checkpointNow();
        // Armed AFTER the durable baseline: the cut falls inside the window
        // of work that follows.
        switch (cut) {
            .before_ledger => test_seams.before_ledger = ctx.hook(),
            .after_ledger => test_seams.after_ledger = ctx.hook(),
            .before_smo_entry => test_seams.before_smo_entry = ctx.hook(),
            .after_ack_before_barrier => {},
        }
        switch (cut) {
            .after_ack_before_barrier => {
                for (0..12) |i| {
                    const n = try std.fmt.allocPrint(gpa, "win-{d}", .{i});
                    try names.append(gpa, n);
                    try acked.append(gpa, try commitCreate(be, n));
                }
                minted_max = be.nextIno() - 1;
                try snapshotImage(v.path(), dst);
            },
            .before_ledger, .after_ledger => {
                for (0..12) |i| {
                    const n = try std.fmt.allocPrint(gpa, "win-{d}", .{i});
                    try names.append(gpa, n);
                    try acked.append(gpa, try commitCreate(be, n));
                }
                minted_max = be.nextIno() - 1;
                try be.checkpointNow();
                try testing.expectEqual(@as(u32, 1), ctx.fired.load(.acquire));
            },
            .before_smo_entry => {
                // Overflow the xattr leaf (64 KiB nodes): ~20 × 4 KiB values
                // force a split at the flush.
                for (0..24) |i| {
                    const n = try std.fmt.allocPrint(gpa, "big-{d}", .{i});
                    try names.append(gpa, n);
                    const t = try be.begin(@src());
                    try acked.append(gpa, try stageCreate(be, t, n, 4000));
                    try be.commit(t);
                }
                minted_max = be.nextIno() - 1;
                try be.checkpointNow();
                try testing.expectEqual(@as(u32, 1), ctx.fired.load(.acquire));
            },
        }
        // The live mount goes on (the kill is the copy); close it cleanly so
        // the live image is itself consistent.
        be.close();
    }
    // Reopen the CUT image.
    const copy = try KvBackend.open(gpa, testing.io, dst, .{ .env = &v.env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
    defer copy.close();
    for (acked.items, names.items) |ino, n| {
        try testing.expect(try present(copy, ino));
        try testing.expect(try dentryPresent(copy, n));
    }
    try testing.expect(copy.nextIno() > minted_max);
    // A partial tx never shows: every dentry's child inode is present.
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    const lo = key.dentryKey(1, 0, 0);
    const hi = key.dentryKey(1, std.math.maxInt(u64) >> 10, std.math.maxInt(u8));
    try copy.range(gpa, key.TREE_DENTRIES, &lo, &hi, 1024, &kvs);
    try testing.expectEqual(acked.items.len, kvs.items.len);
    for (kvs.items) |kv| {
        const dv = try record.DentryValue.decode(kv.value);
        try testing.expect(try present(copy, inos.routeIno(dv.child_ino, inos.ROUTING_WIDTH).local));
    }
    const verdict = try fsckClean(gpa, dst);
    if (verdict) |clean| try testing.expect(clean);
    std.debug.print("\nkill matrix: cut {s} → reopened {d} acked txs whole, next_ino {d} > {d}, replay entries {d}, torn dropped {d}; fsck {s}\n", .{ @tagName(cut), acked.items.len, copy.nextIno(), minted_max, copy.replayStats().entries, copy.replayStats().dropped_torn, if (verdict) |c| (if (c) "clean" else "FINDINGS") else "absent" });
}

test "kill matrix: after ack before the barrier (coalesced) — the landed entries replay whole" {
    try killMatrixRow(testing.allocator, .after_ack_before_barrier);
}

test "kill matrix: mid-checkpoint before the ledger write — the previous record is selected, the window replays whole" {
    try killMatrixRow(testing.allocator, .before_ledger);
}

test "kill matrix: after the ledger write before reusable_upto advances — the new record is selected, nothing lost" {
    try killMatrixRow(testing.allocator, .after_ledger);
}

test "kill matrix: mid-SMO entry — the swap happened, the entry never landed; the tree routes consistently through the old structure" {
    try killMatrixRow(testing.allocator, .before_smo_entry);
}

test "4-thread commit storm with concurrent checkpoints, then reopen: every ack present, identities close at quiesce, fsck clean" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "storm", "20");
    defer v.destroy();
    const threads = 4;
    const per_thread = 60;
    var acked: [threads][per_thread]u64 = undefined;
    {
        const be = try v.open();
        const Storm = struct {
            fn run(b: *KvBackend, lane_id: usize, out: *[per_thread]u64) void {
                for (0..per_thread) |i| {
                    var nb: [24]u8 = undefined;
                    const n = std.fmt.bufPrint(&nb, "storm-{d}-{d}", .{ lane_id, i }) catch unreachable;
                    out[i] = commitCreate(b, n) catch unreachable;
                    if (i % 20 == 19) b.checkpointNow() catch unreachable;
                }
            }
        };
        var handles: [threads]std.Thread = undefined;
        for (&handles, 0..) |*h, i| h.* = try std.Thread.spawn(.{}, Storm.run, .{ be, i, &acked[i] });
        for (handles) |h| h.join();
        for (acked) |lane_acks| for (lane_acks) |ino| try testing.expect(try present(be, ino));
        const cs = be.conveyorStats();
        try testing.expectEqual(@as(u64, threads * per_thread), cs.acks.load());
        try testing.expectEqual(@as(u64, 0), cs.refusals.load());
        try testing.expectEqual(cs.drained_txs.load(), cs.acks.load() + cs.refusals.load());
        try testing.expectEqual(cs.leader_passes.load(), cs.group_size.total());
        try testing.expectEqual(cs.lane_handoffs.load(), be.laneSnapshot().lane_writes);
        try testing.expectEqual(@as(u64, 0), cs.windows_inflight.load());
        try testing.expectEqual(@as(u64, 0), cs.queued.load());
        try testing.expectEqual(@as(u64, 0), cs.commit_parked.load());
        try testing.expectEqual(@as(u64, 0), be.cache.heapPromised());
        std.debug.print("\nstorm: {d} commits in {d} passes (group-size buckets 1:{d} 2:{d} 3:{d} 4:{d} ≤8:{d} >8:{d}), windows hwm {d}, lane parks {d}\n", .{ threads * per_thread, cs.leader_passes.load(), cs.group_size.load(0), cs.group_size.load(1), cs.group_size.load(2), cs.group_size.load(3), cs.group_size.load(4) + cs.group_size.load(5) + cs.group_size.load(6) + cs.group_size.load(7), cs.group_size.load(8) + cs.group_size.load(9) + cs.group_size.load(10) + cs.group_size.load(11), cs.windows_inflight_hwm.load(), be.laneSnapshot().parks });
        be.close();
    }
    {
        const be = try v.open();
        defer be.close();
        try testing.expect(be.replayStats().clean_unmount);
        for (acked) |lane_acks| for (lane_acks) |ino| try testing.expect(try present(be, ino));
    }
    if (try fsckClean(gpa, v.path())) |clean| try testing.expect(clean);
}

test "threshold writeback on a mounted volume: commits that push one leaf over the writeback threshold are drained by the checkpoint task — the bset page is appended before any cadence cycle, the tree's queue empties, queue_full stays 0" {
    const gpa = testing.allocator;
    // A 600 s flush interval: no cadence tick and no lane barrier fires on
    // its own, so every append below is the threshold drain's.
    const v = try TestVol.create(gpa, "threshold", "600000");
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    const xattrs = be.treeOf(key.TREE_XATTRS).?;
    const items0 = checkpoint.stats.meta_kv_maintenance_items.load();
    const appends0 = device_loader.stats.appends.load();
    const checkpoints0 = checkpoint.stats.meta_kv_checkpoints.load();
    const queue_full0 = tree.stats.meta_kv_maintenance_queue_full.load();
    // One page of xattr value per create: the xattr leaf's open delta
    // crosses `writeback_delta_bytes` (one bset page) on every commit.
    for (0..3) |i| {
        var nb: [16]u8 = undefined;
        const t = try be.begin(@src());
        _ = try stageCreate(be, t, try std.fmt.bufPrint(&nb, "thr-{d}", .{i}), node_cache.DEFAULT_WRITEBACK_DELTA_BYTES);
        try be.commit(t);
    }
    // The task drains on its own thread; the bound is a failure, not a wait.
    const started = clock.monotonicNs();
    while (xattrs.maintenancePending() or checkpoint.stats.meta_kv_maintenance_items.load() == items0) {
        if (clock.monotonicNs() - started > 10 * std.time.ns_per_s) return error.ThresholdDrainNeverRan;
        std.Thread.yield() catch {};
    }
    try testing.expect(checkpoint.stats.meta_kv_maintenance_items.load() > items0);
    try testing.expect(device_loader.stats.appends.load() > appends0);
    try testing.expectEqual(checkpoints0, checkpoint.stats.meta_kv_checkpoints.load());
    try testing.expectEqual(queue_full0, tree.stats.meta_kv_maintenance_queue_full.load());
    for (be.trees) |t| try testing.expect(!t.maintenancePending());
}

test "the admission park on the device: the smallest ring fills under a serial create stream (every commit a grain-padded window), the pass parks holding no node lock, the park kick runs a barriered cycle, every commit lands" {
    const gpa = testing.allocator;
    // The smallest admissible ring (256 KiB + one max entry): its user
    // slice is ~128 KiB, a few hundred padded commits.
    const v = try TestVol.createWithRing(gpa, "park", "600000", journal.RING_OVERRIDE_FLOOR);
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    const park_cycles0 = checkpoint.stats.meta_kv_checkpoint_park_cycles.load();
    const cs = be.conveyorStats();
    var acked: std.ArrayList(u64) = .empty;
    defer acked.deinit(gpa);
    // Past the first park, a hundred more commits ride the reclaimed ring.
    var after_park: usize = 0;
    while (after_park < 100 and acked.items.len < 4000) {
        var nb: [16]u8 = undefined;
        try acked.append(gpa, try commitCreate(be, try std.fmt.bufPrint(&nb, "park-{d}", .{acked.items.len})));
        if (cs.full_stalls.load() > 0) after_park += 1;
    }
    for (acked.items) |ino| try testing.expect(try present(be, ino));
    try testing.expect(cs.full_stalls.load() > 0);
    // Every park resumed on the barrier's own wake, never the backstop tick.
    try testing.expectEqual(@as(u64, 0), cs.park_tick_rescues.load());
    try testing.expect(checkpoint.stats.meta_kv_checkpoint_park_cycles.load() > park_cycles0);
    try testing.expectEqual(acked.items.len, cs.acks.load());
    try testing.expectEqual(@as(u64, 0), cs.refusals.load());
    try testing.expect(!be.isFailed());
    std.debug.print("\nadmission park: {d} serial commits on a {d}-page ring (grain {d}) — {d} stalls, {d} park-kicked cycles\n", .{ acked.items.len, be.geo.pages, be.geo.grain, cs.full_stalls.load(), checkpoint.stats.meta_kv_checkpoint_park_cycles.load() - park_cycles0 });
}

// ---------------------------------------------------------------------------
// The kill -9 matrix: a re-exec'd child opens the volume and churns creates,
// appending `ack <ino>` to a side ledger (one write + fdatasync per line)
// after each commit; the parent SIGKILLs it mid-stream and reopens.
// ---------------------------------------------------------------------------

const crash_child_knob = "SQUEEZEFS_TEST_CRASH_CHILD";
const crash_child_test_name = "crash child: churn creates until killed";

fn appendLine(path: []const u8, line: []const u8) void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = align_probe.pathZ(&zbuf, path) catch return;
    const rc = linux.open(z, .{ .ACCMODE = .WRONLY, .CLOEXEC = true, .CREAT = true, .APPEND = true }, 0o644);
    if (linux.errno(rc) != .SUCCESS) return;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    _ = linux.write(fd, line.ptr, line.len);
    _ = linux.fdatasync(fd);
}

test "crash child: churn creates until killed" {
    const gpa = testing.allocator;
    var env = try testing.environ.createMap(gpa);
    defer env.deinit();
    const vol = env.get(crash_child_knob) orelse return error.SkipZigTest;
    var ledger_buf: [192]u8 = undefined;
    const ledger_path = try std.fmt.bufPrint(&ledger_buf, "{s}.acks", .{vol});
    const be = try KvBackend.open(gpa, testing.io, vol, .{ .env = &env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
    var i: u64 = 0;
    while (true) : (i += 1) {
        var nb: [24]u8 = undefined;
        const n = try std.fmt.bufPrint(&nb, "k{d}", .{i});
        const ino = commitCreate(be, n) catch break;
        var lb: [64]u8 = undefined;
        appendLine(ledger_path, try std.fmt.bufPrint(&lb, "ack {d} {s}\n", .{ ino, n }));
        if (i % 50 == 49) be.checkpointNow() catch {};
    }
    // Never reached on a healthy volume: the parent kills us mid-stream.
    be.close();
}

fn childTestIndex() ?u32 {
    for (builtin.test_functions, 0..) |f, i| {
        if (std.mem.endsWith(u8, f.name, crash_child_test_name)) return @intCast(i);
    }
    return null;
}

fn ledgerLineCount(gpa: Allocator, path: []const u8) !usize {
    const f = std.Io.Dir.openFileAbsolute(testing.io, path, .{}) catch return 0;
    defer f.close(testing.io);
    const len: usize = @intCast(try f.length(testing.io));
    const buf = try gpa.alloc(u8, len);
    defer gpa.free(buf);
    const got = try f.readPositionalAll(testing.io, buf, 0);
    return std.mem.count(u8, buf[0..got], "\n");
}

test "kill -9: a child mid-commit-stream is SIGKILLed; the reopened volume holds every ledger-acked create whole, next_ino is above every minted ino, fsck is clean, the torn tripwire never fires" {
    const gpa = testing.allocator;
    const idx = childTestIndex() orelse return error.SkipZigTest;
    const exe = "/proc/self/exe";
    const tripwire0 = journal.replay_dropped_after_clean_unmount.load();
    for ([_]usize{ 20, 75, 160 }) |target_acks| {
        var name_buf: [32]u8 = undefined;
        const v = try TestVol.create(gpa, try std.fmt.bufPrint(&name_buf, "kill9-{d}", .{target_acks}), "50");
        defer v.destroy();
        var ledger_buf: [192]u8 = undefined;
        const ledger_path = try std.fmt.bufPrint(&ledger_buf, "{s}.acks", .{v.path()});
        defer unlinkPath(ledger_path);
        var env = try testing.environ.createMap(gpa);
        defer env.deinit();
        try env.put(crash_child_knob, v.path());
        var child = try std.process.spawn(testing.io, .{
            .argv = &.{ exe, "--listen=-" },
            .environ_map = &env,
            .stdin = .pipe,
            .stdout = .pipe,
            .stderr = .ignore,
        });
        // The zig test-runner protocol: run exactly the child routine.
        const hdr: std.zig.Client.Message.Header = .{ .tag = .run_test, .bytes_len = 4 };
        try child.stdin.?.writeStreamingAll(testing.io, std.mem.asBytes(&hdr));
        try child.stdin.?.writeStreamingAll(testing.io, std.mem.asBytes(&idx));
        // Spin until the child acked enough, then kill -9 at that instant.
        const started = clock.monotonicNs();
        while ((try ledgerLineCount(gpa, ledger_path)) < target_acks) {
            if (clock.monotonicNs() - started > 120 * std.time.ns_per_s) return error.ChildTooSlow;
            std.Thread.yield() catch {};
        }
        _ = linux.kill(child.id.?, .KILL);
        const term = try child.wait(testing.io);
        try testing.expect(term == .signal);
        // The child's ledger: every acked (ino, name).
        const f = try std.Io.Dir.openFileAbsolute(testing.io, ledger_path, .{});
        defer f.close(testing.io);
        const len: usize = @intCast(try f.length(testing.io));
        const text = try gpa.alloc(u8, len);
        defer gpa.free(text);
        _ = try f.readPositionalAll(testing.io, text, 0);
        const be = try v.open();
        var lines = std.mem.splitScalar(u8, text, '\n');
        var acked_n: usize = 0;
        var max_ino: u64 = 0;
        while (lines.next()) |line| {
            if (line.len == 0) continue;
            var parts = std.mem.splitScalar(u8, line, ' ');
            _ = parts.next();
            const ino = try std.fmt.parseInt(u64, parts.next().?, 10);
            const n = parts.next().?;
            try testing.expect(try present(be, ino));
            try testing.expect(try dentryPresent(be, n));
            max_ino = @max(max_ino, ino);
            acked_n += 1;
        }
        try testing.expect(acked_n >= target_acks);
        try testing.expect(be.nextIno() > max_ino);
        try testing.expect(!be.replayStats().clean_unmount or be.replayStats().entries == 0);
        const rs = be.replayStats();
        be.close();
        const verdict = try fsckClean(gpa, v.path());
        if (verdict) |clean| try testing.expect(clean);
        std.debug.print("\nkill -9 round: {d} acked creates present after SIGKILL; replay {d} entries, {d} torn dropped, next_ino > {d}; fsck {s}\n", .{ acked_n, rs.entries, rs.dropped_torn, max_ino, if (verdict) |c| (if (c) "clean" else "FINDINGS") else "absent" });
    }
    try testing.expectEqual(tripwire0, journal.replay_dropped_after_clean_unmount.load());
}
