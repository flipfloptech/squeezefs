# Rung 10 — the "routing loop" after a kill: a named guest root recovered unallocated

**The finding** (`docs/gates/flakes.md`, owner `routingloop`). Once in 11 runs of the smoke row
"UNDER THE WRITE-BACK TIMER fsync, fdatasync AND syncfs …" (`.wt-wbtimer`, beside two storm rows),
`fsck --offline` after `kill -9` refused the metadata open: `tree 0 (root 0xa00000@…): traversal
retry budget exhausted descending to level 0 (routing loop — SMO protocol bug) restarts
[root-retired, root-seq, routing-hole, child-retired, child-seq] = { 0, 256, 0, 0, 0 }`.

**Reading it.** Every restart was a root-seq mismatch inside an offline open: the cache's node at the
root's address carried another seq, 256 times — no SMO window, an OVERWRITTEN root. Tree 0 is a
slot tree (header id 0). `KvTree.open` had verified that root, so the extent was rewritten inside
the open: by `KvTree.create`, the guest re-mint `openGuestTrees` runs for a slot the window names
without a root — its `claimInternal` handed out the named root's extent, as free.

**Root cause — (a), a crash-consistency defect, not an oracle misread.** A guest slot tree is minted
on first touch (`mintGuestTree` → `KvTree.create`): ONE extent claimed in the RAM allocator, with
no journaled delta (the plain mount's 4 files minted 4: `routing_guest_trees_minted 4`). The root
is NAMED at once — appender 0's page refresh at C4b (before the barrier), C11's page plan, or a
tree-0 record — while the claim reaches the device only in a later cycle's C6 bitmap pages. A kill
between the naming and that page recovers the root, named and verified, in a FREE extent; the
mount's first claim (the window's re-mint, any SMO, the next guest mint) writes over it. The fsck
open's own re-mint did it before its replay #2 reached the named tree; without a re-mint the
overwrite waits for a later claim and corrupts silently.

**Red test** (`meta.backend` "kill matrix: mid-C4b …"; new seam `after_guest_roots`, the end of
C4b): two guest trees minted, the cut after C4b named both roots and before C6. Against `dev`: both
roots reopen unallocated (`expected true`; probed: `0x160000@…870 allocated false`, `0x170000@…871
allocated false`), and the third slot's mint takes `0x160000` and reproduces the field signature
byte for byte: `tree 0 (root 0x160000@7308044090248885870): … = { 0, 256, 0, 0, 0 }`. Deterministic:
every run.

**The fix** (`KvBackend.adoptNamedClaims`, `ExtentAllocator.adoptLive`): at mount, after the
replay's allocator fold and before the first claim (`openGuestTrees` runs it ahead of its
re-mints), every opened tree whose root extent is free has every extent it reaches adopted — bit
set, page dirty (the next checkpoint writes it), `meta_kv_replay_root_claims_adopted`, one warn line.
A root that is allocated is skipped: below a minted root every claim is an SMO's journaled delta. The
same law as the dropped root free (`meta_kv_replay_root_frees_dropped`): recovery reconciles the
allocator with the roots it names. The exhaustion line now names the forest slot, the seq the root's
extent held, and says "an overwritten root" when every restart was root-seq.

**Why not order the naming after the claim.** The page refresh carries the live words BEFORE the
barrier on purpose (a root left for the next cycle is one the tail may pass); deferring a fresh root
would need its records clamped in the window by another floor. A journaled claim at mint would leak
the extent when the crash precedes the naming. The adoption needs neither.

**Owed.** A builder-built tree (`copySlotTree`, adopted, its claims unjournaled) whose root's page
landed while a lower page of the same C6 did not: its lower extents stay free. The lease plane's
joined mounts open grant-mirror trees elsewhere (not this path).

**Numbers.** Reproduction loop (the three write-back-timer rows, 3 loops side by side on the VM):
300 / 300 green — the window (page refresh → C6) is milliseconds; the seam cut is the repro.
`fsckClean` now keeps the post-kill image and the daemon log in `/build/tmp/fsck-refused-<pid>/`.

**Gate.** One full venue gate through the lock (VM, the series on `703dd98`): 49/49 steps,
2,934 / 3,016 tests (82 skipped). Targeted: the unit kill matrix (5 cuts) and `adoptLive`; the smoke's
79 kill / write-back-timer rows green.

## Round 2 — the whole reachable set; the symmetric plane (0003 §AO)

