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
//!    refuses `Busy` loud before a byte is read, naming the claim holder
//!    the probe reads. Layers B1/B2 (PR, the writer claim) are the GATE of
//!    step 10 (`guard/gate.zig`).
//! 2. The device: registered O_DIRECT (`uring_fs.registerMetaDevice`), the
//!    io pool, the volume length.
//! 3. Both superblock copies (DUR-5: a primary that classifies is
//!    authoritative; the tail copy serves only when sector 0 errors) →
//!    format v4 with at most the block-map-tree and symmetric-plane bits
//!    (layout deltas and the volume lifecycle name later rungs'
//!    mechanisms); `SQUEEZEFS_SYMMETRIC_META=0` on a stamped volume
//!    refuses the writable open ("once armed, stays armed").
//! 4. The newest valid ledger slot (none on a v4 volume is corruption).
//! 5. The ring replayed from the record's tail (`replayScanImage`: nothing
//!    inside the window fails loud; torn entries drop and count —
//!    `replay_dropped_torn > 0` after a CLEAN unmount is the tripwire).
//! 6. The allocator from the newest A/B pages + the window's deltas.
//! 7. The node cache over the device loader; the one-member forest's two
//!    trees (the control tree and the native slot tree) opened at the
//!    record's roots; the two-phase replay into the cache; the
//!    replayed in-window frees parked (a live root's dropped); `next_ino =
//!    max(ledger, replayed + 1)`.
//! 8. The journal core resumed at the recovered head, the registry, the
//!    head's sector pad when a torn window left it mid-sector.
//! 9. The journal lane (`sqz-jrnl<N>`: an `exec.Lane` whose ring is the
//!    volume's journal ring) → the checkpointer bound to the device sinks →
//!    the bring-up residue covered (the ring-recovery preflight: a parked
//!    claim would have no drain source yet) → the conveyor → the conveyor's
//!    PASS FIBER on the journal lane (charter §5.2: both stages on ONE OS
//!    thread per writable volume; decision 0002 row 6).
//! 10. THE GATE (charter §5.7 ordering law): the replayed `writer_claim`
//!    classified → B1 arbitrated → our claim committed through the conveyor
//!    and BARRIERED → the residue covered once more (the claim tx never sits
//!    committed-but-uncovered) → on a symmetric volume the appender set
//!    (`sym/bind.zig`: the directory census, region 0's page Live with
//!    `is_manager` — the D0 winner IS the manager, charter §5.7 — and the
//!    manager lease held) → the checkpoint task's thread → the heartbeat.
//!    A refusal releases the PR and closes the flock fd before returning.
//!    FUSE arms later, in `mount`.
//!
//! The backend's `io` must be `SqzIo`'s (asserted at `open`): the pass is a
//! lane fiber and every wait it takes — the ring-admission park on the
//! space `Notify`, a contended leaf lock, the SMO lock — must park the
//! FIBER, which only our executor's futex ops do; under `Io.Threaded` the
//! park would hold the lane thread, and with it the CQEs whose completion
//! frees the ring the pass is waiting for.
//!
//! `close` is the clean dismount: no new commits, the conveyor quiesced,
//! the pass fiber stopped, the checkpoint task's final tick (the fixpoint:
//! `tail == head`, no dirty bitmap page — an empty replay window for the
//! next mount; its SMO entries ride the still-live lane), the lane drained
//! (its trailing barrier), the flock released.
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
const core_sort = @import("../core/sort.zig");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../core/counters.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const align_probe = @import("../io/align_probe.zig");
const uring_fs = @import("../io/uring_fs.zig");
const sync = @import("../sync/sync.zig");
const exec = @import("../exec/exec.zig");
const key = @import("key.zig");
const record = @import("record.zig");
const node = @import("node.zig");
const builder = @import("builder.zig");
const node_cache = @import("node_cache.zig");
const mem_budget = @import("../mem/budget.zig");
const node_seq = @import("node_seq.zig");
const tree = @import("tree.zig");
const smo = @import("smo.zig");
const leaf_merge = @import("leaf_merge.zig");
const jc = @import("journal_core.zig");
const journal = @import("journal.zig");
const journal_lane = @import("journal_lane.zig");
const journal_tail = @import("journal_tail.zig");
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
const conveyor_core = @import("conveyor_core.zig");
const format = @import("format.zig");
const geometry = @import("geometry.zig");
const device_loader = @import("device_loader.zig");
const guard = @import("guard.zig");
const routing = @import("routing.zig");
const slot_state = routing.slot_state;
const stamp_cell = routing.stamp_cell;
const sym = @import("sym.zig");
const data_bitmap = @import("../data/sym_planes/data_bitmap.zig");
const sym_records = @import("../data/sym_planes/records.zig");
const slots_plane = @import("sym/slots/plane.zig");
const manager = @import("sym/manager.zig");
const endpoint = @import("../wire/endpoint.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("backend is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const KvTx = tx.KvTx;
pub const KvTree = tree.KvTree;
pub const Outcome = conveyor.Outcome;
pub const Error = conveyor.Error;

/// Why a mount refused (loud, typed; each name maps onto the ONE errno
/// table through `openCoreError`).
pub const OpenError = error{
    /// Layer A: another writer holds the flock; layer B2: a live (or, on a
    /// non-PR substrate, a stale) foreign writer claim.
    Busy,
    /// `SQUEEZEFS_HOSTNQN`/`SQUEEZEFS_HOSTID` configured one without the
    /// other (pair-or-neither).
    InvalidIdentity,
    /// Blank superblock: `squeezefs format` first.
    NotFormatted,
    /// A pre-v5 volume: reformat required.
    LegacyFormat,
    /// Unknown incompat bits, or a capability bit of a later rung: reformat
    /// or a later binary.
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

/// The mount refusals onto the ONE error set: the guard is `Busy`; a volume
/// this binary does not mount is `UnsupportedVolume` (EINVAL); a device the
/// io plane refuses is its `DirectOpenRefused`; everything broken is EIO.
pub fn openCoreError(e: OpenError) errors.Error {
    return switch (e) {
        error.Busy => error.Busy,
        error.NotFormatted, error.LegacyFormat, error.UnsupportedFormat => error.UnsupportedVolume,
        error.DeviceRefused => error.DirectOpenRefused,
        error.Corrupt, error.Io, error.LaneSpawnFailed, error.ThreadSpawnFailed => error.InputOutput,
        error.InvalidIdentity => error.InvalidArgument,
        error.OutOfMemory => error.OutOfMemory,
    };
}

pub fn openErrno(e: OpenError) linux.E {
    return errors.toErrno(openCoreError(e));
}

pub const Stats = struct {
    /// `writer_guard_busy_refusals`: mounts refused by a held flock.
    guard_busy_refusals: counters.ShardedCounter = .{},
    /// Retries of the writer flock behind a holder that is not this binary
    /// (udev's block-device locks — `guard.proc.flockExclusive`).
    guard_shared_lock_waits: counters.ShardedCounter = .{},
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
    /// C4b: every guest root the cycle names is on the device (appender
    /// 0's page refresh, tree-0 records) — its dirty bitmap pages (C6) are
    /// not.
    after_guest_roots: ?node_cache.Hook = null,
    /// C1b/C4b: a guest root's tree-0 publish finds the checkpoint class
    /// full (`ConveyorFull`), as a ring a storm filled does.
    refuse_guest_publish: bool = false,
    /// C6: one dirty bitmap page landed — the cycle's later pages (in page
    /// order) and its record are not on the device.
    after_bitmap_page: ?node_cache.Hook = null,
};

pub var test_seams: TestSeams = .{};

pub const OpenOptions = struct {
    env: *const std.process.Environ.Map,
    /// The process's io pool; one is created (and owned) when null.
    pool: ?*uring_fs.Pool = null,
    /// `sqz-jrnl<index>`.
    index: u16 = 0,
    /// The node-cache budget (the R5 share `max(budget/16, 512 MiB)` at the
    /// daemon; tests shrink it).
    cache_budget_bytes: u64 = node_cache.DEFAULT_CACHE_BUDGET_BYTES,
    /// Possible CPUs for the batch-cap derivation.
    cpus: u32 = 0,
    /// Error logs the LANE thread is expected to emit in a test (the log
    /// seam is thread-local; `journal_lane.Options.test_error_logs`).
    test_error_logs: if (builtin.is_test) usize else void = if (builtin.is_test) 0 else {},
    /// The D0 gate's mode, host identity and seams (`guard/gate.zig`).
    guard: guard.gate.Options = .{},
    /// A held writer lock is the JOIN DOOR's signal, not a refusal: the
    /// caller (`meta_set`) opens volume 0 of a symmetric set with a join
    /// posture at hand and will `openJoined` on `Busy`, so the flock's
    /// `EWOULDBLOCK` logs at info naming the manager. False keeps the loud
    /// refusal (a `--single-writer` volume, a member past 0, a reader, the
    /// in-process harnesses).
    join_door: bool = false,
};

/// The mount's posture (charter §5.7): the D0 ladder's winner is the
/// MANAGER of the volume (the flock, the claim, the fixed ledger, tree 0);
/// a later RW mount of a symmetric set is JOINED (no flock, the region
/// over the wire, the page as its ledger); a `-o ro` mount of a symmetric
/// set is a READER (charter §5.5 "`-o ro` mounts are member-readers +
/// token clients … with NO live writer … the QUIESCENT reader of the
/// checkpointed projection"): the device read-only, no flock, no claim,
/// no ring, every tree a projection at the newest checkpoint.
pub const Posture = enum { manager, joined, reader };

/// `openJoined`'s inputs beyond `OpenOptions`.
pub const JoinOptions = struct {
    /// The mount point: the joiner's `mount_slot` derives from it.
    mountpoint: []const u8,
    /// `--join <endpoint>`: the manager's listener when the durable
    /// `endpoint:{0}` record is not to be trusted (or not yet written).
    endpoint: ?endpoint.EndpointWire = null,
};

pub const FormatOptions = struct {
    env: *const std.process.Environ.Map,
    pool: ?*uring_fs.Pool = null,
    /// The symmetric plane by default; `--single-writer` is the opt-out.
    class: geometry.Class = .symmetric,
    node_size: u64 = superblock.default_node_size,
    journal_len_override: ?u64 = null,
    /// Per-volume identity (the CLI mints it from `Io.randomSecure`).
    uuid: [16]u8,
    /// The set's identity (`geometry.mintSetIdentity` for a new set; the
    /// first member's for a joiner).
    set: geometry.SetIdentity,
    root_uid: u32 = 0,
    root_gid: u32 = 0,
    /// This member's place in the set (`format.FormatOptions`): position 0
    /// hosts slot 0's native keyspace and the `set_config` record.
    member_position: u16 = 0,
    member_count: u16 = 1,
    /// `format.FormatOptions.stamp_override` (the `add-meta` joiner).
    stamp_override: ?ledger.MembershipStamp = null,
    /// Tree 0's format-time records (`format.FormatOptions.control_records`:
    /// the `set_config` record on the set's member 0).
    control_records: []const record.RecordRef = &.{},
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
        .class = opts.class,
        .node_size = opts.node_size,
        .journal_len_override = opts.journal_len_override,
        .uuid = opts.uuid,
        .set = opts.set,
        .root_uid = opts.root_uid,
        .root_gid = opts.root_gid,
        .member_position = opts.member_position,
        .member_count = opts.member_count,
        .stamp_override = opts.stamp_override,
        .control_records = opts.control_records,
    }, &diag) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.VolumeTooSmall => error.VolumeTooSmall,
        error.InvalidNodeSize => error.InvalidNodeSize,
        error.InvalidJournalOverride => error.InvalidJournalOverride,
        else => error.Corrupt,
    };
    defer plan.deinit();
    try dev.writeSteps(plan.steps);
    log.info("formatted {s}: format v4 ({s}), member {d} of {d}, {d} B, node {d} B, journal {d} B, {d} extents", .{ path, @tagName(opts.class), opts.member_position, opts.member_count, volume_len, opts.node_size, plan.superblock.journal.len, plan.superblock.totalExtents() });
}

// ---------------------------------------------------------------------------
// The backend.
// ---------------------------------------------------------------------------

