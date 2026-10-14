//! The §5.9 recovery driver over the REAL backend (Linux; charter §5.5
//! "Death and recovery (§5.9)", "Fencing"; `docs/porting/
//! sym_wire_membership_recovery.md` §3): `recovery.Driver`'s `Volume` seam
//! bound to one `KvBackend` this process manages — the dead appender's
//! pages through the directory (newest-valid over four slots), its ring
//! read as a replay window over its segment table (`journal.
//! replayScanImage` ≥ the page's `ledger_tail_seq`), tree 0 through the
//! control tree, the cache barrier + the writer-legal root install at ring
//! 0's head, the two-phase replay into the dead slots' trees in the
//! STRUCTURAL class (the frames the flush writes carry the recoverer's
//! `(0, g)` off the lease table's `Releasing{dead}` mark — the frame
//! plane's one source), the data-bitmap deltas folded onto the dead
//! holder's pages, barriered cycles until no slot-tree node is dirty, the
//! tails of EVERY leaf on a non-PR substrate (the resident ones under a
//! device fence), the trees' images and the grant's returns through the
//! manager's own verb (the RAM bits released: a `free(retire 0)` delta is
//! immediately reusable), the orphan census with the node-seq handle
//! floored above every stamp an orphan carries, the RAM custody
//! quarantine. `PollBinding` composes the driver into the death ledger's
//! `VolumeHook` the manager's poll drives (`wire_plane`): a dead member
//! whose region is on this volume is recovered within one poll tick of
//! its record.
//!
//! The directory chain is CACHED (a death's sweep probes every id the
//! chain could hold; the walk is re-read only when the id lands just past
//! the chain's known end and the last link grew a `next`).

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;

