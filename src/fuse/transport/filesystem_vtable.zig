//! The `Filesystem` face the transport calls and the reply handle a
//! handler answers through (`docs/porting/fuse_handlers_lanes.md` §1, §5;
//! charter §4 "READ dispatch law", §9). Portable; the lifecycle agent's
//! real filesystem is built against this SAME shape.
//!
//! The exactly-one-reply obligation is a VALUE: `Handle{slot, unique}` (a
//! ring slot by `(qid, ent_idx, commit_id)` or the classical venue by
//! `unique`) plus the transport's `Tx` face. A handler calls exactly one of
//! `reply`, `replyFdBody`, `replyPrefilled` or `fail`; a second call is
//! refused by the slot table (never applied), a missing call is caught by
//! the teardown drain's `fail_ent`. No drop guards, no unwinding (decision
//! 0002 rows 9 and Q3): a handler that cannot answer says so with `fail`.
//!
//! Request views: `Request.op`'s slices (names, xattr values, the FORGET
//! array, WRITE data) point into the ent's header/payload regions, which
//! the kernel does not touch between delivery and the COMMIT this handler
//! itself triggers — they are valid until the reply is submitted. WRITE
//! data additionally rides a `PayloadLease` the handler releases exactly
//! once (the commit gate parks a reply submitted while it is live).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const slot = @import("slot.zig");
const zc = @import("zc.zig");
const geometry = @import("../geometry.zig");
const errors = @import("../../core/errors.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");

/// Where a reply goes: a ring slot by value, or the classical sideband
/// (the request arrived through `/dev/fuse` and is answered by a device write).
pub const ReplySlot = union(enum) {
    ring: slot.SlotRef,
    classical,
};

/// A decoded request with its identity.
pub const Request = struct {
    header: abi.fuse_in_header,
    op: codec.Request,

    pub fn unique(self: Request) u64 {
        return self.header.unique;
    }
    pub fn nodeid(self: Request) u64 {
        return self.header.nodeid;
    }
};

/// The transport face (type-erased so this module stays portable): the
/// session fills it; a handle carries a pointer to it.
pub const Tx = struct {
    ctx: *anyopaque,
    submit: *const fn (ctx: *anyopaque, h: Handle, r: codec.Reply) void,
    submit_fd_body: *const fn (ctx: *anyopaque, h: Handle, r: codec.Reply, fd: linux.fd_t, off: u64) void,
    submit_prefilled: *const fn (ctx: *anyopaque, h: Handle, n: u32) void,
    fail: *const fn (ctx: *anyopaque, h: Handle, errno: linux.E) void,
    /// The RETAIN commit (zero-copy retention): the reply lands, the request
    /// ends, the slot's pages stay registered until `release_retained`.
    submit_retain: *const fn (ctx: *anyopaque, h: Handle, r: codec.Reply) void,
    release_retained: *const fn (ctx: *anyopaque, h: Handle) void,
};

/// The reply obligation by value.
pub const Handle = struct {
    tx: *const Tx,
    slot: ReplySlot,
    unique: u64,
    /// The slot's payload window on the ring (empty on the classical
    /// venue): a handler may serve a body INTO it so the commit's copy is
    /// elided; the window is writable only while no lease is live over it.
    window: []u8,

    pub fn reply(self: Handle, r: codec.Reply) void {
        self.tx.submit(self.tx.ctx, self, r);
    }

    /// The zc fd→slot bridge (`READ_FIXED(fd @ off → slot)`); the body is
    /// the keepalive and the fallback on a UserEnts queue.
    pub fn replyFdBody(self: Handle, r: codec.Reply, fd: linux.fd_t, off: u64) void {
        self.tx.submit_fd_body(self.tx.ctx, self, r, fd, off);
    }

    /// Header only: `n` bytes already sit in the slot's payload.
    pub fn replyPrefilled(self: Handle, n: u32) void {
        self.tx.submit_prefilled(self.tx.ctx, self, n);
    }

    /// Header-only `-errno`.
    pub fn fail(self: Handle, errno: linux.E) void {
        self.tx.fail(self.tx.ctx, self, errno);
    }

    /// The ACK-early reply (zero-copy retention, charter §3 "Payload
    /// retention"): the reply lands and write(2) returns NOW while the held
    /// WRITE's pages stay registered for a `WRITE_FIXED(device ← slot)` the
    /// handler finishes afterwards, then `releaseRetained`. Legal only for a
    /// `.page_cache` lease (the stability law: a `.direct_gup` payload must be
    /// extracted first); the worker refuses the rest and commits plain.
    pub fn replyRetain(self: Handle, r: codec.Reply) void {
        self.tx.submit_retain(self.tx.ctx, self, r);
    }

    /// Ends a retention: `RELEASE_PAYLOAD` for this slot. Never before the
    /// `replyRetain` it pairs with (the worker orders a racing release after
    /// the RETAIN commit in the same batch).
    pub fn releaseRetained(self: Handle) void {
        self.tx.release_retained(self.tx.ctx, self);
    }

    /// A typed error through the ONE errno table (`core/errors.zig`).
    pub fn failWith(self: Handle, err: errors.Error) void {
        self.fail(errors.toErrno(err));
    }
};

/// What `init` tells the filesystem after INIT negotiated.
pub const InitCaps = struct {
    /// The folded capability word the kernel will apply.
    negotiated: u64,
    minor: u32,
    plan: geometry.Plan,
    /// The executor handle handlers may park on (`std.Io` futex ops park
    /// the fiber, never the lane thread).
    io: std.Io,
    /// The lattice rung this session runs (decided ONCE from the probes).
    buffer_mode: geometry.BufferMode = .user_ents,
    /// `SQUEEZEFS_READ_ZC_SERVE` ∧ a zero-copy session: build the fill pool
    /// as ONE MAP_SHARED memfd slab and reply warm serves with `replyFdBody`
    /// (the fd-source leg, zero daemon passes).
    read_zc_serve: bool = false,
    /// Payload retention negotiated: `replyRetain` is available.
    retention: bool = false,
    /// Mainline zero-copy is granted per open file (decision 0003 §BE):
    /// `zc_open` asks the filesystem to SERVE regular-file opens and mark
    /// every OPEN/CREATE reply `FOPEN_IO_URING_ZERO_COPY` (the session
    /// already dropped zero-message opens for this mount); `zc_create_only`
    /// keeps zero-message opens and marks CREATE replies alone (the one
    /// reply a no-open connection still sends — a created file's first
    /// open is zero-copy, a re-open is not).
    zc_open: bool = false,
    zc_create_only: bool = false,
};

/// The hold gate's input: the WRITE's shape and class, decided ON THE
/// WORKER THREAD at delivery. The gate is a pure predicate over this and
/// the filesystem's published per-inode snapshot — never a cache lookup, a
/// lock or an await (0002 ledger: the reference probed its metadata cache
/// on the queue worker). The transport already proved `zc.holdShape`
/// (aligned, one block); the filesystem adds the class (`zc.holdClass` on
/// its PATCH_MAX) and what only it knows (striped on a passthrough set; for
/// W1 non-extending and not stream-adjacent; for the overlay its arm on).
pub const HoldHint = struct {
    ino: u64,
    offset: u64,
    len: u32,
    class: zc.PayloadClass,
};

