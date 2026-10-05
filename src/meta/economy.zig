//! Journal economy — the routed namespace arms' staging shapes (charter §5.2
//! "Journal economy"; survey `docs/porting/kv_tree_cache_commit_survey.md`
//! §2.16, §4.1 "Pending-times batch / cap", §7 row 16 + §7.1; reference
//! `backend.rs` `routed_{create,unlink,link,rename}_local`,
//! `stage_parent_update`, `setattr_locked`, `pending_times`, the times
//! drain). PORTABLE: pure staging over a `tx.KvTx` plus one state machine;
//! nothing here reads a tree, takes a 4a guard or touches a clock — the
//! caller hands in what it read under its guards and `now`.
//!
//! Three measured journal-entry terms, each a law here:
//!
//! - **rename = ONE whole-tx entry**: dentry surgery + both parents' Δtimes
//!   + the moved inode's Δctime (+ dest accounting, the whiteout) in one
//!   `KvTx` — records grew, entries did not (1.003/op measured; gate 1.02).
//!   One entry is one crash exposure: naming and times commit or revert
//!   together, and strict mode pays ONE barrier where two fragments paid two.
//! - **the SETATTR echo is ABSORBED, never committed**: under writeback
//!   cache the kernel authors ctime locally after every rename/unlink/link/
//!   setxattr (those replies carry no attrs) and synchronously flushes a
//!   times-only `FUSE_SETATTR(MTIME|CTIME)` whose mtime is the daemon's own
//!   round-tripped value. The wire message is protocol-mandated — it cannot
//!   be suppressed daemon-side — and it measured a whole 1.000 entries/op on
//!   rename AND unlink storms. `PendingTimes.absorb` recognizes it BY SHAPE
//!   (times-only, mtime absent-or-equal to the folded stored value, ctime
//!   present) and parks the refinement; everything else commits exactly as
//!   before. Never blanket suppression: a changed mtime (the buffered-write
//!   flush carrying kernel-authored write times), an explicit utimes, chmod/
//!   chown/truncate all stage.
//! - **unlink destroys ride batched `destroy_inodes`** (`inos.stageDestroy`):
//!   the unlink tx is one entry (dentry delete + parent update + child
//!   nlink/ctime); the corpse's destroy is the reclaim gather's ONE entry per
//!   batch — `1 + 1/fill + drain ≤ 1.05` (`inos.unlinkEntriesPerOp`).
//!
//! **Parent-time updates are Δtime MERGE records under the SHARED parent
//! guard** (`SharedTimes`): a `Delta(MTIME|CTIME)` never reads the parent,
//! so two children's concurrent creates/unlinks stage two Δs that fold
//! last-writer-wins on the time fields and never clobber nlink/mode/uid; a
//! parent `Put` is an RMW and needs the EXCLUSIVE class. The guard class a
//! staged set requires is therefore DATA derived from the records
//! (`GuardPlan`), in `dlm`'s own request types, so the caller feeds it to
//! `dlm.Manager.lockMany` unchanged.

const std = @import("std");
const Allocator = std.mem.Allocator;
const linux = std.os.linux;
const counters = @import("../core/counters.zig");
const log = @import("../core/log.zig");
const key = @import("key.zig");
const record = @import("record.zig");
const tx = @import("tx.zig");
const dlm = @import("dlm.zig");
const jc = @import("journal_core.zig");

const assert = std.debug.assert;
const InodeValue = record.InodeValue;
const InodeDelta = record.InodeDelta;

