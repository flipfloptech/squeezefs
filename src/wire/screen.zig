//! THE SCREEN: every wire word against durable or derived state BEFORE any
//! effect — "bounded codec = bounded execution" (charter §5.5; `docs/
//! porting/sym_wire_membership_recovery.md` §1.8; `sym_appenders_manager.md`
//! §4.5; `sym_slot_leases.md` §4.7; `sym_data_planes.md` §1.2–§1.3, §3.7;
//! `sym_tokens_intents_striping.md` §2.6, §2.10). The codec proved the
//! frame well-formed; the screen proves its integers name something the
//! durable state can hold. A failed screen is `STATUS_REJECTED` (the
//! buggy/hostile-peer class, counted apart from the must-stay-0 witness
//! refusals), nothing written.
//!
//! Every screen is a PURE function of the payload and a `*Bounds` value
//! the handler reads off its durable/derived state (the heap geometry,
//! the grant record's floors, the census) — so each bound is a line a
//! test can poison, and the fuzz target can drive every screen with
//! arbitrary words and arbitrary bounds. Portable.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const verbs = @import("verbs.zig");
const key = @import("../meta/key.zig");
const node_seq = @import("../meta/node_seq.zig");
const inos = @import("../meta/inos.zig");

/// Why a word was rejected (the log line and the test oracle name it).
pub const Reason = enum {
    volume_out_of_width,
    identity_zero,
    identity_not_peer,
    run_empty,
    run_overflow,
    run_past_total,
    seq_floor_not_above_floor,
    seq_floor_past_bound,
    cursor_below_recorded,
    cursor_past_guest_base,
    extents_past_total,
    root_misaligned,
    root_outside_heap,
    tail_misaligned,
    tail_outside_heap,
    slot_out_of_width,
    want_past_cap,
    offer_to_self,
    offer_to_none,
    not_volume_zero,
    vol_tag_unknown,
    block_past_capacity,
    len_zero,
    blocks_zero,
    blocks_past_known,
    home_vol_out_of_width,
    bitmap_ref_vol_out_of_width,
    bitmap_ref_empty,
    bitmap_ref_overflow,
    bitmap_ref_misaligned,
    bitmap_ref_outside_heap,
    bitmap_too_small,
    bitmap_too_large,
    death_own_identity,
    death_member_live,
    death_key_contradicts,
    member_id_zero,
    member_id_tail_nonzero,
    window_range_empty,
    window_range_past_blocks,
    window_not_ascending,
    object_zero,
    wait_past_t_owner,
    step_unused_word,
    step_ino_zero,
    guard_ino_zero,
    value_past_record_cap,
    node_seq_base_below_base,
    node_seq_base_off_stride,
    node_seq_base_manager_space,
    node_seq_base_past_capacity,
};

pub const Verdict = union(enum) {
    ok,
    rejected: Reason,

    pub fn isOk(self: Verdict) bool {
        return self == .ok;
    }
};

fn reject(r: Reason) Verdict {
    return .{ .rejected = r };
}

// ---------------------------------------------------------------------------
// Constants, with WHY.
// ---------------------------------------------------------------------------

/// A seq frontier past this is a corrupt word, never a real ring: the
/// record-seq space starts at 0 and advances one per record, so half of
/// u64 is 2^63 records — more than any ring family could stamp.
pub const SEQ_FRONTIER_SANE_MAX: u64 = std.math.maxInt(u64) / 2;
/// `M = clamp(W / (2 × writers_known), 1, 64)` rotor slots at join;
/// an over-cap rotor may ask up to `2M` (charter §5.5) — so the widest
/// honest `AcquireSlots.want` is 128.
pub const SLOTS_WANT_MAX: u16 = 128;
/// The set's routing width (u16 slot ids; charter §5.4).
pub const ROUTING_WIDTH: u64 = inos.ROUTING_WIDTH;
/// A guest cursor lives below the guest namespace base (`key.GUEST_NS_BASE`).
pub const CURSOR_MAX: u64 = key.GUEST_NS_BASE;
/// `T_owner` (the membership lease): the longest any wire park may be
/// asked to stand (`claim.STALE_TTL_S × 1000`; a `wait_ms` past it names
/// a lease that has already expired).
pub const T_OWNER_MS: u32 = 45_000;

// ---------------------------------------------------------------------------
// Bounds the handlers read off durable/derived state.
// ---------------------------------------------------------------------------

/// The metadata heap of one volume: node-aligned addresses in `[start, end)`.
pub const HeapBounds = struct {
    start: u64,
    end: u64,
    node_size: u32,

    pub fn holdsNode(self: HeapBounds, addr: u64) Verdict {
        if (addr % self.node_size != 0) return reject(.root_misaligned);
        if (addr < self.start or addr >= self.end) return reject(.root_outside_heap);
        return .ok;
    }
};

/// A data volume the set knows, with its durable block count.
pub const DataVolumeBounds = struct { vol_tag: u64, blocks: u64 };

pub fn dataVolume(known: []const DataVolumeBounds, vol_tag: u64) ?DataVolumeBounds {
    for (known) |d| if (d.vol_tag == vol_tag) return d;
    return null;
}

/// The release/publication bounds of one slot under one appender's grant
/// (`ReleaseWordBounds`, A:1450-1477).
pub const SlotWordBounds = struct {
    /// The grant's recorded floor: a release's `seq_floor` is STRICTLY above it.
    granted_floor: u64,
    /// `min(max(page.seq_offset, granted_floor_max + 1) + head_hint + 2 × ring_len, SEQ_FRONTIER_SANE_MAX)`:
    /// the newest durable page is at most one checkpoint stale; a head
    /// advances at most one ring length past the tail its page declared.
    release_seq_floor_bound: u64,
    recorded_cursor: u64,
    recorded_root: verbs.RootWire,
    total_extents: u64,
    heap: HeapBounds,
};

