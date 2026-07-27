# 2026-07-27 — Shim-row write amplification: terminal-free Write-Zeroes conviction + BLKDISCARD fix

Branch `perf/shim-write-amplification` (off dev `e314bec`, **unmerged
pending review**). Commits: red `473a830` (reclaim-economy pins +
`tests/write_amp_rig.sh` + the `SQUEEZEFS_IL_MAX_RUN_SLOTS` testing
lever), green `fda2f60` (`reclaim_freed_range_sync` — BLKDISCARD on
namespaces, punch only on file backings, counted reclaims). Contract
suite: `tests/block_free_reclaim_tests.rs` (6 tests; 2 red at `473a830`).

**The charter**: the scoreboard was stopped for a FIELD-CONFIRMED
mechanism — 6-node cluster, relaxed seq-write 1 MiB shim row, iostat
during: device **6.6 GB/s at 98 % util** serving user **3.6 GB/s** =
**1.85× write amplification**, `wareq-sz` ≈ 2.8 MiB against 4 MiB blocks;
kernel path same battery ≈ 1.1×. Working hypothesis handed in: 64 KiB
ring-chunk streams (P3 pipelined flights under slot fragmentation) were
triggering write-through/flush on blocks whose coverage union was still
growing. **This note is the confirming evidence for the field capture and
the adjudication of that hypothesis.**

## 1. Substrate + instrument (labeled)

32-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. The killed sibling's
nvmet-tcp localhost substrate, finished and reused: zram-backed
namespaces exported via `nvmet-tcp` (`sqztcp-oss0` → `/dev/nvme3n1` 32 GiB
data, `sqztcp-mds0` → `/dev/nvme4n1` 4 GiB meta; both `TRAN=tcp`,
RESCAP=0). Fresh cache-less format per rig run (4 MiB blocks); mount
`--daemon --allow-other --interception --mem-cache-size 1GB`,
`SQUEEZEFS_IPC_SERVICE_THREADS=8`, daemon+shim both
`SQUEEZEFS_IPC_ALLOW_DEV=1` (dev `-dirty` builds are a degenerate KD-7
identity BOTH halves must forgive — a mismatched-stamp pair fails the
rig's engagement check exactly as designed, observed live before the pair
was rebuilt).

**Instrument (stated)**: `tests/write_amp_rig.sh` (new; salvaged from the
sibling's `fabric_matrix.sh` scaffolding) — elbencho (dynamic 3.0.25,
shim-loadable), the scoreboard row spelling verbatim (`-w -t 16 -s 512m
-b 1m --direct`, 16 files, fresh after `rm -f`), medians of 3.
**Red/green = `device write bytes ÷ user bytes` on the DATA namespace**
from per-row `/proc/diskstats` deltas (meta rides its own namespace, so
the data delta is exact), plus `wareq`/`d_ops` (discard) columns and full
stats-inode deltas per rep. Engagement per charter rule 4: a shim row is
INVALID unless `ipc_ops_*` accounts for its ops (exits nonzero).
Rows: `kernel`, `shim` (ring/op 1.00), and `shim-frag` —
`SQUEEZEFS_IL_MAX_RUN_SLOTS=1` (new, **testing only**, in
`crates/squeezefs-preload/src/session.rs::claim_run`) caps every claimed
run at one slot so every 1 MiB op chunks to 16 × 64 KiB pipelined
out-of-order flights: the deterministic stand-in for field slot
fragmentation.

## 2. Reproduction (RED, dev `e314bec` pair)

| Row (1 rep smoke, 8 GiB user) | MiB/s | **AMP** | wareq | daemon write counters |
|---|---|---|---|---|
| kernel (first pass, fresh format) | 2598 | **0.995** | 3.98 MiB | `write_through_blocks`=2032 + escalations ≈ user bytes |
| shim | 2654 | **1.996** | 3.99 MiB | identical write counters — the extra 8 GiB is **unaccounted by every upload path** |
| shim-frag (131072 ring ops, 77k ooo runs) | 1812 | **1.994** | 3.98 MiB | `write_through_blocks`=2032 — **exactly one write-through per block despite 64 KiB out-of-order arrival** |

