//! The drain-group worker (charter §4 "Bring-up", "Drain groups",
//! "Delivery", "Worker pass", "Buffer-mode lattice", "Teardown";
//! `docs/porting/fuse_uring_transport.md` §1, §2, §5, §8, §11;
//! `fuse_kmbuf_zc.md` §3–§7): one OS thread `sqz-fuse-ur<qid>` per drain
//! group owning ONE SQE128 io_uring (`ParkedRing128`), the member queues'
//! slot tables, the 288 B ent headers, the payload buffers of the session's
//! BUFFER MODE and the group's commit-cell queue. Linux only.
//!
//! The pass, verbatim in the model-checked order (§8.1): drain → DISARM →
//! pump the commit cells through the per-ent gate → pump the handlers'
//! bridge requests (lazy extractions, releases) → mid-pass reap of finished
//! bridges → poll ONE fused handler frame → unpark scan → the bridge
//! deadline scan → re-arm the standing READ → the 100 ms backstop iff the
//! group owes any reply, a bridge is pending or a fused frame is queued →
//! ONE `io_uring_enter` carrying every SQE this pass pushed and the wait
//! (unbounded only when idle and owing nothing; non-blocking when work is in
//! hand) → reap → per CQE: the wake, the error ladder, a DELIVERY, a bridge
//! or cancel completion, a RELEASE verdict → re-REGISTER and the backoff
//! retries. Commits BATCH: a push only appends to the SQ, the loop-bottom
//! enter flushes (31 → 9 `io_uring_enter` per create on the reference).
//!
//! THE BUFFER MODES (one per session, decided ONCE by the probes):
//!   - `UserEnts` (stock): two REGISTER iovecs `[header, arena buffer]`; the
//!     kernel GUPs the arena per 4 KiB at every copy.
//!   - `BufRing` (kmbuf): the headers are fixed buffer index 0, the payload
//!     is a buffer the kernel SELECTS from its own ring at bgid 0 and hands
//!     over as a KERNEL address through the CQE bid (`zc.Attachment`); the
//!     12.8 % + 2.6 % per-4 KiB lock→GUP→unlock term of client cycles is
//!     gone (measured on the kern EXA read row). Width 1: the ring's fixed
//!     index 0 and bgid 0 serve exactly one queue.
//!   - `ZeroCopy`: + a sparse fixed-buffer table the kernel fills with the
//!     app's pages at delivery (`imu->ubuf = 0`: `addr` is the byte offset
//!     into the slot). Paged READ bodies move in by `READ_FIXED(fd → slot)`
//!     — the direct device leg, the fd-source leg from a MAP_SHARED fill
//!     pool, or the bounce leg from the session's memfd slab (daemon CPU/GiB
//!     −24 % measured on the fd-source leg). Paged WRITE payloads are HELD
//!     in the slot when W1-eligible (holding ineligible shapes measured
//!     0.45×) and extracted at delivery otherwise. Bridges are
//!     deadline-bounded (30 s, AsyncCancel once, must-stay-0 cancels), and
//!     the commit FOLLOWS the bridge CQE: the slot stays Delivered until then.
//!
//! THE ARM BARRIER COUNTS SUBMISSIONS, NOT COMPLETIONS: a REGISTER's CQE is
//! not an ack — the kernel parks the ent and completes it only when it
//! DELIVERS a request on it, and it routes requests to the ring only once
//! `is_ring_ready()` observes EVERY queue armed.
//!
//! TEARDOWN RELEASES EVERY RETAINED SLOT WHILE THE CONNECTION IS LIVE: a
//! retained ent (`replyRetain`, the ACK-early arm) the kernel reclaims at
//! its own teardown is logged `fuse-uring: … teardown with retained zc
//! payloads` and its still-registered fixed buffer pins the io_uring exit —
//! the exiting process sits in D state until the container is recreated.
//! So the drain RELEASEs every `retained` slot and reaps the verdicts
//! BEFORE it publishes `drained` (the session's `umount2` waits for it).
//! What the daemon cannot cover: an exit that runs no teardown (SIGKILL,
//! SIGABRT) and an EXTERNAL `umount(8)` of a type-`fuse` mount — the kernel
//! sends no DESTROY there (`fc->destroy` is fuseblk/virtiofs only) and
//! aborts the connection outright, so a retained slot at that instant is
//! the kernel's drain's (counted `transport_retained_kernel_drained`).
//!
//! Why a dedicated thread and not an `exec.Lane`: `Lane` owns std's 64 B
//! `ParkedRing`; the FUSE uring command needs the 80 B command area of an
//! SQE128 ring (`io/uring128.zig`). The fused handler poll therefore runs
//! the filesystem's SYNC try-only probe path (arm (a)'s ladder) once per
//! pass per frame from a bounded run-queue; a frame the ladder cannot
//! answer within `FUSED_POLL_MAX` polls is handed to the handler lane.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const assert = std.debug.assert;

