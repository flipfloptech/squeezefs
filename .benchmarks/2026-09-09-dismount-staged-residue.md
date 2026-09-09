# Dismount "Remaining local staged files: N" — what the residue is (2026-09-09)

**Question** (1.2.2 release gate, `.benchmarks/2026-09-08-1.2.2-release-gate.md`
§6, P1): 47 of 407 fstests dismounts reported 1–2 remaining local staged
files, four reported 200+ (generic/795's remounts 210/225/234/293), and the
fstests TEST device reported **exactly 2,193 on every test cycle** of run 2.
Orphans of deleted files, or acked bytes lost at umount?

**Verdict: (ii), sharpened.** The count is the resident population of
**staged-LAYOUT files** — every file whose size is in
`(MAX_INLINE_SIZE = 4 KiB, block_size]` on a volume with a staging dir. Their
bytes are acked, `fsync(2)`-durable **on the local staging ring only**, and
are **never promoted to the shared data backend** unless the staging pool
crosses its 75 % high-water mark. A clean unmount does not promote them, the
dismount drain wait cannot drain them (it stalls for the full
`--dismount-wait`), a same-host same-mount-point remount recovers them
byte-exact, and **every other client of the volume set — another mount point
on the same host, or another host — reads them as size-consistent ZEROS,
silently**, with a log line that blames a crash that never happened. Nothing
is lost at umount and nothing "gave up": this is a steady state, so the WARN
text, the umount CLI's "[w] Wait … drain/flush" option, and the 10 s constant
are all misdescribing it. No fix was landed (the mandate: fix only for (iii));
the fix plan is §7.

Tree: `dev` @ `b9354366`. Instrument: the release binary of this tree
(`squeezefs 1.2.2 (b935436626f9) profile release`), unprivileged file-backed
sandbox on the dev laptop (`SQUEEZEFS_FUSE_ZC=0` — the fstests runner's
posture). Scoping evidence by the venue rule; every number below is a count or
a wall clock, not a throughput.

---

## 1. What `list_staged_files()` counts

`NvmeStaging::list_staged_files` (`src/cache/nvme.rs:2361-2367`) returns
**every key in the staging ring** (`staging_nvme_cache.list_keys()`), which
holds three record kinds:

| key shape | what it is | budget-counted (`staged_ledger`) | promoted/flushed by |
|---|---|---|---|
| `active_block:inode_N:block_B[.scope]` | whole-block write custody of a STRIPED file awaiting writeback | no (`nvme.rs:1261-1269`) | the writeback worker; the dismount sweep `flush_all_staged_blocks_to_backend` (`src/fuse_client.rs:21888-22036`) |
| `active_block_ext:inode_N:block_B` | W2 extent record | no | fsync/teardown fold (`fuse_client.rs:21894-21910`) |
| `<uuid>` (a `file_id`) | the WHOLE payload of a **staged-layout** file | yes (`nvme.rs:1270-1278`) | the merge worker — **only** under pool pressure (§1.2) |

The dismount census (`fuse_client.rs:22931-22941`) splits on the
`active_block:` prefix: `active_writes_count` = the first row,
**`staged_count` = everything else**, i.e. the `file_id` rows (extent records
too, but a clean teardown folds them — `21894-21910`). The stats inode's
`nvme_staged_write_file_count` (`fuse_client.rs:10592-10620`) is the same
split, and `squeezefs umount` reads it to decide "unflushed"
(`src/main.rs:7360-7380`).

### 1.1 A staged-layout file's bytes exist in exactly one place before promotion

The staged write arm (`src/routing.rs:16513-16607`): a write that lands a
file in `(4 KiB, block_size]` on a volume with staging dirs calls
`stage_write` (ring entry keyed by `file_id`, `nvme.rs:1357-1533`) and then
publishes the RAM layout `file_type = "staged", file_id = Some(uuid),
block_map = None, layout_dirty = true` (`routing.rs:16557-16565`) —
"Layout: staged — mmap stage + RAM meta; MetaLV layout deferred to fsync"
(`16516`). **No data-device write happens.** `fsync(2)` on such a file
(`flush_inode_to_backend_prof`, `fuse_client.rs:20561-20687`) does:
`sync_key(file_id)` on the staging ring (`20641-20643` / `20656` →
`NvmeCache::sync_key`, `src/tiering/nvme.rs:1723-1743` → `File::sync_all` on
the shard file, `:1214-1219`) and then `persist_dirty_layout_if_needed`
(`20678-20680`), which commits `{staged, file_id, block_map: None}` to MetaLV.
So after `fsync` the **metadata is on the shared volume and says "the bytes
live in ring entry `<uuid>`"**, and the bytes live in that ring on this
host's local disk. Nowhere else. (`tests/staged_crash_recovery_tests.rs:4-8`
states "acked+fsynced staged data is promoted to durable blocks by fsync";
the code does not do that — §3 leg dw60 measured it: 50 explicit `fsync`s,
count unchanged.)

