//! The per-entry slot state machine, the payload lease + commit gate, the
//! reply application into an ent and the REGISTER backoff ladder (charter
//! §4 "Per-entry slot state machine", "Payload lease + commit gate";
//! `docs/porting/fuse_uring_transport.md` §4, §6, §2.3, §4.5). Portable: a
//! value type the worker thread owns; nothing here touches a ring.
//!
//! THE LAW (charter §4): for every request delivered on a slot exactly one
//! COMMIT_AND_FETCH carrying a reply or a synthesized error is submitted
//! before the slot leaves Delivered, and it leaves that state only by that
//! submission. A lost reply is an application in uninterruptible sleep and
//! `umount` EBUSY. The law is STRUCTURAL here: the only exits from
//! `delivered`/`parked` are `onCommitSubmitted` (a reply the gate admitted),
//! `failEnt` (a synthesized header-only `-errno`) and `abandon` (the
//! must-stay-0 tripwire: even the synthesized push failed). A stale or
//! foreign commit id is refused loud, never applied over the owed request;
//! an oversize reply commits header-only `-EIO`, never a truncated body
//! under a header claiming the full length.
//!
//! Ownership: one `SlotTable` per member queue, every transition on its
//! worker thread. Other threads address a slot BY VALUE —
//! `SlotRef{qid, ent_idx, commit_id}` rides the request (the historical
//! `unique → slot` map is deleted, not wrapped). The only cross-thread words
//! are the lease gate (`LeaseState`) and the two per-ent atomics the worker
//! publishes for foreign readers (`owed_since_ns` for the watchdog,
//! `owed_commit_id` for the submit-side economy) — the owed COUNT itself is
//! derived from the slot states (`owedCount`), never a shadow ledger
//! (decision 0002 ledger: Q10).
//!
//! The lease gate's two SeqCst points: Zig 0.17 has no standalone fence
//! builtin (`@fence` left the language in 0.14) and a seq_cst RMW on an
//! unrelated word is NOT a fence on ARMv8 (`ldaxr/stlxr` orders only its
//! own location). So the four accesses of the Dekker pair are themselves
//! SeqCst: C11's single total order over all seq_cst operations forbids
//! both sides reading the pre-race value (the standard's own Dekker
//! example; on ARMv8 they compile to `stlr`/`ldar`, whose RCsc rule forbids
//! the `ldar` passing the earlier `stlr`; on x86 the SC store is a locked
//! `xchg`). `slot_model.zig` keeps the fence as an explicit store-buffer
//! drain step — an SC store IS a drained store — and proves the weakening
//! (release/acquire alone: the drain deleted) hangs.

