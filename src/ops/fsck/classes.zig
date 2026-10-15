//! C1–C17 as WRAPPERS over the landed oracles, and THE LADDER (`ops_fsck.md`
//! §1.3, §3): every class NOMINATES suspects from the census; the ladder
//! takes each suspect through Phase A (the allocation-epoch side map = the
//! allocator's per-cell word, then the in-flight registry, then the
//! pack-open ledger — registry-absence FIRST, then the reference state), the
//! settle, and Phase C (ONE fresh census per confirm pass, memoized; the
//! object's exclusive 4a lease online) before anything is a finding.
//! Offline nothing is in flight by definition, so suspects report directly.
//! Detection never mutates. Age alone is never a verdict.
//!
//! Portable over `census.Store` + the allocator/bitmap types (both portable)
//! + the vtable faces a Linux harness binds (the pack-open ledger, the
//! crossing registry, the open-intent set, the staging dir, the forest's
//! appender census). The repair engine is `repair.zig`.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const assert = std.debug.assert;

const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const derive = @import("../../core/derive.zig");
const key = @import("../../meta/key.zig");
const record = @import("../../meta/record.zig");
const tree = @import("../../meta/tree.zig");
const inos = @import("../../meta/inos.zig");
const dlm = @import("../../meta/dlm.zig");
const economy = @import("../../meta/economy.zig");
const layout = @import("../../data/layout.zig");
const kvmap = @import("../../data/kvmap.zig");
const block_refs = @import("../../data/block_refs.zig");
const block_alloc = @import("../../data/block_alloc.zig");
const staging = @import("../../data/staging.zig");
const data_bitmap = @import("../../data/sym_planes/data_bitmap.zig");
const shared_index = @import("../../data/sym_planes/shared_index.zig");
const stripe = @import("../../meta/sym/xv/stripe.zig");
const lease_table = @import("../../meta/sym/slots/lease_table.zig");
const appender_page = @import("../../meta/sym/appender_page.zig");
const cr = @import("../../meta/sym/control_records.zig");
const fsck = @import("../fsck.zig");
const finding = @import("finding.zig");
const census = @import("census.zig");

pub const Finding = finding.Finding;
pub const Error = census.Error;
pub const Store = census.Store;

// ---------------------------------------------------------------------------
// The host faces.
// ---------------------------------------------------------------------------

pub const Volume = struct {
    vol_tag: u64,
    alloc: *block_alloc.BlockAllocator,
    /// The holder's bitmap when THIS process holds the allocation lease
    /// (the grant-armed C6 arm); null = the flat arithmetic arm.
    bitmap: ?*data_bitmap.DataBitmap = null,
    passthrough: bool = true,
    /// Blocks the volume reserves below its data (the plane tracks index
    /// 0 — the data superblock chunk — at refcount 1 forever): never a
    /// reference, never leaked.
    reserved_blocks: u64 = 0,
};

/// The pack-open ledger: the +1 PIN of an OPEN pack reads as C2/C3/C8
/// drift before its first tenant commits — resolved by `(vol_tag, block)`,
/// never by key spelling.
pub const PackLedger = struct {
    ctx: ?*anyopaque,
    isOpenPack: *const fn (?*anyopaque, vol_tag: u64, block_idx: u64) bool,
};

/// C11's crossing registry: a live train holds the ino's 4a across sweep →
/// chunks → flip, so it is consulted BEFORE the lease.
pub const CrossingRegistry = struct {
    ctx: ?*anyopaque,
    inFlight: *const fn (?*anyopaque, key_ino: u64) bool,
};

/// The open cross-volume intents (C9/C10's exemption): a create commits
/// the child's record before its `InsertDentry` ships.
pub const IntentRegistry = struct {
    ctx: ?*anyopaque,
    isOpen: *const fn (?*anyopaque, global_ino: u64) bool,
};

/// The staging dir (C4/C5) over the v4 segment + marker.
pub const StagingFace = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// Every resident custody record (owned by `gpa`).
        residents: *const fn (*anyopaque, Allocator) Error![]staging.Resident,
        /// The marker's bind verdict against this set; null = rebindable.
        marker: *const fn (*anyopaque) ?finding.StagingVerdict,
        /// The verbatim record image for the quarantine copy (owned), null = gone.
        image: *const fn (*anyopaque, Allocator, staging.StagingKey) Error!?[]u8,
        /// Retire a record at its generation; false = already gone.
        remove: *const fn (*anyopaque, staging.StagingKey, generation: u64) bool,
        /// Move the whole dir aside after its copy (C5's repair).
        quarantineDir: *const fn (*anyopaque, *Quarantine) Error!void,
    };
};

/// One appender page as the forest census sees it.
pub const PageView = struct {
    vol: u16,
    appender_id: u32,
    state: appender_page.State,
    node_token: u64,
    mount_slot: u32,
    is_manager: bool,
    /// The dead ring's un-replayed entries (`head_hint − ledger_tail_seq`).
    window_entries: u64,
    entries: []const appender_page.SlotEntry,
};

pub const Tree0Row = struct { vol: u16, slot: u16, state: cr.SlotState };

/// One appender's grant record as the census read it: every chunk of its
/// family decoded and assembled (`grant_record.Family.load`), or not.
pub const GrantView = struct { vol: u16, appender: u32, assembles: bool };

/// The forest's custody census (C14/C15): every volume's pages, every
/// volume's tree-0 `slot_state` rows, volume 0's death ledger, every
/// volume's grant records.
pub const ForestCensus = struct {
    pages: []const PageView,
    tree0: []const Tree0Row,
    deaths: []const cr.StableIdentity,
    grants: []const GrantView = &.{},
};

pub const ForestFace = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// ONE fresh census (allocated in `arena`).
        custody: *const fn (*anyopaque, Allocator) Error!ForestCensus,
        /// C13: every image a grant claims that no root reaches, content-judged.
        orphanImages: *const fn (*anyopaque, Allocator, *std.ArrayList(Finding)) Error!void,
        /// C13's repair: `free(extent)` through the appender's ring; false = healed.
        returnOrphan: *const fn (*anyopaque, vol: u16, appender: u32, extent: u64) Error!bool,
        /// C15's repair (online, the volume's MANAGER only); false = already recovered.
        recoverDead: *const fn (*anyopaque, vol: u16, appender: u32) Error!bool,
    };
};

/// C16's two sorted key lists per data volume (the SHARED-flagged kind-6
/// references and the kind-9 index entries).
pub const SharedIndexFace = struct {
    ctx: *anyopaque,
    lists: *const fn (*anyopaque, Allocator, vol_tag: u64, flagged: *std.ArrayList([]const u8), index: *std.ArrayList([]const u8)) Error!void,
};

/// The block reader C2-lost's content verify and the quarantine copy use
/// (the scrub's reader).
pub const BlockReader = struct {
    ctx: *anyopaque,
    read: *const fn (*anyopaque, vol_tag: u64, device_off: u64, dest: []u8) Error!void,
};

pub const Host = struct {
    gpa: Allocator,
    io: Io,
    store: Store,
    geo: layout.Geometry,
    name_key: *const key.NameKey,
    volumes: []const Volume,
    scope: *const census.SlotScope,
    mode: finding.Mode,
    /// The meta volume index the C1 findings name.
    meta_vol: u16 = 0,
    /// The 4a manager (online); null offline (the D0 guard is the exclusion).
    dlm: ?*dlm.Manager = null,
    pack: ?PackLedger = null,
    crossings: ?CrossingRegistry = null,
    intents: ?IntentRegistry = null,
    staging: ?StagingFace = null,
    forest: ?ForestFace = null,
    shared: ?SharedIndexFace = null,
    reader: ?BlockReader = null,
    mem_budget: u64,
    shard: ?finding.Shard = null,
    /// The fleet member's residue (`census.Options.custody_only`).
    custody_only: bool = false,
    /// `SQUEEZEFS_FSCK_SETTLE_MS` (online only).
    settle_ns: u64,
    cancel: ?*const std.atomic.Value(bool) = null,
    /// A pause is a cancel for a detection run (no partial-resume state: a
    /// paused detection re-runs whole — a stop latch, never an abort).
    pause: ?*const std.atomic.Value(bool) = null,
    hold: ?*const std.atomic.Value(bool) = null,
    /// The ladder's test hook: fires at the points a live writer's step
    /// must not split — C8 before the first round's refs scan, at a
    /// re-check round's start and between the re-check's durable and
    /// derived reads; Phase A at its registry boundary; the C2-leaked / C3
    /// re-checks between the fresh derivation and the allocator's re-read.
    recheck_hook: ?RecheckHook = null,

    pub const RecheckPoint = enum { c8_round1_refs, c8_recheck_start, c8_recheck_between_reads, phase_a_registry, count_recheck_derived };
    pub const RecheckHook = struct {
        ctx: *anyopaque,
        at: *const fn (ctx: *anyopaque, point: RecheckPoint, block_idx: u64) void,

        fn fire(self: RecheckHook, point: RecheckPoint, block_idx: u64) void {
            self.at(self.ctx, point, block_idx);
        }
    };

    /// A residue of the set — an ino shard or a fleet member's custody:
    /// the whole-set classes (C8, C11–C17) and the allocator-truth arms
    /// are the coordinator's whole run's, exactly as for an offline shard.
    pub fn residue(self: *const Host) bool {
        return self.shard != null or self.custody_only;
    }

    pub fn volume(self: *const Host, vol_tag: u64) ?*const Volume {
        for (self.volumes) |*v| if (v.vol_tag == vol_tag) return v;
        return null;
    }

    pub fn knownTags(self: *const Host, buf: []u64) []u64 {
        for (self.volumes, 0..) |v, i| buf[i] = v.vol_tag;
        return buf[0..self.volumes.len];
    }

    pub fn cancelled(self: *const Host) bool {
        if (self.cancel) |c| if (c.load(.acquire)) return true;
        if (self.pause) |p| if (p.load(.acquire)) return true;
        return false;
    }

    pub fn online(self: *const Host) bool {
        return self.mode == .online;
    }

    /// The object's exclusive 4a lease (online); offline the guard is held
    /// by construction.
    pub fn lockIno(self: *const Host, key_ino: u64) ?dlm.Guard {
        const m = self.dlm orelse return null;
        return m.lockInode(self.io, key_ino, .exclusive) catch null;
    }

    pub fn unlock(self: *const Host, g: ?dlm.Guard) void {
        if (g) |guard| guard.release(self.io);
    }

    /// ONE canonical acquisition of several inos' exclusive leases
    /// (ascending, deduped by stripe — two inos on one stripe would
    /// self-deadlock as two acquisitions). Null = offline (no manager) or
    /// more requests than the wide plan holds (the caller refuses).
    pub fn lockInos(self: *const Host, inos_: []const u64) error{PlanTooLarge}!?dlm.WideGuardSet {
        const m = self.dlm orelse return null;
        if (inos_.len > dlm.wide_requests) return error.PlanTooLarge;
        var reqs: [dlm.wide_requests]dlm.InodeReq = undefined;
        for (inos_, 0..) |i, n| reqs[n] = .{ .ino = i, .mode = .exclusive };
        return try m.lockManyWide(self.io, reqs[0..inos_.len], &.{});
    }

    pub fn unlockInos(self: *const Host, set: *?dlm.WideGuardSet) void {
        if (set.*) |*g| g.release(self.io);
        set.* = null;
    }

    /// The layout record of `key_ino` (owned value + its chain slot), null = none.
    pub fn readLayout(self: *const Host, gpa: Allocator, key_ino: u64) Error!?LayoutRec {
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        const h = key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, self.name_key);
        const start = key.xattrKey(key_ino, h, 0);
        const end = key.xattrKey(key_ino, h, std.math.maxInt(u8));
        try self.store.range(gpa, key.TREE_XATTRS, &start, &end, 256, &kvs);
        for (kvs.items) |kv| {
            if (kv.key.len != key.XATTR_KEY_LEN) continue;
            const xv = record.XattrValue.decode(kv.value) catch continue;
            if (!std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) continue;
            const value = try gpa.dupe(u8, xv.value);
            errdefer gpa.free(value);
            const l = layout.Layout.decode(value, self.geo) catch return error.Corrupt;
            return .{ .value = value, .coll_seq = kv.key[kv.key.len - 1], .layout = l };
        }
        return null;
    }

    pub fn readInode(self: *const Host, gpa: Allocator, key_ino: u64) Error!?record.InodeValue {
        const v = (try self.store.lookup(gpa, key.TREE_INODES, &key.inodeKey(key_ino))) orelse return null;
        defer gpa.free(v);
        return record.InodeValue.decode(v) catch error.Corrupt;
    }

    /// The bindings `key_ino`'s CURRENT layout names (fresh; the kvmap
    /// records expanded), null = no layout.
    pub fn currentBindings(self: *const Host, gpa: Allocator, key_ino: u64, out: *std.ArrayList(census.MappingRef)) Error!bool {
        const rec = (try self.readLayout(gpa, key_ino)) orelse return false;
        defer gpa.free(rec.value);
        if (rec.layout.isKvmap()) {
            var binds: std.ArrayList(block_refs.Binding) = .empty;
            defer binds.deinit(gpa);
            try kvmap.deriveBindings(self.store.mapStore(), gpa, key_ino, self.geo.chunk, &binds);
            for (binds.items) |b| try out.append(gpa, .{ .ino = key_ino, .block_index = b.block_index, .mapping = layout.Mapping.wholeBlock(b.vol_tag, b.block_idx * self.geo.chunk, 0) });
        } else {
            var it = rec.layout.named(self.geo.chunk);
            while (it.next()) |e| try out.append(gpa, .{ .ino = key_ino, .block_index = e.block, .mapping = e.mapping });
        }
        return true;
    }
};