const uring128 = @import("../../io/uring128.zig");
const ring_park = @import("../../io/ring_park.zig");
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const geometry = @import("../geometry.zig");
const slot = @import("slot.zig");
const zc = @import("zc.zig");
const kmbuf = @import("kmbuf.zig");
const commit_queue = @import("commit_queue.zig");
const fsv = @import("filesystem_vtable.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const derive = @import("../../core/derive.zig");
const thread_name = @import("../../core/thread_name.zig");
const futex = @import("../../sync/futex.zig");
const completion = @import("../../sync/completion.zig");
const mpmc = @import("../../sync/mpmc.zig");
const exec = @import("../../exec/exec.zig");
const placement = @import("../../exec/placement.zig");
const mem_budget = @import("../../mem/budget.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("fuse/transport/worker is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const SqzIo = exec.SqzIo;

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The park backstop while the group owes a reply, a bridge is pending or a
/// fused frame is queued: the ipc dd-reaper's shipped cadence. Field
/// captures proved cross-thread wake chains lose exactly one wake under
/// load (two delivered LOOKUPs whose handlers REPLIED sat unpumped for 23
/// min while every worker slept in cq-wait, generic/795); idle workers keep
/// the zero-cost unbounded park.
pub const PARK_BACKSTOP_NS: u64 = 100 * std.time.ns_per_ms;
/// The attribution WARN rate limit for a tick rescue (per worker).
pub const RESCUE_LOG_INTERVAL_S: u64 = 60;
/// The whole-group wait for live leases at the drain (was 1 ms…100 ms PER
/// ENT in series = 3.2 s at depth 32).
pub const DRAIN_LEASE_BUDGET_NS: u64 = 100 * std.time.ns_per_ms;
/// Reply residency after the drain (finding 32): the unmount's own
/// FLUSH/RELEASE against this session arrives AFTER the drain, and an ent
/// left in userspace is `waiting ≥ 1` — the 59-minute cloud wedge. Bounded
/// parks on the worker's own ring, never a sleep-poll (Q14).
pub const RESIDENCY_NS: u64 = 30 * std.time.ns_per_s;
pub const RESIDENCY_PARK_NS: u64 = 10 * std.time.ns_per_ms;
/// How long the drain waits for its RELEASE CQEs: the kernel answers a
/// RELEASE_PAYLOAD inline in the submitting enter (`fuse_uring_cmd` returns
/// the verdict, never -EIOCBQUEUED), so the wait is one enter and the bound
/// only guards a dead ring; a lapse is loud (the slot is then the kernel's
/// and its reclaim pins the exit).
pub const TEARDOWN_RELEASE_BOUND_NS: u64 = 2 * std.time.ns_per_s;
/// Read fusion ceiling: `max(payload / 8, 4096)` — two wakes and two
/// schedules ≈ 5–10 µs ≈ 50–100 KiB at ~10 GB/s of merge copy (128 KiB at
/// a 1 MiB payload).
pub fn fusionCeiling(payload_sz: u32) u32 {
    return @max(payload_sz / 8, 4096);
}
/// The arena stride: each ent's payload page-aligned so the kernel's GUP
/// and a later fixed-buffer registration see whole pages (one vma).
pub const ARENA_ALIGN: usize = 4096;

/// CQE `res` errnos that mean the connection is gone.
pub fn isDisconnect(e: linux.E) bool {
    return switch (e) {
        .NOTCONN, .CONNABORTED, .NODEV, .PIPE, .BADF, .SHUTDOWN => true,
        else => false,
    };
}

/// The kernel rejected the protocol itself (not this request).
fn isProtocolRejection(e: linux.E) bool {
    return switch (e) {
        .OPNOTSUPP, .INVAL, .NOSYS => true,
        else => false,
    };
}

// ---------------------------------------------------------------------------
// Counters (stats-inode keys).
// ---------------------------------------------------------------------------

/// `transport_ring_requests` / `transport_ring_replies`: deliveries
/// dispatched / COMMIT_AND_FETCH pushed (auto-commits and inline serves
/// included).
pub var ring_requests: counters.ShardedCounter = .{};
pub var ring_replies: counters.ShardedCounter = .{};
/// `transport_ring_reads`: READs delivered — the partition law's left side.
pub var ring_reads: counters.ShardedCounter = .{};
/// `transport_registers`: REGISTER SQEs pushed (arm + re-REGISTER + retries).
pub var registers: counters.ShardedCounter = .{};
/// `transport_cqe_errors`: negative-result CQEs.
pub var cqe_errors: counters.ShardedCounter = .{};
/// `transport_forget_autocommits`: FORGET/BATCH_FORGET deliveries committed
/// by the worker; `transport_forget_entries` the nlookup records inside them.
pub var forget_autocommits: counters.ShardedCounter = .{};
pub var forget_entries: counters.ShardedCounter = .{};
/// `transport_fast_dispatch_serves` / `_demotes`: the READ law's arms (a) / (c).
pub var fast_dispatch_serves: counters.ShardedCounter = .{};
pub var fast_dispatch_demotes: counters.ShardedCounter = .{};
/// `transport_fused_read_fusions` / `_demotions`: arm (b) taken / refused
/// (fusible by the lever and the ceiling, but no fused capacity or not a
/// zero-copy session). `transport_fused_read_late_demotions`: a fused frame
/// the ladder never answered within its polls, handed to the lane (outside
/// the partition: the READ was counted a fusion at delivery).
/// `transport_fused_polls`: frames polled.
pub var fused_read_fusions: counters.ShardedCounter = .{};
pub var fused_read_demotions: counters.ShardedCounter = .{};
pub var fused_read_late_demotions: counters.ShardedCounter = .{};
pub var fused_polls: counters.ShardedCounter = .{};
/// `transport_dispatch_refused`: a handler lane refused the spawn (its
/// capacity); the request was answered EAGAIN-class (0002 row 10).
pub var dispatch_refused: counters.ShardedCounter = .{};
/// `transport_park_backstop_ticks`: bounded parks that ticked out
/// (non-zero under traffic = the backstop WORKING).
pub var park_backstop_ticks: counters.ShardedCounter = .{};
/// `transport_park_tick_commit_rescues` / `_cqe_rescues` (≈ 0): messages
/// pumped / CQEs surfaced by a tick with no wake delivered.
pub var park_tick_commit_rescues: counters.ShardedCounter = .{};
pub var park_tick_cqe_rescues: counters.ShardedCounter = .{};
/// `transport_enters`: `io_uring_enter` calls by the workers (the
/// enters-per-op instrument).
pub var enters: counters.ShardedCounter = .{};
/// `transport_passes`: worker passes run.
pub var passes: counters.ShardedCounter = .{};
/// `transport_residency_replies`: fresh deliveries answered ENOTCONN
/// after the drain.
pub var residency_replies: counters.ShardedCounter = .{};
/// `transport_cq_overflows` (must-stay-0): kernel-dropped CQEs.
pub var cq_overflows: counters.Tripwire = .init("transport_cq_overflows");
/// `transport_cqe_tag_unknown` (must-stay-0): a CQE tag no push of ours
/// produced (decision 0002 ledger Q6: a tripwire, not a REGISTER).
pub var cqe_tag_unknown: counters.Tripwire = .init("transport_cqe_tag_unknown");
/// `transport_delivery_on_leased_ent` (must-stay-0): a delivery landed on
/// an ent whose previous payload lease is still live.
pub var delivery_on_leased: counters.Tripwire = .init("transport_delivery_on_leased_ent");
/// `transport_sq_full` (must-stay-0): the SQ stayed full after a flush.
pub var sq_full: counters.Tripwire = .init("transport_sq_full");
/// `transport_commit_batch_flushes` / `_commits`: COMMIT SQEs per enter;
/// `commits / flushes ≈ 1` under a storm is the regression.
pub var commit_batch_flushes: counters.ShardedCounter = .{};
pub var commit_batch_commits: counters.ShardedCounter = .{};
/// `transport_commit_batch` buckets: exact 1–8, ≤ 16, ≤ 32, > 32 — a
/// "≈ 1" verdict never rides rounding.
pub const commit_batch_labels = [_][]const u8{ "1", "2", "3", "4", "5", "6", "7", "8", "le16", "le32", "gt32" };
pub var commit_batch: [commit_batch_labels.len]std.atomic.Value(u64) = @splat(.init(0));

// ---- the kmbuf / zero-copy families (`transport_kmbuf_*`, `transport_zc_*`) ----

/// `transport_kmbuf_attached_deliveries`: deliveries whose payload was read
/// at the kernel address the CQE bid named (no user arena, no GUP).
pub var kmbuf_attached_deliveries: counters.ShardedCounter = .{};
/// `transport_kmbuf_attachment_breach` (must-stay-0): a CQE bid at or past
/// the ring — a protocol breach; the session shuts down.
pub var kmbuf_attachment_breach: counters.Tripwire = .init("transport_kmbuf_attachment_breach");
/// `transport_zc_replies`: paged replies whose body rode the slot
/// (prefilled, direct leg or bridged) — the kernel's folio copy skipped.
pub var zc_replies: counters.ShardedCounter = .{};
/// `transport_zc_serve_bytes`: READ bytes that moved by DMA from a
/// filesystem fd (direct device leg + fd-source leg) — a closure term of the
/// read copy ledger (`zc_serve + bounce ≡ user bytes`).
pub var zc_serve_bytes: counters.ShardedCounter = .{};
/// `transport_zc_bounce_bytes_bridged`: READ bytes bridged from the bounce
/// slab (one daemon pass staged them).
pub var zc_bounce_bytes: counters.ShardedCounter = .{};
/// `transport_zc_bounce_copies`: reply bodies the worker memcpy'd INTO the
/// bounce (the body was not served in place).
pub var zc_bounce_copies: counters.ShardedCounter = .{};
/// `transport_zc_bounce_bytes`: bounce-window bytes the kernel holds pages
/// for (every window's page-rounded high-water since its last trim, summed
/// over the session) — the `transport_zc_bounce` component's gauge. The
/// slab is a LAZILY populated memfd (`Slab`): its reserve is virtual.
pub var zc_bounce_resident_bytes: counters.Gauge = .{};
/// `transport_zc_bounce_trimmed_bytes`: window bytes punched back to the
/// kernel by the idle trim (Yellow/Red ticks).
pub var zc_bounce_trimmed_bytes: counters.ShardedCounter = .{};
/// `transport_zc_fd_body_replies` / `_fallbacks`: replies bridged straight
/// from the filesystem's fd; fd bridges that failed and re-staged through
/// the bounce (≈ 0).
pub var zc_fd_body_replies: counters.ShardedCounter = .{};
pub var zc_fd_body_fallbacks: counters.ShardedCounter = .{};
/// `transport_zc_direct_legs` / `_fallbacks`: direct device legs landed;
/// legs that failed or came short and fell through to the ordinary ladder.
pub var zc_direct_legs: counters.ShardedCounter = .{};
pub var zc_direct_leg_fallbacks: counters.ShardedCounter = .{};
/// `transport_zc_direct_leg_recheck_withdrawn`: a landed leg whose post-DMA
/// recheck failed — the prefilled reply withdrawn, the ladder re-served.
pub var zc_direct_leg_recheck_withdrawn: counters.ShardedCounter = .{};
/// `transport_zc_direct_leg_dma_bytes`: every byte the direct device leg's
/// `READ_FIXED(device fd → slot)` landed, validated or withdrawn — the
/// transport's arm of the device-read ledger (decision 0003 §K; the
/// filesystem's DMAs never see this read). `transport_zc_direct_leg_serve_
/// bytes`: the validated subset the app's pages kept — the user side.
pub var zc_direct_leg_dma_bytes: counters.ShardedCounter = .{};
pub var zc_direct_leg_serve_bytes: counters.ShardedCounter = .{};
/// `transport_zc_write_directs` / `_bytes` / `_failures`: held WRITE
/// payloads landed by `WRITE_FIXED(slot → device)` — the sole-owner patch
/// class with zero daemon passes — and the bridges that came back short.
pub var zc_write_directs: counters.ShardedCounter = .{};
pub var zc_write_direct_bytes: counters.ShardedCounter = .{};
pub var zc_write_direct_failures: counters.ShardedCounter = .{};
/// `transport_zc_fd_source_copies`: a probe's fd-source answer served by
/// the copy arm (a non-zc session, a fused frame).
pub var zc_fd_source_copies: counters.ShardedCounter = .{};
/// `transport_zc_fallbacks` (≈ 0): a bounce bridge failed and the reply
/// degraded to the kmbuf attachment or header-only EIO.
pub var zc_fallbacks: counters.ShardedCounter = .{};
/// `transport_zc_readdir_latch`: 0 unresolved, 1 paged, 2 copyable — the
/// readdir class's pagedness as the first bridge probed it.
pub var zc_readdir_latch: counters.Gauge = .{};
/// `transport_zc_bridges`: bridge SQEs pushed; `transport_zc_bridge_pends`:
/// live pends (0 at quiesce).
pub var zc_bridges: counters.ShardedCounter = .{};
pub var zc_bridge_pends: counters.Gauge = .{};
/// `transport_zc_bridge_cancels` (must-stay-0): a bridge the ring never
/// completed inside the deadline.
pub var bridge_cancels: counters.Tripwire = .init("transport_zc_bridge_cancels");
/// `transport_zc_bridge_lost` (must-stay-0): a posted completion the
/// transport never reaped (the cancel's -ENOENT with a live pend).
pub var bridge_lost: counters.Tripwire = .init("transport_zc_bridge_lost");
/// `transport_zc_bridge_orphans` (must-stay-0): a live pend with no
/// deadline stamp, re-stamped.
pub var bridge_orphans: counters.Tripwire = .init("transport_zc_bridge_orphans");
/// `transport_zc_bridge_restamps`: cancel CQEs that found the op still
/// running kernel-side (re-stamped, loud).
pub var bridge_restamps: counters.ShardedCounter = .{};
/// `transport_zc_scan_max_age_ms`: the oldest live bridge's age at the last scan.
pub var scan_max_age_ms: counters.Gauge = .{};
/// `transport_zc_release_inert`: a release for a slot that was not retained
/// (the retain was refused and committed plain, or a teardown race).
pub var zc_release_inert: counters.ShardedCounter = .{};
/// `transport_retained_released_at_teardown`: retained slots the drain
/// RELEASEd itself — the RELEASE CQE answered 0 while the connection was
/// still live. The kernel's own reclaim of a retained ent (`fuse-uring: …
/// teardown with retained zc payloads`) pins the exiting process; an
/// orderly exit never leaves one.
pub var retained_released_at_teardown: counters.ShardedCounter = .{};
/// `transport_retained_kernel_drained`: retained slots the drain found on
/// a connection already gone, or whose RELEASE met one (`-ENOTCONN`: an
/// external `umount(8)`, a sysfs abort or a lazy detach beat the drain) —
/// the kernel's drain owns them. Non-zero only on a disconnect the kernel
/// began: an external `umount(8)`, or the end of a lazily detached mount
/// (`squeezefs umount --lazy`) once its last holder left.
pub var retained_kernel_drained: counters.ShardedCounter = .{};
/// `transport_zc_releases_inflight`: RELEASE SQEs pushed whose CQE is not
/// yet reaped — the RELEASE ledger's closure term (`releases − verdicts`),
/// 0 at quiesce.
pub var releases_inflight: counters.Gauge = .{};

fn noteCommitBatch(n: u32) void {
    if (n == 0) return;
    commit_batch_flushes.inc();
    commit_batch_commits.add(n);
    const idx: usize = if (n <= 8) n - 1 else if (n <= 16) 8 else if (n <= 32) 9 else 10;
    _ = commit_batch[idx].fetchAdd(1, .monotonic);
}

// ---------------------------------------------------------------------------
// user_data tags
// ---------------------------------------------------------------------------

pub const OpKind = enum(u8) { register = 1, commit = 2, bridge = 3, cancel = 4, release = 5 };

pub fn tagOf(kind: OpKind, gent: u32) u64 {
    return (uring128.Tag{ .kind = @backingInt(kind), .a = @intCast(gent), .b = 0, .c = 0 }).pack();
}

pub const DecodedTag = struct { kind: OpKind, gent: u32 };

pub fn decodeTag(ud: u64) ?DecodedTag {
    const t = uring128.Tag.unpack(ud);
    const kind = std.enums.fromInt(OpKind, t.kind) orelse return null;
    if (t.b != 0 or t.c != 0) return null;
    return .{ .kind = kind, .gent = t.a };
}

// ---------------------------------------------------------------------------
// The FUSE uring command (byte-exact; porting spec §1.5–1.6, kmbuf §3.3)
// ---------------------------------------------------------------------------

fn cmdBytes(commit_id: u64, qid: u16, union_flags: u16, queue_depth: u16) [uring128.cmd_len]u8 {
    var cmd: [uring128.cmd_len]u8 = @splat(0);
    const req: abi.fuse_uring_cmd_req = .{ .flags = 0, .commit_id = commit_id, .qid = qid, .init_flags = union_flags, .queue_depth = queue_depth, .padding = @splat(0) };
    cmd[0..@sizeOf(abi.fuse_uring_cmd_req)].* = abi.encode(req);
    return cmd;
}

/// A REGISTER for a UserEnts ent: `addr` = the 2-iovec array, `len` = 2,
/// `buf_index` 0, the command `{flags 0, commit_id 0, qid}`.
pub fn prepRegister(sqe: *uring128.Sqe128, qid: u16, iovs: *const [2]std.posix.iovec, gent: u32) void {
    const cmd = cmdBytes(0, qid, 0, 0);
    uring128.prepUringCmd(sqe, .{ .fixed = 0 }, abi.FUSE_IO_URING_CMD_REGISTER, &cmd);
    sqe.addr = @intFromPtr(iovs);
    sqe.len = abi.REGISTER_IOVEC_COUNT;
    sqe.buf_index = 0;
    sqe.user_data = tagOf(.register, gent);
}

/// A REGISTER for a kmbuf / zero-copy ent: NO iovecs; `init.flags` = the
/// mode word, `init.queue_depth` = depth under zc (the kernel refuses zc
/// with a zero depth), `buf_index` = the ent index (the kernel reads
/// `fixed_buf_id` from it: the header at `ent × 288`, the zc slot at `ent`).
pub fn prepRegisterKmbuf(sqe: *uring128.Sqe128, qid: u16, ent_idx: u16, init_flags: u16, queue_depth: u16, gent: u32) void {
    const cmd = cmdBytes(0, qid, init_flags, queue_depth);
    uring128.prepUringCmd(sqe, .{ .fixed = 0 }, abi.FUSE_IO_URING_CMD_REGISTER, &cmd);
    sqe.buf_index = ent_idx;
    sqe.user_data = tagOf(.register, gent);
}

/// COMMIT_AND_FETCH: no iovecs; the reply rides in the ent. `commit_flags`
/// is 0 or `FUSE_URING_COMMIT_RETAIN` (the union u16 at cmd offset 18).
pub fn prepCommit(sqe: *uring128.Sqe128, qid: u16, commit_id: u64, gent: u32, commit_flags: u16) void {
    const cmd = cmdBytes(commit_id, qid, commit_flags, 0);
    uring128.prepUringCmd(sqe, .{ .fixed = 0 }, abi.FUSE_IO_URING_CMD_COMMIT_AND_FETCH, &cmd);
    sqe.user_data = tagOf(.commit, gent);
}

/// RELEASE_PAYLOAD (opcode 3): ends a retention by its commit id.
pub fn prepRelease(sqe: *uring128.Sqe128, qid: u16, commit_id: u64, gent: u32) void {
    const cmd = cmdBytes(commit_id, qid, 0, 0);
    uring128.prepUringCmd(sqe, .{ .fixed = 0 }, abi.FUSE_IO_URING_CMD_RELEASE_PAYLOAD, &cmd);
    sqe.user_data = tagOf(.release, gent);
}

/// The fd→slot bridge (`READ_FIXED(fd @ off → slot ent_idx)`, addr 0 = the
/// slot's byte offset 0 under a kernel-registered zc slot).
pub fn prepBridgeFetch(sqe: *uring128.Sqe128, fd: linux.fd_t, off: u64, len: u32, ent_idx: u16, gent: u32) void {
    uring128.prepReadFixed(sqe, .{ .raw = fd }, ent_idx, off, len, 0);
    sqe.user_data = tagOf(.bridge, gent);
}

/// The slot→fd bridge (`WRITE_FIXED(slot ent_idx → fd @ off)`): extraction
/// into the bounce, or rung 4's direct store into a device.
pub fn prepBridgeStore(sqe: *uring128.Sqe128, fd: linux.fd_t, off: u64, len: u32, ent_idx: u16, gent: u32) void {
    uring128.prepWriteFixed(sqe, .{ .raw = fd }, ent_idx, off, len, 0);
    sqe.user_data = tagOf(.bridge, gent);
}

// ---------------------------------------------------------------------------
// Shared session-level state every group reads.
// ---------------------------------------------------------------------------

pub const BarrierOutcome = enum(u8) { armed, failed };

/// The session's bounce SLAB (zero-copy): ONE MAP_SHARED memfd for every
/// queue's ent windows (`(qid × depth + ent) × stride`), two faces of one
/// memory — a VA for CPU serves and leases, an fd for the ring's
/// `READ_FIXED`/`WRITE_FIXED`. LAZILY populated: a window costs pages only
/// once a body is staged through it (rung 9 measured the populated slab
/// at 288 MiB resident on an idle mount), and an idle window's pages
/// return under the idle-trim law (`punch`: a hole in the memfd, the
/// window reads zeros until the next stage). A non-sheddable budget
/// component over the resident bytes (`zc_bounce_resident_bytes`).
pub const Slab = struct {
    fd: linux.fd_t,
    base: []align(4096) u8,
    stride: usize,
    depth: u32,

    pub fn init(nqueues: u32, depth: u32, payload_sz: u32) error{SlabFailed}!Slab {
        const stride: usize = @intCast(derive.alignUp(payload_sz, ARENA_ALIGN));
        const span: usize = stride * @as(usize, nqueues) * depth;
        const rc = linux.memfd_create("sqz-fuse-zc-bounce", linux.MFD.CLOEXEC);
        if (linux.errno(rc) != .SUCCESS) {
            log.err("transport: memfd_create for the zc bounce slab failed ({t})", .{linux.errno(rc)});
            return error.SlabFailed;
        }
        const fd: linux.fd_t = @intCast(rc);
        errdefer _ = linux.close(fd);
        if (linux.errno(linux.ftruncate(fd, @intCast(span))) != .SUCCESS) {
            log.err("transport: ftruncate({d}) of the zc bounce slab failed", .{span});
            return error.SlabFailed;
        }
        const map = linux.mmap(null, span, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
        if (linux.errno(map) != .SUCCESS) {
            log.err("transport: mmap of the {d}-byte zc bounce slab failed ({t})", .{ span, linux.errno(map) });
            return error.SlabFailed;
        }
        const ptr: [*]align(4096) u8 = @ptrFromInt(map);
        return .{ .fd = fd, .base = ptr[0..span], .stride = stride, .depth = depth };
    }

    pub fn deinit(self: *Slab) void {
        _ = linux.munmap(self.base.ptr, self.base.len);
        _ = linux.close(self.fd);
        self.* = undefined;
    }

    pub fn offsetOf(self: *const Slab, qid: u16, ent_idx: u16) u64 {
        return (@as(u64, qid) * self.depth + ent_idx) * self.stride;
    }

    pub fn window(self: *Slab, qid: u16, ent_idx: u16, len: usize) []u8 {
        return self.base[@intCast(self.offsetOf(qid, ent_idx))..][0..len];
    }

    pub fn bytes(self: *const Slab) u64 {
        return self.base.len;
    }

    /// Return one window's pages (`FALLOC_FL_PUNCH_HOLE | KEEP_SIZE`: the
    /// offsets stay, the shmem pages go). True when the kernel took it.
    pub fn punch(self: *Slab, qid: u16, ent_idx: u16) bool {
        const mode = linux.FALLOC.FL_PUNCH_HOLE | linux.FALLOC.FL_KEEP_SIZE;
        return linux.errno(linux.fallocate(self.fd, mode, @intCast(self.offsetOf(qid, ent_idx)), @intCast(self.stride))) == .SUCCESS;
    }
};

pub const Shared = struct {
    fuse_fd: linux.fd_t,
    plan: geometry.Plan,
    fs: fsv.Filesystem,
    exec: *SqzIo,
    handler_group: *Io.Group,
    /// The session's reply face (handles point at it).
    tx: *const fsv.Tx,
    nqueues: u32,
    /// qid == CPU id (the kernel's possible-CPU count): home the handler on it.
    qid_is_cpu: bool,
    /// `SQUEEZEFS_FUSE_READ_FUSION` ∧ a zero-copy session (never on UserEnts).
    read_fusion_on: bool,
    /// `SQUEEZEFS_FUSE_SQPOLL`: the kernel submission poller (A/B lever).
    sqpoll: bool,
    /// The topology for lane homing (null node when unknown).
    node_of: *const fn (ctx: ?*const anyopaque, cpu: u32) ?u32,
    node_ctx: ?*const anyopaque,
    /// The lattice rung (decided ONCE by the probes) and its resources.
    mode: geometry.BufferMode = .user_ents,
    kmbuf_opcodes: ?abi.KmbufRegisterOpcodes = null,
    /// Payload retention negotiated (zc ∧ the RELEASE probe present).
    retention: bool = false,
    page: usize = 4096,
    /// The volume's block size: the W1 shape's `block/8` cap.
    block_size: u64 = 4 * derive.MiB,
    /// The executor's `Io`: the kmbuf ENOMEM retry waits through it.
    io: Io,
    slab: ?*Slab = null,
    /// The bridge deadline (the constant, or the test seam's).
    bridge_deadline_ns: u64 = zc.BRIDGE_DEADLINE_NS,
    /// The readdir class's pagedness, latched by the first bridge (one per
    /// session: the kernel's FUSE tree is one fact).
    readdir_latch: std.atomic.Value(u8) = .init(@backingInt(zc.ReaddirLatch.unresolved)),
    /// The session's liveness: flipped once by `shutdown`; every worker
    /// drains when it reads false.
    active: std.atomic.Value(bool) = .init(true),
    /// The teardown detached the mountpoint lazily with a holder still on
    /// it (the session's race-case exit): no umount straggler can arrive and
    /// no holder will end the connection, so the residency has nothing to
    /// wait for (the connection ends at the process's exit).
    detached: std.atomic.Value(bool) = .init(false),
    /// The arm barrier: queues whose REGISTERs were all PUSHED.
    queues_registered: std.atomic.Value(u32) = .init(0),
    /// Completed by the worker that pushes the last queue's REGISTERs, or by
    /// a failing worker.
    barrier: completion.Completion(BarrierOutcome) = .{},
    /// `SQUEEZEFS_TEST_FUSE_REGISTER_REFUSE`: the next N REGISTER pushes are
    /// answered as refused without reaching the kernel.
    register_refuse_remaining: std.atomic.Value(u32) = .init(0),
    /// `SQUEEZEFS_TEST_KMBUF_REGISTER_ENOMEM`: every kmbuf ring registration
    /// answers ENOMEM without reaching the kernel.
    test_kmbuf_enomem: bool = false,
    /// `SQUEEZEFS_TEST_ZC_BRIDGE_STALL`: the next N reply bridges read a
    /// pipe with no writer (`stall_pipe[0]`) and never complete.
    bridge_stall_remaining: std.atomic.Value(u32) = .init(0),
    stall_pipe: [2]linux.fd_t = .{ -1, -1 },
    /// Error logs each worker thread expects in a test (`log.testing`).
    test_error_logs: usize = 0,

    pub fn nodeOf(self: *const Shared, qid: u16) ?u16 {
        const n = self.node_of(self.node_ctx, qid) orelse return null;
        return @intCast(n);
    }

    /// Idempotent: the first caller flips it.
    pub fn shutdown(self: *Shared) bool {
        return self.active.swap(false, .acq_rel);
    }

    pub fn isActive(self: *const Shared) bool {
        return self.active.load(.acquire);
    }

    pub fn usesKmbuf(self: *const Shared) bool {
        return self.mode != .user_ents;
    }

    pub fn zeroCopy(self: *const Shared) bool {
        return self.mode == .zero_copy;
    }

    pub fn readdirLatch(self: *const Shared) zc.ReaddirLatch {
        return @fromBackingInt(@intCast(self.readdir_latch.load(.acquire)));
    }

    fn latchReaddir(self: *Shared, v: zc.ReaddirLatch) void {
        if (self.readdir_latch.cmpxchgStrong(@backingInt(zc.ReaddirLatch.unresolved), @backingInt(v), .acq_rel, .acquire) == null) {
            zc_readdir_latch.set(@backingInt(v));
            log.info("transport: readdir class probed {t} by its first bridge", .{v});
        }
    }
};

pub const Member = struct {
    qid: u16,
    slots: slot.SlotTable,
    /// Per ent: a commit cell is parked behind a live lease.
    parked: []bool,
    /// REGISTER pushes still owed before this queue counts for the barrier.
    initial_pending: u32,
    counted: bool = false,
};

/// One in-flight bridge per ent (an ent serves one request between two
/// commits); the pass resolves it at its CQE.
const Pend = struct {
    kind: Kind,
    /// The bridge's DMA length.
    len: u32,
    /// The reply's payload length when it differs from `len` (a short
    /// paged READ bridges its zero-completed pages: `len` ≥ `reply_len`);
    /// 0 = `len`.
    reply_len: u32 = 0,
    /// Readdir-class slot while the latch is unresolved: a -EFAULT here is
    /// the probe's "copyable" verdict, not a failure.
    readdir_probe: bool = false,
    /// The filesystem's completion hook (the direct leg's post-DMA recheck,
    /// an fd-source serve's pin release), run at the CQE.
    hook: ?fsv.CompletionHook = null,

    fn replyLen(self: Pend) u32 {
        return if (self.reply_len != 0) self.reply_len else self.len;
    }

    const Kind = enum(u8) {
        /// `READ_FIXED(slab @ bounce_off → slot)`; the commit cell is parked.
        reply_bounce,
        /// `READ_FIXED(fs fd @ off → slot)`; the cell is parked; short ⇒ restage.
        reply_fd,
        /// `READ_FIXED(device fd → slot)` at delivery; short ⇒ the lane serves.
        direct_leg,
        /// `WRITE_FIXED(slot → slab)` at delivery; the WRITE dispatches at the CQE.
        write_extract,
        /// `WRITE_FIXED(slot → slab)` on a handler's `extract`; the cell answers.
        lazy_extract,
        /// `WRITE_FIXED(slot → device fd @ off)` on a handler's `storeHeld`:
        /// the sole-owner patch class lands straight from the app's pages;
        /// the store cell answers.
        direct_store,
    };
};

/// Where a bridge-parked reply's message lives: a handler's commit cell, or
/// the worker's own `local_msgs` entry (a probe-served READ never took a
/// cell).
const ParkedReply = enum(u8) { none, cell, local };

/// A handler's lazy-extraction request cell (per gent): the handler fiber
/// parks on `event`, the worker completes `res` at the bridge CQE.
const ExtractCell = struct {
    event: Io.Event = .unset,
    res: i32 = 0,
    /// Memoized for the current request: a second `extract` pays no bridge.
    done_len: u32 = 0,
};

/// A handler's direct-store request cell (per gent): the destination the
/// handler named, the fiber's park, the worker's verdict at the CQE.
const StoreCell = struct {
    event: Io.Event = .unset,
    res: i32 = 0,
    fd: linux.fd_t = -1,
    off: u64 = 0,
    len: u32 = 0,
};

/// A fused READ frame (arm (b)): the request re-polled through the
/// filesystem's try-only ladder once per pass.
const FusedFrame = struct {
    req: fsv.Request,
    in: abi.fuse_read_in,
    sref: slot.SlotRef,
    polls: u8,
};

pub const Group = struct {
    gpa: std.mem.Allocator,
    shared: *Shared,
    index: u16,
    members: []Member,
    depth: u32,
    payload_len: usize,
    stride: usize,
    /// UserEnts: ONE anonymous mapping for every member's payload buffers.
    arena: []align(ARENA_ALIGN) u8,
    headers: kmbuf.Headers,
    iovs: [][2]std.posix.iovec,
    cells: commit_queue.Queue,
    /// The group's ring: built ON the worker thread (`SINGLE_ISSUER`).
    ring: uring128.ParkedRing128 = undefined,
    ring_rung: usize = 0,
    /// kmbuf / zc resources, registered on the ring (worker thread).
    kmb: ?kmbuf.Kmbuf = null,
    /// Per gent: the attached kmbuf buffer (the CQE-bid law).
    attached: []zc.Attachment,
    /// Per gent: the in-flight bridge, its deadline stamp, the reply parked
    /// behind it (a handler's cell or the worker's own message), the held
    /// WRITE's length, the fused frame, the extraction cell.
    pends: []?Pend,
    deadlines: zc.Deadlines,
    bridge_parked: []ParkedReply,
    local_msgs: []commit_queue.Msg,
    held_len: []u32,
    /// Per gent: the bounce window's page-rounded high-water since its
    /// last punch (0 = no pages), and the budget tick the ent last went
    /// idle (committed) — the idle-trim law's inputs.
    bounce_populated: []u32,
    bounce_idle_tick: []u64,
    /// The sampler's trim request: `min_idle_ticks + 1` (0 = none); the
    /// pass consumes it (`trimIdleWindows`).
    bounce_trim_request: std.atomic.Value(u64) = .init(0),
    fused: zc.FusedQueue,
    frames: []FusedFrame,
    extract_cells: []ExtractCell,
    store_cells: []StoreCell,
    /// Per gent: the hook a probe's fd-source serve attached for the bridge
    /// `commitReadyZc` pushes.
    hooks: []?fsv.CompletionHook,
    /// Handler → worker inboxes: lazy extractions, direct stores and
    /// releases (gents).
    extract_inbox: mpmc.BoundedMpmc(u32),
    store_inbox: mpmc.BoundedMpmc(u32),
    release_inbox: mpmc.BoundedMpmc(u32),
    /// Per gent: the in-flight RELEASE was the drain's, not a handler's —
    /// one RELEASE per retention, so one flag.
    release_teardown: []bool,
    last_scan_ns: u64 = 0,
    affinity: ?placement.CpuSet,
    thread: ?std.Thread = null,
    /// 0 starting, 1 armed, 2 failed, 3 failed with the typed kmbuf
    /// ENOMEM refusal (`StartError.KmbufRingEnomem`).
    started: futex.Word = .init(0),
    /// 1 once the drain flushed its synthesized commits (the session's
    /// `umount2` waits for it so no blocked caller still holds the mount).
    drained: futex.Word = .init(0),
    name_buf: thread_name.CommBuf = undefined,
    // ---- pass state (worker thread only) ----
    wake_cqe_seen: bool = false,
    tick_rescue_pending: bool = false,
    commits_pushed: u32 = 0,
    rescue_log_last_s: u64 = 0,
    cq_overflow_seen: u32 = 0,
    disconnected: bool = false,
    protocol_rejected: bool = false,
    /// This group's RELEASE SQEs pushed and not yet answered (the drain
    /// waits for 0 before the session may unmount).
    releases_pending: u32 = 0,

    pub const InitError = std.mem.Allocator.Error || error{ArenaMapFailed};
    /// `KmbufRingEnomem`: the kmbuf ring's contiguous-page allocation
    /// refused after every retry (`kmbuf.Error`), typed so the daemon's
    /// refusal can name the remedy.
    pub const StartError = error{ ThreadSpawnFailed, RingSetupFailed, KmbufRingEnomem };
    const RingError = error{ RingSetupFailed, KmbufRingEnomem };

    /// Allocate the group's tables and buffers (startup; nothing per op).
    pub fn init(gpa: std.mem.Allocator, shared: *Shared, index: u16, qids: []const u16, affinity: ?placement.CpuSet) InitError!*Group {
        const depth = shared.plan.depth;
        const gents: usize = @as(usize, qids.len) * depth;
        const payload_len: usize = shared.plan.payload_sz;
        const stride: usize = @intCast(derive.alignUp(payload_len, ARENA_ALIGN));
        // Width 1 under kmbuf/zc: the fixed index 0 / bgid 0 serve one queue.
        assert(!shared.usesKmbuf() or qids.len == 1);

        const self = try gpa.create(Group);
        errdefer gpa.destroy(self);
        self.* = .{
            .gpa = gpa,
            .shared = shared,
            .index = index,
            .members = &.{},
            .depth = depth,
            .payload_len = payload_len,
            .stride = stride,
            .arena = &.{},
            .headers = undefined,
            .iovs = &.{},
            .cells = undefined,
            .attached = &.{},
            .pends = &.{},
            .deadlines = undefined,
            .bridge_parked = &.{},
            .local_msgs = &.{},
            .held_len = &.{},
            .bounce_populated = &.{},
            .bounce_idle_tick = &.{},
            .fused = undefined,
            .frames = &.{},
            .extract_cells = &.{},
            .store_cells = &.{},
            .hooks = &.{},
            .extract_inbox = undefined,
            .store_inbox = undefined,
            .release_inbox = undefined,
            .release_teardown = &.{},
            .affinity = affinity,
        };
        const members = try gpa.alloc(Member, qids.len);
        errdefer gpa.free(members);
        var built: usize = 0;
        errdefer for (members[0..built]) |*m| {
            m.slots.deinit(gpa);
            gpa.free(m.parked);
        };
        for (qids, 0..) |qid, i| {
            const parked = try gpa.alloc(bool, depth);
            errdefer gpa.free(parked);
            @memset(parked, false);
            members[i] = .{ .qid = qid, .slots = try slot.SlotTable.init(gpa, qid, depth), .parked = parked, .initial_pending = depth };
            built += 1;
        }
        self.members = members;
        self.headers = kmbuf.Headers.init(@intCast(gents), shared.page) catch return error.ArenaMapFailed;
        errdefer self.headers.deinit();
        self.iovs = try gpa.alloc([2]std.posix.iovec, gents);
        errdefer gpa.free(self.iovs);
        if (!shared.usesKmbuf()) {
            // Born zeroed, page-aligned, non-aliasing; THP-hinted.
            const span = stride * gents;
            const rc = linux.mmap(null, span, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
            if (linux.errno(rc) != .SUCCESS) {
                log.err("transport: group {d}: mmap of the {d}-byte payload arena failed ({t})", .{ index, span, linux.errno(rc) });
                return error.ArenaMapFailed;
            }
            const base: [*]align(ARENA_ALIGN) u8 = @ptrFromInt(rc);
            self.arena = base[0..span];
            _ = linux.madvise(self.arena.ptr, self.arena.len, linux.MADV.HUGEPAGE);
        }
        errdefer if (self.arena.len > 0) {
            _ = linux.munmap(self.arena.ptr, self.arena.len);
        };
        for (0..gents) |g| {
            const h = self.headers.at(@intCast(g));
            h.* = abi.zeroed(abi.fuse_uring_req_header);
            // The kernel reads the ent's payload length at REGISTER.
            h.ring_ent_in_out.payload_sz = @intCast(payload_len);
            if (!shared.usesKmbuf()) {
                self.iovs[g] = .{
                    .{ .base = @ptrCast(h), .len = @sizeOf(abi.fuse_uring_req_header) },
                    .{ .base = self.arenaPtr(@intCast(g)), .len = payload_len },
                };
            }
        }
        self.cells = try commit_queue.Queue.init(gpa, gents, self.waker());
        errdefer self.cells.deinit(gpa);
        self.attached = try gpa.alloc(zc.Attachment, gents);
        errdefer gpa.free(self.attached);
        @memset(self.attached, .{});
        self.pends = try gpa.alloc(?Pend, gents);
        errdefer gpa.free(self.pends);
        @memset(self.pends, null);
        self.deadlines = try zc.Deadlines.init(gpa, gents);
        errdefer self.deadlines.deinit(gpa);
        self.bridge_parked = try gpa.alloc(ParkedReply, gents);
        errdefer gpa.free(self.bridge_parked);
        @memset(self.bridge_parked, .none);
        self.local_msgs = try gpa.alloc(commit_queue.Msg, gents);
        errdefer gpa.free(self.local_msgs);
        self.held_len = try gpa.alloc(u32, gents);
        errdefer gpa.free(self.held_len);
        @memset(self.held_len, 0);
        self.bounce_populated = try gpa.alloc(u32, gents);
        errdefer gpa.free(self.bounce_populated);
        @memset(self.bounce_populated, 0);
        self.bounce_idle_tick = try gpa.alloc(u64, gents);
        errdefer gpa.free(self.bounce_idle_tick);
        @memset(self.bounce_idle_tick, 0);
        self.fused = try zc.FusedQueue.init(gpa, gents);
        errdefer self.fused.deinit(gpa);
        self.frames = try gpa.alloc(FusedFrame, gents);
        errdefer gpa.free(self.frames);
        self.extract_cells = try gpa.alloc(ExtractCell, gents);
        errdefer gpa.free(self.extract_cells);
        @memset(self.extract_cells, .{});
        self.extract_inbox = try mpmc.BoundedMpmc(u32).init(gpa, gents);
        errdefer self.extract_inbox.deinit(gpa);
        self.store_cells = try gpa.alloc(StoreCell, gents);
        errdefer gpa.free(self.store_cells);
        @memset(self.store_cells, .{});
        self.hooks = try gpa.alloc(?fsv.CompletionHook, gents);
        errdefer gpa.free(self.hooks);
        @memset(self.hooks, null);
        self.store_inbox = try mpmc.BoundedMpmc(u32).init(gpa, gents);
        errdefer self.store_inbox.deinit(gpa);
        self.release_inbox = try mpmc.BoundedMpmc(u32).init(gpa, gents);
        errdefer self.release_inbox.deinit(gpa);
        self.release_teardown = try gpa.alloc(bool, gents);
        @memset(self.release_teardown, false);
        return self;
    }

    /// After `join` (or a never-started group) and after every handler that
    /// could hold a lease has finished.
    pub fn deinit(self: *Group) void {
        assert(self.thread == null);
        if (self.started.load(.acquire) == 1) self.ring.deinit();
        self.gpa.free(self.release_teardown);
        self.release_inbox.deinit(self.gpa);
        self.store_inbox.deinit(self.gpa);
        self.gpa.free(self.hooks);
        self.gpa.free(self.store_cells);
        self.extract_inbox.deinit(self.gpa);
        self.gpa.free(self.extract_cells);
        self.gpa.free(self.frames);
        self.fused.deinit(self.gpa);
        for (self.bounce_populated) |b| zc_bounce_resident_bytes.sub(b);
        self.gpa.free(self.bounce_idle_tick);
        self.gpa.free(self.bounce_populated);
        self.gpa.free(self.held_len);
        self.gpa.free(self.local_msgs);
        self.gpa.free(self.bridge_parked);
        self.deadlines.deinit(self.gpa);
        self.gpa.free(self.pends);
        self.gpa.free(self.attached);
        self.cells.deinit(self.gpa);
        if (self.arena.len > 0) _ = linux.munmap(self.arena.ptr, self.arena.len);
        self.gpa.free(self.iovs);
        self.headers.deinit();
        for (self.members) |*m| {
            m.slots.deinit(self.gpa);
            self.gpa.free(m.parked);
        }
        self.gpa.free(self.members);
        self.gpa.destroy(self);
    }

    pub fn arenaBytes(self: *const Group) u64 {
        return self.arena.len;
    }

    fn arenaPtr(self: *Group, gent: u32) [*]u8 {
        return self.arena.ptr + @as(usize, gent) * self.stride;
    }

    /// The ent's payload window the handler serves into / the worker
    /// applies a reply into: the arena buffer (UserEnts); the attached kmbuf
    /// buffer (BufRing, and a zc slot with no paged body); the bounce window
    /// (a zc slot holding the app's READ destination pages — the body is
    /// staged here and bridged into the slot). Empty when a kmbuf ent has
    /// no attachment (a payload-less request: its reply is header-only).
    pub fn window(self: *Group, gent: u32) []u8 {
        switch (self.shared.mode) {
            .user_ents => return self.arenaPtr(gent)[0..self.payload_len],
            .buf_ring => return self.attachedWindow(gent),
            .zero_copy => {
                const m = self.memberOf(gent);
                const e = self.entIdxOf(gent);
                if (m.slots.ent(e).dir == .dest) return self.bounce(gent);
                return self.attachedWindow(gent);
            },
        }
    }

    fn attachedWindow(self: *Group, gent: u32) []u8 {
        const k = &(self.kmb orelse return &.{});
        return self.attached[gent].window(k.region, k.buf_size, self.payload_len);
    }

    /// The gent's bounce window in the session slab (zc only).
    fn bounce(self: *Group, gent: u32) []u8 {
        const s = self.shared.slab orelse return &.{};
        return s.window(self.memberOf(gent).qid, self.entIdxOf(gent), self.payload_len);
    }

    fn bounceOffset(self: *Group, gent: u32) u64 {
        return self.shared.slab.?.offsetOf(self.memberOf(gent).qid, self.entIdxOf(gent));
    }

    /// The window's resident high-water rises to `len` (page-rounded,
    /// capped at the stride); the gauge takes the delta. Worker thread.
    fn noteBouncePopulated(self: *Group, gent: u32, len: u32) void {
        const stride: u32 = @intCast(self.shared.slab.?.stride);
        const rounded: u32 = @intCast(@min(derive.alignUp(len, self.shared.page), stride));
        const cur = self.bounce_populated[gent];
        if (rounded <= cur) return;
        self.bounce_populated[gent] = rounded;
        zc_bounce_resident_bytes.add(rounded - cur);
    }

    /// The sampler's face of the idle-trim law (`mem_budget.TrimFn`): ask
    /// the pass to punch idle windows and wake the worker (an idle group
    /// is parked in its enter). Any thread.
    pub fn requestBounceTrim(self: *Group, min_idle_ticks: u64) void {
        if (self.shared.slab == null) return;
        self.bounce_trim_request.store(min_idle_ticks + 1, .release);
        if (self.started.load(.acquire) == 1) _ = self.ring.wake();
    }

    /// The pass half: punch every populated window whose ent owes nothing
    /// (registered, replied or retained — no handler holds its window, no
    /// bridge moves through it) and went idle ≥ `min` ticks ago. Worker
    /// thread only: the ent states are its own.
    fn trimIdleWindows(self: *Group) void {
        const req = self.bounce_trim_request.swap(0, .acquire);
        if (req == 0) return;
        const min_idle_ticks = req - 1;
        const sl = self.shared.slab orelse return;
        const now = mem_budget.authority.ticks();
        for (self.members, 0..) |*m, ms| {
            var e: u16 = 0;
            while (e < self.depth) : (e += 1) {
                const gent = self.gentOf(@intCast(ms), e);
                const populated = self.bounce_populated[gent];
                if (populated == 0 or self.pends[gent] != null) continue;
                const ent = m.slots.ent(e);
                if (ent.owing() != null or ent.lease.leased()) continue;
                if (now -| self.bounce_idle_tick[gent] < min_idle_ticks) continue;
                if (!sl.punch(m.qid, e)) continue;
                self.bounce_populated[gent] = 0;
                zc_bounce_resident_bytes.sub(populated);
                zc_bounce_trimmed_bytes.add(populated);
            }
        }
    }

    pub fn gentOf(self: *const Group, member_slot: u32, ent_idx: u16) u32 {
        return member_slot * self.depth + ent_idx;
    }

    fn memberOf(self: *Group, gent: u32) *Member {
        return &self.members[gent / self.depth];
    }

    fn entIdxOf(self: *const Group, gent: u32) u16 {
        return @intCast(gent % self.depth);
    }

    fn headerOf(self: *Group, gent: u32) *abi.fuse_uring_req_header {
        return self.headers.at(gent);
    }

    /// The foreign face of this group's wake (lease drops, commit cells).
    pub fn waker(self: *Group) slot.Waker {
        return .{ .ctx = self, .wake = wakeFromForeign };
    }

    fn wakeFromForeign(ctx: ?*anyopaque) bool {
        const self: *Group = @ptrCast(@alignCast(ctx.?));
        return self.ring.wake();
    }

    /// The unconditional wake (shutdown): the worker must wake to read
    /// `active == false` whatever the coalescer says.
    pub fn forceWake(self: *Group) void {
        if (self.started.load(.acquire) == 1) self.ring.forceWake();
    }

    // ---- lifecycle ----

    /// Spawn the thread; returns once its ring stands and every REGISTER
    /// is pushed (or the ring was refused).
    pub fn start(self: *Group) StartError!void {
        assert(self.thread == null);
        self.thread = std.Thread.spawn(.{}, threadMain, .{self}) catch return error.ThreadSpawnFailed;
        while (self.started.load(.acquire) == 0) futex.wait(&self.started, 0);
        switch (self.started.load(.acquire)) {
            1 => {},
            else => |failed| {
                self.thread.?.join();
                self.thread = null;
                return if (failed == 3) error.KmbufRingEnomem else error.RingSetupFailed;
            },
        }
    }

    /// Join the worker (the session flipped `active` and force-woke it).
    pub fn join(self: *Group) void {
        const t = self.thread orelse return;
        t.join();
        self.thread = null;
    }

    pub fn owedCount(self: *const Group) u32 {
        var n: u32 = 0;
        for (self.members) |*m| n += m.slots.owedCount();
        return n;
    }

    fn threadMain(self: *Group) void {
        var base: [24]u8 = undefined;
        _ = thread_name.nameCurrentThread(&self.name_buf, std.fmt.bufPrint(&base, "sqz-fuse-ur{d}", .{self.members[0].qid}) catch unreachable);
        if (builtin.is_test) log.testing.expectErrorLogs(self.shared.test_error_logs);
        self.applyAffinity();
        self.initRing() catch |e| {
            _ = self.shared.barrier.complete(.failed);
            self.started.store(if (e == error.KmbufRingEnomem) 3 else 2, .release);
            futex.wakeAll(&self.started);
            return;
        };
        self.registerAll();
        self.started.store(1, .release);
        futex.wakeAll(&self.started);
        self.runLoop();
        self.drain();
        if (self.kmb) |*k| k.teardown(&self.ring.ring);
        self.kmb = null;
        self.ring.quiesce();
    }

    fn applyAffinity(self: *Group) void {
        const set = self.affinity orelse return;
        var cpu_set: linux.cpu_set_t = @splat(0);
        var it = set.iterator(.{});
        while (it.next()) |cpu| {
            cpu_set[cpu / @bitSizeOf(usize)] |= @as(usize, 1) << @intCast(cpu % @bitSizeOf(usize));
        }
        // Best-effort: a refused mask leaves the worker where the scheduler put it.
        linux.sched_setaffinity(0, &cpu_set) catch {};
    }

    /// The SQPOLL ladder never composes with DEFER_TASKRUN (the poller owns
    /// the task work); the lever is "measured not recommended" and exists as
    /// the control arm only.
    const sqpoll_ladder = [_]u32{
        linux.IORING_SETUP_SQPOLL | linux.IORING_SETUP_SINGLE_ISSUER,
        linux.IORING_SETUP_SQPOLL,
    };

    fn initRing(self: *Group) RingError!void {
        const entries: u16 = @intCast(geometry.sqEntries(@intCast(self.members.len), self.depth));
        const ladder: []const u32 = if (self.shared.sqpoll) &sqpoll_ladder else &uring128.taskrun_ladder_flags;
        self.ring = uring128.ParkedRing128.initProbing(self.gpa, entries, ladder, &self.ring_rung) catch return error.RingSetupFailed;
        errdefer self.ring.deinit();
        // Fixed files `[fuse_fd, wake_eventfd]`: the uring command needs
        // `Fixed(0)`; a refusal fails the worker — and the mount.
        self.ring.ring.registerFiles(&.{ self.shared.fuse_fd, self.ring.event_fd }) catch |e| {
            log.err("transport: group {d}: register_files([fuse_fd, eventfd]) refused ({s}) — the ring cannot carry the FUSE command", .{ self.index, @errorName(e) });
            return error.RingSetupFailed;
        };
        if (self.shared.usesKmbuf()) {
            // Post-probe: a refusal here fails the mount loud (charter §3);
            // the ENOMEM class keeps its type (kmbuf logged the remedy).
            self.kmb = kmbuf.Kmbuf.setup(&self.ring.ring, self.shared.io, &self.headers, self.shared.kmbuf_opcodes.?, self.shared.zeroCopy(), self.shared.plan.payload_sz, self.shared.page, self.shared.test_kmbuf_enomem) catch |e| {
                if (e == error.KmbufRingEnomem) return error.KmbufRingEnomem;
                log.err("transport: group {d}: the {t} buffer setup refused ({s}) after the probe confirmed it — the mount fails", .{ self.index, self.shared.mode, @errorName(e) });
                return error.RingSetupFailed;
            };
        }
        if (!self.ring.armWake()) return error.RingSetupFailed;
        const posture = if (self.shared.sqpoll) "sqpoll" else uring128.taskrun_ladder[self.ring_rung].name;
        log.info("transport: group {d} (qids {d}..{d}, {d} members × depth {d}, {t}) ring armed on rung {d} ({s}), sq {d}, payload {d} B, arena {d} B, kmbuf {d} B", .{ self.index, self.members[0].qid, self.members[self.members.len - 1].qid, self.members.len, self.depth, self.shared.mode, self.ring_rung, posture, entries, self.payload_len, self.arena.len, if (self.kmb) |k| k.pinnedBytes() else 0 });
    }

    // ---- SQE pushes (append only; the loop-bottom enter flushes) ----

    fn takeSqe(self: *Group) ?*uring128.Sqe128 {
        return self.ring.getSqe() orelse {
            sq_full.fire("transport ring SQ full after a flush");
            return null;
        };
    }

    /// Push one REGISTER (or answer the test seam's refusal) and account
    /// the arm barrier: a queue counts once every ent's first REGISTER push
    /// happened — SUBMISSIONS, never completions (module doc).
    fn pushRegister(self: *Group, member_slot: u32, ent_idx: u16) void {
        const m = &self.members[member_slot];
        const gent = self.gentOf(member_slot, ent_idx);
        const now = clock.monotonicNs();
        const refused = blk: {
            var left = self.shared.register_refuse_remaining.load(.monotonic);
            while (left > 0) {
                if (self.shared.register_refuse_remaining.cmpxchgWeak(left, left - 1, .acq_rel, .monotonic)) |actual| {
                    left = actual;
                    continue;
                }
                break :blk true;
            }
            break :blk false;
        };
        if (refused) {
            self.onRegisterRefused(member_slot, ent_idx, .BUSY, now);
            return;
        }
        const sqe = self.takeSqe() orelse {
            self.onRegisterRefused(member_slot, ent_idx, .AGAIN, now);
            return;
        };
        if (self.shared.usesKmbuf()) {
            const flags = zc.initFlags(self.shared.zeroCopy(), self.shared.retention);
            prepRegisterKmbuf(sqe, m.qid, ent_idx, flags, if (self.shared.zeroCopy()) @intCast(self.depth) else 0, gent);
        } else {
            prepRegister(sqe, m.qid, &self.iovs[gent], gent);
        }
        m.slots.onRegisterSubmitted(ent_idx);
        registers.inc();
        self.noteInitialPush(m);
    }

    fn noteInitialPush(self: *Group, m: *Member) void {
        if (m.counted) return;
        if (m.initial_pending > 0) m.initial_pending -= 1;
        if (m.initial_pending != 0) return;
        m.counted = true;
        const n = self.shared.queues_registered.fetchAdd(1, .acq_rel) + 1;
        if (n == self.shared.nqueues) _ = self.shared.barrier.complete(.armed);
    }

    fn onRegisterRefused(self: *Group, member_slot: u32, ent_idx: u16, errno: linux.E, now: u64) void {
        const m = &self.members[member_slot];
        switch (m.slots.noteRegisterFailure(ent_idx, now)) {
            .retry => |r| log.debug("transport: queue {d} ent {d}: REGISTER refused ({t}); retry in {d} ms", .{ m.qid, ent_idx, errno, r.after_ns / std.time.ns_per_ms }),
            .retire => {
                log.err("transport: queue {d} ent {d}: 8 consecutive REGISTER refusals ({t}) — ent retired", .{ m.qid, ent_idx, errno });
                if (m.slots.allRetired()) {
                    log.err("transport: queue {d}: every ent retired — the kernel routes by CPU, so this queue would strand its CPU; the session fails", .{m.qid});
                    _ = self.shared.shutdown();
                    _ = self.shared.barrier.complete(.failed);
                }
            },
        }
    }

    /// The initial REGISTER of every ent of every member, then ONE enter so
    /// the kernel holds them before the barrier reports this group.
    fn registerAll(self: *Group) void {
        for (self.members, 0..) |_, ms| {
            var e: u16 = 0;
            while (e < self.depth) : (e += 1) self.pushRegister(@intCast(ms), e);
        }
        self.flushNonBlocking();
    }

    fn pushCommit(self: *Group, gent: u32, commit_id: u64) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const sqe = self.takeSqe() orelse {
            m.slots.abandon(e);
            return;
        };
        prepCommit(sqe, m.qid, commit_id, gent, 0);
        m.slots.onCommitSubmitted(e);
        self.bounce_idle_tick[gent] = mem_budget.authority.ticks();
        ring_replies.inc();
        self.commits_pushed += 1;
    }

    /// The RETAIN commit: the request ends, the slot's pages stay
    /// registered, the cmd parks kernel-side (its CQE is the delivery after
    /// the RELEASE). A release that arrived early fires right behind it in
    /// the same batch (release never precedes its commit).
    fn pushCommitRetain(self: *Group, gent: u32, commit_id: u64) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const sqe = self.takeSqe() orelse {
            m.slots.abandon(e);
            return;
        };
        prepCommit(sqe, m.qid, commit_id, gent, abi.FUSE_URING_COMMIT_RETAIN);
        m.slots.onCommitSubmittedRetained(e);
        self.bounce_idle_tick[gent] = mem_budget.authority.ticks();
        ring_replies.inc();
        self.commits_pushed += 1;
        if (m.slots.ent(e).release_pending) self.pushRelease(gent, commit_id, .handler);
    }

    const ReleaseOrigin = enum { handler, teardown };

    fn pushRelease(self: *Group, gent: u32, commit_id: u64, origin: ReleaseOrigin) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const sqe = self.takeSqe() orelse return;
        prepRelease(sqe, m.qid, commit_id, gent);
        m.slots.onReleaseSubmitted(e);
        self.release_teardown[gent] = origin == .teardown;
        self.releases_pending += 1;
        releases_inflight.add(1);
    }

    /// RELEASE every retained slot of this group — the drain's half of the
    /// retention law: a retained ent the kernel reclaims itself pins the
    /// exiting process. Idempotent: a released slot is `replied`.
    fn releaseRetainedAll(self: *Group) void {
        for (self.members, 0..) |*m, ms| {
            for (m.slots.ents, 0..) |*ent, e| switch (ent.state) {
                .retained => |cid| self.pushRelease(self.gentOf(@intCast(ms), @intCast(e)), cid, .teardown),
                else => {},
            };
        }
    }

    /// `fail_ent`: the header-only `-errno` for an owing slot; nothing
    /// otherwise (THE LAW's non-reply exit). A parked cell or bridge is
    /// dropped with the synthesis.
    fn failEnt(self: *Group, gent: u32, errno: linux.E) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        switch (m.slots.failEnt(e)) {
            .synthesize => |o| {
                if (m.parked[e] or self.bridge_parked[gent] == .cell) {
                    m.parked[e] = false;
                    self.cells.recycle(gent);
                }
                self.bridge_parked[gent] = .none;
                slot.applyErrorHeader(self.headerOf(gent), o.unique, errno);
                self.pushCommit(gent, o.commit_id);
            },
            .nothing => {},
        }
    }

    /// The message a bridge-parked reply came from.
    fn parkedMsg(self: *Group, gent: u32) ?*const commit_queue.Msg {
        return switch (self.bridge_parked[gent]) {
            .none => null,
            .cell => self.cells.msg(gent),
            .local => &self.local_msgs[gent],
        };
    }

    /// The bridged reply landed (or fell back to a plain commit): release
    /// what parked it.
    fn resumeParked(self: *Group, gent: u32) void {
        if (self.bridge_parked[gent] == .cell) self.cells.recycle(gent);
        self.bridge_parked[gent] = .none;
    }

    // ---- bridges (porting spec kmbuf §7) ----

    fn stallSource(self: *Group) ?linux.fd_t {
        var left = self.shared.bridge_stall_remaining.load(.monotonic);
        while (left > 0) {
            if (self.shared.bridge_stall_remaining.cmpxchgWeak(left, left - 1, .acq_rel, .monotonic)) |actual| {
                left = actual;
                continue;
            }
            return self.shared.stall_pipe[0];
        }
        return null;
    }

    /// Push one bridge on this ring and stamp its deadline. At most ONE pend
    /// per ent: a second is refused (the caller falls back). `fetch` moves
    /// fd → slot, else slot → fd.
    fn pushBridge(self: *Group, gent: u32, pend: Pend, fd: linux.fd_t, off: u64, fetch: bool) bool {
        if (self.pends[gent] != null) return false;
        if (pend.len == 0) return false;
        const sqe = self.takeSqe() orelse return false;
        var src_fd = fd;
        var src_off = off;
        if (fetch) if (self.stallSource()) |pipe_rd| {
            src_fd = pipe_rd;
            src_off = 0;
        };
        const e = self.entIdxOf(gent);
        if (fetch) prepBridgeFetch(sqe, src_fd, src_off, pend.len, e, gent) else prepBridgeStore(sqe, src_fd, src_off, pend.len, e, gent);
        // Every bounce bridge names the window bytes in play (a staged body
        // read out of it, an extraction written into it).
        if (self.shared.slab) |sl| if (fd == sl.fd) self.noteBouncePopulated(gent, pend.len);
        self.pends[gent] = pend;
        if (self.deadlines.stamp(gent, clock.monotonicNs())) zc_bridge_pends.add(1);
        zc_bridges.inc();
        return true;
    }

    fn clearPend(self: *Group, gent: u32) ?Pend {
        const p = self.pends[gent] orelse return null;
        self.pends[gent] = null;
        if (self.deadlines.live(gent)) {
            self.deadlines.clear(gent);
            zc_bridge_pends.sub(1);
        }
        return p;
    }

    /// Stage `body` in the bounce (unless it already IS the bounce) and
    /// bridge it into the slot; the commit parks until the CQE. Returns
    /// false when no bridge could be pushed.
    /// `zero_pad` (a short paged READ): the bounce holds zeros past the
    /// body up to it; the bridge moves the padded length into the
    /// installed pages (the kernel copy path's page_zeroing) while the
    /// commit's payload length stays the body's.
    fn bridgeBounce(self: *Group, gent: u32, body: []const u8, readdir_probe: bool, zero_pad: u32) bool {
        const win = self.bounce(gent);
        if (body.len > win.len) return false;
        if (body.ptr != win.ptr) {
            @memcpy(win[0..body.len], body);
            zc_bounce_copies.inc();
        }
        const dma: u32 = @intCast(@min(win.len, @max(body.len, zero_pad)));
        if (dma > body.len) @memset(win[body.len..dma], 0);
        return self.pushBridge(gent, .{ .kind = .reply_bounce, .len = dma, .reply_len = @intCast(body.len), .readdir_probe = readdir_probe }, self.shared.slab.?.fd, self.bounceOffset(gent), true);
    }

    /// The bridge deadline scan (every `BRIDGE_SCAN_PERIOD` while a pend is
    /// live): each overdue ent gets ONE AsyncCancel and the tripwire; a live
    /// pend with no stamp is an orphan, re-stamped loud.
    fn deadlineScan(self: *Group, now: u64) void {
        if (self.deadlines.outstanding == 0) {
            var any = false;
            for (self.pends) |p| if (p != null) {
                any = true;
            };
            if (!any) return;
        }
        if (now -| self.last_scan_ns < zc.BRIDGE_SCAN_PERIOD_NS) return;
        self.last_scan_ns = now;
        for (self.pends, 0..) |p, g| if (p != null and !self.deadlines.live(g)) {
            bridge_orphans.fire("a live bridge pend had no deadline stamp");
            if (self.deadlines.stamp(g, now)) zc_bridge_pends.add(1);
        };
        scan_max_age_ms.set(self.deadlines.maxAgeNs(now) / std.time.ns_per_ms);
        while (self.deadlines.nextOverdue(now, self.shared.bridge_deadline_ns)) |g| {
            bridge_cancels.fire("a zc bridge exceeded its deadline; AsyncCancel issued");
            const gent: u32 = @intCast(g);
            log.err("transport: queue {d} ent {d}: {t} bridge of {d} bytes outstanding {d} ms — cancelling", .{ self.memberOf(gent).qid, self.entIdxOf(gent), self.pends[g].?.kind, self.pends[g].?.len, (now -| self.deadlines.born_ns[g]) / std.time.ns_per_ms });
            const sqe = self.takeSqe() orelse continue;
            uring128.prepAsyncCancel(sqe, .{ .user_data = tagOf(.bridge, gent) });
            sqe.user_data = tagOf(.cancel, gent);
        }
    }

    /// A bridge CQE: resolve the pend by its kind.
    fn handleBridgeCqe(self: *Group, gent: u32, res: i32) void {
        const pend = self.clearPend(gent) orelse return;
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const ok = res >= 0 and @as(u32, @intCast(res)) == pend.len;
        const efault = res == -@as(i32, @intCast(@backingInt(linux.E.FAULT)));
        if (pend.readdir_probe and pend.kind == .reply_bounce) self.shared.latchReaddir(if (ok) .paged else if (efault) .copyable else .unresolved);
        switch (pend.kind) {
            .reply_bounce, .reply_fd => {
                const msg = self.parkedMsg(gent) orelse {
                    if (pend.hook) |h| _ = h.run(ok);
                    return;
                };
                if (ok) {
                    self.commitPrefilled(gent, msg.header.unique, pend.replyLen());
                    if (pend.kind == .reply_fd) {
                        zc_fd_body_replies.inc();
                        zc_serve_bytes.add(pend.len);
                    } else zc_bounce_bytes.add(pend.len);
                    self.resumeParked(gent);
                    if (pend.hook) |h| _ = h.run(true);
                    return;
                }
                if (pend.kind == .reply_fd) {
                    // Restage the keepalive body through the bounce (the
                    // view is pinned until the hook runs, AFTER the copy).
                    zc_fd_body_fallbacks.inc();
                    const restaged = self.bridgeBounce(gent, msg.body, false, msg.zero_pad);
                    if (pend.hook) |h| _ = h.run(false);
                    if (restaged) return;
                }
                if (pend.readdir_probe and efault) {
                    // The kernel never registered the slot: the body goes to
                    // the kmbuf attachment like any copyable reply.
                    const win = self.attachedWindow(gent);
                    if (win.len > 0 and slot.applyReply(self.headerOf(gent), win, msg.asReply())) {
                        self.pushCommit(gent, msg.slot.commit_id);
                        self.resumeParked(gent);
                        return;
                    }
                }
                zc_fallbacks.inc();
                log.warn("transport: queue {d} ent {d}: {t} bridge of {d} bytes answered {d} — header-only EIO", .{ m.qid, e, pend.kind, pend.len, res });
                self.failEnt(gent, .IO);
            },
            .direct_leg => {
                // The device moved `res` bytes whatever the slot's fate.
                if (res > 0) zc_direct_leg_dma_bytes.add(@intCast(res));
                if (m.slots.ent(e).owing() == null) {
                    if (pend.hook) |h| _ = h.run(ok);
                    return;
                }
                // THE LAW: the post-DMA recheck (incarnation + binding) runs
                // AFTER the bytes landed; a failed recheck WITHDRAWS the
                // prefilled reply — the ordinary ladder re-serves the whole
                // body (its bounce overwrites whatever landed), never
                // unvalidated device bytes in the app's pages.
                const validated = if (pend.hook) |h| h.run(ok) else true;
                if (ok and validated) {
                    zc_direct_legs.inc();
                    zc_serve_bytes.add(pend.len);
                    zc_direct_leg_serve_bytes.add(pend.len);
                    const unique = std.mem.readInt(u64, self.headerOf(gent).in_out[8..16], .little);
                    self.commitPrefilled(gent, unique, pend.len);
                    return;
                }
                if (ok) zc_direct_leg_recheck_withdrawn.inc() else zc_direct_leg_fallbacks.inc();
                self.redeliver(gent);
            },
            .write_extract => {
                if (ok) {
                    slot.zc_write_extractions.inc();
                    slot.zc_write_extract_bytes.add(pend.len);
                    self.deliverExtracted(gent, pend.len);
                    return;
                }
                log.warn("transport: queue {d} ent {d}: WRITE extraction of {d} bytes answered {d} — EIO", .{ m.qid, e, pend.len, res });
                self.failEnt(gent, .IO);
            },
            .lazy_extract => {
                const cell = &self.extract_cells[gent];
                if (ok) {
                    slot.zc_write_extractions.inc();
                    slot.zc_write_extract_bytes.add(pend.len);
                    slot.zc_write_lazy_extractions.inc();
                    cell.done_len = pend.len;
                }
                cell.res = res;
                cell.event.set(self.shared.io);
            },
            .direct_store => {
                const cell = &self.store_cells[gent];
                if (ok) {
                    zc_write_directs.inc();
                    zc_write_direct_bytes.add(pend.len);
                } else {
                    zc_write_direct_failures.inc();
                }
                cell.res = res;
                cell.event.set(self.shared.io);
            },
        }
    }

    /// The cancel's OWN CQE: the four-arm ladder.
    fn handleCancelCqe(self: *Group, gent: u32, res: i32) void {
        switch (zc.cancelAction(res, self.pends[gent] != null)) {
            .nothing, .await_original => {},
            .synthesize_lost => {
                bridge_lost.fire("a bridge completion was posted but never reaped; synthesized");
                self.handleBridgeCqe(gent, -@as(i32, @intCast(@backingInt(linux.E.TIMEDOUT))));
            },
            .restamp => {
                bridge_restamps.inc();
                log.err("transport: queue {d} ent {d}: cancel answered {d} — the bridge still runs kernel-side; re-stamped (a synthesis would let a late DMA alias a recycled slot)", .{ self.memberOf(gent).qid, self.entIdxOf(gent), res });
                _ = self.deadlines.stamp(gent, clock.monotonicNs());
            },
        }
    }

    // ---- the handlers' bridge requests (lazy extraction, release) ----

    /// The held slot's face (`slot.HeldFace.extract`): any thread. Posts
    /// the gent, wakes the worker, parks on the cell's event.
    fn extractFromForeign(ctx: ?*anyopaque, gent: u32, io: Io) slot.ExtractError![]const u8 {
        const self: *Group = @ptrCast(@alignCast(ctx.?));
        const cell = &self.extract_cells[gent];
        if (cell.done_len != 0) return self.bounce(gent)[0..cell.done_len];
        if (!self.shared.isActive()) return error.SessionClosed;
        cell.event.reset();
        cell.res = 0;
        if (!self.extract_inbox.tryPush(gent)) return error.BridgeFailed;
        if (self.ring.wake()) slot.wake_writes.inc() else slot.wakes_elided.inc();
        cell.event.wait(io) catch return error.SessionClosed;
        if (cell.done_len != 0) return self.bounce(gent)[0..cell.done_len];
        return error.BridgeFailed;
    }

    fn heldFace(self: *Group) slot.HeldFace {
        return .{ .ctx = self, .extract = extractFromForeign };
    }

    /// The direct store of a HELD WRITE payload (`WRITE_FIXED(slot → fd @
    /// off)`, the sole-owner patch class): any thread. Posts the gent,
    /// wakes the worker, parks on the store cell's event; the verdict is
    /// the bridge CQE's (a short or failed DMA is the caller's EIO — the
    /// bytes still sit in the slot for its fallback `extract`).
    pub fn storeFromForeign(self: *Group, gent: u32, fd: linux.fd_t, off: u64, len: u32, io: Io) slot.ExtractError!void {
        if (!self.shared.isActive()) return error.SessionClosed;
        const cell = &self.store_cells[gent];
        cell.event.reset();
        cell.res = 0;
        cell.fd = fd;
        cell.off = off;
        cell.len = len;
        if (!self.store_inbox.tryPush(gent)) return error.BridgeFailed;
        if (self.ring.wake()) slot.wake_writes.inc() else slot.wakes_elided.inc();
        cell.event.wait(io) catch return error.SessionClosed;
        if (cell.res >= 0 and @as(u32, @intCast(cell.res)) == len) return;
        return error.BridgeFailed;
    }

    /// Pump the direct stores: `WRITE_FIXED(slot → device)` for a held slot
    /// still owing (the pages stay registered until the commit); refusals
    /// answer the cell now.
    fn pumpStores(self: *Group) void {
        while (self.store_inbox.tryPop()) |gent| {
            const m = self.memberOf(gent);
            const e = self.entIdxOf(gent);
            const ent = m.slots.ent(e);
            const cell = &self.store_cells[gent];
            const live = ent.owing() != null or ent.state == .retained;
            if (!ent.held or !live or cell.len == 0 or cell.len > self.held_len[gent]) {
                cell.res = -@as(i32, @intCast(@backingInt(linux.E.NOENT)));
                cell.event.set(self.shared.io);
                continue;
            }
            if (!self.pushBridge(gent, .{ .kind = .direct_store, .len = cell.len }, cell.fd, cell.off, false)) {
                cell.res = -@as(i32, @intCast(@backingInt(linux.E.BUSY)));
                cell.event.set(self.shared.io);
            }
        }
    }

    /// A RELEASE from the handler (`Session.releaseRetained`): any thread.
    pub fn requestRelease(self: *Group, gent: u32) void {
        if (!self.release_inbox.tryPush(gent)) return;
        if (self.ring.wake()) slot.wake_writes.inc() else slot.wakes_elided.inc();
    }

    /// Pump the lazy extractions: `WRITE_FIXED(slot → slab)` for a held
    /// slot (delivered, parked or retained — the pages stay registered
    /// until the plain commit or the RELEASE); refusals answer the cell now.
    fn pumpExtracts(self: *Group) void {
        while (self.extract_inbox.tryPop()) |gent| {
            const m = self.memberOf(gent);
            const e = self.entIdxOf(gent);
            const ent = m.slots.ent(e);
            const cell = &self.extract_cells[gent];
            const live = ent.owing() != null or ent.state == .retained;
            if (!ent.held or !live) {
                cell.res = -@as(i32, @intCast(@backingInt(linux.E.NOENT)));
                cell.event.set(self.shared.io);
                continue;
            }
            if (!self.pushBridge(gent, .{ .kind = .lazy_extract, .len = self.held_len[gent] }, self.shared.slab.?.fd, self.bounceOffset(gent), false)) {
                cell.res = -@as(i32, @intCast(@backingInt(linux.E.BUSY)));
                cell.event.set(self.shared.io);
            }
        }
    }

    /// Pump the releases AFTER the pass's commits: a retained slot releases
    /// now; a slot still owing (its RETAIN commit not yet pushed) parks the
    /// release behind that commit; anything else is inert.
    fn pumpReleases(self: *Group) void {
        while (self.release_inbox.tryPop()) |gent| {
            const m = self.memberOf(gent);
            const e = self.entIdxOf(gent);
            const ent = m.slots.ent(e);
            switch (ent.state) {
                .retained => |cid| self.pushRelease(gent, cid, .handler),
                .delivered, .parked => ent.release_pending = true,
                else => zc_release_inert.inc(),
            }
        }
    }

    /// A RELEASE CQE: 0 is the kernel's acceptance; a disconnect errno
    /// means the connection died first and the kernel's drain owns the
    /// slot (counted, never the tripwire); anything else is the
    /// must-stay-0 failure (ENOENT a double release, EBUSY a live commit).
    fn handleReleaseCqe(self: *Group, gent: u32, res: i32) void {
        self.releases_pending -|= 1;
        releases_inflight.sub(1);
        if (res == 0) {
            if (self.release_teardown[gent]) retained_released_at_teardown.inc();
            return;
        }
        cqe_errors.inc();
        const e: linux.E = @fromBackingInt(@as(u16, @intCast(-res)));
        if (isDisconnect(e)) {
            retained_kernel_drained.inc();
            self.disconnected = true;
            return;
        }
        self.releaseFailure(gent, e);
    }

    // ---- the commit path ----

    /// Header only with `payload_sz = n`: the body already sits in the slot.
    fn commitPrefilled(self: *Group, gent: u32, unique: u64, n: u32) void {
        const hdr = self.headerOf(gent);
        hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(codec.okHeader(unique, n));
        hdr.ring_ent_in_out.payload_sz = n;
        zc_replies.inc();
        const cid = self.memberOf(gent).slots.ent(self.entIdxOf(gent)).owing().?.commit_id;
        self.pushCommit(gent, cid);
    }

    /// `parked` names where `msg` lives should a bridge park it.
    fn commitReady(self: *Group, gent: u32, msg: *const commit_queue.Msg, parked: ParkedReply) void {
        if (self.shared.zeroCopy()) return self.commitReadyZc(gent, msg, parked);
        const hdr = self.headerOf(gent);
        const payload = self.window(gent);
        switch (msg.kind) {
            .reply, .reply_retain => {
                if (msg.kind == .reply_retain) slot.retain_refused_class.inc();
                _ = slot.applyReply(hdr, payload, msg.asReply());
            },
            .fd_body => {
                // No zc slot to bridge into: the body view is the reply.
                commit_queue.fd_body_fallbacks.inc();
                _ = slot.applyReply(hdr, payload, msg.asReply());
            },
            .prefilled => {
                if (msg.prefilled > payload.len) {
                    slot.replies_oversize.fire("prefilled length larger than the ent payload");
                    slot.applyErrorHeader(hdr, msg.header.unique, .IO);
                } else {
                    hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(msg.header);
                    hdr.ring_ent_in_out.payload_sz = msg.prefilled;
                }
            },
            .fail => slot.applyErrorHeader(hdr, msg.header.unique, msg.errno),
            .release => unreachable, // releases ride their own inbox
        }
        self.pushCommit(gent, msg.slot.commit_id);
    }

    /// The zero-copy commit routing (porting spec kmbuf §4.3–4.4, §5.3):
    /// a paged READ body moves through the slot (bridged: the commit parks
    /// until the CQE); a WRITE slot takes no body (the direction law); a
    /// RETAIN commit only for a held `.page_cache` WRITE on a
    /// retention-armed queue; everything else lands in the kmbuf attachment.
    fn commitReadyZc(self: *Group, gent: u32, msg: *const commit_queue.Msg, parked: ParkedReply) void {
        const hdr = self.headerOf(gent);
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const ent = m.slots.ent(e);
        const dir = ent.dir;
        switch (msg.kind) {
            .fail => {
                slot.applyErrorHeader(hdr, msg.header.unique, msg.errno);
                self.pushCommit(gent, msg.slot.commit_id);
            },
            .prefilled => {
                if (dir != .dest or msg.prefilled > self.payload_len) {
                    slot.zc_direction_refusals.inc();
                    log.err("transport: queue {d} ent {d}: a prefilled reply of {d} bytes on a {t} slot — header-only EIO", .{ m.qid, e, msg.prefilled, dir });
                    slot.applyErrorHeader(hdr, msg.header.unique, .IO);
                    self.pushCommit(gent, msg.slot.commit_id);
                    return;
                }
                hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(msg.header);
                hdr.ring_ent_in_out.payload_sz = msg.prefilled;
                zc_replies.inc();
                self.pushCommit(gent, msg.slot.commit_id);
            },
            .reply, .fd_body, .reply_retain => {
                const r = msg.asReply();
                if (dir == .dest) {
                    // A paged reply: header-extra bytes have nowhere to go.
                    if (r.out.len > 0) {
                        slot.zc_direction_refusals.inc();
                        slot.applyErrorHeader(hdr, msg.header.unique, .IO);
                        self.pushCommit(gent, msg.slot.commit_id);
                        return;
                    }
                    if (r.body.len == 0) {
                        // EOF / an empty page: header-only, NEVER a zero-length READ_FIXED.
                        hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(msg.header);
                        hdr.ring_ent_in_out.payload_sz = 0;
                        self.pushCommit(gent, msg.slot.commit_id);
                        return;
                    }
                    if (r.body.len > self.payload_len) {
                        slot.replies_oversize.fire("paged reply body larger than the ent payload");
                        slot.applyErrorHeader(hdr, msg.header.unique, .IO);
                        self.pushCommit(gent, msg.slot.commit_id);
                        return;
                    }
                    const probe = self.readdirProbe(gent);
                    const hook = self.hooks[gent];
                    self.hooks[gent] = null;
                    const pushed = if (msg.kind == .fd_body)
                        self.pushBridge(gent, .{ .kind = .reply_fd, .len = @intCast(r.body.len), .readdir_probe = probe, .hook = hook }, msg.fd, msg.fd_off, true)
                    else
                        self.bridgeBounce(gent, r.body, probe, r.zero_pad);
                    if (!pushed) {
                        if (hook) |h| _ = h.run(false);
                    }
                    if (pushed) {
                        // The commit FOLLOWS the bridge CQE: the slot stays
                        // owed (and refuses a second reply) until then.
                        m.slots.onCommitBridged(e);
                        self.bridge_parked[gent] = parked;
                        return;
                    }
                    zc_fallbacks.inc();
                    slot.applyErrorHeader(hdr, msg.header.unique, .IO);
                    self.pushCommit(gent, msg.slot.commit_id);
                    return;
                }
                if (dir == .source and r.body.len > 0) {
                    // The direction law: a READ_FIXED into a write slot is -EFAULT.
                    slot.zc_direction_refusals.inc();
                    log.err("transport: queue {d} ent {d}: a {d}-byte reply body for a WRITE slot — header-only EIO", .{ m.qid, e, r.body.len });
                    slot.applyErrorHeader(hdr, msg.header.unique, .IO);
                    self.pushCommit(gent, msg.slot.commit_id);
                    return;
                }
                if (msg.kind == .fd_body) commit_queue.fd_body_fallbacks.inc();
                const win = self.attachedWindow(gent);
                _ = slot.applyReply(hdr, win, r);
                if (msg.kind == .reply_retain) {
                    if (dir == .source and ent.held and ent.class == .page_cache and self.shared.retention) {
                        self.pushCommitRetain(gent, msg.slot.commit_id);
                        return;
                    }
                    slot.retain_refused_class.inc();
                    log.warn("transport: queue {d} ent {d}: replyRetain refused (dir {t}, held {}, class {t}, retention {}) — committed plain", .{ m.qid, e, dir, ent.held, ent.class, self.shared.retention });
                }
                self.pushCommit(gent, msg.slot.commit_id);
            },
            .release => unreachable,
        }
    }

    /// Whether this slot's bridge is the readdir class's first (the probe).
    fn readdirProbe(self: *Group, gent: u32) bool {
        if (self.shared.readdirLatch() != .unresolved) return false;
        const op = (abi.decodePrefix(abi.fuse_in_header, &self.headerOf(gent).in_out) orelse return false).op();
        return op == .readdir or op == .readdirplus;
    }

    /// One pumped cell: admit → the per-ent gate → commit now or park.
    fn handleCommitCell(self: *Group, gent: u32) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const msg = self.cells.msg(gent);
        if (msg.slot.qid != m.qid or msg.slot.ent_idx != e) {
            slot.replies_refused_stale.inc();
            self.cells.recycle(gent);
            return;
        }
        if (self.pends[gent] != null and self.pends[gent].?.kind == .direct_leg) {
            // The lane answered while a direct leg was still in flight (it
            // cannot: the leg is resolved before any dispatch) — refuse.
            slot.replies_refused_stale.inc();
            self.cells.recycle(gent);
            return;
        }
        switch (m.slots.admitCommit(e, msg.slot.commit_id)) {
            .refuse_stale => {
                log.warn("transport: queue {d} ent {d}: reply for commit {d} refused — the slot does not owe it (double, late or foreign reply)", .{ m.qid, e, msg.slot.commit_id });
                self.cells.recycle(gent);
            },
            .accept => switch (m.slots.ent(e).lease.tryCommit()) {
                .ready => {
                    self.commitReady(gent, msg, .cell);
                    if (self.bridge_parked[gent] == .none) self.cells.recycle(gent);
                },
                .parked => {
                    m.slots.onCommitParked(e);
                    m.parked[e] = true;
                },
            },
        }
    }

    fn unparkScan(self: *Group) void {
        for (self.members, 0..) |*m, ms| {
            for (m.parked, 0..) |p, e| {
                if (!p) continue;
                const ent_idx: u16 = @intCast(e);
                if (!m.slots.ent(ent_idx).lease.tryUnpark()) continue;
                const gent = self.gentOf(@intCast(ms), ent_idx);
                slot.unparked_commits.inc();
                m.parked[e] = false;
                m.slots.onUnparked(ent_idx);
                self.commitReady(gent, self.cells.msg(gent), .cell);
                if (self.bridge_parked[gent] == .none) self.cells.recycle(gent);
            }
        }
    }

    // ---- the fused handler poll (arm (b)) ----

    /// Poll ONE fused frame: the filesystem's try-only ladder once more;
    /// served ⇒ commit; else re-queue, or hand to the lane after
    /// `FUSED_POLL_MAX` polls.
    fn pollFused(self: *Group) void {
        const gent = self.fused.pop() orelse return;
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const f = &self.frames[gent];
        const owed = m.slots.ent(e).owing() orelse return;
        if (owed.commit_id != f.sref.commit_id) return;
        fused_polls.inc();
        f.polls += 1;
        const win = self.window(gent);
        if (self.shared.fs.probeRead(f.req, win)) |pr| {
            const reply: codec.Reply = switch (pr) {
                .reply => |r| r,
                .fd_source => |fs| blk: {
                    // The fused frame serves through the window: the copy arm.
                    const n = @min(fs.body.len, win.len);
                    @memcpy(win[0..n], fs.body[0..n]);
                    zc_fd_source_copies.inc();
                    if (fs.done) |h| _ = h.run(true);
                    break :blk codec.replyData(fs.unique, win[0..n]);
                },
            };
            self.commitRead(gent, f.req, f.in, reply);
            return;
        }
        if (f.polls < zc.FUSED_POLL_MAX) {
            assert(self.fused.push(gent));
            return;
        }
        fused_read_late_demotions.inc();
        self.dispatch(gent, f.req, f.sref, null);
    }

    // ---- the pass ----

    fn runLoop(self: *Group) void {
        while (self.shared.isActive() and !self.disconnected and !self.protocol_rejected) {
            passes.inc();
            // 1. Rescue capture.
            const after_tick = self.tick_rescue_pending;
            self.tick_rescue_pending = false;
            const tick_wake_seen = self.wake_cqe_seen;
            self.wake_cqe_seen = false;
            // 2–3. The READ CQE already drained the counter; disarm, then scan.
            _ = self.ring.service();
            // 4. Pump every published cell through the gate; then the
            //    handlers' bridge requests; releases after the commits.
            var pass_msgs: u32 = 0;
            while (self.cells.pop()) |gent| {
                pass_msgs += 1;
                self.handleCommitCell(gent);
            }
            self.pumpExtracts();
            self.pumpStores();
            self.pumpReleases();
            // 4b. The idle-trim request, if the sampler left one.
            self.trimIdleWindows();
            // 5. Mid-pass reap of finished bridges, then ONE fused frame.
            if (self.deadlines.outstanding > 0 and !self.fused.isEmpty()) {
                self.flushNonBlocking();
                self.reap();
            }
            self.pollFused();
            // 6. Unpark scan.
            self.unparkScan();
            // 7. Exit if the session died under us.
            if (!self.shared.isActive()) break;
            const now = clock.monotonicNs();
            // 8. The bridge deadline scan.
            self.deadlineScan(now);
            // 9. Re-arm the standing READ.
            var armed = true;
            if (!self.ring.armWake()) {
                sq_full.fire("no SQE for the wake READ");
                armed = false;
            }
            // 10. The backstop iff the group owes any reply (slot states), a
            //     bridge is pending or a fused frame waits.
            const owed = self.owedCount();
            const backstop: ?u64 = if (owed > 0 or self.deadlines.outstanding > 0 or !self.fused.isEmpty() or !armed) PARK_BACKSTOP_NS else null;
            // 11. Rescue ledger, commit half.
            if (after_tick and pass_msgs > 0 and !tick_wake_seen) {
                park_tick_commit_rescues.add(pass_msgs);
                self.rescueWarn("commit", pass_msgs);
            }
            // 12. Work in hand ⇒ a non-blocking flush and loop.
            var retry_left: ?u64 = null;
            for (self.members) |*m| if (m.slots.nextRetryDeadline()) |d| {
                const left = d -| now;
                retry_left = if (retry_left) |r| @min(r, left) else left;
            };
            if (pass_msgs > 0 or !self.fused.isEmpty()) {
                self.flushNonBlocking();
            } else {
                // 13. THE enter: every SQE pushed this pass and the wait.
                var bound: ?u64 = backstop;
                if (retry_left) |r| bound = if (bound) |b| @min(b, r) else r;
                const verdict = self.park(bound);
                if (verdict == .tick and backstop != null and (retry_left == null or backstop.? <= retry_left.?)) {
                    park_backstop_ticks.inc();
                    self.tick_rescue_pending = true;
                }
            }
            if (self.ring.isDead()) {
                self.disconnected = true;
                break;
            }
            // 14–15. Reap and act on every CQE.
            self.reap();
            // 16. Backoff retries due.
            self.pushRetriesDue();
        }
    }

    fn rescueWarn(self: *Group, what: []const u8, n: u32) void {
        const now_s = clock.realtimeCoarseS();
        if (now_s -| self.rescue_log_last_s < RESCUE_LOG_INTERVAL_S) return;
        self.rescue_log_last_s = now_s;
        log.warn("transport: group {d}: the 100 ms park backstop rescued {d} {s}(s) with no wake delivered — a lost wake in the chain (counted transport_park_tick_{s}_rescues)", .{ self.index, n, what, what });
    }

    /// The eager non-blocking GETEVENTS flush (a `DEFER_TASKRUN` ring runs
    /// its deferred completions only inside an enter).
    fn flushNonBlocking(self: *Group) void {
        noteCommitBatch(self.commits_pushed);
        self.commits_pushed = 0;
        self.noteBridgesSubmitted();
        enters.inc();
        _ = self.ring.ring.submitAndWaitTimed(1, 0) catch |e| switch (e) {
            error.Interrupted => {},
            else => {
                log.err("transport: group {d}: io_uring_enter failed ({s}) — the ring is dead", .{ self.index, @errorName(e) });
                self.ring.markDead();
            },
        };
    }

    fn noteBridgesSubmitted(self: *Group) void {
        if (self.deadlines.outstanding == 0) return;
        for (self.pends, 0..) |p, g| if (p != null) self.deadlines.noteSubmitted(g);
    }

    /// ONE enter: submit first, then the (bounded) wait. A timed park with
    /// NOTHING to submit (the common owed park: the handler is out on a
    /// lane, this pass pushed no SQE) is ONE enter whose ETIME is
    /// trustworthy because it submits nothing (Q8); otherwise the ring's
    /// two-enter timed park. Measured on the venue storm (4 threads, 10 s):
    /// 4.34 → 2.82 enters per ring request.
    fn park(self: *Group, bound: ?u64) ring_park.Verdict {
        noteCommitBatch(self.commits_pushed);
        self.commits_pushed = 0;
        self.noteBridgesSubmitted();
        if (bound) |b| {
            if (!self.ring.isDead() and self.ring.ring.sqReady() == 0 and self.ring.ring.features & linux.IORING_FEAT_EXT_ARG != 0) {
                enters.inc();
                const entered = self.ring.ring.submitAndWaitTimed(1, b) catch |e| switch (e) {
                    error.Interrupted => return .event,
                    else => {
                        log.err("transport: group {d}: io_uring_enter failed ({s}) — the ring is dead", .{ self.index, @errorName(e) });
                        self.ring.markDead();
                        return .event;
                    },
                };
                return if (entered.timed_out) .tick else .event;
            }
            enters.add(2);
        } else enters.inc();
        return self.ring.park(bound);
    }

    fn reap(self: *Group) void {
        var cqes: [64]uring128.Cqe32 = undefined;
        var total: u32 = 0;
        while (true) {
            const n = self.ring.ring.copyCqes(&cqes, 0) catch |e| {
                log.err("transport: group {d}: reaping CQEs failed ({s}) — the ring is dead", .{ self.index, @errorName(e) });
                self.ring.markDead();
                self.disconnected = true;
                return;
            };
            for (cqes[0..n]) |cqe| self.handleCqe(cqe);
            total += n;
            if (n < cqes.len) break;
        }
        // The overflow watch: a wrapping delta of the kernel's counter.
        const overflow = self.ring.ring.cq.overflow.*;
        if (overflow != self.cq_overflow_seen) {
            cq_overflows.fire("kernel dropped CQEs on a transport ring");
            self.cq_overflow_seen = overflow;
        }
        if (self.tick_rescue_pending and total > 0) {
            park_tick_cqe_rescues.add(total);
            self.rescueWarn("cqe", total);
        }
    }

    fn handleCqe(self: *Group, cqe: uring128.Cqe32) void {
        if (self.ring.consumeWake(cqe)) {
            self.wake_cqe_seen = true;
            return;
        }
        if (uring128.Tag.isReserved(cqe.user_data)) return;
        const tag = decodeTag(cqe.user_data) orelse {
            cqe_tag_unknown.fire("CQE tag no transport push produced");
            return;
        };
        if (tag.gent >= self.headers.depth) {
            cqe_tag_unknown.fire("CQE gent out of range");
            return;
        }
        switch (tag.kind) {
            .bridge => return self.handleBridgeCqe(tag.gent, cqe.res),
            .cancel => return self.handleCancelCqe(tag.gent, cqe.res),
            .release => return self.handleReleaseCqe(tag.gent, cqe.res),
            .register, .commit => {},
        }
        if (cqe.res < 0) return self.handleCqeError(tag, cqe.err());
        self.deliver(tag.gent, tag.kind == .register, cqe.flags);
    }

    fn releaseFailure(self: *Group, gent: u32, e: linux.E) void {
        slot.release_failures.fire("RELEASE_PAYLOAD answered nonzero");
        log.err("transport: queue {d} ent {d}: RELEASE_PAYLOAD answered {t} ({s})", .{ self.memberOf(gent).qid, self.entIdxOf(gent), e, if (e == .NOENT) "a double release" else if (e == .BUSY) "raced a live commit" else "unexpected" });
    }

    /// The CQE error ladder (porting spec §8.2), in order.
    fn handleCqeError(self: *Group, tag: DecodedTag, e: linux.E) void {
        cqe_errors.inc();
        const m = self.memberOf(tag.gent);
        const ent_idx = self.entIdxOf(tag.gent);
        const now = clock.monotonicNs();
        if (isDisconnect(e)) {
            self.disconnected = true;
            return;
        }
        // A RETAIN commit the kernel refused: the must-stay-0 tripwire and a
        // plain re-commit of the applied reply (before the protocol arm).
        if (tag.kind == .commit and e == .INVAL and m.slots.ent(ent_idx).state == .retained) {
            m.slots.onRetainRefused(ent_idx, now);
            self.pushCommit(tag.gent, m.slots.ent(ent_idx).owing().?.commit_id);
            return;
        }
        if (isProtocolRejection(e)) {
            log.err("transport: queue {d} ent {d}: the kernel rejected the FUSE uring protocol ({t} on {s}) — the session fails (charter §1.2: no classical fallback)", .{ m.qid, ent_idx, e, @tagName(tag.kind) });
            self.protocol_rejected = true;
            _ = self.shared.shutdown();
            _ = self.shared.barrier.complete(.failed);
            return;
        }
        switch (tag.kind) {
            .commit => {
                if (e == .AGAIN or e == .INTR) {
                    // Re-push the SAME commit: the ent still holds the applied reply.
                    if (m.slots.onCommitRetry(ent_idx, now)) |cid| {
                        self.pushCommit(tag.gent, cid);
                        return;
                    }
                }
                // The kernel discarded the reply: re-REGISTER the ent.
                log.warn("transport: queue {d} ent {d}: COMMIT_AND_FETCH failed ({t}); re-REGISTER", .{ m.qid, ent_idx, e });
                self.failEnt(tag.gent, .IO);
                self.pushRegister(tag.gent / self.depth, ent_idx);
            },
            .register => {
                self.failEnt(tag.gent, .IO);
                self.onRegisterRefused(tag.gent / self.depth, ent_idx, e, now);
            },
            else => unreachable,
        }
    }

    fn pushRetriesDue(self: *Group) void {
        const now = clock.monotonicNs();
        for (self.members, 0..) |*m, ms| {
            while (m.slots.nextRetryDue(now)) |e| self.pushRegister(@intCast(ms), e);
        }
    }

    // ---- delivery (porting spec §5; kmbuf §3.4, §4.4) ----

    fn deliver(self: *Group, gent: u32, from_register: bool, cqe_flags: u32) void {
        const m = self.memberOf(gent);
        const ent_idx = self.entIdxOf(gent);
        const hdr = self.headerOf(gent);
        const now = clock.monotonicNs();
        if (from_register) m.slots.noteRegisterSuccess(ent_idx);
        if (self.kmb) |*k| {
            // The attachment law: flagged re-points, unflagged keeps, a bid
            // past the ring is a breach.
            switch (self.attached[gent].note(cqe_flags, k.ring_entries)) {
                .kept => {},
                .repointed => kmbuf_attached_deliveries.inc(),
                .breach => |bid| {
                    kmbuf_attachment_breach.fire("CQE buffer id past the kmbuf ring");
                    log.err("transport: queue {d} ent {d}: kmbuf bid {d} ≥ ring entries {d} — protocol breach, the session fails", .{ m.qid, ent_idx, bid, k.ring_entries });
                    _ = self.shared.shutdown();
                    return;
                },
            }
        }
        const unique = std.mem.readInt(u64, hdr.in_out[8..16], .little);
        var commit_id = hdr.ring_ent_in_out.commit_id;
        if (commit_id == 0) commit_id = unique;
        if (unique == 0) {
            if (commit_id != 0) {
                // The forced EIO commit is the machine's normal exit here.
                m.slots.onDeliverDegenerate(ent_idx, commit_id, now);
                slot.applyErrorHeader(hdr, 0, .IO);
                self.pushCommit(gent, commit_id);
            } else {
                self.pushRegister(gent / self.depth, ent_idx);
            }
            return;
        }
        m.slots.onDeliver(ent_idx, unique, commit_id, now);
        ring_requests.inc();
        self.extract_cells[gent].done_len = 0;
        const ent = m.slots.ent(ent_idx);
        if (ent.lease.leased()) {
            // A previous lease still reads this buffer: never hand it out again.
            delivery_on_leased.fire("delivery on an ent whose payload lease is live");
            return self.failEnt(gent, .IO);
        }
        const announced: usize = hdr.ring_ent_in_out.payload_sz;
        if (self.shared.zeroCopy()) {
            const op = (abi.decodePrefix(abi.fuse_in_header, &hdr.in_out) orelse return self.failEnt(gent, .INVAL)).op();
            ent.dir = zc.dirOf(op, self.shared.readdirLatch());
            if (ent.dir == .source and announced > 0) return self.deliverPagedWrite(gent, announced);
        }
        const win = if (self.shared.zeroCopy() and ent.dir == .dest) self.attachedWindow(gent) else self.window(gent);
        const capped = @min(announced, win.len);
        self.deliverParsed(gent, win[0..capped], capped, null);
    }

    /// A paged WRITE on a zero-copy slot: the bytes sit in the slot, the
    /// kmbuf attachment holds nothing. HELD iff the shape and the
    /// filesystem's pure gate say so (the lease carries the slot); else
    /// extracted at delivery (`WRITE_FIXED(slot → slab)`) and dispatched at
    /// the CQE over the bounce bytes.
    fn deliverPagedWrite(self: *Group, gent: u32, announced: usize) void {
        const m = self.memberOf(gent);
        const ent_idx = self.entIdxOf(gent);
        const hdr = self.headerOf(gent);
        const ent = m.slots.ent(ent_idx);
        if (announced > self.payload_len) return self.failEnt(gent, .IO);
        const win = abi.decodePrefix(abi.fuse_write_in, &hdr.op_in) orelse return self.failEnt(gent, .INVAL);
        ent.class = zc.PayloadClass.classify(win.flags, win.write_flags);
        const hint: fsv.HoldHint = .{ .ino = std.mem.readInt(u64, hdr.in_out[16..24], .little), .offset = win.offset, .len = win.size, .class = ent.class };
        const hold = @as(usize, win.size) == announced and
            zc.holdCandidate(win.offset, win.size, zc.holdBound(self.shared.plan.payload_sz)) and
            zc.w1Shape(win.offset, win.size, self.shared.block_size) and
            self.shared.fs.writeHoldEligible(hint);
        if (hold) {
            ent.held = true;
            self.held_len[gent] = win.size;
            const lease = slot.PayloadLease.acquireHeld(&ent.lease, self.waker(), win.size, ent.class, m.qid, ent_idx, gent, self.heldFace());
            self.deliverParsed(gent, &.{}, announced, lease);
            return;
        }
        if (!self.pushBridge(gent, .{ .kind = .write_extract, .len = @intCast(announced) }, self.shared.slab.?.fd, self.bounceOffset(gent), false)) {
            return self.failEnt(gent, .IO);
        }
    }

    /// The at-delivery extraction landed: the WRITE dispatches now over a
    /// lease on the bounce bytes.
    fn deliverExtracted(self: *Group, gent: u32, len: u32) void {
        const m = self.memberOf(gent);
        const ent_idx = self.entIdxOf(gent);
        if (m.slots.ent(ent_idx).owing() == null) return;
        const win = self.bounce(gent);
        self.deliverParsed(gent, win[0..len], len, null);
    }

    /// Re-run the delivery of an owing slot through the ordinary ladder
    /// (after a direct leg fell through): the header is intact.
    fn redeliver(self: *Group, gent: u32) void {
        const hdr = self.headerOf(gent);
        const win = self.attachedWindow(gent);
        const capped = @min(@as(usize, hdr.ring_ent_in_out.payload_sz), win.len);
        const delivery = codec.Delivery.fromRing(&hdr.in_out, &hdr.op_in, win[0..capped], capped) catch return self.failEnt(gent, .INVAL);
        const parsed = codec.parse(delivery) catch |e| return self.failEnt(gent, codec.parseErrno(e));
        const req: fsv.Request = .{ .header = delivery.header, .op = parsed };
        const m = self.memberOf(gent);
        const ent_idx = self.entIdxOf(gent);
        const sref: slot.SlotRef = .{ .qid = m.qid, .ent_idx = ent_idx, .commit_id = m.slots.ent(ent_idx).owing().?.commit_id };
        self.dispatch(gent, req, sref, null);
    }

    /// Parse and route a delivered request. `payload` is the copyable body
    /// (the arena / attached / bounce bytes); `payload_len` the authoritative
    /// body length (a held slot: `> payload.len`); `held` the held lease.
    fn deliverParsed(self: *Group, gent: u32, payload: []const u8, payload_len: usize, held: ?slot.PayloadLease) void {
        const m = self.memberOf(gent);
        const ent_idx = self.entIdxOf(gent);
        const hdr = self.headerOf(gent);
        const ent = m.slots.ent(ent_idx);
        const unique = std.mem.readInt(u64, hdr.in_out[8..16], .little);
        const commit_id = ent.owing().?.commit_id;
        const delivery = codec.Delivery.fromRing(&hdr.in_out, &hdr.op_in, payload, payload_len) catch {
            if (held) |l| l.release();
            return self.failEnt(gent, .INVAL);
        };
        const parsed = codec.parse(delivery) catch |e| {
            if (held) |l| l.release();
            return self.failEnt(gent, codec.parseErrno(e));
        };
        const req: fsv.Request = .{ .header = delivery.header, .op = parsed };
        const sref: slot.SlotRef = .{ .qid = m.qid, .ent_idx = ent_idx, .commit_id = commit_id };
        switch (parsed) {
            .forget => |f| {
                forget_entries.inc();
                self.shared.fs.forget(req.nodeid(), f.nlookup);
                self.autoCommit(gent, unique, commit_id);
            },
            .batch_forget => |b| {
                forget_entries.add(b.entries.len);
                for (b.entries) |e| self.shared.fs.forget(e.nodeid, e.nlookup);
                self.autoCommit(gent, unique, commit_id);
            },
            // No-reply opcodes that reached the ring: the ent must still be re-armed.
            .interrupt, .notify_reply => self.autoCommit(gent, unique, commit_id),
            .read => |in| self.dispatchRead(gent, req, in, sref),
            .write => |w| {
                const lease = held orelse slot.PayloadLease.acquire(&ent.lease, w.data, self.waker());
                self.dispatch(gent, req, sref, lease);
            },
            else => self.dispatch(gent, req, sref, null),
        }
    }

    /// FORGET/BATCH_FORGET (and the other no-reply opcodes): an empty
    /// reply commits the ent right here — over the ring an ent stays in
    /// userspace until COMMIT even for a "no reply" opcode.
    fn autoCommit(self: *Group, gent: u32, unique: u64, commit_id: u64) void {
        const hdr = self.headerOf(gent);
        hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(codec.okHeader(unique, 0));
        hdr.ring_ent_in_out.payload_sz = 0;
        forget_autocommits.inc();
        self.pushCommit(gent, commit_id);
    }

    /// A served READ reply: into the window (UserEnts/BufRing — the arena
    /// or the attached buffer), or bridged through the slot (zc).
    fn commitRead(self: *Group, gent: u32, req: fsv.Request, in: abi.fuse_read_in, reply: codec.Reply) void {
        var r = codec.replyRead(req.unique(), reply.body, in.size);
        r.zero_pad = self.zeroReadTail(gent, in, r.body.len);
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const cid = m.slots.ent(e).owing().?.commit_id;
        if (self.shared.zeroCopy()) {
            // The worker's own message: a bridge parks it in `local_msgs`.
            self.local_msgs[gent] = commit_queue.Msg.reply(.{ .qid = m.qid, .ent_idx = e, .commit_id = cid }, r);
            self.commitReadyZc(gent, &self.local_msgs[gent], .local);
            return;
        }
        _ = slot.applyReply(self.headerOf(gent), self.window(gent), r);
        self.pushCommit(gent, cid);
    }

    /// A probe's fd-source serve: on a zero-copy dest slot the fd→slot
    /// bridge with the pin-release hook at its CQE (zero daemon passes);
    /// anywhere else the body is copied into the window and the hook runs
    /// at once (the copy arm, counted by the filesystem's ledger).
    fn commitReadFdSource(self: *Group, gent: u32, req: fsv.Request, in: abi.fuse_read_in, fs: fsv.FdSource) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const sref: slot.SlotRef = .{ .qid = m.qid, .ent_idx = e, .commit_id = m.slots.ent(e).owing().?.commit_id };
        // A short serve (EOF inside the requested pages) takes the copy arm:
        // the fill-pool view has no zeros to bridge past its bytes.
        const served: usize = @min(fs.body.len, in.size);
        const padded = served < @min(in.size, self.payload_len);
        if (self.shared.zeroCopy() and m.slots.ent(e).dir == .dest and fs.body.len > 0 and !padded) {
            self.hooks[gent] = fs.done;
            self.local_msgs[gent] = commit_queue.Msg.fdBody(sref, codec.replyData(fs.unique, fs.body), fs.fd, fs.off);
            self.commitReadyZc(gent, &self.local_msgs[gent], .local);
            return;
        }
        const win = self.window(gent);
        const n = @min(fs.body.len, win.len);
        @memcpy(win[0..n], fs.body[0..n]);
        zc_fd_source_copies.inc();
        if (fs.done) |h| _ = h.run(true);
        self.commitRead(gent, req, in, codec.replyData(fs.unique, win[0..n]));
    }

    /// The kernel's `page_zeroing` for a short paged READ on this slot's
    /// window (`codec.readZeroTail`): under zero-copy the window IS the
    /// installed pages and no kernel copy clears them; elsewhere the copy
    /// path clears them again (idempotent). The O_DIRECT flag is the
    /// FILE's open flag, never the I/O path's: a page-cache fill of an
    /// O_DIRECT fd (an mmap fault, readahead) owes the same zeros, and a
    /// direct read's user buffer is exactly `in.size` long — the bound.
    fn zeroReadTail(self: *Group, gent: u32, in: abi.fuse_read_in, len: usize) u32 {
        return codec.readZeroTail(self.window(gent), len, in.size);
    }

    /// THE READ DISPATCH LAW at the delivery CQE: (a) the filesystem's
    /// try-only warm ladder serves inline; (b) a zc session fuses a READ at
    /// or under the ceiling onto the worker's run-queue (lever default OFF;
    /// no capacity or not zc ⇒ counted a demotion); (c) hand directly to
    /// the handler lane homed on the queue's CPU. Partition: `serves +
    /// fusions + demotes ≡ READs`. Under zc a declined probe first asks for
    /// the direct device leg.
    fn dispatchRead(self: *Group, gent: u32, req: fsv.Request, in: abi.fuse_read_in, sref: slot.SlotRef) void {
        ring_reads.inc();
        const win = self.window(gent);
        if (self.shared.fs.probeRead(req, win)) |pr| {
            fast_dispatch_serves.inc();
            switch (pr) {
                .reply => |reply| self.commitRead(gent, req, in, reply),
                .fd_source => |fs| self.commitReadFdSource(gent, req, in, fs),
            }
            return;
        }
        if (self.shared.zeroCopy() and self.memberOf(gent).slots.ent(self.entIdxOf(gent)).dir == .dest) {
            if (self.shared.fs.directLeg(req)) |leg| {
                // A leg without its recheck hook is never bridged (THE LAW).
                if (leg.hook != null and leg.len > 0 and leg.len <= self.payload_len and self.pushBridge(gent, .{ .kind = .direct_leg, .len = leg.len, .hook = leg.hook }, leg.fd, leg.off, true)) {
                    fast_dispatch_demotes.inc();
                    return;
                }
                zc_direct_leg_fallbacks.inc();
            }
        }
        const fusible = self.shared.read_fusion_on and zc.fuseCandidate(in.size, fusionCeiling(self.shared.plan.payload_sz), true);
        if (fusible and self.shared.zeroCopy() and self.fused.push(gent)) {
            self.frames[gent] = .{ .req = req, .in = in, .sref = sref, .polls = 0 };
            fused_read_fusions.inc();
            return;
        }
        if (fusible) fused_read_demotions.inc();
        fast_dispatch_demotes.inc();
        self.dispatch(gent, req, sref, null);
    }

    /// Arm (c) and every other opcode: the lane homed on the queue's CPU.
    fn dispatch(self: *Group, gent: u32, req: fsv.Request, sref: slot.SlotRef, lease: ?slot.PayloadLease) void {
        const m = self.memberOf(gent);
        const handle: fsv.Handle = .{ .tx = self.shared.tx, .slot = .{ .ring = sref }, .unique = req.unique(), .window = self.window(gent) };
        const cpu: ?u16 = if (self.shared.qid_is_cpu) m.qid else null;
        const lane = self.shared.exec.homeLane(cpu, self.shared.nodeOf(m.qid));
        self.shared.exec.spawnGroupOn(lane, self.shared.handler_group, handlerMain, .{ self.shared.fs, req, handle, lease }) catch {
            dispatch_refused.inc();
            // Nothing is parked yet: the drop wakes nobody.
            if (lease) |l| l.release();
            self.failEnt(gent, .AGAIN);
        };
    }

    // ---- teardown (porting spec §11.2) ----

    fn drain(self: *Group) void {
        // 1. Fused frames and bridge pends die with the worker: their slots
        //    are still owed and the row-8 pass below synthesizes them.
        while (self.fused.pop()) |_| {}
        for (self.pends, 0..) |p, g| if (p != null) {
            const pend = self.clearPend(@intCast(g)).?;
            if (pend.kind == .lazy_extract) {
                self.extract_cells[g].res = -@as(i32, @intCast(@backingInt(linux.E.NOTCONN)));
                self.extract_cells[g].event.set(self.shared.io);
            }
            if (pend.kind == .direct_store) {
                self.store_cells[g].res = -@as(i32, @intCast(@backingInt(linux.E.NOTCONN)));
                self.store_cells[g].event.set(self.shared.io);
            }
            if (pend.hook) |h| _ = h.run(false);
        };
        while (self.extract_inbox.tryPop()) |g| {
            self.extract_cells[g].res = -@as(i32, @intCast(@backingInt(linux.E.NOTCONN)));
            self.extract_cells[g].event.set(self.shared.io);
        }
        while (self.store_inbox.tryPop()) |g| {
            self.store_cells[g].res = -@as(i32, @intCast(@backingInt(linux.E.NOTCONN)));
            self.store_cells[g].event.set(self.shared.io);
        }
        // A handler's release still queued is subsumed by step 3.
        while (self.release_inbox.tryPop()) |_| {}
        // 2. Final messages: every published cell still queued joins the
        //    parked ones; a leased ent gets the whole-group bounded wait.
        while (self.cells.pop()) |gent| self.handleCommitCell(gent);
        self.awaitLeases();
        // 3–4. Parked and still leased ⇒ header-only EIO (never the payload,
        //    never counted as an unpark); a bridge-parked cell ⇒ its body
        //    never reached the pages: header-only EIO; every other owing
        //    slot ⇒ fail_ent.
        for (self.members, 0..) |*m, ms| {
            for (m.parked, 0..) |p, e| {
                const ent_idx: u16 = @intCast(e);
                const gent = self.gentOf(@intCast(ms), ent_idx);
                if (p) {
                    if (m.slots.ent(ent_idx).lease.tryUnpark()) {
                        slot.unparked_commits.inc();
                        m.parked[e] = false;
                        const msg = self.cells.msg(gent);
                        // A paged body would need a bridge nobody will reap: header-only.
                        if (self.shared.zeroCopy() and m.slots.ent(ent_idx).dir == .dest and msg.asReply().body.len > 0) {
                            self.failEnt(gent, .IO);
                        } else {
                            m.slots.onUnparked(ent_idx);
                            self.commitReady(gent, msg, .cell);
                            if (self.bridge_parked[gent] == .none) self.cells.recycle(gent);
                        }
                        continue;
                    }
                    self.failEnt(gent, .IO);
                    continue;
                }
                if (m.slots.ent(ent_idx).owing() != null) self.failEnt(gent, .IO);
            }
        }
        // 5. RELEASE every retained slot while the connection is live — after
        //    every commit of this drain (a RETAIN an unparked cell just pushed
        //    precedes its RELEASE in SQ order). A retained ent the kernel
        //    reclaims at its own teardown pins the exiting process
        //    (`fuse-uring: … teardown with retained zc payloads`); after a
        //    disconnect the kernel's drain already owns them (counted).
        if (!self.disconnected) self.releaseRetainedAll() else self.countKernelDrained();
        // 6. One flush for the whole drain, then the RELEASE verdicts: the
        //    kernel answers them inside that enter, so the reap is immediate
        //    and the session may unmount only once every RELEASE is reaped
        //    (a RELEASE after `umount2` meets a stopped queue).
        self.flushNonBlocking();
        self.awaitReleases();
        // 7. The session may unmount now.
        self.drained.store(1, .release);
        futex.wakeAll(&self.drained);
        // 8. Residency.
        self.residency();
        // Anything still owed when residency lapsed is the honest tripwire.
        for (self.members) |*m| {
            for (m.slots.ents, 0..) |*ent, e| if (ent.owing() != null) m.slots.abandon(@intCast(e));
        }
    }

    /// The drain found the connection already gone: every retained slot is
    /// the kernel's drain's (its RELEASE would answer -ENOTCONN).
    fn countKernelDrained(self: *Group) void {
        for (self.members) |*m| {
            for (m.slots.ents) |*ent| if (ent.state == .retained) retained_kernel_drained.inc();
        }
    }

    /// Reap this group's teardown RELEASE CQEs (`releases_pending` → 0):
    /// inline with the flush in practice; bounded parks on the ring guard a
    /// dead ring, and a lapse is loud (the slot is then the kernel's).
    fn awaitReleases(self: *Group) void {
        const deadline = clock.monotonicNs() + TEARDOWN_RELEASE_BOUND_NS;
        while (self.releases_pending > 0 and !self.disconnected and !self.ring.isDead()) {
            self.reapQuiet();
            if (self.releases_pending == 0) return;
            const now = clock.monotonicNs();
            if (now >= deadline) {
                log.warn("transport: group {d}: {d} teardown RELEASE(s) unanswered after {d} ms — the kernel reclaims those slots at its teardown", .{ self.index, self.releases_pending, TEARDOWN_RELEASE_BOUND_NS / std.time.ns_per_ms });
                return;
            }
            _ = self.ring.armWake();
            _ = self.park(@min(RESIDENCY_PARK_NS, deadline - now));
            _ = self.ring.service();
        }
    }

    /// ONE bounded, event-driven wait for every parked ent's lease: the
    /// drops arrive through the eventfd; budget 100 ms for the group.
    fn awaitLeases(self: *Group) void {
        const deadline = clock.monotonicNs() + DRAIN_LEASE_BUDGET_NS;
        while (true) {
            var leased_parked = false;
            for (self.members) |*m| {
                for (m.parked, 0..) |p, e| if (p and m.slots.ent(@intCast(e)).lease.leased()) {
                    leased_parked = true;
                };
            }
            if (!leased_parked) return;
            const now = clock.monotonicNs();
            if (now >= deadline) return;
            if (self.ring.isDead()) return;
            _ = self.ring.armWake();
            _ = self.park(deadline - now);
            self.reapQuiet();
            _ = self.ring.service();
        }
    }

    /// Reap during the drain: wakes consumed, deliveries answered ENOTCONN
    /// header-only, RELEASE verdicts accounted, a refused RETAIN re-committed
    /// plain (the request is still live kernel-side), errors noted for the
    /// disconnect verdict, bridge/cancel CQEs dropped (their pends are gone).
    fn reapQuiet(self: *Group) void {
        var cqes: [64]uring128.Cqe32 = undefined;
        while (true) {
            const n = self.ring.ring.copyCqes(&cqes, 0) catch {
                self.ring.markDead();
                self.disconnected = true;
                return;
            };
            for (cqes[0..n]) |cqe| {
                if (self.ring.consumeWake(cqe)) {
                    self.wake_cqe_seen = true;
                    continue;
                }
                if (uring128.Tag.isReserved(cqe.user_data)) continue;
                const tag = decodeTag(cqe.user_data) orelse continue;
                if (tag.gent >= self.headers.depth) continue;
                if (tag.kind == .release) {
                    self.handleReleaseCqe(tag.gent, cqe.res);
                    continue;
                }
                if (cqe.res < 0) {
                    cqe_errors.inc();
                    const e = cqe.err();
                    if (isDisconnect(e)) {
                        self.disconnected = true;
                    } else if (tag.kind == .commit and e == .INVAL and self.memberOf(tag.gent).slots.ent(self.entIdxOf(tag.gent)).state == .retained) {
                        const m = self.memberOf(tag.gent);
                        const ent_idx = self.entIdxOf(tag.gent);
                        m.slots.onRetainRefused(ent_idx, clock.monotonicNs());
                        self.pushCommit(tag.gent, m.slots.ent(ent_idx).owing().?.commit_id);
                    }
                    continue;
                }
                if (tag.kind != .register and tag.kind != .commit) continue;
                // A fresh delivery on a still-registered ent: ENOTCONN,
                // header-only (a leased payload is never written).
                const m = self.memberOf(tag.gent);
                const ent_idx = self.entIdxOf(tag.gent);
                const hdr = self.headerOf(tag.gent);
                const unique = std.mem.readInt(u64, hdr.in_out[8..16], .little);
                var cid = hdr.ring_ent_in_out.commit_id;
                if (cid == 0) cid = unique;
                if (cid == 0) continue;
                m.slots.onDeliver(ent_idx, unique, cid, clock.monotonicNs());
                residency_replies.inc();
                self.failEnt(tag.gent, .NOTCONN);
            }
            if (n < cqes.len) return;
        }
    }

    /// Stay resident up to 30 s answering fresh deliveries ENOTCONN, in
    /// bounded parks on this ring; exit on a disconnect, or at once when
    /// the session detached the mount lazily (acquire pairs with
    /// `unmountDetached`'s release).
    fn residency(self: *Group) void {
        const deadline = clock.monotonicNs() + RESIDENCY_NS;
        while (!self.disconnected and !self.ring.isDead() and !self.shared.detached.load(.acquire)) {
            const now = clock.monotonicNs();
            if (now >= deadline) {
                log.warn("transport: group {d}: residency backstop lapsed with {d} slot(s) owing", .{ self.index, self.owedCount() });
                return;
            }
            _ = self.ring.armWake();
            _ = self.park(@min(RESIDENCY_PARK_NS, deadline - now));
            self.reapQuiet();
            _ = self.ring.service();
        }
    }
};