const std = @import("std");
const linux = std.os.linux;
const Io = std.Io;
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const zc = @import("zc.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// A refused REGISTER backs off 1 ms doubling to 128 ms for 8 attempts,
/// then the ent retires: a wedged ent must not spin a core, and must not
/// take minutes to recover (charter §4). Sequence 1, 2, 4, 8, 16, 32, 64 ms,
/// retirement on the 8th.
pub const REGISTER_RETRY_MAX: u8 = 8;
pub const REGISTER_BACKOFF_BASE_NS: u64 = 1 * std.time.ns_per_ms;
pub const REGISTER_BACKOFF_MAX_NS: u64 = 128 * std.time.ns_per_ms;

/// A lease held ≥ 1 s is counted and logged, NEVER a panic: the reference's
/// `debug_assert!(age < 1 s)` panicked a write handler mid-flight, so the
/// reply was never sent (the kernel's writeback folio parked forever,
/// `fsync(2)` in D-state, `waiting=28` captured live) and the unwind
/// skipped the release, parking the ent's re-arm forever. ≥ 1 s is not
/// proof of escape: admission backpressure legitimately exceeds it on slow
/// substrates; a genuine escape shows as unbounded ages plus a growing
/// `transport_parked_commits` gap.
pub const LEASE_OVERLONG_NS: u64 = 1 * std.time.ns_per_s;

// ---------------------------------------------------------------------------
// Counters (the stats-inode keys are the field docs).
// ---------------------------------------------------------------------------

/// `transport_requests_abandoned` (must-stay-0): a delivered request left
/// its slot with no commit.
pub var requests_abandoned: counters.Tripwire = .init("transport_requests_abandoned");
/// `transport_replies_oversize` (must-stay-0): `out + body` exceeded the
/// ent payload; a header-only `-EIO` was committed instead of a truncation.
pub var replies_oversize: counters.Tripwire = .init("transport_replies_oversize");
/// `transport_replies_refused_stale`: double / late / foreign-cid replies refused.
pub var replies_refused_stale: counters.ShardedCounter = .{};
/// `transport_requests_failed_synthetic`: header-only errors the transport
/// synthesized (CQE reclaim, failed dispatch, teardown) — honest, not a bug
/// by itself.
pub var requests_failed_synthetic: counters.ShardedCounter = .{};
/// `transport_ents_retired`: ents retired after 8 consecutive REGISTER failures.
pub var ents_retired: counters.ShardedCounter = .{};
/// `transport_register_refusals`: REGISTER pushes the kernel (or the test
/// seam) refused; each enters the backoff ladder.
pub var register_refusals: counters.ShardedCounter = .{};
/// `transport_parked_commits` / `transport_unparked_commits`: commits parked
/// behind a live lease / later committed. `parked − unparked` at quiesce is
/// the wedge count (the shutdown header-only fallback deliberately counts
/// no unpark, so a genuine escape stays visible as an unclosed ledger).
pub var parked_commits: counters.ShardedCounter = .{};
pub var unparked_commits: counters.ShardedCounter = .{};
/// `transport_payload_leases`: FUSE_WRITE payloads delivered as leases.
pub var payload_leases: counters.ShardedCounter = .{};
/// `transport_dest_dma_leases`: read-destination owner tokens claimed.
pub var dest_dma_leases: counters.ShardedCounter = .{};
/// `transport_leases_outstanding`: live leases; 0 at quiesce.
pub var leases_outstanding: counters.Gauge = .{};
/// `transport_lease_max_age_ms`: the high-water lease age.
pub var lease_max_age_ms: std.atomic.Value(u64) = .init(0);
/// `transport_lease_overlong`: leases ≥ 1 s (loud, never fatal).
pub var lease_overlong: counters.ShardedCounter = .{};
/// `transport_requests_displaced`: a second delivery landed on an owing
/// slot (`fuse_resend`'s double delivery); counted beside the abandonment
/// it implies.
pub var requests_displaced: counters.ShardedCounter = .{};
/// `transport_zc_retain_commits` / `transport_zc_releases` /
/// `transport_zc_retained_outstanding`: RETAIN commits submitted (the
/// ACK-early engagement counter), RELEASE_PAYLOAD SQEs submitted, and their
/// difference (0 at quiesce — the closure law).
pub var retain_commits: counters.ShardedCounter = .{};
pub var releases: counters.ShardedCounter = .{};
pub var retained_outstanding: counters.Gauge = .{};
/// `transport_zc_release_failures` (must-stay-0): a RELEASE CQE other than
/// 0 — ENOENT a double release, EBUSY a release that raced a live commit.
pub var release_failures: counters.Tripwire = .init("transport_zc_release_failures");
/// `transport_zc_retain_refused` (must-stay-0): the kernel answered a RETAIN
/// commit -EINVAL; the slot re-committed plain (the submit site mirrors the
/// kernel's checks, so this never fires).
pub var retain_refused: counters.Tripwire = .init("transport_zc_retain_refused");
/// `transport_zc_retain_refused_class`: a `replyRetain` the submit site
/// refused — a `.direct_gup` payload (the stability law: snapshot first),
/// a slot whose pages are not a held WRITE, or a session without retention
/// — committed plain instead.
pub var retain_refused_class: counters.ShardedCounter = .{};
/// `transport_zc_direction_refusals`: a reply body aimed at a WRITE
/// (`ITER_SOURCE`) slot — a `READ_FIXED` into it would be -EFAULT — refused
/// header-only -EIO.
pub var zc_direction_refusals: counters.ShardedCounter = .{};
/// `transport_zc_write_extractions` / `_extract_bytes`: completed slot →
/// bounce extractions (at-delivery + lazy) and their bytes; a WRITE row
/// whose op count these plus the held direct stores do not account is
/// invalid.
pub var zc_write_extractions: counters.ShardedCounter = .{};
pub var zc_write_extract_bytes: counters.ShardedCounter = .{};
/// `transport_zc_write_lazy_extractions` / `_lazy_extract_bytes`: the LATE
/// split — a held payload the handler extracted after dispatch (the gate's
/// snapshot was stale, or the arm it was held for declined under its
/// guard: a W1 patch refused, an overlay store declined to accumulation —
/// the one extraction a hold can still pay, one wake later than at
/// delivery; ≈ 0 of the holds).
pub var zc_write_lazy_extractions: counters.ShardedCounter = .{};
pub var zc_write_lazy_extract_bytes: counters.ShardedCounter = .{};
/// `transport_zc_write_holds` / `_hold_bytes`: WRITE payloads HELD in the
/// slot (the filesystem's gate admitted the W1 or overlay class) and their
/// bytes — the extraction at delivery they did not pay; `hold_bytes −
/// lazy_extract_bytes` is what moved to the device with zero daemon copies.
pub var zc_write_holds: counters.ShardedCounter = .{};
pub var zc_write_hold_bytes: counters.ShardedCounter = .{};
/// `transport_zc_write_hold_ns`: a held slot's life, delivery → the lease's
/// release (the handler's admission, the DMA it waited for, or its lazy
/// extraction) — how long the app's pages stay pinned in the slot.
pub var zc_write_hold_ns: counters.ShardedLatencyHistogram = .{};

// ---------------------------------------------------------------------------
// Addressing
// ---------------------------------------------------------------------------

/// A ring slot by value (charter §4): what a request carries and a reply
/// names. `commit_id` is the kernel's (`ring_ent_in_out.commit_id` = the
/// request's `unique` at delivery).
pub const SlotRef = struct {
    qid: u16,
    ent_idx: u16,
    commit_id: u64,
};

// ---------------------------------------------------------------------------
// The payload lease + commit gate (porting spec §6; model `slot_model.zig`)
// ---------------------------------------------------------------------------

pub const Gate = enum { ready, parked };

/// One per ent. `refs` counts live leases over the ent's payload buffer
/// (0/1 for a WRITE payload lease; N for read-destination DMA tokens);
/// `parked` says the worker has a commit message parked waiting on
/// `refs == 0`.
pub const LeaseState = struct {
    refs: std.atomic.Value(u32) = .init(0),
    parked: std.atomic.Value(bool) = .init(false),

    /// Delivery-time acquisition (worker only). Returns the previous count;
    /// the caller asserts it was 0: an ent is only re-armed after a commit
    /// the gate proved lease-free.
    pub fn acquire(self: *LeaseState) u32 {
        // acq_rel pairs with `release`'s seq_cst fetchSub (a later lease sees
        // the earlier one's accesses) and with `tryUnpark`'s acquire load.
        return self.refs.fetchAdd(1, .acq_rel);
    }

    /// DMA-destination token (any thread): refs may already be non-zero —
    /// one token per in-flight SQE. Sound by program order: every claim
    /// precedes its request's reply (mint → claim → submit → await), so a
    /// commit that observed `refs == 0` proves no claimed SQE can still
    /// write the buffer.
    pub fn acquireDest(self: *LeaseState) void {
        _ = self.refs.fetchAdd(1, .acq_rel);
    }

    /// Lease drop (any thread). True ⇒ this drop released the LAST lease
    /// and a commit is parked: the caller MUST wake the worker (through the
    /// coalescer). A spurious wake is harmless; a missed wake is a hang.
    pub fn release(self: *LeaseState) bool {
        // The Dekker pair's releaser half: `refs = 0` then `r1 = parked`,
        // both SeqCst (module doc: the SC total order is the fence). Pairs
        // with `tryCommit`'s `parked = true; r2 = refs`.
        if (self.refs.fetchSub(1, .seq_cst) == 1) {
            return self.parked.load(.seq_cst);
        }
        return false;
    }

    /// The worker's commit gate. `ready` ⇒ refs was observed 0 and `parked`
    /// is clear: the reply may be applied (payload writes included) and the
    /// COMMIT_AND_FETCH pushed. `parked` ⇒ a lease is live; the caller parks
    /// the message and relies on `release`'s wake. Publish-then-recheck:
    /// `parked` is stored BEFORE the second refs load, so a drop that raced
    /// the first load either observes `parked == true` (and wakes) or its
    /// `refs = 0` is observed by the re-check (and the commit proceeds now).
    pub fn tryCommit(self: *LeaseState) Gate {
        // acquire pairs with `release`'s fetchSub: a `ready` verdict sees
        // every access the dropped lease made before the payload is written.
        if (self.refs.load(.acquire) == 0) return .ready;
        // The Dekker pair's worker half (SeqCst store then SeqCst load; the
        // SC total order is the fence — pairs with `release`).
        self.parked.store(true, .seq_cst);
        if (self.refs.load(.seq_cst) == 0) {
            // The last lease dropped between the first load and the park:
            // un-publish and commit now (its wake, if any, is spurious).
            self.parked.store(false, .release);
            return .ready;
        }
        return .parked;
    }

    /// The parked-scan / shutdown-drain probe (worker only): when the lease
    /// is gone, clears `parked` and returns true — the caller applies and
    /// commits the parked message. The acquire pairs with `release`'s
    /// fetchSub so the payload write that follows happens-after every access
    /// the dropped lease made; once a probe observes `refs == 0` no new
    /// lease can appear (delivery requires a prior commit).
    pub fn tryUnpark(self: *LeaseState) bool {
        if (self.refs.load(.acquire) == 0) {
            self.parked.store(false, .monotonic);
            return true;
        }
        return false;
    }

    pub fn leased(self: *const LeaseState) bool {
        return self.refs.load(.acquire) != 0;
    }

    pub fn isParked(self: *const LeaseState) bool {
        return self.parked.load(.acquire);
    }
};

/// How a lease drop reaches the worker: the group's coalescer-gated eventfd
/// write (`ring_park.Wake.wake` behind a portable face). Returns true when
/// the write was paid, false when elided.
pub const Waker = struct {
    ctx: ?*anyopaque,
    wake: *const fn (ctx: ?*anyopaque) bool,

    pub fn call(self: Waker) bool {
        return self.wake(self.ctx);
    }
};

/// `transport_wake_writes` / `transport_wakes_elided`: eventfd writes paid
/// vs elided by the group coalescer (replies, lease drops, bridge messages).
/// `writes / (writes + elided) ≈ 1` under saturation is the pre-coalescer
/// 1.67-writes-per-op regression.
pub var wake_writes: counters.ShardedCounter = .{};
pub var wakes_elided: counters.ShardedCounter = .{};

/// `lease_release_and_wake`: the ONE wake law shared by the payload lease
/// and the dest-DMA token. Publish (the fetchSub inside `release`) BEFORE
/// `arm` (the coalescer's producer law).
pub fn releaseAndWake(state: *LeaseState, waker: Waker) void {
    if (state.release()) {
        if (waker.call()) wake_writes.inc() else wakes_elided.inc();
    }
}

/// The HELD slot's face (zero-copy): the pages sit in the registering
/// ring's fixed-buffer slot, so only a bridge on THAT ring can move them.
/// `extract` posts `WRITE_FIXED(bounce ← slot)` to the worker and parks the
/// caller (an `Io` futex: a fiber parks, never its lane thread) until the
/// CQE, then answers the bounce view; memoized per request — a second call
/// pays no second bridge.
pub const HeldFace = struct {
    ctx: ?*anyopaque,
    extract: *const fn (ctx: ?*anyopaque, gent: u32, io: Io) ExtractError![]const u8,
};

pub const ExtractError = error{ BridgeFailed, SlotNotHeld, SessionClosed };

/// The slot-source coordinates a rung-4 `WRITE_FIXED(device fd ← slot)`
/// names: the fixed-buffer index on the registering queue's ring, the
/// held length, and `addr = 0` (the slot's byte offset — `imu->ubuf == 0`).
pub const HeldSource = struct { qid: u16, buf_index: u16, len: u32 };

/// A FUSE_WRITE payload lease (refs 0 → 1 at delivery) over the ent's
/// payload buffer, handed to the handler by value and released exactly once.
/// A lease never escapes the handler; a leased buffer is never re-posted or
/// written (the gate parks the reply; shutdown commits header-only).
///
/// Under zero-copy a W1-eligible payload is HELD (`held`): `data` is empty,
/// the bytes sit in the slot; the handler consumes them by `heldSource()`
/// (a direct `WRITE_FIXED(device ← slot)`) or `extract()` (lazily, counted
/// `transport_zc_write_lazy_extractions`). `class` is the stability law's
/// stamp: only `.page_cache` may `replyRetain`.
pub const PayloadLease = struct {
    state: *LeaseState,
    data: []const u8,
    born_ns: u64,
    waker: Waker,
    held: bool = false,
    held_len: u32 = 0,
    class: zc.PayloadClass = .page_cache,
    qid: u16 = 0,
    ent_idx: u16 = 0,
    gent: u32 = 0,
    face: ?HeldFace = null,

    /// Worker side at delivery: `refs` must have been 0.
    pub fn acquire(state: *LeaseState, data: []const u8, waker: Waker) PayloadLease {
        const prev = state.acquire();
        std.debug.assert(prev == 0);
        payload_leases.inc();
        leases_outstanding.add(1);
        return .{ .state = state, .data = data, .born_ns = clock.monotonicNs(), .waker = waker };
    }

    /// Worker side at a HELD delivery (zero-copy, the gate admitted the
    /// shape): no bytes in hand, the slot coordinates and the bridge face
    /// instead.
    pub fn acquireHeld(state: *LeaseState, waker: Waker, held_len: u32, class: zc.PayloadClass, qid: u16, ent_idx: u16, gent: u32, face: HeldFace) PayloadLease {
        var l = acquire(state, &.{}, waker);
        l.held = true;
        l.held_len = held_len;
        l.class = class;
        l.qid = qid;
        l.ent_idx = ent_idx;
        l.gent = gent;
        l.face = face;
        zc_write_holds.inc();
        zc_write_hold_bytes.add(held_len);
        return l;
    }

    /// The direct leg's coordinates, or null when the payload is in hand.
    pub fn heldSource(self: PayloadLease) ?HeldSource {
        if (!self.held) return null;
        return .{ .qid = self.qid, .buf_index = self.ent_idx, .len = self.held_len };
    }

    /// The payload bytes INTO `dst`: a held slot is extracted first (one
    /// bridge, memoized), then copied; a payload in hand is copied. Returns
    /// the byte count. `dst.len ≥ len` is the caller's contract.
    pub fn extract(self: PayloadLease, io: Io, dst: []u8) ExtractError!usize {
        const src = try self.bytes(io);
        std.debug.assert(dst.len >= src.len);
        if (dst.ptr != src.ptr) @memcpy(dst[0..src.len], src);
        return src.len;
    }

    /// The payload as a view: `data` when in hand, else the extracted
    /// bounce view (valid until the reply is submitted).
    pub fn bytes(self: PayloadLease, io: Io) ExtractError![]const u8 {
        if (!self.held) return self.data;
        const face = self.face orelse return error.SlotNotHeld;
        return face.extract(face.ctx, self.gent, io);
    }

    /// The drop (any thread): the 1 s rule, the gauges, then the wake law.
    pub fn release(self: PayloadLease) void {
        const age_ns = clock.monotonicNs() -| self.born_ns;
        const age_ms = age_ns / std.time.ns_per_ms;
        _ = lease_max_age_ms.fetchMax(age_ms, .monotonic);
        if (self.held) zc_write_hold_ns.record(age_ns);
        leases_outstanding.sub(1);
        if (age_ns >= LEASE_OVERLONG_NS) {
            lease_overlong.inc();
            log.err("transport: a FUSE_WRITE payload lease lived {d} ms (≥ 1 s) — counted transport_lease_overlong, never a panic; a genuine escape shows as unbounded ages plus a growing parked-commit gap", .{age_ms});
        }
        releaseAndWake(self.state, self.waker);
    }
};

// ---------------------------------------------------------------------------
// The state machine
// ---------------------------------------------------------------------------

pub const Owed = struct { unique: u64, commit_id: u64 };

pub const State = union(enum) {
    /// REGISTER submitted; the kernel owns the ent; no request out.
    registered,
    /// A request was handed out; EXACTLY ONE commit must follow.
    delivered: Owed,
    /// The admitted commit is parked behind a live payload lease; still owed.
    parked: Owed,
    /// COMMIT_AND_FETCH submitted; the kernel owns the ent again (its CQE is
    /// the next delivery).
    replied: u64,
    /// COMMIT with RETAIN submitted (zc ACK-early): the request ended
    /// kernel-side, the slot pages stay registered, the cmd is parked (no
    /// CQE); owes one RELEASE_PAYLOAD. Compiled, inert until retention lands.
    retained: u64,
    /// 8 consecutive REGISTER failures; out of rotation.
    retired,
};

pub const Admit = enum { accept, refuse_stale };
pub const Fail = union(enum) { synthesize: Owed, nothing };
pub const RegisterVerdict = union(enum) { retry: struct { after_ns: u64 }, retire };

pub const Ent = struct {
    state: State = .registered,
    lease: LeaseState = .{},
    /// What the kernel installed in this ent's zc slot at the current
    /// delivery (`.none` on a non-zc session).
    dir: zc.Dir = .none,
    /// The stability class of the current WRITE payload.
    class: zc.PayloadClass = .page_cache,
    /// The current WRITE payload is HELD in the slot (never extracted yet).
    held: bool = false,
    /// A RELEASE arrived before the RETAIN commit was pushed: fire it right
    /// after that commit, in the same batch (release never precedes commit).
    release_pending: bool = false,
    /// Consecutive REGISTER failures (reset on success).
    failures: u8 = 0,
    /// Monotonic deadline of the pending re-REGISTER; 0 = none.
    retry_at_ns: u64 = 0,
    /// Foreign-readable: when this ent started owing (0 = owes nothing).
    /// Written by the worker at deliver/commit; the watchdog folds the cells.
    owed_since_ns: std.atomic.Value(u64) = .init(0),
    /// Foreign-readable: the owed commit id (0 = none) — the submit side's
    /// economy check before it takes a cell; the worker's `admitCommit` is
    /// the authority.
    owed_commit_id: std.atomic.Value(u64) = .init(0),

    pub fn owing(self: *const Ent) ?Owed {
        return switch (self.state) {
            .delivered, .parked => |o| o,
            else => null,
        };
    }
};

/// The per-member-queue table. Owned by its worker thread.
pub const SlotTable = struct {
    qid: u16,
    ents: []Ent,

    pub fn init(gpa: std.mem.Allocator, qid: u16, depth: u32) std.mem.Allocator.Error!SlotTable {
        const ents = try gpa.alloc(Ent, depth);
        for (ents) |*e| e.* = .{};
        return .{ .qid = qid, .ents = ents };
    }

    pub fn deinit(self: *SlotTable, gpa: std.mem.Allocator) void {
        gpa.free(self.ents);
        self.* = undefined;
    }

    pub fn ent(self: *SlotTable, ent_idx: u16) *Ent {
        return &self.ents[ent_idx];
    }

    fn publishOwed(e: *Ent, o: ?Owed, now_ns: u64) void {
        // release pairs with a foreign reader's acquire: the state the
        // worker installed is visible when the word reads non-zero.
        e.owed_commit_id.store(if (o) |x| x.commit_id else 0, .release);
        e.owed_since_ns.store(if (o != null) now_ns else 0, .release);
    }

    // ---- transitions (porting spec §4.3) ----

    /// A REGISTER was pushed. From `delivered`/`parked` this is exactly row
    /// 5's loss (the caller must `failEnt` first): counted abandoned.
    pub fn onRegisterSubmitted(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        if (e.owing() != null) {
            requests_abandoned.fire("REGISTER pushed over an owing slot");
            publishOwed(e, null, 0);
        }
        e.state = .registered;
        e.retry_at_ns = 0;
    }

    /// A delivery CQE. From `delivered`/`parked` the previous request is
    /// DISPLACED (`fuse_resend`'s double delivery): abandoned + displaced,
    /// loud, and the new delivery is accepted.
    pub fn onDeliver(self: *SlotTable, ent_idx: u16, unique: u64, commit_id: u64, now_ns: u64) void {
        const e = self.ent(ent_idx);
        if (e.owing()) |prev| {
            requests_displaced.inc();
            requests_abandoned.fire("a second delivery displaced an owing request");
            log.err("transport: queue {d} ent {d}: delivery (unique {d}) displaced the owing request (unique {d}, commit {d})", .{ self.qid, ent_idx, unique, prev.unique, prev.commit_id });
        }
        e.state = .{ .delivered = .{ .unique = unique, .commit_id = commit_id } };
        e.failures = 0;
        e.retry_at_ns = 0;
        e.dir = .none;
        e.class = .page_cache;
        e.held = false;
        e.release_pending = false;
        publishOwed(e, .{ .unique = unique, .commit_id = commit_id }, now_ns);
    }

    /// `unique == 0 ∧ commit_id != 0`: the forced EIO commit is the
    /// machine's normal exit (an ent left in userspace is `waiting ≥ 1`).
    pub fn onDeliverDegenerate(self: *SlotTable, ent_idx: u16, commit_id: u64, now_ns: u64) void {
        self.onDeliver(ent_idx, 0, commit_id, now_ns);
    }

    /// A commit message arrived for `commit_id`: accepted iff the slot is
    /// `delivered` with that exact id. Everything else (parked, replied,
    /// retained, a foreign id) is refused — never an overwrite, never a
    /// second COMMIT.
    pub fn admitCommit(self: *SlotTable, ent_idx: u16, commit_id: u64) Admit {
        const e = self.ent(ent_idx);
        switch (e.state) {
            .delivered => |o| if (o.commit_id == commit_id) return .accept,
            else => {},
        }
        replies_refused_stale.inc();
        return .refuse_stale;
    }

    pub fn onCommitParked(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        switch (e.state) {
            .delivered => |o| {
                e.state = .{ .parked = o };
                parked_commits.inc();
            },
            else => unreachable, // the gate runs only on an admitted (delivered) commit
        }
    }

    /// The admitted reply's body is in flight through a zc bridge: the slot
    /// stays owed and refuses a second reply until the bridge CQE commits
    /// it (the same `parked` state; the lease ledger is not touched).
    pub fn onCommitBridged(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        switch (e.state) {
            .delivered => |o| e.state = .{ .parked = o },
            else => unreachable, // a bridge is pushed only for an admitted (delivered) commit
        }
    }

    /// The lease-parked commit resumes (the unpark scan proved the lease
    /// gone): the slot is `delivered` again for the commit path, which may
    /// bridge-park it once more.
    pub fn onUnparked(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        switch (e.state) {
            .parked => |o| e.state = .{ .delivered = o },
            else => unreachable, // only a parked slot unparks
        }
    }

    /// THE one exit from owing: a COMMIT_AND_FETCH was pushed.
    pub fn onCommitSubmitted(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        const o = e.owing() orelse unreachable; // every commit push follows an admitted or synthesized owed request
        e.state = .{ .replied = o.commit_id };
        publishOwed(e, null, 0);
    }

    /// COMMIT with RETAIN pushed (retention): the request ended kernel-side,
    /// the slot owes no reply but still owes a RELEASE — the commit id stays
    /// published (the release's economy check names it); `owed_since_ns`
    /// clears (the watchdog names replies, not retentions).
    pub fn onCommitSubmittedRetained(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        const o = e.owing() orelse unreachable;
        e.state = .{ .retained = o.commit_id };
        e.owed_commit_id.store(o.commit_id, .release);
        e.owed_since_ns.store(0, .release);
        retain_commits.inc();
        retained_outstanding.add(1);
    }

    /// A RELEASE for `commit_id`: accepted iff the slot is `retained` with
    /// that id (the kernel would answer ENOENT/EBUSY otherwise).
    pub fn admitRelease(self: *SlotTable, ent_idx: u16, commit_id: u64) Admit {
        const e = self.ent(ent_idx);
        switch (e.state) {
            .retained => |cid| if (cid == commit_id) return .accept,
            else => {},
        }
        return .refuse_stale;
    }

    /// RELEASE_PAYLOAD pushed; inert on any other state (a teardown race).
    pub fn onReleaseSubmitted(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        switch (e.state) {
            .retained => |cid| {
                e.state = .{ .replied = cid };
                e.release_pending = false;
                publishOwed(e, null, 0);
                releases.inc();
                retained_outstanding.sub(1);
            },
            else => {},
        }
    }

    /// A commit CQE `-EINVAL` while retained: recover by a plain re-commit
    /// (the ent still holds its applied reply). The must-stay-0 tripwire.
    pub fn onRetainRefused(self: *SlotTable, ent_idx: u16, now_ns: u64) void {
        const e = self.ent(ent_idx);
        switch (e.state) {
            .retained => |cid| {
                retain_refused.fire("kernel refused a RETAIN commit with -EINVAL");
                retained_outstanding.sub(1);
                e.release_pending = false;
                e.state = .{ .delivered = .{ .unique = 0, .commit_id = cid } };
                publishOwed(e, .{ .unique = 0, .commit_id = cid }, now_ns);
            },
            else => {},
        }
    }

    /// A commit CQE `EAGAIN`/`EINTR`: re-push the SAME commit, never
    /// re-REGISTER (that discarded the applied reply). Returns the commit id
    /// to re-push, or null when the slot was not `replied`.
    pub fn onCommitRetry(self: *SlotTable, ent_idx: u16, now_ns: u64) ?u64 {
        const e = self.ent(ent_idx);
        switch (e.state) {
            .replied => |cid| {
                e.state = .{ .delivered = .{ .unique = 0, .commit_id = cid } };
                publishOwed(e, .{ .unique = 0, .commit_id = cid }, now_ns);
                return cid;
            },
            else => return null,
        }
    }

    /// Every non-reply exit: an owing slot yields `synthesize` (the caller
    /// commits `errorHeader(unique, errno)` header-only — legal against a
    /// live lease because it touches only the separately held header — and
    /// then calls `onCommitSubmitted`); any other state yields `nothing`, so
    /// a second `failEnt` can never produce a second commit.
    pub fn failEnt(self: *SlotTable, ent_idx: u16) Fail {
        const e = self.ent(ent_idx);
        if (e.owing()) |o| {
            // A parked message is dropped with the synthesis: nothing waits
            // on the lease any more, so its drop need not wake anyone (a
            // racing drop that still reads true wakes spuriously: harmless).
            if (e.state == .parked) e.lease.parked.store(false, .monotonic);
            requests_failed_synthetic.inc();
            return .{ .synthesize = o };
        }
        return .nothing;
    }

    /// The commit push itself failed: the honest tripwire case.
    pub fn abandon(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        if (e.owing() != null) {
            requests_abandoned.fire("commit push failed; the request is lost");
            publishOwed(e, null, 0);
        }
        e.state = .registered;
    }

    /// A REGISTER was refused: `failures += 1`; the 8th retires the ent,
    /// otherwise a retry after `min(1 ms << (failures − 1), 128 ms)`.
    pub fn noteRegisterFailure(self: *SlotTable, ent_idx: u16, now_ns: u64) RegisterVerdict {
        const e = self.ent(ent_idx);
        register_refusals.inc();
        e.failures += 1;
        if (e.failures >= REGISTER_RETRY_MAX) {
            e.state = .retired;
            e.retry_at_ns = 0;
            ents_retired.inc();
            return .retire;
        }
        const shift: u6 = @intCast(e.failures - 1);
        const after_ns = @min(REGISTER_BACKOFF_BASE_NS << shift, REGISTER_BACKOFF_MAX_NS);
        e.retry_at_ns = now_ns + after_ns;
        return .{ .retry = .{ .after_ns = after_ns } };
    }

    /// A REGISTER delivered: an occasional EBUSY never accumulates.
    pub fn noteRegisterSuccess(self: *SlotTable, ent_idx: u16) void {
        const e = self.ent(ent_idx);
        e.failures = 0;
        e.retry_at_ns = 0;
    }

    // ---- queries ----

    /// `delivered ∪ parked` — the owed count derived from the states, THE
    /// authority for the park backstop (no shadow ledger).
    pub fn owedCount(self: *const SlotTable) u32 {
        var n: u32 = 0;
        for (self.ents) |*e| if (e.owing() != null) {
            n += 1;
        };
        return n;
    }

    pub fn anyOwing(self: *const SlotTable) bool {
        for (self.ents) |*e| if (e.owing() != null) return true;
        return false;
    }

    /// The owed count read from ANOTHER thread (the watchdog's worker
    /// census): the per-ent atomic economy word, never the state union.
    pub fn owedCountForeign(self: *const SlotTable) u32 {
        var n: u32 = 0;
        for (self.ents) |*e| if (e.owed_commit_id.load(.acquire) != 0) {
            n += 1;
        };
        return n;
    }

    pub fn allRetired(self: *const SlotTable) bool {
        for (self.ents) |*e| if (e.state != .retired) return false;
        return true;
    }

    /// The nearest pending re-REGISTER deadline (bounds the park), or null.
    pub fn nextRetryDeadline(self: *const SlotTable) ?u64 {
        var best: ?u64 = null;
        for (self.ents) |*e| if (e.retry_at_ns != 0) {
            best = if (best) |b| @min(b, e.retry_at_ns) else e.retry_at_ns;
        };
        return best;
    }

    /// Iterate the ents whose retry deadline passed, consuming each
    /// deadline (the caller pushes the REGISTER).
    pub fn nextRetryDue(self: *SlotTable, now_ns: u64) ?u16 {
        for (self.ents, 0..) |*e, i| {
            if (e.retry_at_ns != 0 and e.retry_at_ns <= now_ns) {
                e.retry_at_ns = 0;
                return @intCast(i);
            }
        }
        return null;
    }
};

// ---------------------------------------------------------------------------
// Reply application into an ent (porting spec §4.5)
// ---------------------------------------------------------------------------

/// Place a reply in the ent: `in_out[0..16] = fuse_out_header`, the
/// out-struct then the body in the payload buffer, `payload_sz = bytes
/// placed` (the kernel copies `out_args` from the payload at COMMIT and
/// never reads `op_in`). A body already sitting at `payload + out.len` (the
/// serve-into-payload path) is not copied. `want > payload.len` ⇒ the
/// header-only `-EIO` shape and `transport_replies_oversize` — never a
/// truncated body under a header claiming the full length. Returns whether
/// the reply was applied whole.
pub fn applyReply(hdr: *abi.fuse_uring_req_header, payload: []u8, reply: codec.Reply) bool {
    const want = reply.extraLen();
    if (want > payload.len) {
        replies_oversize.fire("reply body larger than the ent payload");
        log.err("transport: reply for unique {d} wants {d} bytes in a {d}-byte payload buffer — committing header-only -EIO", .{ reply.header.unique, want, payload.len });
        applyErrorHeader(hdr, reply.header.unique, .IO);
        return false;
    }
    hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(reply.header);
    if (reply.out.len > 0) @memcpy(payload[0..reply.out.len], reply.out);
    if (reply.body.len > 0) {
        const dst = payload[reply.out.len..][0..reply.body.len];
        if (dst.ptr != reply.body.ptr) @memcpy(dst, reply.body);
    }
    hdr.ring_ent_in_out.payload_sz = @intCast(want);
    return true;
}

/// The header-only `-errno` commit (`fail_ent`'s and the oversize arm's):
/// touches ONLY the header struct, hence legal against a live lease.
pub fn applyErrorHeader(hdr: *abi.fuse_uring_req_header, unique: u64, errno: linux.E) void {
    hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(codec.errorHeader(unique, errno));
    hdr.ring_ent_in_out.payload_sz = 0;
}

// ---------------------------------------------------------------------------
// Tests: the §4.6 suite, the gate, the reply application.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn freshTable() !SlotTable {
    return SlotTable.init(testing.allocator, 3, 4);
}

