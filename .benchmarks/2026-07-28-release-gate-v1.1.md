# 2026-07-28 — Release gate, v1.1 candidate (three-suite from-zero pass)

**Mandate:** AGENTS.md release-gate tier — pjdfstests + full LTP + full fstests
(`-g auto`) must ALL pass from zero on the release-candidate binary before any
release tag. Fail-fast fix-loop discipline + repro-port mandate binding.

## Candidate binary

- **Initial candidate:** `f3579f7` (dev tip at gate start). fstests fail-fasted at
  generic/003 (F1 below); the fix loop produced `fix/write-times-single-authority`,
  ff-merged to dev.
- **FINAL candidate:** **`f5468ed`** (dev tip after the F1 merge) — all three
  suites restarted from zero on it per the counted-run discipline (the f3579f7
  pjd/LTP passes verified the old binary; recorded below as provisional only).
- **Since last gate:** async block reclaim + fence + field-ledger fix, write-pipeline
  depth engine, fuse_ops counter, killpriv-V2 (FUSE_HANDLE_KILLPRIV_V2 negotiation +
  clearing law) — write-path + FUSE-semantics churn; suid/sgid/xattr-adjacent tests
  watched specially — plus the F1 write-times single-authority fix from this gate.

## Substrate & instruments

- Dev-box virtual NVMe substrate created (`sudo tests/dev_substrate.sh create`,
  loop transport): 4× mds null_blk 1G (`/dev/nvme1n1..4n1`) + 4× oss zram 8G
  (`/dev/nvme5n1..8n1`).
- Suite runners are self-contained per their own expectations: pjdfstests / LTP /
  fstests format+mount their own /dev/shm-backed volumes (runner defaults) —
  functional-correctness venue, not a measurement venue (instrument stated per
  the standing instrument-alignment lesson).
- Standing fstests adjudications (2026-07-23, the ONLY ones): generic/003 + 192
  (noatime/atime class), generic/213 (thin provisioning), generic/634 (i64-ns
  timestamp range) — pinned expected shapes must match byte-exact; generic/464
  and generic/074 are expected-PASS.

## Suite results

### Provisional (old binary f3579f7 — NOT acceptance; superseded by the F1 fix)

| Suite | Result | Counts | Runtime |
|---|---|---|---|
| pjdfstests | PASS (from zero) | 238 files / 8798 tests, 0 failed | 162 s prove wallclock (~6m51s runner) |
| LTP syscalls (full) | PASS (from zero) | 174 / 0 FAILED / 0 BROKEN / 9 TCONF | 11m55s |
| fstests `-g auto` | FAIL-FAST at generic/003 (test 3/787) | → F1 | ~2 min |

### Acceptance (FINAL binary f5468ed — from zero)

| Suite | Result | Counts | Runtime | Notes |
|---|---|---|---|---|
| pjdfstests | **PASS** (from zero) | 238 files / 8798 tests, 0 failed; expected-fail table ∅ | 161 s prove wallclock (runner ~6m17s) | `prove=0 verdict=0`; chown/00.t TODO-passed 16 subtests (harness TODO bookkeeping, not failures) |
| LTP syscalls (full) | **PASS** (from zero) | 174 passed / 0 FAILED / 0 BROKEN / 9 TCONF-skipped | 6m40s | Fail-fast never fired; skips are capability TCONFs (statx08 `FS_*_FL`, statx10 ext4/xfs-only class) |
| fstests `-g auto` (full) | **PASS** (from zero, fail-fast-clean end-to-end) | **787 ran, 783 clean, 4 expected-shape, 0 unexpected** (exit 0) | 6h40m | Expected-shape = exactly the standing adjudications: generic/003 (matched the core 4-line noatime variant this run), 192, 213, 634 — each byte-exact. **generic/464 PASS (176s), generic/074 PASS (11s)** per their expected-PASS pins. Killpriv-V2-adjacent suid/sgid tests (193, 314, 355, 683–685, 688) all PASS; capability skips (`[not run]`) are the usual FUSE/fiemap/ACL/reflink classes |

## Failures / fixes

### F1 — fstests fail-fast at generic/003 (test 3 of 787): pinned expected shape no longer byte-exact

**Symptom.** The from-zero `-g auto` run aborted at generic/003: observed diff was
the 4 atime lines only — a strict SUBSET of the 6-line pinned shape (the two
`change time has changed …` ctime lines absent). Sampling showed the ctime lines
were **nondeterministic** (4-, 5-, or 6-line shapes across runs).