/// The handler fiber: the filesystem answers the handle exactly once.
fn handlerMain(fs: fsv.Filesystem, req: fsv.Request, handle: fsv.Handle, lease: ?slot.PayloadLease) void {
    fs.dispatch(req, handle, lease);
}

// ---------------------------------------------------------------------------
// Tests: the SQE bytes and the tag space (the live transport is
// `session.zig`'s).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "REGISTER SQE bytes: URING_CMD on fixed file 0, cmd_op 1 at +8, the 2-iovec array at +16 with len 2 at +24, buf_index 0, qid at +64, the union u16 at +66 zero; COMMIT carries commit_id at +56 and cmd_op 2" {
    var sqe: uring128.Sqe128 = undefined;
    var iovs: [2]std.posix.iovec = .{ .{ .base = @ptrFromInt(0x1000), .len = 288 }, .{ .base = @ptrFromInt(0x2000), .len = 1 << 20 } };
    prepRegister(&sqe, 7, &iovs, 3);
    const raw: *const [128]u8 = @ptrCast(&sqe);
    try testing.expectEqual(@backingInt(linux.IORING_OP.URING_CMD), raw[0]);
    try testing.expect(raw[1] & linux.IOSQE_FIXED_FILE != 0);
    try testing.expectEqual(@as(i32, 0), std.mem.readInt(i32, raw[4..8], .little));
    try testing.expectEqual(@as(u32, 1), std.mem.readInt(u32, raw[8..12], .little));
    try testing.expectEqual(@intFromPtr(&iovs), std.mem.readInt(u64, raw[16..24], .little));
    try testing.expectEqual(@as(u32, 2), std.mem.readInt(u32, raw[24..28], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, raw[40..42], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, raw[48..56], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, raw[56..64], .little));
    try testing.expectEqual(@as(u16, 7), std.mem.readInt(u16, raw[64..66], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, raw[66..68], .little));
    for (raw[72..128]) |b| try testing.expectEqual(@as(u8, 0), b);
    try testing.expectEqual(tagOf(.register, 3), sqe.user_data);

    prepCommit(&sqe, 7, 0xDEAD_BEEF, 3, 0);
    const c: *const [128]u8 = @ptrCast(&sqe);
    try testing.expectEqual(@as(u32, 2), std.mem.readInt(u32, c[8..12], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, c[16..24], .little));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, c[24..28], .little));
    try testing.expectEqual(@as(u64, 0xDEAD_BEEF), std.mem.readInt(u64, c[56..64], .little));
    try testing.expectEqual(@as(u16, 7), std.mem.readInt(u16, c[64..66], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, c[66..68], .little));
    try testing.expectEqual(tagOf(.commit, 3), sqe.user_data);

    prepBridgeFetch(&sqe, 9, 8192, 4096, 5, 11);
    try testing.expectEqual(linux.IORING_OP.READ_FIXED, sqe.opcode);
    try testing.expectEqual(@as(i32, 9), sqe.fd);
    try testing.expectEqual(@as(u64, 8192), sqe.off);
    try testing.expectEqual(@as(u32, 4096), sqe.len);
    try testing.expectEqual(@as(u16, 5), sqe.buf_index);
    try testing.expectEqual(@as(u64, 0), sqe.addr);
    try testing.expectEqual(tagOf(.bridge, 11), sqe.user_data);

    prepBridgeStore(&sqe, 9, 16384, 4096, 6, 12);
    try testing.expectEqual(linux.IORING_OP.WRITE_FIXED, sqe.opcode);
    try testing.expectEqual(@as(u64, 16384), sqe.off);
    try testing.expectEqual(@as(u16, 6), sqe.buf_index);
    try testing.expectEqual(tagOf(.bridge, 12), sqe.user_data);
}