pub const LayoutRec = struct { value: []u8, coll_seq: u8, layout: layout.Layout };

/// The durable-refs staging face a repair's layout publish rides (the
/// plane's own seam stages the same ops; a DAMAGED flip names the same
/// reference, so nothing is added or released).
var refs_seam_ctx: u8 = 0;
pub const refs_seam: layout.RefsSeam = .{ .ctx = &refs_seam_ctx, .stage_add = &refsStageAdd, .stage_release = &refsStageRelease };

fn refsStageAdd(_: *anyopaque, t: *@import("../../meta/tx.zig").KvTx, op: layout.RefOp) Allocator.Error!void {
    try block_refs.stageOps(t, &.{block_refs.taken(.{ .vol_tag = op.vol_tag, .block_idx = op.block_idx, .owner_ino = op.owner_ino, .block_index = op.block_index })});
}

fn refsStageRelease(_: *anyopaque, t: *@import("../../meta/tx.zig").KvTx, op: layout.RefOp) Allocator.Error!void {
    try block_refs.stageOps(t, &.{block_refs.released(.{ .vol_tag = op.vol_tag, .block_idx = op.block_idx, .owner_ino = op.owner_ino, .block_index = op.block_index })});
}

/// The quarantine (`ops_fsck.md` §8): every `put` lands BEFORE the
/// action's commit mutation; the sink is the Linux per-run dir (files +
/// the manifest, each `fdatasync`ed) or a RAM sink. "Nothing is destroyed
/// without a copy."
pub const Quarantine = struct {
    sink: ?Sink,
    records: u64 = 0,
    blocks: u64 = 0,
    bytes: u64 = 0,

    pub const Sink = struct {
        ctx: *anyopaque,
        /// One copy: the class, a label naming the object, the bytes.
        put: *const fn (*anyopaque, class: finding.Class, label: []const u8, bytes: []const u8) Error!void,
    };

    pub fn putRecord(self: *Quarantine, class: finding.Class, label: []const u8, bytes: []const u8) Error!void {
        if (self.sink) |s| try s.put(s.ctx, class, label, bytes);
        self.records += 1;
        self.bytes += bytes.len;
        fsck.stats.fsck_quarantined_records.inc();
        fsck.stats.fsck_quarantined_bytes.add(bytes.len);
    }

    pub fn putBlock(self: *Quarantine, class: finding.Class, label: []const u8, bytes: []const u8) Error!void {
        if (self.sink) |s| try s.put(s.ctx, class, label, bytes);
        self.blocks += 1;
        self.bytes += bytes.len;
        fsck.stats.fsck_quarantined_blocks.inc();
        fsck.stats.fsck_quarantined_bytes.add(bytes.len);
    }
};

// ---------------------------------------------------------------------------
// The pass: the census, the nominations, the ladder.
// ---------------------------------------------------------------------------

