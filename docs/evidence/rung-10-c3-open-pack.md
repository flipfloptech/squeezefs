# Rung 10 — the merged census's `[C3] refcount 37 vs 36`: an open pack's pin, not a drift

**The finding.** `smoke: ONLINE FSCK THROUGH THE ADMIN LANE` failed at the two-daemon set's
`scrub --online` with `[C3] … refcount 37 vs 36` — 45 of 144 loaded runs on `c39db22`, 22 of 114 on
the reader branch, 4 of 10 on a dev baseline (2026-10-22). It was in no ledger and never chased.

**The verdict: (b), an online census gap.** Instrumented (the merged arm naming the block, the
holder's count, the open-pack ledger, the registry, both sides' referents), 24 loaded runs beside the
full smoke: 9 failed, every one the same shape — the block is the HOLDER's (A's) **open pack**
(`isOpenPack` true, in-flight false), `refcount 37` = 36 committed tenants + the packer's +1 pin,
the merged references 36 = A's census 33–35 + B's custody 1–3. After both dismounts (the dismount
seal releases the pin) `fsck --offline --scrub` was **clean in all 9**. The count is the pin
arithmetic exactly (`committed + in-flight + 1` with every committed tenant a referent), and the
block's 36 referents are every tenant the open pack took.

**Why 37 and 36, and why only under load.** The fill's 200 × 64 KiB files land on two data volumes:
100 tenants each = one sealed 64-tenant pack + one OPEN 36-tenant pack per volume (no idle seal, by
design). Unloaded, the fill is still staged at the set's scrub (15 blocks read: the three big files);
loaded, a syncfs reaches A's mount and promotes it (`fsync_promoted_files 200`, 215 blocks read).
The merged arm judges only blocks a MEMBER references: B's 64 leased slots hold 1–3 fill inodes in
the runs that failed (the slot routing decides; the other runs promoted with no member tenant in
an open pack).

**The gap.** `classes.crossMemberBlockPlane` — the coordinator's merged lost/C3 arm over its own
census and the members' — shielded the in-flight registry and the lessee's bitmap bit, but not the
pack-open ledger the whole-view ladder's Phase A has (`ops_fsck.md` §3.3: "an open pack reads
`rc = tenants + 1`"). **Fix 1** (`e0835f0`): `liveOwnerShielded` — the registry first, then the
pack-open ledger — is ONE function for Phase A and the merged arms; an open pack's count is judged by
its seal's census and by C3 once sealed.

**The snapshot law (fix 2, `27b7ba3`, found reading the same arm).** The confirming round re-read the
members' census but reused the holder's PASS-START census (`parts[0].owners`, taken before the settle
and the scrub): a reference the holder released in between read as the same `refcount N vs N+1`
twice and was reported. `runner.confirmMerged`: every side's references are read after the first
round's shields — the members' second census and the holder's view census'd again — and the
allocators after both.

**Red tests** (`ops/fsck/tests.zig`): "the merged census's C3 passes the pack-open ledger …"
(`[C3] vol-…10 block 10 — refcount 4 vs 3`, the row's shape) and "the merged arms' CONFIRMING round
reads the holder's references fresh …" (`confirmed: [C3] … block 7 — refcount 2 vs 3`); both 100/100.
Each keeps its genuine-drift arm: a sealed pack one above its referents, a drift standing in both
rounds, is still the finding.

**Counts** (the row's binary 4-wide beside the full smoke, `scripts/venue`, one slot each). Before
(`ac7986e`, instrumented): 9 of 24 failed, all 9 the open pack, offline clean 9/9. After: **32/32**,
0 C3 findings; the merged arm's pack-open ledger engaged in 13 runs (10 × one volume's open pack,
3 × both) — the runs that failed before; 16 runs never promoted the fill, 3 promoted with no member
tenant in an open pack. The row now prints the set scrub's `pack-ledger exempted` and `merged
suspects cleared`. Rows 9/9: FSCK AND SCRUB, ONLINE FSCK, THE FLEET'S MERGED BLOCK CENSUS (its
genuine C2 lost still confirmed through the fresh round), THE FLEET READ SHARD, THE JOBS MATRIX,
THE REKEY ROW, the data plane WITH the write cache, FILE HANDLES (hard links), RENAME_EXCHANGE's
nlink. Unit filters `fsck` 117, `scrub` 46, `pack` 99, `refcount` 67 passed (smoke-tier skips
only); rails 32/32; `zig fmt --check` clean.
