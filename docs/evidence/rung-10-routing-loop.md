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