pub const KvBackend = struct {
    gpa: Allocator,
    io: Io,
    env: *const std.process.Environ.Map,
    path: []u8,
    index: u16,
    /// Layer A: the dedicated flock fd, held until `close`.
    guard_fd: linux.fd_t,
    pool: *uring_fs.Pool,
    pool_owned: bool,
    volume_len: u64,
    sb: superblock.Superblock,
    /// `sb.hash_seed` expanded once at open (`key.NameKey`): every name hash
    /// on this volume takes it by pointer.
    name_key: key.NameKey,
    /// The DUR-5 generation the next superblock write carries (one past the
    /// newest valid image at open).
    sb_generation: u64,
    /// The capability stampers serialize here (a mount-time class: one
    /// writer per bit, never on a hot path).
    sb_lock: Io.Mutex = .init,
    /// The mounted record (its stamp rides every checkpoint record).
    mounted: ledger.LedgerRecord,
    dev: device_loader.DeviceLoader,
    layout: node.NodeLayout,
    cache: *node_cache.NodeCache,
    alloc: alloc_ext.ExtentAllocator,
    seq: node_seq.NodeSeqHandle,
    /// The live tree set: the control tree, the native slot tree (member 0)
    /// and every guest slot tree opened from tree 0's `slot_state` records
    /// or minted on first touch (charter §5.5 "slot trees are minted
    /// lazily"); writers serialize on `trees_lock`.
    trees: tree.TreeTable,
    trees_lock: Io.Mutex = .init,
    /// The runtime membership stamp with its guest-cursor cells (what every
    /// ledger record carries; the flip's stamp writes mutate it).
    stamp: *stamp_cell.StampCell,
    /// The durable `slot_state` copy of every guest root (seeded from tree
    /// 0 at open; a root the tree moved past is republished at the next
    /// cycle's C1b).
    guest_published: std.AutoHashMapUnmanaged(u16, tree.RootPtr) = .empty,
    /// The cursor floors tree 0's `slot_state` records carried at open.
    guest_cursor_floors: std.AutoHashMapUnmanaged(u16, u64) = .empty,
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
    space_notify: sync.Notify = .init,
    ckpt_thread: ?std.Thread = null,
    cv: *conveyor.Conveyor,
    /// The conveyor's pass: a committer fiber on the journal lane, woken by
    /// the election winner, joined through its group at `close`.
    pass_fiber: ?*exec.fiber.Fiber = null,
    pass_group: Io.Group = .init,
    /// `SQUEEZEFS_META_PASS_VENUE`: the election winner runs the pass (and
    /// the apply) on its own thread instead of waking the lane's fiber.
    pass_on_committer: bool = false,
    /// The journal tail (decision 0003 §BB.2): the ring's kernel-held
    /// mirror the supervisor replays at the daemon's death; null on a
    /// reader. Created right after the lane (the slot table is sized to
    /// twice the lane's cells).
    tail: ?journal_tail.linux_tail.Tail = null,
    /// The applier fibers' group (decision 0003 §AW): joined at close,
    /// after the conveyor's quiesce (no window in flight ⇒ every applier
    /// has returned).
    apply_group: Io.Group = .init,
    pass_stop: std.atomic.Value(bool) = .init(false),
    ino: inos.InoWatermark,
    replay: ReplayStats,
    /// The threshold drain's round-robin position over `trees` (task-owned).
    maintenance_cursor: usize = 0,
    /// Layers B1/B2 (the claim, the PR posture, the heartbeat); null only
    /// until the gate ran.
    guard: ?*guard.gate.Guard = null,
    /// The D0 ladder's measured duration at this open (the failover
    /// bound's ladder term, charter §5.5 "Extent grants").
    gate_ns: u64 = 0,
    /// The symmetric plane's appender set (region 0 = this mount's ring
    /// with `is_manager`, the in-process regions, the manager); null on a
    /// single-writer volume and on a joiner.
    sym: ?*sym.bind.AppenderSet = null,
    /// The JOINED posture's plane (`openJoined`): the region over the
    /// wire, the grant pool, the manager's session; null otherwise.
    joined: ?*sym.joined.JoinedSet = null,
    posture: Posture = .manager,
    /// The cache budget this open was given (a joiner re-creates its
    /// cache while it polls the projection).
    cache_budget_bytes: u64 = node_cache.DEFAULT_CACHE_BUDGET_BYTES,
    /// Held across the joiner's cache swap (`reopenControlProjection`) and
    /// by the R5 component's gauge/shed, so the sampler never reads or
    /// kicks a cache mid-replacement.
    cache_swap: jc.SpinLock = .{},
    /// The slot-lease plane: the commit door from the first commit (its
    /// cutover state serves every volume), armed by the appender set.
    slots: *slots_plane.Plane,
    /// THE KEPT WINDOW (`sym_data_planes.md` §2.6): the kind-5 data-
    /// allocation deltas the mount's replay met in the fixed ring, kept
    /// because the bring-up checkpoint advances the tail past them BEFORE
    /// any allocation lease is re-held — the same-node successor of a
    /// dead manager folds them onto its predecessor's `KVDA` pages. Owned
    /// copies; consumed by the data planes' arm or dropped at `close`.
    kept_data_deltas: std.ArrayList(KeptDelta) = .empty,
    /// The data allocation bitmaps' page sink (the holder plane's), run at
    /// the cycle's C6 before barrier #1; installed and cleared under
    /// `smo_lock` (the cycle runs under it), so the task never reads a
    /// torn seam. Null until the holder plane arms, and on a joiner.
    kvda_sink: ?KvdaSink = null,
    /// `SQUEEZEFS_TEST_KVDA_PAGE_SKIP`: the cycle skips the sink (the
    /// fallback kill row), read once at the open.
    kvda_pages_skipped: bool = false,
    closed: bool = false,

    pub const KeptDelta = struct { seq: u64, key: [sym_records.DELTA_KEY_LEN]u8, value: [sym_records.DELTA_LEN]u8 };

    /// `checkpoint_cycle.RecordSource.dataPages`' target: every held
    /// bitmap's dirty pages written through its refs at `generation`.
    pub const KvdaSink = struct {
        ctx: *anyopaque,
        writePages: *const fn (ctx: *anyopaque, generation: u64) checkpoint_cycle.IoError!void,
    };

    // ---- open ----

    pub fn open(gpa: Allocator, io: Io, path: []const u8, opts: OpenOptions) OpenError!*KvBackend {
        // The pass is a lane fiber: its waits must park the fiber (module doc).
        std.debug.assert(io.vtable == &exec.SqzIo.vtable);
        const t0 = clock.monotonicNs();
        // 1. Layer A (a test seam skips it: the "another host" simulation,
        // where the claim alone must refuse).
        const skip_flock = if (builtin.is_test) opts.guard.test_skip_flock else false;
        const guard_fd = if (skip_flock) openGuardFd(path) catch return error.DeviceRefused else acquireWriterFlock(gpa, io, path, opts) catch |e| return e;
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
        const classified = try readSuperblock(gpa, &dev, path, opts.env, volume_len);
        const sb = classified.sb;

        // 4. The ledger.
        var mounted = try readNewestLedger(gpa, &dev, path, sb);
        errdefer mounted.deinit(gpa);

        // 5. The ring: one read, the chain walk from the tail.
        const ring_ext = geometry.fixedRingExtent(sb);
        const ring_layout = journal.RingLayout.fromExtent(.{ .start = ring_ext.start, .len = ring_ext.len });
        const geo = journal.productionGeometry(ring_layout.pages, mode.grain());
        const ring_img = try dev.readRange(gpa, ring_layout.base, @intCast(ring_layout.ringBytes()));
        defer gpa.free(ring_img);
        var recovery = try journal.replayScanImage(gpa, ring_img, geo, mounted.journal_tail_seq, journal.FIXED_RING_APPENDER);
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
            .name_key = .init(sb.hash_seed),
            .sb_generation = classified.next_generation,
            .mounted = mounted,
            .dev = dev,
            .layout = layout,
            .cache = undefined,
            .alloc = alloc,
            .seq = node_seq.NodeSeqHandle.shared(@max(mounted.seq, mounted.node_seq_watermark)),
            .trees = undefined,
            .stamp = undefined,
            .slots = undefined,
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
        // The ledger-rooted trees: the control tree and (on slot 0's host)
        // the native slot tree. A guest slot tree's root is a tree-0
        // `slot_state` record, so the control records are replayed FIRST,
        // then the guest trees open (or are minted for slots the window
        // names without a published root), then the window replays again
        // through the complete routing — idempotent by the per-key LWW gate.
        for (be.mounted.tree_roots) |r| {
            const t = KvTree.openFromLedger(gpa, be.cache, &be.mounted, r.id, &be.seq) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.Io => return error.Io,
                else => return error.Corrupt,
            };
            errdefer t.destroy();
            be.seq.raiseTo(r.node_seq);
            try trees.append(gpa, t);
        }
        be.trees = try tree.TreeTable.init(gpa, trees.items);
        trees.deinit(gpa);
        trees = .empty;
        errdefer {
            for (be.trees.load()) |t| t.destroy();
            be.trees.deinit();
        }
        _ = tree.replayEntries(gpa, be.trees.load(), recovery.entries) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
            else => return error.Corrupt,
        };
        be.openGuestTrees(recovery.entries, &be.alloc) catch |e| switch (e) {
            error.RootStale => return error.Corrupt,
            else => |x| return x,
        };
        errdefer {
            be.guest_published.deinit(gpa);
            be.guest_cursor_floors.deinit(gpa);
        }
        _ = tree.replayEntries(gpa, be.trees.load(), recovery.entries) catch |e| switch (e) {
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
        be.kvda_pages_skipped = knobs.boolKnob("SQUEEZEFS_TEST_KVDA_PAGE_SKIP", opts.env);
        try be.keepDataDeltas(recovery.entries);
        errdefer be.kept_data_deltas.deinit(gpa);
        {
            var roots_buf: std.ArrayList(u64) = .empty;
            defer roots_buf.deinit(gpa);
            for (be.trees.load()) |t| try roots_buf.append(gpa, be.cache.addrExtent(t.rootPtr().addr));
            _ = try be.alloc.parkReplayedFrees(roots_buf.items);
        }
        var fold: inos.RecoveryFold = .{ .native_slot = mounted.membership_stamp.native_slot };
        defer fold.deinit(gpa);
        fold.foldEntries(gpa, recovery.entries) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Corrupt => return error.Corrupt,
        };
        be.ino.installFloor(fold.nextIno(mounted.next_ino));
        // The stamp with its cells: every stamped cursor, then every guest
        // slot the window mentions (its replay maximum + 1), then every
        // published slot_state cursor — all FLOORS, max wins.
        be.stamp = try stamp_cell.StampCell.init(gpa, mounted.membership_stamp);
        errdefer be.stamp.deinit();
        {
            var it = fold.guest_max.iterator();
            while (it.next()) |e| try be.stamp.installFloor(e.key_ptr.*, fold.guestCursor(e.key_ptr.*, mounted.membership_stamp.cursorFor(e.key_ptr.*) orelse 0));
            var fit = be.guest_cursor_floors.iterator();
            while (fit.next()) |e| {
                // The native slot's lease cursor floors the native
                // watermark (the lease carries the cursor; the ledger
                // stamp's `next_ino` is the older of the two homes).
                if (mounted.membership_stamp.native_slot == e.key_ptr.*) be.ino.installFloor(e.value_ptr.*) else try be.stamp.installFloor(e.key_ptr.*, e.value_ptr.*);
            }
        }

        // 8. The registry at the recovered head; the sector-pad alignment of
        // a head a torn window left mid-sector.
        be.reg = try jc.InflightRegistry.init(gpa, recovery.head_pos, conveyor.queueCapacity(geo.pages));
        errdefer be.reg.deinit();
        be.reg.recoverSeqOffset(journal.seqOffsetOfWindow(recovery.entries));
        try be.alignRingHead();

        // 9. The slot plane (the door from the first commit; the cycle's
        // publication hooks read it), the lane, the checkpointer, the
        // bring-up cover, the conveyor, the task thread.
        be.slots = try slots_plane.Plane.init(gpa, io, be, opts.env);
        errdefer be.slots.deinit();
        be.lane = journal_lane.JournalLane.spawn(gpa, opts.env, .{
            .path = be.path,
            .index = opts.index,
            .core = &be.core,
            .registry = &be.reg,
            .ring_stats = &be.ring_stats,
            .hooks = .{ .ctx = be, .rollback = laneRollbackHook, .checkpoint_past = laneCheckpointPastHook, .barrier_started = laneBarrierStartedHook, .barrier_landed = laneBarrierLandedHook, .cover_upto = laneCoverHook },
            .test_error_logs = opts.test_error_logs,
        }) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => return error.LaneSpawnFailed,
        };
        errdefer be.lane.deinit();
        // Before the first request: a handler fiber's fsync parks alone.
        be.lane.routeWaits(io);
        be.tail = try openTail(be.path, ring_layout, geo);
        errdefer if (be.tail) |*t| t.deinit();
        // Every checkpoint-side device error now classifies for the fence.
        be.dev.latch = &be.lane.latch;
        be.smo_journal = .{ .gpa = gpa, .layout = ring_layout, .geo = geo, .core = &be.core, .reg = &be.reg, .image = &.{}, .barrier_hook = be.dev.barrierSink(), .latch = &be.lane.latch, .device = .{ .ctx = be, .write = smoWriteHook, .completed = smoCompletedHook } };
        be.smo_journal.retire_seq.store(mounted.seq + 1, .release);
        be.smo_ctx = smo.SmoContext.withJournal(&be.alloc, &be.smo_journal);
        be.ckpt_seam = .{ .ctx = &be.smo_ctx, .trees = &be.trees };
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
            .pages = .{ .ctx = be, .writePage = bitmapPageHook },
            .record = .{ .ctx = be, .treeRoots = treeRootsFn, .next_ino = &be.ino, .node_seq = &be.seq, .membershipStamp = stampForLedger, .publishGuestRoots = publishGuestRootsHook, .unpublishedGuestFloor = unpublishedGuestFloor, .appenderPages = if (sb.symmetricPlaneStamped()) appenderPagesHook else null, .dataPages = if (sb.symmetricPlaneStamped()) dataPagesHook else null },
            .smo = be.ckpt_seam.seam(),
            .maintenance = .{ .ctx = be, .pop = popMaintenance, .pending = maintenancePending, .requeue = requeueMaintenance },
        });
        errdefer be.cp.deinit();
        be.smo_ctx.space_retry = be.cp.spaceRetryHold();
        be.cp.space_wake = .{ .notify = &be.space_notify, .io = io };
        be.task = checkpoint_cycle.CheckpointTask.init(&be.cp, io);
        be.task.serial = &be.smo_lock;
        be.cache.installCheckpointKick(be.task.hook());
        const cpus: u32 = if (opts.cpus != 0) opts.cpus else derive.Facts.probe().cpus;
        be.pass_on_committer = std.mem.eql(u8, knobs.enumKnob("SQUEEZEFS_META_PASS_VENUE", opts.env), "committer");
        be.cv = try conveyor.Conveyor.init(gpa, .{
            .io = io,
            .gpa = gpa,
            .volume = be.path,
            .trees = &be.trees,
            .cache = be.cache,
            .alloc = &be.alloc,
            .core = &be.core,
            .reg = &be.reg,
            .ring_layout = ring_layout,
            .geo = geo,
            .appender_id = journal.FIXED_RING_APPENDER,
            .latch = &be.lane.latch,
            .lane = .{ .ctx = be, .submit = laneSubmit, .barrier = laneBarrier },
            .pass = .{ .ctx = be, .wake = wakePass },
            .checkpoint = .{ .ctx = be, .kickPark = kickPark, .kickThreshold = kickThreshold, .checkpointNow = checkpointNowHook, .checkpointPast = checkpointPastHook, .space = &be.space_notify, .posture = &be.cp.posture, .space_retry = be.cp.spaceRetryHold() },
            .smo_journal = &be.smo_journal,
            .caps = conveyor.resolveCaps(opts.env, cpus, geo),
            .park_escalation_ns = conveyor.PARK_ESCALATION_NS,
            .door = be.slots.doorSeam(),
            .window_hold = conveyor.windowHoldFromEnv(opts.env, flush_ms, geo.grain),
            .caller = .{ .idle = exec.lane.laneIdleBesidesCaller, .polls = exec.lane.lanePolls },
            .applier = be.applierSeam(opts.env, cpus),
            .ack_at_apply = ackAtApply(opts.env, flush_ms),
            .tail = be.tailSeam(),
            .self_apply = std.mem.eql(u8, knobs.enumKnob("SQUEEZEFS_META_APPLY", opts.env), "self"),
        });
        errdefer be.cv.deinit();
        try be.startPass();
        errdefer be.stopPass();

        // 10. The gate in two phases: the claim read and classified and
        // the B1 arbitration (register ladder, acquire, preempt a stale
        // holder) BEFORE any device write of the open — a successor under
        // another host identity is fenced until its preempt lands; then
        // the ring-recovery preflight (the reference's `preclaim_ring_
        // recovery`: the whole ring reclaimable before the claim tx is
        // admitted — the checkpoint task does not exist yet, so a parked
        // claim would have no drain source); then the claim committed
        // through `be.cv` (the pass fiber is live) and barriered through
        // the lane; then the cover once more so the claim tx never sits
        // committed-but-uncovered; then the checkpoint thread; then the
        // heartbeat. A gate refusal unwinds the `errdefer`s above and
        // closes the flock fd LAST.
        const t_gate = clock.monotonicNs();
        be.guard = try guard.gate.Guard.prepare(gpa, io, be, opts.guard);
        errdefer {
            be.guard.?.deinit();
            be.guard = null;
        }
        be.smo_lock.lockUncancelable(io);
        const precovered = be.cp.coverBringUpResidue();
        be.smo_lock.unlock(io);
        precovered catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
            else => return error.Corrupt,
        };
        try be.guard.?.commit();
        be.gate_ns = clock.monotonicNs() - t_gate;
        be.smo_lock.lockUncancelable(io);
        const covered = be.cp.coverBringUpResidue();
        be.smo_lock.unlock(io);
        covered catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
            else => return error.Corrupt,
        };
        // The symmetric plane: the D0 winner IS the manager (charter §5.7)
        // — region 0 goes Live with `is_manager` under this identity, the
        // directory census runs, the lease is held; one barriered cycle
        // makes the join durable.
        if (sb.symmetricPlaneStamped()) {
            be.sym = try sym.bind.AppenderSet.open(be);
            errdefer {
                be.sym.?.deinit();
                be.sym = null;
            }
            be.smo_lock.lockUncancelable(io);
            const joined = be.cp.checkpointNow();
            be.smo_lock.unlock(io);
            joined catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.Io => return error.Io,
                else => return error.Corrupt,
            };
        }
        be.ckpt_thread = std.Thread.spawn(.{}, ckptMain, .{be}) catch return error.ThreadSpawnFailed;
        try be.guard.?.startHeartbeat();
        be.replay.replay_ns = clock.monotonicNs() - t0;
        stats.mounts.inc();
        log.info("meta volume {s}: mounted — ledger seq {d}, tail {d}, replayed {d} entries ({d} content records, {d} torn dropped{s}), next_ino {d}, {d} trees, {d}/{d} extents free, {s} cadence, guard {s}", .{ be.path, mounted.seq, mounted.journal_tail_seq, be.replay.entries, be.replay.content_records, be.replay.dropped_torn, if (clean_unmount) ", clean unmount" else "", be.ino.snapshot(), be.trees.load().len, be.alloc.freeExtents(), total_extents, if (flush_ms == 0) "strict" else "coalesced", be.guard.?.modeName() });
        return be;
    }

    fn ckptMain(be: *KvBackend) void {
        be.task.runUntilStopped();
    }

    const ClassifiedSuperblock = struct { sb: superblock.Superblock, next_generation: u64 };

    /// Step 3 of both opens: both copies read and classified (DUR-5), the
    /// format and capability gates, the "once armed, stays armed" knob
    /// gate; the loader's bases set.
    fn readSuperblock(gpa: Allocator, dev: *device_loader.DeviceLoader, path: []const u8, env: *const std.process.Environ.Map, volume_len: u64) OpenError!ClassifiedSuperblock {
        const sector0 = try dev.readRange(gpa, 0, superblock.sector_len);
        defer gpa.free(sector0);
        const backup: ?[]u8 = if (superblock.backupOffsetFor(volume_len)) |off| try dev.readRange(gpa, off, superblock.sector_len) else null;
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
            .legacy => |v| {
                log.err("{s} is a pre-v5 squeezefs format (version {d}), which this binary does not mount — reformat required", .{ path, v });
                return error.LegacyFormat;
            },
            .v5 => |s| s,
        };
        if (classified.slot == .backup) log.err("{s}: sector 0 is unreadable — mounting from the redundant superblock copy; investigate the device", .{path});
        if (sb.unknownIncompat() != 0) {
            log.err("{s}: unknown incompat feature bits {x} — this binary cannot mount it (reformat required)", .{ path, sb.unknownIncompat() });
            return error.UnsupportedFormat;
        }
        if (sb.features_incompat & ~(superblock.block_map_tree | superblock.symmetric_plane) != 0) {
            // Of the four capability bits the block-map tree and the
            // symmetric plane are implemented (layout deltas and the volume
            // lifecycle are later rungs').
            log.err("{s}: capability bits {x} name mechanisms this rung does not implement — refusing the mount", .{ path, sb.features_incompat & ~(superblock.block_map_tree | superblock.symmetric_plane) });
            return error.UnsupportedFormat;
        }
        if (sb.symmetricPlaneStamped() and !knobs.boolKnob("SQUEEZEFS_SYMMETRIC_META", env)) {
            log.err("{s}: SQUEEZEFS_SYMMETRIC_META=0 but the superblock carries SYMMETRIC_PLANE — once armed, stays armed; the writable open is refused (`-o ro` is the read posture, never a knob value)", .{path});
            return error.UnsupportedFormat;
        }
        dev.ledger_base = sb.root_ledger.start;
        dev.bitmap_base = sb.alloc_bitmap.start;
        return .{ .sb = sb, .next_generation = superblock.nextSuperblockGeneration(sector0, backup) };
    }

    /// Step 4 of both opens: the newest valid ledger record (none on a v4
    /// volume is corruption). Owned by the caller.
    fn readNewestLedger(gpa: Allocator, dev: *device_loader.DeviceLoader, path: []const u8, sb: superblock.Superblock) OpenError!ledger.LedgerRecord {
        const ledger_img = try dev.readRange(gpa, sb.root_ledger.start, @intCast(sb.root_ledger.len));
        defer gpa.free(ledger_img);
        return (try ledger.newestValid(gpa, ledger_img)) orelse {
            log.err("{s}: no valid root-ledger record in any of the 32 slots — the volume carries a v4 superblock, so this is corruption, not a fresh format", .{path});
            return error.Corrupt;
        };
    }

    /// The JOINED open (charter §5.7 "a later RW mount JOINS it"; the spec's
    /// joined door, `docs/porting/sym_wire_membership_recovery.md` §5.2) —
    /// the posture of a `squeezefs mount` of a symmetric set whose D0
    /// ladder another process won:
    ///
    /// 1. NO flock (the claim IS the join), no superblock repair, no
    ///    writer claim: the device registered O_DIRECT read-write.
    /// 2. The superblock (it must carry the symmetric plane: a
    ///    `--single-writer` volume has no join door) and the newest ledger
    ///    record — the checkpointed PROJECTION. Ring 0 is NOT replayed: a
    ///    replay would dirty tree 0's nodes in THIS cache and the joiner's
    ///    flush pass would append them over the manager's leaves.
    /// 3. The cache; the control tree opened READ-ONLY at the record's
    ///    root (every control write is a wire verb; `set_config`, the
    ///    secret and the manager's endpoint are read here). The manager's
    ///    native tree is NOT opened: slot 0's objects (`/` among them) are
    ///    read under tokens through the divert, never off a projection.
    /// 4. `sym/joined.zig`: the directory census, the dial, `JoinAppender`
    ///    → the region: THIS backend's ring is the region's segments, its
    ///    seq space the join reply's continuation + offset, its node seqs
    ///    the minted incarnation, its allocator the GRANT MIRROR.
    /// 5. The lane over the region's ring, the structural writer, the
    ///    checkpointer in the PAGE-AS-LEDGER posture (no ledger slot, no
    ///    bitmap page: the page carries roots, cursors, `ckpt_seq`,
    ///    `ledger_tail_seq`, the pool's largest runs), the conveyor with
    ///    the region's appender id, grant-fed (`GrantExhausted`, never
    ///    ENOSPC), the park gate before admission.
    /// 6. The slot plane armed JOINED: tree 0's projection as the lease
    ///    view, `M` rotor slots over the wire, the door's first touch over
    ///    the wire.
    /// 7. The first page write under this identity (both directory slots:
    ///    the directory-first law), the checkpoint thread.
    ///
    /// Membership (the heartbeat, the park gate's clocks, the endpoint
    /// publication) is the wire plane's: it needs this mount's own
    /// listener. `close` releases every lease, leaves the region and frees
    /// the page under the manager's pen.
    pub fn openJoined(gpa: Allocator, io: Io, path: []const u8, opts: OpenOptions, jopts: JoinOptions) OpenError!*KvBackend {
        std.debug.assert(io.vtable == &exec.SqzIo.vtable);
        const t0 = clock.monotonicNs();
        // 1. The device fd without the flock (held for the mount's life, the
        // same dedicated-fd rule: never a pool fd).
        const guard_fd = openGuardFd(path) catch return error.DeviceRefused;
        errdefer _ = linux.close(guard_fd);
        const mode = uring_fs.registerMetaDevice(gpa, opts.env, path) catch return error.DeviceRefused;
        const pool = opts.pool orelse (uring_fs.Pool.init(gpa, uring_fs.Config.resolve(opts.env, derive.Facts.probe())) catch return error.DeviceRefused);
        errdefer if (opts.pool == null) pool.deinit();
        const volume_len = device_loader.volumeLength(path) catch return error.DeviceRefused;
        const be = try gpa.create(KvBackend);
        errdefer gpa.destroy(be);
        const owned_path = try gpa.dupe(u8, path);
        errdefer gpa.free(owned_path);
        var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = owned_path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };

        // 2. The superblock and the projection's record.
        const classified = try readSuperblock(gpa, &dev, path, opts.env, volume_len);
        const sb = classified.sb;
        if (!sb.symmetricPlaneStamped()) {
            log.err("meta volume {s}: another writer holds it and the volume is `--single-writer` (no symmetric plane) — there is no join door; EBUSY", .{path});
            return error.Busy;
        }
        var mounted = try readNewestLedger(gpa, &dev, path, sb);
        var mounted_in_be = false;
        errdefer if (!mounted_in_be) mounted.deinit(gpa);
        const ring_ext = geometry.fixedRingExtent(sb);
        const ring0_layout = journal.RingLayout.fromExtent(.{ .start = ring_ext.start, .len = ring_ext.len });
        const ring0_geo = journal.productionGeometry(ring0_layout.pages, mode.grain());

        // 3. The cache and the control tree (read-only). The ring words are
        // provisional until the join answers (ring 0's geometry: nothing is
        // written before step 5 replaces them).
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
            .name_key = .init(sb.hash_seed),
            .sb_generation = classified.next_generation,
            .mounted = mounted,
            .dev = dev,
            .layout = layout,
            .cache = undefined,
            .alloc = undefined,
            .seq = node_seq.NodeSeqHandle.shared(@max(mounted.seq, mounted.node_seq_watermark)),
            .trees = undefined,
            .stamp = undefined,
            .slots = undefined,
            .ring_layout = ring0_layout,
            .geo = ring0_geo,
            .core = jc.JournalCore.init(ring0_geo, mounted.journal_tail_seq, mounted.journal_tail_seq),
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
            .replay = .{ .entries = 0, .dropped_torn = 0, .content_records = 0, .clean_unmount = true, .replay_ns = 0 },
            .posture = .joined,
        };
        mounted_in_be = true;
        errdefer be.mounted.deinit(gpa);
        be.dev.path = be.path;
        be.cache_budget_bytes = opts.cache_budget_bytes;
        be.cache = try node_cache.NodeCache.init(gpa, io, .{ .layout = layout, .heap_base = sb.heap.start, .budget_bytes = opts.cache_budget_bytes }, be.dev.loader());
        errdefer be.cache.deinit();
        be.cache.setDurableTail(mounted.journal_tail_seq);
        be.trees = try tree.TreeTable.init(gpa, &.{});
        errdefer {
            for (be.trees.load()) |t| t.destroy();
            be.trees.deinit();
        }
        try be.openControlProjection();
        // The stamp: the ledger's (the hosting map); the cursor cells of
        // the slots this mount leases are floored by the grants' words.
        be.stamp = try stamp_cell.StampCell.init(gpa, mounted.membership_stamp);
        errdefer be.stamp.deinit();
        errdefer {
            be.guest_published.deinit(gpa);
            be.guest_cursor_floors.deinit(gpa);
        }

        // 4. The join: the census, the dial, the region (sym/joined.zig).
        // The projection may be re-read (and the cache re-created) while
        // the manager's endpoint is awaited, so the slot plane — whose
        // frame seam lives in the cache — arms after it.
        const js = sym.joined.JoinedSet.open(be, jopts.mountpoint, jopts.endpoint) catch |e| return switch (e) {
            error.ManagerUnreachable, error.PredecessorLive, error.Capacity => error.Busy,
            error.Rejected, error.Corrupt => error.Corrupt,
            error.Io => error.Io,
            error.OutOfMemory => error.OutOfMemory,
        };
        be.joined = js;
        errdefer {
            js.deinit();
            be.joined = null;
        }
        be.slots = try slots_plane.Plane.init(gpa, io, be, opts.env);
        errdefer be.slots.deinit();
        const standing = try js.standing();
        defer gpa.free(standing.granted);
        // The ring IS the region's: its layout, geometry, head and tail,
        // the seq offset, the incarnation's node seqs.
        be.ring_layout = standing.ring_layout;
        be.geo = standing.geo;
        be.core = jc.JournalCore.init(standing.geo, standing.head_pos, standing.tail_pos);
        be.seq = standing.seq;
        be.reg = try jc.InflightRegistry.init(gpa, standing.head_pos, conveyor.queueCapacity(standing.geo.pages));
        errdefer be.reg.deinit();
        be.reg.recoverSeqOffset(standing.seq_offset);
        // The grant mirror: every extent claimed but the granted runs.
        be.alloc = alloc_ext.ExtentAllocator.grantMirror(gpa, sb.totalExtents(), sym.joined.MIRROR_RESERVE_EXTENTS, alloc_ext.pending_free_cap, standing.granted) catch return error.OutOfMemory;
        errdefer be.alloc.deinit();
        // The pool's proactive refill rides the mirror's low-water mark.
        js.armPoolRefill();

        // 5. The lane over the region's ring, the structural writer, the
        // checkpointer (page-as-ledger), the conveyor (grant-fed).
        be.lane = journal_lane.JournalLane.spawn(gpa, opts.env, .{
            .path = be.path,
            .index = opts.index,
            .core = &be.core,
            .registry = &be.reg,
            .ring_stats = &be.ring_stats,
            .hooks = .{ .ctx = be, .rollback = laneRollbackHook, .checkpoint_past = laneCheckpointPastHook, .barrier_started = laneBarrierStartedHook, .barrier_landed = laneBarrierLandedHook, .cover_upto = laneCoverHook },
            .test_error_logs = opts.test_error_logs,
        }) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => return error.LaneSpawnFailed,
        };
        errdefer be.lane.deinit();
        be.lane.routeWaits(io);
        be.tail = try openTail(be.path, be.ring_layout, be.geo);
        errdefer if (be.tail) |*t| t.deinit();
        be.dev.latch = &be.lane.latch;
        be.smo_journal = .{ .gpa = gpa, .layout = be.ring_layout, .geo = be.geo, .core = &be.core, .reg = &be.reg, .image = &.{}, .barrier_hook = be.dev.barrierSink(), .latch = &be.lane.latch, .device = .{ .ctx = be, .write = smoWriteHook, .completed = smoCompletedHook }, .appender_id = standing.appender_id };
        be.smo_journal.retire_seq.store(standing.ckpt_seq + 1, .release);
        be.smo_ctx = smo.SmoContext.withJournal(&be.alloc, &be.smo_journal);
        be.smo_ctx.grant_fed = true;
        be.smo_ctx.grant_holds = js.grantHolds();
        be.ckpt_seam = .{ .ctx = &be.smo_ctx, .trees = &be.trees };
        be.flusher = .{ .cache = be.cache, .smo = be.ckpt_seam.seam() };
        const flush_ms = checkpoint.flushIntervalMsFromEnv(opts.env);
        be.cp = checkpoint_cycle.Checkpointer.init(gpa, .{
            .volume = be.path,
            .max_age_ms = checkpoint.maxAgeMsFromEnv(opts.env),
            .flush_interval_ms = flush_ms,
            .max_dirty_nodes = checkpoint.maxDirtyNodesFromEnv(opts.env, opts.cache_budget_bytes, sb.node_size),
            .forest = true,
            .checkpoint_seq = standing.ckpt_seq,
            .last_ledger_tail = standing.tail_pos,
        }, .{
            .core = &be.core,
            .inflight = &be.reg,
            .cache = be.cache,
            .alloc = &be.alloc,
            .latch = &be.lane.latch,
            .flush = be.flusher.sink(),
            .barrier = be.dev.barrierSink(),
            .ledger = null,
            .pages = null,
            .record = .{ .ctx = be, .treeRoots = treeRootsFn, .next_ino = &be.ino, .node_seq = &be.seq, .membershipStamp = stampForLedger, .publishGuestRoots = publishGuestRootsHook, .unpublishedGuestFloor = unpublishedGuestFloor, .appenderPages = joinedPagesHook },
            .smo = be.ckpt_seam.seam(),
            .maintenance = .{ .ctx = be, .pop = popMaintenance, .pending = maintenancePending, .requeue = requeueMaintenance },
        });
        errdefer be.cp.deinit();
        be.smo_ctx.space_retry = be.cp.spaceRetryHold();
        be.cp.space_wake = .{ .notify = &be.space_notify, .io = io };
        be.task = checkpoint_cycle.CheckpointTask.init(&be.cp, io);
        be.task.serial = &be.smo_lock;
        be.cache.installCheckpointKick(be.task.hook());
        const cpus: u32 = if (opts.cpus != 0) opts.cpus else derive.Facts.probe().cpus;
        be.pass_on_committer = std.mem.eql(u8, knobs.enumKnob("SQUEEZEFS_META_PASS_VENUE", opts.env), "committer");
        be.cv = try conveyor.Conveyor.init(gpa, .{
            .io = io,
            .gpa = gpa,
            .volume = be.path,
            .trees = &be.trees,
            .cache = be.cache,
            .alloc = &be.alloc,
            .core = &be.core,
            .reg = &be.reg,
            .ring_layout = be.ring_layout,
            .geo = be.geo,
            .appender_id = standing.appender_id,
            .latch = &be.lane.latch,
            .lane = .{ .ctx = be, .submit = laneSubmit, .barrier = laneBarrier },
            .pass = .{ .ctx = be, .wake = wakePass },
            .checkpoint = .{ .ctx = be, .kickPark = kickPark, .kickThreshold = kickThreshold, .checkpointNow = checkpointNowHook, .checkpointPast = checkpointPastHook, .space = &be.space_notify, .posture = &be.cp.posture, .space_retry = be.cp.spaceRetryHold() },
            .smo_journal = &be.smo_journal,
            .caps = conveyor.resolveCaps(opts.env, cpus, be.geo),
            .park_escalation_ns = conveyor.PARK_ESCALATION_NS,
            .door = be.slots.doorSeam(),
            .grant_fed = true,
            .caller = .{ .idle = exec.lane.laneIdleBesidesCaller, .polls = exec.lane.lanePolls },
            .pre_admission = .{ .ctx = &js.park_gate, .wait = joinedPreAdmission },
            .window_hold = conveyor.windowHoldFromEnv(opts.env, flush_ms, be.geo.grain),
            .applier = be.applierSeam(opts.env, cpus),
            .ack_at_apply = ackAtApply(opts.env, flush_ms),
            .tail = be.tailSeam(),
            .self_apply = std.mem.eql(u8, knobs.enumKnob("SQUEEZEFS_META_APPLY", opts.env), "self"),
        });
        errdefer be.cv.deinit();
        try be.startPass();
        errdefer be.stopPass();

        // 6. The slot plane armed JOINED (rung 6 over the wire).
        var mgr_ep: manager.Endpoint = .{};
        const manager_ep_now = js.managerEndpoint();
        const ep_bytes = std.mem.asBytes(&manager_ep_now);
        @memcpy(mgr_ep.bytes[0..ep_bytes.len], ep_bytes);
        mgr_ep.len = @intCast(ep_bytes.len);
        be.slots.armJoined(js, mgr_ep) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io, error.Deferred => error.Io,
            error.Refused, error.Corrupt => error.Corrupt,
        };
        // 7. The first cycle: the page under this identity with the rotor
        // (both directory slots), the thread, the cadence.
        be.smo_lock.lockUncancelable(io);
        const first = be.cp.checkpointNow();
        be.smo_lock.unlock(io);
        first catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
            else => return error.Corrupt,
        };
        be.ckpt_thread = std.Thread.spawn(.{}, ckptMain, .{be}) catch return error.ThreadSpawnFailed;
        be.replay.replay_ns = clock.monotonicNs() - t0;
        stats.mounts.inc();
        log.info("meta volume {s}: mounted JOINED as appender {d} — projection seq {d} (tail {d}), ring {d} B at head {d}, {d} tree(s), {d} grant extent(s) free, {s} cadence", .{ be.path, standing.appender_id, mounted.seq, mounted.journal_tail_seq, be.ring_layout.ringBytes(), be.core.headPos(), be.trees.load().len, be.alloc.freeExtents(), if (flush_ms == 0) "strict" else "coalesced" });
        return be;
    }

    /// The READER open (charter §5.5 "`-o ro` mounts are member-readers +
    /// token clients"; `tokens/ro_mount.zig`): the device read-only with
    /// no flock and no claim ("a reader claims nothing"), the newest
    /// checkpoint's record, the control tree and every slot tree at the
    /// roots that record and tree 0 name — a PROJECTION: no ring, no lane,
    /// no conveyor, no checkpointer; every mutation entry answers
    /// `PartitionRefused` (EROFS). What the reader serves off these trees
    /// is the quiescent posture's business (`sym/reader_plane.zig`); under
    /// a live writer every object is read under a token instead.
    pub fn openReader(gpa: Allocator, io: Io, path: []const u8, opts: OpenOptions) OpenError!*KvBackend {
        const t0 = clock.monotonicNs();
        const guard_fd = openReaderFd(path) catch return error.DeviceRefused;
        errdefer _ = linux.close(guard_fd);
        const mode = uring_fs.registerMetaDevice(gpa, opts.env, path) catch return error.DeviceRefused;
        const pool = opts.pool orelse (uring_fs.Pool.init(gpa, uring_fs.Config.resolve(opts.env, derive.Facts.probe())) catch return error.DeviceRefused);
        errdefer if (opts.pool == null) pool.deinit();
        const volume_len = device_loader.volumeLength(path) catch return error.DeviceRefused;
        const be = try gpa.create(KvBackend);
        errdefer gpa.destroy(be);
        const owned_path = try gpa.dupe(u8, path);
        errdefer gpa.free(owned_path);
        var dev: device_loader.DeviceLoader = .{ .gpa = gpa, .pool = pool, .path = owned_path, .mode = mode, .ledger_base = 0, .bitmap_base = 0 };
        const classified = try readSuperblock(gpa, &dev, path, opts.env, volume_len);
        const sb = classified.sb;
        if (!sb.symmetricPlaneStamped()) {
            log.err("meta volume {s}: a `-o ro` reader of a `--single-writer` volume has no token plane to read under — mount it writable or format the set symmetric", .{path});
            return error.UnsupportedFormat;
        }
        var mounted = try readNewestLedger(gpa, &dev, path, sb);
        var mounted_in_be = false;
        errdefer if (!mounted_in_be) mounted.deinit(gpa);
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
            .name_key = .init(sb.hash_seed),
            .sb_generation = classified.next_generation,
            .mounted = mounted,
            .dev = dev,
            .layout = layout,
            .cache = undefined,
            .alloc = undefined,
            .seq = node_seq.NodeSeqHandle.shared(@max(mounted.seq, mounted.node_seq_watermark)),
            .trees = undefined,
            .stamp = undefined,
            .slots = undefined,
            .ring_layout = undefined,
            .geo = undefined,
            .core = undefined,
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
            .replay = .{ .entries = 0, .dropped_torn = 0, .content_records = 0, .clean_unmount = true, .replay_ns = 0 },
            .posture = .reader,
        };
        mounted_in_be = true;
        errdefer be.mounted.deinit(gpa);
        be.dev.path = be.path;
        be.cache_budget_bytes = opts.cache_budget_bytes;
        be.cache = try node_cache.NodeCache.init(gpa, io, .{ .layout = layout, .heap_base = sb.heap.start, .budget_bytes = opts.cache_budget_bytes }, be.dev.loader());
        errdefer be.cache.deinit();
        be.cache.setDurableTail(mounted.journal_tail_seq);
        be.trees = try tree.TreeTable.init(gpa, &.{});
        errdefer {
            for (be.trees.load()) |t| t.destroy();
            be.trees.deinit();
        }
        be.stamp = try stamp_cell.StampCell.init(gpa, mounted.membership_stamp);
        errdefer be.stamp.deinit();
        errdefer {
            be.guest_published.deinit(gpa);
            be.guest_cursor_floors.deinit(gpa);
        }
        try be.openProjectionTreesRepointing();
        be.replay.replay_ns = clock.monotonicNs() - t0;
        stats.mounts.inc();
        log.info("meta volume {s}: mounted as a READER — the checkpointed projection at seq {d} (tail {d}), {d} tree(s); no flock, no claim, no ring", .{ be.path, mounted.seq, mounted.journal_tail_seq, be.trees.load().len });
        return be;
    }

    /// Every tree of the checkpoint `be.mounted` names: the ledger's own
    /// roots (the control tree, the native slot tree) and every guest slot
    /// tree tree 0's `slot_state` records root (page 0's lessee entries
    /// folded in — the two-homes law).
    fn openProjectionTrees(be: *KvBackend) (OpenError || error{RootStale})!void {
        const gpa = be.gpa;
        for (be.mounted.tree_roots) |r| {
            const t = KvTree.openFromLedgerAsReader(gpa, be.cache, &be.mounted, r.id, &be.seq) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.Io => return error.Io,
                error.RootStale => return error.RootStale,
                else => {
                    log.err("meta volume {s}: reader: ledger record {d}'s root {d} does not open ({s})", .{ be.path, be.mounted.seq, @backingInt(r.id), @errorName(e) });
                    return error.Corrupt;
                },
            };
            errdefer t.destroy();
            be.seq.raiseTo(r.node_seq);
            try be.trees.add(t);
        }
        try be.openGuestTrees(&.{}, null);
    }

    /// A reader re-points at most this often before a stale root is
    /// corruption: a writer's checkpoint cadence is tens of milliseconds, its
    /// ledger slot write (the window in which the newest record is not yet
    /// readable) shorter, so 64 steps of 2 ms outlast both.
    const READER_REPOINTS_MAX: u32 = 64;

    /// `openProjectionTrees` as the reader's race with a live writer wants
    /// it: a root the record names that a later checkpoint retired (its
    /// extent reused — `RootStale`) drops what was opened, re-points at the
    /// newest record on the device and tries again — newest-valid-wins —
    /// until a record's roots all load, or the bound: a record whose roots
    /// are gone with no newer record behind it is corruption.
    fn openProjectionTreesRepointing(be: *KvBackend) OpenError!void {
        var repoints: u32 = 0;
        while (true) {
            be.openProjectionTrees() catch |e| switch (e) {
                error.RootStale => {
                    if (repoints == READER_REPOINTS_MAX) {
                        log.err("meta volume {s}: reader: the projection's roots stayed stale through {d} re-points (record {d}) — refusing", .{ be.path, repoints, be.mounted.seq });
                        return error.Corrupt;
                    }
                    repoints += 1;
                    try be.dropProjection();
                    // Not newer yet: the writer's record is mid-write; a step later it reads.
                    if (!try be.repointNewest()) be.io.sleep(.fromMilliseconds(2), .awake) catch {};
                    continue;
                },
                else => |x| return x,
            };
            return;
        }
    }

    /// Every cached node and every opened tree of the projection dropped
    /// (the trees RETIRED from the table — destroyed with it, never under a
    /// reader mid-walk).
    fn dropProjection(be: *KvBackend) Allocator.Error!void {
        _ = be.cache.discardAllNodes();
        const old = be.trees.load();
        var i: usize = old.len;
        while (i > 0) {
            i -= 1;
            try be.trees.remove(old[i]);
        }
        be.guest_published.clearRetainingCapacity();
        be.guest_cursor_floors.clearRetainingCapacity();
    }

    /// The newest record on the device adopted as the projection's when it
    /// is newer than the mounted one (the trees are the caller's to reopen);
    /// answers whether it moved.
    fn repointNewest(be: *KvBackend) OpenError!bool {
        var fresh = try readNewestLedger(be.gpa, &be.dev, be.path, be.sb);
        if (fresh.seq <= be.mounted.seq) {
            fresh.deinit(be.gpa);
            return false;
        }
        be.mounted.deinit(be.gpa);
        be.mounted = fresh;
        be.cache.setDurableTail(fresh.journal_tail_seq);
        return true;
    }

    /// Re-point the projection at the newest checkpoint on the device (the
    /// quiescent reader adopting the writers' last checkpoint after they
    /// left): every cached node dropped (a leaf appended to in place keeps
    /// its address), the old trees RETIRED from the table (destroyed with
    /// it, never under a reader mid-walk), the record's trees opened in
    /// their place — a concurrent reader sees the old list or the new.
    /// Answers whether the record moved.
    pub fn adoptNewestCheckpoint(be: *KvBackend) OpenError!bool {
        std.debug.assert(be.posture == .reader);
        be.trees_lock.lockUncancelable(be.io);
        defer be.trees_lock.unlock(be.io);
        if (!try be.repointNewest()) return false;
        try be.dropProjection();
        try be.openProjectionTreesRepointing();
        return true;
    }

    pub fn isReader(be: *const KvBackend) bool {
        return be.posture == .reader;
    }

    /// The reader's device fd: read-only, no lock of any kind ("a reader
    /// claims nothing") — held for the mount's life like the guard fd.
    fn openReaderFd(path: []const u8) error{DeviceRefused}!linux.fd_t {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = align_probe.pathZ(&zbuf, path) catch return error.DeviceRefused;
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) {
            log.err("meta volume {s}: cannot open for reading ({t})", .{ path, linux.errno(rc) });
            return error.DeviceRefused;
        }
        return @intCast(rc);
    }

    /// The joined posture's `RecordSource.appenderPages`: the page IS the
    /// record (`sym/joined.zig`).
    fn joinedPagesHook(ctx: *anyopaque, rec: *const ledger.LedgerRecord, _: bool) checkpoint_cycle.IoError!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        return be.joined.?.onCheckpoint(rec);
    }

    fn joinedPreAdmission(ctx: *anyopaque) error{Expired}!void {
        const gate: *sym.membership.park_reclaim.ParkGate = @ptrCast(@alignCast(ctx));
        return gate.preAdmission();
    }

    /// Open the control tree at the mounted record's root (the joiner's
    /// read-only projection of tree 0); the manager's native tree is left
    /// closed (module doc of `openJoined`).
    fn openControlProjection(be: *KvBackend) OpenError!void {
        for (be.mounted.tree_roots) |r| {
            if (r.id != .control) continue;
            const t = KvTree.openFromLedger(be.gpa, be.cache, &be.mounted, r.id, &be.seq) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.Io => return error.Io,
                else => return error.Corrupt,
            };
            errdefer t.destroy();
            try be.trees.add(t);
            return;
        }
        log.err("meta volume {s}: the ledger record names no control tree — refusing the join", .{be.path});
        return error.Corrupt;
    }

    /// A joiner's re-read of the projection (before any ring of its own
    /// exists): the newest ledger record replaces the mounted one and the
    /// control tree re-opens at its root — the manager's latest
    /// checkpoint, for a control record its window still held.
    pub fn reopenControlProjection(be: *KvBackend) OpenError!void {
        std.debug.assert(be.posture == .joined and be.joined == null);
        var fresh = try readNewestLedger(be.gpa, &be.dev, be.path, be.sb);
        if (fresh.seq == be.mounted.seq) {
            fresh.deinit(be.gpa);
            return;
        }
        // The old tree AND its cached nodes go (a leaf the manager appended
        // to in place keeps its address: the cache would serve the stale
        // image); nothing else holds a node at this point of the open. The
        // record moves into `be.mounted` (the caller's unwind owns it).
        for (be.trees.load()) |t| t.destroy();
        be.trees.deinit();
        be.cache_swap.lock();
        defer be.cache_swap.unlock();
        be.cache.deinit();
        be.mounted.deinit(be.gpa);
        be.mounted = fresh;
        be.trees = try tree.TreeTable.init(be.gpa, &.{});
        be.cache = try node_cache.NodeCache.init(be.gpa, be.io, .{ .layout = be.layout, .heap_base = be.sb.heap.start, .budget_bytes = be.cache_budget_bytes }, be.dev.loader());
        be.cache.setDurableTail(fresh.journal_tail_seq);
        try be.openControlProjection();
    }

    /// The R5 component over THIS backend's live node cache (`kv_node_cache`:
    /// floor 64 MiB, weight 1, shed = a checkpoint kick). Registered through
    /// the backend, never the cache: `be.cache` is replaced by a joiner's
    /// `reopenControlProjection`, and a component holding `&cache.cached_bytes`
    /// dangled after the swap — the sampler read a freed gauge, the garbage
    /// pressure drove the joiner into Red, and its shed kicked a freed cache
    /// (the kill rows' B wrote a record with no local ino into its ring).
    pub fn budgetComponent(be: *KvBackend) mem_budget.Component {
        return mem_budget.Component.init("kv_node_cache", node_cache.KV_NODE_CACHE_FLOOR_BYTES, node_cache.KV_NODE_CACHE_SHED_WEIGHT, .{ .func = .{ .ctx = be, .call = &cacheBytesFn } }, .{ .ctx = be, .call = &cacheShedFn });
    }

    fn cacheBytesFn(ctx: *anyopaque) u64 {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.cache_swap.lock();
        defer be.cache_swap.unlock();
        return be.cache.cachedBytes();
    }

    fn cacheShedFn(ctx: *anyopaque, target: u64) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.cache_swap.lock();
        defer be.cache_swap.unlock();
        be.cache.shed(target);
    }

    /// A control record read OFF THE DEVICE at the checkpoint `rec` names,
    /// through a transient cache (never this mount's: a leaf the manager
    /// appends to in place keeps its address, so the cached image would
    /// serve the stale words) — a joiner's re-resolution of its manager's
    /// listener (charter §5.5 "re-resolve off durable state").
    pub fn lookupControlAtRecord(be: *KvBackend, gpa: Allocator, rec: *const ledger.LedgerRecord, k: []const u8) OpenError!?[]u8 {
        const cache = try node_cache.NodeCache.init(gpa, be.io, .{ .layout = be.layout, .heap_base = be.sb.heap.start, .budget_bytes = TRANSIENT_CONTROL_CACHE_BYTES }, be.dev.loader());
        defer cache.deinit();
        cache.setDurableTail(rec.journal_tail_seq);
        var seq = node_seq.NodeSeqHandle.shared(rec.node_seq_watermark);
        const t = KvTree.openFromLedger(gpa, cache, rec, .control, &seq) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        defer t.destroy();
        return t.lookup(gpa, k) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    /// A one-record read needs the control tree's root and one leaf: the
    /// floor the cache's shards derive from (a budget below one node is
    /// one shard).
    pub const TRANSIENT_CONTROL_CACHE_BYTES: u64 = 16 << 20;

    /// A slot tree that is no longer this mount's (a released lease on a
    /// joiner): out of the live set (retired with the table) — its extents
    /// are the manager's custody now, nothing here frees them.
    pub fn forgetTree(be: *KvBackend, slot: u16) void {
        be.trees_lock.lockUncancelable(be.io);
        defer be.trees_lock.unlock(be.io);
        const t = be.trees.find(.{ .slot = key.guestForestSlot(slot) }) orelse return;
        be.trees.remove(t) catch return;
        _ = be.guest_published.remove(slot);
    }

    // ---- the pass fiber (stage A on the journal lane) ----

    /// Spawn the pass as a committer fiber (cancel protection blocked — a
    /// committer is never cancelled) on the journal lane; a group member,
    /// so the lane recycles it at exit and `stopPass` joins it.
    fn startPass(be: *KvBackend) OpenError!void {
        // The committer venue has no pass fiber: the election winner runs
        // the pass itself. (A fiber here would run `runScheduledPass` on
        // its tick while a committer holds leadership mid-pass — two
        // passes over one batch buffer.)
        if (be.pass_on_committer) return;
        exec.group.addMember(&be.pass_group);
        be.pass_fiber = be.lane.execLane().spawn(.group, .{ .group = passEntry }, std.mem.asBytes(&be), &be.pass_group, .blocked) catch {
            exec.group.removeMember(&be.pass_group);
            log.err("meta volume {s}: the conveyor's pass fiber could not be spawned on the journal lane", .{be.path});
            return error.ThreadSpawnFailed;
        };
    }

    /// Stop + join the pass fiber (idempotent). The conveyor is quiesced
    /// first (nothing queued, no leader), so the fiber is parked or about
    /// to park; the stop latch is observed at its next poll.
    fn stopPass(be: *KvBackend) void {
        if (be.pass_on_committer) return;
        // release pairs with the fiber's acquire loads in `passEntry`.
        be.pass_stop.store(true, .release);
        wakePass(be);
        exec.group.awaitGroup(&be.pass_group, exec.lane.currentFiber(), &.{}) catch {};
        be.pass_fiber = null;
    }

    /// The pass fiber's loop: run the scheduled pass, re-read the leader
    /// latch, park until the next election wakes it. A wake that lands
    /// mid-poll parks the delivery word as NOTIFIED and the next park
    /// returns at once (law 1); the re-read (`leaderHeld`) catches the
    /// election whose wake the word absorbed while the fiber was already
    /// SCHEDULED — by the queue mutex's unlock when `pending()` parked on a
    /// committer's lock hold (`conveyor_core.zig` module doc; the
    /// 2026-10-10 wedge) — and no suspension point sits between it and the
    /// park. The park rides the 2 s liveness tick (`sync/tick.zig`: every
    /// unbounded wait does): a tick that finds the latch SET with no wake
    /// delivered is a lost election wake of whatever class comes next —
    /// absorbed here and counted in `meta_conveyor_park_tick_rescues` (must
    /// stay ≈ 0), never a wedge.
    fn passEntry(context: *const anyopaque) void {
        const p: *const *KvBackend = @ptrCast(@alignCast(context));
        const be = p.*;
        const f = exec.lane.currentFiber().?;
        while (true) {
            if (be.pass_stop.load(.acquire)) return;
            be.cv.runScheduledPass();
            if (be.pass_stop.load(.acquire)) return;
            if (be.cv.leaderHeld()) continue;
            f.lane.timers.arm(f, clock.monotonicNs() + sync.tick.tick_ns);
            f.park();
            const ticked = f.timerFired();
            if (!ticked) _ = f.lane.timers.cancel(f);
            if (ticked and be.cv.leaderHeld()) be.cv.noteVenueTickRescue();
        }
    }

    /// The applier venue (decision 0003 §AW; `SQUEEZEFS_META_APPLIERS`, 0 =
    /// the inline control arm, shipped): a window's RAM apply, encode and
    /// submit as a group fiber on one of N handler lanes (`sqz-fuse-ln<i>`:
    /// one per CPU, idle while the drain workers run the handlers on their
    /// own lanes; the `sqz-meta` pool where there are none — the tests —
    /// which the reclaim's destroy batches share: unlinks ran at half speed
    /// with the appliers queued behind them). A refused spawn applies
    /// inline.
    /// The applier venue (decision 0003 §AW.3): `applierParallelism(cpus)`
    /// handler lanes unless the knob names a width; 0 is the inline arm.
    fn applierSeam(be: *KvBackend, env: *const std.process.Environ.Map, cpus: u32) conveyor.ApplySeam {
        // The committer venue applies on the pass's own thread: a window
        // handed to an applier would put the venue's hop back on the path.
        if (be.pass_on_committer) return conveyor.inline_applier;
        const n = knobs.optIntKnob("SQUEEZEFS_META_APPLIERS", u32, env) orelse conveyor.applierParallelism(cpus);
        if (n == 0) return conveyor.inline_applier;
        return .{ .ctx = be, .spawn = &spawnApplier, .parallelism = n };
    }

    fn spawnApplier(ctx: *anyopaque, w: *conveyor.Window) bool {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const sqz = exec.SqzIo.fromIo(be.io);
        // A FIXED set of lanes, the venue's width wide, chosen by the
        // window's affinity (its parent-side tree): the appliers' caches
        // stay warm (the rotation over every handler lane spread the
        // windows over 32 idle threads and doubled the apply's CPU), and
        // the windows of one hot directory queue on one lane instead of
        // handing its leaf lock between lanes (a shared directory's
        // creates ran 0.84× with the round-robin).
        const fuse = sqz.fuseLanes();
        const lane = if (fuse.len == 0) sqz.homeLane(null, null) else blk: {
            const k = @min(fuse.len, be.cv.deps.applier.parallelism);
            const l = fuse[@intCast(w.affinity % k)];
            break :blk if (l.alive.load(.acquire)) l else sqz.homeLane(null, null);
        };
        sqz.spawnGroupOn(lane, &be.apply_group, conveyor.Conveyor.applyWindow, .{ be.cv, w }) catch return false;
        return true;
    }

    /// `conveyor.PassSeam.wake`: the election winner's one wake, from any
    /// thread (a committer) — same-lane callers take the ready FIFO.
    fn wakePass(ctx: *anyopaque) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        // The committer venue: the winner IS the pass, here and now — the
        // election serializes passes whatever thread runs them, and the
        // batch it drains is its own tx plus whatever queued behind it.
        if (be.pass_on_committer) return be.cv.runScheduledPass();
        const f = be.pass_fiber orelse return;
        if (f.state.wake()) f.lane.enqueue(f);
    }

    /// The dedicated guard fd (the flock's home for the mount's life; a
    /// pool-cached fd would drop the lock at LRU eviction).
    fn openGuardFd(path: []const u8) error{DeviceRefused}!linux.fd_t {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = align_probe.pathZ(&zbuf, path) catch return error.DeviceRefused;
        const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) {
            log.err("meta volume {s}: cannot open for the writer guard ({t})", .{ path, linux.errno(rc) });
            return error.DeviceRefused;
        }
        return @intCast(rc);
    }

    /// Layer A of the D0 guard: `flock(LOCK_EX | LOCK_NB)` on the dedicated
    /// fd. `EWOULDBLOCK` answers `Busy` at once, naming the on-volume claim
    /// holder when the probe can read one (B:17770-17774): a loud refusal,
    /// or — under `opts.join_door` — the normal signal that a live manager
    /// holds the volume and this mount joins it (info, never an error).
    fn acquireWriterFlock(gpa: Allocator, io: Io, path: []const u8, opts: OpenOptions) OpenError!linux.fd_t {
        const fd = try openGuardFd(path);
        errdefer _ = linux.close(fd);
        var locker: ?guard.proc.Holder = null;
        switch (guard.proc.flockExclusive(io, fd, &stats.guard_shared_lock_waits, &locker)) {
            .SUCCESS => return fd,
            .AGAIN => {
                if (!opts.join_door) stats.guard_busy_refusals.inc();
                var buf: [512]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                // Best-effort: the holder's claim through the probe (a
                // refusal path — a pool of its own when the mount owns none).
                const probe_pool: ?*uring_fs.Pool = opts.pool orelse (uring_fs.Pool.init(gpa, uring_fs.Config.resolve(opts.env, derive.Facts.probe())) catch null);
                defer if (opts.pool == null) if (probe_pool) |p| p.deinit();
                if (probe_pool) |pool| {
                    if (guard.gate.probeClaim(gpa, io, opts.env, pool, path)) |raw| {
                        defer if (raw) |r| gpa.free(r);
                        if (raw) |r| {
                            if (guard.claim.Claim.decode(r)) |c| {
                                w.writeAll(" (claim: ") catch {};
                                c.writeSummary(&w, clock.realtimeCoarseNs()) catch {};
                                w.writeAll(")") catch {};
                            } else |_| w.writeAll(" (unparseable claim value)") catch {};
                        }
                    } else |_| {}
                }
                if (opts.join_door) {
                    log.info("meta volume {s}: volume 0 is held by the manager{s}; joining as appender over the wire", .{ path, w.buffered() });
                } else {
                    var lb: [192]u8 = undefined;
                    var lw: Io.Writer = .fixed(&lb);
                    guard.proc.writeLocker(&lw, locker) catch {};
                    log.err("meta volume {s}: {s}{s} — concurrent mounts of one metadata volume are refused (single-writer guard, EBUSY)", .{ path, lw.buffered(), w.buffered() });
                }
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
        var plan = (journal.planHeadAlignment(be.gpa, &be.core, &be.reg, be.ring_layout, journal.FIXED_RING_APPENDER) catch |e| switch (e) {
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
        for (be.trees.load()) |t| if (t.ledgerRoot()) |r| try out.append(gpa, r);
    }

    /// `checkpoint_cycle.RecordSource.membershipStamp`: the stored
    /// membership with the live cursor cells folded in.
    fn stampForLedger(ctx: *anyopaque) ledger.MembershipStamp {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        var st = be.stamp.forLedger();
        // Under the symmetric plane the ledger stamp carries NO per-slot
        // cursors (spec §2.5, §9 Q6: a FLAT budget as the home of an
        // unbounded population fail-stopped a manager at 281 leases); the
        // cursor's homes are the page entry and tree 0.
        if (be.sb.symmetricPlaneStamped()) st.slot_cursors = &.{};
        return st;
    }

    fn publishGuestRootsHook(ctx: *anyopaque) checkpoint_cycle.CycleError!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        return be.publishGuestRoots();
    }

    /// The per-volume `sym` words of a volume with no appender set (the
    /// `--single-writer` class): zeros and `solo` (`sym/bind.zig`).
    pub fn writeSoloSymStatsJson(be: *KvBackend, w: *Io.Writer) Io.Writer.Error!void {
        return sym.bind.AppenderSet.writeSoloStatsJson(be, w);
    }

    /// `checkpoint_cycle.RecordSource.appenderPages`: every region's page
    /// after the ledger record (`sym/bind.zig`).
    fn appenderPagesHook(ctx: *anyopaque, rec: *const ledger.LedgerRecord, cache_clean: bool) checkpoint_cycle.IoError!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const set = be.sym orelse return;
        return set.onCheckpoint(rec, cache_clean);
    }

    /// `checkpoint_cycle.RecordSource.dataPages`: the held `KVDA` pages
    /// through the installed sink (the cycle holds `smo_lock`, the seam's
    /// own lock).
    fn dataPagesHook(ctx: *anyopaque, generation: u64) checkpoint_cycle.IoError!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const sink = be.kvda_sink orelse return;
        if (be.kvda_pages_skipped) return;
        return sink.writePages(sink.ctx, generation);
    }

    /// The holder plane's arm installs its page sink (null clears it at
    /// the stop) under the SMO lock, so a cycle in flight sees the seam
    /// whole or not at all.
    pub fn installKvdaSink(be: *KvBackend, sink: ?KvdaSink) void {
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        be.kvda_sink = sink;
    }

    /// The window's kind-5 frames kept as owned copies (module field doc).
    fn keepDataDeltas(be: *KvBackend, entries: []const journal.ReplayedEntry) Allocator.Error!void {
        for (entries) |e| {
            var it = e.records();
            while (it.next()) |f| {
                const t = journal.untag(f.tag);
                if (t.level != 0 or t.tree_id != key.TREE_DATA_ALLOC_RESERVED) continue;
                // A frame of another shape is the decoder's refusal at the
                // fold, never a reason to drop the window here.
                if (f.key.len != sym_records.DELTA_KEY_LEN or f.value.len != sym_records.DELTA_LEN) continue;
                try be.kept_data_deltas.append(be.gpa, .{ .seq = f.seq, .key = f.key[0..sym_records.DELTA_KEY_LEN].*, .value = f.value[0..sym_records.DELTA_LEN].* });
            }
        }
    }

    /// The kept window as the bitmap's replay records (borrowing the kept
    /// copies; the caller frees the slice alone).
    pub fn keptDataDeltas(be: *KvBackend, gpa: Allocator) Allocator.Error![]data_bitmap.DataBitmap.ReplayRecord {
        const out = try gpa.alloc(data_bitmap.DataBitmap.ReplayRecord, be.kept_data_deltas.items.len);
        for (be.kept_data_deltas.items, out) |*k, *o| o.* = .{ .seq = k.seq, .key = &k.key, .value = &k.value };
        return out;
    }

    /// The threshold drain's source: the trees' queues, round-robin from
    /// the tree after the last pop so a storm on one tree never starves
    /// another's items (every queue is lock-free; the task is the one popper).
    fn popMaintenance(ctx: *anyopaque) ?u64 {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const trees = be.trees.load();
        const n = trees.len;
        for (0..n) |_| {
            be.maintenance_cursor = (be.maintenance_cursor + 1) % n;
            if (trees[be.maintenance_cursor].popMaintenance()) |addr| return addr;
        }
        return null;
    }

    fn maintenancePending(ctx: *anyopaque) bool {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        for (be.trees.load()) |t| if (t.maintenancePending()) return true;
        return false;
    }

    /// A deferred item goes back on its owner's queue; a node the cache no
    /// longer maps has nothing left to maintain.
    fn requeueMaintenance(ctx: *anyopaque, addr: u64) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const n = be.cache.tryGet(addr) orelse return;
        defer n.release();
        for (be.trees.load()) |t| {
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

    /// A C6 bitmap page through the device, with the kill matrix's cut
    /// point after it.
    fn bitmapPageHook(ctx: *anyopaque, offset: u64, image: *const [alloc_ext.page_len]u8) checkpoint_cycle.IoError!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        const sink = be.dev.pageSink();
        try sink.writePage(sink.ctx, offset, image);
        if (test_seams.after_bitmap_page) |h| h.call(h.ctx);
    }

    /// A structural entry (an SMO's, the rollback's compensation) through
    /// the lane: the same aligned-run path as a user window, answered when
    /// it LANDED. The caller may be the checkpoint task's thread (a futex
    /// park), the pass fiber (a space-retry cycle) or the stage-B fiber
    /// itself (a compensation inside a verdict) — the latter two park as
    /// fibers and the lane's own CQE answers them. A full queue is the
    /// lane draining; the structural writer waits for it.
    fn smoWriteHook(ctx: *anyopaque, ops: []const journal.WriteOp) error{Io}!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        if (test_seams.before_smo_entry) |h| h.call(h.ctx);
        const t = while (true) {
            if (be.lane.submitStructural(ops)) |t| break t else |e| {
                if (e != error.LaneQueueFull) return error.Io;
                std.Thread.yield() catch {};
            }
        };
        t.wait().result catch return error.Io;
    }

    /// A reservation completed outside stage B's groups (a structural
    /// entry, an abandoned window): kick the lane (a stage-B prefix wait
    /// may sit behind it).
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

    /// §BB.4 under §BC: the conveyor's reservation high-water mark.
    fn laneCoverHook(ctx: ?*anyopaque) u64 {
        const be: *KvBackend = @ptrCast(@alignCast(ctx.?));
        return be.cv.reservedHwm();
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

    /// §BB.1: the ack point — at the apply under the coalesced cadence
    /// (`SQUEEZEFS_META_ACK_POINT=apply`, shipped), at the lane's verdict
    /// under strict mode or the `landing` arm.
    fn ackAtApply(env: *const std.process.Environ.Map, flush_ms: u64) bool {
        return flush_ms != 0 and std.mem.eql(u8, knobs.enumKnob("SQUEEZEFS_META_ACK_POINT", env), "apply");
    }

    /// The tail: a memfd of the ring's size plus a slot table of twice the
    /// lane's cells. A refusal is the open's (a box that cannot give the
    /// tail its memory cannot keep the crash law).
    fn openTail(path: []const u8, layout: journal.RingLayout, geo: jc.CoreGeometry) OpenError!journal_tail.linux_tail.Tail {
        return journal_tail.linux_tail.Tail.create(.{
            .layout = layout,
            .grain = geo.grain,
            .path = path,
            .n_slots = @intCast(2 * journal_lane.inboundCap(layout.pages)),
        }) catch |e| {
            log.err("meta volume {s}: cannot create the journal tail ({s})", .{ path, @errorName(e) });
            return error.DeviceRefused;
        };
    }

    fn tailSeam(be: *KvBackend) ?conveyor.TailSeam {
        if (be.tail == null) return null;
        return .{ .ctx = be, .claim = tailClaim, .release = tailRelease };
    }

    fn tailClaim(ctx: *anyopaque, seq_base: u64, ops: []const journal.WriteOp) ?u32 {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        return be.tail.?.claim(seq_base, ops);
    }

    fn tailRelease(ctx: *anyopaque, slot: u32) void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.tail.?.release(slot);
    }

    fn laneSubmit(ctx: *anyopaque, w: *conveyor.Window) Error!void {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        be.lane.submitWriteBatchDetached(.{
            .window = w.res,
            .seq_base = w.seq_base,
            .ops = w.ops(),
            .entries = w.entries,
            .acct = w.acct(),
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
    /// discipline; the conveyor's abandon arm). Never a WAIT for the lock:
    /// the caller is the lane's stage-B fiber — the one that lands every
    /// structural entry — and a holder parked on such an entry (the
    /// checkpointer mid-SMO in `smoWriteHook`) waits on this very fiber,
    /// so the wait is a deadlock (xfstests generic/753 under dm-error: every
    /// committer parked for the kernel's 135 s). Held ⇒ the holder's own
    /// cycle covers the hole; this cover fails loud and the lane escalates.
    fn checkpointPast(be: *KvBackend, pos: u64) bool {
        if (!be.smo_lock.tryLock()) {
            log.warn("meta volume {s}: the journal hole ending at {d} is not covered from the lane — the SMO lock is held (a checkpoint cycle in flight, parked on this lane); its own cycle covers it", .{ be.path, pos });
            return false;
        }
        defer be.smo_lock.unlock(be.io);
        be.cp.checkpointPast(pos) catch return false;
        return true;
    }

    // ---- guest slot trees (charter §5.5: roots in tree 0, minted lazily) ----

    /// The forest slot of routing slot `slot` on this volume: the native
    /// tree iff the stamp names it native, else the guest tree `slot + 1`.
    fn forestSlotOf(be: *const KvBackend, slot: u16) key.ForestSlot {
        if (be.stamp.native_slot) |ns| if (ns == slot) return key.NATIVE_FOREST_SLOT;
        return key.guestForestSlot(slot);
    }

    /// Open every guest tree tree 0 names (the replayed control records are
    /// in RAM), then mint an EMPTY tree for every guest slot the replay
    /// window mentions without a published root (a tree created in the
    /// crashed daemon's last window: its records are in the ring, its root
    /// never reached tree 0 — the recreated empty tree replays identically).
    /// `recovered`: the writer's allocator as the replay folded it — the
    /// named roots' claims are adopted into it before the first mint; null
    /// on a projection (it claims nothing; its allocator is not the bitmap).
    fn openGuestTrees(be: *KvBackend, entries: []const journal.ReplayedEntry, recovered: ?*alloc_ext.ExtentAllocator) (OpenError || error{RootStale})!void {
        const gpa = be.gpa;
        const control = be.trees.find(.control) orelse {
            log.err("meta volume {s}: no control tree among the opened roots", .{be.path});
            return error.Corrupt;
        };
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        const scan = slot_state.scanRange();
        // A READER walks tree 0 and reads page 0 live, beside the writers:
        // a node retired under the walk, a page mid-write, the two homes
        // read at different instants — each is its race (`RootStale`: it
        // re-points), never the writer's loud refusal.
        const reader = be.posture == .reader;
        control.range(gpa, scan.start.slice(), scan.end.slice(), std.math.maxInt(usize), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => if (reader) error.RootStale else error.Corrupt,
        };
        // Appender 0's page: the lessee's home of every slot it names —
        // at least as new as tree 0's grant-time word (the two-homes law);
        // a Live entry at tree 0's generation under another lessee is C14.
        const page0 = be.readAppender0Page() catch |e| switch (e) {
            error.Corrupt => return if (reader) error.RootStale else error.Corrupt,
            else => |x| return x,
        };
        var roots: std.ArrayList(slot_state.GuestRoot) = .empty;
        defer roots.deinit(gpa);
        slot_state.foldGuestRoots(gpa, kvs.items, page0, &roots) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Corrupt => {
                if (reader) return error.RootStale;
                log.err("meta volume {s}: a tree-0 slot_state record does not decode", .{be.path});
                return error.Corrupt;
            },
            error.Conflict => {
                if (reader) return error.RootStale;
                log.err("meta volume {s}: a slot is Live on appender 0's page at tree 0's generation under another lessee — refusing the mount (C14; `squeezefs appender clear`)", .{be.path});
                return error.Corrupt;
            },
        };
        // Trees another appender leases (live, or dead until the recovery
        // installs them): projections here, never walked (§U.1).
        var projections: std.ArrayList(key.ForestSlot) = .empty;
        defer projections.deinit(gpa);
        for (roots.items) |r| {
            try be.guest_cursor_floors.put(gpa, r.slot, r.cursor);
            // The native slot's lease names the ledger's own native root
            // (KD-SYM-2): the tree is already open under its native
            // identity; a second KvTree over the same nodes would hold a
            // root pointer no SMO of the native tree ever moves.
            if (be.mounted.membership_stamp.native_slot == r.slot) continue;
            // A granted, never-minted slot owns no extent yet.
            if (r.root.addr == 0) continue;
            const root: tree.RootPtr = .{ .addr = r.root.addr, .seq = r.root.seq };
            if (r.lessee) |a| if (a != journal.FIXED_RING_APPENDER) try projections.append(gpa, key.guestForestSlot(r.slot));
            const t = KvTree.openAsReader(gpa, be.cache, .{ .slot = key.guestForestSlot(r.slot) }, root, &be.seq) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.Io => return error.Io,
                // A writer's open logs and refuses the stale root itself.
                error.RootStale => if (be.posture == .reader) return error.RootStale else return error.Corrupt,
                else => {
                    log.err("meta volume {s}: slot {d}'s root 0x{x} does not open ({s})", .{ be.path, r.slot, root.addr, @errorName(e) });
                    return error.Corrupt;
                },
            };
            errdefer t.destroy();
            t.seedNodeCount(r.extents);
            be.seq.raiseTo(root.seq);
            try be.trees.add(t);
            try be.guest_published.put(gpa, r.slot, root);
        }
        // Before the first claim (the mints below).
        if (recovered) |a| try be.adoptReachableClaims(a, projections.items);
        // Slots the window names without a tree.
        var mentioned = std.AutoHashMapUnmanaged(u16, void).empty;
        defer mentioned.deinit(gpa);
        for (entries) |e| {
            var it = e.records();
            while (it.next()) |f| {
                const t = journal.untag(f.tag);
                if (t.level > 0 or !key.isSlotTreeKind(t.tree_id)) continue;
                const fs = key.checkSlotTreeKey(t.tree_id, f.key) catch continue;
                if (fs == key.NATIVE_FOREST_SLOT) continue;
                try mentioned.put(gpa, routing.route_table.slotOfForestSlot(fs), {});
            }
        }
        var it = mentioned.keyIterator();
        while (it.next()) |slot| {
            if (be.trees.find(.{ .slot = key.guestForestSlot(slot.*) }) != null) continue;
            // A re-mint's records are the replayed window's: its floor is the window's start.
            (try be.mintGuestTree(slot.*)).setRootFloor(be.core.reusableUpto());
        }
    }

    /// Every root the recovered state names is live, and so is every extent
    /// it reaches: before the mount's first claim (the re-mints below),
    /// every opened tree is walked and each reachable extent the recovered
    /// allocator holds free is adopted. A claim made in RAM reaches the
    /// device only with a later C6 bitmap page while the tree holding it is
    /// named at once (appender 0's page refresh at C4b, C11's page plan, a
    /// tree-0 record): a guest root minted on first touch (one extent, no
    /// journaled delta), and every node of a builder-built tree
    /// (`copySlotTree`) — C6 writes its pages in page order, the builder
    /// claims its root last, and a release landing mid-build drops the
    /// lowest-first cursor, so a kill inside C6 can land the root's page
    /// without a leaf's (`rung-10-routing-loop.md`). Cost: one read per
    /// interior node of every tree, leaves never read (their parents name
    /// them) — the walk's extents and wall are `meta_kv_mount_reach_walk_*`.
    /// `projections`: the forest slots another appender leases — skipped
    /// (§U.1: a lessee's tree is its lessee's to judge; its extents sit in
    /// that lessee's grant, whose bits the grant's entry journals, and the
    /// handover's cycle lands a holder's claims before the grant's entry
    /// moves its images).
    fn adoptReachableClaims(be: *KvBackend, alloc: *alloc_ext.ExtentAllocator, projections: []const key.ForestSlot) OpenError!void {
        const t0 = clock.monotonicNs();
        var extents: std.ArrayList(u64) = .empty;
        defer extents.deinit(be.gpa);
        var walked: u64 = 0;
        for (be.trees.load()) |t| {
            if (t.forest_slot) |fs| if (std.mem.indexOfScalar(key.ForestSlot, projections, fs) != null) continue;
            extents.clearRetainingCapacity();
            be.collectTreeExtents(t, &extents) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Corrupt,
            };
            walked += extents.items.len;
            var adopted: usize = 0;
            for (extents.items) |ext| adopted += @intFromBool(alloc.adoptLive(ext));
            if (adopted == 0) continue;
            const root = t.rootPtr();
            log.warn("meta volume {s}: slot tree {?d} (root 0x{x}@{d}) reaches {d} extent(s) the recovered allocator held free — a kill between their claims and the bitmap pages naming them; adopted (meta_kv_replay_root_claims_adopted)", .{ be.path, t.forest_slot, root.addr, root.seq, adopted });
        }
        alloc_ext.meta_kv_mount_reach_walk_extents.set(walked);
        alloc_ext.meta_kv_mount_reach_walk_ns.set(clock.monotonicNs() - t0);
    }

    /// Appender 0's newest valid page (null on a flat volume or a blank
    /// rotation): read at open BEFORE the symmetric plane binds, for the
    /// slot roots it names.
    fn readAppender0Page(be: *KvBackend) OpenError!?sym.appender_page.Page {
        if (!be.sb.symmetricPlaneStamped()) return null;
        const offs = sym.appender_page.appender0PageOffsets(be.sb.journal.start);
        var bufs: [4][sym.appender_page.PAGE_LEN]u8 = undefined;
        var imgs: [4]?[]const u8 = .{ null, null, null, null };
        for (offs, 0..) |off, i| {
            const img = be.dev.readRange(be.gpa, off, sym.appender_page.PAGE_LEN) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
            };
            defer be.gpa.free(img);
            @memcpy(&bufs[i], img[0..sym.appender_page.PAGE_LEN]);
            imgs[i] = &bufs[i];
        }
        return sym.appender_page.newestValid(imgs) catch return error.UnsupportedFormat;
    }

    /// The tree of routing slot `slot` on this volume, if it exists.
    pub fn slotTree(be: *KvBackend, slot: u16) ?*KvTree {
        return be.trees.find(.{ .slot = be.forestSlotOf(slot) });
    }

    /// The tree of routing slot `slot`, minted on first touch: an empty
    /// root in a fresh extent (its image written now, barriered and named
    /// in tree 0 by the next checkpoint's C1b; a crash before that recreates
    /// it from the window). Serialized on `trees_lock`; a concurrent first
    /// toucher finds the winner's tree.
    pub fn mintGuestTree(be: *KvBackend, slot: u16) OpenError!*KvTree {
        const identity: tree.TreeIdentity = .{ .slot = key.guestForestSlot(slot) };
        if (be.trees.find(identity)) |t| return t;
        be.trees_lock.lockUncancelable(be.io);
        defer be.trees_lock.unlock(be.io);
        if (be.trees.find(identity)) |t| return t;
        const t = KvTree.create(be.gpa, be.cache, &be.alloc, identity, &be.seq) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.Io => return error.Io,
            error.NoSpace => {
                log.err("meta volume {s}: no heap extent for slot {d}'s first tree (heap full)", .{ be.path, slot });
                return error.Io;
            },
            else => return error.Corrupt,
        };
        errdefer t.destroy();
        // Its records are this commit's and later: none sits under the head.
        t.setRootFloor(be.core.headPos());
        try be.trees.add(t);
        routing.stats.stats.routing_guest_trees_minted.inc();
        return t;
    }

    /// A guest tree built elsewhere (the migration engine's bulk copy)
    /// joins the live set; its root is published at the next C1b (or the
    /// engine's explicit `checkpointNow`).
    pub fn adoptTree(be: *KvBackend, t: *KvTree) Allocator.Error!void {
        be.trees_lock.lockUncancelable(be.io);
        defer be.trees_lock.unlock(be.io);
        t.setRootFloor(be.core.headPos());
        try be.trees.add(t);
    }

    /// Drop a guest tree (the flip's source teardown, a re-run's residue
    /// wipe): the tree leaves the live set, its `slot_state` record is
    /// deleted through a checkpoint-class entry, and every node's extent
    /// enters pending-free gated on that entry's position — O(nodes), the
    /// SMO pending-free protocol (charter §5.3), never a record-at-a-time
    /// delete.
    pub fn dropTree(be: *KvBackend, slot: u16) Error!void {
        const identity: tree.TreeIdentity = .{ .slot = key.guestForestSlot(slot) };
        be.trees_lock.lockUncancelable(be.io);
        defer be.trees_lock.unlock(be.io);
        const t = be.trees.find(identity) orelse return;
        var extents: std.ArrayList(u64) = .empty;
        defer extents.deinit(be.gpa);
        try be.collectTreeExtents(t, &extents);
        try be.trees.remove(t);
        const gate = try be.writeSlotStateEntry(slot, null);
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        for (extents.items) |ext| be.smo_ctx.freePending(ext, gate, true) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => return error.Corrupt,
        };
        _ = be.guest_published.remove(slot);
    }

    /// Every node of `t` (the root, every interior, every leaf) as heap
    /// extents, through the tree's own latch-free snapshots. Only interior
    /// nodes are read: a leaf's extent is its parent's pointer (a slot
    /// lease's custody move walks a tree it may have to read from the
    /// device — the interior share is < 1 % of the images at the shipped
    /// fan-out).
    pub fn collectTreeExtents(be: *KvBackend, t: *KvTree, out: *std.ArrayList(u64)) Error!void {
        const gpa = be.gpa;
        const Frame = struct { addr: u64, level: u8 };
        var stack: std.ArrayList(Frame) = .empty;
        defer stack.deinit(gpa);
        const root_level = t.rootLevel() catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        try stack.append(gpa, .{ .addr = t.rootPtr().addr, .level = root_level });
        while (stack.pop()) |f| {
            try out.append(gpa, be.cache.addrExtent(f.addr));
            if (f.level == 0) continue;
            const n = t.getNode(f.addr) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Corrupt,
            };
            defer n.release();
            if (n.level != f.level) return error.Corrupt;
            const snap = n.snapshotLoad();
            defer snap.release();
            var pos = key.KeyBuf.fromSlice("");
            while (true) {
                var live = (snap.value.nextLive(gpa, pos.slice(), null) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Corrupt,
                }) orelse break;
                defer live.deinit();
                const iv = key.InteriorValue.decode(live.value) catch return error.Corrupt;
                try stack.append(gpa, .{ .addr = iv.childAddr(), .level = f.level - 1 });
                if (!key.lessThan(live.key, &tree.KEY_SPACE_MAX)) break;
                pos = key.keySuccessor(live.key);
            }
        }
    }

    /// Every leaf of `t` with its flushed tail offset (the `slot_tails` a
    /// release records): resident leaves off RAM, evicted ones loaded once.
    pub fn collectLeafTails(be: *KvBackend, t: *KvTree, out: *std.ArrayList(sym.control_records.TailEntry)) Error!void {
        const gpa = be.gpa;
        var stack: std.ArrayList(u64) = .empty;
        defer stack.deinit(gpa);
        try stack.append(gpa, t.rootPtr().addr);
        while (stack.pop()) |addr| {
            const n = t.getNode(addr) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Corrupt,
            };
            defer n.release();
            if (n.level == 0) {
                n.lock.lockSharedUncancelable(be.io);
                const tail: u32 = @intCast(@min(n.dirty.tailOffset(), std.math.maxInt(u32)));
                n.lock.unlockShared(be.io);
                try out.append(gpa, .{ .leaf_addr = addr, .tail = tail });
                continue;
            }
            const snap = n.snapshotLoad();
            defer snap.release();
            var pos = key.KeyBuf.fromSlice("");
            while (true) {
                var live = (snap.value.nextLive(gpa, pos.slice(), null) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Corrupt,
                }) orelse break;
                defer live.deinit();
                const iv = key.InteriorValue.decode(live.value) catch return error.Corrupt;
                try stack.append(gpa, iv.childAddr());
                if (!key.lessThan(live.key, &tree.KEY_SPACE_MAX)) break;
                pos = key.keySuccessor(live.key);
            }
        }
    }

    /// Publish every guest root whose durable copy is stale (C1b). Under
    /// an armed slot plane the leased slots are the plane's (the page
    /// homes, the overflow law — `slots/plane.zig` `publishBeforeTail`);
    /// the UNLEASED trees stay the manager's structural custody here.
    fn publishGuestRoots(be: *KvBackend) checkpoint_cycle.CycleError!void {
        if (be.slots.armed) be.slots.publishBeforeTail() catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Corrupt,
            else => error.Io,
        };
        for (be.trees.load()) |t| {
            const fs = t.forest_slot orelse continue;
            if (t.identity.rootId() != null) continue;
            const slot = routing.route_table.slotOfForestSlot(fs);
            if (be.slots.armed and be.slots.table.view(slot).state != .unleased) continue;
            const root = t.rootPtr();
            if (be.guest_published.get(slot)) |p| if (p.addr == root.addr and p.seq == root.seq) continue;
            _ = be.writeSlotStateEntry(slot, root) catch |e| switch (e) {
                // A ring a storm filled: the root stays pending (the next C4b
                // retries), and `unpublishedGuestFloor` holds the tail under it.
                error.ConveyorFull => {
                    log.debug("meta volume {s}: slot {d}'s root publish deferred (the checkpoint class is full)", .{ be.path, slot });
                    continue;
                },
                else => return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.VolumeFailed => error.VolumeFailed,
                    error.Io, error.JournalWriteFailed => error.Io,
                    else => error.Corrupt,
                },
            };
            try be.guest_published.put(be.gpa, slot, root);
        }
        if (test_seams.after_guest_roots) |h| h.call(h.ctx);
    }

    /// The C8 input: the lowest `rootFloor` among the guest roots
    /// `publishGuestRoots` would publish (the same filter), `no_floor` when
    /// tree 0 names every one.
    fn unpublishedGuestFloor(ctx: *anyopaque) u64 {
        const be: *KvBackend = @ptrCast(@alignCast(ctx));
        var floor: u64 = node_cache.no_floor;
        for (be.trees.load()) |t| {
            const fs = t.forest_slot orelse continue;
            if (t.identity.rootId() != null) continue;
            const slot = routing.route_table.slotOfForestSlot(fs);
            if (be.slots.armed and be.slots.table.view(slot).state != .unleased) continue;
            const root = t.rootPtr();
            if (be.guest_published.get(slot)) |p| if (p.addr == root.addr and p.seq == root.seq) continue;
            floor = @min(floor, t.rootFloor());
        }
        return floor;
    }

    const StampCtx = struct {
        j: *smo.SmoJournal,
        adm: jc.Admission,
        registered: ?jc.InflightRegistry.Registered = null,

        fn stamp(ctx: *anyopaque) Allocator.Error!tree.Stamp {
            const self: *StampCtx = @ptrCast(@alignCast(ctx));
            const r = try self.j.reserve(self.adm);
            self.registered = r;
            return .{ .seq = r.seq_base, .entry_start = r.res.start };
        }
    };

    /// ONE checkpoint-class control entry: `Put(slot_state:{s}, root)`
    /// (or its `Delete` when `root` is null), admitted from the reserve
    /// (never behind user commits), reserved INSIDE the control leaf's lock
    /// window, applied, written through the lane and answered when it
    /// landed. Returns the entry's position (the pending-free gate of a
    /// teardown). The one-record shape of the SMO's own commit.
    fn writeSlotStateEntry(be: *KvBackend, slot: u16, root: ?tree.RootPtr) Error!u64 {
        const cursor = be.stamp.cursorSnapshot(slot) orelse inos.LOCAL_INO_BASE;
        // An unleased tree keeps the generation tree 0 already names (a
        // republish never resets `g` under a live lease history).
        var st = slot_state.unleased(if (root) |r| r.addr else 0, if (root) |r| r.seq else 0, cursor);
        st.g = be.slots.table.gOf(slot);
        return be.writeSlotStateRecord(slot, if (root != null) st else null);
    }

    /// The lease plane's form: the record verbatim (`Leased`/`Unleased` at
    /// the plane's words) or its `Delete`.
    pub fn writeSlotStateRecord(be: *KvBackend, slot: u16, state: ?slot_state.SlotState) Error!u64 {
        const control = be.trees.find(.control) orelse return error.Corrupt;
        const ck = slot_state.keyOf(slot);
        const k = ck.slice();
        const value: []const u8 = if (state) |st| &slot_state.encode(st) else &.{};
        const kind: record.RecordKind = if (state != null) .put else .delete;
        var frames = [_]journal.RecordFrame{.{ .tag = key.tagFor(key.TREE_CONTROL, 0), .key = k, .kind = @fromBackingInt(@intCast(@backingInt(kind))), .seq = 0, .value = value }};
        const len = journal.entryLenFor(&frames) catch return error.Corrupt;
        if (test_seams.refuse_guest_publish) return error.ConveyorFull;
        const adm = be.core.tryAdmit(len, .checkpoint) orelse return error.ConveyorFull;
        var st: StampCtx = .{ .j = &be.smo_journal, .adm = adm };
        var attempt: u32 = 0;
        while (attempt < node.RETRY_BUDGET) : (attempt += 1) {
            const leaf = control.resolveLeaf(k) catch |e| {
                be.core.release(adm);
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                    else => error.Corrupt,
                };
            };
            defer leaf.release();
            const outcome = control.applyAtSeqClass(leaf, k, kind, value, .{ .reserve = .{ .ctx = &st, .stamp = StampCtx.stamp } }, true) catch |e| {
                be.core.release(adm);
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                    else => error.Corrupt,
                };
            };
            switch (outcome) {
                .applied => {
                    const r = st.registered.?;
                    frames[0].seq = r.seq_base;
                    be.smo_journal.commitEntry(r.res, &frames) catch return error.JournalWriteFailed;
                    return r.res.start;
                },
                .stale => continue,
            }
        }
        be.core.release(adm);
        log.err("meta volume {s}: the slot_state entry for slot {d} exhausted its lock-revalidate budget (SMO protocol bug)", .{ be.path, slot });
        return error.Corrupt;
    }

    /// Mint a local ino in routing slot `slot`: the native watermark on
    /// the native slot, the slot's cursor cell otherwise. The slot's tree
    /// is minted on first touch so the record's commit routes.
    pub fn allocateInoInSlot(be: *KvBackend, slot: u16) OpenError!u64 {
        if (be.stamp.native_slot) |ns| if (ns == slot) return be.ino.allocate();
        if (be.slotTree(slot) == null) _ = try be.mintGuestTree(slot);
        const cell = try be.stamp.cursorFor(slot);
        return cell.allocate();
    }

    pub fn setEpoch(be: *const KvBackend) u64 {
        return be.stamp.set_epoch;
    }

    pub fn setUuid(be: *const KvBackend) [16]u8 {
        return be.stamp.set_uuid;
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
        if (be.posture == .reader) {
            t.deinit();
            return error.PartitionRefused;
        }
        return be.cv.commit(t);
    }

    pub fn commitGroup(be: *KvBackend, txs: []const *KvTx, results: []Outcome) void {
        if (be.posture == .reader) {
            for (txs, results) |t, *r| {
                t.deinit();
                be.gpa.destroy(t);
                r.* = error.PartitionRefused;
            }
            return;
        }
        be.cv.commitGroup(txs, results);
        for (txs) |t| be.gpa.destroy(t);
    }

    pub fn treeOf(be: *KvBackend, identity: tree.TreeIdentity) ?*KvTree {
        return be.trees.find(identity);
    }

    /// The tree a `(kind, key)` routes to (`tree.route`, the ONE law).
    fn treeForKey(be: *KvBackend, kind: u8, k: []const u8) ?*KvTree {
        return tree.routeContent(be.trees.load(), kind, k);
    }

    /// Point lookup through the tree (latch-free); an owned copy.
    pub fn lookup(be: *KvBackend, gpa: Allocator, kind: u8, k: []const u8) Error!?[]u8 {
        const t = be.treeForKey(kind, k) orelse return error.Corrupt;
        return t.lookup(gpa, k) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    /// Range scan over `[start, end]` of one kind; both bounds route to the
    /// same slot tree (a slot IS a contiguous key range). The block-refs
    /// kind is the ONE exception: a ref's slot is its OWNER's (`06 ‖ vol_tag
    /// ‖ block_idx ‖ owner_ino ‖ …` routes by the owner at a fixed offset),
    /// so a `(vol_tag, block_idx)` prefix range spans EVERY slot tree this
    /// backend holds — the mount seed, the refcount probe and the census
    /// read them all, k-way merged in key order (`rangeRefs`).
    pub fn range(be: *KvBackend, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
        if (kind == key.TREE_BLOCK_REFS) return be.rangeRefs(gpa, start, end, max, out);
        const t = be.treeForKey(kind, start) orelse return error.Corrupt;
        if (be.treeForKey(kind, end) != t) return error.Corrupt;
        t.range(gpa, start, end, max, out) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    /// The refs range over every slot tree, PAGED soundly: each tree is
    /// asked for `max`; when any tree filled its page the merged page is
    /// cut at the SMALLEST such tree's last key (every key below it from
    /// every tree is in hand; the keys above return on the caller's next
    /// page from the cursor it derives from the last key), so no ref is
    /// skipped across the tree boundary. Sorted ascending.
    fn rangeRefs(be: *KvBackend, gpa: Allocator, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) Error!void {
        const first = out.items.len;
        var bound: ?[]const u8 = null;
        for (be.trees.load()) |t| {
            if (t.forest_slot == null) continue;
            const before = out.items.len;
            t.range(gpa, start, end, max, out) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Corrupt,
            };
            const got = out.items.len - before;
            if (got == max and max != std.math.maxInt(usize)) {
                const last = out.items[out.items.len - 1].key;
                if (bound == null or key.lessThan(last, bound.?)) bound = last;
            }
        }
        const page = out.items[first..];
        core_sort.stable(tree.KeyValue, page, {}, struct {
            fn lt(_: void, a: tree.KeyValue, b: tree.KeyValue) bool {
                return key.lessThan(a.key, b.key);
            }
        }.lt);
        if (bound) |b| {
            var keep = page.len;
            while (keep > 0 and key.lessThan(b, page[keep - 1].key)) : (keep -= 1) {
                var kv = page[keep - 1];
                kv.deinit(gpa);
            }
            out.items.len = first + keep;
        }
    }

    /// `fsync`: the trailing coalesced barrier (every acked entry durable on
    /// return).
    pub fn fsync(be: *KvBackend) Error!void {
        if (be.posture == .reader) return;
        return be.cv.barrier();
    }

    /// One full cycle with an immediate barrier.
    pub fn checkpointNow(be: *KvBackend) checkpoint_cycle.CycleError!void {
        std.debug.assert(be.posture != .reader);
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        return be.cp.checkpointNow();
    }

    // @anchor:ops_defrag_meta_faces — the D4 arm's two faces (rung 10).

    /// The resident node census (`.stats`: the D4 before/after words): live
    /// leaves and interior nodes of every tree mapped in the cache — no
    /// device read, so an evicted node is not counted.
    pub const NodeCensus = struct { leaves: u64 = 0, interior: u64 = 0 };

    pub fn residentNodeCensus(be: *KvBackend) NodeCensus {
        const Count = struct {
            out: NodeCensus = .{},
            fn visit(self: *@This(), n: *node_cache.CachedNode) void {
                if (n.state.isSuperseded()) return;
                if (n.level == 0) self.out.leaves += 1 else self.out.interior += 1;
            }
        };
        var c: Count = .{};
        be.cache.forEachNode(&c, Count.visit);
        return c.out;
    }

    /// D4 (`defrag --meta`, `ops_jobs_lifecycle.md` §3.1): under the SMO
    /// lock — the same serialization every structural task runs in — the
    /// resident leaves of every tree are measured
    /// (`leaf_merge.leafOccupancy`), the ones at or past
    /// `smo.DEFRAG_COMPACT_DEAD_PERMILLE` ranked by the bytes a compaction
    /// reclaims, and up to `n` of them compact through the §4.6 replacement
    /// (`smo.compactForDefrag`). A refusal a barriered cycle remedies (the
    /// FIFO valve, the ring reserve) runs ONE cycle here and retries the
    /// node once; the compaction floor (`NoSpace`) ends the chunk — the
    /// next cycle returns extents. Returns the count compacted.
    pub fn defragNudgeCompactions(be: *KvBackend, gpa: Allocator, n: u32) Allocator.Error!u32 {
        std.debug.assert(be.posture != .reader);
        const Cand = struct { t: *tree.KvTree, addr: u64, dead: u64 };
        var cands: std.ArrayList(Cand) = .empty;
        defer cands.deinit(gpa);
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        for (be.trees.load()) |t| {
            const leaves = try leaf_merge.residentNodesAt(t, gpa, 0, "");
            defer gpa.free(leaves);
            for (leaves) |l| {
                const c = be.cache.tryGet(l.addr) orelse continue;
                defer c.release();
                if (c.state.isSuperseded() or !t.owns(c)) continue;
                const occ = leaf_merge.leafOccupancy(t, c);
                if (occ.deadPermille() < smo.DEFRAG_COMPACT_DEAD_PERMILLE) continue;
                try cands.append(gpa, .{ .t = t, .addr = l.addr, .dead = occ.deadBytes() });
            }
        }
        std.mem.sort(Cand, cands.items, {}, struct {
            fn more(_: void, a: Cand, b: Cand) bool {
                return a.dead > b.dead;
            }
        }.more);
        var compacted: u32 = 0;
        for (cands.items) |c| {
            if (compacted >= n) break;
            const node_ = be.cache.tryGet(c.addr) orelse continue;
            defer node_.release();
            var out: smo.MaintenanceOutcome = .{};
            var retried = false;
            while (true) {
                const did = smo.compactForDefrag(c.t, &be.smo_ctx, node_, &out) catch |e| switch (e) {
                    error.NoSpace => return compacted,
                    error.PendingFreeFull, error.JournalReserveExhausted => {
                        if (retried) break;
                        retried = true;
                        be.cp.checkpointNow() catch break;
                        continue;
                    },
                    error.OutOfMemory => return error.OutOfMemory,
                    else => {
                        log.warn("defrag --meta: leaf 0x{x} on {s} did not compact ({s}); left to the flush pass", .{ c.addr, be.path, @errorName(e) });
                        break;
                    },
                };
                if (did) compacted += 1;
                break;
            }
        }
        return compacted;
    }

    /// D4's merge lap (`defrag --meta`): ONE whole lap of the §4.6a sweep
    /// per tree, a tree's lap completing before the next tree's starts —
    /// bounded calls of one tick period each under the SMO lock, the lock
    /// DROPPED between calls so a committer's space retry or the cadence's
    /// cycle interleaves, the FIFO valve KEPT (the cycle's sweep runs
    /// forced; a maintenance job does not) and answered the way the D4 arm
    /// does: a barriered cycle covers the pending frees, the parked cursor
    /// resumes. Returns the merges + root collapses of the lap.
    pub fn defragMergeLap(be: *KvBackend) u64 {
        std.debug.assert(be.posture != .reader);
        var merged: u64 = 0;
        const budget_ns = checkpoint.mergeSweepBudgetMs(be.cp.cfg.flush_interval_ms) * std.time.ns_per_ms;
        // A lap projects every resident node once per level and once in
        // its count; two calls per resident node bounds the laps a tree the
        // budget keeps cutting can take — past it the lap is the cadence's.
        var calls: u64 = 0;
        const calls_max: u64 = @max(16, be.cache.mappedCount() * 2);
        for (be.trees.load()) |t| {
            while (calls < calls_max) : (calls += 1) {
                be.smo_lock.lockUncancelable(be.io);
                const s = leaf_merge.mergeUnderfull(t, &be.smo_ctx, false, clock.monotonicNs() + budget_ns) catch |e| {
                    be.smo_lock.unlock(be.io);
                    log.warn("defrag --meta: the merge lap on {s} stopped ({s}); the cursor stands for the cadence's sweep", .{ be.path, @errorName(e) });
                    return merged;
                };
                be.smo_lock.unlock(be.io);
                merged += s.outcome.merges + s.outcome.root_collapses;
                if (s.lap_complete) break;
                if (s.refusal != null) be.checkpointNow() catch return merged;
            }
        }
        return merged;
    }

    pub fn allocateIno(be: *KvBackend) u64 {
        return be.ino.allocate();
    }

    pub fn nextIno(be: *const KvBackend) u64 {
        return be.ino.snapshot();
    }

    pub fn nameKey(be: *const KvBackend) *const key.NameKey {
        return &be.name_key;
    }

    /// The block-map tree's self-arming ratchet (charter §5.6; finding 43):
    /// stamp `BLOCK_MAP_TREE` at the FIRST crossing — both superblock copies
    /// at the next generation, each barriered — BEFORE any kind-7 record can
    /// become durable, so an older binary refuses the volume loud instead of
    /// reading a map it cannot resolve. Idempotent; an untouched volume
    /// stays byte-identical. A failed stamp refuses the crossing (the write
    /// keeps its custody and retries) — never a silent degrade.
    pub fn stampBlockMapTree(be: *KvBackend) Error!void {
        if (be.sb.blockMapTreeStamped()) return;
        be.sb_lock.lockUncancelable(be.io);
        defer be.sb_lock.unlock(be.io);
        if (be.sb.blockMapTreeStamped()) return;
        var sb = be.sb;
        _ = sb.stampBit(superblock.block_map_tree);
        const plan = superblock.planWrite(sb, be.sb_generation, null) catch return error.Corrupt;
        for (plan.slice()) |step| {
            be.pool.writeAt(be.path, step.offset, plan.bytes()) catch return error.Io;
            if (step.barrier_after) be.dev.barrier() catch return error.Io;
        }
        be.sb = sb;
        be.sb_generation += 1;
        log.info("meta volume {s}: BLOCK_MAP_TREE stamped at generation {d} (the first block-map-tree crossing; barriered before any map record)", .{ be.path, plan.generation });
    }

    // @anchor:ops_jobs_lifecycle_bit — the lifecycle bit's stamp (rung 8).

    /// Stamp `VOLUME_LIFECYCLE` (bit 3) at the first lifecycle commit — both
    /// superblock copies, barriered — BEFORE the record tx (bit-before-
    /// record: a crash after the bit and before the record leaves a set old
    /// binaries refuse and this binary mounts unchanged). Idempotent.
    pub fn stampVolumeLifecycle(be: *KvBackend) Error!void {
        if (be.sb.features_incompat & superblock.volume_lifecycle != 0) return;
        be.sb_lock.lockUncancelable(be.io);
        defer be.sb_lock.unlock(be.io);
        if (be.sb.features_incompat & superblock.volume_lifecycle != 0) return;
        var sb = be.sb;
        _ = sb.stampBit(superblock.volume_lifecycle);
        const plan = superblock.planWrite(sb, be.sb_generation, null) catch return error.Corrupt;
        for (plan.slice()) |step| {
            be.pool.writeAt(be.path, step.offset, plan.bytes()) catch return error.Io;
            if (step.barrier_after) be.dev.barrier() catch return error.Io;
        }
        be.sb = sb;
        be.sb_generation += 1;
        log.info("meta volume {s}: VOLUME_LIFECYCLE stamped at generation {d} (the first lifecycle commit; barriered before the record)", .{ be.path, plan.generation });
    }

    pub fn nodeSize(be: *const KvBackend) usize {
        return be.sb.node_size;
    }

    /// The `statfs` view of the heap: node-sized extents, the allocator's
    /// free population.
    pub const Space = struct { total_bytes: u64, free_bytes: u64 };

    pub fn space(be: *const KvBackend) Space {
        // A reader holds no allocator: the heap is full to it (it mints
        // nothing), the total its own.
        if (be.posture == .reader) return .{ .total_bytes = be.sb.heap.len, .free_bytes = 0 };
        return .{ .total_bytes = be.sb.heap.len, .free_bytes = be.alloc.freeExtents() * be.sb.node_size };
    }

    /// The store face `fuse/handler/kv_fs.KvFs` binds to: this backend's
    /// error set and its names onto the ONE error set (the conveyor's
    /// table: the commit outcomes are its).
    pub const StoreError = Error;

    pub fn coreError(e: Error) errors.Error {
        return conveyor.coreError(e);
    }

    pub fn isFailed(be: *const KvBackend) bool {
        if (be.posture == .reader) return false;
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
        if (be.posture == .reader) {
            // A reader wrote nothing: the projection goes.
            for (be.trees.load()) |t| t.destroy();
            be.trees.deinit();
            be.stamp.deinit();
            be.guest_published.deinit(gpa);
            be.guest_cursor_floors.deinit(gpa);
            be.cache.deinit();
            be.mounted.deinit(gpa);
            if (be.pool_owned) be.pool.deinit();
            _ = linux.close(be.guard_fd);
            gpa.free(be.path);
            gpa.destroy(be);
            return;
        }
        if (!be.closed) {
            be.closed = true;
            // The guard FIRST (charter §5.7 clean unmount): the heartbeat
            // stopped, the claim deleted exactly once while the write gate
            // is open; the final checkpoint below makes the deletion durable.
            if (be.guard) |g| {
                g.stopHeartbeat();
                g.deleteClaim();
            }
            be.cv.close();
            // Nothing queued, no leader, no window in flight: the registry
            // holds no user reservation past this point, and every applier
            // fiber has returned (the group join is the witness).
            be.cv.quiesce();
            be.stopPass();
            exec.group.awaitGroup(&be.apply_group, exec.lane.currentFiber(), &.{}) catch {};
            // The slot cadence's flight joined while the task and the lane
            // still serve its cycles.
            be.slots.quiesce();
            // The in-process regions leave BEFORE the final tick (their
            // control entries ride ring 0 and the tick covers them), each
            // drained at a fixpoint cycle first.
            if (be.sym) |set| set.leaveAll();
            // A joiner: every lease released over the wire, the page naming
            // no slot, the ring drained, `LeaveAppender` (the page Free) —
            // while the task and the lane still serve the cycles it needs.
            if (be.joined) |js| js.leave();
            // The task's final tick BEFORE the lane goes: its SMO entries are
            // structural writes on the lane. The tick asserts the registry
            // quiet (`checkpoint_final_tick_open_reservations`) — the
            // quiesce above is what makes that true.
            be.task.stop();
            if (be.ckpt_thread) |th| {
                th.join();
                be.ckpt_thread = null;
            }
            // The lane last: its trailing barrier leaves every acked entry
            // durable.
            be.lane.shutdown();
            be.dev.barrier() catch {};
            // The reservation released after the final barrier; the flock
            // fd closed last of all (below).
            if (be.guard) |g| g.releasePr();
            stats.clean_unmounts.inc();
            log.info("meta volume {s}: clean unmount (checkpoint seq {d}, tail {d}, head {d}, {s})", .{ be.path, be.cp.checkpointSeq(), be.cp.lastLedgerTail(), be.core.headPos(), @tagName(be.lane.latch.verdict()) });
        }
        if (be.guard) |g| {
            g.deinit();
            be.guard = null;
        }
        if (be.sym) |set| {
            set.deinit();
            be.sym = null;
        }
        if (be.joined) |js| {
            js.deinit();
            be.joined = null;
        }
        be.cv.deinit();
        be.slots.deinit();
        be.lane.deinit();
        if (be.tail) |*t| t.deinit();
        be.cp.deinit();
        for (be.trees.load()) |t| t.destroy();
        be.trees.deinit();
        be.stamp.deinit();
        be.guest_published.deinit(gpa);
        be.guest_cursor_floors.deinit(gpa);
        be.kept_data_deltas.deinit(gpa);
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
// Tests (Linux; regular files on the venue's ext4 under /build/tmp, O_DIRECT).
// The oracle is OUR invariants: every acked record present after a reopen,
// replay ≡ RAM, a clean unmount's empty window, the torn tripwire at 0.
// ---------------------------------------------------------------------------

const testing = std.testing;
const economy = @import("economy.zig");

const test_dir = "/build/tmp";
const test_volume_len: u64 = 64 << 20;
const test_set: geometry.SetIdentity = .{ .set_uuid = "kv-backend-set!!".*, .hash_seed = 0x5EED_CAFE_F00D_D00D };

const TestVol = struct {
    gpa: Allocator,
    env: std.process.Environ.Map,
    /// The executor the mount runs on (the pass fiber's futex ops).
    sqz: *exec.SqzIo,
    path_buf: [128]u8 = undefined,
    path_len: usize = 0,
    /// Error logs the lane thread will emit (the FAILED latch's one line).
    lane_error_logs: usize = 0,

    fn path(self: *const TestVol) []const u8 {
        return self.path_buf[0..self.path_len];
    }

    fn io(self: *TestVol) Io {
        return self.sqz.io();
    }

    /// A fresh one-member forest formatted through the device path.
    fn create(gpa: Allocator, name: []const u8, flush_ms: ?[]const u8) !*TestVol {
        return createWithRing(gpa, name, flush_ms, 1 << 20);
    }

    fn createWithRing(gpa: Allocator, name: []const u8, flush_ms: ?[]const u8, journal_len: u64) !*TestVol {
        return createSized(gpa, name, flush_ms, journal_len, test_volume_len);
    }

    fn createSized(gpa: Allocator, name: []const u8, flush_ms: ?[]const u8, journal_len: u64, volume_len: u64) !*TestVol {
        return createClassed(gpa, name, flush_ms, journal_len, volume_len, .symmetric);
    }

    /// A flat single-writer volume: no lease plane, so every guest root
    /// publishes through `publishGuestRoots` (the class `ops.fsck.scale` runs).
    fn createSingleWriter(gpa: Allocator, name: []const u8, flush_ms: ?[]const u8) !*TestVol {
        return createClassed(gpa, name, flush_ms, 1 << 20, test_volume_len, .single_writer);
    }

    fn createClassed(gpa: Allocator, name: []const u8, flush_ms: ?[]const u8, journal_len: u64, volume_len: u64, class: geometry.Class) !*TestVol {
        const self = try gpa.create(TestVol);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .env = std.process.Environ.Map.init(gpa), .sqz = undefined };
        errdefer self.env.deinit();
        self.sqz = try exec.SqzIo.testInstance();
        errdefer self.sqz.deinit();
        if (flush_ms) |v| try self.env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", v);
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, test_dir), 0o755);
        self.path_len = (try std.fmt.bufPrint(&self.path_buf, "{s}/sqz-be-{d}-{s}.img", .{ test_dir, linux.getpid(), name })).len;
        try truncateFile(self.path(), volume_len);
        try formatVolume(gpa, self.path(), .{
            .env = &self.env,
            .node_size = 65536,
            .journal_len_override = journal_len,
            .uuid = "kv-backend-uuid!".*,
            .set = test_set,
            .class = class,
        });
        return self;
    }

    fn destroy(self: *TestVol) void {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        if (align_probe.pathZ(&zbuf, self.path())) |z| {
            _ = linux.unlink(z);
        } else |_| {}
        self.sqz.deinit();
        self.env.deinit();
        self.gpa.destroy(self);
    }

    fn open(self: *TestVol) !*KvBackend {
        return KvBackend.open(self.gpa, self.io(), self.path(), .{ .env = &self.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .test_error_logs = self.lane_error_logs });
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

/// One create under the root through `economy.stageCreate`; the ino.
fn stageCreate(be: *KvBackend, t: *KvTx, name: []const u8, value_len: usize) !u64 {
    const ino = be.allocateIno();
    const parent_value: record.InodeValue = .{ .mode = economy.S.IFDIR | 0o755, .nlink = 2 };
    // The dentry names the GLOBAL ino (charter §5.4: `(local − 2)·W + slot
    // + 2`, slot 0 on the single volume).
    _ = try economy.stageCreate(t, .{ .name_key = be.nameKey(), .now = 1000, .parent = 1, .parent_value = parent_value, .name = name, .mode = economy.S.IFREG | 0o644, .uid = 0, .gid = 0, .local_ino = ino, .global_ino = inos.makeGlobalIno(ino, 0, inos.ROUTING_WIDTH) });
    if (value_len > 0) {
        const v = try be.gpa.alloc(u8, value_len);
        defer be.gpa.free(v);
        @memset(v, 0x5A);
        var xb: [8192]u8 = undefined;
        const xv = try (record.XattrValue{ .name = "user.pad", .value = v }).encodeInto(&xb);
        try t.stagePut(key.TREE_XATTRS, &key.xattrKey(ino, key.xattrNameHash56("user.pad", be.nameKey()), 0), xv);
    }
    return ino;
}

fn commitCreate(be: *KvBackend, name: []const u8) !u64 {
    const t = try be.begin(@src());
    const ino = try stageCreate(be, t, name, 0);
    try be.commit(t);
    return ino;
}

/// `commitCreate` as this mount's kernel's own op (`kernel_origin`): the
/// shape the apply ack applies to (§BB.1); the plain form is a tool's or
/// a served verb's, acked at the landing.
fn commitCreateKernel(be: *KvBackend, name: []const u8) !u64 {
    const t = try be.begin(@src());
    t.kernel_origin = true;
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
    const r = tx.dentryChainRange(1, key.dentryNameHash54(name, be.nameKey()));
    try be.range(testing.allocator, key.TREE_DENTRIES, &r.start, &r.end, 8, &kvs);
    return kvs.items.len != 0;
}

test "the block-map-tree ratchet: a fresh format carries no bit; the first stamp writes both copies at the next generation and is idempotent; the bit survives close → reopen and the mount accepts it (every other capability bit still refuses)" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "kvmap-ratchet", null);
    defer v.destroy();
    var gen: u64 = 0;
    {
        const be = try v.open();
        try testing.expect(!be.sb.blockMapTreeStamped());
        gen = be.sb_generation;
        try be.stampBlockMapTree();
        try testing.expect(be.sb.blockMapTreeStamped());
        try testing.expectEqual(gen + 1, be.sb_generation);
        try be.stampBlockMapTree();
        try testing.expectEqual(gen + 1, be.sb_generation);
        be.close();
    }
    {
        const be = try v.open();
        try testing.expect(be.sb.blockMapTreeStamped());
        try testing.expectEqual(superblock.block_map_tree | superblock.symmetric_plane, be.sb.features_incompat);
        try testing.expectEqual(gen + 1, be.sb_generation);
        be.close();
    }
}