### 1.2 Promotion is pressure-driven only

`promote_staged_file` (`routing.rs:13414-13582`) is the primitive that
allocates a backend block, DMAs the image, commits `block_map[0]` and
releases the ring entry. Its ONLY caller is the merge worker's
`promote_batch` (`nvme.rs:2145-2178`), fed by `write_tx`, which is fed by:

* `stage_write`'s high-water arm — enqueue this entry only when
  `current_staged_write_bytes > max_write_bytes − max_write_bytes/4`
  (`nvme.rs:1521-1532`: "Keep the hot path enqueue-free (promoting every
  small stage to the backend competed with create/fsync), but arm the drain
  *before* the pool hard-fills");
* `stage_write`'s over-cap arm — `kick_promotion(…, 64)` (`1416-1440`).

There is no idle promotion, no fsync promotion, no dismount promotion. With
`--disk-cache-size 500MB` the write half is 250 MiB (16 shards ×
16,384,000 B), so the high-water mark is 187.5 MiB: the fstests TEST device's
2,193 small files never reached it and were never promoted — hence the same
number on every cycle.

### 1.3 The dismount path never touches them

`run_dismount_teardown` (`fuse_client.rs:22844-22950`):

1. `write_pipeline.quiesce(dismount_wait)` (`22855-22868`);
2. overlay drains (`22874-22881`);
3. `let _ = self.force_flush_all_staged_data().await` (`22884`) —
   `force_flush_all_staged_data` (`22038-22042`) itself `let _ =`s BOTH
   halves: `flush_all_memory_buffers_to_staging` (RAM `active_block_buffers`
   → staging/write-through) and `flush_all_staged_blocks_to_backend`, whose
   sweep **filters to `key.starts_with("active_block:")`** (`21919`) — the
   `file_id` entries are skipped by construction, and the summary's own
   `warn!` on failure (`22024-22028`) is the only report the swallowed
   `Result` ever gets;
4. the drain wait (`22886-22905`): loop until
   `staged_writes_in_flight == 0` or `dismount_wait` elapsed.
   `staged_writes_in_flight` counts **every ring key** — seeded at mount with
   `list_keys().len()` (`nvme.rs:1321-1323`), incremented per NEW `file_id`
   stage (`1509-1512`) and per new active block (`1762-1765`), decremented
   only by `remove_staged_if_generation` (`1619-1626`) and
   `remove_active_block` (`1875-1881`). With N resident staged-layout files
   and nothing in step 3 removing them, **the loop can only exit on the
   timer** — every unmount of such a mount stalls for exactly
   `dismount_wait` (§3: 10.08 s / 60.09 s wall);
5. the census (`22931-22941`) and the WARN (`22943-22947`).

The `squeezefs umount` CLI's interactive "[w] Wait for staged files to
drain/flush to NVMe-oF backend (recommended)" (`main.rs:7398-7400`) polls the
same counters and therefore can never succeed for this population either; its
"[c] Continue unmount now (staged data stays on disk; the next mount recovers
it)" (`7402`) is the accurate sentence in the whole surface.

## 2. The remount contract

**Recovery scan** (`NvmeStaging::new`, `nvme.rs:1093-1333`):

