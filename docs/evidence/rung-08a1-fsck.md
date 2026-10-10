# Rung 8 wave A1 — fsck C1–C17 and the scrub: OUR oracle over the landed oracles

Spec: `docs/porting/ops_fsck.md` (§1 the engine and the zero-FP ladder, §2 the census, §3 every
class, §4 the scrub, §5 shards + `merge-reports`, §6 the report, §7 the harnesses, §8 the repair
engine, §9 knobs, §11 quirks, §12 the port plan). Charter §8 "fsck", §5.5 "fsck classes on the
forest", §5.6 (C8), §6.1 (C12), §1.8, §1.10. Decision 0003 §G; decision 0002 rows 395–407.
Modules: `src/ops/fsck.zig` (root: constants, the `fsck` stats family) and `src/ops/fsck/{finding,
census, classes, repair, scrub, runner}.zig`; the CLI verbs `src/cli/fsck.zig` (+ the grammar in
`args.zig`); the test fixture `ram_set.zig` and the contracts `tests.zig`; the smoke's `FSCK AND
SCRUB` row and the kill matrix's `fsck --offline --scrub` oracle.

## What landed

- **ONE tagged finding union** (`finding.zig`): 27 arms over C1–C17, the tag the class, the payload
  the byte-exact identity (block findings carry `vol_tag`/`block_idx`/`(ino, block_index)`; the
  inode plane carries GLOBAL inos, the block plane KEY inos; C10 dangling the dentry RECORD
  `(parent, suffix, child)`). `class`/`severity` (`loss` = the stop-and-read signal)/`verb`/
  `reportOnly` derived, never stored. Identity order over the tag and the fields (key buffers by
  bytes), `Report.normalize` = sort + dedup. The report: schema 1, `mode`, `shard`, the counters
  (SUM/MAX merge classes), the repair rows, `findings_elided`; the JSON writer + the `std.json`
  reader (every arm round-trips, `u64` past `i64` through `number_string`, hex key buffers, enum
  tags); `boundedJson` (the binary search over the prefix; the elided tail IS a finding for the
  verdict); the text form; `exitCode` 0/1; `publish` folds the counters once per run and fires
  `fsck_findings` once on a non-empty report. `merge`: findings concatenated + normalized, census
  counters summed, per-run gauges maxed, `shard = null`.
- **The census** (`census.zig`): the ONE store face (`range`/`lookup`/`begin`/`commit` over the
  kv_fs shape, `merged_kinds` naming the set's mixed-leaf walk) with the refs and map oracle faces
  derived from it; `InoBitmap` (one bit per ino ever allocated, dense per keyspace, the root never
  indexed, the CEILING an allocation authority — a dentry's child never sizes a vector —, the byte
  budget ⇒ `truncated` = no verdict, `mark` answering `newly_set`, the word-parallel `AND NOT`
  difference); the derived budgets (`max(budget/64, 16 MiB)`, `/64` entries, `/8` candidates,
  `meta_lanes × 4`) with tie tests; `SlotScope` (leased / foreign-live / unleased, the start
  watermarks, the era floor with tree 0 as witness and the probe's prior-era posture, the foreign
  windows, the `-o ro` abstention, `unjudged` per posture) as ONE snapshot every walker reads
  (0002 row 405); the kind pagers over the ino-major space (a merged page's foreign kinds filtered,
  the resume at the next key of the kind, never `key ‖ 0x00`; the raw mode for C1; the census-hold
  seam); the inode pass (bounded at the start watermarks with the SLOT JUMP; the shard filter by
  the global residue; `live` / `corpses` / `dirs` bitmaps; `odd_nlink` under the entry budget) and
  the layout pass (the C8 derived half with corpses INCLUDED — the corpse-census correction —, the
  C12/C7 mapping list, the C11 (b) nominees, the unresolvable bindings, and the TOLERANT base count
  of a window the law refuses so an Overrun never reads as C2 lost); the dentry pass (the referenced
  bitmap, C10's `multi` map, the collected identities deduped by record, C17's views keyed by the
  directory's global, the foreign-window and projection scoping, `complete = false` on a tear).
- **The classes as wrappers** (`classes.zig`): C1 the raw walk (one pass on a merged store;
  ordering, the kind's schema, the forest codec's defect classes; a window-law violation is C12's,
  not a schema fault), C2/C3 against the allocator (lost on every shard; leaked/C3/C6 unsharded;
  the holder bitmap's SET bit exempting a former lessee's mint), C6 both arms (`data_bitmap.
  verifyBitmap` LOSS only; the flat arithmetic with limbo/in-flight/trim excluded; a grant-armed
  non-holder declined), C4/C5 over the staging face, C8 `block_refs.verifyRefs` with the pack and
  in-flight exemptions, C11 `kvmap.nextOwner` + `verifyBlockMap` (+ the census nominees), C12 the
  interval sweep per `(vol_tag, block_idx, incarnation)` (same-`off` prefixes collapse; the Overrun
  arm reads the raw wire), C13/C14/C15 over the forest face (`lease_table.settleEntry` `.conflict`
  + the two-pages face + the death ledger), C16 `shared_index.sharedIndexDrift` twice, C17
  `stripe.verifyStripes` over the pass's views, C9 the word-parallel difference through the gates
  (lessee scope → window → THE era floor → the open intent), C10 the count arms and the REVERSE
  difference with the dangling identity walk. **The ladder**: Phase A (the per-cell epoch word
  `≥ scan_epoch`, the pre-registry hook, the in-flight registry, the pack-open ledger), the settle
  (`SQUEEZEFS_FSCK_SETTLE_MS` then `fsckBumpEpoch`), Phase C per class under the object's exclusive
  4a lease (`Host.lockIno` / the canonical `lockInos` for pairs and recounts — two inos on one stripe
  self-deadlocked as two acquisitions) over ONE memoized fresh census / dentry pass / forest census
  / `verifyRefs` per volume / shared-index drift / stripe pass (0002 row 406); the C10 `(nlink,
  ctime)` witness bracket; `block_refs.confirm` the verdict site over the confirmed rows only.
- **The repair engine** (`repair.zig`): plan = apply's input, dry-run the default; the apply order
  (inode plane, C9 before C10, then identity order), verify-before-repair per class with its
  refusal reason, the quarantine BEFORE the commit (a RAM sink in tests, the `DirSink` per-run dir
  with `fdatasync`ed files + a rewritten manifest on Linux), the abort seam after the quarantine
  and before the commit (idempotent re-detection), `census_stale` after an inode-plane raise, the
  17 verbs: C1 rebuild-in-place through the raw-key deletion gate (a known kind in a wrong shape
  dropped under its declared kind; unknown/truncated never), C2 free-leaked REFUSED while the
  durable ledger names the block, C2 lost verify-content (the scrub's verifier) → allocator repair
  or the DAMAGED flip, C3 recount under the canonical leases, C4/C5/C6, C7's flip (supersession-
  safe: only where the captured mapping is still current), C8 through `block_refs.repairPlan/
  stageRepair` offline-only (the fabricate direction refused), C9 destroy (quarantine the inode +
  every xattr with the block keys in the manifest, ONE tx of releases + destroy, blocks through
  the terminal-free law), C10's one ladder + the dangling removal under the parent (`DT_DIR`
  refuses), C13/C15 through the forest face, every report-only class refused with its reason.
- **The scrub** (`scrub.zig`): the exact tenant window, `max_stored_image_len` rounded to the DMA
  grain on a transformed volume, AEAD / frame / READABILITY-ONLY (the honesty gauge
  `scrub_readability_only`), sorted by `(volume, offset)`, `SCRUB_BATCH` per pacer unit with the
  duty-cycle law `elapsed × (100 − pct)/pct` (tie test at 25/50/75), the read-fault seam, the
  ladder (the current layout must name the mapping verbatim; `pinBlockValidated` ⇒ `pinned`
  re-verify / `pinned_unstable` cleared / `refused` cleared).
- **The harnesses** (`runner.zig`): `run` = pass 1 → nominations → settle → re-check → the scrub
  over a FRESH post-settle census → report → repair → publish. Linux: `bind` over the `MetaSet` +
  `DataPlane` (every slot this open's, the cursors as watermarks AND floors — conservative for a
  host bound after the open —, the probe posture offline), the device reader (`readInto`, DMA-
  aligned image), the plane's locked transform behind the scrub face, the pack-open ledger, the
  staging ring face (residents / `get` / `remove` / the marker re-read), the forest face off the
  devices (appender 0's four slots + the directory chain, tree 0's `slot_state`, volume 0's death
  ledger), `DirSink`; `JobBody` = the fabric's `JobType.fsck` body (the job's cancel/pause intent
  as the stop latch, its live duty as the pacer, the last report's bounded admin view).
- **The CLI** (`cli/fsck.zig`, the grammar): `fsck [--online|--offline] [--json] [--scrub]
  [--throttle PCT] [--shards k/N] [--repair [--apply] [--quarantine-dir DIR]] TARGET`, `scrub`,
  `fsck merge-reports [--json] FILE…`; the lattice as usage refusals; offline through
  `offline.withSet` (the writer door: a live mount refuses by name) with the plane bound; the
  quarantine home `<first cache path>/quarantine` or `--quarantine-dir` (required on a cache-less
  set under `--apply`); exit 1 iff findings; `--online` refused BY NAME until the admin lane
  serves the fabric's `AdminJob` seam (the `job pause/resume` precedent). The
  `SQUEEZEFS_TEST_FSCK_INJECT` seam plants one defect under the guarded open.
- **Observability**: the `fsck` stats family (`fsck_*`, `scrub_*`, `fsck_repair_*`,
  `fsck_repair_class[17]`; `fsck_findings` and `fsck_tenant_overlap_findings` tripwires;
  `fsck_slot_custody_conflicts` / `fsck_unrecovered_appenders` / `fsck_findings_last` gauges);
  `SQUEEZEFS_FSCK_SETTLE_MS` (A/B lever, 2000) and the inject seam as registry rows.

## Red → green (portable, over the RAM set)

A healthy populated set (dirs, striped files on two volumes, a clone sharing a block, a hard link,
two packed tenants of one block) reports 0 findings offline and online with every counter engaged;
then one defect per class: C9 (orphan: finding → dry-run plans and mutates nothing → apply destroys,
releases 2 references, frees 2 blocks → clean; a CURRENT-era orphan is shielded,
`current_era_exempted` moves), C10 (too high lowers, too low raises, zero-named is `loss`, dangling
removed under the parent, directories and hard links never findings), C2 leaked frees / C2 lost
recovers the allocator after the content verifies / C3 recounts; C2 leaked REFUSES under a standing
ledger record; C8 the dropped record is the fabricate direction (refused), the stray record released
offline and refused online, `fsck_repair_class_c8` moves; C1 torn (quarantine-report-only, persists),
semantic (deleted after quarantine), the raw-key deletion gate; C7 a corrupt PLAIN block is
readability-only (no finding — the honesty gauge), a corrupt lz4 frame is a finding whose repair flips
the mapping DAMAGED and the next scrub skips it; C11 (a)/(b) and C12 overlap/overrun report-only with
their refusals, the clone's identical window and the same-off prefix legal; C6's bitmap LOSS a
finding, a LEAK counted only; C14 pairs + C15 (dead with window entries; `Recovering` unledgered)
and the mount's `.conflict` refusal from the same oracle; C13/C16/C17 through their faces (C13 applied
then healed; C16 twice; C17 `misrouted_name`); C4 quarantined then retired and C5 moved aside through
the staging face; the kill-9 convergence seam (aborted after the quarantine ⇒ nothing mutated ⇒
re-detected ⇒ the second apply plans zero); shards 0/3,1/3,2/3 merged ≡ the whole inode-plane report
with a shard judging only the lost arm; **the zero-FP storm**: two writer lanes (create → the record
then the name → a block minted under the in-flight registry and published → unlink in ONE tx →
destroy + the terminal free, every op under the canonical 4a acquisition) during ten online runs ⇒ 0
findings while the shields' counters grow; Phase A's epoch/registry exemption vs the genuine leak the
next offline run reports; the pack-open ledger and the open intent shields; forest scoping (a foreign-
live slot's orphan is its lessee's; a reader's uncovered window abstains; an unknown floor is fail-
closed until tree 0 witnesses the slot unleased); the start watermarks (a record at/past the
watermark is the next census's) and the report JSON round trip of a run.

## Linux (the venue, `scripts/venue zig build gate`)

- The kill matrix now runs **our oracle**: `squeezefs fsck --offline --scrub` after each of the three
  mid-`dd` kills, after the storm kill and after the clean unmount — `findings: 0 (clean)`, exit 0 —
  beside the mount-time C8/C2 lines.
- `smoke: FSCK AND SCRUB`: format (a plain set with a cache path) → fill over two mounts (three
  20 MiB `fsync`ed striped files; then, after a remount, a directory tree, 200 packed 64 KiB files
  and hard links) → umount → the offline `clone` → `fsck --offline --scrub --json` 0 findings over
  ≥ 210 inodes and ≥ 20 blocks, every block `scrub_readability_only` (a plain volume carries no frame
  checksum — the honesty gauge equals `scrub_blocks_scanned`, `scrub_failures = 0`) → mount, the 8-way
  storm, kill -9 → `fsck --offline` clean (its guarded open replays the journal; the remount reads the
  checkpointed set) → remount, clean umount → a planted stray reference: the planting run (its
  allocator seeded before the plant) reports the ledger's `[C8]` (exit 1); the reopened dry run
  reports exactly `[C3]` + `[C8]` on that block (the next open seeds the refcount from the stale
  record — C3 reads the word, C8 the ledger) and mutates nothing; `--repair --apply` applies both
  (`0 planned, 2 applied, 0 refused`: `recount_and_set_refcount`, `release_stale_block_refs`),
  clean → a planted orphan inode is exactly `[C9]`, destroyed on apply, clean → two residue shards'
  `merge-reports` ≡ the whole run.
  - WHY the fill spans two mounts: a mount holding staged residents beside fresh striped blocks asserts
    in the dismount's pack promotion (`block_alloc.nextFreshBlock`'s virgin-bit law via
    `staging.promoteAllAtDismount → promotePacked`; 20 × 64 KiB staged files + 3 × 20 MiB
    `dd conv=fsync` → `umount` reproduces it at HEAD). A data-plane defect this row records; fsck's
    object is the set the mounts leave behind.
  - A transformed (lz4) set's first striped publish refused in `layout.Mapping.check`: the chunk of a
    transformed volume is `block + 4 KiB` (the frame header), so the offset test is a modulus, and
    `derive.isAligned`'s power-of-two assertion was the wrong oracle. Fixed in the same wave with a
    tie row (`xchunk = 4 MiB + 4096`) in `layout.zig`'s mapping-law ties.

## Owed / stated

- The `FSCK AND SCRUB` row's storm segment shares the kill matrix's structure (an 8-way `cp` storm
  judged by claimed size inside 60 s, `kill -9`, `umount2`) and so its flake disposition (rung 6's
  evidence note): one of seven consecutive venue reruns of the committed tree failed inside that
  segment before the post-kill fsck line printed (the detail was not captured); the other six and the
  full gate's run passed every step, and every fsck verdict the row ever printed was `findings: 0`
  or the planted class.

- The ONLINE submission: `fsck --online <mountpoint>` refuses by name until the daemon serves the
  admin lane; the engine's online face (`JobBody`) is complete and the portable storm row is its
  zero-FP evidence. The joined posture's `SlotScope` (foreign-live lessees, the ring windows) is
  the fabric's binding at mount; the harness binds the one-process set.
- C13 on a mounted N-daemon set is the live-appender arm over the grant pool; the offline harness's
  census is empty by construction (the open's recovery wave returns every dead grant). C16's lists
  on a real set come from the clone protocol's index home (the face is bound by the symmetric
  plane's owner).
- The measured floors of the spec (scan/census 0.677 at 1,001,014 inodes; scrub/seq 0.752) are
  carried as the derivation's WHY; the venue's null_blk rows are the fabric wave's.