/// A bridge-completion hook: runs ON THE WORKER THREAD at the bridge's CQE
/// (ok = the DMA landed its full length), sync and non-blocking. For the
/// direct device leg it is the post-DMA incarnation + binding RECHECK: a
/// `false` on a landed leg WITHDRAWS the prefilled reply (the slot takes
/// the ordinary ladder, never unvalidated device bytes in an app's pages).
/// For an fd-source serve it releases the served view's pin. `token` is
/// the filesystem's own facts (no allocation per op on the worker).
/// `landed` is the bridge's destination as the CQE left it — the direct
/// leg's window (the read cache's deposit, charter §6.3), empty for a
/// hook that moved nothing the filesystem may read (an fd-source serve, a
/// leg whose slot was given up).
pub const CompletionHook = struct {
    ctx: ?*anyopaque,
    token: [6]u64 = @splat(0),
    call: *const fn (ctx: ?*anyopaque, token: *const [6]u64, ok: bool, landed: []const u8) bool,

    pub fn run(self: CompletionHook, ok: bool, landed: []const u8) bool {
        return self.call(self.ctx, &self.token, ok, landed);
    }
};

/// The direct device leg of a paged READ (charter §4 "Buffer-mode
/// lattice"): the worker bridges `READ_FIXED(fd @ off → slot)` straight
/// into the app's pages and commits header-only `payload_sz = len`.
/// Eligibility is the filesystem's (cold ∧ passthrough ∧ 4 KiB-aligned ∧
/// single block ∧ device resolvable); `len` is the FULL reply length (a
/// short leg is a refusal — the bounce leg then serves the whole body).
/// `hook` is the post-DMA recheck (charter §6.3: every fill is validated
/// AFTER the bytes are in hand) — a leg without one is never armed.
pub const DirectLeg = struct { fd: linux.fd_t, off: u64, len: u32, hook: ?CompletionHook = null };

/// A probe's fd-source answer (`SQUEEZEFS_READ_ZC_SERVE`): the served
/// bytes sit on the fill pool's MAP_SHARED memfd slab at `(fd, off)`; the
/// worker bridges `READ_FIXED(fd → slot)` with zero daemon passes. `body`
/// is the SAME bytes as a VA view — the keepalive and the restage source
/// when the bridge comes back short; `done` releases the view's pin at the
/// CQE (after any restage copy).
pub const FdSource = struct {
    unique: u64,
    fd: linux.fd_t,
    off: u64,
    body: []const u8,
    done: ?CompletionHook = null,
};

/// What `probe_read` answers: a reply served INTO the window, or an
/// fd-source the transport bridges on a zero-copy dest slot (elsewhere the
/// worker copies `body` into the window and runs `done` at once).
pub const ProbeReply = union(enum) {
    reply: codec.Reply,
    fd_source: FdSource,
};

pub const Filesystem = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// After INIT negotiated, before the session is ready.
        init: *const fn (ctx: *anyopaque, caps: InitCaps) void,
        /// DESTROY arrived, or the session is torn down (once).
        destroy: *const fn (ctx: *anyopaque) void,
        /// On a handler lane homed on the request's queue CPU (or the
        /// sideband's lane for a classical straggler): answer `handle`
        /// exactly once; release `lease` exactly once when present.
        dispatch: *const fn (ctx: *anyopaque, req: Request, handle: Handle, lease: ?slot.PayloadLease) void,
        /// The READ dispatch law's arm (a), ON THE WORKER THREAD at the
        /// delivery CQE: SYNC, try-only, non-blocking (a contended lock is
        /// a decline, never a spin; never a device touch; never an
        /// allocation). Serve into `window` and return the reply, or null to
        /// decline (the request then takes arm (c)). On a zero-copy
        /// session a warm hit on the memfd fill pool may answer an
        /// `fd_source` instead (zero daemon passes).
        probe_read: *const fn (ctx: *anyopaque, req: Request, window: []u8) ?ProbeReply,
        /// FORGET / BATCH_FORGET nlookup hand-off, on the worker or sideband
        /// thread before the auto-commit: cheap and non-blocking. The
        /// lifecycle layer owns the open-count law (`open_count_stranded`:
        /// a final FORGET with open handles = a lost RELEASE) behind it.
        forget: *const fn (ctx: *anyopaque, ino: u64, nlookup: u64) void,
        /// The W1 hold gate (zero-copy), ON THE WORKER THREAD at a paged
        /// WRITE's delivery: a PURE predicate over the hint and a published
        /// snapshot (no lookup, no lock, no await). True ⇒ the payload is
        /// HELD in the slot (`PayloadLease.held`); false ⇒ extracted at
        /// delivery. The transport proved the shape already.
        write_hold_eligible: *const fn (ctx: *anyopaque, hint: HoldHint) bool,
        /// The direct device leg of a paged READ, ON THE WORKER THREAD after
        /// `probe_read` declined: `{fd, off, len}` when the whole reply can
        /// DMA from a device fd into the app's pages, else null (the bounce
        /// leg or the lane).
        direct_leg: *const fn (ctx: *anyopaque, req: Request) ?DirectLeg,
        /// The metadata twin of `probe_read` (decision 0003 §AB.1), ON THE
        /// WORKER THREAD at a LOOKUP's or GETATTR's delivery — and a no-open
        /// session's FLUSH (0003 §AF: the close of a file holding no write
        /// custody answers here, success only): SYNC,
        /// try-only, non-blocking — answered from RAM (a node-cache hit on
        /// a slot this mount holds) into `out`, or null to decline (the
        /// request then takes the lane). Null = every metadata request
        /// takes the lane.
        probe_meta: ?*const fn (ctx: *anyopaque, req: Request, out: *codec.OutBuf) ?codec.Reply = null,
    };

    pub fn init(self: Filesystem, caps: InitCaps) void {
        self.vtable.init(self.ctx, caps);
    }
    pub fn destroy(self: Filesystem) void {
        self.vtable.destroy(self.ctx);
    }
    pub fn dispatch(self: Filesystem, req: Request, handle: Handle, lease: ?slot.PayloadLease) void {
        self.vtable.dispatch(self.ctx, req, handle, lease);
    }
    pub fn probeRead(self: Filesystem, req: Request, window: []u8) ?ProbeReply {
        return self.vtable.probe_read(self.ctx, req, window);
    }
    pub fn forget(self: Filesystem, ino: u64, nlookup: u64) void {
        self.vtable.forget(self.ctx, ino, nlookup);
    }
    pub fn writeHoldEligible(self: Filesystem, hint: HoldHint) bool {
        return self.vtable.write_hold_eligible(self.ctx, hint);
    }
    pub fn probeMeta(self: Filesystem, req: Request, out: *codec.OutBuf) ?codec.Reply {
        const f = self.vtable.probe_meta orelse return null;
        return f(self.ctx, req, out);
    }
    pub fn directLeg(self: Filesystem, req: Request) ?DirectLeg {
        return self.vtable.direct_leg(self.ctx, req);
    }
};

