//! The filesystem: the POSIX must-implement set over the KV metadata
//! backend and the data plane (charter §5.1 keys and records, §5.2 journal
//! economy, §6.1 layouts and promotion, §6.2 the write admission, §6.3 the
//! read path, §6.4 fsync; `docs/porting/fuse_handlers_lanes.md` §5;
//! `fuse_abi_session.md` §6, §13 KILLPRIV_V2; `data_layouts.md` §2).
//!
//! Generic over a `Store` (comptime duck-typed, `assertStore`) AND a `Data`
//! face (`assertData`): the Linux `meta/backend.KvBackend` + `data/plane.
//! DataPlane` is the product binding (`handler.BackendFs`); the in-RAM
//! `MemStore` + `NoData` below runs the same POSIX suite on every host as
//! a set with NO data volumes (the honest posture of such a set: a file
//! lives inline and a write past `inline_max` is `EFBIG` — the largest
//! file the set can hold, counted `fuse_write_no_data_volume_refused`).
//!
//! THE LAYOUT RECORD is the v4 `layout.Layout` under the xattr `layout`
//! (decision 0002 rows 2, 95–97): inline (the payload IS the record),
//! staged (the ring entry `file_id ‖ ino`), packed (a tenant window),
//! striped (the inline block map). This module owns every layout-record
//! TRANSACTION — the write's routing (`inline` ≤ `inline_max`; `staged` ≤
//! one block when the set declares cache paths; else `striped`, promoting
//! the resident payload DURABLY to its own block BEFORE the type flip), the
//! truncate per kind (passthrough shrink of a packed tenant = the mapping
//! clip; a striped shrink clips the map and zero-completes the kept tail
//! so a later grow reads zeros), the destroy rider (`−ref` records ride the
//! corpse's destroy entry, every freed block through the reclaimer AFTER
//! the commit) — and the data plane supplies the block-level acts behind
//! them (the striped admission per piece, the reads, the own-block DMA,
//! the staging ring, the frees, the fsync legs, `df`). Every publish goes
//! through `layout.publishLayout` so the `+ref`/`−ref` diff rides the SAME
//! transaction (charter §5.6) and a block can never be named before its
//! `DmaProof` exists.
//!
//! Lock discipline (charter §2): this module is BELOW level (1) — the
//! lifecycle took the op guard, classified (`WriteArgs.class`) — and takes
//! the 4a object locks `I{ino}` / `D{parent,name}` through `dlm.Manager.
//! lockMany` as ONE canonical set per op; an op that discovers its targets
//! mid-flight (unlink's child, rename's victim) reads first WITHOUT guards,
//! locks the full set, then REVALIDATES under the guards before staging.
//! The held set travels into the transaction (`KvTx.holdGuards`) and the
//! conveyor drops it at the terminal outcome BEFORE the ack (D-3). A
//! striped write takes NO 4a guard across its data I/O: the pieces run
//! under the write path's level (3) and the inode's size claim is a
//! postlude transaction (SIZE NEVER LEADS DATA). A TRUNCATE (SETATTR size,
//! `O_TRUNC`) and the tree-mapped corpse mark take the data plane's
//! per-ino layout lock — level (3.7), `Data.lockLayout` — BEFORE the 4a
//! set and hold it until the commit landed: the layout record they read is
//! the one their commit lands on, against the plane's merges, sweeps and
//! promotions (the head's ONE authority; 0002 row 210). It drops before
//! `afterCommit`, whose synchronous sweep takes it again.
//!
//! Records: inode `ino ‖ 01` (`record.InodeValue`), dentry `parent ‖ 02 ‖
//! hash54 ‖ coll` (the GLOBAL child ino), xattr `ino ‖ 03 ‖ hash56 ‖ coll`
//! — user xattrs, `system.symlink` (the target) and `layout` share the
//! kind; the two reserved names are screened from every xattr op.
//!
//! Times are i64-ns stamps from the coarse realtime clock; the SETATTR echo
//! is ABSORBED through `economy.PendingTimes` (never blanket suppression);
//! nlink/ctime/mtime move only through `economy.stage*`. Readdir cookies are
//! `key.encodeReaddirCookie` (`3 + (hash54 << 8 | coll)`); `.` is offset 1,
//! `..` offset 2 — the parent of a directory is a RAM memo filled by every
//! lookup/mkdir/rename (the KV holds no parent pointer; the kernel never
//! asks about a directory it has not looked up, except the root, whose
//! `..` is itself).
//!
//! KILLPRIV_V2 (fuse_abi_session.md §13): on a WRITE flagged `FUSE_WRITE_
//! KILL_SUIDGID`, a SETATTR flagged `FATTR_KILL_SUIDGID` (truncate, chown)
//! and an O_TRUNC OPEN flagged `FUSE_OPEN_KILL_SUIDGID`: `S_ISUID` is always
//! cleared, `S_ISGID` only when `S_IXGRP` is set (sgid without group-exec is
//! the mandatory-locking marker — preserved), and `security.capability` is
//! dropped if present — folded into the op's ONE commit (`fuse_killpriv_
//! clears`). Without V2 the kernel sends a `GETXATTR("security.capability")`
//! per `write(2)` — half of every write-syscall-bound stream's requests.
//!
//! Unlinked corpses (`nlink == 0`) survive until the kernel's final FORGET
//! (an unlinked-open file is still readable), then join a RECLAIM batch
//! whose destroy is ONE journal entry per `RECLAIM_BATCH` inos packed under
//! the whole-entry cap with their `−ref` records (`inos.stageDestroy`,
//! `inos.packDestroyEntries`; the batched-destroy economy, `unlink ≤ 1.05
//! entries/op`). A forgotten corpse is unreachable by name and unknown to
//! the kernel, so its destroy needs no 4a guard. A corpse whose FORGET or
//! gather died with the daemon (`kill -9`) is found by the next mount's
//! sweep (`sweepCorpsesAtMount`: one pass over the inode tree for
//! `nlink == 0`, the same destroy rider) before any request is served.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const linux = std.os.linux;
const assert = std.debug.assert;