test "the D4 faces on a device-backed volume: one xattr band rewritten across checkpoints leaves a leaf mostly dead → `defragNudgeCompactions(8)` compacts it (≥ 1, every value current); a tree grown to many leaves then emptied by deletes → `defragMergeLap` merges (≥ 1) and a second lap finds nothing; both under the SMO lock, the cadence task live" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "d4-faces", null);
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    const ino = try commitCreate(be, "d4");
    // The band: 32 xattr values of 256 B (≈ 9.5 KiB a round with keys and
    // frames), rewritten 4 times with a checkpoint between rounds (the
    // rewrites must be ON the serialized log — the open delta is RAM —
    // for the dead share to grow): ≈ 38 KiB of log inside the 64 KiB node
    // (the threshold maintenance never fires), three quarters of it dead.
    var pad: [256]u8 = undefined;
    var round: u8 = 0;
    while (round < 4) : (round += 1) {
        const t = try be.begin(@src());
        var i: u64 = 0;
        while (i < 32) : (i += 1) {
            @memset(&pad, round +% @as(u8, @intCast(i)));
            var nb: [16]u8 = undefined;
            const name = try std.fmt.bufPrint(&nb, "user.b{d}", .{i});
            var xb: [1024]u8 = undefined;
            const xv = try (record.XattrValue{ .name = name, .value = &pad }).encodeInto(&xb);
            try t.stagePut(key.TREE_XATTRS, &key.xattrKey(ino, key.xattrNameHash56(name, be.nameKey()), 0), xv);
        }
        try be.commit(t);
        try be.checkpointNow();
    }
    const c0 = smo.stats.meta_kv_node_compactions.load();
    const kicked = try be.defragNudgeCompactions(gpa, 8);
    try testing.expect(kicked >= 1);
    try testing.expectEqual(c0 + kicked, smo.stats.meta_kv_node_compactions.load());
    // A second nudge finds the band compact.
    try testing.expectEqual(@as(u32, 0), try be.defragNudgeCompactions(gpa, 8));
    {
        var nb: [16]u8 = undefined;
        const name = try std.fmt.bufPrint(&nb, "user.b{d}", .{7});
        const got = (try be.lookup(gpa, key.TREE_XATTRS, &key.xattrKey(ino, key.xattrNameHash56(name, be.nameKey()), 0))) orelse return error.TestExpectedLive;
        defer gpa.free(got);
        const xv = try record.XattrValue.decode(got);
        try testing.expectEqual(@as(u8, 3 +% 7), xv.value[0]);
    }
    // The merge lap: 3,000 creates with 2 KiB xattrs span many leaves;
    // delete every one but each 50th and the leaves fall under ¼.
    var inos_: std.ArrayList(u64) = .empty;
    defer inos_.deinit(gpa);
    var n: usize = 0;
    while (n < 3000) : (n += 1) {
        const t = try be.begin(@src());
        var nb: [24]u8 = undefined;
        try inos_.append(gpa, try stageCreate(be, t, try std.fmt.bufPrint(&nb, "m{d}", .{n}), 2048));
        try be.commit(t);
    }
    try be.checkpointNow();
    const leaves_before = try leaf_merge.residentNodesAt(be.treeOf(.native).?, gpa, 0, "");
    defer gpa.free(leaves_before);
    try testing.expect(leaves_before.len >= 8);
    const census_before = be.residentNodeCensus();
    try testing.expect(census_before.leaves >= leaves_before.len);
    try testing.expect(census_before.interior >= 1);
    for (inos_.items, 0..) |i, k| {
        if (k % 50 == 0) continue;
        const t = try be.begin(@src());
        try t.stageDelete(key.TREE_XATTRS, &key.xattrKey(i, key.xattrNameHash56("user.pad", be.nameKey()), 0));
        try t.stageDelete(key.TREE_INODES, &key.inodeKey(i));
        try be.commit(t);
    }
    try be.checkpointNow();
    const m0 = leaf_merge.stats.meta_kv_node_merges.load();
    const merged = be.defragMergeLap();
    try testing.expect(merged >= 1);
    try testing.expect(leaf_merge.stats.meta_kv_node_merges.load() >= m0 + 1);
    try be.checkpointNow();
    // The lap converges: a second lap after the cover merges nothing new
    // once the recovery wave settles (bounded laps; each pairs what the
    // previous one's successors allow).
    var laps: u32 = 0;
    while (laps < 8) : (laps += 1) {
        if (be.defragMergeLap() == 0) break;
        try be.checkpointNow();
    }
    try testing.expect(laps < 8);
    const leaves_after = try leaf_merge.residentNodesAt(be.treeOf(.native).?, gpa, 0, "");
    defer gpa.free(leaves_after);
    try testing.expect(leaves_after.len < leaves_before.len);
    try testing.expect(be.residentNodeCensus().leaves < census_before.leaves);
    for (inos_.items, 0..) |i, k| try testing.expectEqual(k % 50 == 0, try present(be, i));
}