const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const knobs = @import("../../../core/knobs.zig");
const log = @import("../../../core/log.zig");
const checkpoint = @import("../../checkpoint.zig");
const backend = @import("../../backend.zig");
const journal = @import("../../journal.zig");
const key = @import("../../key.zig");
const node = @import("../../node.zig");
const tree = @import("../../tree.zig");
const alloc_ext = @import("../../alloc_ext.zig");
const route_table = @import("../../routing/route_table.zig");
const snapshot = @import("../../node_snapshot.zig");
const bind = @import("../bind.zig");
const page = @import("../appender_page.zig");
const dir = @import("../appender_dir.zig");
const manager = @import("../manager.zig");
const cr = @import("../control_records.zig");
const grant_record = @import("../grant_record.zig");
const grant = @import("../extent_grant.zig");
const lt = @import("../slots/lease_table.zig");
const slots_plane = @import("../slots/plane.zig");
const slots_acquire = @import("../slots/acquire.zig");
const slots_door = @import("../slots/door.zig");
const tails_spill = @import("../slots/tails_spill.zig");
const xv_set_bind = @import("../xv/set_bind.zig");
const smo = @import("../../smo.zig");
const block_alloc = @import("../../../data/block_alloc.zig");
const data_bitmap = @import("../../../data/sym_planes/data_bitmap.zig");
const records = @import("../../../data/sym_planes/records.zig");
const recovery = @import("recovery.zig");
const death_ledger = @import("death_ledger.zig");
const fencing = @import("fencing.zig");
const shard = @import("shard.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("sym/membership/volume_bind is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const KvBackend = backend.KvBackend;
const KvTree = tree.KvTree;
const Error = recovery.Error;
const WindowRecord = recovery.WindowRecord;
const WriterIdentity = recovery.WriterIdentity;
const GrantRun = cr.GrantRun;

pub const stats = struct {
    /// Data-bitmap pages rewritten by a recovery's delta fold.
    pub var recovery_data_bitmap_pages_written: counters.ShardedCounter = .{};
    /// Dead holders' allocation leases whose deltas a recovery folded.
    pub var recovery_data_bitmaps_replayed: counters.ShardedCounter = .{};
    /// A dead grant's return whose cleared count differs from the extents
    /// its record named (must-stay-0: nothing else writes a dead
    /// appender's record) — nothing released, the leak direction.
    pub var recovery_return_mismatches: counters.Tripwire = .init("recovery_return_mismatches");
};

/// One record of the dead ring's window as the replay resolves it: the
/// frame borrows the window's arena (alive through the run).
const Ref = struct {
    entry_seq: u64,
    frame: journal.RecordFrame,
};

pub const VolumeBinding = struct {
    gpa: Allocator,
    be: *KvBackend,
    ledger: *death_ledger.Ledger,
    driver: recovery.Driver,
    poll: recovery.PollBinding,
    /// The directory chain as last walked (module doc).
    links: std.ArrayList(dir.Link) = .empty,
    /// The dead ring's window read at step 3, alive through the replay.
    window: ?journal.JournalRecovery = null,
    refs: std.ArrayList(Ref) = .empty,
    /// The slots the window names (a slot with no tree and no record
    /// needs none minted).
    window_slots: std.ArrayList(u16) = .empty,
    /// The appender under recovery (the returns name it).
    recovering_id: u32 = 0,
    /// Ring 0's head at the root install: every replayed record's floor
    /// (a position in ring 0's domain; the dead ring's positions never
    /// reach ring 0's tail rule).
    install_floor: u64 = 0,
    /// Slots whose tree this run adopted fresh (the rollback removes it).
    adopted: std.ArrayList(u16) = .empty,
    /// The run's hold over the dead grant's retirements, installed on
    /// the backend's SMO context (`holdRetirementsFn`).
    retire_hold: ?smo.RetireHold = null,
    /// The run holds the SMO lock from its first root install to its
    /// replay's last record (`holdStructureFn`).
    structure_held: bool = false,
    /// The RAM custody quarantine: slot → realtime-ns deadline.
    quarantines: std.AutoHashMapUnmanaged(u16, u64) = .empty,
    /// `SQUEEZEFS_TEST_RECOVERY_HOLD`, read once here.
    hold: Hold = .{},

    /// The recovery's test hold: `park_after_flush` parks the run at the
    /// end of step 6 until the harness sets `release`.
    pub const Hold = struct {
        at: enum { off, park_after_flush } = .off,
        reached: std.Io.Event = .unset,
        release: std.Io.Event = .unset,
    };

    /// `census` = the home shard's owner (step 0's "listed LIVE again");
    /// `skew_max_ms` the shard's clock skew bound (the quarantine's term),
    /// `renew_ms` its renew interval (the coherence hold's).
    pub fn create(gpa: Allocator, be: *KvBackend, ledger: *death_ledger.Ledger, census: ?*const shard.Owner, skew_max_ms: u64, renew_ms: u64) Allocator.Error!*VolumeBinding {
        const aset = be.sym orelse unreachable;
        const self = try gpa.create(VolumeBinding);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .be = be, .ledger = ledger, .driver = undefined, .poll = undefined };
        if (std.mem.eql(u8, knobs.enumKnob("SQUEEZEFS_TEST_RECOVERY_HOLD", be.env), "park_after_flush")) self.hold.at = .park_after_flush;
        const identity: WriterIdentity = .{ .node_token = aset.identity.node_token, .mount_slot = aset.identity.mount_slot, .writer_id = aset.identity.writer_id };
        self.driver = .{
            .gpa = gpa,
            .vol = .{ .ctx = self, .vt = &vtable },
            .mount = .{
                .identity = identity,
                .meta_pr = if (be.guard) |g| g.prClient() else null,
                // The data namespaces' registrants travel with the data
                // planes' device arm (owed): the metadata preempt alone
                // decides `pr_fenced`.
                .data_pr = &.{},
                .screen = &be.slots.frames.screen,
                .stamps = &be.slots.frames.stamps,
                .ledger = ledger,
                .census = census,
                .skew_max_ms = skew_max_ms,
                .coherence_hold_ms = recovery.coherenceHoldMs(renew_ms, skew_max_ms),
            },
        };
        self.poll = .{ .driver = &self.driver, .named_ctx = self, .namedAsHolder = &namedAsHolderFn };
        // The bound in force for this volume's rings: the fixed ring's
        // size is every joiner's ceiling here (`ringCeiling` ≤ the solo
        // ring), so the gauge names the worst case a death can cost.
        recovery.stats.appender_recovery_bound_ms.set(recovery.recoveryBoundMs(be.sb.journal.len, be.sb.node_size));
        return self;
    }

    pub fn destroy(self: *VolumeBinding) void {
        self.dropWindow();
        self.refs.deinit(self.gpa);
        self.window_slots.deinit(self.gpa);
        self.links.deinit(self.gpa);
        self.adopted.deinit(self.gpa);
        self.quarantines.deinit(self.gpa);
        self.gpa.destroy(self);
    }

    pub fn hook(self: *VolumeBinding) death_ledger.VolumeHook {
        return self.poll.hook();
    }

    /// The custody quarantine's remaining term for `slot` (null = none or
    /// expired) — the grant paths' question once the custody plane binds.
    pub fn custodyQuarantineRemainingNs(self: *VolumeBinding, slot: u16, now_realtime_ns: u64) ?u64 {
        const until = self.quarantines.get(slot) orelse return null;
        if (now_realtime_ns >= until) {
            _ = self.quarantines.remove(slot);
            return null;
        }
        return until - now_realtime_ns;
    }

    fn set(self: *VolumeBinding) *bind.AppenderSet {
        return self.be.sym.?;
    }

    /// A region of THIS process (the manager's region 0, an in-process
    /// joiner) — read under the SMO lock, the regions list's own.
    fn ownsRegion(self: *VolumeBinding, appender_id: u32) bool {
        for (self.set().regions.items) |r| if (r.appenderId() == appender_id) return true;
        return false;
    }

    fn self_(ctx: *anyopaque) *VolumeBinding {
        return @ptrCast(@alignCast(ctx));
    }

    fn dropWindow(self: *VolumeBinding) void {
        if (self.window) |*w| w.deinit();
        self.window = null;
        self.refs.clearRetainingCapacity();
        self.window_slots.clearRetainingCapacity();
    }

    fn noteWindowSlot(self: *VolumeBinding, slot: u16) Allocator.Error!void {
        for (self.window_slots.items) |s| if (s == slot) return;
        try self.window_slots.append(self.gpa, slot);
    }

    fn windowNames(self: *const VolumeBinding, slot: u16) bool {
        for (self.window_slots.items) |s| if (s == slot) return true;
        return false;
    }

    fn mapBackend(e: backend.Error) Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io, error.JournalWriteFailed, error.VolumeFailed => error.Io,
            else => error.Corrupt,
        };
    }

    fn mapTree(e: tree.Error) Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
    }

    fn mapVerb(e: manager.VerbError) Error {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            error.Deferred, error.JournalReserveExhausted => error.Deferred,
            error.Rejected, error.Refused, error.NoSpace, error.Capacity => error.Refused,
            error.Corrupt => error.Corrupt,
        };
    }

    // ---- the pages ----

    fn pageReader(self: *VolumeBinding) dir.PageReader {
        return .{ .ctx = self, .read = &readPageSeam };
    }

    fn readPageSeam(ctx: *anyopaque, offset: u64, out: *[page.PAGE_LEN]u8) error{Io}!void {
        const self = self_(ctx);
        const img = self.be.dev.readRange(self.gpa, offset, page.PAGE_LEN) catch return error.Io;
        defer self.gpa.free(img);
        @memcpy(out, img[0..page.PAGE_LEN]);
    }

    fn walkLinks(self: *VolumeBinding) Error!void {
        const sb = self.be.sb;
        self.links.clearRetainingCapacity();
        dir.walkChain(self.gpa, self.pageReader(), .{ .start = sb.appender_dir.start, .len = sb.appender_dir.len }, sb.node_size, .{ .start = sb.heap.start, .len = sb.heap.len }, sb.totalExtents(), &self.links) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            error.Corrupt => error.Corrupt,
        };
    }

    /// The directory slots of `appender_id` (≥ 1) off the cached chain;
    /// null past its end.
    fn dirSlotsOf(self: *VolumeBinding, appender_id: u32) Error!?[2]u64 {
        const pairs = dir.pairCount(self.be.sb.node_size);
        const loc = dir.locate(appender_id, pairs);
        if (self.links.items.len == 0) try self.walkLinks();
        if (loc.chain_index == self.links.items.len) {
            // Just past the known end: did the last link grow a `next`?
            const last = self.links.items[self.links.items.len - 1];
            var buf: [page.PAGE_LEN]u8 = undefined;
            readPageSeam(self, last.extent.start, &buf) catch return error.Io;
            const h = dir.Header.decode(&buf) catch return error.Corrupt;
            if (h.hasNext()) try self.walkLinks();
        }
        if (loc.chain_index >= self.links.items.len) return null;
        return dir.pairOffsets(self.links.items[loc.chain_index].extent, loc.pair);
    }

    /// The newest valid image over the directory pair and — once a
    /// directory image names a first segment — the two ring-side slots.
    fn readFourSlots(self: *VolumeBinding, dir_slots: [2]u64) Error!?page.Page {
        var bufs: [page.PAGE_SLOTS][page.PAGE_LEN]u8 = undefined;
        var imgs: [page.PAGE_SLOTS]?[]const u8 = .{ null, null, null, null };
        for (dir_slots, 0..) |off, i| {
            readPageSeam(self, off, &bufs[i]) catch return error.Io;
            imgs[i] = &bufs[i];
        }
        const dir_best = page.newestValid(.{ imgs[0], imgs[1], null, null }) catch return error.Corrupt;
        if (dir_best) |p| if (p.segments.len > 0) {
            for (page.ringSideOffsets(p.segments.items[0]), 2..) |off, i| {
                readPageSeam(self, off, &bufs[i]) catch return error.Io;
                imgs[i] = &bufs[i];
            }
        };
        return page.newestValid(imgs) catch return error.Corrupt;
    }

    fn readPageFn(ctx: *anyopaque, appender_id: u32) Error!?page.Page {
        const self = self_(ctx);
        if (appender_id == journal.FIXED_RING_APPENDER) return null;
        const slots = (try self.dirSlotsOf(appender_id)) orelse return null;
        return self.readFourSlots(slots);
    }

    fn writePageFn(ctx: *anyopaque, p: *const page.Page) Error!void {
        const self = self_(ctx);
        var copy = p.*;
        self.set().manager.writeForeignPage(&copy) catch |e| return mapVerb(e);
    }

    // ---- the ring ----

    fn volFn(ctx: *anyopaque) u16 {
        return self_(ctx).be.index;
    }

    /// The dead ring as ONE logical image over its segment table (the
    /// first segment minus the ring-side page slots), scanned from the
    /// page's tail: every verified entry's records become window records
    /// the replay resolves by handle.
    fn readRingFn(ctx: *anyopaque, gpa: Allocator, p: *const page.Page, out: *std.ArrayList(WindowRecord)) Error!recovery.RingRead {
        const self = self_(ctx);
        self.dropWindow();
        self.recovering_id = p.appender_id;
        recovery.stats.appender_recovery_bound_ms.set(recovery.recoveryBoundMs(p.ringBytes(), self.be.sb.node_size));
        const part = page.ringPartOf(p.appender_id, &p.segments);
        var exts: [page.RING_SEGMENTS_MAX]journal.ExtentRef = undefined;
        for (part.slice(), 0..) |s, i| exts[i] = .{ .start = s.start, .len = s.len };
        const layout = journal.RingLayout.fromSegments(exts[0..part.len]) catch return error.Corrupt;
        const geo = journal.productionGeometry(layout.pages, self.be.dev.mode.grain());
        const image = try gpa.alloc(u8, @intCast(layout.ringBytes()));
        defer gpa.free(image);
        var off: usize = 0;
        for (part.slice()) |s| {
            const bytes = self.be.dev.readRange(gpa, s.start, @intCast(s.len)) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
            };
            defer gpa.free(bytes);
            @memcpy(image[off .. off + bytes.len], bytes);
            off += bytes.len;
        }
        self.window = try journal.replayScanImage(self.gpa, image, geo, p.ledger_tail_seq, p.appender_id);
        const rec = &self.window.?;
        if (rec.dropped_torn != 0) log.warn("recovery: appender {d}'s ring dropped {d} torn record(s) past its head — never acked, never replayed", .{ p.appender_id, rec.dropped_torn });
        const native: ?u16 = self.be.stamp.native_slot;
        for (rec.entries) |e| {
            var it = e.records();
            while (it.next()) |f| {
                const tag = journal.untag(f.tag);
                const handle: u64 = self.refs.items.len;
                try self.refs.append(self.gpa, .{ .entry_seq = e.seq, .frame = f });
                if (tag.level > 0) {
                    if (tag.tree_id != key.KIND_INTERIOR) continue;
                    const split = key.splitInteriorJournalKey(f.key) catch return error.Corrupt;
                    const slot = routingSlot(split.slot, native);
                    try self.noteWindowSlot(slot);
                    try out.append(gpa, .{ .slot = slot, .level = tag.level, .seq = f.seq, .kind = windowKindOf(f.kind), .handle = handle });
                    continue;
                }
                if (tag.tree_id == alloc_ext.tree_alloc_reserved) {
                    if (f.key.len != alloc_ext.extent_key_len) return error.Corrupt;
                    // The window's frees are the retired images' (their
                    // extents are the orphan census's, never a claim).
                    if (f.value.len == 0 or f.value[0] != alloc_ext.rec_allocated) continue;
                    try out.append(gpa, .{ .slot = 0, .level = 0, .seq = f.seq, .kind = .alloc, .extent = std.mem.readInt(u64, f.key[0..8], .big), .handle = handle });
                    continue;
                }
                if (tag.tree_id == records.DELTA_KIND) {
                    try out.append(gpa, .{ .slot = 0, .level = 0, .seq = f.seq, .kind = .data_delta, .handle = handle });
                    continue;
                }
                if (!key.isSlotTreeKind(tag.tree_id)) continue;
                const fs = key.checkSlotTreeKey(tag.tree_id, f.key) catch return error.Corrupt;
                const key_ino = std.mem.readInt(u64, f.key[key.routeOff(tag.tree_id)..][0..8], .big);
                const local: u64 = if (key.splitGuestLocal(key_ino)) |g| g.local else key_ino;
                const slot = routingSlot(fs, native);
                try self.noteWindowSlot(slot);
                // An xattr-kind record at local 0 is an intent at the slot's
                // home (`xv/intent.homeIno`; no inode is ever local 0).
                const home = local == 0 and tag.tree_id == key.TREE_XATTRS;
                try out.append(gpa, .{ .slot = slot, .level = 0, .seq = f.seq, .kind = windowKindOf(f.kind), .local_ino = local, .home = home, .handle = handle });
            }
        }
        const seq_offset = @max(p.seq_offset, journal.seqOffsetOfWindow(rec.entries));
        return .{ .head = rec.head_pos, .seq_frontier = rec.head_pos +| seq_offset, .seq_offset = seq_offset };
    }

    /// A frame's kind as the driver's: 1:1 over the three slot-tree kinds
    /// (a `delta` replays as a delta — folded into the key's newest base —
    /// never as a tombstone).
    fn windowKindOf(k: journal.RecordKind) recovery.Kind {
        return switch (k) {
            .put => .put,
            .delta => .delta,
            .delete => .delete,
        };
    }

    fn routingSlot(fs: key.ForestSlot, native: ?u16) u16 {
        if (fs == key.NATIVE_FOREST_SLOT) return native orelse 0;
        return route_table.slotOfForestSlot(fs);
    }

    // ---- tree 0 ----

    fn slotStateFn(ctx: *anyopaque, slot: u16) Error!?cr.SlotState {
        const self = self_(ctx);
        const v = (self.be.lookup(self.gpa, key.TREE_CONTROL, cr.slotStateKey(slot).slice()) catch |e| return mapBackend(e)) orelse return null;
        defer self.gpa.free(v);
        return cr.SlotStateWire.decode(v) catch error.Corrupt;
    }

    fn leasedToFn(ctx: *anyopaque, gpa: Allocator, appender_id: u32, out: *std.ArrayList(u16)) Error!void {
        const self = self_(ctx);
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        const r = cr.rangeOf(.slot_state);
        self.be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs) catch |e| return mapBackend(e);
        for (kvs.items) |kv| {
            const slot = cr.slotOfKey(kv.key) orelse continue;
            const s = cr.SlotStateWire.decode(kv.value) catch return error.Corrupt;
            if (s.variant != .unleased and s.appender_id == appender_id) try out.append(gpa, slot);
        }
        std.mem.sort(u16, out.items, {}, std.sort.asc(u16));
    }

    /// `appender_id`'s grant record as tree 0 holds it.
    fn grantOf(self: *VolumeBinding, gpa: Allocator, appender_id: u32) Error!grant_record.Family {
        const r = cr.extentGrantFamily(appender_id);
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        self.be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs) catch |e| return mapBackend(e);
        return grant_record.Family.load(gpa, appender_id, kvs.items);
    }

    fn grantFamilyFn(ctx: *anyopaque, gpa: Allocator, appender_id: u32, out: *std.ArrayList(manager.KeyValue)) Error!void {
        const self = self_(ctx);
        const r = cr.extentGrantFamily(appender_id);
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        self.be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs) catch |e| return mapBackend(e);
        try out.ensureUnusedCapacity(gpa, kvs.items.len);
        // The slices move to `out` (the caller frees them).
        for (kvs.items) |kv| out.appendAssumeCapacity(.{ .key = kv.key, .value = kv.value });
        kvs.clearRetainingCapacity();
    }

    /// ONE ring-0 control entry in the USER class, DRAIN-AND-RETRY on a
    /// full window (the recovery never parks behind the checkpoint task:
    /// it cycles the checkpoint itself, bounded by `COVER_CYCLES_MAX`).
    fn spillTailsFn(ctx: *anyopaque, gpa: Allocator, tails: []const cr.TailEntry) Error!tails_spill.Spilled {
        const self = self_(ctx);
        return tails_spill.spill(gpa, self.set().volume(), tails) catch |e| switch (e) {
            error.NoSpace => error.Corrupt,
            error.Io => error.Io,
            error.OutOfMemory => error.OutOfMemory,
        };
    }

    fn controlEntryFn(ctx: *anyopaque, recs: []const manager.ControlRecord) Error!void {
        const self = self_(ctx);
        const vol = self.set().volume();
        var cycles: u32 = 0;
        while (true) {
            vol.vt.controlEntry(vol.ctx, recs) catch |e| switch (e) {
                error.JournalReserveExhausted => {
                    if (cycles == checkpoint.COVER_CYCLES_MAX) return error.Deferred;
                    cycles += 1;
                    _ = vol.vt.cycleCheckpoint(vol.ctx) catch return error.Io;
                    continue;
                },
                error.OutOfMemory => return error.OutOfMemory,
                error.ValueTooLarge, error.Corrupt => return error.Corrupt,
                error.Io => return error.Io,
            };
            return;
        }
    }

    /// Step 7 landed `Unleased`: the published-root mirror and the slot's
    /// cursor cell follow the record (C1b would otherwise republish the
    /// grant-time words over it), and the holder cache's LEARNT view of
    /// the slot drops — the arm learnt "leased by the dead appender" off
    /// tree 0, and a view that outlived the release answered
    /// `unreachable_` (EAGAIN) for every read of the recovered slot while
    /// the table said unleased (the manager maintains it: local).
    fn noteUnleasedFn(ctx: *anyopaque, slot: u16, state: cr.SlotState) Error!void {
        const self = self_(ctx);
        const be = self.be;
        try be.guest_published.put(self.gpa, slot, .{ .addr = state.root.addr, .seq = state.root.seq });
        if (be.stamp.native_slot != slot) be.stamp.installFloor(slot, state.cursor) catch return error.OutOfMemory;
        be.slots.cache.forget(slot);
    }

    fn tableFn(ctx: *anyopaque) *lt.LeaseTable {
        return self_(ctx).be.slots.table;
    }

    // ---- the slot trees ----

    fn forestSlotOf(self: *VolumeBinding, slot: u16) key.ForestSlot {
        if (self.be.stamp.native_slot) |ns| if (ns == slot) return key.NATIVE_FOREST_SLOT;
        return key.guestForestSlot(slot);
    }

    fn treeStateFn(ctx: *anyopaque, slot: u16) ?recovery.TreeState {
        const self = self_(ctx);
        const t = self.be.slotTree(slot) orelse return null;
        const r = t.rootPtr();
        return .{ .root = .{ .addr = r.addr, .seq = r.seq }, .floor = t.rootFloor() };
    }

    /// The cache barrier, then the install (under the SMO lock: no cycle
    /// walks the tree while its custody moves): the slot's cached nodes
    /// dropped, the root pointer stored and floored at ring 0's head, the
    /// root node loaded under the screen and verified against the pointer
    /// (a lying pointer refuses); a tree this mount holds none of is
    /// opened at the root (or minted empty for a slot the window names
    /// whose root never reached tree 0) and adopted into the forest.
    fn installRootFn(ctx: *anyopaque, slot: u16, root: cr.Root, floor: u64) Error!void {
        const self = self_(ctx);
        const be = self.be;
        const fs = self.forestSlotOf(slot);
        const took = self.lockStructure();
        defer self.unlockStructure(took);
        _ = be.cache.discardSlotNodes(fs);
        self.install_floor = floor;
        if (be.slotTree(slot)) |t| {
            if (root.addr != 0) {
                try t.storeRoot(.{ .addr = root.addr, .seq = root.seq });
                be.seq.raiseTo(root.seq);
            }
            t.setRootFloor(floor);
            be.cache.noteDyingFloor(floor);
            try verifyRoot(t);
            return;
        }
        if (root.addr == 0) {
            // No tree anywhere: a slot the window never names owns no
            // extent (its `Unleased` names root 0); one it names needs a
            // tree — an empty one replays identically to the one the dead
            // region minted in its last window.
            if (!self.windowNames(slot)) return;
            const minted = be.mintGuestTree(slot) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Corrupt,
            };
            minted.setRootFloor(floor);
            try self.adopted.append(self.gpa, slot);
            return;
        }
        const opened = KvTree.open(self.gpa, be.cache, .{ .slot = key.guestForestSlot(slot) }, .{ .addr = root.addr, .seq = root.seq }, &be.seq) catch |e| return mapTree(e);
        errdefer opened.destroy();
        opened.setRootFloor(floor);
        be.seq.raiseTo(root.seq);
        be.adoptTree(opened) catch return error.OutOfMemory;
        try self.adopted.append(self.gpa, slot);
        be.cache.noteDyingFloor(floor);
    }

    /// The root node's seq against the pointer (`install_recovered_root`).
    fn verifyRoot(t: *KvTree) Error!void {
        const r = t.rootPtr();
        const n = t.getNode(r.addr) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
            else => error.Corrupt,
        };
        defer n.release();
        if (n.node_seq != r.seq or n.tree_id != key.KIND_INTERIOR) {
            log.err("recovery: the root pointer of slot tree {?d} names node_seq {d} but extent 0x{x} holds {d} (tree id {d}) — refusing the install", .{ t.forest_slot, r.seq, r.addr, n.node_seq, n.tree_id });
            return error.Corrupt;
        }
    }

    /// The rollback's arm: the slot's nodes discarded (dirty ones
    /// included), the tree back at its pre-install words or — adopted
    /// fresh by this run — out of the forest (an installed floor published
    /// nowhere would pin ring 0's tail for ever).
    fn restoreTreeFn(ctx: *anyopaque, slot: u16, prior: ?recovery.TreeState) void {
        const self = self_(ctx);
        const be = self.be;
        const took = self.lockStructure();
        defer self.unlockStructure(took);
        _ = be.cache.discardSlotNodes(self.forestSlotOf(slot));
        const t = be.slotTree(slot) orelse return;
        if (prior) |p| {
            t.storeRoot(.{ .addr = p.root.addr, .seq = p.root.seq }) catch {};
            t.setRootFloor(p.floor);
            return;
        }
        for (self.adopted.items, 0..) |s, i| if (s == slot) {
            _ = self.adopted.swapRemove(i);
            break;
        };
        // A tree minted by this run whose root tree 0 never named: its
        // extent goes back at once.
        const r = t.rootPtr();
        const published = be.guest_published.get(slot) != null;
        be.trees_lock.lockUncancelable(be.io);
        be.trees.remove(t) catch {};
        be.trees_lock.unlock(be.io);
        if (!published) be.alloc.releaseUnpublished(be.cache.addrExtent(r.addr));
    }

    /// One window record into the dead slot's tree in the structural
    /// class: interior records to their level, content by key — floored at
    /// ring 0's head (the install floor), never at the dead ring's
    /// position. The frames the flush writes take their `(0, g)` from the
    /// cache's frame seam (the table's `Releasing{dead}` mark), the same
    /// word the driver passes.
    fn replayFn(ctx: *anyopaque, rec: WindowRecord, stamp: fencing.FrameStamp) Error!void {
        const self = self_(ctx);
        std.debug.assert(stamp.appender_id == lt.MANAGER);
        const t = self.be.slotTree(rec.slot) orelse return error.Corrupt;
        const ref = self.refs.items[@intCast(rec.handle)];
        const kind = tree.kindOf(ref.frame.kind);
        if (rec.level > 0) {
            const split = key.splitInteriorJournalKey(ref.frame.key) catch return error.Corrupt;
            if (kind == .put) {
                if (key.InteriorValue.decode(ref.frame.value)) |iv| t.seq.raiseTo(iv.childSeq()) else |_| {}
            }
            _ = t.applyReplayedInterior(split.separator, rec.level, ref.frame.seq, kind, ref.frame.value, self.install_floor) catch |e| return mapTree(e);
            return;
        }
        t.applyReplayed(ref.frame.key, ref.frame.seq, kind, ref.frame.value, self.install_floor) catch |e| return mapTree(e);
    }

    fn holdStructureFn(ctx: *anyopaque, held: bool) void {
        const self = self_(ctx);
        const be = self.be;
        if (held) be.smo_lock.lockUncancelable(be.io) else be.smo_lock.unlock(be.io);
        self.structure_held = held;
    }

    /// The SMO lock for one structural act of the recovery: taken here
    /// unless the run already holds it (`holdStructureFn`).
    fn lockStructure(self: *VolumeBinding) bool {
        if (self.structure_held) return false;
        self.be.smo_lock.lockUncancelable(self.be.io);
        return true;
    }

    fn unlockStructure(self: *VolumeBinding, took: bool) void {
        if (took) self.be.smo_lock.unlock(self.be.io);
    }

    // ---- the data-bitmap deltas ----

    /// For every `alloc_lease:` of volume 0's tree 0 held by the dead
    /// identity and homed here: the bitmap image read through its refs,
    /// loaded, the window's kind-5 deltas of the lease's term folded, the
    /// dirty pages written at `checkpoint_seq + 1` through the refs,
    /// barriered.
    fn replayDataDeltasFn(ctx: *anyopaque, identity: WriterIdentity, recs: []const WindowRecord) Error!void {
        const self = self_(ctx);
        const gpa = self.gpa;
        var deltas: std.ArrayList(data_bitmap.DataBitmap.ReplayRecord) = .empty;
        defer deltas.deinit(gpa);
        for (recs) |r| if (r.kind == .data_delta) {
            const ref = self.refs.items[@intCast(r.handle)];
            try deltas.append(gpa, .{ .seq = ref.frame.seq, .key = ref.frame.key, .value = ref.frame.value });
        };
        if (deltas.items.len == 0) return;
        var kvs: std.ArrayList(death_ledger.KeyValue) = .empty;
        defer death_ledger.freeKvs(gpa, &kvs);
        const r = cr.rangeOf(.alloc_lease);
        self.ledger.store.range(self.ledger.store.range_ctx, gpa, r.start.slice(), r.end.slice(), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        for (kvs.items) |kv| {
            const dec = cr.decodeAllocLease(kv.value) catch return error.Corrupt;
            const lease = dec.lease;
            const holder: WriterIdentity = .{ .node_token = lease.node_token, .mount_slot = lease.mount_slot, .writer_id = lease.writer_id };
            if (!holder.sameMount(identity) or lease.home_vol != self.be.index) continue;
            if (dec.count() > records.BITMAP_REFS_MAX) return error.Corrupt;
            var refs: [records.BITMAP_REFS_MAX]records.BitmapRef = undefined;
            var i: usize = 0;
            while (i < dec.count()) : (i += 1) refs[i] = dec.bitmapAt(i);
            const vol_tag = cr.volTagOfKey(kv.key) orelse return error.Corrupt;
            try self.foldOneBitmap(vol_tag, lease, refs[0..dec.count()], deltas.items);
        }
    }

    fn foldOneBitmap(self: *VolumeBinding, vol_tag: u64, lease: cr.AllocLease, refs: []const records.BitmapRef, deltas: []const data_bitmap.DataBitmap.ReplayRecord) Error!void {
        const be = self.be;
        const heaps = [_]data_bitmap.HeapGeometry{.{ .vol = be.index, .heap_start = be.sb.heap.start, .heap_len = be.sb.heap.len, .node_size = be.sb.node_size }};
        if (!data_bitmap.screenBitmapRefs(refs, lease.blocks, &heaps)) return error.Corrupt;
        var io: DeviceRegion = .{ .be = be };
        const out = try data_bitmap.foldOntoRegion(self.gpa, io.io(), refs, vol_tag, lease.blocks, lease.term, deltas, be.cp.checkpointSeq() + 1);
        if (out.bits_changed == 0) return;
        stats.recovery_data_bitmap_pages_written.add(out.pages_written);
        stats.recovery_data_bitmaps_replayed.inc();
    }

    /// The home volume's device face for the pages.
    pub const DeviceRegion = struct {
        be: *KvBackend,

        const vt: data_bitmap.RegionIo.VTable = .{ .write = &write, .read = &read, .barrier = &barrier };

        fn io(self: *DeviceRegion) data_bitmap.RegionIo {
            return .{ .ctx = self, .vtable = &vt };
        }

        fn write(ctx: *anyopaque, off: u64, bytes: []const u8) error{Io}!void {
            const self: *DeviceRegion = @ptrCast(@alignCast(ctx));
            self.be.pool.writeAt(self.be.path, off, bytes) catch return error.Io;
        }

        fn read(ctx: *anyopaque, off: u64, out: []u8) error{Io}!void {
            const self: *DeviceRegion = @ptrCast(@alignCast(ctx));
            const img = self.be.dev.readRange(self.be.gpa, off, out.len) catch return error.Io;
            defer self.be.gpa.free(img);
            @memcpy(out, img[0..out.len]);
        }

        fn barrier(ctx: *anyopaque) error{Io}!void {
            const self: *DeviceRegion = @ptrCast(@alignCast(ctx));
            self.be.dev.barrier() catch return error.Io;
        }
    };

    // ---- the flush, the tails, the images ----

    /// Barriered cycles until no node of the named slots is dirty, bounded
    /// by `COVER_CYCLES_MAX` ("a stuck tail is a defect, never a longer
    /// wait").
    fn cycleUntilCleanFn(ctx: *anyopaque, slots: []const u16) Error!u32 {
        const self = self_(ctx);
        var cycles: u32 = 0;
        while (cycles < checkpoint.COVER_CYCLES_MAX) {
            self.be.checkpointNow() catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Corrupt => error.Corrupt,
                else => error.Io,
            };
            cycles += 1;
            var dirty = false;
            for (slots) |s| dirty = dirty or self.be.cache.slotHasDirty(self.forestSlotOf(s));
            if (!dirty) {
                if (self.hold.at == .park_after_flush) {
                    self.hold.reached.set(self.be.io);
                    self.hold.release.waitUncancelable(self.be.io);
                }
                return cycles;
            }
        }
        log.err("recovery: {d} cycles left a recovered slot tree dirty — refusing (a stuck tail is a defect)", .{cycles});
        return error.Corrupt;
    }

    fn flushedRootFn(ctx: *anyopaque, slot: u16) Error!recovery.FlushedRoot {
        const self = self_(ctx);
        const t = self.be.slotTree(slot) orelse return .{ .root = .{ .addr = 0, .seq = 0 }, .extents = 0 };
        const r = t.rootPtr();
        return .{ .root = .{ .addr = r.addr, .seq = r.seq }, .extents = @intCast(@min(t.nodeCount(), std.math.maxInt(u32))) };
    }

    const Walk = union(enum) {
        /// Every leaf's tail: `all` loads a non-resident leaf (one extent
        /// read, counted); `resident` skips it.
        tails: struct { out: *std.ArrayList(cr.TailEntry), bytes: *u64, resident_only: bool },
        /// Every node's extent (no leaf is loaded: a parent's pointer
        /// names its leaves).
        extents: *std.ArrayList(u64),
    };

    /// Every node of `t` through the tree's own slot-aware loader
    /// (`leaf_addrs_unloaded`: the interior population names the leaves,
    /// so a leaf is loaded only when its TAIL is wanted).
    fn walkTree(self: *VolumeBinding, gpa: Allocator, t: *KvTree, w: Walk) Error!void {
        const Frame = struct { addr: u64, level: u8 };
        var stack: std.ArrayList(Frame) = .empty;
        defer stack.deinit(gpa);
        const root_level = t.rootLevel() catch |e| return mapTree(e);
        try stack.append(gpa, .{ .addr = t.rootPtr().addr, .level = root_level });
        while (stack.pop()) |f| {
            switch (w) {
                .extents => |ex| {
                    try ex.append(gpa, self.be.cache.addrExtent(f.addr));
                    if (f.level == 0) continue;
                },
                .tails => |tl| if (f.level == 0) {
                    const resident = self.be.cache.tryGet(f.addr);
                    if (resident == null and tl.resident_only) continue;
                    if (resident == null) tl.bytes.* += self.be.sb.node_size;
                    const leaf = resident orelse (t.getNode(f.addr) catch |e| return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        error.Io => error.Io,
                        else => error.Corrupt,
                    });
                    defer leaf.release();
                    leaf.lock.lockSharedUncancelable(self.be.io);
                    const tail: u32 = @intCast(@min(leaf.dirty.tailOffset(), std.math.maxInt(u32)));
                    leaf.lock.unlockShared(self.be.io);
                    try tl.out.append(gpa, .{ .leaf_addr = f.addr, .tail = tail });
                    continue;
                },
            }
            const n = t.getNode(f.addr) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
                else => error.Corrupt,
            };
            defer n.release();
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
                try stack.append(gpa, .{ .addr = iv.childAddr(), .level = n.level - 1 });
                if (!key.lessThan(live.key, &tree.KEY_SPACE_MAX)) break;
                pos = key.keySuccessor(live.key);
            }
        }
    }

    /// Every leaf's tail. Non-PR: EVERY leaf (a non-resident one costs one
    /// extent read, counted in bytes); under a device fence the RESIDENT
    /// ones only — the preempt made the scan unnecessary.
    fn leafTailsFn(ctx: *anyopaque, gpa: Allocator, slot: u16, pr_fenced: bool, out: *std.ArrayList(cr.TailEntry)) Error!u64 {
        const self = self_(ctx);
        // Under the SMO lock: a split or a root swap the flush pass runs
        // on the recovered tree between the walk's level read and its
        // pointer read would hand it a node of the wrong level.
        self.be.smo_lock.lockUncancelable(self.be.io);
        defer self.be.smo_lock.unlock(self.be.io);
        const t = self.be.slotTree(slot) orelse return 0;
        var bytes: u64 = 0;
        try self.walkTree(gpa, t, .{ .tails = .{ .out = out, .bytes = &bytes, .resident_only = pr_fenced } });
        return bytes;
    }

    // ---- the dead grant's retirements (decision 0003 §U.1) ----

    fn holdRetirementsFn(ctx: *anyopaque, grant_runs: []const GrantRun) Error!void {
        const self = self_(ctx);
        const be = self.be;
        const runs = try self.gpa.alloc(smo.RetireHold.Run, grant_runs.len);
        defer self.gpa.free(runs);
        for (grant_runs, runs) |g, *r| r.* = .{ .start = g.start, .len = g.len };
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        std.debug.assert(self.retire_hold == null and be.smo_ctx.retire_hold == null);
        self.retire_hold = try smo.RetireHold.init(self.gpa, runs);
        be.smo_ctx.retire_hold = &self.retire_hold.?;
    }

    /// `park`: every held extent no tree reaches NOW into the coverage
    /// clock at ring 0's reservation head — every gate the FIFO took so
    /// far is a position below it, and so is every held extent's own free
    /// record. A held extent a tree reaches again (a root install that
    /// re-adopted an image an earlier SMO had retired off the cache) is a
    /// live image and stays allocated.
    fn endRetirementHoldFn(ctx: *anyopaque, park: bool) void {
        const self = self_(ctx);
        const be = self.be;
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        be.smo_ctx.retire_hold = null;
        var hold = self.retire_hold orelse return;
        self.retire_hold = null;
        defer hold.deinit();
        if (!park or hold.held.items.len == 0) return;
        var images: std.ArrayList(u64) = .empty;
        defer images.deinit(self.gpa);
        self.maintainedImages(self.gpa, &images) catch |e| {
            log.warn("recovery: appender {d}'s {d} retired image extent(s) stay allocated — the trees could not be walked to prove them unreached ({s}); fsck C13 reclaims", .{ self.recovering_id, hold.held.items.len, @errorName(e) });
            return;
        };
        std.mem.sort(u64, images.items, {}, std.sort.asc(u64));
        const gate = be.core.headPos();
        for (hold.held.items) |e| {
            if (grant.sortedContains(images.items, e)) continue;
            be.alloc.freePendingForced(e, gate) catch {
                log.warn("recovery: a retired image extent ({d}) of appender {d}'s grant could not park (out of memory) — it stays allocated; fsck C13 reclaims", .{ e, self.recovering_id });
            };
        }
    }

    /// The images of every tree this process maintains — the recovered
    /// slots', the unleased ones, its own regions', the control and
    /// native trees. A live foreign lessee's tree is a projection here,
    /// its nodes its lessee's to judge (its interior may name extents the
    /// lessee retired long ago): never walked. The caller holds the SMO
    /// lock.
    fn maintainedImages(self: *VolumeBinding, gpa: Allocator, out: *std.ArrayList(u64)) Error!void {
        const be = self.be;
        for (be.trees.load()) |t| {
            // An empty slot owns no extent.
            if (t.rootPtr().addr < be.sb.heap.start) continue;
            if (t.forest_slot) |fs| if (fs != key.NATIVE_FOREST_SLOT) {
                const v = be.slots.table.view(route_table.slotOfForestSlot(fs));
                if (v.state != .unleased and v.state != .migrating and v.holder != self.recovering_id and !self.ownsRegion(v.holder)) continue;
            };
            try self.walkTree(gpa, t, .{ .extents = out });
        }
    }

    /// Every extent of `grant_runs` the volume holds, in ONE SMO-lock
    /// window (no SMO retires, claims or drains while it reads): the
    /// images of every tree this process maintains, and the hold's.
    fn keptExtentsFn(ctx: *anyopaque, gpa: Allocator, grant_runs: []const GrantRun, out: *std.ArrayList(u64)) Error!void {
        const self = self_(ctx);
        const be = self.be;
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        var images: std.ArrayList(u64) = .empty;
        defer images.deinit(gpa);
        try self.maintainedImages(gpa, &images);
        for (images.items) |e| {
            for (grant_runs) |r| if (r.contains(e)) {
                try out.append(gpa, e);
                break;
            };
        }
        if (self.retire_hold) |*h| try out.appendSlice(gpa, h.held.items);
    }

    // ---- the returns ----

    /// Through the manager's own verb (the record minus the runs, one
    /// `free(retire 0)` delta per extent), then the RAM bits released: a
    /// retire-0 free is immediately reusable (the leave's own order). Only
    /// what the record names leaves it, and exactly those bits clear: an
    /// extent the record does not name is not the dead grant's to give.
    fn returnExtentsFn(ctx: *anyopaque, extents: []const u64) Error!void {
        const self = self_(ctx);
        if (extents.len == 0) return;
        const gpa = self.gpa;
        var family = try self.grantOf(gpa, self.recovering_id);
        defer family.deinit();
        var named: std.ArrayList(u64) = .empty;
        defer named.deinit(gpa);
        for (extents) |e| if (family.contains(e)) try named.append(gpa, e);
        if (named.items.len == 0) return;
        std.mem.sort(u64, named.items, {}, std.sort.asc(u64));
        var runs: std.ArrayList(GrantRun) = .empty;
        defer runs.deinit(gpa);
        for (named.items) |e| {
            if (runs.items.len > 0 and runs.items[runs.items.len - 1].end() == e) {
                runs.items[runs.items.len - 1].len += 1;
            } else try runs.append(gpa, .{ .start = e, .len = 1 });
        }
        const out = self.set().manager.returnExtents(.{ .appender_id = self.recovering_id, .runs = runs.items }) catch |e| return mapVerb(e);
        if (out.cleared != named.items.len) {
            stats.recovery_return_mismatches.fire("a dead grant's return cleared other than the extents its record named");
            return error.Corrupt;
        }
        for (named.items) |e| self.be.alloc.releaseUnpublished(e);
    }

    /// A released region's ring back to the heap (the leave's own order:
    /// the page went Free first): the RAM bits released, one `free(retire
    /// 0)` delta per extent in ONE control entry.
    fn freeRingFn(ctx: *anyopaque, segments: []const page.Extent) Error!void {
        const self = self_(ctx);
        const be = self.be;
        var recs: std.ArrayList(manager.ControlRecord) = .empty;
        defer recs.deinit(self.gpa);
        for (segments) |s| {
            var e = (s.start - be.sb.heap.start) / be.sb.node_size;
            const end = e + s.len / be.sb.node_size;
            while (e < end) : (e += 1) {
                if (be.alloc.isAllocated(e)) be.alloc.releaseUnpublished(e);
                try recs.append(self.gpa, .{ .free = e });
            }
        }
        if (recs.items.len != 0) try controlEntryFn(ctx, recs.items);
    }

    /// The census of the remaining grant (fsck C13 for a dead appender):
    /// every extent still allocated that no root reaches and no
    /// `alloc_lease:` names as a bitmap page, read once — the node-seq
    /// handle floored above every stamp it carries — and judged by
    /// content: a residue the dead appender stamped is an orphan IMAGE;
    /// anything else it never wrote (a refill after its last page write, a
    /// RAM claim, a run past the page's four) is unclaimed.
    fn orphanImagesFn(ctx: *anyopaque, gpa: Allocator, appender_id: u32, remaining: []const GrantRun, kept: []const u64, orphans: *std.ArrayList(u64), unwritten: *std.ArrayList(u64)) Error!void {
        const self = self_(ctx);
        const be = self.be;
        var bitmap_extents: std.ArrayList(u64) = .empty;
        defer bitmap_extents.deinit(gpa);
        try self.bitmapExtents(gpa, &bitmap_extents);
        for (remaining) |run| {
            var e = run.start;
            while (e < run.end()) : (e += 1) {
                if (!be.alloc.isAllocated(e)) continue;
                var hit = grant.sortedContains(kept, e);
                for (bitmap_extents.items) |b| hit = hit or b == e;
                if (hit) continue;
                const img = be.dev.readRange(gpa, be.cache.extentAddr(e), @intCast(be.sb.node_size)) catch |err| return switch (err) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.Io => error.Io,
                };
                defer gpa.free(img);
                be.seq.raiseTo(node.residueSeqCeiling(img));
                try (if (node.residueStampedBy(img, appender_id)) orphans else unwritten).append(gpa, e);
            }
        }
    }

    /// Extents on THIS volume any `alloc_lease:` names as bitmap pages.
    fn bitmapExtents(self: *VolumeBinding, gpa: Allocator, out: *std.ArrayList(u64)) Error!void {
        var kvs: std.ArrayList(death_ledger.KeyValue) = .empty;
        defer death_ledger.freeKvs(gpa, &kvs);
        const r = cr.rangeOf(.alloc_lease);
        self.ledger.store.range(self.ledger.store.range_ctx, gpa, r.start.slice(), r.end.slice(), &kvs) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        const be = self.be;
        for (kvs.items) |kv| {
            const dec = cr.decodeAllocLease(kv.value) catch return error.Corrupt;
            var i: usize = 0;
            while (i < dec.count()) : (i += 1) {
                const ref = dec.bitmapAt(i);
                if (ref.vol != be.index or ref.start < be.sb.heap.start) continue;
                var e = (ref.start - be.sb.heap.start) / be.sb.node_size;
                const end = e + (ref.len + be.sb.node_size - 1) / be.sb.node_size;
                while (e < end) : (e += 1) try out.append(gpa, e);
            }
        }
    }

    // ---- the words ----

    fn raiseSeqFloorFn(ctx: *anyopaque, seq_floor: u64) void {
        const be = self_(ctx).be;
        _ = be.reg.raiseSeqFloor(&be.core, seq_floor);
    }

    fn ring0HeadFn(ctx: *anyopaque) u64 {
        return self_(ctx).be.core.headPos();
    }

    fn writerTermFn(ctx: *anyopaque) u64 {
        return self_(ctx).set().manager.term;
    }

    fn quarantineCustodyFn(ctx: *anyopaque, slot: u16, until_realtime_ns: u64) void {
        const self = self_(ctx);
        // A longer quarantine extends, a shorter never shortens.
        const gop = self.quarantines.getOrPut(self.gpa, slot) catch return;
        if (!gop.found_existing or gop.value_ptr.* < until_realtime_ns) gop.value_ptr.* = until_realtime_ns;
    }

    /// The realtime deadline on this volume's monotonic clock.
    fn holdCoherenceFn(ctx: *anyopaque, slot: u16, until_realtime_ns: u64) Error!void {
        const left = until_realtime_ns -| @as(u64, @intCast(clock.realtimeCoarseNs()));
        if (left == 0) return;
        try self_(ctx).be.slots.table.holdCoherence(slot, clock.monotonicNs() + left);
    }

    fn notifyHandoverDoneFn(ctx: *anyopaque) void {
        self_(ctx).be.slots.door.wakeParkers();
    }

    /// `release_recovered_regions`' predicate: any `alloc_lease:` of volume
    /// 0's tree 0 still names the identity as holder (by its stable part:
    /// the ledger's identities carry no writer id).
    fn namedAsHolderFn(ctx: *anyopaque, identity: WriterIdentity) bool {
        const self = self_(ctx);
        var kvs: std.ArrayList(death_ledger.KeyValue) = .empty;
        defer death_ledger.freeKvs(self.gpa, &kvs);
        const r = cr.rangeOf(.alloc_lease);
        self.ledger.store.range(self.ledger.store.range_ctx, self.gpa, r.start.slice(), r.end.slice(), &kvs) catch return true;
        for (kvs.items) |kv| {
            const dec = cr.decodeAllocLease(kv.value) catch return true;
            const holder: WriterIdentity = .{ .node_token = dec.lease.node_token, .mount_slot = dec.lease.mount_slot, .writer_id = dec.lease.writer_id };
            if (holder.sameMount(identity)) return true;
        }
        return false;
    }

    const vtable: recovery.Volume.VTable = .{
        .vol = &volFn,
        .readPage = &readPageFn,
        .writePage = &writePageFn,
        .readRing = &readRingFn,
        .slotState = &slotStateFn,
        .leasedTo = &leasedToFn,
        .grantFamily = &grantFamilyFn,
        .controlEntry = &controlEntryFn,
        .spillTails = &spillTailsFn,
        .noteUnleased = &noteUnleasedFn,
        .table = &tableFn,
        .treeState = &treeStateFn,
        .installRoot = &installRootFn,
        .restoreTree = &restoreTreeFn,
        .replay = &replayFn,
        .holdStructure = &holdStructureFn,
        .replayDataDeltas = &replayDataDeltasFn,
        .cycleUntilClean = &cycleUntilCleanFn,
        .flushedRoot = &flushedRootFn,
        .leafTails = &leafTailsFn,
        .holdRetirements = &holdRetirementsFn,
        .endRetirementHold = &endRetirementHoldFn,
        .keptExtents = &keptExtentsFn,
        .returnExtents = &returnExtentsFn,
        .freeRing = &freeRingFn,
        .orphanImages = &orphanImagesFn,
        .raiseSeqFloor = &raiseSeqFloorFn,
        .ring0Head = &ring0HeadFn,
        .writerTerm = &writerTermFn,
        .quarantineCustody = &quarantineCustodyFn,
        .holdCoherence = &holdCoherenceFn,
        .notifyHandoverDone = &notifyHandoverDoneFn,
    };
};