/// What `RecordDeath`'s key screen needs (REC:353-370, pure).
pub const DeathBounds = struct {
    own: verbs.WireIdentity,
    listed_live: bool,
    /// The key the census REGISTERED for the member (null = unvalidated:
    /// "the tail scan is the fence", the word is kept and counted).
    registered_key: ?u64,
    /// Keys this process or a live member holds.
    held_keys: []const u64,
};

// ---------------------------------------------------------------------------
// The screens.
// ---------------------------------------------------------------------------

/// A frame naming an ordinal the set lacks is `STATUS_NOT_MANAGER`; the
/// screen's word is the same check for payload-internal ordinals.
pub fn volume(ordinal: u16, width: u16) Verdict {
    return if (ordinal < width) .ok else reject(.volume_out_of_width);
}

/// "An appender speaks for itself alone" (MGR:1383-1403): the identity
/// word must derive the authenticated peer. `strict` (PublishEndpoint,
/// LeaveAppender) refuses when no member-id peer is present; a join from
/// a not-yet-member peer binds nothing.
pub fn identityPeer(id: verbs.WireIdentity, peer: ?verbs.WireIdentity, strict: bool) Verdict {
    if (id.isZero()) return reject(.identity_zero);
    if (peer) |p| {
        if (!p.sameMember(id)) return reject(.identity_not_peer);
    } else if (strict) return reject(.identity_not_peer);
    return .ok;
}

/// `validate_return_runs` (A:1094-1108): every run non-empty, no
/// overflow, inside `total_extents`; the first offender is the verdict.
pub fn returnRuns(runs: []align(1) const verbs.GrantRunWire, total_extents: u64) Verdict {
    for (runs) |r| {
        if (r.len == 0) return reject(.run_empty);
        const end = std.math.add(u64, r.start, r.len) catch return reject(.run_overflow);
        if (end > total_extents) return reject(.run_past_total);
    }
    return .ok;
}

fn rootWord(root: verbs.RootWire, b: *const SlotWordBounds) Verdict {
    if (root.addr == b.recorded_root.addr and root.seq == b.recorded_root.seq) return .ok;
    return b.heap.holdsNode(root.addr);
}

/// `screen_release_words` (A:1556-1597): the seq floor strictly above the
/// grant's and under the derived bound; the cursor between the recorded
/// one and the guest base; extents under the total; the root verbatim or
/// a node-aligned heap address (the extent-held and node-decode witnesses
/// are the handler's I/O); every tail a node-aligned heap address.
pub fn releaseWords(words: verbs.SlotWordsWire, tails: []align(1) const verbs.LeafTailWire, b: *const SlotWordBounds) Verdict {
    if (words.seq_floor <= b.granted_floor) return reject(.seq_floor_not_above_floor);
    if (words.seq_floor > @min(b.release_seq_floor_bound, SEQ_FRONTIER_SANE_MAX)) return reject(.seq_floor_past_bound);
    if (words.cursor < b.recorded_cursor) return reject(.cursor_below_recorded);
    if (words.cursor > CURSOR_MAX) return reject(.cursor_past_guest_base);
    if (words.slot_tree_extents > b.total_extents) return reject(.extents_past_total);
    switch (rootWord(words.root, b)) {
        .ok => {},
        .rejected => |r| return reject(r),
    }
    for (tails) |t| {
        if (t.leaf_addr % b.heap.node_size != 0) return reject(.tail_misaligned);
        if (t.leaf_addr < b.heap.start or t.leaf_addr >= b.heap.end) return reject(.tail_outside_heap);
    }
    return .ok;
}

/// `screen_publish_root_words` (A:1599-1638): the release bounds MINUS
/// the seq-floor clauses — a publication moves no floor.
pub fn publishRoot(root: verbs.SlotRootWire, b: *const SlotWordBounds) Verdict {
    if (root.slot >= ROUTING_WIDTH) return reject(.slot_out_of_width);
    if (root.cursor < b.recorded_cursor) return reject(.cursor_below_recorded);
    if (root.cursor > CURSOR_MAX) return reject(.cursor_past_guest_base);
    if (root.slot_tree_extents > b.total_extents) return reject(.extents_past_total);
    return rootWord(root.root, b);
}

pub fn slot(s: u16) Verdict {
    return if (s < ROUTING_WIDTH) .ok else reject(.slot_out_of_width);
}

/// `AcquireSlots.want`: 0 = the derived `M`; explicit ≤ `2M`'s ceiling.
pub fn acquireWant(want: u16) Verdict {
    return if (want <= SLOTS_WANT_MAX) .ok else reject(.want_past_cap);
}

/// An offer's `to` must be a known appender other than the holder; 0 is
/// the manager's own id and never an offeree.
pub fn offer(req: verbs.OfferSlotReq) Verdict {
    if (req.to == 0) return reject(.offer_to_none);
    if (req.to == req.appender_id) return reject(.offer_to_self);
    return slot(req.slot);
}

/// The `dir_rename` lease is volume 0's manager's alone.
pub fn dirRename(req: verbs.DirRenameReq) Verdict {
    return if (req.volume == 0) .ok else reject(.not_volume_zero);
}

fn blockInVolume(block_idx: u64, dv: ?DataVolumeBounds) Verdict {
    const d = dv orelse return reject(.vol_tag_unknown);
    return if (block_idx < d.blocks) .ok else reject(.block_past_capacity);
}

pub fn markShared(req: verbs.MarkSharedReq, dv: ?DataVolumeBounds) Verdict {
    return blockInVolume(req.block_idx, dv);
}

pub fn shareBlock(req: verbs.ShareBlockReq, dv: ?DataVolumeBounds) Verdict {
    return blockInVolume(req.head.block_idx, dv);
}

pub fn releaseShared(req: verbs.ReleaseSharedReq, dv: ?DataVolumeBounds) Verdict {
    return blockInVolume(req.block_idx, dv);
}