const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const mount_options = @import("../mount_options.zig");
const filesystem = @import("filesystem.zig");
const op_guard = @import("op_guard.zig");
const op_trace = @import("op_trace.zig");
const errors = @import("../../core/errors.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const tx = @import("../../meta/tx.zig");
const tree = @import("../../meta/tree.zig");
const dlm = @import("../../meta/dlm.zig");
const lock_order = @import("../../sync/lock_order.zig");
const sync = @import("../../sync/sync.zig");
const economy = @import("../../meta/economy.zig");
const guard_claim = @import("../../meta/guard/claim.zig");
const virtual_inodes = @import("virtual_inodes.zig");
const inos = @import("../../meta/inos.zig");
const jc = @import("../../meta/journal_core.zig");
const route_table = @import("../../meta/routing/route_table.zig");
const sym_handover = @import("../../meta/sym/slots/handover.zig");
const sym_acquire = @import("../../meta/sym/slots/acquire.zig");
const read_divert = @import("../../meta/sym/tokens/read_divert.zig");
const sym_xv = @import("../../meta/sym/xv.zig");
const layout = @import("../../data/layout.zig");
const staging = @import("../../data/staging.zig");
const block_refs = @import("../../data/block_refs.zig");
const pack = @import("../../data/pack.zig");
const write_admit = @import("../../data/write_admit.zig");
const seams = @import("../../data/write_seams.zig");
const ipc_wire = @import("../../ipc/wire.zig");

const Ctx = filesystem.Ctx;
const Error = filesystem.Error;
const InodeValue = record.InodeValue;
const S = economy.S;
const Layout = layout.Layout;
const Mapping = layout.Mapping;

/// The xattr that carries a file's layout record (charter §5.1).
pub const LAYOUT_XATTR: []const u8 = layout.LAYOUT_XATTR_NAME;
/// The Linux capability xattr KILLPRIV_V2 drops.
pub const CAPS_XATTR: []const u8 = "security.capability";
/// `NAME_MAX`: `f_namelen` and every name op's ENAMETOOLONG bound.
pub const NAME_MAX: usize = record.NAME_MAX;
/// Corpses per destroy entry: the reclaim gather's measured fill (≈ 62 at
/// 100 k-op storms), so `1 + 1/64 + drain ≤ 1.05` entries per unlink.
pub const RECLAIM_BATCH: usize = 64;
/// The mount sweep's scan page over the inode tree: a page of ≈ 100 B
/// inode records fits one node read; the sweep is one pass.
pub const SWEEP_PAGE: usize = 1024;
/// `statfs.f_frsize`/`f_bsize`: the page — the unit the kernel's `df`
/// arithmetic expects and the inline bound's unit.
pub const STATFS_BSIZE: u32 = 4096;
/// `f_ffree`: inos are a monotone 64-bit watermark, never reused; the
/// report is the remaining watermark space (an over-report, never under).
pub const INO_SPACE: u64 = 1 << 56;
/// A truncate's zero-complete of the kept partial block and a corpse's
/// binding gather bound the on-stack work: the inline map's deepest cap
/// at the 256 KiB node (`(64 KiB − framing) / 48`) is 1,364 entries.
pub const MAX_BINDINGS_PER_INO: usize = 1400;

/// `renameat2(2)` flags (uapi `linux/fs.h`; `std.os.linux` carries none).
pub const RENAME_NOREPLACE: u32 = 1 << 0;
pub const RENAME_EXCHANGE: u32 = 1 << 1;
pub const RENAME_WHITEOUT: u32 = 1 << 2;
/// `setxattr(2)` flags (uapi `linux/xattr.h`).
pub const XATTR_CREATE: u32 = 0x1;
pub const XATTR_REPLACE: u32 = 0x2;
pub const O_TRUNC = filesystem.O_TRUNC;
pub const O_DIRECT = filesystem.O_DIRECT;

// ---------------------------------------------------------------------------
// KILLPRIV_V2.
// ---------------------------------------------------------------------------

/// `kill_suidgid_mode`: suid always; sgid iff group-exec.
pub fn killSuidgidMode(mode: u32) u32 {
    var m = mode & ~@as(u32, S.ISUID);
    if (m & S.IXGRP != 0) m &= ~@as(u32, S.ISGID);
    return m;
}

// ---------------------------------------------------------------------------
// Instruments.
// ---------------------------------------------------------------------------

/// `fuse_write_no_data_volume_refused`: a WRITE/truncate past `inline_max`
/// on a set with NO data volumes answered EFBIG — the set's honest
/// capacity, not a stopgap (a set with volumes never refuses here).
pub var fuse_write_no_data_volume_refused: counters.ShardedCounter = .{};
/// `fuse_write_u32_index_refused`: a WRITE or a SETATTR size whose end lies
/// past `block × u32::MAX` — block indices are u32 across the striped
/// layout (map keys, refs, the pipeline), `u32::MAX` itself is the reserved
/// MAP_BLOB sentinel, so the file ceiling is 16 PiB at 4 MiB blocks and
/// the refusal is EFBIG at admission with zero map-plane side effects (the
/// codec's refusal of the reserved index stays structurally unreachable).
pub var fuse_write_u32_index_refused: counters.ShardedCounter = .{};
/// `fuse_kvmap_sweeps_kicked`: shrinks and unlinks of tree-mapped files
/// that planted a sweep cursor (size-flip-first) and kicked the drain.
pub var fuse_kvmap_sweeps_kicked: counters.ShardedCounter = .{};
/// The write routing per layout arm.
pub var fuse_write_inline: counters.ShardedCounter = .{};
pub var fuse_write_staged: counters.ShardedCounter = .{};
pub var fuse_write_striped: counters.ShardedCounter = .{};
/// `fuse_write_times_parked`: size-neutral striped writes whose Δ(mtime,
/// ctime) was PARKED (decision 0003 §L.6) — fold-visible on every read,
/// journaled by the batched drain — instead of committed as a 76 B entry
/// that cost one grain-aligned window and one device round trip per op.
pub var fuse_write_times_parked: counters.ShardedCounter = .{};
/// Durable promotions a write forced BEFORE its type flip (inline/staged/
/// packed → striped: the resident payload landed on its own block).
pub var fuse_write_promotions_to_striped: counters.ShardedCounter = .{};
/// `fuse_write_staged_ring_full_own_block`: staged-arm writes the ring
/// refused (no hole for the image, the pool at its cap) that took the
/// own-block arm instead of answering ENOSPC — the ring is a cache, never
/// the set's capacity (rung 9: the fsstress 20-writer burst).
pub var fuse_write_staged_ring_full_own_block: counters.ShardedCounter = .{};
/// Truncates by arm: the packed passthrough clip (zero device writes),
/// the striped map clip (+ the kept tail's zero-complete), the re-stage.
pub var fuse_truncate_packed_clips: counters.ShardedCounter = .{};
pub var fuse_truncate_striped_clips: counters.ShardedCounter = .{};
pub var fuse_truncate_restages: counters.ShardedCounter = .{};
/// The kept partial block's zero-complete after a striped shrink: bytes the
/// filesystem itself admitted (the admission ledger ≡ WRITE payload bytes +
/// these).
pub var fuse_truncate_zero_tail_bytes: counters.ShardedCounter = .{};
/// `fuse_killpriv_clears`: KILLPRIV_V2 strips folded into a commit.
pub var fuse_killpriv_clears: counters.ShardedCounter = .{};
/// `fuse_killpriv_negotiated`: 0/1 from the INIT reply.
pub var fuse_killpriv_negotiated: counters.Gauge = .{};
/// `fuse_reclaim_destroys`: corpses destroyed; `fuse_reclaim_drains`:
/// destroy entries committed (the batched-destroy economy's two words);
/// `fuse_reclaim_blocks_freed`: bindings the destroy riders released.
pub var fuse_reclaim_destroys: counters.ShardedCounter = .{};
/// Corpses the MOUNT sweep destroyed (left at `nlink == 0` by a crashed
/// daemon whose forgets never drained).
pub var fuse_corpse_sweep_destroyed: counters.ShardedCounter = .{};
pub var fuse_reclaim_drains: counters.ShardedCounter = .{};
pub var fuse_reclaim_blocks_freed: counters.ShardedCounter = .{};
/// `fuse_reclaim_queue_overflow`: a forget found the gather full and drove
/// its corpse inline.
pub var fuse_reclaim_queue_overflow: counters.ShardedCounter = .{};
/// A dentry that names an ino whose record is gone (a corpse reachable by
/// name): fsck C9/C10 territory, never a handler outcome (must stay 0).
pub var dentry_without_inode: counters.Tripwire = .init("fuse_dentry_without_inode");
/// A stored layout record this binary cannot decode (must stay 0: the
/// codec is total over the encoder's forms).
pub var layout_record_corrupt: counters.Tripwire = .init("fuse_layout_record_corrupt");
/// `fuse_reserved_xattr_refusals`: the allowlist's engagement — a set/get/
/// remove of a name outside `user.*` (minus `user.squeezefs.`), `security.*`,
/// `trusted.*` (a counter, not a tripwire: a probing tenant is not a
/// cannot-happen).
pub var fuse_reserved_xattr_refusals: counters.ShardedCounter = .{};
/// `fuse_virtual_lookups`: `.stats`/`.config`/`.trace` generations minted.
pub var fuse_virtual_lookups: counters.ShardedCounter = .{};
/// `fuse_readdir_entries`: dirents answered (READDIR + READDIRPLUS, the
/// dot entries included) — the metadata read ledger's denominator for a
/// directory walk (`node_read_bytes ÷ entries`, decision 0003 §K).
pub var fuse_readdir_entries: counters.ShardedCounter = .{};

/// The `user.squeezefs.il0` rendezvous blob an ARMED session host
/// synthesizes (charter §7; `ipc/rendezvous.zig`): GETXATTR of the name
/// answers the 312 B blob (`size == 0` ⇒ the length, a short buffer ⇒
/// ERANGE); unbound (`-o interception` absent) the name stays the reserved
/// family's ENODATA — the cheap negative probe. Never on-disk bytes; the
/// screen already refuses SET (EPERM) and LIST filters the family.
pub const Il0Face = struct {
    ctx: *anyopaque,
    synthesize: *const fn (ctx: *anyopaque, out: *[ipc_wire.BLOB_LEN]u8) void,
};

/// The `.stats`/`.config`/`.trace` renderer the daemon binds (`stats/
/// stats.zig`): `build` renders one class's payload into bytes the
/// generation owns. Unbound (the loopback tests, a plane-less mount), the
/// three names are ordinary negative lookups.
pub const VirtualFace = struct {
    ctx: *anyopaque,
    build: *const fn (ctx: *anyopaque, class: virtual_inodes.Class, gpa: Allocator) Allocator.Error!virtual_inodes.Owned,
};

// ---------------------------------------------------------------------------
// The store contract and the data face.
// ---------------------------------------------------------------------------

pub fn assertStore(comptime T: type) void {
    comptime {
        for ([_][]const u8{ "StoreError", "coreError", "begin", "abandon", "commit", "lookup", "range", "fsync", "allocateIno", "allocatedInos", "nameKey", "nodeSize", "space", "isFailed" }) |name| {
            if (!@hasDecl(T, name)) @compileError(@typeName(T) ++ " is not a kv_fs Store: lacks `" ++ name ++ "`");
        }
    }
}

/// What the filesystem needs of its data plane (`data/plane.DataPlane` is
/// the product; `NoData` is the volume-less set).
pub fn assertData(comptime T: type) void {
    comptime {
        for ([_][]const u8{ "hasVolumes", "stagingOn", "blockSize", "inlineMax", "geometry", "refsSeam", "noteShape", "forgetShape", "isPassthrough", "canStoreHeld", "write", "read", "ownBlock", "abandonOwnBlock", "freeBindings", "flushCustody", "releaseIno", "fsync", "statfs", "mintFileId", "stageImage", "appendStaged", "releaseStaged", "stagedLease", "readMappingImage", "lockLayout", "stageKvmapShrink", "stageKvmapCorpse", "drainKvmapSweep" }) |name| {
            if (!@hasDecl(T, name)) @compileError(@typeName(T) ++ " is not a kv_fs Data face: lacks `" ++ name ++ "`");
        }
    }
}

pub const Options = struct {
    ttls: mount_options.Ttls = .{},
    /// The page: `fuse_attr.blksize`.
    page_size: u32 = 4096,
    /// The 4a stripe width (`dlm.resolveWidth` at the daemon).
    dlm_stripes: usize = dlm.dlm_stripes_shipped,
    /// The mount identity: the owner of the virtual control inodes.
    uid: u32 = 0,
    gid: u32 = 0,
    /// The coalesced cadence (`SQUEEZEFS_META_FLUSH_INTERVAL_MS` > 0): a
    /// size-neutral striped write PARKS its times (decision 0003 §L.6) and
    /// the daemon's drain cadence journals them. Strict mode (`= 0`, a
    /// barrier before every ack) commits them per write as before — that
    /// mount declared latency over bytes and its ack means durable.
    times_park: bool = true,
};

/// A held 4a set travelling with a transaction; it drops at the terminal
/// outcome (the commit door's per-slot tokens ride the queue entry beside
/// it — charter §5.5, taken at the conveyor's step 0).
const HeldGuards = struct {
    gpa: Allocator,
    io: Io,
    set: dlm.GuardSet,

    fn releaseFn(ctx: *anyopaque) void {
        const self: *HeldGuards = @ptrCast(@alignCast(ctx));
        self.set.release(self.io);
        self.gpa.destroy(self);
    }

    fn guardSet(self: *HeldGuards) tx.GuardSet {
        return tx.GuardSet.init(self, &releaseFn);
    }
};

/// A dentry as found: the record's position plus the child's two inos.
const Found = struct {
    pos: economy.DentryPos,
    local: u64,
    global: u64,

    fn isDir(self: Found) bool {
        return @as(u32, self.pos.file_type) << 12 == S.IFDIR;
    }
};

const ChainBuf = struct {
    keys: [256][]const u8 = undefined,
    len: usize = 0,

    fn slice(self: *const ChainBuf) []const []const u8 {
        return self.keys[0..self.len];
    }
};

/// The layout record as stored: the decoded layout borrows `value`.
pub const LayoutRec = struct {
    value: []u8,
    coll_seq: u8,
    layout: Layout,
};

/// What a layout mutation leaves for AFTER its commit (RES-1): bindings
/// to free through the reclaimer and a ring entry to release.
pub const AfterCommit = struct {
    bindings: [MAX_BINDINGS_PER_INO]layout.MapEntry = undefined,
    n: usize = 0,
    staged: ?struct { key: staging.StagingKey, generation: ?u64 } = null,
    /// A tree-mapped shrink or corpse mark landed: the sweep drains
    /// `removed` blocks (synchronously under the threshold, else on the
    /// meta pool).
    kvmap_sweep: ?struct { ino: u64, removed: u64 } = null,

    fn push(self: *AfterCommit, block: u32, m: Mapping) void {
        if (self.n < self.bindings.len) {
            self.bindings[self.n] = .{ .block = block, .mapping = m };
            self.n += 1;
        }
    }

    fn slice(self: *const AfterCommit) []const layout.MapEntry {
        return self.bindings[0..self.n];
    }
};

pub fn KvFs(comptime Store: type, comptime Data: type) type {
    assertStore(Store);
    assertData(Data);
    return struct {
        const Self = @This();

        gpa: Allocator,
        io: Io,
        store: *Store,
        data: *Data,
        ttls: mount_options.Ttls,
        inline_max: u64,
        block_size: u64,
        geo: layout.Geometry,
        blksize: u32,
        pending: economy.PendingTimes,
        /// `Options.times_park`.
        times_park: bool,
        /// The drain cadence's wake (the daemon's `TimesDrainCadence` parks
        /// on it with the flush interval as its deadline): a park that
        /// crosses `PENDING_TIMES_DRAIN_CAP` notifies it ahead of the tick.
        times_drain_wake: sync.Notify = .init,
        locks: dlm.Manager,
        /// The `..` memo (module doc).
        parents_lock: jc.SpinLock = .{},
        parents: std.AutoHashMapUnmanaged(u64, u64) = .empty,
        /// The reclaim gather.
        reclaim_lock: jc.SpinLock = .{},
        reclaim: std.ArrayList(u64) = .empty,
        next_fh: std.atomic.Value(u64) = .init(1),
        /// From the INIT reply.
        minor: u32 = abi.FUSE_KERNEL_MINOR_VERSION,
        killpriv_v2: bool = false,
        /// The mount identity (the virtual inodes' owner).
        uid: u32,
        gid: u32,
        /// The virtual control inodes (`virtual_inodes.zig`) and the
        /// renderer behind them.
        virtuals: virtual_inodes.Registry,
        virtual: ?VirtualFace = null,
        /// The armed session host's `il0` synthesizer; null = not armed.
        il0: ?Il0Face = null,
        /// The read divert (charter §5.5): every foreign-slot read of the
        /// five read verbs goes through the token client; null = a flat
        /// mount (two probes, read locally). Bound by the daemon after
        /// `mount`, BELOW the level-1 op guard and before any (3) guard.
        divert: ?*read_divert.Divert = null,
        /// The cross-owner plane (charter §5.5): an op whose objects span
        /// two members or two holders is ONE intent through it; null = a
        /// one-member mount (every op is the single entry). Bound by the
        /// daemon after `mount`.
        xv: ?*XvPlane = null,
        /// The set-wide `dir_rename` lease's taker (every directory-source
        /// rename across holders takes it); null = a flat mount.
        dir_rename: ?*sym_xv.dir_rename.Taker = null,
        /// Committed corpse destroys on this volume: the subtrahend of the
        /// LIVE inode population `statfs.ffree` reports (POSIX-1: never the
        /// monotone watermark, which made `IUsed` rise forever).
        destroys: inos.DestroyTally = .{},

        /// Bind the store and the data face (the daemon's mount; `init` is
        /// the FUSE op).
        pub fn mount(gpa: Allocator, io: Io, store: *Store, data: *Data, opts: Options) Allocator.Error!Self {
            var self: Self = .{
                .gpa = gpa,
                .io = io,
                .store = store,
                .data = data,
                .ttls = opts.ttls,
                .inline_max = data.inlineMax(),
                .block_size = data.blockSize(),
                .geo = data.geometry(),
                .blksize = opts.page_size,
                .pending = try economy.PendingTimes.init(gpa),
                .times_park = opts.times_park,
                .locks = undefined,
                .uid = opts.uid,
                .gid = opts.gid,
                .virtuals = virtual_inodes.Registry.init(gpa),
            };
            errdefer self.pending.deinit();
            self.locks = try dlm.Manager.init(gpa, opts.dlm_stripes);
            errdefer self.locks.deinit();
            try self.reclaim.ensureTotalCapacity(gpa, 4 * RECLAIM_BATCH);
            return self;
        }

        pub fn unmount(self: *Self) void {
            self.virtuals.deinit();
            self.reclaim.deinit(self.gpa);
            self.parents.deinit(self.gpa);
            self.locks.deinit();
            self.pending.deinit();
            self.* = undefined;
        }

        /// Bind the `.stats`/`.config`/`.trace` renderer (after `mount`,
        /// before the session arms).
        pub fn bindVirtual(self: *Self, face: VirtualFace) void {
            self.virtual = face;
        }

        /// Bind the token read divert (after `mount`, before the session
        /// arms).
        pub fn bindDivert(self: *Self, d: *read_divert.Divert) void {
            self.divert = d;
        }

        /// Arm the `il0` rendezvous blob (an interception mount's session
        /// host, after the session is up) — or, with null, unarm it (the
        /// host's teardown: GETXATTR answers ENODATA again).
        pub fn bindIl0(self: *Self, face: ?Il0Face) void {
            self.il0 = face;
        }

        pub const XvPlane = sym_xv.ops.Plane(Store);

        /// Bind the cross-owner plane (after `mount`, before the session
        /// arms); the roll-forward of open intents runs here, before any
        /// request is served.
        pub fn bindXv(self: *Self, p: *XvPlane) Error!u32 {
            self.xv = p;
            p.witness = .{ .ctx = self, .readInode = &xvWitnessRead };
            return p.recoverOpen(now()) catch |e| return xvErr(e);
        }

        /// The cadence tick the daemon's meta fiber drives: the expiry
        /// sweep, the roll-forward of abandoned intents, the gauges.
        pub fn xvCadenceTick(self: *Self) void {
            const p = self.xv orelse return;
            _ = p.cadenceTick(clock.monotonicNs()) catch |e| log.warn("cross-owner cadence: a roll-forward pass failed ({t}); the next tick retries", .{e});
        }

        fn xvWitnessRead(ctx: *anyopaque, global: u64) errors.Error!?InodeValue {
            const self: *Self = @ptrCast(@alignCast(ctx));
            return self.readInodeOpt(localOf(global) catch return null);
        }

        fn xvErr(e: sym_xv.ops.Error) Error {
            return sym_xv.ops.coreError(e);
        }

        /// Do these GLOBAL inos live on two members or two holders?
        fn spansHomes(self: *Self, globals: []const u64) bool {
            const p = self.xv orelse return false;
            return p.spans(globals);
        }

        /// Does another appender lease `global`'s slot (the record ship's
        /// resolver: a flat mount never says yes)?
        fn foreignSlot(self: *Self, global: u64) bool {
            const p = self.xv orelse return false;
            return !p.isLocal(global);
        }

        /// ONE shipped record verb to the file's holder.
        fn shipRecord(self: *Self, global: u64, call: sym_xv.record_ship.Call) Error!sym_xv.record_ship.Reply {
            const p = self.xv.?;
            return (sym_xv.record_ship.ship(p.resolver, p.volume, global, call, 0) catch |e| return sym_xv.record_ship.coreError(e)) orelse error.SlotMoved;
        }

        /// Run `plan` as ONE intent under its travelling guards; a live
        /// refusal answers the refused step's errno.
        fn runPlan(self: *Self, c: *Ctx, plan: *const sym_xv.steps.Plan) Error!sym_xv.ops.Done {
            const p = self.xv.?;
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            const scope = p.mintScope();
            var held = p.acquirePlanGuards(plan, scope, false) catch |e| return xvErr(e);
            defer held.release(self.gpa);
            const done = p.execute(plan, scope, now()) catch |e| return xvErr(e);
            if (done.refused) |e| return e;
            return done;
        }

        /// A locally dispatched op whose commit door answered `SlotBusy`
        /// (the slot read ours at the plan, another appender held it at the
        /// door) is re-dispatched ONCE through the cross-owner arm.
        fn redispatches(self: *Self, e: Error) bool {
            if (e != error.SlotMoved or self.xv == null) return false;
            sym_xv.ops.stats.xv_cross_owner_op_slot_moved_redispatches.inc();
            return true;
        }

        /// Whether `found` (a directory) holds any entry, at its holder.
        fn dirHasEntriesAt(self: *Self, found: Found) Error!bool {
            if (try self.tokenServe(found.local, .{ .dentries = true })) |served| {
                defer served.release(self.io);
                const e = served.entry orelse return false;
                return (e.dir orelse return false).len != 0;
            }
            return self.dirHasEntries(found.local);
        }

        /// `tokenServe(object, wants)`: null = read locally; else the pinned
        /// token entry (released by the caller) or `gone`.
        fn tokenServe(self: *Self, object: u64, wants: read_divert.cache.Wants) Error!?read_divert.cache.Serve {
            const d = self.divert orelse return null;
            return d.serve(self.io, object, wants) catch |e| return read_divert.coreError(e);
        }

        // ---- the virtual control inodes (virtual_inodes.zig) ----

        /// The frozen attributes of a generation: a regular 0400 file owned
        /// by the mount identity, `size` the served bytes, `blocks` in
        /// 512 B units, times = now.
        fn virtualAttr(self: *Self, ino: u64, size: u64) abi.fuse_attr {
            const stamp: i64 = @bitCast(now());
            const v: InodeValue = .{ .mode = S.IFREG | virtual_inodes.VIRTUAL_INODE_MODE, .uid = self.uid, .gid = self.gid, .nlink = 1, .size = size, .atime = @bitCast(stamp), .mtime = @bitCast(stamp), .ctime = @bitCast(stamp) };
            var a = codec.attrFromInode(ino, v, self.blksize);
            a.blocks = virtual_inodes.blocksOf(size);
            return a;
        }

        /// `lookup(1, ".stats"|".config"|".trace")`: render ONCE, mint a
        /// generation, reply with BOTH TTLs 0 (every walk revalidates — a
        /// fresh generation per consumer, a prompt FORGET).
        fn lookupVirtual(self: *Self, c: *Ctx, class: virtual_inodes.Class) Error!filesystem.Entry {
            const face = self.virtual orelse return error.FileNotFound;
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            const payload = try face.build(face.ctx, class, self.gpa);
            errdefer self.gpa.free(payload.buf);
            const ino = try self.virtuals.mint(class, payload);
            fuse_virtual_lookups.inc();
            return .{ .nodeid = ino, .entry_ttl_ns = 0, .attr_ttl_ns = 0, .attr = self.virtualAttr(ino, payload.len) };
        }

        fn virtualGetattr(self: *Self, ino: u64) Error!filesystem.Attr {
            const size = self.virtuals.size(ino) orelse return error.StaleHandle;
            return .{ .attr_ttl_ns = 0, .attr = self.virtualAttr(ino, size) };
        }

        /// A name a user may create, remove, rename or link at `parent`: the
        /// root's three virtual names are refused EPERM (they are served by
        /// lookup alone and a real dentry would be shadowed forever).
        fn checkMutableName(parent_global: u64, name: []const u8) Error!void {
            try checkName(name);
            if (parent_global == inos.ROOT_INO and virtual_inodes.classOfName(name) != null) return error.PermissionDenied;
        }

        // ---- clone (charter §8 `clone`; stats_cli.md §4.1) ----

        pub const CloneOutcome = struct {
            /// The dest's GLOBAL ino (a NEW inode).
            dest_ino: u64,
            /// Bindings now referenced by both owners with the SHARED bit.
            blocks_shared: u64,
            /// Inline bytes copied (an inline source has no blocks).
            inline_bytes: u64,
        };

        pub const CloneError = Error || error{ StagedSource, TreeMappedSource, NotAFile };

        /// INSTANT CoW at the refs level: a NEW dest inode, the source's
        /// layout record copied, and ONE transaction staging a durable
        /// SHARED `+ref` per binding for BOTH owners (a remount reads
        /// refcount 2 for a shared block instead of re-deriving it from a
        /// tree walk; W1's sole-owner patch class refuses both files until
        /// a CoW rewrite). Nothing is DMA'd: a packed tenant shares its
        /// window, an inline source copies its bytes. A STAGED source
        /// refuses (its acked payload lives in the mount's isolated staging
        /// — a clone would mint a zero-filled twin), as does a tree-mapped
        /// one (the block-map tree's clone is owed). Offline only: the
        /// verb holds the writer guard, so no RAM pin round is needed.
        pub fn cloneFile(self: *Self, c: *Ctx, src_global: u64, dest_parent: u64, dest_name: []const u8) CloneError!CloneOutcome {
            const src = try localOf(src_global);
            const sv = try self.readInode(src);
            if (sv.mode & S.IFMT != S.IFREG) return error.NotAFile;
            const rec = try self.readLayout(src);
            defer self.freeLayout(rec);
            const kind = kindOf(rec);
            if (kind == .staged) return error.StagedSource;
            if (rec) |r| if (r.layout.isKvmap()) return error.TreeMappedSource;
            const entry = try self.createChild(c, dest_parent, dest_name, .{ .mode = (sv.mode & 0o7777) | S.IFREG });
            const dest = try localOf(entry.nodeid);
            const t = try self.beginExclusive(dest, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            var dv = try self.readInode(dest);
            var shared: u64 = 0;
            var inline_bytes: u64 = 0;
            if (rec) |r| {
                const enc = try r.layout.encodeAlloc(self.gpa);
                defer self.gpa.free(enc);
                const k = try self.xattrKeyFor(dest, LAYOUT_XATTR);
                if (k.existing) |e| self.gpa.free(e);
                try self.stageXattrPut(t, &k.k, LAYOUT_XATTR, enc);
                if (kind == .inline_) inline_bytes = r.layout.data.len;
                var it = r.layout.named(self.geo.chunk);
                while (it.next()) |e| {
                    const base: block_refs.Reference = .{ .vol_tag = e.mapping.vol_tag, .block_idx = e.mapping.blockIdx(self.geo.chunk), .owner_ino = src, .block_index = e.block };
                    var dref = base;
                    dref.owner_ino = dest;
                    try block_refs.stageOps(t, &.{ block_refs.takenShared(base), block_refs.takenShared(dref) });
                    shared += 1;
                }
            }
            dv.size = sv.size;
            const stamp = now();
            dv.mtime = stamp;
            dv.ctime = stamp;
            try t.stagePut(key.TREE_INODES, &key.inodeKey(dest), &dv.encode());
            t_owned = false;
            try self.commit(c, t);
            _ = self.pending.retire(dest);
            self.data.noteShape(dest, sv.size, kind);
            return .{ .dest_ino = entry.nodeid, .blocks_shared = shared, .inline_bytes = inline_bytes };
        }

        // ---- helpers ----

        fn now() u64 {
            return @bitCast(clock.realtimeCoarseNs());
        }

        fn storeErr(e: Store.StoreError) Error {
            return Store.coreError(e);
        }

        /// A key ino → its global (`route_table.globalOfEffective`).
        fn globalOf(local: u64) u64 {
            return route_table.globalOfEffective(local);
        }

        /// A kernel nodeid → the KEY ino its records live under (charter
        /// §5.4; `route_table.effectiveIno`: slot 0 the native keyspace,
        /// every other slot its guest keyspace — a pure law, so the key
        /// stays valid across a slot flip; the store resolves the member).
        /// A nodeid outside the mint's space (garbage, another set's ino)
        /// is ENOENT — never an alias of a real record.
        pub fn localOf(global: u64) Error!u64 {
            if (!route_table.isRoutable(global)) return error.FileNotFound;
            return route_table.effectiveIno(global);
        }

        fn checkName(name: []const u8) Error!void {
            if (name.len == 0) return error.InvalidArgument;
            if (name.len > NAME_MAX) return error.NameTooLong;
        }

        /// The reserved-xattr screen is an ALLOWLIST (`stats_cli.md` §1.7):
        /// `user.*` minus the `user.squeezefs.` family, `security.*` and
        /// `trusted.*` pass; everything else — the layout record, the
        /// symlink target, the guard's control records on ino 1
        /// (`writer_claim`, `client:*`), `job:*`, "and anything added
        /// later" — is never listed, read, set or removed through FUSE. WHY
        /// an allowlist: a denylist left `system.symlink` (the VFS performs
        /// NO permission check for `system.*`), the layout (a decodable
        /// foreign value redirects reads to another file's blocks) and the
        /// claim writable from an unprivileged shell. The guard's own
        /// predicate stays the authority for its names.
        pub fn xattrNameAllowed(name: []const u8) bool {
            if (guard_claim.isGuardXattr(name)) return false;
            // `user.squeezefs.gather` is the ONE user-settable name of the
            // reserved family (charter §5.5 "`setfattr -n
            // user.squeezefs.gather -v 1 <dir>`"), admitted BY NAME.
            if (std.mem.eql(u8, name, sym_acquire.GATHER_XATTR)) return true;
            if (std.mem.startsWith(u8, name, "user.")) return !std.mem.startsWith(u8, name, "user.squeezefs.");
            return std.mem.startsWith(u8, name, "security.") or std.mem.startsWith(u8, name, "trusted.");
        }

        /// The directory's `user.squeezefs.gather` opt-in: its children
        /// mint into the directory's slot (read-mostly directories).
        fn gatherOptIn(self: *Self, parent: u64) Error!bool {
            const v = (try self.xattrValue(parent, sym_acquire.GATHER_XATTR)) orelse return false;
            defer self.gpa.free(v);
            return sym_acquire.gatherOptIn(v);
        }

        fn isReservedXattr(name: []const u8) bool {
            return !xattrNameAllowed(name);
        }

        /// POSIX ACL names are not supported (EOPNOTSUPP, like a filesystem
        /// mounted without `acl`); a reserved name is a counted refusal.
        fn screenXattr(name: []const u8, comptime refusal: Error) Error!void {
            if (std.mem.startsWith(u8, name, "system.posix_acl_")) return error.NotSupported;
            if (isReservedXattr(name)) {
                fuse_reserved_xattr_refusals.inc();
                return refusal;
            }
        }

        /// The inode as stored, FOLDED through the pending refinements;
        /// null when absent.
        fn readInodeOpt(self: *Self, ino: u64) Error!?InodeValue {
            if (try self.tokenServe(ino, .{})) |served| {
                // A foreign object: the holder's folded record (its pending
                // times are the holder's business).
                defer served.release(self.io);
                const e = served.entry orelse return null;
                return e.attrs;
            }
            const bytes = (self.store.lookup(self.gpa, key.TREE_INODES, &key.inodeKey(ino)) catch |e| return storeErr(e)) orelse return null;
            defer self.gpa.free(bytes);
            var v = InodeValue.decode(bytes) catch return error.InputOutput;
            self.pending.fold(ino, &v);
            return v;
        }

        fn readInode(self: *Self, ino: u64) Error!InodeValue {
            return (try self.readInodeOpt(ino)) orelse error.FileNotFound;
        }

        fn readDir(self: *Self, ino: u64) Error!InodeValue {
            const v = try self.readInode(ino);
            if (!economy.isDirMode(v.mode)) return error.NotDir;
            return v;
        }

        fn attrOf(self: *Self, global: u64, v: InodeValue) abi.fuse_attr {
            return codec.attrFromInode(global, v, self.blksize);
        }

        fn entryOf(self: *Self, global: u64, v: InodeValue) filesystem.Entry {
            return .{ .nodeid = global, .entry_ttl_ns = self.ttls.entryTtlFor(economy.isDirMode(v.mode)), .attr_ttl_ns = self.attrTtlFor(v), .attr = self.attrOf(global, v) };
        }

        fn attrReply(self: *Self, global: u64, v: InodeValue) filesystem.Attr {
            return .{ .attr_ttl_ns = self.attrTtlFor(v), .attr = self.attrOf(global, v) };
        }

        /// The attr TTL a record is served with: the mount's, except for a
        /// file carrying S_ISUID/S_ISGID — KILLPRIV_V2 is the daemon's
        /// (module doc), the WRITE reply carries no attrs and the kernel
        /// invalidates none on it, so a cached mode would outlive the clear
        /// (fstests generic/355: stat after a priv-killing write showed
        /// -rwSr-Sr--). 0 = every stat of such a file asks; they are rare.
        fn attrTtlFor(self: *const Self, v: InodeValue) u64 {
            if (v.mode & (S.ISUID | S.ISGID) != 0 and v.mode & S.IFMT == S.IFREG) return 0;
            return self.ttls.attr_ns;
        }

        /// The live chain of `(parent, hash54(name))`: keys into `buf`, the
        /// matching dentry (by FULL name) if any.
        fn scanChain(self: *Self, parent: u64, name: []const u8, buf: *ChainBuf, kvs: *std.ArrayList(tree.KeyValue)) Error!?Found {
            const r = tx.dentryChainRange(parent, key.dentryNameHash54(name, self.store.nameKey()));
            self.store.range(self.gpa, key.TREE_DENTRIES, &r.start, &r.end, 256, kvs) catch |e| return storeErr(e);
            var found: ?Found = null;
            buf.len = 0;
            for (kvs.items) |kv| {
                buf.keys[buf.len] = kv.key;
                buf.len += 1;
                const dv = record.DentryValue.decode(kv.value) catch return error.InputOutput;
                if (std.mem.eql(u8, dv.name, name)) {
                    var pos: economy.DentryPos = .{ .key = undefined, .child_ino = dv.child_ino, .file_type = dv.file_type };
                    @memcpy(&pos.key, kv.key[0..key.DENTRY_KEY_LEN]);
                    found = .{ .pos = pos, .local = try localOf(dv.child_ino), .global = dv.child_ino };
                }
            }
            return found;
        }

        fn findDentry(self: *Self, parent: u64, name: []const u8) Error!?Found {
            if (try self.tokenServe(parent, .{ .dentries = true })) |served| {
                defer served.release(self.io);
                const e = served.entry orelse return null;
                const rec = e.lookupName(name) orelse return null;
                return try foundFromToken(parent, rec);
            }
            var buf: ChainBuf = .{};
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            return self.scanChain(parent, name, &buf, &kvs);
        }

        /// A token's dentry record as a `Found`: the cookie IS the key's
        /// `(hash54, coll_seq)` suffix.
        fn foundFromToken(parent: u64, rec: read_divert.cache.DirRecord) Error!Found {
            const pos = key.decodeReaddirCookie(rec.cookie) catch return error.InputOutput;
            const entry = switch (pos) {
                .after_entry => |e| e,
                else => return error.InputOutput,
            };
            const dpos: economy.DentryPos = .{ .key = key.dentryKey(parent, entry.hash54, entry.coll_seq), .child_ino = rec.child_ino, .file_type = rec.file_type };
            return .{ .pos = dpos, .local = try localOf(rec.child_ino), .global = rec.child_ino };
        }

        /// Whether a directory holds any entry.
        fn dirHasEntries(self: *Self, dir: u64) Error!bool {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            const start = key.dentryKey(dir, 0, 0);
            const end = key.dentryKey(dir, key.HASH54_MAX, std.math.maxInt(u8));
            self.store.range(self.gpa, key.TREE_DENTRIES, &start, &end, 1, &kvs) catch |e| return storeErr(e);
            return kvs.items.len != 0;
        }

        /// The xattr chain of `(ino, hash56(name))`: the matching record
        /// (by full name) and the chain's occupied `coll_seq`s.
        const XattrHit = struct { k: [key.XATTR_KEY_LEN]u8, value: []u8 };

        fn findXattr(self: *Self, ino: u64, name: []const u8, occupied: *[256]u8, occupied_len: *usize) Error!?XattrHit {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            const h = key.xattrNameHash56(name, self.store.nameKey());
            const start = key.xattrKey(ino, h, 0);
            const end = key.xattrKey(ino, h, std.math.maxInt(u8));
            self.store.range(self.gpa, key.TREE_XATTRS, &start, &end, 256, &kvs) catch |e| return storeErr(e);
            occupied_len.* = 0;
            var hit: ?XattrHit = null;
            for (kvs.items) |kv| {
                occupied[occupied_len.*] = kv.key[kv.key.len - 1];
                occupied_len.* += 1;
                const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
                if (hit == null and std.mem.eql(u8, xv.name, name)) {
                    var k: [key.XATTR_KEY_LEN]u8 = undefined;
                    @memcpy(&k, kv.key[0..key.XATTR_KEY_LEN]);
                    hit = .{ .k = k, .value = try self.gpa.dupe(u8, xv.value) };
                }
            }
            return hit;
        }

        /// The xattr value by name (owned), null when absent.
        fn xattrValue(self: *Self, ino: u64, name: []const u8) Error!?[]u8 {
            if (try self.tokenServe(ino, .{})) |served| {
                defer served.release(self.io);
                const e = served.entry orelse return null;
                const v = e.xattr(name) orelse return null;
                return try self.gpa.dupe(u8, v);
            }
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            const hit = (try self.findXattr(ino, name, &occ, &n)) orelse return null;
            return hit.value;
        }

        /// The key a NEW xattr `name` occupies, or the existing record's.
        fn xattrKeyFor(self: *Self, ino: u64, name: []const u8) Error!struct { k: [key.XATTR_KEY_LEN]u8, existing: ?[]u8 } {
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            if (try self.findXattr(ino, name, &occ, &n)) |hit| return .{ .k = hit.k, .existing = hit.value };
            const coll = key.firstFreeCollSeq(occ[0..n]) orelse return error.NoSpaceLeft;
            return .{ .k = key.xattrKey(ino, key.xattrNameHash56(name, self.store.nameKey()), coll), .existing = null };
        }

        fn stageXattrPut(self: *Self, t: *tx.KvTx, k: *const [key.XATTR_KEY_LEN]u8, name: []const u8, value: []const u8) Error!void {
            const buf = try self.gpa.alloc(u8, 1 + name.len + value.len);
            defer self.gpa.free(buf);
            const enc = (record.XattrValue{ .name = name, .value = value }).encodeInto(buf) catch return error.NameTooLong;
            try t.stagePut(key.TREE_XATTRS, k, enc);
        }

        /// The op's canonical 4a set (charter §2 lock order: one multi-key
        /// acquisition per op, taken holding no other 4a guard; the commit
        /// door comes after it, at the commit).
        fn lockPlan(self: *Self, plan: *const economy.GuardPlan) Error!*HeldGuards {
            const held = try self.gpa.create(HeldGuards);
            errdefer self.gpa.destroy(held);
            held.* = .{ .gpa = self.gpa, .io = self.io, .set = undefined };
            held.set = self.locks.lockManyUncancelable(self.io, plan.inodes(), plan.dentries()) catch return error.InvalidArgument;
            return held;
        }

        /// A transaction carrying `held`: from here the guards drop with the
        /// tx (abandon or terminal outcome), never twice.
        fn beginWith(self: *Self, held: *HeldGuards, site: tx.Site) Error!*tx.KvTx {
            const t = self.store.begin(site) catch |e| {
                HeldGuards.releaseFn(held);
                return e;
            };
            t.holdGuards(held.guardSet());
            return t;
        }

        /// The exclusive `I{ino}` guard + a transaction (the one-inode ops).
        fn beginExclusive(self: *Self, local: u64, site: tx.Site) Error!*tx.KvTx {
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = local, .mode = .exclusive };
            plan.inode_len = 1;
            const held = try self.lockPlan(&plan);
            return self.beginWith(held, site);
        }

        fn commit(self: *Self, c: *Ctx, t: *tx.KvTx) Error!void {
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            // The 4a set leaves this frame with the tx (the conveyor drops it
            // at the terminal outcome, on its thread).
            lock_order.handedOff(.object);
            self.store.commit(t) catch |e| return storeErr(e);
        }

        fn parentMemoSeam(ctx: *anyopaque, dir: u64) ?u64 {
            const self: *Self = @ptrCast(@alignCast(ctx));
            return self.parentOf(dir);
        }

        fn rememberParent(self: *Self, dir: u64, parent: u64) void {
            self.parents_lock.lock();
            defer self.parents_lock.unlock();
            self.parents.put(self.gpa, dir, parent) catch {};
        }

        fn parentOf(self: *Self, dir: u64) ?u64 {
            if (dir == inos.ROOT_INO) return inos.ROOT_INO;
            self.parents_lock.lock();
            defer self.parents_lock.unlock();
            return self.parents.get(dir);
        }

        fn forgetParent(self: *Self, dir: u64) void {
            self.parents_lock.lock();
            defer self.parents_lock.unlock();
            _ = self.parents.remove(dir);
        }

        fn regularOpenFlags(self: *const Self) u32 {
            var f: u32 = abi.FOPEN_KEEP_CACHE;
            // FOPEN_NOFLUSH: fuse ≥ 7.35; FOPEN_PARALLEL_DIRECT_WRITES: ≥ 7.38.
            if (self.minor >= 35) f |= abi.FOPEN_NOFLUSH;
            if (self.minor >= 38) f |= abi.FOPEN_PARALLEL_DIRECT_WRITES;
            return f;
        }

        fn mintFh(self: *Self) u64 {
            return self.next_fh.fetchAdd(1, .monotonic);
        }

        // ---- the layout record ----

        /// The layout record of `ino` (owned; the layout borrows it), null
        /// when never written.
        pub fn readLayout(self: *Self, ino: u64) Error!?LayoutRec {
            if (try self.tokenServe(ino, .{})) |served| {
                // A foreign object: the holder's carried `layout` xattr (the
                // read divert's law — never this mount's projection, whose
                // stale or absent record would read as an inline file of
                // zeros). The record's collision seq is the holder's key;
                // a reader never re-keys a foreign layout.
                defer served.release(self.io);
                const e = served.entry orelse return null;
                const v = e.xattr(LAYOUT_XATTR) orelse return null;
                const value = try self.gpa.dupe(u8, v);
                errdefer self.gpa.free(value);
                const l = Layout.decode(value, self.geo) catch {
                    layout_record_corrupt.fire("a carried layout record failed the total codec");
                    return error.InputOutput;
                };
                return .{ .value = value, .coll_seq = 0, .layout = l };
            }
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            const hit = (try self.findXattr(ino, LAYOUT_XATTR, &occ, &n)) orelse return null;
            errdefer self.gpa.free(hit.value);
            const l = Layout.decode(hit.value, self.geo) catch {
                layout_record_corrupt.fire("a stored layout record failed the total codec");
                return error.InputOutput;
            };
            return .{ .value = hit.value, .coll_seq = hit.k[hit.k.len - 1], .layout = l };
        }

        fn freeLayout(self: *Self, r: ?LayoutRec) void {
            if (r) |rec| self.gpa.free(rec.value);
        }

        fn kindOf(r: ?LayoutRec) layout.Kind {
            return if (r) |rec| rec.layout.kind else .inline_;
        }

        fn stagingKeyOf(ino: u64, l: Layout) staging.StagingKey {
            return staging.StagingKey.init(l.file_id, ino);
        }

        /// Stage `new` for `ino` (the record Put + the `+ref`/`−ref` diff
        /// in `t`); a refused shape is a caller bug (every layout built
        /// here is admissible) and answers EIO.
        fn publish(self: *Self, t: *tx.KvTx, ino: u64, prev: ?LayoutRec, new: Layout, proofs: []const layout.DmaProof) Error!void {
            layout.publishLayout(t, .{ .ino = ino, .name_key = self.store.nameKey(), .coll_seq = if (prev) |p| p.coll_seq else 0, .prev = if (prev) |p| p.layout else null, .layout = new, .proofs = proofs }, self.data.refsSeam(), self.geo) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.InputOutput,
            };
        }

        /// The resident payload of a non-striped layout as ONE image (the
        /// re-stage / promotion source): inline bytes, the ring entry, the
        /// packed tenant window. `out.len ≥ size` (and ≥ the tenant's
        /// grain window). Bytes past the image are zero.
        fn residentImage(self: *Self, ino: u64, r: ?LayoutRec, out: []u8) Error!usize {
            @memset(out, 0);
            const rec = r orelse return 0;
            switch (rec.layout.kind) {
                .inline_ => {
                    const n = @min(out.len, rec.layout.data.len);
                    @memcpy(out[0..n], rec.layout.data[0..n]);
                    return n;
                },
                .staged => {
                    if (self.data.stagedLease(stagingKeyOf(ino, rec.layout))) |lease| {
                        defer lease.release();
                        const b = lease.bytes();
                        const n = @min(out.len, b.len);
                        @memcpy(out[0..n], b[0..n]);
                        return n;
                    }
                    if (rec.layout.durableCopy()) |m| return (try self.data.readMappingImage(m, out)).len;
                    return 0;
                },
                .packed_ => {
                    const m = rec.layout.durableCopy() orelse return error.InputOutput;
                    return (try self.data.readMappingImage(m, out)).len;
                },
                .striped => return error.InvalidArgument,
            }
        }

        /// The displaced durable copy/tenant of a non-striped layout and
        /// its ring entry (what a successful publish away from it frees).
        fn noteDisplaced(ino: u64, r: ?LayoutRec, after: *AfterCommit) void {
            const rec = r orelse return;
            switch (rec.layout.kind) {
                .staged => {
                    after.staged = .{ .key = stagingKeyOf(ino, rec.layout), .generation = null };
                    if (rec.layout.durableCopy()) |m| after.push(0, m);
                },
                .packed_ => if (rec.layout.durableCopy()) |m| after.push(0, m),
                .inline_, .striped => {},
            }
        }

        /// After the transaction landed: the ring release, the frees, the
        /// tree-mapped sweep's drain.
        fn afterCommit(self: *Self, after: *const AfterCommit) void {
            if (after.staged) |s| self.data.releaseStaged(s.key, s.generation);
            if (after.kvmap_sweep) |sw| {
                fuse_kvmap_sweeps_kicked.inc();
                self.data.drainKvmapSweep(sw.ino, sw.removed);
            }
            if (after.n == 0) return;
            var maps: [MAX_BINDINGS_PER_INO]Mapping = undefined;
            for (after.slice(), 0..) |e, i| maps[i] = e.mapping;
            self.data.freeBindings(self.io, maps[0..after.n]);
        }

        /// `min(i64::MAX, block × u32::MAX)`: the striped file ceiling
        /// (`fuse_write_u32_index_refused`'s doc).
        fn maxFileSize(self: *const Self) u64 {
            return @min(@as(u64, std.math.maxInt(i64)), self.block_size * @as(u64, std.math.maxInt(u32)));
        }

        /// KILLPRIV_V2 fold for a tx that is rewriting `v`: the mode strip
        /// and the caps drop, counted once when anything moved.
        fn killpriv(self: *Self, t: *tx.KvTx, ino: u64, v: *InodeValue) Error!void {
            const stripped = killSuidgidMode(v.mode);
            var moved = stripped != v.mode;
            v.mode = stripped;
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            if (try self.findXattr(ino, CAPS_XATTR, &occ, &n)) |hit| {
                self.gpa.free(hit.value);
                try t.stageDelete(key.TREE_XATTRS, &hit.k);
                moved = true;
            }
            if (moved) fuse_killpriv_clears.inc();
        }

        /// The write-side custody of a regular file lands or drops before a
        /// layout is clipped (truncate, O_TRUNC, destroy of a striped corpse).
        fn settleCustody(self: *Self, ino: u64, kind: layout.Kind) void {
            if (kind == .striped and self.data.hasVolumes()) self.data.flushCustody(self.io, ino);
        }

        // ---- truncate ----

        /// Truncate `ino` to `size` inside `t`: the layout per kind (module
        /// doc); the inode `Put` the caller stages; `after` collects what
        /// the commit frees.
        fn stageTruncate(self: *Self, c: *Ctx, t: *tx.KvTx, ino: u64, stored: InodeValue, size: u64, after: *AfterCommit) Error!void {
            _ = c;
            const old_size = stored.size;
            if (!self.data.hasVolumes() and size > self.inline_max) {
                fuse_write_no_data_volume_refused.inc();
                return error.FileTooBig;
            }
            if (size > self.maxFileSize()) {
                fuse_write_u32_index_refused.inc();
                return error.FileTooBig;
            }
            const rec = try self.readLayout(ino);
            defer self.freeLayout(rec);
            switch (kindOf(rec)) {
                .inline_ => {
                    const r = rec orelse return;
                    if (size >= r.layout.data.len) return;
                    try self.publish(t, ino, rec, Layout.inlineData(size, r.layout.data[0..@intCast(size)]), &.{});
                },
                .staged => {
                    const r = rec.?;
                    if (size == 0) {
                        try self.publish(t, ino, rec, Layout.inlineData(0, &.{}), &.{});
                        noteDisplaced(ino, rec, after);
                        return;
                    }
                    if (size >= old_size) return;
                    // Re-stage the clipped image under the same identity.
                    const buf = try self.gpa.alloc(u8, @intCast(layout.ceilGrain(old_size)));
                    defer self.gpa.free(buf);
                    const got = try self.residentImage(ino, rec, buf);
                    const clipped = buf[0..@intCast(@min(size, got))];
                    const k = stagingKeyOf(ino, r.layout);
                    _ = try self.data.stageImage(k, clipped, .none);
                    try self.publish(t, ino, rec, Layout.staged(size, r.layout.file_id, null), &.{});
                    if (r.layout.durableCopy()) |m| after.push(0, m);
                    fuse_truncate_restages.inc();
                },
                .packed_ => {
                    const r = rec.?;
                    const m = r.layout.durableCopy() orelse return error.InputOutput;
                    if (size == 0) {
                        try self.publish(t, ino, rec, Layout.inlineData(0, &.{}), &.{});
                        after.push(0, m);
                        return;
                    }
                    if (size >= m.len) return;
                    if (self.data.isPassthrough()) {
                        // The passthrough shrink: a pure clip (same reference).
                        try self.publish(t, ino, rec, Layout.packedTenant(size, r.layout.file_id, pack.clipTenant(m, @intCast(size))), &.{});
                        fuse_truncate_packed_clips.inc();
                        return;
                    }
                    // Transformed: a new image — re-staged, promoted later.
                    const buf = try self.gpa.alloc(u8, @intCast(layout.ceilGrain(m.len)));
                    defer self.gpa.free(buf);
                    const got = try self.residentImage(ino, rec, buf);
                    const k = stagingKeyOf(ino, r.layout);
                    _ = try self.data.stageImage(k, buf[0..@intCast(@min(size, got))], .none);
                    try self.publish(t, ino, rec, Layout.staged(size, r.layout.file_id, null), &.{});
                    after.push(0, m);
                    fuse_truncate_restages.inc();
                },
                .striped => {
                    const r = rec.?;
                    if (size >= old_size) return;
                    if (r.layout.isKvmap()) {
                        // SIZE-FLIP-FIRST (charter §5.6): one head Put
                        // {size, cursor}; the records past the cut are
                        // residue the sweep drains after this commit —
                        // never materialized here, whatever the size.
                        _ = try self.data.stageKvmapShrink(t, ino, r.layout, r.coll_seq, size);
                        after.kvmap_sweep = .{ .ino = ino, .removed = (old_size - size + self.block_size - 1) / self.block_size };
                        fuse_truncate_striped_clips.inc();
                        return;
                    }
                    // Blocks at or past the new end leave the map (their
                    // `−ref` rides this tx); the kept partial block is
                    // zero-completed AFTER the commit through the write
                    // path, so a later grow or gap write reads zeros.
                    const bs = self.block_size;
                    const keep_blocks: u64 = (size + bs - 1) / bs;
                    var kept: std.ArrayList(layout.MapEntryWire) = .empty;
                    defer kept.deinit(self.gpa);
                    for (r.layout.entries) |e| {
                        if (e.block() < keep_blocks) {
                            try kept.append(self.gpa, e);
                        } else {
                            after.push(e.block(), Mapping.decode(e.mapping, self.geo.chunk) catch return error.InputOutput);
                        }
                    }
                    try self.publish(t, ino, rec, Layout.stripedInline(size, kept.items), &.{});
                    fuse_truncate_striped_clips.inc();
                },
            }
        }

        /// The striped shrink's tail: zeros over `[size, min(block_end,
        /// old_size))` of the kept partial block through the write path
        /// (one sub-block write; it seeds from the mapped block and lands
        /// with the file's custody).
        fn zeroTailAfterShrink(self: *Self, ino: u64, size: u64, old_size: u64) void {
            const bs = self.block_size;
            if (size == 0 or size % bs == 0 or size >= old_size) return;
            const block_end = (size / bs + 1) * bs;
            const end = @min(block_end, old_size);
            const zeros = self.gpa.alloc(u8, @intCast(end - size)) catch return;
            defer self.gpa.free(zeros);
            @memset(zeros, 0);
            _ = self.data.write(self.io, ino, size, .{ .bytes = zeros }, .entire_op, .{ .kind = .striped, .size = size, .map_resident = true }, null) catch |e| {
                log.err("kv_fs: ino {d}: the truncate's tail zero-complete failed ({s}) — bytes past the new end may read back after a grow", .{ ino, @errorName(e) });
                return;
            };
            fuse_truncate_zero_tail_bytes.add(zeros.len);
        }

        // ---- the ops ----

        pub fn init(self: *Self, _: *Ctx, a: filesystem.InitArgs) Error!void {
            self.minor = a.minor;
            self.killpriv_v2 = a.has(abi.FUSE_HANDLE_KILLPRIV_V2);
            // The directory-parent memo feeds the handover's subtree credit.
            if (@hasDecl(Store, "bindParentMemo")) self.store.bindParentMemo(.{ .ctx = self, .parentOf = parentMemoSeam });
            // The handover's step 0 drains the departing slot's parked stamps.
            if (@hasDecl(Store, "bindTimesDrain")) self.store.bindTimesDrain(.{ .ctx = self, .drainSlot = timesDrainSeam });
            fuse_killpriv_negotiated.set(@intFromBool(self.killpriv_v2));
            log.info("kv_fs: FUSE 7.{d}; KILLPRIV_V2 {s}; inline_max {d} B; block {d} B; data volumes {s}; staging {s}", .{ self.minor, if (self.killpriv_v2) "negotiated" else "absent", self.inline_max, self.block_size, if (self.data.hasVolumes()) "yes" else "NONE (inline only)", if (self.data.stagingOn()) "on" else "off" });
        }

        pub fn destroy(self: *Self, c: *Ctx, _: filesystem.DestroyArgs) void {
            // The dismount sweep: every gathered corpse destroyed, then
            // every parked stamp journaled (the dismount's durability point).
            self.reclaimDrain(c) catch |e| log.err("kv_fs: dismount reclaim drain failed: {t}", .{e});
            _ = self.drainPendingTimes(c) catch |e| log.err("kv_fs: dismount times drain failed: {t} ({d} refinement(s) stay parked)", .{ e, self.pending.len() });
        }

        pub fn lookup(self: *Self, c: *Ctx, a: filesystem.LookupArgs) Error!filesystem.Entry {
            try checkName(a.name);
            if (a.parent == inos.ROOT_INO) if (virtual_inodes.classOfName(a.name)) |class| {
                if (self.virtual != null) return self.lookupVirtual(c, class);
            };
            if (std.mem.eql(u8, a.name, ".")) {
                const v = try self.readDir(try localOf(a.parent));
                return self.entryOf(a.parent, v);
            }
            if (std.mem.eql(u8, a.name, "..")) {
                const p = self.parentOf(a.parent) orelse return error.FileNotFound;
                const v = try self.readDir(try localOf(p));
                return self.entryOf(p, v);
            }
            c.stage(.meta_backend_start);
            const found = (try self.findDentry(try localOf(a.parent), a.name)) orelse {
                c.stage(.meta_backend_done);
                if (!self.ttls.negativeEnabled()) return error.FileNotFound;
                return filesystem.Entry.negative(self.ttls.negative_ns);
            };
            const v = (try self.readInodeOpt(found.local)) orelse {
                dentry_without_inode.fire("lookup found a dentry whose inode record is absent");
                return error.InputOutput;
            };
            c.stage(.meta_backend_done);
            if (economy.isDirMode(v.mode)) self.rememberParent(found.global, a.parent);
            return self.entryOf(found.global, v);
        }

        pub fn forget(self: *Self, c: *Ctx, a: filesystem.ForgetArgs) void {
            if (!a.evicted) return;
            if (virtual_inodes.isVirtualIno(a.ino)) return self.virtuals.forget(a.ino);
            self.forgetParent(a.ino);
            const local = localOf(a.ino) catch return;
            self.data.forgetShape(local);
            // A reader destroys nothing: a corpse is its holder's.
            if (@hasDecl(Store, "isReader") and self.store.isReader()) return;
            // A FOREIGN ino's corpse is its holder's to destroy (at the
            // holder's own forget, under its door): a reader's forget
            // drops its cached view alone — never a destroy on a slot
            // another appender leases (`SlotMoved`).
            if (self.divert) |d| if (d.isForeign(local)) return;
            const v = (self.readInodeOpt(local) catch return) orelse return;
            if (v.nlink != 0) return;
            self.gatherCorpse(c, local);
        }

        /// A forgotten corpse joins the batch; a full batch drains inline
        /// (one entry per `RECLAIM_BATCH`); a gather that cannot grow drives
        /// the corpse alone (never lost).
        fn gatherCorpse(self: *Self, c: *Ctx, ino: u64) void {
            var drain = false;
            {
                self.reclaim_lock.lock();
                defer self.reclaim_lock.unlock();
                self.reclaim.append(self.gpa, ino) catch {
                    fuse_reclaim_queue_overflow.inc();
                    self.destroyCorpses(c, &.{ino}) catch |e| log.err("kv_fs: inline corpse destroy of ino {d} failed: {t}", .{ ino, e });
                    return;
                };
                drain = self.reclaim.items.len >= RECLAIM_BATCH;
            }
            if (drain) self.reclaimDrain(c) catch |e| log.err("kv_fs: reclaim drain failed: {t}", .{e});
        }

        /// Destroy every gathered corpse, `RECLAIM_BATCH` per entry.
        pub fn reclaimDrain(self: *Self, c: *Ctx) Error!void {
            while (true) {
                var batch: [RECLAIM_BATCH]u64 = undefined;
                var n: usize = 0;
                {
                    self.reclaim_lock.lock();
                    defer self.reclaim_lock.unlock();
                    while (n < RECLAIM_BATCH and self.reclaim.items.len > 0) : (n += 1) batch[n] = self.reclaim.pop().?;
                }
                if (n == 0) return;
                try self.destroyCorpses(c, batch[0..n]);
            }
        }

        /// THE CORPSE SWEEP AT MOUNT: an inode record at `nlink == 0` after
        /// a crash is a corpse whose final FORGET (or the gather it joined)
        /// died with the previous daemon — reachable by no name, unknown to
        /// the kernel (no connection exists yet), its blocks referenced on
        /// the ledger forever. One pass over the native inode tree destroys
        /// every such corpse with its `−ref` riders, `RECLAIM_BATCH` per
        /// entry, before the mount serves. Returns the corpses destroyed.
        /// (fsck C9 is the census at scale; this is the mount's belt.)
        pub fn sweepCorpsesAtMount(self: *Self) Error!u64 {
            const gpa = self.gpa;
            var trace = op_trace.OpTrace.begin(0, .other);
            defer trace.finish();
            var ctx: Ctx = .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = self.io, .trace = &trace, .wd_slot = null, .window = &.{} };
            // The whole ino-major space: the store merges every slot tree
            // of every member in key order (native locals first, then the
            // guest keyspaces slot by slot).
            var cursor = key.KeyBuf.fromSlice(&key.inodeKey(0));
            const end = key.inodeKey(std.math.maxInt(u64));
            var batch: [RECLAIM_BATCH]u64 = undefined;
            var n: usize = 0;
            var destroyed: u64 = 0;
            while (true) {
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(gpa, &kvs);
                self.store.range(gpa, key.TREE_INODES, cursor.slice(), &end, SWEEP_PAGE, &kvs) catch |e| return storeErr(e);
                if (kvs.items.len == 0) break;
                // The slot tree interleaves every ino-major kind (`ino ‖
                // kind ‖ …`): only the inode records are read, and the
                // resume cursor is the NEXT ino's inode key.
                for (kvs.items) |kv| {
                    if (kv.key.len != key.INODE_KEY_LEN or kv.key[key.INO_MAJOR_KIND_OFF] != key.TREE_INODES) continue;
                    const v = InodeValue.decode(kv.value) catch continue;
                    if (v.nlink != 0) continue;
                    batch[n] = std.mem.readInt(u64, kv.key[0..8], .big);
                    n += 1;
                    if (n == RECLAIM_BATCH) {
                        try self.destroyCorpses(&ctx, batch[0..n]);
                        destroyed += n;
                        n = 0;
                    }
                }
                if (kvs.items.len < SWEEP_PAGE) break;
                const last_ino = std.mem.readInt(u64, kvs.items[kvs.items.len - 1].key[0..8], .big);
                if (last_ino == std.math.maxInt(u64)) break;
                cursor = key.KeyBuf.fromSlice(&key.inodeKey(last_ino + 1));
            }
            if (n > 0) {
                try self.destroyCorpses(&ctx, batch[0..n]);
                destroyed += n;
            }
            fuse_corpse_sweep_destroyed.add(destroyed);
            if (destroyed != 0) log.info("corpse sweep at mount: {d} corpse(s) left by the previous daemon destroyed, their references released", .{destroyed});
            return destroyed;
        }

        /// One corpse's destroy plan: its xattr keys, its layout's named
        /// bindings (the `−ref` riders) and the framed bytes they cost.
        const Corpse = struct {
            ino: u64,
            kvs: std.ArrayList(tree.KeyValue) = .empty,
            after: AfterCommit = .{},
            bytes: u64 = 0,
        };

        /// The destroy rider (charter §5.6): every corpse's inode + xattr
        /// Deletes and its reference releases in ONE entry, members packed
        /// greedily under the whole-entry cap; the blocks go to the
        /// reclaimer AFTER the commit (RES-1).
        ///
        /// The order is the contract. (1) A striped corpse's write-side
        /// custody SETTLES first, with no lock held: the flush's own merge
        /// republishes the layout onto the blocks it uploaded and frees the
        /// ones it displaced. (2) THEN every corpse's (3.7) layout lock,
        /// held across the plan read AND the destroy commit, so the plan
        /// names exactly the bindings the record carries at the commit
        /// (the plane's promotion of a staged corpse takes the same lock).
        /// A plan read BEFORE the settle named the displaced block — freed
        /// twice — and never the fresh one: its `+ref` stayed durable
        /// forever (fstests generic/013: fsstress write+truncate+unlink,
        /// fsck C2/C8 "durable 1 vs derived 0", block_untracked_free_
        /// refusals). The striped mutexes are not reentrant and two batches
        /// may share a stripe: the locks are taken in ONE order (ascending
        /// stripe, each once) — the multi-lock holder's order rule.
        fn destroyCorpses(self: *Self, c: *Ctx, corpses: []const u64) Error!void {
            const gpa = self.gpa;
            assert(corpses.len <= RECLAIM_BATCH);
            // (1) The nlink re-check (a forget of a live inode never reached
            // the gather) and the settle.
            var live: [RECLAIM_BATCH]u64 = undefined;
            var n_live: usize = 0;
            for (corpses) |ino| {
                const v = (try self.readInodeOpt(ino)) orelse continue;
                if (v.nlink != 0) continue;
                const rec = try self.readLayout(ino);
                const kind = kindOf(rec);
                self.freeLayout(rec);
                self.settleCustody(ino, kind);
                live[n_live] = ino;
                n_live += 1;
            }
            if (n_live == 0) return;
            // (2) The locks, ascending stripe, each stripe once.
            std.mem.sort(u64, live[0..n_live], {}, struct {
                fn lessThan(_: void, a: u64, b: u64) bool {
                    return layout.LayoutLocks.stripeOf(a) < layout.LayoutLocks.stripeOf(b);
                }
            }.lessThan);
            var guards: [RECLAIM_BATCH]Data.LayoutGuard = undefined;
            var n_guards: usize = 0;
            defer for (guards[0..n_guards]) |g| g.release(self.io);
            for (live[0..n_live], 0..) |ino, i| {
                if (i > 0 and layout.LayoutLocks.stripeOf(live[i - 1]) == layout.LayoutLocks.stripeOf(ino)) continue;
                guards[n_guards] = self.data.lockLayout(self.io, ino);
                n_guards += 1;
            }
            // (3) The plans, from the records as they stand under the locks.
            var plans: std.ArrayList(Corpse) = .empty;
            defer {
                for (plans.items) |*p| tree.deinitKeyValues(gpa, &p.kvs);
                plans.deinit(gpa);
            }
            var payloads: std.ArrayList(u64) = .empty;
            defer payloads.deinit(gpa);
            var sweeps: std.ArrayList(struct { ino: u64, removed: u64 }) = .empty;
            defer sweeps.deinit(gpa);
            for (live[0..n_live]) |ino| {
                const v = (try self.readInodeOpt(ino)) orelse continue;
                var p: Corpse = .{ .ino = ino };
                {
                    errdefer tree.deinitKeyValues(gpa, &p.kvs);
                    const start = key.xattrKey(ino, 0, 0);
                    const end = key.xattrKey(ino, key.HASH56_MAX, std.math.maxInt(u8));
                    self.store.range(gpa, key.TREE_XATTRS, &start, &end, 1024, &p.kvs) catch |e| return storeErr(e);
                    var tree_mapped = false;
                    for (p.kvs.items) |kv| {
                        const xv = record.XattrValue.decode(kv.value) catch continue;
                        if (!std.mem.eql(u8, xv.name, LAYOUT_XATTR)) continue;
                        const l = Layout.decode(xv.value, self.geo) catch {
                            layout_record_corrupt.fire("a corpse's layout record failed the total codec");
                            continue;
                        };
                        if (l.isKvmap()) {
                            // A tree-mapped corpse: its own tx marks it
                            // (head {size 0, sweep 0}); the sweep's terminal
                            // chunk destroys the inode and its xattrs.
                            try self.markKvmapCorpse(c, ino, l);
                            try sweeps.append(gpa, .{ .ino = ino, .removed = (v.size + self.block_size - 1) / self.block_size });
                            tree_mapped = true;
                            break;
                        }
                        if (l.kind == .staged) p.after.staged = .{ .key = stagingKeyOf(ino, l), .generation = null };
                        var it = l.named(self.geo.chunk);
                        while (it.next()) |e| p.after.push(e.block, e.mapping);
                    }
                    if (tree_mapped) {
                        tree.deinitKeyValues(gpa, &p.kvs);
                        continue;
                    }
                    p.bytes = inos.destroyEntryBytes(p.kvs.items.len) + @as(u64, p.after.n) * block_refs.REF_RECORD_JOURNAL_BYTES;
                    try payloads.ensureUnusedCapacity(gpa, 1);
                    try plans.append(gpa, p);
                }
                payloads.appendAssumeCapacity(p.bytes);
            }
            // (4) The commits, under the locks.
            if (plans.items.len != 0) {
                const ranges = try inos.packDestroyEntries(gpa, payloads.items);
                defer gpa.free(ranges);
                for (ranges) |rg| {
                    // Every corpse's part is complete on its own member: a
                    // range is cut where the member changes (one entry per run).
                    var start = rg.start;
                    while (start < rg.end) {
                        var end = start + 1;
                        while (end < rg.end and !self.spansHomes(&.{ globalOf(plans.items[start].ino), globalOf(plans.items[end].ino) })) end += 1;
                        try self.destroyRun(c, plans.items[start..end]);
                        start = end;
                    }
                }
            }
            // (5) The locks drop; the frees, the ring releases and the
            // sweep drains run after them (RES-1; the sweep takes (3.7)).
            for (guards[0..n_guards]) |g| g.release(self.io);
            n_guards = 0;
            for (plans.items) |*p| {
                self.afterCommit(&p.after);
                fuse_reclaim_blocks_freed.add(@intCast(p.after.n));
            }
            for (sweeps.items) |sw| {
                var after: AfterCommit = .{};
                after.kvmap_sweep = .{ .ino = sw.ino, .removed = sw.removed };
                self.afterCommit(&after);
            }
        }

        /// ONE tx per run (under the caller's (3.7) locks); the frees are
        /// the caller's, after its locks drop.
        fn destroyRun(self: *Self, c: *Ctx, run: []Corpse) Error!void {
            const t = try self.store.begin(@src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            var destroyed: u64 = 0;
            for (run) |*p| {
                var xkeys: [1024][]const u8 = undefined;
                for (p.kvs.items, 0..) |kv, i| xkeys[i] = kv.key;
                try inos.stageDestroy(t, p.ino, xkeys[0..p.kvs.items.len]);
                for (p.after.slice()) |e| {
                    try block_refs.stageOps(t, &.{block_refs.released(.{ .vol_tag = e.mapping.vol_tag, .block_idx = e.mapping.blockIdx(self.geo.chunk), .owner_ino = p.ino, .block_index = e.block })});
                }
                _ = self.pending.retire(p.ino);
                destroyed += 1;
            }
            t_owned = false;
            try self.commit(c, t);
            fuse_reclaim_destroys.add(destroyed);
            self.destroys.noteCommitted(destroyed);
            fuse_reclaim_drains.inc();
        }

        /// The corpse mark of an unlinked tree-mapped file (block_map_tree.md
        /// §4.3 "unlink corpse"): ONE tx with the head Put {size 0, sweep 0},
        /// judged off the durable head alone; the inode record stays (nlink
        /// 0, unreachable) until the sweep destroys it. Idempotent: a mount
        /// sweep that meets an already-marked corpse only re-kicks the drain
        /// (the caller's, after its locks drop). Under the CALLER's (3.7):
        /// the head is re-read under it so the Put's `prev` is the record
        /// the commit lands on (a sweep chunk or a crossing may have moved
        /// it since the gather).
        fn markKvmapCorpse(self: *Self, c: *Ctx, ino: u64, l: Layout) Error!void {
            const head = l.kvmapHead().?;
            if (!(head.sweep == 0 and l.size == 0)) {
                const fresh = try self.readLayout(ino);
                defer self.freeLayout(fresh);
                if (fresh) |cur| if (cur.layout.isKvmap() and !(cur.layout.kvmapHead().?.sweep == 0 and cur.layout.size == 0)) {
                    const t = try self.store.begin(@src());
                    var t_owned = true;
                    errdefer if (t_owned) self.store.abandon(t);
                    try self.data.stageKvmapCorpse(t, ino, cur.layout, cur.coll_seq);
                    t_owned = false;
                    try self.commit(c, t);
                };
            }
            _ = self.pending.retire(ino);
            fuse_reclaim_destroys.inc();
            self.destroys.noteCommitted(1);
        }

        pub fn getattr(self: *Self, c: *Ctx, a: filesystem.GetattrArgs) Error!filesystem.Attr {
            if (virtual_inodes.isVirtualIno(a.ino)) return self.virtualGetattr(a.ino);
            c.stage(.meta_backend_start);
            const v = self.readInode(try localOf(a.ino)) catch |e| blk: {
                // The root under an unreachable holder (a parked joiner
                // whose manager died): the kernel revalidates the root
                // before every virtual lookup, so `.stats` stays reachable
                // off the last root entry this mount held — the same
                // exemption the `-o ro` fail-stop gate grants `getattr(1)`.
                if (a.ino == inos.ROOT_INO and errors.classOf(e) == .retryable) if (self.divert) |d| if (d.lastRoot()) |last| break :blk last;
                return e;
            };
            c.stage(.meta_backend_done);
            return self.attrReply(a.ino, v);
        }

        pub fn setattr(self: *Self, c: *Ctx, a: filesystem.SetattrArgs) Error!filesystem.Attr {
            // Every mutation of a virtual inode refuses (EACCES, the
            // reference's class for a setattr on a control file).
            if (virtual_inodes.isVirtualIno(a.ino)) return error.AccessDenied;
            const local = try localOf(a.ino);
            if (self.foreignSlot(a.ino)) {
                // A foreign-slot file: ONE shipped verb to its holder
                // (charter §5.5 "Record-level mutations of a foreign-slot
                // file"); the reply is the holder's folded record.
                const req = a.attr.toEconomy();
                const rep = try self.shipRecord(a.ino, .{ .setattr = .{ .mode = req.mode, .uid = req.uid, .gid = req.gid, .size = req.size, .atime = req.atime, .mtime = req.mtime, .ctime = req.ctime } });
                return self.attrReply(a.ino, rep.inode orelse return error.FileNotFound);
            }
            var after: AfterCommit = .{};
            var old_size: u64 = 0;
            var kind: layout.Kind = .inline_;
            var layout_lock: ?Data.LayoutGuard = null;
            errdefer if (layout_lock) |g| g.release(self.io);
            if (a.attr.size) |_| {
                const probe = (try self.readInodeOpt(local)) orelse return error.FileNotFound;
                if (!(probe.mode & S.IFMT == S.IFREG)) return error.InvalidArgument;
                const rec = try self.readLayout(local);
                kind = kindOf(rec);
                self.freeLayout(rec);
                // Under the lifecycle's exclusive guard: the custody lands
                // before the clip (the layout read below is then the truth).
                self.settleCustody(local, kind);
                // (3.7) before the 4a set, held across the commit (module doc).
                layout_lock = self.data.lockLayout(self.io, local);
            }
            const t = try self.beginExclusive(local, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            const stored = try self.readInode(local);
            if (a.attr.size) |sz| {
                old_size = stored.size;
                try self.stageTruncate(c, t, local, stored, sz, &after);
            }
            var req = a.attr.toEconomy();
            // chmod never changes the type: the S_IFMT bits are the record's.
            if (req.mode) |m| req.mode = (stored.mode & S.IFMT) | (m & ~@as(u32, S.IFMT));
            var out = try economy.stageSetattr(t, &self.pending, local, stored, req, now());
            if (out.verdict == .absorbed) {
                t_owned = false;
                self.store.abandon(t);
                if (layout_lock) |g| g.release(self.io);
                layout_lock = null;
                return self.attrReply(a.ino, out.value);
            }
            if (a.attr.kill_suidgid) {
                var v = out.value;
                try self.killpriv(t, local, &v);
                if (v.mode != out.value.mode) {
                    out.value = v;
                    try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &v.encode());
                }
            }
            t_owned = false;
            try self.commit(c, t);
            if (layout_lock) |g| g.release(self.io);
            layout_lock = null;
            _ = self.pending.retire(local);
            if (a.attr.size) |sz| {
                self.afterCommit(&after);
                if (kind == .striped and sz < old_size) self.zeroTailAfterShrink(local, sz, old_size);
                self.data.noteShape(local, sz, if (kind == .striped) .striped else .inline_);
            }
            return self.attrReply(a.ino, out.value);
        }

        pub fn readlink(self: *Self, c: *Ctx, a: filesystem.ReadlinkArgs) Error!filesystem.Data {
            const local = try localOf(a.ino);
            const v = try self.readInode(local);
            if (v.mode & S.IFMT != S.IFLNK) return error.InvalidArgument;
            const target = (try self.xattrValue(local, economy.SYMLINK_XATTR)) orelse return error.InputOutput;
            defer self.gpa.free(target);
            if (target.len > c.window.len) return error.NameTooLong;
            @memcpy(c.window[0..target.len], target);
            return .{ .len = target.len };
        }

        /// create/mkdir/mknod/symlink share the parent-side protocol: the
        /// parent guard as the child's type demands + the D-guard, the
        /// parent read and the EEXIST probe under them, ONE entry.
        const NewChild = struct { mode: u32, rdev: u32 = 0, target: ?[]const u8 = null };

        fn createChild(self: *Self, c: *Ctx, parent_global: u64, name: []const u8, child: NewChild) Error!filesystem.Entry {
            try checkMutableName(parent_global, name);
            const is_dir = economy.isDirMode(child.mode);
            const parent = try localOf(parent_global);
            // The mint first: the child's home decides the arm (a foreign
            // parent or a rotor on another member ⇒ ONE intent).
            const local = self.store.allocateIno(parent, false, try self.gatherOptIn(parent)) catch |e| return storeErr(e);
            const global = globalOf(local);
            if (self.spansHomes(&.{ parent_global, global })) return self.xvCreate(c, parent_global, name, child, local, global);
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = parent, .mode = if (is_dir) .exclusive else .shared };
            plan.inode_len = 1;
            plan.dentry_reqs[0] = .{ .parent = parent, .name = name, .mode = .exclusive };
            plan.dentry_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            const pv = try self.readDir(parent);
            var chain: ChainBuf = .{};
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            if (try self.scanChain(parent, name, &chain, &kvs) != null) return error.PathAlreadyExists;
            const stamp = now();
            const out = if (child.target) |target|
                economy.stageSymlink(t, .{ .name_key = self.store.nameKey(), .now = stamp, .parent = parent, .parent_value = pv, .name = name, .uid = c.uid, .gid = c.gid, .local_ino = local, .global_ino = global, .target = target, .chain = chain.slice() }) catch |e| return switch (e) {
                    error.SymlinkTargetTooLong, error.NameTooLong => error.NameTooLong,
                    error.DentryChainOverflow => error.NoSpaceLeft,
                    error.OutOfMemory => error.OutOfMemory,
                }
            else
                economy.stageCreate(t, .{ .name_key = self.store.nameKey(), .now = stamp, .parent = parent, .parent_value = pv, .name = name, .mode = child.mode, .uid = c.uid, .gid = c.gid, .rdev = child.rdev, .local_ino = local, .global_ino = global, .chain = chain.slice() }) catch |e| return switch (e) {
                    error.NameTooLong => error.NameTooLong,
                    error.DentryChainOverflow => error.NoSpaceLeft,
                    error.OutOfMemory => error.OutOfMemory,
                };
            t_owned = false;
            self.commit(c, t) catch |e| {
                if (self.redispatches(e)) return self.xvCreate(c, parent_global, name, child, local, global);
                return e;
            };
            if (is_dir) self.rememberParent(global, parent_global);
            if (child.mode & S.IFMT == S.IFREG) self.data.noteShape(local, 0, .inline_);
            return self.entryOf(global, out.child);
        }

        /// The cross-owner create: the EEXIST probe through the divert, the
        /// plan `[CreateInode @ the creator's rotor, InsertDentry @ the
        /// parent's holder]` as ONE intent. A symlink's target rides its
        /// own entry on the child's slot BEFORE the intent (a crash between
        /// leaves an xattr no inode owns: fsck's orphan-xattr class, never a
        /// symlink without its target).
        fn xvCreate(self: *Self, c: *Ctx, parent_global: u64, name: []const u8, child: NewChild, local: u64, global: u64) Error!filesystem.Entry {
            const parent = try localOf(parent_global);
            const pv = try self.readDir(parent);
            if (try self.findDentry(parent, name) != null) return error.PathAlreadyExists;
            var mode = child.mode;
            var gid = c.gid;
            if (pv.mode & S.ISGID != 0) {
                gid = pv.gid;
                if (economy.isDirMode(mode)) mode |= S.ISGID;
            }
            var size: u64 = 0;
            if (child.target) |target| {
                if (target.len > economy.SYMLINK_TARGET_MAX) return error.NameTooLong;
                size = target.len;
                const t = try self.store.begin(@src());
                var t_owned = true;
                errdefer if (t_owned) self.store.abandon(t);
                try self.stageXattrPut(t, &key.xattrKey(local, key.xattrNameHash56(economy.SYMLINK_XATTR, self.store.nameKey()), 0), economy.SYMLINK_XATTR, target);
                t.token_quiet = true;
                t_owned = false;
                try self.commit(c, t);
            }
            const plan = sym_xv.steps.planCreate(.{ .parent = parent_global, .name = name, .child = global, .mode = mode, .uid = c.uid, .gid = gid, .rdev = child.rdev, .size = size, .now = now() });
            const done = try self.runPlan(c, &plan);
            if (economy.isDirMode(mode)) self.rememberParent(global, parent_global);
            if (mode & S.IFMT == S.IFREG) self.data.noteShape(local, 0, .inline_);
            return self.entryOf(global, done.inode(0) orelse return error.InputOutput);
        }

        pub fn symlink(self: *Self, c: *Ctx, a: filesystem.SymlinkArgs) Error!filesystem.Entry {
            return self.createChild(c, a.parent, a.name, .{ .mode = S.IFLNK | 0o777, .target = a.link });
        }

        pub fn mknod(self: *Self, c: *Ctx, a: filesystem.MknodArgs) Error!filesystem.Entry {
            // The kernel already applied the umask (DONT_MASK is not advertised).
            return self.createChild(c, a.parent, a.name, .{ .mode = a.mode, .rdev = a.rdev });
        }

        pub fn mkdir(self: *Self, c: *Ctx, a: filesystem.MkdirArgs) Error!filesystem.Entry {
            return self.createChild(c, a.parent, a.name, .{ .mode = (a.mode & ~@as(u32, S.IFMT)) | S.IFDIR });
        }

        pub fn create(self: *Self, c: *Ctx, a: filesystem.CreateArgs) Error!filesystem.Created {
            const e = try self.createChild(c, a.parent, a.name, .{ .mode = (a.mode & ~@as(u32, S.IFMT)) | S.IFREG });
            return .{ .entry = e, .fh = self.mintFh(), .open_flags = self.regularOpenFlags() };
        }

        /// unlink/rmdir: find (no guards) → lock the full set → revalidate
        /// → stage → commit.
        fn removeChild(self: *Self, c: *Ctx, parent_global: u64, name: []const u8, want_dir: bool) Error!void {
            try checkMutableName(parent_global, name);
            const parent = try localOf(parent_global);
            const probe = (try self.findDentry(parent, name)) orelse return error.FileNotFound;
            if (want_dir and !probe.isDir()) return error.NotDir;
            if (!want_dir and probe.isDir()) return error.IsDir;
            if (self.spansHomes(&.{ parent_global, probe.global })) return self.xvRemove(c, parent_global, name, probe, want_dir);
            var plan: economy.GuardPlan = .{};
            const shared = economy.unlinkParentShared(want_dir, parent, probe.local);
            plan.inode_reqs[0] = .{ .ino = parent, .mode = if (shared) .shared else .exclusive };
            plan.inode_reqs[1] = .{ .ino = probe.local, .mode = .exclusive };
            plan.inode_len = 2;
            plan.dentry_reqs[0] = .{ .parent = parent, .name = name, .mode = .exclusive };
            plan.dentry_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            // Revalidate under the guards: the same name still names the same child.
            const found = (try self.findDentry(parent, name)) orelse return error.FileNotFound;
            if (found.global != probe.global) return error.FileNotFound;
            const cv = try self.readInode(found.local);
            if (want_dir and try self.dirHasEntries(found.local)) return error.DirectoryNotEmpty;
            const pv: ?InodeValue = if (shared) null else try self.readDir(parent);
            const req: economy.Unlink = .{ .now = now(), .parent = parent, .name = name, .pos = found.pos, .child = found.local, .child_value = cv, .is_dir = want_dir, .parent_value = pv };
            _ = if (want_dir) try economy.stageRmdir(t, req) else try economy.stageUnlink(t, req);
            t_owned = false;
            self.commit(c, t) catch |e| {
                if (self.redispatches(e)) return self.xvRemove(c, parent_global, name, found, want_dir);
                return e;
            };
            if (want_dir) self.forgetParent(found.global);
        }

        /// The cross-owner unlink/rmdir: the name FIRST, the count second
        /// (its witness at the child's holder); a child with NO record is
        /// the dangling arm (the name goes alone, loud).
        fn xvRemove(self: *Self, c: *Ctx, parent_global: u64, name: []const u8, found: Found, want_dir: bool) Error!void {
            const cv = try self.readInodeOpt(found.local);
            if (cv == null) sym_xv.ops.stats.xv_cross_owner_dangling_names.fire("a name whose child has no record at its holder");
            if (want_dir and try self.dirHasEntriesAt(found)) return error.DirectoryNotEmpty;
            const plan = sym_xv.steps.planUnlink(.{ .parent = parent_global, .name = name, .child = found.global, .child_value = cv, .is_dir = want_dir, .now = now() });
            _ = try self.runPlan(c, &plan);
            if (want_dir) self.forgetParent(found.global);
        }

        pub fn unlink(self: *Self, c: *Ctx, a: filesystem.UnlinkArgs) Error!void {
            return self.removeChild(c, a.parent, a.name, false);
        }

        pub fn rmdir(self: *Self, c: *Ctx, a: filesystem.UnlinkArgs) Error!void {
            return self.removeChild(c, a.parent, a.name, true);
        }

        pub fn rename(self: *Self, c: *Ctx, a: filesystem.RenameArgs) Error!void {
            try checkMutableName(a.parent, a.name);
            try checkMutableName(a.newparent, a.newname);
            const flags: economy.RenameFlags = .{ .noreplace = a.flags & RENAME_NOREPLACE != 0, .exchange = a.flags & RENAME_EXCHANGE != 0 };
            const whiteout = a.flags & RENAME_WHITEOUT != 0;
            if (a.flags & ~@as(u32, RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT) != 0) return error.InvalidArgument;
            if ((flags.noreplace and flags.exchange) or (whiteout and flags.exchange)) return error.InvalidArgument;
            const old_parent = try localOf(a.parent);
            const new_parent = try localOf(a.newparent);
            // Phase 1: discover both ends without guards.
            const old0 = (try self.findDentry(old_parent, a.name)) orelse return error.FileNotFound;
            const new0 = try self.findDentry(new_parent, a.newname);
            if (flags.exchange and new0 == null) return error.FileNotFound;
            {
                var homes: [4]u64 = .{ a.parent, a.newparent, old0.global, 0 };
                var n: usize = 3;
                if (new0) |d| {
                    homes[3] = d.global;
                    n = 4;
                }
                if (self.spansHomes(homes[0..n])) return self.xvRename(c, a, flags, whiteout, old0, new0);
            }
            // The conservative set: both parents exclusive (≥ what the
            // staged records require), both inodes, both names.
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = old_parent, .mode = .exclusive };
            plan.inode_reqs[1] = .{ .ino = new_parent, .mode = .exclusive };
            plan.inode_reqs[2] = .{ .ino = old0.local, .mode = .exclusive };
            plan.inode_len = 3;
            if (new0) |n| {
                plan.inode_reqs[3] = .{ .ino = n.local, .mode = .exclusive };
                plan.inode_len = 4;
            }
            plan.dentry_reqs[0] = .{ .parent = old_parent, .name = a.name, .mode = .exclusive };
            plan.dentry_reqs[1] = .{ .parent = new_parent, .name = a.newname, .mode = .exclusive };
            plan.dentry_len = 2;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            // Phase 2: revalidate and read under the guards.
            var old_chain: ChainBuf = .{};
            var old_kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &old_kvs);
            const old = (try self.scanChain(old_parent, a.name, &old_chain, &old_kvs)) orelse return error.FileNotFound;
            if (old.global != old0.global) return error.FileNotFound;
            var new_chain: ChainBuf = .{};
            var new_kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &new_kvs);
            const new = try self.scanChain(new_parent, a.newname, &new_chain, &new_kvs);
            if ((new == null) != (new0 == null) or (new != null and new.?.global != new0.?.global)) return error.FileNotFound;
            if (new) |n| {
                if (!flags.exchange) {
                    // POSIX: a directory replaces only a directory and vice versa.
                    if (old.isDir() and !n.isDir()) return error.NotDir;
                    if (!old.isDir() and n.isDir()) return error.IsDir;
                }
            }
            const old_pv = try self.readDir(old_parent);
            const new_pv: InodeValue = if (old_parent == new_parent) old_pv else try self.readDir(new_parent);
            const dest_value: ?InodeValue = if (new) |n| try self.readInode(n.local) else null;
            const dest_has_entries = if (new) |n| (n.isDir() and try self.dirHasEntries(n.local)) else false;
            var wo: ?economy.Whiteout = null;
            if (whiteout) {
                // On the old parent's member: the op stays on two members
                // at most (the dentries' and the moved inode's).
                const l = self.store.allocateIno(old_parent, true, false) catch |e| return storeErr(e);
                wo = .{ .local = l, .global = globalOf(l) };
            }
            _ = economy.stageRename(t, .{
                .name_key = self.store.nameKey(),
                .now = now(),
                .old_parent = old_parent,
                .old_name = a.name,
                .new_parent = new_parent,
                .new_name = a.newname,
                .flags = flags,
                .old_pos = old.pos,
                .new_pos = if (new) |n| n.pos else null,
                .new_chain = new_chain.slice(),
                .old_chain = old_chain.slice(),
                .src_local = old.local,
                .dest_local = if (new) |n| n.local else null,
                .old_parent_value = old_pv,
                .new_parent_value = new_pv,
                .dest_value = dest_value,
                .dest_has_entries = dest_has_entries,
                .whiteout = wo,
            }) catch |e| return switch (e) {
                error.InvalidFlags => error.InvalidArgument,
                error.SourceNotFound => error.FileNotFound,
                error.TargetExists => error.PathAlreadyExists,
                error.TargetNotEmpty => error.DirectoryNotEmpty,
                error.NameTooLong => error.NameTooLong,
                error.DentryChainOverflow => error.NoSpaceLeft,
                error.OutOfMemory => error.OutOfMemory,
            };
            t_owned = false;
            self.commit(c, t) catch |e| {
                if (self.redispatches(e)) return self.xvRename(c, a, flags, whiteout, old, new);
                return e;
            };
            if (old.isDir()) self.rememberParent(old.global, a.newparent);
            if (new) |n| {
                if (flags.exchange) {
                    if (n.isDir()) self.rememberParent(n.global, a.parent);
                } else if (n.isDir()) self.forgetParent(n.global);
            }
        }

        /// The cross-owner rename: a directory source takes the set-wide
        /// `dir_rename` lease OUTERMOST and walks the destination's
        /// ancestry on exact data; the destination's witness at ITS holder;
        /// the ENOTEMPTY probe before any step; ONE intent.
        fn xvRename(self: *Self, c: *Ctx, a: filesystem.RenameArgs, flags: economy.RenameFlags, whiteout: bool, old0: Found, new0: ?Found) Error!void {
            var lease: ?sym_xv.dir_rename.Taker.Held = null;
            defer if (lease) |*l| l.release();
            if (old0.isDir()) {
                if (self.dir_rename) |taker| {
                    lease = taker.take(clock.monotonicNs() + sym_xv.dir_rename.TAKE_WAIT_NS) catch |e| return switch (e) {
                        error.Deferred => error.Deferred,
                        error.HolderUnreachable => error.HolderUnreachable,
                        else => error.InputOutput,
                    };
                }
                if (a.parent != a.newparent) sym_xv.dir_rename.refuseRenameIntoOwnSubtree(self.ancestry(), old0.global, a.newparent) catch |e| return sym_xv.dir_rename.walkError(e);
            }
            const cv = (try self.readInodeOpt(old0.local)) orelse return error.FileNotFound;
            if (new0) |n| {
                if (!flags.exchange) {
                    if (old0.isDir() and !n.isDir()) return error.NotDir;
                    if (!old0.isDir() and n.isDir()) return error.IsDir;
                    if (flags.noreplace) return error.PathAlreadyExists;
                    if (n.isDir() and try self.dirHasEntriesAt(n)) return error.DirectoryNotEmpty;
                }
            }
            const stamp = now();
            if (flags.exchange) {
                const n = new0.?;
                const nv = (try self.readInodeOpt(n.local)) orelse return error.FileNotFound;
                const plan = sym_xv.steps.planExchange(.{ .old_parent = a.parent, .old_name = a.name, .new_parent = a.newparent, .new_name = a.newname, .a = old0.global, .a_mode = cv.mode, .b = n.global, .b_mode = nv.mode, .now = stamp });
                _ = try self.runPlan(c, &plan);
                if (old0.isDir()) self.rememberParent(old0.global, a.newparent);
                if (n.isDir()) self.rememberParent(n.global, a.parent);
                return;
            }
            var dest: ?struct { ino: u64, value: InodeValue } = null;
            if (new0) |n| dest = .{ .ino = n.global, .value = (try self.readInodeOpt(n.local)) orelse return error.FileNotFound };
            var wo: ?u64 = null;
            if (whiteout) {
                const l = self.store.allocateIno(try localOf(a.parent), true, false) catch |e| return storeErr(e);
                wo = globalOf(l);
            }
            const plan = sym_xv.steps.planRename(.{ .old_parent = a.parent, .old_name = a.name, .new_parent = a.newparent, .new_name = a.newname, .child = old0.global, .child_mode = cv.mode, .dest = if (dest) |d| .{ .ino = d.ino, .value = d.value } else null, .whiteout = wo, .now = stamp });
            _ = try self.runPlan(c, &plan);
            if (old0.isDir()) self.rememberParent(old0.global, a.newparent);
            if (new0) |n| if (n.isDir()) self.forgetParent(n.global);
        }

        /// The ancestry walk's seams over this filesystem: the `..` memo
        /// for the parent, the parent's dentry set (through the divert) for
        /// the name, the exact lookup at the parent's holder.
        fn ancestry(self: *Self) sym_xv.dir_rename.Ancestry {
            return .{ .ctx = self, .linkOf = &ancestryLinkOf, .scanParent = &ancestryScan, .lookupExact = &ancestryExact, .stripeOwner = &ancestryStripe };
        }

        fn ancestryLinkOf(_: *anyopaque, _: u64, _: *[255]u8) ?sym_xv.dir_rename.ParentLink {
            // The memo keeps the parent, not the name: every hop takes the
            // scan belt (counted) — a directory rename across holders is rare.
            return null;
        }

        fn ancestryScan(ctx: *anyopaque, dir: u64, name_buf: *[255]u8) ?sym_xv.dir_rename.ParentLink {
            const self: *Self = @ptrCast(@alignCast(ctx));
            const parent = self.parentOf(dir) orelse return null;
            const plocal = localOf(parent) catch return null;
            var arena_state = std.heap.ArenaAllocator.init(self.gpa);
            defer arena_state.deinit();
            var recs: std.ArrayList(sym_xv.stripe.DirRec) = .empty;
            var after: u64 = 0;
            while (true) {
                recs.clearRetainingCapacity();
                const more = self.dirPage(arena_state.allocator(), plocal, after, 512, &recs) catch return null;
                for (recs.items) |r| {
                    if (r.child == dir) {
                        @memcpy(name_buf[0..r.name.len], r.name);
                        return .{ .parent = parent, .name = name_buf[0..r.name.len] };
                    }
                    after = r.cookie;
                }
                if (!more or recs.items.len == 0) return null;
            }
        }

        fn ancestryExact(ctx: *anyopaque, parent: u64, name: []const u8) errors.Error!?u64 {
            const self: *Self = @ptrCast(@alignCast(ctx));
            const f = (try self.findDentry(try localOf(parent), name)) orelse return null;
            return f.global;
        }

        fn ancestryStripe(_: *anyopaque, _: u64) ?u64 {
            return null;
        }

        /// A directory's dentry page at its holder (the divert's dentry set
        /// for a foreign one; the store's range for ours).
        fn dirPage(self: *Self, arena: Allocator, dir_local: u64, after: u64, max: usize, out: *std.ArrayList(sym_xv.stripe.DirRec)) Error!bool {
            if (try self.tokenServe(dir_local, .{ .dentries = true })) |served| {
                defer served.release(self.io);
                const e = served.entry orelse return false;
                const page = e.pageAfter(after);
                const n = @min(page.len, max);
                for (page[0..n]) |r| try out.append(arena, .{ .cookie = r.cookie, .child = r.child_ino, .file_type = r.file_type, .name = try arena.dupe(u8, r.name) });
                return page.len > max;
            }
            return sym_xv.stripe.pageFromStore(Store, self.store, arena, globalOf(dir_local), after, max, out) catch |e| switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.InputOutput,
            };
        }

        pub fn link(self: *Self, c: *Ctx, a: filesystem.LinkArgs) Error!filesystem.Entry {
            try checkMutableName(a.newparent, a.newname);
            if (virtual_inodes.isVirtualIno(a.ino)) return error.PermissionDenied;
            const parent = try localOf(a.newparent);
            const child = try localOf(a.ino);
            if (self.spansHomes(&.{ a.newparent, a.ino })) return self.xvLink(c, a);
            var plan: economy.GuardPlan = .{};
            plan.inode_reqs[0] = .{ .ino = parent, .mode = .exclusive };
            plan.inode_reqs[1] = .{ .ino = child, .mode = .exclusive };
            plan.inode_len = 2;
            plan.dentry_reqs[0] = .{ .parent = parent, .name = a.newname, .mode = .exclusive };
            plan.dentry_len = 1;
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            const pv = try self.readDir(parent);
            const cv = try self.readInode(child);
            if (economy.isDirMode(cv.mode)) return error.AccessDenied;
            var chain: ChainBuf = .{};
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            if (try self.scanChain(parent, a.newname, &chain, &kvs) != null) return error.PathAlreadyExists;
            const out = economy.stageLink(t, .{ .name_key = self.store.nameKey(), .now = now(), .parent = parent, .parent_value = pv, .name = a.newname, .child = child, .global_child = a.ino, .child_value = cv, .chain = chain.slice() }) catch |e| return switch (e) {
                error.TooManyLinks => error.TooManyLinks,
                error.NameTooLong => error.NameTooLong,
                error.DentryChainOverflow => error.NoSpaceLeft,
                error.OutOfMemory => error.OutOfMemory,
            };
            t_owned = false;
            self.commit(c, t) catch |e| {
                if (self.redispatches(e)) return self.xvLink(c, a);
                return e;
            };
            _ = self.pending.retire(child);
            return self.entryOf(a.ino, out.child);
        }

        /// The cross-owner link: the count FIRST (its witness at the file's
        /// holder; EMLINK before any step), the insert second.
        fn xvLink(self: *Self, c: *Ctx, a: filesystem.LinkArgs) Error!filesystem.Entry {
            const parent = try localOf(a.newparent);
            const child = try localOf(a.ino);
            _ = try self.readDir(parent);
            const cv = try self.readInode(child);
            if (economy.isDirMode(cv.mode)) return error.AccessDenied;
            if (try self.findDentry(parent, a.newname) != null) return error.PathAlreadyExists;
            const plan = sym_xv.steps.planLink(.{ .parent = a.newparent, .name = a.newname, .ino = a.ino, .child_value = cv, .now = now() }) catch return error.TooManyLinks;
            const done = try self.runPlan(c, &plan);
            _ = self.pending.retire(child);
            return self.entryOf(a.ino, done.inode(0) orelse cv);
        }

        pub fn open(self: *Self, c: *Ctx, a: filesystem.OpenArgs) Error!filesystem.Opened {
            if (virtual_inodes.isVirtualIno(a.ino)) {
                // The generation IS the pin: fh = the generation ino, one
                // reference until RELEASE. FOPEN_DIRECT_IO: the kernel
                // trusts the read replies (short read = EOF) instead of
                // a cached i_size; a write-intent open is EACCES (0400).
                if (a.flags & @as(u32, @bitCast(linux.O{ .ACCMODE = .WRONLY })) != 0 or a.flags & @as(u32, @bitCast(linux.O{ .ACCMODE = .RDWR })) != 0) return error.AccessDenied;
                if (!self.virtuals.pin(a.ino)) return error.StaleHandle;
                return .{ .fh = a.ino, .open_flags = abi.FOPEN_DIRECT_IO };
            }
            const local = try localOf(a.ino);
            const v = try self.readInode(local);
            if (economy.isDirMode(v.mode)) return error.IsDir;
            // ATOMIC_O_TRUNC: the truncate rides the open (no separate SETATTR).
            if (a.flags & O_TRUNC != 0 and v.mode & S.IFMT == S.IFREG) {
                const rec = try self.readLayout(local);
                const kind = kindOf(rec);
                self.freeLayout(rec);
                self.settleCustody(local, kind);
                var after: AfterCommit = .{};
                // (3.7) before the 4a set, held across the commit (module doc).
                var layout_lock: ?Data.LayoutGuard = self.data.lockLayout(self.io, local);
                errdefer if (layout_lock) |g| g.release(self.io);
                const t = try self.beginExclusive(local, @src());
                var t_owned = true;
                errdefer if (t_owned) self.store.abandon(t);
                var stored = try self.readInode(local);
                try self.stageTruncate(c, t, local, stored, 0, &after);
                stored.size = 0;
                const stamp = now();
                stored.mtime = stamp;
                stored.ctime = stamp;
                if (a.open_flags & abi.FUSE_OPEN_KILL_SUIDGID != 0) try self.killpriv(t, local, &stored);
                try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &stored.encode());
                t_owned = false;
                try self.commit(c, t);
                layout_lock.?.release(self.io);
                layout_lock = null;
                _ = self.pending.retire(local);
                self.afterCommit(&after);
                self.data.noteShape(local, 0, if (kind == .striped) .striped else .inline_);
            }
            return .{ .fh = self.mintFh(), .open_flags = self.regularOpenFlags() };
        }

        pub fn read(self: *Self, c: *Ctx, a: filesystem.ReadArgs) Error!filesystem.Data {
            if (virtual_inodes.isVirtualIno(a.ino)) {
                // The slice of the frozen generation (empty past EOF).
                const n = @min(@as(usize, a.size), c.window.len);
                return .{ .len = self.virtuals.read(a.ino, a.offset, c.window[0..n]) orelse return error.StaleHandle };
            }
            const local = try localOf(a.ino);
            c.stage(.meta_backend_start);
            const v = try self.readInode(local);
            if (a.offset >= v.size) {
                c.stage(.meta_backend_done);
                return .{ .len = 0 };
            }
            const want: usize = @intCast(@min(@as(u64, a.size), v.size - a.offset));
            const n = @min(want, c.window.len);
            const rec = try self.readLayout(local);
            defer self.freeLayout(rec);
            c.stage(.meta_backend_done);
            switch (kindOf(rec)) {
                .inline_ => {
                    // The record IS the payload: served here for every store.
                    @memset(c.window[0..n], 0);
                    if (rec) |r| if (a.offset < r.layout.data.len) {
                        const avail = r.layout.data[@intCast(a.offset)..];
                        const take = @min(n, avail.len);
                        @memcpy(c.window[0..take], avail[0..take]);
                    };
                    return .{ .len = n };
                },
                .staged, .packed_, .striped => {
                    c.stage(.read_routed);
                    defer c.stage(.read_return);
                    const got = try self.data.read(self.io, local, a.offset, c.window[0..n], c.window.len > 0, a.flags & O_DIRECT != 0);
                    return .{ .len = got };
                },
            }
        }

        /// THE WRITE ROUTING (module doc): inline ≤ `inline_max` on an
        /// inline file; staged ≤ one block while the set stages (a
        /// non-striped file); else striped — promoting a resident payload
        /// to its own block DURABLY first.
        pub fn write(self: *Self, c: *Ctx, a: filesystem.WriteArgs) Error!filesystem.Written {
            const local = try localOf(a.ino);
            const len: u64 = if (a.held) |h| h.src.len else a.data.len;
            const end = std.math.add(u64, a.offset, len) catch return error.FileTooBig;
            // (3.7) BEFORE the layout read, held across the arm's commit
            // (module doc): the inline and staged arms and the striped
            // promotion are layout-record RMWs — against the plane's own
            // promotion of this ino under (3.5)+(3.7), which re-checks the
            // staged identity but cannot see a write whose read→publish→
            // commit interleaves with its own (fsstress -p 20: a staged
            // write's publish overwrote the packed record, its tenant's
            // +ref left durable — fsck C2/C3/C8 on every pack block). The
            // striped arm's data I/O takes none: the plane's uploads take
            // (3.7) themselves inside their (3.5).
            var layout_lock: ?Data.LayoutGuard = self.data.lockLayout(self.io, local);
            errdefer if (layout_lock) |g| g.release(self.io);
            const rec = try self.readLayout(local);
            defer self.freeLayout(rec);
            const kind = kindOf(rec);
            if (!self.data.hasVolumes()) {
                if (end > self.inline_max) {
                    fuse_write_no_data_volume_refused.inc();
                    return error.FileTooBig;
                }
                return self.writeInline(c, local, a, rec, end, &layout_lock);
            }
            if (kind == .inline_ and end <= self.inline_max) return self.writeInline(c, local, a, rec, end, &layout_lock);
            // The staged arm rings the WHOLE file: its bound is the file's
            // reach — `max(size, end)` — never the write's end alone (a
            // file grown past the block by truncate rang a 5 MiB image no
            // promotion could place: fstests generic/030, fsstress).
            const reach = @max(end, (try self.readInode(local)).size);
            if (self.data.stagingOn() and kind != .striped and reach <= self.block_size) {
                if (try self.writeStaged(c, local, a, rec, end, &layout_lock)) |w| return w;
                // The ring cannot hold the image (no hole, the pool at its
                // cap): the ring is a cache, never the set's capacity — the
                // own-block arm takes the write (the resident image lands
                // durably on its own block, the piece is admitted striped;
                // ENOSPC stays the data plane's own terminal verdict). The
                // payload is in hand already: the striped arm reads it as
                // bytes (rung 9: a 20-writer fsstress burst answered ENOSPC
                // on a volume with 1,700 free blocks).
                fuse_write_staged_ring_full_own_block.inc();
                var in_hand = a;
                in_hand.data = try self.payloadBytes(a);
                in_hand.held = null;
                return self.writeStriped(c, local, in_hand, rec, end, &layout_lock);
            }
            return self.writeStriped(c, local, a, rec, end, &layout_lock);
        }

        /// The (3.7) guard leaves right after its commit landed (before
        /// `afterCommit`, whose synchronous sweep takes it again).
        fn dropLayoutLock(self: *Self, lock: *?Data.LayoutGuard) void {
            if (lock.*) |g| g.release(self.io);
            lock.* = null;
        }

        /// The payload bytes in hand (a held slot extracted once).
        fn payloadBytes(self: *Self, a: filesystem.WriteArgs) Error![]const u8 {
            const h = a.held orelse return a.data;
            return h.extract(h.ctx, self.io) catch return error.InputOutput;
        }

        fn writeInline(self: *Self, c: *Ctx, local: u64, a: filesystem.WriteArgs, rec: ?LayoutRec, end: u64, layout_lock: *?Data.LayoutGuard) Error!filesystem.Written {
            const data = try self.payloadBytes(a);
            const t = try self.beginExclusive(local, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            var v = try self.readInode(local);
            if (v.mode & S.IFMT != S.IFREG) return error.InvalidArgument;
            // The new image: the old bytes zero-extended to max(old, end), the write overlaid.
            const old_data: []const u8 = if (rec) |r| r.layout.data else &.{};
            const new_len: usize = @intCast(@max(old_data.len, end));
            const buf = try self.gpa.alloc(u8, new_len);
            defer self.gpa.free(buf);
            @memset(buf, 0);
            @memcpy(buf[0..old_data.len], old_data);
            @memcpy(buf[@intCast(a.offset)..@intCast(end)], data);
            const size = @max(v.size, end);
            try self.publish(t, local, rec, Layout.inlineData(size, buf), &.{});
            try self.stageInodePostlude(t, local, &v, size, a.write_flags);
            t_owned = false;
            try self.commit(c, t);
            self.dropLayoutLock(layout_lock);
            _ = self.pending.retire(local);
            self.data.noteShape(local, size, .inline_);
            fuse_write_inline.inc();
            write_admit.noteInlineWrite(data.len);
            return .{ .size = @intCast(data.len) };
        }

        /// The staged arm (decision 0003 §J.2). An EXTENDING write (at or
        /// past the end: O_APPEND, a sequential small write) into a file
        /// whose ring record has slack lands IN PLACE — the appended bytes
        /// into the slot, no image copied either way; otherwise the whole
        /// payload image is re-staged under the file's identity (a fresh one
        /// for an inline/packed file) with append slack when the write
        /// extends. Then ONE tx: the staged layout (its size) and the
        /// inode's Δ(size, mtime, ctime). A displaced tenant freed after.
        /// Null ⇔ the ring refused the image (nothing recorded, nothing
        /// published): the caller's own-block arm.
        fn writeStaged(self: *Self, c: *Ctx, local: u64, a: filesystem.WriteArgs, rec: ?LayoutRec, end: u64, layout_lock: *?Data.LayoutGuard) Error!?filesystem.Written {
            const data = try self.payloadBytes(a);
            const probe = try self.readInode(local);
            if (probe.mode & S.IFMT != S.IFREG) return error.InvalidArgument;
            const size = @max(probe.size, end);
            const extending = a.offset >= probe.size;
            const keep_identity = rec != null and rec.?.layout.kind == .staged;
            const file_id = if (keep_identity) rec.?.layout.file_id else self.data.mintFileId(local);
            const k = staging.StagingKey.init(file_id, local);
            // The resident image's length is the file's size (the ring holds
            // the whole payload), so an extending write appends at its offset.
            var in_place = false;
            var generation: u64 = 0;
            if (extending and keep_identity) {
                if (try self.data.appendStaged(k, a.offset, data)) |g| {
                    generation = g;
                    in_place = true;
                }
            }
            if (!in_place) {
                const buf = try self.gpa.alloc(u8, @intCast(layout.ceilGrain(size)));
                defer self.gpa.free(buf);
                _ = try self.residentImage(local, rec, buf);
                @memcpy(buf[@intCast(a.offset)..@intCast(end)], data);
                const image = buf[0..@intCast(size)];
                generation = self.data.stageImage(k, image, if (extending) .append else .none) catch |e| switch (e) {
                    error.NoSpaceLeft => return null,
                    else => |x| return x,
                };
            }
            var after: AfterCommit = .{};
            if (!keep_identity) noteDisplaced(local, rec, &after);
            const t = try self.beginExclusive(local, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            var v = try self.readInode(local);
            try self.publish(t, local, rec, Layout.staged(size, file_id, null), &.{});
            try self.stageInodePostlude(t, local, &v, size, a.write_flags);
            t_owned = false;
            self.commit(c, t) catch |e| {
                // A whole image that will never be named leaves; an in-place
                // append stays (the longer image is the file's, its tail
                // past the durable size invisible).
                if (!in_place) self.data.releaseStaged(k, generation);
                return e;
            };
            self.dropLayoutLock(layout_lock);
            _ = self.pending.retire(local);
            self.afterCommit(&after);
            self.data.noteShape(local, size, .staged);
            fuse_write_staged.inc();
            write_admit.noteInlineWrite(data.len);
            return .{ .size = @intCast(data.len) };
        }

        /// `write_admit.ParkHook.parked`: the op's station while the data
        /// plane parks it at the parked-buffer cap.
        fn stampParkedPool(ctx: *anyopaque) void {
            const c: *Ctx = @ptrCast(@alignCast(ctx));
            c.stage(.write_parked_pool);
        }

        /// The striped arm: a non-striped file promotes first (its resident
        /// payload DMA'd to its own block, the striped layout naming it
        /// published — block I/O before the type flip), then the data
        /// plane's admission per piece, then the inode's size claim.
        fn writeStriped(self: *Self, c: *Ctx, local: u64, a: filesystem.WriteArgs, rec: ?LayoutRec, end: u64, layout_lock: *?Data.LayoutGuard) Error!filesystem.Written {
            if (end > self.maxFileSize()) {
                fuse_write_u32_index_refused.inc();
                return error.FileTooBig;
            }
            const probe = try self.readInode(local);
            if (probe.mode & S.IFMT != S.IFREG) return error.InvalidArgument;
            var old_size = probe.size;
            // The promotion's layout RMW runs under the (3.7) guard the
            // write took; the data plane's own I/O below runs without it.
            if (kindOf(rec) != .striped) old_size = try self.promoteToStriped(c, local, rec, probe.size);
            self.dropLayoutLock(layout_lock);
            const payload: write_admit.Payload = if (a.held) |h|
                (if (self.data.canStoreHeld()) write_admit.Payload{ .slot = .{ .src = h.src, .class = if (h.class == .direct_gup) .odirect else .page_cache, .commit_retain = false, .ctx = h.ctx, .extract = h.extract } } else .{ .bytes = try self.payloadBytes(a) })
            else if (a.placed) |p|
                // A ring write's placed sever: the striped arm adopts the
                // assembly (or copies from it); every other arm reads `data`.
                .{ .placed = p }
            else
                .{ .bytes = a.data };
            c.stage(.write_admitted);
            const outcome = self.data.write(self.io, local, a.offset, payload, a.class, .{ .kind = .striped, .size = old_size, .map_resident = true }, .{ .ctx = c, .parked = &stampParkedPool }) catch |e| return switch (e) {
                error.NoSpaceLeft => error.NoSpaceLeft,
                error.InputOutput => error.InputOutput,
                error.VolumeFailed => error.VolumeFailed,
                error.WriterGuardFenced => error.WriterGuardFenced,
                error.OutOfMemory => error.OutOfMemory,
                error.WorkerShutdown => error.WorkerShutdown,
                error.QueueFull => error.QueueFull,
                error.ReadOnlyFileSystem => error.ReadOnlyFileSystem,
                error.FileTooBig => error.FileTooBig,
            };
            c.stage(.write_done);
            // The postlude: SIZE NEVER LEADS DATA — the claim lands after the
            // pieces were acked by their arms.
            const t = try self.beginExclusive(local, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            var v = try self.readInode(local);
            const size = @max(v.size, outcome.size_claim);
            if (self.times_park and size == v.size and a.write_flags & abi.FUSE_WRITE_KILL_SUIDGID == 0) {
                // §L.6: a size-neutral write's only record would be the
                // 76 B Δ(mtime, ctime) — one grain-aligned window and one
                // device round trip for two timestamps. Park the stamp
                // under the guard instead (fold-visible on every read; the
                // drain journals it); the tx stays empty, nothing commits.
                try self.parkWriteTimes(local);
                t_owned = false;
                self.store.abandon(t);
            } else {
                try self.stageInodePostlude(t, local, &v, size, a.write_flags);
                t_owned = false;
                try self.commit(c, t);
                _ = self.pending.retire(local);
            }
            self.data.noteShape(local, size, .striped);
            fuse_write_striped.inc();
            return .{ .size = @intCast(outcome.written) };
        }

        /// The write's stamp parked as `local`'s pending refinement (under
        /// the caller's exclusive guard — `PendingTimes`' per-ino law): the
        /// signed monotone max, the same law `fold` serves it by. A park
        /// that crosses the cap wakes the drain ahead of its tick.
        fn parkWriteTimes(self: *Self, local: u64) Error!void {
            const stamp = now();
            switch (try self.pending.park(local, stamp, stamp)) {
                .parked => {},
                .parked_wake_drain => self.times_drain_wake.notifyOne(self.io),
            }
            fuse_write_times_parked.inc();
        }

        /// The stored record UNFOLDED (null when absent): the drain's base —
        /// `economy.drainDelta` measures the parked stamp's advance against
        /// what is durable, and a folded read would already carry it.
        fn readInodeStored(self: *Self, ino: u64) Error!?InodeValue {
            const bytes = (self.store.lookup(self.gpa, key.TREE_INODES, &key.inodeKey(ino)) catch |e| return storeErr(e)) orelse return null;
            defer self.gpa.free(bytes);
            return InodeValue.decode(bytes) catch return error.InputOutput;
        }

        /// Commit one drain batch under ONE canonical exclusive plan (the
        /// ordinary op's width, `dlm.max_requests`): the members whose
        /// stamp still advances the stored record are staged (the rest —
        /// destroyed, or superseded by a committed write — carry nothing);
        /// then every member is retired IF its parked stamp is still the one
        /// drained — the guards drop at the commit's terminal outcome, and a
        /// write that parked a newer stamp before the retire keeps it for
        /// the next drain. A failed commit keeps the batch parked for the
        /// next trigger — except a slot that moved to another appender
        /// (`SlotMoved`), whose refinements this mount can never commit:
        /// those are dropped and counted (`meta_kv_times_echo_foreign_
        /// dropped`), never left to refuse every later batch at the door.
        fn drainTimesBatch(self: *Self, c: *Ctx, batch: []const u64) Error!u64 {
            assert(batch.len <= dlm.max_requests);
            var plan: economy.GuardPlan = .{};
            for (batch) |ino| {
                plan.inode_reqs[plan.inode_len] = .{ .ino = ino, .mode = .exclusive };
                plan.inode_len += 1;
            }
            const held = try self.lockPlan(&plan);
            const t = try self.beginWith(held, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            var drained: [dlm.max_requests]?economy.Times = @splat(null);
            var staged: u64 = 0;
            for (batch, 0..) |ino, i| {
                const parked = self.pending.parked(ino) orelse continue;
                drained[i] = parked;
                if ((try economy.stageDrainMember(t, ino, parked, try self.readInodeStored(ino))) == .staged) staged += 1;
            }
            t_owned = false;
            if (staged == 0) {
                self.store.abandon(t);
            } else self.commit(c, t) catch |e| {
                if (e != error.SlotMoved) return e;
                for (batch) |ino| _ = self.pending.retire(ino);
                sym_handover.stats.meta_kv_times_echo_foreign_dropped.add(batch.len);
                return 0;
            };
            for (batch, drained[0..batch.len]) |ino, d| if (d) |times| {
                _ = self.pending.retireIfStill(ino, times);
            };
            if (staged != 0) economy.noteDrainCommitted(staged);
            return staged;
        }

        /// THE TIMES DRAIN (decision 0003 §L.6; charter §5.2 "Journal
        /// economy"): every parked refinement — the write path's stamps,
        /// the absorbed SETATTR echoes — made durable in batched Δ commits,
        /// `dlm.max_requests` inos per entry. Run by the daemon's cadence
        /// (the flush interval; the cap wakes it early), by `fsyncdir` and
        /// the dismount ahead of their barriers, and by tests. Returns the
        /// refinements made durable.
        pub fn drainPendingTimes(self: *Self, c: *Ctx) Error!u64 {
            var total: u64 = 0;
            var buf: [economy.PENDING_TIMES_DRAIN_BATCH]u64 = undefined;
            while (true) {
                const snapshot = self.pending.drainBatch(&buf);
                if (snapshot.len == 0) return total;
                var i: usize = 0;
                while (i < snapshot.len) : (i += dlm.max_requests) {
                    total += try self.drainTimesBatch(c, snapshot[i..@min(i + dlm.max_requests, snapshot.len)]);
                }
                if (snapshot.len < buf.len) return total;
            }
        }

        /// THE HANDOVER'S STEP 0 (`sym_handover.TimesDrain`; charter §5.5):
        /// every stamp parked on an inode of routing slot `slot` committed
        /// as a Δ while this holder's door still admits the slot's commits,
        /// `dlm.max_requests` inos per entry; the other slots' stamps stay
        /// for the cadence. Returns the refinements made durable.
        pub fn drainPendingTimesOfSlot(self: *Self, c: *Ctx, slot: u16) Error!u64 {
            var inos_of_slot: std.ArrayList(u64) = .empty;
            defer inos_of_slot.deinit(self.gpa);
            try self.pending.parkedOfSlot(self.gpa, route_table.forestSlotOf(slot), &inos_of_slot);
            var total: u64 = 0;
            var i: usize = 0;
            while (i < inos_of_slot.items.len) : (i += dlm.max_requests) {
                total += try self.drainTimesBatch(c, inos_of_slot.items[i..@min(i + dlm.max_requests, inos_of_slot.items.len)]);
            }
            return total;
        }

        /// The handover's step 0 as the plane calls it (its own op context:
        /// the transfer runs on the wire service or the plane's cadence,
        /// never under a FUSE request).
        fn timesDrainSeam(ctx: *anyopaque, slot: u16) u64 {
            const self: *Self = @ptrCast(@alignCast(ctx));
            var trace = op_trace.OpTrace.begin(0, .other);
            defer trace.finish();
            var c: Ctx = .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = self.io, .trace = &trace, .wd_slot = null, .window = &.{} };
            return self.drainPendingTimesOfSlot(&c, slot) catch |e| {
                log.warn("kv_fs: the handover's step-0 times drain of slot {d} failed ({t}); the slot's remaining parked refinements meet the moved door at the next drain and are dropped counted", .{ slot, e });
                return 0;
            };
        }

        /// `fsync(ino)`'s durability covers the ino's parked stamp: ONE Δ
        /// commit ahead of the barriers when one is parked, nothing
        /// otherwise (the common case is one relaxed load).
        fn drainTimesOf(self: *Self, c: *Ctx, local: u64) Error!void {
            if (self.pending.parked(local) == null) return;
            _ = try self.drainTimesBatch(c, &.{local});
        }

        /// The resident payload to its own block, the striped layout
        /// published (ONE tx), the displaced custody freed after. Returns
        /// the size the striped file starts at. The image DMA'd is at most
        /// ONE block: a resident payload never exceeds it, and a size past
        /// it (a truncate-grown file) is the sparse zero run the striped
        /// layout's `size` names — never a second block of zeros (and never
        /// EFBIG: fstests generic/030).
        fn promoteToStriped(self: *Self, c: *Ctx, local: u64, rec: ?LayoutRec, size: u64) Error!u64 {
            var after: AfterCommit = .{};
            var entries: [1]layout.MapEntryWire = undefined;
            var n_entries: usize = 0;
            var ob: ?Data.OwnBlock = null;
            if (size > 0) {
                const image_len = @min(size, self.block_size);
                const buf = try self.gpa.alloc(u8, @intCast(layout.ceilGrain(image_len)));
                defer self.gpa.free(buf);
                _ = try self.residentImage(local, rec, buf);
                ob = try self.data.ownBlock(self.io, local, buf[0..@intCast(image_len)]);
                entries[0] = (layout.MapEntry{ .block = 0, .mapping = ob.?.mapping }).encode();
                n_entries = 1;
            }
            errdefer if (ob) |o| self.data.abandonOwnBlock(o);
            noteDisplaced(local, rec, &after);
            const t = try self.beginExclusive(local, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            const proofs: []const layout.DmaProof = if (ob) |o| &.{o.proof} else &.{};
            try self.publish(t, local, rec, Layout.stripedInline(size, entries[0..n_entries]), proofs);
            t_owned = false;
            try self.commit(c, t);
            self.afterCommit(&after);
            fuse_write_promotions_to_striped.inc();
            return size;
        }

        /// The inode's size claim + times + KILLPRIV_V2 in the write's tx:
        /// ONE Δ(size, mtime, ctime) record — the economy's shape (charter
        /// §5.2; decision 0003 §J.2) — under the ino's exclusive guard, so
        /// the Δs of one ino land in commit order and the fold's newest size
        /// is the max; the mode strip (KILLPRIV) rewrites the record whole.
        /// A size-neutral striped write under the coalesced cadence never
        /// reaches here: its stamp is parked (`parkWriteTimes`, §L.6).
        fn stageInodePostlude(self: *Self, t: *tx.KvTx, local: u64, v: *InodeValue, size: u64, write_flags: u32) Error!void {
            const stamp = now();
            v.size = @max(v.size, size);
            v.mtime = stamp;
            if (economy.laterThan(stamp, v.ctime)) v.ctime = stamp;
            if (write_flags & abi.FUSE_WRITE_KILL_SUIDGID != 0) {
                try self.killpriv(t, local, v);
                try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &v.encode());
                return;
            }
            try t.stageDelta(key.TREE_INODES, &key.inodeKey(local), .{ .mask = record.DELTA_SIZE | record.DELTA_TIMES, .fields = .{ .size = v.size, .mtime = v.mtime, .ctime = v.ctime } });
        }

        /// Nothing is deferred at the metadata plane: every write is a landed
        /// journal entry (or write-side custody that drains at RELEASE), so a
        /// FLUSH has no latched error to carry.
        pub fn flush(_: *Self, _: *Ctx, _: filesystem.FlushArgs) Error!void {}

        /// RELEASE: the write-back class drains (charter §6.2 "flush at
        /// release").
        pub fn release(self: *Self, _: *Ctx, a: filesystem.ReleaseArgs) Error!void {
            if (virtual_inodes.isVirtualIno(a.ino)) return self.virtuals.unpin(a.ino);
            const local = localOf(a.ino) catch return;
            if (self.data.hasVolumes()) self.data.releaseIno(self.io, local);
        }

        /// fsync (charter §6.4): the ino's parked stamp journaled (§L.6),
        /// the data legs through the plane, then the trailing coalesced
        /// meta barrier (every acked entry durable).
        pub fn fsync(self: *Self, c: *Ctx, a: filesystem.FsyncArgs) Error!void {
            const local = try localOf(a.ino);
            try self.drainTimesOf(c, local);
            if (self.data.hasVolumes()) {
                const rec = try self.readLayout(local);
                defer self.freeLayout(rec);
                const staged_key: ?staging.StagingKey = if (rec) |r| (if (r.layout.kind == .staged) stagingKeyOf(local, r.layout) else null) else null;
                c.stage(.write_admitted);
                try self.data.fsync(self.io, local, staged_key);
                c.stage(.write_done);
                return;
            }
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            self.store.fsync() catch |e| return storeErr(e);
        }

        /// fsyncdir: the syncfs-class durability point — every parked
        /// stamp journaled ahead of the barrier that covers it.
        pub fn fsyncdir(self: *Self, c: *Ctx, _: filesystem.FsyncArgs) Error!void {
            _ = try self.drainPendingTimes(c);
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            self.store.fsync() catch |e| return storeErr(e);
        }

        pub fn opendir(self: *Self, _: *Ctx, a: filesystem.OpenArgs) Error!filesystem.Opened {
            _ = try self.readDir(try localOf(a.ino));
            return .{ .fh = self.mintFh(), .open_flags = 0 };
        }

        pub fn releasedir(_: *Self, _: *Ctx, _: filesystem.ReleaseArgs) Error!void {}

        pub fn readdir(self: *Self, c: *Ctx, a: filesystem.ReaddirArgs) Error!filesystem.Data {
            return self.listDir(c, a, false);
        }

        pub fn readdirplus(self: *Self, c: *Ctx, a: filesystem.ReaddirArgs) Error!filesystem.Data {
            return self.listDir(c, a, true);
        }

        /// Entries per range call: a page of 24+name-byte dirents is ≈ 100
        /// entries; one scan fills a 4 KiB window, a 64 KiB `getdents`
        /// buffer takes a handful.
        const READDIR_SCAN = 128;

        fn listDir(self: *Self, c: *Ctx, a: filesystem.ReaddirArgs, plus: bool) Error!filesystem.Data {
            const dir = try localOf(a.ino);
            const dv = try self.readDir(dir);
            // The virtual control inodes are LOOKUP-ONLY: never listed; their
            // reserved cookies (never emitted) resume as the end of the stream.
            if (virtual_inodes.isReservedCookie(a.offset)) return .{ .len = 0 };
            const pos = key.decodeReaddirCookie(a.offset) catch return error.InvalidArgument;
            var plain = codec.DirentWriter.init(c.window, a.size);
            var rich = codec.DirentPlusWriter.init(c.window, a.size);
            // `.` and `..`: nodeid 0 in the plus form (the kernel skips their
            // dentry linkage); the `..` ino is the memo's, else the dir's own.
            if (pos == .start) {
                if (!(try self.emit(&plain, &rich, plus, a.ino, key.READDIR_OFFSET_DOT, dv, ".", true))) return .{ .len = self.written(&plain, &rich, plus) };
            }
            if (pos == .start or pos == .after_dot) {
                const p = self.parentOf(a.ino) orelse a.ino;
                const pv = (try self.readInodeOpt(try localOf(p))) orelse dv;
                if (!(try self.emit(&plain, &rich, plus, p, key.READDIR_OFFSET_DOTDOT, pv, "..", true))) return .{ .len = self.written(&plain, &rich, plus) };
            }
            c.stage(.meta_backend_start);
            defer c.stage(.meta_backend_done);
            // A foreign directory: the page is the token's cookie-ordered set
            // strictly after the resume cookie (one binary search).
            if (try self.tokenServe(dir, .{ .dentries = true })) |served| {
                defer served.release(self.io);
                const e = served.entry orelse return error.FileNotFound;
                const after: u64 = switch (pos) {
                    .after_entry => a.offset,
                    else => 0,
                };
                for (e.pageAfter(after)) |rec| {
                    // The striping markers (NUL-led) never show (the ONE pager).
                    if (sym_xv.stripe.isMarkerName(rec.name)) continue;
                    const child_local = localOf(rec.child_ino) catch {
                        dentry_without_inode.fire("readdir found a dentry naming a foreign-slot ino");
                        continue;
                    };
                    const cv = (try self.readInodeOpt(child_local)) orelse {
                        dentry_without_inode.fire("readdir found a dentry whose inode record is absent");
                        continue;
                    };
                    if (plus and economy.isDirMode(cv.mode)) self.rememberParent(rec.child_ino, a.ino);
                    if (!(try self.emit(&plain, &rich, plus, rec.child_ino, rec.cookie, cv, rec.name, false))) break;
                }
                return .{ .len = self.written(&plain, &rich, plus) };
            }
            var cursor: ?[key.DENTRY_KEY_LEN]u8 = switch (pos) {
                .after_entry => |e| nextDentryKey(dir, e.hash54, e.coll_seq),
                else => key.dentryKey(dir, 0, 0),
            };
            const end = key.dentryKey(dir, key.HASH54_MAX, std.math.maxInt(u8));
            scan: while (cursor) |start| {
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(self.gpa, &kvs);
                self.store.range(self.gpa, key.TREE_DENTRIES, &start, &end, READDIR_SCAN, &kvs) catch |e| return storeErr(e);
                if (kvs.items.len == 0) break;
                var last_hash: u64 = 0;
                var last_coll: u8 = 0;
                for (kvs.items) |kv| {
                    const dk = key.DentryKey.decode(kv.key) catch return error.InputOutput;
                    last_hash = dk.hash54();
                    last_coll = dk.collSeq();
                    const dent = record.DentryValue.decode(kv.value) catch return error.InputOutput;
                    const cookie = key.encodeReaddirCookie(last_hash, last_coll);
                    if (sym_xv.stripe.isMarkerName(dent.name)) continue;
                    const child_local = localOf(dent.child_ino) catch {
                        dentry_without_inode.fire("readdir found a dentry naming a foreign-slot ino");
                        continue;
                    };
                    const cv = (try self.readInodeOpt(child_local)) orelse {
                        dentry_without_inode.fire("readdir found a dentry whose inode record is absent");
                        continue;
                    };
                    if (plus and economy.isDirMode(cv.mode)) self.rememberParent(dent.child_ino, a.ino);
                    if (!(try self.emit(&plain, &rich, plus, dent.child_ino, cookie, cv, dent.name, false))) break :scan;
                }
                if (kvs.items.len < READDIR_SCAN) break;
                cursor = nextDentryKey(dir, last_hash, last_coll);
            }
            return .{ .len = self.written(&plain, &rich, plus) };
        }

        /// The smallest dentry key of `dir` strictly after `(hash54, coll)`;
        /// null past the last one (the key stays 17 bytes — a `keySuccessor`
        /// would lengthen it past the kind's shape).
        fn nextDentryKey(dir: u64, hash54: u64, coll: u8) ?[key.DENTRY_KEY_LEN]u8 {
            const suffix = key.dentryKeySuffix(hash54, coll);
            if (suffix == key.dentryKeySuffix(key.HASH54_MAX, std.math.maxInt(u8))) return null;
            const next = suffix + 1;
            return key.dentryKey(dir, next >> 8, @truncate(next));
        }

        /// One dirent into whichever writer is live; false when it did not fit.
        fn emit(self: *Self, plain: *codec.DirentWriter, rich: *codec.DirentPlusWriter, plus: bool, global: u64, cookie: u64, v: InodeValue, name: []const u8, dot: bool) Error!bool {
            if (plus) {
                var e = self.entryOf(global, v);
                if (dot) e.nodeid = 0;
                rich.add(e, cookie, name) catch |err| return switch (err) {
                    error.Full => false,
                    error.EmptyName, error.NameTooLong => error.InputOutput,
                };
            } else {
                plain.add(global, cookie, v.mode, name) catch |err| return switch (err) {
                    error.Full => false,
                    error.EmptyName, error.NameTooLong => error.InputOutput,
                };
            }
            fuse_readdir_entries.inc();
            return true;
        }

        fn written(_: *Self, plain: *const codec.DirentWriter, rich: *const codec.DirentPlusWriter, plus: bool) usize {
            return if (plus) rich.len else plain.len;
        }

        /// `df`: the data volumes' allocators when the set has them (the
        /// charter's rule), else the metadata heap; `ffree` from the LIVE
        /// inode population (minted − destroyed, + the root), published as
        /// `live_inodes`.
        pub fn statfs(self: *Self, _: *Ctx, _: filesystem.StatfsArgs) Error!filesystem.Statfs {
            const used_inos = inos.publishLiveInodes(&.{self.store.allocatedInos() -| self.destroys.load()});
            var total: u64 = 0;
            var free: u64 = 0;
            if (self.data.statfs()) |sp| {
                total = sp.total_bytes;
                free = sp.free_bytes;
            } else {
                const sp = self.store.space();
                total = sp.total_bytes;
                free = sp.free_bytes;
            }
            return .{
                .blocks = total / STATFS_BSIZE,
                .bfree = free / STATFS_BSIZE,
                .bavail = free / STATFS_BSIZE,
                .files = INO_SPACE,
                .ffree = INO_SPACE -| used_inos,
                .bsize = STATFS_BSIZE,
                .namelen = @intCast(NAME_MAX),
                .frsize = STATFS_BSIZE,
            };
        }

        pub fn setxattr(self: *Self, c: *Ctx, a: filesystem.SetxattrArgs) Error!void {
            try checkName(a.name);
            if (virtual_inodes.isVirtualIno(a.ino)) return error.PermissionDenied;
            try screenXattr(a.name, error.PermissionDenied);
            if (a.value.len > record.xattrValueCap(self.store.nodeSize())) return error.ValueTooLarge;
            const local = try localOf(a.ino);
            if (self.foreignSlot(a.ino)) {
                const present = (try self.xattrValue(local, a.name)) != null;
                if (a.flags & XATTR_CREATE != 0 and present) return error.PathAlreadyExists;
                if (a.flags & XATTR_REPLACE != 0 and !present) return error.NoAttribute;
                _ = try self.shipRecord(a.ino, .{ .setxattr = .{ .name = a.name, .value = a.value } });
                return;
            }
            const t = try self.beginExclusive(local, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            var v = try self.readInode(local);
            const k = try self.xattrKeyFor(local, a.name);
            if (k.existing) |e| self.gpa.free(e);
            if (a.flags & XATTR_CREATE != 0 and k.existing != null) return error.PathAlreadyExists;
            if (a.flags & XATTR_REPLACE != 0 and k.existing == null) return error.NoAttribute;
            try self.stageXattrPut(t, &k.k, a.name, a.value);
            v.ctime = now();
            try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &v.encode());
            t_owned = false;
            try self.commit(c, t);
            _ = self.pending.retire(local);
        }

        pub fn getxattr(self: *Self, c: *Ctx, a: filesystem.GetxattrArgs) Error!filesystem.Xattr {
            try checkName(a.name);
            if (virtual_inodes.isVirtualIno(a.ino)) return error.NoAttribute;
            // `user.squeezefs.stripes` is a COMMAND, never a stored value: a
            // read answers the striping width in force on a directory.
            if (std.mem.eql(u8, a.name, sym_xv.stripe.COMMAND_XATTR)) {
                const p = self.xv orelse return error.NoAttribute;
                if (!economy.isDirMode((try self.readInode(try localOf(a.ino))).mode)) return error.NoAttribute;
                var buf: [4]u8 = undefined;
                const digits = std.fmt.bufPrint(&buf, "{d}\n", .{p.stripe_k}) catch unreachable;
                if (a.size == 0) return .{ .size = @intCast(digits.len) };
                if (digits.len > a.size or digits.len > c.window.len) return error.BufferTooSmall;
                @memcpy(c.window[0..digits.len], digits);
                return .{ .data = digits.len };
            }
            // `user.squeezefs.il0` is SYNTHESIZED by an armed session host,
            // never stored: the daemon's rendezvous blob (charter §7). The
            // read rides the already-authorized kernel path (read access ⇒
            // may read it). Unarmed, the reserved-family screen's ENODATA.
            if (std.mem.eql(u8, a.name, ipc_wire.IL0_XATTR)) {
                const face = self.il0 orelse return error.NoAttribute;
                var blob: [ipc_wire.BLOB_LEN]u8 = undefined;
                face.synthesize(face.ctx, &blob);
                if (a.size == 0) return .{ .size = @intCast(blob.len) };
                if (blob.len > a.size or blob.len > c.window.len) return error.BufferTooSmall;
                @memcpy(c.window[0..blob.len], &blob);
                return .{ .data = blob.len };
            }
            try screenXattr(a.name, error.NoAttribute);
            const value = (try self.xattrValue(try localOf(a.ino), a.name)) orelse return error.NoAttribute;
            defer self.gpa.free(value);
            if (a.size == 0) return .{ .size = @intCast(value.len) };
            if (value.len > a.size or value.len > c.window.len) return error.BufferTooSmall;
            @memcpy(c.window[0..value.len], value);
            return .{ .data = value.len };
        }

        pub fn listxattr(self: *Self, c: *Ctx, a: filesystem.ListxattrArgs) Error!filesystem.Xattr {
            if (virtual_inodes.isVirtualIno(a.ino)) return if (a.size == 0) .{ .size = 0 } else .{ .data = 0 };
            const local = try localOf(a.ino);
            if (try self.tokenServe(local, .{})) |served| {
                defer served.release(self.io);
                const e = served.entry orelse return error.FileNotFound;
                var total: usize = 0;
                for (e.xattrs) |x| if (!isReservedXattr(x.name)) {
                    total += x.name.len + 1;
                };
                if (a.size == 0) return .{ .size = @intCast(total) };
                if (total > a.size or total > c.window.len) return error.BufferTooSmall;
                var off: usize = 0;
                for (e.xattrs) |x| if (!isReservedXattr(x.name)) {
                    @memcpy(c.window[off..][0..x.name.len], x.name);
                    c.window[off + x.name.len] = 0;
                    off += x.name.len + 1;
                };
                return .{ .data = off };
            }
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(self.gpa, &kvs);
            const start = key.xattrKey(local, 0, 0);
            const end = key.xattrKey(local, key.HASH56_MAX, std.math.maxInt(u8));
            self.store.range(self.gpa, key.TREE_XATTRS, &start, &end, 4096, &kvs) catch |e| return storeErr(e);
            var total: usize = 0;
            for (kvs.items) |kv| {
                const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
                if (isReservedXattr(xv.name)) continue;
                total += xv.name.len + 1;
            }
            if (a.size == 0) return .{ .size = @intCast(total) };
            if (total > a.size or total > c.window.len) return error.BufferTooSmall;
            var off: usize = 0;
            for (kvs.items) |kv| {
                const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
                if (isReservedXattr(xv.name)) continue;
                @memcpy(c.window[off..][0..xv.name.len], xv.name);
                c.window[off + xv.name.len] = 0;
                off += xv.name.len + 1;
            }
            return .{ .data = off };
        }

        pub fn removexattr(self: *Self, c: *Ctx, a: filesystem.RemovexattrArgs) Error!void {
            try checkName(a.name);
            if (virtual_inodes.isVirtualIno(a.ino)) return error.PermissionDenied;
            try screenXattr(a.name, error.PermissionDenied);
            const local = try localOf(a.ino);
            if (self.foreignSlot(a.ino)) {
                const rep = try self.shipRecord(a.ino, .{ .removexattr = .{ .name = a.name } });
                if (rep.inode == null) return error.NoAttribute;
                return;
            }
            const t = try self.beginExclusive(local, @src());
            var t_owned = true;
            errdefer if (t_owned) self.store.abandon(t);
            var v = try self.readInode(local);
            var occ: [256]u8 = undefined;
            var n: usize = 0;
            const hit = (try self.findXattr(local, a.name, &occ, &n)) orelse return error.NoAttribute;
            self.gpa.free(hit.value);
            try t.stageDelete(key.TREE_XATTRS, &hit.k);
            v.ctime = now();
            try t.stagePut(key.TREE_INODES, &key.inodeKey(local), &v.encode());
            t_owned = false;
            try self.commit(c, t);
            _ = self.pending.retire(local);
        }
    };
}

// ---------------------------------------------------------------------------
// The volume-less data face: a set that formatted no `sqdata://` member.
// Every striped-arm entry is unreachable (`hasVolumes` gates them) and
// says so loud if ever reached.
// ---------------------------------------------------------------------------

pub const NoData = struct {
    /// The derived inline bound at one page (charter §6.1).
    inline_max: u64 = layout.INLINE_MAX_FLOOR,
    /// `lockLayout` acquisitions and the balance (test observables).
    layout_locks_taken: u32 = 0,
    layout_lock_held: u32 = 0,

    pub const OwnBlock = struct { mapping: Mapping, proof: layout.DmaProof };
    pub const Space = struct { total_bytes: u64, free_bytes: u64 };

    pub fn hasVolumes(_: *const NoData) bool {
        return false;
    }
    pub fn stagingOn(_: *const NoData) bool {
        return false;
    }
    pub fn isPassthrough(_: *const NoData) bool {
        return true;
    }
    pub fn blockSize(_: *const NoData) u64 {
        return layout.DEFAULT_CHUNK;
    }
    pub fn inlineMax(self: *const NoData) u64 {
        return self.inline_max;
    }
    pub fn geometry(_: *const NoData) layout.Geometry {
        return .default;
    }
    pub fn refsSeam(_: *NoData) layout.RefsSeam {
        return .{ .ctx = @ptrCast(@constCast(&none_ctx)), .stage_add = &noRef, .stage_release = &noRef };
    }
    const none_ctx: u8 = 0;
    fn noRef(_: *anyopaque, _: *tx.KvTx, _: layout.RefOp) Allocator.Error!void {}
    pub fn noteShape(_: *NoData, _: u64, _: u64, _: layout.Kind) void {}
    pub fn forgetShape(_: *NoData, _: u64) void {}
    pub fn canStoreHeld(_: *const NoData) bool {
        return false;
    }
    pub fn write(_: *NoData, _: Io, _: u64, _: u64, _: write_admit.Payload, _: op_guard.Class, _: seams.FileView, _: ?write_admit.ParkHook) write_admit.Error!write_admit.Outcome {
        unreachable; // gated by hasVolumes
    }
    pub fn read(_: *NoData, _: Io, _: u64, _: u64, _: []u8, _: bool, _: bool) Error!usize {
        unreachable; // every layout on this set is inline
    }
    pub fn ownBlock(_: *NoData, _: Io, _: u64, _: []const u8) Error!OwnBlock {
        unreachable;
    }
    pub fn abandonOwnBlock(_: *NoData, _: OwnBlock) void {}
    pub fn freeBindings(_: *NoData, _: Io, _: []const Mapping) void {}
    pub fn flushCustody(_: *NoData, _: Io, _: u64) void {}
    pub fn releaseIno(_: *NoData, _: Io, _: u64) void {}
    pub fn fsync(_: *NoData, _: Io, _: u64, _: ?staging.StagingKey) Error!void {}
    pub fn statfs(_: *NoData) ?Space {
        return null;
    }
    pub fn mintFileId(_: *NoData, _: u64) layout.FileId {
        return layout.zero_file_id;
    }
    pub fn stageImage(_: *NoData, _: staging.StagingKey, _: []const u8, _: staging.Slack) Error!u64 {
        return error.NotSupported;
    }
    pub fn appendStaged(_: *NoData, _: staging.StagingKey, _: u64, _: []const u8) Error!?u64 {
        return null;
    }
    pub fn releaseStaged(_: *NoData, _: staging.StagingKey, _: ?u64) void {}
    pub fn stagedLease(_: *NoData, _: staging.StagingKey) ?staging.Segment.Lease {
        return null;
    }
    pub fn readMappingImage(_: *NoData, _: Mapping, _: []u8) Error![]const u8 {
        return error.NotSupported;
    }
    /// The (3.7) layout lock of a plane-less set: nothing to exclude, but
    /// the acquisition is RECORDED (the rail sees (1) → (3.7) → (4a) on
    /// the truncate paths) and counted for the tests.
    pub const LayoutGuard = struct {
        owner: *NoData,

        pub fn release(self: LayoutGuard, _: Io) void {
            lock_order.released(.layout);
            self.owner.layout_lock_held -= 1;
        }
    };
    pub fn lockLayout(self: *NoData, _: Io, _: u64) LayoutGuard {
        lock_order.acquiring(.layout);
        self.layout_locks_taken += 1;
        self.layout_lock_held += 1;
        return .{ .owner = self };
    }
    pub fn stageKvmapShrink(_: *NoData, _: *tx.KvTx, _: u64, _: Layout, _: u8, _: u64) Error!u32 {
        unreachable; // no tree-mapped head exists on a volume-less set
    }
    pub fn stageKvmapCorpse(_: *NoData, _: *tx.KvTx, _: u64, _: Layout, _: u8) Error!void {
        unreachable;
    }
    pub fn drainKvmapSweep(_: *NoData, _: u64, _: u64) void {}
};

// ---------------------------------------------------------------------------
// Tests. The in-RAM store runs the POSIX mini-suite through the lifecycle
// on every host; the Linux KV backend runs it through the device path.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// An in-RAM store: a sorted `(kind, key) → value` table that applies a
/// transaction's records on commit with the fold algebra's semantics (Put
/// upserts, Delete removes, a Delta overwrites its masked fields on an
/// existing base and folds to nothing on an absent one).
pub const MemStore = struct {
    pub const StoreError = error{ OutOfMemory, Io };

    gpa: Allocator,
    lock: jc.SpinLock = .{},
    entries: std.ArrayList(Row) = .empty,
    /// Starts past the base so no test ino is identity-mapped between its
    /// local and global forms (`(local − 2) · W + 2`): a global/local mix-up
    /// in a handler is a loud ENOENT here, not a coincidence.
    next_ino: u64 = inos.LOCAL_INO_BASE + 3,
    seed: u64 = 0x5EED_0F_7E57,
    name_key: key.NameKey = .init(0x5EED_0F_7E57),
    node_size_: usize = 65536,
    commits: u64 = 0,
    fsyncs: u64 = 0,

    const Row = struct { kind: u8, k: []u8, v: []u8 };

    pub fn init(gpa: Allocator) Allocator.Error!MemStore {
        var self: MemStore = .{ .gpa = gpa };
        // The root directory, as `format` writes it.
        const root: InodeValue = .{ .mode = S.IFDIR | 0o755, .nlink = 2, .atime = 1, .mtime = 1, .ctime = 1 };
        try self.upsert(key.TREE_INODES, &key.inodeKey(inos.ROOT_INO), &root.encode());
        return self;
    }

    pub fn deinit(self: *MemStore) void {
        for (self.entries.items) |r| {
            self.gpa.free(r.k);
            self.gpa.free(r.v);
        }
        self.entries.deinit(self.gpa);
    }

    pub fn coreError(e: StoreError) errors.Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.InputOutput,
        };
    }

    fn order(kind: u8, k: []const u8, r: Row) std.math.Order {
        if (kind != r.kind) return std.math.order(kind, r.kind);
        return key.order(k, r.k);
    }

    /// Index of `(kind, k)` or where it would go.
    fn find(self: *MemStore, kind: u8, k: []const u8) struct { idx: usize, hit: bool } {
        var lo: usize = 0;
        var hi: usize = self.entries.items.len;
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            switch (order(kind, k, self.entries.items[mid])) {
                .lt => hi = mid,
                .gt => lo = mid + 1,
                .eq => return .{ .idx = mid, .hit = true },
            }
        }
        return .{ .idx = lo, .hit = false };
    }

    fn upsert(self: *MemStore, kind: u8, k: []const u8, v: []const u8) Allocator.Error!void {
        const f = self.find(kind, k);
        const vd = try self.gpa.dupe(u8, v);
        if (f.hit) {
            self.gpa.free(self.entries.items[f.idx].v);
            self.entries.items[f.idx].v = vd;
            return;
        }
        errdefer self.gpa.free(vd);
        const kd = try self.gpa.dupe(u8, k);
        errdefer self.gpa.free(kd);
        try self.entries.insert(self.gpa, f.idx, .{ .kind = kind, .k = kd, .v = vd });
    }

    fn remove(self: *MemStore, kind: u8, k: []const u8) void {
        const f = self.find(kind, k);
        if (!f.hit) return;
        const r = self.entries.orderedRemove(f.idx);
        self.gpa.free(r.k);
        self.gpa.free(r.v);
    }

    pub fn begin(self: *MemStore, site: tx.Site) Allocator.Error!*tx.KvTx {
        const t = try self.gpa.create(tx.KvTx);
        t.* = tx.KvTx.init(self.gpa, site);
        return t;
    }

    pub fn abandon(self: *MemStore, t: *tx.KvTx) void {
        t.deinit();
        self.gpa.destroy(t);
    }

    /// Apply, then drop the guards with the tx (the terminal outcome).
    pub fn commit(self: *MemStore, t: *tx.KvTx) StoreError!void {
        defer self.abandon(t);
        self.lock.lock();
        defer self.lock.unlock();
        for (t.staged.items) |s| {
            switch (s.kind) {
                .put => try self.upsert(s.tree_id, s.key, s.value),
                .delete => self.remove(s.tree_id, s.key),
                .delta => {
                    const f = self.find(s.tree_id, s.key);
                    if (!f.hit) continue;
                    var base = InodeValue.decode(self.entries.items[f.idx].v) catch return error.Io;
                    const d = record.InodeDelta.decode(s.value) catch return error.Io;
                    d.apply(&base);
                    const enc = base.encode();
                    try self.upsert(s.tree_id, s.key, &enc);
                },
            }
        }
        self.commits += 1;
    }

    pub fn lookup(self: *MemStore, gpa: Allocator, kind: u8, k: []const u8) StoreError!?[]u8 {
        self.lock.lock();
        defer self.lock.unlock();
        const f = self.find(kind, k);
        if (!f.hit) return null;
        return try gpa.dupe(u8, self.entries.items[f.idx].v);
    }

    pub fn range(self: *MemStore, gpa: Allocator, kind: u8, start: []const u8, end: []const u8, max: usize, out: *std.ArrayList(tree.KeyValue)) StoreError!void {
        self.lock.lock();
        defer self.lock.unlock();
        var i = self.find(kind, start).idx;
        var n: usize = 0;
        while (i < self.entries.items.len and n < max) : (i += 1) {
            const r = self.entries.items[i];
            if (r.kind != kind or key.lessThan(end, r.k)) break;
            try out.append(gpa, .{ .key = try gpa.dupe(u8, r.k), .value = try gpa.dupe(u8, r.v) });
            n += 1;
        }
    }

    pub fn fsync(self: *MemStore) StoreError!void {
        self.fsyncs += 1;
    }

    /// One store, one keyspace: the parent and the mint decision are the
    /// set facade's concerns; every mint is slot 0's.
    pub fn allocateIno(self: *MemStore, _: u64, _: bool, _: bool) StoreError!u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const i = self.next_ino;
        self.next_ino += 1;
        return i;
    }

    pub fn allocatedInos(self: *MemStore) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        return self.next_ino - inos.LOCAL_INO_BASE;
    }

    pub fn nameKey(self: *const MemStore) *const key.NameKey {
        return &self.name_key;
    }

    pub fn nodeSize(self: *const MemStore) usize {
        return self.node_size_;
    }

    pub const Space = struct { total_bytes: u64, free_bytes: u64 };

    pub fn space(_: *const MemStore) Space {
        return .{ .total_bytes = 64 << 20, .free_bytes = 48 << 20 };
    }

    pub fn isFailed(_: *const MemStore) bool {
        return false;
    }

    /// Records of one kind (a test's census).
    pub fn countKind(self: *MemStore, kind: u8) usize {
        var n: usize = 0;
        for (self.entries.items) |r| {
            if (r.kind == kind) n += 1;
        }
        return n;
    }
};

