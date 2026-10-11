# Rung 9 — the external-suite release gates (pjdfstest, LTP, xfstests)

Charter §11 "Gates": release gate = pjdfstest + full LTP + full fstests `-g auto` all passing,
adjudicated by-design exceptions only (noatime, thin provisioning, ±292-year i64 timestamps);
require-mount and capability-gated suites as hard failures; the multi-run discipline (a failure
on any early run aborts the count; the count restarts from zero on the fixed binary); every
externally found failure lands with an in-process repro-port that is red before the fix.

Venue: the `sqz-zig` container (Ubuntu 24.04, kernel 7.2.6-sqz, arm64, 16 GiB shared with
other agents' daemons), `scripts/venue`. Suites run against a REAL mount: a fresh set per run
(meta 1 GiB sparse + 2 × 8 GiB sparse data + a staging/cache path under `/build/tmp`), mounted
with the release posture `-o default_permissions,allow_other` (each option's WHY in
`scripts/suites/common.sh`'s header — never a weaker posture to pass a test, charter §1.6),
`squeezefs fsck --offline --scrub` after every run (0 findings is part of the pass), the daemon
log scanned for `error`/`TRIPWIRE` lines (any = the run is void), the adjudication ledger
(`docs/gates/<suite>-exceptions.md`: an unlisted failure fails the gate; a row with no current
failure is reported stale and fails it too).

## The harness (`scripts/suites/`)

| file | role |
|---|---|
| `common.sh` | the ONE harness: `suite_prepare` (apt packages, versions pinned in `apt.lock` — a drift is a refusal), `suite_mount/umount/fsck`, `suite_daemon_alerts` (errors, tripwires, an OOM-killed daemon), `suite_judge` (the ledger), results under `/build/tmp/suites-<suite>-<date>/` with the daemon log |
| `pjdfstest.sh` | clone → `autoreconf -ifs && ./configure && make` → `prove -rv tests/` INSIDE the mount; `# TODO` subtests are the suite's own expectations, counted apart |
| `ltp.sh` | clone → `make autotools && ./configure --prefix=/build/ltp && make -j && make install`; runner `kirk` (runltp is retired upstream) with `TMPDIR` on the mount, `LTP_FILES="fs syscalls"`, a wall bound per file; EVERY failure is re-run on an ext4 control mount — a failure that fails there too is the venue's (`ltp-<file>-run<n>.venue`), never ours; `ltp-venue-skip.txt` holds the standing venue rows |
| `fstests.sh` + `mkfs.squeezefs`, `mount.squeezefs`, `fsck.squeezefs`, `setpaths.sh` | `FSTYP=fuse.squeezefs`: each set's meta image sits behind a LOOP device (`TEST_DEV`/`SCRATCH_DEV` — `check` needs a device `findmnt -S` can match), data images + cache beside the backing file; `/sbin/{mkfs,mount,umount,fsck}.fuse.squeezefs` symlink to the helpers; `_check_*_fs` after EVERY test runs our offline scrub — the per-test corruption oracle; `check -R xunit -g auto` |
| `docs/gates/*-exceptions.md` | the ledgers: `test-id | reason-class | evidence` |

Build steps: `zig build suites-smoke` (on the per-commit gate: a 2-minute pjdfstest subset,
`PJD_SMOKE=1 SUITE_RUNS=1`, against a fresh mount) and `zig build gate-release` (the three full
suites, `SUITE_RUNS=3`). Both Linux-only (`addFail` elsewhere).

## pjdfstest

Build: `pjd/pjdfstest` HEAD, `autoreconf -ifs && ./configure && make` (≈ 40 s). Run shape:
`cd $MNT && prove -rv $PJD/tests` as root — 238 files, 8,798 subtests, 157 s wall per run.

| run | binary | failing | suite TODO | fsck | verdict |
|---|---|---|---|---|---|
| first | d45a62a⁻ | fsck answered **Corrupt** on the fresh flat set | — | C-class | harness red → defect 1 |
| 1 | 8fdab12 | 0 | 9 | 0 findings | PASSED |
| 1–3 | 382a13f (final) | 0 / 0 / 0 | 9 / 9 / 9 | 0 / 0 / 0 | **3 consecutive clean runs** (18:29–18:38) |

The 9 `# TODO` rows are pjdfstest's own declared Linux divergences (`chown/00.t` ×8: "Linux
doesn't clear the SGID/SUID bits for directories"; `unlink/08.t#2`: unlink of a directory is
EISDIR on Linux) — the kernel's semantics under `default_permissions`, not a filesystem
decision. Ledger `docs/gates/pjdfstest-exceptions.md`: **no rows** (nothing to adjudicate).

## LTP

Build: `linux-test-project/ltp` HEAD, `make autotools && ./configure --prefix=/build/ltp &&
make -j$(nproc) && make install` (≈ 25 min, the venue's 9 cores); runner `kirk` (`--framework
ltp:root=/build/ltp --run-suite fs --env TMPDIR=$MNT --exec-timeout 900 --json-report`).

`fs` (73 tests): run shape 30 min wall; the file's write/read workers run entirely on the mount
(`growfiles`, `fs_fill`, `iogen`/`doio`, `rwtest`, `fs_inod`, `ftest`, `openfile`, …).

| run | binary | pass | fail | conf | venue | fsck | notes |
|---|---|---|---|---|---|---|---|
| first full | 807c8b7⁻ | — | daemon died | | | | OOM-killed by the venue during gf17/gf18 (16 GiB shared; `anon-rss 3.8 GB`) — the harness now names an OOM-killed daemon and voids the run |
| triage rounds | | 6/8, 3/4 | rwtest03 wedge, gf18, gf27–29, iogen01 | | | | defects 3–6 below, each red-first |
| 1 | 8268e9d | 67 | 0 | 5 | 1 (proc01) | 0 | TRIPWIRE `overlay_unpublished_at_fsync` ×5 in `fs_fill` → defect 7 |
| 1 | 7c509f8 | 67 | 0 | 5 | 1 (proc01) | 0 | PASSED, 1826 s wall, 0 alerts |
| 1 (void) | 382a13f | 36 | 32 | 5 | — | — | the venue's global OOM at 18:57:09 (another agent's daemon at 10.8 GB anon RSS was killed; every LTP worker on the mount died of SIGBUS at that second: `terminating on signal 7`) — the run is VOID, not a verdict |
| 1–3 | 9a79831 (final) | NOT RUN in this session | | | | | the venue could not hold three daemons at once (see "The venue" below); the 7c509f8 pass stands as the last clean `fs` run; the 3-run count restarts on the integrated binary |

`proc01` reads every `/proc` file and meets `/proc/kmsg` EPERM inside the container: it fails on
the ext4 control too → `ltp-venue-skip.txt`. The 5 CONF rows are LTP's own "needs a feature"
skips (quota, loop-device fs types absent on the venue). Ledger `docs/gates/ltp-exceptions.md`:
**no rows**.

`syscalls`: NOT RUN in this session (≈ 2 h unattended on a venue that OOM-killed four processes in
the last hour of it). The harness runs it with `LTP_FILES=syscalls SUITE_RUNS=1`; the per-file
wall bound is `LTP_TIMEOUT_S` (10,800 s), per test `LTP_EXEC_TIMEOUT_S` (900 s).

## xfstests

Build: `xfstests-dev` HEAD, `make -j$(nproc)` (≈ 6 min; `ltp/fsstress` is the driver several
generic tests and the triage probes use). Run shape: `check -R xunit -g auto` with
`FSTYP=fuse.squeezefs`, TEST_DEV `/dev/loop0`, SCRATCH_DEV `/dev/loop1`, one mkfs + mount +
fsck per test through the helpers; the daemon log of every (re)mount lands beside the set.

Two full-run attempts; neither completed as ONE verdict (both broken by the venue, below), but
together they cover the generic range once:

| attempt | binary | range reached | ran | not run | failed | failure classes |
|---|---|---|---|---|---|---|
| 1 (18:07–18:50) | d6a3a1e+pack fix | generic/001–074 (aborted) | 18 | 51 | 2 | `generic/030` EFBIG → **defect 10**, fixed; `generic/069` 3 M appends → the staged-arm amplification (below), writer aborted by hand after 13 min |
| 2 (18:50–20:5x) | 9a79831 | generic/001–074, then 551–end | 31 + the tail | 50 + the tail | see below | `generic/054`, `062`: scratch mount lost to a kmbuf order-8 ENOMEM during another agent's OOM (helper now compacts + retries); `generic/069` as above; `generic/074` (fstest, 3 × 512 B writers): the test daemon DIED under the 19:21–19:27 venue thrash (no panic, no OOM line for its pid; ops parked ≥ 90 s in the commit, `transport: the 100 ms park backstop rescued … a lost wake in the chain`), its stale claim then refused every later mount (075–550: 452 cascade "failures", VOID) until the helper learned to attest and clear a dead holder's claim; 551–end: `564`/`597`/`598` sysctl + rlimit bound (venue excludes), `563` loop-over-loop (helper shape, excluded), `589`/`604`/`617`/`619` two more daemon deaths' claims (same window, `daemon-died` ×3 recorded), `618` dmesg (an OOM kill of a `zig` process) |

The final `Ran/Not run/Failures` lines of attempt 2 are in
`/build/tmp/suites-fstests-20261010-185020/check-run1.out` (`fstests-auto-run2.log`); the
075–550 range needs its first honest run on a quiet venue.

Expected `_notrun` classes (fine, charter: `-g auto` passing modulo adjudication, generic tests
are the signal): shutdown/freeze (`fuse.squeezefs does not support shutdown/freezing`), xfs_io
`fpunch`/`fzero`/`fcollapse`/`finsert`/`falloc`/`fiemap` (no `FALLOCATE`/`FIEMAP` in the
must-implement set), quota, dm-* (no device-mapper over a FUSE device), xfs/ext4/btrfs-specific
groups.

### Triage before the full run (fsstress rounds, `scripts/suites/.probe.sh`)

`generic/013` (fsstress, 1000 ops × 20 procs) failed its post-test fsck: `c2_leaked` /
`c3_refcount` / `c8_drift` on pack blocks, `pack_pin_refcount_drift` ×14 in the daemon log.
Bisected by op class (five 20-proc rounds): **truncate + unlink** was the pair; a single-process
round (`-p 1`, write/truncate/unlink, no promotion at all) reproduced 8 findings + 4
`block_untracked_free_refusals` — defects 8–10 below. After the fixes: eight full-op rounds
(`-p 20 -r -n 1000`, seeds 2/5/9 + the op-class rounds) all fsck-clean, 0 untracked frees,
0 drift.

One wedge in those rounds (1 of 8), then again in `generic/074` of the first fstests attempt:
16 (then 4) committers parked in the commit conveyor, no pass scheduled, every daemon thread
asleep (`io_cqring_wait`/futex), `meta_conveyor_queued = meta_commit_parked`, `windows_inflight
0`, `leader_passes = lane_handoffs = pass_total`, 0 tick rescues (nothing was ever enqueued to
rescue). Root cause (defect 11): the pass VENUE is one long-lived fiber woken through the
task-state word; a wake that lands while the fiber is already SCHEDULED is a no-op — and the
fiber IS scheduled whenever it parked on the queue mutex inside `unleadAndRecheck`'s
`pending()` and the committer's unlock woke it; that committer's `tryLead` then beats the
fiber's own re-election, the fiber exits the pass and parks with the latch set. Stats of both
wedges: `suites-probe-20261010-165924/d3-wedge-stats.json`,
`suites-fstests-20261010-180752/074-wedge-stats.json`.

## Defects found by the suites — every one red-first, fixed in the product

| # | found by | defect | repro-port (red → green) | fix |
|---|---|---|---|---|
| 1 | pjdfstest harness | offline fsck's forest census ran on a flat (`--single-writer`) volume → Corrupt on a fresh format | `ops/fsck` runner test "flat volume: the census is skipped" | d45a62a |
| 2 | pjdfstest utimensat rows (venue probe `touch -d`) | explicit atime/mtime set left ctime unmoved | kv_fs test "utimensat bumps ctime" | 8fdab12 |
| 3 | LTP fs `rwtest03` (wedge) | the uring worker's warm-ladder probe took a BLOCKING block lock / overlay store on the reap thread | write_admit gf27 repro + `warm_ladder` try-only test | 18b205b |
| 4 | LTP fs `gf27–gf29` | the direct device leg served a block the write side held custody of (the 65th 1-byte grow read 0) | `read_path` custody-decline test | 386209b |
| 5 | LTP fs `gf18` | a short paged READ under zero-copy left page tails as the installed buffer's old bytes | `codec.readZeroTail` tests | 8835f1d, 8268e9d (the pad ends at `in.size`: past it a direct-io buffer answered EFAULT) |
| 6 | LTP fs `iogen01`/`rwtest` | `FUSE_AUTO_INVAL_DATA` advertised: the kernel's mtime heuristic raced a local write (read-after-write returned the previous lap) | lifecycle INIT flags test | 807c8b7 |
| 7 | LTP fs `fs_fill` | the fsync drain enumerated one 64-block page of an ino's overlay records (`overlay_unpublished_at_fsync`) | `device_overlay` test "the fsync drain covers EVERY open record … 70 blocks" | 7c509f8 |
| 8 | fstests `generic/013` | the corpse destroy read its plan BEFORE settling the write-side custody, and no (3.7) covered plan → commit; the staged/inline write arms published with no (3.7) | kv_fs `SettlingData` test "the corpse destroy settles the write-side custody BEFORE it reads the layout it frees, under (3.7)" | d6a3a1e |
| 9 | fstests `generic/013` (fsstress) | the pack seal's arithmetic ignored tenants released while the pack was open (`pack_pin_refcount_drift` on every rewrite/unlink of a packed file) | `pack` test "a tenant released while its pack is still OPEN … drift 0" | 63a6cb7 |
| 10 | fstests `generic/030` | a file grown past one block by truncate answered EFBIG on its next write (`promoteToStriped` DMA'd `size` bytes); the staged arm rang the whole grown file (unpromotable 5 MiB images) | kv_fs `SettlingData` test "a file grown past one block by truncate is written through the striped arm" | 382a13f |
| 11 | fsstress round d3, fstests `generic/074` | the conveyor's pass fiber parked on the strength of the election winner's wake alone; the wake coalesced into the fiber's own mutex-park wake — the latch stayed set, every committer parked forever | `conveyor_core_model.PassFiber` (model 7: the word + the inner mutex park; the park without the latch re-read fails `FinalViolated`) | 9a79831 (`passEntry` re-reads `Conveyor.leaderHeld()` before every park). The defects agent fixed the same wedge independently (`.wt-defects`: the latch loop + a 2 s liveness-tick backstop with `meta_conveyor_park_tick_rescues`, a red test against a real volume); the two reconcile into ONE fix at integration — both repros stay if they cover different interleavings |

Decision 0002 rows 442–451 record each divergence from the reference's shape.

## The venue (why the 3-run counts are incomplete)

The `sqz-zig` container has 15 GiB for every agent's daemons at once. In this session's last
two hours the kernel OOM-killed four processes (18:57 a 10.8 GB daemon, 19:27 a 12.5 GB
daemon, 19:4x a `zig` test runner, 19:53 a 2.7 GB daemon — none ours by pid, all on the same
cgroup), the container stopped accepting new processes for minutes (vsock dial-back timeouts,
load average 40), and our daemons died three times inside `generic/074`–`619` without a panic
line. Every run that overlapped those windows is marked VOID above, never a verdict. The gate
numbers that stand: pjdfstest 3 × clean on 382a13f (+ the conveyor fix changes no handler
path), LTP `fs` clean on 7c509f8, fsstress 8/8 clean on 63a6cb7+, `zig build gate` green on
macOS (1830 pass, 24 skip; rails 7/7) — the Linux gate's unit step is red ONLY in
`mem.budget` (the fleet-share census found other agents' daemons in the cgroup: "fleet share
2", a solo-venue assumption) and in the session/smoke matrix rows that need a quiet venue;
rerun on a quiet venue before the release gate.

## Known, not yet fixed (named here so the ledger never hides them)

- **Staged-arm append amplification** (`generic/069`: `append_writer 3000000` — 3 M 4-byte
  O_APPEND writes): every write to a staged file re-stages the WHOLE image (one copy out of the
  ring, one copy in) and commits one journal entry; measured ≈ 380 appends/s at 1.2 MiB
  (≈ 2.6 ms each) → the test needs hours. Correct, slow: a write-path design item (an in-place
  ring append under the record's grain padding, or the write-back class for the staged arm).
- **Staging ring full → ENOSPC** under a 20-writer burst of 3–4 MiB files (`staging ring full:
  a 3.5 MB record found no hole in a 343 MB segment with 155 live entries — refusing
  StorageFull`): the promotion under pressure lags the writers and the application sees ENOSPC
  on a volume with 1,700 free blocks. Loud (charter §1.8), but the right arm is a bounded park
  or the own-block arm, not a refusal.
  *Resolution* (`7f76af1` the arm, `508e668` the log; decision 0002 row 459). The ring is a
  cache, never the set's capacity: `kv_fs.writeStaged` answers null when the ring refuses the
  image (no hole, or the pool at its cap — nothing recorded, nothing published) and `write`
  routes the same payload through the striped arm — the resident image promoted durably to its
  own block, the layout flipped to striped, the piece admitted through the data plane
  (`fuse_write_staged_ring_full_own_block`); a truly exhausted set is still refused, by the
  plane's own terminal law. A full ring is one `warn` per episode (latched on the first refusal,
  cleared by the next placement that lands), not an error line per placement. Red test (kv_fs,
  the `SettlingData` harness's new `stage_refuse` seam): "the staged arm when the ring is
  FULL…" — a 1 MiB staged resident, the ring fills, a 2.5 MiB tail: pre-fix `expected null,
  found .NOSPC`; post-fix one own-block DMA of the 1 MiB resident, the tail admitted striped,
  the size 3.5 MiB, the ring saw no second image; a fresh file's first write goes striped with
  zero DMAs; the healed ring stages again. Venue repro (`--single-writer --disk-cache-paths`,
  `SQUEEZEFS_MEM_BUDGET_MB=512` ⇒ the 64 MiB ring floor, 20 concurrent 3.5 MiB buffered writers
  then `fsstress -p 20 -n 1000 -r`): at `53d05e1` **19 of 20 writers failed ENOSPC**, 594
  `staging ring full` error lines; at the fix **0 failed**, `staging_ring_full_refusals` 291 ≡
  `fuse_write_staged_ring_full_own_block` 291, `write_enospc_refusals` 0, fsstress rc 0 with 0
  "No space left", `fsck --offline` clean on both. Owed still: the staged-arm append
  amplification above (every write to a staged file re-stages the whole image), which is what
  makes the ring fill under a burst in the first place.
- **Daemon RSS**: ≈ 1.9 GB idle → ≈ 3.0–3.7 GB under the suites and it does not return after
  idle/unlink (`exec_fiber_stack_bytes` 752 MB, `transport_zc_bounce` + `kmbuf_region` 302 MB
  each, `block_free_reclaim_queue_bytes` up to 2 GB transiently); on the 16 GiB shared venue
  one full LTP run was OOM-killed. The allocator's leak report is clean (one benign 144 B
  `nvmet/initiator.zig:875` leak) — the growth is pooled, not leaked; unprofiled.
- **Dismount promotion `1 FAILED`** in two fsstress rounds (of 286 and 220 promotions): the
  resident stays in the ring until the next mount promotes it — a stated behaviour, root cause
  (a stale identity vs a storage-full arm) not pinned.
- The conveyor wedge above (1 in 8 heavy rounds).