// ---------------------------------------------------------------------------
// EchoFs: the test filesystem — a fixed tree served from static bytes.
// Test binaries only (referencing it from product code fails to compile).
// ---------------------------------------------------------------------------

pub const EchoFs = if (builtin.is_test) EchoFsImpl else @compileError("EchoFs is a test filesystem");

const EchoFsImpl = struct {
    const Self = @This();

    /// The tree: `/hello` (static text), `/big` (a 1 MiB pattern, several
    /// READs per file), `/park` (reads park until `release_parked`), `/scratch`
    /// (accepts WRITEs; the handler may hold the lease across its reply).
    pub const ino_root: u64 = abi.FUSE_ROOT_ID;
    pub const ino_hello: u64 = 2;
    pub const ino_big: u64 = 3;
    pub const ino_park: u64 = 4;
    pub const ino_scratch: u64 = 5;
    pub const hello_text = "hello world\n";
    pub const big_len: usize = 1 << 20;
    pub const scratch_cap: usize = 1 << 16;
    pub const names = [_][]const u8{ "hello", "big", "park", "scratch" };
    pub const inos = [_]u64{ ino_hello, ino_big, ino_park, ino_scratch };
    /// A name outside the tree whose LOOKUP parks in its handler (answered
    /// negative once released); every name with this prefix parks, so N
    /// distinct names are N concurrent parked LOOKUPs (the kernel runs one
    /// lookup per name at a time: `d_alloc_parallel`).
    pub const park_lookup_name = "park-lookup";

    fn parksLookup(name: []const u8) bool {
        return std.mem.startsWith(u8, name, park_lookup_name);
    }

    /// `held_store`'s face: the session's `storeHeld`, type-erased.
    pub const HeldStore = struct {
        ctx: *anyopaque,
        fd: linux.fd_t,
        store: *const fn (ctx: *anyopaque, src: slot.HeldSource, fd: linux.fd_t, off: u64, io: std.Io) slot.ExtractError!void,
    };

    /// Every READ declines the probe (arm (a)) and takes the lane (arm (c)).
    probe_declines: bool = false,
    /// LOOKUP and GETATTR are answered by `probe_meta` on the worker (the
    /// in-RAM tree); off = they take the lane like every other request.
    meta_probe: bool = false,
    /// The no-open posture (decision 0003 §AF): OPEN answers ENOSYS (the
    /// kernel latches and opens locally); a FLUSH is answered by
    /// `probe_meta` unless `flush_custody` says the file holds write-back
    /// custody (then it takes the lane, counted in `flushes_dispatched`).
    /// Cleared at INIT when the session serves opens for zero-copy
    /// (`InitCaps.zc_open`) — the lifecycle's flip, mirrored.
    no_open: bool = false,
    flush_custody: bool = false,
    /// The write-back timer's close (decision 0003 §AJ): every FLUSH is
    /// answered ENOSYS (the kernel latches `no_flush`), counted here.
    flush_enosys: bool = false,
    flushes_enosys: std.atomic.Value(u64) = .init(0),
    opens_enosys: std.atomic.Value(u64) = .init(0),
    file_releases: std.atomic.Value(u64) = .init(0),
    flush_probes_served: std.atomic.Value(u64) = .init(0),
    flushes_dispatched: std.atomic.Value(u64) = .init(0),
    /// SETATTRs carrying a size (`/scratch` truncates to it).
    size_sets: std.atomic.Value(u64) = .init(0),
    meta_probes_served: std.atomic.Value(u64) = .init(0),
    /// The probe declines the FIRST poll of each READ and serves the next:
    /// the fused frame's re-poll (arm (b)) is what serves it.
    probe_declines_first: bool = false,
    /// A WRITE handler replies BEFORE releasing its lease and waits until
    /// the worker parked the commit (the gate's Parked arm, observable).
    hold_lease_across_reply: bool = false,
    /// The hold gate says yes for `/scratch` (a test filesystem that declares
    /// eligibility; the transport proved the shape already).
    hold_eligible: bool = false,
    /// The direct store of a held payload (`WRITE_FIXED(fd ← slot)` on the
    /// ring that registered it): a held `/scratch` WRITE lands in `fd` at
    /// its offset through `store` — the session's `storeHeld` behind a
    /// type-erased face — instead of the scratch array; the lazy
    /// extraction arm stays the fallback on a failed store.
    held_store: ?HeldStore = null,
    direct_stores: std.atomic.Value(u64) = .init(0),
    direct_store_failures: std.atomic.Value(u64) = .init(0),
    /// A held `.page_cache` WRITE is answered ACK-early (`replyRetain`),
    /// extracted after `retain_gate` opens, then released.
    retain_writes: bool = false,
    /// With `retain_writes`: ask to retain even a `.direct_gup` payload
    /// (AFTER extracting it — the snapshot-first law keeps the bytes safe)
    /// so the worker's class refusal is observable.
    retain_ignore_class: bool = false,
    io: ?std.Io = null,
    caps: ?InitCaps = null,
    inited: counters.Gauge = .{},
    destroyed: counters.Gauge = .{},
    /// `/park` readers wait on this (set from the test); handlers park as
    /// fibers through the `Io` futex, never the lane thread.
    park_gate: std.Io.Event = .unset,
    parked_readers: std.atomic.Value(u32) = .init(0),
    /// `park_lookup_name` lookups park on `park_gate` too; released, the
    /// handler first judges whether its request's name (the payload
    /// buffer's bytes) is still mapped, then reads it.
    parked_lookups: std.atomic.Value(u32) = .init(0),
    names_unmapped_after_park: std.atomic.Value(u64) = .init(0),
    names_intact_after_park: std.atomic.Value(u64) = .init(0),
    /// A retaining WRITE handler waits here between its ACK-early reply and
    /// its extraction + release.
    retain_gate: std.Io.Event = .unset,
    /// nlookup ledger: entry replies +1, FORGET −n.
    lookups: [8]std.atomic.Value(u64) = @splat(.init(0)),
    forgets: std.atomic.Value(u64) = .init(0),
    forget_nlookup: std.atomic.Value(u64) = .init(0),
    dispatches: std.atomic.Value(u64) = .init(0),
    /// The thread the last dispatched handler ran on (decision 0003 §AB.3:
    /// the delivering worker's, on the merged arm).
    handler_tid: std.atomic.Value(std.Thread.Id) = .init(0),
    probes_served: std.atomic.Value(u64) = .init(0),
    probes_declined: std.atomic.Value(u64) = .init(0),
    /// Daemon copy passes over READ bodies (`readInto`): the side-by-side
    /// "daemon copies per READ" instrument.
    read_copies: std.atomic.Value(u64) = .init(0),
    fd_body_replies: std.atomic.Value(u64) = .init(0),
    writes_seen: std.atomic.Value(u64) = .init(0),
    write_bytes: std.atomic.Value(u64) = .init(0),
    held_writes: std.atomic.Value(u64) = .init(0),
    lazy_extracts: std.atomic.Value(u64) = .init(0),
    retained_replies: std.atomic.Value(u64) = .init(0),
    release_calls: std.atomic.Value(u64) = .init(0),
    extract_failures: std.atomic.Value(u64) = .init(0),
    /// The stability class of the last WRITE (0 page_cache, 1 direct_gup).
    last_write_class: std.atomic.Value(u8) = .init(0),
    lease_waits_parked: std.atomic.Value(u64) = .init(0),
    scratch: [scratch_cap]u8 = @splat(0),
    scratch_len: std.atomic.Value(u64) = .init(0),
    /// `/scratch`'s mtime (seconds), honoured by SETATTR: the TIME_LIMITS
    /// clamp is observable here.
    scratch_mtime: std.atomic.Value(i64) = .init(1_700_000_000),
    /// The fill pool (`SQUEEZEFS_READ_ZC_SERVE` on a zc session): ONE
    /// MAP_SHARED memfd slab holding `/big`, so a READ replies `fd_body`;
    /// the mapping stays for the body views.
    pool_fd: linux.fd_t = -1,
    pool_map: []u8 = &.{},

    pub const vtable: Filesystem.VTable = .{
        .init = fsInit,
        .destroy = fsDestroy,
        .dispatch = fsDispatch,
        .probe_read = fsProbeRead,
        .forget = fsForget,
        .write_hold_eligible = fsWriteHoldEligible,
        .direct_leg = fsDirectLeg,
        .probe_meta = fsProbeMeta,
    };

    pub fn filesystem(self: *Self) Filesystem {
        return .{ .ctx = self, .vtable = &vtable };
    }

    fn of(ctx: *anyopaque) *Self {
        return @ptrCast(@alignCast(ctx));
    }

    fn fsInit(ctx: *anyopaque, caps: InitCaps) void {
        const self = of(ctx);
        self.caps = caps;
        self.io = caps.io;
        if (caps.zc_open) self.no_open = false;
        self.inited.add(1);
        if (caps.read_zc_serve and builtin.os.tag == .linux) self.buildPool();
    }

    /// The fill-pool slab: `memfd_create` + `ftruncate(big_len)` + a
    /// MAP_SHARED fill of the pattern; the fd is the fd-source leg's.
    fn buildPool(self: *Self) void {
        const rc = linux.memfd_create("sqz-echo-fill-pool", linux.MFD.CLOEXEC);
        if (linux.errno(rc) != .SUCCESS) return;
        const fd: linux.fd_t = @intCast(rc);
        if (linux.errno(linux.ftruncate(fd, big_len)) != .SUCCESS) {
            _ = linux.close(fd);
            return;
        }
        const map = linux.mmap(null, big_len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
        if (linux.errno(map) != .SUCCESS) {
            _ = linux.close(fd);
            return;
        }
        const bytes: [*]u8 = @ptrFromInt(map);
        for (bytes[0..big_len], 0..) |*b, i| b.* = bigByte(i);
        self.pool_map = bytes[0..big_len];
        self.pool_fd = fd;
    }

    fn fsDestroy(ctx: *anyopaque) void {
        of(ctx).destroyed.add(1);
    }

    /// After the session is gone (no handler can still view the pool).
    pub fn deinit(self: *Self) void {
        if (self.pool_fd >= 0) {
            _ = linux.munmap(@alignCast(self.pool_map.ptr), self.pool_map.len);
            _ = linux.close(self.pool_fd);
            self.pool_fd = -1;
            self.pool_map = &.{};
        }
    }

    fn fsWriteHoldEligible(ctx: *anyopaque, hint: HoldHint) bool {
        const self = of(ctx);
        return self.hold_eligible and hint.ino == ino_scratch;
    }

    fn fsDirectLeg(_: *anyopaque, _: Request) ?DirectLeg {
        return null;
    }

    fn fsForget(ctx: *anyopaque, ino: u64, nlookup: u64) void {
        const self = of(ctx);
        _ = self.forgets.fetchAdd(1, .monotonic);
        _ = self.forget_nlookup.fetchAdd(nlookup, .monotonic);
        if (ino < self.lookups.len) {
            const cur = self.lookups[@intCast(ino)].load(.monotonic);
            self.lookups[@intCast(ino)].store(cur -| @max(nlookup, 1), .monotonic);
        }
    }

    /// The pattern byte of `/big` at `off`.
    pub fn bigByte(off: usize) u8 {
        return @truncate((off * 7 + (off >> 12)) & 0xFF);
    }

    fn fileLen(self: *const Self, ino: u64) ?u64 {
        return switch (ino) {
            ino_hello => hello_text.len,
            ino_big => big_len,
            ino_park => hello_text.len,
            ino_scratch => self.scratch_len.load(.acquire),
            else => null,
        };
    }

    fn attrOf(self: *const Self, ino: u64) ?abi.fuse_attr {
        const is_dir = ino == ino_root;
        const len = if (is_dir) 0 else (self.fileLen(ino) orelse return null);
        const mode: u32 = if (is_dir) 0o40755 else if (ino == ino_scratch) 0o100644 else 0o100444;
        const mtime: i64 = if (ino == ino_scratch) self.scratch_mtime.load(.acquire) else 1_700_000_000;
        return .{
            .ino = ino,
            .size = len,
            .blocks = (len + 511) / 512,
            .atime = 1_700_000_000,
            .mtime = @bitCast(mtime),
            .ctime = 1_700_000_000,
            .atimensec = 0,
            .mtimensec = 0,
            .ctimensec = 0,
            .mode = mode,
            .nlink = if (is_dir) 2 else 1,
            .uid = 0,
            .gid = 0,
            .rdev = 0,
            .blksize = 4096,
            .flags = 0,
        };
    }

    fn lookupName(name: []const u8) ?u64 {
        for (names, inos) |n, i| if (std.mem.eql(u8, n, name)) return i;
        return null;
    }

    fn entry(self: *Self, ino: u64) ?codec.EntryReply {
        const attr = self.attrOf(ino) orelse return null;
        if (ino < self.lookups.len and ino != ino_root) _ = self.lookups[@intCast(ino)].fetchAdd(1, .monotonic);
        return .{ .nodeid = ino, .entry_ttl_ns = std.time.ns_per_s, .attr_ttl_ns = std.time.ns_per_s, .attr = attr };
    }

    /// The bytes of `ino` at `[off, off + size)` served into `dst`: one
    /// daemon copy pass.
    fn readInto(self: *Self, ino: u64, off: u64, size: u32, dst: []u8) ?usize {
        const len = self.fileLen(ino) orelse return null;
        if (off >= len) return 0;
        const n: usize = @intCast(@min(@as(u64, size), @min(len - off, dst.len)));
        if (n > 0) _ = self.read_copies.fetchAdd(1, .monotonic);
        switch (ino) {
            ino_hello, ino_park => @memcpy(dst[0..n], hello_text[@intCast(off)..][0..n]),
            ino_big => for (dst[0..n], 0..) |*b, i| {
                b.* = bigByte(@intCast(off + i));
            },
            ino_scratch => @memcpy(dst[0..n], self.scratch[@intCast(off)..][0..n]),
            else => return null,
        }
        return n;
    }

    /// The tree is static RAM: every LOOKUP under the root and every
    /// GETATTR answers here when `meta_probe` is on (`/park-lookup` parks:
    /// the lane's).
    fn fsProbeMeta(ctx: *anyopaque, req: Request, out: *codec.OutBuf) ?codec.Reply {
        const self = of(ctx);
        if (req.op == .flush) {
            if (self.flush_enosys) {
                _ = self.flushes_enosys.fetchAdd(1, .monotonic);
                return codec.replyError(req.unique(), .NOSYS);
            }
            if (!self.no_open or self.flush_custody) return null;
            _ = self.flush_probes_served.fetchAdd(1, .monotonic);
            return codec.replyOk(req.unique());
        }
        if (!self.meta_probe) return null;
        const u = req.unique();
        const ino = req.nodeid();
        const reply: codec.Reply = switch (req.op) {
            .lookup => |l| blk: {
                if (ino != ino_root or parksLookup(l.name)) return null;
                const child = lookupName(l.name) orelse break :blk codec.replyEntry(out, u, codec.EntryReply.negative(std.time.ns_per_s));
                break :blk codec.replyEntry(out, u, self.entry(child).?);
            },
            .getattr => codec.replyAttr(out, u, .{ .attr_ttl_ns = std.time.ns_per_s, .attr = self.attrOf(ino) orelse return null }),
            else => return null,
        };
        _ = self.meta_probes_served.fetchAdd(1, .monotonic);
        return reply;
    }

    fn fsProbeRead(ctx: *anyopaque, req: Request, window: []u8) ?ProbeReply {
        const self = of(ctx);
        if (self.probe_declines) return null;
        const ino = req.nodeid();
        // `/park` always declines: its handler parks on the lane.
        if (ino == ino_park) return null;
        // `/big` on an fd-source session is the lane's `replyFdBody`.
        if (ino == ino_big and self.pool_fd >= 0) return null;
        if (self.probe_declines_first) {
            // Odd polls decline, even polls serve: the fused re-poll serves.
            const k = self.probes_declined.fetchAdd(1, .monotonic);
            if (k % 2 == 0) return null;
        }
        const in = req.op.read;
        const n = self.readInto(ino, in.offset, in.size, window) orelse return null;
        _ = self.probes_served.fetchAdd(1, .monotonic);
        return .{ .reply = codec.replyData(req.unique(), window[0..n]) };
    }

    /// A WRITE whose payload is HELD in the zc slot (the gate admitted it):
    /// the direct-store arm (`held_store`: the bytes move slot → fd with no
    /// daemon pass; a failed store falls to the extraction below), the
    /// retain arm (ACK-early, extract after the gate, release) or the lazy
    /// extraction arm. The lease is released before any reply.
    fn heldWrite(self: *Self, h: Handle, u: u64, w: codec.Write, l: slot.PayloadLease, out: *codec.OutBuf) void {
        _ = self.held_writes.fetchAdd(1, .monotonic);
        const io = self.io.?;
        const end = w.in.offset + w.in.size;
        if (self.held_store) |hs| {
            if (hs.store(hs.ctx, l.heldSource().?, hs.fd, w.in.offset, io)) |_| {
                _ = self.direct_stores.fetchAdd(1, .monotonic);
                l.release();
                _ = self.scratch_len.fetchMax(end, .acq_rel);
                _ = self.write_bytes.fetchAdd(w.in.size, .monotonic);
                return h.reply(codec.replyWrite(out, u, w.in.size));
            } else |_| {
                _ = self.direct_store_failures.fetchAdd(1, .monotonic);
            }
        }
        if (end > scratch_cap) {
            l.release();
            return h.fail(.FBIG);
        }
        const dst = self.scratch[@intCast(w.in.offset)..][0..w.in.size];
        if (self.retain_writes and (l.class == .page_cache or self.retain_ignore_class)) {
            if (l.class == .direct_gup) {
                // The snapshot-first law: a GUP payload is owned before any ACK.
                _ = l.extract(io, dst) catch {
                    _ = self.extract_failures.fetchAdd(1, .monotonic);
                    l.release();
                    return h.fail(.IO);
                };
                _ = self.lazy_extracts.fetchAdd(1, .monotonic);
                _ = self.scratch_len.fetchMax(end, .acq_rel);
                _ = self.write_bytes.fetchAdd(w.in.size, .monotonic);
            }
            l.release();
            h.replyRetain(codec.replyWrite(out, u, w.in.size));
            _ = self.retained_replies.fetchAdd(1, .monotonic);
            self.retain_gate.wait(io) catch {};
            if (l.class == .page_cache) {
                // The pages stay registered under retention: extract after the ACK.
                _ = l.extract(io, dst) catch {
                    _ = self.extract_failures.fetchAdd(1, .monotonic);
                };
                _ = self.lazy_extracts.fetchAdd(1, .monotonic);
                _ = self.scratch_len.fetchMax(end, .acq_rel);
                _ = self.write_bytes.fetchAdd(w.in.size, .monotonic);
            }
            h.releaseRetained();
            _ = self.release_calls.fetchAdd(1, .monotonic);
            return;
        }
        _ = l.extract(io, dst) catch {
            _ = self.extract_failures.fetchAdd(1, .monotonic);
            l.release();
            return h.fail(.IO);
        };
        _ = self.lazy_extracts.fetchAdd(1, .monotonic);
        l.release();
        _ = self.scratch_len.fetchMax(end, .acq_rel);
        _ = self.write_bytes.fetchAdd(w.in.size, .monotonic);
        h.reply(codec.replyWrite(out, u, w.in.size));
    }

    fn fsDispatch(ctx: *anyopaque, req: Request, h: Handle, lease: ?slot.PayloadLease) void {
        const self = of(ctx);
        _ = self.dispatches.fetchAdd(1, .monotonic);
        self.handler_tid.store(std.Thread.getCurrentId(), .release);
        var out: codec.OutBuf = undefined;
        const u = req.unique();
        const ino = req.nodeid();
        switch (req.op) {
            .lookup => |l| {
                if (ino != ino_root) return h.fail(.NOTDIR);
                if (parksLookup(l.name)) return self.parkedLookup(h, u, l.name, &out);
                const child = lookupName(l.name) orelse return h.reply(codec.replyEntry(&out, u, codec.EntryReply.negative(std.time.ns_per_s)));
                h.reply(codec.replyEntry(&out, u, self.entry(child).?));
            },
            .getattr => {
                const attr = self.attrOf(ino) orelse return h.fail(.NOENT);
                h.reply(codec.replyAttr(&out, u, .{ .attr_ttl_ns = std.time.ns_per_s, .attr = attr }));
            },
            .open => {
                if (self.no_open) {
                    _ = self.opens_enosys.fetchAdd(1, .monotonic);
                    return h.fail(.NOSYS);
                }
                if (self.attrOf(ino) == null) return h.fail(.NOENT);
                // DIRECT_IO: every read and write reaches the daemon. A
                // mainline zero-copy session's opens carry the per-file grant.
                const zc_flag: u32 = if (self.caps != null and self.caps.?.zc_open) abi.FOPEN_IO_URING_ZERO_COPY else 0;
                h.reply(codec.replyOpen(&out, u, ino, abi.FOPEN_DIRECT_IO | zc_flag));
            },
            .opendir => h.reply(codec.replyOpen(&out, u, ino, 0)),
            .release => {
                _ = self.file_releases.fetchAdd(1, .monotonic);
                h.reply(codec.replyOk(u));
            },
            .flush => {
                if (self.flush_enosys) {
                    _ = self.flushes_enosys.fetchAdd(1, .monotonic);
                    return h.fail(.NOSYS);
                }
                _ = self.flushes_dispatched.fetchAdd(1, .monotonic);
                h.reply(codec.replyOk(u));
            },
            .releasedir, .fsync, .fsyncdir => h.reply(codec.replyOk(u)),
            .read => |in| {
                if (ino == ino_park) {
                    _ = self.parked_readers.fetchAdd(1, .monotonic);
                    self.park_gate.wait(self.io.?) catch {};
                    _ = self.parked_readers.fetchSub(1, .monotonic);
                }
                // The fd-source leg: `/big` from the fill pool, zero copies;
                // the body view into the pool mapping is the keepalive and
                // the restage fallback.
                if (ino == ino_big and self.pool_fd >= 0 and h.slot == .ring) {
                    const n: usize = @intCast(@min(@as(u64, in.size), big_len -| in.offset));
                    _ = self.fd_body_replies.fetchAdd(1, .monotonic);
                    return h.replyFdBody(codec.replyData(u, self.pool_map[@intCast(in.offset)..][0..n]), self.pool_fd, in.offset);
                }
                // Serve into the window when there is one (the ring);
                // static bytes on the classical venue.
                if (h.window.len > 0) {
                    const n = self.readInto(ino, in.offset, in.size, h.window) orelse return h.fail(.NOENT);
                    return h.reply(codec.replyData(u, h.window[0..n]));
                }
                if (ino == ino_hello or ino == ino_park) {
                    return h.reply(codec.replyRead(u, if (in.offset >= hello_text.len) "" else hello_text[@intCast(in.offset)..], in.size));
                }
                h.fail(.IO);
            },
            .write => |w| {
                _ = self.writes_seen.fetchAdd(1, .monotonic);
                const l = lease orelse return h.fail(.INVAL);
                self.last_write_class.store(@backingInt(l.class), .release);
                if (ino != ino_scratch) {
                    l.release();
                    return h.fail(.ROFS);
                }
                if (l.held) return self.heldWrite(h, u, w, l, &out);
                const end = w.in.offset + w.in.size;
                if (end > scratch_cap) {
                    l.release();
                    return h.fail(.FBIG);
                }
                defer l.release();
                @memcpy(self.scratch[@intCast(w.in.offset)..][0..w.in.size], w.data);
                _ = self.scratch_len.fetchMax(end, .acq_rel);
                _ = self.write_bytes.fetchAdd(w.in.size, .monotonic);
                h.reply(codec.replyWrite(&out, u, w.in.size));
                if (self.hold_lease_across_reply) {
                    // Hold the lease until the worker's gate parked the
                    // commit (a bounded poll), so the Parked → Replied arm
                    // is exercised deterministically. The poll yields its
                    // fiber: on the merged arm this handler runs on the
                    // worker's own thread, whose pass must run to park the
                    // commit (decision 0003 §AB.3).
                    const deadline = clock.monotonicNs() + 2 * std.time.ns_per_s;
                    while (!l.state.isParked() and clock.monotonicNs() < deadline) self.io.?.sleep(.fromMicroseconds(50), .awake) catch {};
                    if (l.state.isParked()) _ = self.lease_waits_parked.fetchAdd(1, .monotonic);
                }
            },
            .readdir => |in| {
                if (ino != ino_root) return h.fail(.NOTDIR);
                var scratch: [4096]u8 = undefined;
                const buf: []u8 = if (h.window.len > 0) h.window else &scratch;
                var w = codec.DirentWriter.init(buf, in.size);
                var cookie: u64 = in.offset;
                while (cookie < names.len) : (cookie += 1) {
                    w.add(inos[@intCast(cookie)], cookie + 1, 0o100444, names[@intCast(cookie)]) catch break;
                }
                // A classical reply is written before `reply` returns, so
                // the stack scratch is a legal body there.
                h.reply(codec.replyData(u, w.bytes()));
            },
            .readdirplus => |in| {
                if (ino != ino_root) return h.fail(.NOTDIR);
                var scratch: [4096]u8 = undefined;
                const buf: []u8 = if (h.window.len > 0) h.window else &scratch;
                var w = codec.DirentPlusWriter.init(buf, in.size);
                var cookie: u64 = in.offset;
                while (cookie < names.len) : (cookie += 1) {
                    const e = self.entry(inos[@intCast(cookie)]).?;
                    w.add(e, cookie + 1, names[@intCast(cookie)]) catch {
                        // Not emitted: hand the lookup reference back.
                        fsForget(ctx, e.nodeid, 1);
                        break;
                    };
                }
                h.reply(codec.replyData(u, w.bytes()));
            },
            .statfs => h.reply(codec.replyStatfs(&out, u, .{ .blocks = 1024, .bfree = 512, .bavail = 512, .files = 8, .ffree = 2, .bsize = 4096, .namelen = 255, .frsize = 4096 })),
            .access => h.reply(codec.replyOk(u)),
            .setattr => |sa| {
                // `/scratch` honours an explicit mtime: what the VFS clamped
                // to our advertised TIME_LIMITS range arrives here.
                if (ino == ino_scratch and sa.valid & abi.FATTR_MTIME != 0) self.scratch_mtime.store(@bitCast(sa.mtime), .release);
                if (sa.valid & abi.FATTR_SIZE != 0) {
                    _ = self.size_sets.fetchAdd(1, .monotonic);
                    if (ino == ino_scratch) self.scratch_len.store(@min(sa.size, scratch_cap), .release);
                }
                const attr = self.attrOf(ino) orelse return h.fail(.NOENT);
                h.reply(codec.replyAttr(&out, u, .{ .attr_ttl_ns = std.time.ns_per_s, .attr = attr }));
            },
            .forget, .batch_forget, .interrupt, .notify_reply => {},
            .unsupported => h.reply(codec.replyEnosys(u)),
            else => h.reply(codec.replyEnosys(u)),
        }
    }

    /// A lookup parked until `park_gate` opens, then the mapping law on its
    /// own request: `msync` answers ENOMEM over a range with an unmapped
    /// page, so the name is judged before a byte of it is read.
    fn parkedLookup(self: *Self, h: Handle, u: u64, name: []const u8, out: *codec.OutBuf) void {
        _ = self.parked_lookups.fetchAdd(1, .monotonic);
        self.park_gate.wait(self.io.?) catch {};
        _ = self.parked_lookups.fetchSub(1, .monotonic);
        const page = std.heap.pageSize();
        const lo = std.mem.alignBackward(usize, @intFromPtr(name.ptr), page);
        const hi = std.mem.alignForward(usize, @intFromPtr(name.ptr) + name.len, page);
        if (linux.errno(linux.msync(@ptrFromInt(lo), hi - lo, linux.MSF.ASYNC)) != .SUCCESS) {
            _ = self.names_unmapped_after_park.fetchAdd(1, .monotonic);
            return h.fail(.IO);
        }
        if (parksLookup(name)) _ = self.names_intact_after_park.fetchAdd(1, .monotonic);
        h.reply(codec.replyEntry(out, u, codec.EntryReply.negative(std.time.ns_per_s)));
    }

    /// Release every parked `/park` reader.
    pub fn releaseParked(self: *Self) void {
        self.park_gate.set(self.io.?);
    }

    /// Let every retaining WRITE handler extract and release.
    pub fn openRetainGate(self: *Self) void {
        self.retain_gate.set(self.io.?);
    }
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

const RecordingTx = struct {
    replies: u32 = 0,
    fails: u32 = 0,
    prefilled: u32 = 0,
    fd_bodies: u32 = 0,
    retains: u32 = 0,
    releases: u32 = 0,
    last_errno: linux.E = .SUCCESS,
    last_len: u32 = 0,
    last_body: []const u8 = &.{},

    fn submit(ctx: *anyopaque, _: Handle, r: codec.Reply) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.replies += 1;
        self.last_len = r.header.len;
        self.last_body = r.body;
    }
    fn submitFdBody(ctx: *anyopaque, _: Handle, _: codec.Reply, _: linux.fd_t, _: u64) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.fd_bodies += 1;
    }
    fn submitPrefilled(ctx: *anyopaque, _: Handle, n: u32) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.prefilled += 1;
        self.last_len = n;
    }
    fn fail(ctx: *anyopaque, _: Handle, errno: linux.E) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.fails += 1;
        self.last_errno = errno;
    }
    fn submitRetain(ctx: *anyopaque, _: Handle, r: codec.Reply) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.retains += 1;
        self.last_len = r.header.len;
    }
    fn releaseRetained(ctx: *anyopaque, _: Handle) void {
        const self: *RecordingTx = @ptrCast(@alignCast(ctx));
        self.releases += 1;
    }
    fn tx(self: *RecordingTx) Tx {
        return .{ .ctx = self, .submit = submit, .submit_fd_body = submitFdBody, .submit_prefilled = submitPrefilled, .fail = fail, .submit_retain = submitRetain, .release_retained = releaseRetained };
    }
};