pub const MemFs = KvFs(MemStore, NoData);

test "the layout record is the v4 `layout.Layout`: an inline write publishes kind inline with the payload as the record; the data face of a volume-less set is one page inline" {
    var nd: NoData = .{};
    try testing.expectEqual(@as(u64, 4096), nd.inlineMax());
    try testing.expect(!nd.hasVolumes() and !nd.stagingOn());
    const data: [5]u8 = "hello".*;
    const l = Layout.inlineData(5, &data);
    const enc = try l.encodeAlloc(testing.allocator);
    defer testing.allocator.free(enc);
    const back = try Layout.decode(enc, .default);
    try testing.expectEqual(layout.Kind.inline_, back.kind);
    try testing.expectEqualStrings("hello", back.data);
}

test "a file carrying S_ISUID/S_ISGID is served with attr TTL 0 on GETATTR, LOOKUP, CREATE and SETATTR replies — the kernel caches no mode KILLPRIV_V2 is about to clear under it (the WRITE reply carries no attrs and the kernel invalidates none); a plain file keeps the mount's TTL (repro-port: fstests generic/355 — stat after a priv-killing direct write showed the cached -rwSr-Sr--)" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const ttl = h.fs.ttls.attr_ns;
    try testing.expect(ttl > 0);
    const plain = try h.create(1, "plain", 0o644);
    const g0 = h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, plain);
    try testing.expectEqual(ttl / std.time.ns_per_s, (abi.decodeExact(abi.fuse_attr_out, g0.outBytes()) orelse return error.TestBadReply).attr_valid);
    // CREATE of a setuid file: the entry's attr TTL is 0.
    const c = h.call(.{ .create = .{ .name = "suid", .flags = 0, .mode = 0o4755, .umask = 0, .open_flags = 0 } }, 1);
    const ce = abi.decodePrefix(abi.fuse_entry_out, c.outBytes()) orelse return error.TestBadReply;
    try testing.expectEqual(@as(u64, 0), ce.attr_valid);
    try testing.expectEqual(@as(u64, 0), ce.attr_valid_nsec);
    const suid = ce.nodeid;
    // GETATTR and LOOKUP: 0 as well.
    const g1 = h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, suid);
    const a1 = abi.decodeExact(abi.fuse_attr_out, g1.outBytes()) orelse return error.TestBadReply;
    try testing.expectEqual(@as(u64, 0), a1.attr_valid);
    try testing.expectEqual(@as(u64, 0), a1.attr_valid_nsec);
    const l1 = try h.entry(h.lookup(1, "suid"));
    try testing.expectEqual(@as(u64, 0), l1.attr_valid);
    // A setgid-only file (the group-exec sgid KILLPRIV clears) too.
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_MODE;
    sa.mode = 0o2755;
    const s1 = h.setattr(plain, sa);
    try testing.expectEqual(@as(u64, 0), (abi.decodeExact(abi.fuse_attr_out, s1.outBytes()) orelse return error.TestBadReply).attr_valid);
    // Once the bits are gone the mount's TTL returns.
    _ = h.write(suid, 0, "y", abi.FUSE_WRITE_KILL_SUIDGID);
    const g2 = h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, suid);
    const a2 = abi.decodeExact(abi.fuse_attr_out, g2.outBytes()) orelse return error.TestBadReply;
    try testing.expectEqual(@as(u32, S.IFREG | 0o755), a2.attr.mode);
    try testing.expectEqual(ttl / std.time.ns_per_s, a2.attr_valid);
}

