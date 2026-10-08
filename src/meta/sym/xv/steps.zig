//! The six step kinds, their witnesses, their inverses and the per-op
//! PLANS (charter §5.5 "Cross-owner namespace ops"; `docs/porting/
//! sym_tokens_intents_striping.md` §2.1 the field table, §2.2 the witness
//! table, §2.5 the plans, §2.7 the inverses). PURE: a plan is a list of
//! steps over GLOBAL inos ("so a plan survives a slot migration between
//! crash and recovery — the participant is resolved by routing at apply
//! time"), built from witnesses the caller read under its guards; the
//! applier (`applier.zig`) is the only reader of a step's effect.
//!
//! The witness table (one row per kind — the ONE applier judges by it):
//!
//!   RemoveDentry{parent, name, expect_child}  the dentry names expect_child │ absent = done │ another ino = foreign
//!   InsertDentry{parent, name, child, ft}     the name is free              │ present→child = done │ present→other = foreign
//!   SetNlink{ino, pre, post, ctime?}          nlink == pre (CAS)            │ nlink == post ∧ pre ≠ post = done │ else foreign
//!   TouchCtime{ino, ctime}                    — (a monotone Δ)              │ — │ no record = foreign
//!   MintInode{ino, mode, uid, gid, rdev}      no record                     │ present = done │ —
//!   CreateInode{ino, …, size, ts_ns}          no record                     │ present = done (its record answered) │ —

const std = @import("std");
const verbs = @import("../../../wire/verbs.zig");
const errors = @import("../../../core/errors.zig");
const economy = @import("../../economy.zig");
const record = @import("../../record.zig");
const intent = @import("intent.zig");

const assert = std.debug.assert;
const S = economy.S;

pub const Kind = verbs.StepKind;
pub const ParentUpdate = verbs.ParentUpdate;
pub const StepWire = verbs.XvStepWire;
pub const InodeValue = record.InodeValue;