pub const stats = struct {
    /// `meta_kv_times_echo_absorbed`: kernel SETATTR echoes absorbed with
    /// zero journal entries (the G4 gate's mechanism counter: it must
    /// track the storm's echo count 1:1).
    pub var meta_kv_times_echo_absorbed: counters.ShardedCounter = .{};
    /// `meta_kv_times_echo_drain_commits`: drain transactions committed —
    /// ONE journal entry each, the honest residual the absorber pays.
    pub var meta_kv_times_echo_drain_commits: counters.ShardedCounter = .{};
    /// `meta_kv_times_echo_drained`: refinements made durable by drains
    /// (records, not entries; pairs with `_drain_commits` for the
    /// amortization ratio).
    pub var meta_kv_times_echo_drained: counters.ShardedCounter = .{};
    /// `meta_kv_times_echo_pending`: refinements parked and not yet drained,
    /// Σ over volumes.
    pub var meta_kv_times_echo_pending: counters.Gauge = .{};
    /// `dir_nlink_underflows`: a parent-side `nlink − 1` that the `> 2`
    /// floor SUPPRESSED (POSIX-11). The count is then permanently one too
    /// low for the subdirectories the parent still holds and `find`'s leaf
    /// optimization (`nlink == 2` ⇒ no subdirs) skips them — a bug signal,
    /// never routine; the floor stays because an underflow is worse.
    pub var dir_nlink_underflows: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// Mode bits and the format's fixed numbers.
// ---------------------------------------------------------------------------

/// The on-disk mode word is Linux's (the format is Linux-only), so the
/// constants are `std.os.linux`'s on every host.
pub const S = linux.S;

/// Dentry-value `file_type` byte from mode bits: `dt` such that
/// `dt << 12 == mode & S_IFMT`.
pub fn ftByte(mode: u32) u8 {
    return @intCast((mode & S.IFMT) >> 12);
}

pub fn isDirMode(mode: u32) bool {
    return mode & S.IFMT == S.IFDIR;
}

/// The hard-link ceiling the reference refuses at (`EMLINK`): ext4's
/// `EXT4_LINK_MAX`, the ceiling every Linux tool already expects.
pub const NLINK_MAX: u32 = 65_000;

/// `PATH_MAX` (4096) COUNTS THE NUL, so the longest symlink target that
/// round-trips through any `PATH_MAX` buffer is 4095 bytes; a longer one is
/// refused at creation (`ENAMETOOLONG`) like ext4/xfs instead of storing a
/// target `readlink` can only truncate.
pub const SYMLINK_TARGET_MAX: usize = 4095;

/// The xattr that carries every symlink's target (the VFS keeps symlink
/// bodies out of the data plane).
pub const SYMLINK_XATTR: []const u8 = "system.symlink";

/// The u8 `name_len` dentry envelope: one encoded dentry value fits here.
const DENTRY_VALUE_MAX: usize = @sizeOf(record.DentryValueHeader) + record.NAME_MAX;

/// Times are i64 ns carried in the u64 word (pre-epoch values are
/// representable — fstests generic/258); every newest-wins compare is
/// SIGNED, or a backdated pre-epoch stamp would out-rank every later echo
/// forever and an unsigned drain would read a pre-epoch stored stamp as
/// huge and drop the refinement the fold served.
pub fn laterThan(a: u64, b: u64) bool {
    return @as(i64, @bitCast(a)) > @as(i64, @bitCast(b));
}

// ---------------------------------------------------------------------------
// Guard class requirements — data, in the lock manager's request types.
// ---------------------------------------------------------------------------

/// The 4a guards a staged set REQUIRES, per class: `I{ino}` with its mode
/// and `D{parent,name}` (always exclusive — a name is a single writer's).
/// Repeated inos keep the strongest mode (a same-directory rename's two
/// parents are one ino; a self-link's parent and child may coincide), the
/// same rule `dlm.Manager.lockMany` applies to a shared+exclusive stripe.
pub const GuardPlan = struct {
    inode_reqs: [dlm.max_requests]dlm.InodeReq = undefined,
    inode_len: usize = 0,
    dentry_reqs: [dlm.max_requests]dlm.DentryReq = undefined,
    dentry_len: usize = 0,

    pub fn inodes(self: *const GuardPlan) []const dlm.InodeReq {
        return self.inode_reqs[0..self.inode_len];
    }

    pub fn dentries(self: *const GuardPlan) []const dlm.DentryReq {
        return self.dentry_reqs[0..self.dentry_len];
    }

    fn inode(self: *GuardPlan, ino: u64, mode: dlm.Mode) void {
        for (self.inode_reqs[0..self.inode_len]) |*r| {
            if (r.ino == ino) {
                if (mode == .exclusive) r.mode = .exclusive;
                return;
            }
        }
        assert(self.inode_len < self.inode_reqs.len);
        self.inode_reqs[self.inode_len] = .{ .ino = ino, .mode = mode };
        self.inode_len += 1;
    }

    fn dentry(self: *GuardPlan, parent: u64, name: []const u8) void {
        for (self.dentry_reqs[0..self.dentry_len]) |r| {
            if (r.parent == parent and std.mem.eql(u8, r.name, name)) return;
        }
        assert(self.dentry_len < self.dentry_reqs.len);
        self.dentry_reqs[self.dentry_len] = .{ .parent = parent, .name = name, .mode = .exclusive };
        self.dentry_len += 1;
    }

    /// The mode planned for `ino`; null when the plan never names it.
    pub fn inodeMode(self: *const GuardPlan, ino: u64) ?dlm.Mode {
        for (self.inodes()) |r| if (r.ino == ino) return r.mode;
        return null;
    }

    pub fn hasDentry(self: *const GuardPlan, parent: u64, name: []const u8) bool {
        for (self.dentries()) |r| if (r.parent == parent and std.mem.eql(u8, r.name, name)) return true;
        return false;
    }
};

// ---------------------------------------------------------------------------
// The parent update: SharedTimes vs the exclusive RMW.
// ---------------------------------------------------------------------------

/// The Δtime merge record on a parent directory — `Delta(MTIME|CTIME,
/// mtime = ctime = now)`, designed for the SHARED parent guard: it carries
/// no value fields, so concurrent same-directory children's records fold
/// last-writer-wins on the times alone and never clobber nlink/mode/uid.
/// The guard class the record requires is `.shared` — the law the plan
/// derives from the record kind (`parentGuardFor`).
pub const SharedTimes = struct {
    parent: u64,
    now: u64,

    pub fn delta(self: SharedTimes) InodeDelta {
        return InodeDelta.times(self.now, self.now);
    }

    pub fn stage(self: SharedTimes, t: *tx.KvTx) Allocator.Error!void {
        try t.stageDelta(key.TREE_INODES, &key.inodeKey(self.parent), self.delta());
    }

    pub fn guard(self: SharedTimes) dlm.InodeReq {
        return .{ .ino = self.parent, .mode = .shared };
    }
};

/// What a parent update staged.
pub const ParentRecord = enum {
    /// `SharedTimes`: the Δtime merge record.
    delta_times,
    /// The full RMW `Put` (times, and the nlink shift when a directory
    /// child comes or goes).
    put,
    /// Nothing: the exclusive arm found no parent record (best-effort, the
    /// v2 arms' shape — a missing parent never fails the child's op).
    none,
};

/// The guard class a parent record REQUIRES: a Δ merges under `.shared`;
/// a `Put` is an RMW and needs `.exclusive`; a planned RMW that staged
/// nothing still planned the exclusive class.
pub fn parentGuardFor(rec: ParentRecord) dlm.Mode {
    return switch (rec) {
        .delta_times => .shared,
        .put, .none => .exclusive,
    };
}

/// Stage the parent-directory update: under a SHARED parent the Δtime
/// merge record; under an EXCLUSIVE parent a full `Put` carrying
/// `nlink_delta` too (`+1` a directory child inserted, `−1` removed). An
/// nlink mutation is never shared (asserted): it is the RMW the class
/// exists for.
pub fn stageParentUpdate(t: *tx.KvTx, parent: u64, shared: bool, nlink_delta: i32, now: u64, parent_value: ?InodeValue) Allocator.Error!ParentRecord {
    if (shared) {
        assert(nlink_delta == 0);
        try (SharedTimes{ .parent = parent, .now = now }).stage(t);
        return .delta_times;
    }
    var pv = parent_value orelse return .none;
    pv.mtime = now;
    pv.ctime = now;
    if (nlink_delta > 0) {
        pv.nlink += @intCast(nlink_delta);
    } else if (nlink_delta < 0) {
        const dec: u32 = @intCast(-nlink_delta);
        // A directory's nlink never drops below 2 through the parent-side
        // decrement (`.` and its own entry); a suppressed decrement is the
        // counted POSIX-11 defect, not a silent floor.
        if (pv.nlink > 2) {
            pv.nlink -= dec;
        } else {
            stats.dir_nlink_underflows.inc();
            log.warn("directory nlink underflow guard fired on parent {d}: nlink {d} cannot absorb a -{d} parent-side decrement — the deficit is now permanent (POSIX-11; run fsck)", .{ parent, pv.nlink, dec });
        }
    }
    try t.stagePut(key.TREE_INODES, &key.inodeKey(parent), &pv.encode());
    return .put;
}

// ---------------------------------------------------------------------------
// Dentry helpers.
// ---------------------------------------------------------------------------

/// A live dentry as the caller found it under its guards.
pub const DentryPos = struct {
    key: [key.DENTRY_KEY_LEN]u8,
    /// The GLOBAL child ino the value stores.
    child_ino: u64,
    file_type: u8,
};

pub const DentryError = tx.DentryInsertError || Allocator.Error;

/// Stage the dentry `Put` for `(parent, name) → child` at the chain's
/// first free `coll_seq` through the tx overlay (a slot this tx just
/// deleted is free again — the rename EXCHANGE and the whiteout re-insert
/// into the slot they vacated). Single-copy: the value encodes straight
/// into a stack frame the tx dupes once.
fn stageDentryPut(t: *tx.KvTx, hash_seed: u64, parent: u64, name: []const u8, chain: []const []const u8, child_global: u64, file_type: u8) DentryError![key.DENTRY_KEY_LEN]u8 {
    const dk = try tx.dentryInsertKey(t, hash_seed, parent, name, chain);
    var buf: [DENTRY_VALUE_MAX]u8 = undefined;
    const dv = (record.DentryValue{ .child_ino = child_global, .file_type = file_type, .name = name }).encodeInto(&buf) catch unreachable;
    try t.stagePut(key.TREE_DENTRIES, &dk, dv);
    return dk;
}

// ---------------------------------------------------------------------------
// create / mkdir / symlink / link.
// ---------------------------------------------------------------------------

pub const Create = struct {
    hash_seed: u64,
    /// The coarse kernel-domain stamp (a fine clock could LEAD the kernel's
    /// own wb-cache cmtime by a tick and invert cross-inode ctime order —
    /// fstests generic/423).
    now: u64,
    parent: u64,
    /// Read under the guard: setgid inheritance reads it, and the directory
    /// arm's nlink bump rewrites it.
    parent_value: InodeValue,
    name: []const u8,
    mode: u32,
    uid: u32,
    gid: u32,
    rdev: u32 = 0,
    /// The size AT CREATE, durable in the same entry (0 for every ordinary
    /// create; a symlink's is its target length — a cache-only patch
    /// reported `st_size == 0` on the first post-TTL `lstat`).
    initial_size: u64 = 0,
    /// The LOCAL key ino (the volume's watermark or a guest cursor mint).
    local_ino: u64,
    /// Its GLOBAL encoding, stored in the dentry.
    global_ino: u64,
    /// The live scan of the name's chain (`tx.dentryChainRange`).
    chain: []const []const u8 = &.{},
};

pub const CreateOutcome = struct {
    guards: GuardPlan,
    /// The record as staged — the reply's attrs.
    child: InodeValue,
};

/// Routed same-volume create — ONE whole-tx entry: inode `Put`, dentry
/// `Put`, and the parent update (a regular child: `SharedTimes`; a
/// directory: the exclusive `Put` with `nlink + 1`). EEXIST is the caller's
/// check (it holds the D-guard and found no dentry). Setgid directories
/// hand their gid down (and the bit itself to a directory child).
pub fn stageCreate(t: *tx.KvTx, p: Create) DentryError!CreateOutcome {
    var mode = p.mode;
    var gid = p.gid;
    if (p.parent_value.mode & S.ISGID != 0) {
        gid = p.parent_value.gid;
        if (isDirMode(p.mode)) mode |= S.ISGID;
    }
    const is_dir = isDirMode(mode);
    const child: InodeValue = .{
        .mode = mode,
        .uid = p.uid,
        .gid = gid,
        .nlink = if (is_dir) 2 else 1,
        .flags = 0,
        .rdev = p.rdev,
        .size = p.initial_size,
        .atime = p.now,
        .mtime = p.now,
        .ctime = p.now,
    };
    try t.stagePut(key.TREE_INODES, &key.inodeKey(p.local_ino), &child.encode());
    _ = try stageDentryPut(t, p.hash_seed, p.parent, p.name, p.chain, p.global_ino, ftByte(mode));
    const rec = try stageParentUpdate(t, p.parent, !is_dir, if (is_dir) 1 else 0, p.now, p.parent_value);
    var plan: GuardPlan = .{};
    plan.inode(p.parent, parentGuardFor(rec));
    plan.dentry(p.parent, p.name);
    return .{ .guards = plan, .child = child };
}

/// `stageCreate` with the directory type forced: `nlink = 2`, the parent's
/// nlink bump under its exclusive guard.
pub fn stageMkdir(t: *tx.KvTx, p: Create) DentryError!CreateOutcome {
    var q = p;
    q.mode = (p.mode & ~@as(u32, S.IFMT)) | S.IFDIR;
    return stageCreate(t, q);
}

pub const Symlink = struct {
    hash_seed: u64,
    now: u64,
    parent: u64,
    parent_value: InodeValue,
    name: []const u8,
    uid: u32,
    gid: u32,
    local_ino: u64,
    global_ino: u64,
    target: []const u8,
    chain: []const []const u8 = &.{},
};

pub const SymlinkError = DentryError || error{SymlinkTargetTooLong};

/// A symlink is a create of `S_IFLNK | 0777` sized `target.len` plus the
/// `system.symlink` xattr `Put` carrying the target — in the SAME entry, so
/// a symlink never exists without its target (the reference commits the
/// xattr as a second `setxattr` tx; the record set is identical, the entry
/// count is one). A fresh inode has no xattr chain: `coll_seq` 0.
pub fn stageSymlink(t: *tx.KvTx, p: Symlink) SymlinkError!CreateOutcome {
    if (p.target.len > SYMLINK_TARGET_MAX) return error.SymlinkTargetTooLong;
    const out = try stageCreate(t, .{
        .hash_seed = p.hash_seed,
        .now = p.now,
        .parent = p.parent,
        .parent_value = p.parent_value,
        .name = p.name,
        .mode = S.IFLNK | 0o777,
        .uid = p.uid,
        .gid = p.gid,
        .initial_size = p.target.len,
        .local_ino = p.local_ino,
        .global_ino = p.global_ino,
        .chain = p.chain,
    });
    var buf: [1 + SYMLINK_XATTR.len + SYMLINK_TARGET_MAX]u8 = undefined;
    const xv = (record.XattrValue{ .name = SYMLINK_XATTR, .value = p.target }).encodeInto(&buf) catch unreachable;
    try t.stagePut(key.TREE_XATTRS, &key.xattrKey(p.local_ino, key.xattrNameHash56(SYMLINK_XATTR, p.hash_seed), 0), xv);
    return out;
}

pub const Link = struct {
    hash_seed: u64,
    now: u64,
    parent: u64,
    /// null = unreadable: the parent times are best-effort (the v2 arm).
    parent_value: ?InodeValue,
    name: []const u8,
    /// LOCAL child ino and its GLOBAL encoding for the dentry.
    child: u64,
    global_child: u64,
    /// The child's value FOLDED through `PendingTimes.fold` by the caller:
    /// the reply is served from this record, so the ctime bump must never
    /// regress below a base + refinement a reader already saw.
    child_value: InodeValue,
    chain: []const []const u8 = &.{},
};

pub const LinkError = DentryError || error{TooManyLinks};

pub const LinkOutcome = struct {
    guards: GuardPlan,
    child: InodeValue,
};

/// Routed same-volume link — ONE whole-tx entry: child `Put` (`nlink + 1`,
/// ctime bumped MONOTONE over the folded view), dentry `Put`, parent
/// times `Put` (exclusive parent — the reference's shape). After the
/// commit lands the caller retires the child's pending refinement: the
/// `Put` carries it.
pub fn stageLink(t: *tx.KvTx, p: Link) LinkError!LinkOutcome {
    var child = p.child_value;
    if (child.nlink >= NLINK_MAX) return error.TooManyLinks;
    child.nlink += 1;
    if (laterThan(p.now, child.ctime)) child.ctime = p.now;
    try t.stagePut(key.TREE_INODES, &key.inodeKey(p.child), &child.encode());
    _ = try stageDentryPut(t, p.hash_seed, p.parent, p.name, p.chain, p.global_child, ftByte(child.mode));
    const rec = try stageParentUpdate(t, p.parent, false, 0, p.now, p.parent_value);
    var plan: GuardPlan = .{};
    plan.inode(p.parent, parentGuardFor(rec));
    plan.inode(p.child, .exclusive);
    plan.dentry(p.parent, p.name);
    return .{ .guards = plan, .child = child };
}

// ---------------------------------------------------------------------------
// unlink / rmdir.
// ---------------------------------------------------------------------------

pub const Unlink = struct {
    now: u64,
    parent: u64,
    name: []const u8,
    /// The dentry as found under the D-guard.
    pos: DentryPos,
    /// LOCAL child ino and its record.
    child: u64,
    child_value: InodeValue,
    is_dir: bool,
    /// Read under the guard when the parent is EXCLUSIVE (a directory
    /// child, or a self-reference); null = unreadable, best-effort.
    parent_value: ?InodeValue = null,
};

pub const UnlinkOutcome = struct {
    guards: GuardPlan,
    /// The child as staged: `nlink == 0` marks a corpse — the FORGET-side
    /// reclaim gathers it into ONE batched destroy entry
    /// (`inos.stageDestroy`); the record itself outlives the unlink until
    /// then (the kernel may `getattr` an unlinked-open inode).
    child: InodeValue,
    parent_shared: bool,

    pub fn dying(self: UnlinkOutcome) bool {
        return self.child.nlink == 0;
    }
};

/// A regular file's parent is SHARED (its update is `SharedTimes`); a
/// directory child's needs the nlink RMW and a self-reference the same
/// record twice — both EXCLUSIVE.
pub fn unlinkParentShared(is_dir: bool, parent: u64, child: u64) bool {
    return !is_dir and parent != child;
}

/// Routed same-volume unlink — ONE whole-tx entry: dentry `Delete`, the
/// parent update (`SharedTimes`, or the exclusive `Put` with `nlink − 1`
/// for a directory), the child `Put` (`nlink − 1`; a directory ⇒ 0, its
/// `.` self-link and its parent entry vanish together; `ctime = now`).
pub fn stageUnlink(t: *tx.KvTx, p: Unlink) Allocator.Error!UnlinkOutcome {
    const shared = unlinkParentShared(p.is_dir, p.parent, p.child);
    try t.stageDelete(key.TREE_DENTRIES, &p.pos.key);
    const rec = try stageParentUpdate(t, p.parent, shared, if (p.is_dir) -1 else 0, p.now, p.parent_value);
    var child = p.child_value;
    if (p.is_dir) {
        child.nlink = 0;
    } else if (child.nlink > 0) {
        child.nlink -= 1;
    }
    child.ctime = p.now;
    try t.stagePut(key.TREE_INODES, &key.inodeKey(p.child), &child.encode());
    var plan: GuardPlan = .{};
    plan.inode(p.parent, parentGuardFor(rec));
    plan.inode(p.child, .exclusive);
    plan.dentry(p.parent, p.name);
    return .{ .guards = plan, .child = child, .parent_shared = shared };
}

/// `stageUnlink` of a directory child: parent `nlink − 1` under the
/// exclusive parent, child `nlink = 0`. Emptiness is the caller's probe.
pub fn stageRmdir(t: *tx.KvTx, p: Unlink) Allocator.Error!UnlinkOutcome {
    var q = p;
    q.is_dir = true;
    return stageUnlink(t, q);
}

// ---------------------------------------------------------------------------
// rename — ONE tx.
// ---------------------------------------------------------------------------

pub const RenameFlags = struct {
    noreplace: bool = false,
    exchange: bool = false,
};

/// `RENAME_WHITEOUT`: the char-0:0 whiteout minted at the OLD name in the
/// same entry (overlayfs' rename-whiteout-over-victim depends on the
/// atomicity). `(local key ino, global dentry ino)` pre-allocated by the
/// routed layer.
pub const Whiteout = struct { local: u64, global: u64 };

pub const Rename = struct {
    hash_seed: u64,
    now: u64,
    old_parent: u64,
    old_name: []const u8,
    new_parent: u64,
    new_name: []const u8,
    flags: RenameFlags = .{},
    /// Both dentries as found under the guards.
    old_pos: ?DentryPos,
    new_pos: ?DentryPos,
    /// Live chain scans for the re-inserts: the new name's always; the old
    /// name's for EXCHANGE and the whiteout.
    new_chain: []const []const u8 = &.{},
    old_chain: []const []const u8 = &.{},
    /// The moved (source) inode's LOCAL ino when it lives on this volume;
    /// a remote child's stamp is the routed layer's per-volume fragment.
    src_local: ?u64 = null,
    /// The replaced (destination) inode's LOCAL ino when local.
    dest_local: ?u64 = null,
    /// Parent values, read under the guards, for the directory-move nlink
    /// shift; null = unreadable (best-effort, nothing staged for it).
    old_parent_value: ?InodeValue = null,
    new_parent_value: ?InodeValue = null,
    /// The destination inode's value when `dest_local` and `new_pos`.
    dest_value: ?InodeValue = null,
    /// The ENOTEMPTY probe of a directory destination.
    dest_has_entries: bool = false,
    whiteout: ?Whiteout = null,
};

pub const RenameError = DentryError || error{
    /// `NOREPLACE|EXCHANGE`, or `WHITEOUT` with `EXCHANGE` (VFS-forbidden).
    InvalidFlags,
    /// The source (or, under EXCHANGE, either) dentry is absent.
    SourceNotFound,
    /// `NOREPLACE` and the destination exists.
    TargetExists,
    /// A directory destination with entries.
    TargetNotEmpty,
};

pub const RenameOutcome = struct {
    guards: GuardPlan,
    /// The replaced destination's post-op nlink when it was local (null:
    /// nothing replaced here); 0 marks the corpse for the batched destroy.
    dest_nlink: ?u32 = null,
    /// The new dentry's key (its `coll_seq` is the overlay-aware probe's).
    new_key: [key.DENTRY_KEY_LEN]u8,
};

/// Routed same-volume rename — ONE whole-tx entry: EXCHANGE swap /
/// NOREPLACE guard / replace with destination nlink accounting + ENOTEMPTY,
/// the directory-move parent nlink shift (`..` now names the new parent:
/// old `− 1`, new `+ 1`, as full `Put`s that carry the parents' times), the
/// parents' `SharedTimes` otherwise, the moved inode's Δctime, the whiteout.
/// Stage order is the reference's. Parents' guards: SHARED where they
/// receive only a Δ, EXCLUSIVE where the directory move rewrites them.
pub fn stageRename(t: *tx.KvTx, p: Rename) RenameError!RenameOutcome {
    if (p.flags.noreplace and p.flags.exchange) return error.InvalidFlags;
    if (p.whiteout != null and p.flags.exchange) return error.InvalidFlags;
    const cross_dir = p.old_parent != p.new_parent;
    var plan: GuardPlan = .{};
    plan.dentry(p.old_parent, p.old_name);
    plan.dentry(p.new_parent, p.new_name);

    if (p.flags.exchange) {
        const old_d = p.old_pos orelse return error.SourceNotFound;
        const new_d = p.new_pos orelse return error.SourceNotFound;
        try t.stageDelete(key.TREE_DENTRIES, &old_d.key);
        try t.stageDelete(key.TREE_DENTRIES, &new_d.key);
        const nk = try stageDentryPut(t, p.hash_seed, p.new_parent, p.new_name, p.new_chain, old_d.child_ino, old_d.file_type);
        _ = try stageDentryPut(t, p.hash_seed, p.old_parent, p.old_name, p.old_chain, new_d.child_ino, new_d.file_type);
        // Both parents' Δtimes + both swapped inodes' Δctimes ride the
        // swap entry, deduped for the shared-parent / same-inode shapes.
        try (SharedTimes{ .parent = p.old_parent, .now = p.now }).stage(t);
        if (cross_dir) try (SharedTimes{ .parent = p.new_parent, .now = p.now }).stage(t);
        if (p.src_local) |src| try t.stageDelta(key.TREE_INODES, &key.inodeKey(src), InodeDelta.ctime(p.now));
        if (p.dest_local) |dst| {
            if (p.src_local != dst) try t.stageDelta(key.TREE_INODES, &key.inodeKey(dst), InodeDelta.ctime(p.now));
        }
        plan.inode(p.old_parent, .shared);
        plan.inode(p.new_parent, .shared);
        if (p.src_local) |src| plan.inode(src, .exclusive);
        if (p.dest_local) |dst| plan.inode(dst, .exclusive);
        return .{ .guards = plan, .new_key = nk };
    }

    if (p.flags.noreplace and p.new_pos != null) return error.TargetExists;
    const old_d = p.old_pos orelse return error.SourceNotFound;
    const is_dir = @as(u32, old_d.file_type) << 12 == S.IFDIR;
    if (is_dir and cross_dir) {
        // The moved directory's `..` now names the new parent: the nlink
        // shift rewrites both parents, and the times fold into those
        // `Put`s (same records, same entry; no separate Δ).
        const old_rec = try stageParentUpdate(t, p.old_parent, false, -1, p.now, p.old_parent_value);
        const new_rec = try stageParentUpdate(t, p.new_parent, false, 1, p.now, p.new_parent_value);
        plan.inode(p.old_parent, parentGuardFor(old_rec));
        plan.inode(p.new_parent, parentGuardFor(new_rec));
    } else {
        try (SharedTimes{ .parent = p.old_parent, .now = p.now }).stage(t);
        if (cross_dir) try (SharedTimes{ .parent = p.new_parent, .now = p.now }).stage(t);
        plan.inode(p.old_parent, .shared);
        plan.inode(p.new_parent, .shared);
    }

    var dest_replaced: ?u64 = null;
    var dest_nlink: ?u32 = null;
    if (p.new_pos) |new_d| {
        if (p.dest_local) |dest| {
            if (p.dest_value) |dv0| {
                var dv = dv0;
                if (isDirMode(dv.mode)) {
                    if (p.dest_has_entries) return error.TargetNotEmpty;
                    // A replaced directory is REMOVED: its `.` self-link and
                    // its parent entry vanish together (generic/035).
                    dv.nlink = 0;
                } else if (dv.nlink > 0) {
                    dv.nlink -= 1;
                }
                dv.ctime = p.now;
                try t.stagePut(key.TREE_INODES, &key.inodeKey(dest), &dv.encode());
                dest_replaced = dest;
                dest_nlink = dv.nlink;
            }
        }
        try t.stageDelete(key.TREE_DENTRIES, &new_d.key);
    }
    try t.stageDelete(key.TREE_DENTRIES, &old_d.key);
    const nk = try stageDentryPut(t, p.hash_seed, p.new_parent, p.new_name, p.new_chain, old_d.child_ino, old_d.file_type);
    // The moved inode's ctime in the same entry — unless the dest-replace
    // `Put` already stamped the same local inode (rename over a hardlink
    // of itself).
    if (p.src_local) |src| {
        if (dest_replaced != src) try t.stageDelta(key.TREE_INODES, &key.inodeKey(src), InodeDelta.ctime(p.now));
    }
    if (p.whiteout) |w| {
        // VFS contract: S_IFCHR, perm 0, rdev 0, owner 0:0 (overlayfs checks
        // type + rdev only).
        const wv: InodeValue = .{
            .mode = S.IFCHR,
            .uid = 0,
            .gid = 0,
            .nlink = 1,
            .flags = 0,
            .rdev = 0,
            .size = 0,
            .atime = p.now,
            .mtime = p.now,
            .ctime = p.now,
        };
        try t.stagePut(key.TREE_INODES, &key.inodeKey(w.local), &wv.encode());
        _ = try stageDentryPut(t, p.hash_seed, p.old_parent, p.old_name, p.old_chain, w.global, ftByte(S.IFCHR));
    }
    if (p.src_local) |src| plan.inode(src, .exclusive);
    if (p.dest_local) |dst| plan.inode(dst, .exclusive);
    return .{ .guards = plan, .dest_nlink = dest_nlink, .new_key = nk };
}

/// The rename economy gate (G4): one tx per rename plus the drain's
/// amortization; 1.003/op measured through the mount at 100 k ops.
pub const RENAME_ENTRIES_PER_OP_GATE: f64 = 1.02;

/// Entries per rename over a storm: one entry per op (the whole-tx shape)
/// plus the batched drains of the absorbed echoes.
pub fn renameEntriesPerOp(ops: u64, drain_commits: u64) f64 {
    assert(ops > 0);
    return @as(f64, @floatFromInt(ops + drain_commits)) / @as(f64, @floatFromInt(ops));
}

// ---------------------------------------------------------------------------
// setattr + THE ABSORBER.
// ---------------------------------------------------------------------------

/// Inos per drain transaction — one DLM `lockMany` set, one entry (the
/// `destroy_inodes` batching shape: wide enough that a storm's echoes
/// amortize to a few entries per thousand ops, narrow enough that one
/// entry's records stay far inside the 128 KiB whole-entry cap at 10–18 B
/// per Δ).
pub const PENDING_TIMES_DRAIN_BATCH: usize = 128;

/// The parked population that wakes the drain AHEAD of its cadence tick:
/// bounds the map and the crash-loss window by COUNT, not just time (a
/// refinement is µs-grade time polish, but an unbounded map under a storm
/// would be an unbounded loss on a kill).
pub const PENDING_TIMES_DRAIN_CAP: u64 = 512;

/// A parked refinement: the kernel's post-op times for one ino.
pub const Times = struct { mtime: u64, ctime: u64 };

/// What `park` answers: whether the population just crossed the cap (the
/// drain's early wake).
pub const Park = enum { parked, parked_wake_drain };

pub const SetattrReq = struct {
    mode: ?u32 = null,
    uid: ?u32 = null,
    gid: ?u32 = null,
    size: ?u64 = null,
    atime: ?u64 = null,
    mtime: ?u64 = null,
    ctime: ?u64 = null,

    /// The echo's first shape test: no value field, no atime.
    pub fn timesOnly(self: SetattrReq) bool {
        return self.mode == null and self.uid == null and self.gid == null and self.size == null and self.atime == null;
    }
};

/// The absorber's verdict.
pub const Absorb = enum {
    /// Parked (or already at/behind the folded view): zero journal entries.
    absorbed,
    /// Real news: the caller stages the commit arm.
    stage,
};

/// The pending-times map, `ino → (mtime, ctime)`: the kernel's post-op
/// writeback echoes parked by `absorb` (and the write path's own stamp —
/// the generic/003 single-authority law: the stamp the handler publishes
/// to the attr cache is the ONLY author of a write's times, and this is
/// how it becomes durable). Folded over EVERY inode read (`fold`) as a
/// signed monotone max — a stale refinement never regresses a fresher
/// committed write; made durable by batched drain transactions on the
/// flush cadence / fsync / unmount / the cap.
///
/// Mutations of ONE ino run under its 4a I-guard (absorb, the commit-side
/// retire) or the drain's own guard set, so entries never race per ino;
/// the container is striped under spin locks (O(1) sections — the
/// metadata-transaction class) with a lock-free count so the read path's
/// common case — an empty map — is one relaxed load. The stripes are
/// pre-sized to the cap: the drain keeps the population near or below it,
/// and growth past it is one allocation per doubling, never per op.
pub const PendingTimes = struct {
    gpa: Allocator,
    stripes: [stripe_count]Stripe,
    count: std.atomic.Value(u64) = .init(0),

    /// 16 stripes: an inode-read storm folds under a lock only while the
    /// map is non-empty, and spreading the line over 16 keeps that cost
    /// off any single cache line.
    const stripe_count = 16;

    const Stripe = struct {
        lock: jc.SpinLock = .{},
        map: std.AutoHashMapUnmanaged(u64, Times) = .empty,
    };

    pub fn init(gpa: Allocator) Allocator.Error!PendingTimes {
        var self: PendingTimes = .{ .gpa = gpa, .stripes = @splat(.{}) };
        errdefer self.deinit();
        for (&self.stripes) |*s| try s.map.ensureTotalCapacity(gpa, PENDING_TIMES_DRAIN_CAP / stripe_count);
        return self;
    }

    pub fn deinit(self: *PendingTimes) void {
        stats.meta_kv_times_echo_pending.sub(self.count.load(.monotonic));
        for (&self.stripes) |*s| s.map.deinit(self.gpa);
        self.* = undefined;
    }

    fn stripeOf(self: *PendingTimes, ino: u64) *Stripe {
        // Fibonacci hashing: consecutive mints spread over the stripes.
        return &self.stripes[@intCast((ino *% 0x9E3779B97F4A7C15) >> 60)];
    }

    /// Live refinements (the `meta_kv_times_echo_pending` gauge's input and
    /// the drain task's "anything to do?" probe).
    pub fn len(self: *const PendingTimes) u64 {
        return self.count.load(.monotonic);
    }

    /// The parked refinement of `ino`, if any (the drain's re-read under
    /// its guard: a committed setattr may have retired it since the
    /// snapshot).
    pub fn parked(self: *PendingTimes, ino: u64) ?Times {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        return s.map.get(ino);
    }

    /// Fold the pending refinement over an inode value — signed monotone
    /// max per field. One relaxed load when the map is empty.
    pub fn fold(self: *PendingTimes, ino: u64, v: *InodeValue) void {
        if (self.count.load(.monotonic) == 0) return;
        const p = self.parked(ino) orelse return;
        if (laterThan(p.mtime, v.mtime)) v.mtime = p.mtime;
        if (laterThan(p.ctime, v.ctime)) v.ctime = p.ctime;
    }

    /// Park a refinement: signed monotone merge per field (a parked stamp
    /// never regresses a fresher one). Answers the cap crossing.
    pub fn park(self: *PendingTimes, ino: u64, mtime: u64, ctime: u64) Allocator.Error!Park {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        const gop = try s.map.getOrPut(self.gpa, ino);
        if (gop.found_existing) {
            if (laterThan(mtime, gop.value_ptr.mtime)) gop.value_ptr.mtime = mtime;
            if (laterThan(ctime, gop.value_ptr.ctime)) gop.value_ptr.ctime = ctime;
        } else {
            gop.value_ptr.* = .{ .mtime = mtime, .ctime = ctime };
            _ = self.count.fetchAdd(1, .monotonic);
            stats.meta_kv_times_echo_pending.add(1);
        }
        return if (self.count.load(.monotonic) >= PENDING_TIMES_DRAIN_CAP) .parked_wake_drain else .parked;
    }

    /// Retire `ino`'s refinement: a committed inode write now carries — or
    /// intentionally supersedes — it (an exact-set utimes must never be
    /// max-clamped by a dead echo); a destroyed corpse's is moot. Under the
    /// ino's I-guard, so it never races an absorb. True iff one was parked.
    pub fn retire(self: *PendingTimes, ino: u64) bool {
        const s = self.stripeOf(ino);
        s.lock.lock();
        defer s.lock.unlock();
        if (s.map.fetchRemove(ino) == null) return false;
        _ = self.count.fetchSub(1, .monotonic);
        stats.meta_kv_times_echo_pending.sub(1);
        return true;
    }

    /// THE ABSORB POLICY. `v` is the stored record; it leaves FOLDED (the
    /// eligibility compares against the freshest view, and the commit
    /// arm's RMW must carry, never clobber, a newer refined ctime) and, on
    /// absorption, carrying the served ctime. Absorbed iff times-only AND
    /// mtime absent-or-equal to the folded value AND ctime present — the
    /// echo's shape and nothing wider: a changed mtime is the buffered-
    /// write flush (kernel-authored write times), atime is an explicit
    /// utimes, a value field is chmod/chown/truncate — all stage. A ctime
    /// at-or-behind the folded view (the kernel's coarse clock can stamp
    /// behind the daemon's in-tx stamp) persists nothing: still an
    /// absorption, still zero entries, ctime never moves backwards.
    pub fn absorb(self: *PendingTimes, ino: u64, v: *InodeValue, req: SetattrReq) Allocator.Error!Absorb {
        self.fold(ino, v);
        if (!req.timesOnly()) return .stage;
        if (req.mtime) |m| if (m != v.mtime) return .stage;
        const req_ctime = req.ctime orelse return .stage;
        stats.meta_kv_times_echo_absorbed.inc();
        if (laterThan(req_ctime, v.ctime)) {
            v.ctime = req_ctime;
            _ = try self.park(ino, v.mtime, req_ctime);
        }
        return .absorbed;
    }

    /// Snapshot up to one batch of parked inos (any order) — BEFORE any
    /// gate or guard, so a drain with no work touches nothing. A full
    /// batch tells the drain to loop.
    pub fn drainBatch(self: *PendingTimes, out: *[PENDING_TIMES_DRAIN_BATCH]u64) []u64 {
        var n: usize = 0;
        for (&self.stripes) |*s| {
            s.lock.lock();
            defer s.lock.unlock();
            var it = s.map.keyIterator();
            while (it.next()) |k| {
                if (n == out.len) return out[0..n];
                out[n] = k.*;
                n += 1;
            }
        }
        return out[0..n];
    }
};

/// The drain's per-ino record: the ADVANCE only — `times(m, c)` when mtime
/// advances (the write path's stamp), `ctime(c)` when only ctime does (an
/// echo-born refinement: its mtime is invariant-equal to the stored one);
/// null when the stored record already carries it (a fresher committed
/// write superseded the refinement — the drain must not regress it).
pub fn drainDelta(parked_times: Times, stored: InodeValue) ?InodeDelta {
    const adv_m = laterThan(parked_times.mtime, stored.mtime);
    const adv_c = laterThan(parked_times.ctime, stored.ctime);
    if (!adv_m and !adv_c) return null;
    const m = if (adv_m) parked_times.mtime else stored.mtime;
    const c = if (adv_c) parked_times.ctime else stored.ctime;
    return if (adv_m) InodeDelta.times(m, c) else InodeDelta.ctime(c);
}

pub const DrainMember = enum {
    /// A Δ staged into the drain tx.
    staged,
    /// The inode is gone (destroyed since the park): GC, no record.
    gc_destroyed,
    /// The stored record already carries the refinement.
    no_advance,
};

/// Stage one batch member under its exclusive guard: `stored` is the
/// inode as read now (null = destroyed). Every outcome retires the ino
/// after the drain tx lands (a failed commit keeps it parked; the next
/// trigger retries).
pub fn stageDrainMember(t: *tx.KvTx, ino: u64, parked_times: Times, stored: ?InodeValue) Allocator.Error!DrainMember {
    const v = stored orelse return .gc_destroyed;
    const d = drainDelta(parked_times, v) orelse return .no_advance;
    try t.stageDelta(key.TREE_INODES, &key.inodeKey(ino), d);
    return .staged;
}

/// A drain tx landed: one entry, `records` refinements durable.
pub fn noteDrainCommitted(records: u64) void {
    stats.meta_kv_times_echo_drain_commits.inc();
    stats.meta_kv_times_echo_drained.add(records);
}

pub const SetattrVerdict = enum { absorbed, staged };

pub const SetattrOutcome = struct {
    verdict: SetattrVerdict,
    /// The served reply: the folded view (absorbed) or the record as
    /// staged.
    value: InodeValue,
    /// `I{ino}` exclusive either way — the absorb arm mutates the map under
    /// it.
    guards: GuardPlan,
};

/// The setattr arm: `absorb` first; otherwise the field RMW over the
/// FOLDED view — explicit sets verbatim (exact-set semantics), a value-
/// field change auto-bumps ctime MONOTONE to `now` (a parked refinement
/// can sit ahead of the coarse clock inside a tick; an auto-stamp must
/// never regress a ctime a reader already saw — generic/423) — staged as
/// one inode `Put`. After the commit lands the caller retires the pending
/// refinement: the `Put` carries or supersedes it.
pub fn stageSetattr(t: *tx.KvTx, pending: *PendingTimes, ino: u64, stored: InodeValue, req: SetattrReq, now: u64) Allocator.Error!SetattrOutcome {
    var v = stored;
    var plan: GuardPlan = .{};
    plan.inode(ino, .exclusive);
    if ((try pending.absorb(ino, &v, req)) == .absorbed) return .{ .verdict = .absorbed, .value = v, .guards = plan };
    var ctime_updated = false;
    if (req.mode) |m| {
        v.mode = m;
        ctime_updated = true;
    }
    if (req.uid) |u| {
        v.uid = u;
        ctime_updated = true;
    }
    if (req.gid) |g| {
        v.gid = g;
        ctime_updated = true;
    }
    if (req.size) |s| {
        v.size = s;
        ctime_updated = true;
    }
    if (req.atime) |a| v.atime = a;
    if (req.mtime) |m| v.mtime = m;
    if (req.ctime) |c| {
        v.ctime = c;
    } else if (ctime_updated and laterThan(now, v.ctime)) {
        v.ctime = now;
    }
    try t.stagePut(key.TREE_INODES, &key.inodeKey(ino), &v.encode());
    return .{ .verdict = .staged, .value = v, .guards = plan };
}

// ---------------------------------------------------------------------------
// Tests — every record set decoded and pinned; the fold of each staged set
// over its base yields the post-op inode.
// ---------------------------------------------------------------------------

const testing = std.testing;
const inos = @import("inos.zig");
const journal = @import("journal.zig");

const seed: u64 = 0xC0FFEE;
const t_now: u64 = 1_700_000_000_000_000_000;

fn dirValue(nlink: u32) InodeValue {
    return .{ .mode = S.IFDIR | 0o755, .uid = 10, .gid = 20, .nlink = nlink, .size = 4096, .atime = 1, .mtime = 2, .ctime = 3 };
}

fn fileValue(nlink: u32) InodeValue {
    return .{ .mode = S.IFREG | 0o644, .uid = 10, .gid = 20, .nlink = nlink, .size = 7, .atime = 4, .mtime = 5, .ctime = 6 };
}

fn newTx() tx.KvTx {
    return tx.KvTx.init(testing.allocator, @src());
}

/// A live dentry for `(parent, name)` at `coll` naming `child`.
fn pos(parent: u64, name: []const u8, coll: u8, child: u64, mode: u32) DentryPos {
    return .{ .key = key.dentryKey(parent, key.dentryNameHash54(name, seed), coll), .child_ino = child, .file_type = ftByte(mode) };
}

const Shape = struct { tree: u8, kind: record.RecordKind };

fn expectShape(t: *const tx.KvTx, want: []const Shape) !void {
    try testing.expectEqual(want.len, t.staged.items.len);
    for (want, t.staged.items) |w, s| {
        try testing.expectEqual(w.tree, s.tree_id);
        try testing.expectEqual(w.kind, s.kind);
    }
}

fn stagedInode(t: *const tx.KvTx, i: usize) !InodeValue {
    try testing.expectEqual(key.TREE_INODES, t.staged.items[i].tree_id);
    try testing.expectEqual(record.RecordKind.put, t.staged.items[i].kind);
    return InodeValue.decode(t.staged.items[i].value);
}

fn stagedDelta(t: *const tx.KvTx, i: usize, ino: u64, mask: u16) !InodeDelta {
    const s = t.staged.items[i];
    try testing.expectEqual(key.TREE_INODES, s.tree_id);
    try testing.expectEqual(record.RecordKind.delta, s.kind);
    try testing.expectEqual(ino, try key.decodeInodeKey(s.key));
    const d = try InodeDelta.decode(s.value);
    try testing.expectEqual(mask, d.mask);
    return d;
}

fn stagedDentry(t: *const tx.KvTx, i: usize, parent: u64, name: []const u8) !record.DentryValue {
    const s = t.staged.items[i];
    try testing.expectEqual(key.TREE_DENTRIES, s.tree_id);
    try testing.expectEqual(record.RecordKind.put, s.kind);
    const dk = try key.DentryKey.decode(s.key);
    try testing.expectEqual(parent, dk.parentIno());
    try testing.expectEqual(key.dentryNameHash54(name, seed), dk.hash54());
    const dv = try record.DentryValue.decode(s.value);
    try testing.expectEqualStrings(name, dv.name);
    return dv;
}

fn expectDentryDelete(t: *const tx.KvTx, i: usize, k: []const u8) !void {
    const s = t.staged.items[i];
    try testing.expectEqual(key.TREE_DENTRIES, s.tree_id);
    try testing.expectEqual(record.RecordKind.delete, s.kind);
    try testing.expectEqualSlices(u8, k, s.key);
}

/// THE fold of the staged inode records for `ino` over `base` (seqs by
/// stage position above the base's): the post-op record a reader sees.
fn foldInode(t: *const tx.KvTx, ino: u64, base: ?InodeValue) !?InodeValue {
    const k = key.inodeKey(ino);
    var refs: std.ArrayList(record.RecordRef) = .empty;
    defer refs.deinit(testing.allocator);
    var i = t.staged.items.len;
    while (i > 0) {
        i -= 1;
        const s = t.staged.items[i];
        if (s.tree_id != key.TREE_INODES or !std.mem.eql(u8, s.key, &k)) continue;
        try refs.append(testing.allocator, .{ .key = s.key, .seq = i + 1, .kind = s.kind, .value = s.value });
    }
    var base_enc: [record.INODE_VALUE_LEN]u8 = undefined;
    if (base) |b| {
        base_enc = b.encode();
        try refs.append(testing.allocator, .{ .key = &k, .seq = 0, .kind = .put, .value = &base_enc });
    }
    var folded = try record.foldNewestFirst(testing.allocator, refs.items);
    defer folded.deinit(testing.allocator);
    const live = folded.liveValue() orelse return null;
    return try InodeValue.decode(live);
}

/// The journal-entry count of a staged op: ONE, admitted by the entry
/// arithmetic the commit enforces (`tx.entryLenFor` refuses past the cap).
fn entriesOf(t: *const tx.KvTx) !u64 {
    const recs = try testing.allocator.alloc(tx.Rec, t.staged.items.len);
    defer testing.allocator.free(recs);
    for (t.staged.items, recs) |s, *r| r.* = .{ .tree_id = s.tree_id, .key = s.key, .kind = s.kind, .value = s.value };
    try testing.expect((try tx.entryLenFor(recs)) <= journal.MAX_ENTRY_LEN);
    return 1;
}

// ---- guard plan ----

test "guard plan: repeated inos keep the strongest mode, dentries dedup, the parent class derives from the record kind" {
    var plan: GuardPlan = .{};
    plan.inode(5, .shared);
    plan.inode(7, .exclusive);
    plan.inode(5, .exclusive);
    plan.inode(7, .shared);
    plan.inode(9, .shared);
    plan.dentry(5, "a");
    plan.dentry(5, "a");
    plan.dentry(5, "b");
    try testing.expectEqual(@as(usize, 3), plan.inodes().len);
    try testing.expectEqual(dlm.Mode.exclusive, plan.inodeMode(5).?);
    try testing.expectEqual(dlm.Mode.exclusive, plan.inodeMode(7).?);
    try testing.expectEqual(dlm.Mode.shared, plan.inodeMode(9).?);
    try testing.expect(plan.inodeMode(11) == null);
    try testing.expectEqual(@as(usize, 2), plan.dentries().len);
    try testing.expect(plan.hasDentry(5, "a") and plan.hasDentry(5, "b") and !plan.hasDentry(6, "a"));
    for (plan.dentries()) |d| try testing.expectEqual(dlm.Mode.exclusive, d.mode);
    try testing.expectEqual(dlm.Mode.shared, parentGuardFor(.delta_times));
    try testing.expectEqual(dlm.Mode.exclusive, parentGuardFor(.put));
    try testing.expectEqual(dlm.Mode.exclusive, parentGuardFor(.none));
    try testing.expectEqual(dlm.Mode.shared, (SharedTimes{ .parent = 1, .now = 9 }).guard().mode);
}

// ---- create / mkdir / symlink / link ----

test "create (regular file): inode Put + dentry Put + parent Δtimes; the parent guard is SHARED; the fold moves only the parent's times" {
    var t = newTx();
    defer t.deinit();
    const out = try stageCreate(&t, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = dirValue(3), .name = "f", .mode = S.IFREG | 0o644, .uid = 100, .gid = 200, .local_ino = 42, .global_ino = 42_000 });
    try expectShape(&t, &.{ .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta } });
    try testing.expectEqual(@as(u64, 42), try key.decodeInodeKey(t.staged.items[0].key));
    const child = try stagedInode(&t, 0);
    try testing.expectEqual(InodeValue{ .mode = S.IFREG | 0o644, .uid = 100, .gid = 200, .nlink = 1, .rdev = 0, .size = 0, .atime = t_now, .mtime = t_now, .ctime = t_now }, child);
    try testing.expectEqual(child, out.child);
    const dv = try stagedDentry(&t, 1, 1, "f");
    try testing.expectEqual(@as(u64, 42_000), dv.child_ino);
    try testing.expectEqual(ftByte(S.IFREG), dv.file_type);
    try testing.expectEqual(@as(u8, 0), (try key.DentryKey.decode(t.staged.items[1].key)).collSeq());
    const d = try stagedDelta(&t, 2, 1, record.DELTA_TIMES);
    try testing.expectEqual(t_now, d.fields.mtime);
    try testing.expectEqual(t_now, d.fields.ctime);
    try testing.expectEqual(dlm.Mode.shared, out.guards.inodeMode(1).?);
    try testing.expectEqual(@as(usize, 1), out.guards.inodes().len);
    try testing.expect(out.guards.hasDentry(1, "f"));
    const parent = (try foldInode(&t, 1, dirValue(3))).?;
    var want = dirValue(3);
    want.mtime = t_now;
    want.ctime = t_now;
    try testing.expectEqual(want, parent);
    try testing.expectEqual(child, (try foldInode(&t, 42, null)).?);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));
}