/// `held_unconsumed` is a claim about the writer's own window: it can
/// never exceed the volume.
pub fn blockGrant(req: verbs.BlockGrantReq, dv: ?DataVolumeBounds) Verdict {
    const d = dv orelse return reject(.vol_tag_unknown);
    if (req.writer.isZero()) return reject(.identity_zero);
    if (req.held_unconsumed > d.blocks) return reject(.block_past_capacity);
    return .ok;
}

pub fn returnBlocks(req: verbs.ReturnBlocksReq, dv: ?DataVolumeBounds) Verdict {
    const d = dv orelse return reject(.vol_tag_unknown);
    if (req.len == 0) return reject(.len_zero);
    const end = std.math.add(u64, req.start, req.len) catch return reject(.run_overflow);
    if (end > d.blocks) return reject(.block_past_capacity);
    return .ok;
}

/// `screen_alloc_lease_acquire` (AL:1462-1492): a registered data volume,
/// `0 < blocks ≤ known` ("a smaller ask is a smaller bitmap over the same
/// volume"), `home_vol < set width`.
pub fn allocLeaseAcquire(req: verbs.AllocLeaseAcquireReq, width: u16, dv: ?DataVolumeBounds) Verdict {
    const d = dv orelse return reject(.vol_tag_unknown);
    if (req.identity.isZero()) return reject(.identity_zero);
    if (req.blocks == 0) return reject(.blocks_zero);
    if (req.blocks > d.blocks) return reject(.blocks_past_known);
    if (req.home_vol >= width) return reject(.home_vol_out_of_width);
    return .ok;
}

/// The bitmap region a volume of `blocks` needs: one bit per block.
pub fn bitmapRegionLen(blocks: u64) u64 {
    return (blocks + 7) / 8;
}

/// `screen_bitmap_refs` (AL:2598-2656): every ref names a volume the set
/// has, lies inside that volume's heap, starts and runs a whole number of
/// nodes, is non-empty, no overflow; `Σ len ≥ region` and `≤ ceil(region
/// / node) × node`.
pub fn bitmapRefs(refs: []align(1) const verbs.BitmapRefWire, blocks: u64, heaps: []const HeapBounds) Verdict {
    var sum: u64 = 0;
    var node: u64 = 0;
    for (refs) |r| {
        if (r.vol >= heaps.len) return reject(.bitmap_ref_vol_out_of_width);
        const h = heaps[r.vol];
        if (r.len == 0) return reject(.bitmap_ref_empty);
        const end = std.math.add(u64, r.start, r.len) catch return reject(.bitmap_ref_overflow);
        if (r.start % h.node_size != 0 or r.len % h.node_size != 0) return reject(.bitmap_ref_misaligned);
        if (r.start < h.start or end > h.end) return reject(.bitmap_ref_outside_heap);
        sum = std.math.add(u64, sum, r.len) catch return reject(.bitmap_ref_overflow);
        node = @max(node, h.node_size);
    }
    const region = bitmapRegionLen(blocks);
    if (sum < region) return reject(.bitmap_too_small);
    if (node != 0 and sum > ((region + node - 1) / node) * node) return reject(.bitmap_too_large);
    return .ok;
}

/// `screen_record_death` (REC:581-634): never this mount's own identity,
/// never a member the installed owner lists LIVE, never a key this
/// process or a live member holds or one contradicting the registered key
/// (0 and the registered key are accepted; an unregistered member's word
/// is accepted and the handler counts it as unvalidated).
pub fn recordDeath(req: verbs.RecordDeathReq, b: *const DeathBounds) Verdict {
    if (req.member.isZero()) return reject(.identity_zero);
    if (req.member.sameMember(b.own)) return reject(.death_own_identity);
    if (b.listed_live) return reject(.death_member_live);
    if (req.pr_key != 0) {
        for (b.held_keys) |k| if (k == req.pr_key) return reject(.death_key_contradicts);
        if (b.registered_key) |rk| if (rk != req.pr_key) return reject(.death_key_contradicts);
    }
    return .ok;
}

pub fn recordRecovered(req: verbs.RecordRecoveredReq, width: u16) Verdict {
    if (req.member.isZero()) return reject(.identity_zero);
    return volume(req.vol, width);
}

/// A member id is never all-zero; a WRITER's id is the 12 B peer id with
/// a zero tail (a reader's is 16 random bytes).
pub fn memberId(id: [16]u8, role: verbs.Role) Verdict {
    if (std.mem.allEqual(u8, &id, 0)) return reject(.member_id_zero);
    if (role == .writer and !std.mem.allEqual(u8, id[12..], 0)) return reject(.member_id_tail_nonzero);
    return .ok;
}

pub fn join(req: verbs.JoinReq) Verdict {
    return memberId(req.head.id, @fromBackingInt(req.head.role));
}

/// `screen_window_decl` (BG:122-148): every declared range non-empty,
/// inside `[0, blocks)` judged without wrap, ascending and disjoint; one
/// bad range refuses the WHOLE word.
pub fn windowDecl(decl: verbs.WindowDecl, known: []const DataVolumeBounds) Verdict {
    const d = dataVolume(known, decl.vol_tag) orelse return reject(.vol_tag_unknown);
    var prev_end: u64 = 0;
    for (decl.ranges, 0..) |r, i| {
        if (r.len == 0) return reject(.window_range_empty);
        const end = std.math.add(u64, r.start, r.len) catch return reject(.window_range_past_blocks);
        if (end > d.blocks) return reject(.window_range_past_blocks);
        if (i != 0 and r.start < prev_end) return reject(.window_not_ascending);
        prev_end = end;
    }
    return .ok;
}

pub fn renew(req: verbs.RenewReq, known: []const DataVolumeBounds) Verdict {
    if (std.mem.allEqual(u8, &req.head.id, 0)) return reject(.member_id_zero);
    var it = req.windows();
    while (it.next()) |w| switch (windowDecl(w, known)) {
        .ok => {},
        .rejected => |r| return reject(r),
    };
    return .ok;
}