test "deliver then commit walks the state machine; the owed count is derived from the states; the foreign words follow" {
    var t = try freshTable();
    defer t.deinit(testing.allocator);
    t.onRegisterSubmitted(0);
    try testing.expectEqual(State.registered, t.ent(0).state);
    try testing.expectEqual(@as(u32, 0), t.owedCount());
    t.onDeliver(0, 77, 77, 1000);
    try testing.expectEqual(@as(u32, 1), t.owedCount());
    try testing.expect(t.anyOwing());
    try testing.expectEqual(@as(u64, 77), t.ent(0).owed_commit_id.load(.acquire));
    try testing.expectEqual(@as(u64, 1000), t.ent(0).owed_since_ns.load(.acquire));
    try testing.expectEqual(Admit.accept, t.admitCommit(0, 77));
    try testing.expectEqual(Gate.ready, t.ent(0).lease.tryCommit());
    t.onCommitSubmitted(0);
    try testing.expectEqual(State{ .replied = 77 }, t.ent(0).state);
    try testing.expectEqual(@as(u32, 0), t.owedCount());
    try testing.expectEqual(@as(u64, 0), t.ent(0).owed_commit_id.load(.acquire));
    try testing.expectEqual(@as(u64, 0), t.ent(0).owed_since_ns.load(.acquire));
    // The commit CQE is the next delivery.
    t.onDeliver(0, 79, 79, 2000);
    try testing.expectEqual(State{ .delivered = .{ .unique = 79, .commit_id = 79 } }, t.ent(0).state);
}

