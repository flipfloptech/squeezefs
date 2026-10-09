//! Placed sever (charter §7 "placed sever of whole-block stream chunks
//! straight into a shared assembly the first merge ADOPTS"; spec
//! `docs/porting/il_daemon.md` §3.2). PORTABLE over `accumulate.Pool`.
//!
//! The 1-copy ring write: the pooled path pays TWO copies where the kernel
//! path pays one (arena → severed slab, slab → the block buffer). Placed
//! sever makes the severed DESTINATION the block's future backing: the
//! whole-block-stream chunks of one `(ino, block)` sever into ONE shared
//! ASSEMBLY — a cell of the accumulation pool plus page-granular CLAIMS —
//! the first merging handler adopts as the block's buffer with every
//! LANDED claim already recorded as coverage, so every sibling merge is a
//! no-op ack (its bytes rode the adoption) or a coverage record with the
//! copy elided (pointer proof: only a placed payload can present bytes
//! that already live in the cell).
//!
//! Keying: `(ino, block)` → one assembly, created under the registry's
//! guard and charged to `ipc_placed_assembly_bytes` against the cap
//! `min(budget/8, 2 GiB)` (the session-shm cap's shape; the 2 GiB rail
//! alone before the budget is armed). A claim is `rel/4096 .. +len/4096`
//! exclusive bits (`CLAIM_PAGE`: placed severs are page-aligned by
//! contract); an overlap with a live claim or a SEALED assembly is the
//! pooled fallback (`ipc_placed_sever_fallbacks`). The ONE arena read
//! `cell[rel..][..len] ← arena` runs OUTSIDE the registry guard (a 1 MiB
//! memcpy must not hold the latch) inside the claims WRITER window; the
//! `landed` bits are set under the guard when it ends, so an adoption
//! records exactly the bytes that are in the cell — a claim whose copy
//! never started (the seal beat its writer entry) is never coverage.
//!
//! THE DEKKER (`placed_claims_model.zig`): the claimer publishes
//! `writers += 1` THEN checks `sealed`; the adopter publishes `sealed`
//! THEN reads `writers`; both accesses of each side are SeqCst (the house
//! idiom — Zig 0.17 has no standalone fence), so C11's single total order
//! forbids both sides observing the pre-race value: an adoption never
//! lands over a copy in flight, and a copy never lands into a sealed
//! assembly. Sealing is PERMANENT: a refused seal (a writer mid-copy)
//! leaves the assembly sealed, so the whole cohort demotes to the copy
//! path (`ipc_placed_adoption_refusals`) and the entry reaps at the last
//! payload drop.
//!
//! Refcounts: the cell's own `refs` (one for the registry's share, one per
//! live payload). Adoption hands the registry's share to the table entry
//! with the payloads' shares still out, so the adopted cell is NON-UNIQUE
//! and an in-place mutation copies away instead of touching severed bytes
//! (`accumulate.Pool.uniqueOrCopy`). An adopted entry LINGERS in the
//! registry (sealed, `adopted`) until its last payload drops, so a sibling
//! merge can learn its bytes rode the adoption; a never-adopted assembly
//! reaps at `outstanding == 0` (`ipc_placed_reaps`), the cell back to the
//! pool.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const derive = @import("../../core/derive.zig");
const jc = @import("../../meta/journal_core.zig");
const budget = @import("../../mem/budget.zig");
const accumulate = @import("../../data/accumulate.zig");
const cov = @import("../../data/write_coverage.zig");
const stats = @import("stats.zig").stats;

pub const Key = accumulate.Key;
pub const Pool = accumulate.Pool;
pub const Cell = accumulate.Pool.Cell;
pub const Run = cov.Run;

/// Placed severs are page-aligned by contract: the claim grain.
pub const CLAIM_PAGE: u32 = 4096;
/// The assembly cap's rail (`min(budget/8, 2 GiB)`).
pub const ASSEMBLY_CAP_RAIL: u64 = 2 * derive.GiB;
/// The most landed runs an adoption may carry: the coverage union's own
/// table (a placed chunk is ≥ a quarter block, so ≤ 4 runs in practice).
pub const MAX_ADOPTED_RUNS: usize = cov.CoverageUnion.max_runs;

