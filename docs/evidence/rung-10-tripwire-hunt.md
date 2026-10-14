# Rung 10 — the tripwire hunt: a moved tree's images were no one's custody; fsck's per-suspect lookups are linear

The fsck-scale agent (`rung-10-fsck-scale.md` §5, "Disk incident" / "Unattributed") saw the shared
hot directory row (`smoke: THE SHARED HOT DIRECTORY ACROSS DAEMONS`, `src/cli/smoke.zig`) fail
twice on its branch firing `extent_grant_closure_violations` and/or
`meta_kv_replay_extent_violations` — two must-stay-0 tripwires no earlier smoke log names — and
pass 12/14 standalone there and 13/13 on pristine `dev`. Item 2 is that note's "Owed": fsck's
scans per suspect. Decisions: 0003 §AD (item 1), §AC.6 (item 2); 0002 row 524.

## 1. The two tripwires

| Tripwire | Site | What it asserts |
|---|---|---|
| `extent_grant_closure_violations` | `sym/joined.zig` `checkClosure` (after a pool refill and after a surplus return) | A joiner's grant pool: `held + unclaimed + returned ≡ granted` over its grant bits — `granted` counts every extent a grant reply added, `returned` every extent a surplus return gave back. `held + unclaimed` is the bits' count, so the identity breaks only when a return names an extent the bits never held (or a grant adds one twice). |
| `meta_kv_replay_extent_violations` | `sym/membership/recovery.zig` `screenWindow` (§5.9 step 3) | A dead joiner's ring window: every `alloc` delta names an extent its manager-side `extent_grant:` record holds. A violation refuses the recovery (`Violation`): it rolls back and the next poll re-runs it — for ever. |

Both say the same thing from two ends: **a joiner's pool held an extent its grant never gave it** —
the surplus return withdrew it (closure), or an SMO claimed it into an image the ring journaled
(the screen).

## 2. The hunt

- **The merged walk (`b4b66e8`) and `MetaSet.range`.** The product's cross-slot pagers are the
  mount's corpse sweep (`kv_fs` `TREE_INODES` from `inodeKey(0)` to the end) and every refs range
  (`TREE_BLOCK_REFS`, the data plane's `block_refs`); readdir, xattr lists, the served
  dentry/xattr lists, the stripe and record-ship reads stay inside one parent's slot (one tree, no
  merge). Recovery, the membership census, the manager's verbs and the death ledger range over the
  CONTROL tree (`TREE_CONTROL`, one tree on volume 0) — never the merge. The merge returns exactly
  what the old one did for a forest that keeps the routing law (its paging-equivalence row). None of
  these touch an extent grant: ruled out as a cause; the change only moved the row's timing (76 →
  37 s).
- **The recovery custody law (0003 §U.1)** holds a dead grant's retirements from the first root
  install to the table release — it judges the dead grant's record, so it is the screen's victim
  here, not the cause.
- **The grant closure.** `held + unclaimed` is the grant bits' count; only a return outside the bits
  breaks it. A surplus return withdraws through `alloc.withdrawGranted()` — a claim of ANY free
  extent of the mirror. So the mirror's free set held an extent outside the grant bits.
- **How a grant mirror gains a free extent outside its grant.** The mirror marks every extent of the
  volume claimed except the granted runs; an SMO's retirement parks the image on the joiner's own
  tail and releases it into the free set — whatever image it was. The joiner B in the row takes
  `hot`'s slot from A by handover: the tree's images are A's (A's bitmap; no grant record names
  them). B's storm splits and compacts that tree; every retired A image recycles into B's pool.
  The spec's custody move — T1, "the grantee's `extent_grant` record gaining the trees' live
  images, same entry"; T5, "the grant record minus the trees' images"
  (`docs/porting/sym_slot_leases.md` §1, §4.5–4.6; the reference's `transfer_in` / `transfer_out`,
  `grant_record_minus_images`) — was never written in the port: `acquire.Verbs.acquireSlots`
  and `releaseSlot` never touched `extent_grant:` (the recovery agent named the release half as
  owed, `rung-10-recovery-correctness.md` §1.5).

Why it is rare: the pool's next-fit claim cursor reaches a recycled foreign extent at a time of its
choosing; the screen sees it only if the claim's `alloc` is still inside B's ring window when B is
killed (the last checkpoint interval), the closure only if a quiet cadence's surplus return runs
(`unclaimed > 2 × target`) and withdraws it. Any joiner that takes a slot whose tree it did not
grow — a handover, a rotor pick of an unleased slot a recovery left — is exposed the same way.