/// One step, decoded. Inos are GLOBAL. Names borrow the plan's storage.
pub const Step = union(Kind) {
    remove_dentry: struct { parent: u64, name: []const u8, expect_child: u64, parent_update: ParentUpdate },
    insert_dentry: struct { parent: u64, name: []const u8, child: u64, ft_bits: u32, parent_update: ParentUpdate },
    set_nlink: struct { ino: u64, pre: u32, post: u32, ctime: ?u64 },
    touch_ctime: struct { ino: u64, ctime: u64 },
    mint_inode: struct { ino: u64, mode: u32, uid: u32, gid: u32, rdev: u32 },
    create_inode: struct { ino: u64, mode: u32, uid: u32, gid: u32, rdev: u32, size: u64, ts_ns: u64 },

    /// The GLOBAL ino whose slot HOMES the step (the key ino of every
    /// record it stages): the parent of a dentry step, the inode else.
    pub fn homeIno(self: Step) u64 {
        return switch (self) {
            .remove_dentry => |s| s.parent,
            .insert_dentry => |s| s.parent,
            .set_nlink => |s| s.ino,
            .touch_ctime => |s| s.ino,
            .mint_inode => |s| s.ino,
            .create_inode => |s| s.ino,
        };
    }

    /// A fresh mint names nothing until the op's own dentry step lands:
    /// exempt from the late-guard rule (spec §2.9).
    pub fn isFreshMint(self: Step) bool {
        return self == .mint_inode or self == .create_inode;
    }

    /// The step as the record and the wire carry it.
    pub fn toRec(self: Step) intent.StepRec {
        var w: StepWire = .{ .kind = @backingInt(@as(Kind, self)), .parent_update = 0, .name_len = 0, .flags = 0, .ft_bits = 0, .a = 0, .b = 0, .c = 0, .w0 = 0, .w1 = 0, .w2 = 0, .w3 = 0 };
        var name: []const u8 = "";
        switch (self) {
            .remove_dentry => |s| {
                w.a = s.parent;
                w.b = s.expect_child;
                w.parent_update = @backingInt(s.parent_update);
                w.name_len = @intCast(s.name.len);
                name = s.name;
            },
            .insert_dentry => |s| {
                w.a = s.parent;
                w.b = s.child;
                w.ft_bits = s.ft_bits;
                w.parent_update = @backingInt(s.parent_update);
                w.name_len = @intCast(s.name.len);
                name = s.name;
            },
            .set_nlink => |s| {
                w.a = s.ino;
                w.w0 = s.pre;
                w.w1 = s.post;
                if (s.ctime) |c| {
                    w.flags = verbs.STEP_FLAG_HAS_CTIME;
                    w.c = c;
                }
            },
            .touch_ctime => |s| {
                w.a = s.ino;
                w.c = s.ctime;
            },
            .mint_inode => |s| {
                w.a = s.ino;
                w.w0 = s.mode;
                w.w1 = s.uid;
                w.w2 = s.gid;
                w.w3 = s.rdev;
            },
            .create_inode => |s| {
                w.a = s.ino;
                w.b = s.size;
                w.c = s.ts_ns;
                w.w0 = s.mode;
                w.w1 = s.uid;
                w.w2 = s.gid;
                w.w3 = s.rdev;
            },
        }
        assert(w.check() and intent.stepWordsAdmissible(w));
        return .{ .w = w, .name = name };
    }

    /// The inverse: a checked wire word (`check` + `stepWordsAdmissible`
    /// already held by the record/wire decoders).
    pub fn fromRec(r: intent.StepRec) Step {
        const w = r.w;
        return switch (@as(Kind, @fromBackingInt(w.kind))) {
            .remove_dentry => .{ .remove_dentry = .{ .parent = w.a, .name = r.name, .expect_child = w.b, .parent_update = @fromBackingInt(w.parent_update) } },
            .insert_dentry => .{ .insert_dentry = .{ .parent = w.a, .name = r.name, .child = w.b, .ft_bits = w.ft_bits, .parent_update = @fromBackingInt(w.parent_update) } },
            .set_nlink => .{ .set_nlink = .{ .ino = w.a, .pre = w.w0, .post = w.w1, .ctime = if (w.flags & verbs.STEP_FLAG_HAS_CTIME != 0) w.c else null } },
            .touch_ctime => .{ .touch_ctime = .{ .ino = w.a, .ctime = w.c } },
            .mint_inode => .{ .mint_inode = .{ .ino = w.a, .mode = w.w0, .uid = w.w1, .gid = w.w2, .rdev = w.w3 } },
            .create_inode => .{ .create_inode = .{ .ino = w.a, .mode = w.w0, .uid = w.w1, .gid = w.w2, .rdev = w.w3, .size = w.b, .ts_ns = w.c } },
        };
    }

    /// The errno a LIVE witness refusal of this step answers the op
    /// (spec §2.3 step 5): insert/mint → EEXIST, remove → ENOENT,
    /// count/ctime → ESTALE.
    pub fn foreignSkippedError(self: Step) errors.Error {
        return switch (self) {
            .insert_dentry, .mint_inode, .create_inode => error.PathAlreadyExists,
            .remove_dentry => error.FileNotFound,
            .set_nlink, .touch_ctime => error.StaleHandle,
        };
    }
};

/// The outcome of one apply (the wire's `StepOutcome`).
pub const Status = verbs.StepStatus;

pub const Outcome = struct {
    status: Status,
    /// The post-image a count/create step answers (the reply's attrs).
    inode: ?InodeValue = null,
};

// ---------------------------------------------------------------------------
// Plans.
// ---------------------------------------------------------------------------

/// A plan: ≤ `XV_MAX_STEPS` steps in order, step 0 first. Names are
/// borrowed from the caller (an op's request) or copied into `names`.
pub const Plan = struct {
    op: intent.Op,
    steps: [intent.XV_MAX_STEPS]Step = undefined,
    len: usize = 0,

    pub fn slice(self: *const Plan) []const Step {
        return self.steps[0..self.len];
    }

    pub fn push(self: *Plan, s: Step) void {
        assert(self.len < self.steps.len);
        self.steps[self.len] = s;
        self.len += 1;
    }

    /// The record image's steps.
    pub fn recs(self: *const Plan, out: []intent.StepRec) []intent.StepRec {
        for (self.slice(), 0..) |s, i| out[i] = s.toRec();
        return out[0..self.len];
    }

    pub fn fromIntent(it: intent.Intent) Plan {
        var p: Plan = .{ .op = it.op };
        for (it.steps) |r| p.push(Step.fromRec(r));
        return p;
    }
};