test "KILLPRIV_V2 mode law: suid always; sgid iff group-exec" {
    try testing.expectEqual(@as(u32, 0o755), killSuidgidMode(0o4755));
    try testing.expectEqual(@as(u32, 0o755), killSuidgidMode(0o6755));
    try testing.expectEqual(@as(u32, 0o2745), killSuidgidMode(0o6745));
    try testing.expectEqual(@as(u32, 0o644), killSuidgidMode(0o644));
}

test "the must-implement set is bound; the rung-4 ops default to ENOSYS" {
    try testing.expect(filesystem.Filesystem.implementsMustSet(MemFs));
    try testing.expect(!@hasDecl(MemFs, "fallocate"));
    try testing.expect(!@hasDecl(MemFs, "access"));
}

// ---------------------------------------------------------------------------
// The POSIX mini-suite through `lifecycle.dispatch` over the loopback.
// ---------------------------------------------------------------------------

const lifecycle = @import("lifecycle.zig");
const seam = @import("transport_seam.zig");
const watchdog = @import("watchdog.zig");
const open_table = @import("open_table.zig");
const Loopback = seam.Loopback;
const LoopDispatcher = lifecycle.Dispatcher(Loopback);

/// One mounted `KvFs(MemStore, Data)` behind a dispatcher; every call is a
/// real `dispatch` on slot 0 with a fresh unique, and the captured reply
/// comes back. `Data` is the data face: `NoData` for the mini-suite, a
/// recording double for the paths whose contract is what they tell the
/// data plane.
fn HarnessOf(comptime Data: type) type {
    return struct {
        const Self = @This();
        pub const Fs = KvFs(MemStore, Data);

        store: MemStore,
        data: Data,
        fs: Fs,
        lb: Loopback,
        reg: watchdog.Registry,
        guards: op_guard.Table,
        opens: open_table.OpenTable,
        d: LoopDispatcher,
        unique: u64 = 2,

        const slot = Loopback.slot(0);
        const window_bytes = 8192;

        fn init(self: *Self) !void {
            self.store = try MemStore.init(testing.allocator);
            self.data = .{};
            self.fs = try Fs.mount(testing.allocator, testing.io, &self.store, &self.data, .{ .dlm_stripes = 64 });
            self.lb = try Loopback.init(testing.allocator, 1, window_bytes);
            self.reg = try watchdog.Registry.init(testing.allocator, 1);
            self.guards = try op_guard.Table.init(testing.allocator, 64);
            self.opens = try open_table.OpenTable.init(testing.allocator, 64);
            self.unique = 2;
            self.d = LoopDispatcher.init(&self.lb, .{ .fs = filesystem.Filesystem.of(Fs, &self.fs), .io = testing.io, .registry = &self.reg, .guards = &self.guards, .opens = &self.opens });
        }

        fn deinit(self: *Self) void {
            self.opens.deinit();
            self.guards.deinit();
            self.reg.deinit();
            self.lb.deinit();
            self.fs.unmount();
            self.store.deinit();
        }

        fn call(self: *Self, req: codec.Request, nodeid: u64) Loopback.Captured {
            const u = self.unique;
            self.unique += abi.FUSE_REQ_ID_STEP;
            const before = self.lb.replyCount(slot);
            self.d.dispatch(req, .{ .unique = u, .nodeid = nodeid, .uid = 1000, .gid = 1000, .pid = 7 }, slot, null);
            std.debug.assert(self.lb.replyCount(slot) == before + 1);
            const c = self.lb.captured(slot).?;
            std.debug.assert(c.header.unique == u);
            return c;
        }

        /// A no-reply op (FORGET/BATCH_FORGET/INTERRUPT).
        fn fire(self: *Self, req: codec.Request, nodeid: u64) void {
            const before = self.lb.replyCount(slot);
            self.d.dispatch(req, .{ .unique = self.unique, .nodeid = nodeid, .uid = 1000, .gid = 1000, .pid = 7 }, slot, null);
            self.unique += abi.FUSE_REQ_ID_STEP;
            std.debug.assert(self.lb.replyCount(slot) == before);
        }

        fn errnoOf(c: Loopback.Captured) ?linux.E {
            return c.errno();
        }

        fn entry(self: *Self, c: Loopback.Captured) !abi.fuse_entry_out {
            _ = self;
            if (c.errno()) |e| {
                std.debug.print("expected an entry, got errno {t}\n", .{e});
                return error.TestUnexpectedErrno;
            }
            return abi.decodeExact(abi.fuse_entry_out, c.outBytes()) orelse error.TestBadReply;
        }

        fn attr(self: *Self, c: Loopback.Captured) !abi.fuse_attr {
            _ = self;
            if (c.errno()) |e| {
                std.debug.print("expected attrs, got errno {t}\n", .{e});
                return error.TestUnexpectedErrno;
            }
            return (abi.decodeExact(abi.fuse_attr_out, c.outBytes()) orelse return error.TestBadReply).attr;
        }

        fn lookup(self: *Self, parent: u64, name: []const u8) Loopback.Captured {
            return self.call(.{ .lookup = .{ .name = name } }, parent);
        }

        fn mkdir(self: *Self, parent: u64, name: []const u8) !u64 {
            return (try self.entry(self.call(.{ .mkdir = .{ .name = name, .mode = 0o755, .umask = 0 } }, parent))).nodeid;
        }

        /// CREATE: the entry's ino (the handle is open until `release`).
        fn create(self: *Self, parent: u64, name: []const u8, mode: u32) !u64 {
            const c = self.call(.{ .create = .{ .name = name, .flags = 0, .mode = mode, .umask = 0, .open_flags = 0 } }, parent);
            if (c.errno()) |e| {
                std.debug.print("create: errno {t}\n", .{e});
                return error.TestUnexpectedErrno;
            }
            const e = abi.decodePrefix(abi.fuse_entry_out, c.outBytes()) orelse return error.TestBadReply;
            return e.nodeid;
        }

        fn write(self: *Self, ino: u64, offset: u64, data: []const u8, write_flags: u32) Loopback.Captured {
            const in: abi.fuse_write_in = .{ .fh = 1, .offset = offset, .size = @intCast(data.len), .write_flags = write_flags, .lock_owner = 0, .flags = 0, .padding = 0 };
            return self.call(.{ .write = .{ .in = in, .data = data } }, ino);
        }

        fn read(self: *Self, ino: u64, offset: u64, size: u32) Loopback.Captured {
            const in: abi.fuse_read_in = .{ .fh = 1, .offset = offset, .size = size, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
            return self.call(.{ .read = in }, ino);
        }

        fn getattr(self: *Self, ino: u64) !abi.fuse_attr {
            return self.attr(self.call(.{ .getattr = .{ .flags = 0, .fh = null } }, ino));
        }

        fn setattr(self: *Self, ino: u64, in: abi.fuse_setattr_in) Loopback.Captured {
            return self.call(.{ .setattr = in }, ino);
        }

        fn release(self: *Self, ino: u64) void {
            _ = self.call(.{ .release = .{ .fh = 1, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, ino);
        }

        fn forget(self: *Self, ino: u64, n: u64) void {
            self.fire(.{ .forget = .{ .nlookup = n } }, ino);
        }

        /// Every name a READDIR of `dir` yields from `offset`, joined by ','.
        fn readdirNames(self: *Self, dir: u64, offset: u64, buf: []u8) ![]const u8 {
            const in: abi.fuse_read_in = .{ .fh = 1, .offset = offset, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
            const c = self.call(.{ .readdir = in }, dir);
            if (c.errno()) |e| {
                std.debug.print("readdir: errno {t}\n", .{e});
                return error.TestUnexpectedErrno;
            }
            var w: std.Io.Writer = .fixed(buf);
            var off: usize = 0;
            var first = true;
            while (off + abi.fuse_dirent.NAME_OFFSET <= c.body.len) {
                const d = abi.decodePrefix(abi.fuse_dirent, c.body[off..]).?;
                const name = c.body[off + abi.fuse_dirent.NAME_OFFSET ..][0..d.namelen];
                if (!first) try w.writeByte(',');
                first = false;
                try w.writeAll(name);
                off += abi.fuse_dirent.recordLen(d.namelen);
            }
            return w.buffered();
        }

        /// The cookie READDIR stamped on entry `name` (the continuation offset).
        fn readdirCookie(self: *Self, dir: u64, name: []const u8) !u64 {
            const in: abi.fuse_read_in = .{ .fh = 1, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
            const c = self.call(.{ .readdir = in }, dir);
            var off: usize = 0;
            while (off + abi.fuse_dirent.NAME_OFFSET <= c.body.len) {
                const d = abi.decodePrefix(abi.fuse_dirent, c.body[off..]).?;
                const n = c.body[off + abi.fuse_dirent.NAME_OFFSET ..][0..d.namelen];
                if (std.mem.eql(u8, n, name)) return d.off;
                off += abi.fuse_dirent.recordLen(d.namelen);
            }
            return error.TestNameNotListed;
        }
    };
}

const Harness = HarnessOf(NoData);

/// A data face WITH volumes whose write side holds custody: `flushCustody`
/// lands it as the plane's upload merge does — the layout republished onto
/// a fresh block in one tx (`+ref` new, `−ref` displaced), the displaced
/// block freed by the flush itself. Every `freeBindings` and every staged
/// `±ref` is recorded: what the handler tells the data plane IS the
/// contract under test. Everything else answers as `NoData`.
const SettlingData = struct {
    base: NoData = .{},
    store: ?*MemStore = null,
    /// The custody armed to land at the next flush of `ino`: block 0 moves
    /// `from` → `to`.
    merge: ?struct { ino: u64, size: u64, from: Mapping, to: Mapping } = null,
    flushes: u32 = 0,
    /// The (3.7) balance observed inside the flush: a held lock there is
    /// the deadlock the plane's own merge would meet.
    locks_held_at_flush: u32 = 0,
    locks_taken_at_flush: u32 = 0,
    freed: std.ArrayList(Mapping) = .empty,
    adds: std.ArrayList(layout.RefOp) = .empty,
    releases: std.ArrayList(layout.RefOp) = .empty,
    /// The staging ring, when the test turns it on: the largest image
    /// staged (the ring holds whole files ≤ one block, never more); the
    /// ring FULL (no hole for the image, the pool at its cap): every stage
    /// refuses `NoSpaceLeft`.
    staging: bool = false,
    stage_refuse: bool = false,
    staged_max: u64 = 0,
    staged_images: u32 = 0,
    /// A REAL RAM ring behind the staged arm (the append law's test): the
    /// segment, its pool, and the image they live over.
    ring: ?*RamRing = null,

    /// The own-block promotions: the largest image DMA'd (≤ one block).
    own_block_max: u64 = 0,
    own_blocks: u32 = 0,
    next_block_off: u64 = 0,
    /// The striped data writes: `(offset, len, the file view's size)`.
    writes: std.ArrayList(struct { offset: u64, len: u64, view_size: u64 }) = .empty,
    /// The level-1 guard probe at the flush: `guards` + the FUSE nodeid
    /// whose guard the op under test must hold EXCLUSIVE while the custody
    /// settles; `shared_refused_at_flush` records a shared taker's verdict.
    guards: ?*op_guard.Table = null,
    guard_nodeid: u64 = 0,
    shared_refused_at_flush: ?bool = null,

    pub const RamRing = struct {
        image: []u8,
        seg: staging.Segment,
        pool: staging.Pool,

        fn init(cap: usize, pool_bytes: u64) !*RamRing {
            const r = try testing.allocator.create(RamRing);
            errdefer testing.allocator.destroy(r);
            r.image = try testing.allocator.alloc(u8, cap);
            errdefer testing.allocator.free(r.image);
            @memset(r.image, 0);
            r.seg = try staging.Segment.init(testing.allocator, r.image, .none);
            r.pool = staging.Pool.init(testing.allocator, pool_bytes, .none);
            return r;
        }

        fn deinit(self: *RamRing) void {
            self.pool.deinit();
            self.seg.deinit();
            testing.allocator.free(self.image);
            testing.allocator.destroy(self);
        }
    };

    pub const OwnBlock = NoData.OwnBlock;
    pub const Space = NoData.Space;
    pub const LayoutGuard = NoData.LayoutGuard;

    fn deinit(self: *SettlingData) void {
        self.freed.deinit(testing.allocator);
        self.adds.deinit(testing.allocator);
        self.releases.deinit(testing.allocator);
        self.writes.deinit(testing.allocator);
    }

    /// The striped layout `[0 → m]` of `ino` published through the store
    /// (the own-block shape a staged file promotes to).
    fn publishStriped(self: *SettlingData, ino: u64, size: u64, prev: ?Layout, m: Mapping) !void {
        const store = self.store.?;
        const entries = [_]layout.MapEntryWire{(layout.MapEntry{ .block = 0, .mapping = m }).encode()};
        const t = try store.begin(@src());
        errdefer store.abandon(t);
        const proof: layout.DmaProof = .{ .vol_tag = m.vol_tag, .offset = m.offset, .len = layout.DEFAULT_CHUNK, .completed_ns = 1 };
        try layout.publishLayout(t, .{ .ino = ino, .name_key = store.nameKey(), .prev = prev, .layout = Layout.stripedInline(size, &entries), .proofs = &.{proof} }, self.refsSeam(), .default);
        try store.commit(t);
    }

    pub fn hasVolumes(_: *const SettlingData) bool {
        return true;
    }
    pub fn stagingOn(self: *const SettlingData) bool {
        return self.staging;
    }
    pub fn isPassthrough(_: *const SettlingData) bool {
        return true;
    }
    pub fn blockSize(_: *const SettlingData) u64 {
        return layout.DEFAULT_CHUNK;
    }
    pub fn inlineMax(self: *const SettlingData) u64 {
        return self.base.inline_max;
    }
    pub fn geometry(_: *const SettlingData) layout.Geometry {
        return .default;
    }
    /// The plane's seam: the ledger record rides the tx (so the store's
    /// block-ref tree is the in-process C2/C8 oracle) and the op is noted.
    pub fn refsSeam(self: *SettlingData) layout.RefsSeam {
        return .{ .ctx = self, .stage_add = &recordAdd, .stage_release = &recordRelease };
    }
    fn refOf(op: layout.RefOp) block_refs.Reference {
        return .{ .vol_tag = op.vol_tag, .block_idx = op.block_idx, .owner_ino = op.owner_ino, .block_index = op.block_index };
    }
    fn recordAdd(ctx: *anyopaque, t: *tx.KvTx, op: layout.RefOp) Allocator.Error!void {
        const self: *SettlingData = @ptrCast(@alignCast(ctx));
        try block_refs.stageOps(t, &.{block_refs.taken(refOf(op))});
        try self.adds.append(testing.allocator, op);
    }
    fn recordRelease(ctx: *anyopaque, t: *tx.KvTx, op: layout.RefOp) Allocator.Error!void {
        const self: *SettlingData = @ptrCast(@alignCast(ctx));
        try block_refs.stageOps(t, &.{block_refs.released(refOf(op))});
        try self.releases.append(testing.allocator, op);
    }
    pub fn noteShape(_: *SettlingData, _: u64, _: u64, _: layout.Kind) void {}
    pub fn forgetShape(_: *SettlingData, _: u64) void {}
    pub fn canStoreHeld(_: *const SettlingData) bool {
        return false;
    }
    /// The striped data write: recorded, every byte acked.
    pub fn write(self: *SettlingData, _: Io, _: u64, offset: u64, payload: write_admit.Payload, _: op_guard.Class, view: seams.FileView, _: ?write_admit.ParkHook) write_admit.Error!write_admit.Outcome {
        const n = payload.len();
        self.writes.append(testing.allocator, .{ .offset = offset, .len = n, .view_size = view.size }) catch return error.OutOfMemory;
        return .{ .written = n, .size_claim = offset + n, .pieces = 1 };
    }
    pub fn read(_: *SettlingData, _: Io, _: u64, _: u64, _: []u8, _: bool, _: bool) Error!usize {
        unreachable;
    }
    /// The plane's own-block DMA: one block at most (the plane's own
    /// refusal), the image length recorded.
    pub fn ownBlock(self: *SettlingData, _: Io, _: u64, image: []const u8) Error!OwnBlock {
        if (image.len > layout.DEFAULT_CHUNK) return error.FileTooBig;
        self.own_blocks += 1;
        self.own_block_max = @max(self.own_block_max, image.len);
        const off = self.next_block_off;
        self.next_block_off += layout.DEFAULT_CHUNK;
        return .{ .mapping = Mapping.wholeBlock(0x11, off, 1), .proof = .{ .vol_tag = 0x11, .offset = off, .len = layout.DEFAULT_CHUNK, .completed_ns = 1 } };
    }
    pub fn abandonOwnBlock(_: *SettlingData, _: OwnBlock) void {}
    pub fn freeBindings(self: *SettlingData, _: Io, maps: []const Mapping) void {
        for (maps) |m| self.freed.append(testing.allocator, m) catch unreachable;
    }
    /// The custody lands: the plane's merge under its own (3.5)+(3.7).
    pub fn flushCustody(self: *SettlingData, io: Io, ino: u64) void {
        self.flushes += 1;
        self.locks_held_at_flush = self.base.layout_lock_held;
        self.locks_taken_at_flush = self.base.layout_locks_taken;
        if (self.guards) |g| {
            if (g.tryAcquire(io, self.guard_nodeid, .shared)) |held| {
                held.release(io);
                self.shared_refused_at_flush = false;
            } else self.shared_refused_at_flush = true;
        }
        const m = self.merge orelse return;
        if (m.ino != ino) return;
        self.merge = null;
        const prev_entries = [_]layout.MapEntryWire{(layout.MapEntry{ .block = 0, .mapping = m.from }).encode()};
        self.publishStriped(ino, m.size, Layout.stripedInline(m.size, &prev_entries), m.to) catch unreachable;
        self.freeBindings(io, &.{m.from});
    }
    pub fn releaseIno(_: *SettlingData, _: Io, _: u64) void {}
    pub fn fsync(_: *SettlingData, _: Io, _: u64, _: ?staging.StagingKey) Error!void {}
    pub fn statfs(_: *SettlingData) ?Space {
        return null;
    }
    pub fn mintFileId(_: *SettlingData, _: u64) layout.FileId {
        return layout.zero_file_id;
    }
    pub fn stageImage(self: *SettlingData, k: staging.StagingKey, image: []const u8, slack: staging.Slack) Error!u64 {
        if (!self.staging) return error.NotSupported;
        if (self.stage_refuse) return error.NoSpaceLeft;
        self.staged_images += 1;
        self.staged_max = @max(self.staged_max, image.len);
        if (self.ring) |r| {
            const s = staging.stageWrite(&r.pool, &r.seg, k, image, 1, slack) catch |e| return switch (e) {
                error.StorageFull => error.NoSpaceLeft,
                error.OutOfMemory => error.OutOfMemory,
            };
            return s.generation;
        }
        return self.staged_images;
    }
    /// In place through the RAM ring when the test mounted one; without a
    /// ring every staged write re-stages the whole image.
    pub fn appendStaged(self: *SettlingData, k: staging.StagingKey, at: u64, data: []const u8) Error!?u64 {
        const r = self.ring orelse return null;
        const s = (staging.appendStaged(&r.pool, &r.seg, k, at, data, 1) catch return error.OutOfMemory) orelse return null;
        return s.generation;
    }
    pub fn releaseStaged(self: *SettlingData, k: staging.StagingKey, generation: ?u64) void {
        const r = self.ring orelse return;
        if (generation) |g| {
            _ = staging.releaseStaged(&r.pool, &r.seg, k, g);
        } else if (r.seg.remove(k, null)) _ = r.pool.credit(k.file_id, null);
    }
    pub fn stagedLease(self: *SettlingData, k: staging.StagingKey) ?staging.Segment.Lease {
        const r = self.ring orelse return null;
        return r.seg.get(k);
    }
    pub fn readMappingImage(_: *SettlingData, _: Mapping, _: []u8) Error![]const u8 {
        return error.NotSupported;
    }
    pub fn lockLayout(self: *SettlingData, io: Io, ino: u64) LayoutGuard {
        return self.base.lockLayout(io, ino);
    }
    pub fn stageKvmapShrink(_: *SettlingData, _: *tx.KvTx, _: u64, _: Layout, _: u8, _: u64) Error!u32 {
        unreachable;
    }
    pub fn stageKvmapCorpse(_: *SettlingData, _: *tx.KvTx, _: u64, _: Layout, _: u8) Error!void {
        unreachable;
    }
    pub fn drainKvmapSweep(_: *SettlingData, _: u64, _: u64) void {}
};

test "the corpse destroy settles the write-side custody BEFORE it reads the layout it frees, under (3.7): the flush's merge moved the file onto a fresh block — the plan frees the fresh block once and the displaced one never again (repro-port: fstests generic/013, fsstress write+truncate+unlink — fsck C2/C8 `durable 1 vs derived 0`, block_untracked_free_refusals)" {
    const H = HarnessOf(SettlingData);
    var h: H = undefined;
    try h.init();
    defer h.deinit();
    defer h.data.deinit();
    h.data.store = &h.store;
    const f = try h.create(1, "f", 0o644);
    const lf = try H.Fs.localOf(f);
    const size: u64 = 1 << 20;
    const from = Mapping.wholeBlock(0x11, 0, 1);
    const to = Mapping.wholeBlock(0x11, layout.DEFAULT_CHUNK, 2);
    // The own-block shape: striped over `from`; the write side holds
    // custody of block 0 that lands as `to` when the destroy settles it.
    try h.data.publishStriped(lf, size, null, from);
    h.data.merge = .{ .ino = lf, .size = size, .from = from, .to = to };
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "f" } }, 1).errno());
    h.release(f);
    h.forget(f, 1);
    try testing.expectEqual(@as(usize, 1), h.fs.reclaim.items.len);
    var tr = @import("op_trace.zig").OpTrace.begin(1, .other);
    var win: [16]u8 = undefined;
    var c: Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 0, .io = testing.io, .trace = &tr, .wd_slot = null, .window = &win };
    try h.fs.reclaimDrain(&c);
    tr.finish();
    // Frees: `from` once (the flush's merge), `to` once (the destroy).
    try testing.expectEqual(@as(usize, 2), h.data.freed.items.len);
    try testing.expect(h.data.freed.items[0].eql(from));
    try testing.expect(h.data.freed.items[1].eql(to));
    // The durable ledger: `+from` (the publish), `+to −from` (the merge)
    // through the seam; the destroy rider's `−to` rides the handler's own
    // tx — the block-ref tree ends EMPTY (the fsck C2/C8 oracle in-process:
    // a `+to` left behind is "durable 1 vs derived 0").
    try testing.expectEqual(@as(usize, 2), h.data.adds.items.len);
    try testing.expectEqual(@as(usize, 1), h.data.releases.items.len);
    try testing.expectEqual(from.blockIdx(layout.DEFAULT_CHUNK), h.data.releases.items[0].block_idx);
    try testing.expectEqual(@as(usize, 0), h.store.countKind(key.TREE_BLOCK_REFS));
    try testing.expectEqual(@as(usize, 0), h.store.countKind(key.TREE_XATTRS));
    // The custody settled exactly once, with no (3.7) held (the plane's
    // merge takes it) and before the destroy's own acquisition.
    try testing.expectEqual(@as(u32, 1), h.data.flushes);
    try testing.expectEqual(@as(u32, 0), h.data.locks_held_at_flush);
    try testing.expectEqual(@as(u32, 0), h.data.locks_taken_at_flush);
    try testing.expectEqual(@as(u32, 1), h.data.base.layout_locks_taken);
    try testing.expectEqual(@as(u32, 0), h.data.base.layout_lock_held);
    try testing.expectEqual(linux.E.NOENT, h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, f).errno().?);
}

