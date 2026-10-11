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

## The final counts (pass 2, the integrated binary c65935a = dev 2026-10-11, quiet-ish venue)

| suite | run | pass | fail | not run / conf | fsck | verdict |
|---|---|---|---|---|---|---|
| pjdfstest (238 files / 8,798 subtests) | 1 of 1 on c65935a's parent 6c5f5cc; 3 of 3 on 382a13f | 8,798 − 9 TODO | 0 | 9 suite TODO | 0 | PASS (3× clean + the confirming run) |
| LTP `fs` (72 after the venue skip) | 1 / 2 / 3 | 67 / 67 / 67 | 0 / 0 / 0 | 5 conf | 0 / 0 / 0 | **PASS, 3 consecutive** |
| LTP `syscalls` (1,552) | 1 (complete); 2, 3 (daemon SIGKILLed ≈ 28 min in) | 1,236 | 93 (87 venue, 6 ours → 4 ledger + fcntl12 ×2 timeout → `LTP_TIMEOUT_MUL` 4) | 223 conf | 0 | one honest run; the 3-run count open (the venue's memory cage vs the daemon's RSS) |
| xfstests `-g auto` (generic 001–800) | the honest run + 2 re-runs | 139 + 68 + 6 = **213 distinct tests passed** | 2 stalls (074, 551) + 069 (perf) + 464 (stall) open | 553 not run (expected classes) + 16 venue excludes | 0 on every scrub | NOT a pass: three liveness stalls stand |

Ledgers: `pjdfstest-exceptions.md` 0 rows; `ltp-exceptions.md` 4 rows; `fstests-exceptions.md`
6 rows (`noatime` ×1, `by-design-single-writer-guard` ×5). Venue lists: `ltp-venue-skip.txt` 1
row (proc01); `fstests-venue-exclude.txt` 16 rows, each with its control or missing capability.

## pjdfstest

Build: `pjd/pjdfstest` HEAD, `autoreconf -ifs && ./configure && make` (≈ 40 s). Run shape:
`cd $MNT && prove -rv $PJD/tests` as root — 238 files, 8,798 subtests, 157 s wall per run.