pub const Pass = struct {
    host: *const Host,
    gpa: Allocator,
    arena: std.heap.ArenaAllocator,
    cen: census.Census,
    dentries: census.DentryPass,
    /// Blocks a live lessee's record references (`(vol_tag, block_idx)`):
    /// the lessee's ring lags this census, so their refs are its to judge —
    /// C2/C3/C8 scope them out (`block_plane_foreign_scoped`).
    foreign_blocks: std.AutoHashMapUnmanaged(ForeignBlock, void) = .empty,
    counters: finding.Counters = .{},
    suspects: std.ArrayList(Finding) = .empty,
    findings: std.ArrayList(Finding) = .empty,
    /// The allocation epoch each volume's scan latched (Phase A's bar).
    scan_epoch: []u64,
    /// The confirm pass's memoized fresh evidence (ONE per pass).
    fresh: Fresh = .{},
    /// Candidates past the ceiling (deferred loudly).
    ceiling: u64,
    /// C9's index of the census's owners: key ino → the bindings its
    /// layout names (built at the first candidate: one pass over the
    /// owners, one probe per candidate — 0003 §AC.6).
    owner_blocks: ?std.AutoHashMapUnmanaged(u64, u32) = null,

    pub const Fresh = struct {
        cen: ?census.Census = null,
        dentries: ?census.DentryPass = null,
        forest: ?ForestCensus = null,
        /// Per volume: the confirmed C8 rows.
        c8: std.AutoHashMapUnmanaged(u64, block_refs.DriftReport) = .empty,
        c13: ?[]Finding = null,
        c16: std.AutoHashMapUnmanaged(u64, shared_index.Drift) = .empty,
        c17: ?[]stripe.Finding = null,
        /// C10's index of the fresh pass's collected names: child → its
        /// distinct `(parent, suffix)` identities (built at the first
        /// re-check: one pass over the names, one probe per candidate).
        name_counts: ?std.AutoHashMapUnmanaged(u64, u32) = null,
        /// Per volume: the fresh census's derived references (the C2-leaked,
        /// C3 and C6 re-checks share ONE derivation per volume).
        derived: std.AutoHashMapUnmanaged(u64, std.AutoHashMapUnmanaged(u64, Derived)) = .empty,
    };

    pub fn deinit(self: *Pass) void {
        const gpa = self.gpa;
        self.cen.deinit();
        self.dentries.deinit();
        self.foreign_blocks.deinit(gpa);
        self.suspects.deinit(gpa);
        self.findings.deinit(gpa);
        gpa.free(self.scan_epoch);
        if (self.fresh.cen) |*c| c.deinit();
        if (self.fresh.dentries) |*d| d.deinit();
        var it = self.fresh.c8.valueIterator();
        while (it.next()) |r| r.deinit(gpa);
        self.fresh.c8.deinit(gpa);
        self.fresh.c16.deinit(gpa);
        if (self.fresh.c13) |l| gpa.free(l);
        if (self.fresh.c17) |l| gpa.free(l);
        if (self.fresh.name_counts) |*m| m.deinit(gpa);
        var dit = self.fresh.derived.valueIterator();
        while (dit.next()) |m| m.deinit(gpa);
        self.fresh.derived.deinit(gpa);
        if (self.owner_blocks) |*m| m.deinit(gpa);
        self.arena.deinit();
        self.* = undefined;
    }

    fn censusOptions(self: *const Pass, tags: []const u64) census.Options {
        return .{ .scope = self.host.scope, .shard = self.host.shard, .custody_only = self.host.custody_only, .mem_budget = self.host.mem_budget, .geo = self.host.geo, .known_vol_tags = tags, .hold = self.host.hold };
    }

    /// Pass 1: latch the allocators' scan epochs, the census, the dentry pass.
    pub fn begin(host: *const Host) Error!Pass {
        const gpa = host.gpa;
        const epochs = try gpa.alloc(u64, host.volumes.len);
        errdefer gpa.free(epochs);
        for (host.volumes, 0..) |v, i| {
            try v.alloc.fsckBeginScan();
            epochs[i] = v.alloc.fsck_epoch.load(.acquire);
        }
        var tags_buf: [64]u64 = undefined;
        const tags = host.knownTags(&tags_buf);
        var p: Pass = .{ .host = host, .gpa = gpa, .arena = std.heap.ArenaAllocator.init(gpa), .cen = undefined, .dentries = undefined, .scan_epoch = epochs, .ceiling = census.candidateCeiling(host.mem_budget) };
        errdefer p.arena.deinit();
        const t0 = clock.monotonicNs();
        p.cen = try census.run(gpa, host.store, p.censusOptions(tags));
        errdefer p.cen.deinit();
        const t1 = clock.monotonicNs();
        p.dentries = try census.dentryPass(gpa, host.store, .{ .scope = host.scope, .shard = host.shard, .mem_budget = host.mem_budget, .ceiling = p.cen.rawCeiling(), .hold = host.hold });
        p.counters.census_ms = msBetween(t0, t1);
        p.counters.dentries_ms = msBetween(t1, clock.monotonicNs());
        // A member's custody names live in OTHER members' directories (a
        // joiner's tree under the manager's root, every cross-owner name),
        // which this mount does not read: its referenced set is incomplete
        // by construction, so the name-dependent arms (C9, C10) take no
        // verdict here — the first fleet run read B's own directory as
        // unreferenced. The block plane and C1/C4 of its slots are its.
        if (host.custody_only) p.dentries.complete = false;
        for (p.cen.owners.items) |o| if (host.scope.unjudged(census.ofKeyIno(o.ino).slot)) {
            for (o.bindings) |b| try p.foreign_blocks.put(gpa, .{ .vol_tag = b.vol_tag, .block_idx = b.block_idx }, {});
        };
        p.counters.inodes_scanned = p.cen.inodes_scanned;
        p.counters.dentry_refs_indexed = p.dentries.refs_indexed;
        p.counters.inode_plane_foreign_dentry_scoped = p.dentries.foreign_dentry_scoped;
        p.counters.inode_plane_window_scoped = p.dentries.window_scoped;
        p.counters.inode_plane_slots_covered = host.scope.slotsCovered();
        return p;
    }

    /// The allocators' scan latches off (the epoch map is not drained: the
    /// next `fsckBeginScan` bumps the epoch, a stale stamp reads OLDER).
    pub fn end(self: *Pass) void {
        for (self.host.volumes) |v| v.alloc.fsckEndScan();
    }

    fn suspect(self: *Pass, f: Finding) Allocator.Error!void {
        if (self.suspects.items.len >= self.ceiling) {
            self.counters.deferred += 1;
            return;
        }
        try self.suspects.append(self.gpa, f);
        self.counters.suspects += 1;
    }

    /// The fresh census for the confirm pass (ONE, memoized).
    fn freshCensus(self: *Pass) Error!*census.Census {
        if (self.fresh.cen == null) {
            var tags_buf: [64]u64 = undefined;
            const tags = self.host.knownTags(&tags_buf);
            self.fresh.cen = try census.run(self.gpa, self.host.store, self.censusOptions(tags));
        }
        return &self.fresh.cen.?;
    }

    /// The fresh census's derived references of one volume (the C2-leaked,
    /// C3 and C6 re-checks): derived ONCE per volume, its bindings examined
    /// counted (0003 §AC.6).
    fn recheckDerived(self: *Pass, vol_tag: u64) Error!*const std.AutoHashMapUnmanaged(u64, Derived) {
        const gop = try self.fresh.derived.getOrPut(self.gpa, vol_tag);
        if (!gop.found_existing) {
            gop.value_ptr.* = .empty;
            errdefer _ = self.fresh.derived.remove(vol_tag);
            const c = try self.freshCensus();
            errdefer gop.value_ptr.deinit(self.gpa);
            try self.derivedCounts(c, vol_tag, gop.value_ptr);
            for (c.owners.items) |o| self.counters.recheck_derived_steps += o.bindings.len;
        }
        return gop.value_ptr;
    }

    /// The fresh dentry pass with the inode-plane suspects' identities
    /// collected (ONE, memoized). Null = INCOMPLETE: the inode-plane
    /// suspects CLEAR (a name may exist beyond the tear).
    fn freshDentries(self: *Pass) Error!?*census.DentryPass {
        if (self.fresh.dentries == null) {
            var collect: std.AutoHashMapUnmanaged(u64, void) = .empty;
            defer collect.deinit(self.gpa);
            for (self.suspects.items) |s| switch (s) {
                .c9_unreferenced => |f| try collect.put(self.gpa, f.ino, {}),
                .c10_nlink_too_high, .c10_nlink_too_low => |f| try collect.put(self.gpa, f.ino, {}),
                .c10_zero_nlink_named => |f| try collect.put(self.gpa, f.ino, {}),
                .c10_dangling => |f| try collect.put(self.gpa, f.child, {}),
                else => {},
            };
            const c = try self.freshCensus();
            self.fresh.dentries = try census.dentryPass(self.gpa, self.host.store, .{ .scope = self.host.scope, .shard = self.host.shard, .mem_budget = self.host.mem_budget, .ceiling = c.rawCeiling(), .collect = &collect, .stripes = true, .hold = null });
        }
        const d = &self.fresh.dentries.?;
        return if (d.complete) d else null;
    }

    // ---- the class nominations ----

    /// Every class in the normative order (`ops_fsck.md` §1.2): C1 → C2/C3/C6
    /// → C8 → C11 → C12 → C13 → C16 → C17 → C14/C15 → C9 → C10. C7 (the
    /// scrub) is the caller's, after the settle.
    pub fn nominate(self: *Pass) Error!void {
        const t0 = clock.monotonicNs();
        try self.c1();
        const t1 = clock.monotonicNs();
        self.counters.c1_ms = msBetween(t0, t1);
        defer self.counters.classes_ms = msBetween(t1, clock.monotonicNs());
        try self.c2c3c6();
        try self.c4c5();
        try self.c8();
        try self.c11();
        try self.c12();
        try self.c13();
        try self.c16();
        try self.c17();
        try self.c14c15();
        try self.c9();
        try self.c10();
    }

    // ---- C1: tree integrity (the raw walk) ----

    fn c1(self: *Pass) Error!void {
        const gpa = self.gpa;
        const host = self.host;
        var last: ?key.KeyBuf = null;
        // A merged store returns every ino-major kind inside the inode
        // kind's bounds: ONE raw pass; a partitioned store needs one per kind.
        const kinds: []const u8 = if (host.store.merged_kinds) &.{key.TREE_INODES} else &.{ key.TREE_INODES, key.TREE_DENTRIES, key.TREE_XATTRS, key.TREE_BLOCK_MAP };
        for (kinds) |kind| {
            var pager = census.KindPager.init(host.store, gpa, kind, 0);
            pager.raw = true;
            defer pager.deinit();
            last = null;
            while (true) {
                if (host.cancelled()) return;
                const kv = (pager.next() catch |e| switch (e) {
                    error.OutOfMemory => return error.OutOfMemory,
                    else => {
                        // An unreadable node: the walk stops HERE.
                        const slot = key.slotTreeKeySlot(pager.cursor.slice()) catch 0;
                        try self.suspect(.{ .c1_torn = .{ .vol = host.meta_vol, .slot = slot, .cursor = pager.cursor } });
                        break;
                    },
                }) orelse break;
                try self.c1Record(kv, &last);
            }
        }
        // The refs kind (its own key family).
        var cursor = key.volumeRange(0).lo;
        const refs_end = key.volumeRange(std.math.maxInt(u64)).hi;
        last = null;
        while (true) {
            var page: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(gpa, &page);
            host.store.range(gpa, key.TREE_BLOCK_REFS, cursor.slice(), refs_end.slice(), fsck.SCAN_PAGE, &page) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => {
                    try self.suspect(.{ .c1_torn = .{ .vol = host.meta_vol, .slot = 0, .cursor = cursor } });
                    break;
                },
            };
            if (page.items.len == 0) break;
            for (page.items) |kv| try self.c1Record(kv, &last);
            if (page.items.len < fsck.SCAN_PAGE) break;
            cursor = key.sameKindSuccessor(page.items[page.items.len - 1].key) orelse break;
        }
    }

    /// (b) ascending keys, (c) the kind's schema, (d) the forest codec —
    /// the kind judged is the KEY's own (a raw walk sees every kind). A
    /// record of a slot the scope does not judge (a live lessee's tree
    /// changing under the walk) is scoped out and counted, never judged.
    fn c1Record(self: *Pass, kv: tree.KeyValue, last: *?key.KeyBuf) Error!void {
        const host = self.host;
        const kb = key.KeyBuf.fromSlice(kv.key);
        if (key.slotTreeKeySlot(kv.key)) |fslot| {
            const routing_slot: u16 = if (fslot == key.NATIVE_FOREST_SLOT) 0 else @intCast(fslot - 1);
            if (host.scope.unjudged(routing_slot)) {
                self.counters.c1_projection_slots_scoped += 1;
                return;
            }
        } else |_| {}
        if (key.classifyRefusedKey(kv.key)) |d| {
            const slot: u32 = switch (d) {
                .slot_out_of_namespace => |s| s.slot,
                else => 0,
            };
            const defect: finding.RawKeyDefect = switch (d) {
                .malformed_known_kind => .malformed_known_kind,
                .unknown_kind => .unknown_kind,
                .truncated => .truncated,
                .slot_out_of_namespace => .slot_out_of_namespace,
            };
            try self.suspect(.{ .c1_raw_key = .{ .vol = host.meta_vol, .slot = slot, .key = kb, .defect = defect } });
            return;
        }
        const slot = key.slotTreeKeySlot(kv.key) catch 0;
        const kind = key.slotTreeKeyKind(kv.key) catch unreachable; // classified above
        if (last.*) |l| if (!key.lessThan(l.slice(), kv.key)) {
            try self.suspect(.{ .c1_semantic = .{ .vol = host.meta_vol, .slot = slot, .key = kb } });
        };
        last.* = kb;
        if (!recordSatisfiesSchema(kind, kv.key, kv.value, host.geo)) try self.suspect(.{ .c1_semantic = .{ .vol = host.meta_vol, .slot = slot, .key = kb } });
    }

    // ---- C4 / C5: the staging dir ----

    /// C4: a live custody record whose ino has no live record (`NotFound`
    /// or `nlink == 0`); sharded by the custody's ino. C5: the marker's
    /// bind verdict against this set.
    fn c4c5(self: *Pass) Error!void {
        const face = self.host.staging orelse return;
        const gpa = self.gpa;
        const residents = try face.vtable.residents(face.ctx, gpa);
        defer gpa.free(residents);
        for (residents) |r| {
            const ino = r.key.ino();
            if (self.host.shard) |sh| if (!sh.owns(census.globalOfKeyIno(ino))) continue;
            // The staging dir is the data plane's: a joiner stages through
            // it too, so a custody may name an ino of a slot this census
            // does not judge (a live lessee's, changing under the walk) or
            // one minted in the current era (its record may follow the
            // custody). Scoped out and counted: its holder judges it.
            const sl = census.ofKeyIno(ino);
            if (self.host.scope.unjudged(sl.slot) or self.host.scope.era(sl) != .prior) {
                self.counters.c4_custody_scoped += 1;
                continue;
            }
            const v = self.host.readInode(gpa, ino) catch continue; // an infrastructure error is never a finding
            if (v == null or v.?.nlink == 0) try self.suspect(.{ .c4_orphan = .{ .file_id = r.key.file_id, .ino = ino } });
        }
        if (face.vtable.marker(face.ctx)) |verdict| try self.suspect(.{ .c5_staging = .{ .verdict = verdict } });
    }

    // ---- C2 / C3 / C6: the allocator vs the census ----

    /// One block's derived references and its FIRST owner in census order
    /// (C2-lost names it: indexed with the count, never a scan per block).
    const Derived = struct { count: u32, ino: u64, block_index: u32 };

    /// Derived references per block of one volume (owners' bindings).
    fn derivedCounts(self: *Pass, cen: *const census.Census, vol_tag: u64, out: *std.AutoHashMapUnmanaged(u64, Derived)) Allocator.Error!void {
        for (cen.owners.items) |o| for (o.bindings) |b| if (b.vol_tag == vol_tag) {
            (try out.getOrPutValue(self.gpa, b.block_idx, .{ .count = 0, .ino = o.ino, .block_index = b.block_index })).value_ptr.count += 1;
        };
    }

    fn c2c3c6(self: *Pass) Error!void {
        const gpa = self.gpa;
        const host = self.host;
        // A fleet member is a non-holder: its allocator is a pulled window,
        // never the truth these arms compare against (the coordinator's).
        if (host.custody_only) return;
        // The reference-count arms (C3, the leaked arm) compare the
        // allocator's truth against the references THIS forest shows; a
        // live lessee's slot trees left it at the grant (`forgetTree`) with
        // every record they carry — a predecessor's files included — so
        // while any lessee is live the count is incomplete by construction
        // and the arms take no verdict (counted). The members' derived
        // censuses merged at the coordinator are the fleet's next step;
        // the lost arm (a reference the allocator does not track) stays.
        const counts_complete = host.scope.foreign_live.count() == 0;
        const sharded = host.residue() or !counts_complete;
        if (!counts_complete) self.counters.alloc_census_declined += 1;
        for (host.volumes) |v| {
            var derived: std.AutoHashMapUnmanaged(u64, Derived) = .empty;
            defer derived.deinit(gpa);
            try self.derivedCounts(&self.cen, v.vol_tag, &derived);
            const cap = v.alloc.capacityBlocks();
            const highest = v.alloc.highestBlockIndex();
            // Lost: referenced but untracked / past capacity (every shard
            // judges this arm; the bitmap's SET bit exempts a former
            // lessee's mint — the bitmap is the allocation truth).
            var it = derived.iterator();
            while (it.next()) |e| {
                const idx = e.key_ptr.*;
                if (self.foreign_blocks.contains(.{ .vol_tag = v.vol_tag, .block_idx = idx })) {
                    self.counters.block_plane_foreign_scoped += 1;
                    continue;
                }
                self.counters.blocks_checked += 1;
                const out_of_range = idx >= cap;
                const tracked = !out_of_range and v.alloc.refcount(idx) != null;
                if (tracked) {
                    self.counters.refcounts_checked += 1;
                    if (!sharded) {
                        const rc = v.alloc.refcount(idx).?;
                        if (rc != e.value_ptr.count) try self.suspect(.{ .c3_refcount = .{ .vol_tag = v.vol_tag, .block_idx = idx, .expected = e.value_ptr.count, .actual = rc } });
                    }
                    continue;
                }
                if (!out_of_range) if (v.bitmap) |bm| if (bm.isSet(idx)) {
                    self.counters.alloc_bitmap_tracked_exempted += 1;
                    continue;
                };
                self.counters.c2_owner_steps += 1;
                try self.suspect(.{ .c2_lost = .{ .vol_tag = v.vol_tag, .block_idx = idx, .ino = e.value_ptr.ino, .block_index = e.value_ptr.block_index, .unrepairable_shape = out_of_range } });
            }
            // Leaked: tracked with zero referencers (allocator truth is
            // mount-session RAM — meaningless on a shard).
            if (!sharded) {
                var idx: u64 = v.reserved_blocks;
                while (idx < highest) : (idx += 1) {
                    if (v.alloc.refcount(idx) == null) continue;
                    if (derived.contains(idx)) continue;
                    try self.suspect(.{ .c2_leaked = .{ .vol_tag = v.vol_tag, .block_idx = idx } });
                }
                try self.c6(v, &derived);
            }
        }
        // Unresolvable bindings are lost by construction.
        for (self.cen.unresolvable.items) |m| try self.suspect(.{ .c2_lost = .{ .vol_tag = m.mapping.vol_tag, .block_idx = m.mapping.blockIdx(host.geo.chunk), .ino = m.ino, .block_index = m.block_index, .unrepairable_shape = true } });
    }

    /// C6's two arms: the bitmap oracle on a grant-armed holder (LOSS =
    /// referenced ∧ clear is the finding; LEAK counted), else the flat
    /// arithmetic `used = highest − free − inflight − graced` vs tracked.
    fn c6(self: *Pass, v: Volume, derived: *const std.AutoHashMapUnmanaged(u64, Derived)) Error!void {
        const gpa = self.gpa;
        if (v.bitmap) |bm| {
            const words = bm.alloc.bitmapWords().len;
            const referenced = try gpa.alloc(u64, words);
            defer gpa.free(referenced);
            const granted = try gpa.alloc(u64, words);
            defer gpa.free(granted);
            @memset(referenced, 0);
            @memset(granted, 0);
            var it = derived.keyIterator();
            while (it.next()) |idx| if (idx.* / 64 < words) {
                referenced[@intCast(idx.* / 64)] |= @as(u64, 1) << @intCast(idx.* % 64);
            };
            // The live-owner shield: the in-flight registry's blocks are
            // granted, not leaked.
            var idx: u64 = 0;
            const highest = v.alloc.highestBlockIndex();
            while (idx < highest) : (idx += 1) if (v.alloc.inflightContains(idx)) {
                granted[@intCast(idx / 64)] |= @as(u64, 1) << @intCast(idx % 64);
            };
            const drift = try bm.verifyBitmap(referenced, granted, null);
            if (drift.loss != 0) try self.suspect(.{ .c6_drift = .{ .vol_tag = v.vol_tag, .loss = drift.loss, .leak = drift.leak } });
            return;
        }
        if (v.alloc.grant_source != null) {
            // A grant-armed allocator whose holding this process does not
            // keep (a wire writer): declined.
            self.counters.alloc_census_declined += 1;
            return;
        }
        const highest = v.alloc.highestBlockIndex();
        var tracked: u64 = 0;
        var inflight_untracked: u64 = 0;
        var idx: u64 = 0;
        while (idx < highest) : (idx += 1) {
            if (v.alloc.refcount(idx) != null) {
                tracked += 1;
            } else if (v.alloc.inflightContains(idx)) switch (v.alloc.stateOf(idx)) {
                // A free-listed block is `freeBlocksCount`'s and a trim-
                // windowed one `trimClaimed`'s: a registration on either (a
                // release held past its free, decision 0003 §AH.1; the trim
                // claim's guard) is not a second exemption.
                .free, .trim_windowed => {},
                else => inflight_untracked += 1,
            };
        }
        const used = highest -| v.alloc.freeBlocksCount() -| inflight_untracked -| v.alloc.trimClaimed();
        if (used != tracked) try self.suspect(.{ .c6_drift = .{ .vol_tag = v.vol_tag, .loss = used -| tracked, .leak = tracked -| used } });
    }

    // ---- C8: durable vs derived references ----

    fn c8(self: *Pass) Error!void {
        // The durable-vs-derived drift needs every reference in view (the
        // live-lessee law of `c2c3c6`).
        if (self.host.residue() or self.host.scope.foreign_live.count() != 0) return;
        if (self.host.recheck_hook) |h| h.fire(.c8_round1_refs, 0);
        for (self.host.volumes) |v| {
            var x: C8Exempt = .{ .pass = self, .vol_tag = v.vol_tag };
            var rep = block_refs.verifyRefs(self.gpa, self.host.store.refStore(), v.vol_tag, self.cen.owners.items, x.exemptions()) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => continue,
            };
            defer rep.deinit(self.gpa);
            self.counters.pack_ledger_exempted += rep.pack_exempted;
            self.counters.inflight_exempted += rep.inflight_exempted;
            self.counters.block_plane_foreign_scoped += rep.foreign_exempted;
            for (rep.rows) |r| try self.suspect(.{ .c8_drift = .{ .vol_tag = v.vol_tag, .block_idx = r.block_idx, .durable = r.durable, .derived = r.derived } });
        }
    }

    /// C8's exemption context: the host's open-pack and in-flight faces,
    /// the pass's foreign blocks — all of ONE volume (an index alone names
    /// a block on every volume: B's open pack at N hid A's drift at N).
    const C8Exempt = struct {
        pass: *Pass,
        vol_tag: u64,

        fn exemptions(self: *C8Exempt) block_refs.Exemptions {
            return .{ .ctx = self, .isOpenPack = &isOpenPack, .isInflight = &isInflight, .isForeignRef = &isForeignRef };
        }

        fn isOpenPack(ctx: ?*anyopaque, block_idx: u64) bool {
            const self: *C8Exempt = @ptrCast(@alignCast(ctx.?));
            const p = self.pass.host.pack orelse return false;
            return p.isOpenPack(p.ctx, self.vol_tag, block_idx);
        }

        fn isInflight(ctx: ?*anyopaque, block_idx: u64) bool {
            const self: *C8Exempt = @ptrCast(@alignCast(ctx.?));
            const v = self.pass.host.volume(self.vol_tag) orelse return false;
            return block_idx < v.alloc.capacityBlocks() and v.alloc.inflightContains(block_idx);
        }

        fn isForeignRef(ctx: ?*anyopaque, block_idx: u64) bool {
            const self: *C8Exempt = @ptrCast(@alignCast(ctx.?));
            return self.pass.foreign_blocks.contains(.{ .vol_tag = self.vol_tag, .block_idx = block_idx });
        }
    };

    // ---- C11: the map plane ----

    fn c11(self: *Pass) Error!void {
        if (self.host.residue()) return;
        const gpa = self.gpa;
        const host = self.host;
        const calls0 = if (host.store.tally) |t| t.calls else 0;
        defer self.counters.c11_store_calls += (if (host.store.tally) |t| t.calls else 0) - calls0;
        const ms = host.store.mapStore();
        // (a) + (c): every owner of the map kind off ONE paged walk (0003
        // §AC.6: a page per SCAN_PAGE entries — a merged store returns
        // every ino-major kind inside the bounds and the pager filters —
        // where a probe per ino descended once per inode); an owner's
        // further records in the walk are skipped.
        var pager = census.KindPager.init(host.store, gpa, key.TREE_BLOCK_MAP, 0);
        defer pager.deinit();
        var last: ?u64 = null;
        while (true) {
            if (host.cancelled()) return;
            const kv = (pager.next() catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => break,
            }) orelse break;
            const owner = std.mem.readInt(u64, kv.key[0..8], .big);
            if (last == owner) continue;
            last = owner;
            if (host.crossings) |c| if (c.inFlight(c.ctx, owner)) {
                self.counters.crossing_exempted += 1;
                continue;
            };
            const rep = kvmap.verifyBlockMap(ms, gpa, owner, host.geo.chunk) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => continue,
            };
            if (!(try self.liveKvmapHead(owner))) try self.suspect(.{ .c11_orphan_map_records = .{ .ino = owner, .records = rep.records } });
            if (rep.run_foreign_shadows != 0) try self.suspect(.{ .c11_run_foreign_shadow = .{ .ino = owner, .shadows = rep.run_foreign_shadows } });
        }
        // (b): the census's nominees.
        for (self.cen.kvmap_nominees.items) |ino| {
            if (host.crossings) |c| if (c.inFlight(c.ctx, ino)) {
                self.counters.crossing_exempted += 1;
                continue;
            };
            const rep = kvmap.verifyBlockMap(ms, gpa, ino, host.geo.chunk) catch continue;
            if (rep.records == 0) try self.suspect(.{ .c11_empty_kvmap_head = .{ .ino = ino } });
        }
    }

    /// A live inode record whose layout head is kvmap-class.
    fn liveKvmapHead(self: *Pass, key_ino: u64) Error!bool {
        const gpa = self.gpa;
        const v = (self.host.readInode(gpa, key_ino) catch return false) orelse return false;
        _ = v;
        const rec = (self.host.readLayout(gpa, key_ino) catch return false) orelse return false;
        defer gpa.free(rec.value);
        return rec.layout.isKvmap();
    }

    // ---- C12: tenant ranges ----

    const Window = struct { off: u64, len: u64, ino: u64, block_index: u32 };

    fn c12(self: *Pass) Error!void {
        if (self.host.residue()) return;
        try c12Over(self, &self.cen, true);
        try self.c12Overrun();
    }

    /// One interval sort per `(vol_tag, block_idx, incarnation)`; same-`off`
    /// windows collapse to the longest; a window starting inside the
    /// furthest reach at a DIFFERENT `off` is the pair finding.
    fn c12Over(self: *Pass, cen: *const census.Census, nominate_: bool) Error!void {
        const gpa = self.gpa;
        const chunk = self.host.geo.chunk;
        const Key = struct { vol_tag: u64, block_idx: u64, inc: u64 };
        var groups: std.AutoArrayHashMapUnmanaged(Key, std.ArrayList(Window)) = .empty;
        defer {
            for (groups.values()) |*l| l.deinit(gpa);
            groups.deinit(gpa);
        }
        for (cen.mappings.items) |m| {
            if (m.mapping.damaged) continue;
            const k: Key = .{ .vol_tag = m.mapping.vol_tag, .block_idx = m.mapping.blockIdx(chunk), .inc = m.mapping.incarnation };
            const gop = try groups.getOrPut(gpa, k);
            if (!gop.found_existing) gop.value_ptr.* = .empty;
            try gop.value_ptr.append(gpa, .{ .off = m.mapping.rel_off, .len = m.mapping.slotLen(chunk), .ino = m.ino, .block_index = m.block_index });
        }
        var it = groups.iterator();
        while (it.next()) |e| {
            const ws = e.value_ptr.items;
            if (ws.len < 2) continue;
            std.mem.sort(Window, ws, {}, struct {
                fn lt(_: void, a: Window, b: Window) bool {
                    if (a.off != b.off) return a.off < b.off;
                    return a.len > b.len;
                }
            }.lt);
            var reach: Window = ws[0];
            for (ws[1..]) |w| {
                if (w.off == reach.off) continue; // same off = a prefix of one image (clone / clip)
                if (w.off < reach.off + reach.len) {
                    if (nominate_) try self.suspect(c12Pair(e.key_ptr.vol_tag, e.key_ptr.block_idx, reach, w)) else try self.findings.append(gpa, c12Pair(e.key_ptr.vol_tag, e.key_ptr.block_idx, reach, w));
                }
                if (w.off + w.len > reach.off + reach.len) reach = w;
            }
        }
    }

    fn c12Pair(vol_tag: u64, block_idx: u64, a: Window, b: Window) Finding {
        // The pair's identity is order-free: the lower (ino, index) first.
        const first = a.ino < b.ino or (a.ino == b.ino and a.block_index <= b.block_index);
        const x = if (first) a else b;
        const y = if (first) b else a;
        return .{ .c12_overlap = .{ .vol_tag = vol_tag, .block_idx = block_idx, .ino_a = x.ino, .block_index_a = x.block_index, .ino_b = y.ino, .block_index_b = y.block_index } };
    }

    /// The Overrun arm: the class's OWN tolerant reader over the raw layout
    /// bytes — the read funnel refuses every violation with EIO, so without
    /// this arm fsck would never say why.
    fn c12Overrun(self: *Pass) Error!void {
        const gpa = self.gpa;
        var pager = census.KindPager.init(self.host.store, gpa, key.TREE_XATTRS, 0);
        defer pager.deinit();
        while (true) {
            const kv = (pager.next() catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => return,
            }) orelse break;
            const xv = record.XattrValue.decode(kv.value) catch continue;
            if (!std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) continue;
            const key_ino = std.mem.readInt(u64, kv.key[0..8], .big);
            if (self.host.scope.unjudged(census.ofKeyIno(key_ino).slot)) continue;
            const V = struct {
                pass: *Pass,
                ino: u64,
                fn visit(ctx: *@This(), w: RawWindow) Error!void {
                    const m = wireFields(w.wire);
                    const chunk = ctx.pass.host.geo.chunk;
                    if (m.check(chunk)) |_| {} else |_| {
                        try ctx.pass.suspect(.{ .c12_overrun = .{ .vol_tag = m.vol_tag, .block_idx = m.offset / chunk, .ino = ctx.ino, .block_index = w.block_index } });
                    }
                }
            };
            var v: V = .{ .pass = self, .ino = key_ino };
            try forEachRawWindow(xv.value, &v, V.visit);
        }
    }

    // ---- C13 / C16 / C17 / C14+C15: the forest and the data-plane oracles ----

    fn c13(self: *Pass) Error!void {
        if (self.host.residue()) return;
        const f = self.host.forest orelse return;
        var out: std.ArrayList(Finding) = .empty;
        defer out.deinit(self.gpa);
        try f.vtable.orphanImages(f.ctx, self.gpa, &out);
        for (out.items) |x| try self.suspect(x);
    }

    fn c16(self: *Pass) Error!void {
        if (self.host.residue()) return;
        const face = self.host.shared orelse return;
        for (self.host.volumes) |v| {
            const d = try sharedDrift(self.gpa, face, v.vol_tag);
            if (!d.clean()) try self.suspect(.{ .c16_shared_index_drift = .{ .vol_tag = v.vol_tag, .flag_without_entry = d.flag_without_entry, .entry_without_flag = d.entry_without_flag } });
        }
    }

    fn c17(self: *Pass) Error!void {
        if (self.host.residue()) return;
        var out: std.ArrayList(stripe.Finding) = .empty;
        defer out.deinit(self.gpa);
        try stripeFindings(self, &self.cen, &self.dentries, &out);
        for (out.items) |f| try self.suspect(.{ .c17_stripe = .{ .shape = f.shape, .dir = f.dir, .ino = f.ino } });
    }

    fn c14c15(self: *Pass) Error!void {
        if (self.host.residue()) return;
        const f = self.host.forest orelse return;
        const fc = try f.vtable.custody(f.ctx, self.arena.allocator());
        var out: std.ArrayList(Finding) = .empty;
        defer out.deinit(self.gpa);
        try custodyFindings(self.gpa, fc, &out);
        for (out.items) |x| try self.suspect(x);
    }

    // ---- C9: unreferenced inodes ----

    fn c9(self: *Pass) Error!void {
        if (!self.host.scope.inodePlaneJudges() or !self.cen.complete or !self.dentries.complete) return;
        if (!self.dentries.names_complete) {
            self.counters.inode_plane_names_declined += 1;
            return;
        }
        const V = struct {
            pass: *Pass,
            err: ?Error = null,
            fn visit(ctx: *@This(), sl: census.SlotLocal) bool {
                const p = ctx.pass;
                if (p.suspects.items.len >= p.ceiling) {
                    p.counters.deferred += 1;
                    return false;
                }
                const global = inos.makeGlobalIno(sl.raw, sl.slot, inos.ROUTING_WIDTH);
                // The gates in order: the lessee scope, the window, THE
                // era floor (nonzero under concurrent creates is the proof
                // the shield works), the open intent set.
                if (p.host.scope.unjudged(sl.slot)) {
                    p.counters.inode_plane_foreign_slot_scoped += 1;
                    return true;
                }
                if (p.host.scope.window_inos.contains(global)) {
                    p.counters.inode_plane_window_scoped += 1;
                    return true;
                }
                switch (p.host.scope.era(sl)) {
                    .current => {
                        p.counters.current_era_exempted += 1;
                        return true;
                    },
                    .unknown => return true,
                    .prior => {},
                }
                if (p.host.intents) |i| if (i.isOpen(i.ctx, global)) {
                    p.counters.unreferenced_intent_exempted += 1;
                    return true;
                };
                const blocks = p.ownerBlocks(census.keyInoOf(sl)) catch |e| {
                    ctx.err = e;
                    return false;
                };
                log.debug("fsck C9 candidate: ino {d} (slot {d}, raw {d}) live, prior-era, absent from the referenced set", .{ global, sl.slot, sl.raw });
                p.suspect(.{ .c9_unreferenced = .{ .ino = global, .blocks = blocks } }) catch |e| {
                    ctx.err = e;
                    return false;
                };
                return true;
            }
        };
        var v: V = .{ .pass = self };
        self.cen.live.eachAbsentFrom(&self.dentries.referenced, &v, V.visit);
        if (v.err) |e| return e;
    }

    /// The bindings the census's owner `key_ino` names (0 = no owner):
    /// the index is built at the first call — the FIRST owner of an ino
    /// in census order wins, as the scan it replaces answered.
    fn ownerBlocks(self: *Pass, key_ino: u64) Allocator.Error!u32 {
        if (self.owner_blocks == null) {
            var m: std.AutoHashMapUnmanaged(u64, u32) = .empty;
            errdefer m.deinit(self.gpa);
            for (self.cen.owners.items) |o| {
                self.counters.c9_owner_steps += 1;
                const gop = try m.getOrPut(self.gpa, o.ino);
                if (!gop.found_existing) gop.value_ptr.* = @intCast(o.bindings.len);
            }
            self.owner_blocks = m;
        }
        self.counters.c9_owner_steps += 1;
        return self.owner_blocks.?.get(key_ino) orelse 0;
    }

    // ---- C10: nlink / name consistency ----

    fn c10(self: *Pass) Error!void {
        if (!self.host.scope.inodePlaneJudges()) return;
        const gpa = self.gpa;
        // The count arms (gated on completeness).
        if (self.dentries.multi_complete and self.cen.odd_nlink_complete and self.dentries.complete and self.dentries.names_complete) {
            var it = self.cen.odd_nlink.iterator();
            while (it.next()) |e| {
                const global = e.key_ptr.*;
                if (self.unjudgedGlobal(global)) continue;
                const names = self.dentries.namesOf(global);
                self.counters.nlink_names_counted += 1;
                if (names == 0) continue; // C9's object
                if (e.value_ptr.* > names) try self.suspect(.{ .c10_nlink_too_high = .{ .ino = global, .nlink = e.value_ptr.*, .names = names } }) else if (e.value_ptr.* < names) try self.suspect(.{ .c10_nlink_too_low = .{ .ino = global, .nlink = e.value_ptr.*, .names = names } });
            }
            // Side B: multi-named inos side A did not judge (nlink == 1 files).
            var mit = self.dentries.multi.iterator();
            while (mit.next()) |e| {
                const global = e.key_ptr.*;
                if (self.cen.odd_nlink.contains(global) or self.unjudgedGlobal(global)) continue;
                const sl = census.ofGlobalIno(global) orelse continue;
                if (!self.cen.live.isSet(sl) or self.cen.dirs.isSet(sl)) continue;
                self.counters.nlink_names_counted += 1;
                if (e.value_ptr.* > 1) try self.suspect(.{ .c10_nlink_too_low = .{ .ino = global, .nlink = 1, .names = e.value_ptr.* } });
            }
        }
        // The dangerous arms: the REVERSE difference `referenced \ live`
        // (an incomplete census ⇒ no verdict: a missing live inode would
        // make its names look dangling).
        if (!self.cen.complete or !self.dentries.complete) return;
        var dangling_children: std.AutoHashMapUnmanaged(u64, void) = .empty;
        defer dangling_children.deinit(gpa);
        const V = struct {
            pass: *Pass,
            dangling: *std.AutoHashMapUnmanaged(u64, void),
            err: ?Error = null,
            fn visit(ctx: *@This(), sl: census.SlotLocal) bool {
                const p = ctx.pass;
                const global = inos.makeGlobalIno(sl.raw, sl.slot, inos.ROUTING_WIDTH);
                if (p.host.scope.unjudged(sl.slot)) {
                    p.counters.inode_plane_foreign_slot_scoped += 1;
                    return true;
                }
                if (p.host.scope.era(sl) == .unknown) return true;
                if (p.cen.corpses.isSet(sl)) {
                    p.suspect(.{ .c10_zero_nlink_named = .{ .ino = global, .names = p.dentries.namesOf(global) } }) catch |e| {
                        ctx.err = e;
                        return false;
                    };
                    return true;
                }
                // Not in the census at all: a dangling name, or the
                // census/pass window (a create between the two walks).
                ctx.dangling.put(p.gpa, global, {}) catch |e| {
                    ctx.err = e;
                    return false;
                };
                return true;
            }
        };
        var v: V = .{ .pass = self, .dangling = &dangling_children };
        self.dentries.referenced.eachAbsentFrom(&self.cen.live, &v, V.visit);
        if (v.err) |e| return e;
        if (dangling_children.count() != 0) {
            // One extra dentry walk collecting the RECORD identities of
            // the dangling children (a collision chain holds several keys
            // for one name, so the key is the identity); a child whose
            // record exists NOW is the create window.
            var collected = try census.dentryPass(gpa, self.host.store, .{ .scope = self.host.scope, .shard = self.host.shard, .mem_budget = self.host.mem_budget, .ceiling = self.cen.rawCeiling(), .collect = &dangling_children, .stripes = false });
            defer collected.deinit();
            for (collected.names.items) |rec| {
                const sl = census.ofGlobalIno(rec.child);
                const present = if (sl) |s| (self.host.readInode(gpa, census.keyInoOf(s)) catch null) != null else false;
                if (present) {
                    self.counters.nlink_transient_cleared += 1;
                    continue;
                }
                try self.suspect(.{ .c10_dangling = .{ .parent = rec.parent, .suffix = rec.suffix, .child = rec.child } });
            }
        }
        // The names the bitmap could not index: a child outside the
        // routable space is a corrupt value (dangling by construction);
        // one past the census ceiling takes the inode plane's gates — the
        // lessee scope, then ONLINE the era floor (a create during the
        // walk whose record follows its name; offline nothing mints, so a
        // name past the cursor is corrupt), then the record itself.
        for (self.dentries.unindexed.items) |rec| {
            const sl = census.ofGlobalIno(rec.child) orelse {
                try self.suspect(.{ .c10_dangling = .{ .parent = rec.parent, .suffix = rec.suffix, .child = rec.child } });
                continue;
            };
            if (self.unjudgedGlobal(rec.child)) continue;
            if (self.host.mode == .online) switch (self.host.scope.era(sl)) {
                .current => {
                    self.counters.current_era_exempted += 1;
                    continue;
                },
                .unknown => continue,
                .prior => {},
            };
            if ((self.host.readInode(gpa, census.keyInoOf(sl)) catch null) != null) {
                self.counters.nlink_transient_cleared += 1;
                continue;
            }
            try self.suspect(.{ .c10_dangling = .{ .parent = rec.parent, .suffix = rec.suffix, .child = rec.child } });
        }
    }

    fn unjudgedGlobal(self: *Pass, global: u64) bool {
        const sl = census.ofGlobalIno(global) orelse return true;
        if (self.host.scope.unjudged(sl.slot)) {
            self.counters.inode_plane_foreign_slot_scoped += 1;
            return true;
        }
        return false;
    }

    // -----------------------------------------------------------------------
    // The ladder.
    // -----------------------------------------------------------------------

    /// Settle (online): the window, then `fsckBumpEpoch` on every allocator
    /// so a block minted during the window reads YOUNGER than the scan.
    pub fn settle(self: *Pass) void {
        if (!self.host.online()) return;
        if (self.host.settle_ns != 0) self.host.io.sleep(.fromNanoseconds(@intCast(self.host.settle_ns)), .awake) catch {};
        for (self.host.volumes) |v| _ = v.alloc.fsckBumpEpoch();
    }

    /// Phase A over every suspect, then Phase C over the ones standing;
    /// the survivors are the findings. Offline the suspects report
    /// directly. Every Phase A runs before the first Phase C (decision
    /// 0003 §AH.2): the re-checks memoize ONE fresh census per pass, and a
    /// census read before a suspect's registry check can predate the
    /// publish that ended the suspect's mint — interleaved, the census the
    /// first suspect's re-check read confirmed the next one's block as
    /// leaked.
    pub fn recheck(self: *Pass) Error!void {
        const gpa = self.gpa;
        if (!self.host.online()) {
            try self.findings.appendSlice(gpa, self.suspects.items);
            self.suspects.clearRetainingCapacity();
            try self.confirmVerdicts();
            return;
        }
        var standing: std.ArrayList(Finding) = .empty;
        defer standing.deinit(gpa);
        for (self.suspects.items) |s| {
            if (self.host.cancelled()) return;
            if (!(try self.phaseA(s))) {
                self.counters.suspects_cleared += 1;
                continue;
            }
            try standing.append(gpa, s);
        }
        var kept: std.ArrayList(Finding) = .empty;
        defer kept.deinit(gpa);
        for (standing.items) |s| {
            if (self.host.cancelled()) return;
            if (try self.phaseC(s)) {
                try kept.append(gpa, s);
            } else self.counters.suspects_cleared += 1;
        }
        try self.findings.appendSlice(gpa, kept.items);
        self.suspects.clearRetainingCapacity();
        try self.confirmVerdicts();
    }

    /// The confirmed C8 rows feed the verdict site (`block_refs.confirm`:
    /// THE tripwire + the owners log) — the raw comparison convicted nothing.
    fn confirmVerdicts(self: *Pass) Error!void {
        const gpa = self.gpa;
        for (self.host.volumes) |v| {
            var rows: std.ArrayList(block_refs.Drift) = .empty;
            defer rows.deinit(gpa);
            for (self.findings.items) |f| switch (f) {
                .c8_drift => |d| if (d.vol_tag == v.vol_tag) try rows.append(gpa, .{ .block_idx = d.block_idx, .durable = d.durable, .derived = d.derived }),
                else => {},
            };
            if (rows.items.len == 0) continue;
            block_refs.confirm(gpa, self.host.store.refStore(), v.vol_tag, .{ .rows = rows.items, .pack_exempted = 0, .inflight_exempted = 0 });
        }
    }

    /// Phase A: the cross-object classes' first filters — the epoch side
    /// map (younger than the scan ⇒ exempt), the hook, the in-flight
    /// registry (registry-absence FIRST), the pack-open ledger. True = the
    /// suspect stands for Phase C.
    fn phaseA(self: *Pass, s: Finding) Error!bool {
        const block: ?struct { vol_tag: u64, idx: u64 } = switch (s) {
            .c2_leaked => |f| .{ .vol_tag = f.vol_tag, .idx = f.block_idx },
            .c2_lost => |f| .{ .vol_tag = f.vol_tag, .idx = f.block_idx },
            .c3_refcount => |f| .{ .vol_tag = f.vol_tag, .idx = f.block_idx },
            .c8_drift => |f| .{ .vol_tag = f.vol_tag, .idx = f.block_idx },
            .c12_overlap => |f| .{ .vol_tag = f.vol_tag, .idx = f.block_idx },
            .c12_overrun => |f| .{ .vol_tag = f.vol_tag, .idx = f.block_idx },
            else => null,
        };
        const b = block orelse return true;
        const v = self.host.volume(b.vol_tag) orelse return true;
        if (b.idx >= v.alloc.capacityBlocks()) return true;
        if (self.mintedUnderScan(b.vol_tag, b.idx)) return false;
        if (self.host.recheck_hook) |h| h.fire(.phase_a_registry, b.idx);
        if (self.inFlight(b.vol_tag, b.idx)) return false;
        if (self.host.pack) |p| if (p.isOpenPack(p.ctx, b.vol_tag, b.idx)) {
            self.counters.pack_ledger_exempted += 1;
            return false;
        };
        return true;
    }

    /// The epoch side map: a block minted while this pass's scan latch
    /// stood is younger than the census (counted).
    fn mintedUnderScan(self: *Pass, vol_tag: u64, idx: u64) bool {
        const vi = for (self.host.volumes, 0..) |hv, i| {
            if (hv.vol_tag == vol_tag) break i;
        } else return false;
        const epoch = self.host.volumes[vi].alloc.allocationEpochOf(idx);
        if (epoch == 0 or epoch < self.scan_epoch[vi]) return false;
        self.counters.epoch_exempted += 1;
        return true;
    }

    /// The in-flight registry: a live owner — a mint before its publish, a
    /// release between its commit and its free (decision 0003 §AH.1) —
    /// holds the block (counted).
    fn inFlight(self: *Pass, vol_tag: u64, idx: u64) bool {
        const v = self.host.volume(vol_tag) orelse return false;
        if (!v.alloc.inflightContains(idx)) return false;
        self.counters.inflight_exempted += 1;
        return true;
    }

    /// The tracking arms' re-read AFTER their fresh derivation (decision
    /// 0003 §AH.2; Phase A read the registry before the references, this
    /// reads the allocator after them): the block is still tracked, at
    /// the count the arm judged when it is C3's, with no live owner in
    /// flight and no mint under this scan — a block freed and minted to a
    /// new writer since Phase A is that writer's, never the arm's.
    fn trackingStands(self: *Pass, vol_tag: u64, idx: u64, rc_judged: ?u32) bool {
        const v = self.host.volume(vol_tag) orelse return false;
        const rc = v.alloc.refcount(idx) orelse return false;
        if (rc_judged) |j| if (rc != j) return false;
        if (self.inFlight(vol_tag, idx)) return false;
        return !self.mintedUnderScan(vol_tag, idx);
    }

    /// Phase C per class (`ops_fsck.md` §3): the fresh evidence under the
    /// object's exclusive lease. True = confirmed.
    fn phaseC(self: *Pass, s: Finding) Error!bool {
        const gpa = self.gpa;
        const host = self.host;
        switch (s) {
            .c1_torn => |f| {
                // Re-read the SAME page shape from the cursor: transient I/O clears.
                var page: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(gpa, &page);
                const kind = key.slotTreeKeyKind(f.cursor.slice()) catch key.TREE_INODES;
                host.store.range(gpa, kind, f.cursor.slice(), f.cursor.slice(), 1, &page) catch return true;
                return false;
            },
            .c1_semantic => |f| {
                const kind = key.slotTreeKeyKind(f.key.slice()) catch return true;
                const ino = if (kind == key.TREE_BLOCK_REFS) std.mem.readInt(u64, f.key.slice()[key.BLOCK_REF_OWNER_OFF..][0..8], .big) else std.mem.readInt(u64, f.key.slice()[0..8], .big);
                const g = host.lockIno(ino);
                defer host.unlock(g);
                const v = (host.store.lookup(gpa, kind, f.key.slice()) catch return true) orelse return false;
                defer gpa.free(v);
                return !recordSatisfiesSchema(kind, f.key.slice(), v, host.geo);
            },
            .c1_raw_key => |f| {
                const kind = key.slotTreeKeyKind(f.key.slice()) catch return true;
                const v = (host.store.lookup(gpa, kind, f.key.slice()) catch return true) orelse return false;
                gpa.free(v);
                return true;
            },
            .c2_leaked => |f| {
                const v = host.volume(f.vol_tag) orelse return false;
                if (v.alloc.refcount(f.block_idx) == null) return false;
                const derived = try self.recheckDerived(f.vol_tag);
                if (host.recheck_hook) |h| h.fire(.count_recheck_derived, f.block_idx);
                if (derived.contains(f.block_idx)) return false;
                return self.trackingStands(f.vol_tag, f.block_idx, null);
            },
            .c2_lost => |f| {
                if (f.unrepairable_shape) return true; // permanent by construction
                // The normative order: the ALLOCATOR first, then the
                // reference re-verified by a fresh per-ino layout read.
                const v = host.volume(f.vol_tag) orelse return true;
                if (v.alloc.refcount(f.block_idx) != null) return false;
                return try self.currentMappingPresent(f.ino, f.block_index, f.vol_tag, f.block_idx);
            },
            .c3_refcount => |f| {
                const v = host.volume(f.vol_tag) orelse return false;
                const rc = v.alloc.refcount(f.block_idx) orelse return false;
                const n = ((try self.recheckDerived(f.vol_tag)).get(f.block_idx) orelse return false).count;
                if (host.recheck_hook) |h| h.fire(.count_recheck_derived, f.block_idx);
                if (n == 0 or rc == n) return false;
                return self.trackingStands(f.vol_tag, f.block_idx, rc);
            },
            .c4_orphan => |f| {
                const face = host.staging orelse return false;
                const g = host.lockIno(f.ino);
                defer host.unlock(g);
                const img = (try face.vtable.image(face.ctx, gpa, staging.StagingKey.init(f.file_id, f.ino))) orelse return false;
                gpa.free(img);
                const v = try host.readInode(gpa, f.ino);
                return v == null or v.?.nlink == 0;
            },
            .c5_staging => {
                const face = host.staging orelse return false;
                return face.vtable.marker(face.ctx) != null;
            },
            .c6_drift => |f| {
                const v = host.volume(f.vol_tag) orelse return false;
                const derived = try self.recheckDerived(f.vol_tag);
                const before = self.suspects.items.len;
                try self.c6(v.*, derived);
                const still = self.suspects.items.len > before;
                self.suspects.items.len = before;
                return still;
            },
            .c7_scrub => return true, // the scrub's own ladder ran already
            .c8_drift => |f| return self.c8Settled(f.vol_tag, f.block_idx),
            .c9_unreferenced => |f| {
                if (host.intents) |i| if (i.isOpen(i.ctx, f.ino)) {
                    self.counters.unreferenced_intent_exempted += 1;
                    return false;
                };
                const sl = census.ofGlobalIno(f.ino) orelse return false;
                const kino = census.keyInoOf(sl);
                const g = host.lockIno(kino);
                defer host.unlock(g);
                const v = (try host.readInode(gpa, kino)) orelse return false;
                if (v.nlink == 0) return false;
                if (host.scope.era(sl) != .prior) return false;
                const d = (try self.freshDentries()) orelse return false;
                return d.namesOf(f.ino) == 0;
            },
            .c10_nlink_too_high, .c10_nlink_too_low, .c10_zero_nlink_named => return self.c10Count(s),
            .c10_dangling => |f| {
                const g = host.lockIno(f.parent);
                defer host.unlock(g);
                const dk = key.dentryKey(f.parent, f.suffix >> 8, @truncate(f.suffix));
                const v = (try host.store.lookup(gpa, key.TREE_DENTRIES, &dk)) orelse return false;
                defer gpa.free(v);
                const dv = record.DentryValue.decode(v) catch return false;
                if (dv.child_ino != f.child) return false;
                const csl = census.ofGlobalIno(f.child) orelse return true;
                return (try host.readInode(gpa, census.keyInoOf(csl))) == null;
            },
            .c11_orphan_map_records => |f| {
                if (host.crossings) |c| if (c.inFlight(c.ctx, f.ino)) {
                    self.counters.crossing_exempted += 1;
                    return false;
                };
                const g = host.lockIno(f.ino);
                defer host.unlock(g);
                if (host.crossings) |c| if (c.inFlight(c.ctx, f.ino)) {
                    self.counters.crossing_exempted += 1;
                    return false;
                };
                const rep = kvmap.verifyBlockMap(host.store.mapStore(), gpa, f.ino, host.geo.chunk) catch return true;
                if (rep.records == 0) return false; // the sweep won
                return !(try self.liveKvmapHead(f.ino));
            },
            .c11_empty_kvmap_head => |f| {
                if (host.crossings) |c| if (c.inFlight(c.ctx, f.ino)) return false;
                const g = host.lockIno(f.ino);
                defer host.unlock(g);
                const rec = (try host.readLayout(gpa, f.ino)) orelse return false;
                defer gpa.free(rec.value);
                const head = rec.layout.kvmapHead() orelse return false;
                if (head.sweep != null or rec.layout.size == 0) return false;
                const rep = kvmap.verifyBlockMap(host.store.mapStore(), gpa, f.ino, host.geo.chunk) catch return true;
                return rep.records == 0;
            },
            .c11_run_foreign_shadow => |f| {
                const g = host.lockIno(f.ino);
                defer host.unlock(g);
                const rep = kvmap.verifyBlockMap(host.store.mapStore(), gpa, f.ino, host.geo.chunk) catch return true;
                return rep.run_foreign_shadows != 0;
            },
            .c12_overlap => |f| {
                // Both layouts re-read under their inos' exclusive leases
                // held TOGETHER (ascending; one acquisition when they share
                // an ino).
                var guards = host.lockInos(&.{ f.ino_a, f.ino_b }) catch unreachable;
                defer host.unlockInos(&guards);
                var mini: census.Census = .{ .gpa = gpa, .live = census.InoBitmap.init(gpa, 2, 0), .corpses = census.InoBitmap.init(gpa, 2, 0), .dirs = census.InoBitmap.init(gpa, 2, 0) };
                defer mini.deinit();
                _ = try host.currentBindings(gpa, f.ino_a, &mini.mappings);
                if (f.ino_a != f.ino_b) _ = try host.currentBindings(gpa, f.ino_b, &mini.mappings);
                const before = self.findings.items.len;
                try self.c12Over(&mini, false);
                var still = false;
                for (self.findings.items[before..]) |x| if (x.eql(s)) {
                    still = true;
                };
                self.findings.items.len = before;
                return still;
            },
            .c12_overrun => |f| {
                const g = host.lockIno(f.ino);
                defer host.unlock(g);
                const v = (try host.store.lookup(gpa, key.TREE_INODES, &key.inodeKey(f.ino))) orelse return false;
                gpa.free(v);
                var kvs: std.ArrayList(tree.KeyValue) = .empty;
                defer tree.deinitKeyValues(gpa, &kvs);
                const h = key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, host.name_key);
                try host.store.range(gpa, key.TREE_XATTRS, &key.xattrKey(f.ino, h, 0), &key.xattrKey(f.ino, h, std.math.maxInt(u8)), 256, &kvs);
                const Still = struct {
                    f: @TypeOf(f),
                    chunk: u64,
                    still: bool = false,
                    fn visit(ctx: *@This(), w: RawWindow) Error!void {
                        const m = wireFields(w.wire);
                        if (w.block_index != ctx.f.block_index or m.vol_tag != ctx.f.vol_tag or m.offset / ctx.chunk != ctx.f.block_idx) return;
                        if (m.check(ctx.chunk)) |_| {} else |_| {
                            ctx.still = true;
                        }
                    }
                };
                var st: Still = .{ .f = f, .chunk = host.geo.chunk };
                for (kvs.items) |kv| {
                    const xv = record.XattrValue.decode(kv.value) catch continue;
                    if (!std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) continue;
                    try forEachRawWindow(xv.value, &st, Still.visit);
                }
                return st.still;
            },
            .c13_orphan_image => {
                const l = try self.freshC13();
                for (l) |x| if (x.eql(s)) return true;
                return false;
            },
            .c14_slot_custody_conflict, .c14_grant_record_corrupt, .c15_unrecovered_appender => {
                const fc = try self.freshForest();
                var out: std.ArrayList(Finding) = .empty;
                defer out.deinit(gpa);
                try custodyFindings(gpa, fc, &out);
                for (out.items) |x| if (x.eql(s)) return true;
                return false;
            },
            .c16_shared_index_drift => |f| {
                const face = host.shared orelse return false;
                const gop = try self.fresh.c16.getOrPut(gpa, f.vol_tag);
                if (!gop.found_existing) gop.value_ptr.* = try sharedDrift(gpa, face, f.vol_tag);
                const d = gop.value_ptr.*;
                return d.flag_without_entry == f.flag_without_entry and d.entry_without_flag == f.entry_without_flag and !d.clean();
            },
            .c17_stripe => |f| {
                const l = (try self.freshC17()) orelse return false;
                for (l) |x| if (x.shape == f.shape and x.dir == f.dir and x.ino == f.ino) return true;
                return false;
            },
        }
    }

    /// C10's count ladder: the `(nlink, ctime)` witness read under the
    /// ino's exclusive 4a BEFORE the fresh pass and compared AFTER; the
    /// numerator = the deduped names the fresh pass collected; the direction
    /// is decided from the verified numbers.
    fn c10Count(self: *Pass, s: Finding) Error!bool {
        const host = self.host;
        const gpa = self.gpa;
        const global: u64 = switch (s) {
            .c10_nlink_too_high, .c10_nlink_too_low => |f| f.ino,
            .c10_zero_nlink_named => |f| f.ino,
            else => unreachable,
        };
        if (host.intents) |i| if (i.isOpen(i.ctx, global)) {
            self.counters.unreferenced_intent_exempted += 1;
            return false;
        };
        const sl = census.ofGlobalIno(global) orelse return false;
        const kino = census.keyInoOf(sl);
        const g = host.lockIno(kino);
        defer host.unlock(g);
        const before = (try host.readInode(gpa, kino)) orelse return false;
        if (economy.isDirMode(before.mode)) return false;
        const d = (try self.freshDentries()) orelse return false;
        const after = (try host.readInode(gpa, kino)) orelse return false;
        if (after.nlink != before.nlink or after.ctime != before.ctime) {
            self.counters.nlink_transient_cleared += 1;
            return false;
        }
        const names = try self.dedupedNames(d, global);
        if (names == 0) return false; // C9's
        if (after.nlink == names) {
            self.counters.nlink_transient_cleared += 1;
            return false;
        }
        return switch (s) {
            .c10_nlink_too_high => after.nlink > names,
            .c10_nlink_too_low => after.nlink != 0 and after.nlink < names,
            .c10_zero_nlink_named => after.nlink == 0,
            else => unreachable,
        };
    }

    /// The deduped `(parent, name)` identities the fresh pass collected for
    /// `global` (records may only NOMINATE; a VL5b migration mid-copy has one
    /// dentry on two volumes): one pass over the names at the first call
    /// indexes every child, then one probe per candidate.
    fn dedupedNames(self: *Pass, d: *const census.DentryPass, global: u64) Allocator.Error!u32 {
        const gpa = self.gpa;
        if (self.fresh.name_counts == null) {
            const Identity = struct { child: u64, parent: u64, suffix: u64 };
            var seen: std.AutoHashMapUnmanaged(Identity, void) = .empty;
            defer seen.deinit(gpa);
            var m: std.AutoHashMapUnmanaged(u64, u32) = .empty;
            errdefer m.deinit(gpa);
            for (d.names.items) |a| {
                self.counters.c10_name_steps += 1;
                if ((try seen.getOrPut(gpa, .{ .child = a.child, .parent = a.parent, .suffix = a.suffix })).found_existing) continue;
                (try m.getOrPutValue(gpa, a.child, 0)).value_ptr.* += 1;
            }
            self.fresh.name_counts = m;
        }
        self.counters.c10_name_steps += 1;
        return self.fresh.name_counts.?.get(global) orelse 0;
    }

    /// Is `(ino, block_index)` still bound to `(vol_tag, block_idx)` in the
    /// owner's CURRENT layout? A drain frees a source only after every
    /// referencing publish is durable, so the moved-and-freed race shows
    /// the NEW mapping and clears.
    fn currentMappingPresent(self: *Pass, key_ino: u64, block_index: u32, vol_tag: u64, block_idx: u64) Error!bool {
        const g = self.host.lockIno(key_ino);
        defer self.host.unlock(g);
        var binds: std.ArrayList(census.MappingRef) = .empty;
        defer binds.deinit(self.gpa);
        if (!(try self.host.currentBindings(self.gpa, key_ino, &binds))) return false;
        for (binds.items) |b| if (b.block_index == block_index and b.mapping.vol_tag == vol_tag and b.mapping.blockIdx(self.host.geo.chunk) == block_idx) return true;
        return false;
    }

    /// C8's Phase C, per block: the block's durable references and every
    /// candidate owner's CURRENT bindings read under the owners' exclusive
    /// leases held TOGETHER. A publish and a destroy each move a block's
    /// reference and its layout in ONE tx under the owner's lease, so the
    /// two reads see one state. An owner that appeared between the
    /// candidate read and the acquisition (a re-mint of the block) is added
    /// and the round repeats, bounded. The whole-view re-check (a fresh
    /// census, then a refs scan: two walks) raced a live writer: a destroy
    /// between them read `durable 0 vs derived 1`, a publish `1 vs 0`.
    /// Rounds a C8 re-check may grow its owner set: a round grows only
    /// when the block was freed and minted to a new owner between its
    /// unlocked read and the acquisition — four in a row is a storm on ONE
    /// block, which the next online run re-judges (counted, cleared).
    const C8_SETTLE_ROUNDS: u32 = 4;

    fn c8Settled(self: *Pass, vol_tag: u64, block_idx: u64) Error!bool {
        const gpa = self.gpa;
        const host = self.host;
        var cands: std.ArrayList(u64) = .empty;
        defer cands.deinit(gpa);
        for (self.cen.owners.items) |o| for (o.bindings) |b| if (b.vol_tag == vol_tag and b.block_idx == block_idx) try addCandidate(gpa, &cands, o.ino);
        var refs: std.ArrayList(block_refs.Reference) = .empty;
        defer refs.deinit(gpa);
        var round: u32 = 0;
        while (round < C8_SETTLE_ROUNDS) : (round += 1) {
            if (host.recheck_hook) |h| h.fire(.c8_recheck_start, block_idx);
            refs.clearRetainingCapacity();
            try block_refs.blockRecords(host.store.refStore(), gpa, vol_tag, block_idx, &refs);
            for (refs.items) |r| try addCandidate(gpa, &cands, r.owner_ino);
            var guards = host.lockInos(cands.items) catch return self.c8Whole(vol_tag, block_idx);
            defer host.unlockInos(&guards);
            refs.clearRetainingCapacity();
            try block_refs.blockRecords(host.store.refStore(), gpa, vol_tag, block_idx, &refs);
            var grew = false;
            for (refs.items) |r| if (std.mem.indexOfScalar(u64, cands.items, r.owner_ino) == null) {
                grew = true;
            };
            if (grew) continue;
            if (host.recheck_hook) |h| h.fire(.c8_recheck_between_reads, block_idx);
            var derived: usize = 0;
            var binds: std.ArrayList(census.MappingRef) = .empty;
            defer binds.deinit(gpa);
            for (cands.items) |ino| {
                binds.clearRetainingCapacity();
                _ = host.currentBindings(gpa, ino, &binds) catch |e| switch (e) {
                    error.OutOfMemory => return error.OutOfMemory,
                    // A layout the codec refuses (C12's object): the
                    // census's raw-base reading is the whole view's.
                    else => return self.c8Whole(vol_tag, block_idx),
                };
                for (binds.items) |b| {
                    if (b.mapping.vol_tag == vol_tag and b.mapping.blockIdx(host.geo.chunk) == block_idx) derived += 1;
                }
            }
            return refs.items.len != derived;
        }
        self.counters.c8_owner_set_moving += 1;
        return false;
    }

    fn addCandidate(gpa: Allocator, cands: *std.ArrayList(u64), ino: u64) Allocator.Error!void {
        if (std.mem.indexOfScalar(u64, cands.items, ino) == null) try cands.append(gpa, ino);
    }

    /// The whole-view re-check: a fresh census against a fresh refs scan
    /// (offline-equivalent; the arm for an owner set wider than one lock
    /// plan or a layout the codec refuses).
    fn c8Whole(self: *Pass, vol_tag: u64, block_idx: u64) Error!bool {
        const rep = try self.freshC8(vol_tag);
        for (rep.rows) |r| if (r.block_idx == block_idx) return true;
        return false;
    }

    fn freshC8(self: *Pass, vol_tag: u64) Error!*block_refs.DriftReport {
        const gop = try self.fresh.c8.getOrPut(self.gpa, vol_tag);
        if (!gop.found_existing) {
            const c = try self.freshCensus();
            var x: C8Exempt = .{ .pass = self, .vol_tag = vol_tag };
            gop.value_ptr.* = block_refs.verifyRefs(self.gpa, self.host.store.refStore(), vol_tag, c.owners.items, x.exemptions()) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                else => blk: {
                    const empty: block_refs.DriftReport = .{ .rows = try self.gpa.alloc(block_refs.Drift, 0), .pack_exempted = 0, .inflight_exempted = 0 };
                    break :blk empty;
                },
            };
        }
        return gop.value_ptr;
    }

    fn freshC13(self: *Pass) Error![]Finding {
        if (self.fresh.c13 == null) {
            var out: std.ArrayList(Finding) = .empty;
            errdefer out.deinit(self.gpa);
            if (self.host.forest) |f| try f.vtable.orphanImages(f.ctx, self.gpa, &out);
            self.fresh.c13 = try out.toOwnedSlice(self.gpa);
        }
        return self.fresh.c13.?;
    }

    fn freshForest(self: *Pass) Error!ForestCensus {
        if (self.fresh.forest == null) {
            const f = self.host.forest orelse return .{ .pages = &.{}, .tree0 = &.{}, .deaths = &.{} };
            self.fresh.forest = try f.vtable.custody(f.ctx, self.arena.allocator());
        }
        return self.fresh.forest.?;
    }

    fn freshC17(self: *Pass) Error!?[]stripe.Finding {
        if (self.fresh.c17 == null) {
            const c = try self.freshCensus();
            const d = (try self.freshDentries()) orelse return null;
            var out: std.ArrayList(stripe.Finding) = .empty;
            errdefer out.deinit(self.gpa);
            try stripeFindings(self, c, d, &out);
            self.fresh.c17 = try out.toOwnedSlice(self.gpa);
        }
        return self.fresh.c17.?;
    }

    /// The report: the findings sorted + deduped, the counters.
    pub fn report(self: *Pass, gpa: Allocator) Allocator.Error!finding.Report {
        const list = try gpa.dupe(Finding, self.findings.items);
        const n = finding.Report.normalize(list);
        return .{ .mode = self.host.mode, .shard = self.host.shard, .findings = try gpa.realloc(list, n), .counters = self.counters };
    }
};