fn requestOf(op: codec.Request, unique: u64, nodeid: u64) Request {
    return .{ .header = .{ .len = 40, .opcode = 0, .unique = unique, .nodeid = nodeid, .uid = 0, .gid = 0, .pid = 1, .total_extlen = 0, .padding = 0 }, .op = op };
}

test "the handle routes each reply kind through the Tx face; failWith maps a typed error through the ONE table (EAGAIN for backpressure, EIO for a dead ring)" {
    var rec: RecordingTx = .{};
    const t = rec.tx();
    const h: Handle = .{ .tx = &t, .slot = .classical, .unique = 8, .window = &.{} };
    h.reply(codec.replyOk(8));
    h.replyPrefilled(77);
    h.replyFdBody(codec.replyData(8, "x"), 3, 0);
    h.replyRetain(codec.replyOk(8));
    h.releaseRetained();
    h.fail(.NOENT);
    h.failWith(error.QueueFull);
    try testing.expectEqual(@as(u32, 1), rec.replies);
    try testing.expectEqual(@as(u32, 1), rec.prefilled);
    try testing.expectEqual(@as(u32, 1), rec.fd_bodies);
    try testing.expectEqual(@as(u32, 1), rec.retains);
    try testing.expectEqual(@as(u32, 1), rec.releases);
    try testing.expectEqual(@as(u32, 2), rec.fails);
    try testing.expectEqual(linux.E.AGAIN, rec.last_errno);
    h.failWith(error.RingFailed);
    try testing.expectEqual(linux.E.IO, rec.last_errno);
    h.failWith(error.FileNotFound);
    try testing.expectEqual(linux.E.NOENT, rec.last_errno);
    try testing.expectEqual(@as(u32, 4), rec.fails);
}

