# SqueezeFS 1.2.0

_Release date: 2026-09-04 (tag `stable-2026.09`)_

1.2.0 is the release train after 1.1. It removes the file-size ceiling, closes a set of data-integrity bugs found in the field, and lands the first campaigns of an end-to-end performance program together with the instruments that program runs on. Existing volumes mount unchanged; read [Upgrading from 1.1](#upgrading-from-11) before rolling it out to a mixed fleet.

## Highlights

- **Files of any size.** A file's block map now lives in a dedicated metadata tree once it outgrows its inline record — roughly 8 GiB of file at the default 4 MiB block size. Nothing is configured: the crossing happens once per file, automatically. The former ceiling of about 545 GiB per file is gone; the format's limit is now `(2³² − 1)` blocks per file — 16 PiB less one block at 4 MiB blocks. Truncating or deleting a very large file returns immediately and reclaims its records in the background.
- **Faster and safer: a set of data-integrity fixes.** A freshly written file could lose the last 3 MiB of its first block on a clean unmount of a cache-less mount; writing a file past ~8 GiB could corrupt the volume's metadata and wedge it permanently; co-writer fleets leaked freed blocks until the data volume filled; and on a cache-less mount, a heavy rewrite followed by an unmount could lose a block that was still being flushed. All four are fixed, each pinned by a test that reproduces it — see [Fixes](#fixes).
- **An end-to-end performance program, with instruments first.** Every latency histogram on `.stats` now carries an exact count, sum and mean; a per-operation trace (`.trace`) joins one request's phases on one timeline; daemon CPU is attributed by thread class. The first campaigns on those instruments: about +15 % on 4 KiB random-read IOPS through the kernel path on the reference fabric; and, for multi-writer ingest, the metadata commit pipeline no longer holds its serialized stage across the device write, while co-writer publishes travel batched instead of one round trip each.
- **Build and release changes.** `cargo build --release` is now a thin-LTO build (fast to rebuild — the dev, field-A/B and gate profile); tagged releases ship from the new `dist` profile (fat LTO, one codegen unit) via `task dist:<distro>`. Every binary names its profile: `squeezefs --version` ends in `profile <name>`, and `.stats` exports `build_profile`.
- **Documentation refresh.** The [README](README.md) is a plain overview with the four measured hero numbers; [QUICKSTART.md](QUICKSTART.md) is a hands-on walkthrough; [docs/operations.md](docs/operations.md) now lists every environment knob (with default and purpose) and every `.stats` key, and carries the large-file section, the field performance records and the build-verification gate.

## Upgrading from 1.1

**Volumes.** A volume formatted or written by 1.1 mounts unchanged under 1.2 — the on-disk format is the same, and a volume that never carries a very large file stays byte-identical whichever binary mounts it. Three forward-only boundaries to know about:

- **The block-map tree is stamped on first use.** The first time a 1.2 mount publishes a file past the inline map cap (about 6–8 GiB at 4 MiB blocks), it stamps superblock incompat bit 16 on that volume and announces it in the mount log (`grep kvmap <log>`). **Once a 1.2 mount has written a file past ~8 GiB to a volume, that volume can no longer be mounted by 1.1** — a 1.1 binary refuses it loudly, naming the unknown bit. There is no downgrade path other than reformatting; upgrade the binary, never downgrade the volume. Volumes that never cross mount on either train.
- **Fresh formats are multi-writer-capable by default since 2026-08-16.** A volume set formatted by 1.2 carries the multi-writer capability bits, which 1.1 binaries built before that date refuse. `format --single-writer` produces the older class for exactly that case (a scratch volume an old binary must read); `squeezefs volume enable-multi-writer` upgrades an older set offline.
- **One reformat class.** A volume set formatted by a 1.1 binary from before 2026-08-01 uses the frozen metadata-routing width that release retired; 1.2 refuses it with *reformat required*. Sets formatted on or after that date are unaffected.

**Binaries.** Deploy the daemon and the interception shim (`libsqueezefs_il.so`) from the same build folder — they refuse to pair across builds, so a 1.2 daemon will not accept a 1.1 shim or the reverse. `squeezefs --version` now prints a trailing `profile <name>` (`release` for a plain build, `dist` for a tagged release); tooling that parses the version line should key on `.stats` `build_commit` / `build_tag` / `build_profile` instead, which are unchanged.

**Retired flags, verbs and knob spellings since 1.1 began (2026-07-24).** Each refuses or announces loudly, naming its successor; the complete catalog (including retirements that predate 1.1) is [docs/operations.md → Removed verbs & flags](docs/operations.md#removed-verbs--flags).

| Retired | Since | What happens | Use instead |
|---|---|---|---|
| `format --meta-slots N` | 2026-08-01 | hard error | nothing — metadata routing widths are derived; grow a set with `squeezefs volume add-meta --take-slots …` (offline) or `squeezefs volume migrate-meta-slot` (online) |
| `format --multi-writer` | 2026-08-16 | accepted, announced as having no effect (it is the default) | `format --single-writer` is the explicit opt-out |
| `SQUEEZEFS_RECLAIM_BATCH` | 2026-08-02 | refused at startup | `SQUEEZEFS_INODE_RECLAIM_BATCH` |
| `SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS` | 2026-08-02 | refused at startup | `SQUEEZEFS_INODE_RECLAIM_WINDOW_MS` |
| `SQUEEZEFS_RECLAIM_CONCURRENCY` | 2026-08-02 | refused at startup | `SQUEEZEFS_INODE_RECLAIM_CONCURRENCY` |
| `SQUEEZEFS_FUSE_PLACED_MERGE` | 2026-08-09 | refused at startup | nothing — the mechanism was measured and removed |

Also since 2026-08-02, **every environment knob obeys one value convention**: `0`/`false`/`no`/`off` disables and `1`/`true`/`yes`/`on` enables (case-insensitive) — including knobs whose default is on. Before that, 19 knobs were presence-based, so `SQUEEZEFS_FREE_FORENSICS=0` used to *enable* forensics. A malformed or out-of-range value now refuses the process at startup, naming every offender, instead of being silently clamped or defaulted; an unrecognized `SQUEEZEFS_*` name is announced as a probable typo. Details: [docs/operations.md → Environment knobs — the parsing convention](docs/operations.md#environment-knobs--the-parsing-convention).

**New environment knobs an operator might care about.** Defaults are right; the levers exist so an A/B can be counted, and a fleet running one of them is running an experiment.

| Knob | Default | Purpose |
|---|---|---|
| `SQUEEZEFS_OP_TRACE` | off | Arm the per-operation trace ring at mount; `cat <mnt>/.trace` drains it. Off costs one relaxed load per hook. |
| `SQUEEZEFS_KVMAP` | on | Measurement lever: `0` makes *new* large-file crossings keep the legacy single-blob map. It never disables reading a file that already lives in the tree. |
| `SQUEEZEFS_KVMAP_OVERLAY` | on | Measurement lever: `0` holds a very large file's whole write map in RAM regardless of size instead of the bounded partial store. |
| `SQUEEZEFS_MAP_MIGRATE_CHUNK` | 512 (64–1024) | Map records written per transaction while a file crosses into the tree. |
| `SQUEEZEFS_JOURNAL_LANE` | on | Measurement lever: `0` = the metadata journal's durability stage back on the shared pool instead of one lane per writable volume. |
| `SQUEEZEFS_FUSE_READ_FAST_DISPATCH` | on | Measurement lever: `0` = the pre-1.2 read dispatch path. |
| `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR` | on | Measurement lever: `0` = large-write overlay stores issue open-loop, outside the write-pipeline depth governor. |
| `SQUEEZEFS_FUSE_ZC_READ_FUSION` | off | Measurement lever, ships off on measurement (it loses to the default when composed with fast dispatch). |
| `SQUEEZEFS_FUSE_IO_URING_SPIN_US` | 0 (off) | Measurement lever, ships off on measurement: a spin-before-park cap for the FUSE queue workers. |

The complete registry — every knob, accepted values, range, default, purpose — is [docs/operations.md → Environment knobs — the complete registry](docs/operations.md#environment-knobs--the-complete-registry).

## Fixes

Every fix landed with a test that reproduces the bug (red before, green after). Commits are on `dev`; the internal record names the finding the engineering notes use.

| What you would have seen | Fixed in | Internal record |
|---|---|---|
| **Cache-less mount, heavy rewrites, then `umount`**: `Dismount durable upload failed for ino … journal entry length … exceeds the 131072-byte whole-entry cap` in the log, and the block named there was lost. A long rewrite storm had accumulated more block-reference bookkeeping than one metadata transaction may hold; every publish of it refused, and the unmount drain dropped the block. Oversize loads are now committed in chunks. | `b0ea5f04` | finding 38 — [`.benchmarks/2026-09-01-field-corruption-train.md`](.benchmarks/2026-09-01-field-corruption-train.md) |
| **Writes collapsed ~8× under memory pressure**, with multi-second stalls: the write pipeline's memory-pressure response clamped to a fixed floor instead of the measured drain rate. It now sheds only headroom and keeps the measured completion rate. | `9936c625` | finding 39 — same note |
| **A nearly full data volume (~97 %) made every write pay a 12–22 ms synchronous reclaim** on the fabric: background block reclaim deferred to foreground traffic even when the queued reclaim debt was most of the remaining free space. Reclaim now runs ahead of allocation when the free supply is thin, and once engaged it runs the queue to empty rather than stopping part-way (the follow-up). | `ac0c68bc`, `c985fa8c` | finding 40 — same note; the follow-up commit |
| **Writing a file past ~8 GiB could corrupt the volume's metadata and wedge it permanently**: `divergent layout-delta chain … folds onto 0x0` in the log, then `fsync` failures and every write stalling forever. A metadata node split could separate two records that must stay together; the stranded record became unroutable and poisoned every later fold. Splits now respect record groups, and a node write that would strand a record refuses loudly instead. Verified twice from zero on the 5-node fabric with the exact triggering workload. | `08202c32` | finding 41 — same note |
| **Co-writer fleets leaked freed blocks until the data volume filled** (`authority refused N shipped frees` in the authority's log, co-writers hitting ENOSPC with space that should have been free — about 26 GiB leaked in two minutes on the test fleet). When the authority recomputed a co-writer's publish, the displaced blocks were freed by nobody. The authority now frees them itself after commit, across all four code paths that could recompute. | `6101b09b`, `19e9fb76`, `82de27bf`, `418f06e8` | findings 36 / 36b — same note |
| *(Development builds only.)* The block-map tree never engaged on a real mount: its enabling gate depended on a stamp only the tree itself could write. Large files now self-arm the tree at the first crossing. | `fc0549aa` | finding 43 — [`docs/design-kvmap-block-map-tree.md`](docs/design-kvmap-block-map-tree.md) |
| Zeroed data after a rewrite-and-remount smoke test — traced to an unclean kill in that store's history (acknowledged, un-`fsync`ed writes carry no crash guarantee); not a defect on the shipped path. Content-verification tests for the rewrite/remount venue were added so the class cannot hide. | `7c5aad36` | finding 44 — same document |
| Nothing in the log or `.stats` said whether a large file had entered the block-map tree, so a correct run looked like the feared regression. Each crossing now logs one line (`kvmap crossing: ino N …`). | `a7684785` | finding 45 — same document |
| **Sequential writes fell ~2,000× after a file crossed ~8 GiB** (28.7 GB/s → 0.36 GiB/s, about 1 s per 1 MiB write) while rewrites of the same files ran at full speed: every steady-state publish re-read and re-diffed the file's whole map. A publish now touches only the blocks it publishes. | `87755e2e` | finding 46 — [`.benchmarks/2026-09-02-f46-kvmap-stream-publish.md`](.benchmarks/2026-09-02-f46-kvmap-stream-publish.md) |
| **Small writes could cost a whole block each**: a 4 KiB write into a hole or a clone-shared block paid a 4 MiB read plus a 4 MiB write at settle, and small sequential O_DIRECT segments were stored one device write per segment instead of accumulating into one block write. Writes at or under the in-place patch ceiling (512 KiB at 4 MiB blocks) no longer take the large-write path. | `3a2404e0` | finding 47 — [`.benchmarks/2026-09-02-f47-overlay-length-floor.md`](.benchmarks/2026-09-02-f47-overlay-length-floor.md) |
| **A freshly written file could lose the last 3 MiB of its first block across a clean unmount** on a cache-less mount — reading it back before the unmount showed the right bytes; after remount the bytes past the first 1 MiB read as zeros (23 of 24 files on the field run). Closing the file never settled the block's pending overlay record, and unmount never closed the rewrite state a later read had opened. Close and unmount are now durability boundaries for both. | `3688d1fb` | finding 48 — [`.benchmarks/2026-09-02-f48-warm-cold-overlay-gap.md`](.benchmarks/2026-09-02-f48-warm-cold-overlay-gap.md) |
| **A sustained create/unlink storm in one very large directory could fail-stop the volume**: `commit aborted while parked for ring space` after the journal ring filled, with the checkpoint stuck. The checkpoint's own maintenance drain starved its cadence, and one fold inside it ran for tens of seconds. The drain is now bounded per cadence period and the fold no longer rescans its input for every record. | `febd0d87` | finding 49 — [`.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md`](.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md) (the owed-items board) |
| **`df -i` could briefly report deleted inodes as still in use** after a delete: an explicit inode reclaim could return while a concurrent background reclaim still owned those inodes. It now waits for that owner's outcome. | `3975ac62` | finding 50 — same note |

## Performance

Measured on a 5-node NVMe-oF/TCP fabric (memory-backed targets) with a 32-core client, in interception mode:

| Read bandwidth | Write bandwidth | Read IOPS (4 KiB) | Write IOPS (4 KiB) |
|:---:|:---:|:---:|:---:|
| **43.9 GB/s** | **36.4 GB/s** | **942 k** | **727 k** |

Every measurement behind these numbers — venue, instrument, substrate, and the campaign notes that moved them — is recorded in [docs/operations.md → Performance records](docs/operations.md#performance-records).

## New operator surface

- **Exact latency histograms.** Every latency family on `.stats` exports `buckets` plus an exact `count`, `sum_ns` and `mean_ns` (means used to be estimated from bucket midpoints). Per-row means are now `Δsum_ns / Δcount` between two snapshots; bucket labels are unchanged, so existing tooling keeps working.
- **A per-operation timeline.** Mount with `SQUEEZEFS_OP_TRACE=1` and `cat <mnt>/.trace` drains one clock stamp per phase boundary per sampled operation, keyed by the FUSE request id (which the kernel's FUSE tracepoints also carry) or the interception ticket; `tests/op_trace_stitch.py` stitches a dump against the `.stats` histograms. Owner-only, like `.stats`.
- **Daemon CPU by thread class.** `daemon_cpu_ns_by_class` on `.stats` splits daemon CPU across the FUSE queue workers and handler lanes, the interception service threads, the metadata and journal lanes, the block and NVMe workers, and the timer thread — the denominator every CPU-per-operation figure now cites.
- **fsck class C11 — large-file map consistency (report-only).** `squeezefs fsck` checks the block-map tree for orphan map records, heads with no records, and run-versus-point coverage anomalies. It reports and never auto-repairs; `fsck_map_orphan_records` must stay 0.
- **Background sweeps in `squeezefs job`.** Truncating or deleting a tree-mapped large file returns at once and leaves a resumable background job that reclaims the records and blocks in chunks; `squeezefs job list <mountpoint>` shows them as `kvmap_sweep` jobs, and they regenerate at mount after a crash.
- **`task dist:<distro>`** (`rocky8`, `rocky9`, `ubuntu2404`, `ubuntu2604`, or `dist:all`) builds the fat-LTO release binaries for a tagged release into `dist/<distro>-dist/`; `task build:<distro>` stays the fast thin-LTO build.

## Known limitations

- **Very large write-active files and RAM.** A file whose write map would exceed its derived share of the memory budget takes a bounded partial store (a dirty overlay plus warm windows with tree read-through), so a single petabyte-class file being written does not hold its whole map in RAM. The cost of that store is bounded but its economy is not finished: per-write probes on maps with many scattered blocks, the whole map still travelling with each co-writer publish, and run re-coalescing happening only on truncate/fsync-class saves are the open items, listed at the end of [docs/design-kvmap-block-map-tree.md](docs/design-kvmap-block-map-tree.md).
- **Scale claims and their evidence.** What ships is one write mount per volume set, any number of read-only mounts, and opt-in co-writer mounts. The very-large-fleet design target has been proven on a single-node fleet of many co-located mounts; every scale claim carries its evidence tier in [docs/rc-manifest.md](docs/rc-manifest.md#2-guarantee-table-by-evidence-tier-ruling-d1), and no number in these notes supports a wider claim.
- **The read path's remaining headroom is in the kernel.** On 4 KiB random reads through the kernel path, the FUSE-over-io_uring queue worker now spends most of its per-operation time inside the kernel's commit path — including contention on the FUSE connection's per-connection and per-queue locks — not in the daemon. Moving that is a kernel-side change, not a daemon one.

## Verification

The tag ships only when every line below is ticked. Every leg ran from zero on one commit (`ac3fb7eb`) in one unattended chain; the record is [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md).

- [x] `task check` — clippy (all features), clippy (shipped features), fmt, `cargo test --all-features -- --test-threads=1` (4,598 tests), rustdoc with `-D warnings`, Criterion bench smoke, the `crates/fuse3` fork's own suite, the loom-model build, the fuzz workspace type-check, the markdown link/anchor check, and `cargo audit` over both lockfiles ([the gate's legs](docs/operations.md#verifying-a-build-task-check)) — green on `ac3fb7eb`, [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md).
- [x] pjdfstests (`sudo tests/run_pjdfstests.sh`) — 238 files, 8,798 tests, all successful; [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md)
- [x] LTP filesystem syscalls (`sudo tests/run_ltp_syscalls.sh`) — 1,884 passed, 0 failed, 0 broken, 44 kernel-feature skips; [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md)
- [x] fstests `-g auto`, one complete pass from zero (`sudo tests/run_fstests.sh`) — 198 ran, 0 unexpected failures (the four by-design adjudications matched their pinned shapes; the expected-PASS sentinels 074/464 passed); [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md)
- [x] The fuzz campaign over the twelve `fuzz/fuzz_targets/` decoders (on-disk metadata incl. the block-map tree, the cluster and publish wires, the interception shared memory) — 383.6 M executions, 0 crashes, on the same product code; [.benchmarks/2026-09-03-release-1.2-fuzz.md](.benchmarks/2026-09-03-release-1.2-fuzz.md)
