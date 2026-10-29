//! The warm ladder — the TRY-ONLY probe the transport calls on the queue
//! thread (charter §4 READ dispatch law arm (a): "serve inline if the
//! filesystem's try-only warm ladder answers (per-inode try-lock → active
//! buffer → staging ring → hot tier → read-lane hold → NVMe read cache)";
//! `docs/porting/data_read_path.md` §10). PORTABLE over `read_path.ReadPath`.
//!
//! What the term kills: the hop reap thread → inbound queue → session
//! dispatch task → lane spawn → first poll measured `queue_wait` 69 µs +
//! `dispatch_lag` 89 µs = 158 µs of a 439 µs 4 KiB random read (36 %), with
//! the device at 40 µs; unloaded 6.7 µs hop vs 53 ns inline serve.
//!
//! LAW: nothing in rungs 0–5 awaits, spins, allocates per op or touches a
//! device. A contended per-inode guard is a DEMOTE, never a wait; every
//! miss is a Demote; the handler lane then owns the request (and every
//! count — the ladder is silent on Demote). `Demote` conditions exactly
//! per §10: virtual/zero size, device-true O_DIRECT (a tier serve would
//! turn a device-true row warm — measurement fraud), retained extents, a
//! request wider than the window (FUSE-4e: never a short read below EOF),
//! a contended guard, no size authority, a multi-block request, an open
//! device overlay, W2 runs, an active buffer that does not contain the
//! window, a hole/NeedsAuthority key, every tier missed.
//!
//! Binding currency on rung 4 is structural: the caller holds the inode's
//! shared guard and the key came from the CURRENT map; this daemon's
//! writers update that map synchronously under the same guard.
//!
//! The zc legs: `directLeg` computes the PURE eligibility of the direct
//! device leg (cold ∧ passthrough ∧ 4 KiB-aligned ∧ single block ∧ not
//! device-true ∧ resolvable) and arms it with the post-DMA RECHECK hook the
//! transport runs at the bridge CQE (incarnation still ∧ binding still —
//! charter §6.3's validation AFTER the bytes are in hand; a failed recheck
//! withdraws the prefilled reply); `probeRead` answers a hot-tier / hold
//! hit that lies on the memfd fill pool as an `fd_source` (the pin released
//! by the CQE hook) when the session serves zc; the hold-serve ceremony
//! (`holdServeAdmission`) is DEFERRED to a lane through `ceremony` — the
//! queue thread never publishes into the mmap tier.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const layout = @import("layout.zig");
const rsc = @import("read_size_class.zig");
const read_path = @import("read_path.zig");
const read_cache = @import("read_cache.zig");
const lease_epoch = @import("lease_epoch.zig");
const hot_tier = @import("hot_tier.zig");
const fsv = @import("../fuse/transport/filesystem_vtable.zig");
const codec = @import("../fuse/codec.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");

pub const BlockKey = rsc.BlockKey;

pub const stats = struct {
    /// `fast_dispatch_serves`: READs the ladder answered inline.
    pub var fast_dispatch_serves: counters.ShardedCounter = .{};
    /// `fast_dispatch_demotes`: READs the ladder declined (by reason below).
    pub var fast_dispatch_demotes: counters.ShardedCounter = .{};
    pub var fast_dispatch_demote_pre_screen: counters.ShardedCounter = .{};
    pub var fast_dispatch_demote_contended: counters.ShardedCounter = .{};
    pub var fast_dispatch_demote_write_side: counters.ShardedCounter = .{};
    /// Direct device legs declined because the write side holds custody
    /// of the block (the lane composes the newer bytes).
    pub var direct_leg_custody_declines: counters.ShardedCounter = .{};
    pub var fast_dispatch_demote_miss: counters.ShardedCounter = .{};
    pub var fast_dispatch_eof: counters.ShardedCounter = .{};
    pub var ipc_hold_probe_serves: counters.ShardedCounter = .{};
    pub var ipc_hold_probe_misses: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_inline: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_staged: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_hot: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_hold: counters.ShardedCounter = .{};
    pub var fast_dispatch_serve_cache: counters.ShardedCounter = .{};
    /// READs the read cache's grains served inline (`read_cache.zig`).
    pub var fast_dispatch_serve_grains: counters.ShardedCounter = .{};
    /// Direct legs whose landed window the recheck accepted and the read
    /// cache deposited (the IPC direct-drive's arena at its CQE).
    pub var direct_leg_deposits: counters.ShardedCounter = .{};
    /// Direct legs declined so the lane's ranged read fills the read cache
    /// (`SQUEEZEFS_READ_CACHE_FILL=lane`).
    pub var direct_leg_cache_demotes: counters.ShardedCounter = .{};
    /// Direct legs the predicate admitted (the transport bridges them).
    pub var direct_leg_offers: counters.ShardedCounter = .{};
    /// Direct legs whose post-DMA recheck failed (the reply withdrawn).
    pub var direct_leg_recheck_failures: counters.ShardedCounter = .{};
    /// Probe serves answered as an fd-source (zero daemon passes).
    pub var fast_dispatch_fd_source_serves: counters.ShardedCounter = .{};
    /// Hold-serve ceremonies handed to a lane from the probe path.
    pub var fast_dispatch_hold_ceremonies: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"fast_dispatch_serves\":{d},\"fast_dispatch_demotes\":{d},\"fast_dispatch_demote_pre_screen\":{d},\"fast_dispatch_demote_contended\":{d},\"fast_dispatch_demote_write_side\":{d},\"fast_dispatch_demote_miss\":{d},\"fast_dispatch_eof\":{d},\"ipc_hold_probe_serves\":{d},\"ipc_hold_probe_misses\":{d},\"fast_dispatch_serve_inline\":{d},\"fast_dispatch_serve_staged\":{d},\"fast_dispatch_serve_hot\":{d},\"fast_dispatch_serve_hold\":{d},\"fast_dispatch_serve_cache\":{d},\"direct_leg_offers\":{d}", .{
        stats.fast_dispatch_serves.load(),
        stats.fast_dispatch_demotes.load(),
        stats.fast_dispatch_demote_pre_screen.load(),
        stats.fast_dispatch_demote_contended.load(),
        stats.fast_dispatch_demote_write_side.load(),
        stats.fast_dispatch_demote_miss.load(),
        stats.fast_dispatch_eof.load(),
        stats.ipc_hold_probe_serves.load(),
        stats.ipc_hold_probe_misses.load(),
        stats.fast_dispatch_serve_inline.load(),
        stats.fast_dispatch_serve_staged.load(),
        stats.fast_dispatch_serve_hot.load(),
        stats.fast_dispatch_serve_hold.load(),
        stats.fast_dispatch_serve_cache.load(),
        stats.direct_leg_offers.load(),
    });
    try w.print(",\"direct_leg_recheck_failures\":{d},\"direct_leg_custody_declines\":{d},\"fast_dispatch_fd_source_serves\":{d},\"fast_dispatch_hold_ceremonies\":{d},\"fast_dispatch_serve_grains\":{d},\"direct_leg_deposits\":{d},\"direct_leg_cache_demotes\":{d}}}", .{ stats.direct_leg_recheck_failures.load(), stats.direct_leg_custody_declines.load(), stats.fast_dispatch_fd_source_serves.load(), stats.fast_dispatch_hold_ceremonies.load(), stats.fast_dispatch_serve_grains.load(), stats.direct_leg_deposits.load(), stats.direct_leg_cache_demotes.load() });
}