test "kmbuf / zc / retention SQE bytes: a kmbuf REGISTER carries no iovecs, init.flags at +66, queue_depth at +68 (zc only) and buf_index = ent at +40; a RETAIN commit sets the union u16 to 1; RELEASE_PAYLOAD is cmd_op 3 with the retained commit id" {
    var sqe: uring128.Sqe128 = undefined;
    prepRegisterKmbuf(&sqe, 7, 5, zc.initFlags(false, false), 0, 3);
    const raw: *const [128]u8 = @ptrCast(&sqe);
    try testing.expectEqual(@as(u32, 1), std.mem.readInt(u32, raw[8..12], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, raw[16..24], .little));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, raw[24..28], .little));
    try testing.expectEqual(@as(u16, 5), std.mem.readInt(u16, raw[40..42], .little));
    try testing.expectEqual(@as(u16, 7), std.mem.readInt(u16, raw[64..66], .little));
    try testing.expectEqual(abi.FUSE_URING_BUF_RING, std.mem.readInt(u16, raw[66..68], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, raw[68..70], .little));
    prepRegisterKmbuf(&sqe, 7, 5, zc.initFlags(true, true), 32, 3);
    try testing.expectEqual(abi.FUSE_URING_BUF_RING | abi.FUSE_URING_ZERO_COPY | abi.FUSE_URING_PAYLOAD_RETENTION, std.mem.readInt(u16, raw[66..68], .little));
    try testing.expectEqual(@as(u16, 32), std.mem.readInt(u16, raw[68..70], .little));
    for (raw[72..128]) |b| try testing.expectEqual(@as(u8, 0), b);
    prepCommit(&sqe, 7, 0x77, 3, abi.FUSE_URING_COMMIT_RETAIN);
    try testing.expectEqual(@as(u32, 2), std.mem.readInt(u32, raw[8..12], .little));
    try testing.expectEqual(@as(u16, 1), std.mem.readInt(u16, raw[66..68], .little));
    try testing.expectEqual(@as(u64, 0x77), std.mem.readInt(u64, raw[56..64], .little));
    prepRelease(&sqe, 7, 0x77, 3);
    try testing.expectEqual(@as(u32, 3), std.mem.readInt(u32, raw[8..12], .little));
    try testing.expectEqual(@as(u64, 0x77), std.mem.readInt(u64, raw[56..64], .little));
    try testing.expectEqual(@as(u16, 7), std.mem.readInt(u16, raw[64..66], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, raw[66..68], .little));
    try testing.expectEqual(tagOf(.release, 3), sqe.user_data);
}