/// Objects are local key inos ≥ 1 of the frame's volume.
pub fn tokenGrant(req: verbs.TokenGrantReq) Verdict {
    return if (req.head.object == 0) reject(.object_zero) else .ok;
}

/// A pushed recall frame names local key inos ≥ 1 (the codec bounded its
/// count at the batch cap).
pub fn tokenRecall(req: verbs.TokenRecallReq) Verdict {
    for (req.items) |o| if (o == 0) return reject(.object_zero);
    return .ok;
}

pub fn tokenRelease(req: verbs.TokenReleaseReq) Verdict {
    for (req.items) |o| if (o == 0) return reject(.object_zero);
    return .ok;
}

pub fn custodyGrant(req: verbs.CustodyGrantReq) Verdict {
    if (req.object == 0) return reject(.object_zero);
    if (req.wait_ms > T_OWNER_MS) return reject(.wait_past_t_owner);
    return .ok;
}

/// Every unused word of a step is zero for its kind (the intent record's
/// own law, `sym_tokens_intents_striping.md` §2.1), and every ino word is
/// a GLOBAL ino ≥ 1.
pub fn xvStep(step: verbs.XvStepWire) Verdict {
    const k: verbs.StepKind = @fromBackingInt(step.kind);
    const words_zero = step.w0 == 0 and step.w1 == 0 and step.w2 == 0 and step.w3 == 0;
    if (step.a == 0) return reject(.step_ino_zero);
    switch (k) {
        .remove_dentry => if (step.c != 0 or step.ft_bits != 0 or !words_zero or step.flags != 0) return reject(.step_unused_word),
        .insert_dentry => {
            if (step.b == 0) return reject(.step_ino_zero);
            if (step.c != 0 or !words_zero or step.flags != 0) return reject(.step_unused_word);
        },
        .set_nlink => {
            if (step.b != 0 or step.ft_bits != 0 or step.w2 != 0 or step.w3 != 0) return reject(.step_unused_word);
            if (step.flags & verbs.STEP_FLAG_HAS_CTIME == 0 and step.c != 0) return reject(.step_unused_word);
        },
        .touch_ctime => if (step.b != 0 or step.ft_bits != 0 or !words_zero or step.flags != 0) return reject(.step_unused_word),
        .mint_inode => if (step.b != 0 or step.c != 0 or step.ft_bits != 0 or step.flags != 0) return reject(.step_unused_word),
        .create_inode => if (step.ft_bits != 0 or step.flags != 0) return reject(.step_unused_word),
    }
    return .ok;
}

pub fn xvGuards(req: verbs.XvGuardsReq) Verdict {
    for (req.inodes) |g| if (g.ino == 0) return reject(.guard_ino_zero);
    var it = req.dentries();
    while (it.next()) |d| if (d.head.parent == 0) return reject(.guard_ino_zero);
    return .ok;
}

pub fn lookupExact(req: verbs.LookupExactReq) Verdict {
    return if (req.head.parent == 0) reject(.object_zero) else .ok;
}

/// The shipped value is bounded by THIS volume's record cap (`min(64 KiB,
/// node_size/4)` minus framing), not the codec's widest.
pub fn recordShip(req: verbs.RecordShipReq, record_value_cap: u32) Verdict {
    if (req.head.ino == 0) return reject(.object_zero);
    if (req.value.len > record_value_cap) return reject(.value_past_record_cap);
    return .ok;
}

pub fn freeBlocks(req: verbs.FreeBlocksReq, dv: ?DataVolumeBounds) Verdict {
    const d = dv orelse return reject(.vol_tag_unknown);
    for (req.items) |blk| if (blk >= d.blocks) return reject(.block_past_capacity);
    return .ok;
}

/// The JOINER's screen of a `Joined` reply (`screen_incarnation_base`,
/// JN:33-38; `joined_wire_words_rejected` must-stay-0): the base is the
/// start of incarnation `o ≥ 1` over the volume's base `B` and inside the
/// volume's capacity.
pub fn joinedNodeSeqBase(base: u64, volume_base: u64) Verdict {
    if (base < volume_base) return reject(.node_seq_base_below_base);
    const off = base - volume_base;
    if (off % node_seq.INCARNATION_SPACE != 0) return reject(.node_seq_base_off_stride);
    const ordinal = off / node_seq.INCARNATION_SPACE;
    if (ordinal == 0) return reject(.node_seq_base_manager_space);
    if (node_seq.incarnationBase(volume_base, ordinal) == null) return reject(.node_seq_base_past_capacity);
    return .ok;
}

// ---------------------------------------------------------------------------
// Tests: each bound's refusal, by name.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn expectRejected(expected: Reason, v: Verdict) !void {
    switch (v) {
        .ok => return error.TestExpectedRejection,
        .rejected => |r| try testing.expectEqual(expected, r),
    }
}

const heap: HeapBounds = .{ .start = 1 << 20, .end = 1 << 30, .node_size = 65536 };
const me: verbs.WireIdentity = .{ .node_token = 1, .writer_id = 10, .mount_slot = 0, .reserved = 0 };
const other: verbs.WireIdentity = .{ .node_token = 2, .writer_id = 20, .mount_slot = 0, .reserved = 0 };

test "volume, identity-peer binding, return runs" {
    try testing.expect(volume(2, 3).isOk());
    try expectRejected(.volume_out_of_width, volume(3, 3));
    try expectRejected(.identity_zero, identityPeer(.zero, null, false));
    try testing.expect(identityPeer(me, null, false).isOk());
    try expectRejected(.identity_not_peer, identityPeer(me, null, true));
    try testing.expect(identityPeer(me, me, true).isOk());
    try expectRejected(.identity_not_peer, identityPeer(me, other, false));
    const good = [_]verbs.GrantRunWire{ .{ .start = 0, .len = 4, .reserved = 0 }, .{ .start = 96, .len = 4, .reserved = 0 } };
    try testing.expect(returnRuns(&good, 100).isOk());
    try expectRejected(.run_past_total, returnRuns(&[_]verbs.GrantRunWire{.{ .start = 97, .len = 4, .reserved = 0 }}, 100));
    try expectRejected(.run_empty, returnRuns(&[_]verbs.GrantRunWire{.{ .start = 1, .len = 0, .reserved = 0 }}, 100));
    try expectRejected(.run_overflow, returnRuns(&[_]verbs.GrantRunWire{.{ .start = std.math.maxInt(u64), .len = 1, .reserved = 0 }}, std.math.maxInt(u64)));
}

