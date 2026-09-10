# 2026-09-10 — P0: a kernel-split segment lost to the router's guard-less striped RMW (fixed `d914b673`)

**Verdict: a real, reproduced-on-tape data-loss race on the SHIPPED default
write path — buffered files just over one block — fixed by deleting the
one striped publisher that ran without the block guard.** Two tapes, one
signature: a `4 MiB + 64 KiB` file written `create → write_all →
fsync` (the kernel writes it back as FIVE concurrent 1 MiB FUSE WRITEs)
read its **3–4 MiB segment back as ZEROS** after a SUCCESSFUL fsync, with
exactly one writer-side log line at the moment of the write:

```
ERROR squeezefs] INVARIANT TRIPWIRE 'overlay_foreign_merge': a foreign un-marked Merge
reached an open-overlay index — a one-authority-screen escape (contained: superseded)
```

Rate ≈ 1 in 80–100 such files (tape 1: PK2's dismount contract, 2026-09-09,
`/tmp/sqfs_pack_dismount_214805/`; tape 2: the batch gate on `4fc4aa59`,
`packed_mapping_wire_tests::a_promoted_staged_file_placed_off_the_default_
volume_reads_byte_exact`, `/tmp/sqfs_packwire_pk0_560503/` — mismatch ONE
contiguous range `[0x300000, 0x3fffff]`, all zeros; the durable map
consistent: drift 0, rebinds 0, `staged_payload_lost_reads` 0 — the bytes
were simply not there). Lever-independent; predates packing. Tree: `dev`
@ `408263a3` (red) → `d914b673` (green). Venue: the laptop's unprivileged
file-backed sandbox (`SQUEEZEFS_FUSE_ZC=0`) — a correctness repro.

---

## 1. The mechanism, as verified in the code (not as first read)

The tripwire text says "a foreign un-marked Merge". My first reading blamed
the staged→striped growth promotion's publish. That was WRONG: the
promotion in `DataRouter::write_file` is a whole-layout
`save_metadata_to_backend_refs` under `BLOCK_FLUSH_LOCKS(ino, 0)` + the
3.5 guard; it never reaches the `Merge` hook, and an overlay record cannot
install on a block until the layout reads striped under that same guard.

The un-marked `Merge` was **`DataRouter::write_striped`** — the router's
own striped read-modify-write. The WRITE handler classifies a segment ONCE
("staged within block" → `MetaPrepOnly`, inode guard dropped) and calls
the router; when a SIBLING segment's promotion flipped the layout striped
between that classification and the router's fetch, `write_file` took its
"layout flipped striped while we waited" arm — which DROPPED the block-0
guard, seeded from a start-of-call binding (the device's `k0`, which never
composes an open overlay record), and published `Merge{0: k0'}` holding NO
block guard. Every other `Merge`-class caller (write-through, the
flushes, `fold_upload_block`, the settle with its marker) runs under the
block guard; `write_striped` was the only guard-less publisher of new
bytes.

The five-segment schedule that produces the tape: segments A, B (0–2 MiB)
accumulate staged; E (4 MiB+) crosses the threshold and PROMOTES the file
striped; D (3–4 MiB), classified AFTER the flip, goes `write_file_staged`
→ its overlay arm runs BEFORE accumulation and D is overwrite-eligible
(1 MiB aligned, mapped block, above the finding-47 floor) → D's bytes land
in a fresh overlay dest; C (2–3 MiB), classified BEFORE the flip, reaches
`write_striped`'s stale arm → its RMW seeds `k0` = `[A, B, _, _]` from the
device, composes C, publishes `k0'` = `[A, B, C, 0]`. `overlay_screen_merge`
reads that as a foreign merge on D's open index and "contains" it by
SUPERSEDING D's record — D's dest is freed, the 1 MiB of acked bytes is
gone, and the tripwire counts one. The coverage union (FIND-L1-A) never saw
D because D never reached the accumulation buffer.

## 2. Why "widen the provenance exemption" is the wrong fix