/// A directory child's parent update bumps the parent's nlink under its
/// exclusive guard; a regular child's is the shared Δtime merge.
pub fn parentUpdateFor(child_mode: u32) ParentUpdate {
    return if (economy.isDirMode(child_mode)) .exclusive_times_bump else .shared_times;
}

pub const CreateArgs = struct {
    parent: u64,
    name: []const u8,
    /// The child's GLOBAL ino, minted in the CREATOR's rotor.
    child: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    rdev: u32 = 0,
    size: u64 = 0,
    now: u64,
};

/// `create`/`mkdir`/`mknod`/`symlink` under a FOREIGN directory: the
/// child record FIRST (tx0 = the record + the intent in the creator's
/// ring), the shipped insert second.
pub fn planCreate(a: CreateArgs) Plan {
    var p: Plan = .{ .op = .create };
    p.push(.{ .create_inode = .{ .ino = a.child, .mode = a.mode, .uid = a.uid, .gid = a.gid, .rdev = a.rdev, .size = a.size, .ts_ns = a.now } });
    p.push(.{ .insert_dentry = .{ .parent = a.parent, .name = a.name, .child = a.child, .ft_bits = a.mode & S.IFMT, .parent_update = parentUpdateFor(a.mode) } });
    return p;
}

pub const UnlinkArgs = struct {
    parent: u64,
    name: []const u8,
    child: u64,
    /// The child's record as witnessed at its holder; null = no record
    /// (the dangling-name arm: the name goes, no count step).
    child_value: ?InodeValue,
    is_dir: bool,
    now: u64,
};

/// `unlink`/`rmdir`: the name FIRST ("the half the caller is told
/// about"), the count second.
pub fn planUnlink(a: UnlinkArgs) Plan {
    var p: Plan = .{ .op = if (a.is_dir) .rmdir else .unlink };
    const pu: ParentUpdate = if (a.is_dir) .exclusive_times_bump else .shared_times;
    p.push(.{ .remove_dentry = .{ .parent = a.parent, .name = a.name, .expect_child = a.child, .parent_update = pu } });
    if (a.child_value) |cv| {
        const post: u32 = if (a.is_dir) 0 else cv.nlink -| 1;
        p.push(.{ .set_nlink = .{ .ino = a.child, .pre = cv.nlink, .post = post, .ctime = a.now } });
    }
    return p;
}

pub const LinkArgs = struct {
    parent: u64,
    name: []const u8,
    ino: u64,
    child_value: InodeValue,
    now: u64,
};

pub const LinkError = error{TooManyLinks};

/// `link`: the count FIRST ("the half that can refuse"), the insert
/// second; the ceiling refuses before any step.
pub fn planLink(a: LinkArgs) LinkError!Plan {
    if (a.child_value.nlink >= economy.NLINK_MAX) return error.TooManyLinks;
    var p: Plan = .{ .op = .link };
    p.push(.{ .set_nlink = .{ .ino = a.ino, .pre = a.child_value.nlink, .post = a.child_value.nlink + 1, .ctime = a.now } });
    p.push(.{ .insert_dentry = .{ .parent = a.parent, .name = a.name, .child = a.ino, .ft_bits = a.child_value.mode & S.IFMT, .parent_update = .exclusive_times } });
    return p;
}

pub const RenameArgs = struct {
    old_parent: u64,
    old_name: []const u8,
    new_parent: u64,
    new_name: []const u8,
    child: u64,
    child_mode: u32,
    /// The destination as witnessed at its holder (null = no destination).
    dest: ?struct { ino: u64, value: InodeValue },
    /// `RENAME_WHITEOUT`: the char-0:0 minted at the old name, in the old
    /// parent's mint slot.
    whiteout: ?u64 = null,
    now: u64,
};