test "release words: eight poisoned words each refused by name; the recorded root passes verbatim, a node-aligned heap root passes, anything else refuses" {
    const b: SlotWordBounds = .{ .granted_floor = 100, .release_seq_floor_bound = 10_000, .recorded_cursor = 50, .recorded_root = .{ .addr = 12345, .seq = 7 }, .total_extents = 1000, .heap = heap };
    const ok_words: verbs.SlotWordsWire = .{ .root = .{ .addr = 12345, .seq = 7 }, .cursor = 60, .seq_floor = 101, .slot_tree_extents = 10, .reserved = 0 };
    try testing.expect(releaseWords(ok_words, &.{}, &b).isOk());
    var w = ok_words;
    w.seq_floor = 100;
    try expectRejected(.seq_floor_not_above_floor, releaseWords(w, &.{}, &b));
    w = ok_words;
    w.seq_floor = 10_001;
    try expectRejected(.seq_floor_past_bound, releaseWords(w, &.{}, &b));
    w = ok_words;
    w.seq_floor = std.math.maxInt(u64) - 1; // the charter's "a bad seq_floor near u64::MAX"
    try expectRejected(.seq_floor_past_bound, releaseWords(w, &.{}, &b));
    w = ok_words;
    w.cursor = 49;
    try expectRejected(.cursor_below_recorded, releaseWords(w, &.{}, &b));
    w = ok_words;
    w.cursor = CURSOR_MAX + 1;
    try expectRejected(.cursor_past_guest_base, releaseWords(w, &.{}, &b));
    w = ok_words;
    w.slot_tree_extents = 1001;
    try expectRejected(.extents_past_total, releaseWords(w, &.{}, &b));
    w = ok_words;
    w.root = .{ .addr = (1 << 20) + 65536, .seq = 9 }; // a bogus-but-aligned heap root: the I/O witness is the handler's
    try testing.expect(releaseWords(w, &.{}, &b).isOk());
    w.root.addr += 1;
    try expectRejected(.root_misaligned, releaseWords(w, &.{}, &b));
    w.root.addr = 1 << 30;
    try expectRejected(.root_outside_heap, releaseWords(w, &.{}, &b));
    const tails_bad = [_]verbs.LeafTailWire{.{ .leaf_addr = (1 << 20) + 1, .tail = 0, .reserved = 0 }};
    try expectRejected(.tail_misaligned, releaseWords(ok_words, &tails_bad, &b));
    const tails_out = [_]verbs.LeafTailWire{.{ .leaf_addr = 0, .tail = 0, .reserved = 0 }};
    try expectRejected(.tail_outside_heap, releaseWords(ok_words, &tails_out, &b));
    // A bound past the sane cap is clamped to it.
    const b2: SlotWordBounds = .{ .granted_floor = 0, .release_seq_floor_bound = std.math.maxInt(u64), .recorded_cursor = 0, .recorded_root = .{ .addr = 0, .seq = 0 }, .total_extents = 1, .heap = heap };
    w = ok_words;
    w.seq_floor = SEQ_FRONTIER_SANE_MAX + 1;
    try expectRejected(.seq_floor_past_bound, releaseWords(w, &.{}, &b2));
    // Publication: the same minus the floor clauses; a slot past W refuses.
    const root: verbs.SlotRootWire = .{ .root = .{ .addr = 12345, .seq = 7 }, .cursor = 60, .slot_tree_extents = 10, .g = 1, .slot = 5, .reserved = @splat(0) };
    try testing.expect(publishRoot(root, &b).isOk());
    var r2 = root;
    r2.cursor = 1;
    try expectRejected(.cursor_below_recorded, publishRoot(r2, &b));
}