1. generation gate FIRST — `bind_staging_generation(dir, fs_generation)`
   (`1117-1131` → `314-447`). The **staging generation** is
   `writer_scope::staging_generation` (`src/writer_scope.rs:720-729`) =
   `"{volume-set generation}@node:{node_token:016x}.m{mount_slot:08x}"` on a
   set carrying incompat bit 10 (every default-formatted set since the
   rung-10b flip). The set generation is the superblock uuid per volume
   (`meta_backend::volume_set_generation`, random per `format`); the node
   token derives from the machine-id; the mount slot is
   `xxh3_64(canonicalized mount point)` (`writer_scope.rs:228-235`),
   overridable with `-o client_slot=<hex8>`. The probe's marker:
   `v3:3bce15f1cb7c4376839dbc774e423f5e@node:1c4e7f3e0e89c39d.m66d4530e`
   (`.squeezefs_generation`, header `squeezefs-staging-generation-v1`).
   - marker == expected ⇒ adopt everything (`320-323`) — **a plain same-host,
     same-mount-point remount rotates NOTHING** and takes this arm (§3);
   - same set + node, foreign slot ⇒ the root is *not ours*; the mount opens
     its OWN root (`staging/squeezefs/<slot-dir>/`) and the mount path reports
     the old one as **"MOVED-MOUNT-POINT STAGING RESIDUE"** at ERROR on every
     mount with the three remedies (remount at the original path /
     `-o client_slot=` / `squeezefs staging adopt|discard --slot`) — §3 leg A;
   - foreign set (a reformat) ⇒ wipe loudly (`411-442`,
     `staging_generation_discards`) — "reformat discards data", by design.
2. read-cache segments are wiped unconditionally (`1146-1160`, block keys are
   not incarnation-stable); **staging segments are recovered**
   (`recover_index`, `1221-1230` → `tiering/nvme.rs:1142`) when the dir had
   data.
3. ledger seeding (`1246-1278`): `active_block[_ext]:` keys are
   occupancy-indexed (not budget-counted); every other key is a `file_id`
   and is seeded into `staged_ledger` as `(cost, generation 0)` — **no
   fencing-token comparison, no layout cross-check**. The recovered
   population is exactly the pre-unmount one (§3: 155 → 155).

**The fencing law.** "Stale fencing tokens discard staged work" is applied
at mount to `active_block_ext:` records only (`recover_extent_records`,
`fuse_client.rs:15507-15592`: stamp < the ino's proven currency ⇒
discarded, `extent_records_stale_discarded`). `active_block:` entries are
flushed AUTHORITATIVELY at teardown (owner_token = None, `21976-21991`) and
dropped as orphans when their inode is gone. **Staged-layout `file_id`
entries are never fence-checked**: `StagedMetadata.fencing_token` is stored
(`nvme.rs:599`, `1364-1368`) and read back only by the active-block paths
(`get_staged_fencing_token` callers: `fuse_client.rs:21641`, `29689`,
`29895` — all `active_block:` keys). This is correct for them — the durable
layout record names the `file_id`, the ring entry is its sole copy, and a
newer writer's re-stage or layout flip retires the old `file_id` through
`release_superseded_staged` rather than by era.

**What another claim-holder sees.** The D0 guard hand-off does not move,
copy or invalidate the ring. Another host (or another mount point) mounts the
same volumes, reads a staged-layout inode's layout `{staged, file_id X,
block_map None}`, misses its own ring for X, finds no promoted mapping
(`staged_block_mapping`, `routing.rs:13606-13620`) and **serves
size-consistent zeros** (`17356-17363` → `note_lost_staged_payload`,
`13595-13604`, counted in `staged_payload_lost_reads`). The log line reads
"a crash discarded acked-unfsynced data; serving size-consistent zeros (D0
degrade contract)" — wrong on both counts for this population (§3 leg B: no
crash, and 50 of the 155 had been explicitly `fsync`ed). So: **the bytes are
not lost** (the origin root still holds them, adoptable), but they are
**invisible to every other client of the set, indefinitely**, and that
invisibility is silent at the reader (no EIO) — the read-only-mount /
co-writer postures included. If a second writer then rewrites the file, the
origin host's ring entry for the OLD `file_id` is orphaned in its ledger
until that host's own paths happen to name it (open question, not measured).

## 3. Live probe (laptop, unprivileged, file-backed sandbox)

Recipe (`/tmp/sqz_probe/probe.sh`): `format sqmeta://meta.bin
sqdata://data.bin --disk-cache-paths <dir> --force` (256 MiB meta, 4 GiB
data, 4 MiB block); `mount … --disk-cache-size 500MB --dismount-wait <DW>
--log-file`; write **200 small files** (sizes drawn from
{4, 8, 16, 32, 64} KiB, sha256-patterned) + **3 × 8 MiB** files; `sync(1)`;
idle 15 s sampling `.stats`; `squeezefs umount` (wall-clocked to daemon
exit); remount same host + same mount point; verify every byte against the
manifest; idle 15 s; umount again.