/// `rename` across holders, file or directory: the destination's count +
/// remove (iff one exists; ENOTEMPTY is the caller's probe BEFORE any
/// step), the source remove, the insert, the moved inode's Δctime, the
/// whiteout. A directory move shifts both parents' nlink (`..` now names
/// the new parent).
pub fn planRename(a: RenameArgs) Plan {
    var p: Plan = .{ .op = .rename };
    const is_dir = economy.isDirMode(a.child_mode);
    const cross_dir = a.old_parent != a.new_parent;
    const pu_old: ParentUpdate = if (is_dir and cross_dir) .exclusive_times_bump else .shared_times;
    const pu_new: ParentUpdate = if (is_dir and cross_dir) .exclusive_times_bump else .shared_times;
    if (a.dest) |d| {
        const post: u32 = if (economy.isDirMode(d.value.mode)) 0 else d.value.nlink -| 1;
        p.push(.{ .set_nlink = .{ .ino = d.ino, .pre = d.value.nlink, .post = post, .ctime = a.now } });
        // The destination's name is removed with NO parent update: the
        // insert below rewrites the same parent in the same op.
        p.push(.{ .remove_dentry = .{ .parent = a.new_parent, .name = a.new_name, .expect_child = d.ino, .parent_update = if (economy.isDirMode(d.value.mode)) .exclusive_times_bump else .none } });
    }
    p.push(.{ .remove_dentry = .{ .parent = a.old_parent, .name = a.old_name, .expect_child = a.child, .parent_update = pu_old } });
    p.push(.{ .insert_dentry = .{ .parent = a.new_parent, .name = a.new_name, .child = a.child, .ft_bits = a.child_mode & S.IFMT, .parent_update = pu_new } });
    p.push(.{ .touch_ctime = .{ .ino = a.child, .ctime = a.now } });
    if (a.whiteout) |w| {
        p.push(.{ .mint_inode = .{ .ino = w, .mode = S.IFCHR, .uid = 0, .gid = 0, .rdev = 0 } });
        p.push(.{ .insert_dentry = .{ .parent = a.old_parent, .name = a.old_name, .child = w, .ft_bits = S.IFCHR, .parent_update = .none } });
    }
    return p;
}

pub const ExchangeArgs = struct {
    old_parent: u64,
    old_name: []const u8,
    new_parent: u64,
    new_name: []const u8,
    a: u64,
    a_mode: u32,
    b: u64,
    b_mode: u32,
    now: u64,
};

/// `RENAME_EXCHANGE`: the two removes, the two inserts, both Δctimes.
/// Both parents' times ride the first insert at each; a directory ↔
/// non-directory swap across parents shifts the parents' nlink.
pub fn planExchange(a: ExchangeArgs) Plan {
    var p: Plan = .{ .op = .exchange };
    const cross_dir = a.old_parent != a.new_parent;
    const a_dir = economy.isDirMode(a.a_mode);
    const b_dir = economy.isDirMode(a.b_mode);
    // Across parents a directory ↔ non-directory swap shifts the parents'
    // nlink: the remove that takes a directory away drops (the applier's
    // exclusive RMW reads the direction off the kind), the insert that
    // brings one bumps; every other side is the shared Δtime.
    const shift = cross_dir and a_dir != b_dir;
    const rm_a: ParentUpdate = if (shift and a_dir) .exclusive_times_bump else .none;
    const rm_b: ParentUpdate = if (shift and b_dir) .exclusive_times_bump else .none;
    const ins_a: ParentUpdate = if (shift and a_dir) .exclusive_times_bump else .shared_times;
    const ins_b: ParentUpdate = if (shift and b_dir) .exclusive_times_bump else .shared_times;
    p.push(.{ .remove_dentry = .{ .parent = a.old_parent, .name = a.old_name, .expect_child = a.a, .parent_update = rm_a } });
    p.push(.{ .remove_dentry = .{ .parent = a.new_parent, .name = a.new_name, .expect_child = a.b, .parent_update = rm_b } });
    p.push(.{ .insert_dentry = .{ .parent = a.new_parent, .name = a.new_name, .child = a.a, .ft_bits = a.a_mode & S.IFMT, .parent_update = ins_a } });
    p.push(.{ .insert_dentry = .{ .parent = a.old_parent, .name = a.old_name, .child = a.b, .ft_bits = a.b_mode & S.IFMT, .parent_update = ins_b } });
    p.push(.{ .touch_ctime = .{ .ino = a.a, .ctime = a.now } });
    p.push(.{ .touch_ctime = .{ .ino = a.b, .ctime = a.now } });
    return p;
}

