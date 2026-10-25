# Rung 10 — the writer's node seqs never leave the volume's own space

Charter §5.1 "Node" / §5.5 (amended 2026-10-25). Branch `fix/753-replay-window`: the red unit
test, then the bound, then this note.

## The finding

The release verdict run on `fd7ad76` stopped at xfstests generic/753. Its first cycle's scratch
daemon fired the must-stay-0 `meta_conveyor_pass_panics` (`Corrupt`, 1 member) 140 ms after the
mount, on a HEALTHY volume. The remount on the working table then refused: `cannot open metadata
set …: corruption outside the replay window (see the log)`. The log named nothing. 753 had passed
on `e5253be` and `bd97cda`. The O_NOATIME change (`8916bd3..fd7ad76`) is not involved: it only
gates atime stamps on a flag fsstress never sets, and in `src/meta` it touches test call sites
alone.

## The mechanism

By hand (fsstress's 753 mix, 4 procs, over a set on one switchable dm device; error table without
lockfs, kill, umount, working table, mount), 263 cycles on an aged volume never refused. With a
re-format before every cycle, which is what the harness's per-test `_scratch_mkfs` does to a
recycled scratch device, cycle 8 failed. 400 ms into that cycle's mount, on a healthy volume, the
log showed `SMO fold-source log-view mismatch at 0x3b40000: disk walk ends at 262144, the live
object's append cursor is at 258048`. Every checkpoint after that failed `Corrupt`, the ring
filled, the pass parked 30 s and the volume latched FAILED. The remount's bring-up checkpoint met
the same node and refused.

The snapshot of the metadata slice, taken before the remount, explains it:
- The frame at offset 258048 of extent 0x3b40000 carries the node's own `node_seq`. Its
  `journal_seq_horizon` is 57000345, from the previous format's range (this format's head reached
  25388920), and its keys belong to another node. It is an EARLIER FORMAT'S frame, accepted as
  this node's because the frame incarnation check is an equality.
- The volume's base is `nodeSeqBase(uuid) = 0x60502f1901a7bce2`. The format's two nodes carry
  base+1 and base+2. Every node minted at runtime carries `0x76d6c40e80a49axx`, so the counter
  had left its own space.

The cause: `KvTree.create`, when it mints a guest slot tree on first touch, raises the shared
counter to `residueSeqCeiling` of the recycled extent it claims. That residue is the earlier
format's stamps. The port wired the writer's handle as `NodeSeqHandle.shared`, which has no
ceiling. The reference bounds the manager by incarnation 0's span (`shared_bounded`,
`backend.rs:2514`), and the port had defined `sharedBounded` but never used it. Once the counter
is raised, the new format re-mints the earlier format's seqs. At a recycled extent, the earlier
frames past a new node's log end verify as its own. The latent cases are worse than a refused
mount: a cache-miss load can serve another node's records. This is the 140 ms pass panic, where
the applier's resolve or pre-image read hit such a load.

## The law and the fix

The manager and a flat volume's writer mint in incarnation 0's `[B, B + 2^38)` and ignore every
raise outside it: a recycled extent's residue from an earlier format, or a dead joiner's root. A
joiner was already confined to its own space. `KvBackend.open` seeds `node_seq.managerHandle`,
which is `sharedBounded` over `nodeSeqBase(uuid)`. A watermark that a pre-bound writer had
already lifted past the span keeps the unbounded handle and is named at the open, because
bounded it could mint nothing. The open's bring-up and join checkpoints now name their refusal in
the log.

## Red, then green