// ---------------------------------------------------------------------------
// Helpers shared by the nominations and the ladder.
// ---------------------------------------------------------------------------

/// Whole milliseconds between two monotonic readings (a pass's report row).
pub fn msBetween(t0: u64, t1: u64) u64 {
    return (t1 -| t0) / std.time.ns_per_ms;
}

/// The schema of one record by its kind (`record_schema_violation`).
pub fn recordSatisfiesSchema(kind: u8, k: []const u8, v: []const u8, geo: layout.Geometry) bool {
    switch (kind) {
        key.TREE_INODES => {
            _ = key.InodeKey.decode(k) catch return false;
            _ = record.InodeValue.decode(v) catch return false;
        },
        key.TREE_DENTRIES => {
            _ = key.DentryKey.decode(k) catch return false;
            _ = record.DentryValue.decode(v) catch return false;
        },
        key.TREE_XATTRS => {
            _ = key.XattrKey.decode(k) catch return false;
            const xv = record.XattrValue.decode(v) catch return false;
            if (std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) {
                // A window-law violation is C12's object, not a schema fault.
                _ = layout.Layout.decode(xv.value, geo) catch |e| return e == error.MappingRefused;
            }
        },
        key.TREE_BLOCK_MAP => {
            _ = key.BlockMapKey.decode(k) catch return false;
            _ = record.MapEntry.decode(v) catch return false;
        },
        key.TREE_BLOCK_REFS => {
            _ = key.BlockRefKey.decode(k) catch return false;
            _ = record.BlockRefValue.decode(v) catch return false;
        },
        else => return false,
    }
    return true;
}

