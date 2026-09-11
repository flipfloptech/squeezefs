# 2026-09-11 — P0: the corpse sweep's double release freed live blocks (fixed `86eff517`)

**Verdict: a real data-loss defect on the SHIPPED path, caught by the
1.2.3 release chain's fstests leg (`generic/749`, test 739 of 787), root-
caused on the run's own scratch volume, fixed red-first in two halves, and
accepted on that same volume.** The mount-time corpse sweep (landed
`710e8cd5`, 2026-08-23) released each corpse's durable block references
and `begin_free`d its blocks, then destroyed ALL corpse records in ONE
transaction; with ~68 k corpses that entry (6 MiB) exceeded the journal's
128 KiB whole-entry cap and FAILED, so the records survived with their
references already gone. The next mount seeded RAM refcounts from the
ledger and re-ran the sweep: a surviving corpse's `begin_free` on an offset
a LIVE file now owned decremented the live owner's count and, at zero,
terminally freed (punched) the live block under its layout. Under packing
(default ON since `fe714e6b`) offset 0 is the pack block every promotion
lands in, so it was live at every cycle mount; 1.2.2 has the same double
release and hit it only when a re-minted single-owner block sat at a stale
corpse's offset.

Tree: `dev` @ `3c71742e` (red) → `86eff517` (green). Venue: the laptop;
the tape is the release run's scratch volume
(`/dev/shm/squeezefs_fstests_scratch_{meta,data}`, 68,099 corpses).

---

## 1. The failure

