# Rung 10 — the writer's projection of a live lessee's tree; the online scrub's `[C3] refcount 37 vs 36`

The 10-21 handoff's owed item 3 and its pre-existing `scrub --online` finding (decision 0003 §CF).

## 1. A manager's remount beside a live lessee (§CF.1)

**Scenario.** A (the manager) mints a slot tree and hands the slot to B; B compacts the root (the
grant-time image retires into B's pool) and writes on; A remounts (`kill -9` + remount, the KILL /
DEAD-MANAGER rows' shape) while B stays live. **Cause.** Tree 0's `Leased{B}` carries the GRANT-TIME
words (the live root rides B's page). The writer's `openGuestTrees` opened every slot leased to another
appender as an in-RAM tree at those words, while the live manager holds none — a wire grant forgets it
(`foreignGrantedFn` → `forgetTree`; B's objects are read under B's tokens). Once B's pool recycled the
image the mount refused; before that, every refs census of the manager (`rangeRefs` merges every tree it
holds: the data plane's mount seed, the free executor's `durableRefcount`, fsck) read records B had
superseded. **Red** (`9f02675`, `sym.handover_moves` "THE MANAGER'S REMOUNT BESIDE A LIVE LESSEE HOLDS
NONE OF ITS TREES": A's device cut beside the live B, reopened as the manager). On `7693bab`: (1) after
B's compaction and its delete of a block reference, the remount held slot 1's tree at the grant-time
root `0x160000` and counted the deleted reference (`references counted 1 (B holds 0, the live manager
0)`); (2) after the recycle: `slot 1's tree root 0x160000 (node_seq …488, lessee 1) does not verify: the
extent holds node_seq …431 — refusing the mount` → `Corrupt`.

**Fix** (`a87b73a`). A writer's open opens no tree for a slot leased to another appender (the live
manager's posture; `writer_foreign_trees_deferred`); the window's mint loop never mints one;
`adoptReachableClaims` lost its projection list. A dead lessee's tree is installed by the recovery from
its page (`installRoot` opens a tree it does not hold, as on a live manager). Folding the lessee's page
(the reader's `2093e40` law) was rejected: that tree goes stale at B's next SMO and the free executor
would still count superseded references. The reader posture is unchanged. **The leak census** now has
the live manager's inputs. The KVDA rows do not move (B's slots there are minted after their grants:
grant-time root 0, never projected): `referenced 50, released 0` with the pages, `relearned 142`
without, shas equal, fsck clean — as on dev.

## 2. The rollback after step 7 (§CF.2), exposed by §CF.1

With §CF.1, volume_bind's "every crash window of the §5.9 procedure over the real backend" lost the dead
window's records at `.tree0`: step 7 lands `Unleased{flushed root}`, a failure before the table release
rolled the slot's tree back to its pre-install root or removed it (`prior = null`), and the re-run
absorbs the slot from tree 0 and installs nothing. The remount's projection was the `prior` that masked
it; a LIVE manager always recovers with `prior = null`. **Red** (`7de900d`, the RAM fixture at `.tree0`):
`expected 65536, found 32768` — slot 10 left at `0x8000` where tree 0 names `0x10000`, slot 11 dropped.
**Fix** (`12e7f58`): `SlotPlan.recorded`; the rollback keeps a recorded slot's tree.

## 3. `[C3] refcount 37 vs 36` from `scrub --online` (§CF.3)

**A census race, closed on dev before this branch.** The block is the holder's OPEN pack (`tenants + 1`,
the packer's pin) with tenants in A's view and B's custody. `e0835f0` (the merged arms pass the
pack-open ledger) and `27b7ba3` (the confirming round re-reads every side after the first round's
shields; `fsck_suspects_cleared` counts what re-verification clears) landed 2026-10-24
(`rung-10-c3-open-pack.md`), after the 10-22 "4/10 on a dev baseline". **The rig**
(`~/tmp/writer-projection/c3loop.sh`): the ONLINE FSCK row 4-wide beside 8 × `yes`, a fsync'ing fio and
a `sync(2)` every 200 ms, one venue slot. The global sync is the load that matters (as the full smoke's
other rows): it promotes A's fill into packs before the set's scrub. Without it 32/32 runs on `7693bab`
read 15 blocks and never met an open pack; with it every run below read 215.

| tree | runs | `[C3]` | shield engaged (`pack-ledger exempted ≥ 1`) |
|---|---|---|---|
| `ac7986e` (before `e0835f0`) | 12 | **9** (`37 vs 36` ×7, `38 vs 37` ×2) | — |
| `7693bab` (dev) | 32 | 0 | 24 |
| this head | 32 | **0** | 24 |

## Checks

Reds green; the remount row 30/30 and the crash-window rows (RAM fixture + real backend) 30/30, looped
in one slot. No lock-free core touched (the open's and the recovery driver's single thread): no model.
Unit filters `meta`, `membership`, `handover_moves`, `fsck`, `scrub`, `sym_planes`, `block_refs`;
`zig build fmt rails`. Smoke (one binary, 64 tests: TWO MOUNTS OF ONE SET, the joiner's rejoin, the
three joins after a manager remount/kill, the re-joins in place, the three dead-manager rows, THE KILL
ROWS a/b/c, KVDA ×2, FSCK AND SCRUB, ONLINE FSCK, the merged census, the `-o ro` reader, the shared hot
directory): 3 clean passes of 4 (63 + 1 shim skip); the fourth ran beside the rig's `sync(2)` loop and
lost TWO MOUNTS OF ONE SET at its first check, before B joins (`a_staged >= 200 × 64 KiB`: an outside
sync promoted A's residents). xfstests generic/001 013: passed, 0 alerts, fsck 0 findings.

## 4. The three left seen-not-fixed, fixed (§CF.4–§CF.6, 2026-10-29)

- **§CF.4 a rolled-back recovery's guards.** A failure after step 7 rolls back before 7b; the re-run
  absorbs the slot from tree 0 with no plan, so the coherence hold and the custody quarantine were never
  installed. **Red** (`32f5beb`, the RAM fixture, an EARLY record, `fail_at = .tree0`, against the
  un-rolled run): `TestCoherenceNotHeld`. **Fix** (`9d63925`): `Driver.Guards` off the death record,
  `guardRecovered` applied by 7b and by the absorption (before the word turns `Unleased`).
- **§CF.5 the offline census.** `status`/`df` folded appender 0's page only. **Red** (`db89fe1`): B moves
  the root and replaces A's reference with two — the census counted 1 (the grant-time tree's); after the
  recycle `root pointer stale … extent 0x160000` and the census refused. **Fix** (`b4c7cd0`): every
  holder's `Live`/`Recovering` page folded (`readHolderPages`) — the census reads B's 2 at its page root
  before and after the recycle; a holder moving inside the census (`status.test_seams.after_pages`) is
  `1 slot(s) … not counted`, 0 references, no refusal.
- **§CF.6 the member-scope row.** **Red** (`9a8c578`): an outside `sync -f` of both mounts at each instant
  the row reads residents (what any host `sync(2)` does) fails `a_staged >= 200 × 64 KiB`. **Fix**
  (`9d565482`): the seam `SQUEEZEFS_TEST_SYNCFS_KEEPS_STAGED` holds both members' residents across a
  syncfs (residency IS the row's precondition); the row checks the sync reached A (`fuse_syncfs ≥ 1`).
  20/20 beside `while :; do sync; sleep 0.05; done`, started and stopped (trap EXIT) in one venue command.

Checks on the follow-up head: the three reds green; unit filters `meta`, `membership`, `handover_moves`,
`fsck`, `scrub`, `sym_planes`, `block_refs`, `data.plane`, `data.fsync`, `cli.status`, `cli.df`;
`zig build fmt rails`; the 64-test smoke set above 2 clean passes (63 + 1 shim skip; KVDA `referenced
50, released 0`); the five rows that read the `status`/`df` census (the POSIX pass's status, the SET
format, the data plane with and without the write cache, THE KILL MATRIX) all pass.