/// §8's merged reference arms over a LIVE set: the coordinator's own
/// derived census and every member's (its custody's owners, shipped in its
/// proposal) against THIS mount's allocators — the lost arm (a block a
/// member's layout binds that the holder tracks no more: the data-loss
/// shape, a nonterminal free the holder executed under live tenants) and
/// C3 (the holder's count ≠ the merged references). The bitmap's SET bit
/// exempts a lessee's fresh carve (its reference follows), the in-flight
/// registry a mint mid-publish. The leaked arm and C8 stay the whole-view
/// run's: a leak is space (a member's carve between two censuses would read
/// as one), C8 compares durable refs the holder cannot read. The suspects
/// are the caller's to CONFIRM against a second member census (the ladder's
/// re-check with the member's own fresh evidence).
pub fn crossMemberBlockPlane(gpa: Allocator, host: *const Host, local: []const finding.OwnerRef, members: []const finding.OwnerRef, counters: *finding.Counters, out: *std.ArrayList(Finding)) Allocator.Error!void {
    const Key = struct { vol_tag: u64, block_idx: u64 };
    const Row = struct { count: u32, member_ino: u64, member_block_index: u32, member_refs: u32 };
    var rows: std.AutoHashMapUnmanaged(Key, Row) = .empty;
    defer rows.deinit(gpa);
    for (local) |o| for (o.bindings) |b| {
        const gop = try rows.getOrPutValue(gpa, .{ .vol_tag = b.vol_tag, .block_idx = b.block_idx }, .{ .count = 0, .member_ino = 0, .member_block_index = 0, .member_refs = 0 });
        gop.value_ptr.count += 1;
    };
    for (members) |o| for (o.bindings) |b| {
        const gop = try rows.getOrPutValue(gpa, .{ .vol_tag = b.vol_tag, .block_idx = b.block_idx }, .{ .count = 0, .member_ino = 0, .member_block_index = 0, .member_refs = 0 });
        gop.value_ptr.count += 1;
        gop.value_ptr.member_refs += 1;
        if (gop.value_ptr.member_refs == 1) {
            gop.value_ptr.member_ino = o.ino;
            gop.value_ptr.member_block_index = b.block_index;
        }
        counters.block_plane_member_bindings += 1;
    };
    var it = rows.iterator();
    while (it.next()) |e| {
        // Only blocks a MEMBER references are this arm's: the holder's own
        // view judged its own blocks in its whole run (or declined).
        if (e.value_ptr.member_refs == 0) continue;
        const k = e.key_ptr.*;
        const v = host.volume(k.vol_tag) orelse {
            try out.append(gpa, .{ .c2_lost = .{ .vol_tag = k.vol_tag, .block_idx = k.block_idx, .ino = e.value_ptr.member_ino, .block_index = e.value_ptr.member_block_index, .unrepairable_shape = true } });
            continue;
        };
        const out_of_range = k.block_idx >= v.alloc.capacityBlocks();
        if (!out_of_range and v.alloc.inflightContains(k.block_idx)) {
            counters.inflight_exempted += 1;
            continue;
        }
        if (!out_of_range) if (v.alloc.refcount(k.block_idx)) |rc| {
            counters.refcounts_checked += 1;
            if (rc != e.value_ptr.count) try out.append(gpa, .{ .c3_refcount = .{ .vol_tag = k.vol_tag, .block_idx = k.block_idx, .expected = e.value_ptr.count, .actual = rc } });
            continue;
        };
        if (!out_of_range) if (v.bitmap) |bm| if (bm.isSet(k.block_idx)) {
            counters.alloc_bitmap_tracked_exempted += 1;
            continue;
        };
        try out.append(gpa, .{ .c2_lost = .{ .vol_tag = k.vol_tag, .block_idx = k.block_idx, .ino = e.value_ptr.member_ino, .block_index = e.value_ptr.member_block_index, .unrepairable_shape = out_of_range } });
    }
}

