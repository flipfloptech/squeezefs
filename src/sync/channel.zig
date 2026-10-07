//! Channels whose waiters register under the lock the senders mutate under
//! (charter §9; the reference's `sqz_channel.rs`): `Oneshot`, bounded
//! `Mpsc`, `Watch`.
//!
//! One short-hold interior mutex per channel. The lost-send window does not
//! exist by construction: a receiver re-checks state and registers its
//! waiter under the SAME lock a sender mutates state under, and the sender
//! delivers the wake token under that lock. Every unbounded park is ticked
//! (`tick.zig`, `channel_ticked_reregisters`). The mpsc send side's capacity
//! waiters are FIFO-woken and re-contend (bounded barging).
//!
//! Zig has no drop glue, so the reference's handle lifetimes are explicit
//! calls: `closeSender` / `closeReceiver` (oneshot, watch) and counted
//! `Sender` handles (`sender()` / `Sender.deinit`) on the mpsc. Zero
//! allocation: the mpsc ring is inline (`capacity` is comptime).

const std = @import("std");
const Io = std.Io;
const assert = std.debug.assert;
const tick = @import("tick.zig");

/// A parked receiver or capacity-waiting sender.
pub const Waiter = struct {
    next: ?*Waiter = null,
    prev: ?*Waiter = null,
    linked: bool = false,
    state: std.atomic.Value(u32) = .init(0),
};

/// An intrusive FIFO shared by the three channels.
const Queue = struct {
    head: ?*Waiter = null,
    tail: ?*Waiter = null,

    fn push(q: *Queue, w: *Waiter) void {
        assert(!w.linked);
        w.next = null;
        w.prev = q.tail;
        if (q.tail) |t| t.next = w else q.head = w;
        q.tail = w;
        w.linked = true;
    }

    fn unlink(q: *Queue, w: *Waiter) void {
        assert(w.linked);
        if (w.prev) |p| p.next = w.next else q.head = w.next;
        if (w.next) |x| x.prev = w.prev else q.tail = w.prev;
        w.next = null;
        w.prev = null;
        w.linked = false;
    }

    fn wakeFront(q: *Queue, io: Io) void {
        if (q.head) |w| tick.wake(io, &w.state);
    }

    fn wakeAll(q: *Queue, io: Io) void {
        var cur = q.head;
        while (cur) |w| : (cur = w.next) tick.wake(io, &w.state);
    }
};

/// Park `w` (its word already reset to 0 under the lock) for one tick; the
/// caller unregisters under the lock on `error.Canceled`.
fn parkOnce(io: Io, w: *Waiter, comptime cancelable: bool) Io.Cancelable!void {
    if (cancelable) {
        _ = try tick.park(io, &w.state, &tick.channel_ticked_reregisters);
    } else {
        _ = tick.parkUncancelable(io, &w.state, &tick.channel_ticked_reregisters);
    }
}

// ===========================================================================
// Oneshot
// ===========================================================================