test "THE LAW: a double reply is refused not overwritten; a foreign commit id is refused with the owed request intact; a parked commit still owes and refuses a second" {
    var t = try freshTable();
    defer t.deinit(testing.allocator);
    t.onDeliver(1, 77, 77, 1);
    const before = replies_refused_stale.load();
    try testing.expectEqual(Admit.refuse_stale, t.admitCommit(1, 99));
    try testing.expectEqual(before + 1, replies_refused_stale.load());
    try testing.expectEqual(State{ .delivered = .{ .unique = 77, .commit_id = 77 } }, t.ent(1).state);
    try testing.expectEqual(Admit.accept, t.admitCommit(1, 77));
    // A lease is live: the gate parks; the slot still owes; a second commit is refused.
    _ = t.ent(1).lease.acquire();
    try testing.expectEqual(Gate.parked, t.ent(1).lease.tryCommit());
    t.onCommitParked(1);
    try testing.expectEqual(State{ .parked = .{ .unique = 77, .commit_id = 77 } }, t.ent(1).state);
    try testing.expectEqual(@as(u32, 1), t.owedCount());
    try testing.expectEqual(Admit.refuse_stale, t.admitCommit(1, 77));
    // The drop with a parked commit demands a wake; the probe then unparks.
    try testing.expect(t.ent(1).lease.release());
    try testing.expect(t.ent(1).lease.tryUnpark());
    t.onCommitSubmitted(1);
    try testing.expectEqual(State{ .replied = 77 }, t.ent(1).state);
    try testing.expectEqual(Admit.refuse_stale, t.admitCommit(1, 77));
}