test "format (device path) → open → N creates (singles + a group of 8) → lookup each → checkpointNow → close → reopen: every record present, a clean unmount; the mount is the one-member forest" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "e2e", null);
    defer v.destroy();
    var acked: [24]u64 = undefined;
    {
        const be = try v.open();
        try testing.expect(be.replayStats().clean_unmount);
        try testing.expectEqual(@as(u64, 0), be.replayStats().entries);
        // The forest shape: the control tree and the native slot tree,
        // features 0, the stamp the format wrote.
        try testing.expectEqual(@as(usize, 2), be.trees.load().len);
        try testing.expect(be.treeOf(.control) != null);
        try testing.expect(be.treeOf(.native) != null);
        // The default format is the symmetric class (charter §5.5).
        try testing.expectEqual(superblock.symmetric_plane, be.sb.features_incompat);
        try testing.expectEqualSlices(u8, &test_set.set_uuid, &be.mounted.membership_stamp.set_uuid);
        try testing.expectEqual(test_set.hash_seed, be.sb.hash_seed);
        try testing.expectEqual(@as(?u16, 0), be.mounted.membership_stamp.native_slot);
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
        try testing.expectEqual(24 + guard.gate.OPEN_COMMITS, cs.acks.load());
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
        // The restated ledger record carries the stamp and both roots.
        try testing.expectEqual(@as(usize, 2), be.mounted.tree_roots.len);
        try testing.expect(be.mounted.root(.control) != null);
        try testing.expect(be.mounted.root(.native_slot) != null);
        try testing.expectEqualSlices(u8, &test_set.set_uuid, &be.mounted.membership_stamp.set_uuid);
    }
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