/// Why the ladder declined (the handler owns the request from here).
pub const Demote = enum(u8) {
    pre_screen,
    contended,
    write_side,
    miss,
};

/// Which rung served (the ledger's warm split).
pub const Arm = enum(u8) { inline_, staged, hot, hold, grains, cache };

pub const Verdict = union(enum) {
    served: struct { n: usize, arm: Arm, view: ?[]const u8 = null, pinned: ?Pinned = null },
    eof,
    demote: Demote,
};

/// The pinned fill-pool view behind an fd-source serve (released by the
/// bridge-completion hook).
pub const Pinned = union(enum) { hot: hot_tier.Hit, hold: @import("read_hold.zig").Served };

/// The pure eligibility of the direct device leg: cold is the CALLER's
/// premise (the probe declined first).
pub fn directLegEligible(passthrough: bool, device_true: bool, offset: u64, size: u32, block_size: u64) bool {
    if (!passthrough or device_true or size == 0) return false;
    if (!derive.isAligned(offset, rsc.LBA_GRAIN) or !derive.isAligned(size, rsc.LBA_GRAIN)) return false;
    return offset / block_size == (offset + size - 1) / block_size;
}

/// The probe's resolution of a cold striped single-block request, handed to
/// the direct leg the transport asks next for the SAME request on the SAME
/// thread (`deliverParsed`: `probe_read` declines, then `direct_leg`, back
/// to back): the leg's pre-DMA reads of the file and the binding are the
/// probe's, microseconds old — no fetch between them, and the post-DMA
/// recheck re-reads the binding as before (charter §6.3's validation after
/// the bytes are in hand). Re-reading both cost two layout-record lookups
/// and an inode lookup per cold read (`docs/evidence/rung-10-dev-perf.md`
/// §3.4). Taken by the leg, cleared by every probe; a request that does not
/// match resolves fresh.
const Resolved = struct {
    ladder: *const WarmLadder,
    ino: u64,
    offset: u64,
    size: u32,
    info: read_path.FileInfo,
    binding: layout.Mapping,
};
threadlocal var resolved: ?Resolved = null;