**Builder-built trees (the owed residual).** `copySlotTree`'s builder claims every node in RAM, its
root LAST; C6 writes the bitmap pages in page order. The cursor is lowest-first (a release lowers
it), so a release landing mid-build — a checkpoint's coverage drain beside a live copy — puts the
root BELOW its leaves, and a kill inside C6 lands the root's page without the leaves'. Round 1 tested
the root's bit only and skipped the tree. **Red** (`meta.backend` "kill matrix: mid-C6 …"; seam
`after_bitmap_page`, a 2.5 GiB sparse two-page volume): `the built tree reaches 10 extent(s), 9 of
them free in the recovered allocator; the first claim past page 0 is extent 32576` — a live leaf.
**Fix:** the writer mount walks EVERY maintained tree and adopts each reachable free extent; green:
`0 of them free; the first claim past page 0 is extent 32585`. The mid-C4b cut still adopts its two
roots. The cut snapshot now copies data runs only (SEEK_DATA / SEEK_HOLE).

**Cost** (AO.3; kamrui, x86_64 NVMe, a ReleaseFast build): 1,000,000 files in 8 directories (8 shell
writers, 244 s), 66 trees, 1,026 reachable extents — the walk takes **0.65 ms** per writer mount,
clean or after a kill (`meta_kv_mount_reach_walk_extents` / `_ns`). It reads only interior nodes,
under 1 % of the images at the shipped fan-out. Every writer mount walks: a page refresh names a root
with no journal entry, so an empty window does not prove the bitmap whole.

**The symmetric plane — the review.** Can a named root, or a node it reaches, come back FREE in
the bitmap the manager allocates from after a kill of either member?
- *Manager → joiner (T1).* The handover's `cycleOnce` checkpoints the holder BEFORE the grant's
  entry moves the tree's images into the grantee's record: measured, one A page lands between the
  mint and the grant, and B's pooled image (A's RAM claim moved to B, then retired into B's pool)
  stays allocated on A's reopened bitmap.
- *A joiner's own claims* sit inside its grant runs, and the grant entry journals their bits with
  the record (`Manager.grant`'s alloc frames). A joiner killed: §U.1's recovery installs the newer of
  page and tree-0 root, replays the dead window under the SMO lock, keeps every extent the
  recovered trees reach and returns only the rest. A joiner's rejoin is a new incarnation whose
  mirror is its fresh grants (nothing to adopt).
- *KVDA homes* barrier their images and then journal their claims as one control entry.
- **The one defect: my own round-2 walk.** At the manager's mount it also walked trees that tree 0
  names as leased to a LIVE joiner. Such a tree is a projection whose interior may name extents the
  lessee recycled; §U.1 forbids walking it. **Red** (`sym.handover_moves` "THE MANAGER'S KILL WITH A
  LIVE LESSEE"): `the mount walked 3 extent(s), the maintained trees hold 2`. **Fix:**
  `slot_state.GuestRoot.lessee`, and the walk skips another appender's slots (AO.2). Green: 2 = 2.
  Round 1 on `dev` walked a tree only when its root was free, and a live lessee's root is always
  allocated (it sits in the grant), so `dev` was never exposed.

**Owed (not found reachable here).** The manager killed in the middle of a dead member's recovery,
between the residue return and the replayed trees' first checkpoint, is §U.1's structure law; this
pass did not re-verify it. The kamrui measurement also hit an unrelated wedge: under 8 mdtest ranks
whose MPI TCP pairing failed, one create was parked at `handler_entry` for over 75 s, and the
umount then hung.

**Gate (round 2).** One full gate on kamrui (the VM is the train's), on the series before its last
rebase: 47 of 53 steps passed, 2 failed, and 2,936 of 3,017 tests passed (80 skipped, 1 failed). Both
failures happen without this change and are attributed in `docs/gates/flakes.md`:
- the writer-storm row on kamrui (`gate waits 0, cold flushes 0`), which fails the same way on `dev`
  `260be26` alone;
- `suites-smoke`'s apt.lock drift on kamrui's Ubuntu 26.04 packages.

Targeted runs: the backend, `handover_moves`, `volume_bind` and `slot_state` units, and 94 smoke rows
(kill, daemons, joins, timer, readers). The KVDA page-sink row failed once in that batch and passed
2 of 2 alone; its rows were attributed on `dev` meanwhile.