/// The hand-off the poll runs after a recovery acted: the cross-owner
/// plane ADOPTS the recovered slots' open intents (`xv.ops.Plane.
/// adoptRecovered` — reads only) and its cadence completes them; the poll
/// rides the membership cadence, which never waits on a guard.
pub const AdoptionBinding = struct {
    plane: *xv_set_bind.SetPlane,

    pub fn seam(self: *AdoptionBinding) death_ledger.IntentAdoption {
        return .{ .ctx = self, .adopt = &adopt };
    }

    fn adopt(ctx: ?*anyopaque) u32 {
        const self: *AdoptionBinding = @ptrCast(@alignCast(ctx.?));
        return self.plane.adoptRecovered(clock.monotonicNs()) catch |e| blk: {
            log.warn("recovery: the recovered slots' open intents could not be read for the roll-forward ({s}) — the next pass adopts them", .{@errorName(e)});
            break :blk 0;
        };
    }
};

// ---------------------------------------------------------------------------
// Tests (Linux; a file-backed symmetric set under /build/tmp): the frame
// screen and the stamps through every loader over the real backend, and
// the §5.9 procedure over the real backend driven by the manager's poll.
// ---------------------------------------------------------------------------

const testing = std.testing;
const linux = std.os.linux;
const align_probe = @import("../../../io/align_probe.zig");
const meta_set = @import("../../meta_set.zig");
const record = @import("../../record.zig");
const bset = @import("../../bset.zig");
const dlm = @import("../../dlm.zig");
const inos = @import("../../inos.zig");
const xv = @import("../xv.zig");
const join = @import("../join.zig");
const wire_plane = @import("../wire_plane.zig");

