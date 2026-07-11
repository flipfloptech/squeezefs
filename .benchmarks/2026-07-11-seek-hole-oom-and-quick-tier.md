# 2026-07-11 — generic/285 daemon OOM root-cause/fix + QUICK-tier disposition

Branch `fix/seek-hole-oom` off dev@6a69952. Machine: 32-core, 109 GB RAM,
Linux 7.1.3-2-cachyos; thermal rails taskset 0-15 / JOBS=12 (max Tctl
observed 63.8 °C).

## Item A — generic/285 daemon OOM (~108 GB RSS)

### Root cause

`DataRouter::write_file` (src/routing.rs, pre-fix lines 2183–2242: the
`existing_data.resize(end_offset, 0)` patch-assemble + the
`durable_write_stripe_payload(payload)` promotion): a write into an
inline/staged file materialized the ENTIRE logical span `[0, end_offset)` in
RAM and then durably wrote EVERY block of it — including all zero-filled hole
blocks. generic/285 (`src/seek_sanity_test`, `huge_file_test` tests 10–12)
writes 64 KiB at `filsz − 64 KiB` for filsz = 8 GiB, `alloc_size<<31 + 1 MiB`
(≈8 TiB) and `alloc_size<<32 + 1 MiB` (≈16 TiB): the ~8 TiB `Vec` zero-fill
consumed all physical RAM (~108 GB RSS) → kernel OOM kill → dead ENOTCONN
mount → cascading QUICK failures. NOT an lseek handler bug: SqueezeFS has no
FUSE_LSEEK handler at all (kernel-default llseek semantics, O(1), and exactly
the "default behavior" mode seek_sanity accepts).

### Measured, live (release binary, 64 KiB write at 8 GiB−64 KiB)

| | daemon RSS delta | physical device writes |
|---|---|---|
| before (6a69952) | **+8.67 GiB** (8 GiB shape; the 8 TiB shape OOMs the box) | **8 GiB** for 128 KiB of data |
| after (fix) | **0 kB** (8 GiB AND 8 TiB shapes) | 2 × 4 MiB blocks |

Full `seek_sanity_test` (all 12 subtests incl. the 16 TiB shape): exit 0,
106/106 succ, daemon peak 562 MB (Δ ≈ 49 MB from a 513 MB mount baseline),
29 MB total on the data volume. `./check generic/285`: **pass**, 0s.

### Fix (commit `fix(routing): sparse O(map) layout promotion`)

* Promotion assembles ONLY data-bearing blocks (existing payload ∪ new write —
  ≤ `write_len/block_size + 2` chunks, zero-copy slices for fully-covered
  blocks) via the new `durable_write_sparse_blocks`; hole indices stay
  unmapped (read path already serves zeros for unmapped blocks).
* `new_size` folds in `meta.size`: a small write no longer regresses a
  truncate-up/fallocate-extend hole (100 GiB truncate + 4 KiB write kept
  100 GiB; before: shrank to 4 KiB).
* copy_file_range sources through `read_file_range_zero_copy` (O(chunk));
  the whole-file `DataRouter::read_file` (O(logical size) materialization,
  last caller) is deleted.
* Pins: tests/sparse_write_bounded_tests.rs (peak-RSS delta < 256 MiB,
  O(map) block maps, POSIX reads, cache-less branch, CFR source).

### Safety rail

`tests/run_fstests.sh` gained opt-in `SQUEEZEFS_FSTESTS_MEMMAX` (daemon
scope MemoryMax): an unbounded-allocation regression now cgroup-kills the
leaking daemon, never the box. The red-state CFR pin OOM-killed a 12G-capped
cgroup on the pre-fix tree — reproducing the leak class deterministically
without machine loss.

## Item B — QUICK-set disposition (post-285-fix, SQUEEZEFS_FSTESTS_MEMMAX=8G)

First clean QUICK run (no OOM cascade): failures {003, 074, 213, 616, 617},
notrun {009, 316}.