pub const WarmLadder = struct {
    rp: *read_path.ReadPath,
    guards: *op_guard.Table,
    io: Io,
    /// The direct device leg is OFFERED only with the recheck hook the
    /// transport runs at the bridge completion (`fsv.DirectLeg.hook`); the
    /// mount arms it on a zero-copy session.
    direct_leg_armed: bool = false,
    /// The probe answers memfd-pool views as `fd_source` (a zero-copy
    /// session with `SQUEEZEFS_READ_ZC_SERVE`); off = the copy arm.
    fd_source_armed: bool = false,
    /// The deferred hold-serve ceremony: the mount spawns
    /// `ReadPath.holdCeremony` on a lane; null = the ceremony waits for the
    /// next lane-side serve.
    ceremony: ?Ceremony = null,

    pub const Ceremony = struct {
        ctx: ?*anyopaque,
        spawn: *const fn (ctx: ?*anyopaque, key: BlockKey, class: read_path.FillClass) void,
    };

    /// The probe: serve `[offset, offset + size)` of `ino` INTO `window`.
    pub fn probe(self: *WarmLadder, ino: u64, offset: u64, size: u32, odirect: bool, window: []u8) Verdict {
        resolved = null;
        const rp = self.rp;
        // 0. Pre-screen.
        if (size == 0) return self.demote(.pre_screen);
        if (odirect and rp.cfg.direct_device_true) return self.demote(.pre_screen);
        if (rp.write_side.has_retained_extents(rp.write_side.ctx, ino)) return self.demote(.pre_screen);
        if (size > window.len) return self.demote(.pre_screen);
        // 1. The per-inode TRY-lock: a contended stripe is a Demote.
        const guard = self.guards.tryAcquire(self.io, ino, .shared) orelse return self.demote(.contended);
        defer guard.release(self.io);
        // 2. Size authority (and, from the same layout read, the binding of
        //    the request's first block — under the guard writers take).
        var pre: ??layout.Mapping = null;
        const info = if (rp.meta.file_at) |fa| blk: {
            const at = fa(rp.meta.ctx, ino, offset / rp.cfg.block_size) orelse return self.demote(.miss);
            pre = at.binding;
            break :blk at.info;
        } else rp.meta.file(rp.meta.ctx, ino) orelse return self.demote(.miss);
        if (offset >= info.size) {
            stats.fast_dispatch_eof.inc();
            return .eof;
        }
        const want: usize = @intCast(@min(size, info.size - offset));
        const out = window[0..want];
        const v = self.serve(ino, info, pre, offset, out, odirect) catch |e| return self.demote(switch (e) {
            error.Miss => .miss,
            error.WriteSide => .write_side,
        });
        stats.fast_dispatch_serves.inc();
        read_path.stats.read_copy_dest_bytes.add(v.n);
        read_path.stats.read_user_bytes.add(v.n);
        switch (v.arm) {
            .inline_ => stats.fast_dispatch_serve_inline.inc(),
            .staged => stats.fast_dispatch_serve_staged.inc(),
            .hot => {
                stats.fast_dispatch_serve_hot.inc();
                read_path.stats.read_copy_warm_serve_bytes.add(v.n);
                read_path.stats.read_copy_hot_serve_bytes.add(v.n);
            },
            .hold => {
                stats.fast_dispatch_serve_hold.inc();
                read_path.stats.read_copy_warm_serve_bytes.add(v.n);
                read_path.stats.read_copy_hold_serve_bytes.add(v.n);
            },
            .grains => {
                stats.fast_dispatch_serve_grains.inc();
                read_path.stats.read_copy_warm_serve_bytes.add(v.n);
                read_path.stats.read_copy_grain_serve_bytes.add(v.n);
            },
            .cache => {
                stats.fast_dispatch_serve_cache.inc();
                read_path.stats.read_copy_warm_serve_bytes.add(v.n);
                read_path.stats.read_copy_cache_serve_bytes.add(v.n);
            },
        }
        if (odirect) read_path.stats.read_odirect_requests.inc();
        // 5. The ring feed: silent consumption advances the consume edge.
        rp.ringReadLaneTouch(ino, offset, v.n, info.size);
        return .{ .served = .{ .n = v.n, .arm = v.arm, .view = v.view, .pinned = v.pinned } };
    }

    /// Release the pin an fd-source serve took (the CQE hook's body; also
    /// the probe's own exit when the transport cannot bridge).
    pub fn unpinServed(self: *WarmLadder, pinned: Pinned) void {
        switch (pinned) {
            .hot => |hit| self.rp.hot.unpin(hit),
            .hold => |sv| self.rp.hold.unpin(sv),
        }
    }

    fn demote(_: *WarmLadder, why: Demote) Verdict {
        stats.fast_dispatch_demotes.inc();
        switch (why) {
            .pre_screen => stats.fast_dispatch_demote_pre_screen.inc(),
            .contended => stats.fast_dispatch_demote_contended.inc(),
            .write_side => stats.fast_dispatch_demote_write_side.inc(),
            .miss => stats.fast_dispatch_demote_miss.inc(),
        }
        return .{ .demote = why };
    }

    const Served = struct {
        n: usize,
        arm: Arm,
        /// A view on the fill pool the probe may answer as an fd-source
        /// instead of copying (the copy into `out` was NOT made).
        view: ?[]const u8 = null,
        pinned: ?Pinned = null,
    };

    /// Rungs 3–4 by layout; `Miss` = nothing resident, `WriteSide` = the
    /// write side holds custody the probe may not wait for.
    /// `pre` = the first block's binding the probe resolved with the file
    /// facts (null = not resolved: read it here).
    fn serve(self: *WarmLadder, ino: u64, info: read_path.FileInfo, pre: ??layout.Mapping, offset: u64, out: []u8, odirect: bool) error{ Miss, WriteSide }!Served {
        const rp = self.rp;
        const bs = rp.cfg.block_size;
        switch (info.kind) {
            .inline_ => {
                const n = rp.meta.inline_data(rp.meta.ctx, ino, offset, out);
                @memset(out[n..], 0);
                return .{ .n = out.len, .arm = .inline_ };
            },
            .staged => {
                switch (rp.write_side.staged_payload(rp.write_side.ctx, ino, info.file_id, offset, out.len, out)) {
                    .served => |n| {
                        read_path.stats.cache_hits.inc();
                        return .{ .n = n, .arm = .staged };
                    },
                    else => return error.Miss,
                }
            },
            // A packed tenant is a device window: the lane's.
            .packed_ => return error.Miss,
            .striped => {
                const b = offset / bs;
                const end = offset + out.len;
                if ((end - 1) / bs != b) return error.Miss;
                const rel_s = offset - b * bs;
                const rel_e = end - b * bs;
                const ws = rp.write_side;
                if (ws.open_overlay(ws.ctx, ino, b)) return error.Miss;
                if (ws.extent_runs(ws.ctx, ino, b, rel_s, rel_e, &.{}) != .none) return error.Miss;
                // The write side's custody station, TRY-ONLY: a held block
                // lock or an overlay mid-store is a Demote (the lane asks
                // again blocking) — never a park on the reap thread.
                switch (ws.staged_block(ws.ctx, ino, b, rel_s, rel_e, out, true)) {
                    .served => |n| {
                        @memset(out[n..], 0);
                        return .{ .n = out.len, .arm = .staged };
                    },
                    .busy => return error.WriteSide,
                    .none => {},
                }
                if (rp.cfg.direct_device_true and odirect) return error.Miss;
                const key = (if (pre) |p| p else rp.meta.binding(rp.meta.ctx, ino, b)) orelse return error.Miss;
                const k = BlockKey.fromMapping(key);
                // The payback credit is the device bytes the hit avoided
                // (the ranged window), as on the lane (`readSingleBlock`).
                if (rp.hot.getServing(k, read_path.Window.of(rel_s, rel_e, rp.cfg.block_size).len())) |hit| {
                    const view = rp.pool.bytes(hit.slot)[@intCast(rel_s)..@intCast(rel_e)];
                    hot_tier.stats.hot_block_hits.inc();
                    read_path.stats.cache_hits.inc();
                    if (rp.cfg.read_lane) rp.hold.credit(k, out.len);
                    if (odirect) read_path.stats.read_odirect_tier_serves.inc();
                    if (self.fd_source_armed and rp.fdSource(view) != null) return .{ .n = out.len, .arm = .hot, .view = view, .pinned = .{ .hot = hit } };
                    defer rp.hot.unpin(hit);
                    @memcpy(out, view);
                    return .{ .n = out.len, .arm = .hot };
                }
                if (rp.cfg.read_lane) {
                    if (rp.hold.serve(k, 0)) |sv| {
                        const view = rp.pool.bytes(sv.slot)[@intCast(rel_s)..@intCast(rel_e)];
                        read_path.stats.cache_hits.inc();
                        read_path.stats.read_lane_serves.inc();
                        read_path.stats.read_lane_serve_bytes.add(out.len);
                        stats.ipc_hold_probe_serves.inc();
                        rp.hold.credit(k, out.len);
                        if (odirect) read_path.stats.read_odirect_tier_serves.inc();
                        // The ceremony (the deposit's tier admission) runs on a
                        // lane — never a publish from the queue thread.
                        if (sv.ledger_visible) if (self.ceremony) |c| {
                            stats.fast_dispatch_hold_ceremonies.inc();
                            c.spawn(c.ctx, k, if (odirect) .demand else .demand_stream);
                        };
                        if (self.fd_source_armed and rp.fdSource(view) != null) return .{ .n = out.len, .arm = .hold, .view = view, .pinned = .{ .hold = sv } };
                        defer rp.hold.unpin(sv);
                        @memcpy(out, view);
                        return .{ .n = out.len, .arm = .hold };
                    }
                    stats.ipc_hold_probe_misses.inc();
                }
                // The read cache's grains (charter §6.3): a RAM window
                // serve, one lock, before the disk tier. The binding is
                // current structurally here (the guard, the live map).
                if (rp.grains.get(k, rel_s, rel_e, out)) |n| {
                    read_path.stats.cache_hits.inc();
                    if (rp.cfg.read_lane) rp.hold.credit(k, out.len);
                    if (odirect) read_path.stats.read_odirect_tier_serves.inc();
                    return .{ .n = n, .arm = .grains };
                }
                if (rp.nvme.getRange(k, rel_s, rel_e, out)) |n| {
                    @memset(out[n..], 0);
                    read_path.stats.cache_hits.inc();
                    if (rp.cfg.read_lane) rp.hold.credit(k, out.len);
                    if (odirect) read_path.stats.read_odirect_tier_serves.inc();
                    return .{ .n = out.len, .arm = .cache };
                }
                if (out.len <= std.math.maxInt(u32)) resolved = .{ .ladder = self, .ino = ino, .offset = offset, .size = @intCast(out.len), .info = info, .binding = key };
                return error.Miss;
            },
        }
    }

    /// The direct device leg after the probe declined: `{fd, off, len}`
    /// when the WHOLE reply can DMA from the device into the app's pages
    /// (cold = not in any tier, no flight open), else null. The transport's
    /// face: offered only on a zero-copy session.
    pub fn directLeg(self: *WarmLadder, ino: u64, offset: u64, size: u32, odirect: bool) ?fsv.DirectLeg {
        if (!self.direct_leg_armed) return null;
        // The leg lands in the app's pages this process never sees: under
        // `SQUEEZEFS_READ_CACHE_FILL=lane` a grain-eligible cold read takes
        // the lane instead (a bounce, one copy, the deposit) — measured a
        // regression on the RAM NVMe, so `leg` ships. Device-true reads
        // keep the leg: they are never deposited.
        const rp = self.rp;
        if (rp.grains.enabled() and rp.cfg.read_cache_fill == .lane and size <= read_cache.WINDOW_MAX and !(rp.cfg.direct_device_true and odirect)) {
            stats.direct_leg_cache_demotes.inc();
            return null;
        }
        return self.resolveDirectLeg(ino, offset, size, odirect);
    }

    /// The leg's resolution without the transport's arming: the IPC host's
    /// direct-drive READ (`ipc/host/direct.zig`) DMAs the same leg into the
    /// session arena on its own ring, whatever the FUSE session's buffer
    /// mode — its arming is `SQUEEZEFS_IL_DIRECT_READ`.
    pub fn resolveDirectLeg(self: *WarmLadder, ino: u64, offset: u64, size: u32, odirect: bool) ?fsv.DirectLeg {
        const rp = self.rp;
        const device_true = rp.cfg.direct_device_true and odirect;
        if (!directLegEligible(rp.cfg.passthrough, device_true, offset, size, rp.cfg.block_size)) return null;
        const memo: ?Resolved = if (resolved) |r| (if (r.ladder == self and r.ino == ino and r.offset == offset and r.size == size) r else null) else null;
        resolved = null;
        const info = if (memo) |r| r.info else rp.meta.file(rp.meta.ctx, ino) orelse return null;
        if (offset + size > info.size or info.kind != .striped) return null;
        const b = offset / rp.cfg.block_size;
        // The write side's custody outranks the device (charter §6.3: a
        // dirty local authority outranks durable state): a parked extent
        // or buffer, an overlay record or an epoch shadow on the block
        // holds bytes the mapped key does not — the lane composes them.
        if (rp.write_side.has_custody(rp.write_side.ctx, ino, b)) {
            stats.direct_leg_custody_declines.inc();
            return null;
        }
        const m = if (memo) |r| r.binding else rp.meta.binding(rp.meta.ctx, ino, b) orelse return null;
        const k = BlockKey.fromMapping(m);
        if (rp.hot.contains(k) or rp.hold.contains(k) or rp.nvme.has(k) or rp.flights.inFlight(self.io, k)) return null;
        const fd = rp.vols.device_fd(rp.vols.ctx, k.vol_tag) orelse return null;
        // The incarnation snapshot BEFORE the DMA; the hook re-reads it and
        // the binding AFTER (a dead-lifetime key never DMAs at all).
        const before = rp.incarnationSnapshot(k) orelse return null;
        stats.direct_leg_offers.inc();
        // The token: the block index and the block-relative offset share a
        // word (both < 2³²: the block-map width, the block size); the
        // lease epoch is read BEFORE the DMA, as the ranged read's is.
        const rel = offset - b * rp.cfg.block_size;
        return .{ .fd = fd, .off = k.offset + rel, .len = size, .hook = .{ .ctx = self, .token = .{ ino, (b << 32) | rel, k.vol_tag, k.offset, before, lease_epoch.load(ino) }, .call = &directLegRecheck } };
    }

    /// The post-DMA recheck (worker thread, at the CQE): `incarnation still
    /// (the pre-DMA snapshot) ∧ the CURRENT map still binds (ino, block) →
    /// key` — the read path's (a) ∧ (b), linearized at this recheck.
    /// `landed` is the window the DMA filled (the slot's bytes): the read
    /// cache deposits it FIRST (validated against the snapshot under its
    /// lock) and a failed recheck purges it — the deposit-then-recheck
    /// order every tier fill keeps (`ReadPath.loseForeign`).
    fn directLegRecheck(ctx: ?*anyopaque, token: *const [6]u64, ok: bool, landed: []const u8) bool {
        const self: *WarmLadder = @ptrCast(@alignCast(ctx.?));
        if (!ok) return false;
        const k = BlockKey.init(token[2], token[3]);
        const rp = self.rp;
        const ino = token[0];
        const b = token[1] >> 32;
        const rel = token[1] & std.math.maxInt(u32);
        const deposit = landed.len != 0 and rp.grains.enabled();
        if (deposit) {
            rp.depositWindow(k, rel, landed, ino, token[5], token[4]);
            stats.direct_leg_deposits.inc();
        }
        const valid = rp.incarnationStill(k, token[4]) and (if (rp.meta.binding(rp.meta.ctx, ino, b)) |cur| BlockKey.fromMapping(cur).eql(k) else false);
        if (!valid) {
            if (deposit) _ = rp.grains.purgeRange(k, rel, landed.len);
            stats.direct_leg_recheck_failures.inc();
        }
        return valid;
    }

    /// The fd-source CQE hook: release the view's pin.
    fn fdSourceDone(ctx: ?*anyopaque, token: *const [6]u64, _: bool, _: []const u8) bool {
        const self: *WarmLadder = @ptrCast(@alignCast(ctx.?));
        switch (token[0]) {
            0 => self.rp.hot.unpin(.{ .slot = @intCast(token[1]), .len = @intCast(token[2]) }),
            else => self.rp.hold.unpin(.{ .slot = @intCast(token[1]), .len = @intCast(token[2]), .ledger_visible = token[3] != 0 }),
        }
        return true;
    }

    /// The fill-pool bounce for the handler: `(memfd, off)` of a served
    /// view when it lies on the MAP_SHARED slab (`replyFdBody`), null ⇒ the
    /// copy arm.
    pub fn fdSource(self: *WarmLadder, view: []const u8) ?@import("read_fill_pool.zig").FdOffset {
        return self.rp.fdSource(view);
    }

    // ---- the `Filesystem.VTable` arms (the lifecycle layer wires them) ----

    /// `probe_read(ctx, req, window)`: a served window, an fd-source for a
    /// pinned pool view (the hook releases the pin at the CQE), EOF, or a
    /// decline. `ino` is the request's KEY ino (the binding routes the
    /// kernel nodeid; the plane's tables never see a nodeid).
    pub fn probeRead(self: *WarmLadder, ino: u64, req: fsv.Request, window: []u8) ?fsv.ProbeReply {
        const in = switch (req.op) {
            .read => |r| r,
            else => return null,
        };
        const o_direct: u32 = @bitCast(linux.O{ .DIRECT = true });
        return switch (self.probe(ino, in.offset, in.size, in.flags & o_direct != 0, window)) {
            .served => |s| blk: {
                if (s.view) |view| {
                    const pinned = s.pinned.?;
                    const src = self.rp.fdSource(view) orelse {
                        // Not on the slab after all: the copy arm.
                        @memcpy(window[0..s.n], view);
                        self.unpinServed(pinned);
                        break :blk .{ .reply = codec.replyData(req.unique(), window[0..s.n]) };
                    };
                    stats.fast_dispatch_fd_source_serves.inc();
                    const token: [6]u64 = switch (pinned) {
                        .hot => |hit| .{ 0, hit.slot, hit.len, 0, 0, 0 },
                        .hold => |sv| .{ 1, sv.slot, sv.len, @intFromBool(sv.ledger_visible), 0, 0 },
                    };
                    break :blk .{ .fd_source = .{ .unique = req.unique(), .fd = src.fd, .off = src.off, .body = view, .done = .{ .ctx = self, .token = token, .call = &fdSourceDone } } };
                }
                break :blk .{ .reply = codec.replyData(req.unique(), window[0..s.n]) };
            },
            .eof => .{ .reply = codec.replyData(req.unique(), &.{}) },
            .demote => null,
        };
    }

    /// `direct_leg(ctx, req)` for the request's KEY ino `ino`.
    pub fn directLegFor(self: *WarmLadder, ino: u64, req: fsv.Request) ?fsv.DirectLeg {
        const in = switch (req.op) {
            .read => |r| r,
            else => return null,
        };
        const o_direct: u32 = @bitCast(linux.O{ .DIRECT = true });
        return self.directLeg(ino, in.offset, in.size, in.flags & o_direct != 0);
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "direct-leg eligibility: passthrough ∧ !device_true ∧ 4 KiB-aligned ∧ single block" {
    const bs: u64 = 4 << 20;
    try testing.expect(directLegEligible(true, false, 0, 4096, bs));
    try testing.expect(directLegEligible(true, false, bs - 4096, 4096, bs));
    try testing.expect(!directLegEligible(true, false, bs - 4096, 8192, bs));
    try testing.expect(!directLegEligible(true, false, 100, 4096, bs));
    try testing.expect(!directLegEligible(true, false, 0, 4000, bs));
    try testing.expect(!directLegEligible(false, false, 0, 4096, bs));
    try testing.expect(!directLegEligible(true, true, 0, 4096, bs));
    try testing.expect(!directLegEligible(true, false, 0, 0, bs));
}

test "the warm ladder asks the write side TRY-ONLY: a `busy` custody station is a write_side Demote at once (the lane asks again blocking), a served station answers from RAM, none falls to the tiers (repro-port: LTP rwtest03 — the queue worker parked in BlockLocks.acquire(read) under a write awaiting that worker's ring)" {
    const F = read_path.TestFixture;
    const f = try F.init(testing.allocator, .{});
    defer f.deinit();
    f.size = F.bs;
    f.bind(0, 2);
    var guards = try op_guard.Table.init(testing.allocator, 64);
    defer guards.deinit();
    var ladder: WarmLadder = .{ .rp = f.rp, .guards = &guards, .io = testing.io };
    const window = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), 1 << 20);
    defer testing.allocator.free(window);
    const WS = struct {
        var mode: read_path.StagedBlock = .none;
        var try_only_seen: ?bool = null;
        fn overlay(_: *anyopaque, _: u64, _: u64, _: u64, _: []u8) read_path.OverlayVerdict {
            return .none;
        }
        fn runs(_: *anyopaque, _: u64, _: u64, _: u64, _: u64, _: []u8) read_path.ExtentCoverage {
            return .none;
        }
        fn stagedBlock(_: *anyopaque, _: u64, _: u64, _: u64, _: u64, dest: []u8, try_only: bool) read_path.StagedBlock {
            try_only_seen = try_only;
            if (mode == .served) @memset(dest[0..mode.served], 0x5A);
            return mode;
        }
        fn stagedPayload(_: *anyopaque, _: u64, _: layout.FileId, _: u64, _: u64, _: []u8) read_path.StagedVerdict {
            return .{ .absent = .{} };
        }
        fn retained(_: *anyopaque, _: u64) bool {
            return false;
        }
        fn openOverlay(_: *anyopaque, _: u64, _: u64) bool {
            return false;
        }
        var custody: bool = false;
        fn hasCustody(_: *anyopaque, _: u64, _: u64) bool {
            return custody;
        }
    };
    var dummy: u8 = 0;
    f.rp.write_side = .{ .ctx = &dummy, .overlay_read = &WS.overlay, .extent_runs = &WS.runs, .staged_block = &WS.stagedBlock, .staged_payload = &WS.stagedPayload, .has_retained_extents = &WS.retained, .open_overlay = &WS.openOverlay, .has_custody = &WS.hasCustody };
    const ws0 = stats.fast_dispatch_demote_write_side.load();
    // Busy: the demote names the write side; the probe asked try-only.
    WS.mode = .busy;
    try testing.expectEqual(Verdict{ .demote = .write_side }, ladder.probe(7, 0, 4096, false, window));
    try testing.expectEqual(@as(?bool, true), WS.try_only_seen);
    try testing.expectEqual(ws0 + 1, stats.fast_dispatch_demote_write_side.load());
    // Served: the station's bytes, the rest of the window zeros.
    WS.mode = .{ .served = 100 };
    try testing.expectEqual(Verdict{ .served = .{ .n = 4096, .arm = .staged } }, ladder.probe(7, 0, 4096, false, window));
    try testing.expect(std.mem.allEqual(u8, window[0..100], 0x5A));
    try testing.expect(std.mem.allEqual(u8, window[100..4096], 0));
    // None: the tiers (cold ⇒ miss).
    WS.mode = .none;
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 0, 4096, false, window));
    // The direct device leg after a decline: it DMAs the mapped block
    // straight to the app, so it must decline while the write side holds
    // ANY custody of the block (repro-port: LTP fs gf27 — after the W2
    // fold published block 1, the 65th one-byte write parked a fresh
    // extent and the next read's direct leg served the device's bytes: 0).
    ladder.direct_leg_armed = true;
    WS.custody = true;
    const declines0 = stats.direct_leg_custody_declines.load();
    try testing.expect(ladder.directLeg(7, 0, 4096, false) == null);
    try testing.expectEqual(declines0 + 1, stats.direct_leg_custody_declines.load());
    WS.custody = false;
    // No custody: the gate passes (the fixture resolves no device fd, so
    // the offer itself stays null — the custody counter is what moved).
    try testing.expect(ladder.directLeg(7, 0, 4096, false) == null);
    try testing.expectEqual(declines0 + 1, stats.direct_leg_custody_declines.load());
}