test "slots, offers, dir_rename, the clone and allocation bounds" {
    try testing.expect(slot(65535).isOk());
    try testing.expect(acquireWant(128).isOk());
    try expectRejected(.want_past_cap, acquireWant(129));
    try expectRejected(.offer_to_self, offer(.{ .volume = 0, .slot = 1, .appender_id = 3, .to = 3, .reserved = 0 }));
    try expectRejected(.offer_to_none, offer(.{ .volume = 0, .slot = 1, .appender_id = 3, .to = 0, .reserved = 0 }));
    try testing.expect(offer(.{ .volume = 0, .slot = 1, .appender_id = 3, .to = 4, .reserved = 0 }).isOk());
    try expectRejected(.not_volume_zero, dirRename(.{ .volume = 1, .reserved = 0, .appender_id = 3 }));
    try testing.expect(dirRename(.{ .volume = 0, .reserved = 0, .appender_id = 3 }).isOk());
    const dvs = [_]DataVolumeBounds{.{ .vol_tag = 11, .blocks = 1000 }};
    try testing.expect(markShared(.{ .volume = 0, .reserved = 0, .block_index = 0, .vol_tag = 11, .block_idx = 999, .owner_ino = 5 }, dataVolume(&dvs, 11)).isOk());
    try expectRejected(.block_past_capacity, markShared(.{ .volume = 0, .reserved = 0, .block_index = 0, .vol_tag = 11, .block_idx = 1000, .owner_ino = 5 }, dataVolume(&dvs, 11)));
    try expectRejected(.vol_tag_unknown, markShared(.{ .volume = 0, .reserved = 0, .block_index = 0, .vol_tag = 12, .block_idx = 0, .owner_ino = 5 }, dataVolume(&dvs, 12)));
    try expectRejected(.block_past_capacity, blockGrant(.{ .volume = 0, .reserved = 0, .want = 0, .writer = me, .vol_tag = 11, .held_unconsumed = 1001 }, dataVolume(&dvs, 11)));
    try expectRejected(.identity_zero, blockGrant(.{ .volume = 0, .reserved = 0, .want = 0, .writer = .zero, .vol_tag = 11, .held_unconsumed = 0 }, dataVolume(&dvs, 11)));
    try expectRejected(.len_zero, returnBlocks(.{ .volume = 0, .reserved = 0, .len = 0, .writer = me, .vol_tag = 11, .start = 0 }, dataVolume(&dvs, 11)));
    try expectRejected(.block_past_capacity, returnBlocks(.{ .volume = 0, .reserved = 0, .len = 2, .writer = me, .vol_tag = 11, .start = 999 }, dataVolume(&dvs, 11)));
    try expectRejected(.run_overflow, returnBlocks(.{ .volume = 0, .reserved = 0, .len = 2, .writer = me, .vol_tag = 11, .start = std.math.maxInt(u64) }, dataVolume(&dvs, 11)));
    const acq: verbs.AllocLeaseAcquireReq = .{ .volume = 0, .home_vol = 1, .appender_id = 3, .identity = me, .vol_tag = 11, .blocks = 1000 };
    try testing.expect(allocLeaseAcquire(acq, 2, dataVolume(&dvs, 11)).isOk());
    var a2 = acq;
    a2.blocks = 0;
    try expectRejected(.blocks_zero, allocLeaseAcquire(a2, 2, dataVolume(&dvs, 11)));
    a2.blocks = 1001;
    try expectRejected(.blocks_past_known, allocLeaseAcquire(a2, 2, dataVolume(&dvs, 11)));
    a2 = acq;
    a2.home_vol = 2;
    try expectRejected(.home_vol_out_of_width, allocLeaseAcquire(a2, 2, dataVolume(&dvs, 11)));
    const frees = [_]u64{ 0, 999 };
    try testing.expect(freeBlocks(.{ .head = .{ .volume = 0, .n = 2, .reserved = 0, .vol_tag = 11, .lease_epoch = 1 }, .items = &frees }, dataVolume(&dvs, 11)).isOk());
    const frees_bad = [_]u64{1000};
    try expectRejected(.block_past_capacity, freeBlocks(.{ .head = .{ .volume = 0, .n = 1, .reserved = 0, .vol_tag = 11, .lease_epoch = 1 }, .items = &frees_bad }, dataVolume(&dvs, 11)));
}

test "bitmap refs: volume, heap, alignment, emptiness, overflow, the region sum's two bounds" {
    const heaps = [_]HeapBounds{ heap, .{ .start = 1 << 20, .end = 1 << 24, .node_size = 4096 } };
    // 1000 blocks ⇒ 125 B region ⇒ one 64 KiB node suffices and is the ceiling.
    const ok = [_]verbs.BitmapRefWire{.{ .start = 1 << 20, .len = 65536, .vol = 0, .reserved = @splat(0) }};
    try testing.expect(bitmapRefs(&ok, 1000, &heaps).isOk());
    try expectRejected(.bitmap_ref_vol_out_of_width, bitmapRefs(&[_]verbs.BitmapRefWire{.{ .start = 1 << 20, .len = 65536, .vol = 2, .reserved = @splat(0) }}, 1000, &heaps));
    try expectRejected(.bitmap_ref_empty, bitmapRefs(&[_]verbs.BitmapRefWire{.{ .start = 1 << 20, .len = 0, .vol = 0, .reserved = @splat(0) }}, 1000, &heaps));
    try expectRejected(.bitmap_ref_misaligned, bitmapRefs(&[_]verbs.BitmapRefWire{.{ .start = (1 << 20) + 1, .len = 65536, .vol = 0, .reserved = @splat(0) }}, 1000, &heaps));
    try expectRejected(.bitmap_ref_outside_heap, bitmapRefs(&[_]verbs.BitmapRefWire{.{ .start = 0, .len = 65536, .vol = 0, .reserved = @splat(0) }}, 1000, &heaps));
    try expectRejected(.bitmap_ref_overflow, bitmapRefs(&[_]verbs.BitmapRefWire{.{ .start = std.math.maxInt(u64) - 65535, .len = 65536, .vol = 0, .reserved = @splat(0) }}, 1000, &heaps));
    try expectRejected(.bitmap_too_small, bitmapRefs(&[_]verbs.BitmapRefWire{.{ .start = 1 << 20, .len = 4096, .vol = 1, .reserved = @splat(0) }}, 1 << 20, &heaps));
    try expectRejected(.bitmap_too_large, bitmapRefs(&[_]verbs.BitmapRefWire{ ok[0], .{ .start = 1 << 21, .len = 65536, .vol = 0, .reserved = @splat(0) } }, 1000, &heaps));
    try expectRejected(.bitmap_too_small, bitmapRefs(&.{}, 8, &heaps));
    try testing.expectEqual(@as(u64, 125), bitmapRegionLen(1000));
    try testing.expectEqual(@as(u64, 1), bitmapRegionLen(1));
}

