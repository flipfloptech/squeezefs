//! KEY ROTATION as a defrag-class mover — `defrag --rekey` (charter §6.5
//! + §8 "ONE mover primitive"; `docs/porting/ops_nvmet_keys.md` §7.4
//! "never a cheaper rotation that leaves old headers readable under the
//! retired key"). The operator key never touches a block's bytes: every
//! block header carries the mount's SESSION key wrapped under the KEK the
//! operator key derives, so rotating the operator key means every block's
//! header must carry a wrap under the NEW KEK — and the one law for
//! writing a block is the pre-allocated-unpublished-destination law. So
//! the rotation is THE ONE MOVER with a REWRITE hook (`mover.Seam.rewrite`):
//! the stored image is read, every window in it (the whole-block image or
//! each tenant window the referencers name) decoded under the key it is
//! under and re-encoded under the next key — the same plaintext, the same
//! compression, the same stored length — into a FRESH block the map
//! publishes; the old block enters pending-free. A crash at any step
//! leaves every block readable under exactly one of the two keys the
//! config names.
//!
//! The window (the config's `next_key`, `set_config.openRekey`): opened
//! DURABLY before any block is written under the next key; from then every
//! new write wraps under it (`CryptoState.beginRotation`) and reads unwrap
//! under either. The sweep: per pass the census of every binding, each
//! block's windows CLASSIFIED by their header's wrap (`previous` ⇒ a task),
//! the tasks moved through the mover with the rewrite; a pass that finds
//! no `previous` window among the bindings runs the CONFIRMING census —
//! after the plane's quiescence (the write pipeline drained, the detached
//! promotions done, the open packs sealed, no tenant in flight) — over
//! EVERY allocated block's first header (an image DMA'd before its publish
//! is allocated, so a late publish of an old-key image cannot slip past);
//! only then the config flips the primary to the next key (`closeRekey`,
//! durable) and the old key is forgotten (`CryptoState.endRotation`). The
//! two begin barriers (the codec pool, then the scratch pool) are what
//! make the confirming census complete: every transform under the old
//! wrap has returned and every such image's DMA has landed before the
//! first census runs.
//!
//! Portable over seams: the plane binds `PlaneRekey` (`jobs/coordinator.zig`);
//! the tests bind the mover's RAM world with REAL framed images under two
//! `CryptoState`s and kill the mover at every step.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const counters = @import("../../core/counters.zig");
const transform = @import("../../data/transform.zig");
const layout = @import("../../data/layout.zig");
const fabric_mod = @import("../jobs/fabric.zig");
const census = @import("../lifecycle/census.zig");
const mover = @import("../lifecycle/mover.zig");
const metrics = @import("metrics.zig");

pub const stats = metrics.stats;
const Fabric = fabric_mod.Fabric;
const JobCtl = fabric_mod.JobCtl;
pub const Mapping = layout.Mapping;
pub const Ref = census.Ref;
pub const Task = census.Task;

/// Which key a stored image's header is wrapped under (the sweep's unit).
pub const Class = enum {
    /// Under the next key: nothing to do.
    current,
    /// Under the retiring key: a task.
    previous,
    /// Under neither (a block written under a key this set never named,
    /// or a corrupt header): the sweep leaves it and the job fails loud at
    /// the end — fsck's C7 names it.
    foreign,
    /// Not a transform image at all (a zeroed window, a passthrough byte
    /// run): a pack block whose first tenant left, or an unformatted block.
    none,
};

/// An allocated block the confirming census visits.
pub const BlockKey = struct { vol_tag: u64, block_idx: u64 };

/// What the sweep needs of the plane beyond the mover.
pub const RekeySeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// The mover seam WITH the rewrite hook bound.
        mover: *const fn (ctx: *anyopaque) mover.Seam,
        /// The open window's next key id; null = no window (the job is
        /// stale: a resumed record after the flip).
        windowKeyId: *const fn (ctx: *anyopaque) ?[transform.KEY_ID_LEN]u8,
        /// Every binding of every live file.
        refs: *const fn (ctx: *anyopaque, gpa: Allocator) Allocator.Error![]Ref,
        /// Every ALLOCATED block of every volume (the reserved chunk 0
        /// excluded): the confirming census's universe.
        allocatedBlocks: *const fn (ctx: *anyopaque, gpa: Allocator) Allocator.Error![]BlockKey,
        /// The class of the image at `rel_off` of a block (its first 4 KiB
        /// header read); `none` for a non-image.
        classify: *const fn (ctx: *anyopaque, vol_tag: u64, block_idx: u64, rel_off: u32) Class,
        /// The plane's quiescence before the confirming census: true iff
        /// the write pipeline drained, the detached promotions finished,
        /// the open packs sealed and no tenant is in flight.
        quiesce: *const fn (ctx: *anyopaque, io: Io) bool,
        drainReclaim: *const fn (ctx: *anyopaque) void,
        /// The LAST step: the config's primary ← next (durable), the old
        /// key forgotten.
        close: *const fn (ctx: *anyopaque) error{Io}!void,
        chunk: *const fn (ctx: *anyopaque) u64,
    };
};