Follow-the-bytes (manual probes, same daemon):

- The extra volume is **~half during the row, ~half trailing after it**
  (6.5 GiB device writes in the idle window after a pass — attributed to
  the previous dataset's deferred unlink reclaim overlapping the row: the
  rig rows share a daemon, `rm -f` precedes each write pass).
- **Overwrite legs (no rm)**: kernel **2.000×**, shim **2.000×**, wareq
  4.00 MiB, `w_ops` = 2 × blocks — the duplicate is a full 4 MiB device
  write per displaced block, on BOTH paths.
- Initiator vs backstore: `/dev/nvme3n1` counts the extra 8 GiB;
  zram sees ~56 MiB of it → the second write is **all-zeros** (zram
  same-page dedup).

**Conviction**: `BackendRouter::free_block` → `punch_hole_sync` —
`fallocate(FALLOC_FL_PUNCH_HOLE)` on the RAW NVMe NAMESPACE, which the
block layer implements as `blkdev_issue_zeroout` = a **full block of
Write-Zeroes WRITE bandwidth per terminally-freed block** (lineage:
`333ce23` introduced punch-on-free for sparse-file-backing ENOSPC +
stale-read hygiene; `7b0b461` fixed its ordering). Every steady-state
overwrite (displaced keys) and delete (deferred unlink reclaim) stream
pays ~+1.0×. Arithmetic exact: extra device bytes ≡ freed blocks ×
4 MiB on every probe.

**The handed-in hypothesis is DISCONFIRMED** (this is the adjudication):
the RW3b coverage union holds under chunk-stream arrival — the shim-frag
row (16 × 64 KiB pipelined out-of-order flights per op, `active_block_ooo_runs`
= 77k) produced exactly 2032 write-throughs for 2032 blocks, zero
partial-coverage flushes, `write_path_seed_read_bytes` = 0. No flush
discipline change is needed; the "blocks flushed at ~70 % coverage" read
of the field `wareq` 2.8 MiB is instead the *average* of full-4 MiB data
writes and full-4 MiB (device-split) Write-Zeroes ranges. The kernel-path
~1.1× on the field battery is consistent with its row not overlapping a
free storm (fresh format / reclaim drained), not with a shim-specific
mechanism — the rig shows both paths amplify identically when frees
overlap.

## 3. The fix (green `fda2f60`)

`reclaim_freed_range_sync` replaces the unconditional punch; classification
`routing::free_reclaim_op(st_mode)`:

| Backing | Reclaim | Why |
|---|---|---|
| Regular file | `fallocate(PUNCH_HOLE\|KEEP_SIZE)` (unchanged) | host-FS sparse reclaim — the original ENOSPC motivation; cheap metadata, no device I/O |
| Block device | **`ioctl(BLKDISCARD)`** (NVMe DSM Deallocate) | deallocate is what free means; a range command, **no data payload, not write-bandwidth-accounted** |
| Anything else / refused | **skip, counted** (`block_free_reclaim_skipped`) | NEVER degraded into a zeroing write |

Correctness never depended on freed ranges reading zeros: unmapped blocks
serve zeros from hole semantics (`hole_read_zeros_tests`), reused offsets
are guarded by write-before-publish + the incarnation seqlock
(`reused_key_stale_fill_tests`), and the `begin_free` → reclaim →
`finish_free` window (`7b0b461`) is unchanged — the reclaim still strictly
happens-before reallocation, and non-terminal (clone-shared) frees still
reclaim nothing. Stats added: `block_free_{discards,discard_bytes,
file_punches,punch_bytes,reclaim_skipped}` — a field row's device-byte
delta now reconciles against the write-through family, with reclaims
visible in iostat's DISCARD columns instead of write bandwidth.

## 4. A/B (final pair, medians of 3, counted from zero, engagement-exact)

