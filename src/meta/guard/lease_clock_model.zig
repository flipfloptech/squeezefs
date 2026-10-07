//! Explorer models of the lease-clock core (`lease_clock.zig`
//! `MemberLeaseWords`; charter §9 "Model-check every lock-free core"; the
//! reference's loom `lease_clock_models`, LOOM:5557-5680).
//!
//! Model 1 — `renewal_label_never_pairs_with_the_old_anchor`: the renewal
//! thread publishes the ANCHOR (deadline) then the LABEL, both Release; a
//! reader on another thread loads the label (Acquire) then the anchor. A
//! reader observing NEW_LABEL must read NEW_ANCHOR; OLD_LABEL may pair with
//! either (the conservative direction). Stores are modelled with a
//! per-thread STORE BUFFER whose entries land in separate steps, so the
//! explorer can land them in either order unless a fence forbids it:
//!   - a Release store lands only after every earlier store of its thread
//!     landed (it drains the buffer first);
//!   - the Acquire load forbids the reader's later load from being served
//!     before it; without it the reader's anchor load may be hoisted (the
//!     model lets the anchor be read FIRST and the label second).
//! Weakenings (each must fail): `flipped` — the two stores in the wrong
//! order; `relaxed` — the label store/load pair without Release/Acquire.
//!
//! Model 2 — `self_fence_side_effects_run_exactly_once`: two racing
//! `fence()` calls yield exactly one `true`. Weakening: `load_then_store`
//! (a load and a store instead of ONE swap) lets both claim first.

const std = @import("std");
const explorer = @import("../../model/explorer.zig");

pub const RenewalWeakening = enum { none, flipped, relaxed };

const OLD: u8 = 0;
const NEW: u8 = 1;

pub fn RenewalModel(comptime weakening: RenewalWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const writer = 0;
        const reader = 1;

        /// The two shared words.
        anchor: u8 = OLD,
        label: u8 = OLD,
        /// The writer's store buffer: a pending store per word.
        pending_anchor: bool = false,
        pending_label: bool = false,
        /// What the reader observed.
        saw_label: u8 = OLD,
        saw_anchor: u8 = OLD,
        /// Whether the reader hoisted the anchor load above the label's.
        hoisted: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 0xff;
        }

        /// Land a pending store; a Release store lands only once the
        /// earlier one did (the drain), unless the weakening relaxed it.
        fn landAnchor(self: *Self) void {
            self.anchor = NEW;
            self.pending_anchor = false;
        }

        fn landLabel(self: *Self) void {
            self.label = NEW;
            self.pending_label = false;
        }

        fn writerStep(self: *Self) void {
            switch (self.pc[writer]) {
                0 => {
                    // First store (buffered): anchor, or the label when flipped.
                    if (weakening == .flipped) self.pending_label = true else self.pending_anchor = true;
                    self.pc[writer] = 1;
                },
                1 => {
                    // Second store (buffered). With Release (none, flipped) it
                    // drains the first store before itself is buffered; the
                    // relaxed weakening buffers it independently.
                    if (weakening == .flipped) {
                        if (self.pending_label) self.landLabel();
                        self.pending_anchor = true;
                    } else {
                        if (weakening != .relaxed and self.pending_anchor) self.landAnchor();
                        self.pending_label = true;
                    }
                    self.pc[writer] = 2;
                },
                2 => {
                    // One buffered store lands (the explorer picks which by
                    // the order of these two steps); step 3 lands the other.
                    if (weakening == .flipped) {
                        if (self.pending_anchor) self.landAnchor() else if (self.pending_label) self.landLabel();
                    } else if (weakening == .relaxed) {
                        // Either may land first: land the label first here,
                        // the anchor at step 3 — and the symmetric order is
                        // reachable because the reader may run between.
                        if (self.pending_label) self.landLabel() else if (self.pending_anchor) self.landAnchor();
                    } else {
                        if (self.pending_anchor) self.landAnchor() else if (self.pending_label) self.landLabel();
                    }
                    self.pc[writer] = 3;
                },
                3 => {
                    if (self.pending_anchor) self.landAnchor();
                    if (self.pending_label) self.landLabel();
                    self.pc[writer] = 0xff;
                },
                else => unreachable,
            }
        }

        fn readerStep(self: *Self) void {
            switch (self.pc[reader]) {
                0 => {
                    // The Acquire load of the label pins the anchor load
                    // after it; relaxed lets the hardware serve the anchor
                    // load first (modelled as the alternative at this step).
                    if (weakening == .relaxed and !self.hoisted and self.anchor == OLD) {
                        // Hoist: read the anchor now (OLD), the label later.
                        self.saw_anchor = self.anchor;
                        self.hoisted = true;
                        self.pc[reader] = 2;
                    } else {
                        self.saw_label = self.label;
                        self.pc[reader] = 1;
                    }
                },
                1 => {
                    self.saw_anchor = self.anchor;
                    self.pc[reader] = 0xff;
                },
                2 => {
                    self.saw_label = self.label;
                    self.pc[reader] = 0xff;
                },
                else => unreachable,
            }
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == writer) self.writerStep() else self.readerStep();
        }

        pub fn invariant(_: *const Self) !void {}

        pub fn final(self: *const Self) !void {
            if (self.saw_label == NEW and self.saw_anchor == OLD) return error.NewLabelOldAnchor;
        }
    };
}

pub const FenceWeakening = enum { none, load_then_store };

pub fn FenceModel(comptime weakening: FenceWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        fenced: bool = false,
        claimed: [2]bool = @splat(false),
        loaded: [2]bool = @splat(false),
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 0xff;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (weakening) {
                .none => {
                    // ONE swap: read-and-set in a single atomic action.
                    self.claimed[t] = !self.fenced;
                    self.fenced = true;
                    self.pc[t] = 0xff;
                },
                .load_then_store => switch (self.pc[t]) {
                    0 => {
                        self.loaded[t] = self.fenced;
                        self.pc[t] = 1;
                    },
                    1 => {
                        self.claimed[t] = !self.loaded[t];
                        self.fenced = true;
                        self.pc[t] = 0xff;
                    },
                    else => unreachable,
                },
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.claimed[0] and self.claimed[1]) return error.FenceRanTwice;
        }

        pub fn final(self: *const Self) !void {
            if (!self.claimed[0] and !self.claimed[1]) return error.FenceNeverRan;
        }
    };
}

const testing = std.testing;

test "renewal: the new label never pairs with the old anchor; flipping the stores or relaxing the Release/Acquire pair admits (NEW_LABEL, OLD_ANCHOR)" {
    try explorer.expectHolds(RenewalModel(.none), .{});
    try explorer.expectFails(RenewalModel(.flipped), .{}, error.FinalViolated);
    try explorer.expectFails(RenewalModel(.relaxed), .{}, error.FinalViolated);
}

test "self-fence: two racing fence() calls yield exactly one true; load-then-store lets both claim first" {
    try explorer.expectHolds(FenceModel(.none), .{});
    try explorer.expectFails(FenceModel(.load_then_store), .{}, error.InvariantViolated);
}