/// `min(budget/8, 2 GiB)`; a zero (unarmed) budget is the rail alone.
pub fn assemblyCapBytes(budget_bytes: u64) u64 {
    if (budget_bytes == 0) return ASSEMBLY_CAP_RAIL;
    return @min(budget_bytes / 8, ASSEMBLY_CAP_RAIL);
}

// ---------------------------------------------------------------------------
// The claims core (the model restates it step for step).
// ---------------------------------------------------------------------------

pub const Claims = struct {
    /// One bit per `CLAIM_PAGE`: reserved (exclusive) and landed (the copy
    /// ended), both guarded by the registry's lock.
    claimed: []u64,
    landed: []u64,
    /// The Dekker pair (module doc): SeqCst on both sides.
    writers: Atomic(u32) = .init(0),
    sealed: Atomic(u32) = .init(0),

    pub fn init(gpa: Allocator, block_size: u32) Allocator.Error!Claims {
        const pages = derive.ceilDiv(block_size, CLAIM_PAGE);
        const words: usize = @intCast(derive.ceilDiv(pages, 64));
        const claimed = try gpa.alloc(u64, words);
        errdefer gpa.free(claimed);
        const landed = try gpa.alloc(u64, words);
        @memset(claimed, 0);
        @memset(landed, 0);
        return .{ .claimed = claimed, .landed = landed };
    }

    pub fn deinit(self: *Claims, gpa: Allocator) void {
        gpa.free(self.claimed);
        gpa.free(self.landed);
    }

    fn overlaps(bits: []const u64, first: u32, pages: u32) bool {
        var p = first;
        while (p < first + pages) : (p += 1) if (bits[p / 64] & (@as(u64, 1) << @intCast(p % 64)) != 0) return true;
        return false;
    }

    fn setBits(bits: []u64, first: u32, pages: u32, on: bool) void {
        var p = first;
        while (p < first + pages) : (p += 1) {
            const m = @as(u64, 1) << @intCast(p % 64);
            if (on) bits[p / 64] |= m else bits[p / 64] &= ~m;
        }
    }

    /// Under the registry lock: exclusive bits for `[first, first + pages)`;
    /// false on an overlap or a sealed assembly.
    pub fn beginClaimLocked(self: *Claims, first: u32, pages: u32) bool {
        if (self.isSealed() or overlaps(self.claimed, first, pages)) return false;
        setBits(self.claimed, first, pages, true);
        return true;
    }

    pub fn releaseLocked(self: *Claims, first: u32, pages: u32) void {
        setBits(self.claimed, first, pages, false);
        setBits(self.landed, first, pages, false);
    }

    pub fn landedLocked(self: *Claims, first: u32, pages: u32) void {
        setBits(self.landed, first, pages, true);
    }

    /// The claimer's half of the Dekker, OUTSIDE the lock: publish the
    /// writer (SeqCst), then "did you seal?" (SeqCst). False ⇒ the caller
    /// undoes its claim and takes the pooled path.
    pub fn beginWrite(self: *Claims) bool {
        _ = self.writers.fetchAdd(1, .seq_cst);
        if (self.sealed.load(.seq_cst) != 0) {
            _ = self.writers.fetchSub(1, .seq_cst);
            return false;
        }
        return true;
    }

    /// The copy landed and its bits are set: the writer window closes
    /// (SeqCst — the release half pairs with the adopter's SeqCst read of
    /// `writers`, so an adopter that sees 0 sees the landed bits).
    pub fn endWrite(self: *Claims) void {
        _ = self.writers.fetchSub(1, .seq_cst);
    }

    /// The adopter's half: publish the seal (SeqCst, permanent), then "any
    /// writer?" (SeqCst). False ⇒ a copy is in flight: the adoption is
    /// refused and the assembly stays sealed.
    pub fn sealForAdoption(self: *Claims) bool {
        self.sealed.store(1, .seq_cst);
        return self.writers.load(.seq_cst) == 0;
    }

    pub fn isSealed(self: *const Claims) bool {
        return self.sealed.load(.monotonic) != 0;
    }

    /// The landed pages as byte runs, ascending and coalesced; null when
    /// they exceed `out` (the adoption is refused before any seal).
    pub fn landedRuns(self: *const Claims, block_size: u32, out: *[MAX_ADOPTED_RUNS]Run) ?[]Run {
        var n: usize = 0;
        var p: u32 = 0;
        const pages: u32 = @intCast(derive.ceilDiv(block_size, CLAIM_PAGE));
        while (p < pages) : (p += 1) {
            if (self.landed[p / 64] & (@as(u64, 1) << @intCast(p % 64)) == 0) continue;
            const start = p * CLAIM_PAGE;
            const end = @min(start + CLAIM_PAGE, block_size);
            if (n > 0 and out[n - 1].end == start) {
                out[n - 1].end = end;
            } else {
                if (n == out.len) return null;
                out[n] = .{ .start = start, .end = end };
                n += 1;
            }
        }
        return out[0..n];
    }
};