/// The kill: a byte copy of the live image (the process state abandoned).
fn snapshotImage(src: []const u8, dst: []const u8) !void {
    var sz: [linux.PATH_MAX]u8 = undefined;
    var dz: [linux.PATH_MAX]u8 = undefined;
    const sfd_rc = linux.open(try align_probe.pathZ(&sz, src), .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(sfd_rc) != .SUCCESS) return error.CannotOpen;
    const sfd: linux.fd_t = @intCast(sfd_rc);
    defer _ = linux.close(sfd);
    const dfd_rc = linux.open(try align_probe.pathZ(&dz, dst), .{ .ACCMODE = .WRONLY, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
    if (linux.errno(dfd_rc) != .SUCCESS) return error.CannotCreate;
    const dfd: linux.fd_t = @intCast(dfd_rc);
    defer _ = linux.close(dfd);
    var buf: [1 << 20]u8 = undefined;
    var off: u64 = 0;
    while (true) {
        const n = linux.pread(sfd, &buf, buf.len, @intCast(off));
        if (linux.errno(n) != .SUCCESS) return error.ReadFailed;
        if (n == 0) break;
        const w = linux.pwrite(dfd, &buf, n, @intCast(off));
        if (linux.errno(w) != .SUCCESS or w != n) return error.WriteFailed;
        off += n;
    }
    if (linux.errno(linux.fsync(dfd)) != .SUCCESS) return error.SyncFailed;
}

fn unlinkPath(path: []const u8) void {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    if (align_probe.pathZ(&zbuf, path)) |z| {
        _ = linux.unlink(z);
    } else |_| {}
}

fn openCopy(s: *meta_set.TestSet, path: []const u8) !*meta_set.MetaSet {
    return meta_set.MetaSet.open(s.gpa, s.io(), &.{path}, .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4 });
}

const iv_value = (record.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode();

/// A guest-slot inode key (the slot's tree is minted on first touch).
fn slotKey(be: *KvBackend, slot: u16, local: u64) ![]const u8 {
    _ = try be.mintGuestTree(slot);
    return testing.allocator.dupe(u8, &key.inodeKey(key.guestLocalIno(slot, local)));
}

fn freeKeys(keys: *std.ArrayList([]const u8)) void {
    for (keys.items) |k| testing.allocator.free(k);
    keys.deinit(testing.allocator);
}

fn expectAllPresent(set: *meta_set.MetaSet, keys: []const []const u8) !void {
    for (keys) |k| {
        const got = try set.lookup(testing.allocator, key.TREE_INODES, k);
        defer if (got) |g| testing.allocator.free(g);
        try testing.expect(got != null);
    }
}

/// Every frame of every leaf of `t` off the DEVICE (no cache): the stamps
/// in append order.
fn leafStamps(be: *KvBackend, t: *KvTree, out: *std.ArrayList(fencing.FrameStamp)) !void {
    const gpa = testing.allocator;
    var tails: std.ArrayList(cr.TailEntry) = .empty;
    defer tails.deinit(gpa);
    try be.collectLeafTails(t, &tails);
    for (tails.items) |tl| {
        const img = try be.dev.readRange(gpa, tl.leaf_addr, @intCast(be.sb.node_size));
        defer gpa.free(img);
        const hdr = try node.decodeHeaderPage(img[0..node.NODE_PAGE], null);
        var pos: usize = node.NODE_PAGE;
        while (pos < img.len) {
            switch (bset.probeFrame(img, pos, hdr.node_seq)) {
                .frame => |f| {
                    try out.append(gpa, f.stamp);
                    pos += f.padded_len;
                },
                else => break,
            }
        }
    }
}

test "linux: the one-writer forest — every frame of every leased slot tree carries (0, g) with g the slot's lease generation (tree 0 and the table agree), the remount screens nothing, every record stands" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "frames-solo", 1);
    defer s.destroy();
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    const screened0 = node.meta_kv_foreign_frames_screened.load();
    const unstamped0 = fencing.stats.appender_unstamped_frame.load();
    {
        const set = try s.open();
        defer set.close();
        const be = set.volumes[0];
        try testing.expect(be.slots.frames.primed());
        for (0..12) |i| {
            const t = try be.begin(@src());
            const k = try slotKey(be, 3, 10 + i);
            try keys.append(gpa, k);
            try t.stagePut(key.TREE_INODES, k, &iv_value);
            try be.commit(t);
        }
        try be.checkpointNow();
        const g3 = be.slots.table.gOf(3);
        try testing.expect(g3 >= 1 and be.slots.table.view(3).leasedBy(0));
        const raw = (try be.lookup(gpa, key.TREE_CONTROL, cr.slotStateKey(3).slice())).?;
        defer gpa.free(raw);
        try testing.expectEqual(g3, (try cr.SlotStateWire.decode(raw)).g);
        var stamps: std.ArrayList(fencing.FrameStamp) = .empty;
        defer stamps.deinit(gpa);
        try leafStamps(be, be.slotTree(3).?, &stamps);
        try testing.expect(stamps.items.len >= 1);
        for (stamps.items) |st| try testing.expectEqual(fencing.FrameStamp{ .appender_id = 0, .g = g3 }, st);
        // The native slot's frames: `format`'s base frame carries the
        // structural (0, 0) of a volume with no lease yet; every frame the
        // mount wrote carries (0, g) — monotone, so rule 3 never fires.
        stamps.clearRetainingCapacity();
        const native = be.stamp.native_slot.?;
        try leafStamps(be, be.treeOf(.native).?, &stamps);
        var mounted_frames: usize = 0;
        for (stamps.items) |st| {
            try testing.expectEqual(@as(u32, 0), st.appender_id);
            try testing.expect(st.g == 0 or st.g == be.slots.table.gOf(native));
            if (st.g != 0) mounted_frames += 1;
        }
        try testing.expect(mounted_frames >= 1);
    }
    const set = try s.open();
    defer set.close();
    try expectAllPresent(set, keys.items);
    try testing.expectEqual(screened0, node.meta_kv_foreign_frames_screened.load());
    try testing.expectEqual(unstamped0, fencing.stats.appender_unstamped_frame.load());
}