KD-B4-11's marker exempts the settle's OWN publish. Widening it to "the
same file's transition publish" would leave D's record open across C's
merge — and D's later feed would then displace C's image: C lost instead
of D. Two unserialized whole-block RMWs of one block cannot both win; the
only never-lossy answer is that there are not two. The one-authority screen
was right about the LAW ("a merge on an open-overlay index without the
marker is unreachable"); the tree had a publisher that violated it.

## 3. The fix (`d914b673`, red `1432bf7e`)

- `DataRouter::write_file` → `Result<WriteFileOutcome>`: `Written` |
  `LayoutStriped` (NOTHING written). Both "layout is/became striped" arms
  answer `LayoutStriped`. **`write_striped` is deleted** (460 lines, with
  `LayoutFlip::ToStripedClearStagedIdentity` and `keys::metadata_for_path`,
  its only consumers).
- Every caller re-dispatches through the ONE guarded striped path: the
  WRITE handler via a shared `dispatch_striped_write` (the S11 range fork +
  the FIND-RW5-A fresh-lease retry — used by both the striped arm and the
  redirect, so they cannot drift; the postlude's size floor follows where
  the bytes landed), the punch's inline/staged arm, and `copy_file_range`'s
  non-striped arm. On that path an open overlay record is JOINED (the
  settle seeds the gaps from `k0`) or settled first.
- `overlay_screen_merge` and the tripwire keep their meaning: the foreign
  class is now unreachable by construction, so `overlay_foreign_merge` is
  again a must-stay-0 tripwire with teeth.
- Test seam `SQUEEZEFS_TEST_ROUTER_DISPATCH_STALL_MS` (registered): parks
  the router dispatch so the test can sequence C's stale arm against E's
  promotion and D's overlay store with `sync_file_range`-initiated
  writebacks (O_DIRECT would serialize in-kernel on `i_rwsem`); the writer
  is read back O_DIRECT (a buffered read is the page cache); the test
  panics as an INVALID ROW if the schedule did not land. Contract
  `tests/overlay_growth_merge_tests.rs` (mount-class; registered in the
  require-mount gate): **RED on the unfixed product ×3 — same range, same
  tripwire as both tapes; GREEN ×5 on the fix.**

## 4. What was re-run

fmt / clippy both configs / rustdoc clean. Green with `--test-threads=1`:
`overlay_growth_merge_tests` ×10, `packed_mapping_wire_tests` ×5 (the
suite that caught it), and the write-path population — `write_through_
coverage` 8, `write_through` 26, `overlay_ack_early` 14, `overlay_core` 19,
`overlay_gap_seed_ranged` 4, `overlay_length_floor` 7, `overlay_overwrite`
32, `overlay_settle_wait` 2, `data_path_correctness` 27 (its PR 6 pin of
"the router route the design keeps live" is retired WITH that route and
replaced by the redirect pin), `extent_patch` 21, `rand_write_amp` 7,
`copy_file_range` 5, `small_file_packing` 13, `pack_tenant_ops` 8,
`staged_crash_recovery` 7, `dismount_staged_residue` 3, `fsync_promote_
staged`, `durable_block_refs` 17, `assembly_ownership` 7. The batch gate on
`d914b673` is the landing gate.

## 5. Design-doc consequences

`docs/design-overlay-overwrite.md` §5.7's sentence "a `Merge` on an
open-overlay index WITHOUT the marker remains … unreachable past the
one-authority screen" was FALSE while `write_striped` existed — amended
in place with the escape and its closure. Finding 47 (the overlay's length
floor) is NOT falsified: the floor correctly made D overlay-eligible; the
loss was the unserialized publisher. KD-B4-11's rationale ("installs
require the guard") was true — the hole was a publisher without the guard,
not the marker.

## 6. Release status

1.2.2 ships this race (the router's stale-classification arm has existed
since the staged layout did). Exposure: buffered files that cross the
4 MiB threshold while ≥ 3 of their kernel-split segments are in flight
together, on volumes WITH a staging dir and the device overlay armed (the
default) — ≈ 1 % of such files on the laptop. The field fleet is
cache-less (no staged layout, so no transition — unaffected). 1.2.3
carries the fix; the release notes name it under fixes at the release act.