test "create: the chain probe takes the first free coll_seq; setgid parents hand down gid (and the bit to a directory); rdev and size ride the record" {
    var t = newTx();
    defer t.deinit();
    const k0 = pos(1, "dup", 0, 9, S.IFREG).key;
    const k1 = pos(1, "dup", 1, 9, S.IFREG).key;
    var parent = dirValue(2);
    parent.mode |= S.ISGID;
    parent.gid = 777;
    const f = try stageCreate(&t, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = parent, .name = "dup", .mode = S.IFCHR | 0o600, .uid = 1, .gid = 2, .rdev = 0x0501, .initial_size = 0, .local_ino = 50, .global_ino = 50, .chain = &.{ &k0, &k1 } });
    try testing.expectEqual(@as(u8, 2), (try key.DentryKey.decode(t.staged.items[1].key)).collSeq());
    try testing.expectEqual(@as(u32, 777), f.child.gid);
    try testing.expectEqual(S.IFCHR | 0o600, f.child.mode);
    try testing.expectEqual(@as(u32, 0x0501), f.child.rdev);
    try testing.expectEqual(ftByte(S.IFCHR), (try stagedDentry(&t, 1, 1, "dup")).file_type);
    var t2 = newTx();
    defer t2.deinit();
    const d = try stageMkdir(&t2, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = parent, .name = "sub", .mode = 0o755, .uid = 1, .gid = 2, .local_ino = 51, .global_ino = 51 });
    try testing.expectEqual(S.IFDIR | S.ISGID | 0o755, d.child.mode);
    try testing.expectEqual(@as(u32, 777), d.child.gid);
    try testing.expectEqual(@as(u32, 2), d.child.nlink);
}