test "failEnt synthesizes exactly once for an owing or parked slot (the second call is nothing); a replied slot synthesizes nothing" {
    var t = try freshTable();
    defer t.deinit(testing.allocator);
    t.onDeliver(2, 77, 77, 1);
    const before = requests_failed_synthetic.load();
    try testing.expectEqual(Fail{ .synthesize = .{ .unique = 77, .commit_id = 77 } }, t.failEnt(2));
    try testing.expectEqual(before + 1, requests_failed_synthetic.load());
    t.onCommitSubmitted(2);
    try testing.expectEqual(Fail.nothing, t.failEnt(2));
    try testing.expectEqual(before + 1, requests_failed_synthetic.load());
    // Parked slots synthesize too (header-only: legal against the live lease).
    t.onDeliver(3, 81, 81, 1);
    _ = t.ent(3).lease.acquire();
    try testing.expectEqual(Gate.parked, t.ent(3).lease.tryCommit());
    t.onCommitParked(3);
    try testing.expectEqual(Fail{ .synthesize = .{ .unique = 81, .commit_id = 81 } }, t.failEnt(3));
    t.onCommitSubmitted(3);
    try testing.expect(t.ent(3).lease.leased());
    try testing.expect(!t.ent(3).lease.release());
}

test "a second delivery on an owing slot reports the displaced request and is accepted; a REGISTER over an owing slot and a failed push are abandonments (the tripwire)" {
    log.testing.expectErrorLogs(4);
    var t = try freshTable();
    defer t.deinit(testing.allocator);
    const abandoned = requests_abandoned.load();
    const displaced = requests_displaced.load();
    t.onDeliver(0, 77, 77, 1);
    requests_abandoned.resetLogBudgetForTest();
    t.onDeliver(0, 79, 79, 2);
    try testing.expectEqual(displaced + 1, requests_displaced.load());
    try testing.expectEqual(abandoned + 1, requests_abandoned.load());
    try testing.expectEqual(State{ .delivered = .{ .unique = 79, .commit_id = 79 } }, t.ent(0).state);
    requests_abandoned.resetLogBudgetForTest();
    t.onRegisterSubmitted(0);
    try testing.expectEqual(abandoned + 2, requests_abandoned.load());
    try testing.expectEqual(State.registered, t.ent(0).state);
    t.onDeliver(1, 81, 81, 3);
    requests_abandoned.resetLogBudgetForTest();
    t.abandon(1);
    try testing.expectEqual(abandoned + 3, requests_abandoned.load());
    try testing.expectEqual(State.registered, t.ent(1).state);
    try testing.expectEqual(@as(u64, 0), t.ent(1).owed_commit_id.load(.acquire));
    try log.testing.expectAllFired();
}