test "the single-writer guard waits out a SHARED holder — udev's block-device probe (systemd.io/BLOCK_DEVICE_LOCKING) — and opens once it lets go; no refusal, no error log" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "guard-shared", null);
    defer v.destroy();
    // The stand-in for systemd-udevd: LOCK_SH on its own open file.
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = try std.fmt.bufPrintSentinel(&zbuf, "{s}", .{v.path()}, 0);
    const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    const probe_fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(probe_fd);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(probe_fd, std.posix.LOCK.SH)));
    const refusals0 = stats.guard_busy_refusals.load();
    const waits0 = stats.guard_shared_lock_waits.load();
    const Opener = struct {
        fn run(vol: *TestVol, out: *?(OpenError!*KvBackend), done: *std.atomic.Value(bool)) void {
            out.* = vol.open();
            done.store(true, .release);
        }
    };
    var result: ?(OpenError!*KvBackend) = null;
    var done: std.atomic.Value(bool) = .init(false);
    const t = try std.Thread.spawn(.{}, Opener.run, .{ v, &result, &done });
    // The opener met the shared lock and is waiting (or, refusing it, is
    // done): only then let go.
    while (stats.guard_shared_lock_waits.load() == waits0 and !done.load(.acquire)) try std.Thread.yield();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(probe_fd, std.posix.LOCK.UN)));
    t.join();
    try testing.expect(stats.guard_shared_lock_waits.load() > waits0);
    const be = try result.?;
    be.close();
    try testing.expectEqual(refusals0, stats.guard_busy_refusals.load());
}