/// The inverse of an APPLIED step (spec §2.7): run in reverse under the
/// op's still-held guards through the same applier. The parent-update
/// code travels unchanged — the applier reads the nlink direction off the
/// kind, so a bump on the insert is a drop on its inverse remove.
/// `child_ft` is the removed child's type READ AT ITS HOLDER (a
/// foreign-minted child off the projection would answer no record and no
/// inverse). A minted child has no inverse step: it is DESTROYED
/// (`applier.destroyUnnamed` — a fresh mint has no xattr, no layout, no
/// block); a `TouchCtime` is monotone and stands.
pub fn inverse(s: Step, child_ft: ?u32) ?Step {
    return switch (s) {
        .set_nlink => |x| .{ .set_nlink = .{ .ino = x.ino, .pre = x.post, .post = x.pre, .ctime = null } },
        .remove_dentry => |x| .{ .insert_dentry = .{ .parent = x.parent, .name = x.name, .child = x.expect_child, .ft_bits = child_ft orelse S.IFREG, .parent_update = x.parent_update } },
        .insert_dentry => |x| .{ .remove_dentry = .{ .parent = x.parent, .name = x.name, .expect_child = x.child, .parent_update = x.parent_update } },
        .mint_inode, .create_inode, .touch_ctime => null,
    };
}

// ---------------------------------------------------------------------------
// Tests: the plans restate §2.5's step lists; every kind round-trips.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "every step kind round-trips through the record word with its unused words zero; the errno of a live refusal per kind" {
    const steps = [_]Step{
        .{ .remove_dentry = .{ .parent = 10, .name = "a", .expect_child = 11, .parent_update = .shared_times } },
        .{ .insert_dentry = .{ .parent = 10, .name = "b", .child = 12, .ft_bits = S.IFDIR, .parent_update = .exclusive_times_bump } },
        .{ .set_nlink = .{ .ino = 11, .pre = 2, .post = 1, .ctime = 99 } },
        .{ .set_nlink = .{ .ino = 11, .pre = 1, .post = 2, .ctime = null } },
        .{ .touch_ctime = .{ .ino = 12, .ctime = 5 } },
        .{ .mint_inode = .{ .ino = 13, .mode = S.IFCHR, .uid = 0, .gid = 0, .rdev = 0 } },
        .{ .create_inode = .{ .ino = 14, .mode = S.IFREG | 0o644, .uid = 1, .gid = 2, .rdev = 0, .size = 7, .ts_ns = 8 } },
    };
    for (steps) |s| {
        const r = s.toRec();
        try testing.expect(r.w.check() and intent.stepWordsAdmissible(r.w));
        const back = Step.fromRec(r);
        try testing.expectEqual(@as(Kind, s), @as(Kind, back));
        try testing.expectEqual(s.homeIno(), back.homeIno());
        switch (s) {
            .set_nlink => |x| try testing.expectEqual(x.ctime, back.set_nlink.ctime),
            .insert_dentry => |x| try testing.expectEqualStrings(x.name, back.insert_dentry.name),
            else => {},
        }
    }
    try testing.expectEqual(errors.Error.PathAlreadyExists, steps[1].foreignSkippedError());
    try testing.expectEqual(errors.Error.FileNotFound, steps[0].foreignSkippedError());
    try testing.expectEqual(errors.Error.StaleHandle, steps[2].foreignSkippedError());
    try testing.expect(steps[5].isFreshMint() and steps[6].isFreshMint() and !steps[0].isFreshMint());
}