/// The merged arms' re-check: a suspect stands only when the second
/// member census (and the allocator, re-read by the second evaluation)
/// name the same block the same way; a block whose reference vanished in
/// between (a delete after the first census) or that the holder tracks
/// again is cleared and counted.
pub fn confirmCrossMember(gpa: Allocator, first: []const Finding, second: []const Finding, counters: *finding.Counters, out: *std.ArrayList(Finding)) Allocator.Error!void {
    for (first) |f| {
        var stands = false;
        for (second) |g| if (Finding.eql(f, g)) {
            stands = true;
            break;
        };
        if (stands) try out.append(gpa, f) else counters.suspects_cleared += 1;
    }
}

/// A raw window of a layout record (the C12 Overrun arm's tolerant reader).
pub const RawWindow = struct { block_index: u32, wire: layout.MappingWire };

/// The mappings a layout record carries, read WITHOUT the window law: the
/// durable copy / tenant of the staged and packed kinds, every entry of an
/// inline striped map (a kvmap head carries none). The header and the
/// body framing are still required (anything else is C1's schema object).
pub fn forEachRawWindow(bytes: []const u8, ctx: anytype, comptime visit: fn (@TypeOf(ctx), RawWindow) Error!void) Error!void {
    if (bytes.len < layout.LAYOUT_HEADER_LEN) return;
    const h = std.mem.bytesToValue(layout.LayoutHeaderWire, bytes[0..layout.LAYOUT_HEADER_LEN]);
    const flags = std.mem.readInt(u16, &h.flags_le, .little);
    const body = bytes[layout.LAYOUT_HEADER_LEN..];
    switch (h.layout_kind) {
        1, 2 => {
            const with_copy = h.layout_kind == 2 or flags & layout.F_HAS_DURABLE_COPY != 0;
            if (!with_copy or body.len < 16 + layout.MAPPING_WIRE_LEN) return;
            try visit(ctx, .{ .block_index = 0, .wire = std.mem.bytesToValue(layout.MappingWire, body[16..][0..layout.MAPPING_WIRE_LEN]) });
        },
        3 => {
            if (flags & layout.F_MAP_INLINE == 0 or body.len < 4) return;
            const n: usize = std.mem.readInt(u32, body[0..4], .little);
            if (body.len - 4 != n * layout.MAP_ENTRY_WIRE_LEN) return;
            const entries = std.mem.bytesAsSlice(layout.MapEntryWire, body[4..]);
            for (entries) |e| try visit(ctx, .{ .block_index = e.block(), .wire = e.mapping });
        },
        else => {},
    }
}