test "the join door: a held writer lock on volume 0 of a symmetric set is the manager's — Busy with ZERO error logs (an info line names the holder; the join follows), not counted as a refusal; without the door the same lock refuses loud" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "join-door", null);
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    try testing.expect(be.sb.symmetricPlaneStamped());
    const refusals_before = stats.guard_busy_refusals.load();
    // The runner fails the test on any error log: none is expected here.
    log.testing.expectErrorLogs(0);
    try testing.expectError(error.Busy, KvBackend.open(gpa, v.io(), v.path(), .{ .env = &v.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join_door = true }));
    try testing.expectEqual(refusals_before, stats.guard_busy_refusals.load());
    // The refusal class keeps its error and its counter.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Busy, v.open());
    try log.testing.expectAllFired();
    try testing.expectEqual(refusals_before + 1, stats.guard_busy_refusals.load());
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
        // The gate's claim barrier is the one coalesced barrier so far.
        _ = try commitCreate(be, "a");
        try testing.expectEqual(@as(u64, 0), be.laneSnapshot().barriers_strict);
        try testing.expectEqual(guard.gate.OPEN_BARRIERS, be.laneSnapshot().barriers_coalesced);
        try be.fsync();
        try testing.expectEqual(guard.gate.OPEN_BARRIERS + 1, be.laneSnapshot().barriers_coalesced);
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
    try testing.expectEqual(guard.gate.OPEN_BARRIERS + 1, be.laneSnapshot().barriers_coalesced);
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
    // The durable-at-ack arm (§BB.1's control): the failed write is the
    // committer's to hear.
    try v.env.put("SQUEEZEFS_META_ACK_POINT", "landing");
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

test "the lane's hole checkpoint never waits on the SMO lock: a window failing while the lock is held (the checkpointer mid-SMO, parked on this lane's answer to its structural entry) is rolled back and answered at once, the volume stays open and the holder's own cycle covers the hole (repro-port: xfstests generic/753's soak loop under dm-error — every committer parked for the kernel's 135 s request timeout)" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "hole-lock", "600000");
    defer v.destroy();
    try v.env.put("SQUEEZEFS_META_ACK_POINT", "landing");
    v.lane_error_logs = 1;
    const be = try v.open();
    defer be.close();
    _ = try commitCreate(be, "base");
    // The checkpointer mid-SMO: the SMO lock held by another thread.
    be.smo_lock.lockUncancelable(be.io);
    var held = true;
    defer if (held) be.smo_lock.unlock(be.io);
    journal_lane.test_seams.fail_windows.store(1, .release);
    defer journal_lane.test_seams.fail_windows.store(0, .release);
    const Victim = struct {
        fn run(b: *KvBackend, out: *?anyerror, done: *std.atomic.Value(u32)) void {
            out.* = if (commitCreate(b, "victim")) |_| null else |e| e;
            done.store(1, .release);
        }
    };
    var out: ?anyerror = null;
    var done = std.atomic.Value(u32).init(0);
    const t = try std.Thread.spawn(.{}, Victim.run, .{ be, &out, &done });
    // Answered while the lock stays held (a bound only against the hang:
    // the answer takes milliseconds).
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (done.load(.acquire) == 0 and clock.monotonicNs() < deadline) try testing.io.sleep(.fromMilliseconds(5), .awake);
    const answered = done.load(.acquire) == 1;
    be.smo_lock.unlock(be.io);
    held = false;
    t.join();
    try testing.expect(answered);
    try testing.expectEqual(@as(?anyerror, error.JournalWriteFailed), out);
    try testing.expect(!be.isFailed());
    // The volume serves on; the next checkpoint covers the hole.
    _ = try commitCreate(be, "after");
    try be.checkpointNow();
}

test "§BB.1 the ack at the apply: under the coalesced cadence this mount's kernel's own commit returns while its journal write is still un-issued (the landing-hold seam), its record is readable, the tail holds its window; an fsync behind it parks until the write lands; the landing frees the tail slot; a served verb's (non-kernel) commit, the landing arm and strict mode keep the commit parked" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "ack-apply", "600000");
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    _ = try commitCreate(be, "base");
    const acks0 = be.conveyorStats().acks_at_apply.load();
    journal_lane.test_seams.delay_issue.store(1, .release);
    defer journal_lane.test_seams.delay_issue.store(0, .release);
    // The commit returns: nothing landed (the seam), the record is RAM's.
    const ino = try commitCreateKernel(be, "early");
    try testing.expect(try present(be, ino));
    try testing.expectEqual(acks0 + 1, be.conveyorStats().acks_at_apply.load());
    try testing.expectEqual(@as(u32, 1), be.tail.?.image.writtenCount());
    // fsync parks behind the un-landed window (§BB.4).
    const Sync = struct {
        fn run(b: *KvBackend, out: *?anyerror, done: *std.atomic.Value(u32)) void {
            out.* = if (b.fsync()) |_| null else |e| e;
            done.store(1, .release);
        }
    };
    var out: ?anyerror = null;
    var done = std.atomic.Value(u32).init(0);
    const t = try std.Thread.spawn(.{}, Sync.run, .{ be, &out, &done });
    // Four lane parks later (the backstop's ticks) the fsync still waits.
    const parks0 = be.laneSnapshot().parks;
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (be.laneSnapshot().parks < parks0 + 4 and clock.monotonicNs() < deadline) try testing.io.sleep(.fromMilliseconds(5), .awake);
    try testing.expectEqual(@as(u32, 0), done.load(.acquire));
    // The device answers: the window lands, the barrier follows, fsync
    // returns, the tail slot is free.
    journal_lane.test_seams.delay_issue.store(0, .release);
    t.join();
    try testing.expectEqual(@as(?anyerror, null), out);
    try testing.expectEqual(@as(u32, 0), be.tail.?.image.writtenCount());
    try testing.expectEqual(@as(u64, 0), be.conveyorStats().tail_slot_misses.load());

    // A commit that is not this mount's kernel's (a served verb, a
    // control record, a tool) parks until the landing under the apply arm
    // too: the symmetric protocol's words mean durable (§BB.1).
    {
        const landing0 = be.conveyorStats().acks_at_landing.load();
        journal_lane.test_seams.delay_issue.store(1, .release);
        const Tool = struct {
            fn run(b: *KvBackend, out2: *?anyerror, done2: *std.atomic.Value(u32)) void {
                out2.* = if (commitCreate(b, "served")) |_| null else |e| e;
                done2.store(1, .release);
            }
        };
        var out2: ?anyerror = null;
        var done2 = std.atomic.Value(u32).init(0);
        const t2 = try std.Thread.spawn(.{}, Tool.run, .{ be, &out2, &done2 });
        const p0 = be.laneSnapshot().parks;
        const dl = clock.monotonicNs() + 5 * std.time.ns_per_s;
        while (be.laneSnapshot().parks < p0 + 4 and clock.monotonicNs() < dl) try testing.io.sleep(.fromMilliseconds(5), .awake);
        try testing.expectEqual(@as(u32, 0), done2.load(.acquire));
        journal_lane.test_seams.delay_issue.store(0, .release);
        t2.join();
        try testing.expectEqual(@as(?anyerror, null), out2);
        try testing.expectEqual(landing0 + 1, be.conveyorStats().acks_at_landing.load());
    }

    // The control arm: the commit parks until the landing.
    inline for (.{ .{ "600000", "landing" }, .{ "0", "apply" } }) |arm| {
        const v2 = try TestVol.create(gpa, "ack-landing", arm[0]);
        defer v2.destroy();
        try v2.env.put("SQUEEZEFS_META_ACK_POINT", arm[1]);
        const be2 = try v2.open();
        defer be2.close();
        _ = try commitCreate(be2, "base");
        const landing0 = be2.conveyorStats().acks_at_landing.load();
        journal_lane.test_seams.delay_issue.store(1, .release);
        const Committer = struct {
            fn run(b: *KvBackend, out2: *?anyerror, done2: *std.atomic.Value(u32)) void {
                out2.* = if (commitCreateKernel(b, "held")) |_| null else |e| e;
                done2.store(1, .release);
            }
        };
        var out2: ?anyerror = null;
        var done2 = std.atomic.Value(u32).init(0);
        const t2 = try std.Thread.spawn(.{}, Committer.run, .{ be2, &out2, &done2 });
        const p0 = be2.laneSnapshot().parks;
        const dl = clock.monotonicNs() + 5 * std.time.ns_per_s;
        while (be2.laneSnapshot().parks < p0 + 4 and clock.monotonicNs() < dl) try testing.io.sleep(.fromMilliseconds(5), .awake);
        try testing.expectEqual(@as(u32, 0), done2.load(.acquire));
        journal_lane.test_seams.delay_issue.store(0, .release);
        t2.join();
        try testing.expectEqual(@as(?anyerror, null), out2);
        try testing.expectEqual(landing0 + 1, be2.conveyorStats().acks_at_landing.load());
        try testing.expectEqual(@as(u32, 0), be2.tail.?.image.writtenCount());
    }
}