test "mkdir: inode Put (nlink 2) + dentry Put + parent Put (nlink + 1, times) under the EXCLUSIVE parent; the fold shows the bump" {
    var t = newTx();
    defer t.deinit();
    const out = try stageMkdir(&t, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = dirValue(3), .name = "d", .mode = S.IFDIR | 0o700, .uid = 0, .gid = 0, .local_ino = 60, .global_ino = 60 });
    try expectShape(&t, &.{ .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .put } });
    try testing.expectEqual(@as(u32, 2), out.child.nlink);
    try testing.expectEqual(ftByte(S.IFDIR), (try stagedDentry(&t, 1, 1, "d")).file_type);
    const parent = try stagedInode(&t, 2);
    try testing.expectEqual(@as(u64, 1), try key.decodeInodeKey(t.staged.items[2].key));
    try testing.expectEqual(@as(u32, 4), parent.nlink);
    try testing.expectEqual(t_now, parent.mtime);
    try testing.expectEqual(t_now, parent.ctime);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(1).?);
    try testing.expectEqual(parent, (try foldInode(&t, 1, dirValue(3))).?);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));
}

test "symlink: the create records sized by the target plus the system.symlink xattr Put at coll_seq 0, ONE entry; a target past 4095 refuses" {
    var t = newTx();
    defer t.deinit();
    const out = try stageSymlink(&t, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = dirValue(2), .name = "l", .uid = 5, .gid = 6, .local_ino = 70, .global_ino = 70, .target = "/etc/passwd" });
    try expectShape(&t, &.{ .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_XATTRS, .kind = .put } });
    try testing.expectEqual(S.IFLNK | 0o777, out.child.mode);
    try testing.expectEqual(@as(u64, "/etc/passwd".len), out.child.size);
    try testing.expectEqual(ftByte(S.IFLNK), (try stagedDentry(&t, 1, 1, "l")).file_type);
    const xk = try key.XattrKey.decode(t.staged.items[3].key);
    try testing.expectEqual(@as(u64, 70), xk.ino());
    try testing.expectEqual(key.xattrNameHash56(SYMLINK_XATTR, seed), xk.hash56());
    try testing.expectEqual(@as(u8, 0), xk.collSeq());
    const xv = try record.XattrValue.decode(t.staged.items[3].value);
    try testing.expectEqualStrings(SYMLINK_XATTR, xv.name);
    try testing.expectEqualStrings("/etc/passwd", xv.value);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));

    var t2 = newTx();
    defer t2.deinit();
    const long: [SYMLINK_TARGET_MAX + 1]u8 = @splat('x');
    try testing.expectError(error.SymlinkTargetTooLong, stageSymlink(&t2, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = dirValue(2), .name = "l", .uid = 5, .gid = 6, .local_ino = 71, .global_ino = 71, .target = &long }));
    try testing.expect(t2.isEmpty());
    const max: [SYMLINK_TARGET_MAX]u8 = @splat('y');
    _ = try stageSymlink(&t2, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = dirValue(2), .name = "l", .uid = 5, .gid = 6, .local_ino = 71, .global_ino = 71, .target = &max });
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t2));
}