// ---------------------------------------------------------------------------
// The registry.
// ---------------------------------------------------------------------------

const Assembly = struct {
    key: Key,
    cell: *Cell,
    claims: Claims,
    /// Live payloads (reap at 0).
    outstanding: u32,
    /// Monotonic per registry: a payload names its assembly by `(key, gen)`
    /// so a reaped-and-recreated entry of the same key is never confused
    /// with the one it severed into.
    gen: u64,
    /// The first merge took the cell; the entry lingers for its siblings.
    adopted: bool,
    in_use: bool,
};

/// A placed payload: a view into the assembly's cell, the claim it holds.
pub const Placed = struct {
    key: Key,
    gen: u64,
    cell: *Cell,
    rel: u32,
    len: u32,

    pub fn bytes(self: Placed) []const u8 {
        return self.cell.data[self.rel .. self.rel + self.len];
    }
};

/// Why a placed-eligible chunk fell back to the pooled sever.
pub const Fallback = enum { cap, pool, overlap, sealed, table_full };

pub const SeverOutcome = union(enum) {
    placed: Placed,
    fallback: Fallback,
};

/// What an adoption hands the merge: the cell (the registry's share now
/// the caller's) and the landed runs it must record as coverage.
pub const Adopted = struct {
    cell: *Cell,
    runs_buf: [MAX_ADOPTED_RUNS]Run,
    n_runs: usize,

    pub fn runs(self: *const Adopted) []const Run {
        return self.runs_buf[0..self.n_runs];
    }
};