test "retain commit parks the slot until RELEASE re-arms it (owes no reply, keeps its commit id published for the release's economy check; retain_commits − releases ≡ outstanding); a refused retain is the tripwire and recovers to a plain re-commit; release on an unretained slot is inert; EAGAIN re-pushes the same commit" {
    log.testing.expectErrorLogs(1);
    var t = try freshTable();
    defer t.deinit(testing.allocator);
    const rc = retain_commits.load();
    const rl = releases.load();
    const out = retained_outstanding.load();
    t.onDeliver(0, 77, 77, 1);
    t.onCommitSubmittedRetained(0);
    try testing.expectEqual(State{ .retained = 77 }, t.ent(0).state);
    try testing.expectEqual(@as(u32, 0), t.owedCount());
    try testing.expectEqual(@as(u64, 77), t.ent(0).owed_commit_id.load(.acquire));
    try testing.expectEqual(@as(u64, 0), t.ent(0).owed_since_ns.load(.acquire));
    try testing.expectEqual(rc + 1, retain_commits.load());
    try testing.expectEqual(out + 1, retained_outstanding.load());
    try testing.expectEqual(Admit.refuse_stale, t.admitCommit(0, 77));
    try testing.expectEqual(Admit.refuse_stale, t.admitRelease(0, 78));
    try testing.expectEqual(Admit.accept, t.admitRelease(0, 77));
    try testing.expectEqual(Fail.nothing, t.failEnt(0));
    t.onReleaseSubmitted(0);
    try testing.expectEqual(State{ .replied = 77 }, t.ent(0).state);
    try testing.expectEqual(@as(u64, 0), t.ent(0).owed_commit_id.load(.acquire));
    try testing.expectEqual(rl + 1, releases.load());
    try testing.expectEqual(out, retained_outstanding.load());
    try testing.expectEqual(Admit.refuse_stale, t.admitRelease(0, 77));
    // Inert elsewhere.
    t.onReleaseSubmitted(0);
    try testing.expectEqual(State{ .replied = 77 }, t.ent(0).state);
    try testing.expectEqual(rl + 1, releases.load());
    // Retain refused ⇒ the tripwire, re-commit with the same id.
    t.onDeliver(1, 79, 79, 1);
    t.onCommitSubmittedRetained(1);
    retain_refused.resetLogBudgetForTest();
    const refused = retain_refused.load();
    t.onRetainRefused(1, 5);
    try testing.expectEqual(refused + 1, retain_refused.load());
    try testing.expectEqual(out, retained_outstanding.load());
    try testing.expectEqual(State{ .delivered = .{ .unique = 0, .commit_id = 79 } }, t.ent(1).state);
    try testing.expectEqual(Admit.accept, t.admitCommit(1, 79));
    t.onCommitSubmitted(1);
    // EAGAIN on a replied slot ⇒ the same commit id comes back to delivered.
    try testing.expectEqual(@as(?u64, 79), t.onCommitRetry(1, 6));
    try testing.expectEqual(@as(?u64, null), t.onCommitRetry(1, 7));
    t.onCommitSubmitted(1);
    try log.testing.expectAllFired();
}

