# 2026-09-09 — FIND-PK-2: a promoted staged-layout file's block had no durable reference (P0, fixed `bfcf1e57`)

**Verdict: a real, live data-loss hole in the shipped default format, found
by reading, reproduced red-first, fixed in one commit.** `promote_staged_file`'s
block arm (`src/routing.rs`) published a promoted file's `bk:0:len` mapping
through `save_metadata_to_backend` with NO block-reference op, so the
durable block-reference ledger (design-durable-block-refcounts, incompat
bit 9 — stamped by the DEFAULT `format` since the rung-10b flip) never
learned the block. On any volume whose ledger is non-empty, the next mount
seeds ownership from the durable records ALONE (`recover_durable_block_refs`),
recovers every promoted block FREE, and hands a promoted file's offset to
the next striped allocation: the file's only copy is overwritten, silently.

Tree: `dev` @ `2836fa8b` (red) → `bfcf1e57` (green). Venue: the laptop's
unprivileged file-backed sandbox (`SQUEEZEFS_FUSE_ZC=0`, the fstests
runner's posture) — a correctness repro, counts and byte comparisons only;
no throughput claim.

---

## 1. How it was found

The small-file packing design review (`docs/design-small-file-packing.md`
§1.3, reviewer round 1, issue 1) checked the design's claim that the packed
arm would stage its C8 reference "exactly as the block arm's publish does"
and found the block arm does no such thing. The author of the 2026-08-02
wiring commit (`4db827c6`, "wire the accounting into the map-swap sites +
fsck C8") named **"the staged whole-image promotion"** among the four
map-swap sites in `block_ref_ops_for_map_swap`'s doc comment — and wired
the other three (the staged→striped growth in `write_striped`, the
`StorageFull` durable spill, the staged truncate's prune). The promotion
kept calling the ref-less `save_metadata_to_backend`.

## 2. Why every existing suite was blind to it

`recover_durable_block_refs` (`src/routing.rs` ≈ 3852) DECLINES an EMPTY
ledger ("an un-backfilled stamp reads every live block as free") and falls
back to the layout walk, which backfills. Every suite that promotes staged
files — `dismount_staged_residue_tests`, `fsync_promote_staged_tests`,
`inline_raise_tests` (d) — promotes ONLY staged files, so their ledgers
were empty at the remount and the walk covered the hole. The C8 oracle
(`SQUEEZEFS_BLOCK_REFS_VERIFY=1`) was armed on a volume with a mixed
population in exactly one place — `inline_raise_tests` (c), whose
promotion is the growth path, which was wired.

The pressure-driven merge promotion (the 75 % staging high-water arm) has
called the same block arm since 2026-07-06; on a default-format volume it
has carried this hole since bit 9 became the default (2026-08-16). The
dismount pass (`9ac570ab`, 2026-09-09, unreleased) made it fire at every
clean unmount of a mount holding staged-layout files.

## 3. The repro (contract 3 of `tests/dismount_staged_residue_tests.rs`)

One meta + one data volume formatted WITH a staging dir (the default
format — bit 9 stamped). Mount A: write one **striped anchor** (4 MiB +
64 KiB, fsync — the ledger's non-empty population, 2 records), then 200
staged-layout files (8/16/32/64 KiB, the suite's population), `syncfs`,
`squeezefs umount` (the dismount pass promotes all 200: 200 blocks). Mount
B at a different mount point with `SQUEEZEFS_BLOCK_REFS_VERIFY=1`, then
four fresh striped writes (4 MiB + 4 KiB each = 8 new blocks), then read
all 200 promoted files.

| | unfixed (`2836fa8b`) | fixed (`bfcf1e57`) |
|---|---|---|
| `meta_kv_block_refs_recovered` at mount B | 2 (the anchor's) | 202 |
| `meta_kv_block_refs_drift` (the C8 oracle) | **200** | **0** |
| promoted files clobbered by 8 fresh blocks | **8 of 200** | **0** |
| anchor intact | yes | yes |

The clobber count is the data-loss face: the seeded cursor sits past the
anchor's two indices, the 200 promoted blocks lie above it and read as
free, and the first eight fresh allocations are exactly eight promoted
files' offsets. With more fresh writes every promoted file goes.

## 4. The fix (one commit, `bfcf1e57`)

The block arm's commit now stages the old→new map diff through
`block_ref_ops_for_map_swap(ino, current.block_map, updated.block_map)` and
saves through `save_metadata_to_backend_refs` — the growth site's shape
verbatim — so the `+ref` (and a re-promotion's displaced `−ref`) rides the
SAME checksummed journal entry as the layout that justifies it (spec §6.2
item 1: one tx = the layout + its ledger delta). No format change, no knob,
no new gauge: the existing `meta_kv_block_refs_drift` tripwire is the
instrument, and it now reads 0 across a promotion.

Suites re-run green with the fix: `dismount_staged_residue_tests` (3),
`fsync_promote_staged_tests` (17), `inline_raise_tests` (7),
`durable_block_refs_tests` (2), `staged_crash_recovery_tests` (7); the
batch gate on `bfcf1e57` is the landing gate.

## 5. What this says about the ledger's oracle posture

The oracle's "decline an empty ledger" arm is correct for the un-backfilled
upgrade case it was written for, but it also means **a suite whose entire
population goes through one publish path proves nothing about that path's
accounting**. Every future accounting-bearing publish site gets its red
contract on a MIXED-population volume: one striped anchor first, then the
site under test, then the oracle. Contract 3 is the template. The packing
design's PK1 carries the same shape for the packed arm.

## 6. Release status

1.2.2 (`stable-2026.09.2`) ships the pressure-driven half of the hole (the
merge worker's promotion under staging pressure on a staging-dir volume
with a non-empty ledger). The fleet's field mounts are cache-less
(`.benchmarks/2026-09-09-dismount-staged-residue.md` §5 — no staged layout,
no promotion, unaffected). The fix is 1.2.3-bound with the dismount pass it
makes safe; the 1.2.3 RELEASE_NOTES section (written at the release act, the
repo's convention) carries it under fixes, and the pause note
(`.benchmarks/2026-09-06-pause-state.md`) carries it until then.