test "a file grown past one block by truncate is written through the striped arm: the promotion DMAs at most one block of the resident image (the tail is the sparse zero run the size names) and the staged arm never rings an image above one block (repro-port: fstests generic/030 — `truncate 5017k` then `pwrite 0 5017k` answered EFBIG; fsstress left unpromotable ring images behind)" {
    const H = HarnessOf(SettlingData);
    var h: H = undefined;
    try h.init();
    defer h.deinit();
    defer h.data.deinit();
    h.data.store = &h.store;
    const bs = layout.DEFAULT_CHUNK;
    const grown: u64 = 5017 * 1024;
    // (a) An empty (inline, no record) file grown past the block.
    const f = try h.create(1, "f", 0o644);
    var in = abi.zeroed(abi.fuse_setattr_in);
    in.valid = abi.FATTR_SIZE;
    in.size = grown;
    try testing.expectEqual(@as(?linux.E, null), h.setattr(f, in).errno());
    try testing.expectEqual(grown, (try h.getattr(f)).size);
    const two_pages: [8192]u8 = @splat('X');
    const w = h.write(f, 0, &two_pages, 0);
    try testing.expectEqual(@as(?linux.E, null), w.errno());
    try testing.expectEqual(@as(u32, 1), h.data.own_blocks);
    try testing.expectEqual(bs, h.data.own_block_max);
    try testing.expectEqual(@as(usize, 1), h.data.writes.items.len);
    try testing.expectEqual(grown, h.data.writes.items[0].view_size);
    try testing.expectEqual(grown, (try h.getattr(f)).size);
    const rec = (try h.fs.readLayout(try H.Fs.localOf(f))).?;
    defer h.fs.freeLayout(rec);
    try testing.expectEqual(layout.Kind.striped, rec.layout.kind);
    try testing.expectEqual(grown, rec.layout.size);
    h.release(f);
    // (b) A staged file (the ring on) grown past the block: the next write
    // leaves the ring for the striped arm instead of ringing 5 MiB.
    h.data.staging = true;
    const g = try h.create(1, "g", 0o644);
    try testing.expectEqual(@as(?linux.E, null), h.write(g, 0, &two_pages, 0).errno());
    try testing.expectEqual(@as(u32, 1), h.data.staged_images);
    try testing.expectEqual(@as(?linux.E, null), h.setattr(g, in).errno());
    try testing.expectEqual(@as(?linux.E, null), h.write(g, 4096, "tail", 0).errno());
    try testing.expectEqual(@as(u32, 1), h.data.staged_images);
    try testing.expect(h.data.staged_max <= bs);
    try testing.expectEqual(@as(u32, 2), h.data.own_blocks);
    try testing.expectEqual(bs, h.data.own_block_max);
    try testing.expectEqual(grown, (try h.getattr(g)).size);
    h.release(g);
}