/// The distinct windows of a task: a whole-block image at 0, or every
/// tenant `rel_off` (one per distinct offset; every ref at one offset
/// carries the same stored length on a transformed set — a conflict is a
/// tripwire and defers the block).
pub const Windows = struct {
    offs: [MAX_WINDOWS]u32 = undefined,
    lens: [MAX_WINDOWS]u32 = undefined,
    n: usize = 0,
    whole: bool = false,

    /// A 4 MiB pack at the 4 KiB grain holds at most 1024 tenant slots.
    pub const MAX_WINDOWS: usize = 1024;

    pub fn of(task: Task) error{ TooManyWindows, LenConflict }!Windows {
        var w: Windows = .{};
        for (task.refs) |r| {
            if (r.mapping.whole_block) {
                w.whole = true;
                continue;
            }
            var found = false;
            for (w.offs[0..w.n], w.lens[0..w.n]) |o, l| if (o == r.mapping.rel_off) {
                if (l != r.mapping.len) return error.LenConflict;
                found = true;
            };
            if (found) continue;
            if (w.n == MAX_WINDOWS) return error.TooManyWindows;
            w.offs[w.n] = r.mapping.rel_off;
            w.lens[w.n] = r.mapping.len;
            w.n += 1;
        }
        return w;
    }
};

pub const RekeyBody = struct {
    gpa: Allocator,
    seam: RekeySeam,

    pub fn body(self: *RekeyBody) fabric_mod.Body {
        return .{ .ctx = self, .pass = &pass };
    }

    fn pass(ctx: *anyopaque, io: Io, f: *Fabric, job: *JobCtl) fabric_mod.PassError!fabric_mod.PassOutcome {
        const self: *RekeyBody = @ptrCast(@alignCast(ctx));
        const vt = self.seam.vtable;
        const sctx = self.seam.ctx;
        const key_id = vt.windowKeyId(sctx) orelse {
            // The window closed (a record adopted after the flip): done.
            stats.rekey_window_open.set(0);
            return .completed;
        };
        if (job.rec.arg0 != 0 and job.rec.arg0 != std.mem.readInt(u64, &key_id, .little)) {
            log.err("defrag --rekey: the adopted record names next key id {x:0>16} but the open window's is {x} — refused (one rotation at a time)", .{ job.rec.arg0, &key_id });
            return error.ScopeIneligible;
        }
        stats.rekey_window_open.set(1);
        stats.rekey_passes.inc();
        vt.drainReclaim(sctx);
        const chunk = vt.chunk(sctx);
        const refs = try vt.refs(sctx, self.gpa);
        defer self.gpa.free(refs);
        // Every volume a binding names is a source (the rotation is set-wide).
        var sources: std.ArrayList(u64) = .empty;
        defer sources.deinit(self.gpa);
        for (refs) |r| {
            var seen = false;
            for (sources.items) |s| if (s == r.mapping.vol_tag) {
                seen = true;
            };
            if (!seen) try sources.append(self.gpa, r.mapping.vol_tag);
        }
        var walk: census.RamWalk = .{ .refs = refs };
        var plan = census.censusFor(self.gpa, walk.walk(), sources.items, chunk) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer plan.deinit(self.gpa);
        // The tasks: every block with a window under the previous key.
        var tasks: std.ArrayList(Task) = .empty;
        defer tasks.deinit(self.gpa);
        var foreign: u64 = 0;
        for (plan.tasks) |t| {
            const w = Windows.of(t) catch |e| {
                switch (e) {
                    error.LenConflict => stats.rekey_tenant_len_conflict.fire("two referencers of one tenant window name different stored lengths"),
                    error.TooManyWindows => stats.rekey_tenant_len_conflict.fire("more tenant windows than a chunk holds"),
                }
                continue;
            };
            var old = false;
            if (w.whole) switch (vt.classify(sctx, t.vol_tag, t.block_idx, 0)) {
                .previous => old = true,
                .foreign => foreign += 1,
                .current, .none => {},
            };
            for (w.offs[0..w.n]) |o| switch (vt.classify(sctx, t.vol_tag, t.block_idx, o)) {
                .previous => old = true,
                .foreign => foreign += 1,
                .current, .none => {},
            };
            if (old) try tasks.append(self.gpa, t);
        }
        stats.rekey_remaining_blocks.set(tasks.items.len);
        f.setRemaining(io, job, tasks.items.len);
        if (tasks.items.len != 0) {
            var moved: u64 = 0;
            var deferred: u64 = 0;
            for (tasks.items) |t| {
                const t0 = clock.monotonicNs();
                // A fresh block on the SAME volume when it still takes
                // placements, else the lowest-fill survivor: a rotation
                // never migrates data by itself.
                switch (mover.moveOne(vt.mover(sctx), self.gpa, t, .{ .balance = .{ .dest_hint = t.vol_tag } }, 0) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Io,
                }) {
                    .moved => {
                        moved += 1;
                        stats.rekey_blocks_rewritten.inc();
                        stats.rekey_bytes_rewritten.add(chunk);
                    },
                    .deferred => deferred += 1,
                    // Already under the next key by the time the mover
                    // read it (a foreground rewrite), or the census went
                    // stale: nothing to do for it this pass.
                    .superseded => stats.rekey_blocks_skipped_current.inc(),
                }
                if (f.taskDone(io, job, clock.monotonicNs() - t0) == .stop) return .progressed;
            }
            if (moved > 0) return .progressed;
            return if (deferred > 0) .deferred else .progressed;
        }
        if (foreign != 0) {
            log.err("defrag --rekey: {d} block(s) carry a key wrap under NEITHER the primary nor the next key — the sweep cannot rewrite them; run `squeezefs fsck --scrub` (C7 names them); the window stays open", .{foreign});
            return error.Io;
        }
        // No binding under the previous key: the confirming census over
        // every allocated block, after the plane's quiescence (a DMA'd,
        // not-yet-published image is allocated and so visible here).
        if (!vt.quiesce(sctx, io)) {
            stats.rekey_confirm_deferrals.inc();
            return .deferred;
        }
        vt.drainReclaim(sctx);
        const blocks = try vt.allocatedBlocks(sctx, self.gpa);
        defer self.gpa.free(blocks);
        var pending: u64 = 0;
        for (blocks) |b| switch (vt.classify(sctx, b.vol_tag, b.block_idx, 0)) {
            .previous => pending += 1,
            .current, .none, .foreign => {},
        };
        if (pending != 0) {
            log.info("defrag --rekey: the confirming census found {d} allocated block(s) still under the previous key (an image landed before its publish, or a leak the reclaimer owes) — re-planning", .{pending});
            stats.rekey_confirm_deferrals.inc();
            return .deferred;
        }
        // THE LAST STEP: the primary flips to the next key, durably; the
        // old key is forgotten.
        vt.close(sctx) catch return error.Io;
        stats.rekey_window_open.set(0);
        stats.rekey_remaining_blocks.set(0);
        stats.rekey_windows_closed.inc();
        log.info("defrag --rekey: every block is under key id {x}; the window closed and the previous key is forgotten", .{&key_id});
        return .completed;
    }
};