test "linux: THE PRIMING LAW, red then green — a writer whose first frame writer (the claim gate's ring-recovery preflight) is not primed stamps (0, 0) onto a leased leaf: the tripwire fires and the NEXT mount ends the log there (acked records lost, screened by rule 3); the same kill image opened primed keeps every record and screens nothing" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "frames-prime", 1);
    defer s.destroy();
    var kill_buf: [160]u8 = undefined;
    const kill = try std.fmt.bufPrint(&kill_buf, "{s}.kill", .{s.paths[0]});
    defer unlinkPath(kill);
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{s.paths[0]});
    defer unlinkPath(dst);
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    {
        // Slot 3's leaf with a flushed (0, 1) frame, then four acked
        // records in the WINDOW (fsync, no checkpoint): the kill image.
        const set = try s.open();
        defer set.close();
        const be = set.volumes[0];
        for (0..8) |i| {
            if (i == 4) try be.checkpointNow();
            const t = try be.begin(@src());
            const k = try slotKey(be, 3, 100 + i);
            try keys.append(gpa, k);
            try t.stagePut(key.TREE_INODES, k, &iv_value);
            try be.commit(t);
        }
        try be.fsync();
        try snapshotImage(s.paths[0], kill);
    }
    // RED: the unprimed first writer.
    try snapshotImage(kill, dst);
    {
        fencing.test_unprimed_first_writer.store(true, .release);
        defer fencing.test_unprimed_first_writer.store(false, .release);
        const tw0 = fencing.stats.appender_unstamped_frame.load();
        fencing.stats.appender_unstamped_frame.resetLogBudgetForTest();
        // One tripwire line per second per site: the preflight's and the
        // post-gate cover's flushes may straddle a second.
        log.testing.expectErrorLogs(2);
        const set = try openCopy(s, dst);
        defer set.close();
        try testing.expect(fencing.stats.appender_unstamped_frame.load() > tw0);
        // The replayed records ARE visible on this mount (RAM has them).
        try expectAllPresent(set, keys.items);
    }
    {
        // The next mount: rule 3 ends slot 3's log at the (0, 0) frame — the
        // acked records of that frame are gone.
        const screened0 = node.meta_kv_foreign_frames_screened.load();
        const set = try openCopy(s, dst);
        defer set.close();
        var missing: usize = 0;
        for (keys.items) |k| {
            const got = try set.lookup(gpa, key.TREE_INODES, k);
            defer if (got) |g| gpa.free(g);
            if (got == null) missing += 1;
        }
        try testing.expect(missing > 0);
        try testing.expect(node.meta_kv_foreign_frames_screened.load() > screened0);
    }
    // GREEN: the same kill image opened PRIMED (the law in force).
    try snapshotImage(kill, dst);
    {
        const tw0 = fencing.stats.appender_unstamped_frame.load();
        const set = try openCopy(s, dst);
        defer set.close();
        try testing.expectEqual(tw0, fencing.stats.appender_unstamped_frame.load());
        try expectAllPresent(set, keys.items);
    }
    const screened0 = node.meta_kv_foreign_frames_screened.load();
    const set = try openCopy(s, dst);
    defer set.close();
    try expectAllPresent(set, keys.items);
    try testing.expectEqual(screened0, node.meta_kv_foreign_frames_screened.load());
}

test "linux: two regions — B takes slot 3 over by handover and stamps (1, g + 1); A's loader never screens B's current frames; after B's release records the tails a zombie B frame at the leaf's tail is screened by rule 2 (the log ends before it, nothing earlier lost)" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "frames-two", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_SYM_T_IDLE_MS", "1000");
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    const set = try s.open();
    defer set.close();
    const be = set.volumes[0];
    const aset = be.sym.?;
    for (0..6) |i| {
        const t = try be.begin(@src());
        const k = try slotKey(be, 3, 10 + i);
        try keys.append(gpa, k);
        try t.stagePut(key.TREE_INODES, k, &iv_value);
        try be.commit(t);
    }
    const g_a = be.slots.table.gOf(3);
    const r1 = try aset.joinInProcess(join.HARNESS_MOUNT_SLOT_BASE + 7);
    // B hammers slot 3 until A offers and B's touch accepts (the handover).
    var touches: u32 = 0;
    const k1 = try slotKey(be, 3, 900);
    try keys.append(gpa, k1);
    while (touches < 2000) : (touches += 1) {
        if (aset.appendPut(r1, key.TREE_INODES, k1, &iv_value)) |_| break else |e| switch (e) {
            error.SlotBusy, error.Deferred => {},
            else => return e,
        }
    }
    try testing.expect(touches < 2000);
    try testing.expect(be.slots.table.view(3).leasedBy(1));
    try testing.expectEqual(g_a + 1, be.slots.table.gOf(3));
    for (0..3) |i| {
        const k = try slotKey(be, 3, 901 + i);
        try keys.append(gpa, k);
        try aset.appendPut(r1, key.TREE_INODES, k, &iv_value);
    }
    try be.checkpointNow();
    // B's frames are (1, g + 1) behind A's (0, g): the stamps follow the lease.
    var stamps: std.ArrayList(fencing.FrameStamp) = .empty;
    defer stamps.deinit(gpa);
    try leafStamps(be, be.slotTree(3).?, &stamps);
    var saw_a = false;
    var saw_b = false;
    for (stamps.items) |st| {
        if (st.appender_id == 0) {
            try testing.expectEqual(g_a, st.g);
            saw_a = true;
        } else {
            try testing.expectEqual(fencing.FrameStamp{ .appender_id = 1, .g = g_a + 1 }, st);
            saw_b = true;
        }
    }
    try testing.expect(saw_a and saw_b);
    // A's loader over B's CURRENT frames: dropped from the cache, reloaded
    // under the screen — nothing screened, every record present.
    const screened0 = node.meta_kv_foreign_frames_screened.load();
    _ = be.cache.discardSlotNodes(key.guestForestSlot(3));
    try expectAllPresent(set, keys.items);
    try testing.expectEqual(screened0, node.meta_kv_foreign_frames_screened.load());
    // B leaves: slot 3 released to nobody, its tails recorded at g + 1.
    try aset.leaveInProcess(r1);
    try testing.expectEqual(lt.State.unleased, be.slots.table.view(3).state);
    var tails: std.ArrayList(cr.TailEntry) = .empty;
    defer tails.deinit(gpa);
    try be.collectLeafTails(be.slotTree(3).?, &tails);
    try testing.expectEqual(@as(usize, 1), tails.items.len);
    const leaf = tails.items[0];
    try testing.expectEqual(leaf.tail, be.slots.table.recordedTail(3, leaf.leaf_addr).?.tail);
    try testing.expectEqual(g_a + 1, be.slots.table.recordedTail(3, leaf.leaf_addr).?.g);
    // THE ZOMBIE: a (1, g + 1) frame appended at the released leaf's tail
    // (a B that outlived its release).
    {
        const img = try be.dev.readRange(gpa, leaf.leaf_addr, @intCast(be.sb.node_size));
        defer gpa.free(img);
        const hdr = try node.decodeHeaderPage(img[0..node.NODE_PAGE], null);
        const zk = key.inodeKey(key.guestLocalIno(3, 990));
        const ref = record.RecordRef{ .key = &zk, .seq = 1 << 40, .kind = .put, .value = &iv_value };
        var plan = try node.appendBset(gpa, be.layout.stamped(.{ .appender_id = 1, .g = g_a + 1 }), .{ .node_addr = leaf.leaf_addr, .node_seq = hdr.node_seq, .tail_offset = leaf.tail }, &.{ref}, 1 << 40, null);
        defer plan.deinit(gpa);
        try be.pool.writeAt(be.path, plan.offset, plan.frame);
        try be.dev.barrier();
    }
    _ = be.cache.discardSlotNodes(key.guestForestSlot(3));
    try expectAllPresent(set, keys.items);
    try testing.expectEqual(screened0 + 1, node.meta_kv_foreign_frames_screened.load());
    const zombie = try set.lookup(gpa, key.TREE_INODES, &key.inodeKey(key.guestLocalIno(3, 990)));
    defer if (zombie) |z| gpa.free(z);
    try testing.expect(zombie == null);
}