// Rung 9 (the external suites' fsstress rounds): a 20-writer burst of 3–4 MiB
// files outran the ring's promotion — `staging ring full: a 3.5 MB record
// found no hole in a 343 MB segment with 155 live entries` — and the
// application saw ENOSPC on a volume with 1,700 free blocks. The ring is a
// cache, never the set's capacity: an image the ring cannot hold takes the
// OWN-BLOCK arm (the resident image promoted durably, the write admitted
// striped); ENOSPC stays the data plane's own terminal verdict.
test "the staged arm when the ring is FULL: the write takes the own-block arm instead of answering ENOSPC — the resident image lands on its own block, the layout flips to striped, the piece is admitted; a fresh file likewise; the refusal stays the plane's (repro-port: fsstress 20-writer burst, `staging ring full … refusing StorageFull`)" {
    const H = HarnessOf(SettlingData);
    var h: H = undefined;
    try h.init();
    defer h.deinit();
    defer h.data.deinit();
    h.data.store = &h.store;
    h.data.staging = true;
    const bs = layout.DEFAULT_CHUNK;
    const own_block0 = fuse_write_staged_ring_full_own_block.load();
    // (a) A staged resident of 1 MiB; then the ring fills (every stage
    // refuses): the next write — a 2.5 MiB tail that would re-stage a
    // 3.5 MiB image — promotes the resident to its own block and goes
    // striped. Nothing is answered ENOSPC; the image rang nothing.
    const f = try h.create(1, "f", 0o644);
    const one_mib: []u8 = try testing.allocator.alloc(u8, 1 << 20);
    defer testing.allocator.free(one_mib);
    @memset(one_mib, 'A');
    try testing.expectEqual(@as(?linux.E, null), h.write(f, 0, one_mib, 0).errno());
    try testing.expectEqual(@as(u32, 1), h.data.staged_images);
    h.data.stage_refuse = true;
    const tail: []u8 = try testing.allocator.alloc(u8, (5 << 20) / 2);
    defer testing.allocator.free(tail);
    @memset(tail, 'B');
    try testing.expectEqual(@as(?linux.E, null), h.write(f, 1 << 20, tail, 0).errno());
    try testing.expectEqual(@as(u32, 1), h.data.staged_images); // the ring saw no second image
    try testing.expectEqual(@as(u32, 1), h.data.own_blocks); // the 1 MiB resident promoted
    try testing.expectEqual(@as(u64, 1 << 20), h.data.own_block_max);
    try testing.expectEqual(@as(usize, 1), h.data.writes.items.len); // the tail admitted striped
    try testing.expectEqual(@as(u64, 1 << 20), h.data.writes.items[0].offset);
    try testing.expectEqual(@as(u64, (5 << 20) / 2), h.data.writes.items[0].len);
    try testing.expectEqual(own_block0 + 1, fuse_write_staged_ring_full_own_block.load());
    try testing.expectEqual(@as(u64, (7 << 20) / 2), (try h.getattr(f)).size);
    {
        const rec = (try h.fs.readLayout(try H.Fs.localOf(f))).?;
        defer h.fs.freeLayout(rec);
        try testing.expectEqual(layout.Kind.striped, rec.layout.kind);
    }
    h.release(f);
    // (b) A fresh file's first write while the ring is full: no resident to
    // promote (zero own-block DMAs), the write admitted striped at once.
    const g = try h.create(1, "g", 0o644);
    try testing.expectEqual(@as(?linux.E, null), h.write(g, 0, one_mib, 0).errno());
    try testing.expectEqual(@as(u32, 1), h.data.own_blocks);
    try testing.expectEqual(@as(usize, 2), h.data.writes.items.len);
    try testing.expectEqual(own_block0 + 2, fuse_write_staged_ring_full_own_block.load());
    try testing.expect(h.data.staged_max <= bs);
    h.release(g);
    // (c) The ring heals: a small file stages again.
    h.data.stage_refuse = false;
    const k = try h.create(1, "k", 0o644);
    try testing.expectEqual(@as(?linux.E, null), h.write(k, 0, one_mib[0..8192], 0).errno());
    try testing.expectEqual(@as(u32, 2), h.data.staged_images);
    h.release(k);
}

test "the staged append law (repro-port: fstests generic/069 `append_writer` — 4-byte O_APPEND writes re-staged a 1–2 MiB image each, ≈ 3 ms and 2–4 MiB moved per append on c36b531): M small appends to a staged file land IN PLACE in the record's slack — ring bytes moved ≤ 11 × bytes appended (§J.2), ONE commit per append carrying a Δ(size, mtime, ctime), the image byte-exact, a re-stage only when the slack is exhausted" {
    const H = HarnessOf(SettlingData);
    var h: H = undefined;
    try h.init();
    defer h.deinit();
    defer h.data.deinit();
    h.data.store = &h.store;
    h.data.staging = true;
    h.data.ring = try SettlingData.RamRing.init(64 << 20, 32 << 20);
    defer h.data.ring.?.deinit();
    const appends0 = staging.stats.staging_appends.load();
    const abytes0 = staging.stats.staging_append_bytes.load();
    const restages0 = staging.stats.staging_restages.load();
    const rbytes0 = staging.stats.staging_restage_bytes.load();
    // The file crosses the inline bound with its first write: staged from then on.
    const f = try h.create(1, "log", 0o644);
    const head: []u8 = try testing.allocator.alloc(u8, 6000);
    defer testing.allocator.free(head);
    @memset(head, 'H');
    try testing.expectEqual(@as(?linux.E, null), h.write(f, 0, head, 0).errno());
    const commits0 = h.store.commits;
    // M 4-byte appends at EOF (the integers append_writer writes).
    const m: u32 = 3000;
    var size: u64 = head.len;
    var i: u32 = 0;
    while (i < m) : (i += 1) {
        const word = std.mem.toBytes(std.mem.nativeToLittle(u32, i));
        try testing.expectEqual(@as(?linux.E, null), h.write(f, size, &word, 0).errno());
        size += 4;
    }
    const appended: u64 = 4 * m;
    const appends = staging.stats.staging_appends.load() - appends0;
    const restages = staging.stats.staging_restages.load() - restages0;
    const moved = (staging.stats.staging_append_bytes.load() - abytes0) + (staging.stats.staging_restage_bytes.load() - rbytes0);
    // The law: moved ≤ 11 × appended; nearly every append in place (the
    // re-stages are the slack exhaustions of the geometric chain).
    try testing.expectEqual(@as(u64, m), appends + restages);
    try testing.expect(restages <= 8);
    try testing.expect(moved <= staging.STAGED_APPEND_MOVED_BOUND * appended);
    // ONE commit per append (the Δ record; the conveyor is the RAM store here).
    try testing.expectEqual(@as(u64, m), h.store.commits - commits0);
    try testing.expectEqual(size, (try h.getattr(f)).size);
    // The ring holds ONE record of the file (the image the reads serve),
    // byte-exact: the head, then every integer in order; its layout names
    // the size.
    const rec = (try h.fs.readLayout(try H.Fs.localOf(f))).?;
    defer h.fs.freeLayout(rec);
    try testing.expectEqual(layout.Kind.staged, rec.layout.kind);
    try testing.expectEqual(size, rec.layout.size);
    const lease = h.data.ring.?.seg.get(staging.StagingKey.init(rec.layout.file_id, try H.Fs.localOf(f))).?;
    defer lease.release();
    const img = lease.bytes();
    try testing.expectEqual(@as(usize, size), img.len);
    try testing.expect(std.mem.allEqual(u8, img[0..head.len], 'H'));
    i = 0;
    while (i < m) : (i += 1) try testing.expectEqual(i, std.mem.readInt(u32, img[head.len + 4 * i ..][0..4], .little));
    try testing.expectEqual(@as(usize, 1), h.data.ring.?.seg.live());
    h.release(f);
}

test "§L.6 the write-stamp park (repro-port: frag-512b-write — 533 k size-neutral 512 B overwrites cost 533 k journal entries, 1.56 per 4 KiB window, 2.6 KB of metadata device per op): a size-neutral striped write commits NOTHING — its (mtime, ctime) is parked, fold-visible on getattr while the stored record lags; the drain commits ONE Δ entry per ≤ 16 parked inos and a retire never drops a stamp parked after it; fsync(ino) drains that ino ahead of its barrier; an extending write still commits its Δ(size, mtime, ctime) (§J.2); the cap wakes the cadence; strict mode commits per write" {
    const H = HarnessOf(SettlingData);
    var h: H = undefined;
    try h.init();
    defer h.deinit();
    defer h.data.deinit();
    h.data.store = &h.store;
    const image: []u8 = try testing.allocator.alloc(u8, 8192);
    defer testing.allocator.free(image);
    @memset(image, 'I');
    // Four files past the inline bound: striped from their first write.
    var files: [4]u64 = undefined;
    var names: [4][2]u8 = undefined;
    for (&files, &names, 0..) |*f, *nm, i| {
        nm.* = .{ 'f', @intCast('0' + i) };
        f.* = try h.create(1, nm, 0o644);
        try testing.expectEqual(@as(?linux.E, null), h.write(f.*, 0, image, 0).errno());
    }
    const commits0 = h.store.commits;
    const parked0 = fuse_write_times_parked.load();
    const drains0 = economy.stats.meta_kv_times_echo_drain_commits.load();
    const drained0 = economy.stats.meta_kv_times_echo_drained.load();
    const stored_before = (try h.fs.readInodeStored(try H.Fs.localOf(files[0]))).?;
    // The stamps are the COARSE clock's (`now`): a stamp inside the tick
    // of the last committed one advances nothing (the drain then stages
    // nothing, rightly), so the overwrites below start on a later tick.
    awaitCoarseTickAfter(stored_before.mtime);
    // 100 size-neutral 512 B overwrites per file: zero commits, every one parked.
    var sector: [512]u8 = @splat('s');
    var i: u32 = 0;
    while (i < 100) : (i += 1) {
        for (files) |f| try testing.expectEqual(@as(?linux.E, null), h.write(f, @as(u64, (i * 37) % 15) * 512, &sector, 0).errno());
    }
    try testing.expectEqual(commits0, h.store.commits);
    try testing.expectEqual(parked0 + 400, fuse_write_times_parked.load());
    try testing.expectEqual(@as(u64, 4), h.fs.pending.len());
    // Fold-visible: the served mtime advanced, the stored record did not.
    const served = try h.getattr(files[0]);
    const served_mtime_ns = served.mtime * std.time.ns_per_s + served.mtimensec;
    try testing.expect(economy.laterThan(served_mtime_ns, stored_before.mtime));
    try testing.expectEqual(stored_before.mtime, (try h.fs.readInodeStored(try H.Fs.localOf(files[0]))).?.mtime);
    // The drain: ONE entry carrying the four Δs; the stored record now
    // carries exactly the served stamp; nothing stays parked.
    var trace = op_trace.OpTrace.begin(0, .other);
    defer trace.finish();
    var ctx: filesystem.Ctx = .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = testing.io, .trace = &trace, .wd_slot = null, .window = &.{} };
    try testing.expectEqual(@as(u64, 4), try h.fs.drainPendingTimes(&ctx));
    try testing.expectEqual(commits0 + 1, h.store.commits);
    try testing.expectEqual(drains0 + 1, economy.stats.meta_kv_times_echo_drain_commits.load());
    try testing.expectEqual(drained0 + 4, economy.stats.meta_kv_times_echo_drained.load());
    try testing.expectEqual(@as(u64, 0), h.fs.pending.len());
    try testing.expectEqual(served_mtime_ns, (try h.fs.readInodeStored(try H.Fs.localOf(files[0]))).?.mtime);
    // A drain with nothing parked commits nothing.
    try testing.expectEqual(@as(u64, 0), try h.fs.drainPendingTimes(&ctx));
    try testing.expectEqual(commits0 + 1, h.store.commits);
    // The conditional retire: a stamp parked AFTER the drained one stands.
    const l0 = try H.Fs.localOf(files[0]);
    const older: economy.Times = .{ .mtime = served_mtime_ns, .ctime = served_mtime_ns };
    awaitCoarseTickAfter(served_mtime_ns);
    try testing.expectEqual(@as(?linux.E, null), h.write(files[0], 0, &sector, 0).errno());
    try testing.expect(!h.fs.pending.retireIfStill(l0, older));
    try testing.expectEqual(@as(u64, 1), h.fs.pending.len());
    // fsync(ino) drains that ino alone: one commit, the stamp durable.
    try testing.expectEqual(@as(?linux.E, null), h.write(files[1], 0, &sector, 0).errno());
    try testing.expectEqual(@as(u64, 2), h.fs.pending.len());
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .fsync = .{ .fh = 1, .datasync = false } }, files[0]).errno());
    try testing.expectEqual(commits0 + 2, h.store.commits);
    try testing.expectEqual(@as(u64, 1), h.fs.pending.len());
    try testing.expect(h.fs.pending.parked(l0) == null);
    const a0 = try h.getattr(files[0]);
    try testing.expectEqual(a0.mtime * std.time.ns_per_s + a0.mtimensec, (try h.fs.readInodeStored(l0)).?.mtime);
    // An fsync with nothing parked commits nothing.
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .fsync = .{ .fh = 1, .datasync = false } }, files[0]).errno());
    try testing.expectEqual(commits0 + 2, h.store.commits);
    // An extending write commits its Δ(size, mtime, ctime) per op (§J.2)
    // and retires the ino's parked stamp (the Δ carries a newer one).
    try testing.expectEqual(@as(?linux.E, null), h.write(files[1], 8192, &sector, 0).errno());
    try testing.expectEqual(commits0 + 3, h.store.commits);
    try testing.expectEqual(@as(u64, 0), h.fs.pending.len());
    try testing.expectEqual(@as(u64, 8192 + 512), (try h.getattr(files[1])).size);
    // Strict mode: the size-neutral write commits as before.
    h.fs.times_park = false;
    try testing.expectEqual(@as(?linux.E, null), h.write(files[2], 0, &sector, 0).errno());
    try testing.expectEqual(commits0 + 4, h.store.commits);
    try testing.expectEqual(parked0 + 402, fuse_write_times_parked.load());
    h.fs.times_park = true;
    // The cap: the park that crosses PENDING_TIMES_DRAIN_CAP wakes the
    // cadence (a permit stored for its next registration).
    var wake = h.fs.times_drain_wake.notified();
    try testing.expect(!wake.enable(testing.io));
    wake.cancel(testing.io);
    const cap: usize = @intCast(economy.PENDING_TIMES_DRAIN_CAP);
    const crowd = try testing.allocator.alloc(u64, cap);
    defer testing.allocator.free(crowd);
    var name_buf: [8]u8 = undefined;
    for (crowd, 0..) |*f, n| {
        f.* = try h.create(1, try std.fmt.bufPrint(&name_buf, "c{d}", .{n}), 0o644);
        try testing.expectEqual(@as(?linux.E, null), h.write(f.*, 0, image, 0).errno());
    }
    awaitCoarseTickAfter(@bitCast(clock.realtimeCoarseNs()));
    for (crowd) |f| try testing.expectEqual(@as(?linux.E, null), h.write(f, 512, &sector, 0).errno());
    try testing.expectEqual(economy.PENDING_TIMES_DRAIN_CAP, h.fs.pending.len());
    wake = h.fs.times_drain_wake.notified();
    try testing.expect(wake.enable(testing.io));
    wake.waitUncancelable(testing.io);
    // The drain over the cap's population: ⌈512 / 16⌉ entries.
    const commits_cap = h.store.commits;
    try testing.expectEqual(economy.PENDING_TIMES_DRAIN_CAP, try h.fs.drainPendingTimes(&ctx));
    try testing.expectEqual(commits_cap + (economy.PENDING_TIMES_DRAIN_CAP + dlm.max_requests - 1) / dlm.max_requests, h.store.commits);
    try testing.expectEqual(@as(u64, 0), h.fs.pending.len());
    for (crowd) |f| h.release(f);
    for (files) |f| h.release(f);
}