// ---------------------------------------------------------------------------
// The rewrite over a stored image: the plane's and the RAM world's one
// implementation — pure over a `Codec` of the volume and the two keys.
// ---------------------------------------------------------------------------

pub const RewriteError = error{ Corrupt, OutOfMemory, LenDrift };

/// The whole-block or per-window re-encode of `image[0..len]` IN PLACE
/// (the buffer is a chunk): every window under the previous key is decoded
/// through `codec` (whose crypto state holds both keys) and re-encoded
/// under the current key — the same plaintext, the same compressed bodies,
/// so the stored length is the old one exactly (`LenDrift` is the tripwire
/// otherwise); windows already current are copied verbatim; every other
/// byte of the new image is ZERO (a dead tenant's old-key image never
/// travels to the new block — the confirming census reads first headers).
/// Null = no window was under the previous key.
pub fn rewriteImage(gpa: Allocator, codec: *transform.Codec, crypto: *transform.CryptoState, w: Windows, image: []u8, len: usize) RewriteError!?usize {
    const cfg = codec.cfg();
    const scratch_len = transform.scratchLen(cfg.block_size);
    const out = try gpa.alloc(u8, image.len);
    defer gpa.free(out);
    @memset(out, 0);
    const plain = try gpa.alloc(u8, scratch_len);
    defer gpa.free(plain);
    const enc = try gpa.alloc(u8, scratch_len);
    defer gpa.free(enc);
    var any = false;
    const n_windows: usize = if (w.whole) 1 else w.n;
    var i: usize = 0;
    while (i < n_windows) : (i += 1) {
        const off: usize = if (w.whole) 0 else w.offs[i];
        if (off >= len) return error.Corrupt;
        const window = image[off..len];
        const h = transform.decodeHeader(cfg, window) catch return error.Corrupt;
        // The stored image: the header page's table, then the bodies
        // (`body_len` counts the bytes after the table).
        const stored_len: usize = transform.tableLen(transform.framesFor(h.plain_len)) + h.body_len;
        if (stored_len > window.len) return error.Corrupt;
        const padded = layout.ceilGrain(stored_len);
        const span = @min(padded, window.len);
        switch (crypto.classify(&h.key_blob)) {
            .current => {
                @memcpy(out[off .. off + span], window[0..span]);
                continue;
            },
            .previous => {},
            .foreign => return error.Corrupt,
        }
        const p = codec.processRead(window[0..span], plain) catch return error.Corrupt;
        const fresh = codec.processWrite(p, enc) catch return error.Corrupt;
        if (fresh.len != stored_len and fresh.len != padded) return error.LenDrift;
        if (fresh.len > span) return error.LenDrift;
        @memcpy(out[off .. off + fresh.len], fresh);
        any = true;
    }
    if (!any) return null;
    @memcpy(image[0..len], out[0..len]);
    return len;
}

// ---------------------------------------------------------------------------
// Tests: the RAM world with REAL framed images under two keys.
// ---------------------------------------------------------------------------

const testing = std.testing;
const records = @import("../jobs/records.zig");