test "a cold direct-leg read resolves its file ONCE and its binding TWICE — the probe's reads before the DMA, the recheck's after the bytes: the leg reuses the probe's resolution of the same request; a moved binding still fails the recheck; another request resolves fresh" {
    const F = read_path.TestFixture;
    const f = try F.init(testing.allocator, .{});
    defer f.deinit();
    f.size = 2 * F.bs;
    f.bind(0, 2);
    f.bind(1, 3);
    // Described, never executed: the leg carries the fd to the transport.
    f.dev_fd = 3;
    var guards = try op_guard.Table.init(testing.allocator, 64);
    defer guards.deinit();
    var ladder: WarmLadder = .{ .rp = f.rp, .guards = &guards, .io = testing.io, .direct_leg_armed = true };
    const window = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), 1 << 20);
    defer testing.allocator.free(window);
    // The transport's order for ONE request: probe → decline → leg → CQE hook.
    const files0 = f.file_calls.load(.monotonic);
    const binds0 = f.binding_calls.load(.monotonic);
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 8192, 4096, true, window));
    const leg = ladder.directLeg(7, 8192, 4096, true) orelse return error.TestExpectedEqual;
    try testing.expectEqual(@as(u64, 2 * F.bs + 8192), leg.off);
    const hook = leg.hook.?;
    try testing.expect(hook.call(hook.ctx, &hook.token, true, &.{}));
    try testing.expectEqual(files0 + 1, f.file_calls.load(.monotonic));
    try testing.expectEqual(binds0 + 2, f.binding_calls.load(.monotonic));
    // The binding moves between the probe and the bytes: the leg DMAs the
    // probe's key and the post-DMA recheck withdraws the reply.
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 4096, 4096, true, window));
    const leg2 = ladder.directLeg(7, 4096, 4096, true) orelse return error.TestExpectedEqual;
    f.bind(0, 5);
    const failures0 = stats.direct_leg_recheck_failures.load();
    try testing.expect(!leg2.hook.?.call(leg2.hook.?.ctx, &leg2.hook.?.token, true, &.{}));
    try testing.expectEqual(failures0 + 1, stats.direct_leg_recheck_failures.load());
    // A leg for another request than the probe's resolves fresh.
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 0, 4096, true, window));
    const files1 = f.file_calls.load(.monotonic);
    const binds1 = f.binding_calls.load(.monotonic);
    const leg3 = ladder.directLeg(7, F.bs, 4096, true) orelse return error.TestExpectedEqual;
    try testing.expectEqual(@as(u64, 3 * F.bs), leg3.off);
    try testing.expectEqual(files1 + 1, f.file_calls.load(.monotonic));
    try testing.expectEqual(binds1 + 1, f.binding_calls.load(.monotonic));
}

