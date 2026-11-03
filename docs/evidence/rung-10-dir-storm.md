# Rung 10 — a joined member's create storm into a holder's directory (2026-11-03, decision 0003 §DI)

**The defects** (found by the cross-member ESTALE branch's evidence workload, its §DH.13 (h); reproduced on dev
`10f6182`): B, a joined member, creates 10,000 entries in a directory A holds. (1) fsck found 169 live inodes named
by no dentry (C9); (2) A's `unlink` of its own files there answered EIO, B firing `cluster_wire_service_refusals`
("the holder's records could not be read"); (3) B's creates in a second such directory once answered EIO with the
same tripwire on A. Two causes, plus a third defect met on the way.

## Cause A — a holder's dentry page overflowed its scratch (defects 2, 3)

`Holder.buildPage` charged a dentry `24 B + name.len` against a budget equal to the 512 KiB scratch half it encodes
into; `GrantPage.encodeDir` pads each name to 8 B. A page from a cookie with more than 16,384 short names after it
(512 KiB ÷ 32 B) overflowed → `Corrupt` → refused → EIO at the member. Defect 3's EIO at B's 6,475th create into
A's 10,000 = 16,475 entries; A's `ls` of such a directory EIO too. Instrumented run: `token_grant object
2199023255554 (slot 1) wants 3 after 191401824961615106: Corrupt`, no record-source or node-load error.
**Red** `e77de53` (18,000 six-byte names: `codec Overflow` in `encodeDir`). **Fix** `cd7042c`: charge
`24 + padTo(len, 8)`; the record heads tied to the wire at comptime.

## Cause B — a release published a tree whose parent was dirty (defect 1)

The handover's cover loop cycled until `reusable_upto` passed the slot's record frontier and the root was
published; only door-token commits note the frontier. A flush that moves a leaf (the release's own cover cycle
included) journals the new address into the PARENT past the frontier and leaves it dirty: the cover passed, and a
joiner's release `forgetTree`s the slot — the device's parent still routes to the pre-move leaf image, without the
records that were in its delta (the newest creates). Reproduced (supervised rounds: B storms A's directory until the
handover, stops, unmounts): 72 and 10 names lost, C9 72 and 10. Instrumented: B's cover `frontier 38333876, tail
38337880, 1 cycle`; at the release `DIRTY nodes left 1 (floor 38337880)`. **Red** `50d196f` (deterministic: B fills
the newest leaf's log to its last frame, commits one more, leaves — `1 of 2 record(s) B wrote are gone`; the dirty
node is the level-1 root). **Fix** `d68245e`: the cover also waits for `Steps.treeClean` (no dirty node of the
slot's tree, `NodeCache.holdsDirty`); `slot_handover_tree_dirty_cycles`; a tree that never cleans defers.

## Met on the way — an SMO floored its parent at a stamp

`smoReplace` and the leaf merge floored the parent at a record STAMP (`position + seq_offset`). On a ring that
received a handed-over slot the floor sat past the head (the red root's 313,545 over head 305,400), so the dirty
parent never held the checkpoint tail below the SMO's entry: a crash before the next cycle lost the pointer.
**Reds** `6307a60` (expected 109,828, found 1,158,405), `664ba0b` (merge: 113,960 vs 1,162,537). **Fix** `e151e06`:
the entry's start position.

## Checks

On the fixed tree (Linux 7.3.1-omarchy; counters, never timings): the five reds green ×5 (`-Dfresh`); the
10,000-create shape ×5 (supervised, T_owner 3 s: A fills `e`, B stats and creates 10,000 there, both unlink, then
`h` the same) — every create, stat and unlink answered, `ls` equal on both members (e 19,360, h 20,000), fsck 0,
tripwires 0 on both, B served 11,152–11,438 grants with 0 refusals (A's reads of the directories B took, each past
16,384 names), A's `slot_handover_tree_dirty_cycles` 1, 0, 1, 0, 0; the leave rounds ×3 (18 rounds, 2,270–6,848
names each): 0 missing, fsck 0 (before: 2 of 7 runs lost names); the 13 two-daemon coherence and shipped-verb
rows, the 9 handover, striped-directory and eviction rows and the 7 KILL ROWS ×2 green; unit
filters `meta.` `wire.` `fuse.handler.kv_fs` 1,356 pass, 3 skip; the full unit step 3,247 pass, 184 skip, 0 fail;
`zig fmt --check`; `zig build rails` 32/32.

Not hot paths: one `padTo` per record on a holder's grant serve; a cache walk per release; the checkpoint task's
floor. A joined ring's tail now waits one cycle for an SMO's parent, as the manager's always did.