test "link: child Put (nlink + 1, ctime monotone over the folded refinement) + dentry Put + parent Put; EMLINK at 65000; parent and child EXCLUSIVE" {
    var t = newTx();
    defer t.deinit();
    var pending = try PendingTimes.init(testing.allocator);
    defer pending.deinit();
    // A parked refinement AHEAD of `now` (inside one coarse tick): the
    // served ctime must not regress to `now` (generic/423).
    _ = try pending.park(80, 5, t_now + 500);
    var child = fileValue(1);
    pending.fold(80, &child);
    const out = try stageLink(&t, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = dirValue(2), .name = "hard", .child = 80, .global_child = 80_000, .child_value = child });
    try expectShape(&t, &.{ .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .put } });
    const c = try stagedInode(&t, 0);
    try testing.expectEqual(@as(u32, 2), c.nlink);
    try testing.expectEqual(t_now + 500, c.ctime);
    try testing.expectEqual(c, out.child);
    const dv = try stagedDentry(&t, 1, 1, "hard");
    try testing.expectEqual(@as(u64, 80_000), dv.child_ino);
    try testing.expectEqual(ftByte(S.IFREG), dv.file_type);
    const parent = try stagedInode(&t, 2);
    try testing.expectEqual(@as(u32, 2), parent.nlink);
    try testing.expectEqual(t_now, parent.mtime);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(1).?);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(80).?);
    try testing.expect(out.guards.hasDentry(1, "hard"));
    // The commit landed: the Put carries the refinement — retire it.
    try testing.expect(pending.retire(80));
    try testing.expectEqual(@as(u64, 0), pending.len());

    // Without a refinement the bump is `now`; an unreadable parent stages
    // nothing for it (best-effort) but still plans the exclusive class.
    var t2 = newTx();
    defer t2.deinit();
    const o2 = try stageLink(&t2, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = null, .name = "hard", .child = 80, .global_child = 80_000, .child_value = fileValue(1) });
    try testing.expectEqual(@as(usize, 2), t2.staged.items.len);
    try testing.expectEqual(t_now, o2.child.ctime);
    try testing.expectEqual(dlm.Mode.exclusive, o2.guards.inodeMode(1).?);

    var t3 = newTx();
    defer t3.deinit();
    try testing.expectError(error.TooManyLinks, stageLink(&t3, .{ .hash_seed = seed, .now = t_now, .parent = 1, .parent_value = dirValue(2), .name = "hard", .child = 80, .global_child = 80, .child_value = fileValue(NLINK_MAX) }));
    try testing.expect(t3.isEmpty());
}

// ---- unlink / rmdir ----

test "unlink (regular file): dentry Delete + parent Δtimes + child Put (nlink − 1, ctime); SHARED parent; nlink 0 is the corpse mark" {
    var t = newTx();
    defer t.deinit();
    const p = pos(1, "f", 3, 42_000, S.IFREG);
    const out = try stageUnlink(&t, .{ .now = t_now, .parent = 1, .name = "f", .pos = p, .child = 42, .child_value = fileValue(2), .is_dir = false });
    try expectShape(&t, &.{ .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .put } });
    try expectDentryDelete(&t, 0, &p.key);
    _ = try stagedDelta(&t, 1, 1, record.DELTA_TIMES);
    const c = try stagedInode(&t, 2);
    try testing.expectEqual(@as(u64, 42), try key.decodeInodeKey(t.staged.items[2].key));
    try testing.expectEqual(@as(u32, 1), c.nlink);
    try testing.expectEqual(t_now, c.ctime);
    try testing.expectEqual(@as(u64, 5), c.mtime);
    try testing.expect(out.parent_shared);
    try testing.expect(!out.dying());
    try testing.expectEqual(dlm.Mode.shared, out.guards.inodeMode(1).?);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(42).?);
    try testing.expect(out.guards.hasDentry(1, "f"));
    try testing.expectEqual(c, (try foldInode(&t, 42, fileValue(2))).?);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));

    var t2 = newTx();
    defer t2.deinit();
    const last = try stageUnlink(&t2, .{ .now = t_now, .parent = 1, .name = "f", .pos = p, .child = 42, .child_value = fileValue(1), .is_dir = false });
    try testing.expect(last.dying());
    try testing.expectEqual(@as(u32, 0), last.child.nlink);
    // A self-reference takes the exclusive parent (the same record twice).
    try testing.expect(!unlinkParentShared(false, 42, 42));
    try testing.expect(!unlinkParentShared(true, 1, 42));
    try testing.expect(unlinkParentShared(false, 1, 42));
}

test "rmdir: dentry Delete + parent Put (nlink − 1, times) + child Put (nlink 0); EXCLUSIVE parent; the underflow floor counts instead of regressing below 2" {
    var t = newTx();
    defer t.deinit();
    const p = pos(1, "d", 0, 60, S.IFDIR);
    const out = try stageRmdir(&t, .{ .now = t_now, .parent = 1, .name = "d", .pos = p, .child = 60, .child_value = dirValue(2), .is_dir = false, .parent_value = dirValue(4) });
    try expectShape(&t, &.{ .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .put } });
    const parent = try stagedInode(&t, 1);
    try testing.expectEqual(@as(u32, 3), parent.nlink);
    try testing.expectEqual(t_now, parent.ctime);
    try testing.expectEqual(@as(u32, 0), out.child.nlink);
    try testing.expect(out.dying());
    try testing.expect(!out.parent_shared);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(1).?);
    try testing.expectEqual(parent, (try foldInode(&t, 1, dirValue(4))).?);

    const before = stats.dir_nlink_underflows.load();
    var t2 = newTx();
    defer t2.deinit();
    const floor = try stageRmdir(&t2, .{ .now = t_now, .parent = 1, .name = "d", .pos = p, .child = 60, .child_value = dirValue(2), .is_dir = true, .parent_value = dirValue(2) });
    try testing.expectEqual(@as(u32, 2), (try stagedInode(&t2, 1)).nlink);
    try testing.expectEqual(before + 1, stats.dir_nlink_underflows.load());
    try testing.expect(floor.dying());
    // An unreadable parent: nothing staged for it, the exclusive class planned.
    var t3 = newTx();
    defer t3.deinit();
    const o3 = try stageRmdir(&t3, .{ .now = t_now, .parent = 1, .name = "d", .pos = p, .child = 60, .child_value = dirValue(2), .is_dir = true, .parent_value = null });
    try testing.expectEqual(@as(usize, 2), t3.staged.items.len);
    try testing.expectEqual(dlm.Mode.exclusive, o3.guards.inodeMode(1).?);
}

// ---- rename ----