test "§BC self-apply: a 4-thread storm of this mount's kernel's creates — every member applied itself (self_applies ≡ commits, acks at the apply ≡ commits, no applier venue entered), every ack present and replayed after the reopen; one member poisoned mid-storm rolls out ALONE as a PAD over its range (its window's survivors stand, nothing rolled back, no hole), and the strict and venue arms park every commit until the landing" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "self-apply", "600000");
    defer v.destroy();
    const threads = 4;
    const per_thread = 50;
    var acked: [threads][per_thread]u64 = undefined;
    // The poisoned ino: the first thread's 25th create fails its apply.
    const poison_index: usize = 24;
    {
        const be = try v.open();
        const Storm = struct {
            fn run(b: *KvBackend, lane_id: usize, out: *[per_thread]u64) void {
                for (0..per_thread) |i| {
                    var nb: [24]u8 = undefined;
                    const n = std.fmt.bufPrint(&nb, "self-{d}-{d}", .{ lane_id, i }) catch unreachable;
                    if (lane_id == 0 and i == poison_index) {
                        // Staged first: the poison names the ino it minted.
                        const t = b.begin(@src()) catch unreachable;
                        t.kernel_origin = true;
                        const victim = stageCreate(b, t, n, 0) catch unreachable;
                        conveyor.test_seams.poison_apply_ino.store(victim, .monotonic);
                        out[i] = victim;
                        const r = b.commit(t);
                        conveyor.test_seams.poison_apply_ino.store(0, .monotonic);
                        if (r) |_| unreachable else |e| std.debug.assert(e == error.Corrupt);
                        continue;
                    }
                    out[i] = commitCreateKernel(b, n) catch unreachable;
                }
            }
        };
        const cs = be.conveyorStats();
        const applies0 = cs.self_applies.load();
        const early0 = cs.acks_at_apply.load();
        const rollbacks0 = cs.rollbacks.load();
        var handles: [threads]std.Thread = undefined;
        for (&handles, 0..) |*h, i| h.* = try std.Thread.spawn(.{}, Storm.run, .{ be, i, &acked[i] });
        for (handles) |h| h.join();
        for (acked, 0..) |lane_acks, lane_id| for (lane_acks, 0..) |ino, i| {
            const victim = lane_id == 0 and i == poison_index;
            try testing.expectEqual(!victim, try present(be, ino));
        };
        try testing.expectEqual(@as(u64, threads * per_thread), cs.self_applies.load() - applies0);
        try testing.expectEqual(@as(u64, threads * per_thread), cs.acks_at_apply.load() - early0);
        try testing.expectEqual(rollbacks0, cs.rollbacks.load());
        try testing.expectEqual(@as(u64, 0), cs.tail_slot_misses.load());
        try testing.expectEqual(@as(u32, 0), be.cv.in_apply.load(.acquire));
        try testing.expectEqual(@as(u64, 0), cs.windows_inflight.load());
        try be.fsync();
        try testing.expectEqual(@as(u32, 0), be.tail.?.image.writtenCount());
        be.close();
    }
    {
        const be = try v.open();
        defer be.close();
        try testing.expect(be.replayStats().clean_unmount);
        for (acked, 0..) |lane_acks, lane_id| for (lane_acks, 0..) |ino, i| {
            const victim = lane_id == 0 and i == poison_index;
            try testing.expectEqual(!victim, try present(be, ino));
        };
    }
}

test "§BB.5 a journal write that fails AFTER its window was acked: the commit returned Ok and its record STANDS (no rollback), the volume latches FAILED at once (one loud line), the tail still holds the window for the supervisor's replay" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "acked-fail", "600000");
    defer v.destroy();
    v.lane_error_logs = 1;
    const be = try v.open();
    defer be.close();
    _ = try commitCreate(be, "base");
    // The write fails at issue — but only after the applier answered: the
    // issue is delayed until the applier's ack is observed.
    journal_lane.test_seams.delay_issue.store(1, .release);
    defer journal_lane.test_seams.delay_issue.store(0, .release);
    const ino = try commitCreateKernel(be, "acked");
    try testing.expect(try present(be, ino));
    try testing.expectEqual(@as(u32, 1), be.tail.?.image.writtenCount());
    journal_lane.test_seams.fail_windows.store(1, .release);
    defer journal_lane.test_seams.fail_windows.store(0, .release);
    journal_lane.test_seams.delay_issue.store(0, .release);
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (!be.isFailed() and clock.monotonicNs() < deadline) try testing.io.sleep(.fromMilliseconds(5), .awake);
    try testing.expect(be.isFailed());
    try testing.expectEqual(@as(u64, 1), be.conveyorStats().acked_write_failures.load());
    try testing.expectEqual(@as(u64, 0), be.conveyorStats().rollbacks.load());
    try testing.expect(try present(be, ino));
    try testing.expectError(error.VolumeFailed, commitCreate(be, "post"));
    try testing.expectError(error.VolumeFailed, be.fsync());
    // The tail keeps the window: the replay's bytes.
    try testing.expect(be.tail.?.image.writtenCount() >= 1);
}

/// lseek(2) whences the std table lacks (`include/uapi/linux/fs.h`).
const seek_data: u32 = 3;
const seek_hole: u32 = 4;

/// Snapshot the device image to `dst` (a power cut: the device's landed
/// bytes, nothing in any cache).
/// The cut: a copy of the live image holding every write that COMPLETED
/// before this instant (the kill -9 model: a completed O_DIRECT write
/// survives the process). `fdatasync` first — ext4 completes a direct write
/// into a sparse file's unwritten extent before converting the extent, and
/// until the conversion lands a concurrent reader of the range sees zeros —
/// then O_DIRECT reads, never the page cache of a direct-written file
/// (open(2): the two must not mix on one file).
pub fn snapshotImage(src: []const u8, dst: []const u8) !void {
    var zsrc: [linux.PATH_MAX]u8 = undefined;
    const src_z = try align_probe.pathZ(&zsrc, src);
    const src_rc = linux.open(src_z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true, .DIRECT = true }, 0);
    if (linux.errno(src_rc) != .SUCCESS) return error.CannotOpenSource;
    const src_fd: linux.fd_t = @intCast(src_rc);
    defer _ = linux.close(src_fd);
    if (linux.errno(linux.fdatasync(src_fd)) != .SUCCESS) return error.SnapshotSyncFailed;
    var zdst: [linux.PATH_MAX]u8 = undefined;
    const dst_z = try align_probe.pathZ(&zdst, dst);
    const dst_rc = linux.open(dst_z, .{ .ACCMODE = .WRONLY, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(dst_rc) != .SUCCESS) return error.CannotCreateSnapshot;
    const dst_fd: linux.fd_t = @intCast(dst_rc);
    defer _ = linux.close(dst_fd);
    // Sparse to sparse: only the source's data runs are copied (a
    // multi-GiB test volume is mostly holes).
    const size_rc = linux.lseek(src_fd, 0, linux.SEEK.END);
    if (linux.errno(size_rc) != .SUCCESS) return error.SnapshotReadFailed;
    const size: u64 = size_rc;
    if (linux.errno(linux.ftruncate(dst_fd, @intCast(size))) != .SUCCESS) return error.SnapshotWriteFailed;
    const chunk = 1 << 20;
    const buf = try std.heap.page_allocator.alignedAlloc(u8, .fromByteUnits(4096), chunk);
    defer std.heap.page_allocator.free(buf);
    var off: u64 = 0;
    while (off < size) {
        const data_rc = linux.lseek(src_fd, @intCast(off), seek_data);
        switch (linux.errno(data_rc)) {
            .SUCCESS => {},
            .NXIO => break, // no data at or past `off`
            else => return error.SnapshotReadFailed,
        }
        const hole_rc = linux.lseek(src_fd, @intCast(data_rc), seek_hole);
        if (linux.errno(hole_rc) != .SUCCESS) return error.SnapshotReadFailed;
        off = data_rc;
        while (off < hole_rc) {
            const want: usize = @intCast(@min(chunk, hole_rc - off));
            const got = linux.pread(src_fd, buf.ptr, want, @intCast(off));
            if (linux.errno(got) != .SUCCESS) return error.SnapshotReadFailed;
            if (got == 0) break;
            var written: usize = 0;
            while (written < got) {
                const w = linux.pwrite(dst_fd, buf.ptr + written, got - written, @intCast(off + written));
                if (linux.errno(w) != .SUCCESS) return error.SnapshotWriteFailed;
                written += w;
            }
            off += got;
        }
        off = @max(off, hole_rc);
    }
    if (linux.errno(linux.fdatasync(dst_fd)) != .SUCCESS) return error.SnapshotSyncFailed;
}

const CutCtx = struct {
    src: []const u8,
    dst: []const u8,
    fired: std.atomic.Value(u32) = .init(0),
    /// Acks the committer had received when the cut was taken (release by
    /// the committer after each ack, acquire here): the cut must hold
    /// exactly these; later acks are not owed.
    acked: *const std.atomic.Value(usize),
    acked_at_cut: usize = 0,

    fn hook(self: *CutCtx) node_cache.Hook {
        return .{ .ctx = self, .call = call };
    }

    fn call(ctx: *anyopaque) void {
        const self: *CutCtx = @ptrCast(@alignCast(ctx));
        if (self.fired.swap(1, .acq_rel) != 0) return;
        self.acked_at_cut = self.acked.load(.acquire);
        snapshotImage(self.src, self.dst) catch |e| {
            log.warn("cut snapshot failed: {s}", .{@errorName(e)});
        };
    }
};

const Cut = enum { after_ack_before_barrier, before_ledger, after_ledger, before_smo_entry };

/// One row of the kill matrix: the cut's image reopened — every tx acked
/// (coalesced: landed) BEFORE the cut present whole, no partial tx anywhere,
/// the watermark the DUR-8c law's.
fn killMatrixRow(gpa: Allocator, cut: Cut) !void {
    var name_buf: [32]u8 = undefined;
    const name = try std.fmt.bufPrint(&name_buf, "cut-{s}", .{@tagName(cut)});
    const v = try TestVol.create(gpa, name, "600000");
    defer v.destroy();
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{v.path()});
    defer unlinkPath(dst);
    var acked_n: std.atomic.Value(usize) = .init(0);
    var ctx: CutCtx = .{ .src = v.path(), .dst = dst, .acked = &acked_n };
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
            acked_n.store(acked.items.len, .release);
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
                    acked_n.store(acked.items.len, .release);
                }
                minted_max = be.nextIno() - 1;
                ctx.acked_at_cut = acked.items.len;
                try snapshotImage(v.path(), dst);
            },
            .before_ledger, .after_ledger => {
                for (0..12) |i| {
                    const n = try std.fmt.allocPrint(gpa, "win-{d}", .{i});
                    try names.append(gpa, n);
                    try acked.append(gpa, try commitCreate(be, n));
                    acked_n.store(acked.items.len, .release);
                }
                minted_max = be.nextIno() - 1;
                try be.checkpointNow();
                try testing.expectEqual(@as(u32, 1), ctx.fired.load(.acquire));
                try testing.expectEqual(acked.items.len, ctx.acked_at_cut);
            },
            .before_smo_entry => {
                // 4 KiB xattr values per create push the native leaf past
                // its in-place log: the threshold drain replaces it
                // mid-stream (the first SMO entry — the cut), the flush
                // replaces again. Commits acked after the cut are not owed.
                for (0..24) |i| {
                    const n = try std.fmt.allocPrint(gpa, "big-{d}", .{i});
                    try names.append(gpa, n);
                    const t = try be.begin(@src());
                    try acked.append(gpa, try stageCreate(be, t, n, 4000));
                    try be.commit(t);
                    acked_n.store(acked.items.len, .release);
                }
                minted_max = be.nextIno() - 1;
                try be.checkpointNow();
                try testing.expectEqual(@as(u32, 1), ctx.fired.load(.acquire));
                try testing.expect(ctx.acked_at_cut > 8);
            },
        }
        // The live mount goes on (the kill is the copy); close it cleanly so
        // the live image is itself consistent.
        be.close();
    }
    // Reopen the CUT image: every tx acked before the cut is present whole.
    const copy = try KvBackend.open(gpa, v.io(), dst, .{ .env = &v.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .index = 1 });
    defer copy.close();
    const owed = ctx.acked_at_cut;
    for (acked.items[0..owed], names.items[0..owed]) |ino, n| {
        try testing.expect(try present(copy, ino));
        try testing.expect(try dentryPresent(copy, n));
    }
    // DUR-8c: `max(ledger, replayed + 1)` — above every ino the cut holds and
    // never beyond the next one the live mount minted (a dentry's GLOBAL
    // child ino is routed back to its local before it feeds the watermark).
    try testing.expect(copy.nextIno() > acked.items[owed - 1]);
    try testing.expect(copy.nextIno() <= minted_max + 1);
    // A partial tx never shows: every dentry present names a present inode,
    // and nothing beyond the acks is there.
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(gpa, &kvs);
    const lo = key.dentryKey(1, 0, 0);
    const hi = key.dentryKey(1, std.math.maxInt(u64) >> 10, std.math.maxInt(u8));
    try copy.range(gpa, key.TREE_DENTRIES, &lo, &hi, 1024, &kvs);
    try testing.expect(kvs.items.len >= owed and kvs.items.len <= acked.items.len);
    for (kvs.items) |kv| {
        const dv = try record.DentryValue.decode(kv.value);
        try testing.expect(try present(copy, inos.routeIno(dv.child_ino, inos.ROUTING_WIDTH).local));
    }
    std.debug.print("\nkill matrix: cut {s} → reopened {d} of {d} acked txs whole ({d} owed), next_ino {d} ≤ {d}, replay entries {d}, torn dropped {d}\n", .{ @tagName(cut), kvs.items.len, acked.items.len, owed, copy.nextIno(), minted_max + 1, copy.replayStats().entries, copy.replayStats().dropped_torn });
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

/// A create's inode record in routing slot `slot` (its tree minted on
/// first touch); the KEY ino.
fn commitGuestInode(be: *KvBackend, slot: u16) !u64 {
    const ino = key.guestLocalIno(slot, try be.allocateInoInSlot(slot));
    const t = try be.begin(@src());
    try t.stagePut(key.TREE_INODES, &key.inodeKey(ino), &(record.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode());
    try be.commit(t);
    return ino;
}

test "kill matrix: mid-C4b — two guest trees minted on first touch (each root one extent, claimed in RAM only), the cut after C4b named their roots and before the cycle's bitmap pages landed: the reopened cut holds every root it names ALLOCATED, so the first claim after the crash (a third slot's mint) lands beside them, never over one, and every acked guest record stays present" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "cut-c4b", "600000");
    defer v.destroy();
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{v.path()});
    defer unlinkPath(dst);
    var acked_n: std.atomic.Value(usize) = .init(0);
    var ctx: CutCtx = .{ .src = v.path(), .dst = dst, .acked = &acked_n };
    defer test_seams = .{};
    // Three guest slots (the native one skipped).
    var slots: [3]u16 = undefined;
    var inos_acked: [3]u64 = undefined;
    {
        const be = try v.open();
        defer be.close();
        var next: u16 = 1;
        for (&slots) |*s| {
            if (be.stamp.native_slot) |ns| if (ns == next) {
                next += 1;
            };
            s.* = next;
            next += 1;
        }
        // The durable baseline names no guest tree.
        try be.checkpointNow();
        for (inos_acked[0..2], slots[0..2], 0..) |*ino, s, i| {
            ino.* = try commitGuestInode(be, s);
            acked_n.store(i + 1, .release);
        }
        test_seams.after_guest_roots = ctx.hook();
        try be.checkpointNow();
        try testing.expectEqual(@as(u32, 1), ctx.fired.load(.acquire));
    }
    const copy = try KvBackend.open(gpa, v.io(), dst, .{ .env = &v.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .index = 1 });
    defer copy.close();
    var named: usize = 0;
    for (slots[0..2]) |s| {
        const t = copy.slotTree(s) orelse continue;
        named += 1;
        try testing.expect(copy.alloc.isAllocated(copy.cache.addrExtent(t.rootPtr().addr)));
    }
    // The cut is past C4b: both roots are named, neither is re-minted.
    try testing.expectEqual(@as(usize, 2), named);
    // The first claim after the crash.
    inos_acked[2] = try commitGuestInode(copy, slots[2]);
    for (inos_acked) |ino| try testing.expect(try present(copy, ino));
    for (copy.trees.load()) |t| try testing.expect(copy.alloc.isAllocated(copy.cache.addrExtent(t.rootPtr().addr)));
    std.debug.print("\nkill matrix: cut mid-C4b → 2 guest roots named and allocated after the replay; the third slot minted beside them; 3 of 3 guest records present\n", .{});
}

test "a guest root the cycle cannot publish (the checkpoint class full) defers instead of failing the cycle: the tail stays under the root's floor, a cut taken then still recovers the guest record from the window, and the next cycle with room publishes it (the ops.fsck.scale wedge: every tick failed Corrupt and the ring never drained)" {
    const gpa = testing.allocator;
    const v = try TestVol.createSingleWriter(gpa, "guest-full", "600000");
    defer v.destroy();
    defer test_seams = .{};
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{v.path()});
    defer unlinkPath(dst);
    var ino: u64 = undefined;
    {
        const be = try v.open();
        defer be.close();
        var slot: u16 = 1;
        if (be.stamp.native_slot) |ns| if (ns == slot) {
            slot += 1;
        };
        try be.checkpointNow();
        ino = try commitGuestInode(be, slot);
        const floor = be.slotTree(slot).?.rootFloor();
        test_seams.refuse_guest_publish = true;
        try be.checkpointNow();
        try testing.expect(be.guest_published.get(slot) == null);
        try testing.expect(be.cp.last_ledger_tail.load(.acquire) <= floor);
        try snapshotImage(v.path(), dst);
        test_seams.refuse_guest_publish = false;
        try be.checkpointNow();
        try testing.expect(be.guest_published.get(slot) != null);
    }
    // The cut: the guest tree unnamed in tree 0, its record replayed from the window.
    const copy = try KvBackend.open(gpa, v.io(), dst, .{ .env = &v.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .index = 1 });
    defer copy.close();
    try testing.expect(try present(copy, ino));
}

/// Past one bitmap page at 64 KiB nodes: `alloc_ext.page_bits` (32,576)
/// extents a page, so 2.5 GiB (sparse) spans two.
const two_page_volume_len: u64 = 5 << 29;

/// `copySlotTree`'s build (`routing/migrate.zig`) with one event between
/// the last leaf's image and the root's claim: the held extents of bitmap
/// page 0 are returned — a release landing mid-build (a checkpoint's
/// coverage drain beside a live copy) lowers the lowest-first cursor, so
/// the root, claimed last, takes page 0 while the leaves sit on page 1.
const MidBuildRelease = struct {
    be: *KvBackend,
    held: []const u64,
    released: bool = false,

    fn write(ctx: *anyopaque, addr: u64, image: []const u8) anyerror!void {
        const self: *MidBuildRelease = @ptrCast(@alignCast(ctx));
        try self.be.cache.loader.writeImage(self.be.cache.loader.ctx, addr, image);
        const h = try node.decodeHeaderPage(image[0..node.NODE_PAGE], null);
        if (self.released or h.level != 0 or !std.mem.eql(u8, h.max_key, &key.KEY_SPACE_MAX)) return;
        for (self.held) |e| self.be.alloc.releaseUnpublished(e);
        self.released = true;
    }

    fn mint(ctx: *anyopaque) anyerror!u64 {
        const self: *MidBuildRelease = @ptrCast(@alignCast(ctx));
        return self.be.seq.mint();
    }
};