/// The wire's fields WITHOUT the law (the Overrun arm runs `check` itself).
pub fn wireFields(w: layout.MappingWire) layout.Mapping {
    return .{
        .vol_tag = std.mem.readInt(u64, &w.vol_tag_le, .little),
        .offset = std.mem.readInt(u64, &w.offset_le, .little),
        .incarnation = std.mem.readInt(u64, &w.incarnation_le, .little),
        .rel_off = std.mem.readInt(u32, &w.rel_off_le, .little),
        .len = std.mem.readInt(u32, &w.len_le, .little),
        .whole_block = w.flags & layout.M_WHOLE_BLOCK != 0,
        .damaged = w.flags & layout.M_DAMAGED != 0,
    };
}

fn sharedDrift(gpa: Allocator, face: SharedIndexFace, vol_tag: u64) Error!shared_index.Drift {
    var flagged: std.ArrayList([]const u8) = .empty;
    defer {
        for (flagged.items) |k| gpa.free(k);
        flagged.deinit(gpa);
    }
    var index: std.ArrayList([]const u8) = .empty;
    defer {
        for (index.items) |k| gpa.free(k);
        index.deinit(gpa);
    }
    try face.lists(face.ctx, gpa, vol_tag, &flagged, &index);
    return shared_index.sharedIndexDrift(flagged.items, index.items);
}

