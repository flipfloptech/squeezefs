//! THE OWNER'S CENSUS AS A MEMBER SEES IT (`docs/porting/sym_tokens_intents_
//! striping.md` §1.12 "`Expired` — past the deadline OR not listed: any
//! holder the owner does not list has LEFT or been EVICTED"; §2.4 "a scope
//! is kept while its client is a LIVE member, released the sweep after the
//! owner evicts it"): a joiner keeps no roster of its own, so the owner's
//! `Census` pages — pulled by the member's beat once per renew interval —
//! are its word on every OTHER member's lease: the one liveness law the
//! token holder's lease oracle and the parked guard scopes' sweep read on
//! a joiner (the manager reads its roster directly). Found by the shared
//! hot directory row: the holder B of a striped directory died with its
//! guard scopes parked at C; C had no word on B and kept the stripes'
//! locks the whole 45 s belt, which parked every op on those stripes AND
//! the session serving them. PORTABLE: a map under a mutex, fed rows by
//! the beat, asked verdicts by the sweeps.
//!
//! The verdict: a row `live` ⇒ live; `departed` (past its deadline, the
//! eviction tick pending) ⇒ expired; ABSENT ⇒ expired — the owner evicted
//! it — only when the view is NEWER than the asker's last contact with the
//! member (`since_ns`: a scope parked, a grant asked — a member that was
//! in contact then and is unlisted later has left; one unlisted by an
//! OLDER census may simply have joined since) and the page said the
//! owner's grace window is closed (a successor's roster still filling:
//! silence is unknown); a view never committed, or older than its
//! validity, answers unknown for everyone — the grace belt, never a false
//! death.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const sync = @import("../../../sync/sync.zig");
const verbs = @import("../../../wire/verbs.zig");
const token_table = @import("../tokens/token_table.zig");

pub const ClientId = token_table.ClientId;
pub const Verdict = token_table.LeaseVerdict;

/// A committed view answers for this many renew intervals: one missed
/// pull keeps the view, two make it unknown — the owner's own law for a
/// member that misses renewals (`T_self` ≈ 3 renew intervals) applied one
/// interval tighter, so a stale view never outlives the owner's eviction
/// of this member itself.
pub const VALIDITY_RENEWALS: u64 = 2;

pub const CensusView = struct {
    gpa: Allocator,
    io: Io,
    lock: sync.Mutex = .init,
    /// The standing view (what verdicts read).
    states: std.AutoHashMapUnmanaged(ClientId, verbs.MemberState) = .empty,
    /// The pull in progress (the beat's alone until `commit` swaps it in).
    staging: std.AutoHashMapUnmanaged(ClientId, verbs.MemberState) = .empty,
    grace_open: bool = false,
    /// The instant the standing view was committed (0 = never).
    committed_ns: u64 = 0,
    /// How long a committed view answers (0 until the grant names the
    /// renew interval: nothing is pulled before the join lands).
    valid_ns: u64 = 0,

    pub fn init(gpa: Allocator, io: Io) CensusView {
        return .{ .gpa = gpa, .io = io };
    }

    /// The grant's renew interval sizes the validity.
    pub fn setRenewInterval(self: *CensusView, renew_ns: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        self.valid_ns = VALIDITY_RENEWALS * renew_ns;
    }

    pub fn deinit(self: *CensusView) void {
        self.states.deinit(self.gpa);
        self.staging.deinit(self.gpa);
    }

    /// One census row into the pull in progress.
    pub fn stage(self: *CensusView, client: ClientId, state: verbs.MemberState) Allocator.Error!void {
        try self.staging.put(self.gpa, client, state);
    }

    /// The pull completed: the staged rows become the standing view.
    pub fn commit(self: *CensusView, grace_open: bool, now_ns: u64) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        std.mem.swap(std.AutoHashMapUnmanaged(ClientId, verbs.MemberState), &self.states, &self.staging);
        self.staging.clearRetainingCapacity();
        self.grace_open = grace_open;
        self.committed_ns = now_ns;
    }

    /// The pull failed: the staged rows go, the standing view answers
    /// until it ages out.
    pub fn abandon(self: *CensusView) void {
        self.staging.clearRetainingCapacity();
    }

    /// Is a pull due (`renew_ns` since the last commit, or never pulled)?
    pub fn due(self: *CensusView, now_ns: u64, renew_ns: u64) bool {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        return self.committed_ns == 0 or now_ns -| self.committed_ns >= renew_ns;
    }

    /// `since_ns`: the asker's last contact with the member.
    pub fn verdict(self: *CensusView, client: ClientId, since_ns: u64, now_ns: u64) Verdict {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.committed_ns == 0 or now_ns -| self.committed_ns > self.valid_ns) return .unknown;
        if (self.states.get(client)) |s| return switch (s) {
            .live, .grace => .live,
            .departed => .expired,
        };
        return if (!self.grace_open and self.committed_ns > since_ns) .expired else .unknown;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the census view: unknown before a commit; live/grace rows live, a departed row expired; an absent member is expired only to an asker whose contact predates the view (a scope parked before the pull) and while the owner's grace window is closed — unknown to one in contact now, or under the window; a stale view answers unknown; an abandoned pull leaves the standing view; a pull is due once per renew interval" {
    const renew_ns: u64 = 1000;
    var v = CensusView.init(testing.allocator, testing.io);
    defer v.deinit();
    v.setRenewInterval(renew_ns);
    try testing.expectEqual(2 * renew_ns, v.valid_ns);
    try testing.expectEqual(Verdict.unknown, v.verdict(1, 10, 10));
    try testing.expect(v.due(10, renew_ns));
    try v.stage(1, .live);
    try v.stage(2, .departed);
    try v.stage(3, .grace);
    v.commit(false, 100);
    try testing.expect(!v.due(100, renew_ns));
    try testing.expect(v.due(1100, renew_ns));
    try testing.expectEqual(Verdict.live, v.verdict(1, 50, 100));
    try testing.expectEqual(Verdict.expired, v.verdict(2, 50, 100));
    try testing.expectEqual(Verdict.live, v.verdict(3, 50, 100));
    // Member 4 is unlisted: dead to a scope parked at 50 (the view at 100
    // is newer), unknown to a grant asked now (it may have joined since).
    try testing.expectEqual(Verdict.expired, v.verdict(4, 50, 100));
    try testing.expectEqual(Verdict.unknown, v.verdict(4, 100, 100));
    try testing.expectEqual(Verdict.unknown, v.verdict(4, 150, 150));
    // The grace window: silence is unknown, a listed row still speaks.
    try v.stage(1, .live);
    v.commit(true, 200);
    try testing.expectEqual(Verdict.unknown, v.verdict(4, 50, 200));
    try testing.expectEqual(Verdict.live, v.verdict(1, 50, 200));
    // Member 2 left the page (evicted): absent, and the window closed again.
    try v.stage(1, .live);
    v.commit(false, 300);
    try testing.expectEqual(Verdict.expired, v.verdict(2, 50, 300));
    // A failed pull: the standing view keeps answering.
    try v.stage(9, .live);
    v.abandon();
    try testing.expectEqual(Verdict.live, v.verdict(1, 50, 300));
    try testing.expectEqual(Verdict.expired, v.verdict(9, 50, 300));
    // Past the validity the view is unknown for everyone.
    try testing.expectEqual(Verdict.live, v.verdict(1, 50, 300 + v.valid_ns));
    try testing.expectEqual(Verdict.unknown, v.verdict(1, 50, 301 + v.valid_ns));
    try testing.expectEqual(Verdict.unknown, v.verdict(2, 50, 301 + v.valid_ns));
}