test "kill matrix: mid-C6 — a slot tree built like copySlotTree (every extent claimed in RAM, the root LAST) while a release lands mid-build: its leaves on bitmap page 1, its root on page 0; the cut after C6 landed page 0 (the root's bit) and before page 1: the reopened cut holds EVERY extent the tree reaches allocated — no claim can hand out a live leaf — and every built record is present" {
    const gpa = testing.allocator;
    const v = try TestVol.createSized(gpa, "cut-c6", "600000", 1 << 20, two_page_volume_len);
    defer v.destroy();
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{v.path()});
    defer unlinkPath(dst);
    var acked_n: std.atomic.Value(usize) = .init(0);
    var ctx: CutCtx = .{ .src = v.path(), .dst = dst, .acked = &acked_n };
    defer test_seams = .{};
    var slot: u16 = 1;
    const records: u64 = 4000;
    {
        const be = try v.open();
        defer be.close();
        if (be.stamp.native_slot) |ns| if (ns == slot) {
            slot += 1;
        };
        try be.checkpointNow();
        // Every free extent of page 0 held: the leaves go to page 1.
        var held: std.ArrayList(u64) = .empty;
        defer held.deinit(gpa);
        while (true) {
            const e = try be.alloc.claimInternal();
            if (e >= alloc_ext.page_bits) {
                be.alloc.releaseUnpublished(e);
                break;
            }
            try held.append(gpa, e);
        }
        var rel: MidBuildRelease = .{ .be = be, .held = held.items };
        var sw = builder.StreamWriter.init(gpa, be.layout, be.sb.heap.start, &be.alloc, key.KIND_INTERIOR, .{ .ctx = &rel, .write = MidBuildRelease.write }, .{ .ctx = &rel, .mint = MidBuildRelease.mint });
        defer sw.deinit();
        for (2..2 + records) |local| try sw.append(&key.inodeKey(key.guestLocalIno(slot, local)), &(record.InodeValue{ .mode = 0o100644, .nlink = 1, .size = local }).encode());
        const root = try sw.finish();
        try testing.expect(rel.released);
        try be.dev.barrier();
        const t = try KvTree.open(gpa, be.cache, .{ .slot = key.guestForestSlot(slot) }, .{ .addr = root.addr, .seq = root.seq }, &be.seq);
        try be.adoptTree(t);
        var reach: std.ArrayList(u64) = .empty;
        defer reach.deinit(gpa);
        try be.collectTreeExtents(t, &reach);
        // The root on page 0, at least one leaf on page 1.
        try testing.expectEqual(@as(u64, 0), reach.items[0] / alloc_ext.page_bits);
        var on_page_1: usize = 0;
        for (reach.items) |e| on_page_1 += @intFromBool(e / alloc_ext.page_bits == 1);
        try testing.expect(on_page_1 >= 1);
        test_seams.after_bitmap_page = ctx.hook();
        try be.checkpointNow();
        try testing.expectEqual(@as(u32, 1), ctx.fired.load(.acquire));
    }
    const copy = try KvBackend.open(gpa, v.io(), dst, .{ .env = &v.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .index = 1 });
    defer copy.close();
    var reach: std.ArrayList(u64) = .empty;
    defer reach.deinit(gpa);
    try copy.collectTreeExtents(copy.slotTree(slot) orelse return error.TestExpectedNamedTree, &reach);
    var free_reachable: usize = 0;
    for (reach.items) |e| free_reachable += @intFromBool(!copy.alloc.isAllocated(e));
    // The mount's claims run lowest-first: the first one past page 0 is
    // what a mint or an SMO would write next there.
    const past_page_0 = while (true) {
        const e = try copy.alloc.claimInternal();
        if (e >= alloc_ext.page_bits) break e;
    };
    std.debug.print("\nkill matrix: cut mid-C6 → the built tree reaches {d} extent(s), {d} of them free in the recovered allocator; the first claim past page 0 is extent {d}\n", .{ reach.items.len, free_reachable, past_page_0 });
    try testing.expect(std.mem.indexOfScalar(u64, reach.items, past_page_0) == null);
    try testing.expectEqual(@as(usize, 0), free_reachable);
    for (2..2 + records) |local| try testing.expect(try present(copy, key.guestLocalIno(slot, local)));
}

test "4-thread commit storm with concurrent checkpoints, then reopen: every ack present, identities close at quiesce" {
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
        try testing.expectEqual(threads * per_thread + guard.gate.OPEN_COMMITS, cs.acks.load());
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
}

// Decision 0003 §AW: the applier venue's arm of the lever — every window's
// RAM apply, encode and submit run on an applier fiber off the journal
// lane (the `sqz-meta` pool here: no handler lanes), two of them in
// parallel, windows handed off before they are applied and possibly
// applied out of reservation order (the per-key tail fast path, the lane's
// late-predecessor fold); the contract is the serial pass's.
test "§AW the applier venue (SQUEEZEFS_META_APPLIERS=2): a 4-thread storm with concurrent checkpoints — every ack present, drained ≡ acked, no window or applier left, the promises closed, the reopen replays every acked record; the appliers ran every window (the pass applied none inline past the alone ones)" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "storm-appliers", "20");
    // The applier venue is this row's subject (§BC's control arm).
    try v.env.put("SQUEEZEFS_META_APPLY", "venue");
    defer v.destroy();
    try v.env.put("SQUEEZEFS_META_APPLIERS", "2");
    const threads = 4;
    const per_thread = 60;
    var acked: [threads][per_thread]u64 = undefined;
    {
        const be = try v.open();
        try testing.expectEqual(@as(u32, 2), be.cv.deps.applier.parallelism);
        const Storm = struct {
            fn run(b: *KvBackend, lane_id: usize, out: *[per_thread]u64) void {
                for (0..per_thread) |i| {
                    var nb: [24]u8 = undefined;
                    const n = std.fmt.bufPrint(&nb, "applied-{d}-{d}", .{ lane_id, i }) catch unreachable;
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
        try testing.expectEqual(threads * per_thread + guard.gate.OPEN_COMMITS, cs.acks.load());
        try testing.expectEqual(@as(u64, 0), cs.refusals.load());
        try testing.expectEqual(cs.drained_txs.load(), cs.acks.load() + cs.refusals.load());
        try testing.expectEqual(cs.lane_handoffs.load(), be.laneSnapshot().lane_writes);
        try testing.expectEqual(@as(u64, 0), cs.windows_inflight.load());
        try testing.expectEqual(@as(u32, 0), be.cv.in_apply.load(.acquire));
        try testing.expectEqual(@as(u64, 0), cs.queued.load());
        try testing.expectEqual(@as(u64, 0), cs.commit_parked.load());
        try testing.expectEqual(@as(u64, 0), be.cache.heapPromised());
        // Every window's apply leg was recorded (the inline arm records it
        // too: the phase is the applier's wherever it ran).
        try testing.expectEqual(cs.lane_handoffs.load(), cs.apply_total.snapshot().count);
        be.close();
    }
    {
        const be = try v.open();
        defer be.close();
        try testing.expect(be.replayStats().clean_unmount);
        for (acked) |lane_acks| for (lane_acks) |ino| try testing.expect(try present(be, ino));
    }
}

test "the applier's resolve starts from the committer's heap-admission leaves: on a quiet volume every record's leaf is the seed (resolve_seeded ≡ records, none descended); under a storm with checkpoints and splits seeded + descended ≡ records applied, every ack present" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "resolve-seeds", "20");
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    const cs = be.conveyorStats();
    const seeded0 = cs.resolve_seeded.load();
    const descended0 = cs.resolve_descended.load();
    // Quiet: one committer, no SMO between the admission and the apply.
    const quiet = 24;
    for (0..quiet) |i| {
        var nb: [24]u8 = undefined;
        const n = std.fmt.bufPrint(&nb, "seed-{d}", .{i}) catch unreachable;
        _ = try commitCreate(be, n);
    }
    try testing.expectEqual(seeded0 + recordsOfCreates(quiet), cs.resolve_seeded.load());
    try testing.expectEqual(descended0, cs.resolve_descended.load());
    // A storm with checkpoints: leaves split under the queued txs, their
    // seeds go stale and the apply descends for those — never a record
    // resolved by neither.
    const threads = 4;
    const per_thread = 60;
    const Storm = struct {
        fn run(b: *KvBackend, lane_id: usize) void {
            for (0..per_thread) |i| {
                var nb: [24]u8 = undefined;
                const n = std.fmt.bufPrint(&nb, "seeds-{d}-{d}", .{ lane_id, i }) catch unreachable;
                _ = commitCreate(b, n) catch unreachable;
                if (i % 20 == 19) b.checkpointNow() catch unreachable;
            }
        }
    };
    var handles: [threads]std.Thread = undefined;
    for (&handles, 0..) |*h, i| h.* = try std.Thread.spawn(.{}, Storm.run, .{ be, i });
    for (handles) |h| h.join();
    const total = recordsOfCreates(quiet + threads * per_thread);
    // A revalidation retry re-resolves a member: ≥, exact when none retried.
    try testing.expect(cs.resolve_seeded.load() - seeded0 + cs.resolve_descended.load() - descended0 >= total);
    try testing.expect(cs.resolve_seeded.load() - seeded0 >= total / 2);
}

/// The records `n` `commitCreate`s stage — the inode `Put`, the dentry
/// `Put`, the parent's Δ (the harness stages no park) — each resolved once.
fn recordsOfCreates(n: usize) u64 {
    return @as(u64, n) * 3;
}

test "the committer venue (SQUEEZEFS_META_PASS_VENUE=committer): a 4-thread storm with concurrent checkpoints — the election winner runs the pass and the apply on its own thread (the pass never ran on the lane), every ack present, drained ≡ acked, no window left, the reopen replays every acked record" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "storm-committer-pass", "20");
    defer v.destroy();
    try v.env.put("SQUEEZEFS_META_PASS_VENUE", "committer");
    const threads = 4;
    const per_thread = 60;
    var acked: [threads][per_thread]u64 = undefined;
    {
        const be = try v.open();
        try testing.expect(be.pass_on_committer);
        try testing.expectEqual(@as(u32, 1), be.cv.deps.applier.parallelism);
        const Storm = struct {
            fn run(b: *KvBackend, lane_id: usize, out: *[per_thread]u64) void {
                for (0..per_thread) |i| {
                    var nb: [24]u8 = undefined;
                    const n = std.fmt.bufPrint(&nb, "cpass-{d}-{d}", .{ lane_id, i }) catch unreachable;
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
        try testing.expectEqual(threads * per_thread + guard.gate.OPEN_COMMITS, cs.acks.load());
        try testing.expectEqual(@as(u64, 0), cs.refusals.load());
        try testing.expectEqual(cs.drained_txs.load(), cs.acks.load() + cs.refusals.load());
        try testing.expectEqual(cs.lane_handoffs.load(), be.laneSnapshot().lane_writes);
        try testing.expectEqual(@as(u64, 0), cs.windows_inflight.load());
        try testing.expectEqual(@as(u64, 0), cs.queued.load());
        try testing.expectEqual(@as(u64, 0), cs.commit_parked.load());
        try testing.expectEqual(@as(u64, 0), be.cache.heapPromised());
        // The venue pin: no pass ran on the lane's thread.
        try testing.expect(be.cv.passThread() != @as(u64, @intCast(be.lane.threadId())));
        be.close();
    }
    {
        const be = try v.open();
        defer be.close();
        try testing.expect(be.replayStats().clean_unmount);
        for (acked) |lane_acks| for (lane_acks) |ino| try testing.expect(try present(be, ino));
    }
}

test "ONE thread per volume: the pass runs on sqz-jrnl<N>; a 4-thread storm of single-record txs measures io_uring_enter per commit, fiber parks per window ≤ 2 (coalesced), 0 tick rescues, lane_writes ≡ passes, acks in order" {
    const gpa = testing.allocator;
    // Coalesced at the knob's ceiling: no barrier shares the count.
    const v = try TestVol.create(gpa, "enters", "600000");
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    const threads = 4;
    const per_thread = 64;
    const Storm = struct {
        fn run(b: *KvBackend, lane_id: u64) void {
            for (0..per_thread) |i| {
                const t = b.begin(@src()) catch unreachable;
                const ino: u64 = 1000 + lane_id * per_thread + i;
                var xb: [256]u8 = undefined;
                const xv = (record.XattrValue{ .name = "user.one", .value = "x" }).encodeInto(&xb) catch unreachable;
                t.stagePut(key.TREE_XATTRS, &key.xattrKey(ino, key.xattrNameHash56("user.one", b.nameKey()), 0), xv) catch unreachable;
                b.commit(t) catch unreachable;
            }
        }
    };
    var handles: [threads]std.Thread = undefined;
    for (&handles, 0..) |*h, i| h.* = try std.Thread.spawn(.{}, Storm.run, .{ be, @as(u64, i) });
    for (handles) |h| h.join();
    be.cv.quiesce();
    const cs = be.conveyorStats();
    const snap = be.laneSnapshot();
    const total: u64 = threads * per_thread;
    try testing.expectEqual(total + guard.gate.OPEN_COMMITS, cs.acks.load());
    try testing.expectEqual(@as(u64, 0), cs.refusals.load());
    // lane_writes ≡ passes ≡ handoffs: every pass handed off exactly one window.
    try testing.expectEqual(cs.leader_passes.load(), cs.lane_handoffs.load());
    try testing.expectEqual(cs.lane_handoffs.load(), snap.lane_writes);
    try testing.expectEqual(@as(u64, 0), snap.tick_rescues);
    try testing.expectEqual(@as(u64, 0), journal_lane.stats.ack_out_of_order.load());
    // Parks per window: the wake (admit) and the CQE (group + ack) at most.
    // Every park ends on one wake, and every wake has a cause the lane
    // counts: a publish (a window, a structural entry — the storm's leaf
    // splits — or a barrier request), a CQE (one per run pushed, one per
    // barrier), the completion kick a structural entry owes, the backstop
    // tick; + 1 for the idle park after the last window. The bound was
    // `2 × windows + 1`: it held only while the storm's windows coalesced
    // enough to absorb the splits' entries and the barrier — under 8-way
    // load the windows grew, each cost its 2 wakes, and 2 splits + 1
    // barrier tipped it (256 parks over 125 windows, 3/100 runs red).
    const barriers = snap.barriers_coalesced + snap.barriers_strict;
    const publishes = snap.lane_writes + snap.structural_writes + barriers;
    const cqes = snap.runs + barriers;
    try testing.expect(snap.parks <= publishes + cqes + snap.structural_writes + snap.backstop_ticks + snap.tick_rescues + 1);
    // THE venue pin: stage A ran on the journal lane's thread.
    try testing.expect(cs.leader_passes.load() > 0);
    try testing.expectEqual(@as(u64, @intCast(be.lane.threadId())), be.cv.passThread());
    std.debug.print("\nmeasurement: {d} single-record commits in {d} passes/windows: io_uring_enter {d} ({d}.{d:0>2} per commit, {d}.{d:0>2} per window), fiber parks {d} ({d}.{d:0>2} per window), tick rescues {d}, barriers {d}\n", .{ total, snap.lane_writes, snap.enters, snap.enters / total, (snap.enters * 100 / total) % 100, snap.enters / snap.lane_writes, (snap.enters * 100 / snap.lane_writes) % 100, snap.parks, snap.parks / snap.lane_writes, (snap.parks * 100 / snap.lane_writes) % 100, snap.tick_rescues, snap.barriers_coalesced + snap.barriers_strict });
}

fn anyMaintenancePending(be: *KvBackend) bool {
    for (be.trees.load()) |t| if (t.maintenancePending()) return true;
    return false;
}

test "threshold writeback on a mounted volume: commits that push one leaf over the writeback threshold are drained by the checkpoint task — the bset page is appended before any cadence cycle, the tree's queue empties, queue_full stays 0" {
    const gpa = testing.allocator;
    // A 600 s flush interval: no cadence tick and no lane barrier fires on
    // its own, so every append below is the threshold drain's.
    const v = try TestVol.create(gpa, "threshold", "600000");
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    const native = be.treeOf(.native).?;
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
    // Every tree's queue (the arm's lease grants load the control tree's
    // leaf too), not the native one alone.
    const started = clock.monotonicNs();
    while (anyMaintenancePending(be) or native.maintenancePending() or checkpoint.stats.meta_kv_maintenance_items.load() == items0) {
        if (clock.monotonicNs() - started > 10 * std.time.ns_per_s) return error.ThresholdDrainNeverRan;
        std.Thread.yield() catch {};
    }
    try testing.expect(checkpoint.stats.meta_kv_maintenance_items.load() > items0);
    try testing.expect(device_loader.stats.appends.load() > appends0);
    try testing.expectEqual(checkpoints0, checkpoint.stats.meta_kv_checkpoints.load());
    try testing.expectEqual(queue_full0, tree.stats.meta_kv_maintenance_queue_full.load());
    for (be.trees.load()) |t| try testing.expect(!t.maintenancePending());
}

test "the admission park on the device: the smallest ring fills under a serial create stream (every commit a grain-padded window), the pass parks holding no node lock, the park kick runs a barriered cycle, every commit lands" {
    const gpa = testing.allocator;
    // The smallest admissible ring (256 KiB + one max entry behind appender
    // 0's four page slots): its user slice is ~128 KiB, a few hundred
    // padded commits.
    const v = try TestVol.createWithRing(gpa, "park", "600000", journal.RING_OVERRIDE_FLOOR + geometry.appender0_reserved_bytes);
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
    try testing.expectEqual(acked.items.len + guard.gate.OPEN_COMMITS, cs.acks.load());
    try testing.expectEqual(@as(u64, 0), cs.refusals.load());
    try testing.expect(!be.isFailed());
    std.debug.print("\nadmission park: {d} serial commits on a {d}-page ring (grain {d}) — {d} stalls, {d} park-kicked cycles\n", .{ acked.items.len, be.geo.pages, be.geo.grain, cs.full_stalls.load(), checkpoint.stats.meta_kv_checkpoint_park_cycles.load() - park_cycles0 });
}

/// Spin (yield, never sleep) until `cond` holds or the bound passes.
fn spinUntil(ctx: anytype, comptime cond: fn (@TypeOf(ctx)) bool, bound_ns: u64) error{Timeout}!void {
    const started = clock.monotonicNs();
    while (!cond(ctx)) {
        if (clock.monotonicNs() - started > bound_ns) return error.Timeout;
        std.Thread.yield() catch std.atomic.spinLoopHint();
    }
}

/// The pass fiber's delivery word as the venue sees it.
fn passFiberIdle(be: *KvBackend) bool {
    return be.pass_fiber.?.state.load() == .idle;
}

/// The pass fiber is parked ON THE QUEUE MUTEX: the word it contended reads
/// `contended` (its `swap` before the futex wait) and its delivery word
/// IDLE (the lane's `endPollPending` after the park).
fn passFiberParkedOnQueueMutex(be: *KvBackend) bool {
    return be.cv.core.mutex.state.load(.acquire) == .contended and passFiberIdle(be);
}

// The 2026-10-10 wedge, pinned on the real venue (rung 9 F1; model 7 in
// `conveyor_core_model.zig`): the retiring pass fiber parks on the queue
// mutex inside its `pending()` re-check while a committer holds the lock in
// its enqueue; the committer's unlock schedules the fiber, its CAS wins the
// released leadership, and its `wakePass` is absorbed by the already-
// SCHEDULED word. Without the venue's leader re-read the fiber parks with
// the latch set and one entry queued: the committer — and every later one —
// parks forever (`meta_conveyor_queued` = `meta_commit_parked`, no window in
// flight, every lane thread in `io_cqring_wait`).
test "the venue delivers every election: a committer whose enqueue parked the retiring pass on the queue mutex is drained, never left with the leader latched and the pass fiber parked (the 2026-10-10 wedge)" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "venue-election", "600000");
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    const cs = be.conveyorStats();
    const io = v.io();
    const bound = 10 * std.time.ns_per_s;
    const seams = &conveyor_core.test_seams;
    defer {
        conveyor_core.TestSeams.release(io, &seams.hold_after_unlead);
        conveyor_core.TestSeams.release(io, &seams.hold_in_enqueue);
    }

    // 1. The pass retires through the held window: one commit drains, the
    //    pass stores `leader = false` and holds BEFORE its re-check.
    conveyor_core.TestSeams.arm(&seams.hold_after_unlead, &seams.unlead_held);
    _ = try commitCreate(be, "venue-1");
    try spinUntil(seams, struct {
        fn f(s: *conveyor_core.TestSeams) bool {
            return s.unlead_held.load(.acquire) != 0;
        }
    }.f, bound);
    try testing.expect(!be.cv.leaderHeld());

    // 2. A committer takes the queue mutex in its enqueue and holds it.
    conveyor_core.TestSeams.arm(&seams.hold_in_enqueue, &seams.enqueue_held);
    const Committer = struct {
        result: anyerror!u64 = error.NotRun,
        /// release pairs with the test's acquire: `result` is written first.
        done: std.atomic.Value(bool) = .init(false),

        fn run(b: *KvBackend, c: *@This()) void {
            c.result = commitCreate(b, "venue-2");
            c.done.store(true, .release);
        }
    };
    var c: Committer = .{};
    const committer = try std.Thread.spawn(.{}, Committer.run, .{ be, &c });
    try spinUntil(seams, struct {
        fn f(s: *conveyor_core.TestSeams) bool {
            return s.enqueue_held.load(.acquire) != 0;
        }
    }.f, bound);

    // 3. The pass resumes into `pending()` and PARKS on the held mutex.
    conveyor_core.TestSeams.release(io, &seams.hold_after_unlead);
    try spinUntil(be, passFiberParkedOnQueueMutex, bound);
    try testing.expect(!be.cv.leaderHeld());

    // 4. The committer pushes, unlocks (the futex wake schedules the pass
    //    fiber), wins the released leadership and wakes the venue — a wake
    //    the SCHEDULED word absorbs. The pass fiber's poll then re-checks:
    //    one entry queued, `tryLead` lost to the committer.
    conveyor_core.TestSeams.release(io, &seams.hold_in_enqueue);
    const Watch = struct { be: *KvBackend, c: *const Committer };
    const w: Watch = .{ .be = be, .c = &c };
    spinUntil(w, struct {
        fn f(x: Watch) bool {
            if (x.c.done.load(.acquire)) return true;
            // The wedge: parked IDLE with the latch set and the entry queued.
            return passFiberIdle(x.be) and x.be.cv.leaderHeld() and x.be.cv.pending() == 1;
        }
    }.f, bound) catch |e| {
        // Neither outcome within the bound: unwedge for teardown, then fail.
        be.cv.deps.pass.wake(be.cv.deps.pass.ctx);
        committer.join();
        return e;
    };
    const wedged = !c.done.load(.acquire);
    if (wedged) be.cv.deps.pass.wake(be.cv.deps.pass.ctx); // the test's own rescue, so teardown completes
    committer.join();
    try testing.expect(!wedged);
    _ = try c.result;
    try testing.expectEqual(@as(usize, 0), be.cv.pending());
    // The re-read delivered the election; the venue's tick backstop never had to.
    try testing.expectEqual(@as(u64, 0), cs.park_tick_rescues.load());
}

// The venue's park backstop: an election whose wake is DROPPED (the seam
// stands in for any future lost-wake class) is found by the pass fiber's 2 s
// liveness tick — the commit completes, `meta_conveyor_park_tick_rescues`
// counts exactly the one rescue, and nothing wedges.
test "the venue's tick backstop: a dropped election wake is absorbed by the pass fiber's liveness tick and counted as one park_tick_rescue; the commit completes" {
    const gpa = testing.allocator;
    const v = try TestVol.create(gpa, "venue-backstop", "600000");
    defer v.destroy();
    const be = try v.open();
    defer be.close();
    const cs = be.conveyorStats();
    const rescues0 = cs.park_tick_rescues.load();
    conveyor.test_seams.drop_pass_wakes.store(1, .release);
    defer conveyor.test_seams.drop_pass_wakes.store(0, .release);
    const t0 = clock.monotonicNs();
    const ino = try commitCreate(be, "backstop-1");
    const waited = clock.monotonicNs() - t0;
    try testing.expectEqual(@as(u32, 0), conveyor.test_seams.drop_pass_wakes.load(.acquire));
    try testing.expect(try present(be, ino));
    try testing.expectEqual(rescues0 + 1, cs.park_tick_rescues.load());
    // The rescue rode the liveness tick, not a faster path and not a wedge.
    try testing.expect(waited >= sync.tick.tick_ns / 2 and waited < 4 * sync.tick.tick_ns);
    // A later election with its wake delivered needs no rescue.
    _ = try commitCreate(be, "backstop-2");
    try testing.expectEqual(rescues0 + 1, cs.park_tick_rescues.load());
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
    const vol = env.get(crash_child_knob) orelse return testkit.skip(@src(), .no_seam, "the kill -9 matrix's child marker is not set (the parent test re-execs this body)");
    var ledger_buf: [192]u8 = undefined;
    const ledger_path = try std.fmt.bufPrint(&ledger_buf, "{s}.acks", .{vol});
    const sqz = try exec.SqzIo.testInstance();
    defer sqz.deinit();
    const be = try KvBackend.open(gpa, sqz.io(), vol, .{ .env = &env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
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

test "kill -9: a child mid-commit-stream is SIGKILLed; the reopened volume holds every ledger-acked create whole, next_ino is above every minted ino, the torn tripwire never fires" {
    const gpa = testing.allocator;
    const idx = childTestIndex() orelse return testkit.skip(@src(), .no_seam, "the crash child's test index is not resolvable in this binary");
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
        std.debug.print("\nkill -9 round: {d} acked creates present after SIGKILL; replay {d} entries, {d} torn dropped, next_ino > {d}\n", .{ acked_n, rs.entries, rs.dropped_torn, max_ino });
    }
    try testing.expectEqual(tripwire0, journal.replay_dropped_after_clean_unmount.load());
}