test "RecordDeath's key screen: own identity, a live member, a held or contradicting key refuse; 0 and the registered key pass; an unregistered member's word passes" {
    const held = [_]u64{ 0xAAAA, 0xBBBB };
    var b: DeathBounds = .{ .own = me, .listed_live = false, .registered_key = 0x1234, .held_keys = &held };
    const req: verbs.RecordDeathReq = .{ .volume = 0, .reserved = 0, .reserved2 = 0, .member = other, .epoch = 1, .pr_key = 0x1234 };
    try testing.expect(recordDeath(req, &b).isOk());
    var r0 = req;
    r0.pr_key = 0;
    try testing.expect(recordDeath(r0, &b).isOk());
    var r1 = req;
    r1.member = me;
    try expectRejected(.death_own_identity, recordDeath(r1, &b));
    b.listed_live = true;
    try expectRejected(.death_member_live, recordDeath(req, &b));
    b.listed_live = false;
    var r2 = req;
    r2.pr_key = 0xAAAA;
    try expectRejected(.death_key_contradicts, recordDeath(r2, &b));
    r2.pr_key = 0x9999;
    try expectRejected(.death_key_contradicts, recordDeath(r2, &b));
    b.registered_key = null;
    try testing.expect(recordDeath(r2, &b).isOk());
    var r3 = req;
    r3.member = .zero;
    try expectRejected(.identity_zero, recordDeath(r3, &b));
    try expectRejected(.volume_out_of_width, recordRecovered(.{ .volume = 0, .vol = 3, .reserved = 0, .member = other }, 3));
}

test "membership: member ids, the renewal's window declarations" {
    var id: [16]u8 = @splat(0);
    try expectRejected(.member_id_zero, memberId(id, .writer));
    @memcpy(id[0..12], &me.peerId());
    try testing.expect(memberId(id, .writer).isOk());
    id[15] = 1;
    try expectRejected(.member_id_tail_nonzero, memberId(id, .writer));
    try testing.expect(memberId(id, .reader).isOk());
    const known = [_]DataVolumeBounds{.{ .vol_tag = 11, .blocks = 1000 }};
    const asc = [_]verbs.GrantRunWire{ .{ .start = 0, .len = 10, .reserved = 0 }, .{ .start = 10, .len = 10, .reserved = 0 } };
    try testing.expect(windowDecl(.{ .vol_tag = 11, .ranges = &asc }, &known).isOk());
    try expectRejected(.vol_tag_unknown, windowDecl(.{ .vol_tag = 12, .ranges = &asc }, &known));
    const overlap = [_]verbs.GrantRunWire{ .{ .start = 0, .len = 10, .reserved = 0 }, .{ .start = 9, .len = 10, .reserved = 0 } };
    try expectRejected(.window_not_ascending, windowDecl(.{ .vol_tag = 11, .ranges = &overlap }, &known));
    const empty = [_]verbs.GrantRunWire{.{ .start = 0, .len = 0, .reserved = 0 }};
    try expectRejected(.window_range_empty, windowDecl(.{ .vol_tag = 11, .ranges = &empty }, &known));
    const past = [_]verbs.GrantRunWire{.{ .start = 999, .len = 2, .reserved = 0 }};
    try expectRejected(.window_range_past_blocks, windowDecl(.{ .vol_tag = 11, .ranges = &past }, &known));
    const wrap = [_]verbs.GrantRunWire{.{ .start = std.math.maxInt(u64), .len = 2, .reserved = 0 }};
    try expectRejected(.window_range_past_blocks, windowDecl(.{ .vol_tag = 11, .ranges = &wrap }, &known));
    // Through the decoded renewal frame.
    var wbuf: [256]u8 = undefined;
    var wb: verbs.Builder = .init(&wbuf);
    try verbs.RenewReq.encodeWindows(&wb, &.{.{ .vol_tag = 11, .ranges = &overlap }});
    var fbuf: [512]u8 = undefined;
    var fb: verbs.Builder = .init(&fbuf);
    try (verbs.RenewReq{ .head = .{ .id = id, .epoch = 1, .acked_free_epoch = 0, .n_windows = 1, .reserved = @splat(0) }, .windows_bytes = wb.written() }).encode(&fb);
    const decoded = try verbs.decodeRequest(.renew, fb.written());
    try expectRejected(.window_not_ascending, renew(decoded.renew, &known));
}

test "tokens, steps, guards, the record ship" {
    try expectRejected(.object_zero, tokenGrant(.{ .head = .{ .volume = 0, .wants = 0, .mode = 0, .xattr_after_len = 0, .client_id = 1, .object = 0, .after = 0 }, .bytes = "" }));
    const objs = [_]u64{ 1, 0 };
    try expectRejected(.object_zero, tokenRecall(.{ .head = .{ .volume = 0, .reserved = 0, .n = 2, .frame_id = 1, .holder = 7 }, .items = &objs }));
    try testing.expect(tokenRecall(.{ .head = .{ .volume = 0, .reserved = 0, .n = 1, .frame_id = 1, .holder = 7 }, .items = objs[0..1] }).isOk());
    try expectRejected(.object_zero, tokenRelease(.{ .head = .{ .volume = 0, .n = 2, .reserved = 0, .client_id = 1 }, .items = &objs }));
    try expectRejected(.object_zero, custodyGrant(.{ .volume = 0, .concurrent_write = 0, .reserved = 0, .wait_ms = 0, .client_id = 1, .object = 0, .span_start = 0, .span_len = 0, .lease_epoch = 0, .reserved2 = 0 }));
    const insert: verbs.XvStepWire = .{ .kind = 2, .parent_update = 1, .name_len = 1, .flags = 0, .ft_bits = 0o100000, .a = 5, .b = 6, .c = 0, .w0 = 0, .w1 = 0, .w2 = 0, .w3 = 0 };
    try testing.expect(xvStep(insert).isOk());
    var s = insert;
    s.c = 1;
    try expectRejected(.step_unused_word, xvStep(s));
    s = insert;
    s.b = 0;
    try expectRejected(.step_ino_zero, xvStep(s));
    const nlink: verbs.XvStepWire = .{ .kind = 3, .parent_update = 0, .name_len = 0, .flags = 0, .ft_bits = 0, .a = 5, .b = 0, .c = 99, .w0 = 2, .w1 = 1, .w2 = 0, .w3 = 0 };
    try expectRejected(.step_unused_word, xvStep(nlink)); // a ctime without the flag
    var n2 = nlink;
    n2.flags = verbs.STEP_FLAG_HAS_CTIME;
    try testing.expect(xvStep(n2).isOk());
    const create: verbs.XvStepWire = .{ .kind = 6, .parent_update = 0, .name_len = 0, .flags = 0, .ft_bits = 0, .a = 5, .b = 10, .c = 1000, .w0 = 0o644, .w1 = 1, .w2 = 1, .w3 = 0 };
    try testing.expect(xvStep(create).isOk());
    const g0 = [_]verbs.GuardInodeWire{.{ .ino = 0, .exclusive = 0, .reserved = @splat(0) }};
    try expectRejected(.guard_ino_zero, xvGuards(.{ .head = .{ .volume = 0, .n_inodes = 1, .n_dentries = 0, .reserved = 0, .scope = 5 }, .inodes = &g0, .dentries_bytes = "" }));
    try expectRejected(.object_zero, lookupExact(.{ .head = .{ .volume = 0, .name_len = 1, .reserved = 0, .reserved2 = 0, .parent = 0 }, .bytes = "a" }));
    const ship: verbs.RecordShipReq = .{ .head = .{ .volume = 0, .kind = 2, .reserved = 0, .name_len = 1, .n_proofs = 0, .value_len = 5, .reserved3 = 0, .ino = 9, .lease_epoch = 1, .setattr = .none }, .name = "a", .value = "vvvvv" };
    try testing.expect(recordShip(ship, 16384).isOk());
    try expectRejected(.value_past_record_cap, recordShip(ship, 4));
    var ship0 = ship;
    ship0.head.ino = 0;
    try expectRejected(.object_zero, recordShip(ship0, 16384));
}