**Root cause (daemon bug, fixed).** The write path authored inode times TWICE:
the FUSE write handler publishes `mtime=ctime=coarse_realtime_ns()` to the attr
cache (what every stat serves), while `KvMetaBackend::set_layout_and_size` — the
layout+size persist that the router's inline/staged commits and every
mover/flush/merge re-persist ride — fabricated a SECOND `ctime = now_ns()`
sampled later. When the two samples straddled a coarse tick (~1 ms), the durable
ctime ran one tick ahead of every served value — visible only across a remount.
Manual repro (`/dev/shm` sandbox, default writeback mount): served
`…663495724`, durable `…665495702`. The mtime face: with no kernel flush-times
SETATTR, the write's mtime was never made durable at all (remount regressed
mtime to create-time; reproduced in-process).

**Fix (branch `fix/write-times-single-authority`, merged to dev).**
- `test(meta)` repro-port (RED at f3579f7): `tests/write_times_durability_tests.rs`
  — `set_layout_and_size_never_authors_inode_times`,
  `write_stamp_is_the_single_durable_times_authority`,
  `write_times_park_survives_attr_cache_eviction`.
- `fix(meta,fuse)`: the handler's ONE stamp parks as the ino's pending-times
  refinement (M6 machinery — fold-visible, zero hot-path entries, batched drain
  + fsync/unmount durability points) via the new routed `park_write_times`;
  `set_layout_and_size` never touches times; the SETATTR absorb arm and drain
  advance-compares harmonized to SIGNED (i64-ns fold parity, generic/258
  domain); Write/SetAttr handlers gained entry `debug!` logging.

**Residual (kernel-interface-only, re-adjudicated).** Probed live on this kernel
(7.1.4): under the FUSE writeback cache the kernel authors regular-file m/ctime
at `write(2)`, **overrides GETATTR times incore** for the inode's lifetime, and
sends its stamp to the daemon **only on fsync — never on close** (op stream:
WRITE delivered at FLUSH time; no flush-times SETATTR follows; fsync leg DOES
produce `SETATTR{mtime,ctime}`). The daemon's best durable estimate is its
WRITE-arrival stamp, µs later — ~1 run in 5 straddles a coarse tick, showing a
PAIRED modify+change divergence per remount leg. No FUSE channel exists to
observe the kernel's stamp at close ⇒ the 131/478/504/634 exception class.
`tests/run_fstests.sh` now pins the ENUMERATED byte-exact variant set for
generic/003 (core 4-line noatime shape; +file1 pair; +file3 pair; +both) via a
`<t>@N` variant matcher; anything outside still aborts.

**Verification.** Repro tests RED at f3579f7 → GREEN post-fix; ×20 generic/003
with the fixed binary: **16× core / 2× file1-pair / 1× file3-pair / 1× both —
0 outside the enumeration**; full cargo gate on the branch (below).

## Fix-branch verification gate (code-class, ran from zero on `fix/write-times-single-authority`)

- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --check` — clean.
- `cargo test --all-features -- --test-threads=1` — **1515 passed / 0 failed** across 146 targets (21m33s).
- `cargo doc --no-deps` — exactly the 3 declared pre-existing intra-doc warnings (dev-tip debt; no new).
- `cargo bench --benches -- --test` — exit 0 (all bench targets smoke-pass).
- Merge: `--ff-only` to dev, branch deleted, pushed.

## Acceptance statement

**The release gate is GREEN on a single binary commit: `f5468ed`**
(`squeezefs 1.1.0 (f5468eddc633 / f5468eddc63319f2d37ad6cefb5743b2056e3438)`).

Per suite, each a complete from-zero run on that binary:
1. **pjdfstests** — PASS (8798/8798, expected-fail table ∅).
2. **LTP filesystem syscalls (full)** — PASS (174/174, 0 FAILED/BROKEN, 9 TCONF).
3. **fstests `-g auto` (full, 787 tests)** — fail-fast-clean end-to-end: 0 unexpected
   failures; the ONLY tolerated diffs were the four standing adjudications
   (003/192/213/634), each matching its pinned byte-exact shape; 464 and 074
   passed per their expected-PASS pins.

Runs prior to the F1 fix verified the old binary (f3579f7) and are recorded
above as provisional only, per the counted-run discipline. One fix landed
during the gate (F1, with its cargo repro-port per the mandate); no release
tag was created — that is the user's act.

Substrate: suite runners' own /dev/shm-backed volumes (functional venue);
the dev-box virtual NVMe substrate (loop) was built for the session and torn
down at the end.