var seed: std.atomic.Value(u64) = .init(0x4e4e);
fn rnd() u64 {
    return seed.fetchAdd(0x9E37_79B9_7F4A_7C15, .monotonic) | 1;
}

/// A 16 KiB chunk: a whole-block image (one 4 KiB grain) and a two-tenant
/// pack (two grains) both fit the RAM block.
const CHUNK: u64 = 16 << 10;
const BLOCK: u32 = 8 << 10;

const RamRekeyWorld = struct {
    gpa: Allocator,
    w: mover.RamWorld,
    cfg: transform.VolumeTransform,
    env: std.process.Environ.Map,
    /// The plane's state: current = the next key (B), previous = A once
    /// the window is open.
    crypto: *transform.CryptoState,
    xf: transform.Transform,
    codec: transform.Codec,
    window_open: bool = true,
    closed: u32 = 0,
    quiescent: bool = true,
    /// Test seams for the confirming census: a non-quiescent plane that
    /// becomes quiescent after one deferral; a binding-less allocated
    /// block the world frees after the census saw it once (the late
    /// publish that landed and was swept).
    quiesce_false_once: bool = false,
    late_block: ?BlockKey = null,
    late_seen: bool = false,
    /// The planted plaintexts per binding, for the byte-exact checks.
    expect: std.AutoArrayHashMapUnmanaged(mover.RamWorld.BindKey, []u8) = .empty,
    vt_: mover.Seam.VTable,

    fn init(gpa: Allocator, ka: transform.DerivedKey, kb: transform.DerivedKey) !*RamRekeyWorld {
        const self = try gpa.create(RamRekeyWorld);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .w = mover.RamWorld.init(gpa, CHUNK), .cfg = .{ .compression = .lz4, .encryption = .aes256gcm, .block_size = BLOCK }, .env = std.process.Environ.Map.init(gpa), .crypto = undefined, .xf = undefined, .codec = undefined, .vt_ = undefined };
        try self.w.addVolume(0xA, 16);
        try self.w.addVolume(0xB, 16);
        self.crypto = try transform.CryptoState.init(gpa, testing.io, .aes256gcm, kb, ka, 0, 1);
        self.xf = try transform.Transform.init(&self.env, self.cfg, self.crypto);
        self.codec = try transform.Codec.init(gpa, &self.xf);
        // The mover's seam: the RAM world's table with the rewrite bound.
        self.vt_ = mover.RamWorld.seam(&self.w).vtable.*;
        self.vt_.rewrite = &rewrite;
        return self;
    }

    fn deinit(self: *RamRekeyWorld) void {
        for (self.expect.values()) |v| self.gpa.free(v);
        self.expect.deinit(self.gpa);
        self.codec.deinit(self.gpa);
        self.crypto.deinit();
        self.env.deinit();
        self.w.deinit();
        self.gpa.destroy(self);
    }

    /// Encode `plain` under key `under` (a state whose CURRENT key is it).
    fn imageUnder(self: *RamRekeyWorld, under: *transform.CryptoState, plain: []const u8, out: []u8) ![]const u8 {
        const xf = try transform.Transform.init(&self.env, self.cfg, under);
        var c = try transform.Codec.init(self.gpa, &xf);
        defer c.deinit(self.gpa);
        return c.processWrite(plain, out);
    }

    /// Plant a whole-block file block under `under` at `(vol, idx)`.
    fn plantWhole(self: *RamRekeyWorld, under: *transform.CryptoState, vol: u64, idx: u64, fill: u8, plain_len: usize, owners: []const mover.RamWorld.BindKey) !void {
        const plain = try self.gpa.alloc(u8, plain_len);
        errdefer self.gpa.free(plain);
        for (plain, 0..) |*b, i| b.* = fill ^ @as(u8, @truncate(i * 13));
        var scratch: [CHUNK]u8 = undefined;
        const img = try self.imageUnder(under, plain, &scratch);
        _ = try self.w.plant(vol, idx, img, owners);
        for (owners) |o| try self.expect.put(self.gpa, o, try self.gpa.dupe(u8, plain));
        self.gpa.free(plain);
    }

    /// Plant a pack block with tenants (each `(fill, len)` at successive
    /// 4 KiB slots) under `under`; `owners[i]` binds tenant i.
    fn plantPack(self: *RamRekeyWorld, under: *transform.CryptoState, vol: u64, idx: u64, tenants: []const struct { fill: u8, len: usize }, owners: []const mover.RamWorld.BindKey) !void {
        var block: [CHUNK]u8 = @splat(0);
        var off: usize = 0;
        var lens: [8]u32 = undefined;
        var offs: [8]u32 = undefined;
        for (tenants, 0..) |t, i| {
            const plain = try self.gpa.alloc(u8, t.len);
            errdefer self.gpa.free(plain);
            for (plain, 0..) |*b, k| b.* = t.fill ^ @as(u8, @truncate(k * 7));
            var scratch: [CHUNK]u8 = undefined;
            const img = try self.imageUnder(under, plain, &scratch);
            @memcpy(block[off .. off + img.len], img);
            offs[i] = @intCast(off);
            lens[i] = @intCast(img.len);
            off += layout.ceilGrain(img.len);
            try self.expect.put(self.gpa, owners[i], plain);
        }
        _ = try self.w.plant(vol, idx, block[0..off], &.{});
        for (tenants, 0..) |_, i| _ = try self.w.plantTenant(vol, idx, offs[i], lens[i], owners[i]);
    }

    fn classifyAt(self: *RamRekeyWorld, vol_tag: u64, idx: u64, rel_off: u32) Class {
        const b = self.w.block(vol_tag, idx);
        if (b.state == .free or rel_off >= b.len) return .none;
        const h = transform.decodeHeader(self.cfg, b.data[rel_off..b.len]) catch return .none;
        return switch (self.crypto.classify(&h.key_blob)) {
            .current => .current,
            .previous => .previous,
            .foreign => .foreign,
        };
    }

    /// Decode a binding's bytes under whichever key its header names: the
    /// readability law a crash must keep.
    fn readBinding(self: *RamRekeyWorld, k: mover.RamWorld.BindKey, out: []u8) !struct { class: Class, plain: []const u8 } {
        const m = self.w.bindings.get(k) orelse return error.LostBinding;
        const b = self.w.block(m.vol_tag, m.blockIdx(CHUNK));
        if (b.state != .allocated) return error.LostBinding;
        const off: usize = if (m.whole_block) 0 else m.rel_off;
        const end: usize = if (m.whole_block) b.len else @min(b.len, off + layout.ceilGrain(m.len));
        const window = b.data[off..end];
        const h = transform.decodeHeader(self.cfg, window) catch return error.NotAnImage;
        const class: Class = switch (self.crypto.classify(&h.key_blob)) {
            .current => .current,
            .previous => .previous,
            .foreign => return error.ForeignKey,
        };
        const img = try self.gpa.dupe(u8, window);
        defer self.gpa.free(img);
        const plain = try self.codec.processRead(img, out);
        return .{ .class = class, .plain = plain };
    }

    /// Every binding decodes under exactly one of the two keys to its
    /// planted bytes; the allocator agrees with the bindings' population.
    fn checkReadable(self: *RamRekeyWorld, require: ?Class) !void {
        const out = try self.gpa.alloc(u8, transform.scratchLen(BLOCK));
        defer self.gpa.free(out);
        var it = self.expect.iterator();
        while (it.next()) |kv| {
            const r = try self.readBinding(kv.key_ptr.*, out);
            try testing.expectEqualSlices(u8, kv.value_ptr.*, r.plain);
            if (require) |c| try testing.expectEqual(c, r.class);
        }
        try testing.expectEqual(self.expect.count(), self.w.bindings.count());
        for (self.w.vols.keys(), self.w.vols.values()) |tag, blocks| {
            for (blocks, 0..) |b, i| {
                var pop: u32 = 0;
                var bit = self.w.bindings.iterator();
                while (bit.next()) |kv| if (kv.value_ptr.vol_tag == tag and kv.value_ptr.blockIdx(CHUNK) == i) {
                    pop += 1;
                };
                if (b.state == .allocated) try testing.expectEqual(pop, b.refcount) else try testing.expectEqual(@as(u32, 0), pop);
            }
        }
    }

    fn seam(self: *RamRekeyWorld) RekeySeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: RekeySeam.VTable = .{ .mover = &moverFn, .windowKeyId = &windowKeyId, .refs = &refs, .allocatedBlocks = &allocatedBlocks, .classify = &classify, .quiesce = &quiesce, .drainReclaim = &drainReclaim, .close = &close, .chunk = &chunk };

    fn self_(ctx: *anyopaque) *RamRekeyWorld {
        return @ptrCast(@alignCast(ctx));
    }
    /// The mover's RAM table is called with the RAM WORLD as its ctx;
    /// the rewrite hook recovers the rekey world from it (`rekeyWorldOf`).
    fn moverFn(ctx: *anyopaque) mover.Seam {
        const self = self_(ctx);
        return .{ .ctx = &self.w, .vtable = &self.vt_ };
    }
    fn windowKeyId(ctx: *anyopaque) ?[transform.KEY_ID_LEN]u8 {
        const self = self_(ctx);
        return if (self.window_open) self.crypto.keyId() else null;
    }
    fn refs(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]Ref {
        const self = self_(ctx);
        var list: std.ArrayList(Ref) = .empty;
        var it = self.w.bindings.iterator();
        while (it.next()) |kv| try list.append(gpa, .{ .ino = kv.key_ptr.ino, .block_index = kv.key_ptr.block_index, .mapping = kv.value_ptr.* });
        return list.toOwnedSlice(gpa);
    }
    fn allocatedBlocks(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]BlockKey {
        const self = self_(ctx);
        if (self.late_block) |lb| {
            if (self.late_seen) {
                self.w.block(lb.vol_tag, lb.block_idx).* = .{};
                self.late_block = null;
            }
            self.late_seen = true;
        }
        var list: std.ArrayList(BlockKey) = .empty;
        for (self.w.vols.keys(), self.w.vols.values()) |tag, blocks| for (blocks, 0..) |b, i| if (b.state != .free) try list.append(gpa, .{ .vol_tag = tag, .block_idx = i });
        return list.toOwnedSlice(gpa);
    }
    fn classify(ctx: *anyopaque, vol_tag: u64, idx: u64, rel_off: u32) Class {
        return self_(ctx).classifyAt(vol_tag, idx, rel_off);
    }
    fn quiesce(ctx: *anyopaque, _: Io) bool {
        const self = self_(ctx);
        if (self.quiesce_false_once) {
            self.quiesce_false_once = false;
            return false;
        }
        return self.quiescent;
    }
    fn drainReclaim(_: *anyopaque) void {}
    fn close(ctx: *anyopaque) error{Io}!void {
        const self = self_(ctx);
        self.window_open = false;
        self.closed += 1;
        self.crypto.endRotation();
    }
    fn chunk(_: *anyopaque) u64 {
        return CHUNK;
    }
    fn rewrite(ctx: *anyopaque, task: Task, image: []u8, len: usize) mover.SeamError!?usize {
        const self = rekeyWorldOf(@ptrCast(@alignCast(ctx)));
        const w = Windows.of(task) catch return error.Io;
        return rewriteImage(self.gpa, &self.codec, self.crypto, w, image, len) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.Io,
        };
    }
};