// ---- the §5.9 procedure over the real backend ----

const DEAD_MOUNT_SLOT: u32 = join.HARNESS_MOUNT_SLOT_BASE + 9;
const DEAD_SLOT: u16 = 700;

const Killed = struct {
    identity: WriterIdentity,
    /// An extent of region 1's grant holding an image it stamped that no
    /// root reaches (the C13 orphan image).
    orphan: u64,
    /// An extent of region 1's grant it claimed but never wrote (the page
    /// names it neither: unclaimed by content).
    unwritten: u64,
    ring_bytes: u64,
};

/// Phase 1: region 1 joins in-process, appends `n` records into slot 700,
/// claims two grant extents it never publishes — one it writes a stamped
/// image into, one it leaves blank — then DIES (the image copied at `kill`
/// with its page Live).
fn killWithDeadRegion(s: *meta_set.TestSet, kill: []const u8, keys: *std.ArrayList([]const u8), n: usize) !Killed {
    const gpa = testing.allocator;
    const set = try s.open();
    defer set.close();
    const be = set.volumes[0];
    const aset = be.sym.?;
    const r1 = try aset.joinInProcess(DEAD_MOUNT_SLOT);
    for (0..n) |i| {
        const k = try slotKey(be, DEAD_SLOT, 1000 + i);
        try keys.append(gpa, k);
        try aset.appendPut(r1, key.TREE_INODES, k, &iv_value);
    }
    const orphan = try r1.grant.claim();
    const unwritten = try r1.grant.claim();
    // The orphan's image: a fresh leaf image stamped by region 1 (a root
    // swap's output whose publish never landed), written whole.
    {
        const ok = try slotKey(be, DEAD_SLOT, 999);
        defer gpa.free(ok);
        const ref = record.RecordRef{ .key = ok, .seq = 1 << 40, .kind = .put, .value = &iv_value };
        var w = try node.writeNode(gpa, be.layout.stamped(.{ .appender_id = r1.appenderId(), .g = 1 }), .{ .node_addr = be.cache.extentAddr(orphan), .node_seq = 1 << 40, .tree_id = key.TREE_INODES, .level = 0, .min_key = ok, .max_key = ok }, &.{ref}, 1 << 40, null);
        defer w.deinit(gpa);
        try be.pool.writeAt(be.path, w.node_addr, w.image);
        try be.dev.barrier();
    }
    // A cycle: the page names the leases and the pool minus the claims.
    try be.checkpointNow();
    for (n..n + 4) |i| {
        const k = try slotKey(be, DEAD_SLOT, 1000 + i);
        try keys.append(gpa, k);
        try aset.appendPut(r1, key.TREE_INODES, k, &iv_value);
    }
    // The window carries every slot-tree kind a live workload writes: an
    // inode DELTA on the first flushed inode (a `write`'s size) and a
    // tombstone on the second (an `unlink`) — the two kinds a replay that
    // knows only `put`/`delete` mislabels.
    try aset.appendRecord(r1, key.TREE_INODES, keys.items[0], .delta, dead_delta.encode().slice());
    try aset.appendRecord(r1, key.TREE_INODES, keys.items[1], .delete, &.{});
    try be.fsync();
    try snapshotImage(s.paths[0], kill);
    return .{ .identity = .{ .node_token = r1.identity.node_token, .mount_slot = r1.identity.mount_slot, .writer_id = r1.identity.writer_id }, .orphan = orphan, .unwritten = unwritten, .ring_bytes = r1.core.ringBytes() };
}

/// The dead window's inode delta: `size = 4096`.
const dead_delta: record.InodeDelta = .{ .mask = record.DELTA_SIZE, .fields = .{ .size = 4096 } };

/// The dead region's records as recovered: every put stands, the delta
/// folded into its inode (the size), the tombstoned key gone.
fn expectDeadWindowRecovered(set: *meta_set.MetaSet, keys: []const []const u8) !void {
    const gpa = testing.allocator;
    try expectAllPresent(set, keys[2..]);
    const first = (try set.lookup(gpa, key.TREE_INODES, keys[0])) orelse return error.DeltaBaseLost;
    defer gpa.free(first);
    try testing.expectEqual(@as(u64, 4096), (try record.InodeValue.decode(first)).size);
    const second = try set.lookup(gpa, key.TREE_INODES, keys[1]);
    defer if (second) |v| gpa.free(v);
    try testing.expect(second == null);
}

const Live = struct {
    set: *meta_set.MetaSet,
    locks: dlm.Manager,
    bound: *xv.set_bind.Bound,
    plane: *wire_plane.WirePlane,

    fn open(s: *meta_set.TestSet, path: []const u8) !Live {
        const gpa = testing.allocator;
        const set = try openCopy(s, path);
        errdefer set.close();
        var locks = try dlm.Manager.init(gpa, 64);
        errdefer locks.deinit();
        const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
        errdefer bound.destroy(gpa);
        const plane = (try wire_plane.WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
        return .{ .set = set, .locks = locks, .bound = bound, .plane = plane };
    }

    fn close(self: *Live) void {
        self.plane.stop();
        self.bound.destroy(testing.allocator);
        self.set.close();
        self.locks.deinit();
    }

    fn binding(self: *Live) *VolumeBinding {
        return self.plane.recoveryBinding(0).?;
    }

    fn recordDeath(self: *Live, identity: WriterIdentity, early: bool) void {
        self.plane.deathLedger().?.recordDeath(identity, .{ .epoch = 1, .ts_realtime_ns = @intCast(clock.realtimeCoarseNs()), .pr_key = 0, .early = early });
    }
};

test "linux: a dead in-process region recovered over the real backend by the manager's poll — the page Live → Recovering → Recovered, every acked record of the dead region readable through tree 0's Unleased slot root, the tails recorded, the orphan image and the unclaimed grant returned, recovered: written, the bound asserted; a Recovered ring is never replayed and the region is released at the next poll; a remount holds everything" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "recover", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var kill_buf: [160]u8 = undefined;
    const kill = try std.fmt.bufPrint(&kill_buf, "{s}.kill", .{s.paths[0]});
    defer unlinkPath(kill);
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    const dead = try killWithDeadRegion(s, kill, &keys, 20);
    const recoveries0 = recovery.stats.appender_recoveries.load();
    const acted0 = death_ledger.stats.dead_members_acted.load();
    const orphans0 = recovery.stats.dead_appender_orphans.load();
    const unwritten0 = recovery.stats.dead_appender_unwritten_returned.load();
    const released0 = death_ledger.stats.recovered_regions_released.load();
    const scan0 = recovery.stats.recovery_full_tail_scan_bytes.load();
    {
        var live = try Live.open(s, kill);
        defer live.close();
        const be = live.set.volumes[0];
        const aset = be.sym.?;
        try testing.expect(aset.isUnrecovered(1));
        try testing.expectEqual(page.State.live, (try aset.manager.readPageOf(1)).?.state);
        // This mount learnt the dead lease off tree 0 at its arm: the slot
        // resolves to the dead appender, which has no listener.
        try testing.expectEqual(@as(u32, 1), be.slots.cache.stepHome(DEAD_SLOT).unreachable_.holder);
        // Before the record: the poll acts on nothing.
        live.plane.tick();
        try testing.expectEqual(recoveries0, recovery.stats.appender_recoveries.load());
        live.recordDeath(dead.identity, false);
        const t0 = clock.monotonicNs();
        live.plane.tick();
        const took_ms = (clock.monotonicNs() - t0) / std.time.ns_per_ms;
        try testing.expectEqual(recoveries0 + 1, recovery.stats.appender_recoveries.load());
        try testing.expectEqual(acted0 + 1, death_ledger.stats.dead_members_acted.load());
        // The recovered slot is the manager's to maintain: the learnt view
        // dropped with the release, every read of it local.
        try testing.expect(be.slots.cache.stepHome(DEAD_SLOT) == .local);
        const p1 = (try aset.manager.readPageOf(1)).?;
        try testing.expectEqual(page.State.recovered, p1.state);
        try testing.expectEqual(@as(u16, 0), p1.n_slots);
        try testing.expectEqual(aset.manager.term, p1.recovered_by_term);
        try testing.expect(p1.ledger_tail_seq > 0 and p1.head_hint == p1.ledger_tail_seq);
        // Every acked record of the dead region through the slot's tree.
        try expectDeadWindowRecovered(live.set, keys.items);
        // Tree 0: Unleased at the dead lease's g with the flushed root; the
        // tails recorded; the RAM table agrees; the recorded tails index.
        const raw = (try be.lookup(gpa, key.TREE_CONTROL, cr.slotStateKey(DEAD_SLOT).slice())).?;
        defer gpa.free(raw);
        const st = try cr.SlotStateWire.decode(raw);
        try testing.expectEqual(cr.SlotVariant.unleased, st.variant);
        try testing.expect(st.root.addr != 0);
        try testing.expectEqual(be.slotTree(DEAD_SLOT).?.rootPtr().addr, st.root.addr);
        // `max(recorded, page, window max local + 1)`: the window's highest
        // local was 1023.
        try testing.expect(st.cursor >= 1024 and st.cursor >= inos.LOCAL_INO_BASE);
        try testing.expectEqual(lt.State.unleased, be.slots.table.view(DEAD_SLOT).state);
        try testing.expectEqual(st.g, be.slots.table.gOf(DEAD_SLOT));
        const tails_raw = (try be.lookup(gpa, key.TREE_CONTROL, cr.slotTailsKey(DEAD_SLOT).slice())).?;
        defer gpa.free(tails_raw);
        const tails = try cr.decodeSlotTails(tails_raw);
        try testing.expect(tails.count >= 1 and tails.g == st.g);
        try testing.expect(be.slots.table.recordedTail(DEAD_SLOT, tails.tailEntryAt(0).leaf_addr) != null);
        // Non-PR: every leaf scanned (the resident ones cost nothing).
        try testing.expect(recovery.stats.recovery_full_tail_scan_bytes.load() >= scan0);
        // The census by content: the stamped image is the one orphan (C13),
        // the blank claim is unclaimed — both returned; the grant record
        // names the trees' images alone.
        try testing.expectEqual(orphans0 + 1, recovery.stats.dead_appender_orphans.load());
        try testing.expectEqual(unwritten0 + 1, recovery.stats.dead_appender_unwritten_returned.load());
        try testing.expect(!be.alloc.isAllocated(dead.orphan));
        try testing.expect(!be.alloc.isAllocated(dead.unwritten));
        {
            const r = cr.extentGrantFamily(1);
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(gpa, &kvs);
            try be.range(gpa, key.TREE_CONTROL, r.start.slice(), r.end.slice(), std.math.maxInt(usize), &kvs);
            var fam = try grant_record.Family.load(gpa, 1, kvs.items);
            defer fam.deinit();
            for (fam.record) |run| {
                var e = run.start;
                while (e < run.end()) : (e += 1) try testing.expect(be.alloc.isAllocated(e));
            }
        }
        // recovered:{id, vol} stands; the bound in force covers the run.
        try testing.expect(try live.plane.deathLedger().?.isRecovered(dead.identity, 0));
        try testing.expectEqual(recovery.recoveryBoundMs(dead.ring_bytes, be.sb.node_size), recovery.stats.appender_recovery_bound_ms.load());
        try testing.expect(took_ms <= recovery.stats.appender_recovery_bound_ms.load());
        // A Recovered ring is never replayed; the poll's first act releases
        // the region no lease names: the page Free, id and term kept.
        live.plane.tick();
        try testing.expectEqual(recoveries0 + 1, recovery.stats.appender_recoveries.load());
        try testing.expectEqual(released0 + 1, death_ledger.stats.recovered_regions_released.load());
        const p1b = (try aset.manager.readPageOf(1)).?;
        try testing.expectEqual(page.State.free, p1b.state);
        try testing.expectEqual(p1.term, p1b.term);
        try expectDeadWindowRecovered(live.set, keys.items);
    }
    // The remount: nothing unrecovered, every record durable.
    const set = try openCopy(s, kill);
    defer set.close();
    try testing.expectEqual(@as(usize, 0), set.volumes[0].sym.?.unrecovered.items.len);
    try expectDeadWindowRecovered(set, keys.items);
    try testing.expectEqual(lt.State.unleased, set.volumes[0].slots.table.view(DEAD_SLOT).state);
}

