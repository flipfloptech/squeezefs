//! The session memfd on Linux (charter §7; spec `docs/porting/il_session.md`
//! §1.7, §2.5, §2.8): create + initialise + seal on the daemon side; map
//! PMD-aligned on both sides; the seal check the client runs before it
//! trusts a byte; the THP posture; the NUMA bind. Raw `std.os.linux`
//! syscalls only — the shim links this and `std.posix` would resolve to the
//! shim's own interposed exports (decision 0003 §F).

const std = @import("std");
const linux = std.os.linux;
const layout = @import("layout.zig");
const ring_core = @import("ring_core.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");

pub const Error = error{
    MemfdFailed,
    TruncateFailed,
    SealFailed,
    MapFailed,
    StatFailed,
    /// `F_GET_SEALS` lacks one of SEAL/SHRINK/GROW: without SHRINK the peer
    /// can `ftruncate` under the mapping and every later access is an in-
    /// app SIGBUS — not recoverable by any ladder.
    Unsealed,
};

/// The three seals a session memfd must carry.
pub const REQUIRED_SEALS: u32 = linux.F.SEAL_SEAL | linux.F.SEAL_SHRINK | linux.F.SEAL_GROW;

/// `MADV_POPULATE_WRITE` (Linux 5.14) and `MADV_COLLAPSE` (6.1): UAPI values
/// pinned against an old libc's headers.
pub const MADV_POPULATE_WRITE: u32 = 23;
pub const MADV_COLLAPSE: u32 = 25;
/// `MPOL_PREFERRED`.
pub const MPOL_PREFERRED: usize = 1;

pub const Mapped = struct {
    mapping: layout.Mapping,
    /// What `unmap` releases: the trimmed PMD-aligned window.
    base: [*]align(layout.PAGE_BYTES) u8,
    len: usize,
};

fn pageSize() u64 {
    return std.heap.pageSize();
}

/// The daemon's half: a sealed memfd holding a fresh, VALID session image —
/// header written once (magic, abi, geometry, generation 1, the latch's
/// initial record), the ring seeded (`ring_core.seedForSharing`: the fd pass
/// is the synchronization point), then `F_SEAL_SEAL|SHRINK|GROW`. Returns
/// the fd AND the daemon's own mapping (the fd is sent with SESSION_OK and
/// closed; the mapping keeps the memory — zero persistent residue).
pub fn createSealed(l: layout.SessionLayout, thp: bool) Error!struct { fd: linux.fd_t, mapped: Mapped } {
    const rc = linux.memfd_create("sqz-il-session", linux.MFD.CLOEXEC | linux.MFD.ALLOW_SEALING);
    if (linux.errno(rc) != .SUCCESS) return error.MemfdFailed;
    const fd: linux.fd_t = @intCast(rc);
    errdefer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, @intCast(l.total_bytes))) != .SUCCESS) return error.TruncateFailed;
    const mapped = try mapSharedPmdAligned(fd, l);
    errdefer unmap(mapped);
    const m = mapped.mapping;
    m.header().* = .{ .geometry = l.geometry };
    ring_core.View.fromMapping(m).seedForSharing();
    if (linux.errno(linux.fcntl(fd, linux.F.ADD_SEALS, REQUIRED_SEALS)) != .SUCCESS) return error.SealFailed;
    if (thp) advise(mapped);
    return .{ .fd = fd, .mapped = mapped };
}

/// The client's rung 3: all three seals present, else `Unsealed`.
pub fn checkSeals(fd: linux.fd_t) Error!void {
    const rc = linux.fcntl(fd, linux.F.GET_SEALS, 0);
    if (linux.errno(rc) != .SUCCESS) return error.Unsealed;
    const seals: u32 = @intCast(rc);
    if (seals & REQUIRED_SEALS != REQUIRED_SEALS) return error.Unsealed;
}

/// `fstat(memfd).st_size`: the one size authority the client checks the
/// sealed header's geometry against.
pub fn fileSize(fd: linux.fd_t) Error!u64 {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return error.StatFailed;
    return st.size;
}

/// Both sides map through here: over-reserve `len + 2 MiB` PROT_NONE,
/// `MAP_FIXED|MAP_SHARED` the fd at the PMD-aligned offset, trim the head
/// and tail slack in the kernel's page size (`layout.PmdPlan`). On a
/// reservation failure the plain `mmap(MAP_SHARED)` is the fallback — the
/// alignment buys huge pages, never correctness.
pub fn mapSharedPmdAligned(fd: linux.fd_t, l: layout.SessionLayout) Error!Mapped {
    const len: usize = @intCast(l.total_bytes);
    const page = pageSize();
    const reserve_len: usize = @intCast(layout.PmdPlan.reserveBytes(l.total_bytes, page));
    const reserve = linux.mmap(null, reserve_len, .{}, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
    if (linux.errno(reserve) == .SUCCESS) {
        const plan = layout.PmdPlan.compute(reserve, l.total_bytes, page);
        const fixed = linux.mmap(@ptrFromInt(plan.aligned), len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED, .FIXED = true }, fd, 0);
        if (linux.errno(fixed) == .SUCCESS and fixed == plan.aligned) {
            if (plan.head_slack != 0) _ = linux.munmap(@ptrFromInt(reserve), @intCast(plan.head_slack));
            if (plan.tail_slack != 0) _ = linux.munmap(@ptrFromInt(reserve + plan.reserve_bytes - plan.tail_slack), @intCast(plan.tail_slack));
            const base: [*]align(layout.PAGE_BYTES) u8 = @ptrFromInt(plan.aligned);
            return .{ .mapping = layout.Mapping.init(base, l), .base = base, .len = len };
        }
        _ = linux.munmap(@ptrFromInt(reserve), reserve_len);
    }
    const plain = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
    if (linux.errno(plain) != .SUCCESS) return error.MapFailed;
    const base: [*]align(layout.PAGE_BYTES) u8 = @ptrFromInt(plain);
    return .{ .mapping = layout.Mapping.init(base, l), .base = base, .len = len };
}