/// The RAM world embedded in its rekey world (the mover's ctx → the hook's).
fn rekeyWorldOf(w: *mover.RamWorld) *RamRekeyWorld {
    return @alignCast(@fieldParentPtr("w", w));
}

const Rig = struct {
    world: *RamRekeyWorld,
    store: fabric_mod.RamStore,
    fabric: Fabric,
    body: RekeyBody,

    fn init(gpa: Allocator, ka: transform.DerivedKey, kb: transform.DerivedKey) !*Rig {
        const r = try gpa.create(Rig);
        errdefer gpa.destroy(r);
        r.* = .{ .world = try RamRekeyWorld.init(gpa, ka, kb), .store = fabric_mod.RamStore.init(gpa), .fabric = undefined, .body = undefined };
        r.fabric = Fabric.init(gpa, r.store.store(), .{ .identity = .{ .node_token = 1, .mount_slot = 1 }, .pid = 1, .random = &rnd });
        r.body = .{ .gpa = gpa, .seam = r.world.seam() };
        r.fabric.registerBody(.rekey, r.body.body());
        return r;
    }

    fn deinit(self: *Rig, gpa: Allocator) void {
        self.fabric.deinit();
        self.store.deinit();
        self.world.deinit();
        gpa.destroy(self);
    }

    /// The planted set: three whole-block files under A (one shared by a
    /// clone), one already under B, a two-tenant pack under A, a pack
    /// whose first tenant is dead (zeroed slot) and second live under A.
    fn plantAll(self: *Rig, a: *transform.CryptoState) !void {
        const w = self.world;
        try w.plantWhole(a, 0xA, 1, 0x11, 5000, &.{.{ .ino = 10, .block_index = 0 }});
        try w.plantWhole(a, 0xA, 2, 0x22, BLOCK, &.{ .{ .ino = 11, .block_index = 0 }, .{ .ino = 12, .block_index = 0 } });
        try w.plantWhole(a, 0xB, 3, 0x33, 777, &.{.{ .ino = 13, .block_index = 2 }});
        try w.plantWhole(w.crypto, 0xA, 4, 0x44, 3000, &.{.{ .ino = 14, .block_index = 0 }});
        try w.plantPack(a, 0xA, 5, &.{ .{ .fill = 0x55, .len = 100 }, .{ .fill = 0x66, .len = 2000 } }, &.{ .{ .ino = 15, .block_index = 0 }, .{ .ino = 16, .block_index = 0 } });
        try w.plantPack(a, 0xB, 6, &.{ .{ .fill = 0x77, .len = 50 }, .{ .fill = 0x88, .len = 900 } }, &.{ .{ .ino = 17, .block_index = 0 }, .{ .ino = 18, .block_index = 0 } });
        // Tenant 0 of pack 6 dies: its binding gone, its slot stays as
        // written (an old-key image nobody references).
        _ = w.w.bindings.swapRemove(.{ .ino = 17, .block_index = 0 });
        w.w.block(0xB, 6).refcount -= 1;
        if (w.expect.fetchSwapRemove(.{ .ino = 17, .block_index = 0 })) |kv| w.gpa.free(kv.value);
    }
};

