//! The metadata plane's mutex: the exclusive-only face over the `rwlock.zig`
//! core (the reference's `SqzMutex`), so every law — no ownership
//! assignment to sleepers, bounded barging, the 2 s ticked park counted in
//! `lock_ticked_reregisters`, grant-or-enqueue in one critical section — is
//! the rwlock's by construction.

const std = @import("std");
const Io = std.Io;
const rwlock = @import("rwlock.zig");

pub const Mutex = struct {
    core: rwlock.RwLock = .init,

    pub const init: Mutex = .{};

    pub fn lock(self: *Mutex, io: Io) Io.Cancelable!void {
        return self.core.lock(io);
    }

    /// Committers are never cancelled (charter §9).
    pub fn lockUncancelable(self: *Mutex, io: Io) void {
        self.core.lockUncancelable(io);
    }

    /// FIFO courtesy applies: refused while anyone is queued.
    pub fn tryLock(self: *Mutex, io: Io) bool {
        return self.core.tryLock(io);
    }

    pub fn unlock(self: *Mutex, io: Io) void {
        self.core.unlock(io);
    }

    /// Parked acquirers (the deterministic park observable).
    pub fn waiters(self: *Mutex, io: Io) u32 {
        return self.core.waiters(io);
    }
};

const testing = std.testing;

test "mutex_excludes_under_contention: 8 threads × 1000 increments land exactly" {
    const Shared = struct {
        m: Mutex = .init,
        n: u64 = 0,
        fn run(s: *@This()) void {
            var i: usize = 0;
            while (i < 1000) : (i += 1) {
                s.m.lockUncancelable(testing.io);
                s.n += 1;
                s.m.unlock(testing.io);
            }
        }
    };
    var s: Shared = .{};
    var ts: [8]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, Shared.run, .{&s});
    for (ts) |t| t.join();
    try s.m.lock(testing.io);
    try testing.expectEqual(@as(u64, 8000), s.n);
    s.m.unlock(testing.io);
    try testing.expectEqual(Mutex.init, s.m);
}

test "tryLock refuses a held mutex and a free one with a queued waiter; the release reaches the waiter" {
    const io = testing.io;
    var m: Mutex = .init;
    try testing.expect(m.tryLock(io));
    try testing.expect(!m.tryLock(io));
    const Parked = struct {
        fn run(mx: *Mutex) void {
            mx.lockUncancelable(testing.io);
            mx.unlock(testing.io);
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{&m});
    while (m.waiters(io) == 0) try std.Thread.yield();
    m.unlock(io);
    t.join();
    try testing.expectEqual(Mutex.init, m);
}