test "the read cache on the queue thread: a cold probe demotes; on a zero-copy FUSE session the direct leg is declined so the lane fills (the control arm offers it); the IPC direct-drive's leg DEPOSITS its landed arena window at the CQE hook (then rechecks), so the next probe of the same window serves it inline as the `grains` arm with no device read; an unaligned sub-grain request hits the same grain; a failed recheck purges what the hook deposited" {
    const F = read_path.TestFixture;
    const g = read_cache.GRAIN;
    const f = try F.init(testing.allocator, .{ .read_cache_bytes = 64 * g, .policy = .always });
    defer f.deinit();
    f.size = 2 * F.bs;
    f.bind(0, 2);
    f.bind(1, 3);
    f.dev_fd = 3;
    var guards = try op_guard.Table.init(testing.allocator, 64);
    defer guards.deinit();
    var ladder: WarmLadder = .{ .rp = f.rp, .guards = &guards, .io = testing.io, .direct_leg_armed = true };
    const window = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), 1 << 20);
    defer testing.allocator.free(window);
    const key2 = BlockKey.init(F.vol_tag, 2 * F.bs);
    // On a zero-copy FUSE session the leg lands in the app's pages: the
    // shipped arm offers it (no fill from FUSE); the `lane` arm declines
    // it so the lane fills.
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 8192, 4096, false, window));
    try testing.expect(ladder.directLeg(7, 8192, 4096, false) != null);
    f.rp.cfg.read_cache_fill = .lane;
    const demotes0 = stats.direct_leg_cache_demotes.load();
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 8192, 4096, false, window));
    try testing.expectEqual(@as(?fsv.DirectLeg, null), ladder.directLeg(7, 8192, 4096, false));
    try testing.expectEqual(demotes0 + 1, stats.direct_leg_cache_demotes.load());
    f.rp.cfg.read_cache_fill = .leg;
    // The IPC direct-drive's leg (`resolveDirectLeg`): the DMA lands in the
    // session arena this process owns (simulated here), the hook deposits
    // the landed window and validates.
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 8192, 4096, false, window));
    const leg = ladder.resolveDirectLeg(7, 8192, 4096, false) orelse return error.TestExpectedEqual;
    @memcpy(window[0..4096], f.device[@intCast(2 * F.bs + 8192)..][0..4096]);
    const deposits0 = stats.direct_leg_deposits.load();
    try testing.expect(leg.hook.?.run(true, window[0..4096]));
    try testing.expectEqual(deposits0 + 1, stats.direct_leg_deposits.load());
    try testing.expect(f.rp.grains.contains(key2, 8192, 8192 + g));
    // Warm: the probe serves the grain inline — no lane, no device.
    const reads0 = f.device_reads.load(.monotonic);
    const grains0 = stats.fast_dispatch_serve_grains.load();
    @memset(window[0..4096], 0);
    const v = ladder.probe(7, 8192, 4096, true, window);
    try testing.expectEqual(@as(usize, 4096), v.served.n);
    try testing.expectEqual(Arm.grains, v.served.arm);
    try F.expected(2 * F.bs + 8192, window[0..4096]);
    try testing.expectEqual(reads0, f.device_reads.load(.monotonic));
    try testing.expectEqual(grains0 + 1, stats.fast_dispatch_serve_grains.load());
    // A sub-grain unaligned request inside the deposited window hits too.
    const v2 = ladder.probe(7, 8192 + 100, 300, false, window);
    try testing.expectEqual(@as(usize, 300), v2.served.n);
    try F.expected(2 * F.bs + 8192 + 100, window[0..300]);
    // The neighbouring grain is cold: a demote, the leg; the binding moves
    // under the DMA — the hook deposits, the recheck fails and the grain is
    // purged with the reply withdrawn.
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 8192 + g, 4096, false, window));
    const leg2 = ladder.resolveDirectLeg(7, 8192 + g, 4096, false) orelse return error.TestExpectedEqual;
    f.bind(0, 5);
    try testing.expect(!leg2.hook.?.run(true, window[0..4096]));
    try testing.expect(!f.rp.grains.contains(key2, 8192 + g, 8192 + 2 * g));
    // A leg that landed nothing deposits nothing.
    f.bind(0, 2);
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 3 * g, 4096, false, window));
    const leg3 = ladder.resolveDirectLeg(7, 3 * g, 4096, false) orelse return error.TestExpectedEqual;
    try testing.expect(!leg3.hook.?.run(false, &.{}));
    try testing.expect(!f.rp.grains.contains(key2, 3 * g, 4 * g));
}