fn twoKeys() struct { a: transform.DerivedKey, b: transform.DerivedKey } {
    return .{ .a = transform.deriveVolumeKey("key-A-material-key-A-material-key-A-material", &(@as([32]u8, @splat(0xA1)))), .b = transform.deriveVolumeKey("key-B-material-key-B-material-key-B-material", &(@as([32]u8, @splat(0xB2)))) };
}

test "the rotation sweep over a RAM plane with real framed images: whole blocks, a clone-shared block, a two-tenant pack and a pack with a dead first tenant all re-encrypt onto the next key through the ONE mover; a block already under it is left alone; the window closes (the primary flips, the old key is forgotten) only after the confirming census; every byte exact under the new key" {
    const gpa = testing.allocator;
    const io = testing.io;
    var keys = twoKeys();
    defer keys.a.zeroize();
    defer keys.b.zeroize();
    const a_state = try transform.CryptoState.init(gpa, io, .aes256gcm, keys.a, null, 0, 1);
    defer a_state.deinit();
    const r = try Rig.init(gpa, keys.a, keys.b);
    defer r.deinit(gpa);
    try r.plantAll(a_state);
    try r.world.checkReadable(null);
    // Before: the A-blocks read `previous`, block 4 `current`.
    try testing.expectEqual(Class.previous, r.world.classifyAt(0xA, 1, 0));
    try testing.expectEqual(Class.current, r.world.classifyAt(0xA, 4, 0));
    try testing.expectEqual(Class.previous, r.world.classifyAt(0xA, 5, 4096));
    const used0 = r.world.w.usedBlocks(0xA) + r.world.w.usedBlocks(0xB);
    const rewritten0 = stats.rekey_blocks_rewritten.load();
    const closed0 = stats.rekey_windows_closed.load();
    const id = try r.fabric.submit(io, .{ .job_type = .rekey, .arg0 = std.mem.readInt(u64, &keys.b.key_id, .little) });
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    try testing.expectEqual(records.JobState.completed, r.fabric.snapshot(io, id).?.state);
    // Five blocks were under A (1, 2, 3, 5, 6); block 4 was not touched.
    try testing.expectEqual(rewritten0 + 5, stats.rekey_blocks_rewritten.load());
    try testing.expectEqual(closed0 + 1, stats.rekey_windows_closed.load());
    try testing.expectEqual(@as(u32, 1), r.world.closed);
    try testing.expect(!r.world.crypto.rotating());
    try testing.expectEqual(@as(u64, 0), stats.rekey_window_open.load());
    // Every binding reads under the NEW key and byte-exact; the block
    // count is unchanged (a move frees its source); the clone share
    // still sits on ONE block; the dead tenant's old-key bytes did not
    // travel (the new pack's first slot is zeros → `none`).
    try r.world.checkReadable(.current);
    try testing.expectEqual(used0, r.world.w.usedBlocks(0xA) + r.world.w.usedBlocks(0xB));
    const m11 = r.world.w.bindings.get(.{ .ino = 11, .block_index = 0 }).?;
    const m12 = r.world.w.bindings.get(.{ .ino = 12, .block_index = 0 }).?;
    try testing.expect(m11.sameRef(m12, CHUNK));
    const m18 = r.world.w.bindings.get(.{ .ino = 18, .block_index = 0 }).?;
    try testing.expectEqual(Class.none, r.world.classifyAt(m18.vol_tag, m18.blockIdx(CHUNK), 0));
    try testing.expectEqual(Class.current, r.world.classifyAt(m18.vol_tag, m18.blockIdx(CHUNK), m18.rel_off));
    // A second submission with the window closed completes at once.
    const id2 = try r.fabric.submit(io, .{ .job_type = .rekey });
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    try testing.expectEqual(records.JobState.completed, r.fabric.snapshot(io, id2).?.state);
    try testing.expectEqual(@as(u32, 1), r.world.closed);
}