pub const Registry = struct {
    gpa: Allocator,
    pool: *Pool,
    block_size: u32,
    cap_bytes: u64,
    charged: Atomic(u64) = .init(0),
    next_gen: u64 = 1,
    lock: jc.SpinLock = .{},
    entries: []Assembly,
    free: std.ArrayList(u32) = .empty,
    index: std.AutoHashMapUnmanaged(Key, u32) = .empty,

    /// `cap_bytes / block_size` entries (the cap IS the population bound).
    pub fn init(gpa: Allocator, pool: *Pool, block_size: u32, cap_bytes: u64) Allocator.Error!Registry {
        const n: usize = @intCast(@max(cap_bytes / block_size, 1));
        const entries = try gpa.alloc(Assembly, n);
        errdefer gpa.free(entries);
        for (entries) |*e| e.in_use = false;
        var free = try std.ArrayList(u32).initCapacity(gpa, n);
        errdefer free.deinit(gpa);
        var i: u32 = @intCast(n);
        while (i > 0) : (i -= 1) free.appendAssumeCapacity(i - 1);
        var self: Registry = .{ .gpa = gpa, .pool = pool, .block_size = block_size, .cap_bytes = cap_bytes, .entries = entries, .free = free };
        try self.index.ensureTotalCapacity(gpa, @intCast(n));
        return self;
    }

    pub fn deinit(self: *Registry) void {
        for (self.entries) |*e| if (e.in_use) {
            e.claims.deinit(self.gpa);
            if (!e.adopted) {
                self.pool.release(e.cell);
                stats.ipc_placed_assembly_bytes.sub(self.block_size);
            }
        };
        self.index.deinit(self.gpa);
        self.free.deinit(self.gpa);
        self.gpa.free(self.entries);
        self.* = undefined;
    }

    /// The R5 component: assemblies are NON-sheddable (the first merge
    /// adopts them; they drain by adoption or drop), floor 0, weight 0.
    pub fn component(_: *Registry) budget.Component {
        return budget.Component.init("placed_assemblies", 0, 0, .{ .gauge = &stats.ipc_placed_assembly_bytes }, null);
    }

    pub fn liveAssemblies(self: *Registry) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.index.count();
    }

    /// Sever `src` (the arena window, `len == src.len`) into the assembly
    /// of `(ino, block)` at `rel`: get-or-create under the guard, the claim,
    /// the ONE arena read inside the writer window, outside the guard.
    pub fn sever(self: *Registry, ino: u64, block: u32, rel: u32, src: []const u8) SeverOutcome {
        assert(rel % CLAIM_PAGE == 0 and src.len % CLAIM_PAGE == 0 and src.len != 0);
        assert(rel + src.len <= self.block_size);
        const first: u32 = rel / CLAIM_PAGE;
        const pages: u32 = @intCast(src.len / CLAIM_PAGE);
        const key: Key = .{ .ino = ino, .block = block };
        var a: *Assembly = undefined;
        {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.index.get(key)) |idx| {
                a = &self.entries[idx];
            } else {
                a = self.createLocked(key) catch |e| return .{ .fallback = switch (e) {
                    error.Cap => .cap,
                    error.PoolExhausted => .pool,
                    error.TableFull => .table_full,
                } };
            }
            if (!a.claims.beginClaimLocked(first, pages)) return .{ .fallback = if (a.claims.isSealed()) .sealed else .overlap };
            a.outstanding += 1;
            _ = a.cell.share();
        }
        const cell = a.cell;
        const gen = a.gen;
        if (!a.claims.beginWrite()) {
            // Sealed between the claim and the write: undo, pooled path.
            self.lock.lock();
            defer self.lock.unlock();
            a.claims.releaseLocked(first, pages);
            self.dropLocked(a, cell);
            return .{ .fallback = .sealed };
        }
        @memcpy(cell.data[rel .. rel + src.len], src);
        {
            self.lock.lock();
            defer self.lock.unlock();
            a.claims.landedLocked(first, pages);
        }
        a.claims.endWrite();
        stats.ipc_placed_severs.inc();
        return .{ .placed = .{ .key = key, .gen = gen, .cell = cell, .rel = rel, .len = @intCast(src.len) } };
    }

    fn createLocked(self: *Registry, key: Key) error{ Cap, PoolExhausted, TableFull }!*Assembly {
        if (self.charged.load(.monotonic) + self.block_size > self.cap_bytes) return error.Cap;
        const idx = self.free.pop() orelse return error.TableFull;
        const cell = self.pool.acquire() orelse {
            self.free.appendAssumeCapacity(idx);
            return error.PoolExhausted;
        };
        const claims = Claims.init(self.gpa, self.block_size) catch {
            self.pool.release(cell);
            self.free.appendAssumeCapacity(idx);
            return error.PoolExhausted;
        };
        const a = &self.entries[idx];
        a.* = .{ .key = key, .cell = cell, .claims = claims, .outstanding = 0, .gen = self.next_gen, .adopted = false, .in_use = true };
        self.next_gen += 1;
        self.index.putAssumeCapacity(key, idx);
        _ = self.charged.fetchAdd(self.block_size, .monotonic);
        stats.ipc_placed_assembly_bytes.add(self.block_size);
        return a;
    }

    /// A payload's share goes back; the last one out reaps the entry (a
    /// never-adopted assembly's cell with it). Under the lock.
    fn dropLocked(self: *Registry, a: *Assembly, cell: *Cell) void {
        self.pool.release(cell);
        a.outstanding -= 1;
        if (a.outstanding == 0) self.reapLocked(a);
    }

    fn reapLocked(self: *Registry, a: *Assembly) void {
        assert(self.index.remove(a.key));
        a.claims.deinit(self.gpa);
        if (!a.adopted) {
            // The registry's own share: the cell returns to the pool.
            self.pool.release(a.cell);
            _ = self.charged.fetchSub(self.block_size, .monotonic);
            stats.ipc_placed_assembly_bytes.sub(self.block_size);
            stats.ipc_placed_reaps.inc();
        }
        a.in_use = false;
        const idx: u32 = @intCast((@intFromPtr(a) - @intFromPtr(self.entries.ptr)) / @sizeOf(Assembly));
        self.free.appendAssumeCapacity(idx);
    }

    /// The payload is consumed (merged or abandoned): release its claim
    /// and its share; reap the entry at zero.
    pub fn drop(self: *Registry, p: Placed) void {
        self.lock.lock();
        defer self.lock.unlock();
        if (self.index.get(p.key)) |idx| {
            const a = &self.entries[idx];
            if (a.gen == p.gen) {
                if (!a.adopted) a.claims.releaseLocked(p.rel / CLAIM_PAGE, p.len / CLAIM_PAGE);
                self.dropLocked(a, p.cell);
                return;
            }
        }
        // Already reaped (an impossible late drop guarded by `gen`): the share alone.
        self.pool.release(p.cell);
    }

    /// Did this payload's assembly get adopted (its landed bytes are the
    /// table entry's coverage — the merge is a no-op ack)?
    pub fn adopted(self: *Registry, p: Placed) bool {
        return self.adoptedByToken(p.key, p.gen);
    }

    pub fn adoptedByToken(self: *Registry, key: Key, gen: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const idx = self.index.get(key) orelse return false;
        const a = &self.entries[idx];
        return a.gen == gen and a.adopted;
    }

    /// The write merge's entry-absent branch, under the block's level-3
    /// guard: the assembly of `(ino, block)` whose region at `rel` IS
    /// `ptr` (the pointer proof), not yet adopted, whose landed runs fit
    /// the coverage table and whose seal succeeds hands its cell over —
    /// the registry's share transfers to the caller, the payloads' shares
    /// stay out, the entry lingers `adopted`. Null: no such assembly, a
    /// foreign pointer, or a refused seal (counted; the cohort demotes to
    /// the copy path).
    pub fn takeForAdoption(self: *Registry, ino: u64, block: u32, rel: u32, ptr: [*]const u8) ?Adopted {
        self.lock.lock();
        defer self.lock.unlock();
        const idx = self.index.get(.{ .ino = ino, .block = block }) orelse return null;
        const a = &self.entries[idx];
        if (a.adopted) return null;
        if (@intFromPtr(ptr) != @intFromPtr(a.cell.data.ptr) + rel) return null;
        var out: Adopted = .{ .cell = a.cell, .runs_buf = undefined, .n_runs = 0 };
        const runs = a.claims.landedRuns(self.block_size, &out.runs_buf) orelse {
            stats.ipc_placed_adoption_refusals.inc();
            return null;
        };
        out.n_runs = runs.len;
        if (!a.claims.sealForAdoption()) {
            stats.ipc_placed_adoption_refusals.inc();
            return null;
        }
        a.adopted = true;
        _ = self.charged.fetchSub(self.block_size, .monotonic);
        stats.ipc_placed_assembly_bytes.sub(self.block_size);
        stats.ipc_placed_adoptions.inc();
        return out;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const BS: u32 = 16 * CLAIM_PAGE;

test "assembly cap = min(budget/8, 2 GiB); the rail alone before the budget is armed" {
    try testing.expectEqual(ASSEMBLY_CAP_RAIL, assemblyCapBytes(0));
    try testing.expectEqual(2 * derive.GiB, assemblyCapBytes(176 * derive.GiB));
    try testing.expectEqual(8 * derive.GiB / 8, assemblyCapBytes(8 * derive.GiB));
}

test "placed payload reads back and reaps at zero: two chunks of one block share one assembly; overlapping claims fall back; the cell returns to the pool when the last payload drops; the cap holds" {
    var pool = Pool.init(testing.allocator, BS, 8);
    defer pool.deinit();
    var r = try Registry.init(testing.allocator, &pool, BS, 4 * BS);
    defer r.deinit();
    const bytes0 = stats.ipc_placed_assembly_bytes.load();
    const reaps0 = stats.ipc_placed_reaps.load();
    var src: [4 * CLAIM_PAGE]u8 = @splat(0xA5);
    const a = switch (r.sever(7, 3, 0, &src)) {
        .placed => |p| p,
        .fallback => return error.TestUnexpectedFallback,
    };
    @memset(&src, 0x5A);
    const b = switch (r.sever(7, 3, 4 * CLAIM_PAGE, &src)) {
        .placed => |p| p,
        .fallback => return error.TestUnexpectedFallback,
    };
    try testing.expectEqual(a.cell, b.cell);
    try testing.expectEqual(@as(usize, 1), r.liveAssemblies());
    try testing.expectEqual(bytes0 + BS, stats.ipc_placed_assembly_bytes.load());
    for (a.bytes()) |v| try testing.expectEqual(@as(u8, 0xA5), v);
    for (b.bytes()) |v| try testing.expectEqual(@as(u8, 0x5A), v);
    // Pointer proof: the payload's bytes ARE the cell's region.
    try testing.expectEqual(@intFromPtr(a.cell.data.ptr) + 4 * CLAIM_PAGE, @intFromPtr(b.bytes().ptr));
    // An overlapping claim falls back (the first page of `b`).
    try testing.expectEqual(SeverOutcome{ .fallback = .overlap }, r.sever(7, 3, 4 * CLAIM_PAGE, src[0..CLAIM_PAGE]));
    // Refs: the registry's share + two payloads.
    try testing.expectEqual(@as(u32, 3), a.cell.refs.load(.monotonic));
    try testing.expect(!r.adopted(a));
    r.drop(a);
    try testing.expectEqual(@as(usize, 1), r.liveAssemblies());
    r.drop(b);
    try testing.expectEqual(@as(usize, 0), r.liveAssemblies());
    try testing.expectEqual(reaps0 + 1, stats.ipc_placed_reaps.load());
    try testing.expectEqual(bytes0, stats.ipc_placed_assembly_bytes.load());
    // The cell is back in the pool: the next acquire reuses it.
    try testing.expectEqual(a.cell, pool.acquire().?);
    pool.release(a.cell);
    // The cap: 4 blocks charge; the fifth key falls back.
    var keep: [4]Placed = undefined;
    for (&keep, 0..) |*k, i| k.* = r.sever(100 + @as(u64, i), 0, 0, src[0..CLAIM_PAGE]).placed;
    try testing.expectEqual(SeverOutcome{ .fallback = .cap }, r.sever(200, 0, 0, src[0..CLAIM_PAGE]));
    for (keep) |k| r.drop(k);
}

test "adoption takes the backing with the LANDED runs as coverage and seals new claims out; a foreign pointer never adopts; the entry lingers so siblings learn they rode the adoption; their drops release only shares" {
    var pool = Pool.init(testing.allocator, BS, 8);
    defer pool.deinit();
    var r = try Registry.init(testing.allocator, &pool, BS, 4 * BS);
    defer r.deinit();
    const adopt0 = stats.ipc_placed_adoptions.load();
    const bytes0 = stats.ipc_placed_assembly_bytes.load();
    var src: [CLAIM_PAGE]u8 = @splat(1);
    const a = r.sever(9, 0, 0, &src).placed;
    const b = r.sever(9, 0, CLAIM_PAGE, &src).placed;
    const c = r.sever(9, 0, 3 * CLAIM_PAGE, &src).placed;
    // A foreign pointer (a pooled slab) never adopts.
    var foreign: [CLAIM_PAGE]u8 = undefined;
    try testing.expect(r.takeForAdoption(9, 0, 0, &foreign) == null);
    // The right pointer at the wrong rel is not proof either.
    try testing.expect(r.takeForAdoption(9, 0, CLAIM_PAGE, a.bytes().ptr) == null);
    const ad = r.takeForAdoption(9, 0, 0, a.bytes().ptr).?;
    try testing.expectEqual(a.cell, ad.cell);
    // Two coalesced runs: [0, 2 pages) and [3, 4 pages).
    try testing.expectEqual(@as(usize, 2), ad.runs().len);
    try testing.expectEqual(Run{ .start = 0, .end = 2 * CLAIM_PAGE }, ad.runs()[0]);
    try testing.expectEqual(Run{ .start = 3 * CLAIM_PAGE, .end = 4 * CLAIM_PAGE }, ad.runs()[1]);
    try testing.expectEqual(adopt0 + 1, stats.ipc_placed_adoptions.load());
    try testing.expectEqual(bytes0, stats.ipc_placed_assembly_bytes.load());
    // The entry lingers adopted; a second adoption is refused silently.
    try testing.expectEqual(@as(usize, 1), r.liveAssemblies());
    try testing.expect(r.adopted(a) and r.adopted(b) and r.adopted(c));
    try testing.expect(r.takeForAdoption(9, 0, 0, a.bytes().ptr) == null);
    // The registry's share travelled: 1 (ours) + 3 payloads.
    try testing.expectEqual(@as(u32, 4), ad.cell.refs.load(.monotonic));
    // A later chunk of the same block finds the sealed entry: the pooled
    // fallback (an entry-present block never adopts).
    try testing.expectEqual(SeverOutcome{ .fallback = .sealed }, r.sever(9, 0, 2 * CLAIM_PAGE, &src));
    // Dropping the adopted cohort's payloads releases shares only; the
    // last drop reaps the lingering entry without touching the cell.
    r.drop(a);
    r.drop(b);
    try testing.expectEqual(@as(usize, 1), r.liveAssemblies());
    r.drop(c);
    try testing.expectEqual(@as(usize, 0), r.liveAssemblies());
    try testing.expectEqual(@as(u32, 1), ad.cell.refs.load(.monotonic));
    pool.release(ad.cell);
}

test "the Dekker: a seal while a writer is inside the window is refused and counted, the assembly stays sealed, later claims fall back as sealed, the cohort reaps at the last drop" {
    var pool = Pool.init(testing.allocator, BS, 8);
    defer pool.deinit();
    var r = try Registry.init(testing.allocator, &pool, BS, 4 * BS);
    defer r.deinit();
    const refusals0 = stats.ipc_placed_adoption_refusals.load();
    var src: [CLAIM_PAGE]u8 = @splat(2);
    const a = r.sever(11, 0, 0, &src).placed;
    // Simulate a writer mid-copy on the live assembly.
    const idx = r.index.get(a.key).?;
    try testing.expect(r.entries[idx].claims.beginWrite());
    try testing.expect(r.takeForAdoption(11, 0, 0, a.bytes().ptr) == null);
    try testing.expectEqual(refusals0 + 1, stats.ipc_placed_adoption_refusals.load());
    r.entries[idx].claims.endWrite();
    // Sealed for good: a new claim falls back as sealed; the cohort demoted.
    try testing.expectEqual(SeverOutcome{ .fallback = .sealed }, r.sever(11, 0, CLAIM_PAGE, &src));
    try testing.expect(r.entries[idx].claims.isSealed());
    try testing.expect(!r.adopted(a));
    r.drop(a);
    try testing.expectEqual(@as(usize, 0), r.liveAssemblies());
}

test "landed runs: only pages whose copy ended count; a claim the seal beat is never coverage; the grain and the rail are the contract's" {
    var c = try Claims.init(testing.allocator, BS);
    defer c.deinit(testing.allocator);
    try testing.expect(c.beginClaimLocked(0, 2));
    try testing.expect(c.beginClaimLocked(4, 1));
    try testing.expect(!c.beginClaimLocked(1, 1));
    c.landedLocked(4, 1);
    var buf: [MAX_ADOPTED_RUNS]Run = undefined;
    const runs = c.landedRuns(BS, &buf).?;
    try testing.expectEqual(@as(usize, 1), runs.len);
    try testing.expectEqual(Run{ .start = 4 * CLAIM_PAGE, .end = 5 * CLAIM_PAGE }, runs[0]);
    try testing.expect(c.sealForAdoption());
    try testing.expect(!c.beginClaimLocked(8, 1));
    try testing.expect(!c.beginWrite());
    try testing.expectEqual(@as(u32, 4096), CLAIM_PAGE);
    try testing.expectEqual(2 * derive.GiB, ASSEMBLY_CAP_RAIL);
}