test "the handover's step 0 (rung 10 item 4): drainPendingTimesOfSlot commits the parked stamps of ONE routing slot in one Δ entry — the stored records of that slot carry their served mtimes, another slot's stamp stays parked for the cadence, a slot with nothing to refine commits nothing; the binding is the plane's TimesDrain seam" {
    const H = HarnessOf(SettlingData);
    var h: H = undefined;
    try h.init();
    defer h.deinit();
    defer h.data.deinit();
    h.data.store = &h.store;
    const image: []u8 = try testing.allocator.alloc(u8, 8192);
    defer testing.allocator.free(image);
    @memset(image, 'I');
    // Four striped files (the RAM store mints every ino on slot 0).
    var files: [4]u64 = undefined;
    var names: [4][2]u8 = undefined;
    for (&files, &names, 0..) |*f, *nm, i| {
        nm.* = .{ 'h', @intCast('0' + i) };
        f.* = try h.create(1, nm, 0o644);
        try testing.expectEqual(@as(?linux.E, null), h.write(f.*, 0, image, 0).errno());
        try testing.expectEqual(@as(u16, 0), route_table.slotOf(f.*));
    }
    // The overwrites start on a tick past EVERY stored stamp (the four
    // first writes may straddle a tick): the last-written file's is the max.
    const stored_before = (try h.fs.readInodeStored(try H.Fs.localOf(files[3]))).?;
    awaitCoarseTickAfter(stored_before.mtime);
    var sector: [512]u8 = @splat('s');
    for (files) |f| try testing.expectEqual(@as(?linux.E, null), h.write(f, 0, &sector, 0).errno());
    // A stamp parked on another slot's keyspace (guest slot 5): the drain
    // of slot 0 must leave it.
    const foreign = key.guestLocalIno(5, 77);
    const stamp: u64 = @bitCast(clock.realtimeCoarseNs());
    try testing.expect(try h.fs.pending.park(foreign, stamp, stamp) == .parked);
    try testing.expectEqual(@as(u64, 5), h.fs.pending.len());
    const commits0 = h.store.commits;
    var trace = op_trace.OpTrace.begin(0, .other);
    defer trace.finish();
    var ctx: filesystem.Ctx = .{ .unique = 0, .uid = 0, .gid = 0, .pid = 0, .io = testing.io, .trace = &trace, .wd_slot = null, .window = &.{} };
    // Slot 0: ONE commit carrying the four Δs; the foreign stamp stays.
    const served = try h.getattr(files[1]);
    try testing.expectEqual(@as(u64, 4), try h.fs.drainPendingTimesOfSlot(&ctx, 0));
    try testing.expectEqual(commits0 + 1, h.store.commits);
    try testing.expectEqual(@as(u64, 1), h.fs.pending.len());
    try testing.expect(h.fs.pending.parked(foreign) != null);
    const l1 = try H.Fs.localOf(files[1]);
    try testing.expect(h.fs.pending.parked(l1) == null);
    try testing.expectEqual(served.mtime * std.time.ns_per_s + served.mtimensec, (try h.fs.readInodeStored(l1)).?.mtime);
    // Slot 0 again: nothing parked, no commit.
    try testing.expectEqual(@as(u64, 0), try h.fs.drainPendingTimesOfSlot(&ctx, 0));
    try testing.expectEqual(commits0 + 1, h.store.commits);
    // Slot 5 through the seam the plane binds (its own op context): the
    // stamp names no stored record — nothing to refine, no commit, the
    // park retired.
    try testing.expectEqual(@as(u64, 0), H.Fs.timesDrainSeam(&h.fs, 5));
    try testing.expectEqual(commits0 + 1, h.store.commits);
    try testing.expectEqual(@as(u64, 0), h.fs.pending.len());
    // The seam over a slot with work: the count it answers is the drain's.
    awaitCoarseTickAfter(served.mtime * std.time.ns_per_s + served.mtimensec);
    try testing.expectEqual(@as(?linux.E, null), h.write(files[2], 0, &sector, 0).errno());
    try testing.expectEqual(@as(u64, 1), H.Fs.timesDrainSeam(&h.fs, 0));
    try testing.expectEqual(commits0 + 2, h.store.commits);
    for (files) |f| h.release(f);
}

/// The coarse clock (`now`) past `stamp`: the stamps a test compares must
/// sit on different ticks (4 ms on the venue; the macOS clock is fine).
fn awaitCoarseTickAfter(stamp: u64) void {
    while (!economy.laterThan(@bitCast(clock.realtimeCoarseNs()), stamp)) std.atomic.spinLoopHint();
}

test "POSIX: create → write → read → stat; an overlay write grows the file; a write past the inline bound on a volume-less set is EFBIG and counted; truncate clips, empties at 0 and zero-extends" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o644);
    try testing.expect(f >= 2);
    try testing.expectEqual(@as(u32, 1), h.opens.get(f).?.count);
    const a0 = try h.getattr(f);
    try testing.expectEqual(@as(u64, 0), a0.size);
    try testing.expectEqual(@as(u32, S.IFREG | 0o644), a0.mode);
    try testing.expectEqual(@as(u32, 1), a0.nlink);
    try testing.expectEqual(@as(u32, 1000), a0.uid);
    var payload: [100]u8 = undefined;
    for (&payload, 0..) |*b, i| b.* = @intCast(i);
    const w = h.write(f, 0, &payload, 0);
    try testing.expectEqual(@as(u32, 100), (abi.decodeExact(abi.fuse_write_out, w.outBytes()) orelse return error.TestBadReply).size);
    const r = h.read(f, 0, 4096);
    try testing.expectEqualSlices(u8, &payload, r.body);
    // Overlay at 50: the file grows to 150; bytes 0..50 keep, 50..150 are the new write.
    var second: [100]u8 = @splat(0xEE);
    _ = h.write(f, 50, &second, 0);
    const a1 = try h.getattr(f);
    try testing.expectEqual(@as(u64, 150), a1.size);
    try testing.expect(a1.mtime * std.time.ns_per_s + a1.mtimensec >= a0.mtime * std.time.ns_per_s + a0.mtimensec);
    const r2 = h.read(f, 0, 4096);
    try testing.expectEqual(@as(usize, 150), r2.body.len);
    try testing.expectEqualSlices(u8, payload[0..50], r2.body[0..50]);
    try testing.expectEqualSlices(u8, &second, r2.body[50..150]);
    // A partial read inside the file and one past EOF.
    try testing.expectEqualSlices(u8, payload[10..20], h.read(f, 10, 10).body);
    try testing.expectEqual(@as(usize, 0), h.read(f, 150, 10).body.len);
    // Past the inline bound: EFBIG, counted, nothing changed.
    const refused0 = fuse_write_no_data_volume_refused.load();
    const far = h.write(f, 4000, &payload, 0);
    try testing.expectEqual(linux.E.FBIG, far.errno().?);
    try testing.expectEqual(refused0 + 1, fuse_write_no_data_volume_refused.load());
    try testing.expectEqual(@as(u64, 150), (try h.getattr(f)).size);
    // Truncate to 30: the data clips.
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_SIZE;
    sa.size = 30;
    const t1 = try h.attr(h.setattr(f, sa));
    try testing.expectEqual(@as(u64, 30), t1.size);
    try testing.expectEqualSlices(u8, payload[0..30], h.read(f, 0, 4096).body);
    // Extend to 50 by truncate: zeros past the data.
    sa.size = 50;
    _ = h.setattr(f, sa);
    const ext = h.read(f, 0, 4096);
    try testing.expectEqual(@as(usize, 50), ext.body.len);
    try testing.expectEqualSlices(u8, payload[0..30], ext.body[0..30]);
    try testing.expect(std.mem.allEqual(u8, ext.body[30..50], 0));
    // Truncate to 0: the record carries an empty inline payload.
    sa.size = 0;
    _ = h.setattr(f, sa);
    try testing.expectEqual(@as(usize, 0), h.read(f, 0, 4096).body.len);
    const empty = (try h.fs.readLayout(try MemFs.localOf(f))).?;
    defer testing.allocator.free(empty.value);
    try testing.expectEqual(@as(usize, 0), empty.layout.data.len);
    // Truncate past the inline bound refuses.
    sa.size = 8192;
    try testing.expectEqual(linux.E.FBIG, h.setattr(f, sa).errno().?);
    h.release(f);
    try testing.expectEqual(@as(u32, 0), h.opens.get(f).?.count);
}

test "every layout-record RMW of the filesystem takes the data face's (3.7) layout lock exactly once, held across its commit (balance 0): a WRITE on the inline/staged arm (repro-port: fsstress -p 20 — a write's staged→staged publish raced the plane's promotion under (3.5)+(3.7) and overwrote the packed record, leaving its +ref durable: fsck C2/C3/C8 on every pack block), a truncate (SETATTR size, O_TRUNC) before its 4a set, a refused size included; a chmod takes none; the lock-order rail stays silent across (1) → (3.7) → (4a)" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const inv0 = lock_order.lock_order_inversions.load();
    const f = try h.create(1, "f", 0o644);
    // The write: the lock around its layout read → publish → commit.
    _ = h.write(f, 0, "0123456789", 0);
    try testing.expectEqual(@as(u32, 1), h.data.layout_locks_taken);
    try testing.expectEqual(@as(u32, 0), h.data.layout_lock_held);
    // A refused write (EFBIG past the volume-less inline bound) releases too.
    var big: [4097]u8 = @splat('x');
    try testing.expectEqual(linux.E.FBIG, h.write(f, 0, &big, 0).errno().?);
    try testing.expectEqual(@as(u32, 2), h.data.layout_locks_taken);
    try testing.expectEqual(@as(u32, 0), h.data.layout_lock_held);
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_SIZE;
    sa.size = 4;
    try testing.expectEqual(@as(u64, 4), (try h.attr(h.setattr(f, sa))).size);
    try testing.expectEqual(@as(u32, 3), h.data.layout_locks_taken);
    try testing.expectEqual(@as(u32, 0), h.data.layout_lock_held);
    // The refusal path (EFBIG past the inline bound) releases too.
    sa.size = 1 << 20;
    try testing.expectEqual(linux.E.FBIG, h.setattr(f, sa).errno().?);
    try testing.expectEqual(@as(u32, 4), h.data.layout_locks_taken);
    try testing.expectEqual(@as(u32, 0), h.data.layout_lock_held);
    // A chmod: no layout mutation, no lock.
    var sm = abi.zeroed(abi.fuse_setattr_in);
    sm.valid = abi.FATTR_MODE;
    sm.mode = 0o600;
    try testing.expectEqual(@as(u32, S.IFREG | 0o600), (try h.attr(h.setattr(f, sm))).mode);
    try testing.expectEqual(@as(u32, 4), h.data.layout_locks_taken);
    // A size the file already has (no layout mutation) still balances.
    sa.size = 4;
    _ = h.setattr(f, sa);
    try testing.expectEqual(@as(u32, 5), h.data.layout_locks_taken);
    try testing.expectEqual(@as(u32, 0), h.data.layout_lock_held);
    h.release(f);
    // O_TRUNC rides the open under the lock.
    const op = h.call(.{ .open = .{ .flags = O_TRUNC, .open_flags = 0 } }, f);
    try testing.expectEqual(@as(?linux.E, null), op.errno());
    try testing.expectEqual(@as(u64, 0), (try h.getattr(f)).size);
    try testing.expectEqual(@as(u32, 6), h.data.layout_locks_taken);
    try testing.expectEqual(@as(u32, 0), h.data.layout_lock_held);
    h.release(f);
    try testing.expectEqual(inv0, lock_order.lock_order_inversions.load());
}

// The level-1 taker list says "SETATTR iff size" — and the O_TRUNC open IS a
// size set (ATOMIC_O_TRUNC: the kernel sends no SETATTR). Without the
// exclusive guard the open's truncate ran beside Shared-class writes
// admitted on `end ≤ shape size`: the inode's size dropped under them and
// the data plane fired `write_shared_class_violation` (×6 across fstests
// pass 2, ×1 in a fsstress round).
test "an O_TRUNC open truncates under the level-1 EXCLUSIVE guard exactly as SETATTR-size does: a shared taker is refused while the custody settles; a plain open takes no guard" {
    const H = HarnessOf(SettlingData);
    var h: H = undefined;
    try h.init();
    defer h.deinit();
    defer h.data.deinit();
    h.data.store = &h.store;
    const f = try h.create(1, "f", 0o644);
    const lf = try H.Fs.localOf(f);
    try h.data.publishStriped(lf, 1 << 20, null, Mapping.wholeBlock(0x11, 0, 1));
    h.data.guards = &h.guards;
    h.data.guard_nodeid = f;
    // The control: SETATTR size holds the exclusive guard across the settle.
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_SIZE;
    sa.size = 4096;
    try testing.expectEqual(@as(u64, 4096), (try h.attr(h.setattr(f, sa))).size);
    try testing.expectEqual(@as(u32, 1), h.data.flushes);
    try testing.expectEqual(@as(?bool, true), h.data.shared_refused_at_flush);
    // The O_TRUNC open: the same guard, the same exclusion.
    h.data.shared_refused_at_flush = null;
    const op = h.call(.{ .open = .{ .flags = O_TRUNC, .open_flags = 0 } }, f);
    try testing.expectEqual(@as(?linux.E, null), op.errno());
    try testing.expectEqual(@as(u32, 2), h.data.flushes);
    try testing.expectEqual(@as(?bool, true), h.data.shared_refused_at_flush);
    try testing.expectEqual(@as(u64, 0), (try h.getattr(f)).size);
    h.release(f);
    // A plain open settles nothing and takes no guard: a shared taker is admitted meanwhile.
    h.data.shared_refused_at_flush = null;
    const g = h.guards.acquire(testing.io, f, .shared);
    const plain = h.call(.{ .open = .{ .flags = 0, .open_flags = 0 } }, f);
    g.release(testing.io);
    try testing.expectEqual(@as(?linux.E, null), plain.errno());
    try testing.expectEqual(@as(u32, 2), h.data.flushes);
    h.release(f);
    h.release(f);
}

test "POSIX: mkdir/readdir/rmdir — dot entries, cookie continuation, ENOTEMPTY, a removed directory looks up negative; lookup of . and .." {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const d = try h.mkdir(1, "d");
    const root_after = try h.getattr(1);
    try testing.expectEqual(@as(u32, 3), root_after.nlink);
    try testing.expectEqual(@as(u32, 2), (try h.getattr(d)).nlink);
    const a = try h.create(d, "a", 0o600);
    const b = try h.create(d, "b", 0o600);
    h.release(a);
    h.release(b);
    var buf: [256]u8 = undefined;
    const names = try h.readdirNames(d, 0, &buf);
    try testing.expect(std.mem.startsWith(u8, names, ".,..,"));
    try testing.expect(std.mem.indexOf(u8, names, "a") != null and std.mem.indexOf(u8, names, "b") != null);
    try testing.expectEqual(@as(usize, 4), std.mem.count(u8, names, ",") + 1);
    // Continuation from the cookie of the first real entry yields only the other.
    const first_name = names[5..6];
    const cookie = try h.readdirCookie(d, first_name);
    try testing.expect(cookie >= key.READDIR_COOKIE_BIAS);
    const rest = try h.readdirNames(d, cookie, &buf);
    try testing.expectEqual(@as(usize, 1), rest.len);
    try testing.expect(!std.mem.eql(u8, rest, first_name));
    // From offset 1 (`.` consumed): `..` then the entries.
    try testing.expect(std.mem.startsWith(u8, try h.readdirNames(d, key.READDIR_OFFSET_DOT, &buf), "..,"));
    // A bad cookie is EINVAL.
    const bad: abi.fuse_read_in = .{ .fh = 1, .offset = std.math.maxInt(u64), .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    try testing.expectEqual(linux.E.INVAL, h.call(.{ .readdir = bad }, d).errno().?);
    // rmdir refuses a non-empty directory; unlink refuses a directory.
    try testing.expectEqual(linux.E.NOTEMPTY, h.call(.{ .rmdir = .{ .name = "d" } }, 1).errno().?);
    try testing.expectEqual(linux.E.ISDIR, h.call(.{ .unlink = .{ .name = "d" } }, 1).errno().?);
    try testing.expectEqual(linux.E.NOTDIR, h.call(.{ .rmdir = .{ .name = "a" } }, d).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "a" } }, d).errno());
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "b" } }, d).errno());
    try testing.expectEqual(linux.E.NOENT, h.call(.{ .unlink = .{ .name = "a" } }, d).errno().?);
    // `.` and `..` resolve through the memo.
    try testing.expectEqual(d, (try h.entry(h.lookup(d, "."))).nodeid);
    try testing.expectEqual(@as(u64, 1), (try h.entry(h.lookup(d, ".."))).nodeid);
    try testing.expectEqual(@as(u64, 1), (try h.entry(h.lookup(1, ".."))).nodeid);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rmdir = .{ .name = "d" } }, 1).errno());
    try testing.expectEqual(@as(u32, 2), (try h.getattr(1)).nlink);
    // Negative entry: nodeid 0 with the negative TTL.
    const neg = try h.entry(h.lookup(1, "d"));
    try testing.expectEqual(@as(u64, 0), neg.nodeid);
    try testing.expectEqual(@as(u64, 1), neg.entry_valid);
    // ENAMETOOLONG on a 256-byte name; EEXIST on a duplicate.
    const long: [256]u8 = @splat('x');
    try testing.expectEqual(linux.E.NAMETOOLONG, h.lookup(1, &long).errno().?);
    _ = try h.mkdir(1, "e");
    try testing.expectEqual(linux.E.EXIST, h.call(.{ .mkdir = .{ .name = "e", .mode = 0o755, .umask = 0 } }, 1).errno().?);
}

test "POSIX: rename — plain, replace, NOREPLACE → EEXIST, EXCHANGE, a directory move updates `..`, a name across directories" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const x = try h.create(1, "x", 0o644);
    h.release(x);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "x", .newname = "y" } }, 1).errno());
    try testing.expectEqual(@as(u64, 0), (try h.entry(h.lookup(1, "x"))).nodeid);
    try testing.expectEqual(x, (try h.entry(h.lookup(1, "y"))).nodeid);
    try testing.expectEqual(linux.E.NOENT, h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "x", .newname = "q" } }, 1).errno().?);
    // Replace: z's inode drops to nlink 0 (a corpse until FORGET).
    const z = try h.create(1, "z", 0o644);
    h.release(z);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "y", .newname = "z" } }, 1).errno());
    try testing.expectEqual(x, (try h.entry(h.lookup(1, "z"))).nodeid);
    try testing.expectEqual(@as(u32, 0), (try h.getattr(z)).nlink);
    // NOREPLACE over an existing name.
    const w = try h.create(1, "w", 0o644);
    h.release(w);
    try testing.expectEqual(linux.E.EXIST, h.call(.{ .rename2 = .{ .newdir = 1, .flags = RENAME_NOREPLACE, .name = "w", .newname = "z" } }, 1).errno().?);
    // EXCHANGE swaps the two names' inos.
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rename2 = .{ .newdir = 1, .flags = RENAME_EXCHANGE, .name = "w", .newname = "z" } }, 1).errno());
    try testing.expectEqual(x, (try h.entry(h.lookup(1, "w"))).nodeid);
    try testing.expectEqual(w, (try h.entry(h.lookup(1, "z"))).nodeid);
    try testing.expectEqual(linux.E.INVAL, h.call(.{ .rename2 = .{ .newdir = 1, .flags = RENAME_EXCHANGE | RENAME_NOREPLACE, .name = "w", .newname = "z" } }, 1).errno().?);
    // A directory move: both parents' nlink shift and `..` follows.
    const d1 = try h.mkdir(1, "d1");
    const d2 = try h.mkdir(1, "d2");
    const sub = try h.mkdir(d1, "sub");
    try testing.expectEqual(@as(u32, 3), (try h.getattr(d1)).nlink);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .rename = .{ .newdir = d2, .flags = 0, .name = "sub", .newname = "moved" } }, d1).errno());
    try testing.expectEqual(@as(u32, 2), (try h.getattr(d1)).nlink);
    try testing.expectEqual(@as(u32, 3), (try h.getattr(d2)).nlink);
    try testing.expectEqual(d2, (try h.entry(h.lookup(sub, ".."))).nodeid);
    try testing.expectEqual(sub, (try h.entry(h.lookup(d2, "moved"))).nodeid);
    // A directory cannot replace a file and vice versa.
    try testing.expectEqual(linux.E.ISDIR, h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "w", .newname = "d1" } }, 1).errno().?);
    try testing.expectEqual(linux.E.NOTDIR, h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = "d1", .newname = "w" } }, 1).errno().?);
}

test "POSIX: symlink/readlink and link/unlink — nlink moves through the economy, the target rides the create entry, a directory cannot be linked" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const l = (try h.entry(h.call(.{ .symlink = .{ .name = "l", .link = "/target/path" } }, 1))).nodeid;
    const la = try h.getattr(l);
    try testing.expectEqual(@as(u32, S.IFLNK | 0o777), la.mode);
    try testing.expectEqual(@as(u64, 12), la.size);
    const rl = h.call(.readlink, l);
    try testing.expectEqualStrings("/target/path", rl.body);
    const long_target: [4096]u8 = @splat('t');
    try testing.expectEqual(linux.E.NAMETOOLONG, h.call(.{ .symlink = .{ .name = "l2", .link = &long_target } }, 1).errno().?);
    const f = try h.create(1, "f", 0o644);
    h.release(f);
    try testing.expectEqual(linux.E.INVAL, h.call(.readlink, f).errno().?);
    const linked = try h.entry(h.call(.{ .link = .{ .oldnodeid = f, .newname = "f2" } }, 1));
    try testing.expectEqual(f, linked.nodeid);
    try testing.expectEqual(@as(u32, 2), linked.attr.nlink);
    try testing.expectEqual(@as(u32, 2), (try h.getattr(f)).nlink);
    try testing.expectEqual(linux.E.EXIST, h.call(.{ .link = .{ .oldnodeid = f, .newname = "f2" } }, 1).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "f" } }, 1).errno());
    try testing.expectEqual(@as(u32, 1), (try h.getattr(f)).nlink);
    try testing.expectEqual(f, (try h.entry(h.lookup(1, "f2"))).nodeid);
    const d = try h.mkdir(1, "d");
    try testing.expectEqual(linux.E.ACCES, h.call(.{ .link = .{ .oldnodeid = d, .newname = "dlink" } }, 1).errno().?);
    // mknod: a fifo with the mode and rdev the kernel sent.
    const fifo = try h.entry(h.call(.{ .mknod = .{ .name = "p", .mode = S.IFIFO | 0o600, .rdev = 0, .umask = 0 } }, 1));
    try testing.expectEqual(@as(u32, S.IFIFO | 0o600), fifo.attr.mode);
    const dev = try h.entry(h.call(.{ .mknod = .{ .name = "c", .mode = S.IFCHR | 0o600, .rdev = 0x0501, .umask = 0 } }, 1));
    try testing.expectEqual(@as(u32, 0x0501), dev.attr.rdev);
}

test "POSIX: xattrs — set/get/list/remove, the size probe, ERANGE, CREATE/REPLACE flags, E2BIG, the reserved names are screened" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o644);
    _ = h.write(f, 0, "data", 0);
    h.release(f);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = "user.a", .size = 0 } }, f).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "user.a", .value = "alpha", .flags = 0 } }, f).errno());
    const probe = h.call(.{ .getxattr = .{ .name = "user.a", .size = 0 } }, f);
    try testing.expectEqual(@as(u32, 5), (abi.decodeExact(abi.fuse_getxattr_out, probe.outBytes()) orelse return error.TestBadReply).size);
    try testing.expectEqual(linux.E.RANGE, h.call(.{ .getxattr = .{ .name = "user.a", .size = 3 } }, f).errno().?);
    try testing.expectEqualStrings("alpha", h.call(.{ .getxattr = .{ .name = "user.a", .size = 64 } }, f).body);
    try testing.expectEqual(linux.E.EXIST, h.call(.{ .setxattr = .{ .name = "user.a", .value = "x", .flags = XATTR_CREATE } }, f).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .setxattr = .{ .name = "user.b", .value = "x", .flags = XATTR_REPLACE } }, f).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "user.a", .value = "beta", .flags = XATTR_REPLACE } }, f).errno());
    try testing.expectEqualStrings("beta", h.call(.{ .getxattr = .{ .name = "user.a", .size = 64 } }, f).body);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "user.b", .value = "", .flags = 0 } }, f).errno());
    // The list hides `layout` and `system.symlink`.
    const lp = h.call(.{ .listxattr = .{ .size = 0 } }, f);
    try testing.expectEqual(@as(u32, 14), (abi.decodeExact(abi.fuse_getxattr_out, lp.outBytes()) orelse return error.TestBadReply).size);
    const list = h.call(.{ .listxattr = .{ .size = 256 } }, f);
    try testing.expectEqual(@as(usize, 14), list.body.len);
    try testing.expect(std.mem.indexOf(u8, list.body, "user.a\x00") != null);
    try testing.expect(std.mem.indexOf(u8, list.body, "user.b\x00") != null);
    try testing.expect(std.mem.indexOf(u8, list.body, "layout") == null);
    try testing.expectEqual(linux.E.RANGE, h.call(.{ .listxattr = .{ .size = 5 } }, f).errno().?);
    // Reserved names (the allowlist's complement): EPERM on set/remove,
    // ENODATA on get, each counted; POSIX ACLs EOPNOTSUPP.
    const refused0 = fuse_reserved_xattr_refusals.load();
    try testing.expectEqual(linux.E.PERM, h.call(.{ .setxattr = .{ .name = "layout", .value = "x", .flags = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = "layout", .size = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .removexattr = .{ .name = "system.symlink" } }, f).errno().?);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .setxattr = .{ .name = "job:enroll", .value = "x", .flags = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .setxattr = .{ .name = "user.squeezefs.stripes", .value = "4", .flags = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = "client:abc", .size = 0 } }, f).errno().?);
    try testing.expectEqual(refused0 + 6, fuse_reserved_xattr_refusals.load());
    try testing.expectEqual(linux.E.OPNOTSUPP, h.call(.{ .setxattr = .{ .name = "system.posix_acl_access", .value = "x", .flags = 0 } }, f).errno().?);
    // The admitted families beside `user.*`.
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "trusted.x", .value = "t", .flags = 0 } }, f).errno());
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "security.capability", .value = "c", .flags = 0 } }, f).errno());
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .removexattr = .{ .name = "trusted.x" } }, f).errno());
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .removexattr = .{ .name = "security.capability" } }, f).errno());
    // E2BIG past the record cap; removal; a second removal is ENODATA.
    const huge: [70000]u8 = @splat('v');
    try testing.expectEqual(linux.E.@"2BIG", h.call(.{ .setxattr = .{ .name = "user.big", .value = &huge, .flags = 0 } }, f).errno().?);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .removexattr = .{ .name = "user.a" } }, f).errno());
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .removexattr = .{ .name = "user.a" } }, f).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = "user.a", .size = 0 } }, f).errno().?);
    // The data record is untouched by the xattr traffic.
    try testing.expectEqualStrings("data", h.read(f, 0, 64).body);
}

const Il0Stub = struct {
    calls: u32 = 0,
    fn synthesize(ctx: *anyopaque, out: *[ipc_wire.BLOB_LEN]u8) void {
        const self: *Il0Stub = @ptrCast(@alignCast(ctx));
        self.calls += 1;
        @memset(out, 0);
        @memcpy(out[0..8], "SQZIL0\x00\x00");
        out[8] = 1;
    }
};

test "the il0 rendezvous xattr: ENODATA until the session host arms it; armed, GETXATTR answers the 312 B blob (size probe, ERANGE, the bytes), LISTXATTR filters it, SETXATTR/REMOVEXATTR refuse EPERM, and it is never stored" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o644);
    h.release(f);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = ipc_wire.IL0_XATTR, .size = 0 } }, f).errno().?);
    var stub: Il0Stub = .{};
    h.fs.bindIl0(.{ .ctx = &stub, .synthesize = &Il0Stub.synthesize });
    const probe = h.call(.{ .getxattr = .{ .name = ipc_wire.IL0_XATTR, .size = 0 } }, f);
    try testing.expectEqual(@as(u32, ipc_wire.BLOB_LEN), (abi.decodeExact(abi.fuse_getxattr_out, probe.outBytes()) orelse return error.TestBadReply).size);
    try testing.expectEqual(linux.E.RANGE, h.call(.{ .getxattr = .{ .name = ipc_wire.IL0_XATTR, .size = 100 } }, f).errno().?);
    const got = h.call(.{ .getxattr = .{ .name = ipc_wire.IL0_XATTR, .size = 4096 } }, f);
    try testing.expectEqual(@as(usize, ipc_wire.BLOB_LEN), got.body.len);
    try testing.expectEqualStrings("SQZIL0\x00\x00", got.body[0..8]);
    try testing.expectEqual(@as(u32, 3), stub.calls);
    // The directory's root answers it too (the shim reads it off any fd of the mount).
    try testing.expectEqual(@as(usize, ipc_wire.BLOB_LEN), h.call(.{ .getxattr = .{ .name = ipc_wire.IL0_XATTR, .size = 4096 } }, 1).body.len);
    // Filtered from the list; EPERM on set/remove; nothing stored.
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = "user.a", .value = "alpha", .flags = 0 } }, f).errno());
    const list = h.call(.{ .listxattr = .{ .size = 256 } }, f);
    try testing.expect(std.mem.indexOf(u8, list.body, "il0") == null);
    try testing.expect(std.mem.indexOf(u8, list.body, "user.a\x00") != null);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .setxattr = .{ .name = ipc_wire.IL0_XATTR, .value = "x", .flags = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .removexattr = .{ .name = ipc_wire.IL0_XATTR } }, f).errno().?);
    try testing.expectEqual(@as(?[]u8, null), try h.fs.xattrValue(try MemFs.localOf(f), ipc_wire.IL0_XATTR));
}

test "the xattr allowlist: user.* yes (minus user.squeezefs.*), security.* and trusted.* yes; layout / system.symlink / job:* / client:* / writer_claim / the format record no" {
    try testing.expect(MemFs.xattrNameAllowed("user.x"));
    try testing.expect(MemFs.xattrNameAllowed("user.mime_type"));
    try testing.expect(!MemFs.xattrNameAllowed("user.squeezefs.x"));
    try testing.expect(!MemFs.xattrNameAllowed("user.squeezefs.il0"));
    // The ONE user-settable name of the reserved family.
    try testing.expect(MemFs.xattrNameAllowed("user.squeezefs.gather"));
    try testing.expect(MemFs.xattrNameAllowed("security.capability"));
    try testing.expect(MemFs.xattrNameAllowed("trusted.x"));
    try testing.expect(!MemFs.xattrNameAllowed("layout"));
    try testing.expect(!MemFs.xattrNameAllowed("system.symlink"));
    try testing.expect(!MemFs.xattrNameAllowed("job:x"));
    try testing.expect(!MemFs.xattrNameAllowed("client:x"));
    try testing.expect(!MemFs.xattrNameAllowed("writer_claim"));
    try testing.expect(!MemFs.xattrNameAllowed("format_config"));
    try testing.expect(!MemFs.xattrNameAllowed("system.posix_acl_access"));
    try testing.expect(!MemFs.xattrNameAllowed(""));
}