test "a held lease answers heldSource with the slot coordinates and extracts through its face exactly once per call; a lease in hand copies its data and has no held source" {
    const Face = struct {
        calls: u32 = 0,
        view: [8]u8 = "heldbyte".*,
        fn extract(ctx: ?*anyopaque, gent: u32, _: Io) ExtractError![]const u8 {
            const self: *@This() = @ptrCast(@alignCast(ctx.?));
            self.calls += 1;
            if (gent != 9) return error.SlotNotHeld;
            return &self.view;
        }
    };
    const NoWake = struct {
        fn wake(_: ?*anyopaque) bool {
            return false;
        }
    };
    const waker: Waker = .{ .ctx = null, .wake = NoWake.wake };
    var face: Face = .{};
    var s: LeaseState = .{};
    const holds = zc_write_holds.load();
    const l = PayloadLease.acquireHeld(&s, waker, 8, .direct_gup, 2, 5, 9, .{ .ctx = &face, .extract = Face.extract });
    try testing.expectEqual(holds + 1, zc_write_holds.load());
    try testing.expectEqual(HeldSource{ .qid = 2, .buf_index = 5, .len = 8 }, l.heldSource().?);
    try testing.expectEqual(zc.PayloadClass.direct_gup, l.class);
    var dst: [16]u8 = undefined;
    try testing.expectEqual(@as(usize, 8), try l.extract(testing.io, &dst));
    try testing.expectEqualSlices(u8, "heldbyte", dst[0..8]);
    try testing.expectEqual(@as(u32, 1), face.calls);
    l.release();
    var s2: LeaseState = .{};
    const plain = PayloadLease.acquire(&s2, "inhand", waker);
    try testing.expect(plain.heldSource() == null);
    try testing.expectEqual(@as(usize, 6), try plain.extract(testing.io, &dst));
    try testing.expectEqualSlices(u8, "inhand", dst[0..6]);
    try testing.expectEqual(@as(u32, 1), face.calls);
    plain.release();
}