test "EchoFs: lookup/getattr/read on the fixed tree answer exactly once each; a miss is a negative entry; the probe serves hello and big into the window and declines park; forget returns lookups" {
    var fs: EchoFs = .{};
    var rec: RecordingTx = .{};
    const t = rec.tx();
    var window: [4096]u8 = undefined;
    const f = fs.filesystem();
    f.dispatch(requestOf(.{ .lookup = .{ .name = "hello" } }, 2, EchoFs.ino_root), .{ .tx = &t, .slot = .classical, .unique = 2, .window = &.{} }, null);
    try testing.expectEqual(@as(u32, 1), rec.replies);
    try testing.expectEqual(@as(u32, 16 + 128), rec.last_len);
    try testing.expectEqual(@as(u64, 1), fs.lookups[EchoFs.ino_hello].load(.monotonic));
    f.dispatch(requestOf(.{ .lookup = .{ .name = "nope" } }, 4, EchoFs.ino_root), .{ .tx = &t, .slot = .classical, .unique = 4, .window = &.{} }, null);
    try testing.expectEqual(@as(u32, 2), rec.replies);
    f.dispatch(requestOf(.{ .getattr = .{ .flags = 0, .fh = null } }, 6, EchoFs.ino_big), .{ .tx = &t, .slot = .classical, .unique = 6, .window = &.{} }, null);
    try testing.expectEqual(@as(u32, 3), rec.replies);
    try testing.expectEqual(@as(u32, 16 + 104), rec.last_len);
    f.dispatch(requestOf(.{ .getattr = .{ .flags = 0, .fh = null } }, 8, 99), .{ .tx = &t, .slot = .classical, .unique = 8, .window = &.{} }, null);
    try testing.expectEqual(@as(u32, 1), rec.fails);
    try testing.expectEqual(linux.E.NOENT, rec.last_errno);
    // READ through the lane arm into a window.
    const rd: abi.fuse_read_in = .{ .fh = 2, .offset = 6, .size = 100, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    f.dispatch(requestOf(.{ .read = rd }, 10, EchoFs.ino_hello), .{ .tx = &t, .slot = .{ .ring = .{ .qid = 0, .ent_idx = 0, .commit_id = 10 } }, .unique = 10, .window = &window }, null);
    try testing.expectEqualSlices(u8, "world\n", rec.last_body);
    try testing.expectEqual(@as([*]const u8, &window), rec.last_body.ptr);
    // The probe (arm a).
    const served = f.probeRead(requestOf(.{ .read = .{ .fh = 3, .offset = 4096, .size = 16, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 } }, 12, EchoFs.ino_big), &window).?.reply;
    try testing.expectEqual(@as(usize, 16), served.body.len);
    try testing.expectEqual(EchoFs.bigByte(4096), served.body[0]);
    try testing.expectEqual(EchoFs.bigByte(4111), served.body[15]);
    try testing.expect(f.probeRead(requestOf(.{ .read = rd }, 14, EchoFs.ino_park), &window) == null);
    fs.probe_declines = true;
    try testing.expect(f.probeRead(requestOf(.{ .read = rd }, 16, EchoFs.ino_hello), &window) == null);
    // FORGET returns the reference.
    f.forget(EchoFs.ino_hello, 1);
    try testing.expectEqual(@as(u64, 0), fs.lookups[EchoFs.ino_hello].load(.monotonic));
    try testing.expectEqual(@as(u64, 1), fs.forgets.load(.monotonic));
}

test "EchoFs: a WRITE to scratch consumes the lease exactly once and lands the bytes; a WRITE elsewhere is EROFS and still releases" {
    var fs: EchoFs = .{};
    var rec: RecordingTx = .{};
    const t = rec.tx();
    var state: slot.LeaseState = .{};
    const NoWake = struct {
        fn wake(_: ?*anyopaque) bool {
            return false;
        }
    };
    const waker: slot.Waker = .{ .ctx = null, .wake = NoWake.wake };
    const data = "scratch bytes";
    const win: abi.fuse_write_in = .{ .fh = 5, .offset = 3, .size = data.len, .write_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    const lease = slot.PayloadLease.acquire(&state, data, waker);
    fs.filesystem().dispatch(requestOf(.{ .write = .{ .in = win, .data = data } }, 20, EchoFs.ino_scratch), .{ .tx = &t, .slot = .classical, .unique = 20, .window = &.{} }, lease);
    try testing.expectEqual(@as(u32, 1), rec.replies);
    try testing.expect(!state.leased());
    try testing.expectEqualSlices(u8, data, fs.scratch[3..][0..data.len]);
    try testing.expectEqual(@as(u64, 3 + data.len), fs.scratch_len.load(.acquire));
    const lease2 = slot.PayloadLease.acquire(&state, data, waker);
    fs.filesystem().dispatch(requestOf(.{ .write = .{ .in = win, .data = data } }, 22, EchoFs.ino_hello), .{ .tx = &t, .slot = .classical, .unique = 22, .window = &.{} }, lease2);
    try testing.expectEqual(linux.E.ROFS, rec.last_errno);
    try testing.expect(!state.leased());
}

test "EchoFs: the hold gate is a pure predicate (scratch only, and only when declared); the direct leg is null; a HELD write is extracted lazily through its face and lands; SETATTR stores an explicit mtime" {
    var fs: EchoFs = .{};
    fs.io = testing.io;
    const f = fs.filesystem();
    const hint: HoldHint = .{ .ino = EchoFs.ino_scratch, .offset = 0, .len = 4096, .class = .page_cache };
    try testing.expect(!f.writeHoldEligible(hint));
    fs.hold_eligible = true;
    try testing.expect(f.writeHoldEligible(hint));
    try testing.expect(!f.writeHoldEligible(.{ .ino = EchoFs.ino_hello, .offset = 0, .len = 4096, .class = .page_cache }));
    try testing.expect(f.directLeg(requestOf(.{ .read = .{ .fh = 3, .offset = 0, .size = 4096, .read_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 } }, 1, EchoFs.ino_big)) == null);
    // A held write: the face answers the slot bytes; the handler extracts
    // lazily, releases, replies once.
    const Face = struct {
        calls: u32 = 0,
        bytes: [4096]u8 = @splat(0x5C),
        fn extract(ctx: ?*anyopaque, _: u32, _: std.Io) slot.ExtractError![]const u8 {
            const self: *@This() = @ptrCast(@alignCast(ctx.?));
            self.calls += 1;
            return &self.bytes;
        }
    };
    var face: Face = .{};
    var rec: RecordingTx = .{};
    const t = rec.tx();
    var state: slot.LeaseState = .{};
    const NoWake = struct {
        fn wake(_: ?*anyopaque) bool {
            return false;
        }
    };
    const waker: slot.Waker = .{ .ctx = null, .wake = NoWake.wake };
    const win: abi.fuse_write_in = .{ .fh = 5, .offset = 4096, .size = 4096, .write_flags = 0, .lock_owner = 0, .flags = 0, .padding = 0 };
    const held = slot.PayloadLease.acquireHeld(&state, waker, 4096, .page_cache, 0, 1, 1, .{ .ctx = &face, .extract = Face.extract });
    f.dispatch(requestOf(.{ .write = .{ .in = win, .data = &.{} } }, 30, EchoFs.ino_scratch), .{ .tx = &t, .slot = .{ .ring = .{ .qid = 0, .ent_idx = 1, .commit_id = 30 } }, .unique = 30, .window = &.{} }, held);
    try testing.expectEqual(@as(u32, 1), rec.replies);
    try testing.expectEqual(@as(u32, 1), face.calls);
    try testing.expect(!state.leased());
    try testing.expectEqual(@as(u64, 1), fs.held_writes.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), fs.lazy_extracts.load(.monotonic));
    try testing.expectEqual(@as(u8, 0x5C), fs.scratch[4096]);
    try testing.expectEqual(@as(u64, 8192), fs.scratch_len.load(.acquire));
    // SETATTR with FATTR_MTIME lands in the attr; without it nothing moves.
    var sa = abi.zeroed(abi.fuse_setattr_in);
    sa.valid = abi.FATTR_MTIME;
    sa.mtime = 1_800_000_000;
    f.dispatch(requestOf(.{ .setattr = sa }, 32, EchoFs.ino_scratch), .{ .tx = &t, .slot = .classical, .unique = 32, .window = &.{} }, null);
    try testing.expectEqual(@as(i64, 1_800_000_000), fs.scratch_mtime.load(.acquire));
    try testing.expectEqual(@as(u64, 1_800_000_000), fs.attrOf(EchoFs.ino_scratch).?.mtime);
    try testing.expectEqual(@as(u64, 1_700_000_000), fs.attrOf(EchoFs.ino_hello).?.mtime);
}