| Row (t16, 8 GiB user) | base (red) | **fixed** | verdict |
|---|---|---|---|
| seq-write kernel AMP | 0.995 (first-pass; 2.000 steady-state overwrite) | **0.995** (d_ops ≈ freed blocks) | reclaim off the write path |
| seq-write shim AMP | **1.996** | **0.995** | **bar ≤ 1.15 met** (kernel-class) |
| seq-write shim-frag AMP | 1.994 | **0.993** | holds under 64 KiB ooo chunk arrival |
| seq-write shim MiB/s | 2654 | 2534 (rig band 2380–2750 across sides) | no throughput regression |
| durable leg (fio 16×512 MiB `fsync_on_close` + syncfs, shim) | — | **AMP 1.008** | fsync semantics untouched |
| rand-write-4k shim (fio t8, 10 s, overwrites) | — | 55 k IOPS, `patch_writes`=454,792, `patch_edge_rmw_reads`=0, `write_path_seed_read_bytes`=0 | patch path intact + engaged |
| fio crc32c verify (bs=1m multi-chunk, shim) | — | **err=0**, `fsck_findings`=0 | data integrity |
| seq-read kernel / shim AMP (item 4) | — | 1.124 / **1.018**, rareq 4.0 / ~1.9 MiB | see §5 |

`block_free_reclaim_skipped` = 0 on every leg (zram-backed nvmet-tcp
accepts DSM); discards ≈ freed blocks per row. Multi-run discipline: the
two ENGAGEMENT-INVALID passes before the final count were the KD-7
stale-stamp pair and the daemon-side missing dev override (recorded,
never credited); the counted A/B is the final pair from zero.

## 5. Read-side sibling (charter item 4)

Verified while instrumented: seq-read shim serves **lower** device
amplification than kernel (1.018 vs 1.124–1.16) and higher throughput on
this rig (median 7471 vs 4216–7886 across runs; the kernel side is noisy
on this substrate). rareq differs by design (shim rows ride 64 KiB ring
chunks → ~1.9 MiB merged device reads; kernel whole-block 4 MiB) — **no
partial/refetch pattern**: `r_MiB` ≈ dataset + prefetch tail on both
sides, no wasted-refetch growth. The field's 0.90× seq-read shim row does
not reproduce on this substrate; nothing filed beyond this data.

## 6. Gates

On the final branch state: `cargo clippy --all-targets --all-features --
-D warnings` clean; `cargo fmt --check` clean; `cargo doc --no-deps` — 3
warnings byte-identical to the pre-existing dev set (`handoff_spawn` ×2 +
`GhostTable`); bench smoke `cargo bench --benches -- --test` green;
`cargo test --all-features -- --test-threads=1` full pass (see the branch
report); preload gate legs 1 + 2 (see the branch report). Targeted
suites re-run green: `block_free_reclaim_tests` 6/6,
`refcount_clone_tests`, `hole_read_zeros_tests`,
`reused_key_stale_fill_tests`, `write_through{,_coverage}_tests`,
`extent_patch_tests`, `fsync_single_barrier_tests`.

## 7. Residuals / field follow-up

- **Field confirmation**: re-run the fleet capture post-fix — expect
  device writes ≈ user bytes (≤ 1.15×) with the reclaim volume visible in
  iostat's discard columns (`d/s`, `dMB/s`) and
  `block_free_discards ≈` freed blocks on the stats inode. If any node
  shows `block_free_reclaim_skipped` growth, its namespace refuses DSM
  Deallocate — space is then not returned to thin substrates (never a
  correctness issue; freed ranges are unreadable by construction).
- **shim-frag throughput** (1371 vs 2534 MiB/s at ring/op 16): the
  fragmentation *cost* itself is P3's ring-economy residual, not this
  program's; the lever exists to keep this row measurable.
- **Discard hygiene vs stale-data-at-rest**: freed data now lingers on
  devices whose deallocate is non-zeroing until the offset is rewritten —
  same at-rest posture as staging mmap plaintext (noted in
  design-zero-copy-write-path §security); encrypted volumes are unaffected
  (ciphertext at rest).
- The rig substrate stays up (RAM-backed, reboot-ephemeral), as the
  sibling left it.