test "the per-op plans restate §2.5: create = [CreateInode, InsertDentry]; unlink = [RemoveDentry, SetNlink] (no count step for a dangling name); link = [SetNlink, InsertDentry] with EMLINK before any step; rename = [dest count, dest remove]? + [remove, insert, touch] + whiteout; exchange = 2 removes + 2 inserts + 2 touches" {
    const c = planCreate(.{ .parent = 1, .name = "f", .child = 1000, .mode = S.IFDIR | 0o755, .uid = 1, .gid = 2, .now = 7 });
    try testing.expectEqual(@as(usize, 2), c.len);
    try testing.expectEqual(Kind.create_inode, @as(Kind, c.steps[0]));
    try testing.expectEqual(ParentUpdate.exclusive_times_bump, c.steps[1].insert_dentry.parent_update);
    try testing.expectEqual(ParentUpdate.shared_times, planCreate(.{ .parent = 1, .name = "f", .child = 1000, .mode = S.IFREG, .uid = 1, .gid = 2, .now = 7 }).steps[1].insert_dentry.parent_update);
    const u = planUnlink(.{ .parent = 1, .name = "f", .child = 1000, .child_value = .{ .nlink = 2 }, .is_dir = false, .now = 7 });
    try testing.expectEqual(@as(usize, 2), u.len);
    try testing.expectEqual(@as(u32, 1), u.steps[1].set_nlink.post);
    try testing.expectEqual(@as(u32, 0), planUnlink(.{ .parent = 1, .name = "d", .child = 1000, .child_value = .{ .nlink = 2 }, .is_dir = true, .now = 7 }).steps[1].set_nlink.post);
    try testing.expectEqual(@as(usize, 1), planUnlink(.{ .parent = 1, .name = "f", .child = 1000, .child_value = null, .is_dir = false, .now = 7 }).len);
    const l = try planLink(.{ .parent = 1, .name = "g", .ino = 1000, .child_value = .{ .mode = S.IFREG, .nlink = 1 }, .now = 7 });
    try testing.expectEqual(Kind.set_nlink, @as(Kind, l.steps[0]));
    try testing.expectEqual(@as(u32, 2), l.steps[0].set_nlink.post);
    try testing.expectError(error.TooManyLinks, planLink(.{ .parent = 1, .name = "g", .ino = 1000, .child_value = .{ .nlink = economy.NLINK_MAX }, .now = 7 }));
    const r = planRename(.{ .old_parent = 1, .old_name = "a", .new_parent = 2, .new_name = "b", .child = 1000, .child_mode = S.IFREG, .dest = .{ .ino = 1001, .value = .{ .mode = S.IFREG, .nlink = 1 } }, .whiteout = 1002, .now = 7 });
    try testing.expectEqual(@as(usize, 7), r.len);
    try testing.expectEqual(Kind.set_nlink, @as(Kind, r.steps[0]));
    try testing.expectEqual(@as(u32, 0), r.steps[0].set_nlink.post);
    try testing.expectEqual(Kind.remove_dentry, @as(Kind, r.steps[1]));
    try testing.expectEqual(Kind.remove_dentry, @as(Kind, r.steps[2]));
    try testing.expectEqual(Kind.insert_dentry, @as(Kind, r.steps[3]));
    try testing.expectEqual(Kind.touch_ctime, @as(Kind, r.steps[4]));
    try testing.expectEqual(Kind.mint_inode, @as(Kind, r.steps[5]));
    try testing.expectEqual(@as(u32, S.IFCHR), r.steps[6].insert_dentry.ft_bits);
    const rd = planRename(.{ .old_parent = 1, .old_name = "a", .new_parent = 2, .new_name = "b", .child = 1000, .child_mode = S.IFDIR, .dest = null, .now = 7 });
    try testing.expectEqual(@as(usize, 3), rd.len);
    try testing.expectEqual(ParentUpdate.exclusive_times_bump, rd.steps[0].remove_dentry.parent_update);
    const x = planExchange(.{ .old_parent = 1, .old_name = "a", .new_parent = 2, .new_name = "b", .a = 1000, .a_mode = S.IFDIR, .b = 1001, .b_mode = S.IFREG, .now = 7 });
    try testing.expectEqual(@as(usize, 6), x.len);
    try testing.expectEqual(ParentUpdate.exclusive_times_bump, x.steps[0].remove_dentry.parent_update);
    try testing.expectEqual(ParentUpdate.exclusive_times_bump, x.steps[2].insert_dentry.parent_update);
    try testing.expectEqual(ParentUpdate.none, x.steps[1].remove_dentry.parent_update);
    // The inverses.
    try testing.expectEqual(@as(u32, 2), inverse(u.steps[1], null).?.set_nlink.post);
    try testing.expectEqual(Kind.insert_dentry, @as(Kind, inverse(u.steps[0], S.IFDIR).?));
    try testing.expectEqual(@as(u32, S.IFDIR), inverse(u.steps[0], S.IFDIR).?.insert_dentry.ft_bits);
    try testing.expectEqual(Kind.remove_dentry, @as(Kind, inverse(c.steps[1], null).?));
    try testing.expect(inverse(c.steps[0], null) == null and inverse(r.steps[4], null) == null);
    // A plan round-trips through the record image.
    var recs: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
    var buf: [4096]u8 = undefined;
    const img = try intent.encode(r.op, 0, 77, r.recs(&recs), 16 << 10, &buf);
    var out: [intent.XV_MAX_STEPS]intent.StepRec = undefined;
    const back = Plan.fromIntent(try intent.decode(img, &out));
    try testing.expectEqual(r.len, back.len);
    try testing.expectEqualStrings("b", back.steps[3].insert_dentry.name);
}
