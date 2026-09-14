# Symmetric metadata — PR 11: `volume enable-symmetric` + `format --symmetric` (the real bit-17 stamp)

| | |
|---|---|
| **Date** | 2026-09-14 |
| **Branch** | `feat/sym-convert` (cut from `dev` @ `5eca0e12` — PR 1, 2, 3 and 16 in) |
| **Design** | [`docs/design-symmetric-metadata.md`](../docs/design-symmetric-metadata.md) §6.2, §7.1–§7.3, §5.2.1/§5.2.2, §5.1.8, §5.3.1; PR-plan row 11; KD-SYM-12 |
| **Contracts** | [`tests/sym_convert_tests.rs`](../tests/sym_convert_tests.rs) — 22 contracts, in `tests/run_sym_forest_suites.sh`'s list (layout-blind by construction) |
| **Venue** | the dev laptop — **SCOPING only** (the venue rule; no acceptance row is this rung's: the box brackets are PR 1's — met — and PR 13/14's) |
| **Instrument** | in-process file-backed volumes (64 KiB nodes / 1 MiB ring for the contracts; 256 KiB nodes / the default ring on a 1 GiB file for the rate rows), `cargo test --release` for the rate rows, debug for the contracts |
| **Status** | landed dark: nothing before PR 14 changes a bit-17-absent mount's behaviour — a volume nobody converts is byte-identical, a plain `format` stamps nothing |

## 1. What landed

Bit 17 gains its two PRODUCT stamping arms beside PR 1's test seam:

1. **`squeezefs format --symmetric`** — `ImageBuilder::set_symmetric` / `format_v3_stamped_symmetric`: the seam-stamped builder path made the flag's second caller. ONE code path, so the flag and the seam build **byte-identical images** for one description (pinned whole-image). The class is the nine multi-writer bits plus bit 17; `--single-writer --symmetric` is a clap conflict (pinned through the binary). A default `format` stamps no bit and names no directory (pinned).
2. **`squeezefs volume enable-symmetric <sqmeta-uri> [--dry-run] [--resume]`** — the OFFLINE conversion of every volume of a set from the three shared per-kind trees (plus the block-map and block-reference trees where engaged) to the slot-tree forest, `config_ops::enable_symmetric_with`.

And, found under the conversion's census, **one shipped-bug fix on every layout** (§5).

## 2. As built — the conversion is a relayout driven by durable state

The design row says "converts the three shared trees into slot trees by key prefix, whole-tx per batch, resumable under a `sym_upgrade:` marker". The whole-tx law is honoured **at the conversion's atomic points, not as a journaled key-range move** — deviation 1, stated in §6:

- A journal entry is interpreted by the volume's LAYOUT BIT (`KvMetaBackend::open_inner` routes replay by `sb.symmetric_forest_stamped()`), so one transaction cannot both delete a flat record and insert its forest form: a flat replay would apply the 9-byte forest key into the 8-byte inode tree, a forest replay would route the legacy delete by a key its codec refuses. A hybrid tree set — flat source trees and forest destination trees in one backend, one replay understanding both — is the 18 k-line `backend.rs` surgery PR 4 is concurrently editing, and the brief asked for one small function there.
- Instead, per volume, the durable steps and the state each leaves are:

| # | Step | Durable write | State a kill −9 right after it leaves |
|---|---|---|---|
| 1 | **marker** on EVERY volume of the set (before any conversion) | `sym_upgrade:` xattr on ino 1, journal-committed + checkpointed under the volume's guarded open | flat + marker: every writable open refuses at the D0 gate BEFORE its claim (writes nothing); readers/probes serve the flat trees |
| 2 | **quiesce** | guarded open (`open_for_sym_upgrade` — the D0 ladder: a live foreign writer refuses HERE, never gets its ring reset under it) → `checkpoint_now` → clean shutdown | an EMPTY replay window — asserted by a recovery scan before anything is written; refused otherwise |
| 3 | **read** | none (a probe: every live record of every kind, FOLDED, through the digest oracle's own kind-routed walk; the ledger; the reachable image set) | — |
| 3′ | **census** | `release_unpublished` of every claimed extent no ledger root reaches (a crashed build's orphans — and §5) | RAM only until step 4's bitmap write |
| 4 | **build** | one mixed-kind slot tree per slot the records name (every record at **seq 0** — the format law: a seq-0 record set is checkpoint-covered by a tail-0 ring), tree 0 with `Unleased { root, cursor: max(stamp cursor, max live local ino + 1), g: 0, tails: [] }` per guest slot (§5.1.8: a cursor is never lowered), the appender directory extent + header, the fixed journal extent ZEROED and appender 0's `Free` page in its first slot (§5.3.1), the bitmap at a new generation, barrier | flat + marker; the forest is unreferenced (its extents claimed in the bitmap): **the orphaned-build window** — the resume's census reclaims it (pinned: the resumed volume has EXACTLY the free extents of a straight conversion) |
| 5 | **hybrid ledger** | ONE 4 KiB checksummed slot: the flat roots AND tree 0 + the native slot tree, `journal_tail_seq: 0`, the new bitmap generation, barrier | flat + marker; a flat open finds its roots (it ignores tree ids 8/0) and reads the zeroed ring as empty (the appender page in page 0 is not a journal page); the forest is referenced by this record alone — the resume REBUILDS (the census reclaims the forest as orphans and the record is overwritten) |
| 6 | **stamp** | ONE sector: bit 17 + `appender_dir` (`superblock::set_symmetric_forest`, the DUR-5 copy first) | **forest** + marker (the marker rode the build into the native slot tree): a forest open finds it and refuses writers; readers serve the forest; the old trees' extents are still claimed |
| 7 | **free** | the old trees' nodes (reachable from the flat roots the hybrid ledger still names — nothing else can reach them, and no forest mount has run) released through a bare `NodeCache` walk, the bitmap written, ONE ledger record naming the forest roots ALONE, barrier | forest + marker; the walk is repeatable until the folding record lands (the roots are still named, the nodes untouched); after it there is nothing to free |
| 8 | **marker removal** | through the forest's own guarded open: join, `removexattr`, checkpoint, leave | forest, done |

**The resume reads each volume's state off three durable facts and nothing else** — the bit, the ledger's roots, the marker's presence: flat + marker ⇒ steps 2–8 (the census first); forest + marker + old roots in the ledger ⇒ 7–8; forest + marker only ⇒ 8; forest, no marker ⇒ skip. No progress cursor exists to go stale: a refused writable mount's checkpoint would have rewritten the ledger under a cursor, which is why the marker gate sits BEFORE the claim (`KvMetaBackend::open_writer` — a refused open writes nothing; `a_refused_writable_open_writes_nothing` pins the fixed region byte-identical across a refused open).

**The marker is PER VOLUME** (deviation 2): the mw precedent's marker lives on volume 0 and is probed after the set's guarded opens; the sym marker is read inside `KvMetaBackend::open` — which knows only its own volume — so every volume carries its own, all written before any conversion, each removed as that volume's last act. A half-converted set therefore refuses on the first unconverted volume's marker, naming it (pinned), and no set-level uniformity gate is needed. The read is ONE `getxattr` on ino 1, whose leaf the claim gate reads next anyway (`writer_claim` is an ino-1 xattr) — a volume nobody is converting answers `None` off the cached leaf and pays nothing else. `KvMetaBackend::open` gained no other line.

**Refusals first**, loud, naming the remedy: already symmetric; a bit-8 NON-SOLO partition record on the newest ledger record ("mount solo once"); open `xv_*` intents; in-flight `job:` records (an undecodable one counts — never guess); a live client; a marker without `--resume` (a crash must be acknowledged — deviation 3: the mw verb resumes implicitly); `--resume` with nothing to resume. `--dry-run` inspects and plans (records + bytes per volume, the drops, every refusal) and writes nothing (pinned: sector 0 + ledger + ring + bitmap byte-identical). `set-owners` assignments and a solo bit-8 suffix are reported **DROPPED** (§7.3); the claim-set record itself is copied verbatim like every other ino-1 xattr (never rewritten by this verb).

## 3. Contracts (all green ×10 from zero; 22 tests)

| Contract | Pins |
|---|---|
| `a_converted_volumes_post_fold_digest_equals_the_sources` | digest EQUAL flat → forest; every inode / dentry / xattr (incl. 6 000 B values) / hard link / renamed name / block reference reads back through the PR-3 mount; `dlm_rpcs` unchanged; the ledger names `[0, 8]` only; storm (create/rename/unlink) + remount + population again; offline fsck clean; `claimed ≡ reachable + 1` on the result |
| five `a_crash_*_refuses_writers_until_resumed` | at each window (markers, mid-build, hybrid ledger, stamp, free): the verb aborts on the seam; the marker is present; the writable set refuses NAMING the volume and `--resume`; a read-only set serves the population; a plain re-run refuses; `--resume` completes; digest equal; forest mount; fsck clean. Mid-build and after-stamp additionally pin `free_extents ≡ a straight conversion's` (no leak in either direction) |
| `a_clean_flat_unmount_leaves_no_claimed_extent_its_roots_do_not_reach` | §5's law (RED on the pre-fix tree: `claimed 6 vs reachable 4`) |
| refusals | already symmetric; non-solo bit-8 record (a 2-way partitioned ledger record planted newest); open intent (planted on the intent ino); queued job record; live client; `--resume` with nothing; CLI `--symmetric --single-writer` |
| `the_marker_round_trips_and_refuses_a_torn_image` | codec + torn/truncated/empty |
| `format_symmetric_builds_the_image_the_seam_builds_byte_for_byte` | whole 64 MiB images equal |
| `the_public_symmetric_formatter_mounts_as_a_forest` | mw bits + bit 17; mounts as a forest; churn |
| `a_default_format_stamps_no_bit_and_names_no_directory` | the untouched default |
| `a_dry_run_writes_nothing` | fixed region byte-identical; no marker; digest unchanged |
| `a_four_volume_set_converts_every_volume_in_one_invocation` | the 46-volume shape scaled down: four volumes, mints spread across them, every volume converted, per-volume digests equal, storm, fsck |
| `a_half_converted_set_refuses_writable_mounts_naming_the_volume` | crash at `AfterStamp { volume: 1 }`: 0 done, 1 stamped under its marker, 2/3 flat under theirs; the refusal names volume 1; readers serve; resume: `[AlreadySymmetric, Resumed, Resumed, Resumed]` |
| `a_refused_writable_open_writes_nothing` | the marker gate's placement |

**Sibling suites, both layouts** (flat / stamped, `--test-threads=1`): `sym_forest_tests` 30/30, `sym_appender_tests` 34/34, `sym_manager_tests` 29/29, `sym_fence_tests` 8/8, `kv_backend_tests` 36/36, `crash_contract_tests` 25/25, `readonly_mount_tests` 28/28, `meta_slot_migration_tests` 16/16, `pv_coordinator_tests` 13/13, `cli_version_tests` 21/21, `docs_parity_tests` 5/5, `env_knob_convention_tests` 22/22, `derivation_sweep_tests` 53/53 — green on both legs (run before the §5 fix; the matrix below ran after it).

## 4. The conversion rate (SCOPING — dev laptop, `cargo test --release`, one 1 GiB file-backed volume, 256 KiB nodes)

| files | records | bytes | slot trees | extents written | old extents freed | pass wall | verb wall | records/s | MB/s |
|---|---|---|---|---|---|---|---|---|---|
| 5 000 | 15 013 | 730 650 | 64 | 66 | 8 | 0.059 s | 0.079 s | 254 k | 12.4 |
| 20 000 | 60 043 | 2 922 085 | 64 | 66 | 27 | 0.102 s | 0.122 s | 587 k | 28.6 |
| 50 000 | 150 103 | 7 304 965 | 64 | 66 | 56 | 0.210 s | 0.232 s | 714 k | 34.8 |

The pass wall is the per-volume row (`VolumeConversionRow::secs`: quiesce → read → build → ledger → stamp → free → marker removal); the verb wall adds discovery and the inspection probes. Every derived-width mint spreads over the volume's 64 rotor slots, so a 150 K-record volume is 64 one-leaf slot trees (≈ 114 KB each under a 192 KB three-quarter fill) + tree 0 + the directory extent = 66 extents. The rate is dominated by the probe's folded record walk and the bottom-up node writes, both linear in the record count; the fixed cost (two guarded opens, the probe's bootstrap, the ring zero) is ≈ 50 ms on this substrate. **These are debug-free release numbers on a thermally-capped laptop over a file on a CoW host filesystem — scoping evidence only**; no box row is this rung's. The 46-volume shape is run as a 4-volume set in the contracts (every volume converted in one invocation, the population spread across them); a 46-volume rate row is the same per-volume loop 46 times and was not run (the fixture would be 46 × 64 MiB of files for a number the venue rule labels scoping anyway).

**Memory**: one volume's live records are held in RAM while it is re-laid (`FlatRecords::by_slot`), ≈ the volume's live record bytes plus the forest key's extra byte per record — 7.3 MB for the 150 K-record row; a 100 M-inode volume would need tens of GB. The streaming form (a k-way merge over the per-kind walks — ino-major, so slot-grouped by construction — feeding leaves as they fill, with refs bucketed per slot) is owed (§7).

## 5. Finding — every clean unmount of a flat volume leaked the images its final checkpoint cycle retired (SHIPPED; fixed here)

The pre-build census (`claimed − reachable(ledger roots)` over a quiesced volume with an EMPTY window — the exact condition under which reachability from the ledger's roots is the whole truth) was written to reclaim a crashed build's orphans. On its first straight run it reclaimed **7 extents** from a volume that had never been converted. Attribution (a scratch probe, `open_probe` + `reachable_node_addrs` over `all_trees()` vs `is_allocated`):

| state | claimed | reachable | orphans |
|---|---|---|---|
| after format | 4 | 4 | — |
| after one mount + 200 creates + clean unmount | 15 | 9 | `[2, 4, 5, 6, 8, 9]` |
| + one IDLE mount/unmount | 15 | 9 | same |
| + another IDLE mount/unmount | 16 | 9 | `+ [10]` |
| 50 000-file volume, one mount, clean unmount | — | — | 43 |

**Mechanism.** A checkpoint cycle writes its dirty bitmap pages, barriers, writes its ledger record, barriers — and only THEN (`KvMetaBackend::after_durable_barrier` → `ExtentAllocator::advance_durable(tail)`) releases the pending frees the new tail covers, which `mark_dirty`s their pages for the NEXT cycle ("reclamation lags one cycle" — correct on the cadence). The shutdown fixpoint (`checkpoint.rs`, the `final_cycle` arm) iterated while `head != reusable_upto` or a declared region was uncovered — ring coverage — and returned the moment the window was covered, **with the last cycle's released pages still dirty and unwritten**. The next mount's `ExtentAllocator::load` read those extents' bits SET, the window that held their `free` records was covered (below the tail — never replayed), and the images stayed claimed for ever. The leak is proportional to the number of SMOs the final flush pass runs (a compaction or split per full leaf log): 6 on a 60-file volume, 43 on a 50 K-file one, and one per idle mount cycle (the claim/unclaim xattr's compaction). This is the second face of the class AGENTS.md described as "the bounded flat leak … until a bitmap-vs-reachability census exists" — that sentence named the CRASH face (an unpublished root swap's successor); the clean-unmount face was undetected because nothing ever compared the bitmap to reachability.

**Fix** (`b47f7c02`): `ExtentAllocator::has_dirty_pages()` (any dirty word non-zero) joins the fixpoint's convergence predicate. With nothing dirty the extra cycle journals no SMO, so the term converges in one cycle, inside the existing 16-cycle bound and the `TEST_SHUTDOWN_FIXPOINT_CYCLES` seam (`n − 1` iterations — a seam of `1` still forbids every iteration, so the PR-3 defect-shape pin is unchanged). **Pinned red-first**: `a_clean_flat_unmount_leaves_no_claimed_extent_its_roots_do_not_reach` — `claimed ≡ reachable` after a populated clean unmount and after three idle cycles, and the conversion's `orphans_reclaimed` reads 0; on the pre-fix tree it fails `left: 6, right: 4`. The conversion's census stays: a volume last unmounted by an OLDER binary carries the leaked class, and `orphans reclaimed` in the verb's output is the honest count of what that binary left.

**Not this fix's**: the crash face (an unpublished root swap's successor image after a kill) is unchanged — PR 3 dropped the live root's replayed free; the successor stays claimed-and-unrouted on a flat volume until a census reclaims it. The conversion's census does exactly that for the volumes it converts; a standalone `fsck` class for the flat layout is owed to the program that owns fsck's census (C13's flat twin).

## 5b. Finding — `rename` stages the moved inode's ctime `Delta` without holding `I{moved}` (SHIPPED, every layout; NOT fixed here — outside this PR's surface)

The live-FUSE leg's first storm — `std::fs::write(file)` then an immediate `rename(file)` — failed the converted mount with `EIO` on the rename. The daemon log: the conveyor pass task panicked on the same-key co-queue exclusion debug assertion, *"tree 1 key `[00,00,16,00,00,00,00,02,01]` staged by batch members 0 and 1 with non-merge kinds Delta/Put — a DLM guard was released before its tx's terminal outcome"*, the batch failed loud, and the write-back error latched on the ino. **Attribution, not conversion**: the identical Rust storm shape reproduces the violation on a plain FLAT volume and on a fresh `--symmetric` forest (1 violation each, every run — a scratch variant of `tests/sym_convert_fuse_tests.rs` parametrized over the format), while a bash storm (a process per op — slower) passes on all three.

**Mechanism.** The file's staged write-back publishes its layout + size as ONE tx (`set_layout_and_size` → `stage_layout_and_size`), which takes `I{ino}` EXCLUSIVE and stages a `Put` of the file's inode. `KvMetaBackend::rename` takes `lock_many` over `I{old_parent}`, `I{new_parent}`, `D{old_parent, old_name}`, `D{new_parent, new_name}` — **never `I{moved ino}`** — and stages the moved inode's ctime as a `Delta` on that inode's key (`backend.rs:18154`). With the rename issued before the write-back's publish reached its terminal outcome, both txs sit in one conveyor batch on one key with kinds `Delta`/`Put`: the exclusion the pass relies on ("a conflicting same-key writer cannot co-queue because the earlier tx's D/I guards are alive inside the queue until its terminal outcome") does not hold, because one writer holds no guard on that key. In a debug binary the `debug_assert!` panics the pass task (`detached_task_panics`, the batch fails, the app sees `EIO`); in a release binary the assertion is compiled out and the pass applies both — a `Put` that lands after the `Delta` carries the write-back's base inode value and the rename's ctime is LOST (a POSIX ctime inconsistency), the reverse order is benign.

**Why the live-FUSE trio never saw it**: no suite writes a file and renames it inside the write-back's publish window; the storm here did (a 512 B–7 KiB `write` + `close` then `rename` on the same thread). Every debug-build live-FUSE test with that shape will hit it.

**Repro recipe** (deterministic in-process): `set_layout_and_size(ino, …)` and `rename(parent, name, parent, other, 0)` for the same file issued concurrently on one `KvMetaBackend` so both are enqueued before the pass drains; the pass panics on the assertion in a debug build. Through FUSE: `tests/sym_convert_fuse_tests.rs`'s storm with the `sync_all()` before the rename removed.

**Fix direction** (owed; `kv/backend.rs::rename` — PR 4's file, and not a one-liner): the moved ino is known only after the dentry lookup, which needs the D-guard, so the guard set must be acquired in one `lock_many` INCLUDING `I{moved}` (and the overwritten target's ino) after a guard-less lookup, with the dentry re-validated under the guards and a retry if it moved — the `lock_many` law forbids taking `I{moved}` after the parents' guards on a stripe collision. The storm in `sym_convert_fuse_tests.rs` fsyncs each file before renaming it (the publish precedes the rename) and names this finding; the suite's contract is the conversion.

## 6. Deviations from the PR-plan row / the brief

1. **"Whole-tx per batch, each batch deleting the source records it moved"** → the conversion is a relayout with the atomic points stated in §2 (the marker commit, the hybrid ledger write, the sector flip, the folding ledger write, the marker delete). A transaction spanning the flat source and the forest destination is not expressible without a hybrid tree set and a hybrid replay in `backend.rs`, which the brief asked to keep to one small function and PR 4 is editing. Every record is in exactly one place after every crash (old trees until step 6, forest after — the ledger names both across steps 5–7 and the layout bit says which is read), which is the property the batch law was for.
2. **The marker carries no cursor** and is PER VOLUME (not on volume 0 alone): the resume decides from the bit, the ledger's roots and the marker's presence. The mw precedent's "the D0 guard is held across the whole verb" serialization does not transfer — the offline steps (ring zero, ledger flip) need the volume's guarded open CLOSED — so two concurrent `--resume` invocations on one set are the operator's error (documented); a plain writable mount is refused by the marker at the gate, before it writes.
3. **`--resume` is an explicit act** (a marker present refuses a plain run) where `enable-multi-writer` resumes implicitly — the brief's wording ("naming `enable-symmetric --resume`").
4. **The refs walk on a flat volume** rides `range_kind(TREE_BLOCK_REFS, …)` (legal there — the forest arm is what refuses it); the block-map tree rides `range_kind(TREE_BLOCK_MAP, …)` when engaged. Neither is populated in the contracts beyond the refs the layout publishes (kind 6: two files, three references, counted across slot trees after the conversion); a bit-16 population is owed to a kvmap suite's converted leg.
5. **Records are held in RAM per volume** (§4).
6. **The cursor law** writes `max(stamp cursor_for(slot), max live local ino + 1)` into `Unleased.cursor`; PR 1–3 write `cursor: 0` and read cursors off the membership stamp — PR 4 decides which the lease reads, and both are present after a conversion.
7. **One shipped-bug fix outside the brief's surface** (`checkpoint.rs` + `alloc_ext.rs`, neither in PR 4's list) — §5, pinned red-first; the predecessors' precedent (PR 3's two shipped fixes).

## 6b. The live-FUSE leg

`tests/sym_convert_fuse_tests.rs` (mount-class, in `run_require_mount_gate.sh`'s list and `skip_ledger_tests`' `MOUNT_GATED`): `squeezefs format` (flat, through the binary) → mount → three directories × six files (3 KiB / 16 KiB / 200 KiB staged, promoted by the dismount pass; 4 MiB; 9 MiB + 4 KiB and 13 MiB striped), a hard link, a cross-directory rename, a removal → clean unmount → `volume enable-symmetric --dry-run` (prints `PLAN`) → `volume enable-symmetric` (prints `converted … records/s`) → a second run refuses `already symmetric` → mount at another mount point → **every byte reads back exactly** (`readdir` lists the link and omits the removed file) → `.stats`: `appenders_live [1]`, `appender_joins [1]`, `dlm_rpcs 0` → a 200-round fsync'd create/rename/unlink storm → the population again → clean unmount → offline `squeezefs fsck --json`: zero findings. ×3 green under `SQUEEZEFS_TEST_REQUIRE_MOUNT=1`; the trio (`posix_mount_semantics_tests`, `corpse_sweep_tests`, `inline_raise_tests`) + this suite both legs — see the run log in the summary.

## 7. Owed

- **The rename guard hole** (§5b) — `I{moved}` (and the overwritten target's ino) in `rename`'s `lock_many`, with the lookup-then-lock-then-revalidate shape; a red-first in-process repro (concurrent `set_layout_and_size` + `rename` on one ino) lands with it.
- The **streaming conversion** (bounded memory): the ino-major k-way merge feeding leaves as they fill; refs bucketed per slot (or a per-slot probe once the flat refs tree gains an owner-major index).
- A **bit-16 (block-map tree) population** through the conversion — a `kvmap_tree_tests` converted leg.
- The **46-volume rate row** (the 4-volume set stands in for the shape).
- **The crash face's flat census** — an fsck class (C13's flat twin) reclaiming an unpublished root swap's successor image on a flat volume; the conversion's census does it for converted volumes only.
- The **matrix's "converted" leg**: `run_sym_forest_suites.sh` runs the pre-forest suites flat and stamped; a third leg that formats flat, converts, then runs each suite on the result would prove every KV contract on a CONVERTED forest rather than a format-time one — a harness that formats through a conversion hook, owed.
- The **squeeze-test brackets** are PR 13/14's (the venue rule); nothing here is a number.