/// C17 over the dentry pass's views: every directory with a marker, plus
/// every directory a map names as a stripe (its names route to it).
fn stripeFindings(p: *Pass, cen: *const census.Census, d: *const census.DentryPass, out: *std.ArrayList(stripe.Finding)) Error!void {
    const gpa = p.gpa;
    var views: std.ArrayList(stripe.DirView) = .empty;
    defer views.deinit(gpa);
    var it = d.dirs.iterator();
    while (it.next()) |e| try views.append(gpa, .{ .dir = e.key_ptr.*, .markers = e.value_ptr.*.markers.items, .names = e.value_ptr.*.names.items });
    const Has = struct {
        var census_ptr: ?*const census.Census = null;
        var scope_ptr: ?*const census.SlotScope = null;
        fn has(ino: u64) bool {
            const c = census_ptr orelse return false;
            // A stripe is named by its GLOBAL ino in the marker; the census
            // indexes key inos. A stripe minted in a slot this census does
            // not judge (a live lessee's, a foreign window's) or past the
            // start watermark (the current era) is not MISSING: its record
            // is its holder's to judge — the stripes of one directory are
            // minted across the members' slots, so a joined set's map names
            // inos of every member (the first N-daemon online run read them
            // all as `missing_stripe`).
            if (c.hasInode(ino)) return true;
            const sl = census.ofGlobalIno(ino) orelse return false;
            const s = scope_ptr orelse return false;
            return s.unjudged(sl.slot) or s.era(sl) != .prior;
        }
    };
    Has.census_ptr = cen;
    Has.scope_ptr = p.host.scope;
    defer {
        Has.census_ptr = null;
        Has.scope_ptr = null;
    }
    try stripe.verifyStripes(gpa, views.items, &Has.has, p.host.name_key, out, &p.counters.c17_route_steps);
}

/// C14 + C15 over ONE forest census: a slot `Live` on two pages, a `Live`
/// entry at tree 0's `g` under another lessee (one finding per unordered
/// pair); a dead appender's page still `Live`/`Recovering` with window
/// entries, leased slots or the manager role; a `Recovering` page the
/// ledger does not name.
const PairKey = struct { vol: u16, slot: u16, lo: u32, hi: u32 };

/// A block of one volume (`Pass.foreign_blocks`' key).
pub const ForeignBlock = struct { vol_tag: u64, block_idx: u64 };

pub fn custodyFindings(gpa: Allocator, fc: ForestCensus, out: *std.ArrayList(Finding)) Error!void {
    for (fc.grants) |g| if (!g.assembles) try out.append(gpa, .{ .c14_grant_record_corrupt = .{ .vol = g.vol, .appender = g.appender } });
    var seen: std.AutoHashMapUnmanaged(PairKey, void) = .empty;
    defer seen.deinit(gpa);
    for (fc.pages) |p| {
        if (p.state != .live and p.state != .recovering) continue;
        for (p.entries) |e| {
            const t0: ?cr.SlotState = blk: {
                for (fc.tree0) |r| if (r.vol == p.vol and r.slot == e.slot) break :blk r.state;
                break :blk null;
            };
            switch (lease_table.settleEntry(t0, e, p.appender_id)) {
                .conflict => {
                    const other = t0.?.appender_id;
                    try pushPair(gpa, &seen, out, p.vol, e.slot, p.appender_id, other);
                },
                else => {},
            }
            // The other face: another page Live on the same slot.
            for (fc.pages) |q| {
                if (q.vol != p.vol or q.appender_id == p.appender_id or q.state != .live) continue;
                for (q.entries) |qe| if (qe.slot == e.slot and qe.state == .live and e.state == .live) {
                    try pushPair(gpa, &seen, out, p.vol, e.slot, p.appender_id, q.appender_id);
                };
            }
        }
        var dead = false;
        for (fc.deaths) |d| if (d.node_token == p.node_token and d.mount_slot == p.mount_slot) {
            dead = true;
        };
        const unledgered_recovering = p.state == .recovering and !dead;
        if (unledgered_recovering or (dead and (p.window_entries > 0 or p.entries.len > 0 or p.is_manager))) {
            try out.append(gpa, .{ .c15_unrecovered_appender = .{ .vol = p.vol, .appender = p.appender_id, .node_token = p.node_token, .mount_slot = p.mount_slot, .window_entries = p.window_entries } });
        }
    }
}

fn pushPair(gpa: Allocator, seen: *std.AutoHashMapUnmanaged(PairKey, void), out: *std.ArrayList(Finding), vol: u16, slot: u16, a: u32, b: u32) Error!void {
    const lo = @min(a, b);
    const hi = @max(a, b);
    const gop = try seen.getOrPut(gpa, .{ .vol = vol, .slot = slot, .lo = lo, .hi = hi });
    if (gop.found_existing) return;
    try out.append(gpa, .{ .c14_slot_custody_conflict = .{ .vol = vol, .slot = slot, .appender_a = lo, .appender_b = hi } });
}

test {
    _ = @import("repair.zig");
}