test "the kill matrix: the mover killed at EVERY step of the first block's rewrite → after the remount every binding still decodes, each under exactly one of the two keys, the allocator reconciled to the bindings; the resumed sweep converges and closes the window" {
    const gpa = testing.allocator;
    const io = testing.io;
    var keys = twoKeys();
    defer keys.a.zeroize();
    defer keys.b.zeroize();
    const a_state = try transform.CryptoState.init(gpa, io, .aes256gcm, keys.a, null, 0, 1);
    defer a_state.deinit();
    inline for (@typeInfo(mover.KillPoint).@"enum".field_names) |name| {
        const at = @field(mover.KillPoint, name);
        const r = try Rig.init(gpa, keys.a, keys.b);
        defer r.deinit(gpa);
        try r.plantAll(a_state);
        // The first task the sweep runs is the clone-shared block (the
        // census orders by (vol, idx): 0xA/1 is first; make it the shared
        // one so mid_publish bites): kill the mover at `at` on it.
        r.world.w.kill_at = at;
        if (at == .after_seal) try r.world.w.open_packs.put(gpa, (@as(u128, 0xA) << 64) | 1, {});
        _ = try r.fabric.submit(io, .{ .job_type = .rekey });
        // A kill is the fabric's `io` failure (the daemon died) — one loud
        // line per kill point.
        log.testing.expectErrorLogs(1);
        const st = r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?);
        try log.testing.expectAllFired();
        try testing.expectEqual(records.JobState.failed, st);
        try testing.expect(r.world.w.killed);
        if (at == .after_seal) try testing.expect(r.world.w.open_packs.remove((@as(u128, 0xA) << 64) | 1));
        // "Remount": RAM state gone, the allocator re-seeded from the
        // bindings; the window is still open (the config never flipped).
        r.world.w.remount();
        try testing.expect(r.world.window_open);
        try r.world.checkReadable(null);
        // The resumed sweep (a fresh record adopted) converges.
        _ = try r.fabric.submit(io, .{ .job_type = .rekey });
        try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
        try r.world.checkReadable(.current);
        try testing.expectEqual(@as(u32, 1), r.world.closed);
    }
}