test "REGISTER failures back off 1, 2, 4 … 64 ms (monotone, ≥ base, ≤ max) then retire on the 8th; success resets; the nearest deadline bounds the park and is consumed once due" {
    var t = try freshTable();
    defer t.deinit(testing.allocator);
    const retired = ents_retired.load();
    var last: u64 = 0;
    var i: u8 = 0;
    while (i < REGISTER_RETRY_MAX - 1) : (i += 1) {
        const v = t.noteRegisterFailure(0, 1000);
        const after = v.retry.after_ns;
        try testing.expect(after >= REGISTER_BACKOFF_BASE_NS and after <= REGISTER_BACKOFF_MAX_NS);
        try testing.expect(after >= last);
        try testing.expectEqual(REGISTER_BACKOFF_BASE_NS << @intCast(i), after);
        last = after;
        try testing.expectEqual(@as(?u64, 1000 + after), t.nextRetryDeadline());
    }
    try testing.expectEqual(RegisterVerdict.retire, t.noteRegisterFailure(0, 1000));
    try testing.expectEqual(State.retired, t.ent(0).state);
    try testing.expectEqual(retired + 1, ents_retired.load());
    try testing.expect(!t.allRetired());
    try testing.expectEqual(@as(?u64, null), t.nextRetryDeadline());
    // Success resets the ladder.
    _ = t.noteRegisterFailure(1, 0);
    _ = t.noteRegisterFailure(1, 0);
    t.noteRegisterSuccess(1);
    try testing.expectEqual(@as(u8, 0), t.ent(1).failures);
    try testing.expectEqual(REGISTER_BACKOFF_BASE_NS, t.noteRegisterFailure(1, 5000).retry.after_ns);
    try testing.expectEqual(@as(?u16, null), t.nextRetryDue(5000 + REGISTER_BACKOFF_BASE_NS - 1));
    try testing.expectEqual(@as(?u16, 1), t.nextRetryDue(5000 + REGISTER_BACKOFF_BASE_NS));
    try testing.expectEqual(@as(?u16, null), t.nextRetryDue(std.math.maxInt(u64)));
    // The 128 ms ceiling is reached at the 8th doubling, never exceeded.
    try testing.expectEqual(@as(u64, 128 * std.time.ns_per_ms), @min(REGISTER_BACKOFF_BASE_NS << 7, REGISTER_BACKOFF_MAX_NS));
    for (t.ents) |*e| e.state = .retired;
    try testing.expect(t.allRetired());
}

test "the lease gate: parks while leased and unparks after the drop; ready when the lease already dropped; the re-check clears parked; a non-final token drop never wakes" {
    var s: LeaseState = .{};
    try testing.expectEqual(@as(u32, 0), s.acquire());
    try testing.expect(s.leased());
    try testing.expectEqual(Gate.parked, s.tryCommit());
    try testing.expect(s.isParked());
    try testing.expect(!s.tryUnpark());
    try testing.expect(s.release());
    try testing.expect(!s.leased());
    try testing.expect(s.tryUnpark());
    try testing.expect(!s.isParked());
    // Already dropped: ready, nothing parked.
    try testing.expectEqual(Gate.ready, s.tryCommit());
    // Multi-ref tokens: only the last drop reports; a drop with nothing
    // parked never demands a wake.
    s.acquireDest();
    s.acquireDest();
    try testing.expect(!s.release());
    try testing.expectEqual(Gate.parked, s.tryCommit());
    try testing.expect(s.release());
    try testing.expect(s.tryUnpark());
    try testing.expectEqual(Gate.ready, s.tryCommit());
}

test "the lease drop runs the ONE wake law: a drop with a parked commit pays the write once, a second producer elides, a drop with nothing parked wakes nobody" {
    const Fake = struct {
        armed: bool = false,
        calls: u32 = 0,
        fn wake(ctx: ?*anyopaque) bool {
            const self: *@This() = @ptrCast(@alignCast(ctx.?));
            self.calls += 1;
            if (self.armed) return false;
            self.armed = true;
            return true;
        }
    };
    var fake: Fake = .{};
    const waker: Waker = .{ .ctx = &fake, .wake = Fake.wake };
    var s: LeaseState = .{};
    const writes = wake_writes.load();
    const elided = wakes_elided.load();
    const outstanding = leases_outstanding.load();
    const lease = PayloadLease.acquire(&s, &.{}, waker);
    try testing.expectEqual(outstanding + 1, leases_outstanding.load());
    try testing.expectEqual(Gate.parked, s.tryCommit());
    lease.release();
    try testing.expectEqual(outstanding, leases_outstanding.load());
    try testing.expectEqual(@as(u32, 1), fake.calls);
    try testing.expectEqual(writes + 1, wake_writes.load());
    // A second drop while armed elides.
    var s2: LeaseState = .{};
    const l2 = PayloadLease.acquire(&s2, &.{}, waker);
    try testing.expectEqual(Gate.parked, s2.tryCommit());
    l2.release();
    try testing.expectEqual(elided + 1, wakes_elided.load());
    // Nothing parked: no wake at all.
    var s3: LeaseState = .{};
    const l3 = PayloadLease.acquire(&s3, &.{}, waker);
    l3.release();
    try testing.expectEqual(@as(u32, 2), fake.calls);
}

test "applyReply: a fitting reply is applied whole (header in in_out, out then body in the payload, payload_sz exact); a body already in place is not copied; an oversize reply becomes header-only -EIO, never a truncated body" {
    var hdr = abi.zeroed(abi.fuse_uring_req_header);
    var payload: [64]u8 = @splat(0xEE);
    var out: codec.OutBuf = undefined;
    const r = codec.replyWrite(&out, 42, 17);
    try testing.expect(applyReply(&hdr, &payload, r));
    const oh = abi.decodePrefix(abi.fuse_out_header, &hdr.in_out).?;
    try testing.expectEqual(@as(u32, 24), oh.len);
    try testing.expectEqual(@as(i32, 0), oh.@"error");
    try testing.expectEqual(@as(u64, 42), oh.unique);
    try testing.expectEqual(@as(u32, 17), std.mem.readInt(u32, payload[0..4], .little));
    try testing.expectEqual(@as(u32, 8), hdr.ring_ent_in_out.payload_sz);
    // A data reply whose body already sits in the payload window.
    @memcpy(payload[0..5], "hello");
    const served = codec.replyData(44, payload[0..5]);
    try testing.expect(applyReply(&hdr, &payload, served));
    try testing.expectEqual(@as(u32, 5), hdr.ring_ent_in_out.payload_sz);
    try testing.expectEqualSlices(u8, "hello", payload[0..5]);
    // A body elsewhere is copied in.
    const body = "abcdefgh";
    try testing.expect(applyReply(&hdr, &payload, codec.replyData(46, body)));
    try testing.expectEqualSlices(u8, body, payload[0..8]);
    // Oversize: header-only EIO, payload untouched, the tripwire moved
    // (two loud lines: the tripwire's and the reply's).
    log.testing.expectErrorLogs(2);
    replies_oversize.resetLogBudgetForTest();
    const before = replies_oversize.load();
    var big: [65]u8 = @splat(1);
    try testing.expect(!applyReply(&hdr, &payload, codec.replyData(48, &big)));
    try testing.expectEqual(before + 1, replies_oversize.load());
    const eio = abi.decodePrefix(abi.fuse_out_header, &hdr.in_out).?;
    try testing.expectEqual(@as(u32, 16), eio.len);
    try testing.expectEqual(-@as(i32, @intCast(@backingInt(linux.E.IO))), eio.@"error");
    try testing.expectEqual(@as(u64, 48), eio.unique);
    try testing.expectEqual(@as(u32, 0), hdr.ring_ent_in_out.payload_sz);
    try testing.expectEqualSlices(u8, body, payload[0..8]);
    try log.testing.expectAllFired();
}