`generic/749` case 2: `falloc 0 11k`, `pwrite -S 0xaa -b 512 0 12291`,
`syncfs`, `_scratch_cycle_mount`, an mmap READ of the page tail past EOF,
`_scratch_cycle_mount`, an `mwrite` of the 3-byte tail, `syncfs` →
`Expected csum 5cdc… Actual c0c9…`. Reproduced on the tape
(`/tmp/g749_live.sh`): **the file already read as 12,291 ZERO bytes right
after the FIRST cycle mount** (md5 `c0c9…` = 12,291 zeros); the `mwrite`
merely re-staged the zeroed image (the `.out.bad`'s later 0x58 fill is
xfs_io's default byte over that).

The scratch daemon log at every mount of the run's last hour:

```
block ownership recovered from DURABLE records: 2 reference(s), no inode-tree walk
mount-time corpse sweep: 68099 unlinked inode(s) a prior era never reclaimed … releasing their references and blocks
begin_free REFUSED untracked offset 0: no refcount entry — double-release lineage        (6,372× over the run)
mount-time corpse sweep failed: … journal entry length 6004284 exceeds the 131072-byte whole-entry cap
```

Corpse population across the run: 30,645 at the first over-cap failure
(02:04:37, a 1.5 MiB entry) → 35,289 → 68,093; 323 failed sweeps vs 180
successful smaller ones. Per mount, 32 corpses named offset 0: 2 landed on
the two live tenants' counts (→ 0 → punch), 30 were refused untracked.

## 2. The mechanism, as verified in code

1. `sweep_unlinked_corpses` → per corpse `delete_file`: `release_block_refs`
   (its own commit) + `free_blocks` (RAM `begin_free` per mapped block,
   UNCONDITIONALLY) → then ONE `destroy_inodes(all)`. Over the cap the
   destroy fails; the records survive; the references and RAM counts are
   already gone.
2. Next mount: `recover_durable_block_refs` seeds RAM counts from the
   ledger (the live owners only). The sweep re-runs; a corpse's `Delete` of
   its absent record is a no-op; `begin_free(offset)` refuses when NO entry
   exists (the storm) and DECREMENTS when a live owner holds the offset.
3. The incarnation guard cannot catch it: `incarnation_ok` returns true on
   an UNKNOWN live lifetime, and at mount init no offset has a lifetime
   word until a re-mint (`recover_block`/`seed_from_durable_refs` seed
   counts only).

Two things the first reading got wrong: the derived oracle has COUNTED
`nlink == 0` layouts since 2026-08-23 (so the released-but-undestroyed
state was already visible as C8 drift, `N derived vs 0 durable` — the
design's "the oracle skips corpses, no drift" sentence was stale), and the
FUSE reclaim batch already bisects its destroy (`destroy_batch_bisect`),
so the over-cap failure was sweep-only.

## 3. The fix (`c4060774` + `ba335913`; red `3a26933a`)

**The release witness (the data-loss half).** On a ledger volume a
REFERENCE release may decrement a RAM refcount only if THIS owner's durable
record existed at release time — the ledger is the truth for "does this
ino hold a reference". `KvMetaBackend::commit_block_refs_witnessed` probes
each release (a `TREE_BLOCK_REFS` point lookup under the same 4a guard as
the commit) and answers `ReleaseWitness::{Derived, Shipped, Ledger(held)}`;
`delete_file` builds a per-`(vol_tag, block_idx)` free budget from the held
records plus this process's RAM-only pending TAKEs and retains only covered
keys; the rest are counted `block_release_skipped_no_record` (stats inode,
one WARN per corpse). A FAILED release commit now frees NOTHING (it used to
free anyway). Non-ledger volumes keep the derived posture (the walk seeds
every surviving layout's references, corpses included — self-consistent);
a peer-owned ino's frees ship and the authority's executor validates them
against its own ledger. `incarnation_ok`'s unknown arm stays: it is
load-bearing for every recovered block's first post-mount read/free, and
the witness refuses the stale corpse before the funnel.

**Chunked destroys (the leak half).** `RoutedMetaBackend::plan_destroy_chunks`
prices each corpse's destroy (`KvMetaBackend::destroy_entry_bytes`: the
inode `Delete` + one per xattr, in `journal::record_frame_len` — the same
framing the entry admission uses; bound = `entry_payload_cap()` =
`MAX_ENTRY_LEN − ENTRY_HDR_LEN`) and cuts at the cap; the sweep runs
release + free then destroy PER CHUNK; a chunk failure is loud and stops the
sweep (the remainder untouched — leak-safe). No per-mount wall bound: the
68 k `delete_file`s ran in ~1 s on the tape and a bound had no derivation.

## 4. Contracts (red quoted from the unfixed tree → green)

`tests/durable_block_refs_tests.rs`: `a_corpse_whose_reference_was_already_
released_cannot_free_a_live_owners_block` — RED `the corpse's STALE release
decremented the live owner's refcount … left: None right: Some(1)`;
`an_overcap_corpse_population_is_reclaimed_across_chunked_destroys` (4,300
corpses, derived from the cap and the record framing) — RED `journal entry
length 147476 exceeds the 131072-byte whole-entry cap`.

`tests/corpse_sweep_tests.rs` (new, mount-class; seam
`SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY` registered): (a) a released-but-
undestroyed corpse + a live file at its offset → RED `the LIVE file is
unreadable after the corpse sweep (Input/output error)` → green byte-exact,
drift 0, `block_release_skipped_no_record 2`, refusals 0, tripwires 0;
(c) the exact `generic/749` shape → RED `generic/749's packed file reads
size-consistent ZEROS (the block was punched)` → green through both cycles;
(b) 6,142 held-open corpses + kill -9 → RED the over-cap failure → green
`corpse sweep reclaimed 6142`, fsck clean, the next mount finds none.
`tests/pack_compaction_tests.rs` pins that the mover's census never yields
a corpse referencer (`census_for` skips `nlink == 0`).

## 5. Tape acceptance (the fixed binary on the run's real scratch volume)

```
after pwrite:             md5 5cdcb5e6…  refusals 0  tripwires 0
cycle 1:                  md5 5cdcb5e6…  (packed_reads 1)
cycle 2:                  md5 5cdcb5e6…
after mwrite tail + sync: md5 5cdcb5e6…  SAME   byte histogram {0xaa: 12291}
```
`corpse sweep: 68099 unlinked inode(s) … in 46 destroy chunk(s)` →
`corpse sweep reclaimed 68099` in ~1 s; 32 `block release(s) skipped`
WARNs (the 32 offset-0 corpses); **0** `REFUSED untracked`; 0 tripwires;
the next two mounts find no corpses.

## 6. Release-site classification (every path that reaches `begin_free`)

- **Gated** (a reference release from a layout whose record may already be
  gone): `DataRouter::delete_file` → `free_blocks` — the FUSE reclaim batch,
  the mount-time sweep, fsck C9 repair.
- **Not gated — same-transaction releases** (the record `Delete` and the
  mapping change are ONE entry; the RAM count includes the reference by
  seed or by allocation): truncate/punch removals, the kvmap sweep chunk +
  corpse handoff, the five `fuse_client.rs` displaced loops after
  `merge_block_mappings`, the displaced indirect blob after a save, the
  movers' `free_block` after `MergeExpected` (the census never yields a
  corpse — pinned), the served-compose blob free, the co-writer executor
  (its population shield IS the ledger).
- **Not gated — never-published mints / RAM pins** (no record can exist):
  failed-commit fresh mints and allocation rollback, the clone-pin undo,
  the own-mint orphan blob, `release_pack_tenant_reference` and the pack
  seal's pin, the movers' source/dest pins and raise undos, the allocator
  wrappers, fsck C2's free of a zero-referencer leaked block.

## 7. Residuals (pre-existing, stated, not fixed here)

- A FAILED release commit followed by a SUCCESSFUL destroy (the reclaim
  batch's log-and-proceed) still orphans durable records forever — a
  permanent C8-visible leak, not this bug.
- A single corpse whose own destroy records exceed the cap fails loud at
  every mount (a one-corpse leak).

## 8. Release status

1.2.2 ships the double release (any volume with an over-cap corpse
population — abort-unmounts, kill -9, a busy kernel inode cache — and a
re-minted block at a stale corpse's offset). 1.2.3 carries the fix; the
release notes name it. Design doc amended in `86eff517`
(`docs/design-durable-block-refcounts.md` §6 and the sweep's comment).