test "rename (same dir, regular file): ONE tx = ONE entry — parent Δtimes, dentry Delete, dentry Put, moved Δctime; parent SHARED, src exclusive, both D-guards" {
    var t = newTx();
    defer t.deinit();
    const old = pos(1, "a", 0, 42_000, S.IFREG);
    const out = try stageRename(&t, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .old_pos = old, .new_pos = null, .src_local = 42 });
    try expectShape(&t, &.{ .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta } });
    const pd = try stagedDelta(&t, 0, 1, record.DELTA_TIMES);
    try testing.expectEqual(t_now, pd.fields.mtime);
    try expectDentryDelete(&t, 1, &old.key);
    const dv = try stagedDentry(&t, 2, 1, "b");
    try testing.expectEqual(@as(u64, 42_000), dv.child_ino);
    try testing.expectEqual(ftByte(S.IFREG), dv.file_type);
    try testing.expectEqualSlices(u8, &out.new_key, t.staged.items[2].key);
    const cd = try stagedDelta(&t, 3, 42, record.DELTA_CTIME);
    try testing.expectEqual(t_now, cd.fields.ctime);
    try testing.expectEqual(@as(usize, 2), out.guards.inodes().len);
    try testing.expectEqual(dlm.Mode.shared, out.guards.inodeMode(1).?);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(42).?);
    try testing.expect(out.guards.hasDentry(1, "a") and out.guards.hasDentry(1, "b"));
    try testing.expectEqual(@as(usize, 2), out.guards.dentries().len);
    try testing.expect(out.dest_nlink == null);
    // The fold: the parent's times move, nothing else; the moved inode's
    // ctime moves, its mtime does not.
    const parent = (try foldInode(&t, 1, dirValue(5))).?;
    var want_p = dirValue(5);
    want_p.mtime = t_now;
    want_p.ctime = t_now;
    try testing.expectEqual(want_p, parent);
    const src = (try foldInode(&t, 42, fileValue(1))).?;
    var want_s = fileValue(1);
    want_s.ctime = t_now;
    try testing.expectEqual(want_s, src);
    // ONE entry per op: the tie.
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));
    try testing.expectEqual(@as(f64, 1.0), renameEntriesPerOp(1, 0));
}

test "a staged rename builds ONE conveyor queue entry carrying every record in stage order (the whole-tx shape the pass writes)" {
    var t = newTx();
    const old = pos(1, "a", 0, 42_000, S.IFREG);
    const out = try stageRename(&t, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 2, .new_name = "b", .old_pos = old, .new_pos = null, .src_local = 42 });
    const staged = t.staged.items.len;
    try testing.expectEqual(@as(usize, 5), staged);
    // Consumes the tx: its arena moves into the entry.
    var q = try tx.buildQueuedTx(&t, .forest, 256 * 1024, null);
    defer q.deinit();
    try testing.expectEqual(staged, q.recs.len);
    try testing.expect(q.len <= journal.MAX_ENTRY_LEN);
    try testing.expectEqual(record.RecordKind.delta, q.recs[0].kind);
    try testing.expectEqual(record.RecordKind.delete, q.recs[2].kind);
    try testing.expectEqual(key.FOREST_DENTRY_KEY_LEN, q.recs[3].key.len);
    try testing.expectEqualSlices(u8, &out.new_key, (try key.splitForestKey(q.recs[3].key)).legacy.slice());
    try testing.expectEqual(record.RecordKind.delta, q.recs[4].kind);
}

test "rename (cross dir, regular file): both parents' Δtimes, both SHARED; a directory move rewrites both parents (nlink −1/+1, times) under EXCLUSIVE guards" {
    var t = newTx();
    defer t.deinit();
    const old = pos(1, "a", 0, 42_000, S.IFREG);
    const out = try stageRename(&t, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 2, .new_name = "b", .old_pos = old, .new_pos = null, .src_local = 42 });
    try expectShape(&t, &.{ .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta } });
    _ = try stagedDelta(&t, 0, 1, record.DELTA_TIMES);
    _ = try stagedDelta(&t, 1, 2, record.DELTA_TIMES);
    _ = try stagedDentry(&t, 3, 2, "b");
    try testing.expectEqual(dlm.Mode.shared, out.guards.inodeMode(1).?);
    try testing.expectEqual(dlm.Mode.shared, out.guards.inodeMode(2).?);
    try testing.expect(out.guards.hasDentry(1, "a") and out.guards.hasDentry(2, "b"));

    var t2 = newTx();
    defer t2.deinit();
    const old_dir = pos(1, "d", 0, 60, S.IFDIR);
    const o2 = try stageRename(&t2, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "d", .new_parent = 2, .new_name = "e", .old_pos = old_dir, .new_pos = null, .src_local = 60, .old_parent_value = dirValue(4), .new_parent_value = dirValue(2) });
    try expectShape(&t2, &.{ .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta } });
    const op = try stagedInode(&t2, 0);
    try testing.expectEqual(@as(u64, 1), try key.decodeInodeKey(t2.staged.items[0].key));
    try testing.expectEqual(@as(u32, 3), op.nlink);
    try testing.expectEqual(t_now, op.mtime);
    const np = try stagedInode(&t2, 1);
    try testing.expectEqual(@as(u64, 2), try key.decodeInodeKey(t2.staged.items[1].key));
    try testing.expectEqual(@as(u32, 3), np.nlink);
    try testing.expectEqual(t_now, np.ctime);
    _ = try stagedDelta(&t2, 4, 60, record.DELTA_CTIME);
    try testing.expectEqual(dlm.Mode.exclusive, o2.guards.inodeMode(1).?);
    try testing.expectEqual(dlm.Mode.exclusive, o2.guards.inodeMode(2).?);
    try testing.expectEqual(dlm.Mode.exclusive, o2.guards.inodeMode(60).?);
    try testing.expectEqual(np, (try foldInode(&t2, 2, dirValue(2))).?);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t2));

    // The old parent at the floor: the move's `− 1` is suppressed and counted.
    const before = stats.dir_nlink_underflows.load();
    var t3 = newTx();
    defer t3.deinit();
    _ = try stageRename(&t3, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "d", .new_parent = 2, .new_name = "e", .old_pos = old_dir, .new_pos = null, .old_parent_value = dirValue(2), .new_parent_value = dirValue(2) });
    try testing.expectEqual(@as(u32, 2), (try stagedInode(&t3, 0)).nlink);
    try testing.expectEqual(before + 1, stats.dir_nlink_underflows.load());
    // Unreadable parents on a directory move: nothing staged for them.
    var t4 = newTx();
    defer t4.deinit();
    const o4 = try stageRename(&t4, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "d", .new_parent = 2, .new_name = "e", .old_pos = old_dir, .new_pos = null });
    try testing.expectEqual(@as(usize, 2), t4.staged.items.len);
    try testing.expectEqual(dlm.Mode.exclusive, o4.guards.inodeMode(1).?);
    // A same-directory directory rename: Δtimes only, SHARED.
    var t5 = newTx();
    defer t5.deinit();
    const o5 = try stageRename(&t5, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "d", .new_parent = 1, .new_name = "e", .old_pos = old_dir, .new_pos = null, .src_local = 60 });
    try testing.expectEqual(dlm.Mode.shared, o5.guards.inodeMode(1).?);
    _ = try stagedDelta(&t5, 0, 1, record.DELTA_TIMES);
}

test "rename replacing a target: dest Put (nlink − 1 / dir ⇒ 0, ctime) before the two Deletes; ENOTEMPTY; NOREPLACE; a missing source; rename over a hardlink of itself stamps once" {
    const old = pos(1, "a", 0, 42_000, S.IFREG);
    // The destination sits at coll_seq 2 behind two live same-hash
    // siblings: the re-insert must land in ITS slot, not the chain's head.
    const sib0 = pos(1, "b", 0, 900, S.IFREG);
    const sib1 = pos(1, "b", 1, 901, S.IFREG);
    const new = pos(1, "b", 2, 43_000, S.IFREG);
    const new_chain = [_][]const u8{ &sib0.key, &sib1.key, &new.key };
    var t = newTx();
    defer t.deinit();
    const out = try stageRename(&t, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .old_pos = old, .new_pos = new, .new_chain = &new_chain, .src_local = 42, .dest_local = 43, .dest_value = fileValue(1) });
    try expectShape(&t, &.{ .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta } });
    const dest = try stagedInode(&t, 1);
    try testing.expectEqual(@as(u64, 43), try key.decodeInodeKey(t.staged.items[1].key));
    try testing.expectEqual(@as(u32, 0), dest.nlink);
    try testing.expectEqual(t_now, dest.ctime);
    try testing.expectEqual(@as(u32, 0), out.dest_nlink.?);
    try expectDentryDelete(&t, 2, &new.key);
    try expectDentryDelete(&t, 3, &old.key);
    // The new dentry re-occupies the slot the Delete freed (RYOW probe).
    const nk = try key.DentryKey.decode(t.staged.items[4].key);
    try testing.expectEqual(@as(u8, 2), nk.collSeq());
    try testing.expectEqual(@as(u64, 42_000), (try stagedDentry(&t, 4, 1, "b")).child_ino);
    _ = try stagedDelta(&t, 5, 42, record.DELTA_CTIME);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(43).?);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(42).?);
    try testing.expectEqual(dlm.Mode.shared, out.guards.inodeMode(1).?);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));

    // A hardlinked destination survives with nlink − 1.
    var t2 = newTx();
    defer t2.deinit();
    const o2 = try stageRename(&t2, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .old_pos = old, .new_pos = new, .src_local = 42, .dest_local = 43, .dest_value = fileValue(3) });
    try testing.expectEqual(@as(u32, 2), o2.dest_nlink.?);

    // A directory destination: empty ⇒ nlink 0; populated ⇒ ENOTEMPTY,
    // nothing past the parent's Δ is staged.
    const new_dir = pos(1, "b", 0, 61, S.IFDIR);
    var t3 = newTx();
    defer t3.deinit();
    const o3 = try stageRename(&t3, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .old_pos = pos(1, "a", 0, 60, S.IFDIR), .new_pos = new_dir, .src_local = 60, .dest_local = 61, .dest_value = dirValue(2) });
    try testing.expectEqual(@as(u32, 0), o3.dest_nlink.?);
    var t4 = newTx();
    defer t4.deinit();
    try testing.expectError(error.TargetNotEmpty, stageRename(&t4, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .old_pos = pos(1, "a", 0, 60, S.IFDIR), .new_pos = new_dir, .src_local = 60, .dest_local = 61, .dest_value = dirValue(3), .dest_has_entries = true }));

    var t5 = newTx();
    defer t5.deinit();
    try testing.expectError(error.TargetExists, stageRename(&t5, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .flags = .{ .noreplace = true }, .old_pos = old, .new_pos = new }));
    try testing.expect(t5.isEmpty());
    try testing.expectError(error.SourceNotFound, stageRename(&t5, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .old_pos = null, .new_pos = null }));
    try testing.expect(t5.isEmpty());

    // Rename over a hardlink of itself: the dest Put already stamped the
    // inode — no second Δctime.
    var t6 = newTx();
    defer t6.deinit();
    const o6 = try stageRename(&t6, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .old_pos = old, .new_pos = pos(1, "b", 2, 42_000, S.IFREG), .src_local = 42, .dest_local = 42, .dest_value = fileValue(2) });
    try expectShape(&t6, &.{ .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put } });
    try testing.expectEqual(@as(u32, 1), o6.dest_nlink.?);
    try testing.expectEqual(@as(usize, 2), o6.guards.inodes().len);
    // A destination dentry whose inode is remote (no dest_local): the
    // Delete alone; the remote fragment stamps its inode.
    var t7 = newTx();
    defer t7.deinit();
    _ = try stageRename(&t7, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .old_pos = old, .new_pos = new, .src_local = 42 });
    try expectShape(&t7, &.{ .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta } });
}