test "the confirming census: a binding-less allocated block still under the previous key (an image DMA'd before its publish) defers the close until it is published-and-swept or freed; a plane that is not quiescent defers too; each deferral counted; the window closes once after" {
    const gpa = testing.allocator;
    const io = testing.io;
    var keys = twoKeys();
    defer keys.a.zeroize();
    defer keys.b.zeroize();
    const a_state = try transform.CryptoState.init(gpa, io, .aes256gcm, keys.a, null, 0, 1);
    defer a_state.deinit();
    const r = try Rig.init(gpa, keys.a, keys.b);
    defer r.deinit(gpa);
    try r.world.plantWhole(a_state, 0xA, 1, 0x11, 1000, &.{.{ .ino = 10, .block_index = 0 }});
    // An allocated block nobody references yet, under A: the late publish
    // (the world frees it after the census saw it once).
    try r.world.plantWhole(a_state, 0xA, 7, 0x99, 1000, &.{});
    r.world.w.block(0xA, 7).refcount = 1;
    r.world.late_block = .{ .vol_tag = 0xA, .block_idx = 7 };
    // And a plane that is not quiescent at the first confirming pass.
    r.world.quiesce_false_once = true;
    const deferrals0 = stats.rekey_confirm_deferrals.load();
    _ = try r.fabric.submit(io, .{ .job_type = .rekey });
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    // One deferral for the quiescence, one for the late block.
    try testing.expectEqual(deferrals0 + 2, stats.rekey_confirm_deferrals.load());
    try testing.expectEqual(@as(u32, 1), r.world.closed);
    try testing.expect(!r.world.window_open);
    try r.world.checkReadable(.current);
    try testing.expectEqual(mover.RamWorld.BlockState.free, r.world.w.block(0xA, 7).state);
}

test "Windows: a whole-block task is one window at 0; tenants group by rel_off (a clone share once); two lengths at one offset conflict; rewriteImage leaves an all-current image untouched (null) and refuses a foreign wrap" {
    const gpa = testing.allocator;
    var keys = twoKeys();
    defer keys.a.zeroize();
    defer keys.b.zeroize();
    const w = try RamRekeyWorld.init(gpa, keys.a, keys.b);
    defer w.deinit();
    const refs = [_]Ref{
        .{ .ino = 1, .block_index = 0, .mapping = Mapping.tenant(0xA, 0, 7, 0, 100) },
        .{ .ino = 2, .block_index = 0, .mapping = Mapping.tenant(0xA, 0, 7, 0, 100) },
        .{ .ino = 3, .block_index = 0, .mapping = Mapping.tenant(0xA, 0, 7, 4096, 200) },
    };
    const t: Task = .{ .vol_tag = 0xA, .block_idx = 0, .offset = 0, .incarnation = 7, .refs = @constCast(&refs) };
    const ws = try Windows.of(t);
    try testing.expect(!ws.whole and ws.n == 2 and ws.offs[0] == 0 and ws.offs[1] == 4096 and ws.lens[1] == 200);
    const bad = [_]Ref{ refs[0], .{ .ino = 9, .block_index = 0, .mapping = Mapping.tenant(0xA, 0, 7, 0, 101) } };
    try testing.expectError(error.LenConflict, Windows.of(.{ .vol_tag = 0xA, .block_idx = 0, .offset = 0, .incarnation = 7, .refs = @constCast(&bad) }));
    const whole = [_]Ref{.{ .ino = 1, .block_index = 0, .mapping = Mapping.wholeBlock(0xA, 0, 7) }};
    try testing.expect((try Windows.of(.{ .vol_tag = 0xA, .block_idx = 0, .offset = 0, .incarnation = 7, .refs = @constCast(&whole) })).whole);
    // An image already under the current key: nothing to rewrite.
    var scratch: [CHUNK]u8 = undefined;
    const plain: [280]u8 = @splat(0x5A);
    const img = try w.imageUnder(w.crypto, &plain, &scratch);
    var buf: [CHUNK]u8 = @splat(0);
    @memcpy(buf[0..img.len], img);
    var ww: Windows = .{ .whole = true };
    try testing.expectEqual(@as(?usize, null), try rewriteImage(gpa, &w.codec, w.crypto, ww, &buf, img.len));
    // A foreign wrap refuses.
    var kc = transform.deriveVolumeKey("key-C-material-key-C-material-key-C-material", &(@as([32]u8, @splat(0xC3))));
    defer kc.zeroize();
    const c_state = try transform.CryptoState.init(gpa, testing.io, .aes256gcm, kc, null, 0, 1);
    defer c_state.deinit();
    const img_c = try w.imageUnder(c_state, &plain, &scratch);
    @memcpy(buf[0..img_c.len], img_c);
    ww = .{ .whole = true };
    try testing.expectError(error.Corrupt, rewriteImage(gpa, &w.codec, w.crypto, ww, &buf, img_c.len));
}