pub fn Oneshot(comptime T: type) type {
    return struct {
        const Self = @This();

        interior: Io.Mutex = .init,
        value: ?T = null,
        /// Sender gone (sent or closed).
        closed: bool = false,
        /// Receiver gone — `send` hands the value back.
        rx_gone: bool = false,
        rx: Queue = .{},

        pub const init: Self = .{};

        pub const SendError = error{ReceiverGone};
        pub const RecvError = error{ Canceled, SenderGone };
        pub const TryRecvError = error{ Empty, Closed };

        /// Send once; the sender is closed afterwards.
        pub fn send(self: *Self, io: Io, value: T) SendError!void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            if (self.rx_gone) return error.ReceiverGone;
            assert(!self.closed);
            self.value = value;
            self.closed = true;
            self.rx.wakeFront(io);
        }

        /// The sender goes away without sending: a parked receiver fails.
        pub fn closeSender(self: *Self, io: Io) void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            self.closed = true;
            self.rx.wakeFront(io);
        }

        pub fn closeReceiver(self: *Self, io: Io) void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            self.rx_gone = true;
        }

        pub fn isClosed(self: *Self, io: Io) bool {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            return self.rx_gone;
        }

        /// The receiver is parked (the deterministic park observable).
        pub fn receiverParked(self: *Self, io: Io) bool {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            return self.rx.head != null;
        }

        pub fn tryRecv(self: *Self, io: Io) TryRecvError!T {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            if (self.value) |v| {
                self.value = null;
                return v;
            }
            return if (self.closed) error.Closed else error.Empty;
        }

        pub fn recv(self: *Self, io: Io) RecvError!T {
            return self.recvInner(io, true);
        }

        pub fn recvUncancelable(self: *Self, io: Io) error{SenderGone}!T {
            return self.recvInner(io, false) catch |err| switch (err) {
                error.Canceled => unreachable,
                error.SenderGone => |e| return e,
            };
        }

        fn recvInner(self: *Self, io: Io, comptime cancelable: bool) RecvError!T {
            var w: Waiter = .{};
            while (true) {
                {
                    self.interior.lockUncancelable(io);
                    defer self.interior.unlock(io);
                    if (self.value) |v| {
                        self.value = null;
                        if (w.linked) self.rx.unlink(&w);
                        return v;
                    }
                    if (self.closed) {
                        if (w.linked) self.rx.unlink(&w);
                        return error.SenderGone;
                    }
                    if (!w.linked) self.rx.push(&w);
                    w.state.store(0, .monotonic);
                }
                parkOnce(io, &w, cancelable) catch |err| switch (err) {
                    error.Canceled => {
                        self.interior.lockUncancelable(io);
                        defer self.interior.unlock(io);
                        if (w.linked) self.rx.unlink(&w);
                        return err;
                    },
                };
            }
        }
    };
}

// ===========================================================================
// Mpsc (bounded)
// ===========================================================================