| run | binary | failing | suite TODO | fsck | verdict |
|---|---|---|---|---|---|
| first | d45a62a⁻ | fsck answered **Corrupt** on the fresh flat set | — | C-class | harness red → defect 1 |
| 1 | 8fdab12 | 0 | 9 | 0 findings | PASSED |
| 1–3 | 382a13f | 0 / 0 / 0 | 9 / 9 / 9 | 0 / 0 / 0 | **3 consecutive clean runs** (18:29–18:38) |
| 1 | 6c5f5cc (pass 2, integrated) | 0 | 9 | 0 | PASSED (00:10–00:13; the confirming run on the integrated binary — no handler path changed since the 3× run except the ones named in pass 2's defects) |

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
| 1–3 | 9a79831 | NOT RUN in that session | | | | | the venue could not hold three daemons at once (see "The venue" below) |
| 1 (void) | c65935a (pass 2) | 55 | 12 | 5 | 0 | — | the daemon died silently at 04:42:21 during `ftest07/08` — the second's dmesg names a co-tenant OOM victim (pid 10122, 7.6 GB anon) at that second, nothing for our pid, no panic in our log: every later test `ENOTCONN`. VOID. Consequence: the suites now mount `--supervise` (the release posture's supervisor names a death's wait status and dumps a wedge) |
| 1–3 | c65935a (pass 2, supervised) | 67 / 67 / 67 | 0 / 0 / 0 | 5 / 5 / 5 | 0 (proc01 skipped by the venue list) | 0 / 0 / 0 | **3 consecutive clean runs** (04:47–06:17; 1776 / 1784 / 1787 s wall; 0 tripwires) |

`proc01` reads every `/proc` file and meets `/proc/kmsg` EPERM inside the container: it fails on
the ext4 control too → `ltp-venue-skip.txt`. The 5 CONF rows are LTP's own "needs a feature"
skips (quota, loop-device fs types absent on the venue). Ledger `docs/gates/ltp-exceptions.md`:
**no rows**.

`syscalls` (1,552 tests, ≈ 43 min per run, `LTP_FILES=syscalls`): run 1 on c65935a — 1,236 pass,
223 conf, 93 fail/brok, of which **87 fail on the ext4 control too** (the venue: capabilities,
sysctls, /proc, cgroup and namespace features the container lacks — `ltp-syscalls-run1.venue`)
and 6 are ours: `epoll_ctl06` (FUSE's kernel module gives every file a poll op), `fallocate03`
and `mremap06` (no fallocate — a named feature gap), `ioctl_loop05` (LTP refuses a loop file on
any FUSE fs) — four adjudicated rows in `docs/gates/ltp-exceptions.md` — and `fcntl12` /
`fcntl12_64`: `open()` of one file `getdtablesize()` times inside LTP's 30 s budget; ≈ 50 µs per
OPEN across the ring is the transport's cost (LTP's own remedy is `LTP_TIMEOUT_MUL`, now 4).
Runs 2 and 3 (the ledger rows, the multiplier, `setsockopt11` — a deliberate OOM, anon-rss 15.9 GB
at its kill — on the venue skip list from run 3): the daemon was **SIGKILLed ≈ 28 min in, both
times right after `pipe12`** (the supervisor: `exited (wait status 0x9)`; run 2's log shows
`mem_budget: unreclaimable 13.0 GB ≥ 95 % of the 13.7 GB budget … HARD BACKSTOP` seconds before;
dmesg had wrapped past the kill by the time it was read). The pipe/pidfd tests alone run clean
(10 tests, 2 s, a clean exit); the suite as a whole drives the 16 GiB cgroup to its cage and the
3.5 GB daemon is the kernel's victim — the RSS item below. Run 1 is the complete, honest
`syscalls` run; the 3-run count on this suite is open until the daemon's footprint or the venue
changes. fsck after every run: 0 findings.

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
`/build/tmp/suites-fstests-20261010-185020/check-run1.out` (`fstests-auto-run2.log`).

### Pass 2: the honest run (2026-10-11 00:12–04:06, binary 6c5f5cc, the ENOSPC agent sharing the venue)

`check -R xunit -g auto` reached `generic/759` when the harness's 14,000 s wall bound fired
(generic/069's 3 M appends had been aborted by hand after 30 min, generic/074 and 551 each
held the pass ≈ 30 min before their daemons were killed by hand). **139 passed, 553 not run,
47 failed** (`suites-fstests-20261011-001256/check-run1.out`):

| class | tests | disposition |
|---|---|---|
| cascade after the 074 daemon kill (the stale claim refused every mount until the helper attested it) | 075–097 (23) | VOID — re-run below |
| product defects, fixed in this pass (red-first) | 263, 313, 355 | defects 12–14 below; re-run below |
| harness: `-o remount` refused, the mount front's bring-up lines in stdout, a tearing-down holder refused | 294, 306, 452; 631, 634, 732, 754; 604 | the helper hands remount to the kernel, is silent on success, waits for a dying holder; re-run below |
| adjudicated (ledger `docs/gates/fstests-exceptions.md`) | 192 (`noatime`); 409, 410, 411, 589 (`by-design-single-writer-guard`: a second mount of one volume is a second daemon) | 5 rows |
| the venue's (`fstests-venue-exclude.txt`, each with its control or the capability it lacks) | 310 (dmesg -c), 394 (ulimit -f), 460/750/795 (sysctls), 504 (flock-by-fd rows absent on ext4 too), 519 (FIBMAP needs CAP_SYS_RAWIO), 563 (loop over loop), 564, 597, 598, 426/467/477/756/777 (`open_by_handle_at` EPERM without CAP_DAC_READ_SEARCH — ext4 control identical) | excluded with reasons |
| co-tenant OOM in dmesg | 464 | re-run below |
| **still open** | 069 (staged-arm append amplification), 074 (an OPEN parked at `handler_entry` ≈ 35 min with the conveyor idle and every thread asleep: `fuse_ops_inflight 1`; stats `074-stall-stats.json`; not reproduced standalone in 15 min of the same `fstest -n 3 -F -l 10 -f 5 -s 31457280 -b 512`), 551 (`aio-dio-write-verify`: 53 WRITEs parked past `write_admitted`, `lock_ticked_reregisters` 41,446 and climbing, `parked_buffers 6` / 20 MiB, conveyor idle — the writers wait on block locks while the parked-buffer gate waits for flushes that need those locks; `551-stall-stats.json`) | named in "Known, not yet fixed" |

Tripwires during the run (the harness's per-set `.alerts`): `write_shared_class_violation` ×6
(the shared classification's shape-table size above the inode's for one write — diagnostic, no
finding), and ONE `rewrite_shadow_displaced_drift` + `block_untracked_free_refusals` pair at
02:14:59 during `generic/345` (fsx mixed mmap/buffered soak on the TEST device: "the save
displaced a binding the epoch never parked", then a double release of block 315) — a rewrite-
epoch race, open.

Re-run of the voided/fixed/helper classes and the unrun tail (075–097, 263, 294, 306, 313, 355,
452, 464, 604, 631, 634, 732, 740, 754, 759–800 — 78 tests) on the integrated binary c65935a
(06:17–07:08, 3,042 s): **68 passed, 46 not run, 8 failed** — 263, 294, 313, 355, 452, 740 and
the whole 075–097 range PASS; the 8: `464` stalled again (the third stall record,
`464-stall-stats.json`: ONE `open(O_TRUNC)` parked at `handler_entry` 40 min with the root stat
still answered — so the supervisor's unresponsiveness rung never fired; `xfs_io -ftc pwrite`
from 16 writer/append/sync_range loops over 200 files), `604` (the helper's dying-holder wait
missed the flock holder's phrasing — fixed), `306` (the umount helper ran the orderly umount on
a bind of a file — fixed), `631`, `634`, `732`, `754`, `800` (the mkfs helper's format lines in
stdout — fixed). A second re-run of those seven (08:04): **306, 604, 631, 634, 754, 800 PASS**;
`732` mounts the scratch device at two mountpoints at once → adjudicated
(`by-design-single-writer-guard`). Both scrubs 0 findings.

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
| 12 | fstests `generic/263` (fsx -Z) | the READ zero pad was skipped for O_DIRECT-flagged files; an mmap fault / readahead on an O_DIRECT fd is a page-cache fill carrying the flag → the EOF page's tail stayed the installed buffer's old bytes (0xaaaa past EOF) | lifecycle test "a SHORT paged READ zero-completes … for an O_DIRECT-flagged file too" (red: the exemption) + the fsx op log replayed byte-exact (`--replay-ops`, 1,426 ops) and `fsx -N 10000 … -Z` 10,000 ops A-OK | cada955 |
| 13 | fstests `generic/313` | truncate/ftruncate/O_TRUNC bumped ctime but never mtime (the kernel sends no FATTR_MTIME with a size set) | economy test "a size set … marks the data modification AND the status change" | f808e56 |
| 14 | fstests `generic/355` | a priv-killing write cleared the bits in the record (`fuse_killpriv_clears` counted) but the kernel's cached mode outlived it: the WRITE reply carries no attrs and the kernel invalidates none | kv_fs test "a file carrying S_ISUID/S_ISGID is served with attr TTL 0" | 5c88df9 |
| 15 | fstests `generic/740` | a bare `format` overwrote a device carrying another filesystem (ext2/bfs/cramfs/minix: their superblocks live past our 8 magic bytes) | `meta/foreign_signature` table test + `cli/format` force-gate test; `mkfs.ext4/xfs/bfs/cramfs/minix` images refused by name on the venue | 426d0b2 |
| 11 | fsstress round d3, fstests `generic/074` | the conveyor's pass fiber parked on the strength of the election winner's wake alone; the wake coalesced into the fiber's own mutex-park wake — the latch stayed set, every committer parked forever | `conveyor_core_model.PassFiber` (model 7: the word + the inner mutex park; the park without the latch re-read fails `FinalViolated`) | 9a79831 (`passEntry` re-reads `Conveyor.leaderHeld()` before every park). The defects agent fixed the same wedge independently (`.wt-defects`: the latch loop + a 2 s liveness-tick backstop with `meta_conveyor_park_tick_rescues`, a red test against a real volume); the two reconcile into ONE fix at integration — both repros stay if they cover different interleavings |

Decision 0002 rows 442–451 and 454–456 record each divergence from the reference's shape.

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

- **generic/074 / generic/551 stalls** (pass 2, binary 6c5f5cc, the conveyor fix in): two
  distinct shapes, both with the conveyor idle and every thread asleep. 074 (`fstest`, 3 × 512 B
  writers, truncate loops): ONE `open` parked at `handler_entry` for 35 min
  (`074-stall-stats.json`, the daemon log named in `074-stall-daemon-log.path`); the earlier
  attempt on 9a79831 showed the same test with `lock_ticked_reregisters 13` before its daemon
  died. 551 (`aio-dio-write-verify -t 2164 -a size=15272960`, 53 concurrent direct AIO writes to
  one 15 MB file): every WRITE parked past `write_admitted`, `lock_ticked_reregisters` 41 k and
  climbing, `parked_buffers 6` (20 MiB) — the F2 parked-buffer gate park and the block locks the
  parked writers hold look like a cycle (`551-stall-stats.json`). Standalone `fstest` for 15 min
  did not stall. The supervised mounts (pass 2's last change) will dump every task's kernel
  stack after 30 s of an unanswered root stat on the next occurrence.
- **Dismount promotion `1 FAILED`** — PINNED: the only arm that declines without a storage or
  DMA error is `pack.dispatch` on an image above the chunk, and the staged arm rang exactly such
  images for truncate-grown files until defect 10; three post-fix fsstress rounds (seeds 5/9/11,
  277/280/278 promotions) failed 0, and every decline now names its cause at debug level
  (`cd00f5f`). Closed as a consequence of defect 10.
- **Staged-arm append amplification** (`generic/069`: `append_writer 3000000` — 3 M 4-byte
  O_APPEND writes): every write to a staged file re-stages the WHOLE image (one copy out of the
  ring, one copy in) and commits one journal entry; measured ≈ 270–380 appends/s at 1–2 MiB
  (≈ 3 ms each) → the test needs ≈ 3 h. Correct, slow: a write-path design item (an in-place
  ring append under the record's grain padding, or the write-back class for the staged arm).
- **Staging ring full → ENOSPC** under a 20-writer burst of 3–4 MiB files: owned by the
  `.wt-enospc` agent (F17's sibling) — not touched here.
- **`rewrite_shadow_displaced_drift` + `block_untracked_free_refusals`** once, in `generic/345`
  (fsx mixed mmap/buffered soak): the rewrite epoch's save displaced a binding it never parked
  and the block was released twice. One occurrence in ≈ 10 h of suites; the 345 fsx seed is in
  `345.full`.
- **`write_shared_class_violation`** ×6 across pass 2 (and ×1 in a fsstress round): the handler
  classified a write Shared (`end ≤ shape-table size`) while the inode's size was below `end`;
  diagnostic (the plane serves the write through its ordinary arms), source of the stale-high
  shape size not pinned.
- **exportfs / open_by_handle_at**: not a charter requirement and `FUSE_EXPORT_SUPPORT` is not
  advertised; on a capable host an unlinked file's handle answers ESTALE through the kernel's
  icache miss, a LIVE file's handle after a cycle-mount would too (no `LOOKUP "."` by nodeid).
  If export handles become a requirement the five exportfs tests move from the venue list to a
  `by-design-no-export-handles` ledger row or to an implementation.
- **Daemon RSS**: ≈ 1.9 GB idle → ≈ 3.0–3.7 GB under the suites and it does not return after
  idle/unlink (`exec_fiber_stack_bytes` 752 MB, `transport_zc_bounce` + `kmbuf_region` 302 MB
  each, `block_free_reclaim_queue_bytes` up to 2 GB transiently); on the 16 GiB shared venue the
  co-tenants' OOM kills (four on 2026-10-10, two on 2026-10-11) took our daemons down twice
  without a trace. The allocator's leak report is clean — the growth is pooled, not leaked;
  unprofiled.