| leg | DW | fsync(2) | staged count after writes | after `sync` | after fsync ×50 | idle t+15 s | umount wall | WARN N | remount count | verify | idle t+15 s | umount #2 wall / N |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| dw10 | 10 s | none | 155 | 155 | — | 155 | **10.08 s** | 155 | 155 | **203/203 byte-exact**, `staged_payload_lost_reads` 0 | 155 | **10.06 s** / 155 |
| dw60 | 60 s | first 50 small files | 155 | 155 | **155** | 155 | **60.09 s** | 155 | 155 | 203/203 byte-exact, 0 | 155 | **60.07 s** / 155 |

Layout mix (stats inode, dw10): `layout_inline_writes 45` (the 4 KiB files),
`layout_staged_writes 164` (the 155 files of 8–64 KiB; re-writes counted),
`layout_striped_writes 3` (the 8 MiB files, write-through, `active_write_block_count 0`
throughout). The daemon log's dismount section shows the stall exactly
(`mount1.log`): `12:23:22Z … No staged active blocks to flush.` →
`12:23:32Z … dismount closed 3 open rewrite epoch(s).` → `12:23:32Z WARN …
Remaining local staged files: 155, active write directories: 0`. Staging
dir: 16 `staging_segment/segment_*.bin` shards, 250 MiB apparent / 6.2 MiB
allocated, identical before and after each umount (the segments are the
population). Remount log carries no recovery line for them (the adoption arm
is silent; `staging init: discarded 16 read-cache segment file(s)` is the
read cache). **The count is a steady state: it does not decay while idle, on
`sync`, on `fsync`, across a 60 s dismount wait, or across a remount.**

Cross-client legs (`/tmp/sqz_probe/probe_foreign.sh`, copies of the dw10
volumes):

* **Leg A — same host, same staging root, different mount point:** mount
  succeeds; the daemon prints the ERROR `MOVED-MOUNT-POINT STAGING RESIDUE
  … slot m66d4530e at …/tmp_sqz_probe_dw10_mnt: 8 live staged unit(s), e.g.
  […] remedy: remount at the original path, or mount with -o
  client_slot=66d4530e to adopt; or squeezefs staging adopt|discard`; the
  mount's own fresh root is stamped; **verify: 203 files, 155 mismatches, all
  155 size-consistent all-zeros**, 155 `is gone from local staging` WARNs;
  its own dismount says "Dismount clean".
* **Leg B — "another host" (the root holding the rings not reachable):**
  mount succeeds with no residue report (nothing of ours to report);
  **verify: 155 mismatches, all size-consistent zeros;
  `staged_payload_lost_reads 155`**, 155 WARN lines each ending "a crash
  discarded acked-unfsynced data; serving size-consistent zeros".

## 4. Reading the gate's numbers with this

* **"exactly 2,193 on every cycle" (TEST device, run 2)** = the TEST
  device's resident staged-layout population, recovered byte-exact at every
  remount and never promoted (below the 187.5 MiB high-water mark). Not
  orphans, not lost. It also means run 2 paid a **10 s stall on every
  TEST-device dismount** — the drain wait that can only exit on the timer.
* **1–2 on 47 dismounts (run 1)** = one or two small files a test left on the
  SCRATCH device (formatted per test) — same class.
* **210/225/234/293 on generic/795's remounts** = the fsstress population's
  small files (795 remounts mid-test).
* generic/795's own 23-min wedge (the lost commit wake) is unrelated: that
  was a transport fact, this is a layout fact.

## 5. Verdict against the three readings

* **(i) cosmetic over-count of durable-but-resident entries — NO.** The
  entries are the SOLE copy of acked (and fsync-durable) bytes; the shared
  backend holds nothing for them.
* **(ii) recoverable-on-same-host staged data — YES,** with the boundary
  drawn more tightly than "a cross-host claim handoff": the bytes survive
  every same-host same-mount-point remount (and are adoptable from a moved
  mount point via the printed remedies), but they are invisible — as zeros,
  silently — to **any** other client of the set (other mount point, other
  host, read-only mounts, co-writers) for as long as the origin's pool stays
  below high-water, which for a small-file population is forever.
* **(iii) acked data the drain gave up on within 10 s — NO.** There is no
  drain for this population to give up on; the 10 s is spent waiting for a
  counter nothing in the teardown decrements. Nothing is lost at the
  boundary. (A power cut between the last un-`fsync`ed staged write and
  the unmount is the ordinary acked-unfsynced class — `MS_ASYNC` msync on
  the ring write, `tiering/nvme.rs:671-675`; the dw60 leg's 50 fsyncs made
  those 50 shard-durable.)