test "rename EXCHANGE: two Deletes, two Puts re-occupying the vacated slots, both parents' Δtimes (deduped), both inodes' Δctimes (deduped); flag combos refuse" {
    // Each name sits behind a live same-hash sibling: the swap re-inserts
    // into the slots it vacated (the RYOW probe), not the chains' heads.
    const old_sib = pos(1, "a", 0, 900, S.IFREG);
    const old = pos(1, "a", 1, 42_000, S.IFREG);
    const new_sib = pos(2, "b", 0, 901, S.IFREG);
    const new = pos(2, "b", 1, 60_000, S.IFDIR);
    const old_chain = [_][]const u8{ &old_sib.key, &old.key };
    const new_chain = [_][]const u8{ &new_sib.key, &new.key };
    var t = newTx();
    defer t.deinit();
    const out = try stageRename(&t, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 2, .new_name = "b", .flags = .{ .exchange = true }, .old_pos = old, .new_pos = new, .old_chain = &old_chain, .new_chain = &new_chain, .src_local = 42, .dest_local = 60 });
    try expectShape(&t, &.{ .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .delta } });
    try expectDentryDelete(&t, 0, &old.key);
    try expectDentryDelete(&t, 1, &new.key);
    // (2, b) now names a's child with a's type, in new's own slot.
    try testing.expectEqualSlices(u8, &new.key, t.staged.items[2].key);
    const nb = try stagedDentry(&t, 2, 2, "b");
    try testing.expectEqual(@as(u64, 42_000), nb.child_ino);
    try testing.expectEqual(ftByte(S.IFREG), nb.file_type);
    try testing.expectEqualSlices(u8, &old.key, t.staged.items[3].key);
    const oa = try stagedDentry(&t, 3, 1, "a");
    try testing.expectEqual(@as(u64, 60_000), oa.child_ino);
    try testing.expectEqual(ftByte(S.IFDIR), oa.file_type);
    _ = try stagedDelta(&t, 4, 1, record.DELTA_TIMES);
    _ = try stagedDelta(&t, 5, 2, record.DELTA_TIMES);
    _ = try stagedDelta(&t, 6, 42, record.DELTA_CTIME);
    _ = try stagedDelta(&t, 7, 60, record.DELTA_CTIME);
    try testing.expectEqualSlices(u8, &new.key, &out.new_key);
    try testing.expectEqual(dlm.Mode.shared, out.guards.inodeMode(1).?);
    try testing.expectEqual(dlm.Mode.shared, out.guards.inodeMode(2).?);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(42).?);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(60).?);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));

    // Same parent, same inode (a hardlink pair): one Δtimes, one Δctime.
    var t2 = newTx();
    defer t2.deinit();
    const new1 = pos(1, "b", 0, 42_000, S.IFREG);
    _ = try stageRename(&t2, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .flags = .{ .exchange = true }, .old_pos = old, .new_pos = new1, .src_local = 42, .dest_local = 42 });
    try expectShape(&t2, &.{ .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .delta } });
    _ = try stagedDelta(&t2, 4, 1, record.DELTA_TIMES);
    _ = try stagedDelta(&t2, 5, 42, record.DELTA_CTIME);

    var t3 = newTx();
    defer t3.deinit();
    try testing.expectError(error.SourceNotFound, stageRename(&t3, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .flags = .{ .exchange = true }, .old_pos = old, .new_pos = null }));
    try testing.expectError(error.InvalidFlags, stageRename(&t3, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .flags = .{ .exchange = true, .noreplace = true }, .old_pos = old, .new_pos = new1 }));
    try testing.expectError(error.InvalidFlags, stageRename(&t3, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 1, .new_name = "b", .flags = .{ .exchange = true }, .old_pos = old, .new_pos = new1, .whiteout = .{ .local = 9, .global = 9 } }));
    try testing.expect(t3.isEmpty());
}

test "rename WHITEOUT: the char-0:0 whiteout inode and its dentry at the OLD name (in the slot the Delete freed) ride the same entry" {
    const sib = pos(1, "a", 0, 900, S.IFREG);
    const old = pos(1, "a", 1, 42_000, S.IFREG);
    const old_chain = [_][]const u8{ &sib.key, &old.key };
    var t = newTx();
    defer t.deinit();
    const out = try stageRename(&t, .{ .hash_seed = seed, .now = t_now, .old_parent = 1, .old_name = "a", .new_parent = 2, .new_name = "b", .old_pos = old, .new_pos = null, .old_chain = &old_chain, .src_local = 42, .whiteout = .{ .local = 90, .global = 90_000 } });
    try expectShape(&t, &.{ .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_DENTRIES, .kind = .delete }, .{ .tree = key.TREE_DENTRIES, .kind = .put }, .{ .tree = key.TREE_INODES, .kind = .delta }, .{ .tree = key.TREE_INODES, .kind = .put }, .{ .tree = key.TREE_DENTRIES, .kind = .put } });
    const wv = try stagedInode(&t, 5);
    try testing.expectEqual(@as(u64, 90), try key.decodeInodeKey(t.staged.items[5].key));
    try testing.expectEqual(InodeValue{ .mode = S.IFCHR, .uid = 0, .gid = 0, .nlink = 1, .rdev = 0, .size = 0, .atime = t_now, .mtime = t_now, .ctime = t_now }, wv);
    try testing.expectEqualSlices(u8, &old.key, t.staged.items[6].key);
    const wd = try stagedDentry(&t, 6, 1, "a");
    try testing.expectEqual(@as(u64, 90_000), wd.child_ino);
    try testing.expectEqual(ftByte(S.IFCHR), wd.file_type);
    // The whiteout ino is a fresh mint nobody can reach: not in the plan.
    try testing.expect(out.guards.inodeMode(90) == null);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));
}

test "rename economy tie: one tx per op + the drain's amortization ≤ 1.02 (1.003/op measured); a second entry per op breaks it" {
    try testing.expectEqual(@as(f64, 1.02), RENAME_ENTRIES_PER_OP_GATE);
    try testing.expect(@abs(renameEntriesPerOp(100_000, 300) - 1.003) < 1e-9);
    try testing.expect(renameEntriesPerOp(100_000, 300) <= RENAME_ENTRIES_PER_OP_GATE);
    try testing.expect(renameEntriesPerOp(1000, 20) <= RENAME_ENTRIES_PER_OP_GATE);
    try testing.expect(renameEntriesPerOp(1000, 21) > RENAME_ENTRIES_PER_OP_GATE);
    // The pre-M6 shape: the echo committed as a second entry.
    try testing.expectEqual(@as(f64, 2.0), renameEntriesPerOp(1000, 1000));
}

// ---- setattr + the absorber ----

test "absorber: the echo (times-only, mtime equal, ctime ahead) parks with zero records; served ctime is the echo; a regressive echo absorbs but parks nothing; the fold serves it" {
    var pending = try PendingTimes.init(testing.allocator);
    defer pending.deinit();
    var t = newTx();
    defer t.deinit();
    const stored = fileValue(1);
    const a0 = stats.meta_kv_times_echo_absorbed.load();
    const echo: SetattrReq = .{ .mtime = stored.mtime, .ctime = t_now };
    const out = try stageSetattr(&t, &pending, 42, stored, echo, t_now + 1);
    try testing.expectEqual(SetattrVerdict.absorbed, out.verdict);
    try testing.expect(t.isEmpty());
    try testing.expectEqual(t_now, out.value.ctime);
    try testing.expectEqual(stored.mtime, out.value.mtime);
    try testing.expectEqual(a0 + 1, stats.meta_kv_times_echo_absorbed.load());
    try testing.expectEqual(@as(u64, 1), pending.len());
    try testing.expectEqual(Times{ .mtime = stored.mtime, .ctime = t_now }, pending.parked(42).?);
    try testing.expectEqual(dlm.Mode.exclusive, out.guards.inodeMode(42).?);
    // Every inode read folds it.
    var v = stored;
    pending.fold(42, &v);
    try testing.expectEqual(t_now, v.ctime);
    try testing.expectEqual(stored.mtime, v.mtime);
    // mtime ABSENT is the echo too.
    const o2 = try stageSetattr(&t, &pending, 42, stored, .{ .ctime = t_now + 7 }, t_now);
    try testing.expectEqual(SetattrVerdict.absorbed, o2.verdict);
    try testing.expectEqual(t_now + 7, pending.parked(42).?.ctime);
    // Regressive (the coarse clock behind the in-tx stamp): absorbed,
    // counted, nothing moves backwards.
    const o3 = try stageSetattr(&t, &pending, 42, stored, .{ .mtime = stored.mtime, .ctime = t_now - 3_000_000 }, t_now);
    try testing.expectEqual(SetattrVerdict.absorbed, o3.verdict);
    try testing.expectEqual(t_now + 7, o3.value.ctime);
    try testing.expectEqual(t_now + 7, pending.parked(42).?.ctime);
    try testing.expectEqual(a0 + 3, stats.meta_kv_times_echo_absorbed.load());
    try testing.expect(t.isEmpty());
    // A fresh ino with a regressive echo parks nothing at all.
    const o4 = try stageSetattr(&t, &pending, 43, stored, .{ .ctime = 1 }, t_now);
    try testing.expectEqual(SetattrVerdict.absorbed, o4.verdict);
    try testing.expect(pending.parked(43) == null);
    try testing.expectEqual(@as(u64, 1), pending.len());
}

test "absorber: never blanket suppression — a changed mtime, an explicit utimes (exact, even backwards, with a live refinement), a value field, and a ctime-less times request all stage ONE Put" {
    var pending = try PendingTimes.init(testing.allocator);
    defer pending.deinit();
    const stored = fileValue(1);
    const a0 = stats.meta_kv_times_echo_absorbed.load();
    // (a) the buffered-write flush: mtime CHANGED.
    var t = newTx();
    defer t.deinit();
    const w = try stageSetattr(&t, &pending, 42, stored, .{ .mtime = t_now, .ctime = t_now }, t_now + 9);
    try testing.expectEqual(SetattrVerdict.staged, w.verdict);
    try expectShape(&t, &.{.{ .tree = key.TREE_INODES, .kind = .put }});
    const wv = try stagedInode(&t, 0);
    try testing.expectEqual(t_now, wv.mtime);
    try testing.expectEqual(t_now, wv.ctime);
    try testing.expectEqual(wv, w.value);
    try testing.expectEqual(@as(u64, 1), try entriesOf(&t));
    // (b) explicit utimes with a parked refinement: exact-set, the
    // refinement folds into the RMW base but the explicit values win; the
    // commit then retires the refinement so a dead echo never max-folds.
    _ = try pending.park(42, stored.mtime, t_now + 50);
    var t2 = newTx();
    defer t2.deinit();
    const past = t_now - 86_400_000_000_000;
    const u = try stageSetattr(&t2, &pending, 42, stored, .{ .atime = past, .mtime = past, .ctime = past + 1 }, t_now);
    try testing.expectEqual(SetattrVerdict.staged, u.verdict);
    const uv = try stagedInode(&t2, 0);
    try testing.expectEqual(past, uv.atime);
    try testing.expectEqual(past, uv.mtime);
    try testing.expectEqual(past + 1, uv.ctime);
    try testing.expect(pending.retire(42));
    var served = uv;
    pending.fold(42, &served);
    try testing.expectEqual(past + 1, served.ctime);
    // (c) chmod: the auto-bump is MONOTONE over the folded view — a parked
    // refinement ahead of `now` stands.
    _ = try pending.park(42, stored.mtime, t_now + 500);
    var t3 = newTx();
    defer t3.deinit();
    const c = try stageSetattr(&t3, &pending, 42, stored, .{ .mode = S.IFREG | 0o600 }, t_now);
    try testing.expectEqual(SetattrVerdict.staged, c.verdict);
    const cv = try stagedInode(&t3, 0);
    try testing.expectEqual(S.IFREG | 0o600, cv.mode);
    try testing.expectEqual(t_now + 500, cv.ctime);
    try testing.expect(pending.retire(42));
    // ... and bumps to `now` when nothing is ahead; truncate likewise.
    var t4 = newTx();
    defer t4.deinit();
    const s = try stageSetattr(&t4, &pending, 42, stored, .{ .size = 0 }, t_now);
    try testing.expectEqual(@as(u64, 0), s.value.size);
    try testing.expectEqual(t_now, s.value.ctime);
    // chown with an explicit ctime: verbatim, no bump.
    var t5 = newTx();
    defer t5.deinit();
    const o = try stageSetattr(&t5, &pending, 42, stored, .{ .uid = 7, .gid = 8, .ctime = 99 }, t_now);
    try testing.expectEqual(@as(u32, 7), o.value.uid);
    try testing.expectEqual(@as(u32, 8), o.value.gid);
    try testing.expectEqual(@as(u64, 99), o.value.ctime);
    // (d) times-only with mtime equal but NO ctime: not the echo's shape —
    // it stages (the reference's fallthrough).
    var t6 = newTx();
    defer t6.deinit();
    const n = try stageSetattr(&t6, &pending, 42, stored, .{ .mtime = stored.mtime }, t_now);
    try testing.expectEqual(SetattrVerdict.staged, n.verdict);
    try testing.expectEqual(stored, n.value);
    // atime present with the echo's times: an explicit utimes, stages.
    var t7 = newTx();
    defer t7.deinit();
    const at = try stageSetattr(&t7, &pending, 42, stored, .{ .atime = 1, .mtime = stored.mtime, .ctime = t_now }, t_now);
    try testing.expectEqual(SetattrVerdict.staged, at.verdict);
    try testing.expectEqual(a0, stats.meta_kv_times_echo_absorbed.load());
}