pub fn Mpsc(comptime T: type, comptime capacity: usize) type {
    comptime assert(capacity > 0);
    return struct {
        const Self = @This();

        interior: Io.Mutex = .init,
        buf: [capacity]T = undefined,
        head: usize = 0,
        len: usize = 0,
        /// Live `Sender` handles.
        senders: u32 = 0,
        /// A `Sender` has been minted. The reference's `channel()` hands out
        /// its first sender at creation, so `senders == 0` there always means
        /// "the last one went away"; here `init` has no handle yet, and a
        /// receiver scheduled ahead of the first `sender()` must park on the
        /// not-yet-connected channel rather than read it as closed-and-drained.
        connected: bool = false,
        rx_alive: bool = true,
        /// Receiver-side `close`: sends refuse, drained receives still serve.
        closed: bool = false,
        rx: Queue = .{},
        /// FIFO capacity waiters (the bounded send side).
        tx: Queue = .{},

        pub const init: Self = .{};

        pub const SendError = error{ Canceled, Closed };
        pub const TrySendError = error{ Full, Closed };
        pub const TryRecvError = error{ Empty, Disconnected };

        pub const Sender = struct {
            chan: *Self,

            pub fn trySend(s: Sender, io: Io, value: T) TrySendError!void {
                return s.chan.trySend(io, value);
            }

            /// Parks FIFO on capacity, ticked.
            pub fn send(s: Sender, io: Io, value: T) SendError!void {
                return s.chan.sendInner(io, value, true);
            }

            pub fn sendUncancelable(s: Sender, io: Io, value: T) error{Closed}!void {
                return s.chan.sendInner(io, value, false) catch |err| switch (err) {
                    error.Canceled => unreachable,
                    error.Closed => |e| return e,
                };
            }

            pub fn isClosed(s: Sender, io: Io) bool {
                s.chan.interior.lockUncancelable(io);
                defer s.chan.interior.unlock(io);
                return s.chan.closed or !s.chan.rx_alive;
            }

            /// The handle goes away; the last one closes the channel for the
            /// receiver once it drains.
            pub fn deinit(s: Sender, io: Io) void {
                s.chan.interior.lockUncancelable(io);
                defer s.chan.interior.unlock(io);
                assert(s.chan.senders > 0);
                s.chan.senders -= 1;
                if (s.chan.senders == 0) s.chan.rx.wakeFront(io);
            }
        };

        /// A counted sender handle.
        pub fn sender(self: *Self, io: Io) Sender {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            self.senders += 1;
            self.connected = true;
            return .{ .chan = self };
        }

        /// Every sender is gone: closed-and-drained once empty. Never true
        /// before the first `sender()` (see `connected`).
        fn sendersGoneLocked(self: *const Self) bool {
            return self.connected and self.senders == 0;
        }

        pub fn trySend(self: *Self, io: Io, value: T) TrySendError!void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            if (self.closed or !self.rx_alive) return error.Closed;
            if (self.len == capacity) return error.Full;
            self.pushLocked(value);
            self.rx.wakeFront(io);
        }

        fn sendInner(self: *Self, io: Io, value: T, comptime cancelable: bool) SendError!void {
            var w: Waiter = .{};
            while (true) {
                {
                    self.interior.lockUncancelable(io);
                    defer self.interior.unlock(io);
                    if (self.closed or !self.rx_alive) {
                        if (w.linked) self.tx.unlink(&w);
                        return error.Closed;
                    }
                    // Fresh: room AND nobody queued (FIFO courtesy); queued:
                    // room (the re-contend barges).
                    if (self.len < capacity and (w.linked or self.tx.head == null)) {
                        if (w.linked) self.tx.unlink(&w);
                        self.pushLocked(value);
                        self.rx.wakeFront(io);
                        // Leftover room may serve the next capacity waiter.
                        if (self.len < capacity) self.tx.wakeFront(io);
                        return;
                    }
                    if (!w.linked) self.tx.push(&w);
                    w.state.store(0, .monotonic);
                }
                parkOnce(io, &w, cancelable) catch |err| switch (err) {
                    error.Canceled => {
                        self.interior.lockUncancelable(io);
                        defer self.interior.unlock(io);
                        if (w.linked) self.tx.unlink(&w);
                        if (self.len < capacity) self.tx.wakeFront(io);
                        return err;
                    },
                };
            }
        }

        pub fn tryRecv(self: *Self, io: Io) TryRecvError!T {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            if (self.len > 0) {
                const v = self.popLocked();
                self.tx.wakeFront(io);
                return v;
            }
            return if (self.sendersGoneLocked() or self.closed) error.Disconnected else error.Empty;
        }

        /// Receive; `null` = closed AND drained. Parks ticked.
        pub fn recv(self: *Self, io: Io) Io.Cancelable!?T {
            return self.recvInner(io, true);
        }

        pub fn recvUncancelable(self: *Self, io: Io) ?T {
            return self.recvInner(io, false) catch |err| switch (err) {
                error.Canceled => unreachable,
            };
        }

        fn recvInner(self: *Self, io: Io, comptime cancelable: bool) Io.Cancelable!?T {
            var w: Waiter = .{};
            while (true) {
                {
                    self.interior.lockUncancelable(io);
                    defer self.interior.unlock(io);
                    if (self.len > 0) {
                        if (w.linked) self.rx.unlink(&w);
                        const v = self.popLocked();
                        self.tx.wakeFront(io);
                        return v;
                    }
                    if (self.sendersGoneLocked() or self.closed) {
                        if (w.linked) self.rx.unlink(&w);
                        return null;
                    }
                    if (!w.linked) self.rx.push(&w);
                    w.state.store(0, .monotonic);
                }
                parkOnce(io, &w, cancelable) catch |err| switch (err) {
                    error.Canceled => {
                        self.interior.lockUncancelable(io);
                        defer self.interior.unlock(io);
                        if (w.linked) self.rx.unlink(&w);
                        return err;
                    },
                };
            }
        }

        /// Refuse further sends; drained receives still serve. Parked
        /// senders fail.
        pub fn close(self: *Self, io: Io) void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            self.closed = true;
            self.tx.wakeAll(io);
        }

        /// The receiver goes away: every parked and future send fails.
        pub fn closeReceiver(self: *Self, io: Io) void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            self.rx_alive = false;
            self.closed = true;
            self.tx.wakeAll(io);
        }

        pub fn count(self: *Self, io: Io) usize {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            return self.len;
        }

        /// The receiver is parked (the deterministic park observable).
        pub fn receiverParked(self: *Self, io: Io) bool {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            return self.rx.head != null;
        }

        /// Parked capacity waiters (the deterministic park observable).
        pub fn sendWaiters(self: *Self, io: Io) usize {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            var n: usize = 0;
            var cur = self.tx.head;
            while (cur) |w| : (cur = w.next) n += 1;
            return n;
        }

        fn pushLocked(self: *Self, v: T) void {
            assert(self.len < capacity);
            self.buf[(self.head + self.len) % capacity] = v;
            self.len += 1;
        }

        fn popLocked(self: *Self) T {
            assert(self.len > 0);
            const v = self.buf[self.head];
            self.head = (self.head + 1) % capacity;
            self.len -= 1;
            return v;
        }
    };
}