test "the joiner's node_seq_base screen: below the base, off the stride, the manager's own space, past capacity" {
    const B: u64 = 1 << 50;
    try testing.expect(joinedNodeSeqBase(B + node_seq.INCARNATION_SPACE, B).isOk());
    try testing.expect(joinedNodeSeqBase(B + 7 * node_seq.INCARNATION_SPACE, B).isOk());
    try expectRejected(.node_seq_base_below_base, joinedNodeSeqBase(B - 1, B));
    try expectRejected(.node_seq_base_off_stride, joinedNodeSeqBase(B + node_seq.INCARNATION_SPACE + 1, B));
    try expectRejected(.node_seq_base_manager_space, joinedNodeSeqBase(B, B));
    try expectRejected(.node_seq_base_past_capacity, joinedNodeSeqBase(B + (node_seq.INCARNATION_ORDINAL_MAX + 1) * node_seq.INCARNATION_SPACE, B));
}

fn screenFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [2048]u8 = undefined;
    const len = smith.slice(&buf);
    const bytes = buf[0..len];
    const dvs = [_]DataVolumeBounds{.{ .vol_tag = smith.value(u64), .blocks = smith.value(u64) }};
    const heaps = [_]HeapBounds{.{ .start = smith.value(u64), .end = smith.value(u64), .node_size = @max(1, smith.value(u32)) }};
    const sb: SlotWordBounds = .{ .granted_floor = smith.value(u64), .release_seq_floor_bound = smith.value(u64), .recorded_cursor = smith.value(u64), .recorded_root = .{ .addr = smith.value(u64), .seq = smith.value(u64) }, .total_extents = smith.value(u64), .heap = heaps[0] };
    const db: DeathBounds = .{ .own = me, .listed_live = smith.value(bool), .registered_key = smith.value(u64), .held_keys = &.{smith.value(u64)} };
    if (verbs.decodeRequest(.release_slot, bytes)) |r| _ = releaseWords(r.release_slot.head.words, r.release_slot.items, &sb) else |_| {}
    if (verbs.decodeRequest(.publish_roots, bytes)) |r| {
        for (r.publish_roots.items) |root| _ = publishRoot(root, &sb);
    } else |_| {}
    if (verbs.decodeRequest(.return_extents, bytes)) |r| _ = returnRuns(r.return_extents.items, sb.total_extents) else |_| {}
    if (verbs.decodeRequest(.alloc_lease_bitmap, bytes)) |r| _ = bitmapRefs(r.alloc_lease_bitmap.items, smith.value(u64), &heaps) else |_| {}
    if (verbs.decodeRequest(.record_death, bytes)) |r| _ = recordDeath(r.record_death, &db) else |_| {}
    if (verbs.decodeRequest(.renew, bytes)) |r| _ = renew(r.renew, &dvs) else |_| {}
    if (verbs.decodeRequest(.xv_step, bytes)) |r| _ = xvStep(r.xv_step.head.step) else |_| {}
    if (verbs.decodeRequest(.xv_guards, bytes)) |r| _ = xvGuards(r.xv_guards) else |_| {}
    if (verbs.decodeRequest(.free_blocks, bytes)) |r| _ = freeBlocks(r.free_blocks, dataVolume(&dvs, r.free_blocks.head.vol_tag)) else |_| {}
    if (verbs.decodeRequest(.alloc_lease_acquire, bytes)) |r| _ = allocLeaseAcquire(r.alloc_lease_acquire, smith.value(u16), dataVolume(&dvs, r.alloc_lease_acquire.vol_tag)) else |_| {}
    if (verbs.decodeRequest(.block_grant, bytes)) |r| _ = blockGrant(r.block_grant, dataVolume(&dvs, r.block_grant.vol_tag)) else |_| {}
    if (verbs.decodeRequest(.return_blocks, bytes)) |r| _ = returnBlocks(r.return_blocks, dataVolume(&dvs, r.return_blocks.vol_tag)) else |_| {}
    if (verbs.decodeRequest(.join, bytes)) |r| _ = join(r.join) else |_| {}
    _ = joinedNodeSeqBase(smith.value(u64), smith.value(u64));
}

test "fuzz: every screen is total over arbitrary words and arbitrary bounds (never a panic)" {
    try fuzzkit.fuzz({}, screenFuzz, .{});
}