pub fn unmap(m: Mapped) void {
    _ = linux.munmap(m.base, m.len);
}

/// The client posture: `MADV_HUGEPAGE` alone (a refusal is invisible: the
/// shmem policy may be `never`).
pub fn advise(m: Mapped) void {
    _ = linux.madvise(m.base, m.len, linux.MADV.HUGEPAGE);
}

/// The daemon's deferred prep (a `sqz-ipc-thp` worker after SESSION_OK —
/// running it inline put O(fleet × arena) of memory work in front of every
/// client's first op): `MADV_HUGEPAGE` + `MADV_POPULATE_WRITE` +
/// `MADV_COLLAPSE`, which collapses whatever `shmem_enabled` says. Returns
/// whether the collapse succeeded (a gauge input, never a failure).
pub fn populateCollapse(m: Mapped) bool {
    _ = linux.madvise(m.base, m.len, linux.MADV.HUGEPAGE);
    _ = linux.madvise(m.base, m.len, MADV_POPULATE_WRITE);
    return linux.errno(linux.madvise(m.base, m.len, MADV_COLLAPSE)) == .SUCCESS;
}

/// `mbind(MPOL_PREFERRED, {node})` over the arena BEFORE first touch:
/// memory follows the chosen owner thread. A single-node host is a
/// structural no-op (the caller passes null). Failure is logged once and
/// ignored: placement is economics, never correctness.
pub fn bindArenaToNode(m: Mapped, node: ?u16) void {
    const n = node orelse return;
    var mask: [16]usize = @splat(0);
    mask[n / @bitSizeOf(usize)] |= @as(usize, 1) << @intCast(n % @bitSizeOf(usize));
    const arena = m.mapping.arena();
    const rc = linux.syscall6(.mbind, @intFromPtr(arena.ptr), arena.len, MPOL_PREFERRED, @intFromPtr(&mask), mask.len * @bitSizeOf(usize), 0);
    if (linux.errno(rc) != .SUCCESS) log.warn("ipc: mbind(MPOL_PREFERRED, node {d}) over the arena failed ({t}); the arena stays first-touch placed", .{ n, linux.errno(rc) });
}

/// `SQUEEZEFS_IPC_ARENA_THP` (both halves).
pub fn thpFromEnv(env: *const std.process.Environ.Map) bool {
    return knobs.boolKnob("SQUEEZEFS_IPC_ARENA_THP", env);
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "createSealed: a valid sealed image — header validates, generation 1, the latch's initial record, the ring seeded; all three seals; the size is the layout's total; a plain memfd fails the seal check" {
    const l = try layout.SessionLayout.compute(.{ .ring_entries = 8, .slots = 4, .arena_bytes = 8 * layout.PAGE_BYTES, .max_op_bytes = layout.PAGE_BYTES });
    const s = try createSealed(l, false);
    defer unmap(s.mapped);
    defer _ = linux.close(s.fd);
    try s.mapped.mapping.header().validate();
    try testing.expectEqual(@as(u64, 1), s.mapped.mapping.header().generation.load(.monotonic));
    try testing.expectEqual(std.math.maxInt(u32), s.mapped.mapping.header().cqe.wake_paid_mark.load(.monotonic));
    for (s.mapped.mapping.ringCells(), 0..) |*c, i| try testing.expectEqual(@as(u32, @intCast(i)), c.seq.load(.monotonic));
    try checkSeals(s.fd);
    try testing.expectEqual(l.total_bytes, try fileSize(s.fd));
    // Sealed means sealed: a grow is refused.
    try testing.expect(linux.errno(linux.ftruncate(s.fd, @intCast(l.total_bytes + 4096))) != .SUCCESS);
    // The PMD-aligned map landed aligned.
    try testing.expectEqual(@as(usize, 0), @intFromPtr(s.mapped.base) % layout.PMD_BYTES);
    // An unsealed memfd is refused at rung 3.
    const rc = linux.memfd_create("sqz-unsealed", linux.MFD.CLOEXEC | linux.MFD.ALLOW_SEALING);
    const plain: linux.fd_t = @intCast(rc);
    defer _ = linux.close(plain);
    try testing.expectError(error.Unsealed, checkSeals(plain));
    // Two of three is still unsealed.
    _ = linux.fcntl(plain, linux.F.ADD_SEALS, linux.F.SEAL_SHRINK | linux.F.SEAL_GROW);
    try testing.expectError(error.Unsealed, checkSeals(plain));
}

test "a second mapping of the same memfd sees the first's words (the two sides of a session); the posture calls are safe on a mapping" {
    const l = try layout.SessionLayout.compute(.{ .ring_entries = 2, .slots = 1, .arena_bytes = layout.PAGE_BYTES, .max_op_bytes = layout.PAGE_BYTES });
    const s = try createSealed(l, true);
    defer unmap(s.mapped);
    defer _ = linux.close(s.fd);
    const other = try mapSharedPmdAligned(s.fd, l);
    defer unmap(other);
    s.mapped.mapping.header().heartbeat.store(0xfeed, .release);
    try testing.expectEqual(@as(u64, 0xfeed), other.mapping.header().heartbeat.load(.acquire));
    _ = populateCollapse(other);
    bindArenaToNode(other, null);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expect(thpFromEnv(&env));
}