test "the warm ladder never blocks: a cold block demotes (miss), a held guard demotes (contended) without waiting, a tier-resident block serves byte-exact and EOF-clamped into the window, oversize and device-true O_DIRECT pre-screen; serves + demotes ≡ probes" {
    const F = read_path.TestFixture;
    const f = try F.init(testing.allocator, .{});
    defer f.deinit();
    const tail: u64 = 300 * derive.KiB;
    f.size = F.bs + tail;
    f.bind(0, 2);
    f.bind(1, 3);
    var guards = try op_guard.Table.init(testing.allocator, 64);
    defer guards.deinit();
    var ladder: WarmLadder = .{ .rp = f.rp, .guards = &guards, .io = testing.io };
    const window = try testing.allocator.alignedAlloc(u8, .fromByteUnits(4096), 1 << 20);
    defer testing.allocator.free(window);
    const serves0 = stats.fast_dispatch_serves.load();
    const demotes0 = stats.fast_dispatch_demotes.load();
    var probes: u64 = 0;
    // Cold: a miss, nothing touched the device.
    try testing.expectEqual(Verdict{ .demote = .miss }, ladder.probe(7, 0, 4096, false, window));
    probes += 1;
    try testing.expectEqual(@as(u64, 0), f.device_reads.load(.monotonic));
    // The lane fills it; the probe then serves from the hot tier.
    _ = try f.rp.read(testing.io, 7, 0, 1 << 20, .{ .buf = window }, .{});
    @memset(window, 0);
    const v = ladder.probe(7, 8192, 4096, false, window);
    probes += 1;
    try testing.expectEqual(Verdict{ .served = .{ .n = 4096, .arm = .hot } }, v);
    try F.expected(2 * F.bs + 8192, window[0..4096]);
    // EOF clamp: block 1 holds a 300 KiB tail (a whole-block fill: above
    // the ranged threshold); a 4 KiB request 1000 bytes before EOF serves 1000.
    _ = try f.rp.read(testing.io, 7, F.bs, tail, .{ .buf = window }, .{});
    const v2 = ladder.probe(7, F.bs + tail - 1000, 4096, false, window);
    probes += 1;
    try testing.expectEqual(Verdict{ .served = .{ .n = 1000, .arm = .hot } }, v2);
    try F.expected(3 * F.bs + tail - 1000, window[0..1000]);
    try testing.expectEqual(Verdict.eof, ladder.probe(7, F.bs + tail, 4096, false, window));
    probes += 1;
    // Pre-screens.
    try testing.expectEqual(Verdict{ .demote = .pre_screen }, ladder.probe(7, 0, 0, false, window));
    try testing.expectEqual(Verdict{ .demote = .pre_screen }, ladder.probe(7, 0, 4096, false, window[0..100]));
    probes += 2;
    // A held exclusive guard: Demote at once (no wait — this thread holds
    // the guard, so a probe that waited for it would never return).
    const held = guards.acquire(testing.io, 7, .exclusive);
    try testing.expectEqual(Verdict{ .demote = .contended }, ladder.probe(7, 0, 4096, false, window));
    probes += 1;
    held.release(testing.io);
    // The hold arm after the hot tier dropped the block: block 0's whole-
    // block read credited its full coverage (retired from the hold); block
    // 1's 300 KiB read left the hold entry 700 KiB short of retirement.
    const k0 = BlockKey.init(F.vol_tag, 2 * F.bs);
    try testing.expect(!f.rp.hold.contains(k0));
    const k1 = BlockKey.init(F.vol_tag, 3 * F.bs);
    try testing.expect(f.rp.hot.purge(k1));
    try testing.expect(f.rp.hold.contains(k1));
    try testing.expectEqual(Verdict{ .served = .{ .n = 4096, .arm = .hold } }, ladder.probe(7, F.bs + 4096, 4096, false, window));
    probes += 1;
    try F.expected(3 * F.bs + 4096, window[0..4096]);
    // The partition law.
    try testing.expectEqual(probes - 1, (stats.fast_dispatch_serves.load() - serves0) + (stats.fast_dispatch_demotes.load() - demotes0));
    // Device-true O_DIRECT pre-screens; the direct leg declines unarmed and
    // offers on an aligned cold single-block request once armed.
    const g = try F.init(testing.allocator, .{ .direct_device_true = true });
    defer g.deinit();
    g.size = F.bs;
    g.bind(0, 4);
    var ladder2: WarmLadder = .{ .rp = g.rp, .guards = &guards, .io = testing.io };
    try testing.expectEqual(Verdict{ .demote = .pre_screen }, ladder2.probe(7, 0, 4096, true, window));
    try testing.expect(ladder2.directLeg(7, 0, 4096, false) == null);
    ladder2.direct_leg_armed = true;
    // The fixture resolves no device fd: still null; the predicate itself passed.
    try testing.expect(ladder2.directLeg(7, 0, 4096, false) == null);
    try testing.expect(ladder.fdSource(window) == null);
}