test "linux: every crash window of the §5.9 procedure over the real backend — a run that fails right after each step's durable effects leaves the page Recovering (Live before step 2) and the re-run converges: every acked record of the dead region readable; an EARLY record quarantines the recovered slot's custody" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "recover-crash", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var kill_buf: [160]u8 = undefined;
    const kill = try std.fmt.bufPrint(&kill_buf, "{s}.kill", .{s.paths[0]});
    defer unlinkPath(kill);
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{s.paths[0]});
    defer unlinkPath(dst);
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    const dead = try killWithDeadRegion(s, kill, &keys, 6);
    inline for (@typeInfo(recovery.Step).@"enum".field_values) |value| {
        const step: recovery.Step = @fromBackingInt(@intCast(value));
        try snapshotImage(kill, dst);
        var live = try Live.open(s, dst);
        defer live.close();
        const aset = live.set.volumes[0].sym.?;
        const early = step == .flush;
        live.recordDeath(dead.identity, early);
        live.binding().driver.seams.fail_at = step;
        const rollbacks0 = recovery.stats.appender_recovery_rollbacks.load();
        live.plane.tick();
        try testing.expectEqual(rollbacks0 + 1, recovery.stats.appender_recovery_rollbacks.load());
        const p = (try aset.manager.readPageOf(1)).?;
        switch (step) {
            .decide, .preempt => try testing.expectEqual(page.State.live, p.state),
            .page_recovering, .read_ring, .mark_dead, .replay, .flush, .tails, .tree0, .table_release, .returns => try testing.expectEqual(page.State.recovering, p.state),
            .page_recovered, .recovered_record => try testing.expectEqual(page.State.recovered, p.state),
        }
        // The dead slot's first touch is refused while the page is Recovering.
        if (p.state == .recovering) try testing.expect(live.set.volumes[0].slots.table.view(DEAD_SLOT).state != .unleased or step == .tree0 or step == .table_release or step == .returns);
        live.binding().driver.seams.fail_at = null;
        live.plane.tick();
        try expectDeadWindowRecovered(live.set, keys.items);
        const after = (try aset.manager.readPageOf(1)).?;
        try testing.expect(after.state == .recovered or after.state == .free);
        try testing.expectEqual(lt.State.unleased, live.set.volumes[0].slots.table.view(DEAD_SLOT).state);
        if (early and after.state == .recovered) {
            try testing.expect(live.binding().custodyQuarantineRemainingNs(DEAD_SLOT, @intCast(clock.realtimeCoarseNs())) != null);
            const q = try live.set.volumes[0].lookup(gpa, key.TREE_CONTROL, cr.custodyQuarantineKey(DEAD_SLOT).slice());
            defer if (q) |v| gpa.free(v);
            try testing.expect(q != null);
        }
        // A third poll: nothing re-replayed, the region released.
        const recoveries = recovery.stats.appender_recoveries.load();
        live.plane.tick();
        try testing.expectEqual(recoveries, recovery.stats.appender_recoveries.load());
        try testing.expectEqual(page.State.free, (try aset.manager.readPageOf(1)).?.state);
    }
}

/// The dead holder's allocation lease for a data volume homed on volume
/// 0: its bitmap pages placed in region 1's grant (`holdPages`), the
/// record on tree 0, one SET delta of three blocks at the lease's term in
/// region 1's ring.
const DATA_VOL_TAG: u64 = 0xDA7A_0001;
const DATA_BLOCKS: u64 = 4096;
const DATA_LEASE_TERM: u64 = 3;
const DELTA_START: u64 = 100;
const DELTA_LEN: u32 = 3;

const GrantPool = struct {
    be: *KvBackend,
    r: *bind.Region,

    const vt: data_bitmap.ExtentPool.VTable = .{ .claim = &claim, .release = &release, .node_size = &nodeSize };

    fn pool(self: *GrantPool) data_bitmap.ExtentPool {
        return .{ .ctx = self, .vtable = &vt };
    }

    fn claim(ctx: *anyopaque) error{GrantExhausted}!data_bitmap.Extent {
        const self: *GrantPool = @ptrCast(@alignCast(ctx));
        const e = self.r.grant.claim() catch return error.GrantExhausted;
        return .{ .start = self.be.cache.extentAddr(e), .len = self.be.sb.node_size };
    }

    fn release(_: *anyopaque, _: data_bitmap.Extent) void {}

    fn nodeSize(ctx: *anyopaque) u64 {
        const self: *GrantPool = @ptrCast(@alignCast(ctx));
        return self.be.sb.node_size;
    }
};

test "linux: the data-bitmap deltas of a dead holder's allocation lease homed here are folded onto its pages by the recovery (a SET run of three blocks read back set); the region stays Recovered while the lease names the holder" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "recover-bitmap", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var kill_buf: [160]u8 = undefined;
    const kill = try std.fmt.bufPrint(&kill_buf, "{s}.kill", .{s.paths[0]});
    defer unlinkPath(kill);
    var refs: std.ArrayList(records.BitmapRef) = .empty;
    defer refs.deinit(gpa);
    var identity: WriterIdentity = undefined;
    {
        const set = try s.open();
        defer set.close();
        const be = set.volumes[0];
        const aset = be.sym.?;
        const r1 = try aset.joinInProcess(DEAD_MOUNT_SLOT);
        identity = .{ .node_token = r1.identity.node_token, .mount_slot = r1.identity.mount_slot, .writer_id = r1.identity.writer_id };
        // The pages: a fresh (all-zero) region image in the grant.
        const image = try gpa.alloc(u8, @intCast(data_bitmap.regionLen(DATA_BLOCKS)));
        defer gpa.free(image);
        @memset(image, 0);
        var gp: GrantPool = .{ .be = be, .r = r1 };
        var dev: VolumeBinding.DeviceRegion = .{ .be = be };
        try data_bitmap.holdPages(gpa, gp.pool(), dev.io(), 0, image, &refs);
        // The lease record on tree 0 (volume 0's).
        var lbuf: [records.ALLOC_LEASE_MAX_LEN]u8 = undefined;
        const lease: cr.AllocLease = .{ .node_token = identity.node_token, .mount_slot = identity.mount_slot, .holder_appender_id = 1, .writer_id = identity.writer_id, .home_vol = 0, .control_ino = 0, .blocks = DATA_BLOCKS, .term = DATA_LEASE_TERM, .bitmap = refs.items };
        const enc = try cr.encodeAllocLease(lease, &lbuf);
        const vol = aset.volume();
        try vol.vt.controlEntry(vol.ctx, &.{.{ .put = .{ .key = cr.allocLeaseKey(DATA_VOL_TAG).slice(), .value = enc } }});
        // The SET delta in region 1's ring (the holder's own class).
        const dk = records.deltaKey(DATA_VOL_TAG, DELTA_START);
        const dv = records.encodeDelta(.{ .tag = records.TAG_SET, .len = DELTA_LEN, .term = DATA_LEASE_TERM });
        var frames = [_]journal.RecordFrame{.{ .tag = key.tagFor(records.DELTA_KIND, 0), .key = &dk, .kind = .put, .seq = 0, .value = &dv }};
        const len = try journal.entryLenFor(&frames);
        const adm = r1.core.core.tryAdmit(len, .user).?;
        const reg = try r1.smo_journal.reserve(adm);
        frames[0].seq = reg.seq_base;
        try r1.smo_journal.commitEntry(reg.res, &frames);
        try be.fsync();
        try snapshotImage(s.paths[0], kill);
    }
    const folded0 = stats.recovery_data_bitmaps_replayed.load();
    const pages0 = stats.recovery_data_bitmap_pages_written.load();
    var live = try Live.open(s, kill);
    defer live.close();
    const be = live.set.volumes[0];
    live.recordDeath(identity, false);
    live.plane.tick();
    try testing.expectEqual(folded0 + 1, stats.recovery_data_bitmaps_replayed.load());
    try testing.expectEqual(pages0 + 1, stats.recovery_data_bitmap_pages_written.load());
    // The pages read back: exactly the run's bits set.
    var dev: VolumeBinding.DeviceRegion = .{ .be = be };
    const img = try data_bitmap.readRegionImage(gpa, dev.io(), refs.items, DATA_BLOCKS);
    defer gpa.free(img);
    const alloc = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = DATA_VOL_TAG, .vol = 0, .capacity_bytes = DATA_BLOCKS * block_alloc.CHUNK_SIZE });
    defer alloc.deinit();
    const bm = try data_bitmap.DataBitmap.init(gpa, alloc, .{ .vol_tag = DATA_VOL_TAG, .term = DATA_LEASE_TERM, .blocks = DATA_BLOCKS, .ring_capacity = 1 });
    defer bm.deinit();
    const loaded = try bm.loadRegionImage(img);
    try testing.expectEqual(@as(u64, DELTA_LEN), loaded.bits_set);
    var i: u64 = 0;
    while (i < DELTA_LEN) : (i += 1) try testing.expect(bm.isSet(DELTA_START + i));
    try testing.expect(!bm.isSet(DELTA_START + DELTA_LEN));
    // The lease still names the holder: the region stays Recovered.
    live.plane.tick();
    try testing.expectEqual(page.State.recovered, (try be.sym.?.manager.readPageOf(1)).?.state);
}

// ---- the recovery against the manager's own SMOs (rung 10 recovery item 1) ----

/// Every extent's bit and the free budget agree: `free = total − set bits`
/// (a pending-free extent keeps its bit and is not credited) — read under
/// the SMO lock, so no cycle claims or drains between the two reads.
fn expectBudgetConserved(be: *KvBackend) !void {
    const gpa = testing.allocator;
    be.smo_lock.lockUncancelable(be.io);
    defer be.smo_lock.unlock(be.io);
    const core = &be.alloc.core;
    const words = try gpa.alloc(u64, core.wordCount());
    defer gpa.free(words);
    core.snapshotWords(words);
    var set: u64 = 0;
    for (words) |w| set += @popCount(w);
    try testing.expectEqual(core.total - set, be.alloc.freeExtents());
}

/// A wire joiner B (a second `MetaSet` joined over loopback TCP) writes
/// into its one rotor slot — the tree minted on its first touch in ITS
/// grant — cycles, writes a window more, and dies (the image copied at
/// `kill` with its page Live). Answers B's identity, appender id, slot.
const WireDead = struct { identity: WriterIdentity, appender_id: u32, slot: u16 };

fn killWireJoiner(s: *meta_set.TestSet, kill: []const u8, keys: *std.ArrayList([]const u8)) !WireDead {
    const gpa = testing.allocator;
    const set = try s.open();
    defer set.close();
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
    defer bound.destroy(gpa);
    const plane = (try wire_plane.WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
    defer plane.stop();
    const be0 = set.volumes[0];
    const b = try meta_set.MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-recover-smo-b", .endpoint = plane.advertised } });
    defer b.close();
    const bj = b.volumes[0];
    try testing.expectEqual(backend.Posture.joined, bj.posture);
    const me = bj.joined.?.appenderId();
    var held: std.ArrayList(u16) = .empty;
    defer held.deinit(gpa);
    try bj.slots.table.heldBy(me, &held);
    try testing.expectEqual(@as(usize, 1), held.items.len);
    const slot = held.items[0];
    for (0..16) |i| {
        if (i == 12) try bj.checkpointNow();
        const local = try bj.allocateInoInSlot(slot);
        const k = try gpa.dupe(u8, &key.inodeKey(key.guestLocalIno(slot, local)));
        try keys.append(gpa, k);
        const t = try bj.begin(@src());
        try t.stagePut(key.TREE_INODES, k, &iv_value);
        try bj.commit(t);
    }
    try bj.fsync();
    try be0.checkpointNow();
    try be0.fsync();
    try snapshotImage(s.paths[0], kill);
    const id = bj.joined.?.identity;
    return .{ .identity = .{ .node_token = id.node_token, .mount_slot = id.mount_slot, .writer_id = id.writer_id }, .appender_id = me, .slot = slot };
}

test "linux: THE RECOVERY'S CENSUS AGAINST THE MANAGER'S OWN SMO — a dead wire joiner's slot tree has its root image in the joiner's grant; the recovery parks after its flush (SQUEEZEFS_TEST_RECOVERY_HOLD) while the manager's D4 compaction retires that image; the recovery completes and the retired extent leaves the grant by ONE path: never returned by the census (no orphan), allocated until the coverage clock covers its free, then free — the budget conserved (red: the census judged it an orphan and released it while the pending-free FIFO still held it, the shared hot directory row's `alloc_core.freePending` abort)" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "recover-smo", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    try s.env.put("SQUEEZEFS_SYM_MINT_SLOTS", "1");
    var kill_buf: [160]u8 = undefined;
    const kill = try std.fmt.bufPrint(&kill_buf, "{s}.kill", .{s.paths[0]});
    defer unlinkPath(kill);
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    const dead = try killWireJoiner(s, kill, &keys);
    try s.env.put("SQUEEZEFS_TEST_RECOVERY_HOLD", "park_after_flush");
    var live = try Live.open(s, kill);
    defer live.close();
    const be = live.set.volumes[0];
    const binding = live.binding();
    try testing.expect(be.sym.?.isUnrecovered(dead.appender_id));
    // The precondition: the dead slot's root image — the page's entry, the
    // root the recovery installs — lives in B's grant.
    var grant_fam = try binding.grantOf(gpa, dead.appender_id);
    defer grant_fam.deinit();
    const dead_page = (try be.sym.?.manager.readPageOf(dead.appender_id)).?;
    var root_addr: u64 = 0;
    for (dead_page.slotEntries()) |e| if (e.slot == dead.slot) {
        root_addr = e.root_addr;
    };
    try testing.expect(root_addr >= be.sb.heap.start);
    const x = be.cache.addrExtent(root_addr);
    try testing.expect(grant_fam.contains(x));
    const recoveries0 = recovery.stats.appender_recoveries.load();
    const orphans0 = recovery.stats.dead_appender_orphans.load();
    live.recordDeath(dead.identity, false);
    const Ticker = struct {
        fn run(l: *Live) void {
            l.plane.tick();
        }
    };
    const th = try std.Thread.spawn(.{}, Ticker.run, .{&live});
    var joined = false;
    defer if (!joined) {
        binding.hold.release.set(be.io);
        th.join();
    };
    binding.hold.reached.waitUncancelable(be.io);
    // The manager's own SMO on the recovered tree mid-recovery: the D4 arm
    // compacts its root leaf into a fresh image from the manager's pool.
    const t = be.slotTree(dead.slot).?;
    try testing.expectEqual(x, be.cache.addrExtent(t.rootPtr().addr));
    {
        be.smo_lock.lockUncancelable(be.io);
        defer be.smo_lock.unlock(be.io);
        const n = try t.getNode(t.rootPtr().addr);
        defer n.release();
        var out: smo.MaintenanceOutcome = .{};
        try testing.expect(try smo.compactForDefrag(t, &be.smo_ctx, n, &out));
    }
    try testing.expect(be.cache.addrExtent(t.rootPtr().addr) != x);
    binding.hold.release.set(be.io);
    th.join();
    joined = true;
    try testing.expectEqual(recoveries0 + 1, recovery.stats.appender_recoveries.load());
    try expectAllPresent(live.set, keys.items);
    // The retired image is no orphan of the dead grant: the coverage clock
    // holds it, its bit set until a durable tail covers its free.
    try testing.expectEqual(orphans0, recovery.stats.dead_appender_orphans.load());
    try testing.expect(be.alloc.isAllocated(x));
    try expectBudgetConserved(be);
    // The grant record no longer names it.
    {
        var after = try binding.grantOf(gpa, dead.appender_id);
        defer after.deinit();
        try testing.expect(!after.contains(x));
    }
    // Covered: free exactly once, the budget conserved.
    try be.checkpointNow();
    try be.checkpointNow();
    try testing.expect(!be.alloc.isAllocated(x));
    try expectBudgetConserved(be);
    try expectAllPresent(live.set, keys.items);
}