test "tags: every op kind round-trips for ents 0, 1, 31, 255 and 16383; the wake/reserved values and a foreign kind decode to null (the unknown-tag tripwire's input)" {
    for ([_]OpKind{ .register, .commit, .bridge, .cancel, .release }) |k| {
        for ([_]u32{ 0, 1, 31, 255, 16383 }) |g| {
            const d = decodeTag(tagOf(k, g)).?;
            try testing.expectEqual(k, d.kind);
            try testing.expectEqual(g, d.gent);
        }
    }
    try testing.expect(decodeTag(0) == null);
    try testing.expect(decodeTag((uring128.Tag{ .kind = 9, .a = 1, .b = 0, .c = 0 }).pack()) == null);
    try testing.expect(uring128.Tag.isReserved(ring_park.wake_ud));
    try testing.expectEqual(@as(u32, 128 << 10), fusionCeiling(1 << 20));
    try testing.expectEqual(@as(u32, 512 << 10), fusionCeiling(4 << 20));
    try testing.expectEqual(@as(u32, 4096), fusionCeiling(8192));
    try testing.expect(isDisconnect(.NOTCONN) and isDisconnect(.NODEV) and !isDisconnect(.AGAIN));
}

test "the commit-batch histogram buckets: 1–8 exact, ≤ 16, ≤ 32, > 32" {
    const before = commit_batch_flushes.load();
    const snap = blk: {
        var s: [commit_batch_labels.len]u64 = undefined;
        for (&s, 0..) |*v, i| v.* = commit_batch[i].load(.monotonic);
        break :blk s;
    };
    noteCommitBatch(0);
    noteCommitBatch(1);
    noteCommitBatch(8);
    noteCommitBatch(9);
    noteCommitBatch(16);
    noteCommitBatch(17);
    noteCommitBatch(32);
    noteCommitBatch(33);
    try testing.expectEqual(before + 7, commit_batch_flushes.load());
    try testing.expectEqual(snap[0] + 1, commit_batch[0].load(.monotonic));
    try testing.expectEqual(snap[7] + 1, commit_batch[7].load(.monotonic));
    try testing.expectEqual(snap[8] + 2, commit_batch[8].load(.monotonic));
    try testing.expectEqual(snap[9] + 2, commit_batch[9].load(.monotonic));
    try testing.expectEqual(snap[10] + 1, commit_batch[10].load(.monotonic));
}

test "the zc bounce slab: one memfd for every queue's windows at (qid × depth + ent) × stride, page-aligned, VA and fd faces of one memory" {
    var s = try Slab.init(3, 4, 8192 + 100);
    defer s.deinit();
    try testing.expectEqual(@as(usize, 12288), s.stride);
    try testing.expectEqual(@as(usize, 12288 * 12), s.base.len);
    try testing.expectEqual(@as(u64, 12288 * 9), s.offsetOf(2, 1));
    const w = s.window(2, 1, 16);
    w[0] = 0xAB;
    // The fd face sees the VA write.
    var byte: [1]u8 = .{0};
    const n = linux.pread(s.fd, &byte, 1, @intCast(s.offsetOf(2, 1)));
    try testing.expectEqual(@as(usize, 1), n);
    try testing.expectEqual(@as(u8, 0xAB), byte[0]);
}
