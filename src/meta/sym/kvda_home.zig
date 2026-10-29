//! The KVDA page sink's HOME binding (Linux; charter §5.5 "a data
//! allocation bitmap (1 bit per block, A/B pages `KVDA`, 32 KiB/TiB, in
//! extents of the holder's grant on its HOME volume; mutations are
//! `vol_tag`-prefixed deltas in the holder's ring … pages written before
//! barrier #1)"; `docs/porting/sym_data_planes.md` §2.3–§2.6; rung 10 item
//! 2, 0002 row 292's close): `plane_arm.Home` over the manager's volume 0
//! — the page extents claimed from its heap through the manager's own
//! allocator and journaled as kind-4 `alloc` deltas in ONE control entry
//! (appender 0 claims directly: "its alloc deltas journaled in ring 0 like
//! every claim of its own"), the images written O_DIRECT and barriered,
//! the refs published in `alloc_lease:` on tree 0 (`AllocLeaseBitmap`),
//! every bitmap RUN delta a kind-5 record of ONE ring-0 control entry in
//! the USER class (`ReserveExhausted` on a full window — the deltas stay
//! queued, the next carve or tick journals them), a predecessor's extents
//! freed once this holder's own pages are named; and the same-node
//! successor's fold: the KEPT WINDOW of the mount's replay (the fixed
//! ring's kind-5 deltas, `KvBackend.kept_data_deltas`) folded onto the
//! predecessor's pages at exactly its term before the lease is re-held.

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const backend = @import("../backend.zig");
const checkpoint = @import("../checkpoint.zig");
const manager = @import("manager.zig");
const bind = @import("bind.zig");
const data_bitmap = @import("../../data/sym_planes/data_bitmap.zig");
const alloc_lease = @import("../../data/sym_planes/alloc_lease.zig");
const plane_arm = @import("../../data/sym_planes/plane_arm.zig");
const records = @import("../../data/sym_planes/records.zig");
const block_refs = @import("../../data/block_refs.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("kvda_home is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const KvBackend = backend.KvBackend;

pub const KvdaHome = struct {
    gpa: Allocator,
    /// The home volume (volume 0: the manager's ring and heap).
    be: *KvBackend,
    aset: *bind.AppenderSet,
    control: alloc_lease.ControlStore,
    screen: alloc_lease.Screen,
    identity: records.WriterIdentity,

    const region_vt: data_bitmap.RegionIo.VTable = .{ .write = &regionWrite, .read = &regionRead, .barrier = &regionBarrier };
    const home_vt: plane_arm.Home.VTable = .{ .hold = &hold, .publish = &publish, .freeExtents = &freeExtents, .generation = &generation, .referencedFresh = &referencedFresh };

    pub fn home(self: *KvdaHome) plane_arm.Home {
        return .{ .ctx = self, .home_vol = self.be.index, .io = self.io(), .journal = .{ .ctx = self, .journal = &journalDeltas }, .vt = &home_vt };
    }

    pub fn io(self: *KvdaHome) data_bitmap.RegionIo {
        return .{ .ctx = self, .vtable = &region_vt };
    }

    fn self_(ctx: *anyopaque) *KvdaHome {
        return @ptrCast(@alignCast(ctx));
    }

    // ---- the device face ----

    fn regionWrite(ctx: *anyopaque, off: u64, bytes: []const u8) error{Io}!void {
        const self = self_(ctx);
        self.be.pool.writeAt(self.be.path, off, bytes) catch return error.Io;
    }

    fn regionRead(ctx: *anyopaque, off: u64, out: []u8) error{Io}!void {
        const self = self_(ctx);
        const img = self.be.dev.readRange(self.gpa, off, out.len) catch return error.Io;
        defer self.gpa.free(img);
        @memcpy(out, img[0..out.len]);
    }

    fn regionBarrier(ctx: *anyopaque) error{Io}!void {
        const self = self_(ctx);
        self.be.dev.barrier() catch return error.Io;
    }

    // ---- the ring-0 delta journal ----

    /// ONE control entry in the USER class, `try_`-admitted: a full
    /// window is the drainer's park class (the deltas stay queued).
    fn journalDeltas(ctx: *anyopaque, vol_tag: u64, deltas: []const data_bitmap.RunDelta) data_bitmap.JournalError!void {
        const self = self_(ctx);
        const gpa = self.gpa;
        const keys = gpa.alloc([records.DELTA_KEY_LEN]u8, deltas.len) catch return error.Io;
        defer gpa.free(keys);
        const values = gpa.alloc([records.DELTA_LEN]u8, deltas.len) catch return error.Io;
        defer gpa.free(values);
        const recs = gpa.alloc(manager.ControlRecord, deltas.len) catch return error.Io;
        defer gpa.free(recs);
        for (deltas, 0..) |d, i| {
            keys[i] = d.encodeKey(vol_tag);
            values[i] = d.encodeValue();
            recs[i] = .{ .data_delta = .{ .key = &keys[i], .value = &values[i] } };
        }
        const vol = self.aset.volume();
        vol.vt.controlEntry(vol.ctx, recs) catch |e| return switch (e) {
            error.JournalReserveExhausted => error.ReserveExhausted,
            else => error.Io,
        };
    }

    // ---- the hold, the publish, the free ----

    /// `max(1, ceil(image / node))` extents claimed from the home's heap,
    /// the image written across them in claim order, barriered, THEN the
    /// claims journaled as ONE control entry; a failed write or journal
    /// releases every claim unpublished. The refs are the claims coalesced
    /// into runs.
    fn hold(ctx: *anyopaque, image: []const u8, out_refs: *std.ArrayList(records.BitmapRef)) data_bitmap.HoldError!void {
        const self = self_(ctx);
        const gpa = self.gpa;
        const be = self.be;
        const node: u64 = be.sb.node_size;
        const needed: usize = @intCast(@max(1, (image.len + node - 1) / node));
        var claims: std.ArrayList(u64) = .empty;
        defer claims.deinit(gpa);
        errdefer for (claims.items) |c| be.alloc.releaseUnpublished(c);
        for (0..needed) |_| try claims.append(gpa, be.alloc.claimInternal() catch return error.GrantExhausted);
        var off: usize = 0;
        for (claims.items) |c| {
            if (off >= image.len) break;
            const n: usize = @intCast(@min(image.len - off, node));
            be.pool.writeAt(be.path, be.sb.heap.start + c * node, image[off .. off + n]) catch return error.Io;
            off += n;
        }
        be.dev.barrier() catch return error.Io;
        const recs = try gpa.alloc(manager.ControlRecord, claims.items.len);
        defer gpa.free(recs);
        for (claims.items, recs) |c, *r| r.* = .{ .alloc = c };
        try self.controlEntryCycling(recs);
        const first = out_refs.items.len;
        errdefer out_refs.shrinkRetainingCapacity(first);
        for (claims.items) |c| {
            const start = be.sb.heap.start + c * node;
            if (out_refs.items.len > first) {
                const last = &out_refs.items[out_refs.items.len - 1];
                if (last.start + last.len == start) {
                    last.len += node;
                    continue;
                }
            }
            if (out_refs.items.len >= records.BITMAP_REFS_MAX) return error.TooManyRefs;
            try out_refs.append(gpa, .{ .vol = be.index, .start = start, .len = node });
        }
    }

    /// A control entry the arm may park for (`parking`: the window cycled
    /// clear, bounded by the structural door's cap).
    fn controlEntryCycling(self: *KvdaHome, recs: []const manager.ControlRecord) error{Io}!void {
        const vol = self.aset.volume();
        var cycles: u32 = 0;
        while (true) {
            vol.vt.controlEntry(vol.ctx, recs) catch |e| switch (e) {
                error.JournalReserveExhausted => {
                    if (cycles == checkpoint.COVER_CYCLES_MAX) return error.Io;
                    cycles += 1;
                    _ = vol.vt.cycleCheckpoint(vol.ctx) catch return error.Io;
                    continue;
                },
                else => return error.Io,
            };
            return;
        }
    }

    fn publish(ctx: *anyopaque, vol_tag: u64, term: u64, refs: []const records.BitmapRef) plane_arm.Home.PublishVerdict {
        const self = self_(ctx);
        const v = alloc_lease.publishBitmap(self.control, self.gpa, self.screen, .{ .vol_tag = vol_tag, .identity = self.identity, .term = term, .refs = refs });
        return switch (v) {
            .published => .published,
            .already => .already,
            .busy, .rejected, .deferred, .io_error => blk: {
                log.err("data planes: the bitmap pages of vol {x} at term {d} could not be published in alloc_lease: ({t})", .{ vol_tag, term, v });
                break :blk .refused;
            },
        };
    }

    /// A predecessor's page extents after this holder's own are named:
    /// kind-4 `free(extent, retire 0)` deltas in ONE control entry (the
    /// publish that superseded them was barriered before this).
    fn freeExtents(ctx: *anyopaque, refs: []const records.BitmapRef) void {
        const self = self_(ctx);
        const be = self.be;
        const node: u64 = be.sb.node_size;
        var recs: std.ArrayList(manager.ControlRecord) = .empty;
        defer recs.deinit(self.gpa);
        for (refs) |r| {
            if (r.vol != be.index or r.start < be.sb.heap.start) continue;
            var e = (r.start - be.sb.heap.start) / node;
            const end = e + r.len / node;
            while (e < end) : (e += 1) {
                be.alloc.releaseUnpublished(e);
                recs.append(self.gpa, .{ .free = e }) catch return;
            }
        }
        if (recs.items.len == 0) return;
        self.controlEntryCycling(recs.items) catch log.warn("data planes: the predecessor's {d} bitmap page extent(s) could not be journaled free — the next checkpoint's bitmap page carries the release", .{recs.items.len});
    }

    /// This holder's own page extents after its clean stop released the
    /// leases that named them: the releases made durable first (a crash
    /// before that keeps the records naming the pages — the successor
    /// folds them and frees them as a predecessor's), then the kind-4
    /// frees as `freeExtents` journals a predecessor's.
    pub fn releaseOwnPages(self: *KvdaHome, refs: []const records.BitmapRef) void {
        const vol = self.aset.volume();
        _ = vol.vt.cycleCheckpoint(vol.ctx) catch {
            log.warn("data planes: the released allocation lease(s) could not be made durable — {d} bitmap page run(s) stay allocated until fsck", .{refs.len});
            return;
        };
        freeExtents(self, refs);
    }

    fn generation(ctx: *anyopaque) u64 {
        return self_(ctx).be.cp.checkpointSeq() + 1;
    }

    /// The refs census over the set's trees as this mount holds them (the
    /// convergence's exact census when no other writer is live).
    fn referencedFresh(ctx: *anyopaque, vol_tag: u64, out: []u64) error{Io}!void {
        const self = self_(ctx);
        const gpa = self.gpa;
        @memset(out, 0);
        var idxs: std.ArrayList(u64) = .empty;
        defer idxs.deinit(gpa);
        _ = block_refs.volumePopulation(block_refs.RefStore.from(KvBackend, self.be), gpa, vol_tag, &idxs) catch return error.Io;
        for (idxs.items) |idx| {
            if (idx / 64 < out.len) out[@intCast(idx / 64)] |= @as(u64, 1) << @intCast(idx % 64); // 64 bits per word
        }
    }

    // ---- the same-node successor's fold ----

    /// `alloc_lease.Recover` for the predecessor MANAGER's bitmap (its
    /// ring IS the fixed ring the mount replayed): the kept window folded
    /// onto the predecessor's pages through the lease record's refs at
    /// its term, barriered. True = folded (or nothing to fold); false =
    /// the pages could not be read or written (the arm answers `Io`).
    pub fn foldPredecessor(self: *KvdaHome, vol_tag: u64, refs: []const records.BitmapRef, blocks: u64, term: u64) bool {
        const gpa = self.gpa;
        if (refs.len == 0) return true;
        if (!data_bitmap.screenBitmapRefs(refs, blocks, self.screen.heaps)) {
            log.err("data planes: the predecessor's bitmap refs of vol {x} do not screen against the set's heaps — refusing the fold", .{vol_tag});
            return false;
        }
        const deltas = self.be.keptDataDeltas(gpa) catch return false;
        defer gpa.free(deltas);
        const out = data_bitmap.foldOntoRegion(gpa, self.io(), refs, vol_tag, blocks, term, deltas, self.be.cp.checkpointSeq() + 1) catch |e| {
            log.err("data planes: the kept window could not be folded onto the predecessor's bitmap pages of vol {x} ({s})", .{ vol_tag, @errorName(e) });
            return false;
        };
        data_bitmap.stats.data_alloc_bitmap_predecessor_folds.inc();
        log.info("data planes: vol {x} — the kept window ({d} delta record(s)) folded onto the predecessor's pages at term {d}: {d} bit(s) changed, {d} page(s) rewritten", .{ vol_tag, deltas.len, term, out.bits_changed, out.pages_written });
        return true;
    }
};