// ===========================================================================
// Watch
// ===========================================================================

pub fn Watch(comptime T: type) type {
    return struct {
        const Self = @This();

        interior: Io.Mutex = .init,
        value: T,
        version: u64 = 0,
        tx_alive: bool = true,
        waiters: Queue = .{},

        pub const ChangedError = error{ Canceled, SenderGone };

        pub fn init(initial: T) Self {
            return .{ .value = initial };
        }

        /// Publish a new value (always succeeds — receivers come and go).
        pub fn send(self: *Self, io: Io, value: T) void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            self.value = value;
            self.version += 1;
            self.waiters.wakeAll(io);
        }

        /// No further change can arrive: parked receivers fail.
        pub fn closeSender(self: *Self, io: Io) void {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            self.tx_alive = false;
            self.waiters.wakeAll(io);
        }

        /// A copy of the current value.
        pub fn load(self: *Self, io: Io) T {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            return self.value;
        }

        /// Parked receivers (the deterministic park observable).
        pub fn parked(self: *Self, io: Io) usize {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            var n: usize = 0;
            var cur = self.waiters.head;
            while (cur) |w| : (cur = w.next) n += 1;
            return n;
        }

        /// A receiver that has seen the current version.
        pub fn subscribe(self: *Self, io: Io) Receiver {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            return .{ .watch = self, .seen = self.version };
        }

        pub const Receiver = struct {
            watch: *Self,
            seen: u64,

            /// The current value, marking its version seen.
            pub fn loadAndUpdate(r: *Receiver, io: Io) T {
                r.watch.interior.lockUncancelable(io);
                defer r.watch.interior.unlock(io);
                r.seen = r.watch.version;
                return r.watch.value;
            }

            /// Park until a version newer than the last seen. Ticked.
            pub fn changed(r: *Receiver, io: Io) ChangedError!void {
                return r.changedInner(io, true);
            }

            pub fn changedUncancelable(r: *Receiver, io: Io) error{SenderGone}!void {
                return r.changedInner(io, false) catch |err| switch (err) {
                    error.Canceled => unreachable,
                    error.SenderGone => |e| return e,
                };
            }

            fn changedInner(r: *Receiver, io: Io, comptime cancelable: bool) ChangedError!void {
                const s = r.watch;
                var w: Waiter = .{};
                while (true) {
                    {
                        s.interior.lockUncancelable(io);
                        defer s.interior.unlock(io);
                        if (s.version > r.seen) {
                            r.seen = s.version;
                            if (w.linked) s.waiters.unlink(&w);
                            return;
                        }
                        if (!s.tx_alive) {
                            if (w.linked) s.waiters.unlink(&w);
                            return error.SenderGone;
                        }
                        if (!w.linked) s.waiters.push(&w);
                        w.state.store(0, .monotonic);
                    }
                    parkOnce(io, &w, cancelable) catch |err| switch (err) {
                        error.Canceled => {
                            s.interior.lockUncancelable(io);
                            defer s.interior.unlock(io);
                            if (w.linked) s.waiters.unlink(&w);
                            return err;
                        },
                    };
                }
            }
        };
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;
const sync_testing = @import("testing.zig");

fn oneshotDelivers(io: Io) !void {
    var ch: Oneshot(u32) = .init;
    const Sender = struct {
        fn run(io_: Io, c: *Oneshot(u32)) void {
            c.send(io_, 7) catch unreachable;
        }
    };
    const t = try std.Thread.spawn(.{}, Sender.run, .{ io, &ch });
    try testing.expectEqual(@as(u32, 7), try ch.recv(io));
    t.join();

    var ch2: Oneshot(u32) = .init;
    ch2.closeSender(io);
    try testing.expectError(error.SenderGone, ch2.recv(io));

    var ch3: Oneshot(u32) = .init;
    ch3.closeReceiver(io);
    try testing.expectError(error.ReceiverGone, ch3.send(io, 9));
    try testing.expect(ch3.isClosed(io));

    var ch4: Oneshot(u32) = .init;
    try testing.expectError(error.Empty, ch4.tryRecv(io));
    try ch4.send(io, 3);
    try testing.expectEqual(@as(u32, 3), try ch4.tryRecv(io));
    try testing.expectError(error.Closed, ch4.tryRecv(io));
}

test "oneshot_delivers_and_close_errors" {
    try sync_testing.underBoth(oneshotDelivers);
}

fn oneshotParkedReceiver(io: Io) !void {
    var ch: Oneshot(u32) = .init;
    const Rx = struct {
        fn run(io_: Io, c: *Oneshot(u32)) !void {
            try testing.expectEqual(@as(u32, 11), try c.recvUncancelable(io_));
        }
    };
    const t = try std.Thread.spawn(.{}, Rx.run, .{ io, &ch });
    while (!ch.receiverParked(io)) try std.Thread.yield();
    try ch.send(io, 11);
    t.join();

    var ch2: Oneshot(u32) = .init;
    const Cancelled = struct {
        fn run(io_: Io, c: *Oneshot(u32)) Oneshot(u32).RecvError!u32 {
            return c.recv(io_);
        }
    };
    var fut = io.concurrent(Cancelled.run, .{ io, &ch2 }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    while (!ch2.receiverParked(io)) try std.Thread.yield();
    try testing.expectError(error.Canceled, fut.cancel(io));
    try testing.expect(!ch2.receiverParked(io));
}

test "oneshot: a parked receiver is woken by the send and by a close; a cancelled receiver unregisters" {
    try sync_testing.underBoth(oneshotParkedReceiver);
}

fn mpscBackpressures(io: Io) !void {
    var ch: Mpsc(u32, 2) = .init;
    const tx = ch.sender(io);
    try tx.trySend(io, 1);
    try tx.trySend(io, 2);
    try testing.expectError(error.Full, tx.trySend(io, 3));
    // A parked async send completes once the receiver drains.
    const tx2 = ch.sender(io);
    const Parked = struct {
        fn run(io_: Io, s: Mpsc(u32, 2).Sender) void {
            s.sendUncancelable(io_, 3) catch unreachable;
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{ io, tx2 });
    while (ch.sendWaiters(io) == 0) try std.Thread.yield();
    try testing.expectEqual(@as(?u32, 1), try ch.recv(io));
    t.join();
    tx2.deinit(io);
    try testing.expectEqual(@as(?u32, 2), try ch.recv(io));
    try testing.expectEqual(@as(?u32, 3), try ch.recv(io));
    try testing.expectError(error.Empty, ch.tryRecv(io));
    tx.deinit(io);
    try testing.expectEqual(@as(?u32, null), try ch.recv(io));
    try testing.expectError(error.Disconnected, ch.tryRecv(io));
}

test "mpsc_bounded_backpressures_and_drains" {
    try sync_testing.underBoth(mpscBackpressures);
}

fn mpscClose(io: Io) !void {
    var ch: Mpsc(u32, 4) = .init;
    const tx = ch.sender(io);
    try tx.trySend(io, 1);
    try tx.trySend(io, 2);
    ch.close(io);
    try testing.expectError(error.Closed, tx.trySend(io, 3));
    try testing.expect(tx.isClosed(io));
    try testing.expectEqual(@as(?u32, 1), ch.recvUncancelable(io));
    try testing.expectEqual(@as(?u32, 2), ch.recvUncancelable(io));
    try testing.expectEqual(@as(?u32, null), ch.recvUncancelable(io));
    tx.deinit(io);
}

test "mpsc close semantics: a closed channel refuses sends and drains" {
    try sync_testing.underBoth(mpscClose);
}

fn mpscReceiverDrop(io: Io) !void {
    var ch: Mpsc(u32, 1) = .init;
    const tx = ch.sender(io);
    try tx.trySend(io, 1);
    const Parked = struct {
        fn run(io_: Io, s: Mpsc(u32, 1).Sender) !void {
            try testing.expectError(error.Closed, s.sendUncancelable(io_, 2));
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{ io, tx });
    while (ch.sendWaiters(io) == 0) try std.Thread.yield();
    ch.closeReceiver(io);
    t.join();
    tx.deinit(io);
}

test "mpsc_receiver_drop_fails_parked_senders" {
    try sync_testing.underBoth(mpscReceiverDrop);
}

fn mpscLastSenderAndCancel(io: Io) !void {
    var ch: Mpsc(u32, 1) = .init;
    const tx = ch.sender(io);
    const Rx = struct {
        fn run(io_: Io, c: *Mpsc(u32, 1)) !void {
            try testing.expectEqual(@as(?u32, null), c.recvUncancelable(io_));
        }
    };
    const t = try std.Thread.spawn(.{}, Rx.run, .{ io, &ch });
    while (!ch.receiverParked(io)) try std.Thread.yield();
    tx.deinit(io);
    t.join();

    var ch2: Mpsc(u32, 1) = .init;
    const tx2 = ch2.sender(io);
    try tx2.trySend(io, 1);
    const Cancelled = struct {
        fn run(io_: Io, s: Mpsc(u32, 1).Sender) Mpsc(u32, 1).SendError!void {
            return s.send(io_, 2);
        }
    };
    var fut = io.concurrent(Cancelled.run, .{ io, tx2 }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    while (ch2.sendWaiters(io) == 0) try std.Thread.yield();
    try testing.expectError(error.Canceled, fut.cancel(io));
    try testing.expectEqual(@as(usize, 0), ch2.sendWaiters(io));
    tx2.deinit(io);
}

test "mpsc: the last sender's departure wakes a parked receiver; a cancelled parked send unregisters" {
    try sync_testing.underBoth(mpscLastSenderAndCancel);
}

/// A receiver scheduled before the first `Sender` is minted (a consumer task
/// popped ahead of its producer) must park on a not-yet-connected channel,
/// not read it as closed-and-drained: that misread ended the consumer at
/// once and left the producer parked on capacity forever.
fn mpscRecvBeforeFirstSender(io: Io) !void {
    var ch: Mpsc(u32, 1) = .init;
    try testing.expectError(error.Empty, ch.tryRecv(io));
    const Rx = struct {
        first: ?u32 = undefined,
        second: ?u32 = undefined,
        fn run(io_: Io, c: *Mpsc(u32, 1), out: *@This()) void {
            out.first = c.recvUncancelable(io_);
            out.second = c.recvUncancelable(io_);
        }
    };
    var rx: Rx = .{};
    const t = try std.Thread.spawn(.{}, Rx.run, .{ io, &ch, &rx });
    while (!ch.receiverParked(io)) try std.Thread.yield();
    const tx = ch.sender(io);
    try tx.trySend(io, 7);
    tx.deinit(io);
    t.join();
    try testing.expectEqual(@as(?u32, 7), rx.first);
    try testing.expectEqual(@as(?u32, null), rx.second);
    try testing.expectError(error.Disconnected, ch.tryRecv(io));
}

test "mpsc: a receiver ahead of the first sender parks on the unconnected channel; the first send wakes it; the last deinit closes it" {
    try sync_testing.underBoth(mpscRecvBeforeFirstSender);
}

fn mpscNoLostSend(io: Io) !void {
    const Chan = Mpsc(u64, 4);
    const Shared = struct {
        ch: Chan = .init,
        sum: u64 = 0,
        const per_producer = 2000;
        fn producer(io_: Io, tx: Chan.Sender, base: u64) void {
            var i: u64 = 0;
            while (i < per_producer) : (i += 1) tx.sendUncancelable(io_, base + i) catch unreachable;
            tx.deinit(io_);
        }
        fn consumer(io_: Io, s: *@This()) void {
            while (s.ch.recvUncancelable(io_)) |v| s.sum += v;
        }
    };
    var s: Shared = .{};
    var txs: [4]Chan.Sender = undefined;
    for (&txs) |*tx| tx.* = s.ch.sender(io);
    const c = try std.Thread.spawn(.{}, Shared.consumer, .{ io, &s });
    var ps: [4]std.Thread = undefined;
    for (&ps, 0..) |*p, i| p.* = try std.Thread.spawn(.{}, Shared.producer, .{ io, txs[i], @as(u64, i) * 1_000_000 });
    for (ps) |p| p.join();
    c.join();
    var expected: u64 = 0;
    for (0..4) |i| {
        var j: u64 = 0;
        while (j < Shared.per_producer) : (j += 1) expected += @as(u64, i) * 1_000_000 + j;
    }
    try testing.expectEqual(expected, s.sum);
}

test "mpsc: every send lands exactly once across producers racing a parking consumer (no lost send)" {
    try sync_testing.underBoth(mpscNoLostSend);
}

fn watchPublishes(io: Io) !void {
    var w = Watch(u32).init(0);
    var rx = w.subscribe(io);
    try testing.expectEqual(@as(u32, 0), w.load(io));
    w.send(io, 5);
    try rx.changed(io);
    try testing.expectEqual(@as(u32, 5), rx.loadAndUpdate(io));
    // No re-fire for a seen version: a concurrent publish unparks the waiter.
    const Publisher = struct {
        fn run(io_: Io, wt: *Watch(u32)) void {
            while (wt.parked(io_) == 0) std.Thread.yield() catch {};
            wt.send(io_, 6);
        }
    };
    const t = try std.Thread.spawn(.{}, Publisher.run, .{ io, &w });
    try rx.changed(io);
    try testing.expectEqual(@as(u32, 6), w.load(io));
    t.join();
    // A late subscriber has seen the current version; the sender's close
    // fails its wait.
    var late = w.subscribe(io);
    w.closeSender(io);
    try testing.expectError(error.SenderGone, late.changedUncancelable(io));
}

test "watch_publishes_and_changed_fires" {
    try sync_testing.underBoth(watchPublishes);
}

/// A fiber producer and a fiber consumer on the two meta lanes exchange
/// through a bounded mpsc: every park is a fiber park, every wake a
/// cross-lane inbox push.
fn mpscAcrossLanes(io: Io) !void {
    const Chan = Mpsc(u64, 2);
    const Shared = struct {
        ch: Chan = .init,
        const count = 500;
        fn producer(io_: Io, s: *@This()) Io.Cancelable!void {
            const tx = s.ch.sender(io_);
            var i: u64 = 0;
            while (i < count) : (i += 1) tx.send(io_, i) catch |err| switch (err) {
                error.Canceled => |e| return e,
                error.Closed => unreachable,
            };
            tx.deinit(io_);
        }
        fn consumer(io_: Io, s: *@This()) Io.Cancelable!u64 {
            var sum: u64 = 0;
            while (try s.ch.recv(io_)) |v| sum += v;
            return sum;
        }
    };
    var s: Shared = .{};
    var p = io.concurrent(Shared.producer, .{ io, &s }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    var c = io.concurrent(Shared.consumer, .{ io, &s }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => {
            try p.await(io);
            return error.SkipZigTest;
        },
    };
    try p.await(io);
    try testing.expectEqual(@as(u64, Shared.count * (Shared.count - 1) / 2), try c.await(io));
}

test "mpsc between two concurrent tasks: a producer and a consumer park and wake each other" {
    try sync_testing.underBoth(mpscAcrossLanes);
}