## 3. The attribution loops

The row's binary and test binary built once per tree (`zig build smoke -Dtest-filter=…` on each
worktree), then looped interleaved — round r runs dev HEAD (`05f9fd2`) and `d3a7845` (the gate-speed
landing, before the merged walk) once each, alone on odd rounds and under 4 × `yes` on even ones —
60 rounds; each run's daemon logs polled out of the rig's work dir (the rig deletes it at deinit),
kept for a failing or tripwire run; pid-tagged rig dirs swept per run; `sync -f /build; fstrim
/build` every 10 rounds (`.diag/loop.sh`, not committed). Another agent's work shared the venue at
times (load average up to ≈ 10 under 4 × `yes`).

| Tree | Alone | Under 4 × `yes` | Tripwire runs | Median run |
|---|---|---|---|---|
| dev HEAD `05f9fd2` | 29 / 30 pass | 29 / 30 pass | **2** — run 30 (4 × `yes`): `extent_grant_closure_violations` at B AND at C, then A's metadata heap FULL (`free=0 claimable`), `cluster_wire_service_refusals`, C's creates parked 48 s → EIO, `xv_cross_owner_intent_stuck` (the row: `expected 0, found 2`); run 31 (alone): `meta_kv_replay_extent_violations` × 38 — A's recovery of B refused `Violation` every poll, `appender_recoveries` never reached 1 | 39 s / 39 s |
| `d3a7845` | 30 / 30 | 30 / 30 | 0 | 104 s / 77 s (its closing fsck: the superlinear walk) |

2 of 60 against 0 of 60 is no evidence for the merged walk (Fisher, two-sided p ≈ 0.5); the fsck
agent's counts (2 / 14 on its branch, 13 / 13 on the dev before it) sum with these to 4 / 74 against
0 / 73 — suggestive of a timing shift, nothing more. The attribution is the code's: **the red row
below is red on `d3a7845` too** (cherry-picked: the T1 assertion fails; with it lifted, "B's pool
holds extent 15 … B's record does not grant it", the same as on HEAD). The defect predates the
merged walk; the walk only moved the row's timing.

Run 30 also exhausted A's metadata heap (`free=0 claimable` of the 256 MiB smoke volume's 990
extents) during a phase-2 storm C ran nearly alone (16,704 creates; A's own create was parked at
`meta_backend_start` for 25 s before the refusal) — the fsck agent's "an intent stuck past the
membership TTL after the metadata heap refused a commit" has the same signature. A closure
violation leaks the foreign extents the return names (the manager ignores them, the joiner's pool
has withdrawn them), but a few leaked extents do not account for 990, so the exhaustion is not
established as the leak's. What the loops show: `heap FULL` appeared in exactly the two runs whose
joiners fired the closure violation (this run 30, and run 20 on the new base, §5.1) and in none of
the 120 runs of the fix (§5).

## 4. The red row and the fix