// ---- the coherence hold (rung 10 recovery item 2) ----

test "linux: THE COHERENCE HOLD — a write into a recovered slot right after the recovery is refused (SlotBusy, the EAGAIN class) until every live member's census pull has retired the dead holder (the death record's ts + VALIDITY_RENEWALS × renew + 2 × skew_max): before it a joiner may still serve the dead holder's cached token, and the write would contradict it; a death recorded that long ago admits the write at once (red: the successor served the write the instant the slot was Unleased)" {
    const gpa = testing.allocator;
    const s = try meta_set.TestSet.create(gpa, "recover-hold", 1);
    defer s.destroy();
    try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    var kill_buf: [160]u8 = undefined;
    const kill = try std.fmt.bufPrint(&kill_buf, "{s}.kill", .{s.paths[0]});
    defer unlinkPath(kill);
    var dst_buf: [160]u8 = undefined;
    const dst = try std.fmt.bufPrint(&dst_buf, "{s}.cut", .{s.paths[0]});
    defer unlinkPath(dst);
    var keys: std.ArrayList([]const u8) = .empty;
    defer freeKeys(&keys);
    const dead = try killWithDeadRegion(s, kill, &keys, 6);
    for ([_]bool{ true, false }) |fresh| {
        try snapshotImage(kill, dst);
        var live = try Live.open(s, dst);
        defer live.close();
        const be = live.set.volumes[0];
        const clocks = live.plane.mgr.?.owner.clocks;
        const hold_ms = recovery.coherenceHoldMs(clocks.renew_ms, clocks.skew_max_ms);
        const now_rt: u64 = @intCast(clock.realtimeCoarseNs());
        // The eviction's instant: now (the window open), or one hold and
        // a second ago (every member's pull has retired the holder).
        const ts = if (fresh) now_rt else now_rt - (hold_ms + 1000) * std.time.ns_per_ms;
        live.plane.deathLedger().?.recordDeath(dead.identity, .{ .epoch = 1, .ts_realtime_ns = ts, .pr_key = 0, .early = false });
        const recoveries0 = recovery.stats.appender_recoveries.load();
        live.plane.tick();
        try testing.expectEqual(recoveries0 + 1, recovery.stats.appender_recoveries.load());
        try testing.expectEqual(lt.State.unleased, be.slots.table.view(DEAD_SLOT).state);
        try expectDeadWindowRecovered(live.set, keys.items);
        const k = try slotKey(be, DEAD_SLOT, 5000);
        defer gpa.free(k);
        const t = try be.begin(@src());
        try t.stagePut(key.TREE_INODES, k, &iv_value);
        if (fresh) {
            const refusals0 = slots_door.stats.slot_door_coherence_refusals.load();
            try testing.expectError(error.SlotBusy, be.commit(t));
            try testing.expectEqual(refusals0 + 1, slots_door.stats.slot_door_coherence_refusals.load());
            // A joiner's first touch over the wire meets the same hold.
            const wire0 = slots_acquire.stats.slot_acquire_coherence_refusals.load();
            const out = try be.slots.verbs.acquireSlot(gpa, 5, DEAD_SLOT, .park);
            try testing.expect(out == .refused and out.refused.holder == lt.MANAGER);
            try testing.expectEqual(wire0 + 1, slots_acquire.stats.slot_acquire_coherence_refusals.load());
            try testing.expectEqual(lt.State.unleased, be.slots.table.view(DEAD_SLOT).state);
        } else {
            try be.commit(t);
            try testing.expect(be.slots.table.view(DEAD_SLOT).leasedBy(0));
            const got = try live.set.lookup(gpa, key.TREE_INODES, k);
            defer if (got) |g| gpa.free(g);
            try testing.expect(got != null);
        }
    }
}

// ---- the recovery under the manager's maintenance, looped (rung 10 recovery item 1's stage) ----

const testkit = @import("../../../core/testkit.zig");
const recovery_loops_knob = "SQUEEZEFS_TEST_RECOVERY_RACE_LOOPS";

/// The manager's maintenance racing a recovery: the D4 compaction of
/// every resident leaf of the recovered slot's tree, round after round,
/// under the SMO lock, from the slot's dead mark (`Releasing`) until the
/// recovery is done — the window the shared hot directory row's abort met.
const MaintenanceRacer = struct {
    be: *KvBackend,
    binding: *VolumeBinding,
    slot: u16,
    stop: std.atomic.Value(bool) = .init(false),
    compactions: u64 = 0,
    rounds: u32 = 0,

    /// Two rounds: a maintenance pass compacts a leaf and is done with it
    /// (an endless stream re-dirties the parents faster than the flush's
    /// bounded cycles can land them — no product SMO behaves so).
    const ROUNDS: u32 = 2;

    fn run(self: *MaintenanceRacer) void {
        const gpa = testing.allocator;
        while (!self.stop.load(.acquire) and self.rounds < ROUNDS) {
            self.be.smo_lock.lockUncancelable(self.be.io);
            defer self.be.smo_lock.unlock(self.be.io);
            // From the root install (the recovered tree is the manager's
            // to maintain) to the table release.
            const v = self.be.slots.table.view(self.slot);
            if (v.state != .releasing or !v.dead or self.binding.install_floor == 0) continue;
            const t = self.be.slotTree(self.slot) orelse continue;
            var tails: std.ArrayList(cr.TailEntry) = .empty;
            defer tails.deinit(gpa);
            self.be.collectLeafTails(t, &tails) catch continue;
            for (tails.items) |tl| {
                const n = t.getNode(tl.leaf_addr) catch continue;
                defer n.release();
                var out: smo.MaintenanceOutcome = .{};
                if (smo.compactForDefrag(t, &self.be.smo_ctx, n, &out) catch false) self.compactions += 1;
            }
            self.rounds += 1;
        }
    }
};

/// One custodian per extent after a recovery: every extent a maintained
/// tree reaches is allocated, and no free one is reached — checked by
/// claiming every claimable extent (none may be a live node), the claims
/// handed back.
fn expectOneCustodian(binding: *VolumeBinding) !void {
    const gpa = testing.allocator;
    const be = binding.be;
    be.smo_lock.lockUncancelable(be.io);
    defer be.smo_lock.unlock(be.io);
    var images: std.ArrayList(u64) = .empty;
    defer images.deinit(gpa);
    try binding.maintainedImages(gpa, &images);
    for (images.items) |e| try testing.expect(be.alloc.isAllocated(e));
    std.mem.sort(u64, images.items, {}, std.sort.asc(u64));
    var claimed: std.ArrayList(u64) = .empty;
    defer {
        for (claimed.items) |e| be.alloc.releaseUnpublished(e);
        claimed.deinit(gpa);
    }
    while (be.alloc.claimInternal()) |e| {
        try claimed.append(gpa, e);
        try testing.expect(!grant.sortedContains(images.items, e));
    } else |_| {}
}

test "linux: THE RECOVERY UNDER THE MANAGER'S MAINTENANCE, looped (SQUEEZEFS_TEST_RECOVERY_RACE_LOOPS=N; skipped unarmed): a wire joiner whose slot tree spans several leaves of its grant dies with a window; the manager recovers it while a racer compacts every resident leaf of the recovered tree round after round — every acked record readable, the budget conserved, one custodian per extent before and after the coverage clock drains, every pass" {
    const gpa = testing.allocator;
    const raw = std.testing.environ.getPosix(recovery_loops_knob) orelse return testkit.skip(@src(), .no_seam, recovery_loops_knob ++ " unset: the looped recovery stage runs only when the harness arms it");
    const loops = std.fmt.parseInt(u32, std.mem.trim(u8, raw, " \t"), 10) catch return testkit.skip(@src(), .no_seam, recovery_loops_knob ++ " is not an integer");
    if (loops == 0) return testkit.skip(@src(), .no_seam, recovery_loops_knob ++ " is 0");
    var held_total: u64 = 0;
    var compactions_total: u64 = 0;
    var pass: u32 = 0;
    while (pass < loops) : (pass += 1) {
        const s = try meta_set.TestSet.create(gpa, "recover-race", 1);
        defer s.destroy();
        try s.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
        try s.env.put("SQUEEZEFS_SYM_MINT_SLOTS", "1");
        // B's splits draw its grant: a pool past the derived floor keeps
        // the fill off the refill's wire round trips.
        try s.env.put("SQUEEZEFS_SYM_GRANT_EXTENTS", "64");
        var kill_buf: [160]u8 = undefined;
        const kill = try std.fmt.bufPrint(&kill_buf, "{s}.kill", .{s.paths[0]});
        defer unlinkPath(kill);
        var keys: std.ArrayList([]const u8) = .empty;
        defer freeKeys(&keys);
        // B: 1,200 records over several leaves (its own splits draw its
        // grant), a cycle, then a 300-record window — and dies.
        var dead: WireDead = undefined;
        {
            const set = try s.open();
            defer set.close();
            var locks = try dlm.Manager.init(gpa, 64);
            defer locks.deinit();
            const bound = try xv.set_bind.Bound.create(gpa, s.io(), set, &locks, &s.env);
            defer bound.destroy(gpa);
            const plane = (try wire_plane.WirePlane.start(gpa, s.io(), set, &s.env, 4, 1 << 30, &bound.plane, false)).?;
            defer plane.stop();
            const be0 = set.volumes[0];
            const b = try meta_set.MetaSet.open(gpa, s.io(), s.pathList(), .{ .env = &s.env, .cache_budget_bytes = 64 << 20, .cpus = 4, .join = .{ .mountpoint = "/build/tmp/mnt-recover-race-b", .endpoint = plane.advertised } });
            defer b.close();
            const bj = b.volumes[0];
            const me = bj.joined.?.appenderId();
            var held: std.ArrayList(u16) = .empty;
            defer held.deinit(gpa);
            try bj.slots.table.heldBy(me, &held);
            const slot = held.items[0];
            for (0..15) |batch| {
                if (batch == 12) try bj.checkpointNow();
                const t = try bj.begin(@src());
                for (0..100) |_| {
                    const local = try bj.allocateInoInSlot(slot);
                    const k = try gpa.dupe(u8, &key.inodeKey(key.guestLocalIno(slot, local)));
                    try keys.append(gpa, k);
                    try t.stagePut(key.TREE_INODES, k, &iv_value);
                }
                try bj.commit(t);
            }
            try bj.fsync();
            try be0.checkpointNow();
            try be0.fsync();
            try snapshotImage(s.paths[0], kill);
            const id = bj.joined.?.identity;
            dead = .{ .identity = .{ .node_token = id.node_token, .mount_slot = id.mount_slot, .writer_id = id.writer_id }, .appender_id = me, .slot = slot };
        }
        var live = try Live.open(s, kill);
        defer live.close();
        const be = live.set.volumes[0];
        const binding = live.binding();
        const recoveries0 = recovery.stats.appender_recoveries.load();
        var racer: MaintenanceRacer = .{ .be = be, .binding = binding, .slot = dead.slot };
        const th = try std.Thread.spawn(.{}, MaintenanceRacer.run, .{&racer});
        var joined = false;
        defer if (!joined) {
            racer.stop.store(true, .release);
            th.join();
        };
        const held0 = smo.stats.meta_kv_retire_hold_breaches.load();
        live.recordDeath(dead.identity, false);
        live.plane.tick();
        racer.stop.store(true, .release);
        th.join();
        joined = true;
        try testing.expectEqual(held0, smo.stats.meta_kv_retire_hold_breaches.load());
        try testing.expectEqual(recoveries0 + 1, recovery.stats.appender_recoveries.load());
        try expectAllPresent(live.set, keys.items);
        try expectBudgetConserved(be);
        try expectOneCustodian(binding);
        try be.checkpointNow();
        try be.checkpointNow();
        try expectBudgetConserved(be);
        try expectOneCustodian(binding);
        try expectAllPresent(live.set, keys.items);
        compactions_total += racer.compactions;
        held_total += recovery.stats.appender_recoveries.load() - recoveries0;
    }
    std.debug.print("\nrecovery under maintenance: {d} pass(es), {d} recoveries, {d} racing compactions, 0 custody breaches\n", .{ loops, held_total, compactions_total });
}