| test | disposition | detail |
|---|---|---|
| 285 | **REAL — FIXED** | above |
| 617 | **REAL — FIXED** | O_DIRECT (`fsx -Z`) read below EOF spanning a staged/inline implicit-zero hole tail returned only the physically-backed prefix ("uring read bad io length: 32768 instead of 53248"). Page cache masked it for buffered IO. Fixed: `read_file_range_zero_copy` inline/staged legs now zero-fill to `min(size, meta.size − offset)` (bounded by the request, incl. the uring `dest_addr` zero-copy branch). Pins: tests/read_full_length_tests.rs. |
| 003 | **platform — expected-fail** | atime/relatime/strictatime + ctime-across-remount semantics under FUSE attr caching + writeback-cache mount. 10 deterministic ERROR lines; not a data-path bug. |
| 213 | **platform — expected-fail** | thin provisioning: `fallocate(mode=0)` never reserves physical blocks and statfs is virtual, so the golden "fallocate: No space left on device" line never appears (single-line diff; all other legs pass). |
| 009 / 316 | **platform — expected-notrun** | `_require_xfs_io_command fiemap`; FUSE has no FIEMAP ioctl. Canaries for a future FIEMAP-capable stack. |
| 074 / 616 | **REAL, PRE-EXISTING — stopped with analysis** | see below |

### 074/616 analysis (stop point — distinct effort)

Signature: transient stale/zeros reads under buffered write+read churn; file
durably correct afterward. generic/074 `fstest -n 3 -F -l 2 -f 3 -s 30M
-b 512`: "Corruption in child N" (zeros or prior-content blocks). generic/616
fsx (buffered, 100k ops): `READ BAD DATA` — zeros for a recently-written
range; `fsx.616` == `fsx.616.fsxgood` on disk afterward.

Attribution: **zero-delta vs pristine dev@6a69952** (worktree build, same
capped-mount protocol, same seeds): fixed binary 2/3 seed-42 runs fail (ops
14503/62897), baseline 1/3 fails (op 1797); fstest corruption on both.
Pre-existing, newly *visible* in the QUICK tier because the 285 OOM cascade
previously killed/poisoned the tail of the run (and 616 soaks are
probabilistic per run).

Mechanism trail: a failing run increments `.stats
staged_payload_lost_reads = 41` on a healthy mount — reads race a
staged-identity transition (re-stage / promote / `release_superseded_staged`
window): the reader resolves a stale `file_id`, misses the ring entry, finds
no promoted `block_map[0]` fallback, and lands in the designed lost-payload
zeros-degrade leg (routing.rs staged else-branch). RSS during the soak also
creeps (~24 MB/min steady-state after the ~1.3 GB cache plateau; the harness
daemon hit an 8 G cap in ~2 min of full-speed fsx) — likely the same
transition churn retaining superseded entries; needs the same lifecycle fix.
Proper fix = staged-identity lifecycle hardening (read-side identity
seqlock/retry, or drain-superseded-entry-before-release), a multi-day
concurrency effort touching the staged commit/release protocol — out of
scope for this branch per mandate; left as the documented nondeterministic
expected-fail pair in the QUICK provenance.

## Expected QUICK-tier result (deterministic; deviation = regression)

* PASS: 001 008 013 069 075 091 112 127 263 **285** 469 618, **617**
* NOTRUN (platform): 009 316
* FAIL (platform, deterministic diff): 003 213
* FAIL (real pre-existing, nondeterministic — 074/616 family watch): 074 616
  (616 passes ~1/3 of runs; a pass is not a signal the family is fixed)

## Gate

Every commit: clippy `-D warnings` clean, `cargo fmt --check` clean,
`cargo test --all-features -- --test-threads=1` green (494 tests / 56 bins;
one known-flaky `test_kill9_remount_soak_v3` timing assert reproduced once
under full-suite load, 4/4 green isolated, unrelated subsystem), `cargo doc
--no-deps` **0 warnings**, `cargo bench --benches -- --test` green, loom
models green.