**Red** (`d715c8a`, Linux, `src/meta/sym/handover_moves.zig` "THE CUSTODY OF A MOVED TREE'S
IMAGES"; A and B real members in one process, B dialled over loopback TCP): A grows a tree of
several leaves under an interior root in a rotor slot (1,500 records of 200 B over 64 KiB nodes), B takes the slot by an accepted
offer; B compacts one of A's leaves through its own SMO context (`smo.compactForDefrag`) and
checkpoints until the free drains. On `dev`: the grant's entry left B's record without the tree's
images (the T1 assertion); with that assertion lifted, **B's pool held extent 15 — the retired image
of A's leaf — which B's record does not grant** (the extent B's next claim would journal as the
`alloc` the screen refuses, or a surplus return would withdraw); after B's leave the record still
named the image B's compaction wrote into the tree A now maintains. No window, no seam: the defect
is deterministic once the sequence happens.

**Fix** (`c6e5306`; 0003 §AD; 0002 row 524):

- T1 — the grant's ONE control entry carries the grantee's record GAINING every live image of the
  granted trees (`Manager.rewriteForImages`, the walk through the new `Seams.treeImages`); the
  joiner's `installWireGrant` takes them into its grant as held under its SMO lock before its table
  word names it the holder (`JoinedSet.transferImages`); an in-process region's RAM grant follows
  (`RegionGrant.transferIn`).
- T5 — the release's entry carries the departing record LOSING the released tree's images; a wire
  lessee's tree is read from the device at the released root with the slot's cached nodes dropped
  first (the lessee appended into them in place — 0003 §X.5's cache barrier); the joiner returns
  them from its grant after the release lands (a failed walk there is loud, never an abort that
  would reopen the door of a slot it no longer holds).
- A joiner's SMO that retires an image its grant does not hold keeps it allocated and fires
  `extent_grant_foreign_retirements` (`SmoContext.grant_holds`): a leaked extent is space, an
  extent two custodians hand out is corruption. The joiner's grant bits sit behind their own
  `bits_lock`, held only across bit operations (never across a wire call) so the guard never waits
  on the manager.
- A move past the record's run cap refuses typed (`extent_grant_custody_cap_refusals`; 4,111 runs
  at the default 256 KiB node). The walk reads interior nodes only. Recovery's rewritten record is
  sized to its runs (a fixed 256-run buffer refused `Corrupt` every poll — a recycled tree image is
  a single-extent run). Counters `extent_grant_images_transferred_in` / `_out`.

**No model.** The custody move runs under the manager's verb mutex, the SMO lock and the joiner's
`bits_lock` — no lock-free core is involved (the mirror's lock-free claim is unchanged; what was
wrong was which extents it was allowed to hold).

## 5. The loops on the fix

The same harness with the daemons' `.stats` snapshotted every ~6 s (`.diag/loop2.sh`): the fix
on the attributed base (`05f9fd2` + the fix) and on the base it lands on (dev `40f5d08`, after the
FUSE worker/handler merge of `.wt-perf3`, + the fix), interleaved, 60 rounds:

| Build | Alone | Under 4 × `yes` | Tripwire runs | Median run |
|---|---|---|---|---|
| `05f9fd2` + fix | 30 / 30 | 30 / 30 | **0** | 37 s / 39 s |
| `40f5d08` + fix | 29 / 30 | 29 / 30 | **0** | 33 s / 34 s |

Every one of the 120 runs: A carried 1 image into B's record at the handover of `hot`'s slot
(`extent_grant_images_transferred_in` 1 — the slot's tree was one leaf when A offered it), B and C
retired no image outside their grants (`extent_grant_foreign_retirements` 0), no closure violation.
The two failures on the new base fired no tripwire and are not the custody class: run 27, phase 3's
view — C's `ls | wc -l` listed 10,365 names where `find` and A listed 10,364; run 30, phase 4 — 11
cross-owner intents still open at A after C's kill (C's create had parked at `meta_backend_start`
before the kill). A first attempt of this loop on the new base alone (stopped after two runs) lost A
to `SIGABRT` in its first run: `fuse/transport/session.zig:642` `Sideband.completeCell` →
`recycle` → `assert(free.tryPush(idx))` (a sideband notify cell recycled into a full free ring or
twice) during B's recovery. On the old base the fix ran 60 / 60 clean (§5.1 for the new base without it).

### 5.1 The new base without the fix

Pristine dev `f344862` (the new base, no fix), the same harness, 30 rounds:

| Build | Alone | Under 4 × `yes` | Tripwire runs |
|---|---|---|---|
| `f344862` | 14 / 15 | 14 / 15 | **3** — run 3 (alone): `meta_kv_replay_extent_violations` × 39, A's recovery of B refused every poll; run 20 (4 × `yes`): `extent_grant_closure_violations` with A's heap FULL, `cluster_wire_service_refusals`, `xv_cross_owner_intent_stuck` (run 30's signature on `05f9fd2`); run 4 passed but fired `dlm_token_nonmember_refusals` × 2 at A ("a member whose lease the owner lists EXPIRED asked for a grant") |

The custody class reproduces on the new base without the fix (2 of 30) and never with it (0 of
60). The fix's two non-tripwire failures on the new base (the off-by-one `ls`, the 11 open intents)
were not seen in these 30 runs, and the old base ran the fix 60 / 60 clean; with no mechanism from
the custody move to a listing or an intent's roll-forward, they stand unattributed (§8), as do the
sideband abort (in the transport's notify path, `.wt-perf3`'s merge) and the `dlm_token_nonmember_refusals`
fire.

## 6. Item 2 — fsck's per-suspect lookups

Owner ruling: performance first. Every lookup below scanned a whole census list per suspect; each
now resolves through an index built once (0003 §AC.6, `ops_fsck.md` §6.2). Red rows (`9a9abac`,
`258c59b`; the fixes `deb3c14`, `f4a2e46`) are counter bounds — work ≤ 3 × the inputs — over the RAM set (`fsck/tests.zig`, every
host) and, for C11, the real set (`fsck/scale.zig`, Linux). The work rides the report:
`c2_owner_steps`, `c9_owner_steps`, `c10_name_steps`, `c11_store_calls`, `c17_route_steps`,
`recheck_derived_steps` (the text form's `lookups:` line).

| Scan | Before (elements examined) | After | Growth before → after | Fix |
|---|---|---|---|---|
| C2-lost's owner per lost block | 25,830 (400 fill + 60 lost) → 97,830 (1,600 + 60) | 60 → 60 | × 3.8 → × 1 | the derived census carries each block's first owner (`Pass.Derived`) |
| C9's block count per candidate | 25,830 (400 + 60) → 97,830 (1,600 + 60) → 412,920 (1,600 + 240) | 520 → 1,720 → 2,080 | × 16 → × 4 (both × 4) | ino → bindings index of the owners, built at the first candidate |
| C10's distinct names per candidate (online re-check) | 14,340 (60 candidates) → 230,160 (240) | 180 → 720 | × 16 → × 4 | one pass over the fresh pass's names → child → distinct count |
| C17's routing check | 6,000 (20 striped dirs × 4) → 94,400 (80) | 234 → 934 | × 15.7 → × 4 | every map's stripes indexed once, sorted; one lookup per view |
| The ladder's C2-leaked / C3 / C6 re-check (found here) | 24,000 (400 fill + 60 leaked) → 96,000 (1,600 + 60) | 400 → 1,600 | × 4 per suspect count | one derivation of the fresh census per volume |
| C11's owner scan (store calls) | flat 2,003 (2 k names) → 8,003 (8 k); striped 2,067 → 8,067 | flat 5 → 17; striped 8 → 32 | — a probe per ino → a page per 512 entries | ONE paged walk of the map kind |

The C11 change on the whole offline run (`fsck/scale.zig`, a real one-member set, 64 slot trees):

| Row | store calls | nodes walked | entries visited | entries read |
|---|---|---|---|---|
| flat 2 k | 2,032 → 34 | 3,782 → 1,880 | 16,054 → 16,564 | 17,308 → 17,818 |
| flat 8 k | 8,104 → 118 | 11,625 → 3,787 | 58,054 → 58,564 | 62,239 → 62,749 |
| striped 2 k | 2,109 → 50 | 4,237 → 2,361 | 22,399 → 24,383 | 24,156 → 26,140 |
| striped 8 k | 8,228 → 193 | 12,696 → 5,014 | 87,847 → 95,693 | 95,519 → 103,365 |

The per-ino probe visited one entry per inode but descended the trees for each; the walk visits
each entry of its pages once (≈ one more pass over the space) and descends once per page: node
reads drop 2–3×, store calls 60–70×.

## 7. Gates

| Gate | Tree | Result | Wall |
|---|---|---|---|
| macOS (before the rebases) | `05f9fd2` + the six code commits | 29/29 steps, 2,244 / 2,273 passed (29 skipped) | 3 m 22 s |
| macOS (rebased on `f344862`) | the same six | 28/29: one unit shard failed the "zero false positives" row (a C8 false positive, the pre-existing flake of §8); its re-run green | 3 m 50 s |
| **Linux, uncached** (`src/core/clock.zig` touched) | `13509f4` + the six | **49/49 steps, 2,858 / 2,931 passed (73 skipped)** | **5 m 34 s** |
| **macOS, uncached** | the same | **29/29 steps, 2,247 / 2,276 passed (29 skipped)** | **2 m 46 s** |

## 8. Owed

- A chunked `extent_grant:` record family: a custody move past ≈ 4 k scattered images per joiner
  at the default geometry refuses (0003 §AD.3).
- The online fsck "zero false positives" row (`fsck/tests.zig`): C8 (durable vs derived refs) and
  C2-leaked false positives under its live writer storm when the host is loaded — on the dev box
  under 8 × `yes`, pristine dev 7 / 40, this branch 7 / 40 (and the two intermediate commits 6 / 40
  each); idle 60 / 60. Pre-existing; it failed one macOS gate here (the re-run was green).
- The data plane's mount-time C11 under the verify lever (`plane.verifyBlockMapsAtMount`) still
  walks owners by `kvmap.nextOwner`, a probe per ino (§6's class, outside fsck).
- The new base's hot-directory failures that are not the custody class (§5, §5.1): the sideband
  abort (`fuse/transport/session.zig:642`, `Sideband.completeCell` → `recycle` → `assert(free.tryPush)`),
  C's off-by-one `ls` in phase 3, 11 intents open at A after C's kill, `dlm_token_nonmember_refusals`
  in a passing run — each seen once or twice, unattributed.
- Naming the extent in the two tripwires' details (the screen's `alloc`, the closure's census
  words): the fault was found without it; a future fire would be found faster with it.