| check | `cb7625e` (red) | `afdb099` (fix) |
|---|---|---|
| unit `a re-formatted volume's node seqs never leave its own space`: the new format's header-only node at the next free extent, under the seq it mints next | `expected 0, found 3` (adopts the earlier format's three frames) | pass |
| unit `the writer's handle is bounded …` (node_seq) | — | pass |
| two formats of one device, each 2 s of the 753 mix, clean unmounts: the second format's ledger `node_seq watermark` | outside the own space in 2 of 10 (a raise only climbs: the earlier format's base must lie above) | inside in 10 of 10 (+1001 … +1179) |
| 753 by hand, re-format every cycle, random seeds / seed 1792056321 (753.full's) | refused at cycle 8, then 60/60 (1 of 68: the escape AND a same-extent re-mint AND a stale frame at the new log's end) | 60/60 and 60/60 clean, fsck clean, 0 tripwires, 0 fold mismatches |
| 753 by hand, aged volume | 263/263 (fd7ad76) | 40/40 clean, fsck clean |
| unit filters journal / journal_tail / conveyor / supervisor / replay / node_seq / meta.tree / residue / backend | | 191 / 51 / 101 / 74 / 122 / 51 / 54 / 67 / 86 pass, 0 fail |
| smoke: dm-error, held tail, replay proof, kill -9 mid-tar, format sweep, custody, free hold, the kill rows, join after the manager's kill, re-join ×3, the striped dead-manager rows | | 55 pass, 1 skip (a shim row's other step) |
| smoke: the dm-error and held-tail rows, 5 passes (`-Dfresh=true`) | | 5/5 |
| `zig build rails`, `zig fmt --check` | | 18/18 steps, 32/32 tests; clean |
| xfstests `generic/753 475 750 388` through the harness, 3 runs | | Passed all 4 in each run (388 not run: no shutdown); fsck clean; 0 tripwires, 0 fold mismatches |

## Gate 141: the bound exposed a cross-space order in the offline fold

On `afdb099` the smoke row THE SHARED HOT DIRECTORY ACROSS DAEMONS went red at its final fsck
(`cannot open metadata set …: Corrupt`, with no reason logged). An error-traced build over the
kept image showed `openGuestTrees` → `RootStale`: slot 65's root `0xa00000` carries node_seq
B+2^38+1 (dead joiner B's incarnation 1), but the extent holds B+91.

A had recovered B's region and adopted its slots (`Leased{0, g 2}` in tree 0, with the
recovery's words). A's SMOs moved those trees inside incarnation 0's space and recycled
`0xa00000`. A's page names the moved root at g 2. `slot_state.foldGuestRoots` let a page entry
beat tree 0's word at equal generation only if its root seq was ≥ tree 0's, which is an order
across spaces, and 44 slots of B and C folded to dead roots. Before the bound, the adoption's
raise had lifted A's counter into B's space, so the order held by accident. That escape is
exactly what re-mints a dead space's seqs.

The law: a slot's two homes are ordered by generation alone. At tree 0's generation the settled
page entry is the live home and wins; between pages only a higher generation wins. The node seq
never decides. `SlotWords.floored` had the same order and was unused, so it is deleted. A
writer's open now names a stale guest root (slot, root, lessee, what the extent holds).

| check | result |
|---|---|
| unit `a slot the manager adopted from a dead joiner …` on `df32246` (red) | `expected 0x1f40000, found 0xa00000` |
| the kept image's metadata open | refused on `afdb099`; opens with 194 trees on the fix |
| the row, `afdb099` / `fd7ad76` / the fix (under the 753 loop's load) | 0/5 / 10/10 / 22/22 |
| every two-mount, offer, handover, recovery, re-join, reader, fleet row plus the dm-error, held-tail, replay and free-hold rows | 88 pass, 1 skip (the shim's other step) |
| 753 by hand, re-format every cycle, beside the row loop | 40/40 clean, fsck clean |
| unit filters slot_state / lease_table / guest / reader / backend / recovery / re-join / node_seq / meta.tree / journal / journal_tail / conveyor / supervisor / replay | 0 fail |
| `zig build rails`, `zig fmt --check` | 18/18 steps, 32/32 tests; clean |

## Seen, not fixed

- The held-tail prefix law (§BR.3½) dropped acked windows past a hole in 14 of the 423 cycles
  above, on both binaries, and 13, 6 and 9 times in the three subset runs (`N acked window(s) of
  the held journal tail lay past a hole`). This is the designed trade, and that metadata is lost
  after a device failure.
- A volume that a pre-bound writer already re-minted into another space may still carry colliding
  nodes. The open warns on it, and it should be reformatted.