test "pending times: signed monotone max on park and fold (pre-epoch stamps never out-rank), the count gauge, retire, the 512 cap wake" {
    var pending = try PendingTimes.init(testing.allocator);
    defer pending.deinit();
    const g0 = stats.meta_kv_times_echo_pending.load();
    const neg: u64 = @bitCast(@as(i64, -5_000_000_000));
    try testing.expectEqual(Park.parked, try pending.park(1, 100, 200));
    try testing.expectEqual(Park.parked, try pending.park(1, 90, 250));
    try testing.expectEqual(Times{ .mtime = 100, .ctime = 250 }, pending.parked(1).?);
    // A pre-epoch stamp is BEHIND, not huge.
    _ = try pending.park(1, neg, neg);
    try testing.expectEqual(Times{ .mtime = 100, .ctime = 250 }, pending.parked(1).?);
    _ = try pending.park(2, neg, neg);
    try testing.expectEqual(Times{ .mtime = neg, .ctime = neg }, pending.parked(2).?);
    _ = try pending.park(2, 0, 0);
    try testing.expectEqual(Times{ .mtime = 0, .ctime = 0 }, pending.parked(2).?);
    try testing.expect(laterThan(0, neg) and !laterThan(neg, 0) and !laterThan(5, 5));
    try testing.expectEqual(@as(u64, 2), pending.len());
    try testing.expectEqual(g0 + 2, stats.meta_kv_times_echo_pending.load());
    var v: InodeValue = .{ .mtime = 150, .ctime = 300 };
    pending.fold(1, &v);
    try testing.expectEqual(@as(u64, 150), v.mtime);
    try testing.expectEqual(@as(u64, 300), v.ctime);
    v = .{ .mtime = neg, .ctime = 0 };
    pending.fold(1, &v);
    try testing.expectEqual(@as(u64, 100), v.mtime);
    try testing.expectEqual(@as(u64, 250), v.ctime);
    pending.fold(99, &v);
    try testing.expectEqual(@as(u64, 100), v.mtime);
    try testing.expect(pending.retire(1));
    try testing.expect(!pending.retire(1));
    try testing.expectEqual(@as(u64, 1), pending.len());
    try testing.expectEqual(g0 + 1, stats.meta_kv_times_echo_pending.load());
    // The cap: the 512th distinct ino crosses it; re-parks of a parked ino
    // do not grow the population.
    var ino: u64 = 1000;
    while (pending.len() < PENDING_TIMES_DRAIN_CAP - 1) : (ino += 1) {
        try testing.expectEqual(Park.parked, try pending.park(ino, 1, 1));
    }
    try testing.expectEqual(Park.parked, try pending.park(1000, 2, 2));
    try testing.expectEqual(Park.parked_wake_drain, try pending.park(ino, 1, 1));
    try testing.expectEqual(PENDING_TIMES_DRAIN_CAP, pending.len());
    try testing.expectEqual(@as(usize, 128), PENDING_TIMES_DRAIN_BATCH);
    try testing.expectEqual(@as(u64, 512), PENDING_TIMES_DRAIN_CAP);
}

test "drain: batches of ≤ 128 inos, ONE tx each; the advance only (times vs ctime mask); stale and destroyed members stage nothing; durable ≡ served after the drain" {
    const gpa = testing.allocator;
    var pending = try PendingTimes.init(gpa);
    defer pending.deinit();
    // 300 echoes parked over a stored base; one write-path stamp (mtime
    // advances), one stale (a fresher write committed since), one corpse.
    const stored = fileValue(1);
    var stored_by_ino: std.AutoHashMapUnmanaged(u64, ?InodeValue) = .empty;
    defer stored_by_ino.deinit(gpa);
    var ino: u64 = 100;
    while (ino < 400) : (ino += 1) {
        _ = try pending.park(ino, stored.mtime, t_now + ino);
        try stored_by_ino.put(gpa, ino, stored);
    }
    _ = try pending.park(400, t_now, t_now);
    try stored_by_ino.put(gpa, 400, stored);
    _ = try pending.park(401, stored.mtime, t_now);
    var fresher = stored;
    fresher.ctime = t_now + 1_000_000;
    try stored_by_ino.put(gpa, 401, fresher);
    _ = try pending.park(402, stored.mtime, t_now);
    try stored_by_ino.put(gpa, 402, null);
    try testing.expectEqual(@as(u64, 303), pending.len());

    const dc0 = stats.meta_kv_times_echo_drain_commits.load();
    const dr0 = stats.meta_kv_times_echo_drained.load();
    var entries: u64 = 0;
    var staged_total: u64 = 0;
    var gc: u64 = 0;
    var stale: u64 = 0;
    var batches: u64 = 0;
    while (true) {
        var buf: [PENDING_TIMES_DRAIN_BATCH]u64 = undefined;
        const batch = pending.drainBatch(&buf);
        if (batch.len == 0) break;
        batches += 1;
        try testing.expect(batch.len <= PENDING_TIMES_DRAIN_BATCH);
        var t = newTx();
        defer t.deinit();
        var records: u64 = 0;
        for (batch) |i| {
            // The re-read under the guard; the inode as read now.
            const parked_times = pending.parked(i).?;
            const now_stored = stored_by_ino.get(i).?;
            switch (try stageDrainMember(&t, i, parked_times, now_stored)) {
                .staged => records += 1,
                .gc_destroyed => gc += 1,
                .no_advance => stale += 1,
            }
            // The drained record over the stored base equals the served
            // (folded) view — the generic/003 single-authority law.
            if (now_stored) |base| {
                var served = base;
                pending.fold(i, &served);
                const durable = (try foldInode(&t, i, base)).?;
                try testing.expectEqual(served, durable);
            }
        }
        if (!t.isEmpty()) {
            entries += try entriesOf(&t);
            noteDrainCommitted(records);
        }
        for (batch) |i| _ = pending.retire(i);
        staged_total += records;
        if (batch.len < PENDING_TIMES_DRAIN_BATCH) break;
    }
    try testing.expectEqual(@as(u64, 3), batches);
    try testing.expectEqual(@as(u64, 3), entries);
    try testing.expectEqual(@as(u64, 301), staged_total);
    try testing.expectEqual(@as(u64, 1), gc);
    try testing.expectEqual(@as(u64, 1), stale);
    try testing.expectEqual(@as(u64, 0), pending.len());
    try testing.expectEqual(dc0 + 3, stats.meta_kv_times_echo_drain_commits.load());
    try testing.expectEqual(dr0 + 301, stats.meta_kv_times_echo_drained.load());
    // The record shapes: an echo-born refinement is the 10 B Δctime; the
    // write-path stamp the 18 B Δtimes.
    try testing.expectEqual(record.DELTA_CTIME, drainDelta(.{ .mtime = stored.mtime, .ctime = t_now }, stored).?.mask);
    try testing.expectEqual(record.DELTA_TIMES, drainDelta(.{ .mtime = t_now, .ctime = t_now }, stored).?.mask);
    const partial = drainDelta(.{ .mtime = t_now, .ctime = 0 }, stored).?;
    try testing.expectEqual(record.DELTA_TIMES, partial.mask);
    try testing.expectEqual(stored.ctime, partial.fields.ctime);
    try testing.expect(drainDelta(.{ .mtime = stored.mtime, .ctime = stored.ctime }, stored) == null);
    try testing.expect(drainDelta(.{ .mtime = 0, .ctime = 0 }, stored) == null);
    // 300 absorbed echoes cost 3 drain entries: the amortized residual.
    try testing.expect(renameEntriesPerOp(300, 3) <= RENAME_ENTRIES_PER_OP_GATE);
}

// ---- SharedTimes: order-independence under the shared parent guard ----

test "SharedTimes property: two children's creates interleaved in any order fold to both children present, the parent's value fields untouched, and its times = the latest-committed Δ (= max under a monotone clock)" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0x5EED);
    const rnd = prng.random();
    const parent: u64 = 1;
    const base = dirValue(7);
    var round: usize = 0;
    while (round < 200) : (round += 1) {
        var ta = newTx();
        defer ta.deinit();
        var tb = newTx();
        defer tb.deinit();
        const now_a = t_now + rnd.intRangeAtMost(u64, 0, 1000);
        const now_b = t_now + rnd.intRangeAtMost(u64, 0, 1000);
        _ = try stageCreate(&ta, .{ .hash_seed = seed, .now = now_a, .parent = parent, .parent_value = base, .name = "a", .mode = S.IFREG | 0o644, .uid = 1, .gid = 1, .local_ino = 10, .global_ino = 10 });
        _ = try stageCreate(&tb, .{ .hash_seed = seed, .now = now_b, .parent = parent, .parent_value = base, .name = "b", .mode = S.IFREG | 0o644, .uid = 1, .gid = 1, .local_ino = 11, .global_ino = 11 });
        // A random interleaving preserving each tx's own order, seqs by
        // position: the conveyor stamps queue order.
        var merged: std.ArrayList(tx.Staged) = .empty;
        defer merged.deinit(gpa);
        var ia: usize = 0;
        var ib: usize = 0;
        var seq_a: u64 = 0;
        var seq_b: u64 = 0;
        while (ia < ta.staged.items.len or ib < tb.staged.items.len) {
            const take_a = ib == tb.staged.items.len or (ia < ta.staged.items.len and rnd.boolean());
            const s = if (take_a) ta.staged.items[ia] else tb.staged.items[ib];
            if (take_a) ia += 1 else ib += 1;
            try merged.append(gpa, s);
            if (s.tree_id == key.TREE_INODES and s.kind == .delta) {
                if (take_a) seq_a = merged.items.len else seq_b = merged.items.len;
            }
        }
        // Fold the parent key over the merged history.
        var refs: std.ArrayList(record.RecordRef) = .empty;
        defer refs.deinit(gpa);
        var i = merged.items.len;
        const pk = key.inodeKey(parent);
        while (i > 0) {
            i -= 1;
            const s = merged.items[i];
            if (!std.mem.eql(u8, s.key, &pk)) continue;
            try refs.append(gpa, .{ .key = s.key, .seq = i + 1, .kind = s.kind, .value = s.value });
        }
        const base_enc = base.encode();
        try refs.append(gpa, .{ .key = &pk, .seq = 0, .kind = .put, .value = &base_enc });
        var folded = try record.foldNewestFirst(gpa, refs.items);
        defer folded.deinit(gpa);
        const got = try InodeValue.decode(folded.liveValue().?);
        var want = base;
        const latest = if (seq_a > seq_b) now_a else now_b;
        want.mtime = latest;
        want.ctime = latest;
        try testing.expectEqual(want, got);
        // With the coarse clock monotone across commits (the later seq
        // carries the later stamp) latest ≡ max.
        if ((seq_a > seq_b) == (now_a >= now_b)) try testing.expectEqual(@max(now_a, now_b), got.mtime);
        // Both children's records survive the interleaving untouched.
        for ([_]u64{ 10, 11 }) |c| {
            var found = false;
            for (merged.items) |s| {
                if (s.tree_id == key.TREE_INODES and std.mem.eql(u8, s.key, &key.inodeKey(c))) {
                    found = true;
                    try testing.expectEqual(@as(u32, 1), (try InodeValue.decode(s.value)).nlink);
                }
            }
            try testing.expect(found);
        }
    }
}

// ---- the unlink economy over a synthetic burst ----

test "unlink economy burst: N unlink txs + the gathered destroys packed into whole entries + the echoes' drains ≤ 1.05 entries/op (inos.unlinkEntriesPerOp)" {
    const gpa = testing.allocator;
    const N: usize = 1240;
    const fill: usize = 62;
    var pending = try PendingTimes.init(gpa);
    defer pending.deinit();
    var op_entries: u64 = 0;
    var corpses: std.ArrayList(u64) = .empty;
    defer corpses.deinit(gpa);
    var i: u64 = 0;
    while (i < N) : (i += 1) {
        const ino = 1000 + i;
        var t = newTx();
        defer t.deinit();
        const out = try stageUnlink(&t, .{ .now = t_now + i, .parent = 1, .name = "f", .pos = pos(1, "f", 0, ino, S.IFREG), .child = ino, .child_value = fileValue(1), .is_dir = false });
        op_entries += try entriesOf(&t);
        if (out.dying()) try corpses.append(gpa, ino);
        // The kernel's echo on the unlinked inode: absorbed, no entry.
        var te = newTx();
        defer te.deinit();
        const e = try stageSetattr(&te, &pending, ino, out.child, .{ .mtime = out.child.mtime, .ctime = t_now + i + 1 }, t_now + i + 1);
        try testing.expectEqual(SetattrVerdict.absorbed, e.verdict);
        try testing.expect(te.isEmpty());
    }
    try testing.expectEqual(N, op_entries);
    try testing.expectEqual(N, corpses.items.len);
    // The reclaim gather: destroys packed `fill` at a time into whole
    // entries under the payload cap.
    var destroy_entries: u64 = 0;
    var start: usize = 0;
    while (start < corpses.items.len) {
        const end = @min(start + fill, corpses.items.len);
        var payloads: [fill]u64 = @splat(inos.destroyEntryBytes(.flat, 1));
        const chunks = try inos.packDestroyEntries(gpa, payloads[0 .. end - start]);
        defer gpa.free(chunks);
        for (chunks) |c| {
            var t = newTx();
            defer t.deinit();
            for (corpses.items[start + c.start .. start + c.end]) |ino| {
                const xk = key.xattrKey(ino, 1, 0);
                try inos.stageDestroy(&t, ino, &.{&xk});
                _ = pending.retire(ino);
            }
            destroy_entries += try entriesOf(&t);
        }
        start = end;
    }
    try testing.expectEqual(@as(u64, (N + fill - 1) / fill), destroy_entries);
    try testing.expectEqual(@as(u64, 0), pending.len());
    // Two drains over the burst (the cadence): the amortized residual.
    const drains: u64 = 2;
    const per_op = inos.unlinkEntriesPerOp(N, fill, drains);
    const measured = @as(f64, @floatFromInt(op_entries + destroy_entries + drains)) / @as(f64, @floatFromInt(N));
    try testing.expect(@abs(per_op - measured) < 1e-9);
    try testing.expect(per_op <= inos.UNLINK_ENTRIES_PER_OP_GATE);
    // Had every echo committed (the pre-absorber 2.020 shape), the gate
    // breaks by a whole entry.
    try testing.expect(@as(f64, @floatFromInt(op_entries + N + destroy_entries + drains)) / @as(f64, @floatFromInt(N)) > 2.0);
}