The residue is by design (Tier 3 local NVMe staging — AGENTS.md
§Progressive Data Layout) but the design's own sentence — "writeback/flush
promotes to durable blocks" — is not what ships for this layout: only
pressure promotes. That gap, not the WARN, is the P1.

## 6. Proposed wording (the (ii) deliverable)

`src/fuse_client.rs:22943-22947` — replace the WARN with an INFO/WARN pair
that names the classes separately:

```
staged-layout files resident in local staging at dismount: {staged_count}
({bytes} B) — acked bytes whose ONLY copy is this host's staging root
{root}; recovered byte-exact by the next mount at this mount point (or
adopted with -o client_slot=…), NOT promoted to the shared backend
(promotion is pool-pressure-driven), and read as ZEROS by every other
client of this volume set until promoted. Unmount is clean for the
data-plane custody ({active_writes_count} active write blocks remain).
```

Keep the `active_writes_count > 0` arm as the WARN it is today ("unflushed
write custody"). `src/routing.rs:13599-13603` (`note_lost_staged_payload`):
drop "a crash discarded acked-unfsynced data" — the message fires for a
healthy other-client read of a live staged-layout file; say "not resident in
THIS client's staging root (never promoted — another client's local
custody, or a crash discarded it)". `src/main.rs:7398-7400`: the "[w] Wait …
(recommended)" option must not be offered when `active_write_block_count ==
0` (it cannot succeed). `tests/staged_crash_recovery_tests.rs:7`: strike
"promoted to durable blocks by fsync" or make it true (§7).
`docs/operations.md`: a "Staged-layout files and multi-client visibility"
paragraph stating the above.

## 7. Fix plan (not landed — the mandate stops at (ii))

Ranked by what the probe convicts, each red-first:

1. **The dismount stall (small, clear).** The wait at `fuse_client.rs:22886-22905`
   must not count entries no teardown step drains: wait on
   `staged_writes_in_flight − staged_ledger.len()` (the active-block
   population the sweep and the writeback worker actually retire), or have
   the wait poll the census split it already computes at `22931`. Contract
   test: a mount with N resident staged-layout files and zero active blocks
   unmounts in ≪ `dismount_wait` (the probe's 10.08 s → sub-second) and
   still reports N. Saves ~10 s × every fstests TEST-device dismount.
2. **`let _ =` at `22884` / `22039-22040`.** `force_flush_all_staged_data`
   returns `Result<(), _>` and can only be `Ok` — make it return the
   `TeardownFlushSummary` and log `failed > 0` at ERROR in the caller (the
   summary's own `warn!` exists but the caller's contract says it is
   discarded).
3. **Promote at clean unmount (the design gap; medium).** A clean dismount
   is a durability boundary for every other custody class in this teardown
   (rewrite epochs, overlays, active blocks); make it one for staged-layout
   files too: after step 3 of §1.3, drain the `staged_ledger` through
   `promote_staged_file` (bounded by `bg_admit::striped_block_concurrency`
   like the active-block sweep) so the segment is EMPTY at "Dismount clean"
   and every other client can read the files. Cost = one block + one meta
   commit per file (2,193 × ≤ 4 MiB on the gate's TEST device — seconds,
   against the 10 s it stalls today). The `[w]` option then means what it
   says. Whether `fsync(2)` should promote too (making the
   `staged_crash_recovery_tests.rs` header true and bounding cross-client
   staleness of small fsync'd files) is a design decision for the owner:
   it re-opens the "promoting every small stage competed with create/fsync"
   trade the high-water arm was written to avoid, so it wants the counted
   A/B on squeeze-test, not a laptop verdict.

## 8. Not verified here

* The exact number of run-2 dismounts that paid the stall (the record says
  "every test cycle"; the runner's NOTE prints only N ≥ 100).
* The orphaned-old-`file_id` leak on the origin host after a second writer
  rewrites a staged-layout file (§2 last paragraph) — reasoning only.
* Anything about throughput or the squeeze-test venue; every row above is a
  count or a wall clock on the laptop.

Artifacts: `/tmp/sqz_probe/{dw10,dw60,foreign}/` (daemon logs `mount1.log`,
`mount2.log`, `mountA.log`, `mountB.log`; `umount_*.stdout`), scripts
`/tmp/sqz_probe/probe.sh`, `/tmp/sqz_probe/probe_foreign.sh`, transcripts
`/tmp/sqz_probe/{dw10,dw60,foreign}.out`.