/// A test renderer: a fixed payload per class, counting builds.
const FakeVirtual = struct {
    builds: u32 = 0,

    fn build(ctx: *anyopaque, class: virtual_inodes.Class, gpa: Allocator) Allocator.Error!virtual_inodes.Owned {
        const self: *FakeVirtual = @ptrCast(@alignCast(ctx));
        self.builds += 1;
        var out: std.Io.Writer.Allocating = .init(gpa);
        errdefer out.deinit();
        out.writer.print("{{\"class\":\"{s}\",\"build\":{d}}}\n", .{ @tagName(class), self.builds }) catch return error.OutOfMemory;
        const buf = out.writer.buffer;
        const len = out.writer.end;
        out.writer.buffer = &.{};
        return .{ .buf = buf, .len = len };
    }

    fn face(self: *FakeVirtual) VirtualFace {
        return .{ .ctx = self, .build = build };
    }
};

test ".stats/.config/.trace: lookup-only generation inos (never listed, both TTLs 0, mode 0400 + mount uid/gid, size exact, blocks = ceil(size/512)); two lookups mint two frozen generations; open pins, reads slice, release + forget retire; a retired generation is ESTALE; unbound they are negative" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    // Unbound: a plain negative lookup (the loopback has no renderer).
    try testing.expectEqual(@as(u64, 0), (try h.entry(h.lookup(1, ".stats"))).nodeid);
    var fake: FakeVirtual = .{};
    h.fs.bindVirtual(fake.face());
    h.fs.uid = 1234;
    h.fs.gid = 5678;
    const e1 = try h.entry(h.lookup(1, ".stats"));
    try testing.expect(virtual_inodes.isVirtualIno(e1.nodeid));
    try testing.expectEqual(virtual_inodes.Class.stats, virtual_inodes.classOf(e1.nodeid));
    try testing.expectEqual(@as(u64, 0), e1.entry_valid);
    try testing.expectEqual(@as(u32, 0), e1.entry_valid_nsec);
    try testing.expectEqual(@as(u64, 0), e1.attr_valid);
    try testing.expectEqual(@as(u32, S.IFREG | 0o400), e1.attr.mode);
    try testing.expectEqual(@as(u32, 1234), e1.attr.uid);
    try testing.expectEqual(@as(u32, 5678), e1.attr.gid);
    try testing.expectEqual(@as(u32, 1), e1.attr.nlink);
    const payload1 = "{\"class\":\"stats\",\"build\":1}\n";
    try testing.expectEqual(@as(u64, payload1.len), e1.attr.size);
    try testing.expectEqual(@as(u64, 1), e1.attr.blocks);
    try testing.expectEqual(@as(u32, 1), fake.builds);
    // A second lookup mints a DISTINCT generation; the first stays frozen.
    const e2 = try h.entry(h.lookup(1, ".config"));
    try testing.expect(e2.nodeid != e1.nodeid);
    try testing.expectEqual(virtual_inodes.Class.config, virtual_inodes.classOf(e2.nodeid));
    const a1 = try h.getattr(e1.nodeid);
    try testing.expectEqual(@as(u64, payload1.len), a1.size);
    // OPEN: DIRECT_IO, fh = the generation; READ slices the snapshot.
    const o = h.call(.{ .open = .{ .flags = 0, .open_flags = 0 } }, e1.nodeid);
    const oo = abi.decodeExact(abi.fuse_open_out, o.outBytes()) orelse return error.TestBadReply;
    try testing.expectEqual(e1.nodeid, oo.fh);
    try testing.expect(oo.open_flags & abi.FOPEN_DIRECT_IO != 0);
    try testing.expectEqualStrings(payload1, h.read(e1.nodeid, 0, 4096).body);
    try testing.expectEqualStrings("build\":1}\n", h.read(e1.nodeid, payload1.len - 10, 4096).body);
    try testing.expectEqual(@as(usize, 0), h.read(e1.nodeid, payload1.len, 4096).body.len);
    // A write-intent open is refused by the mode; every mutation refuses.
    try testing.expectEqual(linux.E.ACCES, h.call(.{ .open = .{ .flags = @bitCast(linux.O{ .ACCMODE = .WRONLY }), .open_flags = 0 } }, e1.nodeid).errno().?);
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_SIZE;
    try testing.expectEqual(linux.E.ACCES, h.setattr(e1.nodeid, sa).errno().?);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .setxattr = .{ .name = "user.x", .value = "v", .flags = 0 } }, e1.nodeid).errno().?);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = "user.x", .size = 0 } }, e1.nodeid).errno().?);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .unlink = .{ .name = ".stats" } }, 1).errno().?);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .mkdir = .{ .name = ".trace", .mode = 0o755, .umask = 0 } }, 1).errno().?);
    try testing.expectEqual(linux.E.PERM, h.call(.{ .rename = .{ .newdir = 1, .flags = 0, .name = ".config", .newname = "x" } }, 1).errno().?);
    // Never listed by readdir; the reserved cookies resume as the end.
    var buf: [512]u8 = undefined;
    const names = try h.readdirNames(1, 0, &buf);
    try testing.expect(std.mem.indexOf(u8, names, ".stats") == null and std.mem.indexOf(u8, names, ".config") == null);
    try testing.expectEqual(@as(usize, 0), (try h.readdirNames(1, virtual_inodes.READDIR_STATS_COOKIE, &buf)).len);
    // RELEASE drops the fh pin; FORGET the kernel's; the ring still holds it.
    h.release(e1.nodeid);
    h.forget(e1.nodeid, 1);
    try testing.expectEqual(@as(u64, payload1.len), (try h.getattr(e1.nodeid)).size);
    // Past the ring cap the first generation retires: ESTALE.
    var i: usize = 0;
    while (i < virtual_inodes.REGISTRY_CAP) : (i += 1) {
        const e = try h.entry(h.lookup(1, ".trace"));
        h.forget(e.nodeid, 1);
    }
    try testing.expectEqual(linux.E.STALE, h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, e1.nodeid).errno().?);
    try testing.expectEqual(linux.E.STALE, h.call(.{ .open = .{ .flags = 0, .open_flags = 0 } }, e1.nodeid).errno().?);
    try testing.expectEqual(linux.E.STALE, h.read(e1.nodeid, 0, 16).errno().?);
    // `.stats` in a subdirectory is an ordinary name.
    const d = try h.mkdir(1, "d");
    const sub = try h.create(d, ".stats", 0o644);
    try testing.expect(!virtual_inodes.isVirtualIno(sub));
    h.release(sub);
}

test "clone: an inline source copies its bytes into a NEW inode; a striped source's dest carries an identical map and every binding a SHARED +ref on BOTH owners (refcount 2) with nothing copied; a staged source and a directory refuse" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    var tr = op_trace.OpTrace.begin(1, .other);
    defer tr.finish();
    var win: [64]u8 = undefined;
    var c: Ctx = .{ .unique = 1, .uid = 1000, .gid = 1000, .pid = 7, .io = testing.io, .trace = &tr, .wd_slot = null, .window = &win };
    // Inline.
    const f = try h.create(1, "f", 0o640);
    _ = h.write(f, 0, "hello clone", 0);
    h.release(f);
    const out = try h.fs.cloneFile(&c, f, 1, "f.clone");
    try testing.expect(out.dest_ino != f);
    try testing.expectEqual(@as(u64, 0), out.blocks_shared);
    try testing.expectEqual(@as(u64, 11), out.inline_bytes);
    try testing.expectEqualStrings("hello clone", h.read(out.dest_ino, 0, 64).body);
    const da = try h.getattr(out.dest_ino);
    try testing.expectEqual(@as(u64, 11), da.size);
    try testing.expectEqual(@as(u32, S.IFREG | 0o640), da.mode);
    // The dest is independent: a write to it leaves the source alone.
    _ = h.write(out.dest_ino, 0, "HELLO", 0);
    try testing.expectEqualStrings("hello clone", h.read(f, 0, 64).body);
    // Striped: a hand-laid two-block map on a fresh inode (the volume-less
    // data face never writes one; the refs plane is the store's).
    const g = try h.create(1, "g", 0o644);
    h.release(g);
    const gl = try MemFs.localOf(g);
    const chunk = layout.DEFAULT_CHUNK;
    const entries = [_]layout.MapEntryWire{
        (layout.MapEntry{ .block = 0, .mapping = Mapping.wholeBlock(0x10, 3 * chunk, 1) }).encode(),
        (layout.MapEntry{ .block = 1, .mapping = Mapping.wholeBlock(0x10, 9 * chunk, 1) }).encode(),
    };
    {
        const t = try h.store.begin(@src());
        const enc = try Layout.stripedInline(2 * chunk, &entries).encodeAlloc(testing.allocator);
        defer testing.allocator.free(enc);
        const k = try h.fs.xattrKeyFor(gl, LAYOUT_XATTR);
        try h.fs.stageXattrPut(t, &k.k, LAYOUT_XATTR, enc);
        var gv = try h.fs.readInode(gl);
        gv.size = 2 * chunk;
        try t.stagePut(key.TREE_INODES, &key.inodeKey(gl), &gv.encode());
        try block_refs.stageOps(t, &.{ block_refs.taken(.{ .vol_tag = 0x10, .block_idx = 3, .owner_ino = gl, .block_index = 0 }), block_refs.taken(.{ .vol_tag = 0x10, .block_idx = 9, .owner_ino = gl, .block_index = 1 }) });
        try h.store.commit(t);
    }
    const refs_before = h.store.countKind(key.TREE_BLOCK_REFS);
    const sout = try h.fs.cloneFile(&c, g, 1, "g.clone");
    try testing.expectEqual(@as(u64, 2), sout.blocks_shared);
    try testing.expectEqual(@as(u64, 0), sout.inline_bytes);
    const dl = try MemFs.localOf(sout.dest_ino);
    try testing.expectEqual(@as(u64, 2 * chunk), (try h.getattr(sout.dest_ino)).size);
    // The maps are identical.
    const src_rec = (try h.fs.readLayout(gl)).?;
    defer testing.allocator.free(src_rec.value);
    const dst_rec = (try h.fs.readLayout(dl)).?;
    defer testing.allocator.free(dst_rec.value);
    try testing.expectEqualSlices(u8, src_rec.value, dst_rec.value);
    // refcount 1 → 2 per block, every record SHARED.
    try testing.expectEqual(refs_before + 2, h.store.countKind(key.TREE_BLOCK_REFS));
    for ([_]u64{ 3, 9 }) |blk| {
        for ([_]u64{ gl, dl }) |owner| {
            const idx: u32 = if (blk == 3) 0 else 1;
            const rk = key.blockRefKey(0x10, blk, owner, idx);
            const v = (try h.store.lookup(testing.allocator, key.TREE_BLOCK_REFS, &rk)).?;
            defer testing.allocator.free(v);
            try testing.expect((try record.BlockRefValue.decode(v)).shared);
        }
    }
    // Refusals: a directory, a staged source.
    const d = try h.mkdir(1, "d");
    try testing.expectError(error.NotAFile, h.fs.cloneFile(&c, d, 1, "d.clone"));
    const st = try h.create(1, "s", 0o644);
    h.release(st);
    const sl = try MemFs.localOf(st);
    {
        const t = try h.store.begin(@src());
        const enc = try Layout.staged(100, layout.zero_file_id, null).encodeAlloc(testing.allocator);
        defer testing.allocator.free(enc);
        const k = try h.fs.xattrKeyFor(sl, LAYOUT_XATTR);
        try h.fs.stageXattrPut(t, &k.k, LAYOUT_XATTR, enc);
        try h.store.commit(t);
    }
    try testing.expectError(error.StagedSource, h.fs.cloneFile(&c, st, 1, "s.clone"));
    try testing.expectEqual(@as(u64, 0), (try h.entry(h.lookup(1, "s.clone"))).nodeid);
    // A clone onto an existing name is EEXIST; onto a virtual name EPERM.
    try testing.expectError(error.PathAlreadyExists, h.fs.cloneFile(&c, f, 1, "f.clone"));
    try testing.expectError(error.PermissionDenied, h.fs.cloneFile(&c, f, 1, ".stats"));
}

test "POSIX: fsync reaches the store's barrier; statfs reports the heap in pages, NAME_MAX and the ino space; opendir refuses a file; open refuses a directory" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o644);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .fsync = .{ .fh = 1, .datasync = false } }, f).errno());
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .fsyncdir = .{ .fh = 1, .datasync = true } }, 1).errno());
    try testing.expectEqual(@as(u64, 2), h.store.fsyncs);
    const st = abi.decodeExact(abi.fuse_statfs_out, h.call(.statfs, 1).outBytes()) orelse return error.TestBadReply;
    try testing.expectEqual(@as(u64, (64 << 20) / 4096), st.st.blocks);
    try testing.expectEqual(@as(u64, (48 << 20) / 4096), st.st.bfree);
    try testing.expectEqual(@as(u32, 255), st.st.namelen);
    try testing.expectEqual(@as(u32, 4096), st.st.bsize);
    try testing.expect(st.st.ffree < st.st.files);
    try testing.expectEqual(linux.E.NOTDIR, h.call(.{ .opendir = .{ .flags = 0, .open_flags = 0 } }, f).errno().?);
    try testing.expectEqual(linux.E.ISDIR, h.call(.{ .open = .{ .flags = 0, .open_flags = 0 } }, 1).errno().?);
    const od = h.call(.{ .opendir = .{ .flags = 0, .open_flags = 0 } }, 1);
    try testing.expectEqual(@as(?linux.E, null), od.errno());
    _ = h.call(.{ .releasedir = .{ .fh = 1, .flags = 0, .release_flags = 0, .lock_owner = 0 } }, 1);
    // ENOSYS for the rung-4 and never ops; access under default_permissions.
    try testing.expectEqual(linux.E.NOSYS, h.call(.{ .access = .{ .mask = 4 } }, 1).errno().?);
    try testing.expectEqual(linux.E.NOSYS, h.call(.{ .fallocate = abi.zeroed(abi.fuse_fallocate_in) }, f).errno().?);
    h.release(f);
}

test "the parked joiner's `.stats` under a dead manager (rung 10 item 5a): getattr(1) whose token serve refuses retryable (the root's holder dead) is served off the divert's last root entry — the exemption the -o ro fail-stop gate grants getattr(1); with no root ever served the refusal stands (EAGAIN); another ino never falls back" {
    const H = HarnessOf(NoData);
    var h: H = undefined;
    try h.init();
    defer h.deinit();
    // A divert whose every slot is a DEAD holder's: the serve refuses
    // HolderUnreachable before any dial.
    const Dead = struct {
        fn homeOf(_: *anyopaque, _: u16) read_divert.Home {
            return .{ .dead = .{ .holder = 4 } };
        }
        fn refresh(_: *anyopaque, _: u16, _: u32) read_divert.Home {
            return .{ .dead = .{ .holder = 4 } };
        }
        fn dial(_: *anyopaque, _: u64, _: u32) ?read_divert.cache.Link {
            return null;
        }
    };
    var marker: u8 = 0;
    var d = read_divert.Divert.init(testing.allocator, testing.io, .{ .ctx = &marker, .homeOf = &Dead.homeOf, .refresh = &Dead.refresh, .dial = &Dead.dial }, 1, 0, read_divert.cache.RecallSink.none, read_divert.cache.SelfLease.always, read_divert.cache.recordsBudgetBytes(0));
    defer d.deinit();
    h.fs.bindDivert(&d);
    // No root ever served under a token: the refusal stands, typed EAGAIN.
    try testing.expectEqual(@as(?linux.E, .AGAIN), h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, 1).errno());
    // The last root entry: served, with its words.
    d.last_root = .{ .mode = S.IFDIR | 0o755, .nlink = 9, .mtime = 777 * std.time.ns_per_s, .ctime = 777 * std.time.ns_per_s };
    const served0 = read_divert.stats.dlm_token_root_served_last.load();
    const a = try h.getattr(1);
    try testing.expectEqual(@as(u32, 9), a.nlink);
    try testing.expectEqual(@as(u64, 777), a.mtime);
    try testing.expectEqual(served0 + 1, read_divert.stats.dlm_token_root_served_last.load());
    // Any other ino under the dead holder refuses as before.
    try testing.expectEqual(@as(?linux.E, .AGAIN), h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, 2).errno());
    try testing.expectEqual(served0 + 1, read_divert.stats.dlm_token_root_served_last.load());
}

test "unlinked-open file: readable until the final FORGET; the forget gathers the corpse; the drain destroys inode + layout in ONE commit; open_count_stranded stays 0 and the open table closes" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const inodes0 = h.store.countKind(key.TREE_INODES);
    const xattrs0 = h.store.countKind(key.TREE_XATTRS);
    const f = try h.create(1, "f", 0o644);
    _ = h.write(f, 0, "payload", 0);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "f" } }, 1).errno());
    // Open and unlinked: still readable, nlink 0.
    try testing.expectEqual(@as(u32, 0), (try h.getattr(f)).nlink);
    try testing.expectEqualStrings("payload", h.read(f, 0, 64).body);
    try testing.expectEqual(inodes0 + 1, h.store.countKind(key.TREE_INODES));
    h.release(f);
    const commits0 = h.store.commits;
    const stranded0 = counters.tripwireCount("open_count_stranded");
    // The CREATE counted one lookup reference; the final FORGET evicts it.
    h.forget(f, 1);
    try testing.expectEqual(stranded0, counters.tripwireCount("open_count_stranded"));
    try testing.expectEqual(@as(?open_table.Entry, null), h.opens.get(f));
    // Gathered, not yet destroyed (the batch is 64).
    try testing.expectEqual(inodes0 + 1, h.store.countKind(key.TREE_INODES));
    try testing.expectEqual(@as(usize, 1), h.fs.reclaim.items.len);
    var tr = @import("op_trace.zig").OpTrace.begin(1, .other);
    var win: [16]u8 = undefined;
    var c: Ctx = .{ .unique = 1, .uid = 0, .gid = 0, .pid = 0, .io = testing.io, .trace = &tr, .wd_slot = null, .window = &win };
    try h.fs.reclaimDrain(&c);
    tr.finish();
    try testing.expectEqual(commits0 + 1, h.store.commits);
    try testing.expectEqual(inodes0, h.store.countKind(key.TREE_INODES));
    try testing.expectEqual(xattrs0, h.store.countKind(key.TREE_XATTRS));
    try testing.expectEqual(linux.E.NOENT, h.call(.{ .getattr = .{ .flags = 0, .fh = null } }, f).errno().?);
    try testing.expectEqual(@as(usize, 0), h.fs.reclaim.items.len);
    // A forget of a LIVE inode never gathers.
    const g = try h.create(1, "g", 0o644);
    h.release(g);
    h.forget(g, 1);
    try testing.expectEqual(@as(usize, 0), h.fs.reclaim.items.len);
    try testing.expectEqual(@as(u64, 0), h.opens.openCount());
}

test "the corpse sweep at mount: an unlinked file whose FORGET died with the daemon (nlink 0, no dentry) is destroyed by the next mount's sweep with its layout; live files are untouched; a clean store sweeps nothing" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const inodes0 = h.store.countKind(key.TREE_INODES);
    const xattrs0 = h.store.countKind(key.TREE_XATTRS);
    const f = try h.create(1, "f", 0o644);
    _ = h.write(f, 0, "payload", 0);
    const live = try h.create(1, "live", 0o644);
    _ = h.write(live, 0, "kept", 0);
    h.release(live);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = "f" } }, 1).errno());
    h.release(f);
    // The daemon dies here: no FORGET, the corpse record stays.
    try testing.expectEqual(inodes0 + 2, h.store.countKind(key.TREE_INODES));
    var next = try MemFs.mount(testing.allocator, testing.io, &h.store, &h.data, .{ .dlm_stripes = 16 });
    defer next.unmount();
    const swept0 = fuse_corpse_sweep_destroyed.load();
    try testing.expectEqual(@as(u64, 1), try next.sweepCorpsesAtMount());
    try testing.expectEqual(swept0 + 1, fuse_corpse_sweep_destroyed.load());
    try testing.expectEqual(inodes0 + 1, h.store.countKind(key.TREE_INODES));
    try testing.expectEqual(xattrs0 + 1, h.store.countKind(key.TREE_XATTRS));
    try testing.expect((try next.readInodeOpt(try MemFs.localOf(f))) == null);
    try testing.expectEqual(@as(u32, 1), (try next.readInode(try MemFs.localOf(live))).nlink);
    // Idempotent: a second sweep finds nothing.
    try testing.expectEqual(@as(u64, 0), try next.sweepCorpsesAtMount());
}

test "the corpse sweep pages through a slot tree that interleaves inode, dentry and xattr records (> SWEEP_PAGE of them): only the corpses go, every live file stays" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    // 500 files = 500 inodes + 500 dentries + 500 layout records.
    var files: [500]u64 = undefined;
    for (&files, 0..) |*ino, i| {
        var nb: [16]u8 = undefined;
        const name = try std.fmt.bufPrint(&nb, "f{d}", .{i});
        ino.* = try h.create(1, name, 0o644);
        _ = h.write(ino.*, 0, "x", 0);
        h.release(ino.*);
    }
    // Three corpses whose forgets never come: the first, one mid-page, the last.
    for ([_]usize{ 0, 250, 499 }) |i| {
        var nb: [16]u8 = undefined;
        const name = try std.fmt.bufPrint(&nb, "f{d}", .{i});
        try testing.expectEqual(@as(?linux.E, null), h.call(.{ .unlink = .{ .name = name } }, 1).errno());
    }
    const inodes0 = h.store.countKind(key.TREE_INODES);
    var next = try MemFs.mount(testing.allocator, testing.io, &h.store, &h.data, .{ .dlm_stripes = 16 });
    defer next.unmount();
    try testing.expectEqual(@as(u64, 3), try next.sweepCorpsesAtMount());
    try testing.expectEqual(inodes0 - 3, h.store.countKind(key.TREE_INODES));
    for (files, 0..) |ino, i| {
        const present = (try next.readInodeOpt(try MemFs.localOf(ino))) != null;
        try testing.expectEqual(i != 0 and i != 250 and i != 499, present);
    }
}

test "the SETATTR echo is absorbed (zero commits) while real news commits; nlookup closure: every entry reply is one reference and the batch forget returns them all" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const stranded0 = counters.tripwireCount("open_count_stranded");
    const f = try h.create(1, "f", 0o644);
    h.release(f);
    const a = try h.getattr(f);
    const commits0 = h.store.commits;
    const absorbed0 = economy.stats.meta_kv_times_echo_absorbed.load();
    // The kernel's writeback-cache echo: times-only, mtime == stored, ctime ahead.
    var echo = abi.zeroed(abi.fuse_setattr_in);
    echo.valid = abi.FATTR_MTIME | abi.FATTR_CTIME;
    echo.mtime = a.mtime;
    echo.mtimensec = a.mtimensec;
    echo.ctime = a.ctime + 1;
    echo.ctimensec = 0;
    const r = try h.attr(h.setattr(f, echo));
    try testing.expectEqual(commits0, h.store.commits);
    try testing.expectEqual(absorbed0 + 1, economy.stats.meta_kv_times_echo_absorbed.load());
    try testing.expectEqual(a.ctime + 1, r.ctime);
    // The folded view serves the refined ctime; the stored record still carries the old one.
    try testing.expectEqual(a.ctime + 1, (try h.getattr(f)).ctime);
    // chmod is real news: one commit, the mode and a bumped ctime.
    var chmod = abi.zeroed(abi.fuse_setattr_in);
    chmod.valid = abi.FATTR_MODE;
    chmod.mode = 0o600;
    const m = try h.attr(h.setattr(f, chmod));
    try testing.expectEqual(commits0 + 1, h.store.commits);
    try testing.expectEqual(@as(u32, S.IFREG | 0o600), m.mode);
    // nlookup closure: three lookups + the create = 4 references; a batch
    // returning 3 keeps the entry, the last one evicts.
    _ = try h.entry(h.lookup(1, "f"));
    _ = try h.entry(h.lookup(1, "f"));
    _ = try h.entry(h.lookup(1, "f"));
    try testing.expectEqual(@as(u64, 4), h.opens.get(f).?.lookups);
    const entries = [_]abi.fuse_forget_one{ .{ .nodeid = f, .nlookup = 2 }, .{ .nodeid = f, .nlookup = 1 } };
    h.fire(.{ .batch_forget = .{ .entries = &entries } }, 0);
    try testing.expectEqual(@as(u64, 1), h.opens.get(f).?.lookups);
    h.forget(f, 1);
    try testing.expectEqual(@as(?open_table.Entry, null), h.opens.get(f));
    try testing.expectEqual(stranded0, counters.tripwireCount("open_count_stranded"));
}

test "POSIX utimensat: an explicit atime/mtime set (no ctime on the wire — the kernel sends FATTR_CTIME only under the writeback cache) marks the status change: ctime advances to now, never stays at the backdated value; an atime-only set and the UTIME_NOW pair do too (repro-port: `touch -d` on the venue left ctime unmoved)" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o644);
    h.release(f);
    const created = try h.getattr(f);
    // Backdate every stamp explicitly (an exact set is verbatim), so a
    // later "now" is observable inside one coarse clock tick.
    const past: u64 = 1_000_000;
    var back = abi.zeroed(abi.fuse_setattr_in);
    back.valid = abi.FATTR_ATIME | abi.FATTR_MTIME | abi.FATTR_CTIME;
    back.atime = past;
    back.mtime = past;
    back.ctime = past;
    const b = try h.attr(h.setattr(f, back));
    try testing.expectEqual(past, b.ctime);
    // utimensat(2) with two exact times: FATTR_ATIME | FATTR_MTIME, no ctime.
    var ut = abi.zeroed(abi.fuse_setattr_in);
    ut.valid = abi.FATTR_ATIME | abi.FATTR_MTIME;
    ut.atime = past + 7;
    ut.mtime = past + 9;
    const u = try h.attr(h.setattr(f, ut));
    try testing.expectEqual(past + 7, u.atime);
    try testing.expectEqual(past + 9, u.mtime);
    try testing.expect(u.ctime >= created.ctime);
    try testing.expectEqual(u.ctime, (try h.getattr(f)).ctime);
    // Backdate again; an atime-only set (`touch -a -d`) is a status change too.
    _ = try h.attr(h.setattr(f, back));
    var at = abi.zeroed(abi.fuse_setattr_in);
    at.valid = abi.FATTR_ATIME;
    at.atime = past + 3;
    const a = try h.attr(h.setattr(f, at));
    try testing.expectEqual(past + 3, a.atime);
    try testing.expectEqual(past, a.mtime);
    try testing.expect(a.ctime >= created.ctime);
    // Backdate again; `touch` = ATIME_NOW | MTIME_NOW: all three move to now.
    _ = try h.attr(h.setattr(f, back));
    var nw = abi.zeroed(abi.fuse_setattr_in);
    nw.valid = abi.FATTR_ATIME | abi.FATTR_MTIME | abi.FATTR_ATIME_NOW | abi.FATTR_MTIME_NOW;
    const n = try h.attr(h.setattr(f, nw));
    try testing.expect(n.atime >= created.ctime);
    try testing.expect(n.mtime >= created.ctime);
    try testing.expect(n.ctime >= created.ctime);
    // An explicit ctime on the wire (the writeback flush's shape) is verbatim.
    var ex = abi.zeroed(abi.fuse_setattr_in);
    ex.valid = abi.FATTR_MTIME | abi.FATTR_CTIME;
    ex.mtime = past + 20;
    ex.ctime = past + 21;
    const x = try h.attr(h.setattr(f, ex));
    try testing.expectEqual(past + 21, x.ctime);
}

test "KILLPRIV_V2: a flagged write clears suid (and sgid only with group-exec) and drops security.capability in the write's commit; an unflagged write clears nothing; a flagged truncate and an O_TRUNC open fold the strip too" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const f = try h.create(1, "f", 0o6755);
    try testing.expectEqual(@as(u32, S.IFREG | 0o6755), (try h.getattr(f)).mode);
    try testing.expectEqual(@as(?linux.E, null), h.call(.{ .setxattr = .{ .name = CAPS_XATTR, .value = "caps", .flags = 0 } }, f).errno());
    const clears0 = fuse_killpriv_clears.load();
    const commits0 = h.store.commits;
    _ = h.write(f, 0, "x", 0);
    try testing.expectEqual(@as(u32, S.IFREG | 0o6755), (try h.getattr(f)).mode);
    try testing.expectEqual(clears0, fuse_killpriv_clears.load());
    _ = h.write(f, 0, "y", abi.FUSE_WRITE_KILL_SUIDGID);
    try testing.expectEqual(@as(u32, S.IFREG | 0o755), (try h.getattr(f)).mode);
    try testing.expectEqual(clears0 + 1, fuse_killpriv_clears.load());
    try testing.expectEqual(commits0 + 2, h.store.commits);
    try testing.expectEqual(linux.E.NODATA, h.call(.{ .getxattr = .{ .name = CAPS_XATTR, .size = 0 } }, f).errno().?);
    // sgid without group-exec survives (the mandatory-locking marker).
    const g = try h.create(1, "g", 0o2745);
    _ = h.write(g, 0, "y", abi.FUSE_WRITE_KILL_SUIDGID);
    try testing.expectEqual(@as(u32, S.IFREG | 0o2745), (try h.getattr(g)).mode);
    // A flagged truncate (chown/truncate's FATTR_KILL_SUIDGID).
    const t = try h.create(1, "t", 0o4711);
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_SIZE | abi.FATTR_KILL_SUIDGID;
    sa.size = 0;
    const ta = try h.attr(h.setattr(t, sa));
    try testing.expectEqual(@as(u32, S.IFREG | 0o711), ta.mode);
    // An O_TRUNC open with FUSE_OPEN_KILL_SUIDGID truncates and strips in one commit.
    const o = try h.create(1, "o", 0o4755);
    _ = h.write(o, 0, "content", 0);
    h.release(o);
    const commits1 = h.store.commits;
    const op = h.call(.{ .open = .{ .flags = O_TRUNC, .open_flags = abi.FUSE_OPEN_KILL_SUIDGID } }, o);
    try testing.expectEqual(@as(?linux.E, null), op.errno());
    try testing.expectEqual(commits1 + 1, h.store.commits);
    const oa = try h.getattr(o);
    try testing.expectEqual(@as(u64, 0), oa.size);
    try testing.expectEqual(@as(u32, S.IFREG | 0o755), oa.mode);
    try testing.expectEqual(@as(usize, 0), h.read(o, 0, 64).body.len);
    h.release(o);
    h.release(f);
    h.release(g);
    h.release(t);
}

test "READDIRPLUS lists entries with attrs and both TTL classes, counts a lookup reference per entry, and the dots carry nodeid 0" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const d = try h.mkdir(1, "d");
    const a = try h.create(d, "a", 0o644);
    h.release(a);
    const sub = try h.mkdir(d, "sub");
    const l0 = h.opens.get(a).?.lookups;
    const in: abi.fuse_read_in = .{ .fh = 1, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    const c = h.call(.{ .readdirplus = in }, d);
    try testing.expectEqual(@as(?linux.E, null), c.errno());
    var off: usize = 0;
    var n: usize = 0;
    var seen_sub = false;
    while (off + abi.fuse_direntplus.NAME_OFFSET <= c.body.len) : (n += 1) {
        const rec = abi.decodePrefix(abi.fuse_direntplus, c.body[off..]).?;
        const name = c.body[off + abi.fuse_direntplus.NAME_OFFSET ..][0..rec.dirent.namelen];
        if (std.mem.eql(u8, name, ".") or std.mem.eql(u8, name, "..")) {
            try testing.expectEqual(@as(u64, 0), rec.entry_out.nodeid);
        } else if (std.mem.eql(u8, name, "sub")) {
            seen_sub = true;
            try testing.expectEqual(sub, rec.entry_out.nodeid);
            try testing.expectEqual(@as(u32, S.IFDIR | 0o755), rec.entry_out.attr.mode);
            try testing.expectEqual(@as(u32, 4), rec.dirent.type);
            try testing.expectEqual(@as(u64, 1), rec.entry_out.entry_valid);
            try testing.expectEqual(@as(u64, 1), rec.entry_out.attr_valid);
        }
        off += abi.fuse_direntplus.recordLen(rec.dirent.namelen);
    }
    try testing.expectEqual(@as(usize, 4), n);
    try testing.expect(seen_sub);
    try testing.expectEqual(l0 + 1, h.opens.get(a).?.lookups);
    try testing.expectEqual(d, (try h.entry(h.lookup(sub, ".."))).nodeid);
}

test "every reply's errno is a row of the ONE table" {
    var h: Harness = undefined;
    try h.init();
    defer h.deinit();
    const probes = [_]struct { req: codec.Request, nodeid: u64, want: errors.Error }{
        .{ .req = .{ .lookup = .{ .name = "" } }, .nodeid = 1, .want = error.InvalidArgument },
        .{ .req = .{ .getattr = .{ .flags = 0, .fh = null } }, .nodeid = 999, .want = error.FileNotFound },
        .{ .req = .{ .rmdir = .{ .name = "nope" } }, .nodeid = 1, .want = error.FileNotFound },
        .{ .req = .{ .opendir = .{ .flags = 0, .open_flags = 0 } }, .nodeid = 999, .want = error.FileNotFound },
        .{ .req = .{ .removexattr = .{ .name = "user.x" } }, .nodeid = 1, .want = error.NoAttribute },
        .{ .req = .{ .bmap = .{ .block = 0, .blocksize = 4096 } }, .nodeid = 1, .want = error.NotImplemented },
    };
    for (probes) |p| {
        const c = h.call(p.req, p.nodeid);
        try testing.expectEqual(errors.toErrno(p.want), c.errno().?);
    }
}
